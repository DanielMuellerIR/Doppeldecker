//! Ordner-Synchronisation — das Herzstück von Doppeldecker.
//!
//! Zweistufig, damit nie „blind" etwas gelöscht wird:
//! 1. [`plan`]    berechnet einen **Dry-Run** ([`SyncPlan`]): eine Liste geplanter
//!    Aktionen, ohne irgendetwas zu verändern. Die UI zeigt das zur Bestätigung.
//! 2. [`execute`] führt einen bestätigten Plan aus und meldet Fortschritt.
//!
//! Der schnelle Standard vergleicht Größe + Änderungszeit ([`Compare::SizeAndTime`]).
//! Optional liest [`Compare::ContentHash`] beide Inhalte vollständig per BLAKE3 und
//! bindet die Digests an den bestätigten Plan. Symlinks werden bewusst übersprungen.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::metadata::{MetadataManifest, MetadataProfile};
use crate::vfs::{Entry, ProofListEntry, Vfs};

mod atomic;
mod exclude;
mod execute;
mod metadata;
mod plan;
mod safety;

use atomic::{
    copy_one, create_hardlink_one, current_optional_copy_snapshot, hash_file_hex, hash_tree,
    plan_changed, quarantine_label, regular_copy_snapshot, remove_or_quarantine,
    start_sync_quarantine_run, verify_sync_quarantine_run, CopyFileRequest, CopyResult,
    HardlinkRequest, RemoveOrQuarantineRequest,
};
pub use execute::{execute, ExecuteRequest};
use metadata::{
    summarize_xattrs, transfer_planned_metadata, validate_metadata_transfer,
    verify_planned_metadata, verify_selected_metadata, verify_temp_metadata,
    PlannedMetadataRequest,
};
#[cfg(test)]
use plan::build_metadata_report;
pub use plan::plan;
use plan::planned_fork_bytes;
pub(crate) use safety::canonical_existing;
#[cfg(debug_assertions)]
pub use safety::HeadlessTestPermit;
use safety::{guard_path, validate_anchor_identity, validate_safety};
pub use safety::{
    validate_local_sync_roots, validate_test_sandbox_root, ExecutionMode, Safety,
    TestSandboxPermit, TEST_SANDBOX_MARKER, TEST_SANDBOX_MARKER_CONTENT,
};

/// In welche Richtung/wie stark angeglichen wird.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SyncMode {
    /// Nur Einträge kopieren, die im Ziel noch nicht existieren. Vorhandene Dateien,
    /// Ordner und Typkonflikte bleiben vollständig unangetastet. Das ist der sichere
    /// Standard für erste Läufe.
    AddOnly,
    /// Das Ziel wird soweit für reguläre Dateien sicher möglich an die Quelle
    /// angeglichen: neue/geänderte Dateien werden kopiert, und Einträge nur im Ziel
    /// werden entfernt. Symlinks, Spezialdateien und Typkonflikte bleiben unangetastet.
    Mirror,
    /// Nur **hinzufügen/aktualisieren**: neue und (im Ziel ältere) Dateien werden
    /// kopiert. Im Ziel wird nie gelöscht; Typkonflikte bleiben zur manuellen Klärung
    /// unangetastet.
    Update,
}

/// Wie entschieden wird, ob zwei gleichnamige Dateien „gleich" sind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Compare {
    /// Schnell: gleich, wenn Größe UND Änderungszeit (innerhalb einer Toleranz)
    /// übereinstimmen. Standard.
    SizeAndTime,
    /// Inhalt exakt per BLAKE3 vergleichen. Langsamer, erkennt aber auch gleiche Größe
    /// und gleiche `mtime` bei unterschiedlichen Bytes.
    ContentHash,
}

