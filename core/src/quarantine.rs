//! Verwaltung wiederherstellbarer Sync-Quarantänen.
//!
//! Jeder reale Sync-Lauf schreibt vor dem ersten Ziel-Rename ein Sidecar-Manifest neben
//! seinen Quarantäneordner. Vor jedem einzelnen Verschieben wird der relative Ursprung
//! dauerhaft als Intent ergänzt. Dadurch bleibt auch nach einem Prozessabbruch bekannt,
//! wohin ein vorhandener Quarantäneeintrag zurückgehört.

use std::collections::HashSet;
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::atomic_write::{atomic_write_vfs, AtomicWriteMode};
use crate::error::{coded_error, ErrorCode};
use crate::fileops::guard;
use crate::path::{checked_join, expand_home_path, relative_path_string};
use crate::sync::validate_test_sandbox_root;
use crate::tree_measure::measure_entry;
use crate::vfs::{
    BoundQuarantinePurgeRequest, BoundQuarantineRestoreRequest, ConditionalContentReplaceRequest,
    Entry, LocalVfs, MutationFailure, ProofListEntry, Vfs,
};

const LEGACY_MANIFEST_VERSION: u32 = 1;
const UNBOUND_RECORD_MANIFEST_VERSION: u32 = 2;
const CONTENT_ONLY_RECORD_MANIFEST_VERSION: u32 = 3;
const MANIFEST_VERSION: u32 = 4;
/// Älteste Claim-Digestversion: bindet nur Pfad, Typ, Größe und Inhalt.
const LEGACY_SOURCE_DIGEST_VERSION: u32 = 1;
/// Zweite Claim-Digestversion: zusätzlich Metadatendigest, Modus, Owner, Gruppe und
/// Hardlinkidentität — aber ohne Zeitstempel. Sie wird nicht mehr neu geschrieben und
/// muss byteweise unverändert bleiben, damit vorhandene Manifeste dieser Version
/// weiterhin automatisch wiederherstellbar sind.
const METADATA_SOURCE_DIGEST_VERSION: u32 = 2;
/// Aktuelle Claim-Digestversion: wie Version 2, zusätzlich mtime und Erstellzeit.
const SOURCE_DIGEST_VERSION: u32 = 3;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

/// Aufbewahrungsziel für Quarantäneläufe. In der Sicherheitsphase wird nach Ablauf
/// nichts automatisch gelöscht: endgültiges Leeren bleibt immer separat bestätigt.
pub const QUARANTINE_RETENTION_DAYS: u64 = 30;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineState {
    Active,
    Complete,
}

/// Sichtbarer Laufzeitzustand eines manifestierten Quarantänelaufs.
///
/// `CrashRecovery` ist absichtlich von einem gerade lebenden Lauf getrennt: Beide
/// besitzen im Manifest `Active`, aber nur der lebende Lauf ist im Prozessregister
/// belegt und darf weder wiederhergestellt noch geleert werden.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineRunStatus {
    Live,
    CrashRecovery,
    PurgeRecovery,
    Complete,
    Purged,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    Replaced,
    MirrorRemoval,
    UserDelete,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct QuarantineRecord {
    relative_path: String,
    is_dir: bool,
    reason: QuarantineReason,
    /// Bereits vor dem Quarantäne-Claim gebundene Objektidentität. Version 2
    /// besaß diese Felder noch nicht und darf deshalb keinen neuen Restore starten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_identity: Option<QuarantineRestoreIdentity>,
    /// BLAKE3 über Dateiinhalt beziehungsweise den vollständig gelisteten Unterbaum
    /// zum Claim-Zeitpunkt. Das schützt auch gegen Änderungen innerhalb derselben
    /// Inode oder Provider-Version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_digest: Option<[u8; 32]>,
    /// Claim-Digestversion 2 bindet zusätzlich alle vom VFS exakt darstellbaren
    /// Metadaten, Forks, Sparse-Extents und die lokale Hardlinkidentität. Fehlende
    /// Felder in Manifestversion 3 bezeichnen den früheren reinen Inhaltsdigest.
    #[serde(default = "legacy_source_digest_version")]
    source_digest_version: u32,
    #[serde(default)]
    restored: bool,
    /// Vor der ersten Restore-Wirkung persistierte Bindung. Bleibt auch nach dem
    /// Abschluss als Recovery-Beleg im Manifest erhalten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore_intent: Option<QuarantineRestoreIntent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct QuarantineRestoreIntent {
    destination_relative: String,
    source_is_dir: bool,
    source_identity: QuarantineRestoreIdentity,
}

/// Über einen Rename stabile Identität. Lokale Dateien verwenden Gerät/Inode;
/// Provider dürfen künftig einen von ihrem gebundenen Restore-Vertrag garantierten
/// Token persistieren.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum QuarantineRestoreIdentity {
    Local { device: u64, inode: u64 },
    Provider { token: Vec<u8> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct QuarantineManifest {
    version: u32,
    run_id: String,
    created_unix_ms: u64,
    target_root: PathBuf,
    state: QuarantineState,
    /// Geheimnisfreier Digest der stabilen Provideridentität. Legacy-Manifeste
    /// bleiben kontrolliert lesbar, autorisieren aber keine Recoverywirkung.
    #[serde(default)]
    provider_identity_digest: [u8; 32],
    /// Stabile Identität genau des zufälligen Laufordners. Das Schema des Digests ist
    /// Bestandteil der Manifestversion; ein künftiger Algorithmus erhöht die Version.
    #[serde(default)]
    run_identity_digest: [u8; 32],
    /// Neue Läufe dürfen ihr leeres Manifest vor dem sichtbaren Laufordner anlegen.
    /// `true` besitzt nie Records; Legacy-Manifeste erhalten per Serde `false` und
    /// werden nicht still an den aktuellen Namensinhaber gebunden.
    #[serde(default)]
    run_identity_pending: bool,
    records: Vec<QuarantineRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct QuarantinePurgeIntent {
    version: u32,
    run_id: String,
    manifest_digest: [u8; 32],
    run_identity_digest: [u8; 32],
}

struct VerifiedPurgeIntent {
    proof: ProofListEntry,
    content_digest: [u8; 32],
}

const PURGE_INTENT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuarantineRunPresence {
    Bound,
    Missing,
}

/// Kompakte Laufzeile für die UI.
#[derive(Clone, Debug, Serialize)]
pub struct QuarantineRun {
    pub id: String,
    pub created_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub target_root: String,
    pub state: QuarantineState,
    pub status: QuarantineRunStatus,
    pub items_total: u64,
    pub items_available: u64,
    /// Physisch vorhandene Einträge, deren ursprünglicher Claim einen automatischen
    /// Restore samt vollständigem Readback autorisiert.
    pub items_restorable: u64,
    /// Physisch vorhandene Altstände ohne gebundene Claim-Identität. Sie bleiben
    /// sichtbar, benötigen aber einen ausdrücklich manuellen Recoveryweg.
    pub items_manual_recovery: u64,
    pub items_restored: u64,
    /// Restore-Intent mit fehlender Quelle. Ein Zielabgleich kann diesen Abschluss
    /// ohne erneuten Rename nachholen.
    pub items_recovery_pending: u64,
    pub entries_available: u64,
    pub bytes_available: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct QuarantineLeaseKey {
    provider_identity_digest: [u8; 32],
    target_root: PathBuf,
    run_id: String,
}

fn live_leases() -> &'static Mutex<HashSet<QuarantineLeaseKey>> {
    static LIVE: OnceLock<Mutex<HashSet<QuarantineLeaseKey>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

fn lease_registry() -> std::sync::MutexGuard<'static, HashSet<QuarantineLeaseKey>> {
    live_leases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Nicht serialisierbarer Prozessbesitz eines einzelnen Quarantänelaufs.
///
/// Der Drop ist die entscheidende Fehlerpfadgarantie: Auch bei Abbruch oder `?` wird
/// die Laufzeitbelegung entfernt. Ein weiterhin `Active` manifestierter Lauf wird
/// anschließend sichtbar zum Crash-Recoveryfall.
#[derive(Debug)]
pub struct QuarantineLease {
    key: QuarantineLeaseKey,
}

impl Drop for QuarantineLease {
    fn drop(&mut self) {
        lease_registry().remove(&self.key);
    }
}

/// Einzelner, im Manifest verankerter Wiederherstellungspunkt.
#[derive(Clone, Debug, Serialize)]
pub struct QuarantineEntry {
    pub relative_path: String,
    pub is_dir: bool,
    pub reason: QuarantineReason,
    pub restored: bool,
    pub available: bool,
    pub automatic_restore_available: bool,
    pub manual_recovery_required: bool,
    /// Eine Dateiwirkung kann bereits erfolgt sein; ein erneuter Restore-Aufruf
    /// gleicht Quell- und Zielidentität ab und schließt nur den Manifeststatus ab.
    pub recovery_pending: bool,
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineRestoreOutcome {
    /// Der Aufruf hat den manifestierten Eintrag exklusiv an das Ziel verschoben.
    Restored,
    /// Ein früherer Aufruf hatte bereits verschoben; dieser Aufruf hat nach
    /// Identitätsabgleich ausschließlich den Manifestabschluss nachgeholt.
    Reconciled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineRestoreResult {
    pub path: PathBuf,
    pub outcome: QuarantineRestoreOutcome,
}

/// Sicher gebundener lokaler Zugriff auf genau die Quarantäne eines Zielbereichs.
///
/// CLI und Desktop-App verwenden damit dieselbe Root-Verankerung, Zielfilterung und
/// Sandbox-Schranke. Die internen Pfade bleiben absichtlich verborgen, damit kein
/// Aufrufer eine der Prüfungen versehentlich auslassen kann.
pub struct QuarantineScope {
    vfs: LocalVfs,
    base: PathBuf,
    sandbox: Option<PathBuf>,
    expected_target: Option<PathBuf>,
}

impl QuarantineScope {
    /// Öffnet die Quarantäne eines vorhandenen lokalen Realmodus-Ziels. Der VFS hält
    /// den Elternordner per Deskriptor fest; jedes Manifest muss exakt zum Ziel passen.
    pub fn for_target(target: impl AsRef<Path>) -> io::Result<Self> {
        let target = std::fs::canonicalize(expand_home_path(target))?;
        if !target.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quarantäne-Ziel ist kein vorhandener Ordner",
            ));
        }
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Quarantäne-Ziel besitzt keinen sicheren Elternordner",
            )
        })?;
        let parent = std::fs::canonicalize(parent)?;
        Ok(Self {
            vfs: LocalVfs::rooted(&parent)?,
            base: parent.join(".doppeldecker-quarantine"),
            sandbox: None,
            expected_target: Some(target),
        })
    }

    /// Öffnet ausschließlich eine vollständig validierte, markierte Test-Sandbox.
    /// Auch reine Listen- und Restore-Zugriffe bleiben am gehaltenen Root verankert.
    pub fn for_test_sandbox(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = validate_test_sandbox_root(root.as_ref())?;
        Ok(Self {
            vfs: LocalVfs::rooted(&root)?,
            base: root.join(".doppeldecker-quarantine"),
            sandbox: Some(root),
            expected_target: None,
        })
    }

    pub fn runs(&self) -> io::Result<Vec<QuarantineRun>> {
        list_quarantine_runs_for_target(
            &self.vfs,
            &self.base,
            &self.sandbox,
            self.expected_target.as_deref(),
        )
    }

    pub fn require_run(&self, run_id: &str) -> io::Result<()> {
        if self.runs()?.iter().any(|run| run.id == run_id) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Quarantänelauf gehört nicht zum gewählten Ziel",
            ))
        }
    }

    pub fn entries(&self, run_id: &str) -> io::Result<Vec<QuarantineEntry>> {
        list_quarantine_entries_for_target(
            &self.vfs,
            &self.base,
            run_id,
            &self.sandbox,
            self.expected_target.as_deref(),
        )
    }

    pub fn restore(
        &self,
        run_id: &str,
        relative_path: &str,
    ) -> io::Result<QuarantineRestoreResult> {
        restore_quarantine_entry_for_target(
            &self.vfs,
            &self.base,
            run_id,
            relative_path,
            &self.sandbox,
            self.expected_target.as_deref(),
        )
    }

    pub fn purge(&self, run_id: &str) -> io::Result<()> {
        purge_quarantine_run_for_target(
            &self.vfs,
            &self.base,
            run_id,
            &self.sandbox,
            self.expected_target.as_deref(),
        )
    }
}

/// Sidecar-Manifest vor dem ersten möglichen Ziel-Rename anlegen.
pub(crate) fn start_quarantine_run(
    vfs: &dyn Vfs,
    run_root: &Path,
    target_root: &Path,
) -> io::Result<QuarantineLease> {
    match quarantine_run_identity_digest(vfs, run_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            start_quarantine_run_inner(vfs, run_root, target_root, [0; 32], true)
                .map(|started| started.lease)
        }
        Err(error) => Err(error),
        Ok(_) => Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Quarantänelaufordner existiert bereits ohne manifestierten Erzeugungsbeleg",
        )),
    }
}

pub(crate) struct QuarantineRunStartProof {
    pub lease: QuarantineLease,
    pub manifest_path: PathBuf,
    pub payload_digest: [u8; 32],
}

/// Legt das Startmanifest exklusiv an und gibt seinen exakten Bytedigest zurück.
/// Remote-Claims halten diesen Digest über sämtliche Intent-Ergänzungen bis zum
/// bedingten Abschluss; ein verkürztes oder ausgetauschtes Manifest fällt dadurch
/// spätestens vor der nächsten Wirkung auf.
pub(crate) fn start_quarantine_run_with_proof(
    vfs: &dyn Vfs,
    run_root: &Path,
    target_root: &Path,
    expected_run_identity: &crate::real_mode::RootIdentity,
) -> io::Result<QuarantineRunStartProof> {
    let run_identity_digest = stable_quarantine_run_identity_digest(expected_run_identity)?;
    if quarantine_run_identity_digest(vfs, run_root)? != run_identity_digest {
        return Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Quarantänelauf wurde nach seinem Erzeugungsbeleg ausgetauscht",
        ));
    }
    start_quarantine_run_inner(vfs, run_root, target_root, run_identity_digest, false)
}

fn start_quarantine_run_inner(
    vfs: &dyn Vfs,
    run_root: &Path,
    target_root: &Path,
    run_identity_digest: [u8; 32],
    run_identity_pending: bool,
) -> io::Result<QuarantineRunStartProof> {
    let run_id = run_id_from_root(run_root)?;
    if !target_root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Quarantäne-Zielwurzel muss absolut sein",
        ));
    }
    let provider_identity_digest = provider_identity_digest(vfs, target_root)?;
    let lease = acquire_lease(QuarantineLeaseKey {
        provider_identity_digest,
        target_root: target_root.to_path_buf(),
        run_id: run_id.clone(),
    })?;
    let manifest = QuarantineManifest {
        version: MANIFEST_VERSION,
        run_id,
        created_unix_ms: now_unix_ms(),
        target_root: target_root.to_path_buf(),
        state: QuarantineState::Active,
        provider_identity_digest,
        run_identity_digest,
        run_identity_pending,
        records: Vec::new(),
    };
    let manifest_path = manifest_path(run_root)?;
    let payload_digest = write_manifest_with_digest(vfs, &manifest_path, &manifest, true)?;
    Ok(QuarantineRunStartProof {
        lease,
        manifest_path,
        payload_digest,
    })
}

/// Belegt einen bereits manifestierten Lauf für eine fortgesetzte Recovery-Mutation.
/// Das ist derselbe Prozessvertrag wie beim Neuanlegen, ohne ein Manifest zu ersetzen.
pub(crate) fn lease_quarantine_run(
    vfs: &dyn Vfs,
    run_root: &Path,
    target_root: &Path,
) -> io::Result<QuarantineLease> {
    let base = run_root.parent().ok_or_else(|| invalid_path(run_root))?;
    let run_id = run_id_from_root(run_root)?;
    let (manifest, _, _) = load_validated_run(vfs, base, &run_id, &None, Some(target_root), true)?;
    acquire_lease(lease_key_for_manifest(vfs, &manifest)?)
}

/// Belegt einen vorhandenen Lauf über dieselbe provider-/rootgebundene Sicht, die
/// anschließend für Remote-Probe und Mutation verwendet wird.
pub fn lease_quarantine_run_for_target(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    expected_target: &Path,
) -> io::Result<QuarantineLease> {
    let (manifest, _, _) =
        load_validated_run(vfs, base, run_id, &None, Some(expected_target), true)?;
    acquire_lease(lease_key_for_manifest(vfs, &manifest)?)
}

/// Intent vor dem eigentlichen Rename dauerhaft ergänzen. Scheitert das Journal,
/// bleibt der sichtbare Zielname unverändert.
pub(crate) fn record_quarantine_intent(
    vfs: &dyn Vfs,
    run_root: &Path,
    relative_path: &Path,
    is_dir: bool,
    reason: QuarantineReason,
) -> io::Result<()> {
    let relative = relative_path_string(relative_path)?;
    let path = manifest_path(run_root)?;
    let mut manifest = read_manifest(vfs, &path)?;
    require_manifest_run_identity(vfs, &manifest, run_root)?;
    let source = checked_join(&manifest.target_root, &relative)?;
    let (source_identity, source_digest) = bind_quarantine_record_source(vfs, &source, is_dir)?;
    let record_added = append_quarantine_record(
        &mut manifest,
        relative,
        is_dir,
        reason,
        source_identity,
        source_digest,
    )?;
    if record_added {
        write_manifest(vfs, &path, &manifest, false)?;
    }
    Ok(())
}

