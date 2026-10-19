//! Sicherheitsvertrag für den lokalen Realmodus.
//!
//! Eine reale Ausführung entsteht ausschließlich aus einer frischen, descriptor-
//! gebundenen Vorbereitung. Der Aufrufer erhält keinen konstruierbaren Bool-Schalter,
//! sondern ein kurzlebiges Objekt mit vollständigen Listing-Proofs, Restore-Probe,
//! Speicherbilanz, Zufallsphrase und einmaligem Permit.

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::progress::{Cancel, Progress};
use crate::sync::{self, Action, Compare, Safety, SyncMode, SyncOptions, SyncPlan, SyncStats};
use crate::vfs::{
    FileCommitClass, LocalRootIdentity, LocalVfs, MutationFailure, ProofListEntry, Vfs,
};
use crate::{coded_error, ErrorCode};

pub const REAL_PERMIT_LIFETIME: Duration = Duration::from_secs(10 * 60);
const REAL_MAX_REPLACEMENTS: u64 = 100;
const REAL_MAX_DELETE_ENTRIES: u64 = 50;
const REAL_MAX_DELETE_PERCENT: u128 = 5;
const PROBE_BYTES: usize = 4096;
static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Bestätigungsfehler müssen als eigener Maschinencode bis zu GUI und CLI gelangen.
/// `PermissionDenied` allein würde sie fälschlich wie eine Betriebssystem-Sperre aussehen lassen.
pub(crate) fn confirmation_mismatch(message: impl Into<String>) -> io::Error {
    coded_error(
        io::ErrorKind::PermissionDenied,
        ErrorCode::ConfirmationMismatch,
        message,
    )
}

/// Jede Abweichung vom bestätigten Vorschauzustand verlangt eine vollständige Neuplanung.
/// Der gemeinsame Code verhindert, dass Aufrufer nur einen allgemeinen Abbruch anzeigen.
pub(crate) fn plan_drift(message: impl Into<String>) -> io::Error {
    coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift, message)
}

/// Providerneutrale Root-Identität. Remote-Varianten werden mit den jeweiligen
/// Providern ergänzt; lokale Permits können dadurch nicht mit einem Pfadstring
/// verwechselt werden.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "provider", content = "identity")]
pub enum RootIdentity {
    Local(LocalRootIdentity),
    Sftp(crate::remote::SftpRootIdentity),
    Ftps(crate::remote::FtpsRootIdentity),
    WebDav(crate::remote::WebDavRootIdentity),
    Smb(crate::remote::SmbRootIdentity),
}

/// Ein Fehler, der einen vollständigen Listing-Nachweis verhindert hat.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListingError {
    pub path: String,
    pub reason: String,
}

/// Vollständigkeitsbeleg eines rekursiven Scans. Ein Fehler bleibt als Datenobjekt
/// sichtbar und setzt `complete=false`; er wird niemals in eine leere Liste übersetzt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListingProof {
    pub root_identity: RootIdentity,
    pub directories_started: u64,
    pub directories_completed: u64,
    pub entries_seen: u64,
    pub listing_digest: [u8; 32],
    pub provider_token: Option<Vec<u8>>,
    pub errors: Vec<ListingError>,
    pub complete: bool,
}

/// Wirkungs- und Speicherbilanz der Realmodus-Vorschau.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RealRunImpact {
    pub new_files: u64,
    pub new_bytes: u64,
    pub replacements: u64,
    pub replacement_new_bytes: u64,
    pub replacement_old_bytes: u64,
    pub metadata_changes: u64,
    pub quarantine_entries: u64,
    pub quarantine_bytes: u64,
    pub required_available_bytes: u64,
    pub available_bytes: u64,
    /// `false` bedeutet: Der Provider besitzt keine verlässliche Quota-Abfrage.
    /// Das ist ausschließlich für AddOnly zulässig, weil ein volles Ziel dann nur
    /// den neuen Temp-Transfer scheitern lässt und keinen Bestand gefährdet.
    pub available_bytes_known: bool,
}

/// Serialisierbarer Teil einer backend-gehaltenen Vorbereitung.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RealRunPreview {
    pub source_identity: RootIdentity,
    pub target_identity: RootIdentity,
    pub quarantine_identity: RootIdentity,
    pub source_listing: ListingProof,
    pub target_listing: ListingProof,
    pub plan: SyncPlan,
    pub options: SyncOptions,
    pub impact: RealRunImpact,
    pub plan_digest: [u8; 32],
    pub confirmation_phrase: String,
    pub prepared_at_unix_ms: u128,
    pub expires_at_unix_ms: u128,
    pub restore_probe_ok: bool,
    pub backup_confirmation_required: bool,
    /// Nur bei Providerläufen: konkrete Zielwirkungen der aktuell gebundenen
    /// Sitzung. Lokale Läufe lassen das Feld weg.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_capabilities: Option<crate::ProviderTransferCapabilities>,
    /// Nur der normale lokale Einstieg kann diesen wirkungsgebundenen Vertrag
    /// ausgeben. Er ist weder eine Queuefreigabe noch ein Remote-/Mirrorprofil.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_execution: Option<LocalSyncExecution>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalSyncExecution {
    pub new_file_commit: FileCommitClass,
    pub file_flush: bool,
    pub directory_flush: bool,
    pub irreversible_replacements: u64,
}

impl LocalSyncExecution {
    pub(crate) fn read(vfs: &dyn Vfs, root: &Path) -> io::Result<Self> {
        let RootIdentity::Local(_) = vfs.root_identity(root)? else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Lokales Ausführungsprofil benötigt einen lokalen Root",
            ));
        };
        // Nur die tatsächlich bekannten nativen Treiber erhalten eine Zusage.
        // Auf anderen Mounts versuchen wir beide Flushs weiterhin; ausschließlich
        // ein maschinenlesbares Nichtunterstützt darf dort Best Effort bleiben.
        let (file_flush, directory_flush) = vfs.file_flush_guarantees_at(root)?;
        Ok(Self {
            new_file_commit: vfs.file_commit_class_at(root)?,
            file_flush,
            directory_flush,
            irreversible_replacements: 0,
        })
    }

    pub fn is_best_effort(&self) -> bool {
        self.new_file_commit == FileCommitClass::BestEffort
            || !self.file_flush
            || !self.directory_flush
    }

    fn bind_digest(&self, digest: [u8; 32]) -> io::Result<[u8; 32]> {
        Ok(*blake3::hash(
            &serde_json::to_vec(&("local-sync-v1", digest, self)).map_err(io::Error::other)?,
        )
        .as_bytes())
    }
}

/// Nicht serialisierbare Vorbereitung. Die gehaltenen VFS-Deskriptoren bleiben von
/// der letzten Vorschau bis zur Ausführung Bestandteil dieses Objekts.
pub struct PreparedLocalRealRun {
    source_vfs: LocalVfs,
    target_vfs: LocalVfs,
    source_root: PathBuf,
    target_root: PathBuf,
    quarantine_root: PathBuf,
    quarantine_run: PathBuf,
    preview: RealRunPreview,
    prepared_at: SystemTime,
    expires_at: SystemTime,
}

/// Normaler lokaler AddOnly-/Update-Auftrag. Die technischen Belege entsprechen
/// dem Realmodus; die Zustimmung richtet sich nach der konkreten Wirkung statt
/// nach einer zufälligen Phrase. Mirror kann diesen Typ nicht erzeugen.
pub struct PreparedLocalSyncRun(PreparedLocalRealRun);

pub fn prepare_local_sync_run(
    source: &Path,
    target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedLocalSyncRun> {
    prepare_local_sync_run_with_sandbox(source, target, options, None)
}

pub fn prepare_local_sync_run_with_sandbox(
    source: &Path,
    target: &Path,
    options: SyncOptions,
    sandbox: Option<&Path>,
) -> io::Result<PreparedLocalSyncRun> {
    options.validate()?;
    if options.mode == SyncMode::Mirror {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Mirror benötigt weiterhin den getrennten Realmodus-Assistenten",
        ));
    }
    let mut prepared = prepare_local_real_run_bound(source, target, options, sandbox, true)?;
    prepared.preview.backup_confirmation_required = false;
    Ok(PreparedLocalSyncRun(prepared))
}

impl PreparedLocalSyncRun {
    pub fn preview(&self) -> &RealRunPreview {
        self.0.preview()
    }

    pub fn confirm(self, replacements_confirmed: bool) -> io::Result<LocalRealRun> {
        let impact = &self.0.preview.impact;
        if (impact.replacements > 0
            || impact.metadata_changes > 0
            || self
                .0
                .preview
                .local_execution
                .as_ref()
                .is_some_and(LocalSyncExecution::is_best_effort))
            && !replacements_confirmed
        {
            return Err(confirmation_mismatch(
                "Die sichtbaren Ersetzungen, Metadatenänderungen oder Best-Effort-Risiken wurden nicht bestätigt",
            ));
        }
        // Die Zufallsphrase gehört ausschließlich zum getrennten Assistenten.
        // Dieser versiegelte Typ erlaubt nur AddOnly/Update; alle frischen Root-,
        // Listing-, Digest-, Platz- und Ablaufprüfungen bleiben dieselben.
        let phrase = self.0.preview.confirmation_phrase.clone();
        self.0.confirm(&phrase, true)
    }
}

/// Bestätigter Lauf. Das enthaltene Permit ist weder serialisierbar noch klonbar und
/// wird beim ersten Ausführungsversuch atomar verbraucht.
pub struct LocalRealRun {
    source_vfs: LocalVfs,
    target_vfs: LocalVfs,
    source_root: PathBuf,
    target_root: PathBuf,
    plan: SyncPlan,
    safety: Safety,
    audit: RealAuditContext,
}

#[derive(Clone)]
pub(crate) struct RealAuditContext {
    plan_digest: [u8; 32],
    source_identity_hash: [u8; 32],
    target_identity_hash: [u8; 32],
    impact: RealRunImpact,
    quarantine_run: String,
    restore_probe_ok: bool,
    local_execution: Option<LocalSyncExecution>,
}

#[derive(serde::Serialize)]
struct RealAuditRecord<'a> {
    version: u32,
    timestamp_unix_ms: u128,
    status: &'a str,
    plan_digest: [u8; 32],
    source_identity_hash: [u8; 32],
    target_identity_hash: [u8; 32],
    impact: &'a RealRunImpact,
    restore_probe_ok: bool,
    quarantine_run: &'a str,
    progress: &'a Progress,
    stats: Option<&'a SyncStats>,
    error_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_execution: Option<&'a LocalSyncExecution>,
}

/// Nur `PreparedLocalRealRun::confirm` kann dieses Permit erzeugen.
pub struct RealRunPermit {
    nonce: [u8; 32],
    plan_digest: [u8; 32],
    target_capabilities_digest: Option<[u8; 32]>,
    source_identity: RootIdentity,
    target_identity: RootIdentity,
    quarantine_identity: RootIdentity,
    source_listing: ListingProof,
    target_listing: ListingProof,
    options: SyncOptions,
    source_root: PathBuf,
    target_root: PathBuf,
    quarantine_root: PathBuf,
    issued_at: SystemTime,
    expires_at: SystemTime,
    consumed: AtomicBool,
    local_execution: Option<LocalSyncExecution>,
}