/// Optionen für einen Sync-Lauf.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncOptions {
    pub mode: SyncMode,
    pub compare: Compare,
    /// Umfang des Metadatenvertrags. `Portable` bleibt der sichere Standard; die
    /// nativen Profile werden erst nach ausdrücklicher Auswahl aktiv.
    #[serde(default)]
    pub metadata: MetadataProfile,
    /// Toleranz beim Zeitvergleich in Sekunden. Nötig, weil Dateisysteme
    /// unterschiedliche Zeitauflösung haben (FAT z. B. 2 s).
    pub mtime_tolerance: i64,
    /// Bewusst unsichtbare relative Pfade. Muster ohne `/` gelten für Namen auf
    /// jeder Ebene; `*`/`?` gelten innerhalb einer Ebene, `**` über mehrere Ebenen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            mode: SyncMode::AddOnly,
            compare: Compare::SizeAndTime,
            metadata: MetadataProfile::Portable,
            mtime_tolerance: 2,
            exclude: Vec::new(),
        }
    }
}

impl SyncOptions {
    /// Prüft ausschließlich die Optionssyntax. Der Aufrufer kann das vor dem Öffnen
    /// einer Providerverbindung tun; der Planer prüft zur Sicherheit erneut.
    pub fn validate(&self) -> std::io::Result<()> {
        if self.mtime_tolerance < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Zeitvergleichs-Toleranz darf nicht negativ sein",
            ));
        }
        exclude::ExcludeMatcher::new(&self.exclude).map(|_| ())
    }
}

/// Liest einen optionalen Eintrag, ohne Provider-, Berechtigungs- oder Protokollfehler
/// als vermeintliches „fehlt“ zu verschlucken. `Vfs::exists` ist für Komfortabfragen
/// boolesch; an einer Planbindungsgrenze ist nur echtes `NotFound` Abwesenheit.
fn optional_stat(vfs: &dyn Vfs, path: &Path) -> io::Result<Option<Entry>> {
    match vfs.stat(path) {
        Ok(entry) => Ok(Some(entry)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Nimmt nur ein tatsächlich vollständiges und widerspruchsfreies Providerlisting an.
/// Der Planner besitzt kein „teilweise vollständig“-Feld; ein `Ok`-Plan darf deshalb
/// niemals eine vom Provider ausdrücklich als unvollständig markierte Liste ausgeben.
fn complete_listing(vfs: &dyn Vfs, path: &Path) -> io::Result<Vec<ProofListEntry>> {
    let listing = vfs.list_complete(path)?;
    if !listing.complete {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Verzeichnislisting ist unvollständig: {}", path.display()),
        ));
    }

    let mut raw_names = HashSet::with_capacity(listing.entries.len());
    for item in &listing.entries {
        let name = Path::new(&item.entry.name);
        if item.entry.name.is_empty()
            || item.raw_name.is_empty()
            || item.entry.name.contains('/')
            || item.entry.name.contains('\0')
            || name.components().count() != 1
            || !matches!(
                name.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Verzeichnislisting enthält keinen sicheren Einzelnamen: {}",
                    item.entry.name
                ),
            ));
        }
        if item.safely_decoded && item.raw_name != item.entry.name.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Verzeichnislisting widerspricht seinem Roh-Namensbeleg: {}",
                    item.entry.name
                ),
            ));
        }
        if !raw_names.insert(item.raw_name.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Verzeichnislisting enthält einen doppelten Namen: {}",
                    item.entry.name
                ),
            ));
        }
    }
    Ok(listing.entries)
}

/// Im bestätigten Plan gebundener Teil des nativen Metadatentransfers. Die großen
/// Werte selbst bleiben außerhalb des Plans; gespeichert werden nur Manifest-Digests.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataTransfer {
    pub source: MetadataManifest,
    #[serde(default)]
    pub target: Option<MetadataManifest>,
    /// Numerischen Nutzer und Gruppe vor allen rechteabhängigen Metadaten setzen.
    #[serde(default)]
    pub owner: bool,
    /// ACL nach Owner und vor den gekoppelten Modusbits exakt anwenden.
    #[serde(default)]
    pub acl: bool,
    /// Die Quelle besitzt eine ACL, deren Modell am Ziel nicht verlustfrei
    /// darstellbar ist. BestEffort bilanziert dies, Strict lehnt den Plan ab.
    #[serde(default)]
    pub incompatible_acl_model: bool,
    /// Normale `rwx`-Bits nach Owner und xattrs exakt setzen.
    #[serde(default)]
    pub normal_mode: bool,
    /// setuid/setgid/sticky ausschließlich als letzte Metadatenphase setzen.
    #[serde(default)]
    pub special_mode_bits: bool,
    pub xattrs: bool,
    pub resource_fork: bool,
    #[serde(default)]
    pub sparse_extents: bool,
    /// Das Quellmanifest trägt eine laufbezogene Hardlink-Gruppe. Der eigentliche
    /// Wiederaufbau erfolgt als eigene `CreateHardlink`-Aktion.
    #[serde(default)]
    pub hardlinks: bool,
}