/// Sonderweg für atomare Typwechsel: Dort liegt die zu parkende alte Version nach
/// dem Exchange unter einem zufälligen Staging-Namen und nicht mehr unter dem
/// späteren Restore-Ziel. Der Record bindet trotzdem denselben logischen Relativpfad.
pub(crate) fn record_quarantine_intent_for_source(
    vfs: &dyn Vfs,
    run_root: &Path,
    relative_path: &Path,
    source: &Path,
    is_dir: bool,
    reason: QuarantineReason,
) -> io::Result<()> {
    let relative = relative_path_string(relative_path)?;
    let path = manifest_path(run_root)?;
    let mut manifest = read_manifest(vfs, &path)?;
    require_manifest_run_identity(vfs, &manifest, run_root)?;
    let (source_identity, source_digest) = bind_quarantine_record_source(vfs, source, is_dir)?;
    if append_quarantine_record(
        &mut manifest,
        relative,
        is_dir,
        reason,
        source_identity,
        source_digest,
    )? {
        write_manifest(vfs, &path, &manifest, false)?;
    }
    Ok(())
}

pub(crate) struct QuarantineIntentProof {
    pub manifest_path: PathBuf,
    pub payload_digest: [u8; 32],
}

/// Schreibt denselben dauerhaften Intent und liefert zusätzlich den Digest genau
/// der Manifestbytes, die ein provideratomarer Claim in seiner Wirkung prüfen
/// muss. Dadurch kann ein kurzzeitig ausgetauschter Sidecar nicht per ABA wieder
/// aus dem getrennten Readback verschwinden.
pub(crate) fn record_quarantine_intent_with_proof(
    vfs: &dyn Vfs,
    run_root: &Path,
    relative_path: &Path,
    is_dir: bool,
    reason: QuarantineReason,
    expected_previous_digest: [u8; 32],
) -> Result<QuarantineIntentProof, MutationFailure> {
    let relative = relative_path_string(relative_path).map_err(MutationFailure::Unchanged)?;
    let path = manifest_path(run_root).map_err(MutationFailure::Unchanged)?;
    let (mut manifest, current_digest) =
        read_manifest_with_digest(vfs, &path).map_err(MutationFailure::Unchanged)?;
    require_manifest_run_identity(vfs, &manifest, run_root).map_err(MutationFailure::Unchanged)?;
    if current_digest != expected_previous_digest {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::InvalidData,
            "Quarantäne-Manifest änderte sich seit dem letzten gebundenen Claim",
        )));
    }
    let source =
        checked_join(&manifest.target_root, &relative).map_err(MutationFailure::Unchanged)?;
    let (source_identity, source_digest) =
        bind_quarantine_record_source(vfs, &source, is_dir).map_err(MutationFailure::Unchanged)?;
    if !append_quarantine_record(
        &mut manifest,
        relative,
        is_dir,
        reason,
        source_identity,
        source_digest,
    )
    .map_err(MutationFailure::Unchanged)?
    {
        return Ok(QuarantineIntentProof {
            manifest_path: path,
            payload_digest: current_digest,
        });
    }
    let payload = manifest_payload(&manifest).map_err(MutationFailure::Unchanged)?;
    let payload_digest = *blake3::hash(&payload).as_bytes();
    vfs.replace_file_if_content_matches(ConditionalContentReplaceRequest {
        path: &path,
        expected_digest: current_digest,
        replacement: &payload,
    })?;
    Ok(QuarantineIntentProof {
        manifest_path: path,
        payload_digest,
    })
}

fn append_quarantine_record(
    manifest: &mut QuarantineManifest,
    relative: String,
    is_dir: bool,
    reason: QuarantineReason,
    source_identity: QuarantineRestoreIdentity,
    source_digest: [u8; 32],
) -> io::Result<bool> {
    if let Some(existing) = manifest
        .records
        .iter()
        .find(|record| record.relative_path == relative)
    {
        if existing.is_dir == is_dir
            && existing.reason == reason
            && existing.source_identity.as_ref() == Some(&source_identity)
            && existing.source_digest == Some(source_digest)
            && existing.source_digest_version == SOURCE_DIGEST_VERSION
        {
            return Ok(false);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Widersprüchlicher Quarantäne-Intent für {relative}"),
        ));
    }
    manifest.records.push(QuarantineRecord {
        relative_path: relative,
        is_dir,
        reason,
        source_identity: Some(source_identity),
        source_digest: Some(source_digest),
        source_digest_version: SOURCE_DIGEST_VERSION,
        restored: false,
        restore_intent: None,
    });
    Ok(true)
}

#[derive(Serialize)]
struct QuarantineDigestRecord {
    relative_components: Vec<Vec<u8>>,
    is_dir: bool,
    size: u64,
    content_digest: Option<[u8; 32]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata_digest: Option<[u8; 32]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hardlink_identity: Option<(u64, u64, u64)>,
    /// `None` hält die serialisierten Strukturen der Versionen 1 und 2 bytegleich.
    /// Version 3 trägt dagegen immer den Container; darin bleibt auch ein nicht
    /// verfügbarer Einzelwert als JSON-`null` eindeutig vom alten Schema getrennt.
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamps: Option<QuarantineDigestTimestamps>,
}

#[derive(Serialize)]
struct QuarantineDigestTimestamps {
    mtime: Option<i64>,
    created: Option<i64>,
}

const fn legacy_source_digest_version() -> u32 {
    LEGACY_SOURCE_DIGEST_VERSION
}

fn bind_quarantine_record_source(
    vfs: &dyn Vfs,
    source: &Path,
    is_dir: bool,
) -> io::Result<(QuarantineRestoreIdentity, [u8; 32])> {
    let proof = listed_entry_proof(vfs, source)?;
    if proof.entry.is_symlink
        || proof.entry.is_dir != is_dir
        || (!proof.entry.is_dir && !proof.entry.is_file)
    {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            "Quarantäne-Quelle änderte vor dem manifestierten Claim ihren Typ",
        ));
    }
    let identity = restore_identity_from_proof(&proof)?;
    let digest = quarantine_entry_digest(vfs, source, &proof, SOURCE_DIGEST_VERSION)?;
    Ok((identity, digest))
}

fn quarantine_entry_digest(
    vfs: &dyn Vfs,
    source: &Path,
    proof: &ProofListEntry,
    version: u32,
) -> io::Result<[u8; 32]> {
    let payload = quarantine_digest_payload(vfs, source, proof, version)?;
    Ok(*blake3::hash(&payload).as_bytes())
}

/// Baut die exakten Bytes, aus denen der Claim-Digest entsteht. Die Trennung von der
/// Hashfunktion erlaubt einen Test, der die Serialisierung einer alten Digestversion
/// bytegenau festhält: Sobald jemand dort ein Feld ergänzt, verlieren vorhandene
/// Manifeste dieser Version ihren automatischen Restorepfad.
fn quarantine_digest_payload(
    vfs: &dyn Vfs,
    source: &Path,
    proof: &ProofListEntry,
    version: u32,
) -> io::Result<Vec<u8>> {
    let mut records = Vec::new();
    collect_quarantine_digest_records(vfs, source, proof, Vec::new(), &mut records, version)?;
    records.sort_by(|left, right| left.relative_components.cmp(&right.relative_components));
    let schema = match version {
        LEGACY_SOURCE_DIGEST_VERSION => "quarantine_source_v1",
        METADATA_SOURCE_DIGEST_VERSION => "quarantine_source_v2",
        SOURCE_DIGEST_VERSION => "quarantine_source_v3",
        _ => {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                "Quarantäne-Record besitzt eine unbekannte Claim-Digestversion",
            ))
        }
    };
    serde_json::to_vec(&(schema, records))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn collect_quarantine_digest_records(
    vfs: &dyn Vfs,
    path: &Path,
    proof: &ProofListEntry,
    relative_components: Vec<Vec<u8>>,
    records: &mut Vec<QuarantineDigestRecord>,
    version: u32,
) -> io::Result<()> {
    if proof.entry.is_symlink
        || (!proof.entry.is_dir && !proof.entry.is_file)
        || (proof.entry.is_dir && proof.entry.is_file)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Quarantäne-Digest traf einen Symlink oder Spezialeintrag: {}",
                path.display()
            ),
        ));
    }
    // Version 1 bindet gar keine Metadaten. Ab Version 2 kommen Metadatendigest,
    // Modus, Owner, Gruppe und Hardlinkidentität dazu; die Zeitstempel unten sind
    // allein Version 3 vorbehalten, damit Version 2 bytegleich bleibt.
    let (metadata_digest, mode, owner, group, hardlink_identity, timestamps) = if matches!(
        version,
        METADATA_SOURCE_DIGEST_VERSION | SOURCE_DIGEST_VERSION
    ) {
        let metadata_digest = match vfs.metadata_manifest(path) {
            Ok(manifest) => {
                manifest.validate()?;
                Some(manifest.digest)
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => None,
            Err(error) => return Err(error),
        };
        let hardlink_identity = match (
            proof.entry.device,
            proof.entry.inode,
            proof.entry.hard_links,
        ) {
            (Some(device), Some(inode), Some(links)) => Some((device, inode, links)),
            (None, None, None) => None,
            _ => {
                return Err(coded_error(
                    io::ErrorKind::InvalidData,
                    ErrorCode::InvalidData,
                    "Quarantäne-Eintrag meldet eine unvollständige Hardlinkidentität",
                ))
            }
        };
        (
            metadata_digest,
            proof.entry.mode,
            proof.entry.owner.clone(),
            proof.entry.group.clone(),
            hardlink_identity,
            if version == SOURCE_DIGEST_VERSION {
                Some(QuarantineDigestTimestamps {
                    mtime: proof.entry.mtime,
                    created: proof.entry.created,
                })
            } else {
                None
            },
        )
    } else {
        (None, None, None, None, None, None)
    };
    if proof.entry.is_file {
        records.push(QuarantineDigestRecord {
            relative_components,
            is_dir: false,
            size: proof.entry.size,
            content_digest: Some(*crate::atomic_file::hash_vfs_file(vfs, path)?.as_bytes()),
            metadata_digest,
            mode,
            owner,
            group,
            hardlink_identity,
            timestamps,
        });
        return Ok(());
    }

    records.push(QuarantineDigestRecord {
        relative_components: relative_components.clone(),
        is_dir: true,
        size: 0,
        content_digest: None,
        metadata_digest,
        mode,
        owner,
        group,
        hardlink_identity,
        timestamps,
    });
    let listing = vfs.list_complete(path)?;
    if !listing.complete {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!(
                "Quarantäne-Unterbaum ist nicht vollständig gelistet: {}",
                path.display()
            ),
        ));
    }
    let mut children = listing.entries;
    children.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
    for child in children {
        if child.raw_name.is_empty()
            || child.raw_name == b"."
            || child.raw_name == b".."
            || child.raw_name.contains(&0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Quarantäne-Unterbaum enthält einen unsicheren Namen",
            ));
        }
        let mut child_components = relative_components.clone();
        child_components.push(child.raw_name.clone());
        let child_path = path.join(std::ffi::OsString::from_vec(child.raw_name.clone()));
        collect_quarantine_digest_records(
            vfs,
            &child_path,
            &child,
            child_components,
            records,
            version,
        )?;
    }
    Ok(())
}

/// Erfolgreichen Lauf markieren. Ein Prozessabbruch lässt den Zustand bewusst auf
/// `active`; vorhandene Einträge bleiben trotzdem einzeln wiederherstellbar.
pub(crate) fn complete_quarantine_run(vfs: &dyn Vfs, run_root: &Path) -> io::Result<()> {
    let path = manifest_path(run_root)?;
    let mut manifest = read_manifest(vfs, &path)?;
    bind_pending_manifest_run_identity(vfs, &mut manifest, run_root, false)?;
    manifest.state = QuarantineState::Complete;
    write_manifest(vfs, &path, &manifest, false)
}

/// Schließt einen Remote-Lauf nur aus dem zuletzt bestätigten Intentzustand ab.
/// Ein konkurrierend verkürztes Manifest darf weder als vollständig markiert noch
/// durch einen unbedingten Read-modify-write überdeckt werden.
pub(crate) fn complete_quarantine_run_if_matches(
    vfs: &dyn Vfs,
    run_root: &Path,
    expected_previous_digest: [u8; 32],
) -> Result<[u8; 32], MutationFailure> {
    let path = manifest_path(run_root).map_err(MutationFailure::Unchanged)?;
    let (mut manifest, current_digest) =
        read_manifest_with_digest(vfs, &path).map_err(MutationFailure::Unchanged)?;
    require_manifest_run_identity(vfs, &manifest, run_root).map_err(MutationFailure::Unchanged)?;
    if current_digest != expected_previous_digest {
        return Err(MutationFailure::Unchanged(io::Error::new(
            io::ErrorKind::InvalidData,
            "Quarantäne-Manifest änderte sich vor dem Abschluss",
        )));
    }
    manifest.state = QuarantineState::Complete;
    let payload = manifest_payload(&manifest).map_err(MutationFailure::Unchanged)?;
    let payload_digest = *blake3::hash(&payload).as_bytes();
    vfs.replace_file_if_content_matches(ConditionalContentReplaceRequest {
        path: &path,
        expected_digest: current_digest,
        replacement: &payload,
    })?;
    Ok(payload_digest)
}

pub(crate) fn verify_quarantine_manifest_digest(
    vfs: &dyn Vfs,
    path: &Path,
    expected_digest: [u8; 32],
) -> io::Result<()> {
    let (_, current_digest) = read_manifest_with_digest(vfs, path)?;
    if current_digest != expected_digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Quarantäne-Manifest änderte sich nach der gebundenen Wirkung",
        ));
    }
    Ok(())
}

/// Alle manifestierten Läufe unterhalb der Quarantänebasis anzeigen.
pub fn list_quarantine_runs(
    vfs: &dyn Vfs,
    base: &Path,
    sandbox: &Option<PathBuf>,
) -> io::Result<Vec<QuarantineRun>> {
    list_quarantine_runs_for_target(vfs, base, sandbox, None)
}

/// Listet optional ausschließlich Läufe eines bereits kanonisch gebundenen Ziels.
/// Das Manifestfeld wird vor jeder Live-Prüfung ausgewertet: Geschwisterziele teilen
/// dieselbe Quarantänebasis, dürfen aber weder Provideridentität noch Erreichbarkeit
/// des ausgewählten Ziels beeinflussen.
fn list_quarantine_runs_for_target(
    vfs: &dyn Vfs,
    base: &Path,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
) -> io::Result<Vec<QuarantineRun>> {
    guard(sandbox, base)?;
    let listing = match vfs.list_complete(base) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if !listing.complete || listing.entries.iter().any(|entry| !entry.safely_decoded) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantäneläufe benötigen ein vollständiges, sicher dekodiertes Listing",
        ));
    }
    let mut listed_names = HashSet::with_capacity(listing.entries.len());
    let mut runs = Vec::new();
    for listed in listing.entries {
        if listed.raw_name != listed.entry.name.as_bytes()
            || !listed_names.insert(listed.raw_name.clone())
        {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                "Quarantänelisting enthält einen mehrdeutigen Namen",
            ));
        }
        let entry = listed.entry;
        if !entry.name.ends_with(".json") {
            continue;
        }
        if entry.is_symlink || !entry.is_file {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                "Quarantänemanifestname bezeichnet keine reguläre Datei",
            ));
        }
        let id = entry.name.trim_end_matches(".json");
        validate_run_id(id)?;
        let path = base.join(&entry.name);
        let manifest = read_manifest(vfs, &path)?;
        if manifest.run_id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Manifestname und Lauf-ID widersprechen sich: {}",
                    path.display()
                ),
            ));
        }
        if expected_target.is_some_and(|expected| manifest.target_root != expected) {
            continue;
        }
        guard(sandbox, &manifest.target_root)?;
        validate_manifest_provider_identity(vfs, &manifest)?;
        let run_root = base.join(id);
        let presence = validate_manifest_run_identity(vfs, &manifest, &run_root, true)?;
        let purge_started = purge_intent(vfs, base, id, &manifest)?.is_some();
        let purge_empty =
            purge_started && quarantine_run_is_empty(vfs, &run_root, presence, &manifest)?;
        let mut items_available = 0;
        let mut items_restorable = 0;
        let mut items_manual_recovery = 0;
        let mut items_restored = 0;
        let mut items_recovery_pending = 0;
        let mut entries_available = 0;
        let mut bytes_available = 0;
        for record in &manifest.records {
            if record.restored {
                items_restored += 1;
            }
            if presence == QuarantineRunPresence::Missing {
                if !record.restored && record.restore_intent.is_some() {
                    items_recovery_pending += 1;
                }
                continue;
            }
            let quarantine_path = checked_join(&run_root, &record.relative_path)?;
            match vfs.stat(&quarantine_path) {
                Ok(entry) => {
                    validate_record_type(record, &entry)?;
                    let (entries, bytes) = measure_entry(vfs, &quarantine_path, &entry)?;
                    items_available += 1;
                    if record_has_bound_claim(record) {
                        items_restorable += 1;
                    } else {
                        items_manual_recovery += 1;
                    }
                    entries_available += entries;
                    bytes_available += bytes;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !record.restored && record.restore_intent.is_some() {
                        items_recovery_pending += 1;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let live = lease_is_live(&lease_key_for_manifest(vfs, &manifest)?);
        let status = if live {
            QuarantineRunStatus::Live
        } else if purge_empty {
            QuarantineRunStatus::Purged
        } else if purge_started {
            QuarantineRunStatus::PurgeRecovery
        } else if manifest.state == QuarantineState::Active {
            QuarantineRunStatus::CrashRecovery
        } else {
            QuarantineRunStatus::Complete
        };
        runs.push(QuarantineRun {
            id: id.to_string(),
            created_unix_ms: manifest.created_unix_ms,
            expires_unix_ms: manifest
                .created_unix_ms
                .saturating_add(QUARANTINE_RETENTION_DAYS * MILLIS_PER_DAY),
            target_root: manifest.target_root.to_string_lossy().into_owned(),
            state: manifest.state,
            status,
            items_total: manifest.records.len() as u64,
            items_available,
            items_restorable,
            items_manual_recovery,
            items_restored,
            items_recovery_pending,
            entries_available,
            bytes_available,
        });
    }
    runs.sort_by_key(|run| std::cmp::Reverse(run.created_unix_ms));
    Ok(runs)
}

fn quarantine_run_is_empty(
    vfs: &dyn Vfs,
    run_root: &Path,
    presence: QuarantineRunPresence,
    manifest: &QuarantineManifest,
) -> io::Result<bool> {
    if presence == QuarantineRunPresence::Missing {
        // Nur ein Manifest, das nachweislich noch nie an einen Laufordner gebunden
        // war und keine Records besitzt, belegt bei fehlendem Namen leeren Bestand.
        // Ein vormals gebundener Lauf kann lediglich wegbenannt worden sein.
        return Ok(manifest.run_identity_pending && manifest.records.is_empty());
    }
    let listing = vfs.list_complete(run_root)?;
    if !listing.complete || listing.entries.iter().any(|entry| !entry.safely_decoded) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Purge-Status benötigt ein vollständiges, sicher dekodiertes Lauf-Listing",
        ));
    }
    Ok(listing.entries.is_empty())
}