impl std::fmt::Debug for RealRunPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RealRunPermit")
            .field("nonce_hash", &blake3::hash(&self.nonce).to_hex())
            .field("plan_digest", &self.plan_digest)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("consumed", &self.consumed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Lokalen Realmodus vorbereiten. Diese Funktion verändert keine Nutzerdaten. Sie
/// erzeugt und entfernt lediglich eine zufällige eigene Restore-Probe und darf dafür
/// nur exklusive Namen verwenden.
pub fn prepare_local_real_run(
    source: &Path,
    target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedLocalRealRun> {
    prepare_local_real_run_bound(source, target, options, None, false)
}

fn prepare_local_real_run_bound(
    source: &Path,
    target: &Path,
    options: SyncOptions,
    sandbox: Option<&Path>,
    ordinary: bool,
) -> io::Result<PreparedLocalRealRun> {
    let (source_root, target_root) = sync::validate_local_sync_roots(source, target)?;
    if !ordinary {
        validate_real_target(&target_root)?;
    }
    let target_parent = target_root.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Realmodus-Ziel besitzt keinen sicheren Geschwisterbereich",
        )
    })?;
    let target_parent = std::fs::canonicalize(target_parent)?;

    // Quelle wird exakt an sich selbst gebunden. Das Ziel-VFS hält den Elternordner,
    // weil nur so Ziel und Quarantäne im selben Rename-Namespace erreichbar bleiben.
    let (source_vfs, target_vfs) = if let Some(root) = sandbox {
        let vfs = LocalVfs::rooted(root)?;
        Safety::test_sandbox(root.to_owned(), &vfs)?;
        if source_root == root
            || target_root == root
            || !source_root.starts_with(root)
            || !target_root.starts_with(root)
            || !target_parent.starts_with(root)
        {
            return Err(coded_error(io::ErrorKind::PermissionDenied,
                ErrorCode::SandboxViolation, "Lokaler Sync und seine Quarantäne müssen innerhalb der markierten Testsandbox bleiben"));
        }
        // Auch die Restore-Probe bleibt am gehaltenen Marker-Root. Ein Austausch
        // zwischen Vorabprüfung und Vorbereitung kann keine Fremdwurzel binden.
        (vfs.clone(), vfs)
    } else {
        (
            LocalVfs::rooted(&source_root)?,
            LocalVfs::rooted(&target_parent)?,
        )
    };
    let target_vfs = if ordinary {
        target_vfs.with_file_commit_root(&LocalVfs::rooted(&target_root)?)
    } else {
        target_vfs
    };
    if !source_vfs.anchor_is_current()? || !target_vfs.anchor_is_current()? {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Eine Realmodus-Wurzel änderte sich bereits während des Öffnens",
        ));
    }

    let source_identity = RootIdentity::Local(source_vfs.local_root_identity(&source_root)?);
    let target_identity = RootIdentity::Local(target_vfs.local_root_identity(&target_root)?);
    let source_listing = build_listing_proof(&source_vfs, &source_root, source_identity.clone());
    let target_listing = build_listing_proof(&target_vfs, &target_root, target_identity.clone());
    require_complete("Quelle", &source_listing)?;
    require_complete("Ziel", &target_listing)?;

    let plan = sync::plan(
        &source_vfs,
        &source_root,
        &target_vfs,
        &target_root,
        &options,
    )?;
    validate_real_plan(&plan, &options)?;
    let target_vfs = if ordinary {
        target_vfs.with_sync_parents(&target_root, &plan)?
    } else {
        target_vfs
    };
    let mut local_execution = ordinary
        .then(|| LocalSyncExecution::read(&target_vfs, &target_root))
        .transpose()?;
    if let Some(execution) = &mut local_execution {
        if execution.is_best_effort() {
            execution.irreversible_replacements = plan.update_count;
        }
    }
    let (quarantine_root, quarantine_run, quarantine_identity, restore_probe_ok) = if ordinary
        && (plan.update_count == 0
            || local_execution
                .as_ref()
                .is_some_and(LocalSyncExecution::is_best_effort))
        && plan.delete_count == 0
    {
        // Wie beim additiven Remoteweg bindet der unveränderte Zielroot den
        // unbenutzten Quarantäne-Anker. Kein Geschwister-Schreibrecht und keine
        // künstliche Restore-Probe sind für reine Additionen erforderlich.
        (
            target_root.clone(),
            reserve_quarantine_run_name(&target_vfs, &target_root)?,
            target_identity.clone(),
            false,
        )
    } else {
        if ordinary {
            validate_real_target(&target_root)?;
        }
        let root = target_parent.join(".doppeldecker-quarantine");
        ensure_quarantine_root(&target_vfs, &target_parent, &root)?;
        run_restore_probe(&target_vfs, &target_root, &root)?;
        let run = reserve_quarantine_run_name(&target_vfs, &root)?;
        let identity = RootIdentity::Local(target_vfs.local_root_identity(&root)?);
        (root, run, identity, true)
    };
    ensure_same_mount(&target_identity, &quarantine_identity)?;

    let mut impact = impact_for_plan(&plan);
    impact.available_bytes = target_vfs.available_bytes(&target_root)?;
    impact.available_bytes_known = true;
    if impact.available_bytes < impact.required_available_bytes {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!(
                "Zu wenig freier Platz: {} Bytes benötigt, {} Bytes verfügbar",
                impact.required_available_bytes, impact.available_bytes
            ),
        ));
    }

    let prepared_at = SystemTime::now();
    let expires_at = prepared_at + REAL_PERMIT_LIFETIME;
    let mut plan_digest = binding_digest(
        &plan,
        &options,
        &source_identity,
        &target_identity,
        &quarantine_identity,
        &source_listing,
        &target_listing,
    )?;
    if let Some(execution) = &local_execution {
        plan_digest = execution.bind_digest(plan_digest)?;
    }
    let confirmation_phrase = confirmation_phrase(&target_root)?;
    let backup_confirmation_required = options.mode != SyncMode::AddOnly;
    let preview = RealRunPreview {
        source_identity,
        target_identity,
        quarantine_identity,
        source_listing,
        target_listing,
        plan,
        options,
        impact,
        plan_digest,
        confirmation_phrase,
        prepared_at_unix_ms: unix_millis(prepared_at)?,
        expires_at_unix_ms: unix_millis(expires_at)?,
        restore_probe_ok,
        backup_confirmation_required,
        provider_capabilities: None,
        local_execution,
    };

    Ok(PreparedLocalRealRun {
        source_vfs,
        target_vfs,
        source_root,
        target_root,
        quarantine_root,
        quarantine_run,
        preview,
        prepared_at,
        expires_at,
    })
}

impl PreparedLocalRealRun {
    pub fn preview(&self) -> &RealRunPreview {
        &self.preview
    }

    /// Führt den gemeinsamen Frontend-unabhängigen Abschluss in einem Zug aus:
    /// Bestätigung prüfen, alle gebundenen Zustände frisch lesen, das Einmal-Permit
    /// erzeugen und erst nach dauerhaftem Audit mutieren. CLI und Desktop-App können
    /// dadurch keine dieser Sicherheitsphasen unterschiedlich anordnen.
    pub fn confirm_and_execute_with_audit(
        self,
        phrase: &str,
        backup_confirmed: bool,
        audit_path: &Path,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<SyncStats> {
        self.confirm(phrase, backup_confirmed)?
            .execute_with_audit(audit_path, cancel, on_progress)
    }

    /// Exakte Laufphrase und bei Update/Mirror die Bestätigung eines unabhängigen
    /// Backups prüfen. Danach werden Identitäten, Listings, Plan und Platz ein letztes
    /// Mal gelesen, bevor das einmalige Permit entsteht.
    pub fn confirm(self, phrase: &str, backup_confirmed: bool) -> io::Result<LocalRealRun> {
        if phrase != self.preview.confirmation_phrase {
            return Err(confirmation_mismatch(format!(
                "Realmodus-Bestätigung muss exakt „{}“ lauten",
                self.preview.confirmation_phrase
            )));
        }
        if self.preview.backup_confirmation_required && !backup_confirmed {
            return Err(confirmation_mismatch(
                "Update/Mirror benötigt die bewusste Bestätigung eines unabhängigen Backups",
            ));
        }
        let now = SystemTime::now();
        if now > self.expires_at {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Realmodus-Vorprüfung ist älter als zehn Minuten",
            ));
        }
        if !self.source_vfs.anchor_is_current()? || !self.target_vfs.anchor_is_current()? {
            return Err(plan_drift(
                "Quelle oder Ziel wurde seit der Realmodus-Vorschau ausgetauscht",
            ));
        }
        let source_identity =
            RootIdentity::Local(self.source_vfs.local_root_identity(&self.source_root)?);
        let target_identity =
            RootIdentity::Local(self.target_vfs.local_root_identity(&self.target_root)?);
        let quarantine_identity =
            RootIdentity::Local(self.target_vfs.local_root_identity(&self.quarantine_root)?);
        if source_identity != self.preview.source_identity
            || target_identity != self.preview.target_identity
            || quarantine_identity != self.preview.quarantine_identity
        {
            return Err(plan_drift(
                "Mount- oder Root-Identität änderte sich seit der Realmodus-Vorschau",
            ));
        }
        let source_listing =
            build_listing_proof(&self.source_vfs, &self.source_root, source_identity.clone());
        let target_listing =
            build_listing_proof(&self.target_vfs, &self.target_root, target_identity.clone());
        require_complete("Quelle", &source_listing)?;
        require_complete("Ziel", &target_listing)?;
        if source_listing != self.preview.source_listing
            || target_listing != self.preview.target_listing
        {
            return Err(plan_drift(
                "Quelle oder Ziel änderte sich seit dem vollständigen Listing",
            ));
        }
        let fresh_plan = sync::plan(
            &self.source_vfs,
            &self.source_root,
            &self.target_vfs,
            &self.target_root,
            &self.preview.options,
        )?;
        if fresh_plan != self.preview.plan {
            return Err(plan_drift("Plan änderte sich seit der Realmodus-Vorschau"));
        }
        let available = self.target_vfs.available_bytes(&self.target_root)?;
        if available < self.preview.impact.required_available_bytes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "Freier Platz reicht unmittelbar vor der Permit-Ausgabe nicht mehr aus",
            ));
        }

        if let Some(expected) = &self.preview.local_execution {
            let mut fresh = LocalSyncExecution::read(&self.target_vfs, &self.target_root)?;
            fresh.irreversible_replacements = expected.irreversible_replacements;
            if &fresh != expected {
                return Err(plan_drift(
                    "Lokale Zielgarantien änderten sich seit der Vorschau",
                ));
            }
        }
        let mut permit = create_real_permit(RealPermitRequest {
            plan_digest: self.preview.plan_digest,
            target_capabilities_digest: None,
            source_identity,
            target_identity,
            quarantine_identity,
            source_listing,
            target_listing,
            options: self.preview.options.clone(),
            source_root: self.source_root.clone(),
            target_root: self.target_root.clone(),
            quarantine_root: self.quarantine_root.clone(),
            issued_at: now.max(self.prepared_at),
            expires_at: self.expires_at,
        })?;
        permit.local_execution = self.preview.local_execution.clone();
        let audit = RealAuditContext {
            local_execution: self.preview.local_execution.clone(),
            plan_digest: self.preview.plan_digest,
            source_identity_hash: hash_serializable(&self.preview.source_identity)?,
            target_identity_hash: hash_serializable(&self.preview.target_identity)?,
            impact: self.preview.impact.clone(),
            restore_probe_ok: self.preview.restore_probe_ok,
            quarantine_run: self
                .quarantine_run
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Ungültige Quarantäne-Lauf-ID")
                })?
                .to_string(),
        };
        let plan = self.preview.plan;
        let target_vfs = self
            .target_vfs
            .with_sync_execution(self.preview.local_execution.clone());
        let safety = Safety::real(permit, self.quarantine_run);
        Ok(LocalRealRun {
            source_vfs: self.source_vfs,
            target_vfs,
            source_root: self.source_root,
            target_root: self.target_root,
            plan,
            safety,
            audit,
        })
    }
}

impl LocalRealRun {
    pub(crate) fn with_queue_staging_owner(mut self, queue: &Path, operation: &str) -> Self {
        self.target_vfs = self.target_vfs.with_staging_owner(queue, operation);
        self
    }

    pub fn plan(&self) -> &SyncPlan {
        &self.plan
    }