/// Eine für Nutzer verständliche Klasse des nativen Metadatenvertrags. Die Werte sind
/// absichtlich protokollneutral und werden unverändert an GUI und CLI serialisiert.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum MetadataClass {
    Owner,
    Acl,
    NormalMode,
    SpecialModeBits,
    Xattrs,
    ResourceFork,
    SparseExtents,
    Hardlinks,
}

/// Bilanz einer Metadatenklasse in Vorschau und Abschlussbericht.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataClassStats {
    pub class: MetadataClass,
    pub transferred: u64,
    pub omitted: u64,
}

/// Pfadgenaue Auslassung im Best-Effort-Profil. Ein strikter Plan darf keinen solchen
/// Eintrag enthalten und wird deshalb noch während der reinen Vorschau abgelehnt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataIssue {
    pub path: String,
    pub class: MetadataClass,
    pub kind: MetadataIssueKind,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MetadataIssueKind {
    Unsupported,
    PrivilegeFailure,
    IncompatibleAclModel,
}

/// Persistierbarer Identitätsbeleg eines Zielnamens bis zur Quarantänewirkung.
///
/// Größe, Zeit und Modus allein erkennen keinen gleichartig aufgebauten
/// Nachfolger. Lokale Ziele binden deshalb zusätzlich Gerät und Inode, Provider
/// ihren eigenen Versions-Token. Der rohe Name verhindert, dass eine verlustbehaftete
/// Anzeige einen anderen Eintrag bezeichnet. Inhalts- und Baumhash bleiben in der
/// jeweiligen Aktion, weil sie zugleich die fachliche Vergleichsgrundlage bilden.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncTargetProof {
    raw_name: Vec<u8>,
    provider_token: Option<Vec<u8>>,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
    size: u64,
    mtime: Option<i64>,
    mode: Option<u32>,
    device: Option<u64>,
    inode: Option<u64>,
}