/// Manifestierte Pfade eines Laufs mit aktueller Verfügbarkeit auflisten.
///
/// Ohne Zielbindung: Der Aufrufer bekommt das Manifest, ohne dass es an einen
/// bestimmten Zielroot gebunden wird. Produktivcode benutzt deshalb
/// [`QuarantineScope`] beziehungsweise die `_for_target`-Fassung; diese Kurzform
/// hat heute nur noch Aufrufer in Tests.
#[cfg(test)]
pub(crate) fn list_quarantine_entries(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    sandbox: &Option<PathBuf>,
) -> io::Result<Vec<QuarantineEntry>> {
    list_quarantine_entries_for_target(vfs, base, run_id, sandbox, None)
}

/// Bindet das Manifest im Realmodus zusätzlich an exakt den vom Nutzer gewählten
/// Zielroot; die targetlose Kurzform existiert nur für Modultests.
pub fn list_quarantine_entries_for_target(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
) -> io::Result<Vec<QuarantineEntry>> {
    let (manifest, run_root, presence) =
        load_validated_run(vfs, base, run_id, sandbox, expected_target, true)?;
    let mut entries = Vec::with_capacity(manifest.records.len());
    for record in manifest.records {
        let bound_claim = record_has_bound_claim(&record);
        let recovery_pending = !record.restored && record.restore_intent.is_some();
        let measured = match presence {
            QuarantineRunPresence::Missing => None,
            QuarantineRunPresence::Bound => {
                let quarantine_path = checked_join(&run_root, &record.relative_path)?;
                match vfs.stat(&quarantine_path) {
                    Ok(entry) => {
                        validate_record_type(&record, &entry)?;
                        Some(measure_entry(vfs, &quarantine_path, &entry)?)
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                }
            }
        };
        entries.push(QuarantineEntry {
            relative_path: record.relative_path,
            is_dir: record.is_dir,
            reason: record.reason,
            restored: record.restored,
            available: measured.is_some(),
            automatic_restore_available: bound_claim && (measured.is_some() || recovery_pending),
            manual_recovery_required: measured.is_some() && !bound_claim,
            recovery_pending,
            entries: measured.map(|value| value.0).unwrap_or(0),
            bytes: measured.map(|value| value.1).unwrap_or(0),
        });
    }
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(entries)
}

/// Einen einzelnen manifestierten Eintrag ohne Überschreiben an seinen Ursprung
/// zurückverschieben. Existiert dort inzwischen etwas, gewinnt immer der aktuelle Name.
///
/// Ohne Zielbindung: Die Funktion führt die Restore-Wirkung aus, ohne sie an einen
/// vorgegebenen Zielroot zu binden. Produktivcode benutzt deshalb
/// [`QuarantineScope`] beziehungsweise die `_for_target`-Fassung; diese Kurzform
/// hat heute nur noch Aufrufer in Tests.
#[cfg(test)]
pub(crate) fn restore_quarantine_entry(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    relative_path: &str,
    sandbox: &Option<PathBuf>,
) -> io::Result<QuarantineRestoreResult> {
    restore_quarantine_entry_for_target(vfs, base, run_id, relative_path, sandbox, None)
}

pub fn restore_quarantine_entry_for_target(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    relative_path: &str,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
) -> io::Result<QuarantineRestoreResult> {
    restore_quarantine_entry_to_for_target_inner(
        vfs,
        base,
        run_id,
        relative_path,
        Path::new(relative_path),
        sandbox,
        expected_target,
        None,
    )
}

/// Variante für einen Aufrufer, der die Lauf-Lease bereits vor einer mutierenden
/// Providerprobe erworben hat und sie durch Probe sowie Restore hält.
pub fn restore_quarantine_entry_for_target_with_lease(
    lease: &QuarantineLease,
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    relative_path: &str,
    expected_target: &Path,
) -> io::Result<QuarantineRestoreResult> {
    restore_quarantine_entry_to_for_target_inner(
        vfs,
        base,
        run_id,
        relative_path,
        Path::new(relative_path),
        &None,
        Some(expected_target),
        Some(lease),
    )
}

/// Interner Recovery-Pfad: Ein manifestierter Eintrag darf unter einem anderen,
/// ebenfalls relativen No-Replace-Namen wieder erscheinen. Das benötigt der atomare
/// Typwechsel-Rollback, um die alte Version erst am gebundenen Stagingnamen
/// bereitzustellen; normale Nutzer-Restores rufen weiterhin die Funktion darüber auf.
#[cfg(test)]
pub(crate) fn restore_quarantine_entry_to_for_target(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    relative_path: &str,
    destination_relative: &Path,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
) -> io::Result<QuarantineRestoreResult> {
    restore_quarantine_entry_to_for_target_inner(
        vfs,
        base,
        run_id,
        relative_path,
        destination_relative,
        sandbox,
        expected_target,
        None,
    )
}

pub(crate) fn restore_quarantine_entry_to_for_target_with_lease(
    lease: &QuarantineLease,
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    relative_path: &str,
    destination_relative: &Path,
    expected_target: &Path,
) -> io::Result<QuarantineRestoreResult> {
    restore_quarantine_entry_to_for_target_inner(
        vfs,
        base,
        run_id,
        relative_path,
        destination_relative,
        &None,
        Some(expected_target),
        Some(lease),
    )
}

#[allow(clippy::too_many_arguments)]
fn restore_quarantine_entry_to_for_target_inner(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    relative_path: &str,
    destination_relative: &Path,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
    held_lease: Option<&QuarantineLease>,
) -> io::Result<QuarantineRestoreResult> {
    let (mut manifest, run_root, presence) =
        load_validated_run(vfs, base, run_id, sandbox, expected_target, true)?;
    let key = lease_key_for_manifest(vfs, &manifest)?;
    let acquired_lease;
    if let Some(lease) = held_lease {
        require_held_lease(lease, &key)?;
    } else {
        acquired_lease = acquire_lease(key)?;
        let _ = &acquired_lease;
    }
    if purge_intent(vfs, base, run_id, &manifest)?.is_some() {
        return Err(coded_error(
            io::ErrorKind::PermissionDenied,
            ErrorCode::PermissionDenied,
            "Für diesen Quarantänelauf wurde bereits endgültiges Leeren begonnen",
        ));
    }
    if !vfs.supports_restore_from_bound_quarantine_run() {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Provider unterstützt keinen bis zur Wirkung gebundenen Quarantäne-Restore",
        ));
    }
    let relative = relative_path_string(Path::new(relative_path))?;
    let destination = relative_path_string(destination_relative)?;
    let record = manifest
        .records
        .iter()
        .find(|record| record.relative_path == relative)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Kein Quarantäne-Eintrag {relative}"),
            )
        })?;
    if record.restored {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Der Eintrag wurde bereits wiederhergestellt",
        ));
    }
    let (claim_identity, _claim_digest) = quarantine_record_claim(&record)?;

    let source = checked_join(&run_root, &relative)?;
    let target = checked_join(&manifest.target_root, &destination)?;
    guard(sandbox, &source)?;
    guard(sandbox, &target)?;
    let manifest_file = manifest_path(&run_root)?;
    let expected_target_root = vfs.root_identity(&manifest.target_root)?;
    if stable_provider_identity_digest(&expected_target_root)? != manifest.provider_identity_digest
    {
        return Err(coded_error(
            io::ErrorKind::PermissionDenied,
            ErrorCode::PermissionDenied,
            "Restore-Zielroot gehört nicht mehr zur manifestierten Provideridentität",
        ));
    }

    let (expected_source, intent_digest) = match record.restore_intent.clone() {
        Some(intent) => {
            if intent.destination_relative != destination {
                return Err(coded_error(
                    io::ErrorKind::InvalidInput,
                    ErrorCode::InvalidInput,
                    "Dieser Quarantäneeintrag besitzt bereits einen Restore-Intent für ein anderes Ziel",
                ));
            }
            let source_proof = optional_listed_entry_proof(vfs, &source)?;
            let target_proof = optional_listed_entry_proof(vfs, &target)?;
            match (source_proof, target_proof) {
                (Some(source_proof), None) => {
                    require_restore_identity(&intent, &source_proof)?;
                    if intent.source_identity != claim_identity {
                        return Err(coded_error(
                            io::ErrorKind::InvalidData,
                            ErrorCode::InvalidData,
                            "Restore-Intent stimmt nicht mit der beim Claim manifestierten Identität überein",
                        ));
                    }
                    require_quarantine_record_source(vfs, &record, &source, &source_proof)?;
                    let digest =
                        current_manifest_digest_if_unchanged(vfs, &manifest_file, &manifest)?;
                    (source_proof, digest)
                }
                (None, Some(target_proof)) => {
                    require_restore_identity(&intent, &target_proof).map_err(|error| {
                        coded_error(
                            error.kind(),
                            ErrorCode::OutcomeUnclear,
                            format!(
                                "Restore-Quelle fehlt, aber das Ziel gehört nicht zum manifestierten Eintrag: {error}"
                            ),
                        )
                    })?;
                    require_quarantine_record_source(vfs, &record, &target, &target_proof)
                        .map_err(|error| {
                            coded_error(
                                error.kind(),
                                ErrorCode::OutcomeUnclear,
                                format!(
                                    "Restore-Quelle fehlt, aber der Zielinhalt stimmt nicht mit dem Claim überein: {error}"
                                ),
                            )
                        })?;
                    // Der ursprüngliche Prozess kann direkt nach dem Rename und
                    // vor jedem Verzeichnis-fsync abgebrochen sein. Ein passender
                    // Readback allein erlaubt deshalb noch keinen dauerhaften
                    // Manifestabschluss. Auch neu angelegte Zieleltern müssen
                    // vollständig von innen nach außen gesichert werden.
                    sync_restore_parent_chain(vfs, &target, &manifest.target_root, false)
                        .and_then(|()| sync_restore_parent_chain(vfs, &source, base, true))
                        .map_err(|error| {
                            coded_error(
                                error.kind(),
                                ErrorCode::OutcomeUnclear,
                                format!("Restore-Ziel ist sichtbar, aber der dauerhafte Abschluss bleibt ungeklärt: {error}"),
                            )
                        })?;
                    require_current_root_identity(
                        vfs,
                        &manifest.target_root,
                        &expected_target_root,
                        "Restore-Zielroot änderte sich vor der Abschlussprüfung",
                    )?;
                    require_current_restore_target(
                        vfs,
                        &target,
                        &intent,
                        "Restore-Zieleintrag änderte sich vor der Abschlussprüfung",
                    )?;
                    let digest =
                        current_manifest_digest_if_unchanged(vfs, &manifest_file, &manifest)?;
                    finish_restore_manifest(vfs, &manifest_file, &mut manifest, &relative, digest)?;
                    require_current_root_identity(
                        vfs,
                        &manifest.target_root,
                        &expected_target_root,
                        "Restore-Zielroot änderte sich während der Abschlussprüfung",
                    )
                    .map_err(|error| {
                        coded_error(error.kind(), ErrorCode::OutcomeUnclear, error.to_string())
                    })?;
                    require_current_restore_target(
                        vfs,
                        &target,
                        &intent,
                        "Restore-Zieleintrag änderte sich während der Abschlussprüfung",
                    )
                    .map_err(|error| {
                        coded_error(error.kind(), ErrorCode::OutcomeUnclear, error.to_string())
                    })?;
                    return Ok(QuarantineRestoreResult {
                        path: target,
                        outcome: QuarantineRestoreOutcome::Reconciled,
                    });
                }
                (Some(source_proof), Some(_)) => {
                    require_restore_identity(&intent, &source_proof)?;
                    require_quarantine_record_source(vfs, &record, &source, &source_proof)?;
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Restore-Ziel existiert bereits; der Quarantäneeintrag blieb unverändert",
                    ));
                }
                (None, None) => {
                    return Err(coded_error(
                        io::ErrorKind::NotFound,
                        ErrorCode::OutcomeUnclear,
                        "Weder Restore-Quelle noch das manifestierte Ziel ist vorhanden",
                    ));
                }
            }
        }
        None => {
            if presence == QuarantineRunPresence::Missing {
                return Err(coded_error(
                    io::ErrorKind::NotFound,
                    ErrorCode::OutcomeUnclear,
                    "Quarantänelauf fehlt ohne einen vorbereiteten Restore-Intent",
                ));
            }
            let source_proof = listed_entry_proof(vfs, &source)?;
            validate_record_type(&record, &source_proof.entry)?;
            require_quarantine_record_source(vfs, &record, &source, &source_proof)?;
            let intent = QuarantineRestoreIntent {
                destination_relative: destination.clone(),
                source_is_dir: record.is_dir,
                source_identity: claim_identity,
            };
            let previous_digest =
                current_manifest_digest_if_unchanged(vfs, &manifest_file, &manifest)?;
            manifest
                .records
                .iter_mut()
                .find(|stored| stored.relative_path == relative)
                .expect("zuvor gefundener Quarantänerecord")
                .restore_intent = Some(intent);
            let digest =
                replace_manifest_if_matches(vfs, &manifest_file, &manifest, previous_digest)
                    .map_err(|failure| recovery_mutation_error("Restore-Intent", failure))?;
            (source_proof, digest)
        }
    };
    let expected_intent_manifest = listed_entry_proof(vfs, &manifest_file)?;
    let expected_run_root = current_bound_run_identity(vfs, &manifest, &run_root)?;
    vfs.restore_from_bound_quarantine_run(BoundQuarantineRestoreRequest {
        run_root: &run_root,
        expected_run_root: &expected_run_root,
        source: &source,
        target_root: &manifest.target_root,
        expected_target_root: &expected_target_root,
        target: &target,
        expected_source: &expected_source,
        intent_manifest: &manifest_file,
        expected_intent_manifest: &expected_intent_manifest,
        expected_intent_digest: intent_digest,
    })
    .map_err(|failure| recovery_mutation_error("Quarantäne-Restore", failure))?;

    let restored_proof = listed_entry_proof(vfs, &target).map_err(|error| {
        coded_error(
            error.kind(),
            ErrorCode::OutcomeUnclear,
            format!("Restore-Ziel konnte nach dem Rename nicht gebunden werden: {error}"),
        )
    })?;
    require_quarantine_record_source(vfs, &record, &target, &restored_proof).map_err(|error| {
        coded_error(
            error.kind(),
            ErrorCode::OutcomeUnclear,
            format!("Restore-Ziel änderte den manifestierten Claim-Inhalt: {error}"),
        )
    })?;

    finish_restore_manifest(vfs, &manifest_file, &mut manifest, &relative, intent_digest)?;
    Ok(QuarantineRestoreResult {
        path: target,
        outcome: QuarantineRestoreOutcome::Restored,
    })
}

/// Den Inhalt eines ganzen Laufs endgültig leeren. Manifest, Purge-Intent und leerer
/// Laufordner bleiben als sichtbarer Recovery-Tombstone erhalten, weil ein finales
/// pfadbasiertes Entfernen die zuvor gebundene Identität wieder verlieren würde.
/// Die zwingende Texteingabe wird zusätzlich in der App-Schicht geprüft; der Kern
/// erzwingt hier Pfad-, Manifest- und Sandboxbindung.
///
/// Ohne Zielbindung: Die Funktion führt die Purge-Wirkung aus, ohne sie an einen
/// vorgegebenen Zielroot zu binden. Produktivcode benutzt deshalb
/// [`QuarantineScope`] beziehungsweise die `_for_target`-Fassung; diese Kurzform
/// hat heute nur noch Aufrufer in Tests.
#[cfg(test)]
pub(crate) fn purge_quarantine_run(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    sandbox: &Option<PathBuf>,
) -> io::Result<()> {
    purge_quarantine_run_for_target(vfs, base, run_id, sandbox, None)
}

pub fn purge_quarantine_run_for_target(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
) -> io::Result<()> {
    purge_quarantine_run_for_target_inner(vfs, base, run_id, sandbox, expected_target, None)
}

/// Variante für einen Aufrufer, der die Lease schon vor der mutierenden
/// Provider-Capability-Probe hält.
pub fn purge_quarantine_run_for_target_with_lease(
    lease: &QuarantineLease,
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    expected_target: &Path,
) -> io::Result<()> {
    purge_quarantine_run_for_target_inner(
        vfs,
        base,
        run_id,
        &None,
        Some(expected_target),
        Some(lease),
    )
}