    pub fn execute(
        self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<SyncStats> {
        sync::execute(
            sync::ExecuteRequest {
                src: &self.source_vfs,
                src_root: &self.source_root,
                dst: &self.target_vfs,
                dst_root: &self.target_root,
                plan: &self.plan,
                safety: &self.safety,
            },
            cancel,
            on_progress,
        )
    }

    /// Reale Ausführung mit append-only Audit. Bereits der Startdatensatz wird vor
    /// der ersten Nutzerdatenmutation synchronisiert; scheitert das, bleibt das Permit
    /// zwar verbraucht, aber der Zielbaum unangetastet.
    pub fn execute_with_audit(
        self,
        audit_path: &Path,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<SyncStats> {
        execute_real_with_audit(
            RealAuditRun {
                source_vfs: &self.source_vfs,
                source_root: &self.source_root,
                target_vfs: &self.target_vfs,
                target_root: &self.target_root,
                plan: &self.plan,
                safety: &self.safety,
                audit: &self.audit,
                audit_path,
            },
            cancel,
            on_progress,
        )
    }

    pub(crate) fn execute_with_audit_detailed(
        self,
        audit_path: &Path,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<RealAuditExecutionSuccess> {
        execute_real_with_audit_detailed(
            RealAuditRun {
                source_vfs: &self.source_vfs,
                source_root: &self.source_root,
                target_vfs: &self.target_vfs,
                target_root: &self.target_root,
                plan: &self.plan,
                safety: &self.safety,
                audit: &self.audit,
                audit_path,
            },
            cancel,
            on_progress,
        )
    }
}

/// Gemeinsamer Auditpfad für lokale und Remote-Realmodi. Der Startdatensatz wird vor
/// der ersten Nutzerdatenmutation synchronisiert; Dateinamen werden aus Fortschritt
/// und Fehlerklassifikation entfernt.
pub(crate) struct RealAuditRun<'a> {
    pub(crate) source_vfs: &'a dyn Vfs,
    pub(crate) source_root: &'a Path,
    pub(crate) target_vfs: &'a dyn Vfs,
    pub(crate) target_root: &'a Path,
    pub(crate) plan: &'a SyncPlan,
    pub(crate) safety: &'a Safety,
    pub(crate) audit: &'a RealAuditContext,
    pub(crate) audit_path: &'a Path,
}

/// Erfolgreiche Nutzdatenwirkung mit bewusst noch nicht geschriebenem
/// Abschlussaudit. Ein Queue-Aufrufer muss zuerst `complete` beziehungsweise
/// `cancel` dauerhaft sichern und darf erst danach [`Self::append_completion_audit`]
/// aufrufen. Damit gibt es keinen Absturzzeitpunkt mehr, an dem das Audit bereits
/// „completed“ sagt, das Queuejournal den Lauf aber noch als wiederholbar zeigt.
pub(crate) struct RealAuditExecutionSuccess {
    stats: SyncStats,
    latest: Progress,
    audit: RealAuditContext,
    audit_path: PathBuf,
}

impl RealAuditExecutionSuccess {
    pub(crate) fn stats(&self) -> &SyncStats {
        &self.stats
    }

    pub(crate) fn append_completion_audit(self) -> io::Result<SyncStats> {
        let status = if self.stats.cancelled {
            "cancelled"
        } else {
            "completed"
        };
        append_audit(
            &self.audit_path,
            &self.audit,
            status,
            &self.latest,
            Some(&self.stats),
            None,
        )?;
        Ok(self.stats)
    }
}

pub(crate) fn execute_real_with_audit(
    run: RealAuditRun<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<SyncStats> {
    execute_real_with_audit_detailed(run, cancel, on_progress)
        .and_then(RealAuditExecutionSuccess::append_completion_audit)
}

pub(crate) fn execute_real_with_audit_detailed(
    run: RealAuditRun<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<RealAuditExecutionSuccess> {
    let RealAuditRun {
        source_vfs,
        source_root,
        target_vfs,
        target_root,
        plan,
        safety,
        audit,
        audit_path,
    } = run;
    let mut latest = Progress::default();
    append_audit(audit_path, audit, "started", &latest, None, None)?;
    let result = sync::execute(
        sync::ExecuteRequest {
            src: source_vfs,
            src_root: source_root,
            dst: target_vfs,
            dst_root: target_root,
            plan,
            safety,
        },
        cancel,
        &mut |progress| {
            latest = progress.clone();
            on_progress(progress);
        },
    );
    match result {
        Ok(stats) => Ok(RealAuditExecutionSuccess {
            stats,
            latest,
            audit: audit.clone(),
            audit_path: audit_path.to_path_buf(),
        }),
        Err(error) => {
            let audit_result = append_audit(
                audit_path,
                audit,
                "failed",
                &latest,
                None,
                Some(format!("{:?}", error.kind())),
            );
            if let Err(audit_error) = audit_result {
                return Err(execution_with_failed_audit_error(error, audit_error));
            }
            Err(error)
        }
    }
}

fn execution_with_failed_audit_error(error: io::Error, audit_error: io::Error) -> io::Error {
    // Der Auditfehler ist zusätzlicher Diagnosebestand, darf aber die fachliche
    // Fehlerklasse der bereits gelaufenen Mutation nicht überschreiben. Besonders
    // `OutcomeUnclear` muss bis zur Queue gelangen, weil nur dieser Code einen
    // blinden Wiederholungsversuch sperrt.
    let paths = crate::error::recovery_paths(&error);
    crate::error::with_recovery_paths(crate::coded_error(
        error.kind(),
        crate::error_code(&error),
        format!(
            "Realmodus fehlgeschlagen ({error}); Audit-Abschluss ebenfalls fehlgeschlagen ({audit_error})"
        ),
    ), &paths)
}

pub(crate) struct RealPermitRequest {
    pub(crate) plan_digest: [u8; 32],
    pub(crate) target_capabilities_digest: Option<[u8; 32]>,
    pub(crate) source_identity: RootIdentity,
    pub(crate) target_identity: RootIdentity,
    pub(crate) quarantine_identity: RootIdentity,
    pub(crate) source_listing: ListingProof,
    pub(crate) target_listing: ListingProof,
    pub(crate) options: SyncOptions,
    pub(crate) source_root: PathBuf,
    pub(crate) target_root: PathBuf,
    pub(crate) quarantine_root: PathBuf,
    pub(crate) issued_at: SystemTime,
    pub(crate) expires_at: SystemTime,
}

pub(crate) fn create_real_permit(request: RealPermitRequest) -> io::Result<RealRunPermit> {
    let RealPermitRequest {
        plan_digest,
        target_capabilities_digest,
        source_identity,
        target_identity,
        quarantine_identity,
        source_listing,
        target_listing,
        options,
        source_root,
        target_root,
        quarantine_root,
        issued_at,
        expires_at,
    } = request;
    let mut nonce = [0u8; 32];
    fill_random(&mut nonce)?;
    Ok(RealRunPermit {
        nonce,
        plan_digest,
        target_capabilities_digest,
        source_identity,
        target_identity,
        quarantine_identity,
        source_listing,
        target_listing,
        options,
        source_root,
        target_root,
        quarantine_root,
        issued_at,
        expires_at,
        consumed: AtomicBool::new(false),
        local_execution: None,
    })
}

pub(crate) fn create_real_audit_context(
    plan_digest: [u8; 32],
    source_identity: &RootIdentity,
    target_identity: &RootIdentity,
    impact: RealRunImpact,
    quarantine_run: &Path,
    restore_probe_ok: bool,
) -> io::Result<RealAuditContext> {
    let quarantine_run = quarantine_run
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Ungültige Quarantäne-Lauf-ID"))?
        .to_owned();
    Ok(RealAuditContext {
        local_execution: None,
        plan_digest,
        source_identity_hash: hash_serializable(source_identity)?,
        target_identity_hash: hash_serializable(target_identity)?,
        impact,
        quarantine_run,
        restore_probe_ok,
    })
}

fn append_audit(
    path: &Path,
    context: &RealAuditContext,
    status: &str,
    progress: &Progress,
    stats: Option<&SyncStats>,
    error_kind: Option<String>,
) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Auditpfad besitzt keinen Elternordner",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    // Für Teilfortschritt genügen Phase und Zähler. Relative Dateinamen bleiben aus
    // dem Audit heraus, damit das Sicherheitsprotokoll keine Inhaltsstruktur verrät.
    let mut safe_progress = progress.clone();
    safe_progress.current.clear();
    let record = RealAuditRecord {
        version: 1,
        timestamp_unix_ms: unix_millis(SystemTime::now())?,
        status,
        plan_digest: context.plan_digest,
        source_identity_hash: context.source_identity_hash,
        target_identity_hash: context.target_identity_hash,
        impact: &context.impact,
        restore_probe_ok: context.restore_probe_ok,
        quarantine_run: &context.quarantine_run,
        progress: &safe_progress,
        stats,
        error_kind,
        local_execution: context.local_execution.as_ref(),
    };
    let mut payload = serde_json::to_vec(&record).map_err(io::Error::other)?;
    payload.push(b'\n');
    let (mut file, created) = open_audit_append(path)?;
    file.write_all(&payload)?;
    file.sync_all()?;
    if created {
        let directory_path = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(directory_path)?
            .sync_all()?;
    }
    Ok(())
}

/// Öffnet ausschließlich die bereits gebundene reguläre Auditdatei oder legt sie
/// exklusiv neu an. So kann ein parallel eingesetzter Symlink oder Hardlink nicht als
/// scheinbar harmloses append-only Ziel eine andere Nutzerdatei beschädigen.
fn open_audit_append(path: &Path) -> io::Result<(std::fs::File, bool)> {
    let expected = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.nlink() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Auditpfad ist keine einzelne reguläre Datei",
                ));
            }
            Some((metadata.dev(), metadata.ino()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let mut options = OpenOptions::new();
    options
        .write(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    if expected.is_some() {
        options.create(false);
    } else {
        options.create_new(true);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || expected.is_some_and(|identity| identity != (metadata.dev(), metadata.ino()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Auditdatei änderte sich während des sicheren Öffnens",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok((file, expected.is_none()))
}

fn hash_serializable(value: &impl serde::Serialize) -> io::Result<[u8; 32]> {
    serde_json::to_vec(value)
        .map(|payload| *blake3::hash(&payload).as_bytes())
        .map_err(io::Error::other)
}

impl RealRunPermit {
    #[cfg(test)]
    fn force_expired(&mut self) {
        self.expires_at = UNIX_EPOCH;
    }

    pub(crate) fn consume_and_validate(
        &self,
        src: &dyn Vfs,
        src_root: &Path,
        dst: &dyn Vfs,
        dst_root: &Path,
        plan: &SyncPlan,
    ) -> io::Result<()> {
        if self
            .consumed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "RealRunPermit wurde bereits verbraucht",
            ));
        }
        if SystemTime::now() > self.expires_at {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RealRunPermit ist abgelaufen",
            ));
        }
        if src_root != self.source_root || dst_root != self.target_root {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "RealRunPermit wurde für andere Quell- oder Zielpfade ausgestellt",
            ));
        }
        self.validate_roots(src, dst)?;
        let base_digest = binding_digest(
            plan,
            &self.options,
            &self.source_identity,
            &self.target_identity,
            &self.quarantine_identity,
            &self.source_listing,
            &self.target_listing,
        )?;
        let current_capabilities = dst.safety_capabilities_digest()?;
        if current_capabilities != self.target_capabilities_digest {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Ziel-Fähigkeiten änderten sich nach der Permit-Ausgabe",
            ));
        }
        let mut current_digest = bind_capabilities_digest(base_digest, current_capabilities);
        if let Some(execution) = &self.local_execution {
            current_digest = execution.bind_digest(current_digest)?;
        }
        if current_digest != self.plan_digest {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Bestätigter Realmodus-Plan wurde verändert",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_roots(&self, src: &dyn Vfs, dst: &dyn Vfs) -> io::Result<()> {
        if !src.anchor_is_current()? || !dst.anchor_is_current()? {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Gehaltene Realmodus-Wurzel ist nicht mehr am sichtbaren Pfad",
            ));
        }
        let source = src.root_identity(&self.source_root)?;
        let target = dst.root_identity(&self.target_root)?;
        let quarantine = dst.root_identity(&self.quarantine_root)?;
        if source != self.source_identity
            || target != self.target_identity
            || quarantine != self.quarantine_identity
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Root-, Mount- oder Quarantäne-Identität des Realmodus änderte sich",
            ));
        }
        if let Some(expected) = &self.local_execution {
            let mut fresh = LocalSyncExecution::read(dst, &self.target_root)?;
            fresh.irreversible_replacements = expected.irreversible_replacements;
            if &fresh != expected {
                return Err(plan_drift(
                    "Die bestätigten lokalen Zielgarantien änderten sich",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn local_execution(&self) -> Option<&LocalSyncExecution> {
        self.local_execution.as_ref()
    }

    pub(crate) fn validate_mirror_rescan(
        &self,
        src: &dyn Vfs,
        src_root: &Path,
        dst: &dyn Vfs,
        dst_root: &Path,
        original: &SyncPlan,
    ) -> io::Result<()> {
        if self.options.mode != SyncMode::Mirror || original.delete_count == 0 {
            return Ok(());
        }
        if src_root != self.source_root || dst_root != self.target_root {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Mirror-Zweitscan verwendet andere Permit-Wurzeln",
            ));
        }
        self.validate_roots(src, dst)?;
        let source = build_listing_proof(src, src_root, self.source_identity.clone());
        require_complete("Quelle beim Mirror-Zweitscan", &source)?;
        if source != self.source_listing {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Quellbestand änderte sich vor der Mirror-Entfernungsphase",
            ));
        }
        let target = build_listing_proof(dst, dst_root, self.target_identity.clone());
        require_complete("Ziel beim Mirror-Zweitscan", &target)?;
        // Eine fremde Änderung auf den gewünschten Endwert kann die ausstehende
        // Aktion im frischen Plan verschwinden lassen. Deshalb ihren bestätigten
        // Vorzustand unabhängig davon erneut prüfen, bevor ein Ziel verschwindet.
        for action in &original.actions {
            match action {
                Action::SetMode {
                    path,
                    target_mode,
                    is_dir: true,
                    ..
                } => {
                    let current = dst.stat(&crate::path::checked_join(dst_root, path)?)?;
                    if current.is_symlink || !current.is_dir || current.mode != *target_mode {
                        return Err(plan_drift(
                            "Zielordner-Rechte änderten sich vor der Mirror-Entfernung",
                        ));
                    }
                }
                Action::SetDirectoryMetadata { path, metadata, .. } => {
                    let target_manifest = metadata.target.as_ref().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Ordner-Metadaten besitzen keinen Ziel-Vorzustand",
                        )
                    })?;
                    crate::metadata::verify_metadata_manifest(
                        dst,
                        &crate::path::checked_join(dst_root, path)?,
                        target_manifest,
                    )?;
                }
                _ => {}
            }
        }
        let fresh = sync::plan(src, src_root, dst, dst_root, &self.options)?;
        let expected = deletion_digest(original)?;
        let actual = deletion_digest(&fresh)?;
        let has_other_changes = fresh.actions.iter().any(|action| {
            !matches!(
                action,
                Action::DeleteFile { .. }
                    | Action::DeleteDir { .. }
                    | Action::SkipSymlink { .. }
                    | Action::SkipConflict { .. }
                    | Action::SkipUnsupported { .. }
            ) && !pending_directory_metadata_matches(action, original)
        });
        if fresh.skip_count > 0 || has_other_changes || expected != actual {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Mirror-Zweitscan stimmt nicht mehr mit den bestätigten Entfernungen überein",
            ));
        }
        Ok(())
    }
}