impl SyncTargetProof {
    fn from_listed(listed: &ProofListEntry) -> io::Result<Self> {
        if !listed.safely_decoded || listed.raw_name != listed.entry.name.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Sync-Zielbeleg besitzt keinen eindeutigen UTF-8-Namen",
            ));
        }
        Ok(Self {
            raw_name: listed.raw_name.clone(),
            provider_token: listed.provider_token.clone(),
            is_dir: listed.entry.is_dir,
            is_file: listed.entry.is_file,
            is_symlink: listed.entry.is_symlink,
            size: listed.entry.size,
            mtime: listed.entry.mtime,
            mode: listed.entry.mode,
            device: listed.entry.device,
            inode: listed.entry.inode,
        })
    }

    fn matches_entry(&self, path: &Path, entry: &Entry) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(str::as_bytes)
            == Some(self.raw_name.as_slice())
            && self.matches_identity(entry)
    }

    fn matches_identity(&self, entry: &Entry) -> bool {
        self.is_dir == entry.is_dir
            && self.is_file == entry.is_file
            && self.is_symlink == entry.is_symlink
            && self.size == entry.size
            && self.mtime == entry.mtime
            && self.mode == entry.mode
            && self.device == entry.device
            && self.inode == entry.inode
    }

    fn matches_listed_at(&self, path: &Path, listed: &ProofListEntry) -> bool {
        listed.safely_decoded
            && self.matches_entry(path, &listed.entry)
            && self.provider_token == listed.provider_token
    }

    fn matches_moved_listed(&self, listed: &ProofListEntry) -> bool {
        listed.safely_decoded
            && self.matches_identity(&listed.entry)
            && self.provider_token == listed.provider_token
    }

    fn matches_stable_directory_at(&self, path: &Path, listed: &ProofListEntry) -> bool {
        let stable_identity = match (&self.provider_token, &listed.provider_token) {
            (Some(expected), Some(current)) => expected == current,
            _ => match (
                self.device,
                self.inode,
                listed.entry.device,
                listed.entry.inode,
            ) {
                (Some(expected_device), Some(expected_inode), Some(device), Some(inode)) => {
                    expected_device == device && expected_inode == inode
                }
                _ => false,
            },
        };
        stable_identity
            && listed.safely_decoded
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.as_bytes() == self.raw_name)
            && listed.raw_name == self.raw_name
            && self.is_dir
            && listed.entry.is_dir
            && !self.is_symlink
            && !listed.entry.is_symlink
    }

    fn as_listed_for_path(&self, path: &Path) -> io::Result<ProofListEntry> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Sync-Zielbeleg besitzt keinen sicheren Pfadnamen",
                )
            })?;
        if name.as_bytes() != self.raw_name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Sync-Zielbeleg gehört nicht zum angegebenen Pfad",
            ));
        }
        Ok(ProofListEntry {
            entry: Entry {
                name: name.to_owned(),
                is_dir: self.is_dir,
                is_file: self.is_file,
                is_symlink: self.is_symlink,
                size: self.size,
                mtime: self.mtime,
                created: None,
                owner: None,
                group: None,
                extension: None,
                mode: self.mode,
                device: self.device,
                inode: self.inode,
                hard_links: None,
            },
            raw_name: self.raw_name.clone(),
            safely_decoded: true,
            provider_token: self.provider_token.clone(),
        })
    }

    fn validates_file_action(
        &self,
        path: &str,
        size: u64,
        mtime: Option<i64>,
        mode: Option<u32>,
    ) -> bool {
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.as_bytes() == self.raw_name)
            && self.is_file
            && !self.is_dir
            && !self.is_symlink
            && self.size == size
            && self.mtime == mtime
            && self.mode == mode
    }

    fn validates_directory_action(&self, path: &str) -> bool {
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.as_bytes() == self.raw_name)
            && self.is_dir
            && !self.is_file
            && !self.is_symlink
    }
}

/// Vollständige, an den bestätigten Plan gebundene Metadatenbilanz.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataReport {
    pub profile: MetadataProfile,
    pub content_changes: u64,
    pub metadata_changes: u64,
    pub hardlink_actions: u64,
    pub unsupported_metadata: u64,
    pub privilege_failures: u64,
    pub incompatible_acl_models: u64,
    pub classes: Vec<MetadataClassStats>,
    pub issues: Vec<MetadataIssue>,
    /// Nur wahr, wenn `NativeStrict` nach dem vollständigen Scan keine Auslassung hat.
    pub strict_verified: bool,
}

/// Für die Vergleichsansicht relevante Art eines Eintrags. Diese kleine, stabile
/// Menge vermeidet, dass die Oberfläche aus mehreren booleschen Feldern selbst eine
/// Dateisystem-Semantik ableiten muss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComparisonEntryKind {
    Directory,
    File,
    Symlink,
    Unsupported,
}

/// Anzeige-Metadaten eines Eintrags aus genau dem Listing, das auch den Plan erzeugt
/// hat. Lokale Identitäten wie Inode oder Device bleiben absichtlich außen vor: Sie
/// sind Sicherheitsbelege, aber keine verlässlichen, providerübergreifenden Werte.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComparisonEntry {
    pub kind: ComparisonEntryKind,
    pub size: u64,
    pub mtime: Option<i64>,
    pub created: Option<i64>,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub extension: Option<String>,
    pub mode: Option<u32>,
}