fn purge_quarantine_run_for_target_inner(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
    held_lease: Option<&QuarantineLease>,
) -> io::Result<()> {
    let (manifest, run_root, presence) =
        load_validated_run(vfs, base, run_id, sandbox, expected_target, true)?;
    let key = lease_key_for_manifest(vfs, &manifest)?;
    let acquired_lease;
    if let Some(lease) = held_lease {
        require_held_lease(lease, &key)?;
    } else {
        acquired_lease = acquire_lease(key)?;
        let _ = &acquired_lease;
    }
    if !vfs.supports_purge_bound_quarantine_run_contents() {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Provider unterstützt kein bis zur Wirkung gebundenes Quarantäne-Leeren",
        ));
    }
    if presence == QuarantineRunPresence::Missing
        && !manifest.run_identity_pending
        && manifest.run_identity_digest != [0; 32]
    {
        return Err(coded_error(
            io::ErrorKind::NotFound,
            ErrorCode::OutcomeUnclear,
            "Der gebundene Quarantänelauf fehlt; sein Bestand kann nicht als leer belegt werden",
        ));
    }
    let existing_intent = purge_intent(vfs, base, run_id, &manifest)?;
    if existing_intent.is_none()
        && manifest
            .records
            .iter()
            .any(|record| !record.restored && record.restore_intent.is_some())
    {
        return Err(coded_error(
            io::ErrorKind::PermissionDenied,
            ErrorCode::PermissionDenied,
            "Quarantänelauf besitzt einen offenen Restore-Abschluss; zuerst Wiederherstellung prüfen",
        ));
    }
    let intent = match existing_intent {
        Some(intent) => intent,
        None => ensure_purge_intent(vfs, base, run_id, &manifest)?,
    };
    if presence == QuarantineRunPresence::Missing {
        if manifest.run_identity_pending && manifest.records.is_empty() {
            // Der Lauf wurde nachweislich noch nie angelegt und besitzt keinen
            // Record. Der dauerhafte Intent reicht als Tombstone; einen leeren
            // Ordner anzulegen und danach pfadbasiert wieder zu öffnen würde erst
            // das Austauschfenster erzeugen, das dieser Sonderfall vermeiden muss.
            return Ok(());
        }
        // Zwischen der früheren Abwesenheitsprüfung und diesem Punkt könnte ein
        // gleichnamiger, nicht leerer Lauf entstehen. Ohne provideratomaren Claim
        // eines leeren Tombstone-Ordners darf die bloße Pfadabweisenheit deshalb
        // niemals als erfolgreicher Purge gelten. Der bereits dauerhafte Intent
        // hält Restore fail-closed; ein späterer Aufruf kann einen dann gebundenen
        // Lauf gezielt leeren.
        return Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Der Quarantänelauf war vor dem Purge abwesend; seine Abwesenheit lässt sich nicht atomar bis zum Erfolg binden",
        ));
    }
    if presence == QuarantineRunPresence::Bound {
        let expected_run_root = current_bound_run_identity(vfs, &manifest, &run_root)?;
        let intent_path = purge_intent_path(base, run_id)?;
        vfs.purge_bound_quarantine_run_contents(BoundQuarantinePurgeRequest {
            run_root: &run_root,
            expected_run_root: &expected_run_root,
            intent: &intent_path,
            expected_intent: &intent.proof,
            expected_intent_digest: intent.content_digest,
        })
        .map_err(|failure| recovery_mutation_error("Quarantäne-Purge", failure))?;
        vfs.verify_bound_quarantine_run_empty(BoundQuarantinePurgeRequest {
            run_root: &run_root,
            expected_run_root: &expected_run_root,
            intent: &intent_path,
            expected_intent: &intent.proof,
            expected_intent_digest: intent.content_digest,
        })
        .map_err(|failure| post_purge_validation_failure("gebundener Leerheitsbeleg", failure))?;
    }
    Ok(())
}

fn post_purge_validation_failure(context: &str, failure: MutationFailure) -> io::Error {
    let error = match failure {
        MutationFailure::Unchanged(error) | MutationFailure::OutcomeUnclear(error) => error,
    };
    coded_error(
        error.kind(),
        ErrorCode::OutcomeUnclear,
        format!("Quarantäne-Purge wirkte möglicherweise, aber {context} scheiterte: {error}"),
    )
}

fn recovery_mutation_error(context: &str, failure: MutationFailure) -> io::Error {
    match failure {
        MutationFailure::Unchanged(error) => error,
        MutationFailure::OutcomeUnclear(error) => coded_error(
            error.kind(),
            ErrorCode::OutcomeUnclear,
            format!("{context} kann bereits teilweise gewirkt haben: {error}"),
        ),
    }
}

fn restore_identity_from_proof(proof: &ProofListEntry) -> io::Result<QuarantineRestoreIdentity> {
    match (proof.entry.device, proof.entry.inode) {
        (Some(device), Some(inode)) => Ok(QuarantineRestoreIdentity::Local { device, inode }),
        (None, None) => proof
            .provider_token
            .as_ref()
            .filter(|token| !token.is_empty())
            .cloned()
            .map(|token| QuarantineRestoreIdentity::Provider { token })
            .ok_or_else(|| {
                coded_error(
                    io::ErrorKind::Unsupported,
                    ErrorCode::Unsupported,
                    "Quarantäneeintrag besitzt keine über einen Rename stabile Identität",
                )
            }),
        _ => Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantäneeintrag meldet nur einen Teil seiner lokalen Identität",
        )),
    }
}

fn require_restore_identity(
    intent: &QuarantineRestoreIntent,
    proof: &ProofListEntry,
) -> io::Result<()> {
    if proof.entry.is_symlink
        || proof.entry.is_dir != intent.source_is_dir
        || (!proof.entry.is_dir && !proof.entry.is_file)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Restore-Eintrag änderte seinen manifestierten Typ",
        ));
    }
    let matches = match &intent.source_identity {
        QuarantineRestoreIdentity::Local { device, inode } => {
            proof.entry.device == Some(*device) && proof.entry.inode == Some(*inode)
        }
        QuarantineRestoreIdentity::Provider { token } => {
            !token.is_empty() && proof.provider_token.as_ref() == Some(token)
        }
    };
    if !matches {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Restore-Eintrag stimmt nicht mit der manifestierten Identität überein",
        ));
    }
    Ok(())
}

fn quarantine_record_claim(
    record: &QuarantineRecord,
) -> io::Result<(QuarantineRestoreIdentity, [u8; 32])> {
    if !matches!(
        record.source_digest_version,
        LEGACY_SOURCE_DIGEST_VERSION | METADATA_SOURCE_DIGEST_VERSION | SOURCE_DIGEST_VERSION
    ) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantäne-Record besitzt eine unbekannte Claim-Digestversion",
        ));
    }
    match (record.source_identity.clone(), record.source_digest) {
        (Some(identity), Some(digest)) if digest != [0; 32] => Ok((identity, digest)),
        _ => Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Dieser ältere Quarantäne-Record bindet Identität und Inhalt nicht am ursprünglichen Claim; automatischer Restore ist gesperrt",
        )),
    }
}

fn record_has_bound_claim(record: &QuarantineRecord) -> bool {
    matches!(
        (record.source_identity.as_ref(), record.source_digest),
        (Some(_), Some(digest)) if digest != [0; 32]
    ) && matches!(
        record.source_digest_version,
        LEGACY_SOURCE_DIGEST_VERSION | METADATA_SOURCE_DIGEST_VERSION | SOURCE_DIGEST_VERSION
    )
}

fn require_quarantine_record_source(
    vfs: &dyn Vfs,
    record: &QuarantineRecord,
    path: &Path,
    proof: &ProofListEntry,
) -> io::Result<()> {
    validate_record_type(record, &proof.entry)?;
    let (identity, expected_digest) = quarantine_record_claim(record)?;
    let identity_matches = match identity {
        QuarantineRestoreIdentity::Local { device, inode } => {
            proof.entry.device == Some(device) && proof.entry.inode == Some(inode)
        }
        QuarantineRestoreIdentity::Provider { token } => {
            !token.is_empty() && proof.provider_token == Some(token)
        }
    };
    if !identity_matches {
        return Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::PlanDrift,
            "Quarantäne-Eintrag stimmt nicht mit der beim Claim manifestierten Identität überein",
        ));
    }
    if quarantine_entry_digest(vfs, path, proof, record.source_digest_version)? != expected_digest {
        return Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::PlanDrift,
            "Quarantäne-Eintrag stimmt inhaltlich nicht mit dem manifestierten Claim überein",
        ));
    }
    Ok(())
}

fn optional_listed_entry_proof(vfs: &dyn Vfs, path: &Path) -> io::Result<Option<ProofListEntry>> {
    match listed_entry_proof(vfs, path) {
        Ok(proof) => Ok(Some(proof)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn current_manifest_digest_if_unchanged(
    vfs: &dyn Vfs,
    path: &Path,
    expected: &QuarantineManifest,
) -> io::Result<[u8; 32]> {
    let (current, digest) = read_manifest_with_digest(vfs, path)?;
    if &current != expected {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantänemanifest änderte sich während der Restore-Vorbereitung",
        ));
    }
    Ok(digest)
}

fn require_current_root_identity(
    vfs: &dyn Vfs,
    path: &Path,
    expected: &crate::real_mode::RootIdentity,
    message: &'static str,
) -> io::Result<()> {
    if &vfs.root_identity(path)? != expected {
        return Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::PlanDrift,
            message,
        ));
    }
    Ok(())
}

fn sync_restore_parent_chain(
    vfs: &dyn Vfs,
    path: &Path,
    root: &Path,
    allow_missing: bool,
) -> io::Result<()> {
    let mut current = path.parent().ok_or_else(|| invalid_path(path))?;
    if !current.starts_with(root) {
        return Err(invalid_path(path));
    }
    loop {
        match vfs.sync_dir(current) {
            Ok(()) => {}
            // Ein bereits entfernter Quarantäne-Elternordner braucht keinen
            // eigenen Sync mehr; sein fehlender Name wird im nächsten vorhandenen
            // Vorfahren bis einschließlich Quarantänebasis dauerhaft gesichert.
            Err(error)
                if allow_missing && current != root && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if current == root {
            return Ok(());
        }
        current = current.parent().ok_or_else(|| invalid_path(path))?;
    }
}

fn require_current_restore_target(
    vfs: &dyn Vfs,
    target: &Path,
    intent: &QuarantineRestoreIntent,
    message: &'static str,
) -> io::Result<()> {
    let proof = listed_entry_proof(vfs, target).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            coded_error(io::ErrorKind::NotFound, ErrorCode::PlanDrift, message)
        } else {
            error
        }
    })?;
    require_restore_identity(intent, &proof).map_err(|error| {
        coded_error(
            error.kind(),
            ErrorCode::PlanDrift,
            format!("{message}: {error}"),
        )
    })
}

fn replace_manifest_if_matches(
    vfs: &dyn Vfs,
    path: &Path,
    manifest: &QuarantineManifest,
    expected_digest: [u8; 32],
) -> Result<[u8; 32], MutationFailure> {
    let payload = manifest_payload(manifest).map_err(MutationFailure::Unchanged)?;
    let digest = *blake3::hash(&payload).as_bytes();
    if vfs.supports_replace_file_if_content_matches() {
        vfs.replace_file_if_content_matches(ConditionalContentReplaceRequest {
            path,
            expected_digest,
            replacement: &payload,
        })?;
    } else if vfs.is_local() {
        // Lokale Prozesse teilen den äußeren Doppeldecker-Prozesslock. POSIX bietet
        // keinen atomaren „Digest prüfen und Namen ersetzen“-Aufruf; deshalb prüfen
        // wir unter diesem Lock unmittelbar vor dem bewährten Temp+fsync+Rename-Pfad.
        let (_, current_digest) =
            read_manifest_with_digest(vfs, path).map_err(MutationFailure::Unchanged)?;
        if current_digest != expected_digest {
            return Err(MutationFailure::Unchanged(io::Error::new(
                io::ErrorKind::InvalidData,
                "Quarantänemanifest änderte sich vor dem lokalen Austausch",
            )));
        }
        atomic_write_vfs(
            vfs,
            path,
            &payload,
            ".quarantine-manifest-",
            AtomicWriteMode::Replace,
        )
        .map_err(MutationFailure::OutcomeUnclear)?;
    } else {
        return Err(MutationFailure::Unchanged(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Remote-Restore benötigt einen atomaren inhaltsgebundenen Manifest-Austausch",
        )));
    }
    Ok(digest)
}

fn finish_restore_manifest(
    vfs: &dyn Vfs,
    path: &Path,
    manifest: &mut QuarantineManifest,
    relative: &str,
    expected_digest: [u8; 32],
) -> io::Result<()> {
    manifest
        .records
        .iter_mut()
        .find(|stored| stored.relative_path == relative)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Restore-Record fehlt"))?
        .restored = true;
    replace_manifest_if_matches(vfs, path, manifest, expected_digest).map_err(|failure| {
        let error = match failure {
            MutationFailure::Unchanged(error) | MutationFailure::OutcomeUnclear(error) => error,
        };
        coded_error(
            error.kind(),
            ErrorCode::OutcomeUnclear,
            format!(
                "Eintrag wurde wiederhergestellt, aber das Quarantänemanifest nicht abgeschlossen: {error}"
            ),
        )
    })?;
    Ok(())
}

/// Liest den unmittelbaren Elternordner vollständig und liefert genau den rohen
/// Namenseintrag. Ein getrenntes `stat` wäre kein Identitätsbeleg für die spätere
/// descriptor-gebundene Wirkung.
fn listed_entry_proof(vfs: &dyn Vfs, path: &Path) -> io::Result<ProofListEntry> {
    let parent = path.parent().ok_or_else(|| invalid_path(path))?;
    let raw_name = path
        .file_name()
        .ok_or_else(|| invalid_path(path))?
        .as_encoded_bytes();
    let listing = vfs.list_complete(parent)?;
    if !listing.complete || listing.entries.iter().any(|entry| !entry.safely_decoded) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantäne-Wirkung benötigt ein vollständiges, sicher dekodiertes Elternlisting",
        ));
    }
    let mut matches = listing
        .entries
        .into_iter()
        .filter(|entry| entry.raw_name == raw_name);
    let found = matches.next().ok_or_else(|| {
        coded_error(
            io::ErrorKind::NotFound,
            ErrorCode::NotFound,
            format!("Quarantäne-Eintrag fehlt: {}", path.display()),
        )
    })?;
    if matches.next().is_some() {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantäne-Elternlisting enthält denselben rohen Namen mehrfach",
        ));
    }
    Ok(found)
}

/// Liest den Lauf erneut und prüft den erhaltenen RootIdentity-Beleg gegen das
/// Manifest. Die lokale Wirkung öffnet danach denselben sichtbaren Pfad und bindet
/// ihn am Handle noch einmal an diesen Beleg; ein Swap zwischen beiden Schritten
/// führt daher sicher zu `Unchanged`.
fn current_bound_run_identity(
    vfs: &dyn Vfs,
    manifest: &QuarantineManifest,
    run_root: &Path,
) -> io::Result<crate::real_mode::RootIdentity> {
    let identity = vfs.root_identity(run_root)?;
    if stable_quarantine_run_identity_digest(&identity)? != manifest.run_identity_digest {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantänelauf wurde vor der descriptor-gebundenen Wirkung ausgetauscht",
        ));
    }
    Ok(identity)
}

fn purge_intent_path(base: &Path, run_id: &str) -> io::Result<PathBuf> {
    validate_run_id(run_id)?;
    Ok(base.join(format!("{run_id}.purge-intent")))
}

fn purge_intent(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    manifest: &QuarantineManifest,
) -> io::Result<Option<VerifiedPurgeIntent>> {
    let path = purge_intent_path(base, run_id)?;
    let entry = match vfs.stat(&path) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if entry.is_symlink || !entry.is_file || entry.size > 1024 {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Ungültiges Quarantäne-Purge-Intent",
        ));
    }
    let mut data = Vec::with_capacity(entry.size as usize);
    vfs.open_read(&path)?.take(1025).read_to_end(&mut data)?;
    if data.len() > 1024 {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantäne-Purge-Intent überschreitet die Größenbegrenzung",
        ));
    }
    let intent: QuarantinePurgeIntent = serde_json::from_slice(&data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let (_, manifest_digest) = read_manifest_with_digest(vfs, &manifest_path(&base.join(run_id))?)?;
    if intent.version != PURGE_INTENT_VERSION
        || intent.run_id != run_id
        || intent.manifest_digest != manifest_digest
        || intent.run_identity_digest != manifest.run_identity_digest
    {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantäne-Purge-Intent stimmt nicht mit Lauf und Manifest überein",
        ));
    }
    let proof = listed_entry_proof(vfs, &path)?;
    Ok(Some(VerifiedPurgeIntent {
        proof,
        content_digest: *blake3::hash(&data).as_bytes(),
    }))
}

fn ensure_purge_intent(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    manifest: &QuarantineManifest,
) -> io::Result<VerifiedPurgeIntent> {
    if let Some(intent) = purge_intent(vfs, base, run_id, manifest)? {
        return Ok(intent);
    }
    let sidecar = manifest_path(&base.join(run_id))?;
    let (current, manifest_digest) = read_manifest_with_digest(vfs, &sidecar)?;
    if current != *manifest {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantänemanifest änderte sich vor dem Purge-Intent",
        ));
    }
    let intent = QuarantinePurgeIntent {
        version: PURGE_INTENT_VERSION,
        run_id: run_id.to_string(),
        manifest_digest,
        run_identity_digest: manifest.run_identity_digest,
    };
    let payload = serde_json::to_vec_pretty(&intent)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let path = purge_intent_path(base, run_id)?;
    match atomic_write_vfs(
        vfs,
        &path,
        &payload,
        ".quarantine-purge-intent-",
        AtomicWriteMode::Create,
    ) {
        Ok(()) => purge_intent(vfs, base, run_id, manifest)?.ok_or_else(|| {
            coded_error(
                io::ErrorKind::NotFound,
                ErrorCode::OutcomeUnclear,
                "Quarantäne-Purge-Intent fehlt direkt nach seinem Commit",
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match purge_intent(
            vfs, base, run_id, manifest,
        )? {
            Some(intent) => Ok(intent),
            None => Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::AlreadyExists,
                "Der konkurrierende Quarantäne-Purge-Intent verschwand vor dem Readback; keine Wirkung wurde gestartet",
            )),
        },
        Err(error) => Err(error),
    }
}