/// Der Zweitscan läuft vor den endgültigen Ordner-Metadaten. Nur die bereits
/// bestätigten, noch ausstehenden Werte dürfen deshalb neben Entfernungen bleiben.
/// Ziel-mtime ist die einzige Ausnahme: Unsere Kind-Renames ändern sie absichtlich;
/// Zielrechte, ACLs, Owner und xattrs bleiben weiterhin am alten Manifest gebunden.
fn pending_directory_metadata_matches(action: &Action, original: &SyncPlan) -> bool {
    original
        .actions
        .iter()
        .any(|planned| match (action, planned) {
            (
                Action::SetMode {
                    path,
                    source_mode,
                    target_mode,
                    is_dir: true,
                    content_hash,
                },
                Action::SetMode {
                    path: expected_path,
                    source_mode: expected_source_mode,
                    target_mode: expected_target_mode,
                    is_dir: true,
                    content_hash: expected_hash,
                },
            ) => {
                path == expected_path
                    && source_mode == expected_source_mode
                    && target_mode == expected_target_mode
                    && content_hash == expected_hash
            }
            (
                Action::SetMode {
                    path,
                    source_mode,
                    is_dir: true,
                    content_hash: None,
                    ..
                },
                Action::CreateDir {
                    path: expected_path,
                    source_mode: Some(expected_mode),
                    metadata: None,
                    ..
                },
            ) => path == expected_path && source_mode == expected_mode,
            (
                Action::SetDirectoryMetadata {
                    path,
                    source_mtime,
                    metadata,
                    ..
                },
                Action::SetDirectoryMetadata {
                    path: expected_path,
                    source_mtime: expected_mtime,
                    metadata: expected_metadata,
                    ..
                },
            ) => {
                path == expected_path
                    && source_mtime == expected_mtime
                    && metadata == expected_metadata
            }
            (
                Action::SetDirectoryMetadata {
                    path,
                    source_mtime,
                    metadata,
                    ..
                },
                Action::CreateDir {
                    path: expected_path,
                    source_mtime: expected_mtime,
                    metadata: Some(expected_metadata),
                    ..
                },
            ) => {
                let mut actual = metadata.clone();
                actual.target = None;
                path == expected_path
                    && source_mtime == expected_mtime
                    && &actual == expected_metadata
            }
            _ => false,
        })
}

/// Rekursiven Listing-Proof erstellen. Die Funktion liefert auch bei Providerfehlern
/// ein vollständiges Fehlerobjekt; nur interne Serialisierungsfehler wären ein `Err`.
pub fn build_listing_proof(
    vfs: &dyn Vfs,
    root: &Path,
    root_identity: RootIdentity,
) -> ListingProof {
    struct State {
        started: u64,
        completed: u64,
        entries: u64,
        errors: Vec<ListingError>,
        tokens: blake3::Hasher,
        saw_token: bool,
        listing: blake3::Hasher,
    }

    fn visit(vfs: &dyn Vfs, path: &Path, relative: &[Vec<u8>], state: &mut State) {
        state.started = state.started.saturating_add(1);
        let listing = match vfs.list_complete(path) {
            Ok(listing) => listing,
            Err(error) => {
                state.errors.push(ListingError {
                    path: display_relative(relative),
                    reason: error.to_string(),
                });
                return;
            }
        };
        if let Err(error) = require_complete_safe_listing(&listing, "Realmodus-Listing") {
            state.errors.push(ListingError {
                path: display_relative(relative),
                reason: error.to_string(),
            });
            return;
        }
        if let Some(token) = listing.provider_token {
            state.saw_token = true;
            state.tokens.update(&(token.len() as u64).to_le_bytes());
            state.tokens.update(&token);
        }
        let mut entries = listing.entries;
        entries.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
        for listed in entries {
            state.entries = state.entries.saturating_add(1);
            let mut child = relative.to_vec();
            child.push(listed.raw_name.clone());
            hash_listing_entry(&mut state.listing, &child, &listed);
            if listed.entry.is_dir && !listed.entry.is_symlink {
                visit(vfs, &path.join(&listed.entry.name), &child, state);
            }
        }
        state.completed = state.completed.saturating_add(1);
    }

    let identity_before = vfs.root_identity(root).ok();
    let mut state = State {
        started: 0,
        completed: 0,
        entries: 0,
        errors: Vec::new(),
        tokens: blake3::Hasher::new(),
        saw_token: false,
        listing: blake3::Hasher::new(),
    };
    visit(vfs, root, &[], &mut state);
    let identity_after = vfs.root_identity(root).ok();
    if identity_before.as_ref() != Some(&root_identity)
        || identity_after.as_ref() != Some(&root_identity)
    {
        state.errors.push(ListingError {
            path: ".".into(),
            reason: "Root-Identität änderte sich während der Auflistung".into(),
        });
    }
    let complete = state.errors.is_empty() && state.started == state.completed;
    ListingProof {
        root_identity,
        directories_started: state.started,
        directories_completed: state.completed,
        entries_seen: state.entries,
        listing_digest: *state.listing.finalize().as_bytes(),
        provider_token: state
            .saw_token
            .then(|| state.tokens.finalize().as_bytes().to_vec()),
        errors: state.errors,
        complete,
    }
}

/// Gemeinsamer Vertrag für rekursive Realmodus- und Mengenlistings. `complete=true`
/// allein genügt nicht: Jeder Kindname muss genau eine normale Pfadkomponente sein,
/// bytegenau zu seinem UTF-8-Anzeigenamen passen und innerhalb des Elternlistings
/// eindeutig bleiben.
pub(crate) fn require_complete_safe_listing(
    listing: &crate::vfs::CompleteDirectoryListing,
    action: &str,
) -> io::Result<()> {
    if !listing.complete {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!("{action} ist nicht nachweislich vollständig"),
        ));
    }
    let mut raw_names = std::collections::HashSet::with_capacity(listing.entries.len());
    for listed in &listing.entries {
        let name = listed.entry.name.as_str();
        let mut components = Path::new(name).components();
        let single_normal_component = matches!(
            (components.next(), components.next()),
            (Some(std::path::Component::Normal(_)), None)
        );
        if !listed.safely_decoded
            || name.is_empty()
            || name.contains('\0')
            || !single_normal_component
            || listed.raw_name.is_empty()
            || listed.raw_name.contains(&0)
            || listed.raw_name.contains(&b'/')
            || listed.raw_name != name.as_bytes()
        {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!(
                    "{action} enthält einen ungültigen, mehrdeutigen oder nicht sicher als UTF-8 dekodierbaren Kindnamen"
                ),
            ));
        }
        if !raw_names.insert(listed.raw_name.as_slice()) {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!("{action} enthält einen doppelten rohen Kindnamen"),
            ));
        }
    }
    Ok(())
}