impl From<&crate::vfs::Entry> for ComparisonEntry {
    fn from(entry: &crate::vfs::Entry) -> Self {
        let kind = if entry.is_symlink {
            ComparisonEntryKind::Symlink
        } else if entry.is_dir {
            ComparisonEntryKind::Directory
        } else if entry.is_file {
            ComparisonEntryKind::File
        } else {
            ComparisonEntryKind::Unsupported
        };
        Self {
            kind,
            size: entry.size,
            mtime: entry.mtime,
            created: entry.created,
            owner: entry.owner.clone(),
            group: entry.group.clone(),
            extension: entry.extension.clone(),
            mode: entry.mode,
        }
    }
}

/// Grobe Wirkungsklasse für das sichtbare Badge. Die ausführbare, feinere Semantik
/// bleibt weiterhin ausschließlich in [`Action`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComparisonAction {
    Add,
    Update,
    Quarantine,
    Skip,
}

/// Seite, auf der die geplante Wirkung sichtbar wird. Ein Einweg-Sync verändert nur
/// das Ziel; ausgelassene Einträge tragen `Neither`. `Source` ist bereits Teil des
/// DTO-Vertrags, damit eine spätere gemeinsame Vergleichsansicht nicht neue Werte
/// erfinden muss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComparisonSide {
    Source,
    Target,
    Neither,
}

/// Eine geänderte oder bewusst ausgelassene Position in der Baumansicht.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComparisonRow {
    pub path: String,
    pub depth: u32,
    /// Kumulative relative Ahnenpfade, etwa `["Fotos", "Fotos/2026"]`.
    pub ancestors: Vec<String>,
    pub action: ComparisonAction,
    pub affected_side: ComparisonSide,
    pub source: Option<ComparisonEntry>,
    pub target: Option<ComparisonEntry>,
}

/// Zeilensummen der vier sichtbaren Wirkungsklassen. Sie zählen Planzeilen, nicht die
/// rekursive Inhaltszahl eines als Ganzes quarantänisierten Verzeichnisses.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComparisonCategoryCounts {
    pub additions: u64,
    pub updates: u64,
    pub quarantines: u64,
    pub skipped: u64,
}

/// Plan-gebundener DTO für die zweispaltige Vergleichsansicht.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncComparison {
    pub rows: Vec<ComparisonRow>,
    pub categories: ComparisonCategoryCounts,
}