fn load_validated_run(
    vfs: &dyn Vfs,
    base: &Path,
    run_id: &str,
    sandbox: &Option<PathBuf>,
    expected_target: Option<&Path>,
    allow_missing_run: bool,
) -> io::Result<(QuarantineManifest, PathBuf, QuarantineRunPresence)> {
    validate_run_id(run_id)?;
    guard(sandbox, base)?;
    let run_root = base.join(run_id);
    let manifest = read_manifest(vfs, &manifest_path(&run_root)?)?;
    if manifest.run_id != run_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Manifest-ID stimmt nicht mit dem gewählten Lauf überein",
        ));
    }
    if let Some(expected) = expected_target {
        if manifest.target_root != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Quarantänelauf gehört nicht zum gewählten Realmodus-Ziel",
            ));
        }
    }
    guard(sandbox, &run_root)?;
    guard(sandbox, &manifest.target_root)?;
    // Neue Manifeste binden den Lauf an die stabile Provideridentität. Ein Reconnect
    // derselben Identität bleibt verwaltbar; ein anderer Server, Share oder Account
    // unter demselben virtuellen Pfad wird fail-closed abgelehnt.
    validate_manifest_provider_identity(vfs, &manifest)?;
    let presence = validate_manifest_run_identity(vfs, &manifest, &run_root, allow_missing_run)?;
    Ok((manifest, run_root, presence))
}

fn validate_manifest_provider_identity(
    vfs: &dyn Vfs,
    manifest: &QuarantineManifest,
) -> io::Result<()> {
    if manifest.provider_identity_digest != [0; 32]
        && provider_identity_digest(vfs, &manifest.target_root)?
            != manifest.provider_identity_digest
    {
        return Err(coded_error(
            io::ErrorKind::PermissionDenied,
            ErrorCode::PermissionDenied,
            "Quarantänelauf gehört zu einer anderen Provideridentität",
        ));
    }
    Ok(())
}

fn provider_identity_digest(vfs: &dyn Vfs, target_root: &Path) -> io::Result<[u8; 32]> {
    let identity = vfs.root_identity(target_root)?;
    stable_provider_identity_digest(&identity)
}

fn quarantine_run_identity_digest(vfs: &dyn Vfs, run_root: &Path) -> io::Result<[u8; 32]> {
    let identity = vfs.root_identity(run_root)?;
    stable_quarantine_run_identity_digest(&identity)
}

/// Persistiert nur Objektkennungen, die sich beim Hinzufügen und Entfernen von
/// Kindern nicht ändern. Sitzungsnummern sowie WebDAV-Collection-ETags sind dafür
/// ungeeignet: Sie ändern sich bei Reconnect beziehungsweise bei genau den
/// Quarantänemutationen, die der Lauf aufnehmen soll.
fn stable_quarantine_run_identity_digest(
    identity: &crate::real_mode::RootIdentity,
) -> io::Result<[u8; 32]> {
    let provider = stable_provider_identity_digest(identity)?;
    let stable = match identity {
        crate::real_mode::RootIdentity::Local(value) if value.is_directory => {
            serde_json::to_vec(&(
                "local_quarantine_run_v1",
                provider,
                value.device,
                value.inode,
            ))
        }
        crate::real_mode::RootIdentity::Smb(value) if value.root_file_id != 0 => {
            value.validate()?;
            serde_json::to_vec(&(
                "smb_quarantine_run_v1",
                provider,
                &value.virtual_root,
                value.root_file_id,
            ))
        }
        crate::real_mode::RootIdentity::Local(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Quarantänelaufroot ist kein Ordner",
            ))
        }
        crate::real_mode::RootIdentity::Sftp(_)
        | crate::real_mode::RootIdentity::Ftps(_)
        | crate::real_mode::RootIdentity::WebDav(_)
        | crate::real_mode::RootIdentity::Smb(_) => return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Provider meldet keine über Reconnect und Kindmutationen stabile Laufordneridentität",
        )),
    }
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(*blake3::hash(&stable).as_bytes())
}

fn validate_manifest_run_identity(
    vfs: &dyn Vfs,
    manifest: &QuarantineManifest,
    run_root: &Path,
    allow_missing_run: bool,
) -> io::Result<QuarantineRunPresence> {
    if manifest.version == LEGACY_MANIFEST_VERSION {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Altes Quarantänemanifest besitzt keine gebundene Laufordneridentität",
        ));
    }
    if !matches!(
        manifest.version,
        UNBOUND_RECORD_MANIFEST_VERSION | CONTENT_ONLY_RECORD_MANIFEST_VERSION | MANIFEST_VERSION
    ) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!("Unbekannte Quarantäne-Manifestversion {}", manifest.version),
        ));
    }
    if manifest.run_identity_digest == [0; 32] {
        if manifest.run_identity_pending
            && manifest.state == QuarantineState::Active
            && manifest.records.is_empty()
            && allow_missing_run
        {
            return match vfs.root_identity(run_root) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    Ok(QuarantineRunPresence::Missing)
                }
                Err(error) => Err(error),
                Ok(_) => Err(coded_error(
                    io::ErrorKind::InvalidData,
                    ErrorCode::InvalidData,
                    "Quarantänelaufordner wurde sichtbar, bevor seine Identität manifestiert wurde",
                )),
            };
        }
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantänemanifest besitzt keine gültige Laufordnerbindung",
        ));
    }
    if manifest.run_identity_pending {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantänemanifest enthält zugleich eine gebundene und eine ausstehende Laufidentität",
        ));
    }
    let current = match quarantine_run_identity_digest(vfs, run_root) {
        Ok(current) => current,
        Err(error) if allow_missing_run && error.kind() == io::ErrorKind::NotFound => {
            return Ok(QuarantineRunPresence::Missing)
        }
        Err(error) => return Err(error),
    };
    if current != manifest.run_identity_digest {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Quarantänelaufordner stimmt nicht mit seiner manifestierten Identität überein",
        ));
    }
    Ok(QuarantineRunPresence::Bound)
}

/// Bindet ausschließlich den von einem neuen Manifest ausdrücklich angekündigten
/// Zwischenzustand. Ein Legacy-Manifest ohne Digest besitzt `pending=false` und
/// kann dadurch niemals nachträglich an einen beliebigen gleich benannten Ordner
/// geheftet werden.
fn bind_pending_manifest_run_identity(
    vfs: &dyn Vfs,
    manifest: &mut QuarantineManifest,
    run_root: &Path,
    allow_missing_run: bool,
) -> io::Result<bool> {
    if !matches!(
        manifest.version,
        UNBOUND_RECORD_MANIFEST_VERSION | CONTENT_ONLY_RECORD_MANIFEST_VERSION | MANIFEST_VERSION
    ) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Nur ein aktuelles Quarantänemanifest darf seine angekündigte Laufidentität binden",
        ));
    }
    if manifest.run_identity_digest != [0; 32] {
        validate_manifest_run_identity(vfs, manifest, run_root, allow_missing_run)?;
        return Ok(false);
    }
    if !manifest.run_identity_pending
        || manifest.state != QuarantineState::Active
        || !manifest.records.is_empty()
    {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Nur ein aktives, leeres Pending-Manifest darf eine Laufordneridentität binden",
        ));
    }
    let digest = match quarantine_run_identity_digest(vfs, run_root) {
        Ok(digest) => digest,
        Err(error) if allow_missing_run && error.kind() == io::ErrorKind::NotFound => {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    manifest.run_identity_digest = digest;
    manifest.run_identity_pending = false;
    // Ein alter, noch leerer Pending-Lauf kann sicher auf das Claim-gebundene
    // Schema angehoben werden. Version-2-Läufe mit Records werden dagegen nie
    // nachträglich an den dann sichtbaren Inhalt gebunden.
    manifest.version = MANIFEST_VERSION;
    Ok(true)
}

/// Persistiert die Laufordneridentität nach exklusiver Anlage und Readback, aber
/// zwingend vor dem ersten Nutzdaten-Claim.
pub(crate) fn bind_quarantine_run_identity(vfs: &dyn Vfs, run_root: &Path) -> io::Result<()> {
    let path = manifest_path(run_root)?;
    let mut manifest = read_manifest(vfs, &path)?;
    if bind_pending_manifest_run_identity(vfs, &mut manifest, run_root, false)? {
        write_manifest(vfs, &path, &manifest, false)?;
    }
    Ok(())
}

fn require_manifest_run_identity(
    vfs: &dyn Vfs,
    manifest: &QuarantineManifest,
    run_root: &Path,
) -> io::Result<()> {
    match validate_manifest_run_identity(vfs, manifest, run_root, false)? {
        QuarantineRunPresence::Bound => Ok(()),
        QuarantineRunPresence::Missing => Err(coded_error(
            io::ErrorKind::NotFound,
            ErrorCode::NotFound,
            "Gebundener Quarantänelauf fehlt",
        )),
    }
}

fn stable_provider_identity_digest(
    identity: &crate::real_mode::RootIdentity,
) -> io::Result<[u8; 32]> {
    let stable = match identity {
        crate::real_mode::RootIdentity::Local(value) => match value.mount.volume_id {
            Some(volume_id) => {
                serde_json::to_vec(&("local_volume", volume_id, &value.mount.fs_type))
            }
            None => serde_json::to_vec(&(
                "local_mount",
                value.device,
                &value.mount.fsid,
                &value.mount.fs_type,
            )),
        },
        crate::real_mode::RootIdentity::Sftp(value) => serde_json::to_vec(&("sftp", &value.server)),
        crate::real_mode::RootIdentity::Ftps(value) => serde_json::to_vec(&("ftps", &value.server)),
        crate::real_mode::RootIdentity::WebDav(value) => {
            serde_json::to_vec(&("webdav", &value.server))
        }
        crate::real_mode::RootIdentity::Smb(value) => serde_json::to_vec(&("smb", &value.server)),
    }
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(*blake3::hash(&stable).as_bytes())
}

fn lease_key_for_manifest(
    vfs: &dyn Vfs,
    manifest: &QuarantineManifest,
) -> io::Result<QuarantineLeaseKey> {
    let provider_identity_digest = if manifest.provider_identity_digest == [0; 32] {
        provider_identity_digest(vfs, &manifest.target_root)?
    } else {
        manifest.provider_identity_digest
    };
    Ok(QuarantineLeaseKey {
        provider_identity_digest,
        target_root: manifest.target_root.clone(),
        run_id: manifest.run_id.clone(),
    })
}

fn acquire_lease(key: QuarantineLeaseKey) -> io::Result<QuarantineLease> {
    let mut registry = lease_registry();
    if !registry.insert(key.clone()) {
        return Err(coded_error(
            io::ErrorKind::WouldBlock,
            ErrorCode::QuarantineBusy,
            "Quarantänelauf wird noch von einer laufenden Operation verwendet",
        ));
    }
    Ok(QuarantineLease { key })
}

fn lease_is_live(key: &QuarantineLeaseKey) -> bool {
    lease_registry().contains(key)
}

fn require_held_lease(lease: &QuarantineLease, expected: &QuarantineLeaseKey) -> io::Result<()> {
    if lease.key == *expected && lease_is_live(expected) {
        Ok(())
    } else {
        Err(coded_error(
            io::ErrorKind::PermissionDenied,
            ErrorCode::PermissionDenied,
            "Gehaltene Quarantäne-Lease gehört nicht zum gewählten Lauf",
        ))
    }
}

fn manifest_path(run_root: &Path) -> io::Result<PathBuf> {
    let parent = run_root.parent().ok_or_else(|| invalid_path(run_root))?;
    let id = run_id_from_root(run_root)?;
    Ok(parent.join(format!("{id}.json")))
}

fn run_id_from_root(run_root: &Path) -> io::Result<String> {
    let id = run_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_path(run_root))?;
    validate_run_id(id)?;
    Ok(id.to_string())
}

fn validate_run_id(run_id: &str) -> io::Result<()> {
    if run_id.is_empty()
        || run_id.len() > 128
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Ungültige Quarantäne-Lauf-ID: {run_id}"),
        ));
    }
    Ok(())
}

fn invalid_path(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Unsicherer Quarantänepfad: {}", path.display()),
    )
}

fn validate_record_type(record: &QuarantineRecord, entry: &Entry) -> io::Result<()> {
    if entry.is_symlink || entry.is_dir != record.is_dir || (!entry.is_dir && !entry.is_file) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Quarantäne-Eintrag änderte seinen Typ: {}",
                record.relative_path
            ),
        ));
    }
    Ok(())
}

fn read_manifest(vfs: &dyn Vfs, path: &Path) -> io::Result<QuarantineManifest> {
    read_manifest_with_digest(vfs, path).map(|(manifest, _)| manifest)
}

fn read_manifest_with_digest(
    vfs: &dyn Vfs,
    path: &Path,
) -> io::Result<(QuarantineManifest, [u8; 32])> {
    let entry = vfs.stat(path)?;
    if entry.is_symlink || !entry.is_file || entry.size > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Ungültiges Quarantäne-Manifest: {}", path.display()),
        ));
    }
    let mut data = Vec::with_capacity(entry.size as usize);
    vfs.open_read(path)?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Quarantäne-Manifest überschreitet die Größenbegrenzung: {}",
                path.display()
            ),
        ));
    }
    let manifest: QuarantineManifest = serde_json::from_slice(&data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !matches!(
        manifest.version,
        LEGACY_MANIFEST_VERSION
            | UNBOUND_RECORD_MANIFEST_VERSION
            | CONTENT_ONLY_RECORD_MANIFEST_VERSION
            | MANIFEST_VERSION
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unbekannte Quarantäne-Manifestversion {}", manifest.version),
        ));
    }
    let digest = *blake3::hash(&data).as_bytes();
    Ok((manifest, digest))
}

fn write_manifest(
    vfs: &dyn Vfs,
    path: &Path,
    manifest: &QuarantineManifest,
    create: bool,
) -> io::Result<()> {
    write_manifest_with_digest(vfs, path, manifest, create).map(|_| ())
}

fn manifest_payload(manifest: &QuarantineManifest) -> io::Result<Vec<u8>> {
    let payload = serde_json::to_vec_pretty(manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Quarantäne-Manifest überschreitet die Größenbegrenzung",
        ));
    }
    Ok(payload)
}