fn hash_listing_entry(hasher: &mut blake3::Hasher, path: &[Vec<u8>], listed: &ProofListEntry) {
    hasher.update(&(path.len() as u64).to_le_bytes());
    for component in path {
        hasher.update(&(component.len() as u64).to_le_bytes());
        hasher.update(component);
    }
    let entry = &listed.entry;
    hasher.update(&[
        u8::from(entry.is_dir),
        u8::from(entry.is_file),
        u8::from(entry.is_symlink),
        u8::from(listed.safely_decoded),
    ]);
    hasher.update(&entry.size.to_le_bytes());
    hasher.update(&entry.mtime.unwrap_or(i64::MIN).to_le_bytes());
    hasher.update(&entry.mode.unwrap_or(u32::MAX).to_le_bytes());
    hasher.update(&entry.device.unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(&entry.inode.unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(&entry.hard_links.unwrap_or(u64::MAX).to_le_bytes());
    match &listed.provider_token {
        Some(token) => {
            hasher.update(&(token.len() as u64).to_le_bytes());
            hasher.update(token);
        }
        None => {
            hasher.update(&u64::MAX.to_le_bytes());
        }
    }
}

fn display_relative(components: &[Vec<u8>]) -> String {
    if components.is_empty() {
        return ".".into();
    }
    components
        .iter()
        .map(|value| String::from_utf8_lossy(value))
        .collect::<Vec<_>>()
        .join("/")
}

fn require_complete(role: &str, proof: &ListingProof) -> io::Result<()> {
    if proof.complete {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{role}-Auflistung ist unvollständig: {}",
            proof
                .errors
                .iter()
                .map(|error| format!("{}: {}", error.path, error.reason))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    ))
}

pub(crate) fn validate_real_target(target: &Path) -> io::Result<()> {
    if target == Path::new("/") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Dateisystemwurzel darf kein Realmodus-Ziel sein",
        ));
    }
    if let Ok(home_raw) = std::env::var("HOME") {
        if let Ok(home) = std::fs::canonicalize(home_raw) {
            if target == home {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Benutzerordner darf nicht selbst Realmodus-Ziel sein",
                ));
            }
        }
    }
    for ancestor in target.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Git-Repository darf kein Realmodus-Ziel enthalten",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "Git-Schutzpfad unter {} konnte nicht sicher geprüft werden: {error}",
                        ancestor.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_real_plan(plan: &SyncPlan, options: &SyncOptions) -> io::Result<()> {
    match options.mode {
        SyncMode::AddOnly => {
            if plan.update_count > 0 || plan.delete_count > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Real-AddOnly darf weder ersetzen noch entfernen",
                ));
            }
        }
        SyncMode::Update => {
            if options.compare != Compare::ContentHash {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Real-Update verlangt ContentHash",
                ));
            }
            if plan.update_count > REAL_MAX_REPLACEMENTS {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Real-Update erlaubt höchstens {REAL_MAX_REPLACEMENTS} Ersetzungen"),
                ));
            }
            if plan.delete_count > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Real-Update darf keine Mirror-Entfernungen enthalten",
                ));
            }
        }
        SyncMode::Mirror => {
            if options.compare != Compare::ContentHash {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Real-Mirror verlangt ContentHash",
                ));
            }
            if plan.source_count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Eine vollständig leere Quelle ist im Real-Mirror gesperrt",
                ));
            }
            // Ein ausgelassener Eintrag macht nur eine Mirror-Entfernung unsicher:
            // Ohne geplante Entfernung dürfen unabhängige Ergänzungen und
            // Aktualisierungen weiterlaufen. Der allgemeine Safety-Validator und
            // der Mirror-Zweitscan verwenden dieselbe Grenze.
            if plan.delete_count > 0 && plan.skip_count > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Ausgelassene Einträge sperren sämtliche Real-Mirror-Entfernungen",
                ));
            }
            if plan.delete_count > REAL_MAX_DELETE_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Real-Mirror erlaubt höchstens {REAL_MAX_DELETE_ENTRIES} entfernte Einträge"
                    ),
                ));
            }
            let deletion_percent = u128::from(plan.delete_count).saturating_mul(100);
            let maximum_percent =
                u128::from(plan.target_count).saturating_mul(REAL_MAX_DELETE_PERCENT);
            if plan.target_count > 0 && deletion_percent > maximum_percent {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Real-Mirror überschreitet die Grenze von 5 % des Zielbestands",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn ensure_quarantine_root(
    vfs: &dyn Vfs,
    parent: &Path,
    quarantine: &Path,
) -> io::Result<()> {
    match vfs.stat(quarantine) {
        Ok(entry) => {
            if entry.is_dir && !entry.is_symlink {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Reservierter Quarantänepfad ist kein echter Ordner",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    vfs.create_dir(quarantine)?;
    vfs.sync_dir(parent)
}

pub(crate) fn reserve_quarantine_run_name(vfs: &dyn Vfs, base: &Path) -> io::Result<PathBuf> {
    for _ in 0..100 {
        let mut random = [0u8; 8];
        fill_random(&mut random)?;
        let id = format!(
            "real-{}-{}-{}",
            std::process::id(),
            PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            hex(&random)
        );
        let run = base.join(&id);
        let sidecar = base.join(format!("{id}.json"));
        let run_free = match vfs.stat(&run) {
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(error),
        };
        let sidecar_free = match vfs.stat(&sidecar) {
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(error),
        };
        if run_free && sidecar_free {
            return Ok(run);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Kein kollisionsfreier Quarantäne-Laufname verfügbar",
    ))
}

pub(crate) fn run_restore_probe(vfs: &dyn Vfs, target: &Path, quarantine: &Path) -> io::Result<()> {
    let sequence = PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut nonce = [0u8; 16];
    fill_random(&mut nonce)?;
    let label = format!(
        ".doppeldecker-real-probe-{}-{sequence}-{}",
        std::process::id(),
        hex(&nonce)
    );
    let first = target.join(&label);
    let renamed = target.join(format!("{label}-renamed"));
    let parked = quarantine.join(&label);
    let mut bytes = [0u8; PROBE_BYTES];
    fill_random(&mut bytes)?;
    let expected = blake3::hash(&bytes);

    let (mut writer, _) = vfs
        .open_write_new_with_removal_proof(&first)
        .map_err(|failure| match failure {
            MutationFailure::Unchanged(error) => error,
            MutationFailure::OutcomeUnclear(error) => crate::error::with_recovery_context(
                coded_error(
                    error.kind(),
                    ErrorCode::OutcomeUnclear,
                    format!("Anlage der lokalen Restore-Probe hat einen unklaren Ausgang: {error}"),
                ),
                &[&error],
                &[&first],
            ),
        })?;
    let mut active_probe = Some(first.clone());
    let probe = (|| -> io::Result<()> {
        writer.write_all(&bytes)?;
        writer.flush()?;
        drop(writer);
        vfs.sync_file(&first)?;
        relocate_restore_probe(vfs, &first, &renamed, &mut active_probe)?;
        vfs.sync_dir(target)?;
        if hash_vfs_file(vfs, &renamed)? != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Restore-Probe stimmt nach dem ersten Rename nicht überein",
            ));
        }
        relocate_restore_probe(vfs, &renamed, &parked, &mut active_probe)?;
        vfs.sync_dir(target)?;
        vfs.sync_dir(quarantine)?;
        relocate_restore_probe(vfs, &parked, &first, &mut active_probe)?;
        vfs.sync_dir(quarantine)?;
        vfs.sync_dir(target)?;
        if hash_vfs_file(vfs, &first)? != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Restore-Probe stimmt nach der Wiederherstellung nicht überein",
            ));
        }
        vfs.remove_file(&first)?;
        active_probe = None;
        vfs.sync_dir(target)
    })();

    if probe.is_err() {
        // Ein kollidierender Name wurde nie von uns angelegt. Cleanup folgt nur
        // der erfolgreichen Anlage beziehungsweise dem letzten bestätigten Rename.
        // Nach einer unklaren Mutation bleiben beide möglichen Orte zur Prüfung.
        let mut cleanup_errors = Vec::new();
        if let Some(path) = active_probe.as_deref() {
            // `exists()` liefert bei jedem Fehler `false`. Ein Stat-, Verbindungs-
            // oder Providerfehler hiesse dann „ist schon weg", und der Probename
            // bliebe unbemerkt im echten Zielordner des Nutzers liegen — obwohl der
            // Kommentar darüber ausdrücklich verspricht, dass jeder Cleanup-Fehler
            // sichtbar bleibt. Nur ein echtes `NotFound` ist Abwesenheit.
            match crate::fileops::optional_stat(vfs, path) {
                Ok(None) => {}
                Ok(Some(_)) => {
                    if let Err(error) = vfs.remove_file(path).and_then(|()| {
                        vfs.sync_dir(path.parent().expect("Probenname hat Elternordner"))
                    }) {
                        cleanup_errors.push(crate::remote::error_with_path(path, error));
                    }
                }
                Err(error) => cleanup_errors.push(crate::remote::error_with_path(
                    path,
                    crate::error::with_context(error, "Verbleib unbestätigt"),
                )),
            }
        }
        let cleanup = crate::remote::fold_cleanup_errors(cleanup_errors);
        return crate::remote::combine_probe_outcome(
            "Restore",
            probe,
            cleanup,
            &target.display().to_string(),
        );
    }
    probe
}

fn relocate_restore_probe(
    vfs: &dyn Vfs,
    from: &Path,
    to: &Path,
    active_probe: &mut Option<PathBuf>,
) -> io::Result<()> {
    match vfs.rename_noreplace_with_removal_proof(from, to) {
        Ok(_) => {
            *active_probe = Some(to.to_owned());
            Ok(())
        }
        Err(MutationFailure::Unchanged(error)) => Err(error),
        Err(MutationFailure::OutcomeUnclear(error)) => {
            *active_probe = None;
            Err(crate::error::with_recovery_context(
                coded_error(
                    error.kind(),
                    ErrorCode::OutcomeUnclear,
                    format!("Rename der lokalen Restore-Probe hat einen unklaren Ausgang: {error}"),
                ),
                &[&error],
                &[from, to],
            ))
        }
    }
}

fn hash_vfs_file(vfs: &dyn Vfs, path: &Path) -> io::Result<blake3::Hash> {
    let mut reader = vfs.open_read(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize())
}

pub(crate) fn ensure_same_mount(
    target: &RootIdentity,
    quarantine: &RootIdentity,
) -> io::Result<()> {
    match (target, quarantine) {
        (RootIdentity::Local(target), RootIdentity::Local(quarantine)) => {
            if target.mount != quarantine.mount || target.device != quarantine.device {
                return Err(io::Error::new(
                    io::ErrorKind::CrossesDevices,
                    "Quarantäne liegt nicht im atomaren Rename-Namespace des Ziels",
                ));
            }
        }
        (RootIdentity::Sftp(target), RootIdentity::Sftp(quarantine)) => {
            if target.server != quarantine.server
                || target.session_generation != quarantine.session_generation
            {
                return Err(io::Error::new(
                    io::ErrorKind::CrossesDevices,
                    "SFTP-Quarantäne liegt nicht in derselben gebundenen Sitzung",
                ));
            }
        }
        (RootIdentity::Ftps(target), RootIdentity::Ftps(quarantine)) => {
            if target.server != quarantine.server
                || target.session_generation != quarantine.session_generation
            {
                return Err(io::Error::new(
                    io::ErrorKind::CrossesDevices,
                    "FTPS-Quarantäne liegt nicht in derselben gebundenen Sitzung",
                ));
            }
        }
        (RootIdentity::WebDav(target), RootIdentity::WebDav(quarantine)) => {
            if target.server != quarantine.server
                || target.session_generation != quarantine.session_generation
            {
                return Err(io::Error::new(
                    io::ErrorKind::CrossesDevices,
                    "WebDAV-Quarantäne liegt nicht in derselben gebundenen Sitzung",
                ));
            }
        }
        (RootIdentity::Smb(target), RootIdentity::Smb(quarantine)) => {
            if target.server != quarantine.server
                || target.tree_id != quarantine.tree_id
                || target.session_generation != quarantine.session_generation
            {
                return Err(io::Error::new(
                    io::ErrorKind::CrossesDevices,
                    "SMB-Quarantäne liegt nicht in derselben gebundenen Freigabesitzung",
                ));
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                "Quarantäne und Ziel verwenden verschiedene Provider",
            ));
        }
    }
    Ok(())
}

pub(crate) fn impact_for_plan(plan: &SyncPlan) -> RealRunImpact {
    let mut impact = RealRunImpact {
        metadata_changes: plan.metadata_count,
        quarantine_entries: plan.delete_count,
        quarantine_bytes: plan.delete_bytes,
        ..Default::default()
    };
    for action in &plan.actions {
        match action {
            Action::CopyFile { size, .. } => {
                impact.new_files = impact.new_files.saturating_add(1);
                impact.new_bytes = impact.new_bytes.saturating_add(*size);
            }
            Action::UpdateFile {
                size, target_size, ..
            } => {
                impact.replacements = impact.replacements.saturating_add(1);
                impact.replacement_new_bytes = impact.replacement_new_bytes.saturating_add(*size);
                impact.replacement_old_bytes =
                    impact.replacement_old_bytes.saturating_add(*target_size);
                impact.quarantine_entries = impact.quarantine_entries.saturating_add(1);
                impact.quarantine_bytes = impact.quarantine_bytes.saturating_add(*target_size);
            }
            Action::CreateHardlink {
                target_size: Some(target_size),
                ..
            } => {
                impact.replacements = impact.replacements.saturating_add(1);
                impact.replacement_old_bytes =
                    impact.replacement_old_bytes.saturating_add(*target_size);
                impact.quarantine_entries = impact.quarantine_entries.saturating_add(1);
                impact.quarantine_bytes = impact.quarantine_bytes.saturating_add(*target_size);
            }
            _ => {}
        }
    }
    impact.required_available_bytes = plan
        .bytes_to_transfer
        .saturating_add(impact.quarantine_bytes)
        .saturating_add(PROBE_BYTES as u64);
    impact
}