/// Eine einzelne geplante Aktion. Die relativen Pfade sind immer relativ zum jeweiligen
/// Sync-Root (Quelle bzw. Ziel).
///
/// Serialisiert für die UI als `{ "type": "CopyFile", "path": "...", "size": 123 }`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    /// Verzeichnis im Ziel anlegen (existiert dort noch nicht).
    CreateDir {
        path: String,
        source_mode: Option<u32>,
        #[serde(default)]
        source_mtime: Option<i64>,
        #[serde(default)]
        metadata: Option<MetadataTransfer>,
    },
    /// Datei neu ins Ziel kopieren (existiert dort noch nicht).
    CopyFile {
        path: String,
        size: u64,
        source_mtime: Option<i64>,
        source_mode: Option<u32>,
        #[serde(default)]
        source_hash: Option<String>,
        #[serde(default)]
        metadata: Option<MetadataTransfer>,
    },
    /// Vorhandene Ziel-Datei überschreiben (Inhalt unterscheidet sich).
    UpdateFile {
        path: String,
        size: u64,
        source_mtime: Option<i64>,
        source_mode: Option<u32>,
        target_size: u64,
        target_mtime: Option<i64>,
        target_mode: Option<u32>,
        #[serde(default)]
        target_proof: SyncTargetProof,
        #[serde(default)]
        source_hash: Option<String>,
        #[serde(default)]
        target_hash: Option<String>,
        /// `true`, wenn nur native Metadaten abweichen. Die Datei wird trotzdem über
        /// eine neue Temp-Datei ersetzt, damit ein Fehler das bisherige Ziel schont.
        #[serde(default)]
        metadata_only: bool,
        #[serde(default)]
        metadata: Option<MetadataTransfer>,
    },
    /// Einen weiteren Namen atomar mit der bereits materialisierten Zieldatei
    /// verknüpfen. Ein vorhandenes Ziel wird wie ein Datei-Update quarantänisiert.
    CreateHardlink {
        path: String,
        link_to: String,
        size: u64,
        source_mtime: Option<i64>,
        source_mode: Option<u32>,
        source_hash: String,
        #[serde(default)]
        target_size: Option<u64>,
        #[serde(default)]
        target_mtime: Option<i64>,
        #[serde(default)]
        target_mode: Option<u32>,
        #[serde(default)]
        target_proof: Option<SyncTargetProof>,
        #[serde(default)]
        target_hash: Option<String>,
        metadata: MetadataTransfer,
    },
    /// Inhalt ist bereits passend, aber die POSIX-Rechte unterscheiden sich. Wird
    /// nach allen Kopien ausgeführt, damit ein restriktiver Ordnermodus den Lauf nicht
    /// vorzeitig aus seinem eigenen Zielbaum aussperrt.
    SetMode {
        path: String,
        source_mode: u32,
        target_mode: Option<u32>,
        is_dir: bool,
        #[serde(default)]
        content_hash: Option<String>,
    },
    /// Native Metadaten eines Verzeichnisses post-order anwenden. Quelle und Ziel
    /// bleiben über vollständige Manifeste an die bestätigte Vorschau gebunden.
    SetDirectoryMetadata {
        path: String,
        source_mtime: Option<i64>,
        target_mtime: Option<i64>,
        metadata: MetadataTransfer,
    },
    /// Datei im Ziel löschen (nur im Mirror-Modus; existiert nicht in der Quelle).
    DeleteFile {
        path: String,
        size: u64,
        target_mtime: Option<i64>,
        target_mode: Option<u32>,
        #[serde(default)]
        target_proof: SyncTargetProof,
        #[serde(default)]
        target_hash: Option<String>,
    },
    /// Verzeichnis im Ziel samt Inhalt löschen (nur Mirror).
    DeleteDir {
        path: String,
        /// Rekursive Zahl aller betroffenen Einträge einschließlich des Ordners selbst.
        entries: u64,
        /// Rekursive Größe aller regulären Dateien im betroffenen Teilbaum.
        bytes: u64,
        #[serde(default)]
        target_proof: SyncTargetProof,
        #[serde(default)]
        target_tree_hash: Option<String>,
    },
    /// Symlink in Quelle oder Ziel — wird aus Sicherheitsgründen nicht verändert.
    SkipSymlink { path: String },
    /// Datei-/Ordner-Typkonflikt. Automatisches Ersetzen wäre destruktiv und bleibt
    /// deshalb einer späteren, explizit wiederherstellbaren Konfliktbehandlung vorbehalten.
    SkipConflict { path: String },
    /// Socket, FIFO, Gerät oder anderer nicht regulärer Eintrag.
    SkipUnsupported { path: String },
}

/// Ergebnis der Dry-Run-Berechnung: alle Aktionen plus eine Zusammenfassung fürs UI.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncPlan {
    pub actions: Vec<Action>,
    #[serde(default)]
    pub local_parent_inheritance: Vec<crate::metadata::LocalParentInheritance>,
    /// Aus demselben Listing-Lauf wie `actions` abgeleitete Vergleichsansicht. Das
    /// Feld liegt im Plan und ist damit Bestandteil von Driftvergleich und Digest.
    #[serde(default)]
    pub comparison: SyncComparison,
    pub copy_count: u64,
    pub update_count: u64,
    /// Vorhandene Dateien/Ordner, deren POSIX-Rechte ohne Inhaltskopie angepasst werden.
    pub metadata_count: u64,
    pub delete_count: u64,
    pub skip_count: u64,
    /// Rekursive Zahl aller in der Quelle gesehenen Einträge.
    pub source_count: u64,
    /// Rekursive Zahl aller vor dem Lauf im Ziel vorhandenen Einträge.
    pub target_count: u64,
    /// Gesamtgröße der regulären Dateien, die aus dem Ziel entfernt würden.
    pub delete_bytes: u64,
    /// Summe der Bytes, die kopiert/aktualisiert werden (für die Fortschrittsanzeige).
    pub bytes_to_transfer: u64,
    /// Zahl und Bytes der im optionalen Inhaltsmodus gelesenen Hash-Datenströme.
    pub hashed_files: u64,
    pub bytes_hashed: u64,
    /// Klassen- und pfadgenaue Bilanz des gewählten Metadatenprofils.
    #[serde(default)]
    pub metadata_report: MetadataReport,
}