fn write_manifest_with_digest(
    vfs: &dyn Vfs,
    path: &Path,
    manifest: &QuarantineManifest,
    create: bool,
) -> io::Result<[u8; 32]> {
    let payload = manifest_payload(manifest)?;
    let digest = *blake3::hash(&payload).as_bytes();
    atomic_write_vfs(
        vfs,
        path,
        &payload,
        ".quarantine-manifest-",
        if create {
            AtomicWriteMode::Create
        } else {
            AtomicWriteMode::Replace
        },
    )?;
    Ok(digest)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{TEST_SANDBOX_MARKER, TEST_SANDBOX_MARKER_CONTENT};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct QuarantineFaultVfs {
        inner: LocalVfs,
        incomplete_listing: bool,
        fail_stat: Option<PathBuf>,
        fail_sync: Option<PathBuf>,
        read_override: Option<(PathBuf, Vec<u8>)>,
    }

    impl Vfs for QuarantineFaultVfs {
        fn is_local(&self) -> bool {
            self.inner.is_local()
        }

        fn metadata_manifest(&self, path: &Path) -> io::Result<crate::MetadataManifest> {
            self.inner.metadata_manifest(path)
        }

        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn list_complete(&self, path: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
            let mut listing = self.inner.list_complete(path)?;
            listing.complete = !self.incomplete_listing;
            Ok(listing)
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            if self.fail_stat.as_deref() == Some(path) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulierter nicht lesbarer Quarantäneeintrag",
                ));
            }
            self.inner.stat(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.stat(path).is_ok()
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir(path)
        }

        fn open_read(&self, path: &Path) -> io::Result<Box<dyn std::io::Read + Send>> {
            if let Some((expected, payload)) = &self.read_override {
                if expected == path {
                    return Ok(Box::new(std::io::Cursor::new(payload.clone())));
                }
            }
            self.inner.open_read(path)
        }

        fn open_write(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.inner.open_write(path)
        }

        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.inner.open_write_new(path)
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }

        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            if self.fail_sync.as_deref() == Some(path) {
                return Err(io::Error::other("simulierter Verzeichnis-Syncfehler"));
            }
            self.inner.sync_dir(path)
        }

        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }

        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_file(path)
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
        }

        fn root_identity(&self, path: &Path) -> io::Result<crate::RootIdentity> {
            if self.fail_stat.as_deref() == Some(path) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulierter nicht lesbarer Quarantänelauf",
                ));
            }
            self.inner.root_identity(path)
        }

        fn supports_restore_from_bound_quarantine_run(&self) -> bool {
            true
        }

        fn supports_purge_bound_quarantine_run_contents(&self) -> bool {
            true
        }

        fn restore_from_bound_quarantine_run(
            &self,
            request: BoundQuarantineRestoreRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.inner.restore_from_bound_quarantine_run(request)
        }

        fn replace_file_if_content_matches(
            &self,
            request: ConditionalContentReplaceRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.inner.replace_file_if_content_matches(request)
        }

        fn supports_replace_file_if_content_matches(&self) -> bool {
            self.inner.supports_replace_file_if_content_matches()
        }

        fn purge_bound_quarantine_run_contents(
            &self,
            request: BoundQuarantinePurgeRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.inner.purge_bound_quarantine_run_contents(request)
        }

        fn verify_bound_quarantine_run_empty(
            &self,
            request: BoundQuarantinePurgeRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.inner.verify_bound_quarantine_run_empty(request)
        }
    }

    /// Tauscht den sichtbaren Lauf exakt beim Eintritt in die gebundene Wirkung aus.
    /// Damit trifft der Test die Lücke zwischen Manifestprüfung und Mutation, ohne
    /// auf Scheduler-Timing angewiesen zu sein.
    struct MutationSwapVfs {
        inner: LocalVfs,
        slot: PathBuf,
        parked: PathBuf,
        replacement_file: bool,
        swap_enabled: bool,
        fail_writes_after_restore: bool,
        inject_after_purge: bool,
        vanish_purge_intent_collision: bool,
        remove_intent_before_purge: bool,
        replace_restore_manifest_before_effect: bool,
        report_local: bool,
        reconcile_source_parent: Option<PathBuf>,
        replace_reconcile_target_after_proof: bool,
        swapped: AtomicBool,
        reconcile_target_proof_seen: AtomicBool,
        mutation_finished: AtomicBool,
    }

    impl MutationSwapVfs {
        fn swap_slot_once(&self) -> io::Result<()> {
            if !self.swap_enabled {
                return Ok(());
            }
            if !self.swapped.swap(true, Ordering::SeqCst) {
                std::fs::rename(&self.slot, &self.parked)?;
                std::fs::create_dir(&self.slot)?;
                if self.replacement_file {
                    std::fs::write(self.slot.join("datei"), b"fremd")?;
                }
            }
            Ok(())
        }
    }

    impl Vfs for MutationSwapVfs {
        fn is_local(&self) -> bool {
            self.report_local
        }

        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn list_complete(&self, path: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
            if self.reconcile_source_parent.as_deref() == Some(path)
                && !self.swapped.swap(true, Ordering::SeqCst)
            {
                std::fs::rename(&self.slot, &self.parked)?;
                std::fs::create_dir(&self.slot)?;
                std::fs::rename(path.join("datei"), self.slot.join("datei"))?;
            }
            let listing = self.inner.list_complete(path)?;
            if self.replace_reconcile_target_after_proof && path == self.slot {
                self.reconcile_target_proof_seen
                    .store(true, Ordering::SeqCst);
            }
            Ok(listing)
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.inner.stat(path)
        }

        fn metadata_manifest(&self, path: &Path) -> io::Result<crate::MetadataManifest> {
            self.inner.metadata_manifest(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir(path)
        }

        fn open_read(&self, path: &Path) -> io::Result<Box<dyn std::io::Read + Send>> {
            self.inner.open_read(path)
        }

        fn open_write(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.inner.open_write(path)
        }

        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            if self.fail_writes_after_restore && self.mutation_finished.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulierter Manifestabschlussfehler",
                ));
            }
            self.inner.open_write_new(path)
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }

        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_dir(path)
        }

        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }

        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_file(path)
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
        }

        fn rename_noreplace_with_removal_proof(
            &self,
            from: &Path,
            to: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            if self.vanish_purge_intent_collision
                && to
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".purge-intent"))
            {
                std::fs::write(to, b"konflikt").map_err(MutationFailure::Unchanged)?;
                let result = self.inner.rename_noreplace_with_removal_proof(from, to);
                std::fs::remove_file(to).map_err(MutationFailure::Unchanged)?;
                return result;
            }
            self.inner.rename_noreplace_with_removal_proof(from, to)
        }

        fn root_identity(&self, path: &Path) -> io::Result<crate::RootIdentity> {
            if self.replace_reconcile_target_after_proof
                && path == self.slot
                && self.reconcile_target_proof_seen.load(Ordering::SeqCst)
                && !self.swapped.swap(true, Ordering::SeqCst)
            {
                std::fs::rename(self.slot.join("datei"), &self.parked)?;
                std::fs::write(self.slot.join("datei"), b"fremd")?;
            }
            self.inner.root_identity(path)
        }

        fn restore_from_bound_quarantine_run(
            &self,
            request: BoundQuarantineRestoreRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.swap_slot_once().map_err(MutationFailure::Unchanged)?;
            if self.replace_restore_manifest_before_effect {
                std::fs::rename(request.intent_manifest, &self.parked)
                    .map_err(MutationFailure::Unchanged)?;
                std::fs::write(request.intent_manifest, b"{}")
                    .map_err(MutationFailure::Unchanged)?;
            }
            let result = self.inner.restore_from_bound_quarantine_run(request);
            if result.is_ok() {
                self.mutation_finished.store(true, Ordering::SeqCst);
            }
            result
        }

        fn replace_file_if_content_matches(
            &self,
            request: ConditionalContentReplaceRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.inner.replace_file_if_content_matches(request)
        }

        fn supports_replace_file_if_content_matches(&self) -> bool {
            self.inner.supports_replace_file_if_content_matches()
        }

        fn purge_bound_quarantine_run_contents(
            &self,
            request: BoundQuarantinePurgeRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.swap_slot_once().map_err(MutationFailure::Unchanged)?;
            if self.remove_intent_before_purge {
                std::fs::remove_file(request.intent).map_err(MutationFailure::Unchanged)?;
            }
            let result = self.inner.purge_bound_quarantine_run_contents(request);
            if result.is_ok() && self.inject_after_purge {
                std::fs::write(self.slot.join("spaet"), b"bleibt")
                    .map_err(MutationFailure::OutcomeUnclear)?;
            }
            result
        }

        fn verify_bound_quarantine_run_empty(
            &self,
            request: BoundQuarantinePurgeRequest<'_>,
        ) -> Result<(), MutationFailure> {
            self.inner.verify_bound_quarantine_run_empty(request)
        }

        fn supports_restore_from_bound_quarantine_run(&self) -> bool {
            true
        }

        fn supports_purge_bound_quarantine_run_contents(&self) -> bool {
            true
        }
    }

    struct AppearingRunVfs {
        inner: LocalVfs,
        run: PathBuf,
        appeared: AtomicBool,
    }

    impl Vfs for AppearingRunVfs {
        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn list_complete(&self, path: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
            self.inner.list_complete(path)
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.inner.stat(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir(path)
        }

        fn open_read(&self, path: &Path) -> io::Result<Box<dyn std::io::Read + Send>> {
            self.inner.open_read(path)
        }

        fn open_write(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.inner.open_write(path)
        }

        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.inner.open_write_new(path)
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }

        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_dir(path)
        }

        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }

        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_file(path)
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
        }

        fn root_identity(&self, path: &Path) -> io::Result<crate::RootIdentity> {
            if path == self.run && !self.appeared.swap(true, Ordering::SeqCst) {
                std::fs::create_dir(&self.run)?;
                std::fs::write(self.run.join("fremd.txt"), b"fremd")?;
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "simulierter Namensnachfolger nach Abwesenheitsprüfung",
                ));
            }
            self.inner.root_identity(path)
        }

        fn supports_restore_from_bound_quarantine_run(&self) -> bool {
            true
        }

        fn supports_purge_bound_quarantine_run_contents(&self) -> bool {
            true
        }
    }

    fn fixture() -> (tempfile::TempDir, LocalVfs, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = std::fs::canonicalize(temp.path()).unwrap();
        let base = sandbox.join(".doppeldecker-quarantine");
        let run = base.join("lauf-1");
        let target = sandbox.join("ziel");
        std::fs::create_dir(&target).unwrap();
        (temp, LocalVfs::new(), sandbox, base, run)
    }

    fn record_test_file(
        vfs: &dyn Vfs,
        run: &Path,
        target: &Path,
        relative: &Path,
        reason: QuarantineReason,
        contents: &[u8],
    ) {
        let source = target.join(relative);
        let quarantined = run.join(relative);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(quarantined.parent().unwrap()).unwrap();
        std::fs::write(&source, contents).unwrap();
        record_quarantine_intent(vfs, run, relative, false, reason).unwrap();
        std::fs::rename(source, quarantined).unwrap();
    }

    fn record_test_intent(
        vfs: &dyn Vfs,
        run: &Path,
        target: &Path,
        relative: &Path,
        reason: QuarantineReason,
        contents: &[u8],
    ) {
        let source = target.join(relative);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(source, contents).unwrap();
        record_quarantine_intent(vfs, run, relative, false, reason).unwrap();
    }

    #[test]
    fn manifest_liste_zeigt_nur_tatsaechlich_verschobene_intents() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir_all(run.join("unter")).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("unter/alt.txt"),
            QuarantineReason::Replaced,
            b"alt",
        );
        record_test_intent(
            &vfs,
            &run,
            &target,
            Path::new("noch-nicht.txt"),
            QuarantineReason::MirrorRemoval,
            b"noch nicht verschoben",
        );
        complete_quarantine_run(&vfs, &run).unwrap();

        let runs = list_quarantine_runs(&vfs, &base, &Some(sandbox.clone())).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].items_total, 2);
        assert_eq!(runs[0].items_available, 1);
        assert_eq!(runs[0].bytes_available, 3);
        assert_eq!(runs[0].state, QuarantineState::Complete);

        let entries = list_quarantine_entries(&vfs, &base, "lauf-1", &Some(sandbox)).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].available || entries[1].available);
    }

    #[test]
    fn unvollstaendige_laufliste_wird_nicht_als_vollstaendig_ausgegeben() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let _lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        let incomplete = QuarantineFaultVfs {
            inner: vfs,
            incomplete_listing: true,
            fail_stat: None,
            fail_sync: None,
            read_override: None,
        };

        let error = list_quarantine_runs(&incomplete, &base, &Some(sandbox)).expect_err(
            "unvollständiges Listing darf keinen vollständigen Laufbestand vortäuschen",
        );

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn laufliste_verschweigt_keinen_nicht_lesbaren_manifestierten_eintrag() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let _lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"wichtig",
        );
        let unreadable = QuarantineFaultVfs {
            inner: vfs,
            incomplete_listing: false,
            fail_stat: Some(run.join("datei")),
            fail_sync: None,
            read_override: None,
        };

        let error = list_quarantine_runs(&unreadable, &base, &Some(sandbox))
            .expect_err("ein Lesefehler darf nicht als nicht verfügbar bilanziert werden");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn laufliste_bindet_manifest_an_aktuelle_provideridentitaet() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let _lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        let path = manifest_path(&run).unwrap();
        let mut manifest = read_manifest(&vfs, &path).unwrap();
        manifest.provider_identity_digest = [9; 32];
        write_manifest(&vfs, &path, &manifest, false).unwrap();

        let error = list_quarantine_runs(&vfs, &base, &Some(sandbox))
            .expect_err("ein Lauf einer anderen Provideridentität darf nicht sichtbar werden");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn purge_bewahrt_sidecar_wenn_laufroot_nicht_geprueft_werden_kann() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"wichtig",
        );
        drop(lease);
        let sidecar = manifest_path(&run).unwrap();
        let unreadable = QuarantineFaultVfs {
            inner: vfs,
            incomplete_listing: false,
            fail_stat: Some(run.clone()),
            fail_sync: None,
            read_override: None,
        };

        let error = purge_quarantine_run(&unreadable, &base, "lauf-1", &Some(sandbox))
            .expect_err("ein ungeklärter Laufroot darf sein Recoverymanifest nicht verlieren");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(sidecar.exists());
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"wichtig");
    }

    #[test]
    fn manifestleser_begrenzt_auch_den_tatsaechlichen_datenstrom() {
        let (_temp, vfs, sandbox, _base, run) = fixture();
        let target = sandbox.join("ziel");
        let _lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        let path = manifest_path(&run).unwrap();
        let oversized = QuarantineManifest {
            version: MANIFEST_VERSION,
            run_id: "lauf-1".into(),
            created_unix_ms: 0,
            target_root: target,
            state: QuarantineState::Active,
            provider_identity_digest: [1; 32],
            run_identity_digest: [1; 32],
            run_identity_pending: false,
            records: vec![QuarantineRecord {
                relative_path: "x".repeat(MAX_MANIFEST_BYTES as usize),
                is_dir: false,
                reason: QuarantineReason::Replaced,
                source_identity: Some(QuarantineRestoreIdentity::Local {
                    device: 1,
                    inode: 1,
                }),
                source_digest: Some([1; 32]),
                source_digest_version: SOURCE_DIGEST_VERSION,
                restored: false,
                restore_intent: None,
            }],
        };
        let payload = serde_json::to_vec(&oversized).unwrap();
        assert!(payload.len() as u64 > MAX_MANIFEST_BYTES);
        let oversized_stream = QuarantineFaultVfs {
            inner: vfs,
            incomplete_listing: false,
            fail_stat: None,
            fail_sync: None,
            read_override: Some((path.clone(), payload)),
        };

        let error = read_manifest(&oversized_stream, &path)
            .expect_err("der tatsächliche Lesestrom muss dieselbe Grenze wie stat einhalten");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("Größenbegrenzung"));
    }

    #[test]
    fn scope_verlangt_marker_und_bindet_sandbox_root() {
        let (_temp, vfs, sandbox, _base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();

        assert!(QuarantineScope::for_test_sandbox(&sandbox).is_err());
        std::fs::write(
            sandbox.join(TEST_SANDBOX_MARKER),
            TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();

        let scope = QuarantineScope::for_test_sandbox(&sandbox).unwrap();
        assert_eq!(scope.runs().unwrap().len(), 1);
        assert!(scope.require_run("lauf-1").is_ok());
    }

    #[test]
    fn target_scope_zeigt_nur_exakt_gebundene_lauefe() {
        let temp = tempfile::tempdir().unwrap();
        let parent = std::fs::canonicalize(temp.path()).unwrap();
        let first = parent.join("erstes-ziel");
        let second = parent.join("zweites-ziel");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let base = parent.join(".doppeldecker-quarantine");
        let vfs = LocalVfs::new();
        start_quarantine_run(&vfs, &base.join("lauf-a"), &first).unwrap();
        start_quarantine_run(&vfs, &base.join("lauf-b"), &second).unwrap();

        let scope = QuarantineScope::for_target(&first).unwrap();
        let runs = scope.runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "lauf-a");
        assert!(scope.require_run("lauf-b").is_err());
    }

    #[test]
    fn target_scope_validiert_keinen_nicht_erreichbaren_geschwisterlauf() {
        let temp = tempfile::tempdir().unwrap();
        let parent = std::fs::canonicalize(temp.path()).unwrap();
        let first = parent.join("erstes-ziel");
        let second = parent.join("zweites-ziel");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let base = parent.join(".doppeldecker-quarantine");
        let vfs = LocalVfs::new();
        start_quarantine_run(&vfs, &base.join("lauf-a"), &first).unwrap();
        start_quarantine_run(&vfs, &base.join("lauf-b"), &second).unwrap();
        std::fs::remove_dir(&second).unwrap();

        let runs = QuarantineScope::for_target(&first).unwrap().runs().unwrap();

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "lauf-a");
    }

    #[test]
    fn interner_restore_nutzt_staging_ohne_original_zu_ueberschreiben() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("knoten"),
            QuarantineReason::Replaced,
            b"alt",
        );
        std::fs::write(target.join("knoten"), b"neu").unwrap();

        let restored = restore_quarantine_entry_to_for_target(
            &vfs,
            &base,
            "lauf-1",
            "knoten",
            Path::new(".staging-knoten"),
            &Some(sandbox),
            Some(&target),
        )
        .unwrap();

        assert_eq!(std::fs::read(target.join("knoten")).unwrap(), b"neu");
        assert_eq!(restored.outcome, QuarantineRestoreOutcome::Restored);
        assert_eq!(std::fs::read(restored.path).unwrap(), b"alt");
    }

    #[test]
    fn wiederherstellung_legt_eltern_an_und_ueberschreibt_nicht() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir_all(run.join("unter")).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("unter/alt.txt"),
            QuarantineReason::Replaced,
            b"wichtig",
        );

        let restored = restore_quarantine_entry(
            &vfs,
            &base,
            "lauf-1",
            "unter/alt.txt",
            &Some(sandbox.clone()),
        )
        .unwrap();
        assert_eq!(restored.outcome, QuarantineRestoreOutcome::Restored);
        assert_eq!(restored.path, target.join("unter/alt.txt"));
        assert_eq!(std::fs::read(&restored.path).unwrap(), b"wichtig");

        std::fs::write(run.join("unter/alt.txt"), b"zweite Version").unwrap();
        assert!(
            restore_quarantine_entry(&vfs, &base, "lauf-1", "unter/alt.txt", &Some(sandbox),)
                .is_err()
        );
        assert_eq!(std::fs::read(restored.path).unwrap(), b"wichtig");
    }

    #[test]
    fn restore_lehnt_inhaltliche_aenderung_nach_dem_quarantaene_claim_ab() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"bestaetigt",
        );
        // Derselbe Inode bleibt sichtbar; nur der zusätzliche Content-Digest kann
        // diese in-place Änderung vom manifestierten Claim unterscheiden.
        std::fs::write(run.join("datei"), b"nachtraeglich veraendert").unwrap();

        let error =
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox)).unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert!(!target.join("datei").exists());
        assert_eq!(
            std::fs::read(run.join("datei")).unwrap(),
            b"nachtraeglich veraendert"
        );
    }

    #[test]
    fn restore_lehnt_metadatenaenderung_desselben_inodes_ab() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"unveraendert",
        );
        let quarantined = run.join("datei");
        let inode = std::fs::metadata(&quarantined).unwrap().ino();
        std::fs::set_permissions(&quarantined, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::metadata(&quarantined).unwrap().ino(), inode);

        let error =
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox)).unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert!(!target.join("datei").exists());
        assert_eq!(std::fs::read(quarantined).unwrap(), b"unveraendert");
    }

    #[test]
    fn restore_lehnt_geaenderten_zeitstempel_desselben_inodes_ab() {
        use std::os::unix::fs::MetadataExt;

        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"unveraendert",
        );
        let quarantined = run.join("datei");
        let before = vfs.stat(&quarantined).unwrap();
        let inode = std::fs::metadata(&quarantined).unwrap().ino();
        vfs.set_mtime(
            &quarantined,
            before.mtime.expect("lokale Datei braucht eine mtime") - 120,
        )
        .unwrap();
        assert_eq!(std::fs::metadata(&quarantined).unwrap().ino(), inode);
        assert_ne!(vfs.stat(&quarantined).unwrap().mtime, before.mtime);

        let error =
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox)).unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert!(!target.join("datei").exists());
        assert_eq!(std::fs::read(quarantined).unwrap(), b"unveraendert");
    }

    /// Ein vor der Zeitstempel-Erweiterung geschriebener Claim trägt
    /// `source_digest_version: 2`. Sein Digest entstand ohne Zeitstempel, deshalb muss
    /// die Version-2-Berechnung unverändert bleiben und der Eintrag automatisch
    /// wiederherstellbar sein. Ohne die getrennte Version 3 hätte jedes vorhandene
    /// Manifest hier `plan_drift` gemeldet und die Datei wäre liegen geblieben.
    #[test]
    fn alter_version_zwei_claim_bleibt_automatisch_wiederherstellbar() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"historisch",
        );
        complete_quarantine_run(&vfs, &run).unwrap();
        drop(lease);

        // Das Manifest auf den Stand vor der Digeständerung zurückschreiben: gleiche
        // Identität, aber der Digest der alten Version ohne Zeitstempel.
        let quarantined = run.join("datei");
        let alter_digest =
            *blake3::hash(&historische_version_zwei_bytes(&vfs, &quarantined)).as_bytes();
        let sidecar = manifest_path(&run).unwrap();
        let mut manifest = read_manifest(&vfs, &sidecar).unwrap();
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_ne!(
            manifest.records[0].source_digest,
            Some(alter_digest),
            "Version 2 und Version 3 müssen sich unterscheiden"
        );
        manifest.records[0].source_digest = Some(alter_digest);
        manifest.records[0].source_digest_version = METADATA_SOURCE_DIGEST_VERSION;
        write_manifest(&vfs, &sidecar, &manifest, false).unwrap();

        let entries =
            list_quarantine_entries(&vfs, &base, "lauf-1", &Some(sandbox.clone())).unwrap();
        assert!(entries[0].automatic_restore_available);
        assert!(!entries[0].manual_recovery_required);

        restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox)).unwrap();

        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"historisch");
        assert!(!quarantined.exists());
    }

    /// Baut die Version-2-Digestbytes unabhängig vom Produktivcode noch einmal auf.
    /// Nur so belegen die Tests, dass ein vor der Zeitstempel-Erweiterung
    /// geschriebener Claim heute noch denselben Digest ergibt.
    fn historische_version_zwei_bytes(vfs: &dyn Vfs, path: &Path) -> Vec<u8> {
        #[derive(Serialize)]
        struct HistorischerRecord {
            relative_components: Vec<Vec<u8>>,
            is_dir: bool,
            size: u64,
            content_digest: Option<[u8; 32]>,
            #[serde(skip_serializing_if = "Option::is_none")]
            metadata_digest: Option<[u8; 32]>,
            #[serde(skip_serializing_if = "Option::is_none")]
            mode: Option<u32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            owner: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            group: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            hardlink_identity: Option<(u64, u64, u64)>,
        }

        let proof = listed_entry_proof(vfs, path).unwrap();
        assert!(proof.entry.is_file, "Helfer deckt nur einzelne Dateien ab");
        let metadata = vfs.metadata_manifest(path).unwrap();
        metadata.validate().unwrap();
        let record = HistorischerRecord {
            relative_components: Vec::new(),
            is_dir: false,
            size: proof.entry.size,
            content_digest: Some(
                *crate::atomic_file::hash_vfs_file(vfs, path)
                    .unwrap()
                    .as_bytes(),
            ),
            metadata_digest: Some(metadata.digest),
            mode: proof.entry.mode,
            owner: proof.entry.owner.clone(),
            group: proof.entry.group.clone(),
            hardlink_identity: Some((
                proof.entry.device.unwrap(),
                proof.entry.inode.unwrap(),
                proof.entry.hard_links.unwrap(),
            )),
        };
        serde_json::to_vec(&("quarantine_source_v2", vec![record])).unwrap()
    }

    /// Hält die Version-2-Serialisierung bytegenau fest. Sobald jemand dem gemeinsamen
    /// Digest-Record ein Feld hinzufügt, ohne es auf eine neue Version zu beschränken,
    /// schlägt dieser Test fehl — genau der Fall, der vorhandene Manifeste unrettbar
    /// gemacht hat.
    #[test]
    fn version_zwei_digestpayload_bleibt_ohne_zeitstempel() {
        let (_temp, vfs, sandbox, _base, _run) = fixture();
        let datei = sandbox.join("ziel").join("datei");
        std::fs::write(&datei, b"inhalt").unwrap();
        let proof = listed_entry_proof(&vfs, &datei).unwrap();

        let version_zwei =
            quarantine_digest_payload(&vfs, &datei, &proof, METADATA_SOURCE_DIGEST_VERSION)
                .unwrap();
        assert_eq!(
            String::from_utf8(version_zwei.clone()).unwrap(),
            String::from_utf8(historische_version_zwei_bytes(&vfs, &datei)).unwrap()
        );

        // Version 3 trägt dieselben Felder plus den Zeitstempelcontainer.
        let version_drei =
            quarantine_digest_payload(&vfs, &datei, &proof, SOURCE_DIGEST_VERSION).unwrap();
        let text_drei = String::from_utf8(version_drei).unwrap();
        assert!(text_drei.starts_with("[\"quarantine_source_v3\""));
        assert!(text_drei.contains("\"timestamps\""));
        assert_ne!(version_zwei, text_drei.into_bytes());
    }

    #[test]
    fn restore_lehnt_geaenderte_hardlinktopologie_desselben_inodes_ab() {
        use std::os::unix::fs::MetadataExt;

        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"unveraendert",
        );
        let quarantined = run.join("datei");
        let inode = std::fs::metadata(&quarantined).unwrap().ino();
        std::fs::hard_link(&quarantined, run.join("zusaetzlicher-link")).unwrap();
        let metadata = std::fs::metadata(&quarantined).unwrap();
        assert_eq!(metadata.ino(), inode);
        assert_eq!(metadata.nlink(), 2);

        let error =
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox)).unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert!(!target.join("datei").exists());
        assert_eq!(std::fs::read(quarantined).unwrap(), b"unveraendert");
    }

    #[test]
    fn version_zwei_bleibt_physisch_sichtbar_aber_nur_manuell_rettbar() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"historisch",
        );
        drop(lease);
        let sidecar = manifest_path(&run).unwrap();
        let mut legacy = read_manifest(&vfs, &sidecar).unwrap();
        legacy.version = UNBOUND_RECORD_MANIFEST_VERSION;
        legacy.records[0].source_identity = None;
        legacy.records[0].source_digest = None;
        legacy.records[0].source_digest_version = LEGACY_SOURCE_DIGEST_VERSION;
        write_manifest(&vfs, &sidecar, &legacy, false).unwrap();

        let runs = list_quarantine_runs(&vfs, &base, &Some(sandbox.clone())).unwrap();
        assert_eq!(runs[0].items_available, 1);
        assert_eq!(runs[0].items_restorable, 0);
        assert_eq!(runs[0].items_manual_recovery, 1);
        let entries =
            list_quarantine_entries(&vfs, &base, "lauf-1", &Some(sandbox.clone())).unwrap();
        assert!(entries[0].available);
        assert!(!entries[0].automatic_restore_available);
        assert!(entries[0].manual_recovery_required);
        assert!(restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox)).is_err());
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"historisch");
    }

    #[test]
    fn lebender_lauf_kann_nicht_parallel_wiederhergestellt_werden() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let _lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"wichtig",
        );

        let error =
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox.clone()))
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(crate::error_code(&error), ErrorCode::QuarantineBusy);
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"wichtig");
        assert!(!target.join("datei").exists());

        let purge_error = purge_quarantine_run(&vfs, &base, "lauf-1", &Some(sandbox)).unwrap_err();
        assert_eq!(crate::error_code(&purge_error), ErrorCode::QuarantineBusy);
        assert!(run.exists());
    }

    #[test]
    fn active_ohne_live_lease_ist_sichtbarer_crash_recoveryfall() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();

        let live = list_quarantine_runs(&vfs, &base, &Some(sandbox.clone())).unwrap();
        assert_eq!(live[0].status, QuarantineRunStatus::Live);
        drop(lease);

        let crashed = list_quarantine_runs(&vfs, &base, &Some(sandbox.clone())).unwrap();
        assert_eq!(crashed[0].state, QuarantineState::Active);
        assert_eq!(crashed[0].status, QuarantineRunStatus::CrashRecovery);

        let recovery_lease = lease_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        complete_quarantine_run(&vfs, &run).unwrap();
        assert_eq!(
            list_quarantine_runs(&vfs, &base, &Some(sandbox.clone())).unwrap()[0].status,
            QuarantineRunStatus::Live
        );
        drop(recovery_lease);
        assert_eq!(
            list_quarantine_runs(&vfs, &base, &Some(sandbox)).unwrap()[0].status,
            QuarantineRunStatus::Complete
        );
    }

    #[test]
    fn anderes_fenster_kann_lebenden_lauf_weder_restoren_noch_leeren() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let _lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );

        let other_vfs = vfs.clone();
        let other_base = base.clone();
        let other_sandbox = sandbox.clone();
        let other = std::thread::spawn(move || {
            let restore = restore_quarantine_entry(
                &other_vfs,
                &other_base,
                "lauf-1",
                "datei",
                &Some(other_sandbox.clone()),
            )
            .unwrap_err();
            let purge =
                purge_quarantine_run(&other_vfs, &other_base, "lauf-1", &Some(other_sandbox))
                    .unwrap_err();
            (crate::error_code(&restore), crate::error_code(&purge))
        });

        assert_eq!(
            other.join().unwrap(),
            (ErrorCode::QuarantineBusy, ErrorCode::QuarantineBusy)
        );
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"alt");
        assert!(!target.join("datei").exists());
    }

    #[test]
    fn remote_leases_trennen_provideridentitaet_root_und_lauf_id() {
        use crate::real_mode::RootIdentity;
        use crate::remote::{SftpHostKeyPin, SftpRootIdentity, SftpServerIdentity};

        fn identity(host_key: u8, virtual_root: &str, generation: u64) -> RootIdentity {
            RootIdentity::Sftp(SftpRootIdentity {
                server: SftpServerIdentity {
                    host: "example.invalid".into(),
                    port: 22,
                    host_key: SftpHostKeyPin {
                        algorithm: "ssh-ed25519".into(),
                        sha256: [host_key; 32],
                    },
                    canonical_root: "/".into(),
                    sftp_version: 3,
                    extensions_digest: [7; 32],
                },
                virtual_root: virtual_root.into(),
                session_generation: generation,
            })
        }

        let first = stable_provider_identity_digest(&identity(1, "/a", 1)).unwrap();
        let reconnect = stable_provider_identity_digest(&identity(1, "/b", 2)).unwrap();
        let other_provider = stable_provider_identity_digest(&identity(2, "/a", 1)).unwrap();
        assert_eq!(
            first, reconnect,
            "Root und Reconnect sind kein Providerwechsel"
        );
        assert_ne!(first, other_provider);

        let root = PathBuf::from("/pane-root");
        let held = acquire_lease(QuarantineLeaseKey {
            provider_identity_digest: first,
            target_root: root.clone(),
            run_id: "lauf".into(),
        })
        .unwrap();
        let same = acquire_lease(QuarantineLeaseKey {
            provider_identity_digest: first,
            target_root: root.clone(),
            run_id: "lauf".into(),
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&same), ErrorCode::QuarantineBusy);
        let other_root = acquire_lease(QuarantineLeaseKey {
            provider_identity_digest: first,
            target_root: PathBuf::from("/anderer-root"),
            run_id: "lauf".into(),
        })
        .unwrap();
        let other_run = acquire_lease(QuarantineLeaseKey {
            provider_identity_digest: first,
            target_root: root.clone(),
            run_id: "anderer-lauf".into(),
        })
        .unwrap();
        let other_provider = acquire_lease(QuarantineLeaseKey {
            provider_identity_digest: other_provider,
            target_root: root,
            run_id: "lauf".into(),
        })
        .unwrap();
        drop((held, other_root, other_run, other_provider));
    }

    #[test]
    fn manipuliertes_ziel_ausserhalb_der_sandbox_wird_abgelehnt() {
        let (temp, vfs, sandbox, base, run) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let outside_root = std::fs::canonicalize(outside.path()).unwrap();
        start_quarantine_run(&vfs, &run, &outside_root).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &outside_root,
            Path::new("datei"),
            QuarantineReason::MirrorRemoval,
            b"innen",
        );

        assert!(list_quarantine_runs(&vfs, &base, &Some(sandbox)).is_err());
        assert!(!outside.path().join("datei").exists());
        drop(temp);
    }

    #[test]
    fn endgueltiges_leeren_leert_den_lauf_und_behaelt_einen_sichtbaren_tombstone() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::MirrorRemoval,
            b"weg",
        );

        purge_quarantine_run(&vfs, &base, "lauf-1", &Some(sandbox.clone())).unwrap();
        assert!(run.is_dir());
        assert!(std::fs::read_dir(&run).unwrap().next().is_none());
        assert!(base.join("lauf-1.json").exists());
        assert!(base.join("lauf-1.purge-intent").exists());
        let runs = list_quarantine_runs(&vfs, &base, &None).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, QuarantineRunStatus::Purged);
        assert_eq!(runs[0].items_available, 0);
        let restore =
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox)).unwrap_err();
        assert_eq!(restore.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn restore_mutiert_keinen_nach_der_manifestpruefung_eingesetzten_lauf() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        drop(lease);
        let parked = base.join("urspruenglicher-lauf");
        let swapping = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: parked.clone(),
            replacement_file: true,
            swap_enabled: true,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = restore_quarantine_entry(&swapping, &base, "lauf-1", "datei", &Some(sandbox))
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(parked.join("datei")).unwrap(), b"alt");
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"fremd");
        assert!(!target.join("datei").exists());
        assert!(base.join("lauf-1.json").exists());
    }

    #[test]
    fn purge_mutiert_keinen_nach_der_manifestpruefung_eingesetzten_lauf() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::MirrorRemoval,
            b"alt",
        );
        drop(lease);
        let parked = base.join("urspruenglicher-lauf");
        let swapping = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: parked.clone(),
            replacement_file: true,
            swap_enabled: true,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = purge_quarantine_run(&swapping, &base, "lauf-1", &Some(sandbox)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(parked.join("datei")).unwrap(), b"alt");
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"fremd");
        assert!(base.join("lauf-1.json").exists());
        assert!(base.join("lauf-1.purge-intent").exists());
    }

    #[test]
    fn restore_mutiert_keinen_unmittelbar_vor_der_wirkung_ausgetauschten_zielroot() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        drop(lease);
        let parked_target = sandbox.join("urspruengliches-ziel");
        let swapping = MutationSwapVfs {
            inner: vfs,
            slot: target.clone(),
            parked: parked_target.clone(),
            replacement_file: false,
            swap_enabled: true,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = restore_quarantine_entry(&swapping, &base, "lauf-1", "datei", &Some(sandbox))
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"alt");
        assert!(!target.join("datei").exists());
        assert!(!parked_target.join("datei").exists());
        assert!(base.join("lauf-1.json").exists());
    }

    #[test]
    fn restore_manifestfehler_nach_rename_ist_outcome_unclear() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        drop(lease);
        let failing = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: base.join("unbenutzt"),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: true,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error =
            restore_quarantine_entry(&failing, &base, "lauf-1", "datei", &Some(sandbox.clone()))
                .unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"alt");
        assert!(!run.join("datei").exists());
        let interrupted = read_manifest(&failing, &base.join("lauf-1.json")).unwrap();
        assert!(!interrupted.records[0].restored);
        assert!(interrupted.records[0].restore_intent.is_some());
        let purge_error =
            purge_quarantine_run(&failing.inner, &base, "lauf-1", &Some(sandbox.clone()))
                .unwrap_err();
        assert_eq!(crate::error_code(&purge_error), ErrorCode::PermissionDenied);
        assert!(!base.join("lauf-1.purge-intent").exists());
        std::fs::remove_dir(&run).unwrap();
        let runs = list_quarantine_runs(&failing.inner, &base, &Some(sandbox.clone())).unwrap();
        assert_eq!(runs[0].items_recovery_pending, 1);
        let listed =
            list_quarantine_entries(&failing.inner, &base, "lauf-1", &Some(sandbox.clone()))
                .unwrap();
        assert!(!listed[0].available);
        assert!(listed[0].recovery_pending);

        let restored =
            restore_quarantine_entry(&failing.inner, &base, "lauf-1", "datei", &Some(sandbox))
                .unwrap();

        assert_eq!(restored.outcome, QuarantineRestoreOutcome::Reconciled);
        assert_eq!(restored.path, target.join("datei"));
        assert_eq!(std::fs::read(&restored.path).unwrap(), b"alt");
        let completed = read_manifest(&failing.inner, &base.join("lauf-1.json")).unwrap();
        assert!(completed.records[0].restored);
        assert!(completed.records[0].restore_intent.is_some());
    }

    #[test]
    fn remote_restore_ohne_manifest_cas_stoppt_vor_intent_und_rename() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        drop(lease);
        let remote_without_cas = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: base.join("unbenutzt"),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: false,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = restore_quarantine_entry(
            &remote_without_cas,
            &base,
            "lauf-1",
            "datei",
            &Some(sandbox),
        )
        .unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::Unsupported);
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"alt");
        assert!(!target.join("datei").exists());
        assert!(
            read_manifest(&remote_without_cas.inner, &base.join("lauf-1.json"))
                .unwrap()
                .records[0]
                .restore_intent
                .is_none()
        );
    }

    #[test]
    fn restore_bindet_persistierten_intent_bis_in_die_rename_wirkung() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        drop(lease);
        let parked_manifest = base.join("manifest-vor-austausch");
        let swapping = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: parked_manifest.clone(),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: true,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = restore_quarantine_entry(&swapping, &base, "lauf-1", "datei", &Some(sandbox))
            .unwrap_err();

        assert_ne!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"alt");
        assert!(!target.join("datei").exists());
        assert!(parked_manifest.exists());
    }

    #[test]
    fn restore_recovery_akzeptiert_keinen_fremden_nachfolger_am_ziel() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        drop(lease);
        let failing = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: base.join("unbenutzt"),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: true,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };
        let first =
            restore_quarantine_entry(&failing, &base, "lauf-1", "datei", &Some(sandbox.clone()))
                .unwrap_err();
        assert_eq!(crate::error_code(&first), ErrorCode::OutcomeUnclear);
        std::fs::remove_file(target.join("datei")).unwrap();
        std::fs::write(target.join("datei"), b"fremd").unwrap();

        let retry =
            restore_quarantine_entry(&failing.inner, &base, "lauf-1", "datei", &Some(sandbox))
                .unwrap_err();

        assert_eq!(crate::error_code(&retry), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"fremd");
        assert!(
            !read_manifest(&failing.inner, &base.join("lauf-1.json"))
                .unwrap()
                .records[0]
                .restored
        );
    }

    #[test]
    fn restore_recovery_bleibt_bei_fehlendem_durability_beleg_offen() {
        for failed_directory in [
            "ziel/unter/tief",
            "ziel/unter",
            "ziel",
            "lauf/unter/tief",
            "lauf",
            "basis",
        ] {
            let (_temp, vfs, sandbox, base, run) = fixture();
            let target = sandbox.join("ziel");
            let relative = Path::new("unter/tief/datei");
            let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
            std::fs::create_dir_all(run.join("unter/tief")).unwrap();
            bind_quarantine_run_identity(&vfs, &run).unwrap();
            record_test_file(
                &vfs,
                &run,
                &target,
                relative,
                QuarantineReason::Replaced,
                b"bleibt",
            );
            drop(lease);
            // Dauerhafter Intent, danach Rename ohne Verzeichnis-fsync: genau der
            // Zwischenstand eines Abbruchs innerhalb der ursprünglichen Wirkung.
            let sidecar = manifest_path(&run).unwrap();
            let mut manifest = read_manifest(&vfs, &sidecar).unwrap();
            manifest.records[0].restore_intent = Some(QuarantineRestoreIntent {
                destination_relative: "unter/tief/datei".into(),
                source_is_dir: false,
                source_identity: manifest.records[0].source_identity.clone().unwrap(),
            });
            write_manifest(&vfs, &sidecar, &manifest, false).unwrap();
            std::fs::rename(run.join(relative), target.join(relative)).unwrap();
            let failed = match failed_directory {
                "basis" => base.clone(),
                name if name.starts_with("lauf") => {
                    run.join(name.strip_prefix("lauf").unwrap().trim_start_matches('/'))
                }
                name => sandbox.join(name),
            };
            let failing = QuarantineFaultVfs {
                inner: vfs,
                incomplete_listing: false,
                fail_stat: None,
                fail_sync: Some(failed),
                read_override: None,
            };
            let error = restore_quarantine_entry(
                &failing,
                &base,
                "lauf-1",
                "unter/tief/datei",
                &Some(sandbox.clone()),
            )
            .unwrap_err();
            assert_eq!(
                crate::error_code(&error),
                ErrorCode::OutcomeUnclear,
                "{failed_directory}"
            );
            assert!(error
                .to_string()
                .contains("simulierter Verzeichnis-Syncfehler"));
            assert!(!read_manifest(&failing.inner, &sidecar).unwrap().records[0].restored);
            assert_eq!(std::fs::read(target.join(relative)).unwrap(), b"bleibt");
            assert!(!run.join(relative).exists());
            let result = restore_quarantine_entry(
                &failing.inner,
                &base,
                "lauf-1",
                "unter/tief/datei",
                &Some(sandbox),
            )
            .unwrap();
            assert_eq!(result.outcome, QuarantineRestoreOutcome::Reconciled);
        }
    }

    #[test]
    fn restore_recovery_markiert_keinen_eintrag_in_ausgetauschtem_zielroot() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        std::fs::write(target.join("datei"), b"kollision").unwrap();
        drop(lease);
        let collision =
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox.clone()))
                .unwrap_err();
        assert_eq!(collision.kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_file(target.join("datei")).unwrap();
        let parked_target = sandbox.join("urspruengliches-ziel");
        let swapping = MutationSwapVfs {
            inner: vfs,
            slot: target.clone(),
            parked: parked_target.clone(),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: Some(run.clone()),
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = restore_quarantine_entry(&swapping, &base, "lauf-1", "datei", &Some(sandbox))
            .unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"alt");
        assert!(!parked_target.join("datei").exists());
        assert!(
            !read_manifest(&swapping.inner, &base.join("lauf-1.json"))
                .unwrap()
                .records[0]
                .restored
        );
    }

    #[test]
    fn restore_recovery_markiert_keinen_ausgetauschten_zieleintrag() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        std::fs::write(target.join("datei"), b"kollision").unwrap();
        drop(lease);
        restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox.clone()))
            .unwrap_err();
        std::fs::remove_file(target.join("datei")).unwrap();
        std::fs::rename(run.join("datei"), target.join("datei")).unwrap();
        let parked_entry = sandbox.join("bestaetigter-zieleintrag");
        let swapping = MutationSwapVfs {
            inner: vfs,
            slot: target.clone(),
            parked: parked_entry.clone(),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: true,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = restore_quarantine_entry(&swapping, &base, "lauf-1", "datei", &Some(sandbox))
            .unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"fremd");
        assert_eq!(std::fs::read(parked_entry).unwrap(), b"alt");
        assert!(
            !read_manifest(&swapping.inner, &base.join("lauf-1.json"))
                .unwrap()
                .records[0]
                .restored
        );
    }

    #[test]
    fn spaeter_eintrag_nach_leerheitsbeleg_bleibt_als_purge_recovery_sichtbar() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::MirrorRemoval,
            b"weg",
        );
        drop(lease);
        let late = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: base.join("unbenutzt"),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: false,
            inject_after_purge: true,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = purge_quarantine_run(&late, &base, "lauf-1", &Some(sandbox)).unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(run.join("spaet")).unwrap(), b"bleibt");
        let runs = list_quarantine_runs(&late, &base, &None).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, QuarantineRunStatus::PurgeRecovery);
    }

    #[test]
    fn verschwundener_purge_intent_konflikt_autorisiert_keine_wirkung() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::MirrorRemoval,
            b"bleibt",
        );
        drop(lease);
        let colliding = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: base.join("unbenutzt"),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: true,
            remove_intent_before_purge: false,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error = purge_quarantine_run(&colliding, &base, "lauf-1", &Some(sandbox)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"bleibt");
        assert!(!base.join("lauf-1.purge-intent").exists());
    }

    #[test]
    fn vor_der_gebundenen_wirkung_entfernter_purge_intent_stoppt_das_leeren() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::MirrorRemoval,
            b"bleibt",
        );
        drop(lease);
        let disappearing = MutationSwapVfs {
            inner: vfs,
            slot: run.clone(),
            parked: base.join("unbenutzt"),
            replacement_file: false,
            swap_enabled: false,
            fail_writes_after_restore: false,
            inject_after_purge: false,
            vanish_purge_intent_collision: false,
            remove_intent_before_purge: true,
            replace_restore_manifest_before_effect: false,
            report_local: true,
            reconcile_source_parent: None,
            replace_reconcile_target_after_proof: false,
            swapped: AtomicBool::new(false),
            reconcile_target_proof_seen: AtomicBool::new(false),
            mutation_finished: AtomicBool::new(false),
        };

        let error =
            purge_quarantine_run(&disappearing, &base, "lauf-1", &Some(sandbox)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"bleibt");
        assert!(!base.join("lauf-1.purge-intent").exists());
    }

    #[test]
    fn wegbenannter_gebundener_lauf_gilt_nicht_als_geleert() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::MirrorRemoval,
            b"bleibt",
        );
        drop(lease);
        let parked = base.join("wegbenannter-lauf");
        std::fs::rename(&run, &parked).unwrap();

        let error =
            purge_quarantine_run(&vfs, &base, "lauf-1", &Some(sandbox.clone())).unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(parked.join("datei")).unwrap(), b"bleibt");
        assert!(!base.join("lauf-1.purge-intent").exists());
        let runs = list_quarantine_runs(&vfs, &base, &Some(sandbox)).unwrap();
        assert_eq!(runs[0].status, QuarantineRunStatus::CrashRecovery);
    }

    #[test]
    fn legacy_manifest_autorisiert_weder_liste_restore_noch_purge() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"historisch",
        );
        drop(lease);

        let sidecar = manifest_path(&run).unwrap();
        let mut legacy = read_manifest(&vfs, &sidecar).unwrap();
        legacy.version = LEGACY_MANIFEST_VERSION;
        legacy.run_identity_digest = [0; 32];
        legacy.run_identity_pending = false;
        write_manifest(&vfs, &sidecar, &legacy, false).unwrap();
        assert_eq!(read_manifest(&vfs, &sidecar).unwrap().version, 1);

        assert!(list_quarantine_runs(&vfs, &base, &Some(sandbox.clone())).is_err());
        assert!(list_quarantine_entries(&vfs, &base, "lauf-1", &Some(sandbox.clone())).is_err());
        assert!(
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox.clone()),)
                .is_err()
        );
        assert!(purge_quarantine_run(&vfs, &base, "lauf-1", &Some(sandbox)).is_err());
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"historisch");
        assert!(!target.join("datei").exists());
        assert!(sidecar.exists());
    }

    #[test]
    fn pending_manifest_nimmt_keinen_intent_auf() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();

        let error = record_quarantine_intent(
            &vfs,
            &run,
            Path::new("datei"),
            false,
            QuarantineReason::Replaced,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(read_manifest(&vfs, &manifest_path(&run).unwrap())
            .unwrap()
            .records
            .is_empty());
        drop(lease);
        purge_quarantine_run(&vfs, &base, "lauf-1", &Some(sandbox.clone())).unwrap();
        assert!(!run.exists());
        assert!(base.join("lauf-1.purge-intent").exists());
        let runs = list_quarantine_runs(&vfs, &base, &Some(sandbox)).unwrap();
        assert_eq!(runs[0].status, QuarantineRunStatus::Purged);
    }

    #[test]
    fn pending_start_adoptiert_keinen_manifestlosen_fremdordner() {
        let (_temp, vfs, _sandbox, _base, run) = fixture();
        let target = run.parent().unwrap().parent().unwrap().join("ziel");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("fremd.txt"), b"fremd").unwrap();

        let error = start_quarantine_run(&vfs, &run, &target).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(run.join("fremd.txt")).unwrap(), b"fremd");
        assert!(!manifest_path(&run).unwrap().exists());
    }

    #[test]
    fn bound_start_ersetzt_den_erzeugungsbeleg_nicht_durch_einen_nachfolger() {
        let (_temp, vfs, _sandbox, base, run) = fixture();
        let target = run.parent().unwrap().parent().unwrap().join("ziel");
        std::fs::create_dir_all(&run).unwrap();
        let expected = vfs.root_identity(&run).unwrap();
        std::fs::rename(&run, base.join("urspruenglicher-lauf")).unwrap();
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("fremd.txt"), b"fremd").unwrap();

        let error = match start_quarantine_run_with_proof(&vfs, &run, &target, &expected) {
            Ok(_) => panic!("ein ausgetauschter Lauf darf kein Manifest erhalten"),
            Err(error) => error,
        };

        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(run.join("fremd.txt")).unwrap(), b"fremd");
        assert!(!manifest_path(&run).unwrap().exists());
    }

    #[test]
    fn ausgetauschter_lauf_autorisiert_keinen_kindzugriff() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        record_test_file(
            &vfs,
            &run,
            &target,
            Path::new("datei"),
            QuarantineReason::Replaced,
            b"alt",
        );
        drop(lease);

        let original = base.join("urspruenglicher-lauf");
        std::fs::rename(&run, &original).unwrap();
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("datei"), b"fremd").unwrap();

        assert!(list_quarantine_runs(&vfs, &base, &Some(sandbox.clone())).is_err());
        assert!(list_quarantine_entries(&vfs, &base, "lauf-1", &Some(sandbox.clone())).is_err());
        assert!(
            restore_quarantine_entry(&vfs, &base, "lauf-1", "datei", &Some(sandbox.clone()),)
                .is_err()
        );
        assert!(purge_quarantine_run(&vfs, &base, "lauf-1", &Some(sandbox)).is_err());
        assert_eq!(std::fs::read(run.join("datei")).unwrap(), b"fremd");
        assert!(!target.join("datei").exists());
        assert!(manifest_path(&run).unwrap().exists());
    }

    #[test]
    fn nach_abwesenheitspruefung_erscheinender_lauf_bleibt_beim_purge_unberuehrt() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        let lease = start_quarantine_run(&vfs, &run, &target).unwrap();
        std::fs::create_dir(&run).unwrap();
        bind_quarantine_run_identity(&vfs, &run).unwrap();
        std::fs::remove_dir(&run).unwrap();
        drop(lease);
        let appearing = AppearingRunVfs {
            inner: vfs,
            run: run.clone(),
            appeared: AtomicBool::new(false),
        };

        let error = purge_quarantine_run(&appearing, &base, "lauf-1", &Some(sandbox)).unwrap_err();

        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(run.join("fremd.txt")).unwrap(), b"fremd");
        assert!(manifest_path(&run).unwrap().exists());
        assert!(!base.join("lauf-1.purge-intent").exists());
        assert!(list_quarantine_runs(&appearing, &base, &None).is_err());
    }

    #[test]
    fn lokale_laufidentitaet_ignoriert_nur_kurzlebige_mountfelder() {
        use crate::{LocalMountIdentity, LocalRootIdentity, RootIdentity};

        let identity = |mount_id, mount_flags, display: &str, device, inode, volume| {
            RootIdentity::Local(LocalRootIdentity {
                device,
                inode,
                is_directory: true,
                mount: LocalMountIdentity {
                    mount_id,
                    unique_mount_id: mount_id % 2 == 0,
                    fsid: vec![1, 2],
                    volume_id: Some([volume; 16]),
                    fs_type: "apfs".into(),
                    mount_flags,
                },
                display_path: PathBuf::from(display),
            })
        };
        let first = stable_quarantine_run_identity_digest(&identity(1, 2, "/a", 7, 8, 9)).unwrap();
        let transient =
            stable_quarantine_run_identity_digest(&identity(99, 88, "/b", 7, 8, 9)).unwrap();
        let other_inode =
            stable_quarantine_run_identity_digest(&identity(1, 2, "/a", 7, 10, 9)).unwrap();
        let other_volume =
            stable_quarantine_run_identity_digest(&identity(1, 2, "/a", 7, 8, 11)).unwrap();

        assert_eq!(first, transient);
        assert_ne!(first, other_inode);
        assert_ne!(first, other_volume);
    }

    #[test]
    fn smb_laufidentitaet_ueberlebt_reconnect_aber_keinen_objektwechsel() {
        use crate::remote::{SmbRootIdentity, SmbServerIdentity};
        use crate::RootIdentity;

        let identity = |root_file_id, tree_id, generation| {
            RootIdentity::Smb(SmbRootIdentity {
                server: SmbServerIdentity {
                    host: "smb.example.invalid".into(),
                    port: 445,
                    server_guid: [1; 16],
                    share: "daten".into(),
                    account_digest: [2; 32],
                    canonical_root: "/".into(),
                    dialect: 0x0311,
                    signing_active: true,
                    encryption_active: true,
                    signing_algorithm: "aes-cmac".into(),
                    encryption_cipher: "aes-128-gcm".into(),
                },
                virtual_root: "/quarantine/run".into(),
                root_file_id,
                tree_id,
                session_generation: generation,
            })
        };
        let first = stable_quarantine_run_identity_digest(&identity(7, 8, 9)).unwrap();
        let reconnect = stable_quarantine_run_identity_digest(&identity(7, 80, 90)).unwrap();
        let replacement = stable_quarantine_run_identity_digest(&identity(70, 8, 9)).unwrap();

        assert_eq!(first, reconnect);
        assert_ne!(first, replacement);
    }

    #[test]
    fn sftp_sitzung_ist_keine_dauerhafte_laufordneridentitaet() {
        use crate::remote::{SftpHostKeyPin, SftpRootIdentity, SftpServerIdentity};
        use crate::RootIdentity;

        let identity = RootIdentity::Sftp(SftpRootIdentity {
            server: SftpServerIdentity {
                host: "sftp.example.invalid".into(),
                port: 22,
                host_key: SftpHostKeyPin {
                    algorithm: "ssh-ed25519".into(),
                    sha256: [1; 32],
                },
                canonical_root: "/".into(),
                sftp_version: 3,
                extensions_digest: [2; 32],
            },
            virtual_root: "/quarantine/run".into(),
            session_generation: 7,
        });

        let error = stable_quarantine_run_identity_digest(&identity).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(crate::error_code(&error), ErrorCode::Unsupported);
    }

    #[test]
    fn traversal_in_lauf_oder_eintrag_wird_abgelehnt() {
        let (_temp, vfs, sandbox, base, run) = fixture();
        let target = sandbox.join("ziel");
        start_quarantine_run(&vfs, &run, &target).unwrap();
        assert!(list_quarantine_entries(&vfs, &base, "../x", &Some(sandbox.clone())).is_err());
        assert!(restore_quarantine_entry(&vfs, &base, "lauf-1", "../x", &Some(sandbox),).is_err());
    }
}