pub(crate) fn binding_digest(
    plan: &SyncPlan,
    options: &SyncOptions,
    source: &RootIdentity,
    target: &RootIdentity,
    quarantine: &RootIdentity,
    source_listing: &ListingProof,
    target_listing: &ListingProof,
) -> io::Result<[u8; 32]> {
    let payload = serde_json::to_vec(&(
        1u32,
        plan,
        options,
        source,
        target,
        quarantine,
        source_listing,
        target_listing,
    ))
    .map_err(io::Error::other)?;
    Ok(*blake3::hash(&payload).as_bytes())
}

pub(crate) fn bind_capabilities_digest(base: [u8; 32], capabilities: Option<[u8; 32]>) -> [u8; 32] {
    let Some(capabilities) = capabilities else {
        return base;
    };
    let mut hash = blake3::Hasher::new();
    hash.update(&[1]);
    hash.update(&base);
    hash.update(&capabilities);
    *hash.finalize().as_bytes()
}

fn deletion_digest(plan: &SyncPlan) -> io::Result<[u8; 32]> {
    let deletions = plan
        .actions
        .iter()
        .filter(|action| matches!(action, Action::DeleteFile { .. } | Action::DeleteDir { .. }))
        .collect::<Vec<_>>();
    serde_json::to_vec(&deletions)
        .map(|payload| *blake3::hash(&payload).as_bytes())
        .map_err(io::Error::other)
}