/// Bilanz eines ausgeführten Sync-Laufs.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SyncStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_execution: Option<crate::real_mode::LocalSyncExecution>,
    pub files_copied: u64,
    /// Tatsächlich abgeschlossene schwächere Ersetzungen, deren Altversion nicht
    /// in Quarantäne liegt. Die Vorschau zählt den geplanten Umfang getrennt.
    pub irreversible_replacements: u64,
    pub files_deleted: u64,
    pub metadata_updated: u64,
    pub bytes_copied: u64,
    pub cancelled: bool,
    /// War es ein Testlauf (Simulation ohne echte Schreib-/Löschoperationen)?
    pub dry_run: bool,
    /// Wiederherstellungsordner für ersetzte und entfernte Zielobjekte, falls genutzt.
    pub quarantine: Option<String>,
    /// Eigene Zwischenstände, deren Cleanup nicht bewiesen werden konnte.
    pub temp_recovery_paths: Vec<String>,
    /// Derselbe Bericht wie in der bestätigten Vorschau. Ein erfolgreicher Lauf hat
    /// jede als übertragen gezählte Klasse anschließend exakt rückgelesen.
    pub metadata_report: MetadataReport,
}

#[cfg(test)]
mod metadata_report_tests {
    use super::*;
    use crate::metadata::{BlobSummary, NamedBlobSummary, Owner};

    fn manifest() -> MetadataManifest {
        MetadataManifest::new(
            Some(Owner { uid: 501, gid: 20 }),
            0o4755,
            None,
            vec![NamedBlobSummary {
                name: b"user.test".to_vec(),
                value: BlobSummary::from_bytes(b"wert"),
            }],
            None,
            None,
            Some(7),
        )
        .unwrap()
    }

    #[test]
    fn best_effort_bilanziert_uebertragung_und_auslassung_pfadgenau() {
        let actions = vec![Action::CopyFile {
            path: "programm".into(),
            size: 4,
            source_mtime: Some(1),
            source_mode: Some(0o755),
            source_hash: None,
            metadata: Some(MetadataTransfer {
                source: manifest(),
                target: None,
                owner: true,
                acl: false,
                incompatible_acl_model: false,
                normal_mode: true,
                special_mode_bits: true,
                xattrs: false,
                resource_fork: false,
                sparse_extents: false,
                hardlinks: true,
            }),
        }];

        let report = build_metadata_report(MetadataProfile::NativeBestEffort, &actions);

        assert!(report.classes.iter().any(|item| {
            item.class == MetadataClass::Owner && item.transferred == 1 && item.omitted == 0
        }));
        assert!(report.classes.iter().any(|item| {
            item.class == MetadataClass::Xattrs && item.transferred == 0 && item.omitted == 1
        }));
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].path, "programm");
        assert_eq!(report.issues[0].class, MetadataClass::Xattrs);
        assert_eq!(report.content_changes, 1);
        assert_eq!(report.metadata_changes, 1);
        assert_eq!(report.hardlink_actions, 0);
        assert_eq!(report.unsupported_metadata, 1);
        assert_eq!(report.privilege_failures, 0);
        assert_eq!(report.incompatible_acl_models, 0);
    }

    #[test]
    fn metadatenbericht_bleibt_im_json_maschinenlesbar() {
        let report = MetadataReport {
            profile: MetadataProfile::NativeStrict,
            content_changes: 0,
            metadata_changes: 1,
            hardlink_actions: 1,
            unsupported_metadata: 0,
            privilege_failures: 0,
            incompatible_acl_models: 0,
            classes: vec![MetadataClassStats {
                class: MetadataClass::Hardlinks,
                transferred: 2,
                omitted: 0,
            }],
            issues: vec![],
            strict_verified: true,
        };

        let json = serde_json::to_string(&report).unwrap();
        let decoded: MetadataReport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, report);
    }
}