pub(crate) fn confirmation_phrase(target: &Path) -> io::Result<String> {
    let basename = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Zielname ist nicht UTF-8"))?;
    let mut random = [0u8; 4];
    fill_random(&mut random)?;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let code = (0..6)
        .map(|index| {
            ALPHABET[(random[index % random.len()] as usize + index) % ALPHABET.len()] as char
        })
        .collect::<String>();
    Ok(format!("REAL {basename} {code}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn fill_random(buffer: &mut [u8]) -> io::Result<()> {
    getrandom::fill(buffer)
        .map_err(|error| io::Error::other(format!("Betriebssystem-Zufall fehlgeschlagen: {error}")))
}

fn unix_millis(time: SystemTime) -> io::Result<u128> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn normaler_sync_unterscheidet_case_nach_dem_tatsaechlichen_zieldateisystem() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("Datei.txt"), b"neu").unwrap();
        std::fs::write(target.join("datei.txt"), b"erhalten").unwrap();
        let distinct = match std::fs::symlink_metadata(target.join("Datei.txt")) {
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => panic!("Case-Probe scheiterte: {error}"),
        };
        let prepared = prepare_local_sync_run(&source, &target, SyncOptions::default()).unwrap();
        assert_eq!(prepared.preview().plan.copy_count, u64::from(distinct));
        assert_eq!(prepared.preview().plan.skip_count, u64::from(!distinct));
        prepared
            .confirm(false)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(
            std::fs::read(target.join("datei.txt")).unwrap(),
            b"erhalten"
        );
        if distinct {
            assert_eq!(std::fs::read(target.join("Datei.txt")).unwrap(), b"neu");
        }
    }

    #[test]
    fn normaler_sync_addiert_ohne_phrase_und_verweigert_mirror_vor_probe() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("neu.txt"), b"neu").unwrap();
        let mirror = SyncOptions {
            mode: SyncMode::Mirror,
            compare: Compare::ContentHash,
            ..Default::default()
        };
        assert!(prepare_local_sync_run(&source, &target, mirror).is_err());
        assert!(!temp.path().join(".doppeldecker-quarantine").exists());
        let prepared = prepare_local_sync_run(&source, &target, SyncOptions::default()).unwrap();
        assert!(!prepared.preview().restore_probe_ok);
        let run = prepared.confirm(false).unwrap();
        let stats = run
            .execute_with_audit(
                &temp.path().join("audit.jsonl"),
                &Cancel::new(),
                &mut |_| {},
            )
            .unwrap();
        assert!(!stats.dry_run);
        assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"neu");
        assert!(!temp.path().join(".doppeldecker-quarantine").exists());
        let audit: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(temp.path().join("audit.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(audit["restore_probe_ok"], false);
    }

    #[test]
    fn normale_additionen_brauchen_keinen_beschreibbaren_geschwisterbereich() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let parent = temp.path().join("projekt");
        let target = parent.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir(parent.join(".git")).unwrap();
        std::fs::write(source.join("neu.txt"), b"neu").unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = prepare_local_sync_run(&source, &target, SyncOptions::default())
            .and_then(|prepared| prepared.confirm(false))
            .and_then(|run| run.execute(&Cancel::new(), &mut |_| {}));
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!result.unwrap().dry_run);
        assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"neu");
        assert!(!parent.join(".doppeldecker-quarantine").exists());
        assert!(prepare_local_real_run(&source, &target, SyncOptions::default()).is_err());
    }

    #[test]
    fn normaler_sync_leere_und_gleiche_baeume_erhalten_den_zielbestand() {
        for mode in [SyncMode::AddOnly, SyncMode::Update] {
            for same_tree in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let source = temp.path().join("quelle");
                let target = temp.path().join("ziel");
                std::fs::create_dir(&source).unwrap();
                std::fs::create_dir(&target).unwrap();
                std::fs::write(target.join("erhalten.txt"), b"Zielbestand").unwrap();
                if same_tree {
                    std::fs::write(source.join("erhalten.txt"), b"Zielbestand").unwrap();
                    let time = filetime::FileTime::from_unix_time(1_700_000_000, 0);
                    for root in [&source, &target] {
                        filetime::set_file_mtime(root.join("erhalten.txt"), time).unwrap();
                    }
                }
                let prepared = prepare_local_sync_run(
                    &source,
                    &target,
                    SyncOptions {
                        mode,
                        compare: Compare::ContentHash,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(prepared.preview().impact.replacements, 0);
                assert!(!prepared.preview().restore_probe_ok);
                let stats = prepared
                    .confirm(false)
                    .unwrap()
                    .execute(&Cancel::new(), &mut |_| {})
                    .unwrap();
                assert!(!stats.dry_run);
                assert_eq!(stats.files_copied, 0);
                assert_eq!(
                    std::fs::read(target.join("erhalten.txt")).unwrap(),
                    b"Zielbestand"
                );
                assert!(!temp.path().join(".doppeldecker-quarantine").exists());
            }
        }
    }

    #[test]
    fn normaler_sync_ueberlappende_roots_werden_vor_jeder_probe_abgelehnt() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("wurzel");
        let child = root.join("kind");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(root.join("erhalten.txt"), b"Original").unwrap();
        for (source, target) in [(&root, &root), (&root, &child), (&child, &root)] {
            assert!(prepare_local_sync_run(source, target, SyncOptions::default()).is_err());
        }
        assert_eq!(
            std::fs::read(root.join("erhalten.txt")).unwrap(),
            b"Original"
        );
        assert_eq!(std::fs::read_dir(&child).unwrap().count(), 0);
        assert!(!temp.path().join(".doppeldecker-quarantine").exists());
        assert!(!root.join(".doppeldecker-quarantine").exists());
    }

    #[test]
    fn normaler_sync_rootaustausch_stoppt_vor_der_ersten_wirkung() {
        for swap_source in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("quelle");
            let target = temp.path().join("ziel");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::write(source.join("datei.txt"), b"Original").unwrap();
            let prepared =
                prepare_local_sync_run(&source, &target, SyncOptions::default()).unwrap();
            let swapped = if swap_source { &source } else { &target };
            let preserved = temp.path().join("alter-root");
            std::fs::rename(swapped, &preserved).unwrap();
            std::fs::create_dir(swapped).unwrap();
            // Gleicher sichtbarer Bestand verhindert einen bloßen Planvergleich
            // als Ersatz für die Bestätigung der tatsächlich gehaltenen Wurzel.
            if swap_source {
                std::fs::copy(preserved.join("datei.txt"), source.join("datei.txt")).unwrap();
                let time = filetime::FileTime::from_last_modification_time(
                    &std::fs::metadata(preserved.join("datei.txt")).unwrap(),
                );
                filetime::set_file_mtime(source.join("datei.txt"), time).unwrap();
            }
            assert_eq!(
                crate::error_code(&prepared.confirm(false).err().unwrap()),
                ErrorCode::PlanDrift
            );
            assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            assert_eq!(
                std::fs::read(source.join("datei.txt")).unwrap(),
                b"Original"
            );
            if swap_source {
                assert_eq!(
                    std::fs::read(preserved.join("datei.txt")).unwrap(),
                    b"Original"
                );
            } else {
                assert_eq!(std::fs::read_dir(&preserved).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn normaler_update_braucht_wirkungszustimmung_und_sichert_alte_version() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("gleich-lang.txt"), b"neu").unwrap();
        std::fs::write(target.join("gleich-lang.txt"), b"alt").unwrap();
        let time = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(source.join("gleich-lang.txt"), time).unwrap();
        filetime::set_file_mtime(target.join("gleich-lang.txt"), time).unwrap();
        let options = SyncOptions {
            mode: SyncMode::Update,
            compare: Compare::ContentHash,
            ..Default::default()
        };
        let prepared = prepare_local_sync_run(&source, &target, options.clone()).unwrap();
        assert!(!prepared.preview().backup_confirmation_required);
        assert_eq!(prepared.preview().impact.replacements, 1);
        assert_eq!(
            crate::error_code(&prepared.confirm(false).err().unwrap()),
            ErrorCode::ConfirmationMismatch
        );
        assert_eq!(
            std::fs::read(target.join("gleich-lang.txt")).unwrap(),
            b"alt"
        );
        let stats = prepare_local_sync_run(&source, &target, options)
            .unwrap()
            .confirm(true)
            .unwrap()
            .execute_with_audit(
                &temp.path().join("audit.jsonl"),
                &Cancel::new(),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            std::fs::read(target.join("gleich-lang.txt")).unwrap(),
            b"neu"
        );
        assert!(stats.quarantine.is_some());
        let quarantine = temp.path().join(".doppeldecker-quarantine");
        fn contents(root: &Path, bytes: &mut Vec<Vec<u8>>) {
            for entry in std::fs::read_dir(root).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    contents(&entry.path(), bytes);
                } else {
                    bytes.push(std::fs::read(entry.path()).unwrap());
                }
            }
        }
        let mut old_versions = Vec::new();
        contents(&quarantine, &mut old_versions);
        assert!(old_versions.iter().any(|bytes| bytes == b"alt"));
    }

    #[test]
    fn normaler_sync_erklaert_drift_und_bindet_markierten_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(
            root.join(crate::sync::TEST_SANDBOX_MARKER),
            crate::sync::TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("datei.txt"), b"neu").unwrap();
        let prepared = prepare_local_sync_run_with_sandbox(
            &source,
            &target,
            SyncOptions::default(),
            Some(&root),
        )
        .unwrap();
        std::fs::write(target.join("datei.txt"), b"spater").unwrap();
        assert_eq!(
            crate::error_code(&prepared.confirm(false).err().unwrap()),
            ErrorCode::PlanDrift
        );
        assert_eq!(std::fs::read(target.join("datei.txt")).unwrap(), b"spater");
        let outside = tempfile::tempdir().unwrap();
        assert!(prepare_local_sync_run_with_sandbox(
            &source,
            outside.path(),
            SyncOptions::default(),
            Some(&root)
        )
        .is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    fn fixture(mode: SyncMode) -> (TempDir, PreparedLocalRealRun) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("neu.txt"), b"neu").unwrap();
        let options = SyncOptions {
            mode,
            compare: if mode == SyncMode::AddOnly {
                Compare::SizeAndTime
            } else {
                Compare::ContentHash
            },
            ..Default::default()
        };
        let prepared = prepare_local_real_run(&source, &target, options).unwrap();
        (temp, prepared)
    }

    #[test]
    fn mirror_zweitscan_erlaubt_nur_bestaetigte_ausstehende_ordnerrechte() {
        let action = Action::SetMode {
            path: "ordner".into(),
            source_mode: 0o555,
            target_mode: Some(0o755),
            is_dir: true,
            content_hash: None,
        };
        let original = SyncPlan {
            actions: vec![action.clone()],
            ..Default::default()
        };
        assert!(pending_directory_metadata_matches(&action, &original));
        let mut drifted = action.clone();
        if let Action::SetMode { target_mode, .. } = &mut drifted {
            *target_mode = Some(0o777);
        }
        assert!(!pending_directory_metadata_matches(&drifted, &original));
        let mut changed_source = action;
        if let Action::SetMode { source_mode, .. } = &mut changed_source {
            *source_mode = 0o700;
        }
        assert!(!pending_directory_metadata_matches(
            &changed_source,
            &original
        ));
    }

    #[test]
    fn real_mirror_finalisiert_restriktive_ordnerrechte_nach_quarantaene() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(source.join("ordner")).unwrap();
        std::fs::create_dir_all(target.join("ordner")).unwrap();
        // Ein geplanter Remove bleibt unter der realen Grenze von fünf Prozent.
        for index in 0..20 {
            let name = format!("ordner/behalten-{index}");
            std::fs::write(source.join(&name), b"gleich").unwrap();
            std::fs::write(target.join(&name), b"gleich").unwrap();
        }
        std::fs::write(target.join("ordner/entfernen"), b"alt").unwrap();
        std::fs::set_permissions(
            source.join("ordner"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::set_permissions(
            target.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let result = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {});
        let final_mode = std::fs::metadata(target.join("ordner"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let extra_exists = target.join("ordner/entfernen").exists();
        std::fs::set_permissions(
            source.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            target.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let stats = result.unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(final_mode, 0o555);
        assert!(!extra_exists);
        assert!(stats.quarantine.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_mirror_native_ordner_mtime_bleibt_nach_entfernung_planidentisch() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(source.join("ordner")).unwrap();
        std::fs::create_dir_all(target.join("ordner")).unwrap();
        // Ein geplanter Remove bleibt unter der realen Grenze von fünf Prozent.
        for index in 0..20 {
            let name = format!("ordner/behalten-{index}");
            std::fs::write(source.join(&name), b"gleich").unwrap();
            std::fs::write(target.join(&name), b"gleich").unwrap();
        }
        std::fs::write(target.join("ordner/entfernen"), b"alt").unwrap();
        std::fs::set_permissions(
            source.join("ordner"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::set_permissions(
            target.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        filetime::set_file_mtime(
            source.join("ordner"),
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            target.join("ordner"),
            filetime::FileTime::from_unix_time(1_600_000_000, 0),
        )
        .unwrap();
        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                metadata: crate::MetadataProfile::NativeStrict,
                ..Default::default()
            },
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let result = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {});
        let final_mode = std::fs::metadata(target.join("ordner"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let final_mtime = LocalVfs::new().stat(&target.join("ordner")).unwrap().mtime;
        let extra_exists = target.join("ordner/entfernen").exists();
        std::fs::set_permissions(
            source.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            target.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let stats = result.unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(final_mode, 0o555);
        assert!(!extra_exists);
        assert!(stats.quarantine.is_some());
        assert_eq!(final_mtime, Some(1_700_000_000));
    }

    #[test]
    fn mirror_zweitscan_lehnt_verschwundene_ordnerrechteaktion_vor_entfernung_ab() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(source.join("ordner")).unwrap();
        std::fs::create_dir_all(target.join("ordner")).unwrap();
        for index in 0..20 {
            let name = format!("ordner/behalten-{index}");
            std::fs::write(source.join(&name), b"gleich").unwrap();
            std::fs::write(target.join(&name), b"gleich").unwrap();
        }
        std::fs::write(source.join("neu"), b"neu").unwrap();
        std::fs::write(target.join("ordner/entfernen"), b"alt").unwrap();
        std::fs::set_permissions(
            source.join("ordner"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::set_permissions(
            target.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let mut changed = false;
        let result =
            prepared
                .confirm(&phrase, true)
                .unwrap()
                .execute(&Cancel::new(), &mut |progress| {
                    if progress.phase == "copy" && progress.files_done == 1 && !changed {
                        std::fs::set_permissions(
                            target.join("ordner"),
                            std::fs::Permissions::from_mode(0o555),
                        )
                        .unwrap();
                        changed = true;
                    }
                });
        let extra_exists = target.join("ordner/entfernen").exists();
        std::fs::set_permissions(
            source.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            target.join("ordner"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(changed);
        assert!(result.is_err());
        assert!(extra_exists, "Drift muss vor jeder Entfernung stoppen");
        assert_eq!(std::fs::read(target.join("neu")).unwrap(), b"neu");
    }

    #[test]
    fn webdav_quarantaene_bindet_sitzung_statt_fremde_collection_etag() {
        let server = crate::remote::WebDavServerIdentity {
            origin: "https://dav.example.test".into(),
            certificate: crate::remote::WebDavCertificatePin { sha256: [1; 32] },
            account_digest: [2; 32],
            canonical_root: "/daten".into(),
            dav_header_digest: [3; 32],
            server_header_digest: [4; 32],
            legacy_server_header_digests: Vec::new(),
        };
        let target = RootIdentity::WebDav(crate::remote::WebDavRootIdentity {
            server: server.clone(),
            virtual_root: "/daten/ziel".into(),
            root_etag: "\"ziel-1\"".into(),
            session_generation: 7,
        });
        let quarantine = RootIdentity::WebDav(crate::remote::WebDavRootIdentity {
            server,
            virtual_root: "/daten/.doppeldecker-quarantine/lauf".into(),
            root_etag: "\"quarantaene-1\"".into(),
            session_generation: 7,
        });

        ensure_same_mount(&target, &quarantine).unwrap();
        let mut reconnected = quarantine;
        let RootIdentity::WebDav(identity) = &mut reconnected else {
            unreachable!()
        };
        identity.session_generation += 1;
        assert_eq!(
            ensure_same_mount(&target, &reconnected).unwrap_err().kind(),
            io::ErrorKind::CrossesDevices
        );
    }

    #[test]
    fn add_only_laeuft_nur_mit_exakter_phrase_und_einmaligem_permit() {
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        assert!(prepared
            .confirm("REAL falsches-ziel ABC123", false)
            .is_err());

        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::AddOnly,
                ..Default::default()
            },
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let audit = temp.path().join("real-audit.jsonl");
        let stats = prepared
            .confirm_and_execute_with_audit(&phrase, false, &audit, &Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"neu");
        let audit = std::fs::read_to_string(audit).unwrap();
        assert!(audit.contains("\"status\":\"started\""));
        assert!(audit.contains("\"status\":\"completed\""));
    }

    #[test]
    fn detaillierter_realmodus_verschiebt_den_abschlussaudit_bis_nach_queueabschluss() {
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        let audit_path = temp.path().join("aufgeschobener-audit.jsonl");

        let result = execute_real_with_audit_detailed(
            RealAuditRun {
                source_vfs: &run.source_vfs,
                source_root: &run.source_root,
                target_vfs: &run.target_vfs,
                target_root: &run.target_root,
                plan: &run.plan,
                safety: &run.safety,
                audit: &run.audit,
                audit_path: &audit_path,
            },
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(result.is_ok(), "Nutzdatenlauf muss erfolgreich sein");

        let audit = std::fs::read_to_string(&audit_path).unwrap();
        assert_eq!(
            audit.lines().count(),
            1,
            "Der detaillierte Queue-Weg darf vor dessen terminaler Persistenz nur den Startaudit schreiben"
        );
        assert!(audit.contains("\"status\":\"started\""));
    }

    #[test]
    fn zweiter_auditfehler_bewahrt_den_code_des_mutationsfehlers() {
        let mutation = crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            "Antwort nach Mutation verloren",
        );
        let paths = vec!["/ziel/erhaltener-temp".to_string()];
        let mutation = crate::error::with_recovery_paths(mutation, &paths);
        let combined = execution_with_failed_audit_error(
            mutation,
            io::Error::new(io::ErrorKind::StorageFull, "Auditmedium voll"),
        );

        assert_eq!(
            crate::error_code(&combined),
            crate::ErrorCode::OutcomeUnclear
        );
        assert!(combined
            .to_string()
            .contains("Antwort nach Mutation verloren"));
        assert!(combined.to_string().contains("Auditmedium voll"));
        assert_eq!(crate::error::recovery_paths(&combined), paths);
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let journal = temp.path().join("state/queue.json");
        let queue = crate::OperationQueue::open(&journal).unwrap();
        let entry = queue
            .enqueue(crate::QueueOperation::ProviderSync {
                source: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: temp.path().join("quelle").to_string_lossy().into_owned(),
                }),
                target: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: temp.path().join("ziel").to_string_lossy().into_owned(),
                }),
                options: SyncOptions::default(),
                expected_plan: Box::new(prepared.preview().plan.clone()),
            })
            .unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        queue
            .fail_coded(&entry.operation_id, Progress::default(), &combined)
            .unwrap();
        drop(queue);
        let reopened = crate::OperationQueue::open(journal).unwrap();
        assert_eq!(reopened.snapshot().entries[0].temp_recovery_paths, paths);
    }

    #[test]
    fn plan_drift_verhindert_permit_und_probe_hinterlaesst_keine_datei() {
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let target = temp.path().join("ziel");
        assert!(std::fs::read_dir(&target).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("real-probe")));
        std::fs::write(temp.path().join("quelle/spaet.txt"), b"drift").unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        assert!(prepared.confirm(&phrase, false).is_err());
        assert!(!target.join("neu.txt").exists());
    }

    #[test]
    fn update_verlangt_hash_und_backupbestaetigung() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("datei"), b"neu").unwrap();
        std::fs::write(target.join("datei"), b"alt").unwrap();
        let fast = SyncOptions {
            mode: SyncMode::Update,
            compare: Compare::SizeAndTime,
            ..Default::default()
        };
        assert!(prepare_local_real_run(&source, &target, fast).is_err());

        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        assert!(prepared.confirm(&phrase, false).is_err());
    }

    #[test]
    fn mirror_ohne_entfernung_fuehrt_unabhaengige_ergaenzung_trotz_auslassung_aus() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("neu.txt"), b"neu").unwrap();
        symlink("nicht-folgen", source.join("ausgelassen")).unwrap();

        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(prepared.preview().plan.copy_count, 1);
        assert_eq!(prepared.preview().plan.skip_count, 1);
        assert_eq!(prepared.preview().plan.delete_count, 0);

        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"neu");
        assert!(!target.join("ausgelassen").exists());
    }

    #[test]
    fn mirror_mit_auslassung_und_entfernung_bleibt_gesperrt() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        for index in 0..19 {
            let name = format!("gleich-{index:02}");
            std::fs::write(source.join(&name), b"x").unwrap();
            std::fs::write(target.join(&name), b"x").unwrap();
        }
        symlink("nicht-folgen", source.join("ausgelassen")).unwrap();
        std::fs::write(target.join("entfernen"), b"alt").unwrap();

        let error = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .err()
        .expect("Mirror mit Auslassung und Entfernung muss vor der Probe scheitern");
        assert!(error
            .to_string()
            .contains("Ausgelassene Einträge sperren sämtliche Real-Mirror-Entfernungen"));
        assert!(target.join("entfernen").exists());
    }

    #[test]
    fn update_und_mirror_quarantaenisieren_statt_zu_loeschen() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("datei"), b"neu").unwrap();
        std::fs::write(target.join("datei"), b"alt").unwrap();
        filetime::set_file_mtime(
            source.join("datei"),
            filetime::FileTime::from_unix_time(20, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            target.join("datei"),
            filetime::FileTime::from_unix_time(10, 0),
        )
        .unwrap();
        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"neu");
        assert!(stats.quarantine.is_some());

        // Der reale Lauf ist über denselben manifestgebundenen Kern wiederherstellbar.
        // Das aktuelle neue Ziel wird im Test nur umbenannt, weil Restore niemals
        // einen vorhandenen Namen überschreiben darf.
        std::fs::rename(target.join("datei"), target.join("neue-version")).unwrap();
        let canonical_temp = std::fs::canonicalize(temp.path()).unwrap();
        let canonical_target = std::fs::canonicalize(&target).unwrap();
        let quarantine_vfs = LocalVfs::rooted(&canonical_temp).unwrap();
        let base = canonical_temp.join(".doppeldecker-quarantine");
        let runs = crate::quarantine::list_quarantine_runs(&quarantine_vfs, &base, &None).unwrap();
        let update_run = runs
            .iter()
            .find(|run| Path::new(&run.target_root) == canonical_target)
            .unwrap();
        crate::quarantine::restore_quarantine_entry_for_target(
            &quarantine_vfs,
            &base,
            &update_run.id,
            "datei",
            &None,
            Some(&canonical_target),
        )
        .unwrap();
        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"alt");

        // Ein separater Mirror-Lauf entfernt genau 1 von 20 Einträgen (5 %) und muss
        // das überzählige Ziel ebenfalls als Wiederherstellungspunkt behalten.
        let mirror_source = temp.path().join("mirror-quelle");
        let mirror_target = temp.path().join("mirror-ziel");
        std::fs::create_dir_all(&mirror_source).unwrap();
        std::fs::create_dir_all(&mirror_target).unwrap();
        for index in 0..19 {
            let name = format!("gleich-{index:02}");
            std::fs::write(mirror_source.join(&name), b"x").unwrap();
            std::fs::write(mirror_target.join(&name), b"x").unwrap();
        }
        std::fs::write(mirror_target.join("extra"), b"behalten").unwrap();
        let prepared = prepare_local_real_run(
            &mirror_source,
            &mirror_target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert!(!mirror_target.join("extra").exists());
        assert_eq!(stats.files_deleted, 1);
        assert!(stats.quarantine.is_some());
    }

    #[test]
    fn permit_ist_einmalig_und_laeuft_ab() {
        let (_temp, prepared) = fixture(SyncMode::AddOnly);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        let permit = run.safety.real_permit().unwrap();
        permit
            .consume_and_validate(
                &run.source_vfs,
                &run.source_root,
                &run.target_vfs,
                &run.target_root,
                &run.plan,
            )
            .unwrap();
        assert!(permit
            .consume_and_validate(
                &run.source_vfs,
                &run.source_root,
                &run.target_vfs,
                &run.target_root,
                &run.plan,
            )
            .is_err());

        let (_temp, prepared) = fixture(SyncMode::AddOnly);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let mut run = prepared.confirm(&phrase, false).unwrap();
        run.safety.real_permit_mut().unwrap().force_expired();
        assert!(run.execute(&Cancel::new(), &mut |_| {}).is_err());
    }

    #[test]
    fn vergleichsdto_ist_teil_des_gebundenen_plan_digests() {
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        assert!(!prepared.preview().plan.comparison.rows.is_empty());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let mut run = prepared.confirm(&phrase, false).unwrap();

        // Selbst eine reine Änderung an der sichtbaren Kategoriesumme muss das Permit
        // ungültig machen. Sonst könnte die UI etwas anderes bestätigen als der Kern.
        run.plan.comparison.categories.additions += 1;
        assert!(run.execute(&Cancel::new(), &mut |_| {}).is_err());
        assert!(!temp.path().join("ziel/neu.txt").exists());
    }

    #[test]
    fn mirror_zweitscan_stoppt_inhaltsdrift_vor_entfernung() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        for index in 0..19 {
            let name = format!("datei-{index:02}");
            std::fs::write(source.join(&name), b"aaaa").unwrap();
            std::fs::write(target.join(&name), b"aaaa").unwrap();
        }
        std::fs::write(target.join("entfernen"), b"alt").unwrap();
        let prepared = prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(prepared.preview().plan.delete_count, 1);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();

        // Größe und mtime bleiben gleich; nur der zwingende zweite ContentHash-Plan
        // kann diese Änderung sicher erkennen.
        let changed = source.join("datei-00");
        let mtime = std::fs::metadata(&changed).unwrap().modified().unwrap();
        std::fs::write(&changed, b"bbbb").unwrap();
        filetime::set_file_mtime(&changed, filetime::FileTime::from_system_time(mtime)).unwrap();
        assert!(run.execute(&Cancel::new(), &mut |_| {}).is_err());
        assert!(target.join("entfernen").exists());
    }

    #[test]
    fn zielaustausch_stoppt_vor_realmutation() {
        use std::os::unix::fs::symlink;
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        let target = temp.path().join("ziel");
        let old_target = temp.path().join("ziel-alt");
        std::fs::rename(&target, &old_target).unwrap();
        symlink(&old_target, &target).unwrap();
        assert!(run.execute(&Cancel::new(), &mut |_| {}).is_err());
        assert!(!old_target.join("neu.txt").exists());
    }

    #[test]
    fn injizierter_mountwechsel_stoppt_vor_realmutation() {
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let mut run = prepared.confirm(&phrase, false).unwrap();
        let permit = run.safety.real_permit_mut().unwrap();
        let RootIdentity::Local(identity) = &mut permit.target_identity else {
            panic!("Test erwartet ein lokales Permit");
        };
        identity.mount.mount_id ^= 1;
        assert!(run.execute(&Cancel::new(), &mut |_| {}).is_err());
        assert!(!temp.path().join("ziel/neu.txt").exists());
    }

    #[test]
    fn unvollstaendiger_listing_proof_wird_nie_freigegeben() {
        let root = RootIdentity::Local(LocalRootIdentity {
            device: 1,
            inode: 2,
            is_directory: true,
            mount: crate::vfs::LocalMountIdentity {
                mount_id: 3,
                unique_mount_id: true,
                fsid: vec![4],
                volume_id: None,
                fs_type: "test".into(),
                mount_flags: 0,
            },
            display_path: PathBuf::from("/test"),
        });
        let proof = ListingProof {
            root_identity: root,
            directories_started: 2,
            directories_completed: 1,
            entries_seen: 0,
            listing_digest: [0; 32],
            provider_token: Some(vec![1]),
            errors: vec![ListingError {
                path: "seite-2".into(),
                reason: "Continuation-Token nicht vollständig gelesen".into(),
            }],
            complete: false,
        };
        assert!(require_complete("Mock-Provider", &proof).is_err());
    }

    #[test]
    fn realmodus_listing_verwirft_traversal_rohnamendrift_und_duplikate() {
        fn listed(name: &str, raw_name: &[u8]) -> ProofListEntry {
            ProofListEntry {
                entry: crate::Entry {
                    name: name.into(),
                    is_dir: false,
                    is_file: true,
                    is_symlink: false,
                    size: 1,
                    mtime: None,
                    created: None,
                    owner: None,
                    group: None,
                    extension: None,
                    mode: None,
                    device: None,
                    inode: None,
                    hard_links: None,
                },
                raw_name: raw_name.to_vec(),
                safely_decoded: true,
                provider_token: None,
            }
        }

        for entries in [
            vec![listed("../ausbruch", b"../ausbruch")],
            vec![listed("anzeige", b"anderer-rohname")],
            vec![listed("gleich", b"gleich"), listed("gleich", b"gleich")],
        ] {
            let listing = crate::vfs::CompleteDirectoryListing {
                entries,
                provider_token: None,
                complete: true,
            };
            assert_eq!(
                crate::error_code(
                    &require_complete_safe_listing(&listing, "Testlisting").unwrap_err()
                ),
                crate::ErrorCode::InvalidData
            );
        }
    }

    #[test]
    fn mengenlimits_und_leere_mirror_quelle_sperren_vor_probe() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("extra"), b"x").unwrap();
        assert!(prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Mirror,
                compare: Compare::ContentHash,
                ..Default::default()
            }
        )
        .is_err());

        for index in 0..101 {
            let name = format!("ersetzen-{index:03}");
            std::fs::write(source.join(&name), b"neu").unwrap();
            std::fs::write(target.join(&name), b"alt").unwrap();
            filetime::set_file_mtime(
                source.join(&name),
                filetime::FileTime::from_unix_time(20, 0),
            )
            .unwrap();
            filetime::set_file_mtime(
                target.join(&name),
                filetime::FileTime::from_unix_time(10, 0),
            )
            .unwrap();
        }
        assert!(prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn ungenuegender_platz_sperrt_vor_permit() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let canonical_temp = std::fs::canonicalize(temp.path()).unwrap();
        let target_vfs = LocalVfs::rooted(&canonical_temp).unwrap();
        let available = target_vfs
            .available_bytes(&std::fs::canonicalize(&target).unwrap())
            .unwrap();
        let file = std::fs::File::create(source.join("zu-gross")).unwrap();
        file.set_len(available.saturating_add(1024_u64.pow(4)))
            .unwrap();
        assert!(prepare_local_real_run(
            &source,
            &target,
            SyncOptions {
                mode: SyncMode::AddOnly,
                ..Default::default()
            }
        )
        .is_err());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    }

    #[test]
    fn audit_ist_append_only_und_enthaelt_keine_pfade_oder_inhalte() {
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let source_text = temp.path().join("quelle").to_string_lossy().into_owned();
        let target_text = temp.path().join("ziel").to_string_lossy().into_owned();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        let audit = temp.path().join("audit/real-runs.jsonl");
        run.execute_with_audit(&audit, &Cancel::new(), &mut |_| {})
            .unwrap();
        let text = std::fs::read_to_string(audit).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"status\":\"started\""));
        assert!(text.contains("\"status\":\"completed\""));
        assert!(!text.contains(&source_text));
        assert!(!text.contains(&target_text));
        assert!(!text.contains("neu.txt"));
    }

    #[test]
    fn audit_fehler_stoppt_vor_erster_mutation() {
        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        let blocker = temp.path().join("audit-blocker");
        std::fs::write(&blocker, b"kein ordner").unwrap();
        assert!(run
            .execute_with_audit(
                &blocker.join("real-runs.jsonl"),
                &Cancel::new(),
                &mut |_| {}
            )
            .is_err());
        assert!(!temp.path().join("ziel/neu.txt").exists());
    }

    #[test]
    fn audit_symlink_stoppt_vor_erster_mutation_und_bleibt_unveraendert() {
        use std::os::unix::fs::symlink;

        let (temp, prepared) = fixture(SyncMode::AddOnly);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        let foreign = temp.path().join("fremde-datei.txt");
        let audit = temp.path().join("audit-link.jsonl");
        std::fs::write(&foreign, b"unveraendert").unwrap();
        symlink(&foreign, &audit).unwrap();

        let error = run
            .execute_with_audit(&audit, &Cancel::new(), &mut |_| {})
            .unwrap_err();

        assert!(
            error.raw_os_error() == Some(libc::ELOOP)
                || error.kind() == io::ErrorKind::InvalidInput,
            "unerwarteter Audit-Symlinkfehler: {error}"
        );
        assert_eq!(std::fs::read(&foreign).unwrap(), b"unveraendert");
        assert!(!temp.path().join("ziel/neu.txt").exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn nicht_utf8_name_macht_listing_unvollstaendig() {
        use std::os::unix::ffi::OsStringExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(std::ffi::OsString::from_vec(vec![0xff])), b"x").unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let identity = RootIdentity::Local(vfs.local_root_identity(&root).unwrap());
        let proof = build_listing_proof(&vfs, &root, identity);
        assert!(!proof.complete);
        assert!(proof
            .errors
            .iter()
            .any(|error| error.reason.contains("UTF-8")));
    }
}
