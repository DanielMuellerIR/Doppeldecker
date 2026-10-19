//! Persistente Zustandsmaschine für Datei- und Sync-Operationen.
//!
//! Die Queue speichert nur Absichten und bestätigte Pläne, niemals offene Handles,
//! Secrets oder kurzlebige Permits. Der App-Scheduler validiert jeden Claim erneut und
//! markiert ihn erst unmittelbar vor einer Mutation dauerhaft als `Running`.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use crate::atomic_write::atomic_write_local;
use crate::atomic_write::AtomicWriteMode;
use crate::fileops::{
    capture_file_conflict_proofs, validate_best_effort_file_replacements, FileConflictProof,
    FileOperationActuals,
};
use crate::quarantine::{
    complete_quarantine_run_if_matches, record_quarantine_intent_with_proof, QuarantineReason,
};
use crate::real_mode::reserve_quarantine_run_name;
use crate::vfs::{
    BoundContentDigest, BoundParentsRenameRequest, FileReplaceClass, MutationFailure,
};
use crate::{execute_remote_delete, RemoteDeleteMode, RemoteDeletePlan};
use crate::{
    find_recursive_conflicts, validate_local_sync_roots, Cancel, FileOperationRequest, LocalVfs,
    OpStats, OverwritePolicy, Progress, Safety, SyncOptions, SyncPlan, SyncStats, Vfs,
};
use crate::{
    prepare_ftps_download, prepare_ftps_upload, prepare_provider_transfer, prepare_sftp_download,
    prepare_sftp_upload, prepare_smb_download, prepare_smb_upload, prepare_webdav_download,
    prepare_webdav_upload, FtpsConnectionConfig, FtpsVfs, PreparedSftpRealRun, SecretResolver,
    SftpConnectionConfig, SftpVfs, SmbConnectionConfig, SmbVfs, WebDavConnectionConfig, WebDavVfs,
};
use crate::{
    prepare_local_bidirectional_with_decisions, prepare_provider_bidirectional_with_decisions,
    ConflictResolution, LocalBidirectionalStats, PairId, PreparedLocalBidirectionalRun,
    ProviderEndpoint,
};

pub const QUEUE_JOURNAL_VERSION: u32 = 27;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const LEGACY_UNBOUND_WEBDAV_ETAG: &str = "__doppeldecker_legacy_unbound__";
const LEGACY_WEBDAV_REPLAN_ERROR: &str = "Dieser ältere WebDAV-Löschplan besitzt keine eindeutig gebundene Server- oder Root-Identität. Er wurde nicht ausgeführt und darf nicht wiederholt werden; Zielzustand neu laden und die Auswahl neu bestätigen.";
/// Sichtbarer Grund eines ausdrücklich abgelehnten Bestätigungswunsches. Er
/// unterscheidet den Nutzerabbruch von einem Ausführungsfehler.
const DISCARDED_ATTENTION_ERROR: &str =
    "Die neu entdeckte Lage wurde nicht übernommen; die Operation wurde verworfen";
const LEGACY_WEBDAV_UNCLEAR_ERROR: &str = "Dieser ältere WebDAV-Löschplan besitzt keine eindeutig gebundene Server- oder Root-Identität und war beim letzten Prozess nicht mehr sicher mutationsfrei. Eine mögliche Teilwirkung muss manuell geprüft werden; der Eintrag darf weder wiederholt noch als neuer Plan behandelt werden.";
const LEGACY_REMOTE_IDENTITY_REPLAN_ERROR: &str = "Dieser ältere FTPS-/WebDAV-Löschplan verwendet das frühere Serveridentitäts-Schema. Er war sicher noch nicht gestartet und muss mit einer neuen Vorschau ausdrücklich neu geplant werden.";
const LEGACY_REMOTE_IDENTITY_UNCLEAR_ERROR: &str = "Dieser ältere FTPS-/WebDAV-Löschplan verwendet das frühere Serveridentitäts-Schema und kann bereits eine Teilwirkung gehabt haben. Zielzustand und Recoverypfade müssen manuell geprüft werden; der alte Plan wird niemals wiederholt.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileQueueKind {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SftpQueueDirection {
    Upload,
    Download,
}

/// Geheimnisfrei persistierbarer Endpunkt eines bidirektionalen Queue-Laufs.
/// Remote-Konfigurationen enthalten ausschließlich öffentliche Pins und Referenzen
/// auf den System-Schlüsselbund beziehungsweise SSH-Agenten.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "provider")]
pub enum BidirectionalQueueEndpoint {
    Local {
        root: String,
    },
    Sftp {
        connection: Box<SftpConnectionConfig>,
        root: String,
    },
    Ftps {
        connection: Box<FtpsConnectionConfig>,
        root: String,
    },
    WebDav {
        connection: Box<WebDavConnectionConfig>,
        root: String,
    },
    Smb {
        connection: Box<SmbConnectionConfig>,
        root: String,
    },
}

impl BidirectionalQueueEndpoint {
    pub fn root(&self) -> &str {
        match self {
            Self::Local { root }
            | Self::Sftp { root, .. }
            | Self::Ftps { root, .. }
            | Self::WebDav { root, .. }
            | Self::Smb { root, .. } => root,
        }
    }

    /// Prüft Root und alle geheimnisfreien Verbindungsdaten, ohne ein Secret zu
    /// lesen oder eine Netzwerkverbindung aufzubauen. CLI-Aufrufer verwenden diese
    /// Grenze vor dem Öffnen des Queuejournals; `connect` prüft sie zur Sicherheit
    /// erneut.
    pub fn validate(&self) -> io::Result<()> {
        validate_bidirectional_endpoint(self)
    }

    pub fn connect(&self, secrets: Arc<dyn SecretResolver>) -> io::Result<ProviderEndpoint> {
        // Root und öffentliche Verbindungsdaten sind reine Eingaben. Sie müssen
        // scheitern können, bevor ein Secret gelesen oder ein Server angesprochen
        // wird; sonst hätte eine ungültige persistierte Operation bereits Wirkung.
        self.validate()?;
        let root = Path::new(self.root());
        match self {
            Self::Local { .. } => ProviderEndpoint::local(root),
            Self::Sftp { connection, .. } => ProviderEndpoint::sftp(
                Arc::new(SftpVfs::connect((**connection).clone(), secrets)?),
                root,
            ),
            Self::Ftps { connection, .. } => ProviderEndpoint::ftps(
                Arc::new(FtpsVfs::connect((**connection).clone(), secrets)?),
                root,
            ),
            Self::WebDav { connection, .. } => ProviderEndpoint::webdav(
                Arc::new(WebDavVfs::connect((**connection).clone(), secrets)?),
                root,
            ),
            Self::Smb { connection, .. } => ProviderEndpoint::smb(
                Arc::new(SmbVfs::connect((**connection).clone(), secrets)?),
                root,
            ),
        }
    }

    /// Nur Optimierung für eine bereits **zur Laufzeit** identitätsgeprüfte
    /// Namespace-Beziehung. Gleiche Konfiguration allein ist kein Namespacebeleg.
    pub fn shares_connection_definition(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Sftp {
                    connection: left, ..
                },
                Self::Sftp {
                    connection: right, ..
                },
            ) => left == right,
            (
                Self::Ftps {
                    connection: left, ..
                },
                Self::Ftps {
                    connection: right, ..
                },
            ) => left == right,
            (
                Self::WebDav {
                    connection: left, ..
                },
                Self::WebDav {
                    connection: right, ..
                },
            ) => left == right,
            (
                Self::Smb {
                    connection: left, ..
                },
                Self::Smb {
                    connection: right, ..
                },
            ) => left == right,
            _ => false,
        }
    }
}

/// Dauerhaft speicherbare Nutzerabsicht. `expected_*` ist genau die bereits sichtbare
/// und bestätigte Konflikt-/Planlage; Abweichungen werden nicht still übernommen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum QueueOperation {
    /// Ausschließlich manuelle Prüfung eines dauerhaft erhaltenen FTPS-Intents.
    /// Dieser Eintrag besitzt keinen Executor und kann keine Mutation wiederholen.
    FtpsRecovery {
        identity: Box<crate::remote::FtpsRootIdentity>,
        account_digest: [u8; 32],
    },
    File {
        kind: FileQueueKind,
        items: Vec<String>,
        destination: String,
        overwrite: bool,
        expected_conflicts: Vec<String>,
        #[serde(default)]
        expected_conflict_proofs: Vec<FileConflictProof>,
        #[serde(default)]
        keep_both_targets: Vec<crate::fileops::FileKeepBothTarget>,
        /// Bindet, ob die Vorschau das lokale Ersetzen als Best Effort gezeigt
        /// und bestätigt hat. `None` kennzeichnet einen migrierten Alteintrag.
        #[serde(default)]
        local_replace_best_effort_expected: Option<bool>,
        /// Identität des Zielroots, für den der schwächere Neunamencommit bestätigt wurde.
        #[serde(default)]
        local_copy_new_best_effort_expected: Option<Box<crate::vfs::LocalRootIdentity>>,
    },
    /// Providerfähige interaktive Auswahloperation. Beide Endpunkte enthalten nur
    /// öffentliche Konfiguration und Secret-Referenzen; jeder Claim verbindet neu.
    ProviderFile {
        kind: FileQueueKind,
        source: Box<BidirectionalQueueEndpoint>,
        target: Box<BidirectionalQueueEndpoint>,
        items: Vec<String>,
        destination: String,
        overwrite: bool,
        expected_conflicts: Vec<String>,
        /// Öffentlicher Umfang der zuvor gezeigten interaktiven Baumquelle.
        /// Nach Neustart wird er frisch ermittelt; Handles bleiben ausschließlich RAM.
        #[serde(default)]
        source_tree_expected: Option<Box<crate::vfs::InteractiveSourceTree>>,
        #[serde(default)]
        expected_conflict_proofs: Vec<FileConflictProof>,
        #[serde(default)]
        keep_both_targets: Vec<crate::fileops::FileKeepBothTarget>,
        /// Ausschließlich für die Migration von Journalversion 12. Ein gemeinsames
        /// Feld kann bei gemischten Auswahlen nicht gleichzeitig neue Namen und
        /// Ersetzungen binden und wird in Version 13 nicht mehr geschrieben.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        best_effort_commit_expected: Option<bool>,
        /// Zusätzliche lokale Zielbindung, auch bei einer Remote-Quelle.
        #[serde(default)]
        local_copy_new_best_effort_expected: Option<Box<crate::vfs::LocalRootIdentity>>,
        /// Getrennte Vorschauerwartungen für den Commit eines neuen Namens und das
        /// Ersetzen eines vorhandenen Eintrags. `Some(true)` heißt: Best Effort war
        /// bereits sichtbar; `Some(false)`: abgesicherte Klasse war sichtbar;
        /// `None`: Alteintrag ohne belastbare Bindung.
        #[serde(default)]
        copy_new_best_effort_expected: Option<bool>,
        #[serde(default)]
        replace_best_effort_expected: Option<bool>,
        /// Bindet bei einem Cross-Namespace-Move die in der Vorschau sichtbare
        /// Quellentfernungsklasse. `None` kennzeichnet einen migrierten Alteintrag,
        /// der eine schwächere Wirkung vor der Mutation neu bestätigen muss.
        #[serde(default)]
        remove_source_best_effort_expected: Option<bool>,
    },
    /// Providerfähiges Löschen einer exakt gescannten Auswahl. Die irreversible
    /// Bestätigung erfolgt vor dem Enqueue; der Executor verbindet trotzdem frisch,
    /// wiederholt Capability-Probe und vollständigen Plan und stoppt bei Drift.
    ProviderDelete {
        endpoint: Box<BidirectionalQueueEndpoint>,
        mode: RemoteDeleteMode,
        expected_plan: Box<RemoteDeletePlan>,
    },
    Sync {
        source: String,
        target: String,
        /// `None` bedeutet dauerhaft Dry-Run. Ein gespeicherter Sandbox-Root muss bei
        /// Ausführung erneut exakt derselbe markierte Root sein.
        sandbox: Option<String>,
        options: SyncOptions,
        expected_plan: Box<SyncPlan>,
    },
    /// Normale lokale Ausführung. Persistiert wird nur die Absicht; die gehaltenen
    /// Roots und die wirkungsbezogene Zustimmung bleiben ausschließlich im RAM.
    LocalSync {
        source: String,
        target: String,
        sandbox: Option<String>,
        options: SyncOptions,
        expected_plan: Box<SyncPlan>,
        /// Ausschließlich Anzeige des letzten Backendprofils. Nach Neustart
        /// entsteht auch daraus weder Zustimmung noch ein Ausführungsrecht.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_hint: Option<crate::real_mode::LocalSyncExecution>,
    },
    SftpSync {
        direction: SftpQueueDirection,
        connection: Box<SftpConnectionConfig>,
        local_root: String,
        remote_root: String,
        options: SyncOptions,
        expected_plan: Box<SyncPlan>,
    },
    FtpsSync {
        direction: SftpQueueDirection,
        connection: Box<FtpsConnectionConfig>,
        local_root: String,
        remote_root: String,
        options: SyncOptions,
        expected_plan: Box<SyncPlan>,
    },
    WebDavSync {
        direction: SftpQueueDirection,
        connection: Box<WebDavConnectionConfig>,
        local_root: String,
        remote_root: String,
        options: SyncOptions,
        expected_plan: Box<SyncPlan>,
    },
    SmbSync {
        direction: SftpQueueDirection,
        connection: Box<SmbConnectionConfig>,
        local_root: String,
        remote_root: String,
        options: SyncOptions,
        expected_plan: Box<SyncPlan>,
    },
    /// Beliebige providerübergreifende Richtung. Beide Endpunkte enthalten nur
    /// öffentliche Identität und Secret-Referenzen; Sitzungen entstehen pro Claim neu.
    ProviderSync {
        source: Box<BidirectionalQueueEndpoint>,
        target: Box<BidirectionalQueueEndpoint>,
        options: SyncOptions,
        expected_plan: Box<SyncPlan>,
    },
    Bidirectional {
        pair_id: PairId,
        pair_name: String,
        left: Box<BidirectionalQueueEndpoint>,
        right: Box<BidirectionalQueueEndpoint>,
        baseline_path: String,
        resolutions: Vec<ConflictResolution>,
        expected_baseline_digest: Option<[u8; 32]>,
        expected_plan_digest: [u8; 32],
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueueState {
    Pending,
    Validating,
    Running,
    AwaitingConfirmation,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl QueueState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Neu entdeckte Lage, die vor einem weiteren Ausführungsversuch sichtbar bestätigt
/// werden muss. Der Scheduler kann daraus niemals selbst einen neuen Plan genehmigen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum QueueAttention {
    SourceTreeChanged {
        reason: String,
        confirmation: String,
        current_tree: Option<Box<crate::vfs::InteractiveSourceTree>>,
    },
    PlanChanged {
        reason: String,
        confirmation: String,
        current_plan: Box<SyncPlan>,
    },
    ConflictsChanged {
        reason: String,
        confirmation: String,
        current_conflicts: Vec<String>,
        #[serde(default)]
        current_conflict_proofs: Vec<FileConflictProof>,
    },
    Manual {
        reason: String,
        confirmation: String,
    },
    /// Die lokale Best-Effort-Freigabe gilt für diesen Zielroot samt Inode und
    /// Mount; ein ausgetauschter Root verlangt neue Zustimmung.
    LocalFileCommit {
        reason: String,
        confirmation: String,
        target_identity: crate::vfs::LocalRootIdentity,
    },
    /// Das Ziel bewies bei der Ausführung nur noch einen Best-Effort-Commit,
    /// obwohl die Vorschau eine abgesicherte Ausführung zeigte. Die Bestätigung
    /// bindet die Erwartung des Eintrags atomar auf Best Effort um. Ein
    /// generisches `Manual` würde die Erwartung nicht ändern — derselbe Eintrag
    /// pausierte dann beim nächsten Claim erneut und wäre nie bestätigbar.
    BestEffortCommitDowngrade {
        reason: String,
        confirmation: String,
        #[serde(default)]
        copy_new_best_effort: bool,
        #[serde(default)]
        replace_best_effort: bool,
        #[serde(default)]
        remove_source_best_effort: bool,
    },
    /// Backend-RAM hält gleichzeitig die nicht serialisierbare Vorbereitung. Diese
    /// Phrase darf nicht über den allgemeinen Requeue-Pfad bestätigt werden.
    RemoteRun {
        reason: String,
        confirmation: String,
    },
    BidirectionalPlanChanged {
        reason: String,
        confirmation: String,
        current_baseline_digest: Option<[u8; 32]>,
        current_plan_digest: [u8; 32],
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueEntry {
    pub operation_id: String,
    pub operation: QueueOperation,
    /// Geheimnisfreie Profilstände, aus denen die persistierte Operation entstand.
    /// Alte CLI- und Journal-Einträge dürfen leer bleiben; GUI-Remoteoperationen
    /// binden jede beteiligte Profil-ID genau einmal.
    #[serde(default)]
    pub profile_bindings: Vec<crate::RemoteProfileBinding>,
    pub created_unix_ms: u128,
    pub state: QueueState,
    pub attempts: u32,
    pub progress: Progress,
    pub attention: Option<QueueAttention>,
    pub error: Option<String>,
    /// Maschinenlesbarer Code des sichtbaren Fehlers. Journalversion 18 ergänzt
    /// ihn, damit insbesondere ein unklarer Mutationsausgang nach einem Neustart
    /// nicht wieder wie ein gewöhnlicher, wiederholbarer I/O-Fehler aussieht.
    #[serde(default)]
    pub error_code: Option<crate::ErrorCode>,
    /// Sichtbare Prüfpfade eigener Tempdateien, deren Entfernung nach Abbruch
    /// nicht atomar an den ursprünglichen Eintrag gebunden werden konnte. Sie
    /// autorisieren niemals ein automatisches Löschen.
    #[serde(default)]
    pub temp_recovery_paths: Vec<String>,
    #[serde(default)]
    pub retained_backup_paths: Vec<String>,
    /// Nur `true`, wenn zu genau diesem bidirektionalen Eintrag ein persistentes
    /// Core-Laufjournal existiert. Die Oberfläche leitet Recovery nie aus Text oder
    /// Status allein ab.
    #[serde(default)]
    pub recovery_available: bool,
    /// Die Datenwirkung ist erfolgreich abgeschlossen, ihr Core-Journal aber noch
    /// nicht abgebaut. Bei `Completed`/`Cancelled` bindet derselbe Queue-Commit den
    /// Marker; bei `Interrupted` muss dieser terminale Commit erst nachgeholt werden.
    /// In beiden Fällen ist ausschließlich Cleanup erlaubt, nie neue Dateiwirkung.
    #[serde(default)]
    pub bidirectional_completion_recovery: Option<BidirectionalCompletionRecovery>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidirectionalCompletionRecovery {
    Complete,
    RolledBack,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueSnapshot {
    pub version: u32,
    /// Monotone Journalrevision für die Zusammenführung getrennter Event- und
    /// Command-Antwortwege im Frontend. Alte Journale beginnen sicher bei null.
    #[serde(default)]
    pub revision: u64,
    pub paused: bool,
    pub stop_on_error: bool,
    pub entries: Vec<QueueEntry>,
}

impl Default for QueueSnapshot {
    fn default() -> Self {
        Self {
            version: QUEUE_JOURNAL_VERSION,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: Vec::new(),
        }
    }
}

fn bidirectional_recovery_kind(
    operation: &QueueOperation,
) -> crate::bidirectional::BidirectionalRunRecovery {
    let QueueOperation::Bidirectional { baseline_path, .. } = operation else {
        return crate::bidirectional::BidirectionalRunRecovery::Absent;
    };
    // Auch ein beschädigtes Journal ist ein erreichbarer Recoverybefund und darf
    // nicht als gewöhnlicher Retry ausgegeben werden. Nur `Ok(None)` beweist, dass
    // kein Laufjournal existiert.
    crate::bidirectional::classify_bidirectional_run_recovery(Path::new(baseline_path))
        .unwrap_or(crate::bidirectional::BidirectionalRunRecovery::ResumeOrRollback)
}

/// Thread-sicherer Queue-Speicher. Jede sichtbare Zustandsänderung wird unter demselben
/// Mutex atomar auf Platte geschrieben, bevor ein anderer Starter sie beobachten kann.
pub struct OperationQueue {
    path: PathBuf,
    journal: Mutex<QueueSnapshot>,
    /// Ein nicht lesbarer oder unerwarteter Stand nach einem möglicherweise
    /// wirkenden atomaren Rename darf nie durch einen zweiten Schreibversuch
    /// überschrieben werden. Erst ein erneutes Öffnen klärt den Diskstand.
    journal_outcome_unclear: AtomicBool,
    #[cfg(test)]
    fail_terminal_progress_update: AtomicBool,
    #[cfg(test)]
    fail_nonterminal_progress_update_after: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    terminal_progress_update_attempts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_next_file_failure_commit: AtomicBool,
    #[cfg(test)]
    fail_next_file_completion_commit: AtomicBool,
    #[cfg(test)]
    fail_reconcile_sync: AtomicBool,
    /// Mehrere Fensterqueues desselben Prozesses teilen diesen Besitzer. Ein zweiter
    /// GUI-/CLI-Prozess bleibt durch denselben offenen Deskriptor ausgeschlossen.
    _process_lock: Arc<OperationQueueProcessLock>,
}

/// Nicht frei konstruierbarer Beleg dafür, welche bereits persistierte
/// Core-Wirkung die Queue nur noch terminal festschreiben darf.
#[derive(Clone, Copy)]
struct BidirectionalCompletionProof(BidirectionalCompletionRecovery);

/// Prozessbesitz über alle Queue-Journale eines Konfigurationsordners. Die App
/// erwirbt ihn einmal und reicht ihn an jede Fensterqueue weiter; die CLI nutzt über
/// [`OperationQueue::open`] weiterhin automatisch einen exklusiven Besitzer.
pub struct OperationQueueProcessLock {
    directory: PathBuf,
    root: crate::local_fs::RootHandle,
    file: File,
    lock_owner: u32,
}

impl Drop for OperationQueueProcessLock {
    fn drop(&mut self) {
        // close allein hält flock bis zum letzten geerbten FD. Nur der
        // ursprüngliche Besitzer darf beim Ende seiner letzten Fensterqueue
        // ausdrücklich freigeben; ein fork-Kind darf den Elternlock nicht lösen.
        if self.lock_owner == std::process::id() {
            let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
        }
    }
}

/// Laufzeitkontext bleibt außerhalb des Journals. Insbesondere wird der aktuell
/// validierte Sandbox-Root erneut mit dem gespeicherten Ausführungsmodus verglichen.
pub struct LocalQueueExecution {
    pub sandbox: Option<PathBuf>,
    pub sync_quarantine: Option<PathBuf>,
    pub max_delete_entries: u64,
    pub max_delete_fraction: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOperationClass {
    File,
    Sync,
}

#[derive(Debug)]
pub enum QueueExecutionResult {
    File(OpStats),
    Sync(SyncStats),
    Bidirectional(LocalBidirectionalStats),
    AwaitingConfirmation {
        class: QueueOperationClass,
        reason: String,
    },
}

/// Fehler eines Provider-Dateilaufs samt bis dahin sicher bekannten Wirkungen.
/// Der eigentliche `io::Error` bleibt unverändert, damit sein `ErrorCode` erhalten
/// bleibt; CLI und Audit können die Teilbilanz getrennt serialisieren.
#[derive(Debug)]
pub struct ProviderFileExecutionFailure {
    /// Fachlicher Executorfehler. Er bleibt auch dann unverändert erhalten, wenn
    /// der anschließende atomare Queueabschluss seinerseits scheitert.
    pub error: io::Error,
    /// Fehler der terminalen Journalisierung in zeitlicher Reihenfolge. Aufrufer
    /// dürfen den Abschluss mit denselben strukturierten Recoverypfaden erneut
    /// versuchen, den fachlichen Fehler aber niemals damit überschreiben.
    pub journal_errors: Vec<io::Error>,
    pub actuals: Box<FileOperationActuals>,
    /// Letzter vom Dateiexecutor gemeldeter Fortschritt. Er bleibt auch dann
    /// erhalten, wenn gerade dessen Queue-Commit scheitert.
    pub progress: Box<Progress>,
    /// Eine vollständig vollzogene Dateioperation, deren terminaler Queuecommit
    /// noch nicht sicher geschrieben werden konnte. Aufrufer dürfen ausschließlich
    /// diesen Abschluss erneut schreiben, niemals die Dateioperation wiederholen
    /// oder den Eintrag als gewöhnlich fehlgeschlagen markieren.
    pub pending_completion: Option<Box<PendingFileCompletion>>,
    /// Exakter Lauf, in dem bereits bestätigte Altziele geparkt wurden. Der Pfad
    /// ist nur bei einer Teilwirkung gesetzt und bleibt getrennt vom Fehlertext
    /// maschinenlesbar.
    pub recovery: Option<ProviderFileRecovery>,
}

#[derive(Clone, Debug)]
pub struct PendingFileCompletion {
    cancelled: bool,
    progress: Progress,
    temp_recovery_paths: Vec<String>,
    audit_operation: &'static str,
    audit_context: ProviderAuditContext,
    pub stats: OpStats,
}

impl ProviderFileExecutionFailure {
    pub fn into_io_error(self) -> io::Error {
        public_file_execution_error(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProviderFileRecovery {
    /// Exakter aktuell maßgeblicher Recoverypfad. Während der Basisanlage ist das
    /// `.doppeldecker-quarantine`; unmittelbar vor der Laufanlage wird das Feld auf
    /// den reservierten Laufpfad fortgeschrieben. So existiert vor jeder Mutation
    /// bereits ein strukturierter Pfad, ohne einen noch nicht angelegten Lauf als
    /// sichtbar vorzutäuschen.
    pub quarantine_run: String,
    pub parked_entries: u64,
}

fn public_file_execution_error(failure: ProviderFileExecutionFailure) -> io::Error {
    let mut paths = crate::error::recovery_paths(&failure.error);
    for error in &failure.journal_errors {
        paths.extend(crate::error::recovery_paths(error));
    }
    paths.extend(failure.actuals.temp_recovery_paths.iter().cloned());
    paths.extend(failure.actuals.retained_backup_paths.iter().cloned());
    if let Some(recovery) = &failure.recovery {
        paths.push(recovery.quarantine_run.clone());
    }
    if let Some(completion) = &failure.pending_completion {
        paths.extend(completion.temp_recovery_paths.iter().cloned());
    }
    let execution_outcome_unclear = crate::error_code(&failure.error)
        == crate::ErrorCode::OutcomeUnclear
        || failure
            .journal_errors
            .iter()
            .any(|error| crate::error_code(error) == crate::ErrorCode::OutcomeUnclear);
    let primary = if crate::error_code(&failure.error) == crate::ErrorCode::OutcomeUnclear {
        &failure.error
    } else {
        failure.journal_errors.last().unwrap_or(&failure.error)
    };
    // Ein vorhandener Abschlussdatensatz bedeutet: Die Dateioperation ist bereits
    // vollständig vollzogen. Scheitert danach auch der erneute Journalcommit, ist
    // nicht die Dateiwirkung fehlgeschlagen, sondern ihr dauerhaft gespeicherter
    // Queuezustand unklar. Der konkrete Speicherfehler bleibt unten als Detail
    // erhalten, darf aber nicht als öffentlicher Hauptcode erscheinen.
    let completion_outcome_unclear =
        failure.pending_completion.is_some() && !failure.journal_errors.is_empty();
    let code = if completion_outcome_unclear || execution_outcome_unclear {
        crate::ErrorCode::OutcomeUnclear
    } else {
        crate::error_code(primary)
    };
    let kind = if completion_outcome_unclear || execution_outcome_unclear {
        io::ErrorKind::Other
    } else {
        primary.kind()
    };
    let actuals = serde_json::to_string(&failure.actuals)
        .unwrap_or_else(|_| "{\"actuals\":\"nicht_serialisierbar\"}".into());
    let recovery = serde_json::to_string(&failure.recovery)
        .unwrap_or_else(|_| "{\"recovery\":\"nicht_serialisierbar\"}".into());
    let progress = serde_json::to_string(&failure.progress)
        .unwrap_or_else(|_| "{\"progress\":\"nicht_serialisierbar\"}".into());
    let journal_errors = failure
        .journal_errors
        .iter()
        .map(|error| format!("{}: {error}", crate::error_code(error).as_str()))
        .collect::<Vec<_>>()
        .join("; ");
    let wrapped = crate::error::coded_error(
        kind,
        code,
        format!(
            "Ausführungsfehler {}: {}; terminale Dateiwirkung ohne dauerhaften Queueabschluss: {}; Queue-Journalfehler: [{}]; letzter Fortschritt: {progress}; bekannte Dateioperationswirkungen: {actuals}; Recovery: {recovery}",
            crate::error_code(&failure.error).as_str(),
            failure.error,
            failure.pending_completion.is_some(),
            journal_errors,
        ),
    );
    crate::error::with_recovery_paths(wrapped, &paths)
}

/// Wirkungsspezifische Klasse, die unmittelbar vor dem Mutationsbeginn aus den
/// frisch geprüften Providerfähigkeiten feststeht. Sie bleibt absichtlich intern:
/// Das Queue-Audit braucht nur den stabilen, pfadfreien Beleg und darf daraus kein
/// neues Ausführungsrecht ableiten.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ProviderAuditExecutionClass {
    Assured,
    BestEffort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProviderAuditContext {
    source_provider: Option<crate::ProviderKind>,
    target_provider: crate::ProviderKind,
    execution_class: ProviderAuditExecutionClass,
    source_removal_best_effort_expected: bool,
    target_copy_new_best_effort_expected: bool,
    target_replace_best_effort_expected: bool,
    source_removal_best_effort_actual: Option<bool>,
    target_copy_new_best_effort_actual: Option<bool>,
    target_replace_best_effort_actual: Option<bool>,
}

/// Hält die beiden ausschließlich für eine Wiederherstellung bestimmten
/// Auditfelder zusammen. Dadurch bleibt der Aufruf übersichtlich, obwohl ein
/// Providerlauf seine Quarantäne und ein lokaler Lauf liegen gebliebene
/// Temp-Dateien getrennt ausweisen kann.
struct FileRecoveryPaths<'a> {
    temporary: &'a [String],
    backups: &'a [String],
}

struct FileAuditRecovery<'a> {
    provider: Option<&'a ProviderFileRecovery>,
    temp_paths: &'a [String],
    retained_backups: &'a [String],
}

fn audit_context_with_actuals(
    context: ProviderAuditContext,
    actuals: &FileOperationActuals,
) -> ProviderAuditContext {
    ProviderAuditContext {
        source_removal_best_effort_actual: actuals.source_removal_best_effort,
        target_copy_new_best_effort_actual: actuals.target_copy_new_best_effort,
        target_replace_best_effort_actual: actuals.target_replace_best_effort,
        ..context
    }
}

fn provider_audit_label(kind: crate::ProviderKind) -> &'static str {
    match kind {
        crate::ProviderKind::Local => "local",
        crate::ProviderKind::Sftp => "sftp",
        crate::ProviderKind::Ftps => "ftps",
        crate::ProviderKind::WebDav => "webdav",
        crate::ProviderKind::Smb => "smb",
    }
}

fn provider_file_audit_class(
    target_capabilities: Option<&crate::ProviderFileCapabilities>,
    overwrite: bool,
    source_removal_best_effort: bool,
) -> ProviderAuditExecutionClass {
    let best_effort = source_removal_best_effort
        || target_capabilities.is_some_and(|capabilities| {
            (!capabilities.copy_new && capabilities.copy_new_best_effort)
                || (overwrite && !capabilities.replace && capabilities.replace_best_effort)
        });
    if best_effort {
        ProviderAuditExecutionClass::BestEffort
    } else {
        // Lokale Ziele und der separat geprüfte Rename-Schnellweg besitzen keinen
        // ProviderFileCapabilities-Wert; beide laufen ausschließlich abgesichert.
        ProviderAuditExecutionClass::Assured
    }
}

fn connect_queue_endpoint_with_credential_attention(
    queue: &OperationQueue,
    operation_id: &str,
    endpoint: &BidirectionalQueueEndpoint,
    secrets: Arc<dyn SecretResolver>,
) -> io::Result<Option<ProviderEndpoint>> {
    match endpoint.connect(secrets) {
        Ok(endpoint) => Ok(Some(endpoint)),
        Err(error)
            if !matches!(endpoint, BidirectionalQueueEndpoint::Local { .. })
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
        {
            let reason =
                "Provider-Zugangsdaten sind nicht verfügbar oder wurden abgelehnt".to_string();
            queue.require_confirmation(
                operation_id,
                QueueAttention::Manual {
                    reason,
                    confirmation: format!("SECRET {}", short_id(operation_id)),
                },
            )?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn provider_delete_audit_class(mode: RemoteDeleteMode) -> ProviderAuditExecutionClass {
    match mode {
        RemoteDeleteMode::Quarantine => ProviderAuditExecutionClass::Assured,
        // Ohne erreichbaren Restoreweg ist das ausdrücklich bestätigte dauerhafte
        // Löschen die vorgesehene Best-Effort-Wirkung.
        RemoteDeleteMode::Permanent => ProviderAuditExecutionClass::BestEffort,
    }
}

pub enum SftpQueuePreparation {
    Ready(Box<PreparedSftpQueueRun>),
    AwaitingConfirmation { reason: String },
}

pub enum BidirectionalQueuePreparation {
    Ready(Box<PreparedBidirectionalQueueRun>),
    AwaitingConfirmation { reason: String },
}

/// Nicht serialisierbare lokale Vorbereitung eines bidirektionalen Queue-Eintrags.
/// Root-Handles, Journal-Lock und Einmal-Permit bleiben ausschließlich im Backend-RAM.
pub struct PreparedBidirectionalQueueRun {
    operation_id: String,
    prepared: PreparedLocalBidirectionalRun,
}

impl PreparedBidirectionalQueueRun {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn preview(&self) -> &crate::LocalBidirectionalPreview {
        self.prepared.preview()
    }
}

/// Kurzlebige Queue-Vorbereitung. Verbindung, Handles und Permit bleiben ausschließlich
/// im RAM und werden nie Teil von [`QueueSnapshot`].
pub struct PreparedSftpQueueRun {
    operation_id: String,
    prepared: PreparedSftpRealRun,
}

impl PreparedSftpQueueRun {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn preview(&self) -> &crate::RealRunPreview {
        self.prepared.preview()
    }
}

impl OperationQueue {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let process_lock = Arc::new(OperationQueueProcessLock::acquire(&path)?);
        Self::open_with_process_lock(path, process_lock)
    }

    pub fn open_with_process_lock(
        path: impl Into<PathBuf>,
        process_lock: Arc<OperationQueueProcessLock>,
    ) -> io::Result<Self> {
        let requested_path = path.into();
        let parent = queue_parent(&requested_path)?;
        if parent != process_lock.directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Queue-Journal und Prozesssperre gehören zu verschiedenen Konfigurationsordnern",
            ));
        }
        let name = requested_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Queue-Journal braucht einen Dateinamen",
            )
        })?;
        // Der gemeinsame Prozesslock ist bereits an den kanonischen Elternordner
        // gebunden. Auch das Journal selbst verwendet fortan genau diesen Präfix;
        // dadurch kann der descriptor-relative Leser stabile Systemsymlinks wie
        // macOS `/var` zulassen, ohne später einem austauschbaren Pfad zu folgen.
        let path = parent.join(name);
        let (mut snapshot, journal_missing) = match read_snapshot_bound(&process_lock.root, &path) {
            Ok(snapshot) => (snapshot, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (QueueSnapshot::default(), true)
            }
            Err(error) => return Err(error),
        };
        // Version 2 ergänzte SFTP, Version 3 FTPS, Version 4 WebDAV, Version 5 SMB
        // und Version 6 bidirektionale Läufe. Version 8 ergänzt die generische
        // Provider→Provider-Variante; Version 9 ergänzt interaktive Provider-Dateien,
        // Version 10 Remote-Löschen und Version 11 rekursive Zielkonfliktpfade.
        // Version 12 ergänzt den eigenen Attention-Typ `BestEffortCommitDowngrade`.
        // Version 13 bindet jeden bestätigten Zielkonflikt an Metadaten/Hash und
        // trennt die Best-Effort-Klassen für neue Namen und Ersetzungen. Version 14
        // bindet zusätzlich die sichtbare Klasse des Move-Quellabbaus. Version 15
        // bindet die lokale Best-Effort-Ersetzung getrennt und ausdrücklich.
        // Version 16 ergänzt geheimnisfreie Profilrevisionen je Queueeintrag.
        // Version 17 migriert ältere WebDAV-Löschpläne, denen die damals noch
        // nicht persistierte starke Root-ETag fehlt, in einen sichtbaren und nicht
        // erneut ausführbaren Fehlerzustand. Version 18 persistiert Fehlercodes
        // und migriert zusätzlich WebDAV-v17-Pläne mit dem früheren leeren
        // Server-Header-Digest in denselben fail-closed Zustand. Außerdem
        // kennzeichnet Version 18 ältere fehlgeschlagene oder unterbrochene
        // Mutationen ohne damals noch nicht persistierten Fehlercode konservativ
        // als unklar, damit sie nach dem Upgrade nicht blind wiederholbar werden.
        // Version 19 stellt FTPS-/WebDAV-Löschpläne mit den früheren
        // Serveridentitäts-Digests zustandsabhängig auf Neuplanung oder manuelle
        // Recovery, statt sie unter der neuen Digestbedeutung wieder auszuführen.
        // Ältere Dokumente besitzen den jeweils neuen Variantentag nicht.
        // Ältere Dokumente enthalten den jeweils neueren Variantentag per Definition
        // nicht und lassen sich deshalb ohne Bedeutungsänderung atomar hochstufen.
        // Version 22 bindet backendseitig vergebene Konfliktkopie-Ziele. Ältere
        // Executoren dürfen sie nicht als gewöhnliches Skip ausführen.
        // Version 23 unterscheidet normale lokale Ausführung ausdrücklich von
        // persistentem Dry-Run; ältere Aufträge werden niemals hochgestuft.
        let source_version = snapshot.version;
        // Version 24 bindet rekursive interaktive Quellen und deren Auslassungen.
        // Ein älterer Executor darf den neuen Baumbeleg nicht still ignorieren.
        // Version 25 erhält Remote-Dateiintents und verlangt nach Reopen eine
        // frische Best-Effort-Bestätigung für gewöhnliche WebDAV-Additionen.
        // Version 26 erlaubt die begrenzte lokale Baumquelle für additive WebDAV-Ziele.
        // Ältere Executoren dürfen die Ordnerintents und diese Wirkungsbindung nicht ignorieren.
        // Version 27 zeigt auch vor einem Queueauftrag unterbrochene FTPS-Proben
        // als reine Recovery. Ältere Executoren dürfen sie nicht wiederholen.
        let migrated = if matches!(source_version, 1..=26) {
            for entry in &mut snapshot.entries {
                if let QueueOperation::File {
                    destination,
                    expected_conflicts,
                    ..
                } = &mut entry.operation
                {
                    for conflict in expected_conflicts {
                        if !Path::new(conflict).is_absolute() {
                            *conflict = crate::fileops::path_to_utf8(
                                &Path::new(destination).join(&*conflict),
                            )?;
                        }
                    }
                }
            }
            if source_version <= 10 {
                for entry in &mut snapshot.entries {
                    if let QueueOperation::ProviderFile {
                        destination,
                        expected_conflicts,
                        ..
                    } = &mut entry.operation
                    {
                        for conflict in expected_conflicts {
                            if !Path::new(conflict).is_absolute() {
                                *conflict = crate::fileops::path_to_utf8(
                                    &Path::new(destination).join(&*conflict),
                                )?;
                            }
                        }
                    }
                }
            }
            if source_version <= 12 {
                for entry in &mut snapshot.entries {
                    if let QueueOperation::ProviderFile {
                        overwrite,
                        best_effort_commit_expected,
                        copy_new_best_effort_expected,
                        replace_best_effort_expected,
                        ..
                    } = &mut entry.operation
                    {
                        let legacy = best_effort_commit_expected.take();
                        if *overwrite {
                            *replace_best_effort_expected = legacy;
                        } else {
                            *copy_new_best_effort_expected = legacy;
                        }
                    }
                    // Die alte Bestätigung konnte nicht ausdrücken, welche der
                    // beiden Wirkungen degradiert war. Sie wird deshalb verworfen;
                    // der nächste Claim erzeugt bei Bedarf eine eindeutige Phrase.
                    if matches!(
                        entry.attention,
                        Some(QueueAttention::BestEffortCommitDowngrade { .. })
                    ) {
                        entry.state = QueueState::Pending;
                        entry.attention = None;
                        entry.error = None;
                        entry.error_code = None;
                    }
                }
            }
            if source_version <= 14 {
                for entry in &mut snapshot.entries {
                    if let QueueOperation::File {
                        overwrite,
                        local_replace_best_effort_expected,
                        ..
                    } = &mut entry.operation
                    {
                        // Alte überschreibende Einträge besitzen keine belastbare
                        // Sichtbarkeitsbestätigung und müssen beim nächsten Claim
                        // pausieren. Ohne Überschreiben ist die Klasse eindeutig.
                        *local_replace_best_effort_expected = (!*overwrite).then_some(false);
                    }
                }
            }
            if source_version <= 17 {
                for entry in &mut snapshot.entries {
                    if matches!(entry.state, QueueState::Failed | QueueState::Interrupted)
                        && entry.error_code.is_none()
                    {
                        // Vor Version 18 blieb vom Executorfehler nur der Text im
                        // Journal. Daraus lässt sich nachträglich nicht sicher
                        // ableiten, ob eine Mutation bereits gesendet wurde. Ein
                        // erneuter Lauf bleibt deshalb bis zur ausdrücklichen
                        // Prüfung gesperrt; sicher mutationsfreie Pending-Einträge
                        // und bereits terminale Abschlüsse bleiben unverändert.
                        entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                    }
                }
            }
            if source_version <= 19 {
                for entry in &mut snapshot.entries {
                    let QueueOperation::ProviderDelete {
                        endpoint,
                        expected_plan,
                        ..
                    } = &entry.operation
                    else {
                        continue;
                    };
                    if !matches!(
                        endpoint.as_ref(),
                        BidirectionalQueueEndpoint::Ftps { .. }
                            | BidirectionalQueueEndpoint::WebDav { .. }
                    ) {
                        continue;
                    }
                    // Version 18 konnte bereits einen WebDAV-v15..17-Plan mit
                    // diesem ungültigen ETag-Sentinel stillgelegt haben. Dieser
                    // Zustand belegt durch Status, Fortschritt und Fehlertext
                    // getrennt, ob der Plan sicher nie startete oder eine Wirkung
                    // unklar blieb. Die allgemeinere Schema-Migration darf diese
                    // schon abgeschlossene Einordnung weder verschärfen noch
                    // unbrauchbar machen; die nachfolgende Journalvalidierung
                    // akzeptiert ausschließlich die vollständig passende Form.
                    let already_classified_legacy_webdav = matches!(
                        &expected_plan.root_identity,
                        crate::RootIdentity::WebDav(identity)
                            if identity.root_etag == LEGACY_UNBOUND_WEBDAV_ETAG
                    );
                    if already_classified_legacy_webdav {
                        continue;
                    }
                    // Die älteren WebDAV-Migrationen besitzen einen eigenen,
                    // ausdrücklich quittierbaren Fehlerzustand. Sie laufen direkt
                    // danach und dürfen nicht durch die allgemeinere
                    // Identitätsschema-Migration überschrieben werden.
                    if source_version <= 17
                        && matches!(endpoint.as_ref(), BidirectionalQueueEndpoint::WebDav { .. })
                        && matches!(
                            &expected_plan.root_identity,
                            crate::RootIdentity::WebDav(identity)
                                if identity.root_etag.is_empty()
                                    || identity.server.server_header_digest
                                        == *blake3::hash(&[]).as_bytes()
                        )
                    {
                        continue;
                    }
                    // Version 19 hatte diesen Zustand nur als deutschen Text plus
                    // `plan_drift` gespeichert. Beim Öffnen eines bereits von der
                    // fehlerhaften Migration geschriebenen Journals heben wir exakt
                    // diesen typisierten Altstand auf `replan_required` an.
                    if source_version == 19 {
                        if entry.state == QueueState::Failed
                            && entry.error_code == Some(crate::ErrorCode::PlanDrift)
                            && entry.error.as_deref() == Some(LEGACY_REMOTE_IDENTITY_REPLAN_ERROR)
                        {
                            entry.error_code = Some(crate::ErrorCode::ReplanRequired);
                        }
                        continue;
                    }
                    match entry.state {
                        QueueState::Pending | QueueState::AwaitingConfirmation => {
                            let safely_unstarted = entry.attempts == 0
                                && entry.progress == Progress::default()
                                && entry.error.is_none()
                                && entry.error_code.is_none()
                                && entry.temp_recovery_paths.is_empty()
                                && !entry.recovery_available
                                && entry.bidirectional_completion_recovery.is_none();
                            entry.attention = None;
                            if safely_unstarted {
                                entry.state = QueueState::Failed;
                                entry.progress = Progress::default();
                                entry.error = Some(LEGACY_REMOTE_IDENTITY_REPLAN_ERROR.into());
                                entry.error_code = Some(crate::ErrorCode::ReplanRequired);
                            } else {
                                // Retry setzt einen abgebrochenen Eintrag wieder auf
                                // Pending und leert seinen Fortschritt, behält aber
                                // die Versuchszahl. Ein früherer Claim kann bereits
                                // eine Löschwirkung gehabt haben und darf deshalb
                                // nie als sicher verwerfbarer Altplan erscheinen.
                                entry.state = QueueState::Interrupted;
                                entry.progress.phase = "recovery".into();
                                entry.progress.current.clear();
                                entry.error = Some(LEGACY_REMOTE_IDENTITY_UNCLEAR_ERROR.into());
                                entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                            }
                        }
                        QueueState::Validating
                        | QueueState::Running
                        | QueueState::Failed
                        | QueueState::Interrupted => {
                            entry.state = QueueState::Interrupted;
                            entry.progress.phase = "recovery".into();
                            entry.progress.current.clear();
                            entry.attention = None;
                            entry.error = Some(LEGACY_REMOTE_IDENTITY_UNCLEAR_ERROR.into());
                            entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                            entry.recovery_available = false;
                            entry.bidirectional_completion_recovery = None;
                        }
                        QueueState::Completed | QueueState::Cancelled => {}
                    }
                }
            }
            if source_version <= 17 {
                for entry in &mut snapshot.entries {
                    let QueueOperation::ProviderDelete {
                        endpoint,
                        expected_plan,
                        ..
                    } = &mut entry.operation
                    else {
                        continue;
                    };
                    if !matches!(endpoint.as_ref(), BidirectionalQueueEndpoint::WebDav { .. }) {
                        continue;
                    }
                    let crate::RootIdentity::WebDav(identity) = &mut expected_plan.root_identity
                    else {
                        continue;
                    };
                    let lacks_root_etag = source_version <= 15 && identity.root_etag.is_empty();
                    let has_legacy_empty_server_digest =
                        identity.server.server_header_digest == *blake3::hash(&[]).as_bytes();
                    if lacks_root_etag || has_legacy_empty_server_digest {
                        // Eine ETag lässt sich weder aus Pfad noch Serveridentität
                        // ableiten. Der ungültige Sentinel ist ausdrücklich **kein**
                        // Providerbeleg; er unterscheidet ausschließlich migrierte
                        // Alteinträge von beschädigten aktuellen Journalen.
                        identity.root_etag = LEGACY_UNBOUND_WEBDAV_ETAG.into();
                        match entry.state {
                            QueueState::Pending | QueueState::AwaitingConfirmation => {
                                let safely_unstarted = !has_prior_execution_evidence(entry)
                                    && entry.error.is_none()
                                    && entry.error_code.is_none();
                                entry.attention = None;
                                if safely_unstarted {
                                    entry.state = QueueState::Failed;
                                    entry.error = Some(LEGACY_WEBDAV_REPLAN_ERROR.into());
                                    entry.error_code = Some(crate::ErrorCode::PlanDrift);
                                } else {
                                    entry.state = QueueState::Interrupted;
                                    entry.progress.phase = "recovery".into();
                                    entry.progress.current.clear();
                                    entry.error = Some(LEGACY_WEBDAV_UNCLEAR_ERROR.into());
                                    entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                                }
                                entry.recovery_available = false;
                                entry.bidirectional_completion_recovery = None;
                            }
                            QueueState::Validating
                            | QueueState::Running
                            | QueueState::Failed
                            | QueueState::Interrupted => {
                                entry.state = QueueState::Interrupted;
                                entry.progress.phase = "recovery".into();
                                entry.progress.current.clear();
                                entry.attention = None;
                                entry.error = Some(LEGACY_WEBDAV_UNCLEAR_ERROR.into());
                                entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                                entry.recovery_available = false;
                                entry.bidirectional_completion_recovery = None;
                            }
                            QueueState::Completed | QueueState::Cancelled => {
                                // Terminale Datenwirkung und ihre Bilanz bleiben
                                // unverändert. Der Sentinel dient nur dazu, den
                                // migrierten Ursprung von aktueller Korruption zu
                                // unterscheiden.
                            }
                        }
                    }
                }
            }
            snapshot.version = QUEUE_JOURNAL_VERSION;
            true
        } else {
            false
        };
        if snapshot.version != QUEUE_JOURNAL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Queue-Journalversion {} wird nicht unterstützt (erwartet {})",
                    snapshot.version, QUEUE_JOURNAL_VERSION
                ),
            ));
        }
        let mut recovered = false;
        for entry in &mut snapshot.entries {
            // Frühere Migrationen haben auch einen bereits versuchten Retry als
            // sicher ungestartet markiert. Die erhaltene Versuchszahl und
            // Recoverydaten korrigieren diesen Altstand vor der Validierung.
            if has_legacy_unbound_webdav_delete(entry)
                && entry.state == QueueState::Failed
                && entry.error_code == Some(crate::ErrorCode::PlanDrift)
                && entry.attention.is_none()
                && has_prior_execution_evidence(entry)
            {
                entry.state = QueueState::Interrupted;
                entry.progress.phase = "recovery".into();
                entry.progress.current.clear();
                entry.error = Some(LEGACY_WEBDAV_UNCLEAR_ERROR.into());
                entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                entry.recovery_available = false;
                entry.bidirectional_completion_recovery = None;
                recovered = true;
            }
        }
        let mut operation_ids = HashSet::with_capacity(snapshot.entries.len());
        for entry in &snapshot.entries {
            if entry.operation_id.is_empty() || !operation_ids.insert(entry.operation_id.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Queue-Journal enthält eine leere oder mehrfach vergebene Operations-ID",
                ));
            }
            validate_persisted_entry_state(entry).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Queue-Eintrag {} besitzt einen ungültigen Zustandsdatensatz: {error}",
                        entry.operation_id
                    ),
                )
            })?;
            validate_persisted_entry_operation(entry).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Queue-Eintrag {} ist ungültig: {error}", entry.operation_id),
                )
            })?;
            validate_profile_bindings(&entry.profile_bindings).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Queue-Eintrag {} besitzt ungültige Profilbindungen: {error}",
                        entry.operation_id
                    ),
                )
            })?;
        }
        let staging_recovery =
            crate::local_staging::recover_queue_beneath(&path, &process_lock.root)?;
        for recovery in &staging_recovery {
            if snapshot
                .entries
                .iter()
                .any(|entry| entry.operation_id == recovery.operation)
            {
                continue;
            }
            let Some((root, account_digest)) = &recovery.ftps_root else {
                continue;
            };
            snapshot.entries.push(QueueEntry {
                operation_id: recovery.operation.clone(),
                operation: QueueOperation::FtpsRecovery { identity: Box::new(root.clone()), account_digest: *account_digest },
                profile_bindings: Vec::new(),
                created_unix_ms: unix_millis()?,
                state: QueueState::Interrupted,
                attempts: 1,
                progress: Progress::default(),
                attention: None,
                error: Some("FTPS-Vorbereitung wurde unterbrochen; mögliche Proben- und STOU-Reste prüfen. Ein STOU-Name kann unbekannt sein. Kein automatischer Wiederholungs- oder Löschweg.".into()),
                error_code: Some(crate::ErrorCode::OutcomeUnclear),
                temp_recovery_paths: recovery.paths.clone(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            });
            recovered = true;
        }
        for entry in &mut snapshot.entries {
            // Eine gespeicherte Klasse ersetzt keine frisch angezeigte Wirkung.
            // Ein offener Tempintent sperrt ohnehin jede Wiederholung; sicher noch
            // nicht gestartete Additionen fragen nach dem Neustart erneut.
            if !matches!(entry.state, QueueState::Completed | QueueState::Cancelled) {
                if let QueueOperation::ProviderFile {
                    kind: FileQueueKind::Copy,
                    source,
                    target,
                    overwrite: false,
                    copy_new_best_effort_expected,
                    ..
                } = &mut entry.operation
                {
                    if (matches!(target.as_ref(), BidirectionalQueueEndpoint::Ftps { .. })
                        || (matches!(source.as_ref(), BidirectionalQueueEndpoint::Local { .. })
                            && matches!(
                                target.as_ref(),
                                BidirectionalQueueEndpoint::WebDav { .. }
                            )))
                        && copy_new_best_effort_expected.take().is_some()
                    {
                        recovered = true;
                    }
                }
            }
            for recovery in staging_recovery
                .iter()
                .filter(|recovery| recovery.operation == entry.operation_id)
            {
                for path in &recovery.paths {
                    if !entry.temp_recovery_paths.contains(path) {
                        entry.temp_recovery_paths.push(path.clone());
                        recovered = true;
                    }
                }
                if matches!(
                    entry.state,
                    QueueState::Running
                        | QueueState::Validating
                        | QueueState::Interrupted
                        | QueueState::Failed
                ) {
                    entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                    entry.error = Some(if recovery.ftps_root.is_some() {
                        "FTPS-Intent des vorherigen Laufs blieb erhalten; mögliche Proben- und STOU-Reste manuell prüfen. Ein STOU-Name kann unbekannt sein; keine automatische Wiederholung oder Löschung."
                    } else {
                        "Zwischenstände des vorherigen Laufs blieben erhalten; vor Wiederholung Ziel und Sicherung prüfen"
                    }.into());
                    entry.state = QueueState::Interrupted;
                    recovered = true;
                }
            }
            // Auch zwischen zwei abgeschlossenen Additionen kann der Prozess
            // enden, wenn deren einzelne Intents schon abgeschlossen sind. Die
            // laufende WebDAV-Kopie bleibt dann ohne automatischen Retry gesperrt.
            if entry.state == QueueState::Running {
                if let QueueOperation::ProviderFile {
                    kind,
                    source,
                    target,
                    items,
                    destination,
                    overwrite,
                    ..
                } = &entry.operation
                {
                    if matches!(target.as_ref(), BidirectionalQueueEndpoint::Ftps { .. })
                        || (*kind == FileQueueKind::Move
                            && matches!(source.as_ref(), BidirectionalQueueEndpoint::Ftps { .. }))
                        || (*kind == FileQueueKind::Copy
                            && !overwrite
                            && matches!(source.as_ref(), BidirectionalQueueEndpoint::Local { .. })
                            && matches!(target.as_ref(), BidirectionalQueueEndpoint::WebDav { .. }))
                    {
                        entry.state = QueueState::Interrupted;
                        entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                        entry.error=Some("Prozessende während FTP-/FTPS- oder additiver WebDAV-Dateioperation; Teilwirkungen und mögliche Restpfade prüfen, nicht automatisch wiederholen".into());
                        for item in items {
                            if let Some(name) = Path::new(item).file_name() {
                                let path = Path::new(destination)
                                    .join(name)
                                    .to_string_lossy()
                                    .into_owned();
                                if !entry.temp_recovery_paths.contains(&path) {
                                    entry.temp_recovery_paths.push(path);
                                }
                            }
                        }
                        recovered = true;
                    }
                }
            }
            if entry.state == QueueState::Running {
                let ftps_target = match &entry.operation {
                    QueueOperation::FtpsSync {
                        direction: SftpQueueDirection::Upload,
                        remote_root,
                        ..
                    } => Some(remote_root.clone()),
                    QueueOperation::ProviderSync { target, .. }
                        if matches!(target.as_ref(), BidirectionalQueueEndpoint::Ftps { .. }) =>
                    {
                        Some(target.root().to_owned())
                    }
                    _ => None,
                };
                if let Some(root) = ftps_target {
                    entry.state = QueueState::Interrupted;
                    entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                    entry.error = Some("Prozessende während FTPS-Zielwirkung; Zielbaum und mögliche STOU-Reste manuell prüfen, nicht automatisch wiederholen".into());
                    if !entry.temp_recovery_paths.contains(&root) {
                        entry.temp_recovery_paths.push(root);
                    }
                    recovered = true;
                }
            }
            if matches!(entry.state, QueueState::Validating | QueueState::Running) {
                entry.state = QueueState::Interrupted;
                entry.error = Some(
                    "Der vorherige Prozess endete während dieser Operation; vor Wiederholung erneut prüfen"
                        .into(),
                );
                entry.error_code = Some(crate::ErrorCode::Interrupted);
                recovered = true;
            } else if entry.state == QueueState::AwaitingConfirmation
                && matches!(entry.attention, Some(QueueAttention::RemoteRun { .. }))
            {
                // Die Phrase allein ist wertlos: Verbindung, Handles und Permit
                // existierten nur im alten Prozess. Der Nutzer muss bewusst neu
                // prüfen, statt auf eine nicht mehr vorhandene Vorbereitung zu klicken.
                entry.state = QueueState::Interrupted;
                entry.attention = None;
                entry.error = Some(
                    if matches!(entry.operation, QueueOperation::LocalSync { .. }) {
                        "Lokale Sync-Vorbereitung ging beim Prozessende verloren; vor Wiederholung neu prüfen".into()
                    } else {
                        "Remote-Vorbereitung ging beim Prozessende verloren; vor Wiederholung neu prüfen".into()
                    },
                );
                entry.error_code = Some(crate::ErrorCode::Interrupted);
                recovered = true;
            }
            if let Some(completion) = entry.bidirectional_completion_recovery {
                let valid_state = match completion {
                    BidirectionalCompletionRecovery::Complete => {
                        matches!(entry.state, QueueState::Interrupted | QueueState::Completed)
                    }
                    BidirectionalCompletionRecovery::RolledBack => {
                        matches!(entry.state, QueueState::Interrupted | QueueState::Cancelled)
                    }
                };
                if !matches!(entry.operation, QueueOperation::Bidirectional { .. }) || !valid_state
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Queue-Eintrag {} besitzt einen ungültigen Abschluss-Recovery-Marker",
                            entry.operation_id
                        ),
                    ));
                }
                let expected_phase = match entry.state {
                    QueueState::Completed => "done",
                    QueueState::Cancelled => "rolled_back",
                    _ => "completion_unclear",
                };
                let changed = entry.progress.phase != expected_phase
                    || !entry.progress.current.is_empty()
                    || entry.recovery_available;
                entry.progress.phase = expected_phase.into();
                entry.progress.current.clear();
                entry.recovery_available = false;
                recovered |= changed;
            } else if matches!(entry.operation, QueueOperation::Bidirectional { .. })
                && matches!(entry.state, QueueState::Failed | QueueState::Interrupted)
            {
                let old_state = entry.state;
                let old_phase = entry.progress.phase.clone();
                let had_current = !entry.progress.current.is_empty();
                let old_recovery_available = entry.recovery_available;
                let old_error_code = entry.error_code;
                entry.state = QueueState::Interrupted;
                entry.progress.current.clear();
                match bidirectional_recovery_kind(&entry.operation) {
                    crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup => {
                        entry.state = QueueState::Interrupted;
                        entry.progress.phase = "completion_unclear".into();
                        entry.recovery_available = false;
                        entry.bidirectional_completion_recovery =
                            Some(BidirectionalCompletionRecovery::Complete);
                    }
                    crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup => {
                        entry.state = QueueState::Interrupted;
                        entry.progress.phase = "completion_unclear".into();
                        entry.recovery_available = false;
                        entry.bidirectional_completion_recovery =
                            Some(BidirectionalCompletionRecovery::RolledBack);
                    }
                    crate::bidirectional::BidirectionalRunRecovery::ResumeOrRollback => {
                        entry.progress.phase = "recovery".into();
                        entry.recovery_available = true;
                    }
                    crate::bidirectional::BidirectionalRunRecovery::Absent => {
                        entry.progress.phase = "interrupted".into();
                        entry.recovery_available = false;
                        // Ein älterer Stand hat hier `OutcomeUnclear` persistiert,
                        // obwohl kein Laufjournal existiert. Ohne diese Reparatur
                        // bliebe der Eintrag über jeden Neustart hinweg
                        // unwiederholbar und blockierte als erster nicht-terminaler
                        // Eintrag die ganze Fensterqueue. Die Kombination ist
                        // eindeutig: Ein Abschlussmarker wird oben behandelt, und
                        // `ResumeOrRollback` setzt `recovery_available`.
                        if entry.error_code == Some(crate::ErrorCode::OutcomeUnclear) {
                            entry.error_code = Some(crate::ErrorCode::Interrupted);
                        }
                    }
                }
                recovered |= old_state != entry.state
                    || old_phase != entry.progress.phase
                    || had_current
                    || old_recovery_available != entry.recovery_available
                    || old_error_code != entry.error_code;
            } else {
                entry.recovery_available = false;
            }
        }
        let queue = Self {
            path,
            journal: Mutex::new(snapshot),
            journal_outcome_unclear: AtomicBool::new(false),
            #[cfg(test)]
            fail_terminal_progress_update: AtomicBool::new(false),
            #[cfg(test)]
            fail_nonterminal_progress_update_after: std::sync::atomic::AtomicUsize::new(usize::MAX),
            #[cfg(test)]
            terminal_progress_update_attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_file_failure_commit: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_file_completion_commit: AtomicBool::new(false),
            #[cfg(test)]
            fail_reconcile_sync: AtomicBool::new(false),
            _process_lock: process_lock,
        };
        if journal_missing {
            let snapshot = queue.journal.lock().unwrap();
            write_snapshot_bound(
                &queue._process_lock.root,
                &queue.path,
                &snapshot,
                AtomicWriteMode::Create,
            )?;
        } else if migrated || recovered {
            let snapshot = queue.journal.lock().unwrap();
            write_snapshot_bound(
                &queue._process_lock.root,
                &queue.path,
                &snapshot,
                AtomicWriteMode::Replace,
            )?;
        }
        Ok(queue)
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        self.journal.lock().unwrap().clone()
    }

    /// Bindet eine Vorbereitung an dieses Fensterjournal, ohne den geteilten
    /// Verbindungspool zu verändern. Ein vor dem Enqueue offener Intent wird beim
    /// Neustart als reine manuelle Recovery sichtbar.
    pub fn bind_provider_preparation(
        &self,
        endpoint: &ProviderEndpoint,
    ) -> io::Result<ProviderEndpoint> {
        endpoint.with_queue_owner(self, None, &[])
    }

    /// Der Abbruchtoken bleibt mit dem nicht serialisierbaren Probenowner
    /// verbunden. Das Fenster und seine bereits begonnene Probe teilen ihn.
    pub fn bind_provider_preparation_with_cancel(
        &self,
        endpoint: &ProviderEndpoint,
        cancel: &Cancel,
    ) -> io::Result<ProviderEndpoint> {
        endpoint.with_queue_owner_with_cancel(self, None, &[], Some(cancel))
    }

    pub(crate) fn ftps_owner(
        &self,
        operation: Option<&str>,
    ) -> io::Result<(
        crate::local_staging::StagingOwner,
        Arc<OperationQueueProcessLock>,
        crate::local_fs::RootHandle,
    )> {
        if !self._process_lock.root.is_current()? {
            return Err(io::Error::other("FTPS-Queueordner wurde ausgetauscht"));
        }
        if self.journal_outcome_unclear.load(Ordering::SeqCst) {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Queuejournal ist nicht sicher gebunden",
            ));
        }
        let operation = match operation {
            Some(operation) => {
                let snapshot = self.snapshot();
                if !snapshot.entries.iter().any(|entry| {
                    entry.operation_id == operation
                        && matches!(entry.state, QueueState::Validating | QueueState::Running)
                }) {
                    return Err(io::Error::other(
                        "FTPS-Owner benötigt den aktiven Queueeintrag",
                    ));
                }
                operation.to_owned()
            }
            None => new_operation_id()?,
        };
        Ok((
            crate::local_staging::StagingOwner {
                queue: self.path.clone(),
                operation,
            },
            self._process_lock.clone(),
            self._process_lock.root.clone(),
        ))
    }

    #[cfg(test)]
    fn fail_terminal_progress_update_for_test(&self) {
        self.fail_terminal_progress_update
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn fail_nonterminal_progress_update_after_for_test(&self, successful_updates: usize) {
        self.fail_nonterminal_progress_update_after
            .store(successful_updates, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn terminal_progress_update_attempts_for_test(&self) -> usize {
        self.terminal_progress_update_attempts
            .load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn fail_next_file_failure_commit_for_test(&self) {
        self.fail_next_file_failure_commit
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn fail_next_file_completion_commit_for_test(&self) {
        self.fail_next_file_completion_commit
            .store(true, Ordering::SeqCst);
    }

    /// Append-only Sicherheitsprotokoll ohne Inhalts- oder credentialtragende
    /// Endpunktpfade. Nach einer teilwirksamen Provider-Ersetzung steht hier bewusst
    /// der exakte geheimnisfreie Quarantänelauf, damit Recovery nicht vom Fehlertext
    /// abhängt. Das Queue-Journal bleibt für den Zustandsübergang zuständig.
    fn append_file_audit(
        &self,
        operation_id: &str,
        operation: &str,
        status: &str,
        progress: &Progress,
        context: &ProviderAuditContext,
        recovery: Option<&ProviderFileRecovery>,
    ) -> io::Result<()> {
        self.append_file_audit_with_temp_paths(
            operation_id,
            operation,
            status,
            progress,
            context,
            FileAuditRecovery {
                provider: recovery,
                temp_paths: &[],
                retained_backups: &[],
            },
        )
    }

    fn append_file_audit_with_temp_paths(
        &self,
        operation_id: &str,
        operation: &str,
        status: &str,
        progress: &Progress,
        context: &ProviderAuditContext,
        recovery: FileAuditRecovery<'_>,
    ) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Queue-Audit braucht einen Elternordner",
            )
        })?;
        let root = &self._process_lock.root;
        if parent != root.path() || !root.is_current()? {
            return Err(io::Error::other("Queue-Auditordner wurde ausgetauscht"));
        }
        let journal_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Queue-Journalname ist nicht eindeutig kodierbar",
                )
            })?;
        let audit_name = match journal_name {
            "operations-queue.json" => "operations-audit.jsonl".to_owned(),
            name if name.starts_with("operations-queue-") && name.ends_with(".json") => {
                let suffix = &name["operations-queue-".len()..name.len() - ".json".len()];
                format!("operations-audit-{suffix}.jsonl")
            }
            name => format!("{}-audit.jsonl", name.strip_suffix(".json").unwrap_or(name)),
        };
        let fd = rustix::fs::openat(
            root.fd(),
            audit_name,
            rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::APPEND
                | rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?;
        let mut file = File::from(fd);
        if !file.metadata()?.is_file() || !root.is_current()? {
            return Err(io::Error::other(
                "Queue-Audit verlor seine reguläre Datei oder Ordnerbindung",
            ));
        }
        let record = serde_json::json!({
            "version": 4,
            "unix_ms": unix_millis()?,
            "operation_id": operation_id,
            "operation": operation,
            "status": status,
            "source_provider": context.source_provider.map(provider_audit_label),
            "target_provider": provider_audit_label(context.target_provider),
            "execution_class": context.execution_class,
            "source_removal_best_effort_expected": context.source_removal_best_effort_expected,
            "target_copy_new_best_effort_expected": context.target_copy_new_best_effort_expected,
            "target_replace_best_effort_expected": context.target_replace_best_effort_expected,
            "source_removal_best_effort_actual": context.source_removal_best_effort_actual,
            "target_copy_new_best_effort_actual": context.target_copy_new_best_effort_actual,
            "target_replace_best_effort_actual": context.target_replace_best_effort_actual,
            "recovery": recovery.provider,
            "temp_recovery_paths": recovery.temp_paths,
            "retained_backup_paths": recovery.retained_backups,
            "files_done": progress.files_done,
            "bytes_done": progress.bytes_done,
        });
        serde_json::to_writer(&mut file, &record).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_data()
    }

    pub fn enqueue(&self, operation: QueueOperation) -> io::Result<QueueEntry> {
        validate_enqueue_operation(&operation)?;
        let entry = QueueEntry {
            operation_id: new_operation_id()?,
            operation,
            profile_bindings: Vec::new(),
            created_unix_ms: unix_millis()?,
            state: QueueState::Pending,
            attempts: 0,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        };
        self.transact(|journal| {
            journal.entries.push(entry.clone());
            Ok(entry)
        })
    }

    /// Reiht genau eine Operation ein und klärt einen späten Fehler des atomaren
    /// Schreibens anhand des vollständig erwarteten Journalstands. Ist genau der
    /// neue Stand sichtbar und dessen Datei-/Parent-fsync nachgeholt, wird er in
    /// den RAM übernommen und Erfolg gemeldet. Scheitert der Sync, bleibt die
    /// Queue gesperrt. Ist
    /// nachweislich noch genau der alte Stand sichtbar, bleibt der Fehler retryfähig.
    /// Schon ein zusätzlicher oder veränderter Eintrag macht den Ausgang unklar und
    /// sperrt weitere Mutationen.
    pub fn enqueue_reconciled(&self, operation: QueueOperation) -> io::Result<QueueEntry> {
        self.enqueue_reconciled_using(operation, |path, snapshot| {
            write_snapshot_bound(
                &self._process_lock.root,
                path,
                snapshot,
                AtomicWriteMode::Replace,
            )
        })
    }

    /// GUI-Remoteoperationen persistieren zusätzlich die Profilrevisionen, die
    /// beim Erzeugen von Plan und Konfliktliste aktuell waren. Der Core verbindet
    /// anhand der geheimnisfreien Konfiguration; die Tauri-Schicht prüft vor jeder
    /// Vorbereitung. Gewöhnliche WebDAV-Dateiuploads prüfen die gespeicherte
    /// Bindung zusätzlich im Core bis vor ihre konkrete weitere Wirkung.
    pub fn enqueue_reconciled_with_profile_bindings(
        &self,
        operation: QueueOperation,
        profile_bindings: Vec<crate::RemoteProfileBinding>,
    ) -> io::Result<QueueEntry> {
        self.enqueue_reconciled_with_profile_bindings_using(
            operation,
            profile_bindings,
            |path, snapshot| {
                write_snapshot_bound(
                    &self._process_lock.root,
                    path,
                    snapshot,
                    AtomicWriteMode::Replace,
                )
            },
        )
    }

    fn enqueue_reconciled_using(
        &self,
        operation: QueueOperation,
        write: impl FnOnce(&Path, &QueueSnapshot) -> io::Result<()>,
    ) -> io::Result<QueueEntry> {
        self.enqueue_reconciled_with_profile_bindings_using(operation, Vec::new(), write)
    }

    fn enqueue_reconciled_with_profile_bindings_using(
        &self,
        operation: QueueOperation,
        profile_bindings: Vec<crate::RemoteProfileBinding>,
        write: impl FnOnce(&Path, &QueueSnapshot) -> io::Result<()>,
    ) -> io::Result<QueueEntry> {
        validate_enqueue_operation(&operation)?;
        validate_profile_bindings(&profile_bindings)?;
        let entry = QueueEntry {
            operation_id: new_operation_id()?,
            operation,
            profile_bindings,
            created_unix_ms: unix_millis()?,
            state: QueueState::Pending,
            attempts: 0,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        };
        self.transact_using(
            |journal| {
                journal.entries.push(entry.clone());
                Ok(entry)
            },
            write,
        )
    }

    /// Reiht eine Operation für einen unmittelbaren CLI-Lauf ein und claimt sie in
    /// derselben atomaren Journalversion. Anders als `enqueue` gefolgt von
    /// [`Self::claim_entry`] kann zwischen beiden Zuständen niemals ein ausführbares
    /// `Pending` auf Platte liegen.
    ///
    /// Das ist auch für den seltenen Fehler nach dem atomaren Rename wichtig: Hat der
    /// neue Journalstand den Commit bereits erreicht, enthält er `Validating` und wird
    /// beim nächsten Öffnen zu `Interrupted`. Der Aufrufer kann also einen Fehler sehen,
    /// ohne dass ein späterer GUI-Worker die vermeintlich gescheiterte Aktion ausführt.
    pub fn enqueue_claimed(&self, operation: QueueOperation) -> io::Result<QueueEntry> {
        self.enqueue_claimed_with_profile_bindings(operation, Vec::new())
    }

    pub fn enqueue_claimed_with_profile_bindings(
        &self,
        operation: QueueOperation,
        profile_bindings: Vec<crate::RemoteProfileBinding>,
    ) -> io::Result<QueueEntry> {
        validate_enqueue_operation(&operation)?;
        validate_profile_bindings(&profile_bindings)?;
        let entry = QueueEntry {
            operation_id: new_operation_id()?,
            operation,
            profile_bindings,
            created_unix_ms: unix_millis()?,
            state: QueueState::Validating,
            attempts: 1,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        };
        self.transact(|journal| {
            if journal.paused
                || journal
                    .entries
                    .iter()
                    .any(|entry| !entry.state.is_terminal())
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Eine ältere offene Queue-Operation muss zuerst geklärt werden",
                ));
            }
            journal.entries.push(entry.clone());
            Ok(entry)
        })
    }

    /// Exakt einen nächsten Eintrag atomar claimen. Ein wartender Bestätigungs- oder
    /// Recovery-Eintrag blockiert die Reihenfolge, bis der Nutzer ihn behandelt.
    pub fn claim_next(&self) -> io::Result<Option<QueueEntry>> {
        self.claim_front(None)
    }

    /// Claimt atomar genau den benannten Eintrag — und nur dann, wenn er der
    /// vorderste nicht-terminale Eintrag und `Pending` ist. Ein anderer offener
    /// Eintrag davor bleibt vollständig unangetastet: Anders als ein blindes
    /// [`Self::claim_next`] hinterlässt dieser Weg niemals einen fremden Eintrag
    /// in `Validating`, der beim nächsten Öffnen zu `Interrupted` würde.
    pub fn claim_entry(&self, operation_id: &str) -> io::Result<Option<QueueEntry>> {
        self.claim_front(Some(operation_id))
    }

    /// Gemeinsamer Claim des vordersten nicht-terminalen Eintrags.
    ///
    /// `expected` verlangt zusätzlich genau diese Operations-ID; `None` claimt den
    /// vordersten Eintrag unabhängig von seiner ID. Alles andere ist für beide Wege
    /// gleich und darf es auch bleiben: Eine pausierte Queue oder ein bereits
    /// `Validating`/`Running` stehender Eintrag sperrt den Claim, und der vorderste
    /// nicht-terminale Eintrag muss `Pending` sein — sonst wird nichts angefasst.
    fn claim_front(&self, expected: Option<&str>) -> io::Result<Option<QueueEntry>> {
        self.transact(|journal| {
            if journal.paused
                || journal.entries.iter().any(|entry| {
                    matches!(entry.state, QueueState::Validating | QueueState::Running)
                })
            {
                return Ok(None);
            }
            let next = journal
                .entries
                .iter()
                .position(|entry| !entry.state.is_terminal());
            let Some(index) = next else {
                return Ok(None);
            };
            if journal.entries[index].state != QueueState::Pending
                || expected.is_some_and(|id| journal.entries[index].operation_id != id)
            {
                return Ok(None);
            }
            let entry = &mut journal.entries[index];
            entry.state = QueueState::Validating;
            entry.attempts = entry.attempts.saturating_add(1);
            entry.progress = Progress::default();
            entry.error = None;
            entry.error_code = None;
            entry.attention = None;
            entry.recovery_available = false;
            entry.bidirectional_completion_recovery = None;
            Ok(Some(entry.clone()))
        })
    }

    /// Erst nach erfolgreicher frischer Validierung aufrufen. Die Persistenz geschieht
    /// vor der ersten Mutation und ist damit die Crash-Grenze des Schedulers.
    pub fn mark_running(&self, operation_id: &str) -> io::Result<()> {
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::Validating)?;
            entry.state = QueueState::Running;
            entry.recovery_available = false;
            entry.bidirectional_completion_recovery = None;
            Ok(())
        })
    }

    pub fn update_progress(&self, operation_id: &str, progress: &Progress) -> io::Result<()> {
        #[cfg(test)]
        if file_progress_is_terminal(progress)
            && self.fail_terminal_progress_update.load(Ordering::SeqCst)
        {
            self.terminal_progress_update_attempts
                .fetch_add(1, Ordering::SeqCst);
            return Err(io::Error::other(
                "Testfehler beim getrennten terminalen Fortschritts-Commit",
            ));
        }
        #[cfg(test)]
        if !file_progress_is_terminal(progress) {
            let remaining = self
                .fail_nonterminal_progress_update_after
                .load(Ordering::SeqCst);
            if remaining == 0 {
                self.fail_nonterminal_progress_update_after
                    .store(usize::MAX, Ordering::SeqCst);
                return Err(io::Error::other(
                    "Testfehler beim nichtterminalen Fortschritts-Commit",
                ));
            }
            if remaining != usize::MAX {
                self.fail_nonterminal_progress_update_after
                    .fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::Running)?;
            entry.progress = progress.clone();
            Ok(())
        })
    }

    pub fn complete(&self, operation_id: &str, progress: Progress) -> io::Result<()> {
        self.finish(operation_id, QueueState::Completed, progress, None)
    }

    pub fn cancel(&self, operation_id: &str, progress: Progress) -> io::Result<()> {
        self.finish(operation_id, QueueState::Cancelled, progress, None)
    }

    fn finish_file(
        &self,
        operation_id: &str,
        cancelled: bool,
        progress: Progress,
        temp_recovery_paths: &[String],
        retained_backup_paths: &[String],
    ) -> io::Result<()> {
        #[cfg(test)]
        if self
            .fail_next_file_completion_commit
            .swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::other(
                "Testfehler beim terminalen Datei-Erfolgscommit",
            ));
        }
        self.update(operation_id, |entry| {
            if !matches!(
                entry.operation,
                QueueOperation::File { .. } | QueueOperation::ProviderFile { .. }
            ) {
                return Err(invalid_transition(entry.state));
            }
            require_state(entry, QueueState::Running)?;
            entry.state = if cancelled {
                QueueState::Cancelled
            } else {
                QueueState::Completed
            };
            entry.progress = progress;
            for path in retained_backup_paths {
                if !entry.retained_backup_paths.contains(path) {
                    entry.retained_backup_paths.push(path.clone());
                }
            }
            entry.error = None;
            entry.error_code = None;
            entry.attention = None;
            entry.recovery_available = false;
            entry.bidirectional_completion_recovery = None;
            for path in temp_recovery_paths {
                if !entry.temp_recovery_paths.contains(path) {
                    entry.temp_recovery_paths.push(path.clone());
                }
            }
            Ok(())
        })
    }

    /// Wiederholt ausschließlich den terminalen Journalabschluss einer bereits
    /// vollständig vollzogenen Dateioperation. Der Beleg entsteht intern direkt
    /// vor dem ersten Abschlussversuch und enthält keine neue Ausführungsfreigabe.
    pub fn retry_file_completion(
        &self,
        operation_id: &str,
        completion: &PendingFileCompletion,
    ) -> io::Result<()> {
        self.finish_file(
            operation_id,
            completion.cancelled,
            completion.progress.clone(),
            &completion.temp_recovery_paths,
            &completion.stats.retained_backup_paths,
        )
    }

    /// Schreibt nach einem erfolgreich nachgeholten Queueabschluss denselben
    /// terminalen Auditdatensatz wie der reguläre Executorpfad. Queueabschluss und
    /// Audit bleiben absichtlich getrennte Schritte: Schlägt nur das Audit fehl,
    /// ist die vollzogene Dateiwirkung dennoch nicht erneut ausführbar.
    pub fn append_file_completion_audit(
        &self,
        operation_id: &str,
        completion: &PendingFileCompletion,
    ) -> io::Result<()> {
        self.append_file_audit_with_temp_paths(
            operation_id,
            completion.audit_operation,
            if completion.cancelled {
                "cancelled"
            } else {
                "completed"
            },
            &completion.progress,
            &completion.audit_context,
            FileAuditRecovery {
                provider: None,
                temp_paths: &completion.temp_recovery_paths,
                retained_backups: &completion.stats.retained_backup_paths,
            },
        )
    }

    pub fn fail(
        &self,
        operation_id: &str,
        progress: Progress,
        error: impl Into<String>,
    ) -> io::Result<()> {
        self.fail_with_temp_paths(
            operation_id,
            progress,
            error,
            None,
            FileRecoveryPaths {
                temporary: &[],
                backups: &[],
            },
            false,
        )
    }

    /// Persistiert neben dem Anzeigetext auch Fehlercode und bekannte Recoverypfade.
    /// Die GUI kann dadurch einen unklaren Mutationsausgang nach Neustart weiterhin
    /// von einem gewöhnlichen, sicher wiederholbaren Fehler unterscheiden.
    pub fn fail_coded(
        &self,
        operation_id: &str,
        progress: Progress,
        error: &io::Error,
    ) -> io::Result<()> {
        self.fail_coded_with_recovery(
            operation_id,
            progress,
            error,
            &crate::error::recovery_paths(error),
        )
    }

    /// Schließt eine fehlgeschlagene Dateioperation samt allen strukturiert
    /// bekannten Temp-Prüfpfaden in genau einer Journalmutation ab. Ein späteres
    /// „Neu prüfen“ darf den Fehlertext löschen, diese Recoveryhinweise aber nicht.
    pub fn fail_file(
        &self,
        operation_id: &str,
        progress: Progress,
        error: impl Into<String>,
        temp_recovery_paths: &[String],
    ) -> io::Result<()> {
        #[cfg(test)]
        if self
            .fail_next_file_failure_commit
            .swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::other(
                "Testfehler beim terminalen Datei-Fehlercommit",
            ));
        }
        self.fail_with_temp_paths(
            operation_id,
            progress,
            error,
            None,
            FileRecoveryPaths {
                temporary: temp_recovery_paths,
                backups: &[],
            },
            true,
        )
    }

    pub fn fail_file_coded(
        &self,
        operation_id: &str,
        progress: Progress,
        error: &io::Error,
        temp_recovery_paths: &[String],
    ) -> io::Result<()> {
        #[cfg(test)]
        if self
            .fail_next_file_failure_commit
            .swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::other(
                "Testfehler beim terminalen Datei-Fehlercommit",
            ));
        }
        self.fail_with_temp_paths(
            operation_id,
            progress,
            error.to_string(),
            Some(crate::error_code(error)),
            FileRecoveryPaths {
                temporary: temp_recovery_paths,
                backups: &[],
            },
            true,
        )
    }

    pub fn fail_file_actuals_coded(
        &self,
        operation_id: &str,
        progress: Progress,
        error: &io::Error,
        actuals: &FileOperationActuals,
    ) -> io::Result<()> {
        #[cfg(test)]
        if self
            .fail_next_file_failure_commit
            .swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::other(
                "Testfehler beim terminalen Datei-Fehlercommit",
            ));
        }
        self.fail_with_temp_paths(
            operation_id,
            progress,
            error.to_string(),
            Some(crate::error_code(error)),
            FileRecoveryPaths {
                temporary: &actuals.temp_recovery_paths,
                backups: &actuals.retained_backup_paths,
            },
            true,
        )
    }

    fn fail_coded_with_recovery(
        &self,
        operation_id: &str,
        progress: Progress,
        error: &io::Error,
        paths: &[String],
    ) -> io::Result<()> {
        self.fail_with_temp_paths(
            operation_id,
            progress,
            error.to_string(),
            Some(crate::error_code(error)),
            FileRecoveryPaths {
                temporary: paths,
                backups: &[],
            },
            false,
        )
    }

    fn fail_with_temp_paths(
        &self,
        operation_id: &str,
        progress: Progress,
        error: impl Into<String>,
        error_code: Option<crate::ErrorCode>,
        paths: FileRecoveryPaths<'_>,
        require_file_operation: bool,
    ) -> io::Result<()> {
        let error = error.into();
        self.transact(|journal| {
            let entry = find_mut(journal, operation_id)?;
            if matches!(entry.operation, QueueOperation::Bidirectional { .. }) {
                // Sicherheitsgrenze: Ein generisches `Failed` würde das offene
                // Core-Journal und seinen speziellen Recoveryweg verbergen. Nur
                // `interrupt_bidirectional` darf diesen Laufzustand einordnen.
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Bidirektionale Operation {operation_id} darf nicht generisch als fehlgeschlagen abgeschlossen werden"
                    ),
                ));
            }
            if require_file_operation
                && !matches!(
                    entry.operation,
                    QueueOperation::File { .. } | QueueOperation::ProviderFile { .. }
                )
            {
                return Err(invalid_transition(entry.state));
            }
            if !matches!(entry.state, QueueState::Validating | QueueState::Running) {
                return Err(invalid_transition(entry.state));
            }
            entry.state = QueueState::Failed;
            entry.progress = progress;
            entry.error = Some(error);
            entry.error_code = error_code;
            entry.attention = None;
            entry.recovery_available = false;
            entry.bidirectional_completion_recovery = None;
            for path in paths.temporary {
                if !entry.temp_recovery_paths.contains(path) {
                    entry.temp_recovery_paths.push(path.clone());
                }
            }
            for path in paths.backups { if !entry.retained_backup_paths.contains(path) { entry.retained_backup_paths.push(path.clone()); } }
            if journal.stop_on_error {
                journal.paused = true;
            }
            Ok(())
        })
    }

    /// Ein bidirektionaler Lauf darf nach einem Ausführungsfehler nicht einfach die
    /// nächste Queue-Operation freigeben: Sein Recovery-Journal kann bereits echte
    /// Teilprojektionen binden. `Interrupted` bleibt deshalb bewusst nicht-terminal,
    /// bis der Nutzer denselben Lauf fortsetzt oder vollständig zurückrollt.
    pub fn interrupt_bidirectional(
        &self,
        operation_id: &str,
        progress: Progress,
        error: impl Into<String>,
    ) -> io::Result<()> {
        self.interrupt_bidirectional_with_paths(operation_id, progress, error.into(), &[])
    }

    fn interrupt_bidirectional_coded(
        &self,
        operation_id: &str,
        progress: Progress,
        error: &io::Error,
    ) -> io::Result<()> {
        self.interrupt_bidirectional_with_paths(
            operation_id,
            progress,
            error.to_string(),
            &crate::error::recovery_paths(error),
        )
    }

    fn interrupt_bidirectional_with_paths(
        &self,
        operation_id: &str,
        progress: Progress,
        error: String,
        paths: &[String],
    ) -> io::Result<()> {
        self.transact(|journal| {
            let entry = find_mut(journal, operation_id)?;
            if !matches!(entry.operation, QueueOperation::Bidirectional { .. })
                || !matches!(
                    entry.state,
                    QueueState::Validating | QueueState::Running | QueueState::Interrupted
                )
            {
                return Err(invalid_transition(entry.state));
            }
            entry.state = QueueState::Interrupted;
            entry.progress = progress;
            let recovery_kind = bidirectional_recovery_kind(&entry.operation);
            entry.error = Some(match recovery_kind {
                crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup => format!(
                    "{error}; Daten- und Baselinewirkung sind vollständig bestätigt, nur das Core-Laufjournal muss mutationslos abgeschlossen werden"
                ),
                crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup => format!(
                    "{error}; Rollbackwirkung ist vollständig bestätigt, nur der terminale Queue-/Journalabschluss bleibt offen"
                ),
                _ => error,
            });
            // `Absent` ist der Beweis, dass gar kein Laufjournal existiert: Die
            // Vorbereitung ist vor jeder Mutation gescheitert, der Ausgang ist also
            // gerade nicht unklar. `OutcomeUnclear` würde den Eintrag hier dauerhaft
            // festsetzen — `retry` lehnt diesen Code ab (Zeile mit
            // `ErrorCode::OutcomeUnclear` weiter unten), `acknowledge_unclear_outcome`
            // schließt bidirektionale Operationen aus, und `finish` verweigert den
            // generischen Abschluss. Weil `Interrupted` nicht terminal ist, gäbe
            // `claim_next` danach für dieses Fenster nichts mehr frei. Genau diesen
            // Fall lässt `retry` ausdrücklich zu; er braucht deshalb einen
            // wiederholbaren Code. Nur eine mögliche Teilwirkung bleibt unklar.
            entry.error_code = Some(
                if recovery_kind == crate::bidirectional::BidirectionalRunRecovery::Absent {
                    crate::ErrorCode::Interrupted
                } else {
                    crate::ErrorCode::OutcomeUnclear
                },
            );
            entry.attention = None;
            entry.recovery_available = recovery_kind
                == crate::bidirectional::BidirectionalRunRecovery::ResumeOrRollback;
            entry.bidirectional_completion_recovery = (recovery_kind
                == crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup)
                .then_some(BidirectionalCompletionRecovery::Complete)
                .or_else(|| {
                    (recovery_kind
                        == crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup)
                        .then_some(BidirectionalCompletionRecovery::RolledBack)
                });
            entry.progress.phase = match recovery_kind {
                crate::bidirectional::BidirectionalRunRecovery::ResumeOrRollback => {
                    "recovery".into()
                }
                crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup => {
                    "completion_unclear".into()
                }
                crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup => {
                    "completion_unclear".into()
                }
                crate::bidirectional::BidirectionalRunRecovery::Absent => "interrupted".into(),
            };
            entry.progress.current.clear();
            for path in paths {
                if !entry.temp_recovery_paths.contains(path) {
                    entry.temp_recovery_paths.push(path.clone());
                }
            }
            Ok(())
        })
    }

    /// Prüft den Core-Journalstand unmittelbar vor jedem Resume-/Rollback-Einstieg
    /// erneut. Das ist auch im selben Prozess nötig: Zwei sicher nicht wirkende
    /// Queue-Schreibversuche lassen den älteren RAM-Zustand stehen, während das
    /// Core-Journal die bereits abgeschlossene Datenwirkung eindeutig beweist.
    pub fn refresh_bidirectional_completion_recovery(&self, operation_id: &str) -> io::Result<()> {
        self.refresh_bidirectional_completion_recovery_with_paths(operation_id, &[])
    }

    fn refresh_bidirectional_completion_recovery_with_paths(
        &self,
        operation_id: &str,
        paths: &[String],
    ) -> io::Result<()> {
        let recovery = {
            let journal = self.journal.lock().unwrap();
            let entry = journal
                .entries
                .iter()
                .find(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag nicht gefunden")
                })?;
            if !matches!(entry.operation, QueueOperation::Bidirectional { .. }) {
                return Err(invalid_transition(entry.state));
            }
            bidirectional_recovery_kind(&entry.operation)
        };
        let marker = match recovery {
            crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup => {
                BidirectionalCompletionRecovery::Complete
            }
            crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup => {
                BidirectionalCompletionRecovery::RolledBack
            }
            crate::bidirectional::BidirectionalRunRecovery::Absent
            | crate::bidirectional::BidirectionalRunRecovery::ResumeOrRollback => return Ok(()),
        };
        self.transact(|journal| {
            let entry = find_mut(journal, operation_id)?;
            if !matches!(entry.operation, QueueOperation::Bidirectional { .. }) {
                return Err(invalid_transition(entry.state));
            }
            retain_temp_recovery_paths(entry, paths);
            let terminal_matches = matches!(
                (marker, entry.state),
                (BidirectionalCompletionRecovery::Complete, QueueState::Completed)
                    | (BidirectionalCompletionRecovery::RolledBack, QueueState::Cancelled)
            );
            if entry.bidirectional_completion_recovery == Some(marker)
                && (entry.state == QueueState::Interrupted || terminal_matches)
            {
                return Ok(());
            }
            if entry.state.is_terminal() {
                return Err(invalid_transition(entry.state));
            }
            entry.state = QueueState::Interrupted;
            entry.progress.phase = "completion_unclear".into();
            entry.progress.current.clear();
            entry.error = Some(
                "Datenwirkung ist vollständig bestätigt; nur der mutationslose Abschluss bleibt offen"
                    .into(),
            );
            entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
            entry.attention = None;
            entry.recovery_available = false;
            entry.bidirectional_completion_recovery = Some(marker);
            Ok(())
        })
    }

    /// Persistiert nach bereits erfolgreicher Datenwirkung erst den terminalen
    /// Queuezustand. Ein später Writerfehler wird durch den vollständigen Readback
    /// in `transact_using` geklärt. War der erste Commit sicher wirkungslos, hält
    /// ein zweiter persistierter Marker ausschließlich den noch offenen
    /// Journalabschluss fest; die Datenoperation darf dann nie erneut laufen.
    pub fn finish_bidirectional_after_effect(
        &self,
        operation_id: &str,
        rolled_back: bool,
        progress: Progress,
    ) -> io::Result<()> {
        self.finish_bidirectional_after_effect_with_paths(operation_id, rolled_back, progress, &[])
    }

    fn finish_bidirectional_after_effect_with_paths(
        &self,
        operation_id: &str,
        rolled_back: bool,
        progress: Progress,
        paths: &[String],
    ) -> io::Result<()> {
        let proof = self.bidirectional_completion_proof(operation_id, rolled_back)?;
        self.finish_bidirectional_after_effect_with_proof_using(
            operation_id,
            proof,
            progress,
            paths,
            |path, snapshot| {
                write_snapshot_bound(
                    &self._process_lock.root,
                    path,
                    snapshot,
                    AtomicWriteMode::Replace,
                )
            },
        )
    }

    fn bidirectional_completion_proof(
        &self,
        operation_id: &str,
        rolled_back: bool,
    ) -> io::Result<BidirectionalCompletionProof> {
        let recovery = {
            let journal = self.journal.lock().unwrap();
            let entry = journal
                .entries
                .iter()
                .find(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag nicht gefunden")
                })?;
            if !matches!(entry.operation, QueueOperation::Bidirectional { .. }) {
                return Err(invalid_transition(entry.state));
            }
            bidirectional_recovery_kind(&entry.operation)
        };
        let marker = match (rolled_back, recovery) {
            (false, crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup) => {
                BidirectionalCompletionRecovery::Complete
            }
            (true, crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup) => {
                BidirectionalCompletionRecovery::RolledBack
            }
            _ => {
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Bidirektionaler Queue-Abschluss besitzt keinen passenden persistenten Core-Beleg: Operation {operation_id}, angeforderter Rollback {rolled_back}, Recovery-Zustand {recovery:?}"
                    ),
                ));
            }
        };
        Ok(BidirectionalCompletionProof(marker))
    }

    #[cfg(test)]
    pub(crate) fn finish_bidirectional_after_effect_using(
        &self,
        operation_id: &str,
        rolled_back: bool,
        progress: Progress,
        mut write: impl FnMut(&Path, &QueueSnapshot) -> io::Result<()>,
    ) -> io::Result<()> {
        let proof = self.bidirectional_completion_proof(operation_id, rolled_back)?;
        self.finish_bidirectional_after_effect_with_proof_using(
            operation_id,
            proof,
            progress,
            &[],
            &mut write,
        )
    }

    fn finish_bidirectional_after_effect_with_proof_using(
        &self,
        operation_id: &str,
        proof: BidirectionalCompletionProof,
        progress: Progress,
        paths: &[String],
        mut write: impl FnMut(&Path, &QueueSnapshot) -> io::Result<()>,
    ) -> io::Result<()> {
        let terminal_progress = progress.clone();
        let terminal = self.transact_using(
            |journal| {
                let entry = find_mut(journal, operation_id)?;
                let recovery = bidirectional_recovery_kind(&entry.operation);
                let proof_matches = matches!(
                    (proof.0, recovery),
                    (
                        BidirectionalCompletionRecovery::Complete,
                        crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup
                    ) | (
                        BidirectionalCompletionRecovery::RolledBack,
                        crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup
                    )
                );
                if !proof_matches {
                    return Err(crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        format!(
                            "Persistierter Core-Abschlussbeleg änderte sich vor dem Queue-Commit für Operation {operation_id}"
                        ),
                    ));
                }
                finish_bidirectional_effect_entry(
                    entry,
                    proof.0 == BidirectionalCompletionRecovery::RolledBack,
                    terminal_progress,
                )?;
                // Der terminale Zustand und der noch offene, ausschließlich
                // mutationslose Core-Cleanup werden in derselben Queueversion
                // gebunden. Erst danach darf das Core-Journal verschwinden.
                entry.bidirectional_completion_recovery = Some(proof.0);
                retain_temp_recovery_paths(entry, paths);
                Ok(())
            },
            |path, snapshot| write(path, snapshot),
        );
        let terminal_error = match terminal {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        let marker_progress = progress;
        let marker = self.transact_using(
            |journal| {
                let entry = find_mut(journal, operation_id)?;
                let recovery = bidirectional_recovery_kind(&entry.operation);
                let proof_matches = matches!(
                    (proof.0, recovery),
                    (
                        BidirectionalCompletionRecovery::Complete,
                        crate::bidirectional::BidirectionalRunRecovery::CompletionCleanup
                    ) | (
                        BidirectionalCompletionRecovery::RolledBack,
                        crate::bidirectional::BidirectionalRunRecovery::RollbackCompletionCleanup
                    )
                );
                if !matches!(entry.operation, QueueOperation::Bidirectional { .. })
                    || !proof_matches
                    || !matches!(
                        entry.state,
                        QueueState::Validating
                            | QueueState::Running
                            | QueueState::Failed
                            | QueueState::Interrupted
                    )
                {
                    return Err(invalid_transition(entry.state));
                }
                entry.state = QueueState::Interrupted;
                entry.progress = marker_progress;
                entry.progress.phase = "completion_unclear".into();
                entry.error = Some(format!(
                    "Datenwirkung ist abgeschlossen; Queue-Abschluss muss für Operation {operation_id} erneut committed werden"
                ));
                entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                entry.attention = None;
                entry.recovery_available = false;
                entry.bidirectional_completion_recovery = Some(proof.0);
                retain_temp_recovery_paths(entry, paths);
                Ok(())
            },
            |path, snapshot| write(path, snapshot),
        );

        let detail = match marker {
            Ok(()) => format!(
                "Abschluss-Recovery ist im Queue-Journal für Operation {operation_id} persistiert"
            ),
            Err(marker_error) => {
                // Beide gezielt injizierten Commits waren sicher vor dem Rename
                // wirkungslos. Das Core-Journal ist jetzt die maßgebliche Wahrheit:
                // sein vollständiger Abschluss darf im selben Prozess nicht erst
                // nach einem Neustart sichtbar werden. Der normale Queue-Writer
                // bindet den daraus abgeleiteten Recovery-Zustand dauerhaft.
                let refresh =
                    self.refresh_bidirectional_completion_recovery_with_paths(operation_id, paths);
                match refresh {
                    Ok(()) => format!(
                        "Abschluss-Recovery wurde nach dem zweiten Schreibfehler aus dem Core-Journal neu gebunden; vorheriger Fehler: {marker_error}"
                    ),
                    Err(refresh_error) => format!(
                        "Abschluss-Recovery konnte nicht zusätzlich persistiert werden: {marker_error}; erneute Einordnung scheiterte: {refresh_error}; Queue-Journal: {}; Operation: {operation_id}",
                        self.path.display()
                    ),
                }
            }
        };
        Err(crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!(
                "Bidirektionale Datenwirkung war erfolgreich, aber ihr terminaler Queue-Commit scheiterte: {terminal_error}; {detail}"
            ),
        ))
    }

    /// Schließt nur den zuvor nach erfolgreicher Datenwirkung persistierten
    /// Recovery-Marker. Pfade, Provider und Nutzerdaten werden dabei nicht erneut
    /// geöffnet oder verändert.
    pub fn finish_bidirectional_completion_recovery(&self, operation_id: &str) -> io::Result<()> {
        let mut completion = {
            let journal = self.journal.lock().unwrap();
            let entry = journal
                .entries
                .iter()
                .find(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag nicht gefunden")
                })?;
            let recovery = entry.bidirectional_completion_recovery.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Bidirektionaler Abschluss-Recovery-Marker fehlt",
                )
            })?;
            let baseline_path = match &entry.operation {
                QueueOperation::Bidirectional { baseline_path, .. } => baseline_path.clone(),
                _ => return Err(invalid_transition(entry.state)),
            };
            (recovery, baseline_path, entry.state)
        };
        match completion.0 {
            BidirectionalCompletionRecovery::Complete => {
                if completion.2 == QueueState::Interrupted {
                    // Wie beim Rollback wird zuerst der terminale Queuezustand
                    // zusammen mit seinem offenen Cleanup-Marker persistiert.
                    // Erst dieser Commit erlaubt das anschließende Core-Unlink.
                    self.update(operation_id, |entry| {
                        require_state(entry, QueueState::Interrupted)?;
                        let progress = entry.progress.clone();
                        finish_bidirectional_effect_entry(entry, false, progress)?;
                        entry.bidirectional_completion_recovery =
                            Some(BidirectionalCompletionRecovery::Complete);
                        Ok(())
                    })?;
                    completion.2 = QueueState::Completed;
                } else if completion.2 != QueueState::Completed {
                    return Err(invalid_transition(completion.2));
                }
                crate::bidirectional::finish_bidirectional_run_completion(Path::new(
                    &completion.1,
                ))?;
            }
            BidirectionalCompletionRecovery::RolledBack => {
                if completion.2 == QueueState::Interrupted {
                    // Sicherheitsgrenze: Vor dem Core-Unlink muss der bereits
                    // wirksame Rollback terminal im Queue-Journal stehen. Bleibt
                    // der folgende Cleanup unklar, kann ein Neustart daher niemals
                    // denselben Datenplan erneut ausführen.
                    self.update(operation_id, |entry| {
                        require_state(entry, QueueState::Interrupted)?;
                        let progress = entry.progress.clone();
                        finish_bidirectional_effect_entry(entry, true, progress)?;
                        entry.bidirectional_completion_recovery =
                            Some(BidirectionalCompletionRecovery::RolledBack);
                        Ok(())
                    })?;
                    completion.2 = QueueState::Cancelled;
                } else if completion.2 != QueueState::Cancelled {
                    return Err(invalid_transition(completion.2));
                }
                crate::bidirectional::finish_bidirectional_rollback_completion(Path::new(
                    &completion.1,
                ))?;
            }
        }
        self.update(operation_id, |entry| {
            let recovery = entry.bidirectional_completion_recovery.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Bidirektionaler Abschluss-Recovery-Marker fehlt",
                )
            })?;
            match recovery {
                BidirectionalCompletionRecovery::Complete => {
                    require_state(entry, QueueState::Completed)?;
                    entry.bidirectional_completion_recovery = None;
                    entry.recovery_available = false;
                    entry.error = None;
                    entry.error_code = None;
                    Ok(())
                }
                BidirectionalCompletionRecovery::RolledBack => {
                    require_state(entry, QueueState::Cancelled)?;
                    entry.bidirectional_completion_recovery = None;
                    entry.recovery_available = false;
                    entry.error = None;
                    entry.error_code = None;
                    Ok(())
                }
            }
        })
    }

    pub fn require_confirmation(
        &self,
        operation_id: &str,
        attention: QueueAttention,
    ) -> io::Result<()> {
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::Validating)?;
            entry.state = QueueState::AwaitingConfirmation;
            entry.error = Some(attention_reason(&attention).to_string());
            entry.attention = Some(attention);
            Ok(())
        })
    }

    /// Die aktuell sichtbare Plan-/Konfliktlage explizit übernehmen und den Eintrag
    /// wieder ans Ende der sofort ausführbaren Pending-Folge stellen.
    pub fn approve_attention(&self, operation_id: &str, confirmation: &str) -> io::Result<()> {
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::AwaitingConfirmation)?;
            let attention = entry.attention.take().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Bestätigungsdaten fehlen")
            })?;
            if attention_confirmation(&attention) != confirmation {
                return Err(crate::error::coded_error(
                    io::ErrorKind::PermissionDenied,
                    crate::error::ErrorCode::ConfirmationMismatch,
                    "Queue-Bestätigungsphrase stimmt nicht überein",
                ));
            }
            match (&mut entry.operation, attention) {
                (
                    QueueOperation::ProviderFile {
                        source_tree_expected,
                        ..
                    },
                    QueueAttention::SourceTreeChanged { current_tree, .. },
                ) => {
                    *source_tree_expected = current_tree;
                }
                (
                    QueueOperation::Sync { expected_plan, .. },
                    QueueAttention::PlanChanged { current_plan, .. },
                ) => *expected_plan = current_plan,
                (
                    QueueOperation::SftpSync { expected_plan, .. },
                    QueueAttention::PlanChanged { current_plan, .. },
                ) => *expected_plan = current_plan,
                (
                    QueueOperation::FtpsSync { expected_plan, .. },
                    QueueAttention::PlanChanged { current_plan, .. },
                ) => *expected_plan = current_plan,
                (
                    QueueOperation::WebDavSync { expected_plan, .. },
                    QueueAttention::PlanChanged { current_plan, .. },
                ) => *expected_plan = current_plan,
                (
                    QueueOperation::SmbSync { expected_plan, .. },
                    QueueAttention::PlanChanged { current_plan, .. },
                ) => *expected_plan = current_plan,
                (
                    QueueOperation::ProviderSync { expected_plan, .. },
                    QueueAttention::PlanChanged { current_plan, .. },
                ) => *expected_plan = current_plan,
                (
                    QueueOperation::Bidirectional {
                        expected_baseline_digest,
                        expected_plan_digest,
                        ..
                    },
                    QueueAttention::BidirectionalPlanChanged {
                        current_baseline_digest,
                        current_plan_digest,
                        ..
                    },
                ) => {
                    *expected_baseline_digest = current_baseline_digest;
                    *expected_plan_digest = current_plan_digest;
                }
                (
                    QueueOperation::File {
                        expected_conflicts,
                        expected_conflict_proofs,
                        local_replace_best_effort_expected,
                        ..
                    },
                    QueueAttention::ConflictsChanged {
                        current_conflicts,
                        current_conflict_proofs,
                        ..
                    },
                ) => {
                    *expected_conflicts = current_conflicts;
                    *expected_conflict_proofs = current_conflict_proofs;
                    if expected_conflicts.is_empty() {
                        // Die frische Bestätigung enthält keine Ersetzung mehr.
                        // Eine alte `Some(true)`-Freigabe würde den aktualisierten
                        // Eintrag beim nächsten Claim als widersprüchlich ablehnen.
                        *local_replace_best_effort_expected = Some(false);
                    }
                }
                (
                    QueueOperation::File {
                        local_replace_best_effort_expected,
                        ..
                    },
                    QueueAttention::BestEffortCommitDowngrade {
                        copy_new_best_effort: false,
                        replace_best_effort: true,
                        remove_source_best_effort: false,
                        ..
                    },
                ) => *local_replace_best_effort_expected = Some(true),
                (
                    QueueOperation::ProviderFile {
                        expected_conflicts,
                        expected_conflict_proofs,
                        ..
                    },
                    QueueAttention::ConflictsChanged {
                        current_conflicts,
                        current_conflict_proofs,
                        ..
                    },
                ) => {
                    *expected_conflicts = current_conflicts;
                    *expected_conflict_proofs = current_conflict_proofs;
                }
                (
                    QueueOperation::File {
                        local_copy_new_best_effort_expected,
                        ..
                    },
                    QueueAttention::LocalFileCommit {
                        target_identity, ..
                    },
                ) => *local_copy_new_best_effort_expected = Some(Box::new(target_identity)),
                (
                    QueueOperation::ProviderFile {
                        local_copy_new_best_effort_expected,
                        copy_new_best_effort_expected,
                        ..
                    },
                    QueueAttention::LocalFileCommit {
                        target_identity, ..
                    },
                ) => {
                    *local_copy_new_best_effort_expected = Some(Box::new(target_identity));
                    *copy_new_best_effort_expected = Some(true);
                }
                (_, QueueAttention::Manual { .. }) => {}
                (
                    QueueOperation::ProviderFile {
                        copy_new_best_effort_expected,
                        replace_best_effort_expected,
                        remove_source_best_effort_expected,
                        ..
                    },
                    QueueAttention::BestEffortCommitDowngrade {
                        copy_new_best_effort,
                        replace_best_effort,
                        remove_source_best_effort,
                        ..
                    },
                ) => {
                    if copy_new_best_effort {
                        *copy_new_best_effort_expected = Some(true);
                    }
                    if replace_best_effort {
                        *replace_best_effort_expected = Some(true);
                    }
                    if remove_source_best_effort {
                        *remove_source_best_effort_expected = Some(true);
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Bestätigung passt nicht zur Queue-Operation",
                    ))
                }
            }
            entry.state = QueueState::Pending;
            entry.error = None;
            entry.error_code = None;
            Ok(())
        })
    }

    pub fn retry(&self, operation_id: &str) -> io::Result<()> {
        self.update(operation_id, |entry| {
            if !matches!(
                entry.state,
                QueueState::Failed | QueueState::Cancelled | QueueState::Interrupted
            ) {
                return Err(invalid_transition(entry.state));
            }
            if entry.error_code == Some(crate::ErrorCode::OutcomeUnclear) {
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    "Der Ausgang der letzten Mutation ist unklar; Zielzustand manuell prüfen und den Eintrag anschließend ausdrücklich schließen",
                ));
            }
            if entry.error_code == Some(crate::ErrorCode::ReplanRequired) {
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::ReplanRequired,
                    "Dieser Altplan muss verworfen und aus einer neuen Vorschau neu angelegt werden",
                ));
            }
            if entry.bidirectional_completion_recovery.is_some() {
                // Nach erfolgreicher Datenwirkung darf „Neu prüfen“ niemals den
                // eigentlichen Lauf wiederholen. Nur der mutationslose
                // Queue-Abschluss darf diesen Marker auflösen.
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    "Die bidirektionale Datenwirkung ist abgeschlossen; zuerst den Queue-Abschluss prüfen",
                ));
            }
            if has_legacy_unbound_webdav_delete(entry) {
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    if classify_legacy_unbound_webdav_delete(entry)
                        == Some(LegacyUnboundWebDavDelete::PossiblePartialEffect)
                    {
                        crate::ErrorCode::OutcomeUnclear
                    } else {
                        crate::ErrorCode::PlanDrift
                    },
                    entry
                        .error
                        .as_deref()
                        .unwrap_or(LEGACY_WEBDAV_REPLAN_ERROR),
                ));
            }
            if matches!(entry.operation, QueueOperation::Bidirectional { .. })
                && entry.state == QueueState::Interrupted
                && (entry.recovery_available
                    || bidirectional_recovery_kind(&entry.operation)
                        != crate::bidirectional::BidirectionalRunRecovery::Absent)
            {
                // Ein normaler Retry würde den einzigen Hinweis auf bereits
                // ausgeführte Teilprojektionen löschen. An dieser Grenze sind nur
                // die ausdrücklich geprüften Wege „Fortsetzen“ und „Rollback“ sicher.
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    "Der bidirektionale Lauf besitzt einen offenen Recoveryzustand; zuerst fortsetzen oder zurückrollen",
                ));
            }
            entry.state = QueueState::Pending;
            entry.progress = Progress::default();
            entry.error = None;
            entry.error_code = None;
            entry.attention = None;
            entry.recovery_available = false;
            entry.bidirectional_completion_recovery = None;
            Ok(())
        })
    }

    /// Entfernt ausschließlich einen nachweislich vor der ersten Mutation
    /// stillgelegten Altplan. Der Eintrag kann nicht wiederverwendet werden; eine
    /// neue Vorschau erzeugt später eine neue Operation mit neuem Plandigest.
    pub fn discard_replan_required(&self, operation_id: &str) -> io::Result<()> {
        self.transact(|journal| {
            let index = journal
                .entries
                .iter()
                .position(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag nicht gefunden")
                })?;
            let entry = &journal.entries[index];
            if entry.state != QueueState::Failed
                || entry.error_code != Some(crate::ErrorCode::ReplanRequired)
                || entry.progress != Progress::default()
                || entry.attention.is_some()
                || !entry.temp_recovery_paths.is_empty()
                || entry.recovery_available
                || entry.bidirectional_completion_recovery.is_some()
            {
                return Err(invalid_transition(entry.state));
            }
            journal.entries.remove(index);
            Ok(())
        })
    }

    /// Schließt einen nicht wiederholbaren Fehler erst nach bestätigter manueller
    /// Prüfung. Der Code bleibt am terminalen Eintrag sichtbar; nur seine
    /// Ausführbarkeit endet.
    pub fn acknowledge_unclear_outcome(
        &self,
        operation_id: &str,
        possible_effect_reviewed: bool,
    ) -> io::Result<()> {
        self.update(operation_id, |entry| {
            if !matches!(entry.state, QueueState::Failed | QueueState::Interrupted)
                || entry.error_code != Some(crate::ErrorCode::OutcomeUnclear)
                || matches!(entry.operation, QueueOperation::Bidirectional { .. })
                || entry.bidirectional_completion_recovery.is_some()
                || has_legacy_unbound_webdav_delete(entry)
            {
                return Err(invalid_transition(entry.state));
            }
            if !possible_effect_reviewed {
                return Err(crate::coded_error(
                    io::ErrorKind::PermissionDenied,
                    crate::ErrorCode::ConfirmationMismatch,
                    "Die mögliche Teilwirkung wurde nicht ausdrücklich als manuell geprüft bestätigt",
                ));
            }
            entry.state = QueueState::Cancelled;
            entry.progress.phase = "recovery_acknowledged".into();
            entry.attention = None;
            entry.recovery_available = false;
            let previous = entry.error.take().unwrap_or_else(|| {
                "Der Ausgang der letzten Mutation ist unklar".into()
            });
            entry.error = Some(format!(
                "{previous}; Zielzustand wurde manuell geprüft und der Eintrag ohne Wiederholung geschlossen."
            ));
            Ok(())
        })
    }

    /// Schließt ausschließlich einen nicht wiederholbaren WebDAV-Löschaltstand aus
    /// Journalversion 15 bis 17. Ein nachweislich nie gestarteter Plan wird
    /// verworfen; bei möglicher Teilwirkung bleibt die bisherige Bilanz terminal
    /// sichtbar und verlangt eine ausdrückliche manuelle Prüfung.
    pub fn acknowledge_legacy_webdav_delete(
        &self,
        operation_id: &str,
        possible_effect_reviewed: bool,
    ) -> io::Result<()> {
        self.transact(|journal| {
            let index = journal
                .entries
                .iter()
                .position(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag nicht gefunden")
                })?;
            let entry = &journal.entries[index];
            if !has_legacy_unbound_webdav_delete(entry) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Nur der typisierte WebDAV-Löschaltstand kann hier abgeschlossen werden",
                ));
            }
            let classification = classify_legacy_unbound_webdav_delete(entry);
            if classification == Some(LegacyUnboundWebDavDelete::SafeReplan) {
                journal.entries.remove(index);
                return Ok(());
            }
            if classification != Some(LegacyUnboundWebDavDelete::PossiblePartialEffect) {
                return Err(invalid_transition(entry.state));
            }
            if !possible_effect_reviewed {
                return Err(crate::coded_error(
                    io::ErrorKind::PermissionDenied,
                    crate::ErrorCode::ConfirmationMismatch,
                    "Die mögliche WebDAV-Teilwirkung wurde nicht ausdrücklich als manuell geprüft bestätigt",
                ));
            }
            let entry = &mut journal.entries[index];
            entry.state = QueueState::Cancelled;
            entry.progress.phase = "recovery_acknowledged".into();
            entry.attention = None;
            entry.error = Some(format!(
                "{LEGACY_WEBDAV_UNCLEAR_ERROR} Manuelle Prüfung wurde bestätigt; der Altplan ist terminal geschlossen."
            ));
            entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
            entry.recovery_available = false;
            Ok(())
        })
    }

    fn mark_remote_running(&self, operation_id: &str, confirmation: &str) -> io::Result<()> {
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::AwaitingConfirmation)?;
            let QueueAttention::RemoteRun {
                confirmation: expected,
                ..
            } = entry.attention.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Remote-Bestätigung fehlt")
            })?
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Queue-Eintrag wartet nicht auf ein Remote-Permit",
                ));
            };
            if expected != confirmation {
                return Err(crate::error::coded_error(
                    io::ErrorKind::PermissionDenied,
                    crate::error::ErrorCode::ConfirmationMismatch,
                    "SFTP-Laufphrase stimmt nicht überein",
                ));
            }
            entry.state = QueueState::Running;
            entry.error = None;
            entry.error_code = None;
            entry.attention = None;
            Ok(())
        })
    }

    /// Eine offene Bestätigungsanfrage ausdrücklich ablehnen. Der Eintrag wird
    /// terminal `Cancelled` und gibt die Reihenfolge für alle nachfolgenden
    /// Operationen wieder frei.
    ///
    /// Ohne diesen Weg führte aus `AwaitingConfirmation` einzig die Zustimmung
    /// heraus: `remove_pending`, `move_pending`, `retry`, `cancel` und `fail`
    /// verlangen alle einen anderen Ausgangszustand, und ein Neustart lässt den
    /// Eintrag unverändert stehen. Wer die neue Plan- oder Konfliktlage also
    /// nicht übernehmen wollte, musste genau die Phrase eintippen, die er
    /// ablehnte — sonst blieb die gesamte Fensterqueue dauerhaft stehen.
    ///
    /// Sicher ist das Verwerfen, weil `AwaitingConfirmation` ausschließlich aus
    /// `Validating` entsteht, also vor `mark_running` und damit vor der ersten
    /// Mutation. Es gibt an dieser Grenze keine offene Datenwirkung einzuordnen.
    /// Die beiden Ausnahmen, die eigenen Zustand außerhalb des Journals halten,
    /// weist die Methode ausdrücklich ab.
    pub fn discard_attention(&self, operation_id: &str) -> io::Result<()> {
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::AwaitingConfirmation)?;
            if matches!(entry.attention, Some(QueueAttention::RemoteRun { .. }))
                && !matches!(entry.operation, QueueOperation::LocalSync { .. }) {
                // Die Vorbereitung dieses Laufs lebt als nicht serialisierbarer
                // Backendzustand samt Permit und offenen Verbindungen. Sie muss
                // zuerst dort abgebaut werden; `discard_remote_preparation` stellt
                // den Eintrag danach auf `Pending`, wo er normal entfernbar ist.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Eine wartende Remote-Vorbereitung wird zuerst über discard_remote_preparation abgebaut",
                ));
            }
            if matches!(entry.operation, QueueOperation::Bidirectional { .. })
                && bidirectional_recovery_kind(&entry.operation)
                    != crate::bidirectional::BidirectionalRunRecovery::Absent
            {
                // Dieselbe Grenze wie bei `retry`: Ein offenes Laufjournal kann
                // bereits echte Teilprojektionen binden. Ein stilles `Cancelled`
                // würde den einzigen Hinweis darauf löschen.
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    "Der bidirektionale Lauf besitzt einen offenen Recoveryzustand; zuerst fortsetzen oder zurückrollen",
                ));
            }
            entry.state = QueueState::Cancelled;
            entry.attention = None;
            entry.error = Some(DISCARDED_ATTENTION_ERROR.into());
            entry.error_code = None;
            Ok(())
        })
    }

    pub fn discard_remote_preparation(&self, operation_id: &str) -> io::Result<()> {
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::AwaitingConfirmation)?;
            if !matches!(entry.attention, Some(QueueAttention::RemoteRun { .. })) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Queue-Eintrag besitzt keine Remote-Vorbereitung",
                ));
            }
            entry.state = QueueState::Pending;
            entry.error = None;
            entry.error_code = None;
            entry.attention = None;
            Ok(())
        })
    }

    /// Ausschließlich die frisch im Backend gehaltene lokale Vorschau übernehmen.
    /// Das Journal erhält dadurch keinen Permit; vor Wirkung bleibt die identische
    /// Vorbereitung mit ihren offenen Deskriptoren zwingend erforderlich.
    pub fn approve_local_sync_preparation(
        &self,
        operation_id: &str,
        prepared: &crate::PreparedLocalSyncRun,
    ) -> io::Result<()> {
        self.update(operation_id, |entry| {
            require_state(entry, QueueState::AwaitingConfirmation)?;
            if !matches!(entry.attention, Some(QueueAttention::RemoteRun { .. })) {
                return Err(invalid_transition(entry.state));
            }
            require_local_sync_binding(&entry.operation, prepared, false)?;
            let QueueOperation::LocalSync {
                expected_plan,
                execution_hint,
                ..
            } = &mut entry.operation
            else {
                unreachable!()
            };
            **expected_plan = prepared.preview().plan.clone();
            *execution_hint = prepared.preview().local_execution.clone();
            entry.state = QueueState::Pending;
            entry.attention = None;
            entry.error = None;
            entry.error_code = None;
            Ok(())
        })
    }

    pub fn remove_pending(&self, operation_id: &str) -> io::Result<()> {
        self.transact(|journal| {
            let index = journal
                .entries
                .iter()
                .position(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag fehlt"))?;
            if journal.entries[index].state != QueueState::Pending {
                return Err(invalid_transition(journal.entries[index].state));
            }
            journal.entries.remove(index);
            Ok(())
        })
    }

    /// Nur mit dem nächsten/vorherigen ebenfalls wartenden Eintrag tauschen. So kann
    /// kein laufender, unterbrochener oder bestätigungspflichtiger Eintrag übersprungen werden.
    pub fn move_pending(&self, operation_id: &str, delta: i32) -> io::Result<()> {
        if !matches!(delta, -1 | 1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Queue-Verschiebung muss -1 oder 1 sein",
            ));
        }
        self.transact(|journal| {
            let index = journal
                .entries
                .iter()
                .position(|entry| entry.operation_id == operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag fehlt"))?;
            if journal.entries[index].state != QueueState::Pending {
                return Err(invalid_transition(journal.entries[index].state));
            }
            let other = if delta < 0 {
                (0..index)
                    .rev()
                    .take_while(|candidate| {
                        let state = journal.entries[*candidate].state;
                        state == QueueState::Pending || state.is_terminal()
                    })
                    .find(|candidate| journal.entries[*candidate].state == QueueState::Pending)
            } else {
                ((index + 1)..journal.entries.len())
                    .take_while(|candidate| {
                        let state = journal.entries[*candidate].state;
                        state == QueueState::Pending || state.is_terminal()
                    })
                    .find(|candidate| journal.entries[*candidate].state == QueueState::Pending)
            };
            if let Some(other) = other {
                journal.entries.swap(index, other);
            }
            Ok(())
        })
    }

    pub fn set_paused(&self, paused: bool) -> io::Result<()> {
        self.transact(|journal| {
            journal.paused = paused;
            Ok(())
        })
    }

    pub fn set_stop_on_error(&self, stop: bool) -> io::Result<()> {
        self.transact(|journal| {
            journal.stop_on_error = stop;
            Ok(())
        })
    }

    pub fn has_runnable(&self) -> bool {
        let journal = self.journal.lock().unwrap();
        !journal.paused
            && !journal
                .entries
                .iter()
                .any(|entry| matches!(entry.state, QueueState::Validating | QueueState::Running))
            && journal
                .entries
                .iter()
                .find(|entry| !entry.state.is_terminal())
                .is_some_and(|entry| entry.state == QueueState::Pending)
    }

    fn finish(
        &self,
        operation_id: &str,
        state: QueueState,
        progress: Progress,
        error: Option<String>,
    ) -> io::Result<()> {
        self.finish_with_recovery(operation_id, state, progress, error, &[])
    }

    fn finish_with_recovery(
        &self,
        operation_id: &str,
        state: QueueState,
        progress: Progress,
        error: Option<String>,
        recovery_paths: &[String],
    ) -> io::Result<()> {
        self.update(operation_id, |entry| {
            if matches!(entry.operation, QueueOperation::Bidirectional { .. }) {
                // `Completed` und `Cancelled` benötigen den separat gebundenen
                // Core-Beleg. Der allgemeine Datei-/Sync-Abschluss darf weder den
                // Marker löschen noch die nächste Queue-Operation freigeben.
                return Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Bidirektionale Operation {operation_id} darf nicht generisch terminalisiert werden"
                    ),
                ));
            }
            require_state(entry, QueueState::Running)?;
            entry.state = state;
            entry.progress = progress;
            entry.error = error;
            entry.error_code = None;
            entry.attention = None;
            entry.recovery_available = false;
            entry.bidirectional_completion_recovery = None;
            for path in recovery_paths {
                if !entry.temp_recovery_paths.contains(path) {
                    entry.temp_recovery_paths.push(path.clone());
                }
            }
            Ok(())
        })
    }

    fn update(
        &self,
        operation_id: &str,
        change: impl FnOnce(&mut QueueEntry) -> io::Result<()>,
    ) -> io::Result<()> {
        self.transact(|journal| change(find_mut(journal, operation_id)?))
    }

    /// Erst nach erfolgreichem atomarem Schreiben wird die In-Memory-Sicht ersetzt.
    /// Ein voller Datenträger darf daher keinen nur im RAM sichtbaren Zustand erzeugen.
    fn transact<T>(
        &self,
        change: impl FnOnce(&mut QueueSnapshot) -> io::Result<T>,
    ) -> io::Result<T> {
        self.transact_using(change, |path, snapshot| {
            write_snapshot_bound(
                &self._process_lock.root,
                path,
                snapshot,
                AtomicWriteMode::Replace,
            )
        })
    }

    fn transact_using<T>(
        &self,
        change: impl FnOnce(&mut QueueSnapshot) -> io::Result<T>,
        write: impl FnOnce(&Path, &QueueSnapshot) -> io::Result<()>,
    ) -> io::Result<T> {
        self.require_current_config()?;
        if self.journal_outcome_unclear.load(Ordering::Acquire) {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Der Queue-Journalstand ist nach einem Schreibfehler unklar; Anwendung neu starten und Recovery prüfen",
            ));
        }
        let mut current = self.journal.lock().unwrap();
        if self.journal_outcome_unclear.load(Ordering::Acquire) {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Der Queue-Journalstand ist nach einem Schreibfehler unklar; Anwendung neu starten und Recovery prüfen",
            ));
        }
        let mut next = current.clone();
        next.revision = current.revision.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Queue-Journalrevision läuft über",
            )
        })?;
        let result = change(&mut next)?;
        let write_error = match write(&self.path, &next) {
            Ok(()) => {
                self.require_current_config()?;
                *current = next;
                return Ok(result);
            }
            Err(error) => error,
        };

        // Ein Fehler des atomaren Writers kann vor dem Rename oder erst beim
        // abschließenden Parent-fsync entstehen. Unter derselben Journal-Sperre
        // lesen wir deshalb exakt den sichtbaren Gesamtstand zurück. `next`
        // benötigt zusätzlich einen erfolgreichen Datei-/Parent-fsync; sonst
        // könnte der Neustart nach Stromausfall den alten Pending-Stand ausführen.
        // Nur `current` beweist eine sicher retrybare
        // Nichtwirkung. Jeder dritte oder unlesbare Stand sperrt weitere Mutationen.
        match read_snapshot_bound(&self._process_lock.root, &self.path) {
            Ok(persisted) if persisted == next => {
                let durable = self.sync_reconciled_snapshot(&next);
                if let Err(sync_error) = durable {
                    self.journal_outcome_unclear.store(true, Ordering::Release);
                    return Err(crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        format!("Queue-Journal ist sichtbar, aber nach dem Schreibfehler nicht dauerhaft bestätigt: {write_error}; {sync_error}"),
                    ));
                }
                *current = next;
                Ok(result)
            }
            Ok(persisted) if persisted == *current => Err(write_error),
            Ok(_) => {
                self.journal_outcome_unclear.store(true, Ordering::Release);
                Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Queue-Schreibfehler konnte nicht eindeutig geklärt werden: {write_error}"
                    ),
                ))
            }
            Err(read_error) => {
                self.journal_outcome_unclear.store(true, Ordering::Release);
                Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Queue-Schreibfehler und anschließender Journal-Readback schlugen fehl: {write_error}; {read_error}"
                    ),
                ))
            }
        }
    }

    fn require_current_config(&self) -> io::Result<()> {
        match self._process_lock.root.is_current() {
            Ok(true) => Ok(()),
            result => {
                self.journal_outcome_unclear.store(true, Ordering::Release);
                let detail = match result {
                    Err(error) => format!(": {error}"),
                    _ => String::new(),
                };
                Err(crate::coded_error(io::ErrorKind::Other, crate::ErrorCode::OutcomeUnclear,
                    format!("Queueordner ist nicht mehr sicher gebunden; keine neue Journalwirkung{detail}")))
            }
        }
    }

    fn sync_reconciled_snapshot(&self, expected: &QueueSnapshot) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_reconcile_sync.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("erzwungener Journal-Durability-Fehler"));
        }
        let parent = &self._process_lock.root;
        if !parent.is_current()? {
            return Err(io::Error::other("Queueordner wurde ausgetauscht"));
        }
        let name = self.path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Queue-Journal braucht einen Dateinamen",
            )
        })?;
        let path = parent.path().join(name);
        let file = crate::local_fs::open_read_beneath(parent, &path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || read_snapshot_from_reader(&file, metadata.len(), MAX_JOURNAL_BYTES)? != *expected
        {
            return Err(io::Error::other(
                "Journal änderte sich vor der Dauerhaftigkeitsprüfung",
            ));
        }
        file.sync_all()?;
        crate::local_fs::sync_dir_beneath(parent, parent.path())?;
        if !parent.is_current()? || read_snapshot_bound(parent, &self.path)? != *expected {
            return Err(io::Error::other(
                "Journal änderte sich während der Dauerhaftigkeitsprüfung",
            ));
        }
        Ok(())
    }
}

fn require_local_sync_binding(
    operation: &QueueOperation,
    prepared: &crate::PreparedLocalSyncRun,
    check_plan: bool,
) -> io::Result<()> {
    let QueueOperation::LocalSync {
        source,
        target,
        options,
        expected_plan,
        ..
    } = operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kein normaler lokaler Sync-Auftrag",
        ));
    };
    let preview = prepared.preview();
    let (crate::RootIdentity::Local(source_identity), crate::RootIdentity::Local(target_identity)) =
        (&preview.source_identity, &preview.target_identity)
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Lokale Rootbindung fehlt",
        ));
    };
    if Path::new(source) != Path::new(&source_identity.display_path)
        || Path::new(target) != Path::new(&target_identity.display_path)
        || options != &preview.options
        || (check_plan && expected_plan.as_ref() != &preview.plan)
    {
        return Err(crate::real_mode::plan_drift(
            "Lokaler Queue-Auftrag und gehaltene Vorschau stimmen nicht überein",
        ));
    }
    Ok(())
}

pub fn prepare_local_sync_queue_entry(
    queue: &OperationQueue,
    entry: &QueueEntry,
    current_sandbox: Option<&Path>,
) -> io::Result<crate::PreparedLocalSyncRun> {
    validate_operation(&entry.operation)?;
    let QueueOperation::LocalSync {
        source,
        target,
        options,
        sandbox,
        ..
    } = &entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kein lokaler Sync-Auftrag",
        ));
    };
    if sandbox.as_deref().map(Path::new) != current_sandbox {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Der markierte Testbereich änderte sich; Auftrag neu vorbereiten",
        ));
    }
    let (source, target) = validate_local_sync_roots(Path::new(source), Path::new(target))?;
    if let Some(root) = current_sandbox {
        let vfs = LocalVfs::rooted(root)?;
        Safety::test_sandbox(root.to_owned(), &vfs)?;
        require_sandbox_child(root, &source, "Quelle")?;
        require_sandbox_child(root, &target, "Ziel")?;
    }
    let prepared = crate::prepare_local_sync_run_with_sandbox(
        &source,
        &target,
        options.clone(),
        current_sandbox,
    )?;
    queue.require_confirmation(&entry.operation_id, QueueAttention::RemoteRun {
        reason: "Lokaler Sync wurde frisch geprüft; Queue öffnen und die sichtbare Wirkung bestätigen".into(),
        confirmation: prepared.preview().confirmation_phrase.clone(),
    })?;
    Ok(prepared)
}

pub fn execute_prepared_local_sync_queue_entry(
    queue: &OperationQueue,
    entry: &QueueEntry,
    prepared: crate::PreparedLocalSyncRun,
    replacements_confirmed: bool,
    audit_path: &Path,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    require_local_sync_binding(&entry.operation, &prepared, true)?;
    let run = prepared
        .confirm(replacements_confirmed)?
        .with_queue_staging_owner(&queue.path, &entry.operation_id);
    queue.mark_running(&entry.operation_id)?;
    let mut queue_progress = QueueProgress::new(queue, &entry.operation_id, cancel, on_progress);
    let result = run.execute_with_audit_detailed(audit_path, cancel, &mut |progress| {
        queue_progress.report(progress);
    });
    let (mut latest, persistence_error) = queue_progress.finish();
    if let Some(error) = persistence_error {
        let failure = possible_mutation_progress_error(error, &result);
        let failure = match &result {
            Ok(success) => {
                crate::error::with_recovery_paths(failure, &success.stats().temp_recovery_paths)
            }
            Err(_) => failure,
        };
        if let Err(journal_error) =
            queue.fail_coded(&entry.operation_id, failed_progress(latest), &failure)
        {
            return Err(failed_queue_error_commit(
                &failure,
                &journal_error,
                "terminale lokale Sync-Fehlercommit",
            ));
        }
        return Err(failure);
    }
    match result {
        Ok(success) => {
            latest.phase = if success.stats().cancelled {
                "cancelled"
            } else {
                "done"
            }
            .into();
            queue
                .finish_with_recovery(
                    &entry.operation_id,
                    if success.stats().cancelled {
                        QueueState::Cancelled
                    } else {
                        QueueState::Completed
                    },
                    latest,
                    None,
                    &success.stats().temp_recovery_paths,
                )
                .map_err(|error| {
                    crate::error::with_recovery_paths(error, &success.stats().temp_recovery_paths)
                })?;
            let recovery_paths = success.stats().temp_recovery_paths.clone();
            Ok(QueueExecutionResult::Sync(
                success
                    .append_completion_audit()
                    .map_err(|error| crate::error::with_recovery_paths(error, &recovery_paths))?,
            ))
        }
        Err(error) => {
            if let Err(journal_error) =
                queue.fail_coded(&entry.operation_id, failed_progress(latest), &error)
            {
                return Err(failed_queue_error_commit(
                    &error,
                    &journal_error,
                    "terminale lokale Sync-Fehlercommit",
                ));
            }
            Err(error)
        }
    }
}

/// Nur Verbindungsfehler dürfen die Zugangsdaten-Rückfrage erzeugen. Fehler
/// der anschließenden Root-/Planvorbereitung bleiben unverändert erhalten.
fn prepare_protocol_queue_run<V>(
    queue: &OperationQueue,
    operation_id: String,
    expected_plan: &SyncPlan,
    provider_label: &str,
    connect: impl FnOnce() -> io::Result<V>,
    prepare: impl FnOnce(Arc<V>) -> io::Result<PreparedSftpRealRun>,
) -> io::Result<SftpQueuePreparation> {
    let vfs = match connect() {
        Ok(vfs) => Arc::new(vfs),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            let reason =
                format!("{provider_label}-Zugangsdaten sind nicht verfügbar oder wurden abgelehnt");
            queue.require_confirmation(
                &operation_id,
                QueueAttention::Manual {
                    reason: reason.clone(),
                    confirmation: format!("SECRET {}", short_id(&operation_id)),
                },
            )?;
            return Ok(SftpQueuePreparation::AwaitingConfirmation { reason });
        }
        Err(error) => return Err(error),
    };
    let prepared = prepare(vfs)?;
    finish_remote_queue_preparation(queue, operation_id, expected_plan, provider_label, prepared)
}

fn finish_remote_queue_preparation(
    queue: &OperationQueue,
    operation_id: String,
    expected_plan: &SyncPlan,
    provider_label: &str,
    prepared: PreparedSftpRealRun,
) -> io::Result<SftpQueuePreparation> {
    if prepared.preview().plan != *expected_plan {
        let current_plan = prepared.preview().plan.clone();
        let reason = format!("{provider_label}-Quelle oder -Ziel änderte sich seit dem Enqueue");
        queue.require_confirmation(
            &operation_id,
            QueueAttention::PlanChanged {
                reason: reason.clone(),
                confirmation: format!(
                    "PLAN {} {} {} {}",
                    short_id(&operation_id),
                    current_plan.copy_count,
                    current_plan.update_count,
                    current_plan.delete_count
                ),
                current_plan: Box::new(current_plan),
            },
        )?;
        return Ok(SftpQueuePreparation::AwaitingConfirmation { reason });
    }
    queue.require_confirmation(
        &operation_id,
        QueueAttention::RemoteRun {
            reason: format!(
                "{provider_label}-Lauf ist frisch geprüft und wartet auf die einmalige Laufphrase"
            ),
            confirmation: prepared.preview().confirmation_phrase.clone(),
        },
    )?;
    Ok(SftpQueuePreparation::Ready(Box::new(
        PreparedSftpQueueRun {
            operation_id,
            prepared,
        },
    )))
}

/// Einen geclaimten SFTP-Eintrag frisch verbinden, vollständig vorbereiten und bis
/// zur zufälligen Realmodusphrase im Backend-RAM halten.
pub fn prepare_sftp_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
) -> io::Result<SftpQueuePreparation> {
    // Ein Journal kann aus einer älteren oder fremden Prozessinstanz stammen.
    // Seine rein syntaktischen Optionen müssen erneut scheitern, bevor Secrets
    // gelesen oder eine Verbindung geöffnet wird.
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    let QueueOperation::SftpSync {
        direction,
        connection,
        local_root,
        remote_root,
        options,
        expected_plan,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Remote-Executor erhielt keinen SFTP-Eintrag",
        ));
    };
    prepare_protocol_queue_run(
        queue,
        operation_id,
        &expected_plan,
        "SFTP",
        || SftpVfs::connect(*connection, secrets),
        |vfs| match direction {
            SftpQueueDirection::Upload => prepare_sftp_upload(
                Path::new(&local_root),
                vfs,
                Path::new(&remote_root),
                options,
            ),
            SftpQueueDirection::Download => prepare_sftp_download(
                vfs,
                Path::new(&remote_root),
                Path::new(&local_root),
                options,
            ),
        },
    )
}

/// Generischer Provider-Queue-Gegenlauf. Das Journal enthält beide öffentlichen
/// Endpunktbindungen; Secrets, Sitzungen, Handles und Permit bleiben ausschließlich
/// im frisch erzeugten RAM-Objekt.
pub fn prepare_provider_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
) -> io::Result<SftpQueuePreparation> {
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    let QueueOperation::ProviderSync {
        source,
        target,
        options,
        expected_plan,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider-Executor erhielt keinen ProviderSync-Eintrag",
        ));
    };
    let shares_connection_definition = source.shares_connection_definition(&target);
    let credential_reason =
        "Provider-Zugangsdaten sind nicht verfügbar oder wurden abgelehnt".to_string();
    let Some(source_endpoint) = connect_queue_endpoint_with_credential_attention(
        queue,
        &operation_id,
        &source,
        secrets.clone(),
    )?
    else {
        return Ok(SftpQueuePreparation::AwaitingConfirmation {
            reason: credential_reason,
        });
    };
    let Some(independently_connected_target) =
        connect_queue_endpoint_with_credential_attention(queue, &operation_id, &target, secrets)?
    else {
        return Ok(SftpQueuePreparation::AwaitingConfirmation {
            reason: credential_reason,
        });
    };
    // Erst die beiden `connect`-Aufrufe dürfen eine Credential-Attention erzeugen.
    // Namespace- und Rootfehler stammen bereits aus lebenden Sitzungen und müssen
    // ihren ursprünglichen ErrorKind behalten, statt in eine SECRET-Schleife zu gehen.
    let (source, target, _) = bind_connected_queue_endpoints(
        source_endpoint,
        independently_connected_target,
        shares_connection_definition,
        Path::new(target.root()),
    )?;
    let source = source.with_queue_owner(queue, Some(&operation_id), &[])?;
    let target = target.with_queue_owner(queue, Some(&operation_id), &[])?;
    let prepared = prepare_provider_transfer(source, target, options)?;
    finish_remote_queue_preparation(queue, operation_id, &expected_plan, "Provider", prepared)
}

/// FTPS-Queue-Gegenstück: Das Journal hält nur Pin, Root und Schlüsselbundreferenz;
/// TLS-Sitzung und Einmal-Permit entstehen bei jedem Claim neu im RAM.
pub fn prepare_ftps_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
) -> io::Result<SftpQueuePreparation> {
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    let QueueOperation::FtpsSync {
        direction,
        connection,
        local_root,
        remote_root,
        options,
        expected_plan,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Remote-Executor erhielt keinen FTPS-Eintrag",
        ));
    };
    prepare_protocol_queue_run(
        queue,
        operation_id.clone(),
        &expected_plan,
        "FTPS",
        || {
            FtpsVfs::connect(*connection, secrets)?.for_queue_operation(
                queue,
                Some(&operation_id),
                Path::new(&remote_root),
                &[],
            )
        },
        |vfs| match direction {
            SftpQueueDirection::Upload => prepare_ftps_upload(
                Path::new(&local_root),
                vfs,
                Path::new(&remote_root),
                options,
            ),
            SftpQueueDirection::Download => prepare_ftps_download(
                vfs,
                Path::new(&remote_root),
                Path::new(&local_root),
                options,
            ),
        },
    )
}

/// WebDAV-Queue-Gegenstück. Das Journal enthält nur HTTPS-Origin, Zertifikatspin,
/// Collection-Root und Schlüsselbundreferenz; Passwort und TLS-Sitzung bleiben im RAM.
pub fn prepare_webdav_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
) -> io::Result<SftpQueuePreparation> {
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    let QueueOperation::WebDavSync {
        direction,
        connection,
        local_root,
        remote_root,
        options,
        expected_plan,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Remote-Executor erhielt keinen WebDAV-Eintrag",
        ));
    };
    prepare_protocol_queue_run(
        queue,
        operation_id,
        &expected_plan,
        "WebDAV",
        || WebDavVfs::connect(*connection, secrets),
        |vfs| match direction {
            SftpQueueDirection::Upload => prepare_webdav_upload(
                Path::new(&local_root),
                vfs,
                Path::new(&remote_root),
                options,
            ),
            SftpQueueDirection::Download => prepare_webdav_download(
                vfs,
                Path::new(&remote_root),
                Path::new(&local_root),
                options,
            ),
        },
    )
}

/// SMB-Queue-Gegenstück. Journalisiert werden nur Server-/Freigabeparameter und
/// Schlüsselbundreferenz; Passwort, Sitzung, Tree-ID und Permit bleiben im RAM.
pub fn prepare_smb_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
) -> io::Result<SftpQueuePreparation> {
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    let QueueOperation::SmbSync {
        direction,
        connection,
        local_root,
        remote_root,
        options,
        expected_plan,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Remote-Executor erhielt keinen SMB-Eintrag",
        ));
    };
    prepare_protocol_queue_run(
        queue,
        operation_id,
        &expected_plan,
        "SMB",
        || SmbVfs::connect(*connection, secrets),
        |vfs| match direction {
            SftpQueueDirection::Upload => prepare_smb_upload(
                Path::new(&local_root),
                vfs,
                Path::new(&remote_root),
                options,
            ),
            SftpQueueDirection::Download => prepare_smb_download(
                vfs,
                Path::new(&remote_root),
                Path::new(&local_root),
                options,
            ),
        },
    )
}

/// Eine im RAM gehaltene Remote-Vorbereitung ausführen. `Running` wird dauerhaft
/// geschrieben, nachdem Phrase/Backup und frischer Driftcheck erfolgreich waren,
/// aber noch vor der ersten Nutzerdatenmutation.
pub fn execute_prepared_sftp_queue_entry(
    queue: &OperationQueue,
    prepared: PreparedSftpQueueRun,
    confirmation: &str,
    backup_confirmed: bool,
    audit_path: &Path,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    let operation_id = prepared.operation_id;
    let run = prepared.prepared.confirm(confirmation, backup_confirmed)?;
    queue.mark_remote_running(&operation_id, confirmation)?;
    let mut queue_progress = QueueProgress::new(queue, &operation_id, cancel, on_progress);
    let result = run.execute_with_audit_detailed(audit_path, cancel, &mut |progress| {
        queue_progress.report(progress);
    });
    let (mut latest, persistence_error) = queue_progress.finish();
    if let Some(persistence_error) = persistence_error {
        let failure = possible_mutation_progress_error(persistence_error, &result);
        let failure = match &result {
            Ok(success) => {
                crate::error::with_recovery_paths(failure, &success.stats().temp_recovery_paths)
            }
            Err(_) => failure,
        };
        if let Err(journal_error) =
            queue.fail_coded(&operation_id, failed_progress(latest), &failure)
        {
            return Err(failed_queue_error_commit(
                &failure,
                &journal_error,
                "terminale Queue-Fehlercommit",
            ));
        }
        return Err(failure);
    }
    match result {
        Ok(success) => {
            let cancelled = success.stats().cancelled;
            latest.phase = if cancelled { "cancelled" } else { "done" }.into();
            queue
                .finish_with_recovery(
                    &operation_id,
                    if cancelled {
                        QueueState::Cancelled
                    } else {
                        QueueState::Completed
                    },
                    latest,
                    None,
                    &success.stats().temp_recovery_paths,
                )
                .map_err(|error| {
                    crate::error::with_recovery_paths(error, &success.stats().temp_recovery_paths)
                })?;
            // Erst der terminale Queuecommit macht den vollständig ausgeführten Lauf
            // nicht mehr wiederholbar. Der Abschlussaudit folgt deshalb bewusst
            // danach und darf bei einem Absturz höchstens fehlen, nie voreilen.
            let stats = success.append_completion_audit()?;
            Ok(QueueExecutionResult::Sync(stats))
        }
        Err(error) => {
            if let Err(journal_error) =
                queue.fail_coded(&operation_id, failed_progress(latest), &error)
            {
                return Err(failed_queue_error_commit(
                    &error,
                    &journal_error,
                    "terminale Queue-Fehlercommit",
                ));
            }
            Err(error)
        }
    }
}

/// Einen bidirektionalen Queue-Eintrag aus seinen geheimnisfreien Entscheidungen
/// frisch scannen. Das gespeicherte Journal autorisiert nichts: Erst identische
/// Baseline-/Plan-Digests führen zu einer RAM-gehaltenen Einmal-Vorbereitung.
pub fn prepare_bidirectional_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
) -> io::Result<BidirectionalQueuePreparation> {
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    let QueueOperation::Bidirectional {
        pair_id,
        pair_name,
        left,
        right,
        baseline_path,
        resolutions,
        expected_baseline_digest,
        expected_plan_digest,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bidirektionaler Executor erhielt keinen passenden Queue-Eintrag",
        ));
    };
    let prepared = if matches!(left.as_ref(), BidirectionalQueueEndpoint::Local { .. })
        && matches!(right.as_ref(), BidirectionalQueueEndpoint::Local { .. })
    {
        prepare_local_bidirectional_with_decisions(
            pair_id,
            &pair_name,
            Path::new(left.root()),
            Path::new(right.root()),
            Path::new(&baseline_path),
            &resolutions,
        )?
    } else {
        let left_endpoint = left.connect(secrets.clone())?;
        let right_endpoint = right.connect(secrets)?;
        prepare_provider_bidirectional_with_decisions(
            pair_id,
            &pair_name,
            left_endpoint,
            right_endpoint,
            Path::new(&baseline_path),
            &resolutions,
        )?
    };
    let current_baseline_digest = prepared.preview().plan.baseline_digest;
    let current_plan_digest = prepared.preview().plan.plan_digest;
    if current_baseline_digest != expected_baseline_digest
        || current_plan_digest != expected_plan_digest
    {
        prepared.discard()?;
        let reason =
            "Bidirektionale Baseline, Bäume oder Konfliktentscheidungen änderten sich seit dem Enqueue"
                .to_string();
        queue.require_confirmation(
            &operation_id,
            QueueAttention::BidirectionalPlanChanged {
                reason: reason.clone(),
                confirmation: format!(
                    "BEIDE-PLAN {} {}",
                    short_id(&operation_id),
                    short_digest(&current_plan_digest)
                ),
                current_baseline_digest,
                current_plan_digest,
            },
        )?;
        return Ok(BidirectionalQueuePreparation::AwaitingConfirmation { reason });
    }
    let reason =
        "Bidirektionaler Lauf ist frisch geprüft und wartet auf seine einmalige Laufphrase"
            .to_string();
    if let Err(error) = queue.require_confirmation(
        &operation_id,
        QueueAttention::RemoteRun {
            reason,
            confirmation: prepared.preview().confirmation_phrase.clone(),
        },
    ) {
        prepared.discard()?;
        return Err(error);
    }
    Ok(BidirectionalQueuePreparation::Ready(Box::new(
        PreparedBidirectionalQueueRun {
            operation_id,
            prepared,
        },
    )))
}

fn bidirectional_temp_recovery_paths(stats: &LocalBidirectionalStats) -> Vec<String> {
    stats
        .runs
        .iter()
        .flat_map(|run| run.temp_recovery_paths.iter().cloned())
        .collect()
}

/// Eine RAM-gehaltene bidirektionale Queue-Vorbereitung ausführen. Der Queue-
/// `Running`-Commit liegt nach Phrase, Backup-Gate und frischem Driftcheck, aber vor
/// der ersten Nutzerdatenmutation.
pub fn execute_prepared_bidirectional_queue_entry(
    queue: &OperationQueue,
    prepared: PreparedBidirectionalQueueRun,
    confirmation: &str,
    backup_confirmed: bool,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    let operation_id = prepared.operation_id;
    if prepared.prepared.preview().confirmation_phrase != confirmation {
        return Err(crate::error::coded_error(
            io::ErrorKind::PermissionDenied,
            crate::error::ErrorCode::ConfirmationMismatch,
            "Bidirektionale Queue-Laufphrase stimmt nicht überein",
        ));
    }
    let run = prepared.prepared.confirm(confirmation, backup_confirmed)?;
    queue.mark_remote_running(&operation_id, confirmation)?;
    let mut queue_progress = QueueProgress::new(queue, &operation_id, cancel, on_progress);
    let result = run.execute(cancel, &mut |progress| {
        queue_progress.report(progress);
    });
    let (mut latest, persistence_error) = queue_progress.finish();
    if let Some(error) = persistence_error {
        // Das Journal hält diesen Vorgang als `Interrupted` mit `OutcomeUnclear`
        // fest, weil die Ausführung schon gewirkt haben kann. Derselbe Code muss
        // auch an dieser Executorgrenze ankommen: `io::Error::other` trüge
        // `ErrorCode::Io`, und CLI wie GUI stuften einen möglicherweise
        // teilwirksamen Lauf dann als gewöhnlichen Ein-/Ausgabefehler ein. Das
        // Ergebnis des abgebrochenen Laufs gehört mit in den Text, sonst geht
        // verloren, wie weit die Ausführung selbst gekommen war — der
        // Remote-Zwilling dieser Stelle macht es genauso.
        let failure = possible_mutation_progress_error(error, &result);
        let failure = match &result {
            Ok(stats) => {
                let paths = bidirectional_temp_recovery_paths(stats);
                crate::error::with_recovery_paths(failure, &paths)
            }
            Err(_) => failure,
        };
        if let Err(journal_error) =
            queue.interrupt_bidirectional_coded(&operation_id, recovery_progress(latest), &failure)
        {
            return Err(failed_queue_error_commit(
                &failure,
                &journal_error,
                "Interrupted-Commit im Queue-Journal",
            ));
        }
        return Err(failure);
    }
    match result {
        Ok(stats) => {
            latest.phase = "done".into();
            let paths = bidirectional_temp_recovery_paths(&stats);
            // Pfade und Abschlusszustand gehören in denselben Journalcommit.
            queue
                .finish_bidirectional_after_effect_with_paths(&operation_id, false, latest, &paths)
                .and_then(|()| queue.finish_bidirectional_completion_recovery(&operation_id))
                .map_err(|error| crate::error::with_recovery_paths(error, &paths))?;
            Ok(QueueExecutionResult::Bidirectional(stats))
        }
        Err(error) => {
            if let Err(journal_error) = queue.interrupt_bidirectional_coded(
                &operation_id,
                recovery_progress(latest),
                &error,
            ) {
                return Err(failed_queue_error_commit(
                    &error,
                    &journal_error,
                    "Interrupted-Commit im Queue-Journal",
                ));
            }
            Err(error)
        }
    }
}

/// Einen bereits atomar geclaimten lokalen Eintrag vollständig validieren, ausführen
/// und terminal persistieren. Tauri und spätere Headless-Scheduler benutzen exakt
/// diesen Pfad; nur Eventdarstellung und Worker-Lebenszyklus bleiben außen.
pub fn execute_local_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    execution: &LocalQueueExecution,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    execute_local_queue_entry_detailed(queue, entry, execution, cancel, on_progress)
        .map_err(public_file_execution_error)
}

/// Ergebnis der Journalnachbereitung eines gescheiterten Queue-Eintrags.
enum SettledQueueFailure {
    /// Der bereits vollzogene Dateiabschluss ließ sich nachträglich doch noch
    /// dauerhaft festschreiben. Der Eintrag gilt damit als erfolgreich.
    Completed(QueueExecutionResult),
    /// Der Eintrag bleibt gescheitert. `error` ist der öffentlich führende
    /// Fehler, `journal_errors` sammelt zusätzlich misslungene Journalschritte.
    Failed {
        error: io::Error,
        journal_errors: Vec<io::Error>,
    },
}

/// Schreibt den Journalzustand eines gescheiterten Eintrags fest.
///
/// Der lokale und der Provider-Dateiweg teilen sich diese Nachbereitung
/// vollständig. Geschrieben wird nur ein Eintrag, der noch als laufend geführt
/// ist: Ein inzwischen pausierter, abgebrochener oder von außen umgesetzter
/// Eintrag behält seinen Zustand.
///
/// Liegt ein terminaler Dateiabschluss vor, wird zuerst genau dieser erneut
/// festgeschrieben — die Dateiwirkung ist dann bereits vollzogen und es fehlt
/// nur ihr dauerhafter Queuezustand. `pending_completion` wird genau dann
/// geleert, wenn dieser Commit gelang; daran erkennt der Aufrufer später, ob
/// der Ausgang noch unklar ist.
fn settle_failed_queue_entry(
    queue: &OperationQueue,
    operation_id: &str,
    is_file_operation: bool,
    error: io::Error,
    progress: &Progress,
    actuals: &FileOperationActuals,
    pending_completion: &mut Option<Box<PendingFileCompletion>>,
) -> SettledQueueFailure {
    let mut error = error;
    let state = queue
        .snapshot()
        .entries
        .iter()
        .find(|candidate| candidate.operation_id == operation_id)
        .map(|candidate| candidate.state);
    let mut journal_errors = Vec::new();
    if matches!(state, Some(QueueState::Validating | QueueState::Running)) {
        if let Some(completion) = pending_completion.as_deref().cloned() {
            match queue.retry_file_completion(operation_id, &completion) {
                Ok(()) => {
                    *pending_completion = None;
                    match queue.append_file_completion_audit(operation_id, &completion) {
                        Ok(()) => {
                            return SettledQueueFailure::Completed(QueueExecutionResult::File(
                                completion.stats.clone(),
                            ));
                        }
                        Err(audit_error) => error = audit_error,
                    }
                }
                Err(persistence_error) => {
                    journal_errors.push(queue_progress_persistence_error(persistence_error))
                }
            }
        } else if is_file_operation {
            if let Err(persistence_error) =
                queue.fail_file_actuals_coded(operation_id, progress.clone(), &error, actuals)
            {
                journal_errors.push(queue_progress_persistence_error(persistence_error));
            }
        } else if let Err(persistence_error) = queue.fail_coded_with_recovery(
            operation_id,
            progress.clone(),
            &error,
            &actuals.temp_recovery_paths,
        ) {
            journal_errors.push(queue_progress_persistence_error(persistence_error));
        }
    }
    SettledQueueFailure::Failed {
        error,
        journal_errors,
    }
}

fn staging_recovery_paths(queue: &OperationQueue, operation_id: &str) -> io::Result<Vec<String>> {
    Ok(
        crate::local_staging::recover_queue_beneath(&queue.path, &queue._process_lock.root)?
            .into_iter()
            .filter(|recovery| recovery.operation == operation_id)
            .flat_map(|recovery| recovery.paths)
            .collect(),
    )
}

fn failure_with_staging_recovery(
    queue: &OperationQueue,
    operation_id: &str,
    actuals: &mut FileOperationActuals,
    error: io::Error,
) -> io::Error {
    for path in crate::error::recovery_paths(&error) {
        if !actuals.temp_recovery_paths.contains(&path) {
            actuals.temp_recovery_paths.push(path);
        }
    }
    match crate::local_staging::recover_queue_beneath(&queue.path, &queue._process_lock.root) {
        Ok(recoveries) => {
            let own = recoveries.into_iter().filter(|recovery| recovery.operation == operation_id).collect::<Vec<_>>();
            let ftps_unsettled = own.iter().any(|recovery| recovery.ftps_root.is_some());
            for path in own.into_iter().flat_map(|recovery| recovery.paths) {
                if !actuals.temp_recovery_paths.contains(&path) { actuals.temp_recovery_paths.push(path); }
            }
            if ftps_unsettled {
                crate::error::with_recovery_paths(crate::coded_error(error.kind(), crate::ErrorCode::OutcomeUnclear,
                    format!("Offene FTPS-Intents verlangen manuelle Prüfung ohne Wiederholung: {error}")), &actuals.temp_recovery_paths)
            } else { error }
        }
        Err(recovery_error) => crate::error::with_recovery_paths(
            crate::coded_error(io::ErrorKind::Other, crate::ErrorCode::OutcomeUnclear,
                format!("Ausführung scheiterte ({error}); lokale Rest-Recovery konnte nicht geprüft werden ({recovery_error})")),
            &actuals.temp_recovery_paths,
        ),
    }
}

/// Detaillierter lokaler Executor für Oberflächen, die bei einem Fehler neben dem
/// `ErrorCode` auch bereits sicher bekannte Best-Effort-Wirkungen anzeigen.
pub fn execute_local_queue_entry_detailed(
    queue: &OperationQueue,
    entry: QueueEntry,
    execution: &LocalQueueExecution,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<QueueExecutionResult, ProviderFileExecutionFailure> {
    let mut actuals = FileOperationActuals::default();
    let mut pending_completion = None;
    let operation_id = entry.operation_id.clone();
    let is_file_operation = matches!(&entry.operation, QueueOperation::File { .. });
    let mut progress = entry.progress.clone();
    let mut capture_progress = |current: &Progress| {
        progress = current.clone();
        on_progress(current);
    };
    let result = execute_local_queue_entry_inner(
        queue,
        entry,
        execution,
        cancel,
        &mut capture_progress,
        &mut actuals,
        &mut pending_completion,
    );
    let error = match result {
        Ok(result) => return Ok(result),
        Err(error) => error,
    };
    let error = failure_with_staging_recovery(queue, &operation_id, &mut actuals, error);
    let (error, journal_errors) = match settle_failed_queue_entry(
        queue,
        &operation_id,
        is_file_operation,
        error,
        &progress,
        &actuals,
        &mut pending_completion,
    ) {
        SettledQueueFailure::Completed(result) => return Ok(result),
        SettledQueueFailure::Failed {
            error,
            journal_errors,
        } => (error, journal_errors),
    };
    Err(ProviderFileExecutionFailure {
        error,
        journal_errors,
        actuals: Box::new(actuals),
        progress: Box::new(progress),
        pending_completion,
        recovery: None,
    })
}

fn execute_local_queue_entry_inner(
    queue: &OperationQueue,
    entry: QueueEntry,
    execution: &LocalQueueExecution,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
    pending_completion: &mut Option<Box<PendingFileCompletion>>,
) -> io::Result<QueueExecutionResult> {
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    match entry.operation {
        QueueOperation::File {
            kind,
            items,
            destination,
            overwrite,
            mut expected_conflicts,
            mut expected_conflict_proofs,
            keep_both_targets,
            local_replace_best_effort_expected,
            local_copy_new_best_effort_expected,
        } => execute_local_file_entry(
            LocalFileQueueRequest {
                queue,
                operation_id: &operation_id,
                kind,
                items,
                destination,
                overwrite,
                expected_conflicts: &mut expected_conflicts,
                expected_conflict_proofs: &mut expected_conflict_proofs,
                keep_both_targets: &keep_both_targets,
                local_replace_best_effort_expected,
                local_copy_new_best_effort_expected,
            },
            execution,
            cancel,
            on_progress,
            actuals,
            pending_completion,
        ),
        QueueOperation::Sync {
            source,
            target,
            sandbox,
            options,
            expected_plan,
        } => execute_local_sync_entry(
            LocalSyncQueueRequest {
                queue,
                operation_id: &operation_id,
                source,
                target,
                expected_sandbox: sandbox,
                options,
                expected_plan: *expected_plan,
            },
            execution,
            cancel,
            on_progress,
        ),
        QueueOperation::FtpsRecovery { .. }
        | QueueOperation::LocalSync { .. }
        | QueueOperation::ProviderFile { .. }
        | QueueOperation::ProviderDelete { .. }
        | QueueOperation::SftpSync { .. }
        | QueueOperation::FtpsSync { .. }
        | QueueOperation::WebDavSync { .. }
        | QueueOperation::SmbSync { .. }
        | QueueOperation::ProviderSync { .. }
        | QueueOperation::Bidirectional { .. } => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Remote-Queue-Einträge benötigen den Remote-Executor",
        )),
    }
}

fn require_local_file_commit_confirmation(
    queue: &OperationQueue,
    operation_id: &str,
    vfs: &dyn Vfs,
    destination: &Path,
    expected: Option<&crate::vfs::LocalRootIdentity>,
) -> io::Result<bool> {
    if vfs.file_commit_class_at(destination)? != crate::vfs::FileCommitClass::BestEffort {
        return Ok(false);
    }
    let identity = vfs.local_root_identity(destination)?;
    if expected == Some(&identity) {
        return Ok(false);
    }
    queue.require_confirmation(
        operation_id,
        QueueAttention::LocalFileCommit {
            reason: format!(
                "Noch nichts übertragen. Ziel: {}. {}",
                destination.display(),
                crate::vfs::LOCAL_FILE_COMMIT_LIMITATION
            ),
            confirmation: format!("BEST EFFORT {}", short_id(operation_id)),
            target_identity: identity,
        },
    )?;
    Ok(true)
}

struct LocalFileQueueRequest<'a> {
    queue: &'a OperationQueue,
    operation_id: &'a str,
    kind: FileQueueKind,
    items: Vec<String>,
    destination: String,
    overwrite: bool,
    expected_conflicts: &'a mut Vec<String>,
    expected_conflict_proofs: &'a mut Vec<FileConflictProof>,
    keep_both_targets: &'a [crate::fileops::FileKeepBothTarget],
    local_replace_best_effort_expected: Option<bool>,
    local_copy_new_best_effort_expected: Option<Box<crate::vfs::LocalRootIdentity>>,
}

fn execute_local_file_entry(
    request: LocalFileQueueRequest<'_>,
    execution: &LocalQueueExecution,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
    pending_completion: &mut Option<Box<PendingFileCompletion>>,
) -> io::Result<QueueExecutionResult> {
    let LocalFileQueueRequest {
        queue,
        operation_id,
        kind,
        items,
        destination,
        overwrite,
        expected_conflicts,
        expected_conflict_proofs,
        keep_both_targets,
        local_replace_best_effort_expected,
        local_copy_new_best_effort_expected,
    } = request;
    let items: Vec<PathBuf> = items.into_iter().map(PathBuf::from).collect();
    let destination = PathBuf::from(destination);
    let vfs = LocalVfs::new().with_staging_owner(&queue.path, operation_id);
    let target_binding = LocalVfs::rooted(&destination)?;
    let vfs = vfs.with_file_commit_root(&target_binding);
    if require_local_file_commit_confirmation(
        queue,
        operation_id,
        &target_binding,
        &destination,
        local_copy_new_best_effort_expected.as_deref(),
    )? {
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason: crate::vfs::LOCAL_FILE_COMMIT_LIMITATION.into(),
        });
    }
    crate::fileops::verify_keep_both_targets(&vfs, &vfs, &items, &destination, keep_both_targets)?;
    let local_copy_new_best_effort = target_binding.file_commit_class_at(&destination)?
        == crate::vfs::FileCommitClass::BestEffort;
    // Rekursiv, nicht nur die oberste Ebene: Der Executor verlangt beim
    // Zusammenführen für JEDE bereits vorhandene Zieldatei einen Konfliktnachweis.
    // Mit nur den obersten Namen bräche eine Ordnerkopie am ersten tieferen
    // vorhandenen Ziel ab — nachdem sie neue Untereinträge schon angelegt hat.
    let mut current_conflicts = find_recursive_conflicts(&vfs, &vfs, &destination, &items)?;
    current_conflicts.sort();
    expected_conflicts.sort();
    let current_conflict_proofs = if overwrite {
        capture_file_conflict_proofs(&vfs, &current_conflicts)?
    } else {
        Vec::new()
    };
    expected_conflict_proofs.sort_by(|left, right| left.path.cmp(&right.path));
    if overwrite
        && (current_conflicts != *expected_conflicts
            || current_conflict_proofs != *expected_conflict_proofs)
    {
        let reason = "Die Zielkonflikte haben sich seit der Bestätigung geändert".to_string();
        queue.require_confirmation(
            operation_id,
            QueueAttention::ConflictsChanged {
                reason: reason.clone(),
                confirmation: format!(
                    "KONFLIKTE {} {}",
                    short_id(operation_id),
                    current_conflicts.len()
                ),
                current_conflicts,
                current_conflict_proofs,
            },
        )?;
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason,
        });
    }
    let local_replace_best_effort = overwrite
        && !current_conflicts.is_empty()
        && vfs.file_replace_class() == FileReplaceClass::BestEffort;
    if local_replace_best_effort && local_replace_best_effort_expected != Some(true) {
        // Der Text landet unverändert im Fehlerbalken und im Queue-Eintrag. Er nennt
        // deshalb zuerst die Wirkung („nichts überschrieben"), dann den Grund in
        // Alltagssprache; die Ausführungsklasse steht als Fachwort dahinter, damit
        // Audit und Oberfläche dieselbe Sprache sprechen.
        let reason = "Nichts überschrieben: Dieses Dateisystem kann beim Ersetzen die vorhandene Fassung nicht sicher aufbewahren, bis die neue vollständig geschrieben ist. Geht dabei etwas schief, kann die alte Fassung verloren sein (Ausführungsklasse Best Effort)".to_string();
        queue.require_confirmation(
            operation_id,
            QueueAttention::BestEffortCommitDowngrade {
                reason: reason.clone(),
                confirmation: format!("BEST EFFORT {}", short_id(operation_id)),
                copy_new_best_effort: false,
                replace_best_effort: true,
                remove_source_best_effort: false,
            },
        )?;
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason,
        });
    }
    queue.mark_running(operation_id)?;
    let audit_context = ProviderAuditContext {
        source_provider: Some(crate::ProviderKind::Local),
        target_provider: crate::ProviderKind::Local,
        execution_class: if local_replace_best_effort || local_copy_new_best_effort {
            ProviderAuditExecutionClass::BestEffort
        } else {
            ProviderAuditExecutionClass::Assured
        },
        source_removal_best_effort_expected: false,
        target_copy_new_best_effort_expected: local_copy_new_best_effort,
        target_replace_best_effort_expected: local_replace_best_effort,
        source_removal_best_effort_actual: None,
        target_copy_new_best_effort_actual: None,
        target_replace_best_effort_actual: None,
    };
    let audit_operation = match kind {
        FileQueueKind::Copy => "local_copy",
        FileQueueKind::Move => "local_move",
    };
    queue.append_file_audit(
        operation_id,
        audit_operation,
        "started",
        &Progress::default(),
        &audit_context,
        None,
    )?;
    let policy = if overwrite {
        OverwritePolicy::Overwrite
    } else {
        OverwritePolicy::Skip
    };
    let mut queue_progress = QueueProgress::new(queue, operation_id, cancel, on_progress);
    let result = {
        let mut callback = |progress: &Progress| {
            queue_progress.report(progress);
        };
        match kind {
            FileQueueKind::Copy => crate::fileops::copy_items_with_targets_tracked(
                FileOperationRequest {
                    src: &vfs,
                    dst: &vfs,
                    items: &items,
                    dst_dir: &destination,
                    policy,
                    sandbox: &execution.sandbox,
                },
                crate::fileops::FileTransferBindings {
                    targets: keep_both_targets,
                    replacements: Some(expected_conflict_proofs),
                    skipped_targets: None,
                },
                cancel,
                &mut callback,
                actuals,
            ),
            FileQueueKind::Move => crate::fileops::move_items_with_targets_tracked(
                FileOperationRequest {
                    src: &vfs,
                    dst: &vfs,
                    items: &items,
                    dst_dir: &destination,
                    policy,
                    sandbox: &execution.sandbox,
                },
                crate::fileops::FileTransferBindings {
                    targets: keep_both_targets,
                    replacements: Some(expected_conflict_proofs),
                    skipped_targets: None,
                },
                false,
                cancel,
                &mut callback,
                actuals,
            ),
        }
    };
    let (latest, mut persistence_error) = queue_progress.finish();
    // Nur ein nichtterminaler Fehlerweg braucht noch eine letzte
    // Fortschrittsmomentaufnahme. Bei Erfolg übernimmt `complete` beziehungsweise
    // `cancel` den Endfortschritt in derselben atomaren Journalmutation; ein eigener
    // Update-Schritt davor könnte die vollzogene Dateiwirkung fälschlich als Failed
    // erscheinen lassen.
    if result.is_err() && !file_progress_is_terminal(&latest) {
        flush_latest_file_progress(queue, operation_id, &latest, &mut persistence_error);
    }
    if let Some(error) = persistence_error {
        let failed_context = audit_context_with_actuals(audit_context, actuals);
        let _ = queue.append_file_audit_with_temp_paths(
            operation_id,
            audit_operation,
            "failed",
            &latest,
            &failed_context,
            FileAuditRecovery {
                provider: None,
                temp_paths: &actuals.temp_recovery_paths,
                retained_backups: &actuals.retained_backup_paths,
            },
        );
        return Err(possible_mutation_progress_error(error, &result));
    }
    let stats = match result {
        Ok(stats) => stats,
        Err(error) => {
            let failed_context = audit_context_with_actuals(audit_context, actuals);
            let _ = queue.append_file_audit_with_temp_paths(
                operation_id,
                audit_operation,
                "failed",
                &latest,
                &failed_context,
                FileAuditRecovery {
                    provider: None,
                    temp_paths: &actuals.temp_recovery_paths,
                    retained_backups: &actuals.retained_backup_paths,
                },
            );
            return Err(error);
        }
    };
    complete_file_queue_entry(
        queue,
        operation_id,
        latest,
        stats,
        audit_operation,
        audit_context,
        pending_completion,
    )
}

/// Führt eine interaktive Auswahloperation nach frischem Verbindungsaufbau aus.
/// Das Journal enthält ausschließlich Endpunktkonfiguration, Pfade und die vom
/// Nutzer gesehene Konfliktliste; Sitzungen und Secrets bleiben in diesem Aufruf.
pub fn execute_provider_file_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    execution: &LocalQueueExecution,
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    execute_provider_file_queue_entry_detailed(
        queue,
        entry,
        execution,
        secrets,
        cancel,
        on_progress,
    )
    .map_err(public_file_execution_error)
}

pub fn execute_provider_file_queue_entry_detailed(
    queue: &OperationQueue,
    entry: QueueEntry,
    execution: &LocalQueueExecution,
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<QueueExecutionResult, ProviderFileExecutionFailure> {
    let ftps_preparation = provider_file_has_ftps_endpoint(&entry.operation);
    let mut actuals = FileOperationActuals::default();
    let mut recovery = None;
    let mut pending_completion = None;
    let operation_id = entry.operation_id.clone();
    let mut progress = entry.progress.clone();
    let mut capture_progress = |current: &Progress| {
        progress = current.clone();
        on_progress(current);
    };
    let result = {
        let mut tracking = ProviderFileExecutionTracking {
            actuals: &mut actuals,
            recovery: &mut recovery,
            pending_completion: &mut pending_completion,
        };
        execute_provider_file_queue_entry_inner(
            queue,
            entry,
            execution,
            secrets,
            cancel,
            &mut capture_progress,
            &mut tracking,
        )
    };
    let error = match result {
        Ok(result) => return Ok(result),
        Err(error) => error,
    };
    let mut error = failure_with_staging_recovery(queue, &operation_id, &mut actuals, error);
    // Vor der Nutzerwirkung darf ein sauberer Abbruch keine Fehler-/Retry-Zeile
    // erzeugen. Unklare Probenreste und bereits laufende Wirkungen bleiben Fehler.
    if ftps_preparation
        && cancel.is_cancelled()
        && crate::error_code(&error) == crate::ErrorCode::Interrupted
        && crate::error::recovery_paths(&error).is_empty()
        && actuals.temp_recovery_paths.is_empty()
        && pending_completion.is_none()
        && queue.snapshot().entries.iter().any(|entry| {
            entry.operation_id == operation_id && entry.state == QueueState::Validating
        })
    {
        let cancelled_progress = Progress {
            phase: "cancelled".into(),
            ..Progress::default()
        };
        match queue
            .mark_running(&operation_id)
            .and_then(|_| queue.cancel(&operation_id, cancelled_progress))
        {
            Ok(()) => {
                return Ok(QueueExecutionResult::File(OpStats {
                    cancelled: true,
                    ..OpStats::default()
                }))
            }
            Err(journal_error) => error = journal_error,
        }
    }
    // Der Providerweg führt ausschließlich Dateioperationen aus; ein
    // gescheiterter Eintrag wird deshalb immer über `fail_file` bilanziert.
    let (error, journal_errors) = match settle_failed_queue_entry(
        queue,
        &operation_id,
        true,
        error,
        &progress,
        &actuals,
        &mut pending_completion,
    ) {
        SettledQueueFailure::Completed(result) => return Ok(result),
        SettledQueueFailure::Failed {
            error,
            journal_errors,
        } => (error, journal_errors),
    };
    Err(ProviderFileExecutionFailure {
        error,
        journal_errors,
        actuals: Box::new(actuals),
        progress: Box::new(progress),
        pending_completion,
        recovery,
    })
}

struct ProviderFileExecutionTracking<'a> {
    actuals: &'a mut FileOperationActuals,
    recovery: &'a mut Option<ProviderFileRecovery>,
    pending_completion: &'a mut Option<Box<PendingFileCompletion>>,
}

/// Formuliert, warum ein Provider-Schreibvorgang auf eine Bestätigung wartet.
///
/// Wie beim lokalen Ersetzen steht die Wirkung vorn („Nichts übertragen"), dann der
/// Grund in Alltagssprache; das Fachwort folgt in Klammern, damit Audit, Queue und
/// Oberfläche dieselbe Sprache sprechen. Die vom Provider gemeldeten Grenzen bleiben
/// wörtlich erhalten — nur sie sagen, woran es konkret liegt.
fn best_effort_target_reason(
    copy_new_downgraded: bool,
    replace_downgraded: bool,
    confirmation_missing: bool,
    limitations: &[String],
) -> String {
    let betroffene_wirkung = match (copy_new_downgraded, replace_downgraded) {
        (true, true) => "das Anlegen neuer Dateien und das Ersetzen vorhandener",
        (true, false) => "das Anlegen neuer Dateien",
        (false, true) => "das Ersetzen vorhandener Dateien",
        // Der Aufrufer prüft vorher, dass mindestens eine Wirkung betroffen ist.
        // Bliebe der Fall doch einmal übrig, ist eine unscharfe Aussage besser als
        // eine falsche.
        (false, false) => "diesen Schreibvorgang",
    };
    let reason = if confirmation_missing {
        format!("Für {betroffene_wirkung} fehlt eine aktuelle Bestätigung der Ausführungsklasse Best Effort.")
    } else {
        format!(
            "Der Server sichert {betroffene_wirkung} nicht mehr so zu wie bei der Vorbereitung."
        )
    };
    format!(
        "Nichts übertragen: {reason} Bricht die Übertragung ab, kann eine unvollständige Datei zurückbleiben (Ausführungsklasse Best Effort). Grenzen des Ziels: {}",
        limitations.join("; ")
    )
}

fn execute_provider_file_queue_entry_inner(
    queue: &OperationQueue,
    entry: QueueEntry,
    execution: &LocalQueueExecution,
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    tracking: &mut ProviderFileExecutionTracking<'_>,
) -> io::Result<QueueExecutionResult> {
    let actuals = &mut *tracking.actuals;
    let recovery = &mut *tracking.recovery;
    let pending_completion = &mut *tracking.pending_completion;
    // Persistierte Eingaben müssen scheitern, bevor Destrukturierung, Secretzugriff,
    // Netzwerk oder eine Capability-Probe überhaupt möglich werden.
    validate_operation(&entry.operation)?;
    let preparation_cancel = provider_file_has_ftps_endpoint(&entry.operation).then_some(cancel);
    provider_file_preparation_not_cancelled(preparation_cancel)?;
    let operation_id = entry.operation_id;
    let QueueOperation::ProviderFile {
        kind,
        source,
        target,
        items,
        destination,
        overwrite,
        mut expected_conflicts,
        source_tree_expected,
        mut expected_conflict_proofs,
        keep_both_targets,
        best_effort_commit_expected: _,
        local_copy_new_best_effort_expected,
        copy_new_best_effort_expected,
        replace_best_effort_expected,
        remove_source_best_effort_expected,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider-Dateiexecutor erhielt keinen ProviderFile-Eintrag",
        ));
    };

    crate::fileops::guard_local_transfer_roots(
        matches!(source.as_ref(), BidirectionalQueueEndpoint::Local { .. })
            .then(|| Path::new(source.root())),
        matches!(target.as_ref(), BidirectionalQueueEndpoint::Local { .. })
            .then(|| Path::new(target.root())),
        kind == FileQueueKind::Move,
        &execution.sandbox,
    )?;
    if matches!(source.as_ref(), BidirectionalQueueEndpoint::Local { .. })
        && matches!(target.as_ref(), BidirectionalQueueEndpoint::WebDav { .. })
    {
        crate::validate_interactive_file_transfer_request(
            crate::ProviderKind::Local,
            crate::ProviderKind::WebDav,
            kind == FileQueueKind::Move,
            overwrite,
        )?;
    }
    if kind == FileQueueKind::Copy
        && !overwrite
        && matches!(source.as_ref(), BidirectionalQueueEndpoint::Local { .. })
    {
        if let BidirectionalQueueEndpoint::WebDav { connection, .. } = target.as_ref() {
            crate::webdav::ensure_ordinary_upload_profile_current(
                &queue.path,
                &entry.profile_bindings,
                connection,
            )?;
        }
    }
    let queued_source_root = PathBuf::from(source.root());
    let shares_connection_definition = source.shares_connection_definition(&target);
    let Some(source_endpoint) = connect_queue_endpoint_with_credential_attention(
        queue,
        &operation_id,
        &source,
        secrets.clone(),
    )?
    else {
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason: "Provider-Zugangsdaten sind nicht verfügbar oder wurden abgelehnt".into(),
        });
    };
    let Some(independently_connected_target) =
        connect_queue_endpoint_with_credential_attention(queue, &operation_id, &target, secrets)?
    else {
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason: "Provider-Zugangsdaten sind nicht verfügbar oder wurden abgelehnt".into(),
        });
    };
    let (source, target, shared_session) = bind_connected_queue_endpoints(
        source_endpoint,
        independently_connected_target,
        shares_connection_definition,
        Path::new(target.root()),
    )?;

    let additional_root = [target.root()];
    let source = source.with_queue_owner_with_cancel(
        queue,
        Some(&operation_id),
        if shared_session {
            &additional_root
        } else {
            &[]
        },
        Some(cancel),
    )?;
    let target =
        target.with_queue_owner_with_cancel(queue, Some(&operation_id), &[], Some(cancel))?;
    let source_is_local = source.kind() == crate::ProviderKind::Local;
    let target_is_local = target.kind() == crate::ProviderKind::Local;
    source.validate_interactive_file_transfer(&target, kind == FileQueueKind::Move, overwrite)?;
    let lexical_item_paths = items.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    let source_root = source.root().to_path_buf();
    let item_paths = if source_is_local {
        let (bound_root, bound_items) = crate::fileops::bind_local_file_operation_scope(
            &queued_source_root,
            &lexical_item_paths,
        )?;
        if bound_root != source_root {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Lokaler Quellroot änderte sich bei der descriptor-relativen Bindung",
            ));
        }
        bound_items
    } else {
        lexical_item_paths
    };

    let crate::InteractiveFileContexts {
        source_vfs,
        target_vfs: interactive_target_vfs,
        target_capabilities: target_capabilities_override,
    } = source.interactive_file_contexts_owned(
        &target,
        &item_paths,
        kind == FileQueueKind::Move,
        overwrite,
        cancel,
        Some(crate::remote_real_mode::InteractiveFileOwner {
            staging: crate::local_staging::StagingOwner {
                queue: queue.path.clone(),
                operation: operation_id.clone(),
            },
            profiles: entry.profile_bindings.clone(),
        }),
    )?;
    let current_tree = source_vfs.interactive_source_tree()?;
    if current_tree.as_ref() != source_tree_expected.as_deref() {
        let reason = "Der Quellbaum ist neu oder hat sich seit der Vorschau geändert; vor jeder Zielwirkung erneut bestätigen".to_owned();
        queue.require_confirmation(
            &operation_id,
            QueueAttention::SourceTreeChanged {
                reason: reason.clone(),
                confirmation: format!(
                    "BAUM {} {}",
                    short_id(&operation_id),
                    current_tree.as_ref().map_or(0, |tree| tree.entries)
                ),
                current_tree: current_tree.map(Box::new),
            },
        )?;
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason,
        });
    }
    let (target_vfs, target_root): (Arc<dyn Vfs>, PathBuf) = if source.kind()
        == crate::ProviderKind::Sftp
    {
        let root = target.root().to_path_buf();
        (
            Arc::new(
                LocalVfs::rooted(&root)?.with_required_temp_recovery(&queue.path, &operation_id),
            ),
            root,
        )
    } else {
        (interactive_target_vfs, target.root().to_path_buf())
    };
    let destination = if target_is_local {
        std::fs::canonicalize(destination)?
    } else {
        PathBuf::from(destination)
    };
    if destination != target_root
        || item_paths
            .iter()
            .any(|path| !path.starts_with(&source_root) || path == &source_root)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider-Dateipfade verließen den frisch gebundenen Pane-Root",
        ));
    }
    crate::fileops::verify_keep_both_targets(
        source_vfs.as_ref(),
        target_vfs.as_ref(),
        &item_paths,
        &destination,
        &keep_both_targets,
    )?;
    let current_conflicts = find_recursive_conflicts(
        source_vfs.as_ref(),
        target_vfs.as_ref(),
        &destination,
        &item_paths,
    )?;
    let current_conflict_proofs = if overwrite {
        capture_file_conflict_proofs(target_vfs.as_ref(), &current_conflicts)?
    } else {
        Vec::new()
    };
    expected_conflicts.sort();
    expected_conflict_proofs.sort_by(|left, right| left.path.cmp(&right.path));
    if current_conflicts != expected_conflicts
        || current_conflict_proofs != expected_conflict_proofs
    {
        let reason =
            "Die Provider-Zielkonflikte haben sich seit der Bestätigung geändert".to_string();
        queue.require_confirmation(
            &operation_id,
            QueueAttention::ConflictsChanged {
                reason: reason.clone(),
                confirmation: format!(
                    "KONFLIKTE {} {}",
                    short_id(&operation_id),
                    current_conflicts.len()
                ),
                current_conflicts,
                current_conflict_proofs,
            },
        )?;
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason,
        });
    }

    let mut all_top_level_targets_missing = true;
    for source_path in &item_paths {
        let name = source_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Provider-Dateiquelle besitzt keinen sicheren Namen",
            )
        })?;
        match target_vfs.stat(
            &keep_both_targets
                .iter()
                .find(|target| Path::new(&target.source) == source_path)
                .map(|target| PathBuf::from(&target.target))
                .unwrap_or_else(|| destination.join(name)),
        ) {
            Ok(_) => all_top_level_targets_missing = false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    provider_file_preparation_not_cancelled(preparation_cancel)?;
    let prepared_target = source.probe_interactive_file_target(crate::ProviderFileTargetProbe {
        target: &target,
        items: &item_paths,
        kind,
        overwrite,
        skip_conflicts: !overwrite && keep_both_targets.is_empty(),
        inspect_conflicts: false,
        expected_conflicts: Some(&expected_conflicts),
        // Eine bestätigte Copy-/Remove-Wirkung bleibt auch bei später stärkerem
        // No-Replace bestehen. Ihr Entfernen darf nicht still zu Rename werden.
        shared_session: shared_session
            && (source.kind() != crate::ProviderKind::Ftps
                || remove_source_best_effort_expected != Some(true)),
        targets_missing: all_top_level_targets_missing,
        capabilities_override: target_capabilities_override,
    })?;
    provider_file_preparation_not_cancelled(preparation_cancel)?;
    let skip_only = prepared_target.skip_only;
    let source_removal_required = prepared_target.source_removal_required;
    let skipped_stats = prepared_target.skipped_stats;
    let same_namespace_move = prepared_target.rename_only;
    let target_capabilities = prepared_target.capabilities;
    if !skip_only
        && target_is_local
        && require_local_file_commit_confirmation(
            queue,
            &operation_id,
            target_vfs.as_ref(),
            &target_root,
            local_copy_new_best_effort_expected.as_deref(),
        )?
    {
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason: crate::vfs::LOCAL_FILE_COMMIT_LIMITATION.into(),
        });
    }

    if !skip_only
        && target_capabilities.as_ref().is_some_and(|capabilities| {
            !capabilities.copy_new && !capabilities.copy_new_best_effort
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            target_capabilities
                .expect("unmittelbar zuvor als Some geprüft")
                .blockers
                .join("; "),
        ));
    }
    // Eine Auswahl kann zugleich neue Namen anlegen und vorhandene Einträge ersetzen.
    // Beide Wirkungsklassen werden deshalb unabhängig an die sichtbare Vorschau
    // gebunden. `None` stammt aus einem Alteintrag und gilt nicht als Bestätigung.
    if let Some(capabilities) = target_capabilities.as_ref().filter(|_| !skip_only) {
        let copy_new_best_effort = !capabilities.copy_new && capabilities.copy_new_best_effort;
        let replace_best_effort =
            overwrite && !capabilities.replace && capabilities.replace_best_effort;
        let copy_new_downgraded =
            copy_new_best_effort && copy_new_best_effort_expected != Some(true);
        let replace_downgraded = replace_best_effort && replace_best_effort_expected != Some(true);
        if copy_new_downgraded || replace_downgraded {
            let reason = best_effort_target_reason(
                copy_new_downgraded,
                replace_downgraded,
                (copy_new_downgraded && copy_new_best_effort_expected.is_none())
                    || (replace_downgraded && replace_best_effort_expected.is_none()),
                &capabilities.limitations,
            );
            queue.require_confirmation(
                &operation_id,
                // Eigener Attention-Typ statt `Manual`: Nur so kann die
                // Bestätigung die gebundene Erwartung auf `Some(true)` setzen,
                // ohne dass eine SECRET-Bestätigung dieselbe Wirkung hätte.
                QueueAttention::BestEffortCommitDowngrade {
                    reason: reason.clone(),
                    confirmation: format!("BEST EFFORT {}", short_id(&operation_id)),
                    copy_new_best_effort: copy_new_downgraded,
                    replace_best_effort: replace_downgraded,
                    remove_source_best_effort: false,
                },
            )?;
            return Ok(QueueExecutionResult::AwaitingConfirmation {
                class: QueueOperationClass::File,
                reason,
            });
        }
    }
    if !skip_only
        && overwrite
        && !same_namespace_move
        && target_capabilities
            .as_ref()
            .is_none_or(|capabilities| !capabilities.replace && !capabilities.replace_best_effort)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Ersetzen ist für diese Zielverbindung weder mit Quarantäne noch als bestätigtes Best Effort gebunden",
        ));
    }
    let source_removal_best_effort = if kind == FileQueueKind::Move
        && !same_namespace_move
        && source_removal_required
        && !skip_only
    {
        let source_capabilities = source.probe_file_source_removal_capabilities()?;
        provider_file_preparation_not_cancelled(preparation_cancel)?;
        if !source_capabilities.remove_source && !source_capabilities.remove_source_best_effort {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Die Zielkopie wäre möglich, aber die Quellentfernung ist für diese Verbindung weder provideratomar noch als begrenzter Best-Effort-Pfad verfügbar; Move wurde nicht zu Copy abgeschwächt",
            ));
        }
        let remove_source_best_effort =
            !source_capabilities.remove_source && source_capabilities.remove_source_best_effort;
        if remove_source_best_effort && remove_source_best_effort_expected != Some(true) {
            let reason = format!(
                "Nichts verschoben: Der Server kann die Quelldatei nach dem Kopieren nicht nachweislich entfernen. Sie kann deshalb stehen bleiben, und ob sie wirklich weg ist, lässt sich hinterher nicht sicher sagen (Ausführungsklasse Best Effort). Grenzen der Quelle: {}",
                source_capabilities.limitations.join("; ")
            );
            queue.require_confirmation(
                &operation_id,
                QueueAttention::BestEffortCommitDowngrade {
                    reason: reason.clone(),
                    confirmation: format!("BEST EFFORT {}", short_id(&operation_id)),
                    copy_new_best_effort: false,
                    replace_best_effort: false,
                    remove_source_best_effort: true,
                },
            )?;
            return Ok(QueueExecutionResult::AwaitingConfirmation {
                class: QueueOperationClass::File,
                reason,
            });
        }
        if !remove_source_best_effort && remove_source_best_effort_expected == Some(true) {
            // Auch eine scheinbare Hochstufung ist ein anderer frisch verbundener
            // Providervertrag. Der persistierte Auftrag darf ihn nicht still gegen
            // die bestätigte Vorschau austauschen.
            return Err(crate::error::coded_error(
                io::ErrorKind::Other,
                crate::error::ErrorCode::PlanDrift,
                "Die bestätigte Best-Effort-Klasse der Move-Quellentfernung stimmt nicht mehr mit der frischen Providerfähigkeit überein",
            ));
        }
        remove_source_best_effort
    } else {
        false
    };

    provider_file_preparation_not_cancelled(preparation_cancel)?;

    let best_effort_replace = overwrite
        && target_capabilities
            .as_ref()
            .is_some_and(|capabilities| !capabilities.replace && capabilities.replace_best_effort);
    if best_effort_replace {
        validate_best_effort_file_replacements(
            source_vfs.as_ref(),
            target_vfs.as_ref(),
            &destination,
            &item_paths,
        )?;
    }

    let target_copy_new_best_effort = target_capabilities
        .as_ref()
        .is_some_and(|capabilities| !capabilities.copy_new && capabilities.copy_new_best_effort);
    let target_replace_best_effort = overwrite
        && target_capabilities
            .as_ref()
            .is_some_and(|capabilities| !capabilities.replace && capabilities.replace_best_effort);
    let audit_context = ProviderAuditContext {
        source_provider: Some(source.kind()),
        target_provider: target.kind(),
        execution_class: if source.kind() == crate::ProviderKind::Sftp {
            ProviderAuditExecutionClass::BestEffort
        } else {
            provider_file_audit_class(
                target_capabilities.as_ref(),
                overwrite,
                source_removal_best_effort,
            )
        },
        source_removal_best_effort_expected: source_removal_best_effort,
        target_copy_new_best_effort_expected: target_copy_new_best_effort,
        target_replace_best_effort_expected: target_replace_best_effort,
        source_removal_best_effort_actual: None,
        target_copy_new_best_effort_actual: None,
        target_replace_best_effort_actual: None,
    };
    queue.mark_running(&operation_id)?;
    let audit_operation = match kind {
        FileQueueKind::Copy => "provider_copy",
        FileQueueKind::Move => "provider_move",
    };
    queue.append_file_audit(
        &operation_id,
        audit_operation,
        "started",
        &Progress::default(),
        &audit_context,
        None,
    )?;
    if cancel.is_cancelled() {
        let progress = Progress {
            phase: "cancelled".into(),
            ..Progress::default()
        };
        queue.append_file_audit(
            &operation_id,
            audit_operation,
            "cancelled",
            &progress,
            &audit_context,
            None,
        )?;
        queue.cancel(&operation_id, progress)?;
        return Ok(QueueExecutionResult::File(OpStats {
            cancelled: true,
            ..OpStats::default()
        }));
    }
    let replacement_run =
        if overwrite && !target_is_local && !best_effort_replace && !current_conflicts.is_empty() {
            match quarantine_provider_file_conflicts(
                target_vfs.as_ref(),
                &target_root,
                &current_conflicts,
                &expected_conflict_proofs,
                cancel,
                recovery,
            ) {
                Ok(run) => Some(run),
                Err(error) => {
                    let failed_audit_context = audit_context_with_actuals(audit_context, actuals);
                    let _ = queue.append_file_audit(
                        &operation_id,
                        audit_operation,
                        "failed",
                        &Progress::default(),
                        &failed_audit_context,
                        recovery.as_ref(),
                    );
                    return Err(error);
                }
            }
        } else {
            None
        };
    let policy = if overwrite && (target_is_local || best_effort_replace) {
        OverwritePolicy::Overwrite
    } else {
        // Abgesicherte Remote-Ersetzungen sind vorab manifestiert aus dem Zielnamen
        // geparkt. `Skip` erlaubt danach das Zusammenführen vorhandener Ordner,
        // ersetzt aber keine spät auftauchende, unbestätigte Kollision. Der getrennt
        // bestätigte Best-Effort-Weg geht dagegen mit `Overwrite` in die einzelne
        // Dateioperation; dort erfolgen frischer Zielcheck und genau eine Mutation.
        OverwritePolicy::Skip
    };
    if replacement_run.is_some() && cancel.is_cancelled() {
        let error = provider_cancel_after_parking_error(recovery.as_ref());
        let failed_audit_context = audit_context_with_actuals(audit_context, actuals);
        let _ = queue.append_file_audit(
            &operation_id,
            audit_operation,
            "failed",
            &Progress::default(),
            &failed_audit_context,
            recovery.as_ref(),
        );
        return Err(error);
    }
    let mut queue_progress = QueueProgress::new(queue, &operation_id, cancel, on_progress);
    let result = {
        let mut callback = |progress: &Progress| {
            queue_progress.report(progress);
        };
        let request = FileOperationRequest {
            src: source_vfs.as_ref(),
            dst: target_vfs.as_ref(),
            items: &item_paths,
            dst_dir: &destination,
            policy,
            sandbox: &execution.sandbox,
        };
        if skip_only {
            // Der frische Konfliktvergleich ist abgeschlossen. Dieser terminale
            // Weg ruft keine VFS-Mutation auf, auch wenn ein Ziel danach verschwindet.
            Ok(skipped_stats)
        } else {
            match kind {
                FileQueueKind::Copy => crate::fileops::copy_items_with_targets_tracked(
                    request,
                    crate::fileops::FileTransferBindings {
                        targets: &keep_both_targets,
                        replacements: Some(&expected_conflict_proofs),
                        skipped_targets: (!overwrite && keep_both_targets.is_empty())
                            .then_some(current_conflicts.as_slice()),
                    },
                    cancel,
                    &mut callback,
                    actuals,
                ),
                FileQueueKind::Move => crate::fileops::move_items_with_targets_tracked(
                    request,
                    crate::fileops::FileTransferBindings {
                        targets: &keep_both_targets,
                        replacements: Some(&expected_conflict_proofs),
                        skipped_targets: (!overwrite && keep_both_targets.is_empty())
                            .then_some(current_conflicts.as_slice()),
                    },
                    same_namespace_move && source.kind() == crate::ProviderKind::Ftps,
                    cancel,
                    &mut callback,
                    actuals,
                ),
            }
        }
    };
    let (latest, mut persistence_error) = queue_progress.finish();
    // Nur der nichtterminale Fehlerweg erzwingt einen letzten Zwischenstand. Bei
    // Erfolg schreibt der anschließende Queueabschluss die endgültigen Zähler direkt.
    if result.is_err() && !file_progress_is_terminal(&latest) {
        flush_latest_file_progress(queue, &operation_id, &latest, &mut persistence_error);
    }
    if let Some(error) = persistence_error {
        let failed_audit_context = audit_context_with_actuals(audit_context, actuals);
        let _ = queue.append_file_audit_with_temp_paths(
            &operation_id,
            audit_operation,
            "failed",
            &latest,
            &failed_audit_context,
            FileAuditRecovery {
                provider: recovery.as_ref(),
                temp_paths: &actuals.temp_recovery_paths,
                retained_backups: &actuals.retained_backup_paths,
            },
        );
        return Err(possible_mutation_progress_error(error, &result));
    }
    let mut stats = match result {
        Ok(stats) => stats,
        Err(error) => {
            let failed_audit_context = audit_context_with_actuals(audit_context, actuals);
            let _ = queue.append_file_audit_with_temp_paths(
                &operation_id,
                audit_operation,
                "failed",
                &latest,
                &failed_audit_context,
                FileAuditRecovery {
                    provider: recovery.as_ref(),
                    temp_paths: &actuals.temp_recovery_paths,
                    retained_backups: &actuals.retained_backup_paths,
                },
            );
            let error = if target_vfs.additive_content_target_only() {
                crate::error::with_recovery_context(error, &[], &[&destination])
            } else {
                error
            };
            return Err(error_with_provider_file_recovery(error, recovery.as_ref()));
        }
    };
    stats
        .source_read_limitations
        .extend(source.interactive_file_read_limitations());
    if let Some(tree) = &stats.source_tree {
        stats.source_read_limitations.extend(tree.limitations());
    }
    if stats.cancelled && replacement_run.is_some() {
        let error = provider_cancel_after_parking_error(recovery.as_ref());
        let failed_audit_context = audit_context_with_actuals(audit_context, actuals);
        let _ = queue.append_file_audit_with_temp_paths(
            &operation_id,
            audit_operation,
            "failed",
            &latest,
            &failed_audit_context,
            FileAuditRecovery {
                provider: recovery.as_ref(),
                temp_paths: &actuals.temp_recovery_paths,
                retained_backups: &actuals.retained_backup_paths,
            },
        );
        return Err(error);
    }
    if let Some(run) = replacement_run.as_ref() {
        if let Err(failure) =
            complete_quarantine_run_if_matches(target_vfs.as_ref(), &run.root, run.manifest_digest)
        {
            let error = match failure {
                MutationFailure::Unchanged(error) => crate::error::coded_error(
                    error.kind(),
                    crate::error::ErrorCode::PlanDrift,
                    format!(
                        "Der Provider-Quarantänelauf konnte wegen eines geänderten Manifests nicht abgeschlossen werden: {error}"
                    ),
                ),
                MutationFailure::OutcomeUnclear(error) => crate::error::coded_error(
                    error.kind(),
                    crate::error::ErrorCode::OutcomeUnclear,
                    format!(
                        "Der inhaltsgebundene Abschluss des Provider-Quarantänelaufs hat einen unklaren Ausgang: {error}"
                    ),
                ),
            };
            let failed_audit_context = audit_context_with_actuals(audit_context, actuals);
            let _ = queue.append_file_audit_with_temp_paths(
                &operation_id,
                audit_operation,
                "failed",
                &latest,
                &failed_audit_context,
                FileAuditRecovery {
                    provider: recovery.as_ref(),
                    temp_paths: &actuals.temp_recovery_paths,
                    retained_backups: &actuals.retained_backup_paths,
                },
            );
            return Err(error_with_provider_file_recovery(error, recovery.as_ref()));
        }
    }
    if !stats.cancelled && stats.temp_recovery_paths.is_empty() {
        source.finish_ftps_intents()?;
        target.finish_ftps_intents()?;
    } else {
        for path in staging_recovery_paths(queue, &operation_id)? {
            if !stats.temp_recovery_paths.contains(&path) {
                stats.temp_recovery_paths.push(path.clone());
            }
            if !actuals.temp_recovery_paths.contains(&path) {
                actuals.temp_recovery_paths.push(path);
            }
        }
    }
    if stats.cancelled
        && (target.kind() == crate::ProviderKind::Ftps
            || (kind == FileQueueKind::Move && source.kind() == crate::ProviderKind::Ftps))
        && !stats.temp_recovery_paths.is_empty()
    {
        return Err(crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::Interrupted, crate::ErrorCode::OutcomeUnclear,
            "FTPS-Abbruch erhält mögliche Teilwirkungen; Ziel und STOU-Reste manuell prüfen, nicht wiederholen"), &stats.temp_recovery_paths));
    }
    complete_file_queue_entry(
        queue,
        &operation_id,
        latest,
        stats,
        audit_operation,
        audit_context,
        pending_completion,
    )
}

fn provider_file_has_ftps_endpoint(operation: &QueueOperation) -> bool {
    matches!(operation, QueueOperation::ProviderFile { source, target, .. }
        if matches!(source.as_ref(), BidirectionalQueueEndpoint::Ftps { .. })
            || matches!(target.as_ref(), BidirectionalQueueEndpoint::Ftps { .. }))
}

fn provider_file_preparation_not_cancelled(cancel: Option<&Cancel>) -> io::Result<()> {
    if cancel.is_some_and(Cancel::is_cancelled) {
        Err(crate::error::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Provider-Dateivorbereitung wurde abgebrochen",
        ))
    } else {
        Ok(())
    }
}

fn bind_connected_queue_endpoints(
    source: ProviderEndpoint,
    independently_connected_target: ProviderEndpoint,
    shares_connection_definition: bool,
    target_root: &Path,
) -> io::Result<(ProviderEndpoint, ProviderEndpoint, bool)> {
    // Nur `connect()` darf Credential-Attention auslösen. Diese getrennte Phase
    // bindet lebende Namespace- und Rootidentitäten; PermissionDenied und andere
    // Fehler bleiben deshalb unverändert und können keine SECRET-Schleife starten.
    let same_runtime_namespace =
        source.shares_runtime_namespace(&independently_connected_target)?;
    let shared_session = same_runtime_namespace
        && shares_connection_definition
        && source.shares_connection_mapping(&independently_connected_target);
    let target = if shared_session {
        source.with_root(target_root)?
    } else {
        independently_connected_target
    };
    Ok((source, target, shared_session))
}

struct LiveProviderQuarantineRun {
    root: PathBuf,
    manifest_digest: [u8; 32],
    _lease: crate::quarantine::QuarantineLease,
}

impl std::fmt::Debug for LiveProviderQuarantineRun {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveProviderQuarantineRun")
            .field("root", &self.root)
            .finish()
    }
}

impl std::ops::Deref for LiveProviderQuarantineRun {
    type Target = PathBuf;

    fn deref(&self) -> &Self::Target {
        &self.root
    }
}

fn quarantine_provider_file_conflicts(
    vfs: &dyn crate::Vfs,
    root: &Path,
    conflicts: &[String],
    expected_proofs: &[FileConflictProof],
    cancel: &Cancel,
    recovery: &mut Option<ProviderFileRecovery>,
) -> io::Result<LiveProviderQuarantineRun> {
    if !vfs.supports_create_dir_with_removal_proof()
        || !vfs.supports_rename_noreplace_if_matches_in_bound_parents()
        || !vfs.supports_replace_file_if_content_matches()
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Provider-Quarantäne benötigt atomar gebundene Ordneranlagen, ein bedingtes Rename mit beiden gebundenen Elternordnern und inhaltsgebundene Manifestwechsel",
        ));
    }
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Dateioperation wurde vor dem Anlegen der Provider-Quarantäne abgebrochen",
        ));
    }
    let mut current_proofs = capture_file_conflict_proofs(vfs, conflicts)?;
    let mut expected_proofs = expected_proofs.to_vec();
    current_proofs.sort_by(|left, right| left.path.cmp(&right.path));
    expected_proofs.sort_by(|left, right| left.path.cmp(&right.path));
    if current_proofs != expected_proofs
        || conflicts.len() != expected_proofs.len()
        || conflicts
            .iter()
            .any(|path| !expected_proofs.iter().any(|proof| proof.path == *path))
    {
        return Err(crate::error::coded_error(
            io::ErrorKind::AlreadyExists,
            crate::error::ErrorCode::PlanDrift,
            "Die bestätigten Provider-Konflikte änderten sich vor dem Anlegen der Quarantäne; kein Ziel wurde verschoben",
        ));
    }
    let mut unique_paths = HashSet::with_capacity(conflicts.len());
    let mut prepared = Vec::with_capacity(conflicts.len());
    for conflict in conflicts {
        if !unique_paths.insert(conflict.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Provider-Konfliktliste enthält einen Pfad mehrfach",
            ));
        }
        let source = PathBuf::from(conflict);
        crate::fileops::path_to_utf8(&source)?;
        let matches = expected_proofs
            .iter()
            .filter(|proof| proof.path == *conflict)
            .collect::<Vec<_>>();
        let [expected] = matches.as_slice() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Provider-Konflikt besitzt keinen genau zugeordneten Identitätsbeleg",
            ));
        };
        let relative = source
            .strip_prefix(root)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Provider-Konflikt verlässt den gebundenen Zielroot",
                )
            })?
            .to_path_buf();
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Provider-Konflikt besitzt keinen sicheren relativen Pfad",
            ));
        }
        if expected.is_symlink || (!expected.is_file && !expected.is_dir) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Symlink oder Spezialdatei darf nicht als Provider-Konflikt ersetzt werden",
            ));
        }
        let expected_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                crate::error::coded_error(
                    io::ErrorKind::InvalidData,
                    crate::error::ErrorCode::InvalidData,
                    "Provider-Konfliktname ist nicht eindeutig als UTF-8 darstellbar",
                )
            })?;
        if expected.raw_name != expected_name.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Provider-Konfliktname und Identitätsbeleg gehören nicht zusammen",
            ));
        }
        let conditional_proof = crate::vfs::ProofListEntry {
            entry: crate::Entry {
                name: expected_name.to_owned(),
                is_dir: expected.is_dir,
                is_file: expected.is_file,
                is_symlink: expected.is_symlink,
                size: expected.size,
                mtime: expected.mtime,
                created: None,
                owner: None,
                group: None,
                extension: None,
                mode: expected.mode,
                device: expected.device,
                inode: expected.inode,
                hard_links: None,
            },
            raw_name: expected.raw_name.clone(),
            safely_decoded: true,
            provider_token: expected.provider_token.clone(),
        };
        let source_parent = source.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Provider-Konflikt besitzt keinen Quell-Elternordner",
            )
        })?;
        let source_parent_identity = vfs.root_identity(source_parent)?;
        prepared.push((source, relative, conditional_proof, source_parent_identity));
    }

    // Hashen und vollständige Listings können lange dauern. Ein inzwischen
    // gesetzter Abbruch muss deshalb genau an dieser letzten mutationsfreien Grenze
    // erneut wirken. Ab dem folgenden `ensure_quarantine_root` wäre bereits ein
    // sichtbarer Providername entstanden und nur noch Recovery ehrlich.
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Dateioperation wurde nach der Konfliktprüfung vor dem Anlegen der Provider-Quarantäne abgebrochen",
        ));
    }

    // Erst nachdem die gesamte Menge geprüft ist, darf der erste Quarantänename
    // entstehen. Ein später ungültiger Pfad darf keine frühen Probe- oder Laufreste
    // und erst recht keine bereits geparkten Vorgänger hinterlassen.
    let base = root.join(".doppeldecker-quarantine");
    // Die Namensreservierung ist rein lesend und gehört noch vor den ersten
    // Recoveryzustand. Erst danach kann die Basisanlage sichtbar werden.
    let run = reserve_quarantine_run_name(vfs, &base)?;
    let base_proof = ensure_provider_quarantine_base(vfs, root, &base, recovery)?;
    let base_recovery = recovery.clone();
    *recovery = Some(ProviderFileRecovery {
        quarantine_run: crate::fileops::path_to_utf8(&run)?,
        parked_entries: 0,
    });
    let run_proof = match vfs.create_dir_with_removal_proof(&run) {
        Ok(Some(proof)) => proof_for_path(proof, &run)?,
        Ok(None) => {
            return Err(crate::error::coded_error(
                io::ErrorKind::Other,
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "Der Provider konnte den neuen Quarantänelauf nicht atomar an seine Identität binden. Er bleibt unangetastet. Recoverypfad: {}",
                    run.display()
                ),
            ));
        }
        Err(MutationFailure::Unchanged(error)) => {
            // Der Provider garantiert für diese Fehlerklasse, dass kein Laufname
            // entstand. Der Lauf-Recoveryzustand verschwindet deshalb. Wurde die
            // Basis unmittelbar davor sichtbar neu angelegt, bleibt aber genau
            // dieser getrennte Basiszustand erhalten.
            *recovery = base_recovery;
            return Err(error);
        }
        Err(MutationFailure::OutcomeUnclear(error)) => {
            // Hier kann der Laufname bereits sichtbar sein. Ohne gebundenen
            // Aufräumvertrag bleibt er stehen und der zuvor gesetzte Recoverypfad
            // muss bis in Queue, Audit und Oberfläche erhalten bleiben.
            return Err(crate::error::coded_error(
                error.kind(),
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "Das Anlegen des Provider-Quarantänelaufs meldete einen Fehler nach möglicher Wirkung ({error}). Der mögliche Lauf blieb unangetastet. Recoverypfad: {}",
                    run.display()
                ),
            ));
        }
    };
    vfs.sync_dir(&base).map_err(|sync_error| {
        crate::error::coded_error(
            sync_error.kind(),
            crate::error::ErrorCode::OutcomeUnclear,
            format!(
                "Der Quarantänelauf wurde sichtbar angelegt, aber sein Elternordner konnte nicht bestätigt werden ({sync_error}). Recoverypfad: {}",
                run.display()
            ),
        )
    })?;
    let run_proof = require_same_bound_directory(vfs, &base, &run, &run_proof, "Quarantänelauf")?;
    let run_identity = vfs.root_identity(&run).map_err(|error| {
        crate::error::coded_error(
            io::ErrorKind::Other,
            crate::error::ErrorCode::OutcomeUnclear,
            format!(
                "Der gebundene Quarantänelauf verlor vor dem Manifest seine Identität ({error}). Recoverypfad: {}",
                run.display()
            ),
        )
    })?;
    let run_proof =
        require_same_bound_directory(vfs, &base, &run, &run_proof, "Quarantänelauf vor Manifest")?;
    // Der Basisbeleg bleibt absichtlich bis zur gebundenen Lauf-Rückleseprüfung
    // erhalten. Wird die Basis ausgetauscht, kann derselbe Laufbeleg am bestätigten
    // Pfad nicht erneut erscheinen.
    require_same_bound_directory(vfs, root, &base, &base_proof, "Quarantänebasis")?;
    let started =
        crate::quarantine::start_quarantine_run_with_proof(vfs, &run, root, &run_identity)?;
    let lease = started.lease;
    let mut manifest_digest = started.payload_digest;
    for (source, relative, conditional_proof, source_parent_identity) in prepared {
        let parked = run.join(&relative);
        let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let (parked_parent, _parked_parent_proof) =
            ensure_bound_park_parent(vfs, &run, &run_proof, relative_parent, recovery.as_ref())?;
        let parked_parent_identity = vfs.root_identity(&parked_parent).map_err(|error| {
            crate::error::coded_error(
                error.kind(),
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "Der Quarantäne-Elternordner konnte vor dem gebundenen Park-Rename nicht identifiziert werden ({error}). Recoverypfad: {}",
                    run.display()
                ),
            )
        })?;
        let intent = record_quarantine_intent_with_proof(
            vfs,
            &run,
            &relative,
            conditional_proof.entry.is_dir,
            QuarantineReason::Replaced,
            manifest_digest,
        )
        .map_err(|failure| match failure {
            MutationFailure::Unchanged(error) => crate::error::coded_error(
                error.kind(),
                crate::error::ErrorCode::PlanDrift,
                format!(
                    "Das Quarantänemanifest änderte sich vor dem nächsten Provider-Claim; dieser Konflikt wurde nicht verschoben ({error}). Recoverypfad: {}",
                    run.display()
                ),
            ),
            MutationFailure::OutcomeUnclear(error) => crate::error::coded_error(
                error.kind(),
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "Der inhaltsgebundene Manifestwechsel hat einen unklaren Ausgang ({error}). Recoverypfad: {}",
                    run.display()
                ),
            ),
        })?;
        manifest_digest = intent.payload_digest;
        let source_parent = source.parent().expect("vor der Mutation validiert");
        if let Err(rename_error) =
            vfs.rename_noreplace_if_matches_in_bound_parents(BoundParentsRenameRequest {
                from: &source,
                to: &parked,
                expected: &conditional_proof,
                from_parent: source_parent,
                expected_from_parent: &source_parent_identity,
                to_parent: &parked_parent,
                expected_to_parent: &parked_parent_identity,
                required_content: Some(BoundContentDigest {
                    path: &intent.manifest_path,
                    digest: intent.payload_digest,
                }),
            })
        {
            return Err(crate::error::coded_error(
                io::ErrorKind::Other,
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "Der Provider-Konflikt oder einer seiner beiden Elternordner wurde unmittelbar vor dem gebundenen Park-Rename ausgetauscht, oder der Provider kann die Identitäten nicht bis zur Wirkung halten ({rename_error}). Der aktuelle Konfliktname blieb unangetastet; zuvor geparkte Einträge können bereits im Lauf liegen. Recoverypfade: aktueller Zielpfad: {}; Quarantänelauf: {}",
                    source.display(),
                    run.display(),
                ),
            ));
        }
        if let Some(state) = recovery.as_mut() {
            state.parked_entries = state.parked_entries.saturating_add(1);
        }
        if let Some(parent) = source.parent() {
            vfs.sync_dir(parent).map_err(|sync_error| {
                crate::error::coded_error(
                    io::ErrorKind::Other,
                    crate::error::ErrorCode::OutcomeUnclear,
                    format!(
                        "Der bestätigte Provider-Konflikt wurde nach {} geparkt, aber der Ziel-Elternordner konnte nicht synchronisiert werden ({sync_error}). Recoverypfade: früherer Zielpfad: {}; geparkter Eintrag: {}",
                        parked.display(),
                        source.display(),
                        parked.display(),
                    ),
                )
            })?;
        }
        if let Some(parent) = parked.parent() {
            vfs.sync_dir(parent).map_err(|sync_error| {
                crate::error::coded_error(
                    io::ErrorKind::Other,
                    crate::error::ErrorCode::OutcomeUnclear,
                    format!(
                        "Der bestätigte Provider-Konflikt wurde nach {} geparkt, aber der Quarantäne-Elternordner konnte nicht synchronisiert werden ({sync_error}). Recoverypfade: früherer Zielpfad: {}; geparkter Eintrag: {}",
                        parked.display(),
                        source.display(),
                        parked.display(),
                    ),
                )
            })?;
        }
    }
    Ok(LiveProviderQuarantineRun {
        root: run,
        manifest_digest,
        _lease: lease,
    })
}

fn ensure_provider_quarantine_base(
    vfs: &dyn Vfs,
    parent: &Path,
    base: &Path,
    recovery: &mut Option<ProviderFileRecovery>,
) -> io::Result<crate::vfs::ProofListEntry> {
    if let Some(proof) = listed_bound_directory(vfs, parent, base, "Quarantänebasis")? {
        *recovery = None;
        return Ok(proof);
    }

    *recovery = Some(ProviderFileRecovery {
        quarantine_run: crate::fileops::path_to_utf8(base)?,
        parked_entries: 0,
    });
    let proof = match vfs.create_dir_with_removal_proof(base) {
        Ok(Some(proof)) => proof_for_path(proof, base)?,
        Ok(None) => {
            return Err(crate::error::coded_error(
                io::ErrorKind::Other,
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "Der Provider konnte die neue Quarantänebasis nicht atomar an ihre Identität binden. Sie bleibt unangetastet. Recoverypfad: {}",
                    base.display()
                ),
            ));
        }
        Err(MutationFailure::Unchanged(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            *recovery = None;
            return listed_bound_directory_with_case_alias(
                vfs,
                parent,
                base,
                "Quarantänebasis",
                true,
            )?
            .ok_or(error);
        }
        Err(MutationFailure::Unchanged(error)) => {
            // Nur diese Providerklasse garantiert, dass der Basisname nicht
            // entstand. Darum darf ausschließlich hier der vorbereitete
            // Recoverydatensatz wieder verschwinden.
            *recovery = None;
            return Err(error);
        }
        Err(MutationFailure::OutcomeUnclear(error)) => {
            return Err(crate::error::coded_error(
                error.kind(),
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "Das Anlegen der Provider-Quarantänebasis meldete einen Fehler nach möglicher Wirkung ({error}). Der mögliche Ordner blieb unangetastet. Recoverypfad: {}",
                    base.display()
                ),
            ));
        }
    };
    vfs.sync_dir(parent).map_err(|sync_error| {
        crate::error::coded_error(
            sync_error.kind(),
            crate::error::ErrorCode::OutcomeUnclear,
            format!(
                "Die Provider-Quarantänebasis wurde unter {} sichtbar angelegt, aber ihr Elternordner konnte nicht bestätigt werden ({sync_error}). Recoverypfad: {}",
                base.display(),
                base.display()
            ),
        )
    })?;
    require_same_bound_directory(vfs, parent, base, &proof, "Quarantänebasis")
}

fn proof_for_path(
    mut proof: crate::vfs::ProofListEntry,
    path: &Path,
) -> io::Result<crate::vfs::ProofListEntry> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            crate::error::coded_error(
                io::ErrorKind::InvalidData,
                crate::error::ErrorCode::InvalidData,
                "Provider-Beleg besitzt keinen sicher dekodierbaren Namen",
            )
        })?;
    proof.entry.name = name.to_owned();
    proof.raw_name = name.as_bytes().to_vec();
    proof.safely_decoded = true;
    Ok(proof)
}

fn listed_bound_directory(
    vfs: &dyn Vfs,
    parent: &Path,
    path: &Path,
    role: &str,
) -> io::Result<Option<crate::vfs::ProofListEntry>> {
    listed_bound_directory_with_case_alias(vfs, parent, path, role, false)
}

fn listed_bound_directory_with_case_alias(
    vfs: &dyn Vfs,
    parent: &Path,
    path: &Path,
    role: &str,
    allow_case_alias: bool,
) -> io::Result<Option<crate::vfs::ProofListEntry>> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{role} besitzt keinen sicheren Namen"),
            )
        })?;
    let listing = vfs.list_complete(parent)?;
    if !listing.complete || listing.entries.iter().any(|entry| !entry.safely_decoded) {
        return Err(crate::error::coded_error(
            io::ErrorKind::InvalidData,
            crate::error::ErrorCode::InvalidData,
            format!("{role} benötigt ein vollständiges, sicher dekodiertes Elternlisting"),
        ));
    }
    // Exakte Namen haben Vorrang, auch wenn das Dateisystem `docs` und `Docs`
    // nebeneinander hält. Eine andere Schreibweise wird erst nach EEXIST bei der
    // tatsächlichen Anlage oder beim Readback eines bereits gebundenen Ordners
    // berücksichtigt; eine angenommene Provider-/Mountklasse genügt dafür nicht.
    let has_exact = listing
        .entries
        .iter()
        .any(|entry| entry.raw_name == name.as_bytes());
    let mut matches = listing.entries.into_iter().filter(|entry| {
        entry.raw_name == name.as_bytes()
            || (allow_case_alias && !has_exact && entry.entry.name.eq_ignore_ascii_case(name))
    });
    let Some(found) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || !found.entry.is_dir || found.entry.is_symlink {
        return Err(crate::error::coded_error(
            io::ErrorKind::InvalidData,
            crate::error::ErrorCode::InvalidData,
            format!("{role} ist kein eindeutiger, symlinkfreier Ordner"),
        ));
    }
    Ok(Some(found))
}

fn same_bound_directory(
    expected: &crate::vfs::ProofListEntry,
    current: &crate::vfs::ProofListEntry,
) -> bool {
    let stable_identity = match (&expected.provider_token, &current.provider_token) {
        (Some(expected), Some(current)) => expected == current,
        _ => match (
            expected.entry.device,
            expected.entry.inode,
            current.entry.device,
            current.entry.inode,
        ) {
            (Some(expected_device), Some(expected_inode), Some(device), Some(inode)) => {
                expected_device == device && expected_inode == inode
            }
            _ => false,
        },
    };
    stable_identity
        && expected.raw_name == current.raw_name
        && expected.entry.is_dir == current.entry.is_dir
        && expected.entry.is_symlink == current.entry.is_symlink
}

fn require_same_bound_directory(
    vfs: &dyn Vfs,
    parent: &Path,
    path: &Path,
    expected: &crate::vfs::ProofListEntry,
    role: &str,
) -> io::Result<crate::vfs::ProofListEntry> {
    let current = listed_bound_directory_with_case_alias(vfs, parent, path, role, true)?
        .ok_or_else(|| {
            crate::error::coded_error(
                io::ErrorKind::Other,
                crate::error::ErrorCode::OutcomeUnclear,
                format!(
                    "{role} fehlt nach sichtbarer Anlage. Recoverypfad: {}",
                    path.display()
                ),
            )
        })?;
    if !same_bound_directory(expected, &current) {
        return Err(crate::error::coded_error(
            io::ErrorKind::Other,
            crate::error::ErrorCode::OutcomeUnclear,
            format!(
                "{role} wurde nach seiner Bindung ausgetauscht. Recoverypfad: {}",
                path.display()
            ),
        ));
    }
    Ok(current)
}

fn ensure_bound_park_parent(
    vfs: &dyn Vfs,
    run: &Path,
    run_proof: &crate::vfs::ProofListEntry,
    relative_parent: &Path,
    recovery: Option<&ProviderFileRecovery>,
) -> io::Result<(PathBuf, crate::vfs::ProofListEntry)> {
    let mut parent = run.to_path_buf();
    let mut parent_proof = run_proof.clone();
    for component in relative_parent.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quarantäne-Unterordner besitzt keine normalisierte relative Form",
            ));
        };
        let child = parent.join(name);
        let child_proof = if let Some(proof) =
            listed_bound_directory(vfs, &parent, &child, "Quarantäne-Park-Elternordner")?
        {
            proof
        } else {
            match vfs.create_dir_with_removal_proof(&child) {
                Ok(Some(proof)) => {
                    let proof = proof_for_path(proof, &child)?;
                    vfs.sync_dir(&parent).map_err(|error| {
                        crate::error::coded_error(
                            error.kind(),
                            crate::error::ErrorCode::OutcomeUnclear,
                            format!(
                                "Der Park-Elternordner wurde sichtbar angelegt, aber nicht bestätigt ({error}). Recoverypfade: {}; {}",
                                recovery.map_or("<unbekannter Lauf>", |state| state.quarantine_run.as_str()),
                                child.display()
                            ),
                        )
                    })?;
                    require_same_bound_directory(
                        vfs,
                        &parent,
                        &child,
                        &proof,
                        "Quarantäne-Park-Elternordner",
                    )?
                }
                Ok(None) => {
                    return Err(crate::error::coded_error(
                        io::ErrorKind::Other,
                        crate::error::ErrorCode::OutcomeUnclear,
                        format!(
                            "Der neue Park-Elternordner besitzt keinen atomar gebundenen Identitätsbeleg. Recoverypfade: {}; {}",
                            recovery.map_or("<unbekannter Lauf>", |state| state.quarantine_run.as_str()),
                            child.display()
                        ),
                    ));
                }
                Err(MutationFailure::Unchanged(error))
                    if error.kind() == io::ErrorKind::AlreadyExists =>
                {
                    listed_bound_directory_with_case_alias(
                        vfs,
                        &parent,
                        &child,
                        "Quarantäne-Park-Elternordner",
                        true,
                    )?
                    .ok_or(error)?
                }
                Err(MutationFailure::Unchanged(error)) => return Err(error),
                Err(MutationFailure::OutcomeUnclear(error)) => {
                    return Err(crate::error::coded_error(
                        error.kind(),
                        crate::error::ErrorCode::OutcomeUnclear,
                        format!(
                            "Die Anlage des Park-Elternordners hat einen unklaren Ausgang ({error}). Recoverypfade: {}; {}",
                            recovery.map_or("<unbekannter Lauf>", |state| state.quarantine_run.as_str()),
                            child.display()
                        ),
                    ));
                }
            }
        };
        // Der Beleg des bisherigen Elternordners bleibt bis zum eindeutigen
        // Kind-Readback erhalten. Ein Austausch eines höheren Hierarchieteils lässt
        // das gebundene Kind am bestätigten Pfad verschwinden oder seine ID ändern.
        require_same_bound_directory(
            vfs,
            parent.parent().unwrap_or(run),
            &parent,
            &parent_proof,
            "Quarantäne-Hierarchie",
        )?;
        parent = child;
        parent_proof = child_proof;
    }
    Ok((parent, parent_proof))
}

fn error_with_provider_file_recovery(
    error: io::Error,
    recovery: Option<&ProviderFileRecovery>,
) -> io::Error {
    let Some(recovery) = recovery else {
        return error;
    };
    let mut paths = crate::error::recovery_paths(&error);
    paths.push(recovery.quarantine_run.clone());
    let wrapped = crate::error::coded_error(
        error.kind(),
        crate::error_code(&error),
        format!(
            "{error}; Recoverypfad: {} ({} geparkte Einträge)",
            recovery.quarantine_run, recovery.parked_entries
        ),
    );
    crate::error::with_recovery_paths(wrapped, &paths)
}

fn queue_progress_persistence_error(error: io::Error) -> io::Error {
    let paths = crate::error::recovery_paths(&error);
    let wrapped = crate::error::coded_error(
        error.kind(),
        crate::error_code(&error),
        format!("Queue-Fortschritt konnte nicht gesichert werden: {error}"),
    );
    crate::error::with_recovery_paths(wrapped, &paths)
}

fn failed_queue_error_commit(
    execution_error: &io::Error,
    journal_error: &io::Error,
    context: &str,
) -> io::Error {
    let mut paths = crate::error::recovery_paths(execution_error);
    paths.extend(crate::error::recovery_paths(journal_error));
    crate::error::with_recovery_paths(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!("{execution_error}; auch der {context} scheiterte: {journal_error}"),
        ),
        &paths,
    )
}

/// Ein fehlgeschlagener Fortschrittscommit sagt nur etwas über das Journal aus.
/// Der Dateiexecutor kann zu diesem Zeitpunkt bereits Nutzerdaten verändert haben;
/// deshalb darf weder der konkrete Journalfehler noch ein erfolgreicher Abbruch
/// den öffentlichen Ausgang wiederholbar erscheinen lassen.
fn possible_mutation_progress_error<T>(
    persistence_error: io::Error,
    result: &io::Result<T>,
) -> io::Error {
    let mut paths = crate::error::recovery_paths(&persistence_error);
    let execution = match result {
        Ok(_) => "Ausführung meldete Erfolg oder kontrollierten Abbruch".to_string(),
        Err(error) => {
            paths.extend(crate::error::recovery_paths(error));
            error.to_string()
        }
    };
    let wrapped = crate::error::coded_error(
        io::ErrorKind::Other,
        crate::ErrorCode::OutcomeUnclear,
        format!(
            "Queue-Fortschritt konnte nicht gesichert werden: {persistence_error}; Ergebnis nach möglicher Mutation: {execution}"
        ),
    );
    crate::error::with_recovery_paths(wrapped, &paths)
}

fn complete_file_queue_entry(
    queue: &OperationQueue,
    operation_id: &str,
    mut latest: Progress,
    stats: OpStats,
    audit_operation: &'static str,
    audit_context: ProviderAuditContext,
    pending_completion: &mut Option<Box<PendingFileCompletion>>,
) -> io::Result<QueueExecutionResult> {
    latest.phase = if stats.cancelled { "cancelled" } else { "done" }.into();
    latest.files_done = stats.files;
    latest.bytes_done = stats.bytes;
    // Die Klasse bleibt der vor der Mutation bestätigte Vertrag. Diese drei
    // zusätzlichen Werte beschreiben die tatsächlich abgeschlossenen Wirkungen.
    let final_context = ProviderAuditContext {
        source_removal_best_effort_actual: Some(stats.source_removal_best_effort),
        target_copy_new_best_effort_actual: Some(stats.target_copy_new_best_effort),
        target_replace_best_effort_actual: Some(stats.target_replace_best_effort),
        ..audit_context
    };
    // Der Lauf ist vollständig vollzogen. Vor dem terminalen Journalcommit bleibt
    // sein Abschlussbeleg für einen reinen Commitretry erhalten. Erst nach Erfolg
    // wird er entfernt und das Audit geschrieben. Ein Auditfehler darf die
    // Dateiwirkung niemals wieder ausführbar machen.
    *pending_completion = Some(Box::new(PendingFileCompletion {
        cancelled: stats.cancelled,
        progress: latest,
        temp_recovery_paths: stats.temp_recovery_paths.clone(),
        audit_operation,
        audit_context: final_context,
        stats: stats.clone(),
    }));
    let completion = pending_completion
        .as_deref()
        .expect("unmittelbar zuvor angelegt")
        .clone();
    queue.retry_file_completion(operation_id, &completion)?;
    *pending_completion = None;
    queue.append_file_completion_audit(operation_id, &completion)?;
    Ok(QueueExecutionResult::File(stats))
}

// Jeder Executor hält seinen eigenen Rahmen an seiner Fensterqueue. Ein
// Journalfehler fordert Abbruch an, lässt den Executor aber seinen Cleanup- und
// Recoveryweg beenden; erst danach wird der erste Persistenzfehler ausgewertet.
struct QueueProgress<'a> {
    queue: &'a OperationQueue,
    operation_id: &'a str,
    cancel: &'a Cancel,
    on_progress: &'a mut dyn FnMut(&Progress),
    latest: Progress,
    persistence_error: Option<io::Error>,
    last_persist: Instant,
}

impl<'a> QueueProgress<'a> {
    fn new(
        queue: &'a OperationQueue,
        operation_id: &'a str,
        cancel: &'a Cancel,
        on_progress: &'a mut dyn FnMut(&Progress),
    ) -> Self {
        Self {
            queue,
            operation_id,
            cancel,
            on_progress,
            latest: Progress::default(),
            persistence_error: None,
            last_persist: Instant::now() - Duration::from_secs(1),
        }
    }

    fn report(&mut self, progress: &Progress) {
        self.latest = progress.clone();
        if should_persist_file_progress(progress, self.last_persist.elapsed()) {
            self.last_persist = Instant::now();
            if let Err(error) = self.queue.update_progress(self.operation_id, progress) {
                self.persistence_error.get_or_insert(error);
                self.cancel.cancel();
            }
        }
        (self.on_progress)(progress);
    }

    fn finish(self) -> (Progress, Option<io::Error>) {
        (self.latest, self.persistence_error)
    }
}

fn should_persist_file_progress(progress: &Progress, elapsed: Duration) -> bool {
    !file_progress_is_terminal(progress) && elapsed >= Duration::from_millis(250)
}

fn file_progress_is_terminal(progress: &Progress) -> bool {
    matches!(progress.phase.as_str(), "done" | "cancelled")
}

fn failed_progress(mut progress: Progress) -> Progress {
    progress.phase = "failed".into();
    progress.current.clear();
    progress
}

fn recovery_progress(mut progress: Progress) -> Progress {
    progress.phase = "recovery".into();
    progress.current.clear();
    progress
}

fn flush_latest_file_progress(
    queue: &OperationQueue,
    operation_id: &str,
    latest: &Progress,
    persistence_error: &mut Option<io::Error>,
) {
    if persistence_error.is_none() {
        if let Err(error) = queue.update_progress(operation_id, latest) {
            *persistence_error = Some(error);
        }
    }
}

fn provider_cancel_after_parking_error(recovery: Option<&ProviderFileRecovery>) -> io::Error {
    crate::error::coded_error(
        io::ErrorKind::Other,
        crate::ErrorCode::OutcomeUnclear,
        format!(
            "Der Abbruch traf nach dem Parken bestätigter Altziele ein. Ein sauberer Cancelled-Zustand würde die fehlenden Zielnamen verbergen; Recoverypfad: {} ({} geparkte Einträge)",
            recovery.map_or("<unbekannt>", |state| state.quarantine_run.as_str()),
            recovery.map_or(0, |state| state.parked_entries),
        ),
    )
}

/// Führt einen providerfähigen Lösch-Queue-Eintrag nach frischer Verbindung aus.
/// Der genaue Auswahlplan wird im Löschkern unmittelbar vor der ersten Mutation
/// erneut erzeugt. Ein Journal-Schreibfehler bricht über denselben Cancel-Kanal ab.
fn bind_provider_delete_around_probe<T>(
    vfs: &dyn crate::Vfs,
    root: &Path,
    expected_plan: &RemoteDeletePlan,
    probe: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let selected = expected_plan
        .items
        .iter()
        .map(|item| PathBuf::from(&item.path))
        .collect::<Vec<_>>();
    let verify = |phase: &str| -> io::Result<()> {
        let current = crate::plan_remote_delete(vfs, root, &selected)?;
        if !same_provider_delete_plan_across_session(expected_plan, &current) {
            return Err(crate::error::coded_error(
                io::ErrorKind::Other,
                crate::error::ErrorCode::PlanDrift,
                format!("Provider-Löschroot oder Auswahl änderten sich {phase}"),
            ));
        }
        Ok(())
    };
    verify("vor der Capability-Probe")?;
    let result = probe()?;
    verify("während der Capability-Probe")?;
    Ok(result)
}

/// Eine neue Providersitzung erhält eine neue flüchtige Generation (bei SMB auch
/// eine neue Tree-ID). Diese Sitzungsnummern dürfen den frisch identisch gelesenen
/// Plan nicht allein entwerten. Server-/Kontobindung, kanonischer Root und bei SMB
/// zusätzlich die beständige Root-Datei-ID bleiben dagegen zwingend gleich.
fn same_provider_delete_plan_across_session(
    expected: &RemoteDeletePlan,
    current: &RemoteDeletePlan,
) -> bool {
    use crate::RootIdentity;

    let same_root = match (&expected.root_identity, &current.root_identity) {
        (RootIdentity::Local(left), RootIdentity::Local(right)) => left == right,
        (RootIdentity::Sftp(left), RootIdentity::Sftp(right)) => {
            left.server == right.server && left.virtual_root == right.virtual_root
        }
        (RootIdentity::Ftps(left), RootIdentity::Ftps(right)) => {
            left.server == right.server && left.virtual_root == right.virtual_root
        }
        (RootIdentity::WebDav(left), RootIdentity::WebDav(right)) => {
            left.server == right.server
                && left.virtual_root == right.virtual_root
                && left.root_etag == right.root_etag
        }
        (RootIdentity::Smb(left), RootIdentity::Smb(right)) => {
            left.server == right.server
                && left.virtual_root == right.virtual_root
                && left.root_file_id == right.root_file_id
        }
        _ => false,
    };
    same_root
        && expected.root == current.root
        && expected.items == current.items
        && expected.entries == current.entries
        && expected.bytes == current.bytes
        && expected.digest == current.digest
}

pub fn execute_provider_delete_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    validate_operation(&entry.operation)?;
    let operation_id = entry.operation_id;
    let QueueOperation::ProviderDelete {
        endpoint,
        mode,
        expected_plan,
    } = entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider-Löschexecutor erhielt keinen ProviderDelete-Eintrag",
        ));
    };
    let Some(endpoint) =
        connect_queue_endpoint_with_credential_attention(queue, &operation_id, &endpoint, secrets)?
    else {
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::File,
            reason: "Provider-Zugangsdaten sind nicht verfügbar oder wurden abgelehnt".into(),
        });
    };
    let (vfs, root) = endpoint.file_context()?;
    if root != Path::new(&expected_plan.root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Provider-Löschroot änderte sich seit dem Enqueue",
        ));
    }
    let capabilities =
        bind_provider_delete_around_probe(vfs.as_ref(), &root, &expected_plan, || {
            endpoint.probe_delete_capabilities()
        })?;
    let permitted = match mode {
        RemoteDeleteMode::Quarantine => capabilities.quarantine,
        RemoteDeleteMode::Permanent => capabilities.permanent,
    };
    if !permitted {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            capabilities.blockers.join("; "),
        ));
    }
    let audit_context = ProviderAuditContext {
        source_provider: None,
        target_provider: endpoint.kind(),
        execution_class: provider_delete_audit_class(mode),
        source_removal_best_effort_expected: false,
        target_copy_new_best_effort_expected: false,
        target_replace_best_effort_expected: false,
        source_removal_best_effort_actual: None,
        target_copy_new_best_effort_actual: None,
        target_replace_best_effort_actual: None,
    };
    queue.mark_running(&operation_id)?;
    let audit_operation = match mode {
        RemoteDeleteMode::Quarantine => "provider_quarantine",
        RemoteDeleteMode::Permanent => "provider_permanent_delete",
    };
    queue.append_file_audit(
        &operation_id,
        audit_operation,
        "started",
        &Progress::default(),
        &audit_context,
        None,
    )?;
    let mut queue_progress = QueueProgress::new(queue, &operation_id, cancel, on_progress);
    let result = execute_remote_delete(
        vfs.as_ref(),
        &expected_plan,
        mode,
        cancel,
        &mut |progress| {
            queue_progress.report(progress);
        },
    );
    let (latest, mut persistence_error) = queue_progress.finish();
    if result.is_err() && !file_progress_is_terminal(&latest) {
        flush_latest_file_progress(queue, &operation_id, &latest, &mut persistence_error);
    }
    if let Some(error) = persistence_error {
        let _ = queue.append_file_audit(
            &operation_id,
            audit_operation,
            "failed",
            &latest,
            &audit_context,
            None,
        );
        return Err(possible_mutation_progress_error(error, &result));
    }
    let stats = match result {
        Ok(stats) => stats,
        Err(error) => {
            let _ = queue.append_file_audit(
                &operation_id,
                audit_operation,
                "failed",
                &latest,
                &audit_context,
                None,
            );
            return Err(error);
        }
    };
    finish_provider_delete_queue_entry(
        queue,
        &operation_id,
        audit_operation,
        latest,
        &audit_context,
        stats,
    )
}

fn finish_provider_delete_queue_entry(
    queue: &OperationQueue,
    operation_id: &str,
    audit_operation: &str,
    mut latest: Progress,
    audit_context: &ProviderAuditContext,
    stats: crate::RemoteDeleteStats,
) -> io::Result<QueueExecutionResult> {
    latest.phase = if stats.cancelled { "cancelled" } else { "done" }.into();
    latest.files_done = stats.entries;
    latest.bytes_done = stats.bytes;
    // Der Löschkern ist an dieser Stelle vollständig zurückgekehrt; seine Wirkung
    // darf deshalb nicht noch einmal ausführbar werden, nur weil der getrennte
    // Abschlussauditdatensatz scheitert. Erst Queue terminal sichern, dann Audit.
    if stats.cancelled {
        queue.cancel(operation_id, latest.clone())?;
    } else {
        queue.complete(operation_id, latest.clone())?;
    }
    queue.append_file_audit(
        operation_id,
        audit_operation,
        if stats.cancelled {
            "cancelled"
        } else {
            "completed"
        },
        &latest,
        audit_context,
        None,
    )?;
    Ok(QueueExecutionResult::File(OpStats {
        target_write_limitations: Vec::new(),
        source_read_limitations: Vec::new(),
        source_tree: None,
        files: stats.entries,
        bytes: stats.bytes,
        skipped: 0,
        symlinks_skipped: 0,
        cancelled: stats.cancelled,
        temp_recovery_paths: Vec::new(),
        retained_backup_paths: Vec::new(),
        source_removal_best_effort: false,
        target_copy_new_best_effort: false,
        target_replace_best_effort: false,
    }))
}

struct LocalSyncQueueRequest<'a> {
    queue: &'a OperationQueue,
    operation_id: &'a str,
    source: String,
    target: String,
    expected_sandbox: Option<String>,
    options: SyncOptions,
    expected_plan: SyncPlan,
}

fn execute_local_sync_entry(
    request: LocalSyncQueueRequest<'_>,
    execution: &LocalQueueExecution,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    let LocalSyncQueueRequest {
        queue,
        operation_id,
        source,
        target,
        expected_sandbox,
        options,
        expected_plan,
    } = request;
    let (source, target) = validate_local_sync_roots(Path::new(&source), Path::new(&target))?;
    let sandbox_text = execution
        .sandbox
        .as_ref()
        .map(|root| crate::fileops::path_to_utf8(root))
        .transpose()?;
    if sandbox_text != expected_sandbox {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Ausführungsmodus hat sich seit dem Enqueue geändert; Eintrag entfernen und neu einreihen",
        ));
    }
    if let Some(root) = &execution.sandbox {
        require_sandbox_child(root, &source, "Quelle")?;
        require_sandbox_child(root, &target, "Ziel")?;
    }
    let vfs = match execution.sandbox.as_deref() {
        Some(root) => LocalVfs::rooted(root)?,
        None => LocalVfs::new(),
    };
    let vfs = vfs.with_staging_owner(&queue.path, operation_id);
    let mut safety = match &execution.sandbox {
        Some(root) => Safety::test_sandbox(root.clone(), &vfs)?,
        None => Safety::default(),
    };
    safety.quarantine = execution.sync_quarantine.clone();
    safety.max_delete_entries = Some(execution.max_delete_entries);
    safety.max_delete_fraction = Some(execution.max_delete_fraction);
    safety.allow_empty_source_mirror = false;
    let current_plan = crate::plan(&vfs, &source, &vfs, &target, &options)?;
    if current_plan != expected_plan {
        let reason = "Quelle oder Ziel hat sich seit der Queue-Bestätigung geändert".to_string();
        queue.require_confirmation(
            operation_id,
            QueueAttention::PlanChanged {
                reason: reason.clone(),
                confirmation: format!(
                    "PLAN {} {} {} {}",
                    short_id(operation_id),
                    current_plan.copy_count,
                    current_plan.update_count,
                    current_plan.delete_count
                ),
                current_plan: Box::new(current_plan),
            },
        )?;
        return Ok(QueueExecutionResult::AwaitingConfirmation {
            class: QueueOperationClass::Sync,
            reason,
        });
    }
    queue.mark_running(operation_id)?;
    let mut queue_progress = QueueProgress::new(queue, operation_id, cancel, on_progress);
    let result = crate::execute(
        crate::ExecuteRequest {
            src: &vfs,
            src_root: &source,
            dst: &vfs,
            dst_root: &target,
            plan: &expected_plan,
            safety: &safety,
        },
        cancel,
        &mut |progress| {
            queue_progress.report(progress);
        },
    );
    let (mut latest, mut persistence_error) = queue_progress.finish();
    // Bei einem erfolgreichen Lauf gehören Endphase und Endzähler in denselben
    // atomischen Queuecommit. Ein getrenntes `update_progress("done")` könnte
    // scheitern, obwohl die Datenwirkung bereits vollständig abgeschlossen ist.
    // Nur ein echter Ausführungsfehler braucht den letzten Zwischenstand vor dem
    // anschließenden Fehlerzustand noch separat.
    if result.is_err() {
        flush_latest_file_progress(queue, operation_id, &latest, &mut persistence_error);
    }
    if let Some(error) = persistence_error {
        let failure = possible_mutation_progress_error(error, &result);
        return Err(match &result {
            Ok(stats) => crate::error::with_recovery_paths(failure, &stats.temp_recovery_paths),
            Err(_) => failure,
        });
    }
    let stats = result?;
    latest.phase = if stats.cancelled { "cancelled" } else { "done" }.into();
    queue.update(operation_id, |entry| {
        require_state(entry, QueueState::Running)?;
        entry.state = if stats.cancelled {
            QueueState::Cancelled
        } else {
            QueueState::Completed
        };
        entry.progress = latest;
        entry.error = None;
        entry.error_code = None;
        entry.attention = None;
        entry.recovery_available = false;
        entry.bidirectional_completion_recovery = None;
        entry.temp_recovery_paths.extend(
            stats
                .temp_recovery_paths
                .iter()
                .filter(|path| !entry.temp_recovery_paths.contains(path))
                .cloned()
                .collect::<Vec<_>>(),
        );
        Ok(())
    })?;
    Ok(QueueExecutionResult::Sync(stats))
}

fn require_sandbox_child(root: &Path, path: &Path, role: &str) -> io::Result<()> {
    if path == root || !path.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{role} „{}“ muss ein Unterordner der Test-Sandbox „{}“ sein",
                path.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

/// Kurzform einer Operations-ID für Bestätigungstexte.
///
/// Selbst erzeugte IDs sind 32 Hexziffern lang, also reines ASCII. Ein von außen
/// verändertes oder beschädigtes Journal kann aber jede nicht leere Zeichenkette
/// mitbringen — das Laden prüft nur auf „nicht leer“ und „eindeutig“. Ein Schnitt
/// nach acht *Bytes* würde dort mitten in ein Mehrbyte-Zeichen fallen und den
/// Queue-Worker mit einer Panik beenden, statt den Eintrag fail-closed zu melden.
/// Deshalb wird nach Zeichen geschnitten.
fn short_id(operation_id: &str) -> &str {
    let end = operation_id
        .char_indices()
        .nth(8)
        .map_or(operation_id.len(), |(index, _)| index);
    &operation_id[..end]
}

fn short_digest(digest: &[u8; 32]) -> String {
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_conflict_proofs(
    destination: &Path,
    expected_conflicts: &[String],
    proofs: &[FileConflictProof],
) -> io::Result<()> {
    // Leere Belege sind ausschließlich für migrierte Alteinträge zulässig. Der
    // Executor vergleicht sie mit frisch aufgenommenen Belegen und pausiert vor
    // jeder Mutation, falls tatsächlich Konflikte vorhanden sind.
    if proofs.is_empty() {
        return Ok(());
    }
    let mut conflicts = expected_conflicts.to_vec();
    conflicts.sort();
    conflicts.dedup();
    let mut proof_paths: Vec<_> = proofs.iter().map(|proof| proof.path.clone()).collect();
    proof_paths.sort();
    proof_paths.dedup();
    if proof_paths != conflicts
        || proofs.iter().any(|proof| {
            let path = Path::new(&proof.path);
            !path.is_absolute()
                || !path.starts_with(destination)
                || path == destination
                || proof.raw_name.is_empty()
                || (proof.is_file && !proof.is_symlink && proof.content_hash.is_none())
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Konfliktbelege passen nicht exakt zu den bestätigten Zielpfaden",
        ));
    }
    Ok(())
}

fn has_legacy_unbound_webdav_delete(entry: &QueueEntry) -> bool {
    let QueueOperation::ProviderDelete {
        endpoint,
        expected_plan,
        ..
    } = &entry.operation
    else {
        return false;
    };
    if !matches!(endpoint.as_ref(), BidirectionalQueueEndpoint::WebDav { .. }) {
        return false;
    }
    let crate::RootIdentity::WebDav(identity) = &expected_plan.root_identity else {
        return false;
    };
    identity.root_etag == LEGACY_UNBOUND_WEBDAV_ETAG
}

fn has_prior_execution_evidence(entry: &QueueEntry) -> bool {
    entry.attempts != 0
        || entry.progress != Progress::default()
        || !entry.temp_recovery_paths.is_empty()
        || !entry.retained_backup_paths.is_empty()
        || entry.recovery_available
        || entry.bidirectional_completion_recovery.is_some()
}

/// In welchem Zustand die Migration einen WebDAV-Löschaltstand hinterlassen hat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyUnboundWebDavDelete {
    /// Sicher vor der ersten Mutation gestoppt; er wird nur verworfen und neu geplant.
    SafeReplan,
    /// Beim letzten Prozess nicht mehr sicher mutationsfrei; verlangt manuelle Prüfung.
    PossiblePartialEffect,
    /// Terminale Datenwirkung samt Bilanz; sie bleibt unverändert erhalten.
    PreservedTerminal,
}

/// Ordnet einen migrierten WebDAV-Löschaltstand ausschließlich über Zustand,
/// Fortschritt und den maschinenlesbaren `ErrorCode` ein.
///
/// Bewusst **nicht** über den deutschen Fehlertext: Dieselbe Einordnung entscheidet
/// über `is_valid_legacy_unbound_webdav_delete` und damit darüber, ob
/// `open_with_process_lock` das gesamte Journal annimmt. Eine bloße Umformulierung
/// der Prosakonstante hätte den Vergleich verfehlt, `WebDavRootIdentity::validate`
/// hätte den Sentinel abgelehnt und das Öffnen wäre für jedes Fenster und die CLI
/// mit `InvalidData` gescheitert — samt aller übrigen Einträge.
fn classify_legacy_unbound_webdav_delete(entry: &QueueEntry) -> Option<LegacyUnboundWebDavDelete> {
    if !has_legacy_unbound_webdav_delete(entry) {
        return None;
    }
    match entry.state {
        QueueState::Failed
            if !has_prior_execution_evidence(entry)
                && entry.error_code == Some(crate::ErrorCode::PlanDrift) =>
        {
            Some(LegacyUnboundWebDavDelete::SafeReplan)
        }
        QueueState::Interrupted
            if entry.progress.phase == "recovery"
                && entry.progress.current.is_empty()
                && entry.error_code == Some(crate::ErrorCode::OutcomeUnclear) =>
        {
            Some(LegacyUnboundWebDavDelete::PossiblePartialEffect)
        }
        QueueState::Completed | QueueState::Cancelled => {
            Some(LegacyUnboundWebDavDelete::PreservedTerminal)
        }
        _ => None,
    }
}

fn is_valid_legacy_unbound_webdav_delete(entry: &QueueEntry) -> bool {
    classify_legacy_unbound_webdav_delete(entry).is_some()
        && entry.attention.is_none()
        && !entry.recovery_available
        && entry.bidirectional_completion_recovery.is_none()
}

fn validate_persisted_entry_state(entry: &QueueEntry) -> io::Result<()> {
    if matches!(entry.operation, QueueOperation::FtpsRecovery { .. })
        && (!matches!(entry.state, QueueState::Interrupted | QueueState::Cancelled)
            || entry.error_code != Some(crate::ErrorCode::OutcomeUnclear)
            || entry.attention.is_some()
            || entry.recovery_available
            || entry.bidirectional_completion_recovery.is_some()
            || !entry.profile_bindings.is_empty())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FTPS-Recovery darf keine ausführbare Operation werden",
        ));
    }

    let awaits_confirmation = entry.state == QueueState::AwaitingConfirmation;
    if awaits_confirmation != entry.attention.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Bestätigungszustand und Bestätigungsdaten passen nicht zusammen",
        ));
    }
    if let Some(attention) = &entry.attention {
        if attention_reason(attention).trim().is_empty()
            || attention_confirmation(attention).trim().is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Bestätigungsdaten enthalten keinen Grund oder keine Phrase",
            ));
        }
    }
    Ok(())
}

fn validate_persisted_entry_operation(entry: &QueueEntry) -> io::Result<()> {
    validate_operation(&entry.operation)?;
    let QueueOperation::ProviderDelete { expected_plan, .. } = &entry.operation else {
        return Ok(());
    };
    let crate::RootIdentity::WebDav(identity) = &expected_plan.root_identity else {
        return Ok(());
    };
    if is_valid_legacy_unbound_webdav_delete(entry) {
        return Ok(());
    }
    identity.validate()
}

fn validate_enqueue_operation(operation: &QueueOperation) -> io::Result<()> {
    if matches!(operation, QueueOperation::FtpsRecovery { .. }) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FTPS-Recovery kann nur aus einem dauerhaften Intent entstehen",
        ));
    }
    validate_operation(operation)
}

fn validate_operation(operation: &QueueOperation) -> io::Result<()> {
    match operation {
        QueueOperation::FtpsRecovery {
            identity,
            account_digest,
        } => {
            identity.validate()?;
            validate_normalized_absolute_path(&identity.virtual_root, "FTPS-Recoveryroot")?;
            if account_digest == &[0; 32] || account_digest == blake3::hash(&[]).as_bytes() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FTPS-Recovery fehlt die Kontobindung",
                ));
            }
            Ok(())
        }
        QueueOperation::File {
            items,
            destination,
            overwrite,
            expected_conflicts,
            expected_conflict_proofs,
            keep_both_targets,
            local_replace_best_effort_expected,
            ..
        } => {
            if items.is_empty() || destination.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Dateioperation braucht Quellen und Zielordner",
                ));
            }
            if local_replace_best_effort_expected == &Some(true)
                && (!overwrite || expected_conflicts.is_empty())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Lokale Best-Effort-Ersetzung ist ohne bestätigten Zielkonflikt unzulässig",
                ));
            }
            let item_paths = items.iter().map(PathBuf::from).collect::<Vec<_>>();
            crate::fileops::validate_file_operation_paths(&item_paths, Path::new(destination))?;
            if !keep_both_targets.is_empty() && *overwrite {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Beide behalten darf keine Ersetzung erlauben",
                ));
            }
            crate::fileops::validate_keep_both_targets(
                &item_paths,
                Path::new(destination),
                keep_both_targets,
            )?;
            validate_conflict_proofs(
                Path::new(destination),
                expected_conflicts,
                expected_conflict_proofs,
            )
        }
        QueueOperation::Sync {
            source,
            target,
            options,
            ..
        }
        | QueueOperation::LocalSync {
            source,
            target,
            options,
            ..
        } => {
            options.validate()?;
            if matches!(operation, QueueOperation::LocalSync { .. })
                && (options.mode == crate::SyncMode::Mirror
                    || (options.mode == crate::SyncMode::Update
                        && options.compare != crate::Compare::ContentHash))
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Normaler lokaler Sync erlaubt AddOnly oder Update mit ContentHash; Mirror bleibt getrennt",
                ));
            }
            if source.trim().is_empty() || target.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Sync-Operation braucht Quelle und Ziel",
                ));
            }
            Ok(())
        }
        QueueOperation::SftpSync {
            connection,
            local_root,
            remote_root,
            options,
            ..
        } => {
            connection.validate()?;
            options.validate()?;
            if local_root.trim().is_empty()
                || remote_root.trim().is_empty()
                || !Path::new(remote_root).is_absolute()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SFTP-Queue-Eintrag braucht gültige lokale und entfernte Roots",
                ));
            }
            Ok(())
        }
        QueueOperation::FtpsSync {
            connection,
            local_root,
            remote_root,
            options,
            ..
        } => {
            connection.validate()?;
            options.validate()?;
            if local_root.trim().is_empty()
                || remote_root.trim().is_empty()
                || !Path::new(remote_root).is_absolute()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "FTPS-Queue-Eintrag braucht gültige lokale und entfernte Roots",
                ));
            }
            Ok(())
        }
        QueueOperation::WebDavSync {
            connection,
            local_root,
            remote_root,
            options,
            ..
        } => {
            connection.validate()?;
            options.validate()?;
            if local_root.trim().is_empty()
                || remote_root.trim().is_empty()
                || !Path::new(remote_root).is_absolute()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "WebDAV-Queue-Eintrag braucht gültige lokale und entfernte Roots",
                ));
            }
            Ok(())
        }
        QueueOperation::SmbSync {
            connection,
            local_root,
            remote_root,
            options,
            ..
        } => {
            connection.validate()?;
            options.validate()?;
            if local_root.trim().is_empty()
                || remote_root.trim().is_empty()
                || !Path::new(remote_root).is_absolute()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SMB-Queue-Eintrag braucht gültige lokale und entfernte Roots",
                ));
            }
            Ok(())
        }
        QueueOperation::ProviderFile {
            kind,
            source,
            target,
            items,
            destination,
            overwrite,
            expected_conflicts,
            expected_conflict_proofs,
            keep_both_targets,
            ..
        } => {
            validate_bidirectional_endpoint(source)?;
            validate_bidirectional_endpoint(target)?;
            if let QueueOperation::ProviderFile {
                source_tree_expected: Some(tree),
                ..
            } = operation
            {
                let supported = matches!(source.as_ref(), BidirectionalQueueEndpoint::Sftp { .. })
                    || (matches!(source.as_ref(), BidirectionalQueueEndpoint::Local { .. })
                        && matches!(target.as_ref(), BidirectionalQueueEndpoint::WebDav { .. })
                        && *kind == FileQueueKind::Copy
                        && !*overwrite);
                if !supported {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Baumbeleg gehört zur begrenzten additiven Inhaltsklasse",
                    ));
                }
                tree.validate()?;
                for omission in &tree.omitted {
                    if !items.iter().any(|item| {
                        Path::new(&omission.path).starts_with(item)
                            && (&omission.path != item
                                || matches!(
                                    source.as_ref(),
                                    BidirectionalQueueEndpoint::Local { .. }
                                ))
                    }) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Ausgelassener Quellpfad verlässt die ausgewählten Teilbäume",
                        ));
                    }
                    validate_normalized_absolute_path(
                        &omission.path,
                        "Ausgelassener Quellbaumpfad",
                    )?;
                }
            }
            if matches!(source.as_ref(), BidirectionalQueueEndpoint::Sftp { .. }) {
                if *kind != FileQueueKind::Copy
                    || *overwrite
                    || !matches!(target.as_ref(), BidirectionalQueueEndpoint::Local { .. })
                {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, "SFTP-Inhalte erlauben nur additive lokale Downloads; Verschieben, Ersetzen und Remote-Ziele bleiben gesperrt"));
                }
                crate::sftp::validate_interactive_content_selection(
                    Path::new(source.root()),
                    &items.iter().map(PathBuf::from).collect::<Vec<_>>(),
                )?;
            }
            let source_root =
                validate_normalized_absolute_path(source.root(), "Provider-Quellroot")?;
            let target_root =
                validate_normalized_absolute_path(target.root(), "Provider-Zielroot")?;
            let destination_path =
                validate_normalized_absolute_path(destination, "Provider-Zielordner")?;
            if items.is_empty() || destination_path != target_root {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Provider-Dateioperation besitzt ungebundene Quellen oder Zielpfade",
                ));
            }
            for item in items {
                validate_normalized_scoped_child(&source_root, item, "Provider-Dateiquelle")?;
            }
            let item_paths = items.iter().map(PathBuf::from).collect::<Vec<_>>();
            let (validated_root, validated_items) =
                crate::fileops::validate_file_operation_scope(&source_root, &item_paths)?;
            if validated_root != source_root || validated_items != item_paths {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Provider-Dateiauswahl ist nicht eindeutig normalisiert",
                ));
            }
            for conflict in expected_conflicts {
                validate_normalized_scoped_child(
                    &destination_path,
                    conflict,
                    "Provider-Konfliktziel",
                )?;
            }
            if !keep_both_targets.is_empty() && *overwrite {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Beide behalten darf keine Ersetzung erlauben",
                ));
            }
            crate::fileops::validate_keep_both_targets(
                &item_paths,
                Path::new(destination),
                keep_both_targets,
            )?;
            validate_conflict_proofs(
                &destination_path,
                expected_conflicts,
                expected_conflict_proofs,
            )
        }
        QueueOperation::ProviderDelete {
            endpoint,
            expected_plan,
            ..
        } => {
            validate_bidirectional_endpoint(endpoint)?;
            let root = validate_normalized_absolute_path(endpoint.root(), "Provider-Löschroot")?;
            let plan_root =
                validate_normalized_absolute_path(&expected_plan.root, "Provider-Löschplanroot")?;
            if root != plan_root
                || endpoint.root() != expected_plan.root
                || expected_plan.items.is_empty()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Provider-Löschoperation besitzt keinen gebundenen Plan",
                ));
            }
            for item in &expected_plan.items {
                let item_path =
                    validate_normalized_scoped_child(&root, &item.path, "Provider-Löschauswahl")?;
                if item_path.parent() != Some(root.as_path())
                    || item.records.is_empty()
                    || item.records[0].path != item.path
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Provider-Löschplan enthält keine direkte, vollständige Auswahl",
                    ));
                }
                for record in &item.records {
                    let record_path =
                        validate_normalized_absolute_path(&record.path, "Provider-Löschplanpfad")?;
                    if !record_path.starts_with(&item_path) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Provider-Löschplanpfad verlässt seine gebundene Auswahl",
                        ));
                    }
                }
            }
            Ok(())
        }
        QueueOperation::ProviderSync {
            source,
            target,
            options,
            ..
        } => {
            validate_bidirectional_endpoint(source)?;
            validate_bidirectional_endpoint(target)?;
            options.validate()?;
            Ok(())
        }
        QueueOperation::Bidirectional {
            pair_id,
            pair_name,
            left,
            right,
            baseline_path,
            resolutions,
            expected_baseline_digest,
            expected_plan_digest,
        } => {
            let baseline =
                validate_normalized_absolute_path(baseline_path, "Bidirektionaler Baseline-Pfad")?;
            validate_bidirectional_resolutions(resolutions)?;
            if pair_id.0 == [0; 16]
                || pair_name.trim() != pair_name
                || pair_name.is_empty()
                || pair_name.len() > 80
                || pair_name.chars().any(char::is_control)
                || expected_baseline_digest == &Some([0; 32])
                || expected_plan_digest == &[0; 32]
                || validate_bidirectional_endpoint(left).is_err()
                || validate_bidirectional_endpoint(right).is_err()
                || local_endpoint_contains(left, &baseline)
                || local_endpoint_contains(right, &baseline)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Bidirektionaler Queue-Eintrag besitzt ungültige Paar- oder Digestdaten",
                ));
            }
            Ok(())
        }
    }
}

fn validate_bidirectional_resolutions(resolutions: &[ConflictResolution]) -> io::Result<()> {
    let mut decision_paths = std::collections::BTreeSet::new();
    let mut conflict_paths = Vec::new();
    for resolution in resolutions {
        validate_binary_relative_path(&resolution.path)?;
        if !decision_paths.insert(&resolution.path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bidirektionale Konfliktentscheidung enthält einen Pfad mehrfach",
            ));
        }
        if let crate::ConflictDecision::KeepBoth { conflict_path, .. } = &resolution.decision {
            validate_binary_relative_path(conflict_path)?;
            conflict_paths.push(conflict_path);
        }
    }
    let decisions = decision_paths.iter().copied().collect::<Vec<_>>();
    for (index, path) in decisions.iter().enumerate() {
        if decisions[index + 1..].iter().any(|other| {
            binary_relative_path_contains(path, other) || binary_relative_path_contains(other, path)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Eltern- und Kindkonflikt dürfen nicht gemeinsam entschieden werden",
            ));
        }
    }
    for (index, path) in conflict_paths.iter().enumerate() {
        if decisions.iter().any(|decision| {
            *decision == *path
                || binary_relative_path_contains(decision, path)
                || binary_relative_path_contains(path, decision)
        }) || conflict_paths[index + 1..].iter().any(|other| {
            *other == *path
                || binary_relative_path_contains(other, path)
                || binary_relative_path_contains(path, other)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bidirektionaler Konfliktname kollidiert mit einer anderen Entscheidung",
            ));
        }
    }
    Ok(())
}

fn validate_binary_relative_path(path: &crate::BinaryRelativePath) -> io::Result<()> {
    if path.0.is_empty()
        || path.0.len() > 256
        || path.0.iter().any(|component| {
            component.is_empty()
                || component == b"."
                || component == b".."
                || component.contains(&0)
                || component.contains(&b'/')
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bidirektionale Konfliktentscheidung enthält einen ungültigen relativen Pfad",
        ));
    }
    Ok(())
}

fn binary_relative_path_contains(
    parent: &crate::BinaryRelativePath,
    child: &crate::BinaryRelativePath,
) -> bool {
    parent != child && parent.0.len() < child.0.len() && child.0.starts_with(parent.0.as_slice())
}

fn validate_profile_bindings(bindings: &[crate::RemoteProfileBinding]) -> io::Result<()> {
    let mut identities = HashSet::new();
    for binding in bindings {
        binding.validate()?;
        if !identities.insert((binding.protocol, binding.profile_id.as_str())) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Queue-Eintrag enthält dieselbe Profilbindung mehrfach",
            ));
        }
    }
    Ok(())
}

fn validate_bidirectional_endpoint(endpoint: &BidirectionalQueueEndpoint) -> io::Result<()> {
    validate_normalized_absolute_path(endpoint.root(), "Bidirektionaler Queue-Endpunktroot")?;
    match endpoint {
        BidirectionalQueueEndpoint::Local { .. } => Ok(()),
        BidirectionalQueueEndpoint::Sftp { connection, .. } => connection.validate(),
        BidirectionalQueueEndpoint::Ftps { connection, .. } => connection.validate(),
        BidirectionalQueueEndpoint::WebDav { connection, .. } => connection.validate(),
        BidirectionalQueueEndpoint::Smb { connection, .. } => connection.validate(),
    }
}

fn validate_normalized_absolute_path(path: &str, role: &str) -> io::Result<PathBuf> {
    let original = Path::new(path);
    let normalized = crate::fileops::validate_file_operation_root(original).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{role} ist ungültig: {error}"),
        )
    })?;
    let normalized_text = crate::fileops::path_to_utf8(&normalized)?;
    if path.trim() != path || path != normalized_text {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{role} muss absolut und lexikalisch normalisiert sein"),
        ));
    }
    Ok(normalized)
}

fn validate_normalized_scoped_child(root: &Path, path: &str, role: &str) -> io::Result<PathBuf> {
    let path = validate_normalized_absolute_path(path, role)?;
    if path == root || !path.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{role} verlässt den gebundenen Root"),
        ));
    }
    Ok(path)
}

fn local_endpoint_contains(endpoint: &BidirectionalQueueEndpoint, path: &Path) -> bool {
    matches!(endpoint, BidirectionalQueueEndpoint::Local { root } if path.starts_with(root))
}

fn retain_temp_recovery_paths(entry: &mut QueueEntry, paths: &[String]) {
    for path in paths {
        if !entry.temp_recovery_paths.contains(path) {
            entry.temp_recovery_paths.push(path.clone());
        }
    }
}

fn finish_bidirectional_effect_entry(
    entry: &mut QueueEntry,
    rolled_back: bool,
    mut progress: Progress,
) -> io::Result<()> {
    if !matches!(entry.operation, QueueOperation::Bidirectional { .. })
        || !matches!(
            entry.state,
            QueueState::Validating
                | QueueState::Running
                | QueueState::Failed
                | QueueState::Interrupted
        )
    {
        return Err(invalid_transition(entry.state));
    }
    entry.state = if rolled_back {
        QueueState::Cancelled
    } else {
        QueueState::Completed
    };
    progress.phase = if rolled_back {
        "rolled_back".into()
    } else {
        "done".into()
    };
    entry.progress = progress;
    entry.error = None;
    entry.error_code = None;
    entry.attention = None;
    entry.recovery_available = false;
    entry.bidirectional_completion_recovery = None;
    Ok(())
}

fn find_mut<'a>(journal: &'a mut QueueSnapshot, id: &str) -> io::Result<&'a mut QueueEntry> {
    journal
        .entries
        .iter_mut()
        .find(|entry| entry.operation_id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag fehlt"))
}

fn require_state(entry: &QueueEntry, expected: QueueState) -> io::Result<()> {
    if entry.state == expected {
        Ok(())
    } else {
        Err(invalid_transition(entry.state))
    }
}

fn invalid_transition(current: QueueState) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("Queue-Zustandswechsel aus {current:?} ist hier nicht erlaubt"),
    )
}

fn attention_reason(attention: &QueueAttention) -> &str {
    match attention {
        QueueAttention::PlanChanged { reason, .. }
        | QueueAttention::SourceTreeChanged { reason, .. }
        | QueueAttention::ConflictsChanged { reason, .. }
        | QueueAttention::Manual { reason, .. }
        | QueueAttention::BestEffortCommitDowngrade { reason, .. }
        | QueueAttention::LocalFileCommit { reason, .. }
        | QueueAttention::RemoteRun { reason, .. }
        | QueueAttention::BidirectionalPlanChanged { reason, .. } => reason,
    }
}

fn attention_confirmation(attention: &QueueAttention) -> &str {
    match attention {
        QueueAttention::PlanChanged { confirmation, .. }
        | QueueAttention::SourceTreeChanged { confirmation, .. }
        | QueueAttention::ConflictsChanged { confirmation, .. }
        | QueueAttention::Manual { confirmation, .. }
        | QueueAttention::BestEffortCommitDowngrade { confirmation, .. }
        | QueueAttention::LocalFileCommit { confirmation, .. }
        | QueueAttention::RemoteRun { confirmation, .. }
        | QueueAttention::BidirectionalPlanChanged { confirmation, .. } => confirmation,
    }
}

fn new_operation_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn unix_millis() -> io::Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(io::Error::other)
}

#[cfg(test)]
fn read_snapshot(path: &Path) -> io::Result<QueueSnapshot> {
    // Metadaten und Inhalt stammen vom selben ohne Symlink-Folgen geöffneten
    // Deskriptor. Eine getrennte `symlink_metadata`-/`File::open`-Folge ließe einen
    // Austausch zwischen Prüfung und Lesen zu. Der stabile Systempräfix wird wie
    // beim atomaren Schreiben einmal kanonisiert; nur der finale Journalname bleibt
    // anschließend ein ohne Symlink-Folgen geöffneter Eintrag.
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Queue-Journal braucht einen Elternordner",
        )
    })?;
    let parent = std::fs::canonicalize(parent)?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Queue-Journal braucht einen Dateinamen",
        )
    })?;
    let file = crate::local_fs::open_read(&parent.join(name))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Queue-Journal ist keine reguläre Datei",
        ));
    }
    read_snapshot_from_reader(file, metadata.len(), MAX_JOURNAL_BYTES)
}

fn read_snapshot_from_reader(
    reader: impl Read,
    advertised_len: u64,
    max_bytes: u64,
) -> io::Result<QueueSnapshot> {
    if advertised_len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Queue-Journal überschreitet die Größenbegrenzung",
        ));
    }
    let mut data = Vec::with_capacity(usize::try_from(advertised_len).unwrap_or(0));
    reader.take(max_bytes + 1).read_to_end(&mut data)?;
    // Die erste Längenprüfung beschreibt nur den Zeitpunkt von `fstat`. Eine
    // reguläre Datei kann danach über die Grenze wachsen. Das zusätzliche Byte
    // und diese zweite Prüfung verhindern, dass ein gültiger JSON-Präfix mit
    // anschließendem, ungelesenem Inhalt als vollständiges Journal gilt.
    if data.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Queue-Journal überschreitet die Größenbegrenzung",
        ));
    }
    serde_json::from_slice(&data).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Queue-Journal ist beschädigt: {error}"),
        )
    })
}

impl OperationQueueProcessLock {
    pub fn acquire(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let directory = queue_parent(path)?;
        let root = crate::local_fs::RootHandle::open_nofollow(&directory)?;
        let file = acquire_process_lock(&root)?;
        if !root.is_current()? {
            return Err(io::Error::other("Queueordner wechselte beim Prozesslock"));
        }
        // Ein passender Dateiname beweist keinen Besitz. Auch unter der gemeinsamen
        // Prozesssperre könnte ein fremder Prozess oder Nutzer einen regulären
        // Eintrag beziehungsweise Symlink so benannt haben. Ohne einen bereits bei
        // der exklusiven Temp-Anlage gebundenen Beleg bleibt deshalb jeder alte
        // Tempname als sichtbarer Recoverybestand stehen.
        Ok(Self {
            directory,
            root,
            file,
            lock_owner: std::process::id(),
        })
    }
}

fn queue_parent(path: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Queue-Journal braucht einen Elternordner",
        )
    })?;
    // Der normale GUI-Pfad erreicht diesen Helfer ohne vorherige Sandboxanlage.
    // Nur neue Ordner privat erzeugen; bestehende Rechte und ACLs nicht verändern.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    std::fs::canonicalize(parent)
}

fn acquire_process_lock(root: &crate::local_fs::RootHandle) -> io::Result<File> {
    if !root.is_current()? {
        return Err(io::Error::other(
            "Queueordner wechselte vor dem Prozesslock",
        ));
    }
    let file = File::from(rustix::fs::openat(
        root.fd(),
        "operations-queue.lock",
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::from_raw_mode(0o600),
    )?);
    if !file.metadata()?.is_file() || !root.is_current()? {
        return Err(io::Error::other(
            "Queue-Prozesslock verlor seine reguläre Datei oder Ordnerbindung",
        ));
    }
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
        |error| {
            // Ein belegtes Lock ist der reguläre Fall „GUI und CLI gleichzeitig".
            // Der rohe EAGAIN-Text wäre für Nutzer unverständlich.
            if error == rustix::io::Errno::WOULDBLOCK {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Die Operations-Queue wird bereits von einem anderen Doppeldecker-Prozess (GUI oder CLI) verwendet",
                )
            } else {
                io::Error::from(error)
            }
        },
    )?;
    Ok(file)
}

fn read_snapshot_bound(
    root: &crate::local_fs::RootHandle,
    path: &Path,
) -> io::Result<QueueSnapshot> {
    if !root.is_current()? {
        return Err(io::Error::other("Queueordner wurde ausgetauscht"));
    }
    let file = crate::local_fs::open_read_beneath(root, path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("Queue-Journal ist keine reguläre Datei"));
    }
    let snapshot = read_snapshot_from_reader(file, metadata.len(), MAX_JOURNAL_BYTES)?;
    if !root.is_current()? {
        return Err(io::Error::other(
            "Queueordner wurde während des Readbacks ausgetauscht",
        ));
    }
    Ok(snapshot)
}

fn write_snapshot_bound(
    root: &crate::local_fs::RootHandle,
    path: &Path,
    snapshot: &QueueSnapshot,
    mode: AtomicWriteMode,
) -> io::Result<()> {
    if path.parent() != Some(root.path()) || !root.is_current()? {
        return Err(io::Error::other(
            "Queue-Journal verlor seinen gehaltenen Elternordner",
        ));
    }
    let payload = serde_json::to_vec_pretty(snapshot).map_err(io::Error::other)?;
    if payload.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Queue-Journal überschreitet die Größenbegrenzung",
        ));
    }
    let vfs = LocalVfs::rooted(root.path())?;
    if Vfs::anchored_identity(&vfs) != Some(root.identity()) {
        return Err(io::Error::other(
            "Queueordner wechselte vor dem Journalwrite",
        ));
    }
    crate::atomic_write::atomic_write_vfs_with_mode(
        &vfs,
        path,
        &payload,
        ".operations-queue-",
        mode,
        Some(0o600),
    )
}

#[cfg(test)]
fn write_snapshot(path: &Path, snapshot: &QueueSnapshot) -> io::Result<()> {
    write_snapshot_with_mode(path, snapshot, AtomicWriteMode::Replace)
}

#[cfg(test)]
fn write_snapshot_with_mode(
    path: &Path,
    snapshot: &QueueSnapshot,
    mode: AtomicWriteMode,
) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Queue-Journal braucht einen Elternordner",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let payload = serde_json::to_vec_pretty(snapshot).map_err(io::Error::other)?;
    if payload.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Queue-Journal überschreitet die Größenbegrenzung",
        ));
    }
    atomic_write_local(path, &payload, ".operations-queue-", mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{CompleteDirectoryListing, ConditionalContentReplaceRequest, ProofListEntry};
    use crate::{
        copy_items, Compare, FtpTransportSecurity, FtpsCertificatePin, FtpsCertificatePolicy,
        KeychainSecretReference, MetadataProfile, SftpAuthenticationReference, SftpHostKeyPin,
        SyncMode, Vfs,
    };
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    fn normal_sync_fixture() -> (
        tempfile::TempDir,
        OperationQueue,
        QueueEntry,
        crate::PreparedLocalSyncRun,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(source.join(name), name).unwrap();
        }
        let source = std::fs::canonicalize(source).unwrap();
        let target = std::fs::canonicalize(target).unwrap();
        let options = SyncOptions {
            compare: Compare::ContentHash,
            ..Default::default()
        };
        let prepared = crate::prepare_local_sync_run(&source, &target, options.clone()).unwrap();
        let queue = OperationQueue::open(temp.path().join("state/queue.json")).unwrap();
        let entry = queue
            .enqueue(QueueOperation::LocalSync {
                execution_hint: prepared.preview().local_execution.clone(),
                source: source.to_str().unwrap().into(),
                target: target.to_str().unwrap().into(),
                sandbox: None,
                options,
                expected_plan: Box::new(prepared.preview().plan.clone()),
            })
            .unwrap();
        (temp, queue, entry, prepared)
    }

    #[test]
    fn queue_schreibt_nach_ordnertausch_nicht_in_den_fremden_configbaum() {
        let area = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(area.path()).unwrap();
        let config = root.join("config");
        std::fs::create_dir(&config).unwrap();
        let queue = OperationQueue::open(config.join("queue.json")).unwrap();
        let original = std::fs::read(config.join("queue.json")).unwrap();
        std::fs::rename(&config, root.join("old-config")).unwrap();
        std::fs::create_dir(&config).unwrap();
        std::fs::write(config.join("foreign.txt"), b"behalten").unwrap();
        let result = queue.enqueue(file_operation("root-swap"));
        assert!(
            result.is_err(),
            "Queue schrieb ohne ihre ursprüngliche Ordnerbindung"
        );
        assert_eq!(
            std::fs::read(config.join("foreign.txt")).unwrap(),
            b"behalten"
        );
        assert_eq!(std::fs::read_dir(&config).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(root.join("old-config/queue.json")).unwrap(),
            original
        );
        assert!(queue.snapshot().entries.is_empty());
    }

    #[test]
    fn neuer_queueordner_ist_auch_bei_offener_umask_privat() {
        use std::os::unix::fs::PermissionsExt;
        const MARKER: &str = "DOPPELDECKER_PRIVATE_QUEUE_CHILD";
        if let Some(area) = std::env::var_os(MARKER) {
            let area = PathBuf::from(area);
            assert_eq!(
                std::fs::read(area.join("marker")).unwrap(),
                b"AP16 private queue"
            );
            unsafe {
                libc::umask(0);
            }
            let root = area.join("new/config");
            let queue = OperationQueue::open(root.join("queue.json")).unwrap();
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert!(queue._process_lock.root.has_exclusive_namespace().unwrap());
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("marker"), b"AP16 private queue").unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "queue::tests::neuer_queueordner_ist_auch_bei_offener_umask_privat",
                "--test-threads=1",
            ])
            .env(MARKER, fixture.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }

    #[test]
    fn normal_sync_crash_child() {
        let Ok(root) = std::env::var("DOPPELDECKER_NORMAL_SYNC_CRASH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        assert_eq!(std::fs::read(root.join("crash-marker")).unwrap(), b"AP03");
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        let prepared = prepare_local_sync_queue_entry(&queue, &entry, None).unwrap();
        queue
            .approve_local_sync_preparation(&entry.operation_id, &prepared)
            .unwrap();
        let entry = queue.claim_next().unwrap().unwrap();
        execute_prepared_local_sync_queue_entry(
            &queue,
            &entry,
            prepared,
            false,
            &root.join("audit.jsonl"),
            &Cancel::new(),
            &mut |progress| {
                if progress.bytes_done > 0 {
                    // Ohne Destruktoren abbrechen, solange die erste Datei noch im
                    // privaten Staging liegt und kein Zielcommit stattgefunden hat.
                    std::process::exit(91);
                }
            },
        )
        .unwrap();
        panic!("Der Crashpunkt wurde nicht erreicht");
    }

    #[test]
    fn normaler_sync_prozessabbruch_ordnet_staging_der_fensterqueue_zu() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, queue, entry, prepared) = normal_sync_fixture();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(temp.path().join("crash-marker"), b"AP03").unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::local_fs::write_acl_exact(&root, None).unwrap();
        crate::local_fs::write_acl_exact(&root.join("state"), None).unwrap();
        std::fs::set_permissions(root.join("state"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert!(crate::local_fs::RootHandle::open_nofollow(&root)
            .unwrap()
            .is_private()
            .unwrap());
        assert!(
            crate::local_fs::RootHandle::open_nofollow(&root.join("state"))
                .unwrap()
                .has_exclusive_namespace()
                .unwrap()
        );
        std::fs::set_permissions(
            temp.path().join("ziel"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        drop(prepared);
        let path = queue.path.clone();
        drop(queue);
        let executable = std::env::current_exe().unwrap();
        let child = "queue::tests::normal_sync_crash_child";
        let listed = std::process::Command::new(&executable)
            .args(["--list", "--exact", child])
            .output()
            .unwrap();
        assert!(listed.status.success());
        assert_eq!(
            String::from_utf8(listed.stdout)
                .unwrap()
                .lines()
                .filter(|line| *line == format!("{child}: test"))
                .count(),
            1
        );
        let status = std::process::Command::new(executable)
            .args(["--exact", child, "--nocapture"])
            .env("DOPPELDECKER_NORMAL_SYNC_CRASH_ROOT", &root)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(91));
        let reopened = OperationQueue::open(path).unwrap();
        let snapshot = reopened.snapshot();
        let recovered = &snapshot.entries[0];
        assert_eq!(recovered.operation_id, entry.operation_id);
        assert_eq!(recovered.state, QueueState::Interrupted);
        assert_eq!(recovered.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        assert_eq!(recovered.temp_recovery_paths.len(), 1);
        assert!(Path::new(&recovered.temp_recovery_paths[0]).is_file());
        assert_eq!(
            std::fs::read_dir(temp.path().join("ziel")).unwrap().count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(temp.path().join("quelle"))
                .unwrap()
                .count(),
            3
        );
        assert!(reopened.claim_next().unwrap().is_none());
    }

    #[test]
    fn normaler_sync_queue_abbruch_bilanziert_teilwirkung_und_reopen() {
        let (temp, queue, entry, prepared) = normal_sync_fixture();
        let claimed = queue.claim_next().unwrap().unwrap();
        let cancel = Cancel::new();
        let callback_cancel = cancel.clone();
        let result = execute_prepared_local_sync_queue_entry(
            &queue,
            &claimed,
            prepared,
            false,
            &temp.path().join("audit.jsonl"),
            &cancel,
            &mut |progress| {
                if progress.files_done >= 1 {
                    callback_cancel.cancel();
                }
            },
        )
        .unwrap();
        let QueueExecutionResult::Sync(stats) = result else {
            panic!("Falscher Abschluss")
        };
        assert!(stats.cancelled);
        assert!(!stats.dry_run);
        assert!(stats.files_copied >= 1 && stats.files_copied < 3);
        assert_eq!(
            std::fs::read_dir(temp.path().join("quelle"))
                .unwrap()
                .count(),
            3
        );
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
        let path = queue.path.clone();
        drop(queue);
        let reopened = OperationQueue::open(path).unwrap();
        assert_eq!(
            reopened.snapshot().entries[0].operation_id,
            entry.operation_id
        );
        assert_eq!(reopened.snapshot().entries[0].state, QueueState::Cancelled);
        assert_eq!(
            reopened.snapshot().entries[0].progress.files_done,
            stats.files_copied
        );
        assert!(reopened.claim_next().unwrap().is_none());
    }

    #[test]
    fn normaler_sync_queue_abbruch_vor_wirkung_laesst_alle_dateien_unberuehrt() {
        let (temp, queue, entry, prepared) = normal_sync_fixture();
        let claimed = queue.claim_next().unwrap().unwrap();
        let cancel = Cancel::new();
        cancel.cancel();
        let result = execute_prepared_local_sync_queue_entry(
            &queue,
            &claimed,
            prepared,
            false,
            &temp.path().join("audit.jsonl"),
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        let QueueExecutionResult::Sync(stats) = result else {
            panic!("Falscher Abschluss")
        };
        assert!(stats.cancelled);
        assert_eq!(stats.files_copied, 0);
        assert_eq!(
            std::fs::read_dir(temp.path().join("ziel")).unwrap().count(),
            0
        );
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                std::fs::read(temp.path().join("quelle").join(name)).unwrap(),
                name.as_bytes()
            );
        }
        assert_eq!(queue.snapshot().entries[0].operation_id, entry.operation_id);
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
    }

    #[test]
    fn normaler_sync_queue_neustart_verlangt_frische_wirkungsbestaetigung() {
        let (temp, queue, entry, prepared) = normal_sync_fixture();
        drop(prepared);
        let path = queue.path.clone();
        drop(queue);
        let queue = OperationQueue::open(path).unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let prepared = prepare_local_sync_queue_entry(&queue, &claimed, None).unwrap();
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        assert_eq!(
            std::fs::read_dir(temp.path().join("ziel")).unwrap().count(),
            0
        );
        let phrase = prepared.preview().confirmation_phrase.clone();
        assert!(queue
            .approve_attention(&entry.operation_id, &phrase)
            .is_err());
        queue
            .approve_local_sync_preparation(&entry.operation_id, &prepared)
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        execute_prepared_local_sync_queue_entry(
            &queue,
            &claimed,
            prepared,
            false,
            &temp.path().join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
        assert_eq!(
            std::fs::read_dir(temp.path().join("ziel")).unwrap().count(),
            3
        );
    }

    #[test]
    fn normaler_sync_wartende_zustimmung_wird_nach_neustart_lokal_erklaert() {
        let (temp, queue, entry, prepared) = normal_sync_fixture();
        drop(prepared);
        let claimed = queue.claim_next().unwrap().unwrap();
        let prepared = prepare_local_sync_queue_entry(&queue, &claimed, None).unwrap();
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        drop(prepared);
        let path = queue.path.clone();
        drop(queue);
        let reopened = OperationQueue::open(path).unwrap();
        let snapshot = reopened.snapshot();
        let interrupted = &snapshot.entries[0];
        assert_eq!(interrupted.state, QueueState::Interrupted);
        assert_eq!(interrupted.error_code, Some(crate::ErrorCode::Interrupted));
        assert!(interrupted
            .error
            .as_deref()
            .unwrap()
            .starts_with("Lokale Sync-Vorbereitung"));
        assert!(interrupted.attention.is_none());
        assert!(reopened.claim_next().unwrap().is_none());
        assert_eq!(
            std::fs::read_dir(temp.path().join("ziel")).unwrap().count(),
            0
        );
        reopened.retry(&entry.operation_id).unwrap();
        let claimed = reopened.claim_next().unwrap().unwrap();
        let _prepared = prepare_local_sync_queue_entry(&reopened, &claimed, None).unwrap();
        assert_eq!(
            reopened.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        assert_eq!(
            std::fs::read_dir(temp.path().join("ziel")).unwrap().count(),
            0
        );
    }

    #[test]
    fn normaler_sync_queue_spaete_kollision_und_verwerfen_lassen_originale_intakt() {
        let (temp, queue, entry, prepared) = normal_sync_fixture();
        let claimed = queue.claim_next().unwrap().unwrap();
        std::fs::write(temp.path().join("ziel/a.txt"), b"fremd").unwrap();
        let error = execute_prepared_local_sync_queue_entry(
            &queue,
            &claimed,
            prepared,
            false,
            &temp.path().join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )
        .err()
        .unwrap();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(
            std::fs::read(temp.path().join("ziel/a.txt")).unwrap(),
            b"fremd"
        );
        queue
            .fail_coded(&entry.operation_id, Progress::default(), &error)
            .unwrap();
        queue.set_paused(false).unwrap();
        queue.retry(&entry.operation_id).unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let _prepared = prepare_local_sync_queue_entry(&queue, &claimed, None).unwrap();
        queue.discard_attention(&entry.operation_id).unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
        assert_eq!(
            std::fs::read_dir(temp.path().join("ziel")).unwrap().count(),
            1
        );
    }

    #[test]
    fn normaler_sync_kollision_nach_teilcommit_erhaelt_fremde_datei_und_bilanz() {
        let (temp, queue, entry, prepared) = normal_sync_fixture();
        let claimed = queue.claim_next().unwrap().unwrap();
        let mut injected = false;
        let error = execute_prepared_local_sync_queue_entry(
            &queue,
            &claimed,
            prepared,
            false,
            &temp.path().join("audit.jsonl"),
            &Cancel::new(),
            &mut |progress| {
                if !injected && progress.files_done == 1 {
                    std::fs::write(temp.path().join("ziel/b.txt"), b"Fremde spate Datei").unwrap();
                    injected = true;
                }
            },
        )
        .err()
        .unwrap();
        assert!(injected);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(
            std::fs::read(temp.path().join("ziel/a.txt")).unwrap(),
            b"a.txt"
        );
        assert_eq!(
            std::fs::read(temp.path().join("ziel/b.txt")).unwrap(),
            b"Fremde spate Datei"
        );
        assert!(!temp.path().join("ziel/c.txt").exists());
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                std::fs::read(temp.path().join("quelle").join(name)).unwrap(),
                name.as_bytes()
            );
        }
        let path = queue.path.clone();
        let paths = crate::error::recovery_paths(&error);
        drop(queue);
        let reopened = OperationQueue::open(path).unwrap();
        let snapshot = reopened.snapshot();
        let failed = &snapshot.entries[0];
        assert_eq!(failed.operation_id, entry.operation_id);
        assert_eq!(failed.state, QueueState::Failed);
        assert_eq!(failed.error_code, Some(crate::ErrorCode::PlanDrift));
        assert_eq!(failed.progress.files_done, 1);
        assert_eq!(failed.temp_recovery_paths, paths);
        assert!(reopened.claim_next().unwrap().is_none());
    }

    #[test]
    fn terminaler_fortschritt_wird_nur_im_queueabschluss_persistiert() {
        for phase in ["done", "cancelled"] {
            let progress = Progress {
                phase: phase.into(),
                files_done: 7,
                bytes_done: 11,
                ..Progress::default()
            };
            assert!(
                !should_persist_file_progress(&progress, Duration::from_secs(1)),
                "{phase} darf keinen getrennten, fehleranfälligen Progress-Commit vor complete/cancel auslösen"
            );
        }
    }

    #[derive(Clone, Copy)]
    enum QuarantineRunCreateFailure {
        Unchanged,
        OutcomeUnclear,
    }

    #[derive(Clone, Copy)]
    enum QuarantineParentAttack {
        Base,
        Run,
        BoundParent,
        SourceParent,
    }

    struct QueueConflictRaceVfs {
        inner: LocalVfs,
        namespace: Mutex<()>,
        attack: bool,
        attack_path: Mutex<Option<PathBuf>>,
        conditional_calls: AtomicUsize,
        two_parent_support: AtomicBool,
        manifest_attack_before_rename: AtomicBool,
        displaced: Mutex<Option<PathBuf>>,
        cancel_after_rename: Mutex<Option<Cancel>>,
        cancel_during_proof: Mutex<Option<Cancel>>,
        quarantine_base_create_failure: Mutex<Option<QuarantineRunCreateFailure>>,
        quarantine_run_create_failure: Mutex<Option<QuarantineRunCreateFailure>>,
        fail_sync_path: Mutex<Option<PathBuf>>,
        parent_attack: Mutex<Option<QuarantineParentAttack>>,
        parent_displaced_path: Mutex<Option<PathBuf>>,
        parent_replacement_path: Mutex<Option<PathBuf>>,
    }

    impl QueueConflictRaceVfs {
        fn new(attack: bool) -> Self {
            Self {
                inner: LocalVfs::new(),
                namespace: Mutex::new(()),
                attack,
                attack_path: Mutex::new(None),
                conditional_calls: AtomicUsize::new(0),
                two_parent_support: AtomicBool::new(true),
                manifest_attack_before_rename: AtomicBool::new(false),
                displaced: Mutex::new(None),
                cancel_after_rename: Mutex::new(None),
                cancel_during_proof: Mutex::new(None),
                quarantine_base_create_failure: Mutex::new(None),
                quarantine_run_create_failure: Mutex::new(None),
                fail_sync_path: Mutex::new(None),
                parent_attack: Mutex::new(None),
                parent_displaced_path: Mutex::new(None),
                parent_replacement_path: Mutex::new(None),
            }
        }

        fn arm_attack(&self, path: &Path) {
            *self.attack_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn disable_two_parent_contract(&self) {
            self.two_parent_support.store(false, Ordering::SeqCst);
        }

        fn attack_manifest_before_next_rename(&self) {
            self.manifest_attack_before_rename
                .store(true, Ordering::SeqCst);
        }

        fn cancel_after_next_rename(&self, cancel: &Cancel) {
            *self.cancel_after_rename.lock().unwrap() = Some(cancel.clone());
        }

        fn cancel_while_hashing_proof(&self, cancel: &Cancel) {
            *self.cancel_during_proof.lock().unwrap() = Some(cancel.clone());
        }

        fn fail_quarantine_run_create(&self, failure: QuarantineRunCreateFailure) {
            *self.quarantine_run_create_failure.lock().unwrap() = Some(failure);
        }

        fn fail_quarantine_base_create(&self, failure: QuarantineRunCreateFailure) {
            *self.quarantine_base_create_failure.lock().unwrap() = Some(failure);
        }

        fn fail_sync_at(&self, path: &Path) {
            *self.fail_sync_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn arm_parent_attack(&self, attack: QuarantineParentAttack) {
            *self.parent_attack.lock().unwrap() = Some(attack);
        }

        fn swap_armed_parent(&self, bound_parent: &Path) {
            let Some(attack) = self.parent_attack.lock().unwrap().take() else {
                return;
            };
            let components = bound_parent.components().collect::<Vec<_>>();
            let base_index = components
                .iter()
                .position(|component| {
                    component.as_os_str() == std::ffi::OsStr::new(".doppeldecker-quarantine")
                })
                .expect("gebundener Testparent muss unter der Quarantänebasis liegen");
            let path = match attack {
                QuarantineParentAttack::Base => components[..=base_index].iter().collect(),
                QuarantineParentAttack::Run => components[..=(base_index + 1)].iter().collect(),
                QuarantineParentAttack::BoundParent => bound_parent.to_path_buf(),
                QuarantineParentAttack::SourceParent => {
                    panic!("Quell-Elternangriff braucht beide gebundenen Eltern")
                }
            };
            let displaced = path.with_extension("gebundener-elternordner");
            std::fs::rename(&path, &displaced).unwrap();
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("fremd.txt"), b"fremder-elternordner").unwrap();
            *self.parent_replacement_path.lock().unwrap() = Some(path.clone());
            *self.parent_displaced_path.lock().unwrap() = Some(displaced);
        }

        fn swap_armed_parents(&self, source_parent: &Path, parked_parent: &Path) {
            let Some(attack) = self.parent_attack.lock().unwrap().take() else {
                return;
            };
            let path = if matches!(attack, QuarantineParentAttack::SourceParent) {
                source_parent.to_path_buf()
            } else {
                let components = parked_parent.components().collect::<Vec<_>>();
                let base_index = components
                    .iter()
                    .position(|component| {
                        component.as_os_str() == std::ffi::OsStr::new(".doppeldecker-quarantine")
                    })
                    .expect("Park-Elternordner muss unter der Quarantänebasis liegen");
                match attack {
                    QuarantineParentAttack::Base => components[..=base_index].iter().collect(),
                    QuarantineParentAttack::Run => components[..=(base_index + 1)].iter().collect(),
                    QuarantineParentAttack::BoundParent => parked_parent.to_path_buf(),
                    QuarantineParentAttack::SourceParent => unreachable!(),
                }
            };
            let displaced = path.with_extension("gebundener-elternordner");
            std::fs::rename(&path, &displaced).unwrap();
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("fremd.txt"), b"fremder-elternordner").unwrap();
            *self.parent_replacement_path.lock().unwrap() = Some(path.clone());
            *self.parent_displaced_path.lock().unwrap() = Some(displaced);
        }

        fn listed_proof(&self, path: &Path) -> io::Result<ProofListEntry> {
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Testpfad besitzt keinen Parent",
                )
            })?;
            let name = path.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Testpfad besitzt keinen Namen")
            })?;
            self.inner
                .list_complete(parent)?
                .entries
                .into_iter()
                .find(|entry| entry.raw_name == name.as_encoded_bytes())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Testbeleg fehlt"))
        }

        fn proof_matches(&self, path: &Path, expected: &ProofListEntry) -> bool {
            self.listed_proof(path).is_ok_and(|current| {
                current.provider_token == expected.provider_token
                    && current.entry.is_dir == expected.entry.is_dir
                    && current.entry.is_file == expected.entry.is_file
                    && current.entry.is_symlink == expected.entry.is_symlink
                    && current.entry.device == expected.entry.device
                    && current.entry.inode == expected.entry.inode
            })
        }

        fn is_quarantine_run(path: &Path) -> bool {
            path.parent().and_then(Path::file_name)
                == Some(std::ffi::OsStr::new(".doppeldecker-quarantine"))
        }

        fn create_quarantine_run_with_phase(
            &self,
            path: &Path,
        ) -> Option<Result<(), MutationFailure>> {
            let failure =
                if path.file_name() == Some(std::ffi::OsStr::new(".doppeldecker-quarantine")) {
                    self.quarantine_base_create_failure.lock().unwrap().take()
                } else if Self::is_quarantine_run(path) {
                    self.quarantine_run_create_failure.lock().unwrap().take()
                } else {
                    None
                }?;
            Some(match failure {
                QuarantineRunCreateFailure::Unchanged => {
                    Err(MutationFailure::Unchanged(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "erzwungene mutationsfreie Quarantänelauf-Absage",
                    )))
                }
                QuarantineRunCreateFailure::OutcomeUnclear => match self.inner.create_dir(path) {
                    Ok(()) => Err(MutationFailure::OutcomeUnclear(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "erzwungener Fehler nach sichtbarer Quarantänelauf-Anlage",
                    ))),
                    Err(error) => Err(MutationFailure::OutcomeUnclear(error)),
                },
            })
        }

        fn swap_conflict(&self, path: &Path) {
            if !self.attack
                || self
                    .attack_path
                    .lock()
                    .unwrap()
                    .as_deref()
                    .is_none_or(|target| target != path)
                || self.displaced.lock().unwrap().is_some()
            {
                return;
            }
            let displaced = path.parent().unwrap().join(".konflikt-vor-queue-rename");
            std::fs::rename(path, &displaced).unwrap();
            std::fs::write(path, b"fremder-konflikt").unwrap();
            *self.displaced.lock().unwrap() = Some(displaced);
        }
    }

    impl Vfs for QueueConflictRaceVfs {
        fn is_local(&self) -> bool {
            true
        }
        fn list(&self, path: &Path) -> io::Result<Vec<crate::Entry>> {
            self.inner.list(path)
        }
        fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
            let _namespace = self.namespace.lock().unwrap();
            self.inner.list_complete(path)
        }
        fn stat(&self, path: &Path) -> io::Result<crate::Entry> {
            self.inner.stat(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }
        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }
        fn create_dir(&self, path: &Path) -> io::Result<()> {
            if let Some(result) = self.create_quarantine_run_with_phase(path) {
                return result.map_err(|failure| match failure {
                    MutationFailure::Unchanged(error) | MutationFailure::OutcomeUnclear(error) => {
                        error
                    }
                });
            }
            self.inner.create_dir(path)
        }
        fn create_dir_with_removal_proof(
            &self,
            path: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            let _namespace = self.namespace.lock().unwrap();
            if let Some(result) = self.create_quarantine_run_with_phase(path) {
                return result.map(|()| None);
            }
            self.inner
                .create_dir(path)
                .map_err(MutationFailure::Unchanged)?;
            self.listed_proof(path)
                .map(Some)
                .map_err(MutationFailure::OutcomeUnclear)
        }
        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            if let Some(cancel) = self.cancel_during_proof.lock().unwrap().take() {
                cancel.cancel();
            }
            self.inner.open_read(path)
        }
        fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(path)
        }
        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write_new(path)
        }
        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }
        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            if self.fail_sync_path.lock().unwrap().as_deref() == Some(path) {
                return Err(io::Error::other("erzwungener Quarantäne-Parent-Syncfehler"));
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
            let _namespace = self.namespace.lock().unwrap();
            self.swap_conflict(from);
            let result = self.inner.rename_noreplace(from, to);
            if result.is_ok() {
                if let Some(cancel) = self.cancel_after_rename.lock().unwrap().take() {
                    cancel.cancel();
                }
            }
            result
        }
        fn rename_noreplace_if_matches(
            &self,
            from: &Path,
            to: &Path,
            expected: &ProofListEntry,
        ) -> io::Result<()> {
            let _namespace = self.namespace.lock().unwrap();
            self.conditional_calls.fetch_add(1, Ordering::SeqCst);
            // Austausch und Vergleich liegen in derselben simulierten
            // Provider-Namespace-Sperre. Der Test erfindet den Beleg damit nicht
            // erst nach der Mutation, sondern prüft genau den zuvor gebundenen
            // Eintrag unmittelbar vor der bedingten Wirkung.
            self.swap_conflict(from);
            let parent = from.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Konflikt besitzt keinen Elternordner",
                )
            })?;
            let current = self
                .inner
                .list_complete(parent)?
                .entries
                .into_iter()
                .find(|entry| entry.raw_name == expected.raw_name);
            let matches = current.as_ref().is_some_and(|current| {
                current.provider_token == expected.provider_token
                    && current.entry.is_dir == expected.entry.is_dir
                    && current.entry.is_file == expected.entry.is_file
                    && current.entry.is_symlink == expected.entry.is_symlink
                    && current.entry.size == expected.entry.size
                    && current.entry.mtime == expected.entry.mtime
                    && current.entry.mode == expected.entry.mode
                    && current.entry.device == expected.entry.device
                    && current.entry.inode == expected.entry.inode
            });
            if !matches {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Konfliktidentität änderte sich vor dem gebundenen Rename",
                ));
            }
            self.inner.rename_noreplace(from, to)
        }
        fn supports_rename_noreplace_if_matches(&self) -> bool {
            true
        }
        fn rename_noreplace_if_matches_in_bound_parent(
            &self,
            from: &Path,
            to: &Path,
            expected: &ProofListEntry,
            bound_parent: &Path,
            expected_parent: &ProofListEntry,
        ) -> io::Result<()> {
            let _namespace = self.namespace.lock().unwrap();
            self.conditional_calls.fetch_add(1, Ordering::SeqCst);
            self.swap_conflict(from);
            self.swap_armed_parent(bound_parent);
            if to.parent() != Some(bound_parent)
                || !self.proof_matches(from, expected)
                || !self.proof_matches(bound_parent, expected_parent)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Konflikt oder gebundener Quarantäne-Elternordner wurde ausgetauscht",
                ));
            }
            let result = self.inner.rename_noreplace(from, to);
            if result.is_ok() {
                if let Some(cancel) = self.cancel_after_rename.lock().unwrap().take() {
                    cancel.cancel();
                }
            }
            result
        }
        fn supports_rename_noreplace_if_matches_in_bound_parent(&self) -> bool {
            true
        }
        fn rename_noreplace_if_matches_in_bound_parents(
            &self,
            request: BoundParentsRenameRequest<'_>,
        ) -> io::Result<()> {
            let _namespace = self.namespace.lock().unwrap();
            self.conditional_calls.fetch_add(1, Ordering::SeqCst);
            self.swap_conflict(request.from);
            self.swap_armed_parents(request.from_parent, request.to_parent);
            if self
                .manifest_attack_before_rename
                .swap(false, Ordering::SeqCst)
            {
                let required = request
                    .required_content
                    .as_ref()
                    .expect("Manifestangriff braucht einen gebundenen Intent");
                let mut manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(required.path)?)?;
                manifest["records"] = serde_json::json!([]);
                std::fs::write(required.path, serde_json::to_vec_pretty(&manifest)?)?;
            }
            let content_matches = match request.required_content {
                Some(required) => crate::atomic_file::hash_vfs_file(&self.inner, required.path)
                    .is_ok_and(|actual| *actual.as_bytes() == required.digest),
                None => true,
            };
            if request.from.parent() != Some(request.from_parent)
                || request.to.parent() != Some(request.to_parent)
                || !self.proof_matches(request.from, request.expected)
                || self.inner.root_identity(request.from_parent)? != *request.expected_from_parent
                || self.inner.root_identity(request.to_parent)? != *request.expected_to_parent
                || !content_matches
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Konflikt oder einer seiner beiden gebundenen Elternordner wurde ausgetauscht",
                ));
            }
            let result = self.inner.rename_noreplace(request.from, request.to);
            if result.is_ok() {
                if let Some(cancel) = self.cancel_after_rename.lock().unwrap().take() {
                    cancel.cancel();
                }
            }
            result
        }
        fn supports_rename_noreplace_if_matches_in_bound_parents(&self) -> bool {
            self.two_parent_support.load(Ordering::SeqCst)
        }
        fn replace_file_if_content_matches(
            &self,
            request: ConditionalContentReplaceRequest<'_>,
        ) -> Result<(), MutationFailure> {
            let _namespace = self.namespace.lock().unwrap();
            let current = std::fs::read(request.path).map_err(MutationFailure::Unchanged)?;
            if *blake3::hash(&current).as_bytes() != request.expected_digest {
                return Err(MutationFailure::Unchanged(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Manifestdigest änderte sich vor dem bedingten Austausch",
                )));
            }
            std::fs::write(request.path, request.replacement)
                .map_err(MutationFailure::OutcomeUnclear)
        }
        fn supports_replace_file_if_content_matches(&self) -> bool {
            true
        }
        fn supports_create_dir_with_removal_proof(&self) -> bool {
            true
        }
        fn root_identity(&self, path: &Path) -> io::Result<crate::RootIdentity> {
            self.inner.root_identity(path)
        }
    }

    struct NoSecrets;

    impl SecretResolver for NoSecrets {
        fn resolve(&self, _reference: &KeychainSecretReference) -> io::Result<crate::SecretString> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Lokaler Queue-Test benötigt kein Secret",
            ))
        }
    }

    fn local_bidirectional_endpoint(path: &Path) -> Box<BidirectionalQueueEndpoint> {
        Box::new(BidirectionalQueueEndpoint::Local {
            root: path.to_string_lossy().into_owned(),
        })
    }

    fn no_secrets() -> Arc<dyn SecretResolver> {
        Arc::new(NoSecrets)
    }

    struct CountingSecrets(Arc<AtomicUsize>);

    impl SecretResolver for CountingSecrets {
        fn resolve(&self, _reference: &KeychainSecretReference) -> io::Result<crate::SecretString> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Testresolver darf bei ungültigem Endpunkt nicht laufen",
            ))
        }
    }

    #[test]
    fn queue_endpunkt_validiert_root_vor_secretresolver_und_netzwerk() {
        let resolves = Arc::new(AtomicUsize::new(0));
        for invalid_root in ["relativ/ungueltig", "/daten/../fremd"] {
            let endpoint = BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(FtpsConnectionConfig {
                    host: "ftp.example.test".into(),
                    alternate_hosts: Vec::new(),
                    port: 21,
                    username: "test".into(),
                    requested_root: "/daten".into(),
                    transport_security: FtpTransportSecurity::Plaintext,
                    certificate: None,
                    certificate_policy: None,
                    password_reference: KeychainSecretReference::new("Doppeldecker", "ftp-test")
                        .unwrap(),
                }),
                root: invalid_root.into(),
            };

            let error = match endpoint.connect(Arc::new(CountingSecrets(resolves.clone()))) {
                Ok(_) => panic!("der ungültige Endpunkt darf keine Verbindung liefern"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(resolves.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_queue_gate_verwirft_nicht_normalisierte_pfade() {
        let source = BidirectionalQueueEndpoint::Local {
            root: "/quelle".into(),
        };
        let target = BidirectionalQueueEndpoint::Local {
            root: "/ziel".into(),
        };
        let malformed_file = QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(source),
            target: Box::new(target),
            items: vec!["/quelle/unter/../datei.txt".into()],
            destination: "/ziel".into(),
            overwrite: true,
            expected_conflicts: vec!["/ziel/unter/../datei.txt".into()],
            source_tree_expected: None,
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(false),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected: None,
        };
        assert_eq!(
            validate_operation(&malformed_file).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let selected = root.join("datei.txt");
        std::fs::write(&selected, b"inhalt").unwrap();
        let mut plan =
            crate::plan_remote_delete(&LocalVfs::new(), &root, std::slice::from_ref(&selected))
                .unwrap();
        let malformed = root.join("unter").join("..").join("datei.txt");
        plan.items[0].path = malformed.to_str().unwrap().to_owned();
        plan.items[0].records[0].path = malformed.to_str().unwrap().to_owned();
        let malformed_delete = QueueOperation::ProviderDelete {
            endpoint: local_bidirectional_endpoint(&root),
            mode: RemoteDeleteMode::Permanent,
            expected_plan: Box::new(plan),
        };
        assert_eq!(
            validate_operation(&malformed_delete).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn queue_verwirft_duplikate_und_verschachtelte_auswahl_vor_enqueue() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_dir = root.join("quelle/ordner");
        let source_child = source_dir.join("kind.txt");
        let target = root.join("ziel");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(&source_child, b"unveraendert").unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();

        for items in [
            vec![source_child.clone(), source_child.clone()],
            vec![source_dir.clone(), source_child.clone()],
        ] {
            let error = queue
                .enqueue(QueueOperation::File {
                    kind: FileQueueKind::Move,
                    items: items
                        .iter()
                        .map(|path| path.to_str().unwrap().to_owned())
                        .collect(),
                    destination: target.to_str().unwrap().to_owned(),
                    overwrite: false,
                    expected_conflicts: Vec::new(),
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: Vec::new(),
                    local_replace_best_effort_expected: Some(false),
                    local_copy_new_best_effort_expected: None,
                })
                .expect_err("überlappende Auswahl darf nicht ins Journal gelangen");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }

        assert!(queue.snapshot().entries.is_empty());
        assert_eq!(std::fs::read(&source_child).unwrap(), b"unveraendert");
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    }

    #[test]
    fn provider_datei_sandbox_prueft_lokale_probe_roots_vor_providerkontakt() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_root = std::fs::canonicalize(outside.path()).unwrap();
        let selected = outside_root.join("file.txt");
        std::fs::write(&selected, b"original").unwrap();
        let remote = Box::new(BidirectionalQueueEndpoint::Ftps {
            connection: Box::new(FtpsConnectionConfig {
                host: "unreachable.invalid".into(),
                alternate_hosts: vec![],
                port: 21,
                username: "test".into(),
                requested_root: "/".into(),
                transport_security: FtpTransportSecurity::Plaintext,
                certificate: None,
                certificate_policy: None,
                password_reference: KeychainSecretReference::new("test", "missing").unwrap(),
            }),
            root: "/remote".into(),
        });
        let queue = OperationQueue::open(sandbox.path().join("queue.json")).unwrap();
        let execution = LocalQueueExecution {
            sandbox: Some(sandbox.path().into()),
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 1.0,
        };
        for moving in [false, true] {
            let operation = QueueOperation::ProviderFile {
                kind: if moving {
                    FileQueueKind::Move
                } else {
                    FileQueueKind::Copy
                },
                source: if moving {
                    local_bidirectional_endpoint(&outside_root)
                } else {
                    remote.clone()
                },
                target: if moving {
                    remote.clone()
                } else {
                    local_bidirectional_endpoint(&outside_root)
                },
                items: vec![if moving {
                    selected.to_str().unwrap().into()
                } else {
                    "/remote/file.txt".into()
                }],
                destination: if moving {
                    "/remote".into()
                } else {
                    outside_root.to_str().unwrap().into()
                },
                overwrite: false,
                expected_conflicts: vec![],
                source_tree_expected: None,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: vec![],
                best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(true),
                replace_best_effort_expected: Some(false),
                remove_source_best_effort_expected: moving.then_some(true),
            };
            queue.enqueue(operation).unwrap();
            let entry = queue.claim_next().unwrap().unwrap();
            let resolves = Arc::new(AtomicUsize::new(0));
            let result = execute_provider_file_queue_entry_detailed(
                &queue,
                entry,
                &execution,
                Arc::new(CountingSecrets(resolves.clone())),
                &Cancel::new(),
                &mut |_| {},
            );
            assert_eq!(
                resolves.load(Ordering::SeqCst),
                0,
                "Sandbox muss vor Secret und Capabilityprobe greifen"
            );
            let error = match result {
                Err(failure) => failure.error,
                Ok(_) => panic!("Sandboxverletzung wurde nicht abgelehnt"),
            };
            assert_eq!(
                crate::error_code(&error),
                crate::ErrorCode::SandboxViolation
            );
        }
        assert_eq!(std::fs::read(selected).unwrap(), b"original");
    }

    #[test]
    fn oeffentliche_provider_executoren_validieren_vor_secret_und_netzwerk() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let selected = root.join("datei.txt");
        std::fs::write(&selected, b"inhalt").unwrap();
        let delete_plan =
            crate::plan_remote_delete(&LocalVfs::new(), &root, std::slice::from_ref(&selected))
                .unwrap();
        let invalid_endpoint = Box::new(BidirectionalQueueEndpoint::Ftps {
            connection: Box::new(FtpsConnectionConfig {
                host: "ftp.example.test".into(),
                alternate_hosts: Vec::new(),
                port: 21,
                username: "test".into(),
                requested_root: "/daten".into(),
                transport_security: FtpTransportSecurity::Plaintext,
                certificate: None,
                certificate_policy: None,
                password_reference: KeychainSecretReference::new("Doppeldecker", "ftp-test")
                    .unwrap(),
            }),
            root: "relativ/ungueltig".into(),
        });
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let secrets: Arc<dyn SecretResolver> = Arc::new(CountingSecrets(resolves.clone()));
        let execution = LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 1.0,
        };
        let make_entry = |operation_id: &str, operation| QueueEntry {
            operation_id: operation_id.into(),
            operation,
            profile_bindings: Vec::new(),
            created_unix_ms: 0,
            state: QueueState::Validating,
            attempts: 1,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        };

        let file_failure = match execute_provider_file_queue_entry_detailed(
            &queue,
            make_entry(
                "ungueltig-file",
                QueueOperation::ProviderFile {
                    kind: FileQueueKind::Copy,
                    source: invalid_endpoint.clone(),
                    target: local_bidirectional_endpoint(&root),
                    items: vec![selected.to_string_lossy().into_owned()],
                    destination: root.to_string_lossy().into_owned(),
                    overwrite: false,
                    expected_conflicts: Vec::new(),
                    source_tree_expected: None,
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: Vec::new(),
                    best_effort_commit_expected: None,
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: Some(false),
                    replace_best_effort_expected: Some(false),
                    remove_source_best_effort_expected: None,
                },
            ),
            &execution,
            secrets.clone(),
            &Cancel::new(),
            &mut |_| {},
        ) {
            Ok(_) => panic!("ungültiger ProviderFile-Eintrag wurde ausgeführt"),
            Err(failure) => failure,
        };
        assert_eq!(file_failure.error.kind(), io::ErrorKind::InvalidInput);

        let delete_error = match execute_provider_delete_queue_entry(
            &queue,
            make_entry(
                "ungueltig-delete",
                QueueOperation::ProviderDelete {
                    endpoint: invalid_endpoint,
                    mode: RemoteDeleteMode::Permanent,
                    expected_plan: Box::new(delete_plan),
                },
            ),
            secrets,
            &Cancel::new(),
            &mut |_| {},
        ) {
            Ok(_) => panic!("ungültiger ProviderDelete-Eintrag wurde ausgeführt"),
            Err(error) => error,
        };
        assert_eq!(delete_error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(resolves.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bidirektionaler_executor_validiert_eingaben_vor_secret_und_netzwerk() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel");
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let entry = QueueEntry {
            operation_id: "ungueltig-bidirektional".into(),
            operation: QueueOperation::Bidirectional {
                pair_id: PairId([7; 16]),
                pair_name: " ungueltig ".into(),
                left: sftp_endpoint("ungueltig-bidirektional", "/daten"),
                right: local_bidirectional_endpoint(&target),
                baseline_path: root.join("state/pair.json").to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: None,
                expected_plan_digest: [8; 32],
            },
            profile_bindings: Vec::new(),
            created_unix_ms: 0,
            state: QueueState::Validating,
            attempts: 1,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        };

        let error = prepare_bidirectional_queue_entry(
            &queue,
            entry,
            Arc::new(CountingSecrets(resolves.clone())),
        )
        .err()
        .expect("ungültige Paardaten müssen vor der Verbindung scheitern");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(resolves.load(Ordering::SeqCst), 0);

        let entry = QueueEntry {
            operation_id: "ungueltige-entscheidung".into(),
            operation: QueueOperation::Bidirectional {
                pair_id: PairId([7; 16]),
                pair_name: "Gültiges Paar".into(),
                left: sftp_endpoint("ungueltige-entscheidung", "/daten"),
                right: local_bidirectional_endpoint(&target),
                baseline_path: root.join("state/pair.json").to_string_lossy().into_owned(),
                resolutions: vec![ConflictResolution {
                    path: crate::BinaryRelativePath(vec![b"..".to_vec()]),
                    decision: crate::ConflictDecision::Skip,
                }],
                expected_baseline_digest: None,
                expected_plan_digest: [8; 32],
            },
            profile_bindings: Vec::new(),
            created_unix_ms: 0,
            state: QueueState::Validating,
            attempts: 1,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        };

        let error = prepare_bidirectional_queue_entry(
            &queue,
            entry,
            Arc::new(CountingSecrets(resolves.clone())),
        )
        .err()
        .expect("ungültige Konfliktentscheidung muss vor der Verbindung scheitern");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(resolves.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn namespace_permission_denied_wird_nicht_zu_secret_attention() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        let missing_local = BidirectionalQueueEndpoint::Local {
            root: root.join("fehlt").to_str().unwrap().to_owned(),
        };
        let connect_error = connect_queue_endpoint_with_credential_attention(
            &queue,
            "kein-eintrag",
            &missing_local,
            Arc::new(NoSecrets),
        )
        .err()
        .unwrap();
        assert_eq!(connect_error.kind(), io::ErrorKind::NotFound);
        assert!(queue.snapshot().entries.is_empty());

        let nested = root.join("unterordner");
        std::fs::create_dir(&nested).unwrap();
        let source = ProviderEndpoint::local(&root).unwrap();
        let target = ProviderEndpoint::local(&nested).unwrap();

        let error = bind_connected_queue_endpoints(source, target, false, &nested)
            .err()
            .unwrap();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("ineinander verschachtelt"));
    }

    #[test]
    fn ftps_verbindungsteilung_erhaelt_konto_secret_und_profilroot() {
        let connection = crate::FtpsConnectionConfig {
            host: "127.0.0.1".into(),
            alternate_hosts: vec![],
            port: 21,
            username: "first".into(),
            requested_root: "/profile".into(),
            transport_security: crate::FtpTransportSecurity::Plaintext,
            certificate: None,
            certificate_policy: None,
            password_reference: crate::KeychainSecretReference::new("Doppeldecker-Test", "first")
                .unwrap(),
        };
        let endpoint = |connection| BidirectionalQueueEndpoint::Ftps {
            connection: Box::new(connection),
            root: "/pane".into(),
        };
        let source = endpoint(connection.clone());
        let mut same = endpoint(connection.clone());
        if let BidirectionalQueueEndpoint::Ftps { root, .. } = &mut same {
            *root = "/other".into();
        }
        assert!(source.shares_connection_definition(&same));
        for field in ["account", "secret", "root"] {
            let mut changed = connection.clone();
            match field {
                "account" => changed.username = "second".into(),
                "secret" => {
                    changed.password_reference =
                        crate::KeychainSecretReference::new("Doppeldecker-Test", "second").unwrap()
                }
                _ => changed.requested_root = "/other-profile".into(),
            }
            assert!(
                !source.shares_connection_definition(&endpoint(changed)),
                "Verbindungsdefinition verlor {field}"
            );
        }
    }

    #[test]
    fn ftps_verbindungsteilung_erhaelt_den_beobachteten_kanonischen_root() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut sessions = Vec::new();
            for canonical in ["/first", "/second"] {
                let (mut control, _) = listener.accept().unwrap();
                sessions.push(std::thread::spawn(move || {
                    control
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(control.try_clone().unwrap());
                    control.write_all(b"220 Root mapping test\r\n").unwrap();
                    let mut data_listener = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 {
                            break;
                        }
                        let command = line.trim_end_matches(['\r', '\n']);
                        let response = match command {
                            value if value.starts_with("USER ") => {
                                "331 Password required\r\n".into()
                            }
                            value if value.starts_with("PASS ") => "230 Logged in\r\n".into(),
                            "FEAT" => {
                                "211-Features\r\n MLST Type;Size;Modify;\r\n UTF8\r\n211 End\r\n"
                                    .into()
                            }
                            value if value.starts_with("OPTS ") || value == "TYPE I" => {
                                "200 Accepted\r\n".into()
                            }
                            value if value.starts_with("CWD ") => "250 Changed\r\n".into(),
                            "PWD" => format!("257 \"{canonical}\" is current directory\r\n"),
                            "EPSV" => {
                                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                                let port = listener.local_addr().unwrap().port();
                                data_listener = Some(listener);
                                format!("229 Entering Extended Passive Mode (|||{port}|)\r\n")
                            }
                            "MLSD" => {
                                control.write_all(b"150 Listing\r\n").unwrap();
                                let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                                data.write_all(b"type=dir;modify=20261018170000; from\r\ntype=dir;modify=20261018170000; to\r\n")
                                    .unwrap();
                                drop(data);
                                "226 Complete\r\n".into()
                            }
                            "QUIT" => break,
                            _ => panic!("Unerwarteter Befehl in rein lesender Rootprobe"),
                        };
                        if control.write_all(response.as_bytes()).is_err() {
                            break;
                        }
                    }
                }));
            }
            for session in sessions {
                session.join().unwrap();
            }
        });
        struct Secrets;
        impl crate::SecretResolver for Secrets {
            fn resolve(
                &self,
                _: &crate::KeychainSecretReference,
            ) -> io::Result<crate::SecretString> {
                crate::SecretString::new("anonymous-test".into())
            }
        }
        let connection = crate::FtpsConnectionConfig {
            host: "127.0.0.1".into(),
            alternate_hosts: vec![],
            port,
            username: "anonymous".into(),
            requested_root: "/".into(),
            transport_security: crate::FtpTransportSecurity::Plaintext,
            certificate: None,
            certificate_policy: None,
            password_reference: crate::KeychainSecretReference::new("Doppeldecker-Test", "mapping")
                .unwrap(),
        };
        let source = ProviderEndpoint::ftps(
            Arc::new(crate::FtpsVfs::connect(connection.clone(), Arc::new(Secrets)).unwrap()),
            Path::new("/from"),
        )
        .unwrap();
        let target = ProviderEndpoint::ftps(
            Arc::new(crate::FtpsVfs::connect(connection, Arc::new(Secrets)).unwrap()),
            Path::new("/to"),
        )
        .unwrap();
        let (_, target, shared) =
            bind_connected_queue_endpoints(source, target, true, Path::new("/to")).unwrap();
        assert!(
            !shared,
            "Die Quellverbindung würde den Zielroot anders abbilden"
        );
        let identity = target
            .file_context()
            .unwrap()
            .0
            .root_identity(Path::new("/to"))
            .unwrap();
        let crate::real_mode::RootIdentity::Ftps(identity) = identity else {
            panic!("FTPS-Root erwartet");
        };
        assert_eq!(identity.server.canonical_root, "/second");
        drop(target);
        server.join().unwrap();
    }

    #[test]
    fn provider_sync_erhaelt_den_unabhaengig_beobachteten_ftps_zielroot() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        use std::os::unix::fs::PermissionsExt;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let probe_connections = Arc::new(AtomicUsize::new(0));
        let server_probes = probe_connections.clone();
        let server = std::thread::spawn(move || {
            let mut sessions = Vec::new();
            let mut accepted = 0;
            while !server_stop.load(Ordering::SeqCst) {
                let (mut control, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                control.set_nonblocking(false).unwrap();
                accepted += 1;
                if accepted > 2 {
                    // Die Zielprobe darf erst nach korrekter Rootbindung starten.
                    // Der künstliche Server lehnt sie vor Login und jeder Mutation ab.
                    server_probes.fetch_add(1, Ordering::SeqCst);
                    control
                        .write_all(b"421 Probe intentionally unavailable\r\n")
                        .unwrap();
                    continue;
                }
                let (canonical, name) = if accepted == 1 {
                    ("/first", "from")
                } else {
                    ("/second", "to")
                };
                sessions.push(std::thread::spawn(move || {
                    control
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(control.try_clone().unwrap());
                    control
                        .write_all(b"220 Sync root mapping test\r\n")
                        .unwrap();
                    let mut data_listener = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 {
                            break;
                        }
                        let command = line.trim_end_matches(['\r', '\n']);
                        let response = match command {
                            value if value.starts_with("USER ") => {
                                "331 Password required\r\n".into()
                            }
                            value if value.starts_with("PASS ") => "230 Logged in\r\n".into(),
                            "FEAT" => {
                                "211-Features\r\n MLST Type;Size;Modify;\r\n UTF8\r\n211 End\r\n"
                                    .into()
                            }
                            value if value.starts_with("OPTS ") || value == "TYPE I" => {
                                "200 Accepted\r\n".into()
                            }
                            value if value.starts_with("CWD ") => "250 Changed\r\n".into(),
                            "PWD" => format!("257 \"{canonical}\" is current directory\r\n"),
                            "EPSV" => {
                                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                                let port = listener.local_addr().unwrap().port();
                                data_listener = Some(listener);
                                format!("229 Entering Extended Passive Mode (|||{port}|)\r\n")
                            }
                            "MLSD" => {
                                control.write_all(b"150 Listing\r\n").unwrap();
                                let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                                write!(data, "type=dir;modify=20261019000000; {name}\r\n").unwrap();
                                drop(data);
                                "226 Complete\r\n".into()
                            }
                            "QUIT" => break,
                            _ => {
                                panic!("Unerwarteter Befehl in rein lesender Rootprobe: {command}")
                            }
                        };
                        if control.write_all(response.as_bytes()).is_err() {
                            break;
                        }
                    }
                }));
            }
            for session in sessions {
                session.join().unwrap();
            }
        });
        struct Secrets;
        impl crate::SecretResolver for Secrets {
            fn resolve(
                &self,
                _: &crate::KeychainSecretReference,
            ) -> io::Result<crate::SecretString> {
                crate::SecretString::new("anonymous-test".into())
            }
        }
        let connection = crate::FtpsConnectionConfig {
            host: "127.0.0.1".into(),
            alternate_hosts: vec![],
            port,
            username: "anonymous".into(),
            requested_root: "/".into(),
            transport_security: crate::FtpTransportSecurity::Plaintext,
            certificate: None,
            certificate_policy: None,
            password_reference: crate::KeychainSecretReference::new(
                "Doppeldecker-Test",
                "sync-mapping",
            )
            .unwrap(),
        };
        let endpoint = |root: &str| {
            Box::new(BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(connection.clone()),
                root: root.into(),
            })
        };
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::local_fs::write_acl_exact(&root, None).unwrap();
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        assert!(queue._process_lock.root.has_exclusive_namespace().unwrap());
        let entry = queue
            .enqueue_claimed(QueueOperation::ProviderSync {
                source: endpoint("/from"),
                target: endpoint("/to"),
                options: SyncOptions::default(),
                expected_plan: Box::default(),
            })
            .unwrap();
        let result = prepare_provider_queue_entry(&queue, entry, Arc::new(Secrets));
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        let error = result
            .err()
            .expect("Der Testserver verweigert die Capability-Verbindung");
        assert!(
            probe_connections.load(Ordering::SeqCst) > 0,
            "Sync verlor den unabhängig beobachteten Zielroot vor der Capability-Grenze: {error}"
        );
        assert!(queue.snapshot().entries[0].attention.is_none());
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Validating);
    }

    #[test]
    fn provider_sync_namespacefehler_wird_nicht_zu_secret_attention() {
        let temp = tempfile::tempdir().unwrap();
        let source = std::fs::canonicalize(temp.path()).unwrap();
        let target = source.join("unterordner");
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(source.join("state/queue.json")).unwrap();
        let queued = queue
            .enqueue(QueueOperation::ProviderSync {
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                options: SyncOptions::default(),
                expected_plan: Box::new(SyncPlan::default()),
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();

        let error = prepare_provider_queue_entry(&queue, claimed, no_secrets())
            .err()
            .expect("verschachtelte Laufzeit-Roots müssen unverändert scheitern");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let entry = queue
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == queued.operation_id)
            .unwrap();
        assert_eq!(entry.state, QueueState::Validating);
        assert!(entry.attention.is_none());
        assert!(!entry
            .error
            .as_deref()
            .is_some_and(|message| message.contains("SECRET")));
    }

    #[test]
    fn protokollvorbereitung_unterscheidet_zugangsdaten_und_namespacefehler() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let entry = queue.enqueue_claimed(file_operation("namespace")).unwrap();
        let error = prepare_protocol_queue_run(
            &queue,
            entry.operation_id.clone(),
            &SyncPlan::default(),
            "Test",
            || Ok(LocalVfs::new()),
            |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Root ist nicht zugänglich",
                ))
            },
        )
        .err()
        .expect("Rootfehler muss erhalten bleiben");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(queue.snapshot().entries[0].attention.is_none());
        let result = prepare_protocol_queue_run(
            &queue,
            entry.operation_id,
            &SyncPlan::default(),
            "Test",
            || {
                Err::<LocalVfs, _>(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Secret abgelehnt",
                ))
            },
            |_| panic!("ohne Verbindung darf die Vorbereitung nicht starten"),
        )
        .unwrap();
        assert!(matches!(
            result,
            SftpQueuePreparation::AwaitingConfirmation { .. }
        ));
        assert!(matches!(
            queue.snapshot().entries[0].attention,
            Some(QueueAttention::Manual { .. })
        ));
    }

    #[test]
    fn provider_delete_prueft_plan_vor_und_nach_capability_probe() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let selected = root.join("datei.txt");
        std::fs::write(&selected, b"alt").unwrap();
        let vfs = LocalVfs::new();
        let expected =
            crate::plan_remote_delete(&vfs, &root, std::slice::from_ref(&selected)).unwrap();

        std::fs::write(&selected, b"vorher geaendert").unwrap();
        let probe_called = std::cell::Cell::new(false);
        let error = bind_provider_delete_around_probe(&vfs, &root, &expected, || {
            probe_called.set(true);
            Ok(())
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(!probe_called.get(), "Drift muss vor der Probe stoppen");

        std::fs::write(&selected, b"frisch").unwrap();
        let expected =
            crate::plan_remote_delete(&vfs, &root, std::slice::from_ref(&selected)).unwrap();
        let error = bind_provider_delete_around_probe(&vfs, &root, &expected, || {
            std::fs::write(&selected, b"in der Probe geaendert").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(&selected).unwrap(), b"in der Probe geaendert");
    }

    fn file_operation(name: &str) -> QueueOperation {
        QueueOperation::File {
            kind: FileQueueKind::Copy,
            items: vec![format!("/tmp/{name}")],
            destination: "/tmp/ziel".into(),
            overwrite: false,
            expected_conflicts: Vec::new(),
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            local_replace_best_effort_expected: Some(false),
            local_copy_new_best_effort_expected: None,
        }
    }

    fn webdav_delete_operation() -> QueueOperation {
        let root = "/daten";
        let selected = "/daten/datei.txt";
        let connection = WebDavConnectionConfig {
            origin: "https://dav.example.test".into(),
            username: "test".into(),
            requested_root: root.into(),
            certificate: FtpsCertificatePin { sha256: [4; 32] },
            certificate_policy: FtpsCertificatePolicy::WebPkiAndPin,
            password_reference: KeychainSecretReference::new("Doppeldecker", "dav-test").unwrap(),
        };
        let root_identity = crate::RootIdentity::WebDav(crate::WebDavRootIdentity {
            server: crate::WebDavServerIdentity {
                origin: connection.origin.clone(),
                certificate: connection.certificate.clone(),
                account_digest: [5; 32],
                canonical_root: root.into(),
                dav_header_digest: [6; 32],
                server_header_digest: [7; 32],
                legacy_server_header_digests: Vec::new(),
            },
            virtual_root: root.into(),
            root_etag: "\"root-42\"".into(),
            session_generation: 3,
        });
        QueueOperation::ProviderDelete {
            endpoint: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(connection),
                root: root.into(),
            }),
            mode: RemoteDeleteMode::Permanent,
            expected_plan: Box::new(RemoteDeletePlan {
                root: root.into(),
                root_identity,
                items: vec![crate::RemoteDeleteItem {
                    path: selected.into(),
                    is_dir: false,
                    entries: 1,
                    bytes: 7,
                    digest: [8; 32],
                    source_proof: crate::RemoteDeleteSourceProof::default(),
                    records: vec![crate::RemoteDeleteRecord {
                        path: selected.into(),
                        is_dir: false,
                        size: 7,
                        provider_token: Some(b"\"datei-1\"".to_vec()),
                        content_digest: Some([9; 32]),
                    }],
                }],
                entries: 1,
                bytes: 7,
                digest: [10; 32],
            }),
        }
    }

    #[test]
    fn queue_snapshots_tragen_eine_persistente_monotone_revision() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        assert_eq!(queue.snapshot().revision, 0);

        queue.enqueue(file_operation("revision")).unwrap();
        let after_enqueue = queue.snapshot().revision;
        assert_eq!(after_enqueue, 1);
        queue.set_paused(true).unwrap();
        let after_pause = queue.snapshot().revision;
        assert_eq!(after_pause, 2);
        drop(queue);

        let reopened = OperationQueue::open(path).unwrap();
        assert_eq!(reopened.snapshot().revision, after_pause);
        assert!(reopened.snapshot().revision > after_enqueue);
    }

    fn late_commit_operations(root: &Path) -> Vec<(&'static str, QueueOperation)> {
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let source_file = source.join("datei.txt");
        std::fs::write(&source_file, b"inhalt").unwrap();
        let delete_plan = crate::plan_remote_delete(
            &LocalVfs::new(),
            &source,
            std::slice::from_ref(&source_file),
        )
        .unwrap();
        let source_endpoint = local_bidirectional_endpoint(&source);
        let target_endpoint = local_bidirectional_endpoint(&target);
        let ftps_connection = FtpsConnectionConfig {
            host: "ftp.example.test".into(),
            alternate_hosts: Vec::new(),
            port: 21,
            username: "test".into(),
            requested_root: "/daten".into(),
            transport_security: FtpTransportSecurity::Plaintext,
            certificate: None,
            certificate_policy: None,
            password_reference: KeychainSecretReference::new("Doppeldecker", "ftp-test").unwrap(),
        };
        let webdav_connection = WebDavConnectionConfig {
            origin: "https://dav.example.test".into(),
            username: "test".into(),
            requested_root: "/daten".into(),
            certificate: FtpsCertificatePin { sha256: [4; 32] },
            certificate_policy: FtpsCertificatePolicy::WebPkiAndPin,
            password_reference: KeychainSecretReference::new("Doppeldecker", "dav-test").unwrap(),
        };
        vec![
            ("lokale Datei", file_operation("late-commit")),
            (
                "Provider-Datei",
                QueueOperation::ProviderFile {
                    kind: FileQueueKind::Copy,
                    source: source_endpoint.clone(),
                    target: target_endpoint.clone(),
                    items: vec![source_file.to_string_lossy().into_owned()],
                    destination: target.to_string_lossy().into_owned(),
                    overwrite: false,
                    expected_conflicts: Vec::new(),
                    source_tree_expected: None,
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: Vec::new(),
                    best_effort_commit_expected: None,
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: Some(false),
                    replace_best_effort_expected: Some(false),
                    remove_source_best_effort_expected: None,
                },
            ),
            (
                "Provider-Löschen",
                QueueOperation::ProviderDelete {
                    endpoint: source_endpoint.clone(),
                    mode: RemoteDeleteMode::Permanent,
                    expected_plan: Box::new(delete_plan),
                },
            ),
            (
                "lokaler Sync",
                QueueOperation::Sync {
                    source: source.to_string_lossy().into_owned(),
                    target: target.to_string_lossy().into_owned(),
                    sandbox: None,
                    options: SyncOptions::default(),
                    expected_plan: Box::new(SyncPlan::default()),
                },
            ),
            ("SFTP", sftp_operation()),
            (
                "FTP/FTPS",
                QueueOperation::FtpsSync {
                    direction: SftpQueueDirection::Upload,
                    connection: Box::new(ftps_connection),
                    local_root: source.to_string_lossy().into_owned(),
                    remote_root: "/ziel".into(),
                    options: SyncOptions::default(),
                    expected_plan: Box::new(SyncPlan::default()),
                },
            ),
            (
                "WebDAV",
                QueueOperation::WebDavSync {
                    direction: SftpQueueDirection::Upload,
                    connection: Box::new(webdav_connection),
                    local_root: source.to_string_lossy().into_owned(),
                    remote_root: "/ziel".into(),
                    options: SyncOptions::default(),
                    expected_plan: Box::new(SyncPlan::default()),
                },
            ),
            ("SMB", smb_operation()),
            (
                "Provider-Sync",
                QueueOperation::ProviderSync {
                    source: source_endpoint.clone(),
                    target: target_endpoint.clone(),
                    options: SyncOptions::default(),
                    expected_plan: Box::new(SyncPlan::default()),
                },
            ),
            (
                "bidirektional",
                QueueOperation::Bidirectional {
                    pair_id: PairId([7; 16]),
                    pair_name: "Late Commit".into(),
                    left: source_endpoint,
                    right: target_endpoint,
                    baseline_path: root.join("state/pair.json").to_string_lossy().into_owned(),
                    resolutions: Vec::new(),
                    expected_baseline_digest: None,
                    expected_plan_digest: [8; 32],
                },
            ),
        ]
    }

    fn make_sync_options_invalid(operation: &mut QueueOperation) -> bool {
        let options = match operation {
            QueueOperation::Sync { options, .. }
            | QueueOperation::SftpSync { options, .. }
            | QueueOperation::FtpsSync { options, .. }
            | QueueOperation::WebDavSync { options, .. }
            | QueueOperation::SmbSync { options, .. }
            | QueueOperation::ProviderSync { options, .. } => options,
            _ => return false,
        };
        options.exclude = vec!["../ausbruch".into()];
        true
    }

    #[test]
    fn alle_sync_queueklassen_validieren_optionen_vor_persistenz_und_verbindung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let secrets: Arc<dyn SecretResolver> = Arc::new(CountingSecrets(resolves.clone()));
        let execution = LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 1.0,
        };

        for (index, (name, mut operation)) in late_commit_operations(&root)
            .into_iter()
            .filter(|(_, operation)| {
                matches!(
                    operation,
                    QueueOperation::Sync { .. }
                        | QueueOperation::SftpSync { .. }
                        | QueueOperation::FtpsSync { .. }
                        | QueueOperation::WebDavSync { .. }
                        | QueueOperation::SmbSync { .. }
                        | QueueOperation::ProviderSync { .. }
                )
            })
            .enumerate()
        {
            assert!(make_sync_options_invalid(&mut operation));
            let queue =
                OperationQueue::open(root.join(format!("sync-invalid-{index}.json"))).unwrap();
            let enqueue_error = queue.enqueue(operation.clone()).unwrap_err();
            assert_eq!(enqueue_error.kind(), io::ErrorKind::InvalidInput, "{name}");
            assert!(queue.snapshot().entries.is_empty(), "{name}");

            // Ein bereits persistierter Altstand wird unmittelbar vor der
            // Ausführung erneut geprüft. Auch hier darf kein Secretresolver laufen.
            let entry = QueueEntry {
                operation_id: format!("ungueltig-{index}"),
                operation: operation.clone(),
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Validating,
                attempts: 1,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            };
            let before = resolves.load(Ordering::SeqCst);
            let result: io::Result<()> = match operation {
                QueueOperation::Sync { .. } => execute_local_queue_entry(
                    &queue,
                    entry,
                    &execution,
                    &Cancel::new(),
                    &mut |_| {},
                )
                .map(|_| ()),
                QueueOperation::SftpSync { .. } => {
                    prepare_sftp_queue_entry(&queue, entry, secrets.clone()).map(|_| ())
                }
                QueueOperation::FtpsSync { .. } => {
                    prepare_ftps_queue_entry(&queue, entry, secrets.clone()).map(|_| ())
                }
                QueueOperation::WebDavSync { .. } => {
                    prepare_webdav_queue_entry(&queue, entry, secrets.clone()).map(|_| ())
                }
                QueueOperation::SmbSync { .. } => {
                    prepare_smb_queue_entry(&queue, entry, secrets.clone()).map(|_| ())
                }
                QueueOperation::ProviderSync { .. } => {
                    prepare_provider_queue_entry(&queue, entry, secrets.clone()).map(|_| ())
                }
                _ => unreachable!(),
            };
            let execute_error = result.unwrap_err();
            assert_eq!(execute_error.kind(), io::ErrorKind::InvalidInput, "{name}");
            assert_eq!(resolves.load(Ordering::SeqCst), before, "{name}");
        }
    }

    #[test]
    fn enqueue_erkennt_spaeten_commit_fuer_jede_tauri_operationsklasse() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for (index, (name, operation)) in late_commit_operations(&root).into_iter().enumerate() {
            let path = root.join(format!("state/queue-{index}.json"));
            let queue = OperationQueue::open(&path).unwrap();
            let entry = queue
                .enqueue_reconciled_using(operation.clone(), |path, snapshot| {
                    write_snapshot(path, snapshot)?;
                    Err(io::Error::other(
                        "Rename wirkte, Eltern-fsync meldet Fehler",
                    ))
                })
                .unwrap_or_else(|error| panic!("{name}: später Commit blieb unklar: {error}"));
            let disk = read_snapshot(&path).unwrap();
            assert_eq!(disk, queue.snapshot(), "{name}: RAM und Disk driften");
            assert_eq!(disk.entries.as_slice(), [entry], "{name}: Doppelenqueue");
        }
    }

    #[test]
    fn enqueue_vor_rename_fehler_bleibt_ohne_doppelten_eintrag_retryfaehig() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let operation = file_operation("retry");
        let error = queue
            .enqueue_reconciled_using(operation.clone(), |_, _| {
                Err(io::Error::other("Schreiben stoppte sicher vor Rename"))
            })
            .unwrap_err();
        assert!(error.to_string().contains("vor Rename"));
        assert!(queue.snapshot().entries.is_empty());

        let entry = queue.enqueue_reconciled(operation).unwrap();
        assert_eq!(queue.snapshot().entries.as_slice(), [entry]);
        assert_eq!(read_snapshot(&path).unwrap(), queue.snapshot());
    }

    #[test]
    fn enqueue_reconcile_akzeptiert_keinen_duplikat_oder_zusatzeintrag() {
        let temp = tempfile::tempdir().unwrap();
        for (name, mutate) in [
            (
                "Duplikat",
                (|persisted: &mut QueueSnapshot| {
                    persisted.entries.push(persisted.entries[0].clone());
                }) as fn(&mut QueueSnapshot),
            ),
            ("Zusatz", |persisted: &mut QueueSnapshot| {
                let mut extra = persisted.entries[0].clone();
                extra.operation_id = "fremder-zusatzeintrag".into();
                persisted.entries.push(extra);
            }),
        ] {
            let path = temp.path().join(format!("state/{name}.json"));
            let queue = OperationQueue::open(&path).unwrap();
            let error = queue
                .enqueue_reconciled_using(file_operation(name), |path, snapshot| {
                    let mut persisted = snapshot.clone();
                    mutate(&mut persisted);
                    write_snapshot(path, &persisted)?;
                    Err(io::Error::other("Schreiben meldet einen späten Fehler"))
                })
                .unwrap_err();

            assert_eq!(
                crate::error_code(&error),
                crate::ErrorCode::OutcomeUnclear,
                "{name} darf den erwarteten Eintrag nicht als Commitbeweis verwenden"
            );
            let retry = queue
                .enqueue_reconciled(file_operation("gesperrt"))
                .unwrap_err();
            assert_eq!(crate::error_code(&retry), crate::ErrorCode::OutcomeUnclear);
        }
    }

    #[test]
    fn enqueue_reconcile_akzeptiert_keine_veraenderte_operation_oder_state() {
        let temp = tempfile::tempdir().unwrap();
        for (name, mutate) in [
            (
                "andere-operation",
                (|persisted: &mut QueueSnapshot| {
                    persisted.entries[0].operation = file_operation("manipuliert");
                }) as fn(&mut QueueSnapshot),
            ),
            ("anderer-state", |persisted: &mut QueueSnapshot| {
                persisted.entries[0].state = QueueState::Running;
            }),
        ] {
            let path = temp.path().join(format!("state/{name}.json"));
            let queue = OperationQueue::open(&path).unwrap();
            let error = queue
                .enqueue_reconciled_using(file_operation(name), |path, snapshot| {
                    let mut persisted = snapshot.clone();
                    mutate(&mut persisted);
                    write_snapshot(path, &persisted)?;
                    Err(io::Error::other("Schreiben meldet einen späten Fehler"))
                })
                .unwrap_err();

            assert_eq!(
                crate::error_code(&error),
                crate::ErrorCode::OutcomeUnclear,
                "{name} darf trotz gleicher ID nicht als Commitbeweis gelten"
            );
        }
    }

    #[test]
    fn unklarer_enqueue_readback_sperrt_jede_weitere_queue_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let error = queue
            .enqueue_reconciled_using(file_operation("unklar"), |path, snapshot| {
                let mut foreign = snapshot.clone();
                foreign.entries[0].operation = file_operation("fremd");
                write_snapshot(path, &foreign)?;
                Err(io::Error::other("Rename wirkte mit unerwartetem Stand"))
            })
            .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let retry = queue
            .enqueue_reconciled(file_operation("nicht-mehr-schreiben"))
            .unwrap_err();
        assert_eq!(crate::error_code(&retry), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(read_snapshot(&path).unwrap().entries.len(), 1);
    }

    #[test]
    fn queue_transaktionen_klaeren_spaete_commits_und_sperren_unklare_readbacks() {
        #[derive(Clone, Copy, Debug)]
        enum Mutation {
            MarkRunning,
            Progress,
            Complete,
            Cancel,
            Fail,
            Pause,
        }

        let temp = tempfile::tempdir().unwrap();
        for (index, mutation) in [
            Mutation::MarkRunning,
            Mutation::Progress,
            Mutation::Complete,
            Mutation::Cancel,
            Mutation::Fail,
            Mutation::Pause,
        ]
        .into_iter()
        .enumerate()
        {
            let path = temp.path().join(format!("late/queue-{index}.json"));
            let queue = OperationQueue::open(&path).unwrap();
            let queued = queue.enqueue(file_operation("late")).unwrap();
            queue.claim_next().unwrap().unwrap();
            if matches!(
                mutation,
                Mutation::Progress | Mutation::Complete | Mutation::Cancel | Mutation::Fail
            ) {
                queue.mark_running(&queued.operation_id).unwrap();
            }

            queue
                .transact_using(
                    |journal| {
                        if matches!(mutation, Mutation::Pause) {
                            journal.paused = true;
                            return Ok(());
                        }
                        let entry = find_mut(journal, &queued.operation_id)?;
                        match mutation {
                            Mutation::MarkRunning => entry.state = QueueState::Running,
                            Mutation::Progress => entry.progress.files_done = 7,
                            Mutation::Complete => entry.state = QueueState::Completed,
                            Mutation::Cancel => entry.state = QueueState::Cancelled,
                            Mutation::Fail => entry.state = QueueState::Failed,
                            Mutation::Pause => unreachable!(),
                        }
                        Ok(())
                    },
                    |path, snapshot| {
                        write_snapshot(path, snapshot)?;
                        Err(io::Error::other(
                            "Rename wirkte, Eltern-fsync meldet einen Fehler",
                        ))
                    },
                )
                .unwrap_or_else(|error| {
                    panic!("{mutation:?}: sichtbarer neuer Stand blieb Fehler: {error}")
                });

            assert_eq!(
                read_snapshot(&path).unwrap(),
                queue.snapshot(),
                "{mutation:?}"
            );
            if matches!(mutation, Mutation::Complete) {
                let error = queue
                    .fail(
                        &queued.operation_id,
                        Progress::default(),
                        "später Finalizer",
                    )
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                assert_eq!(
                    read_snapshot(&path).unwrap().entries[0].state,
                    QueueState::Completed
                );
            }
        }

        let retry_path = temp.path().join("retry/queue.json");
        let retry_queue = OperationQueue::open(&retry_path).unwrap();
        let original = retry_queue
            .transact_using(
                |journal| {
                    journal.paused = true;
                    Ok(())
                },
                |_, _| Err(io::Error::new(io::ErrorKind::WouldBlock, "vor Rename")),
            )
            .unwrap_err();
        assert_eq!(original.kind(), io::ErrorKind::WouldBlock);
        retry_queue.set_paused(true).unwrap();

        let foreign_path = temp.path().join("foreign/queue.json");
        let foreign_queue = OperationQueue::open(&foreign_path).unwrap();
        let foreign = foreign_queue
            .transact_using(
                |journal| {
                    journal.paused = true;
                    Ok(())
                },
                |path, snapshot| {
                    let mut foreign = snapshot.clone();
                    foreign.stop_on_error = !foreign.stop_on_error;
                    write_snapshot(path, &foreign)?;
                    Err(io::Error::other("fremder Stand nach Schreibfehler"))
                },
            )
            .unwrap_err();
        assert_eq!(
            crate::error_code(&foreign),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            crate::error_code(&foreign_queue.set_paused(false).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );

        let unreadable_path = temp.path().join("unreadable/queue.json");
        let unreadable_queue = OperationQueue::open(&unreadable_path).unwrap();
        let unreadable = unreadable_queue
            .transact_using(
                |journal| {
                    journal.paused = true;
                    Ok(())
                },
                |path, _| {
                    std::fs::remove_file(path)?;
                    std::fs::create_dir(path)?;
                    Err(io::Error::other("Journalname wurde unlesbar"))
                },
            )
            .unwrap_err();
        assert_eq!(
            crate::error_code(&unreadable),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            crate::error_code(&unreadable_queue.set_paused(false).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
    }

    fn local_execution() -> LocalQueueExecution {
        LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 100,
            max_delete_fraction: 0.25,
        }
    }

    #[test]
    fn move_rename_wird_nur_bei_freien_top_level_zielen_geprueft() {
        assert!(crate::remote_real_mode::file_move_rename_candidate(
            FileQueueKind::Move,
            true,
            true
        ));
        assert!(!crate::remote_real_mode::file_move_rename_candidate(
            FileQueueKind::Move,
            true,
            false
        ));
        assert!(!crate::remote_real_mode::file_move_rename_candidate(
            FileQueueKind::Copy,
            true,
            true
        ));
    }

    #[test]
    fn provider_audit_bindet_remote_arten_und_bestaetigte_klasse() {
        let replacement_capabilities = crate::ProviderFileCapabilities {
            provider: crate::ProviderKind::WebDav,
            copy_new: true,
            copy_new_best_effort: false,
            replace: false,
            replace_best_effort: true,
            remove_source: true,
            remove_source_best_effort: false,
            rename_noreplace: true,
            limitations: vec!["Altversion ist nicht wiederherstellbar".into()],
            blockers: Vec::new(),
        };
        // Nur eine tatsächlich erlaubte Ersetzung zieht die Gesamtwirkung auf Best
        // Effort. Dieselbe Zielverbindung bleibt für reine neue Namen abgesichert.
        assert_eq!(
            provider_file_audit_class(Some(&replacement_capabilities), false, false),
            ProviderAuditExecutionClass::Assured
        );
        assert_eq!(
            provider_file_audit_class(Some(&replacement_capabilities), true, false),
            ProviderAuditExecutionClass::BestEffort
        );
        assert_eq!(
            provider_delete_audit_class(RemoteDeleteMode::Quarantine),
            ProviderAuditExecutionClass::Assured
        );
        assert_eq!(
            provider_delete_audit_class(RemoteDeleteMode::Permanent),
            ProviderAuditExecutionClass::BestEffort
        );

        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        queue
            .append_file_audit(
                "audit-test",
                "provider_copy",
                "completed",
                &Progress::default(),
                &ProviderAuditContext {
                    source_provider: Some(crate::ProviderKind::Sftp),
                    target_provider: crate::ProviderKind::WebDav,
                    execution_class: ProviderAuditExecutionClass::BestEffort,
                    source_removal_best_effort_expected: true,
                    target_copy_new_best_effort_expected: false,
                    target_replace_best_effort_expected: true,
                    source_removal_best_effort_actual: Some(true),
                    target_copy_new_best_effort_actual: Some(false),
                    target_replace_best_effort_actual: Some(true),
                },
                None,
            )
            .unwrap();
        let record: serde_json::Value = serde_json::from_slice(
            std::fs::read(temp.path().join("queue-audit.jsonl"))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(record["version"], 4);
        assert_eq!(record["source_provider"], "sftp");
        assert_eq!(record["target_provider"], "webdav");
        assert_eq!(record["execution_class"], "best_effort");
        assert_eq!(record["source_removal_best_effort_expected"], true);
        assert_eq!(record["source_removal_best_effort_actual"], true);
        assert_eq!(record["target_replace_best_effort_actual"], true);
        assert_eq!(record["temp_recovery_paths"], serde_json::json!([]));
    }

    #[test]
    fn temp_recoverypfade_sind_terminal_in_queue_und_audit_gebunden() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        let queued = queue.enqueue(file_operation("temp-recovery")).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&queued.operation_id).unwrap();
        let paths = vec!["/sandbox/.doppeldecker-fileop-tmp-pruefen".to_owned()];
        let progress = Progress {
            phase: "cancelled".into(),
            ..Progress::default()
        };
        queue
            .finish_file(&queued.operation_id, true, progress.clone(), &paths, &[])
            .unwrap();
        queue
            .append_file_audit_with_temp_paths(
                &queued.operation_id,
                "local_copy",
                "cancelled",
                &progress,
                &ProviderAuditContext {
                    source_provider: Some(crate::ProviderKind::Local),
                    target_provider: crate::ProviderKind::Local,
                    execution_class: ProviderAuditExecutionClass::Assured,
                    source_removal_best_effort_expected: false,
                    target_copy_new_best_effort_expected: false,
                    target_replace_best_effort_expected: false,
                    source_removal_best_effort_actual: Some(false),
                    target_copy_new_best_effort_actual: Some(false),
                    target_replace_best_effort_actual: Some(false),
                },
                FileAuditRecovery {
                    provider: None,
                    temp_paths: &paths,
                    retained_backups: &[],
                },
            )
            .unwrap();

        assert_eq!(queue.snapshot().entries[0].temp_recovery_paths, paths);
        drop(queue);
        let reopened = OperationQueue::open(&journal).unwrap();
        assert_eq!(reopened.snapshot().entries[0].temp_recovery_paths, paths);
        let record: serde_json::Value = serde_json::from_slice(
            std::fs::read(temp.path().join("queue-audit.jsonl"))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(record["temp_recovery_paths"], serde_json::json!(paths));
    }

    #[test]
    fn fehlgeschlagene_dateioperation_behaelt_temp_recoverypfade_beim_retry() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        let queued = queue.enqueue(file_operation("temp-fehler")).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&queued.operation_id).unwrap();
        let paths = vec!["/sandbox/.doppeldecker-fileop-tmp-fehler".to_owned()];
        queue.fail_next_file_failure_commit_for_test();
        queue
            .fail_file(
                &queued.operation_id,
                Progress::default(),
                "erster Abschlussfehler",
                &paths,
            )
            .expect_err("der erste terminale Commit muss sicher retrybar scheitern");
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Running);
        queue
            .fail_file(
                &queued.operation_id,
                Progress {
                    phase: "copy".into(),
                    bytes_done: 17,
                    ..Progress::default()
                },
                "Kopierfehler",
                &paths,
            )
            .unwrap();

        let failed = queue.snapshot().entries[0].clone();
        assert_eq!(failed.state, QueueState::Failed);
        assert_eq!(failed.temp_recovery_paths, paths);
        drop(queue);
        let reopened = OperationQueue::open(&journal).unwrap();
        assert_eq!(reopened.snapshot().entries[0].temp_recovery_paths, paths);

        reopened.retry(&queued.operation_id).unwrap();
        let retried = reopened.snapshot().entries[0].clone();
        assert_eq!(retried.state, QueueState::Pending);
        assert!(retried.error.is_none());
        assert_eq!(retried.temp_recovery_paths, paths);
    }

    #[test]
    fn failed_audit_behaelt_fruehere_wirkung_bei_unklarer_folgephase() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let initial = ProviderAuditContext {
            source_provider: Some(crate::ProviderKind::Sftp),
            target_provider: crate::ProviderKind::Local,
            execution_class: ProviderAuditExecutionClass::BestEffort,
            source_removal_best_effort_expected: true,
            target_copy_new_best_effort_expected: true,
            target_replace_best_effort_expected: false,
            source_removal_best_effort_actual: None,
            target_copy_new_best_effort_actual: None,
            target_replace_best_effort_actual: None,
        };
        let actuals = FileOperationActuals {
            target_write_limitations: Vec::new(),
            source_removal_best_effort: None,
            target_copy_new_best_effort: Some(true),
            target_replace_best_effort: Some(false),
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Box::default(),
        };
        let failed = audit_context_with_actuals(initial, &actuals);
        let recovery = ProviderFileRecovery {
            quarantine_run: "/ziel/.doppeldecker-quarantine/lauf".into(),
            parked_entries: 2,
        };
        queue
            .append_file_audit(
                "partial-test",
                "provider_move",
                "failed",
                &Progress::default(),
                &failed,
                Some(&recovery),
            )
            .unwrap();

        let record: serde_json::Value = serde_json::from_slice(
            std::fs::read(temp.path().join("queue-audit.jsonl"))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(record["target_copy_new_best_effort_actual"], true);
        assert!(record["source_removal_best_effort_actual"].is_null());
        assert_eq!(record["status"], "failed");
        assert_eq!(
            record["recovery"]["quarantine_run"],
            recovery.quarantine_run
        );
        assert_eq!(record["recovery"]["parked_entries"], 2);
    }

    #[test]
    fn oeffentlicher_dateiexecutor_behaelt_errorcode_und_actuals() {
        let error = public_file_execution_error(ProviderFileExecutionFailure {
            error: crate::error::with_recovery_paths(
                crate::error::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    "Antwort nach der Zielwirkung ging verloren",
                ),
                &["/ziel/fehlerrest".into()],
            ),
            journal_errors: vec![crate::error::with_recovery_paths(
                io::Error::other("Journalfehler"),
                &["/queue/journalrest".into()],
            )],
            actuals: Box::new(FileOperationActuals {
                target_write_limitations: Vec::new(),
                source_removal_best_effort: Some(false),
                target_copy_new_best_effort: Some(true),
                target_replace_best_effort: None,
                temp_recovery_paths: vec!["/ziel/staging".into()],
                retained_backup_paths: Box::new(vec!["/ziel/sicherung".into()]),
            }),
            progress: Box::new(Progress {
                phase: "copy".into(),
                files_done: 2,
                bytes_done: 8,
                ..Progress::default()
            }),
            pending_completion: None,
            recovery: Some(ProviderFileRecovery {
                quarantine_run: "/ziel/.doppeldecker-quarantine/lauf".into(),
                parked_entries: 2,
            }),
        });

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let message = error.to_string();
        assert!(message.contains("\"files_done\":2"));
        assert!(message.contains("bekannte Dateioperationswirkungen"));
        assert!(message.contains("\"target_copy_new_best_effort\":true"));
        assert!(message.contains("\"target_replace_best_effort\":null"));
        assert!(message.contains("/ziel/.doppeldecker-quarantine/lauf"));
        assert_eq!(
            crate::error::recovery_paths(&error),
            vec![
                "/ziel/fehlerrest",
                "/queue/journalrest",
                "/ziel/staging",
                "/ziel/sicherung",
                "/ziel/.doppeldecker-quarantine/lauf",
            ]
        );
    }

    #[test]
    fn unklarer_journalfehler_bleibt_an_der_oeffentlichen_dateigrenze_fuehrend() {
        let error = public_file_execution_error(ProviderFileExecutionFailure {
            error: crate::error::coded_error(
                io::ErrorKind::Interrupted,
                crate::ErrorCode::Interrupted,
                "Ausführung wurde kontrolliert abgebrochen",
            ),
            journal_errors: vec![crate::error::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Fortschrittscommit hat möglicherweise gewirkt",
            )],
            actuals: Box::default(),
            progress: Box::new(Progress::default()),
            pending_completion: None,
            recovery: None,
        });

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(error
            .to_string()
            .contains("Fortschrittscommit hat möglicherweise gewirkt"));
    }

    #[test]
    fn gescheiterter_terminaler_wiederholungscommit_ist_outcome_unclear() {
        let audit_context = ProviderAuditContext {
            source_provider: None,
            target_provider: crate::ProviderKind::Local,
            execution_class: ProviderAuditExecutionClass::Assured,
            source_removal_best_effort_expected: false,
            target_copy_new_best_effort_expected: false,
            target_replace_best_effort_expected: false,
            source_removal_best_effort_actual: None,
            target_copy_new_best_effort_actual: Some(false),
            target_replace_best_effort_actual: None,
        };
        let error = public_file_execution_error(ProviderFileExecutionFailure {
            error: io::Error::other("Dateiwirkung war bereits vollständig"),
            journal_errors: vec![crate::error::coded_error(
                io::ErrorKind::StorageFull,
                crate::ErrorCode::StorageFull,
                "Queue-Journal ist voll",
            )],
            actuals: Box::default(),
            progress: Box::new(Progress::default()),
            pending_completion: Some(Box::new(PendingFileCompletion {
                cancelled: false,
                progress: Progress::default(),
                temp_recovery_paths: Vec::new(),
                audit_operation: "copy",
                audit_context,
                stats: OpStats::default(),
            })),
            recovery: None,
        });

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains("storage_full"));
        assert!(error
            .to_string()
            .contains("terminale Dateiwirkung ohne dauerhaften Queueabschluss: true"));
    }

    #[test]
    fn provider_fehler_behaelt_fortschritt_bei_gescheitertem_queue_commit() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let source_file = source.join("datei.bin");
        std::fs::write(&source_file, vec![0x5a; 128 * 1024]).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::ProviderFile {
                kind: FileQueueKind::Copy,
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                items: vec![source_file.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                source_tree_expected: None,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(false),
                replace_best_effort_expected: Some(false),
                remove_source_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let operation_id = claimed.operation_id.clone();
        // Der erste Callback wird persistiert. Der nächste enthält bereits mehr
        // kopierte Bytes, scheitert aber genau an seinem nichtterminalen Queue-
        // Commit. Die anschließende atomare Fehlerfinalisierung muss dennoch den
        // aktuellen Stand zusammen mit dem Fachfehler festhalten.
        queue.fail_nonterminal_progress_update_after_for_test(1);

        let mut callbacks = 0;
        let failure = execute_provider_file_queue_entry_detailed(
            &queue,
            claimed,
            &local_execution(),
            no_secrets(),
            &Cancel::new(),
            &mut |_| {
                callbacks += 1;
                if callbacks == 1 {
                    std::thread::sleep(Duration::from_millis(300));
                }
            },
        )
        .expect_err("der injizierte Fortschrittsfehler muss den Lauf stoppen");
        let persisted = queue
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == operation_id)
            .unwrap()
            .progress;

        assert_eq!(failure.progress.bytes_done, persisted.bytes_done);
        assert!(failure.progress.bytes_done > 0);
        assert!(failure.error.to_string().contains("Queue-Fortschritt"));
    }

    #[test]
    fn move_quellklasse_drift_stoppt_vor_mutation_und_wird_gebunden() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let source_file = source.join("datei.txt");
        let target_file = target.join("datei.txt");
        std::fs::write(&source_file, b"inhalt").unwrap();
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        let queued = queue
            .enqueue(QueueOperation::ProviderFile {
                kind: FileQueueKind::Move,
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                items: vec![source_file.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                source_tree_expected: None,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(false),
                replace_best_effort_expected: Some(false),
                // Die Vorschau behauptet absichtlich noch die starke Klasse. Der
                // frische Executor muss die schwächere Wirkung vor jedem Schreiben
                // sichtbar neu bestätigen lassen.
                remove_source_best_effort_expected: Some(false),
            })
            .unwrap();

        let result = execute_provider_file_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &local_execution(),
            no_secrets(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(
            result,
            QueueExecutionResult::AwaitingConfirmation { .. }
        ));
        assert_eq!(std::fs::read(&source_file).unwrap(), b"inhalt");
        assert!(!target_file.exists());

        let attention = queue
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == queued.operation_id)
            .and_then(|entry| entry.attention)
            .unwrap();
        let phrase = attention_confirmation(&attention).to_owned();
        queue
            .approve_attention(&queued.operation_id, &phrase)
            .unwrap();
        let rebound = queue.claim_next().unwrap().unwrap();
        assert!(matches!(
            &rebound.operation,
            QueueOperation::ProviderFile {
                remove_source_best_effort_expected: Some(true),
                ..
            }
        ));
        let result = execute_provider_file_queue_entry(
            &queue,
            rebound,
            &local_execution(),
            no_secrets(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(
            result,
            QueueExecutionResult::File(OpStats {
                source_removal_best_effort: true,
                ..
            })
        ));
        assert!(!source_file.exists());
        assert_eq!(std::fs::read(target_file).unwrap(), b"inhalt");
        let records = std::fs::read_to_string(root.join("queue-audit.jsonl")).unwrap();
        let records = records
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let started = records
            .iter()
            .find(|record| record["status"] == "started")
            .unwrap();
        let completed = records
            .iter()
            .find(|record| record["status"] == "completed")
            .unwrap();
        assert_eq!(started["execution_class"], "best_effort");
        assert_eq!(started["source_removal_best_effort_expected"], true);
        assert!(started["source_removal_best_effort_actual"].is_null());
        assert_eq!(completed["source_removal_best_effort_actual"], true);
        assert_eq!(completed["target_copy_new_best_effort_actual"], false);
        assert_eq!(completed["target_replace_best_effort_actual"], false);
    }

    #[test]
    fn provider_move_folgt_dem_finalen_lokalen_symlink_nicht() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let real_source = source.join("bestand.txt");
        let selected_link = source.join("auswahl.txt");
        std::fs::write(&real_source, b"darf-nicht-verschoben-werden").unwrap();
        std::os::unix::fs::symlink("bestand.txt", &selected_link).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::ProviderFile {
                kind: FileQueueKind::Move,
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                items: vec![selected_link.to_str().unwrap().to_owned()],
                destination: target.to_str().unwrap().to_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                source_tree_expected: None,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                best_effort_commit_expected: None,
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(false),
                replace_best_effort_expected: Some(false),
                remove_source_best_effort_expected: Some(true),
            })
            .unwrap();

        let result = execute_provider_file_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &local_execution(),
            no_secrets(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();

        let QueueExecutionResult::File(stats) = result else {
            panic!("Provider-Move lieferte kein Dateiergebnis");
        };
        assert_eq!(stats.symlinks_skipped, 1);
        assert_eq!(
            std::fs::read(&real_source).unwrap(),
            b"darf-nicht-verschoben-werden"
        );
        assert!(std::fs::symlink_metadata(&selected_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!target.join("bestand.txt").exists());
        assert!(!target.join("auswahl.txt").exists());

        let real_parent = source.join("echter-ordner");
        let linked_parent = source.join("verknuepfter-ordner");
        std::fs::create_dir(&real_parent).unwrap();
        std::fs::write(real_parent.join("kind.txt"), b"bleibt").unwrap();
        std::os::unix::fs::symlink("echter-ordner", &linked_parent).unwrap();
        crate::fileops::bind_local_file_operation_scope(&source, &[linked_parent.join("kind.txt")])
            .expect_err("auch eine Elternkomponente darf kein Symlink sein");
        assert_eq!(
            std::fs::read(real_parent.join("kind.txt")).unwrap(),
            b"bleibt"
        );
    }

    fn sftp_operation() -> QueueOperation {
        QueueOperation::SftpSync {
            direction: SftpQueueDirection::Upload,
            connection: Box::new(SftpConnectionConfig {
                host: "sftp.example.test".into(),
                alternate_hosts: Vec::new(),
                port: 22,
                username: "test".into(),
                requested_root: "/daten".into(),
                host_key: SftpHostKeyPin {
                    algorithm: "ssh-ed25519".into(),
                    sha256: [1; 32],
                },
                authentication: SftpAuthenticationReference::Password {
                    secret: KeychainSecretReference::new("Doppeldecker", "test").unwrap(),
                },
            }),
            local_root: "/tmp/quelle".into(),
            remote_root: "/ziel".into(),
            options: SyncOptions::default(),
            expected_plan: Box::new(SyncPlan::default()),
        }
    }

    fn sftp_endpoint(account: &str, root: &str) -> Box<BidirectionalQueueEndpoint> {
        Box::new(BidirectionalQueueEndpoint::Sftp {
            connection: Box::new(SftpConnectionConfig {
                host: "sftp.example.test".into(),
                alternate_hosts: Vec::new(),
                port: 22,
                username: "test".into(),
                requested_root: "/daten".into(),
                host_key: SftpHostKeyPin {
                    algorithm: "ssh-ed25519".into(),
                    sha256: [1; 32],
                },
                authentication: SftpAuthenticationReference::Password {
                    secret: KeychainSecretReference::new("Doppeldecker", account).unwrap(),
                },
            }),
            root: root.into(),
        })
    }

    fn smb_operation() -> QueueOperation {
        QueueOperation::SmbSync {
            direction: SftpQueueDirection::Upload,
            connection: Box::new(SmbConnectionConfig {
                host: "smb.example.test".into(),
                port: 445,
                server_guid: [7; 16],
                share: "Daten".into(),
                username: "test".into(),
                domain: "WORKGROUP".into(),
                requested_root: "/daten".into(),
                password_reference: KeychainSecretReference::new("Doppeldecker", "smb-test")
                    .unwrap(),
            }),
            local_root: "/tmp/quelle".into(),
            remote_root: "/ziel".into(),
            options: SyncOptions::default(),
            expected_plan: Box::new(SyncPlan::default()),
        }
    }

    #[test]
    fn reihenfolge_und_pending_umordnung_bleiben_persistent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let first = queue.enqueue(file_operation("eins")).unwrap();
        let second = queue.enqueue(file_operation("zwei")).unwrap();
        queue.move_pending(&second.operation_id, -1).unwrap();
        drop(queue);

        let queue = OperationQueue::open(&path).unwrap();
        assert_eq!(
            queue.claim_next().unwrap().unwrap().operation_id,
            second.operation_id
        );
        assert_eq!(queue.snapshot().entries[1].operation_id, first.operation_id);
    }

    #[test]
    fn review_umordnung_ueberholt_keinen_offenen_recoveryeintrag() {
        for barrier in [QueueState::Interrupted, QueueState::AwaitingConfirmation] {
            for delta in [-1, 1] {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("queue.json");
                let queue = OperationQueue::open(&path).unwrap();
                let first = queue.enqueue(file_operation("eins")).unwrap();
                let middle = queue.enqueue(file_operation("recovery")).unwrap();
                let last = queue.enqueue(file_operation("drei")).unwrap();
                queue
                    .transact(|journal| {
                        let entry = find_mut(journal, &middle.operation_id)?;
                        entry.state = barrier;
                        entry.attention =
                            (barrier == QueueState::AwaitingConfirmation).then(|| {
                                QueueAttention::Manual {
                                    reason: "Prüfung erforderlich".into(),
                                    confirmation: "PRUEFEN".into(),
                                }
                            });
                        Ok(())
                    })
                    .unwrap();
                let expected = queue.snapshot().entries;
                let selected = if delta < 0 { &last } else { &first };
                queue.move_pending(&selected.operation_id, delta).unwrap();
                assert_eq!(queue.snapshot().entries, expected, "{barrier:?}, {delta}");
                drop(queue);
                let reopened = OperationQueue::open(&path).unwrap();
                assert_eq!(reopened.snapshot().entries, expected);
                assert_eq!(
                    reopened.claim_next().unwrap().unwrap().operation_id,
                    first.operation_id
                );
            }
        }
    }

    #[test]
    fn review_sichtbarer_journalcommit_braucht_erfolgreichen_durability_retry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let queued = queue.enqueue_claimed(file_operation("durability")).unwrap();
        let before = queue.snapshot();
        queue.fail_reconcile_sync.store(true, Ordering::SeqCst);
        let error = queue
            .transact_using(
                |journal| {
                    find_mut(journal, &queued.operation_id)?.state = QueueState::Running;
                    Ok(())
                },
                |path, snapshot| {
                    write_snapshot(path, snapshot)?;
                    Err(io::Error::other("erster Parent-fsync meldet Fehler"))
                },
            )
            .expect_err("Readback allein darf Running nicht freigeben");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(queue.snapshot(), before);
        assert_eq!(
            crate::error_code(&queue.set_paused(true).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        drop(queue);
        let reopened = OperationQueue::open(path).unwrap();
        assert_eq!(
            reopened.snapshot().entries[0].state,
            QueueState::Interrupted
        );
        assert!(reopened.claim_next().unwrap().is_none());
    }

    #[test]
    fn review_webdav_altplan_mit_frueherem_versuch_verlangt_manuelle_pruefung() {
        for (version, state) in [
            (15, QueueState::Pending),
            (15, QueueState::AwaitingConfirmation),
            (17, QueueState::Pending),
            (17, QueueState::AwaitingConfirmation),
            (QUEUE_JOURNAL_VERSION, QueueState::Failed),
        ] {
            for reported_progress in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("queue.json");
                let queue = OperationQueue::open(&path).unwrap();
                let queued = queue.enqueue(webdav_delete_operation()).unwrap();
                let mut snapshot = queue.snapshot();
                drop(queue);
                snapshot.version = version;
                let entry = &mut snapshot.entries[0];
                entry.state = state;
                entry.attempts = 1;
                entry.progress.files_done = u64::from(reported_progress);
                entry.progress.bytes_done = if reported_progress { 7 } else { 0 };
                entry.attention =
                    (state == QueueState::AwaitingConfirmation).then(|| QueueAttention::Manual {
                        reason: "Neuer Versuch".into(),
                        confirmation: "PRUEFEN".into(),
                    });
                let QueueOperation::ProviderDelete { expected_plan, .. } = &mut entry.operation
                else {
                    unreachable!()
                };
                let crate::RootIdentity::WebDav(identity) = &mut expected_plan.root_identity else {
                    unreachable!()
                };
                if version == QUEUE_JOURNAL_VERSION {
                    identity.root_etag = LEGACY_UNBOUND_WEBDAV_ETAG.into();
                    entry.error = Some(LEGACY_WEBDAV_REPLAN_ERROR.into());
                    entry.error_code = Some(crate::ErrorCode::PlanDrift);
                } else if version == 15 {
                    identity.root_etag.clear();
                } else {
                    identity.server.server_header_digest = *blake3::hash(&[]).as_bytes();
                }
                std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
                let migrated = OperationQueue::open(&path).unwrap();
                let entry = migrated.snapshot().entries.remove(0);
                assert_eq!(
                    entry.state,
                    QueueState::Interrupted,
                    "v{version}, {state:?}"
                );
                assert_eq!(entry.error_code, Some(crate::ErrorCode::OutcomeUnclear));
                assert_eq!(entry.progress.files_done, u64::from(reported_progress));
                assert_eq!(
                    entry.progress.bytes_done,
                    if reported_progress { 7 } else { 0 }
                );
                assert_eq!(
                    crate::error_code(
                        &migrated
                            .acknowledge_legacy_webdav_delete(&queued.operation_id, false)
                            .unwrap_err()
                    ),
                    crate::ErrorCode::ConfirmationMismatch
                );
                migrated
                    .acknowledge_legacy_webdav_delete(&queued.operation_id, true)
                    .unwrap();
                assert_eq!(migrated.snapshot().entries[0].state, QueueState::Cancelled);
            }
        }
    }

    #[test]
    fn unmittelbares_cli_enqueue_persistiert_nur_geclaimt_oder_gar_nicht() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();

        queue.set_paused(true).unwrap();
        let paused = queue
            .enqueue_claimed(file_operation("pausiert"))
            .unwrap_err();
        assert_eq!(paused.kind(), io::ErrorKind::WouldBlock);
        assert!(queue.snapshot().entries.is_empty());

        queue.set_paused(false).unwrap();
        let claimed = queue.enqueue_claimed(file_operation("direkt")).unwrap();
        assert_eq!(claimed.state, QueueState::Validating);
        assert_eq!(claimed.attempts, 1);
        assert_eq!(queue.snapshot().entries, vec![claimed.clone()]);
        drop(queue);

        // `Validating` ist die absichtliche Crash-Grenze: Nach einem Prozessende wird
        // der Eintrag sichtbar unterbrochen und kann nie still als Pending anlaufen.
        let recovered = OperationQueue::open(path).unwrap().snapshot();
        assert_eq!(recovered.entries[0].state, QueueState::Interrupted);
    }

    #[test]
    fn unmittelbares_cli_enqueue_ueberholt_keinen_offenen_eintrag() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let first = queue.enqueue(file_operation("zuerst")).unwrap();

        let blocked = queue
            .enqueue_claimed(file_operation("spaeter"))
            .unwrap_err();
        assert_eq!(blocked.kind(), io::ErrorKind::WouldBlock);
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].operation_id, first.operation_id);
        assert_eq!(snapshot.entries[0].state, QueueState::Pending);
    }

    #[test]
    fn remote_queueeintrag_persistiert_exakte_profilrevision() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let binding =
            crate::RemoteProfileBinding::new(crate::RemoteProfileProtocol::Sftp, "profil-eins", 17)
                .unwrap();

        let entry = queue
            .enqueue_reconciled_with_profile_bindings(
                file_operation("gebunden"),
                vec![binding.clone()],
            )
            .unwrap();
        assert_eq!(entry.profile_bindings, vec![binding.clone()]);
        drop(queue);

        let reopened = OperationQueue::open(path).unwrap();
        assert_eq!(
            reopened.snapshot().entries[0].profile_bindings,
            vec![binding]
        );
    }

    #[test]
    fn profilinvalidierung_setzt_remote_bestaetigung_auf_pending_zurueck() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let binding =
            crate::RemoteProfileBinding::new(crate::RemoteProfileProtocol::Sftp, "profil-eins", 17)
                .unwrap();
        let entry = queue
            .enqueue_claimed_with_profile_bindings(
                file_operation("gebunden"),
                vec![binding.clone()],
            )
            .unwrap();
        queue
            .require_confirmation(
                &entry.operation_id,
                QueueAttention::RemoteRun {
                    reason: "Vorbereitung wartet".into(),
                    confirmation: "WEITER".into(),
                },
            )
            .unwrap();

        queue
            .discard_remote_preparation(&entry.operation_id)
            .unwrap();
        let current = &queue.snapshot().entries[0];
        assert_eq!(current.state, QueueState::Pending);
        assert_eq!(current.profile_bindings, vec![binding]);
        assert!(current.attention.is_none());
    }

    #[test]
    fn unklarer_mutationsausgang_bleibt_nach_neustart_nicht_wiederholbar() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let entry = queue.enqueue_claimed(file_operation("unklar")).unwrap();
        let failure = crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            "Antwort ging nach der Mutation verloren",
        );
        queue
            .fail_coded(&entry.operation_id, Progress::default(), &failure)
            .unwrap();
        drop(queue);

        let queue = OperationQueue::open(&path).unwrap();
        let persisted = &queue.snapshot().entries[0];
        assert_eq!(persisted.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        let retry = queue.retry(&entry.operation_id).unwrap_err();
        assert_eq!(crate::error_code(&retry), crate::ErrorCode::OutcomeUnclear);
        let missing_review = queue
            .acknowledge_unclear_outcome(&entry.operation_id, false)
            .unwrap_err();
        assert_eq!(
            crate::error_code(&missing_review),
            crate::ErrorCode::ConfirmationMismatch
        );
        queue
            .acknowledge_unclear_outcome(&entry.operation_id, true)
            .unwrap();
        let closed = &queue.snapshot().entries[0];
        assert_eq!(closed.state, QueueState::Cancelled);
        assert_eq!(closed.progress.phase, "recovery_acknowledged");
        assert_eq!(closed.error_code, Some(crate::ErrorCode::OutcomeUnclear));
    }

    #[test]
    fn v17_fehler_ohne_code_bleiben_nach_migration_nicht_wiederholbar() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let entries = [QueueState::Failed, QueueState::Interrupted]
            .into_iter()
            .enumerate()
            .map(|(index, state)| QueueEntry {
                operation_id: format!("alt-fehler-{index}"),
                operation: file_operation(&format!("alt-{index}")),
                profile_bindings: Vec::new(),
                created_unix_ms: index as u128,
                state,
                attempts: 1,
                progress: Progress::default(),
                attention: None,
                error: Some("Alter Executorfehler ohne stabilen Code".into()),
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            })
            .collect();
        let snapshot = QueueSnapshot {
            version: 17,
            entries,
            ..QueueSnapshot::default()
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let queue = OperationQueue::open(&path).unwrap();
        for entry in &queue.snapshot().entries {
            assert_eq!(entry.error_code, Some(crate::ErrorCode::OutcomeUnclear));
            let retry = queue.retry(&entry.operation_id).unwrap_err();
            assert_eq!(crate::error_code(&retry), crate::ErrorCode::OutcomeUnclear);
        }
    }

    #[test]
    fn journalwachstum_nach_metadatenpruefung_ueberschreitet_die_grenze_nicht_still() {
        let mut payload = serde_json::to_vec(&QueueSnapshot::default()).unwrap();
        let advertised_len = payload.len() as u64;
        let max_bytes = advertised_len + 8;
        // Whitespace nach einem gültigen JSON-Dokument würde serde akzeptieren.
        // Der Leser muss trotzdem die tatsächliche Dateigröße nach dem begrenzten
        // Read prüfen, nicht nur die zuvor gemeldete Metadatenlänge.
        payload.resize((max_bytes + 1) as usize, b' ');

        let error =
            read_snapshot_from_reader(std::io::Cursor::new(payload), advertised_len, max_bytes)
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("Größenbegrenzung"));
    }

    #[test]
    fn lokales_v1_journal_wird_atomar_auf_aktuelle_version_migriert() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let mut old = QueueSnapshot {
            version: 1,
            ..QueueSnapshot::default()
        };
        old.entries.push(QueueEntry {
            operation_id: "alt-lokal".into(),
            operation: file_operation("alt"),
            profile_bindings: Vec::new(),
            created_unix_ms: 1,
            state: QueueState::Pending,
            attempts: 0,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        });
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let queue = OperationQueue::open(&path).unwrap();
        assert_eq!(queue.snapshot().version, QUEUE_JOURNAL_VERSION);
        drop(queue);
        let persisted: QueueSnapshot =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted.version, QUEUE_JOURNAL_VERSION);
        assert_eq!(persisted.entries[0].operation_id, "alt-lokal");
    }

    #[test]
    fn altes_webdav_journal_ohne_root_etag_wird_sichtbar_stillgelegt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let snapshot = QueueSnapshot {
            version: 15,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "alt-webdav".into(),
                operation: webdav_delete_operation(),
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Pending,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["entries"][0]["operation"]["expected_plan"]["root_identity"]["identity"]
            .as_object_mut()
            .unwrap()
            .remove("root_etag");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let queue = OperationQueue::open(&path).unwrap();
        let entry = &queue.snapshot().entries[0];
        assert_eq!(entry.state, QueueState::Failed);
        assert_eq!(entry.error.as_deref(), Some(LEGACY_WEBDAV_REPLAN_ERROR));
        assert!(is_valid_legacy_unbound_webdav_delete(entry));
        let retry_error = queue.retry("alt-webdav").unwrap_err();
        assert_eq!(crate::error_code(&retry_error), crate::ErrorCode::PlanDrift);
        queue
            .acknowledge_legacy_webdav_delete("alt-webdav", false)
            .unwrap();
        assert!(queue.snapshot().entries.is_empty());
        drop(queue);

        let persisted: QueueSnapshot =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted.version, QUEUE_JOURNAL_VERSION);
        assert!(persisted.entries.is_empty());
    }

    #[test]
    fn webdav_v17_mit_leerem_server_digest_blockiert_nur_den_alteintrag() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let mut operation = webdav_delete_operation();
        let QueueOperation::ProviderDelete { expected_plan, .. } = &mut operation else {
            unreachable!("Testhelfer liefert Remote-Löschen")
        };
        let crate::RootIdentity::WebDav(identity) = &mut expected_plan.root_identity else {
            unreachable!("Testhelfer liefert WebDAV")
        };
        identity.server.server_header_digest = *blake3::hash(&[]).as_bytes();
        let snapshot = QueueSnapshot {
            version: 17,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "v17-headerlos".into(),
                operation,
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Pending,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let queue = OperationQueue::open(&path).unwrap();
        let migrated = &queue.snapshot().entries[0];
        assert_eq!(migrated.state, QueueState::Failed);
        assert_eq!(migrated.error_code, Some(crate::ErrorCode::PlanDrift));
        assert!(has_legacy_unbound_webdav_delete(migrated));
        assert_eq!(queue.snapshot().version, QUEUE_JOURNAL_VERSION);
    }

    #[test]
    fn v18_bewahrt_bereits_stillgelegten_webdav_altplan_als_sicher_ungestartet() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let mut operation = webdav_delete_operation();
        let QueueOperation::ProviderDelete { expected_plan, .. } = &mut operation else {
            unreachable!("Testhelfer liefert Remote-Löschen")
        };
        let crate::RootIdentity::WebDav(identity) = &mut expected_plan.root_identity else {
            unreachable!("Testhelfer liefert WebDAV")
        };
        identity.root_etag = LEGACY_UNBOUND_WEBDAV_ETAG.into();
        let snapshot = QueueSnapshot {
            version: 18,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "v18-bereits-stillgelegt".into(),
                operation,
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Failed,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: Some(LEGACY_WEBDAV_REPLAN_ERROR.into()),
                error_code: Some(crate::ErrorCode::PlanDrift),
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let queue = OperationQueue::open(&path).unwrap();
        let migrated = &queue.snapshot().entries[0];
        assert_eq!(migrated.state, QueueState::Failed);
        assert_eq!(migrated.error_code, Some(crate::ErrorCode::PlanDrift));
        assert_eq!(migrated.error.as_deref(), Some(LEGACY_WEBDAV_REPLAN_ERROR));
        assert!(is_valid_legacy_unbound_webdav_delete(migrated));
        queue
            .acknowledge_legacy_webdav_delete("v18-bereits-stillgelegt", false)
            .unwrap();
        assert!(queue.snapshot().entries.is_empty());
    }

    /// Die Gültigkeit eines migrierten WebDAV-Altstands darf nicht am deutschen
    /// Fehlertext hängen. Sonst hätte eine bloße Umformulierung der Konstante das
    /// Öffnen des **gesamten** Journals scheitern lassen — für jedes Fenster und
    /// die CLI, samt aller übrigen Einträge.
    #[test]
    fn webdav_altstand_bleibt_ohne_seinen_deutschen_fehlertext_gueltig() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let legacy_entry = |operation_id: &str, state: QueueState, phase: &str, code| {
            let mut operation = webdav_delete_operation();
            let QueueOperation::ProviderDelete { expected_plan, .. } = &mut operation else {
                unreachable!("Testhelfer liefert Remote-Löschen")
            };
            let crate::RootIdentity::WebDav(identity) = &mut expected_plan.root_identity else {
                unreachable!("Testhelfer liefert WebDAV")
            };
            identity.root_etag = LEGACY_UNBOUND_WEBDAV_ETAG.into();
            QueueEntry {
                operation_id: operation_id.into(),
                operation,
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state,
                attempts: 0,
                progress: Progress {
                    phase: phase.into(),
                    ..Progress::default()
                },
                attention: None,
                // Bewusst ein anderer Text als die Prosakonstante: Genau das simuliert
                // eine spätere Umformulierung.
                error: Some("umformulierter Hinweis ohne die alte Prosa".into()),
                error_code: Some(code),
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }
        };
        let snapshot = QueueSnapshot {
            version: QUEUE_JOURNAL_VERSION,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![
                legacy_entry(
                    "altstand-sicher",
                    QueueState::Failed,
                    "",
                    crate::ErrorCode::PlanDrift,
                ),
                legacy_entry(
                    "altstand-unklar",
                    QueueState::Interrupted,
                    "recovery",
                    crate::ErrorCode::OutcomeUnclear,
                ),
            ],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        // Ohne die typisierte Einordnung scheitert schon dieses Öffnen.
        let queue = OperationQueue::open(&path).unwrap();
        let entries = queue.snapshot().entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(
            classify_legacy_unbound_webdav_delete(&entries[0]),
            Some(LegacyUnboundWebDavDelete::SafeReplan)
        );
        assert_eq!(
            classify_legacy_unbound_webdav_delete(&entries[1]),
            Some(LegacyUnboundWebDavDelete::PossiblePartialEffect)
        );

        // Der sichere Altstand bleibt verwerfbar, der unklare verlangt weiterhin die
        // ausdrückliche Bestätigung der manuellen Prüfung.
        let unbestaetigt = queue
            .acknowledge_legacy_webdav_delete("altstand-unklar", false)
            .unwrap_err();
        assert_eq!(
            crate::error_code(&unbestaetigt),
            crate::ErrorCode::ConfirmationMismatch
        );
        queue
            .acknowledge_legacy_webdav_delete("altstand-sicher", false)
            .unwrap();
        queue
            .acknowledge_legacy_webdav_delete("altstand-unklar", true)
            .unwrap();
        let rest = queue.snapshot().entries;
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].state, QueueState::Cancelled);
    }

    #[test]
    fn v18_remote_loeschplaene_werden_nach_identitaetswechsel_nie_wiederholt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let entry = |operation_id: &str, state: QueueState, attempts| QueueEntry {
            operation_id: operation_id.into(),
            operation: webdav_delete_operation(),
            profile_bindings: Vec::new(),
            created_unix_ms: 0,
            state,
            attempts,
            progress: Progress::default(),
            attention: None,
            error: None,
            error_code: None,
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Vec::new(),
            recovery_available: false,
            bidirectional_completion_recovery: None,
        };
        let snapshot = QueueSnapshot {
            version: 18,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![
                entry("v18-pending", QueueState::Pending, 0),
                entry("v18-running", QueueState::Running, 1),
            ],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let queue = OperationQueue::open(path).unwrap();
        let migrated = queue.snapshot();

        assert_eq!(migrated.version, QUEUE_JOURNAL_VERSION);
        assert_eq!(migrated.entries[0].state, QueueState::Failed);
        assert_eq!(
            migrated.entries[0].error_code,
            Some(crate::ErrorCode::ReplanRequired)
        );
        assert_eq!(
            migrated.entries[0].error.as_deref(),
            Some(LEGACY_REMOTE_IDENTITY_REPLAN_ERROR)
        );
        assert_eq!(migrated.entries[1].state, QueueState::Interrupted);
        assert_eq!(
            migrated.entries[1].error_code,
            Some(crate::ErrorCode::OutcomeUnclear)
        );
        assert_eq!(
            migrated.entries[1].error.as_deref(),
            Some(LEGACY_REMOTE_IDENTITY_UNCLEAR_ERROR)
        );
        assert_eq!(
            crate::error_code(&queue.retry("v18-pending").unwrap_err()),
            crate::ErrorCode::ReplanRequired
        );
        queue.discard_replan_required("v18-pending").unwrap();
        assert_eq!(queue.snapshot().entries.len(), 1);
    }

    /// Regression: Ein von außen verändertes Journal kann jede nicht leere
    /// Zeichenkette als Operations-ID mitbringen. Ein Schnitt nach acht *Bytes*
    /// fiele bei Mehrbyte-Zeichen mitten hinein und beendete den Queue-Worker mit
    /// einer Panik, statt den Eintrag fail-closed zu melden.
    #[test]
    fn kurzform_einer_operations_id_schneidet_nach_zeichen() {
        assert_eq!(short_id("0123456789abcdef0123456789abcdef"), "01234567");
        assert_eq!(short_id("äöüäöüäöü"), "äöüäöüäö");
        assert_eq!(short_id("kurz"), "kurz");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn fortschrittsfehler_erhaelt_recovery_bis_ins_dauerhafte_queuejournal() {
        let path = "/isoliert/stage/rest".to_owned();
        let execution = Err::<(), _>(crate::error::with_recovery_paths(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Commit unklar",
            ),
            std::slice::from_ref(&path),
        ));
        let error = possible_mutation_progress_error(io::Error::other("Journal voll"), &execution);
        assert_eq!(crate::error::recovery_paths(&error), vec![path.clone()]);
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        let queued = queue.enqueue(file_operation("recovery-wrapper")).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&queued.operation_id).unwrap();
        queue
            .fail_coded(&queued.operation_id, Progress::default(), &error)
            .unwrap();
        drop(queue);
        let queue = OperationQueue::open(&journal).unwrap();
        let entry = &queue.snapshot().entries[0];
        assert_eq!(entry.temp_recovery_paths, vec![path]);
        assert_eq!(entry.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        assert_eq!(
            crate::error_code(&queue.retry(&queued.operation_id).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
    }

    #[test]
    fn weitere_queue_fehlerhuellen_erhalten_vorhandene_recoverypayloads() {
        let make_error = || {
            crate::error::with_recovery_paths(
                crate::coded_error(
                    io::ErrorKind::StorageFull,
                    crate::ErrorCode::OutcomeUnclear,
                    "Fehler",
                ),
                &["/isoliert/stage/rest".into()],
            )
        };
        let persistence = crate::error::with_recovery_paths(
            io::Error::other("Journalfehler"),
            &["/isoliert/index/rest".into()],
        );
        let execution = Err::<(), _>(make_error());
        let combined = possible_mutation_progress_error(persistence, &execution);
        assert_eq!(
            crate::error_code(&combined),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            crate::error::recovery_paths(&combined),
            vec!["/isoliert/index/rest", "/isoliert/stage/rest"]
        );
        let terminal = failed_queue_error_commit(
            &make_error(),
            &crate::error::with_recovery_paths(
                io::Error::new(io::ErrorKind::StorageFull, "Journal voll"),
                &["/isoliert/index/rest".into()],
            ),
            "terminale Queue-Fehlercommit",
        );
        assert_eq!(
            crate::error_code(&terminal),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            crate::error::recovery_paths(&terminal),
            vec!["/isoliert/stage/rest", "/isoliert/index/rest"]
        );
        let unchanged = error_with_provider_file_recovery(make_error(), None);
        assert_eq!(
            crate::error::recovery_paths(&unchanged),
            vec!["/isoliert/stage/rest"]
        );
        let progress = queue_progress_persistence_error(make_error());
        assert_eq!(progress.kind(), io::ErrorKind::StorageFull);
        assert_eq!(
            crate::error_code(&progress),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            crate::error::recovery_paths(&progress),
            vec!["/isoliert/stage/rest"]
        );
        let recovery = ProviderFileRecovery {
            quarantine_run: "/isoliert/quarantaene/run".into(),
            parked_entries: 1,
        };
        let provider = error_with_provider_file_recovery(make_error(), Some(&recovery));
        assert_eq!(provider.kind(), io::ErrorKind::StorageFull);
        assert_eq!(
            crate::error_code(&provider),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            crate::error::recovery_paths(&provider),
            vec!["/isoliert/stage/rest", "/isoliert/quarantaene/run"]
        );
    }

    #[test]
    fn fortschritts_persistenzfehler_nach_moeglicher_wirkung_ist_outcome_unclear() {
        let execution = Err::<(), _>(crate::error::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Ausführung wurde abgebrochen",
        ));
        let error = possible_mutation_progress_error(
            crate::error::coded_error(
                io::ErrorKind::StorageFull,
                crate::ErrorCode::StorageFull,
                "Queue-Journal ist voll",
            ),
            &execution,
        );
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains("Queue-Journal ist voll"));
        assert!(error.to_string().contains("Ausführung wurde abgebrochen"));
    }

    /// Regression: Die v19-Nachmigration erkennt einen sicher ungestarteten
    /// Altstand ausschließlich daran, dass sein gespeicherter Fehlertext wörtlich
    /// `LEGACY_REMOTE_IDENTITY_REPLAN_ERROR` ist. Ein echtes Feldjournal trägt
    /// genau diesen Wortlaut aus einer früheren Programmversion; wird die Konstante
    /// umformuliert, bleibt so ein Eintrag stumm auf `PlanDrift` stehen, und die
    /// Oberfläche bietet `retry` statt `discard_replan_required` an. Der Test hält
    /// den Wortlaut als Literal fest — wer ihn ändert, muss die Migration mitziehen
    /// (etwa mit einer zusätzlich geprüften Altfassung).
    #[test]
    fn v19_erkennungstext_der_nachmigration_bleibt_wortgleich() {
        assert_eq!(
            LEGACY_REMOTE_IDENTITY_REPLAN_ERROR,
            "Dieser ältere FTPS-/WebDAV-Löschplan verwendet das frühere \
             Serveridentitäts-Schema. Er war sicher noch nicht gestartet und muss \
             mit einer neuen Vorschau ausdrücklich neu geplant werden."
        );
    }

    #[test]
    fn v18_retry_nach_abbruch_bleibt_wegen_frueherem_claim_outcome_unclear() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let queued = queue.enqueue(webdav_delete_operation()).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&queued.operation_id).unwrap();
        queue
            .cancel(
                &queued.operation_id,
                Progress {
                    files_done: 1,
                    bytes_done: 7,
                    ..Progress::default()
                },
            )
            .unwrap();
        queue.retry(&queued.operation_id).unwrap();
        let retried = &queue.snapshot().entries[0];
        assert_eq!(retried.state, QueueState::Pending);
        assert_eq!(retried.attempts, 1);
        assert_eq!(retried.progress, Progress::default());
        drop(queue);

        let mut snapshot: QueueSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot.version = 18;
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let reopened = OperationQueue::open(&path).unwrap();
        let migrated = &reopened.snapshot().entries[0];
        assert_eq!(migrated.state, QueueState::Interrupted);
        assert_eq!(migrated.attempts, 1);
        assert_eq!(migrated.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        assert_eq!(migrated.progress.phase, "recovery");
        assert_eq!(
            crate::error_code(&reopened.retry(&queued.operation_id).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
    }

    #[test]
    fn version_neunzehn_hebt_textmarker_auf_typisierten_neuplanungszustand() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let snapshot = QueueSnapshot {
            version: 19,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "v19-replan".into(),
                operation: webdav_delete_operation(),
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Failed,
                attempts: 1,
                progress: Progress::default(),
                attention: None,
                error: Some(LEGACY_REMOTE_IDENTITY_REPLAN_ERROR.into()),
                error_code: Some(crate::ErrorCode::PlanDrift),
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let queue = OperationQueue::open(path).unwrap();
        let migrated = queue.snapshot();
        assert_eq!(migrated.version, QUEUE_JOURNAL_VERSION);
        assert_eq!(
            migrated.entries[0].error_code,
            Some(crate::ErrorCode::ReplanRequired)
        );
        queue.discard_replan_required("v19-replan").unwrap();
        assert!(queue.snapshot().entries.is_empty());
    }

    #[test]
    fn altes_webdav_journal_bewahrt_moegliche_teilwirkung_und_terminalzustand() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let snapshot = QueueSnapshot {
            version: 15,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![
                QueueEntry {
                    operation_id: "laufend-webdav".into(),
                    operation: webdav_delete_operation(),
                    profile_bindings: Vec::new(),
                    created_unix_ms: 0,
                    state: QueueState::Running,
                    attempts: 1,
                    progress: Progress {
                        files_done: 1,
                        bytes_done: 7,
                        ..Progress::default()
                    },
                    attention: None,
                    error: None,
                    error_code: None,
                    temp_recovery_paths: Vec::new(),
                    retained_backup_paths: Vec::new(),
                    recovery_available: false,
                    bidirectional_completion_recovery: None,
                },
                QueueEntry {
                    operation_id: "fertig-webdav".into(),
                    operation: webdav_delete_operation(),
                    profile_bindings: Vec::new(),
                    created_unix_ms: 0,
                    state: QueueState::Completed,
                    attempts: 1,
                    progress: Progress {
                        phase: "done".into(),
                        files_done: 1,
                        bytes_done: 7,
                        ..Progress::default()
                    },
                    attention: None,
                    error: None,
                    error_code: None,
                    temp_recovery_paths: Vec::new(),
                    retained_backup_paths: Vec::new(),
                    recovery_available: false,
                    bidirectional_completion_recovery: None,
                },
            ],
        };
        let mut value = serde_json::to_value(snapshot).unwrap();
        for entry in value["entries"].as_array_mut().unwrap() {
            entry["operation"]["expected_plan"]["root_identity"]["identity"]
                .as_object_mut()
                .unwrap()
                .remove("root_etag");
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let queue = OperationQueue::open(path).unwrap();
        let snapshot = queue.snapshot();
        let interrupted = &snapshot.entries[0];
        assert_eq!(interrupted.state, QueueState::Interrupted);
        assert_eq!(interrupted.progress.files_done, 1);
        assert_eq!(interrupted.progress.bytes_done, 7);
        assert_eq!(
            interrupted.error.as_deref(),
            Some(LEGACY_WEBDAV_UNCLEAR_ERROR)
        );
        let error = queue.retry("laufend-webdav").unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let confirmation_error = queue
            .acknowledge_legacy_webdav_delete("laufend-webdav", false)
            .unwrap_err();
        assert_eq!(
            crate::error_code(&confirmation_error),
            crate::ErrorCode::ConfirmationMismatch
        );
        queue
            .acknowledge_legacy_webdav_delete("laufend-webdav", true)
            .unwrap();
        let acknowledged = queue
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == "laufend-webdav")
            .unwrap();
        assert_eq!(acknowledged.state, QueueState::Cancelled);
        assert_eq!(acknowledged.progress.phase, "recovery_acknowledged");
        assert!(acknowledged
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Manuelle Prüfung wurde bestätigt")));

        let completed = &snapshot.entries[1];
        assert_eq!(completed.state, QueueState::Completed);
        assert_eq!(completed.progress.phase, "done");
        assert!(completed.error.is_none());
        assert!(is_valid_legacy_unbound_webdav_delete(completed));
    }

    #[test]
    fn aktuelles_webdav_journal_ohne_root_etag_bleibt_beschaedigt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let snapshot = QueueSnapshot {
            version: 16,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "ungueltig-webdav".into(),
                operation: webdav_delete_operation(),
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Pending,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["entries"][0]["operation"]["expected_plan"]["root_identity"]["identity"]
            .as_object_mut()
            .unwrap()
            .remove("root_etag");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let error = match OperationQueue::open(path) {
            Ok(_) => panic!("ungültiges aktuelles WebDAV-Journal wurde geöffnet"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("WebDAV-Root-Identität"));
    }

    #[test]
    fn v14_lokale_ersetzer_migrieren_ohne_erfundene_bestaetigung() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let mut replace = file_operation("ersetzen");
        let QueueOperation::File { overwrite, .. } = &mut replace else {
            unreachable!()
        };
        *overwrite = true;
        let snapshot = QueueSnapshot {
            version: 14,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![
                QueueEntry {
                    operation_id: "alt-ersetzen".into(),
                    operation: replace,
                    profile_bindings: Vec::new(),
                    created_unix_ms: 0,
                    state: QueueState::Pending,
                    attempts: 0,
                    progress: Progress::default(),
                    attention: None,
                    error: None,
                    error_code: None,
                    temp_recovery_paths: Vec::new(),
                    retained_backup_paths: Vec::new(),
                    recovery_available: false,
                    bidirectional_completion_recovery: None,
                },
                QueueEntry {
                    operation_id: "alt-neu".into(),
                    operation: file_operation("neu"),
                    profile_bindings: Vec::new(),
                    created_unix_ms: 0,
                    state: QueueState::Pending,
                    attempts: 0,
                    progress: Progress::default(),
                    attention: None,
                    error: None,
                    error_code: None,
                    temp_recovery_paths: Vec::new(),
                    retained_backup_paths: Vec::new(),
                    recovery_available: false,
                    bidirectional_completion_recovery: None,
                },
            ],
        };
        let mut value = serde_json::to_value(snapshot).unwrap();
        for entry in value["entries"].as_array_mut().unwrap() {
            entry["operation"]
                .as_object_mut()
                .unwrap()
                .remove("local_replace_best_effort_expected");
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let migrated = OperationQueue::open(&path).unwrap().snapshot();
        assert_eq!(migrated.version, QUEUE_JOURNAL_VERSION);
        assert!(matches!(
            migrated.entries[0].operation,
            QueueOperation::File {
                local_replace_best_effort_expected: None,
                local_copy_new_best_effort_expected: None,
                ..
            }
        ));
        assert!(matches!(
            migrated.entries[1].operation,
            QueueOperation::File {
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
                ..
            }
        ));
    }

    #[test]
    fn v10_providerkonfliktname_wird_zum_gebundenen_zielpfad_migriert() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let path = root.join("queue.json");
        let snapshot = QueueSnapshot {
            version: 10,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "v10-provider".into(),
                operation: QueueOperation::ProviderFile {
                    source_tree_expected: None,
                    kind: FileQueueKind::Copy,
                    source: local_bidirectional_endpoint(&source),
                    target: local_bidirectional_endpoint(&target),
                    items: vec![source.join("datei").to_string_lossy().into_owned()],
                    destination: target.to_string_lossy().into_owned(),
                    overwrite: false,
                    expected_conflicts: vec!["datei".into()],
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: Vec::new(),
                    best_effort_commit_expected: None,
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: None,
                    replace_best_effort_expected: None,
                    remove_source_best_effort_expected: None,
                },
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Pending,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let queue = OperationQueue::open(&path).unwrap();
        let QueueOperation::ProviderFile {
            expected_conflicts, ..
        } = &queue.snapshot().entries[0].operation
        else {
            panic!("ProviderFile-Typ ging bei Migration verloren");
        };
        assert_eq!(
            expected_conflicts,
            &[target.join("datei").to_string_lossy().into_owned()]
        );
        drop(queue);
        let persisted: QueueSnapshot =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted.version, QUEUE_JOURNAL_VERSION);
    }

    #[test]
    fn v12_best_effort_erwartung_wird_wirkungsspezifisch_migriert() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let path = root.join("queue.json");
        let snapshot = QueueSnapshot {
            version: 12,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "v12-provider".into(),
                operation: QueueOperation::ProviderFile {
                    source_tree_expected: None,
                    kind: FileQueueKind::Copy,
                    source: local_bidirectional_endpoint(&source),
                    target: local_bidirectional_endpoint(&target),
                    items: vec![source.join("datei").to_string_lossy().into_owned()],
                    destination: target.to_string_lossy().into_owned(),
                    overwrite: true,
                    expected_conflicts: Vec::new(),
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: Vec::new(),
                    best_effort_commit_expected: Some(true),
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: None,
                    replace_best_effort_expected: None,
                    remove_source_best_effort_expected: None,
                },
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Pending,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let operation = OperationQueue::open(path).unwrap().snapshot().entries[0]
            .operation
            .clone();
        let QueueOperation::ProviderFile {
            best_effort_commit_expected,
            copy_new_best_effort_expected,
            replace_best_effort_expected,
            ..
        } = operation
        else {
            panic!("ProviderFile-Typ ging bei Migration verloren");
        };
        assert_eq!(best_effort_commit_expected, None);
        assert_eq!(copy_new_best_effort_expected, None);
        assert_eq!(replace_best_effort_expected, Some(true));
    }

    #[test]
    fn v13_move_quellklasse_wird_als_unbestaetigt_migriert() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let path = root.join("queue.json");
        let snapshot = QueueSnapshot {
            version: 13,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "v13-provider-move".into(),
                operation: QueueOperation::ProviderFile {
                    source_tree_expected: None,
                    kind: FileQueueKind::Move,
                    source: local_bidirectional_endpoint(&source),
                    target: local_bidirectional_endpoint(&target),
                    items: vec![source.join("datei").to_string_lossy().into_owned()],
                    destination: target.to_string_lossy().into_owned(),
                    overwrite: false,
                    expected_conflicts: Vec::new(),
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: Vec::new(),
                    best_effort_commit_expected: None,
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: Some(false),
                    replace_best_effort_expected: Some(false),
                    remove_source_best_effort_expected: None,
                },
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Pending,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        let mut encoded = serde_json::to_value(snapshot).unwrap();
        encoded["entries"][0]["operation"]
            .as_object_mut()
            .unwrap()
            .remove("remove_source_best_effort_expected");
        std::fs::write(&path, serde_json::to_vec(&encoded).unwrap()).unwrap();

        let queue = OperationQueue::open(&path).unwrap();
        let QueueOperation::ProviderFile {
            remove_source_best_effort_expected,
            ..
        } = &queue.snapshot().entries[0].operation
        else {
            panic!("ProviderFile-Typ ging bei Migration verloren");
        };
        assert_eq!(*remove_source_best_effort_expected, None);
        assert_eq!(queue.snapshot().version, QUEUE_JOURNAL_VERSION);
    }

    #[test]
    fn best_effort_downgrade_bestaetigung_bindet_erwartung_um() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        let entry = queue
            .enqueue(QueueOperation::ProviderFile {
                source_tree_expected: None,
                kind: FileQueueKind::Copy,
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                items: vec![source.join("datei").to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                best_effort_commit_expected: Some(false),
                local_copy_new_best_effort_expected: None,
                copy_new_best_effort_expected: Some(false),
                replace_best_effort_expected: Some(false),
                remove_source_best_effort_expected: Some(false),
            })
            .unwrap();
        // Executor-Sicht nachstellen: Claim, dann die Downgrade-Attention.
        let claimed = queue.claim_next().unwrap().unwrap();
        assert_eq!(claimed.operation_id, entry.operation_id);
        queue
            .require_confirmation(
                &entry.operation_id,
                QueueAttention::BestEffortCommitDowngrade {
                    reason: "Das Ziel bewies nur noch einen Best-Effort-Commit".into(),
                    confirmation: "BEST EFFORT test".into(),
                    copy_new_best_effort: true,
                    replace_best_effort: false,
                    remove_source_best_effort: true,
                },
            )
            .unwrap();
        // Regression: Die Bestätigung muss die gebundene Erwartung auf
        // `Some(true)` umbinden — sonst erfüllt derselbe Eintrag beim nächsten
        // Claim wieder die Downgrade-Bedingung und pausiert endlos.
        queue
            .approve_attention(&entry.operation_id, "BEST EFFORT test")
            .unwrap();
        let reclaimed = queue.claim_next().unwrap().unwrap();
        let QueueOperation::ProviderFile {
            copy_new_best_effort_expected,
            replace_best_effort_expected,
            remove_source_best_effort_expected,
            ..
        } = reclaimed.operation
        else {
            panic!("ProviderFile-Eintrag ging verloren");
        };
        assert_eq!(copy_new_best_effort_expected, Some(true));
        assert_eq!(replace_best_effort_expected, Some(false));
        assert_eq!(remove_source_best_effort_expected, Some(true));
    }

    #[test]
    fn claim_entry_claimt_nur_den_vordersten_eigenen_eintrag() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let older = queue.enqueue(file_operation("alt")).unwrap();
        let newer = queue.enqueue(file_operation("neu")).unwrap();
        // Fremder Kopf: Der neuere Eintrag wird nicht geclaimt, und der ältere
        // bleibt unangetastet `Pending` — kein zurückgelassenes `Validating`,
        // das beim nächsten Öffnen zu `Interrupted` würde.
        assert!(queue.claim_entry(&newer.operation_id).unwrap().is_none());
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries[0].state, QueueState::Pending);
        assert_eq!(snapshot.entries[1].state, QueueState::Pending);
        // Der vorderste eigene Eintrag wird exakt geclaimt.
        let claimed = queue.claim_entry(&older.operation_id).unwrap().unwrap();
        assert_eq!(claimed.operation_id, older.operation_id);
        assert_eq!(claimed.state, QueueState::Validating);
    }

    #[test]
    fn remote_laufphrase_wird_nach_neustart_als_interrupted_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let queued = queue.enqueue(sftp_operation()).unwrap();
        let _claimed = queue.claim_next().unwrap().unwrap();
        queue
            .require_confirmation(
                &queued.operation_id,
                QueueAttention::RemoteRun {
                    reason: "bereit".into(),
                    confirmation: "REAL ziel ABC123".into(),
                },
            )
            .unwrap();
        drop(queue);

        let recovered = OperationQueue::open(path).unwrap().snapshot();
        assert_eq!(recovered.entries[0].state, QueueState::Interrupted);
        assert!(recovered.entries[0].attention.is_none());
        assert!(recovered.entries[0]
            .error
            .as_deref()
            .unwrap()
            .contains("verloren"));
    }

    #[test]
    fn smb_journal_persistiert_nur_die_schluesselbundreferenz() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        queue.enqueue(smb_operation()).unwrap();
        drop(queue);

        let json = std::fs::read_to_string(path).unwrap();
        assert!(json.contains("smb-test"));
        assert!(!json.contains("password\":"));
        assert!(!json.contains("session"));
        assert!(!json.contains("permit"));
    }

    #[test]
    fn provider_queue_persistiert_zwei_endpunkte_ohne_secretwerte_oder_permit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        queue
            .enqueue(QueueOperation::ProviderSync {
                source: sftp_endpoint("provider-quelle", "/daten/quelle"),
                target: sftp_endpoint("provider-ziel", "/daten/ziel"),
                options: SyncOptions::default(),
                expected_plan: Box::new(SyncPlan::default()),
            })
            .unwrap();
        drop(queue);

        let json = std::fs::read_to_string(path).unwrap();
        assert!(json.contains("provider-quelle"));
        assert!(json.contains("provider-ziel"));
        assert!(!json.contains("password\":"));
        assert!(!json.contains("confirmation_phrase"));
        assert!(!json.contains("session_generation"));
        assert!(!json.contains("permit"));
    }

    #[test]
    fn sync_spaeterer_fehler_erhaelt_frueheren_stagingrest() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        LocalVfs::new().write_acl_exact(&target, None).unwrap();
        for name in ["a", "b"] {
            std::fs::write(source.join(name), vec![7; 262_144]).unwrap();
        }
        let vfs = LocalVfs::new();
        let plan = crate::plan(&vfs, &source, &vfs, &target, &SyncOptions::default()).unwrap();
        let error = crate::execute(
            crate::ExecuteRequest {
                src: &vfs,
                src_root: &source,
                dst: &vfs,
                dst_root: &target,
                plan: &plan,
                safety: &Safety::headless_test(),
            },
            &Cancel::new(),
            &mut |progress| {
                if progress.current == "a" && progress.bytes_done > 0 {
                    for stage in crate::atomic_file::staging_fault_tests::stages(&target) {
                        std::fs::set_permissions(stage, std::fs::Permissions::from_mode(0o755))
                            .unwrap();
                    }
                }
                if progress.current == "b" && source.join("b").exists() {
                    std::fs::remove_file(source.join("b")).unwrap();
                }
            },
        )
        .unwrap_err();
        assert!(target.join("a").is_file());
        assert!(!target.join("b").exists());
        let stages = crate::atomic_file::staging_fault_tests::stages(&target);
        assert_eq!(stages.len(), 1);
        let paths = crate::error::recovery_paths(&error);
        assert!(paths.contains(&stages[0].to_string_lossy().into_owned()));
        let journal = root.join("state/queue.json");
        let queue = OperationQueue::open(&journal).unwrap();
        let entry = queue.enqueue(file_operation("fruehere-reste")).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        queue
            .fail_coded(&entry.operation_id, Progress::default(), &error)
            .unwrap();
        drop(queue);
        assert_eq!(
            OperationQueue::open(journal).unwrap().snapshot().entries[0].temp_recovery_paths,
            paths
        );
    }

    #[test]
    fn provider_sync_erhaelt_stagingrest_terminal_und_bei_commitfehler() {
        use std::os::unix::fs::PermissionsExt;
        for mode in 0..3 {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            LocalVfs::new().write_acl_exact(&root, None).unwrap();
            let source = root.join("quelle");
            let target = root.join("ziel");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
            LocalVfs::new().write_acl_exact(&target, None).unwrap();
            std::fs::write(source.join("datei"), vec![7; 262_144]).unwrap();
            let options = SyncOptions::default();
            let plan = crate::plan(
                &LocalVfs::new(),
                &source,
                &LocalVfs::new(),
                &target,
                &options,
            )
            .unwrap();
            let journal = root.join("state/queue.json");
            let queue = OperationQueue::open(&journal).unwrap();
            queue
                .enqueue(QueueOperation::ProviderSync {
                    source: local_bidirectional_endpoint(&source),
                    target: local_bidirectional_endpoint(&target),
                    options,
                    expected_plan: Box::new(plan),
                })
                .unwrap();
            let claimed = queue.claim_next().unwrap().unwrap();
            let SftpQueuePreparation::Ready(prepared) =
                prepare_provider_queue_entry(&queue, claimed, no_secrets()).unwrap()
            else {
                panic!("Testendpunkte müssen ausführbar sein");
            };
            let confirmation = prepared.preview().confirmation_phrase.clone();
            let cancel = Cancel::new();
            let saved = root.join("saved-queue.json");
            let mut sabotaged = false;
            let result = execute_prepared_sftp_queue_entry(
                &queue,
                *prepared,
                &confirmation,
                false,
                &root.join("audit.jsonl"),
                &cancel,
                &mut |progress| {
                    if progress.bytes_done > 0 {
                        for stage in crate::atomic_file::staging_fault_tests::stages(&target)
                            .into_iter()
                            .chain(crate::atomic_file::staging_fault_tests::stages(&root))
                            .collect::<Vec<_>>()
                        {
                            std::fs::set_permissions(stage, std::fs::Permissions::from_mode(0o755))
                                .unwrap();
                        }
                        if mode == 1 {
                            cancel.cancel();
                        }
                    }
                    if mode == 2 && progress.phase == "done" && !sabotaged {
                        std::fs::rename(&journal, &saved).unwrap();
                        std::fs::create_dir(&journal).unwrap();
                        sabotaged = true;
                    }
                },
            );
            let stages = crate::atomic_file::staging_fault_tests::stages(&target)
                .into_iter()
                .chain(crate::atomic_file::staging_fault_tests::stages(&root))
                .collect::<Vec<_>>();
            assert_eq!(stages.len(), 1);
            let paths = vec![stages[0].to_string_lossy().into_owned()];
            if mode == 2 {
                let error = result.expect_err("Journalabschluss muss scheitern");
                assert_eq!(crate::error::recovery_paths(&error), paths);
                assert!(sabotaged);
                std::fs::remove_dir(&journal).unwrap();
                std::fs::rename(&saved, &journal).unwrap();
            } else {
                let QueueExecutionResult::Sync(stats) = result.unwrap() else {
                    panic!("Syncstats erwartet");
                };
                assert!(!stats.temp_recovery_paths.is_empty());
                for path in &stats.temp_recovery_paths {
                    assert!(Path::new(path).exists());
                    assert!(Path::new(path).starts_with(&stages[0]));
                }
                let paths = stats.temp_recovery_paths.clone();
                assert_eq!(stats.cancelled, mode == 1);
                drop(queue);
                let reopened = OperationQueue::open(&journal).unwrap();
                let snapshot = reopened.snapshot();
                assert_eq!(snapshot.entries[0].temp_recovery_paths, paths);
                assert_eq!(
                    snapshot.entries[0].state,
                    if mode == 1 {
                        QueueState::Cancelled
                    } else {
                        QueueState::Completed
                    }
                );
            }
        }
    }

    #[test]
    fn remote_sync_bleibt_nach_wirkung_trotz_abschlussaudit_terminal() {
        for (cancelled, expected_state, terminal_phase) in [
            (false, QueueState::Completed, "done"),
            (true, QueueState::Cancelled, "cancelled"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let source = root.join("quelle");
            let target = root.join("ziel");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::write(source.join("datei.txt"), b"vollstaendig").unwrap();
            let options = SyncOptions::default();
            let expected_plan = crate::plan(
                &LocalVfs::new(),
                &source,
                &LocalVfs::new(),
                &target,
                &options,
            )
            .unwrap();
            let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
            let queued = queue
                .enqueue(QueueOperation::ProviderSync {
                    source: local_bidirectional_endpoint(&source),
                    target: local_bidirectional_endpoint(&target),
                    options,
                    expected_plan: Box::new(expected_plan),
                })
                .unwrap();
            let claimed = queue.claim_next().unwrap().unwrap();
            let SftpQueuePreparation::Ready(prepared) =
                prepare_provider_queue_entry(&queue, claimed, no_secrets()).unwrap()
            else {
                panic!("lokale Testendpunkte müssen ausführbar sein");
            };
            let confirmation = prepared.preview().confirmation_phrase.clone();
            let audit = root.join("remote-audit.jsonl");
            let saved_audit = root.join("remote-audit.started.jsonl");
            let mut sabotaged = false;
            let cancel = Cancel::new();
            if cancelled {
                cancel.cancel();
            }

            let error = match execute_prepared_sftp_queue_entry(
                &queue,
                *prepared,
                &confirmation,
                false,
                &audit,
                &cancel,
                &mut |progress| {
                    if progress.phase == terminal_phase && !sabotaged {
                        std::fs::rename(&audit, &saved_audit).unwrap();
                        std::fs::create_dir(&audit).unwrap();
                        sabotaged = true;
                    }
                },
            ) {
                Ok(_) => panic!("erzwungener Abschlussauditfehler wurde verschluckt"),
                Err(error) => error,
            };

            assert!(
                sabotaged,
                "Test muss erst nach dem terminalen Fortschritt sabotieren"
            );
            assert_eq!(target.join("datei.txt").exists(), !cancelled);
            let entry = queue
                .snapshot()
                .entries
                .into_iter()
                .find(|entry| entry.operation_id == queued.operation_id)
                .unwrap();
            assert_eq!(entry.state, expected_state, "Auditfehler: {error}");
        }
    }

    #[test]
    fn remote_sync_committet_terminalen_fortschritt_nur_mit_dem_abschluss() {
        for (cancelled, expected_state, expected_phase, expected_files) in [
            (false, QueueState::Completed, "done", 1),
            (true, QueueState::Cancelled, "cancelled", 0),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let source = root.join("quelle");
            let target = root.join("ziel");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::write(source.join("datei.txt"), b"vollstaendig").unwrap();
            let options = SyncOptions::default();
            let expected_plan = crate::plan(
                &LocalVfs::new(),
                &source,
                &LocalVfs::new(),
                &target,
                &options,
            )
            .unwrap();
            let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
            queue
                .enqueue(QueueOperation::ProviderSync {
                    source: local_bidirectional_endpoint(&source),
                    target: local_bidirectional_endpoint(&target),
                    options,
                    expected_plan: Box::new(expected_plan),
                })
                .unwrap();
            let claimed = queue.claim_next().unwrap().unwrap();
            let SftpQueuePreparation::Ready(prepared) =
                prepare_provider_queue_entry(&queue, claimed, no_secrets()).unwrap()
            else {
                panic!("lokale Testendpunkte müssen ausführbar sein");
            };
            let confirmation = prepared.preview().confirmation_phrase.clone();
            let cancel = Cancel::new();
            if cancelled {
                cancel.cancel();
            }
            queue.fail_terminal_progress_update_for_test();

            execute_prepared_sftp_queue_entry(
                &queue,
                *prepared,
                &confirmation,
                false,
                &root.join("audit.jsonl"),
                &cancel,
                &mut |_| {},
            )
            .unwrap();

            let entry = &queue.snapshot().entries[0];
            assert_eq!(entry.state, expected_state);
            assert_eq!(entry.progress.phase, expected_phase);
            assert_eq!(entry.progress.files_done, expected_files);
            assert_eq!(queue.terminal_progress_update_attempts_for_test(), 0);
        }
    }

    #[test]
    fn remote_fortschrittsfehler_bleibt_als_unklarer_mutationsausgang_gesperrt() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("datei.bin"), vec![0x5a; 512 * 1024]).unwrap();
        let options = SyncOptions::default();
        let expected_plan = crate::plan(
            &LocalVfs::new(),
            &source,
            &LocalVfs::new(),
            &target,
            &options,
        )
        .unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let queued = queue
            .enqueue(QueueOperation::ProviderSync {
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                options,
                expected_plan: Box::new(expected_plan),
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let SftpQueuePreparation::Ready(prepared) =
            prepare_provider_queue_entry(&queue, claimed, no_secrets()).unwrap()
        else {
            panic!("lokale Testendpunkte müssen ausführbar sein");
        };
        let confirmation = prepared.preview().confirmation_phrase.clone();
        queue.fail_nonterminal_progress_update_after_for_test(0);

        let error = execute_prepared_sftp_queue_entry(
            &queue,
            *prepared,
            &confirmation,
            false,
            &root.join("audit.jsonl"),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let entry = queue
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == queued.operation_id)
            .unwrap();
        assert_eq!(entry.state, QueueState::Failed);
        assert_eq!(entry.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        let retry = queue.retry(&entry.operation_id).unwrap_err();
        assert_eq!(crate::error_code(&retry), crate::ErrorCode::OutcomeUnclear);
    }

    #[test]
    fn remote_sync_fehler_nach_done_persistiert_keinen_terminalen_fortschritt() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("datei.txt"), b"neu").unwrap();
        std::fs::write(target.join("datei.txt"), b"alt").unwrap();
        let options = SyncOptions {
            mode: SyncMode::Update,
            compare: Compare::ContentHash,
            ..SyncOptions::default()
        };
        let expected_plan = crate::plan(
            &LocalVfs::new(),
            &source,
            &LocalVfs::new(),
            &target,
            &options,
        )
        .unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::ProviderSync {
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                options,
                expected_plan: Box::new(expected_plan),
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let SftpQueuePreparation::Ready(prepared) =
            prepare_provider_queue_entry(&queue, claimed, no_secrets()).unwrap()
        else {
            panic!("lokale Testendpunkte müssen ausführbar sein");
        };
        let confirmation = prepared.preview().confirmation_phrase.clone();
        let quarantine = root.join(".doppeldecker-quarantine");
        let mut sabotaged = false;

        let error = match execute_prepared_sftp_queue_entry(
            &queue,
            *prepared,
            &confirmation,
            true,
            &root.join("audit.jsonl"),
            &Cancel::new(),
            &mut |progress| {
                if progress.phase == "done" && !sabotaged {
                    let manifest = std::fs::read_dir(&quarantine)
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .find(|path| {
                            path.extension()
                                .is_some_and(|extension| extension == "json")
                        })
                        .expect("aktiver Quarantänelauf braucht sein Manifest");
                    std::fs::rename(&manifest, manifest.with_extension("blockiert")).unwrap();
                    sabotaged = true;
                }
            },
        ) {
            Ok(_) => panic!("erzwungener Quarantäneabschlussfehler wurde verschluckt"),
            Err(error) => error,
        };

        assert!(sabotaged);
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(std::fs::read(target.join("datei.txt")).unwrap(), b"neu");
        let entry = &queue.snapshot().entries[0];
        assert_eq!(entry.state, QueueState::Failed);
        assert_eq!(entry.error_code, Some(crate::ErrorCode::NotFound));
        assert_eq!(entry.progress.phase, "failed");
        assert_ne!(entry.progress.phase, "done");
        assert_ne!(entry.progress.phase, "cancelled");
    }

    #[test]
    fn provider_delete_bleibt_nach_wirkung_trotz_abschlussaudit_terminal() {
        for (cancelled, expected_state) in [
            (false, QueueState::Completed),
            (true, QueueState::Cancelled),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state");
            let queue = OperationQueue::open(state.join("operations-queue.json")).unwrap();
            let queued = queue.enqueue(file_operation("delete-audit")).unwrap();
            queue.claim_next().unwrap().unwrap();
            queue.mark_running(&queued.operation_id).unwrap();
            std::fs::create_dir(state.join("operations-audit.jsonl")).unwrap();
            let context = ProviderAuditContext {
                source_provider: None,
                target_provider: crate::ProviderKind::Sftp,
                execution_class: ProviderAuditExecutionClass::Assured,
                source_removal_best_effort_expected: false,
                target_copy_new_best_effort_expected: false,
                target_replace_best_effort_expected: false,
                source_removal_best_effort_actual: None,
                target_copy_new_best_effort_actual: None,
                target_replace_best_effort_actual: None,
            };

            let error = match finish_provider_delete_queue_entry(
                &queue,
                &queued.operation_id,
                "provider_quarantine",
                Progress::default(),
                &context,
                crate::RemoteDeleteStats {
                    entries: 1,
                    bytes: 7,
                    cancelled,
                    ..crate::RemoteDeleteStats::default()
                },
            ) {
                Ok(_) => panic!("erzwungener Abschlussauditfehler wurde verschluckt"),
                Err(error) => error,
            };

            let entry = queue
                .snapshot()
                .entries
                .into_iter()
                .find(|entry| entry.operation_id == queued.operation_id)
                .unwrap();
            assert_eq!(entry.state, expected_state, "Auditfehler: {error}");
            assert_eq!(entry.progress.files_done, 1);
            assert_eq!(entry.progress.bytes_done, 7);
        }
    }

    #[test]
    fn zwei_konkurrierende_starter_claimen_nur_einmal() {
        let temp = tempfile::tempdir().unwrap();
        let queue = Arc::new(OperationQueue::open(temp.path().join("queue.json")).unwrap());
        queue.enqueue(file_operation("eins")).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let queue = queue.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                queue.claim_next().unwrap().is_some()
            }));
        }
        barrier.wait();
        let claimed = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|claimed| *claimed)
            .count();
        assert_eq!(claimed, 1);
        assert_eq!(queue.snapshot().entries[0].attempts, 1);
    }

    #[test]
    fn zweiter_prozessbesitzer_wird_durch_dateisperre_abgewiesen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let _owner = OperationQueue::open(&path).unwrap();
        let error = OperationQueue::open(&path).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn letzter_fensterbesitzer_gibt_lock_trotz_gehaltenem_fd_frei() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operations-queue.json");
        let owner = Arc::new(OperationQueueProcessLock::acquire(&path).unwrap());
        // try_clone hält dieselbe offene Dateibeschreibung wie ein bei fork
        // geerbter FD, ohne fremde Test-FDs in einen Kindprozess mitzunehmen.
        let _inherited = owner.file.try_clone().unwrap();
        let first = OperationQueue::open_with_process_lock(&path, owner.clone()).unwrap();
        let second = OperationQueue::open_with_process_lock(
            temp.path().join("operations-queue-window-2.json"),
            owner.clone(),
        )
        .unwrap();
        drop(owner);
        drop(first);
        assert_eq!(
            OperationQueueProcessLock::acquire(&path)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        drop(second);
        assert!(
            OperationQueueProcessLock::acquire(&path).is_ok(),
            "Ein geerbter FD darf den beendeten Queuebesitz nicht verlängern",
        );
    }

    #[test]
    fn geerbter_queuebesitzer_gibt_den_elternlock_nicht_frei() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operations-queue.json");
        let mut inherited = OperationQueueProcessLock::acquire(&path).unwrap();
        let parent_descriptor = inherited.file.try_clone().unwrap();
        // Der duplizierte FD hält den Elternbesitz; die abweichende PID
        // entspricht dem im Kind geerbten Besitzerobjekt.
        inherited.lock_owner ^= 1;
        drop(inherited);
        assert_eq!(
            OperationQueueProcessLock::acquire(&path)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        drop(parent_descriptor);
        assert!(OperationQueueProcessLock::acquire(&path).is_ok());
    }

    #[test]
    fn mehrere_fensterqueues_teilen_einen_prozessbesitzer() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("operations-queue.json");
        let second_path = temp.path().join("operations-queue-window-2.json");
        let process_lock = Arc::new(OperationQueueProcessLock::acquire(&first_path).unwrap());

        let first =
            OperationQueue::open_with_process_lock(&first_path, process_lock.clone()).unwrap();
        let second = OperationQueue::open_with_process_lock(&second_path, process_lock).unwrap();
        first.enqueue(file_operation("eins")).unwrap();
        second.enqueue(file_operation("zwei")).unwrap();

        assert_eq!(first.snapshot().entries.len(), 1);
        assert_eq!(second.snapshot().entries.len(), 1);
        assert_ne!(
            first.snapshot().entries[0].operation_id,
            second.snapshot().entries[0].operation_id
        );
        assert_eq!(
            OperationQueue::open(temp.path().join("third.json"))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn prozessabbruch_wird_als_interrupted_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let entry = queue.enqueue(file_operation("eins")).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        drop(queue);

        let queue = OperationQueue::open(&path).unwrap();
        let recovered = &queue.snapshot().entries[0];
        assert_eq!(recovered.state, QueueState::Interrupted);
        assert!(recovered
            .error
            .as_deref()
            .unwrap()
            .contains("vorherige Prozess"));
        assert!(queue.claim_next().unwrap().is_none());
        queue.retry(&entry.operation_id).unwrap();
        assert!(queue.claim_next().unwrap().is_some());
    }

    #[test]
    fn plandrift_braucht_explizite_neue_bestaetigung() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let options = SyncOptions {
            mode: SyncMode::AddOnly,
            compare: Compare::SizeAndTime,
            metadata: MetadataProfile::Portable,
            mtime_tolerance: 2,
            exclude: Vec::new(),
        };
        let old = SyncPlan::default();
        let mut current = old.clone();
        current.copy_count = 1;
        let entry = queue
            .enqueue(QueueOperation::Sync {
                source: "/tmp/quelle".into(),
                target: "/tmp/ziel".into(),
                sandbox: None,
                options,
                expected_plan: Box::new(old),
            })
            .unwrap();
        queue.claim_next().unwrap().unwrap();
        queue
            .require_confirmation(
                &entry.operation_id,
                QueueAttention::PlanChanged {
                    reason: "Plan geändert".into(),
                    confirmation: "PLAN TEST".into(),
                    current_plan: Box::new(current.clone()),
                },
            )
            .unwrap();
        assert!(queue.claim_next().unwrap().is_none());
        assert!(queue
            .approve_attention(&entry.operation_id, "falsch")
            .is_err());
        queue
            .approve_attention(&entry.operation_id, "PLAN TEST")
            .unwrap();
        let approved = queue.claim_next().unwrap().unwrap();
        match approved.operation {
            QueueOperation::Sync { expected_plan, .. } => assert_eq!(*expected_plan, current),
            _ => panic!("falsche Operation"),
        }
    }

    #[test]
    fn provider_plandrift_uebernimmt_nur_exakt_bestaetigten_plan() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("queue.json")).unwrap();
        let current = SyncPlan {
            copy_count: 1,
            ..SyncPlan::default()
        };
        let entry = queue
            .enqueue(QueueOperation::ProviderSync {
                source: local_bidirectional_endpoint(&source),
                target: local_bidirectional_endpoint(&target),
                options: SyncOptions::default(),
                expected_plan: Box::new(SyncPlan::default()),
            })
            .unwrap();
        queue.claim_next().unwrap().unwrap();
        queue
            .require_confirmation(
                &entry.operation_id,
                QueueAttention::PlanChanged {
                    reason: "Provider-Plan geändert".into(),
                    confirmation: "PLAN PROVIDER".into(),
                    current_plan: Box::new(current.clone()),
                },
            )
            .unwrap();
        assert!(queue
            .approve_attention(&entry.operation_id, "falsch")
            .is_err());
        queue
            .approve_attention(&entry.operation_id, "PLAN PROVIDER")
            .unwrap();
        let approved = queue.claim_next().unwrap().unwrap();
        let QueueOperation::ProviderSync { expected_plan, .. } = approved.operation else {
            panic!("falsche Operation");
        };
        assert_eq!(*expected_plan, current);
    }

    #[test]
    fn abbruch_und_fehler_stoppen_nur_nach_konfiguration() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let first = queue.enqueue(file_operation("eins")).unwrap();
        queue.enqueue(file_operation("zwei")).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue.mark_running(&first.operation_id).unwrap();
        queue
            .cancel(&first.operation_id, Progress::default())
            .unwrap();
        let second = queue.claim_next().unwrap().unwrap();
        queue.set_stop_on_error(true).unwrap();
        queue.mark_running(&second.operation_id).unwrap();
        queue
            .fail(&second.operation_id, Progress::default(), "Testfehler")
            .unwrap();
        assert!(queue.snapshot().paused);
        assert_eq!(queue.snapshot().entries[1].state, QueueState::Failed);
    }

    #[test]
    fn neue_kollision_braucht_laufbezogene_phrase() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let entry = queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec!["/tmp/neu".into()],
                destination: "/tmp/ziel".into(),
                overwrite: true,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        queue.claim_next().unwrap().unwrap();
        queue
            .require_confirmation(
                &entry.operation_id,
                QueueAttention::ConflictsChanged {
                    reason: "Neue Kollision".into(),
                    confirmation: "KONFLIKTE TEST 1".into(),
                    current_conflicts: vec!["/tmp/ziel/neu".into()],
                    current_conflict_proofs: Vec::new(),
                },
            )
            .unwrap();
        assert!(queue.approve_attention(&entry.operation_id, "ja").is_err());
        queue
            .approve_attention(&entry.operation_id, "KONFLIKTE TEST 1")
            .unwrap();
        let approved = queue.claim_next().unwrap().unwrap();
        match approved.operation {
            QueueOperation::File {
                expected_conflicts, ..
            } => assert_eq!(expected_conflicts, vec!["/tmp/ziel/neu"]),
            _ => panic!("falsche Operation"),
        }
    }

    #[test]
    fn verschwundene_kollision_setzt_lokale_best_effort_erwartung_zurueck() {
        let temp = tempfile::tempdir().unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let entry = queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec!["/tmp/quelle.txt".into()],
                destination: "/tmp/ziel".into(),
                overwrite: true,
                expected_conflicts: vec!["/tmp/ziel/quelle.txt".into()],
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(true),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        queue.claim_next().unwrap().unwrap();
        queue
            .require_confirmation(
                &entry.operation_id,
                QueueAttention::ConflictsChanged {
                    reason: "Konflikt ist verschwunden".into(),
                    confirmation: "KONFLIKTE LEER".into(),
                    current_conflicts: Vec::new(),
                    current_conflict_proofs: Vec::new(),
                },
            )
            .unwrap();
        queue
            .approve_attention(&entry.operation_id, "KONFLIKTE LEER")
            .unwrap();

        let approved = queue.claim_next().unwrap().unwrap();
        assert!(matches!(
            approved.operation,
            QueueOperation::File {
                expected_conflicts,
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
                ..
            } if expected_conflicts.is_empty()
        ));
    }

    #[test]
    fn tempdatei_ohne_persistierten_besitzbeleg_bleibt_unangetastet() {
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join(".operations-queue-alt.tmp");
        std::fs::write(&stale, b"abgebrochen").unwrap();
        let _queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        assert_eq!(std::fs::read(stale).unwrap(), b"abgebrochen");
    }

    /// Alle Fensterqueues teilen Ordner und Temppräfix. Eine zweite Queue darf
    /// deshalb keine Temp-Datei mehr anfassen: Sie kann die gerade geschriebene
    /// Datei eines aktiven Fensters sein, und deren Rename schlüge dann fehl.
    #[test]
    fn zweite_fensterqueue_entfernt_keine_aktive_tempdatei() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("operations-queue.json");
        let process_lock = Arc::new(OperationQueueProcessLock::acquire(&first_path).unwrap());
        let _first =
            OperationQueue::open_with_process_lock(first_path, process_lock.clone()).unwrap();

        // Steht für die halbfertige Journaldatei, die das erste Fenster gerade
        // schreibt — sie entsteht nach dem Erwerb der Prozesssperre.
        let active = temp.path().join(".operations-queue-aktiv.tmp");
        std::fs::write(&active, b"wird-gerade-geschrieben").unwrap();

        let second_path = temp.path().join("operations-queue-window-2.json");
        let _second = OperationQueue::open_with_process_lock(second_path, process_lock).unwrap();

        assert!(
            active.exists(),
            "Das Öffnen einer weiteren Fensterqueue hat eine aktive Temp-Datei entfernt"
        );
    }

    /// Eine Ordnerkopie muss ALLE tatsächlich zu ersetzenden Ziele als Nachweis
    /// mitbringen — auch die tiefer liegenden. Lokal läuft die bestätigte einzelne
    /// Dateiersetzung sichtbar als Best Effort und darf den übrigen Ordnerbestand
    /// nicht beeinträchtigen.
    #[test]
    fn ordnerkopie_bindet_auch_tiefere_zielkonflikte() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir_all(source.join("ordner")).unwrap();
        std::fs::write(source.join("ordner/tief.txt"), b"neu").unwrap();
        std::fs::write(source.join("ordner/frisch.txt"), b"frisch").unwrap();
        std::fs::create_dir_all(target.join("ordner")).unwrap();
        std::fs::write(target.join("ordner/tief.txt"), b"alt").unwrap();

        let vfs = LocalVfs::new();
        let items = vec![source.join("ordner")];
        let conflicts = find_recursive_conflicts(&vfs, &vfs, &target, &items).unwrap();
        assert_eq!(
            conflicts,
            vec![target
                .join("ordner/tief.txt")
                .to_string_lossy()
                .into_owned()],
            "Zwei gleichnamige Ordner sind kein Konflikt; nur das kollidierende Kind"
        );
        let proofs = capture_file_conflict_proofs(&vfs, &conflicts).unwrap();

        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: items
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
                destination: target.to_string_lossy().into_owned(),
                overwrite: true,
                expected_conflicts: conflicts,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: proofs,
                local_replace_best_effort_expected: Some(true),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let result = execute_local_queue_entry(
            &queue,
            claimed,
            &local_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();

        let recovery = std::fs::read_dir(target.join("ordner"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("doppeldecker-fileop-tmp"))
            });
        let snapshot = queue.snapshot();
        let entry = &snapshot.entries[0];
        let QueueExecutionResult::File(stats) = result else {
            panic!("Ordnerkopie lieferte keinen Dateiergebnis-Typ");
        };
        assert!(stats.target_replace_best_effort);
        assert_eq!(entry.state, QueueState::Completed);
        assert!(entry.error.is_none());
        assert!(
            recovery.is_none(),
            "eigene Tempdatei muss nach dem Commit fort sein"
        );
        assert_eq!(
            std::fs::read(target.join("ordner/tief.txt")).unwrap(),
            b"neu",
            "Die bereits sichtbar gewordene neue Datei bleibt unverändert"
        );
        assert_eq!(
            std::fs::read(target.join("ordner/frisch.txt")).unwrap(),
            b"frisch",
            "neue Geschwister werden vollständig ergänzt"
        );
        let records = std::fs::read_to_string(root.join("state/queue-audit.jsonl")).unwrap();
        let completed: serde_json::Value = records
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .find(|record: &serde_json::Value| record["status"] == "completed")
            .expect("lokaler Abschlussaudit fehlt");
        assert_eq!(completed["execution_class"], "best_effort");
        assert_eq!(completed["target_replace_best_effort_expected"], true);
        assert_eq!(completed["target_replace_best_effort_actual"], true);
    }

    /// Die Wartemeldungen sind das Einzige, was der Nutzer von der
    /// Ausführungsklasse sieht. Sie nennen deshalb zuerst die Wirkung, dann den
    /// Grund und die konkrete Providerwirkung. Eine fehlende frische Zustimmung
    /// ist dabei kein Beleg für einen tatsächlich geänderten Server.
    #[test]
    fn wartemeldung_nennt_wirkung_grund_und_providergrenzen() {
        let grenzen = vec![
            "kein exklusives Anlegen".to_owned(),
            "kein atomarer Commit".to_owned(),
        ];

        let beides = best_effort_target_reason(true, true, false, &grenzen);
        assert!(beides.starts_with("Nichts übertragen:"), "{beides}");
        assert!(
            beides.contains("das Anlegen neuer Dateien und das Ersetzen vorhandener"),
            "{beides}"
        );
        assert!(beides.contains("Ausführungsklasse Best Effort"), "{beides}");
        // Die Providergrenzen sagen als Einzige, woran es konkret liegt.
        assert!(
            beides.contains("kein exklusives Anlegen; kein atomarer Commit"),
            "{beides}"
        );

        // Ist nur eine Wirkung betroffen, nennt der Text auch nur diese.
        // Die angehängten Providergrenzen bleiben außen vor: Sie sind Servertext und
        // können jedes Wort enthalten, auch das der anderen Wirkung.
        let wirkungssatz = |text: &str| {
            text.split("Grenzen des Ziels:")
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let nur_anlegen = wirkungssatz(&best_effort_target_reason(true, false, false, &grenzen));
        assert!(
            nur_anlegen.contains("das Anlegen neuer Dateien"),
            "{nur_anlegen}"
        );
        assert!(!nur_anlegen.contains("Ersetzen"), "{nur_anlegen}");

        let nur_ersetzen = wirkungssatz(&best_effort_target_reason(false, true, false, &grenzen));
        assert!(
            nur_ersetzen.contains("das Ersetzen vorhandener Dateien"),
            "{nur_ersetzen}"
        );
        assert!(!nur_ersetzen.contains("Anlegen"), "{nur_ersetzen}");
        let frisch = best_effort_target_reason(true, false, true, &grenzen);
        assert!(
            frisch.contains("fehlt eine aktuelle Bestätigung"),
            "{frisch}"
        );
        assert!(!frisch.contains("nicht mehr so"), "{frisch}");
    }

    #[test]
    fn lokale_best_effort_ersetzung_braucht_persistierte_bestaetigung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        let conflict = target.join("quelle.txt");
        std::fs::write(&source, b"neu").unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(&conflict, b"alt").unwrap();
        let proofs = capture_file_conflict_proofs(
            &LocalVfs::new(),
            &[conflict.to_string_lossy().into_owned()],
        )
        .unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let entry = queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: true,
                expected_conflicts: vec![conflict.to_string_lossy().into_owned()],
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: proofs,
                local_replace_best_effort_expected: None,
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();

        let first = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &local_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(
            first,
            QueueExecutionResult::AwaitingConfirmation { .. }
        ));
        assert_eq!(std::fs::read(&conflict).unwrap(), b"alt");
        let phrase = match queue.snapshot().entries[0].attention.as_ref().unwrap() {
            QueueAttention::BestEffortCommitDowngrade {
                confirmation,
                reason,
                ..
            } => {
                // Der Text steht so im Fehlerbalken und im Queue-Eintrag: Wirkung
                // zuerst, Fachwort nur als Zusatz in Klammern.
                assert!(reason.starts_with("Nichts überschrieben:"), "{reason}");
                assert!(reason.contains("Ausführungsklasse Best Effort"), "{reason}");
                confirmation.clone()
            }
            _ => panic!("falscher Attention-Typ"),
        };
        queue
            .approve_attention(&entry.operation_id, &phrase)
            .unwrap();
        let approved = queue.claim_next().unwrap().unwrap();
        assert!(matches!(
            approved.operation,
            QueueOperation::File {
                local_replace_best_effort_expected: Some(true),
                local_copy_new_best_effort_expected: None,
                ..
            }
        ));
    }

    #[test]
    fn lokaler_executor_auditiert_actuals_auch_bei_spaetem_fehler() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_dir = root.join("quelle");
        let target_dir = root.join("ziel");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();
        let first_source = source_dir.join("a.txt");
        let second_source = source_dir.join("b.txt");
        let first_target = target_dir.join("a.txt");
        let second_target = target_dir.join("b.txt");
        std::fs::write(&first_source, b"neu-a").unwrap();
        std::fs::write(&second_source, b"neu-b").unwrap();
        std::fs::write(&first_target, b"alt-a").unwrap();
        std::fs::write(&second_target, b"alt-b").unwrap();
        let conflicts = vec![
            first_target.to_string_lossy().into_owned(),
            second_target.to_string_lossy().into_owned(),
        ];
        let proofs = capture_file_conflict_proofs(&LocalVfs::new(), &conflicts).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![
                    first_source.to_string_lossy().into_owned(),
                    second_source.to_string_lossy().into_owned(),
                ],
                destination: target_dir.to_string_lossy().into_owned(),
                overwrite: true,
                expected_conflicts: conflicts,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: proofs,
                local_replace_best_effort_expected: Some(true),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let mut removed = false;
        let failure = match execute_local_queue_entry_detailed(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &local_execution(),
            &Cancel::new(),
            &mut |_| {
                if !removed && std::fs::read(&first_target).is_ok_and(|bytes| bytes == b"neu-a") {
                    std::fs::remove_file(&second_source).unwrap();
                    removed = true;
                }
            },
        ) {
            Ok(_) => panic!("der entfernte zweite Quellname muss den Lauf stoppen"),
            Err(failure) => failure,
        };

        assert_eq!(failure.actuals.target_replace_best_effort, Some(true));
        assert_eq!(std::fs::read(&first_target).unwrap(), b"neu-a");
        assert_eq!(std::fs::read(&second_target).unwrap(), b"alt-b");
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries[0].state, QueueState::Failed);
        assert_eq!(
            snapshot.entries[0].progress.files_done, 1,
            "der letzte gedrosselte Fortschritt muss vor dem Fehler persistiert werden"
        );
        assert_eq!(snapshot.entries[0].progress.bytes_done, 5);
        let records = std::fs::read_to_string(root.join("state/queue-audit.jsonl")).unwrap();
        let failed: serde_json::Value = records
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .find(|record: &serde_json::Value| record["status"] == "failed")
            .expect("lokaler Fehleraudit fehlt");
        assert_eq!(failed["target_replace_best_effort_actual"], true);
    }

    #[test]
    fn vollzogene_dateiwirkung_retryt_nur_den_terminalen_abschluss() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"vollstaendig").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_str().unwrap().to_owned()],
                destination: target.to_str().unwrap().to_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        queue.fail_next_file_completion_commit_for_test();

        let result = execute_local_queue_entry_detailed(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &local_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .expect("der ausschließlich terminale Retry muss den Erfolg zurückgeben");

        assert!(matches!(result, QueueExecutionResult::File(stats) if stats.files == 1));
        assert_eq!(
            std::fs::read(target.join("quelle.txt")).unwrap(),
            b"vollstaendig"
        );
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries[0].state, QueueState::Completed);
        assert_eq!(snapshot.entries[0].progress.phase, "done");
        assert!(queue.retry(&snapshot.entries[0].operation_id).is_err());
        let audit = std::fs::read_to_string(root.join("state/queue-audit.jsonl")).unwrap();
        let statuses = audit
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["status"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                serde_json::Value::String("started".into()),
                serde_json::Value::String("completed".into())
            ]
        );
    }

    #[test]
    fn abschlussauditfehler_macht_vollzogene_dateioperation_nicht_retryfaehig() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"vollstaendig").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_str().unwrap().to_owned()],
                destination: target.to_str().unwrap().to_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let audit = root.join("state/queue-audit.jsonl");
        let displaced_audit = root.join("state/queue-audit-vor-fehler.jsonl");
        let mut blocked = false;

        let failure = match execute_local_queue_entry_detailed(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &local_execution(),
            &Cancel::new(),
            &mut |progress| {
                if progress.phase == "done" && !blocked {
                    std::fs::rename(&audit, &displaced_audit).unwrap();
                    std::fs::create_dir(&audit).unwrap();
                    blocked = true;
                }
            },
        ) {
            Ok(_) => panic!("der absichtlich blockierte Abschlussaudit muss sichtbar scheitern"),
            Err(failure) => failure,
        };

        assert_eq!(failure.error.kind(), io::ErrorKind::IsADirectory);
        assert_eq!(
            std::fs::read(target.join("quelle.txt")).unwrap(),
            b"vollstaendig"
        );
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries[0].state, QueueState::Completed);
        assert_eq!(snapshot.entries[0].progress.phase, "done");
        assert!(snapshot.entries[0].error.is_none());
    }

    #[test]
    fn persistenzfehler_hat_vorrang_vor_spaetem_dateifehler() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_dir = root.join("quelle");
        let target_dir = root.join("ziel");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();
        let first_source = source_dir.join("a.txt");
        let second_source = source_dir.join("b.txt");
        let first_target = target_dir.join("a.txt");
        let second_target = target_dir.join("b.txt");
        std::fs::write(&first_source, b"neu-a").unwrap();
        std::fs::write(&second_source, b"neu-b").unwrap();
        std::fs::write(&first_target, b"alt-a").unwrap();
        std::fs::write(&second_target, b"alt-b").unwrap();
        let conflicts = vec![
            first_target.to_str().unwrap().to_owned(),
            second_target.to_str().unwrap().to_owned(),
        ];
        let proofs = capture_file_conflict_proofs(&LocalVfs::new(), &conflicts).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![
                    first_source.to_str().unwrap().to_owned(),
                    second_source.to_str().unwrap().to_owned(),
                ],
                destination: target_dir.to_str().unwrap().to_owned(),
                overwrite: true,
                expected_conflicts: conflicts,
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: proofs,
                local_replace_best_effort_expected: Some(true),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let mut armed = false;
        let failure = execute_local_queue_entry_detailed(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &local_execution(),
            &Cancel::new(),
            &mut |_| {
                if !armed && std::fs::read(&first_target).is_ok_and(|bytes| bytes == b"neu-a") {
                    // Die vorherige Callback-Momentaufnahme ist bereits geschrieben.
                    // Ab jetzt simuliert das Testflag einen unklaren Journalzustand;
                    // zugleich erzeugen wir den späteren eigentlichen Dateifehler.
                    queue.journal_outcome_unclear.store(true, Ordering::Release);
                    std::fs::remove_file(&second_source).unwrap();
                    armed = true;
                }
            },
        )
        .err()
        .unwrap();

        assert_eq!(
            crate::error_code(&failure.error),
            crate::ErrorCode::OutcomeUnclear
        );
        assert!(failure.error.to_string().contains("Queue-Fortschritt"));
        assert_eq!(std::fs::read(&first_target).unwrap(), b"neu-a");
        assert_eq!(std::fs::read(&second_target).unwrap(), b"alt-b");
    }

    #[test]
    fn lokale_dateikopie_laeuft_bis_completed() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        execute_local_queue_entry(
            &queue,
            claimed,
            &local_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
        assert_eq!(std::fs::read(target.join("quelle.txt")).unwrap(), b"inhalt");
    }

    #[test]
    fn spaete_ersetzungskollision_stoppt_vor_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"neu").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: true,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        std::fs::write(target.join("quelle.txt"), b"fremd").unwrap();
        execute_local_queue_entry(
            &queue,
            claimed,
            &local_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        assert_eq!(std::fs::read(target.join("quelle.txt")).unwrap(), b"fremd");
    }

    #[test]
    fn lokaler_abbruch_laesst_quelle_und_ziel_unveraendert() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let target = root.join("ziel");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&target).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: vec![source.to_string_lossy().into_owned()],
                destination: target.to_string_lossy().into_owned(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                keep_both_targets: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let cancel = Cancel::new();
        cancel.cancel();
        execute_local_queue_entry(&queue, claimed, &local_execution(), &cancel, &mut |_| {})
            .unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Cancelled);
        assert!(!target.join("quelle.txt").exists());
        assert_eq!(std::fs::read(source).unwrap(), b"inhalt");
    }

    #[test]
    fn lokaler_sync_drift_stoppt_vor_ausfuehrung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let options = SyncOptions::default();
        let vfs = LocalVfs::new();
        let expected = crate::plan(&vfs, &source, &vfs, &target, &options).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::Sync {
                source: source.to_string_lossy().into_owned(),
                target: target.to_string_lossy().into_owned(),
                sandbox: None,
                options,
                expected_plan: Box::new(expected),
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        std::fs::write(source.join("spaet.txt"), b"drift").unwrap();
        execute_local_queue_entry(
            &queue,
            claimed,
            &local_execution(),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        assert!(std::fs::read_dir(target).unwrap().next().is_none());
    }

    #[test]
    fn lokaler_sync_persistiert_letzten_fortschritt_vor_fehler() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(
            root.join(crate::TEST_SANDBOX_MARKER),
            crate::TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();
        std::fs::write(source.join("a.txt"), b"aaaa").unwrap();
        std::fs::write(source.join("b.txt"), b"bbbb").unwrap();
        let options = SyncOptions::default();
        let vfs = LocalVfs::new();
        let expected = crate::plan(&vfs, &source, &vfs, &target, &options).unwrap();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::Sync {
                source: source.to_str().unwrap().to_owned(),
                target: target.to_str().unwrap().to_owned(),
                sandbox: Some(root.to_str().unwrap().to_owned()),
                options,
                expected_plan: Box::new(expected),
            })
            .unwrap();
        let mut removed = false;

        let execution = LocalQueueExecution {
            sandbox: Some(root.clone()),
            ..local_execution()
        };
        let error = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution,
            &Cancel::new(),
            &mut |progress| {
                if !removed && progress.files_done == 1 {
                    // Nach der ersten fertigen Datei entfernen wir beide kleinen
                    // Testquellen. Die bereits kopierte Datei ist abgeschlossen;
                    // unabhängig von der Sortierreihenfolge fehlt damit die zweite
                    // Quelle sicher erst beim folgenden Öffnen.
                    for name in ["a.txt", "b.txt"] {
                        if source.join(name).exists() {
                            std::fs::remove_file(source.join(name)).unwrap();
                        }
                    }
                    removed = true;
                }
            },
        )
        .err()
        .unwrap();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let entry = &queue.snapshot().entries[0];
        assert_eq!(entry.state, QueueState::Failed);
        assert_eq!(entry.progress.files_done, 1);
        assert_eq!(entry.progress.bytes_done, 4);
    }

    #[test]
    fn lokaler_sync_committet_terminalen_fortschritt_nur_mit_dem_abschluss() {
        for (cancelled, expected_state, expected_phase, expected_files) in [
            (false, QueueState::Completed, "done", 1),
            (true, QueueState::Cancelled, "cancelled", 0),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let source = root.join("quelle");
            let target = root.join("ziel");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::write(source.join("datei.txt"), b"vollstaendig").unwrap();
            let options = SyncOptions::default();
            let expected_plan = crate::plan(
                &LocalVfs::new(),
                &source,
                &LocalVfs::new(),
                &target,
                &options,
            )
            .unwrap();
            let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
            queue
                .enqueue(QueueOperation::Sync {
                    source: source.to_str().unwrap().to_owned(),
                    target: target.to_str().unwrap().to_owned(),
                    sandbox: None,
                    options,
                    expected_plan: Box::new(expected_plan),
                })
                .unwrap();
            let cancel = Cancel::new();
            if cancelled {
                cancel.cancel();
            }
            queue.fail_terminal_progress_update_for_test();

            execute_local_queue_entry(
                &queue,
                queue.claim_next().unwrap().unwrap(),
                &local_execution(),
                &cancel,
                &mut |_| {},
            )
            .unwrap();

            let entry = &queue.snapshot().entries[0];
            assert_eq!(entry.state, expected_state);
            assert_eq!(entry.progress.phase, expected_phase);
            assert_eq!(entry.progress.files_done, expected_files);
            assert_eq!(queue.terminal_progress_update_attempts_for_test(), 0);
        }
    }

    #[test]
    fn bidirektionale_queue_persistiert_nur_digests_und_frische_entscheidungen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline_path = root.join("state/paar.json");
        let pair_id = PairId([77; 16]);
        let draft =
            crate::prepare_local_bidirectional(pair_id, "Queuepaar", &left, &right, &baseline_path)
                .unwrap();
        let baseline_digest = draft.preview().plan.baseline_digest;
        let plan_digest = draft.preview().plan.plan_digest;
        draft.discard().unwrap();
        drop(draft);

        let queue_path = root.join("state/queue.json");
        let queue = OperationQueue::open(&queue_path).unwrap();
        queue
            .enqueue(QueueOperation::Bidirectional {
                pair_id,
                pair_name: "Queuepaar".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: baseline_digest,
                expected_plan_digest: plan_digest,
            })
            .unwrap();
        let persisted = std::fs::read_to_string(&queue_path).unwrap();
        assert!(!persisted.contains("BEIDE Queuepaar"));
        assert!(!persisted.contains("confirmation_phrase"));

        let claimed = queue.claim_next().unwrap().unwrap();
        let BidirectionalQueuePreparation::Ready(prepared) =
            prepare_bidirectional_queue_entry(&queue, claimed, no_secrets()).unwrap()
        else {
            panic!("identischer Queue-Plan wartet unerwartet auf Driftbestätigung");
        };
        let phrase = prepared.preview().confirmation_phrase.clone();
        let result = execute_prepared_bidirectional_queue_entry(
            &queue,
            *prepared,
            &phrase,
            false,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let QueueExecutionResult::Bidirectional(stats) = result else {
            panic!("bidirektionaler Queue-Lauf lieferte falschen Ergebnistyp");
        };
        assert_eq!(stats.baseline_generation, 1);
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    }

    #[test]
    fn bidirektionaler_lauf_committet_terminalen_fortschritt_nur_mit_dem_abschluss() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("datei.txt"), b"vollstaendig").unwrap();
        let baseline_path = root.join("state/paar.json");
        let pair_id = PairId([78; 16]);
        let resolutions = vec![ConflictResolution {
            path: crate::BinaryRelativePath(vec![b"datei.txt".to_vec()]),
            decision: crate::ConflictDecision::KeepLeft,
        }];
        let draft = crate::prepare_local_bidirectional_with_decisions(
            pair_id,
            "Queuepaar",
            &left,
            &right,
            &baseline_path,
            &resolutions,
        )
        .unwrap();
        let baseline_digest = draft.preview().plan.baseline_digest;
        let plan_digest = draft.preview().plan.plan_digest;
        draft.discard().unwrap();
        drop(draft);

        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::Bidirectional {
                pair_id,
                pair_name: "Queuepaar".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions,
                expected_baseline_digest: baseline_digest,
                expected_plan_digest: plan_digest,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let BidirectionalQueuePreparation::Ready(prepared) =
            prepare_bidirectional_queue_entry(&queue, claimed, no_secrets()).unwrap()
        else {
            panic!("identischer Queue-Plan wartet unerwartet auf Driftbestätigung");
        };
        let phrase = prepared.preview().confirmation_phrase.clone();
        queue.fail_terminal_progress_update_for_test();

        execute_prepared_bidirectional_queue_entry(
            &queue,
            *prepared,
            &phrase,
            false,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();

        let entry = &queue.snapshot().entries[0];
        assert_eq!(entry.state, QueueState::Completed);
        assert_eq!(entry.progress.phase, "done");
        assert_eq!(entry.progress.files_done, 1);
        assert_eq!(queue.terminal_progress_update_attempts_for_test(), 0);
        assert_eq!(
            std::fs::read(right.join("datei.txt")).unwrap(),
            b"vollstaendig"
        );
    }

    #[test]
    fn bidirektionale_queue_erhaelt_reste_bei_erfolg_und_abschlussfehler() {
        use std::collections::BTreeSet;
        use std::os::unix::fs::PermissionsExt;
        for sabotage in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            LocalVfs::new().write_acl_exact(&root, None).unwrap();
            let left = root.join("links");
            let right = root.join("rechts");
            std::fs::create_dir(&left).unwrap();
            std::fs::create_dir(&right).unwrap();
            std::fs::write(left.join("a"), vec![7; 262_144]).unwrap();
            std::fs::write(right.join("b"), vec![8; 262_144]).unwrap();
            let baseline_path = root.join("state/paar.json");
            let pair_id = PairId([98; 16]);
            let resolutions = vec![
                ConflictResolution {
                    path: crate::BinaryRelativePath(vec![b"a".to_vec()]),
                    decision: crate::ConflictDecision::KeepLeft,
                },
                ConflictResolution {
                    path: crate::BinaryRelativePath(vec![b"b".to_vec()]),
                    decision: crate::ConflictDecision::KeepRight,
                },
            ];
            let draft = crate::prepare_local_bidirectional_with_decisions(
                pair_id,
                "Recoverytest",
                &left,
                &right,
                &baseline_path,
                &resolutions,
            )
            .unwrap();
            let baseline_digest = draft.preview().plan.baseline_digest;
            let plan_digest = draft.preview().plan.plan_digest;
            draft.discard().unwrap();
            drop(draft);
            let journal = root.join("state/queue.json");
            let saved = root.join("saved-queue.json");
            let queue = OperationQueue::open(&journal).unwrap();
            queue
                .enqueue(QueueOperation::Bidirectional {
                    pair_id,
                    pair_name: "Recoverytest".into(),
                    left: local_bidirectional_endpoint(&left),
                    right: local_bidirectional_endpoint(&right),
                    baseline_path: baseline_path.to_string_lossy().into_owned(),
                    resolutions,
                    expected_baseline_digest: baseline_digest,
                    expected_plan_digest: plan_digest,
                })
                .unwrap();
            let claimed = queue.claim_next().unwrap().unwrap();
            let BidirectionalQueuePreparation::Ready(prepared) =
                prepare_bidirectional_queue_entry(&queue, claimed, no_secrets()).unwrap()
            else {
                panic!("Testplan muss ausführbar sein");
            };
            let phrase = prepared.preview().confirmation_phrase.clone();
            let mut finished = 0;
            let result = execute_prepared_bidirectional_queue_entry(
                &queue,
                *prepared,
                &phrase,
                false,
                &Cancel::new(),
                &mut |progress| {
                    if progress.bytes_done > 0 {
                        for stage in crate::atomic_file::staging_fault_tests::stages(&root) {
                            std::fs::set_permissions(stage, std::fs::Permissions::from_mode(0o755))
                                .unwrap();
                        }
                    }
                    if progress.phase == "done" {
                        finished += 1;
                        if sabotage && finished == 2 {
                            std::fs::rename(&journal, &saved).unwrap();
                            std::fs::create_dir(&journal).unwrap();
                        }
                    }
                },
            );
            assert_eq!(finished, 2);
            let stages = crate::atomic_file::staging_fault_tests::stages(&root);
            assert_eq!(stages.len(), 2);
            let expected: BTreeSet<_> = stages
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            if sabotage {
                let error = result.expect_err("Queueabschluss muss scheitern");
                assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                assert_eq!(
                    crate::error::recovery_paths(&error)
                        .into_iter()
                        .collect::<BTreeSet<_>>(),
                    expected
                );
                std::fs::remove_dir(&journal).unwrap();
                std::fs::rename(&saved, &journal).unwrap();
            } else {
                let QueueExecutionResult::Bidirectional(stats) = result.unwrap() else {
                    panic!("Bidirektionale Stats erwartet");
                };
                assert_eq!(
                    stats
                        .runs
                        .iter()
                        .flat_map(|run| run.temp_recovery_paths.iter().cloned())
                        .collect::<BTreeSet<_>>(),
                    expected
                );
                drop(queue);
                let reopened = OperationQueue::open(&journal).unwrap();
                let entry = &reopened.snapshot().entries[0];
                assert_eq!(entry.state, QueueState::Completed);
                assert_eq!(
                    entry
                        .temp_recovery_paths
                        .iter()
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                    expected
                );
            }
            assert_eq!(std::fs::read(right.join("a")).unwrap(), vec![7; 262_144]);
            assert_eq!(std::fs::read(left.join("b")).unwrap(), vec![8; 262_144]);
        }
    }

    #[test]
    fn bidirektionaler_mehrprojektionsfehler_markiert_offenen_recoveryfortschritt() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("von-links.txt"), b"links").unwrap();
        std::fs::write(right.join("von-rechts.txt"), b"rechts").unwrap();
        let baseline_path = root.join("state/paar.json");
        let pair_id = PairId([79; 16]);
        let resolutions = vec![
            ConflictResolution {
                path: crate::BinaryRelativePath(vec![b"von-links.txt".to_vec()]),
                decision: crate::ConflictDecision::KeepLeft,
            },
            ConflictResolution {
                path: crate::BinaryRelativePath(vec![b"von-rechts.txt".to_vec()]),
                decision: crate::ConflictDecision::KeepRight,
            },
        ];
        let draft = crate::prepare_local_bidirectional_with_decisions(
            pair_id,
            "Mehrprojektion",
            &left,
            &right,
            &baseline_path,
            &resolutions,
        )
        .unwrap();
        let baseline_digest = draft.preview().plan.baseline_digest;
        let plan_digest = draft.preview().plan.plan_digest;
        draft.discard().unwrap();
        drop(draft);
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::Bidirectional {
                pair_id,
                pair_name: "Mehrprojektion".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions,
                expected_baseline_digest: baseline_digest,
                expected_plan_digest: plan_digest,
            })
            .unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let BidirectionalQueuePreparation::Ready(prepared) =
            prepare_bidirectional_queue_entry(&queue, claimed, no_secrets()).unwrap()
        else {
            panic!("identischer Mehrprojektionsplan muss ausführbar sein");
        };
        let phrase = prepared.preview().confirmation_phrase.clone();
        let mut completed_projections = 0_u64;
        let run_journal = baseline_path.with_extension("run.json");

        let error = match execute_prepared_bidirectional_queue_entry(
            &queue,
            *prepared,
            &phrase,
            false,
            &Cancel::new(),
            &mut |progress| {
                if progress.phase == "done" {
                    completed_projections += 1;
                    if completed_projections == 1 {
                        // Die erste Projektion ist vollständig. Der beschädigte
                        // Laufdatensatz erzwingt den späteren Fehler noch vor dem
                        // ersten Progress-Event der zweiten Projektion.
                        std::fs::write(&run_journal, b"{").unwrap();
                    }
                }
            },
        ) {
            Ok(_) => panic!("Mehrprojektionsdrift wurde verschluckt"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(completed_projections, 1);
        let entry = &queue.snapshot().entries[0];
        assert_eq!(entry.state, QueueState::Interrupted);
        assert!(entry.recovery_available);
        assert_eq!(entry.progress.phase, "recovery");
        assert_ne!(entry.progress.phase, "done");
    }

    #[test]
    fn oeffnen_normalisiert_bidirektionalen_done_fortschritt_nach_recoveryklasse() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();

        let resume_baseline = root.join("state/resume.json");
        let prepared = crate::prepare_local_bidirectional(
            PairId([80; 16]),
            "Recovery beim Oeffnen",
            &left,
            &right,
            &resume_baseline,
        )
        .unwrap();
        let resume_operation = QueueOperation::Bidirectional {
            pair_id: PairId([80; 16]),
            pair_name: "Recovery beim Oeffnen".into(),
            left: local_bidirectional_endpoint(&left),
            right: local_bidirectional_endpoint(&right),
            baseline_path: resume_baseline.to_string_lossy().into_owned(),
            resolutions: Vec::new(),
            expected_baseline_digest: prepared.preview().plan.baseline_digest,
            expected_plan_digest: prepared.preview().plan.plan_digest,
        };
        let resume_path = root.join("resume/queue.json");
        let resume_queue = OperationQueue::open(&resume_path).unwrap();
        let resume_entry = resume_queue.enqueue_claimed(resume_operation).unwrap();
        resume_queue
            .mark_running(&resume_entry.operation_id)
            .unwrap();
        // Dieser Test bildet einen bereits persistierten Altstand ab. Die
        // öffentliche Fehler-API darf einen bidirektionalen Lauf dagegen nie
        // ohne Prüfung seines Core-Journals terminalisieren.
        resume_queue
            .transact(|journal| {
                let entry = find_mut(journal, &resume_entry.operation_id)?;
                entry.state = QueueState::Failed;
                entry.progress = Progress {
                    phase: "done".into(),
                    current: "alte-datei.txt".into(),
                    ..Progress::default()
                };
                entry.error = Some("alter Ausfuehrungsfehler".into());
                Ok(())
            })
            .unwrap();
        drop(resume_queue);

        let recovered = OperationQueue::open(resume_path).unwrap().snapshot();
        assert_eq!(recovered.entries[0].state, QueueState::Interrupted);
        assert_eq!(recovered.entries[0].progress.phase, "recovery");
        assert!(recovered.entries[0].progress.current.is_empty());
        assert!(recovered.entries[0].recovery_available);
        drop(prepared);

        let absent_operation = QueueOperation::Bidirectional {
            pair_id: PairId([81; 16]),
            pair_name: "Abbruch ohne Laufjournal".into(),
            left: local_bidirectional_endpoint(&left),
            right: local_bidirectional_endpoint(&right),
            baseline_path: root
                .join("state/ohne-laufjournal.json")
                .to_string_lossy()
                .into_owned(),
            resolutions: Vec::new(),
            expected_baseline_digest: None,
            expected_plan_digest: [9; 32],
        };
        let absent_path = root.join("absent/queue.json");
        let absent_queue = OperationQueue::open(&absent_path).unwrap();
        let absent_entry = absent_queue.enqueue_claimed(absent_operation).unwrap();
        absent_queue
            .mark_running(&absent_entry.operation_id)
            .unwrap();
        absent_queue
            .update_progress(
                &absent_entry.operation_id,
                &Progress {
                    phase: "done".into(),
                    current: "letzte-datei.txt".into(),
                    ..Progress::default()
                },
            )
            .unwrap();
        drop(absent_queue);

        let recovered = OperationQueue::open(absent_path).unwrap().snapshot();
        assert_eq!(recovered.entries[0].state, QueueState::Interrupted);
        assert_eq!(recovered.entries[0].progress.phase, "interrupted");
        assert!(recovered.entries[0].progress.current.is_empty());
        assert!(!recovered.entries[0].recovery_available);
    }

    #[test]
    fn retry_bewahrt_bidirektionales_resume_oder_rollback_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline_path = root.join("state/paar.json");
        let prepared = crate::prepare_local_bidirectional(
            PairId([82; 16]),
            "Retry-Recovery",
            &left,
            &right,
            &baseline_path,
        )
        .unwrap();
        let queue_path = root.join("queue/queue.json");
        let queue = OperationQueue::open(&queue_path).unwrap();
        let entry = queue
            .enqueue_claimed(QueueOperation::Bidirectional {
                pair_id: PairId([82; 16]),
                pair_name: "Retry-Recovery".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: prepared.preview().plan.baseline_digest,
                expected_plan_digest: prepared.preview().plan.plan_digest,
            })
            .unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        // Der private Schreibweg simuliert gezielt ein altes Journal. Ein
        // neuer Produktionsaufrufer darf diesen Zustand nicht mehr über die
        // generische Fehler-API erzeugen.
        queue
            .transact(|journal| {
                let queued = find_mut(journal, &entry.operation_id)?;
                queued.state = QueueState::Failed;
                queued.progress = Progress {
                    phase: "done".into(),
                    current: "teilwirkung.txt".into(),
                    ..Progress::default()
                };
                queued.error = Some("simulierter alter Fehler".into());
                Ok(())
            })
            .unwrap();
        drop(queue);

        let queue = OperationQueue::open(queue_path).unwrap();
        let before = queue.snapshot().entries[0].clone();
        let error = queue.retry(&entry.operation_id).unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let after = queue.snapshot().entries[0].clone();
        assert_eq!(after, before);
        assert_eq!(after.state, QueueState::Interrupted);
        assert!(after.recovery_available);
        assert_eq!(after.progress.phase, "recovery");
        drop(prepared);
    }

    #[test]
    fn bidirektionaler_remote_eintrag_persistiert_nur_secret_referenz() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let local = root.join("lokal");
        std::fs::create_dir(&local).unwrap();
        let reference = KeychainSecretReference::new("Doppeldecker-Test", "bidi-sftp").unwrap();
        let operation = QueueOperation::Bidirectional {
            pair_id: PairId([93; 16]),
            pair_name: "Remote-Queuepaar".into(),
            left: local_bidirectional_endpoint(&local),
            right: Box::new(BidirectionalQueueEndpoint::Sftp {
                connection: Box::new(SftpConnectionConfig {
                    host: "sftp.example.test".into(),
                    alternate_hosts: Vec::new(),
                    port: 22,
                    username: "tester".into(),
                    requested_root: "/daten".into(),
                    host_key: SftpHostKeyPin {
                        algorithm: "ssh-ed25519".into(),
                        sha256: [7; 32],
                    },
                    authentication: SftpAuthenticationReference::Password { secret: reference },
                }),
                root: "/daten/paar".into(),
            }),
            baseline_path: root.join("state/paar.json").to_string_lossy().into_owned(),
            resolutions: Vec::new(),
            expected_baseline_digest: None,
            expected_plan_digest: [8; 32],
        };
        let path = root.join("state/queue.json");
        OperationQueue::open(&path)
            .unwrap()
            .enqueue(operation)
            .unwrap();

        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(persisted.contains("Doppeldecker-Test"));
        assert!(persisted.contains("bidi-sftp"));
        assert!(!persisted.contains("nicht-persistierbarer-wert"));
        assert!(!persisted.contains("confirmation_phrase"));
        assert!(!persisted.contains("session_generation"));
    }

    #[test]
    fn offenes_resume_oder_rollback_journal_kann_nicht_terminalisiert_werden() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline_path = root.join("state/paar.json");
        let prepared = crate::prepare_local_bidirectional(
            PairId([79; 16]),
            "Recovery-Paar",
            &left,
            &right,
            &baseline_path,
        )
        .unwrap();
        let operation = QueueOperation::Bidirectional {
            pair_id: PairId([79; 16]),
            pair_name: "Recovery-Paar".into(),
            left: local_bidirectional_endpoint(&left),
            right: local_bidirectional_endpoint(&right),
            baseline_path: baseline_path.to_string_lossy().into_owned(),
            resolutions: Vec::new(),
            expected_baseline_digest: prepared.preview().plan.baseline_digest,
            expected_plan_digest: prepared.preview().plan.plan_digest,
        };
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let first = queue.enqueue(operation.clone()).unwrap();
        queue.enqueue(operation).unwrap();
        queue.claim_next().unwrap().unwrap();
        let recovery_path = root.join("stage/rest").to_string_lossy().into_owned();
        let failure = crate::error::with_recovery_paths(
            io::Error::new(io::ErrorKind::Interrupted, "Abbruch"),
            std::slice::from_ref(&recovery_path),
        );
        queue
            .interrupt_bidirectional_coded(&first.operation_id, Progress::default(), &failure)
            .unwrap();
        drop(queue);
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        assert_eq!(
            queue.snapshot().entries[0].temp_recovery_paths,
            vec![recovery_path]
        );

        assert_eq!(queue.snapshot().entries[0].state, QueueState::Interrupted);
        assert!(queue.snapshot().entries[0].recovery_available);
        assert!(!queue.has_runnable());
        assert!(queue.claim_next().unwrap().is_none());

        let before = queue.snapshot();
        for rolled_back in [false, true] {
            let error = queue
                .finish_bidirectional_after_effect(
                    &first.operation_id,
                    rolled_back,
                    Progress::default(),
                )
                .unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert_eq!(queue.snapshot(), before);
        }

        // Sicherheitsgrenze: Der einzige öffentliche Abschlussweg verlangt einen
        // persistierten Completion-Marker. Ein normales Resume-/Rollback-Journal
        // darf ihn weder erraten noch als terminalen Zustand überspringen.
        let error = queue
            .finish_bidirectional_completion_recovery(&first.operation_id)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries[0].state, QueueState::Interrupted);
        assert_eq!(snapshot.entries[0].progress.phase, "recovery");
        assert!(snapshot.entries[0].recovery_available);
        assert!(!queue.has_runnable());
        assert!(queue.claim_next().unwrap().is_none());
        drop(prepared);
    }

    #[test]
    fn generische_abschlusswege_verbergen_keinen_bidirektionalen_recoverylauf() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline_path = root.join("state/paar.json");
        let prepared = crate::prepare_local_bidirectional(
            PairId([83; 16]),
            "Generischer Abschluss",
            &left,
            &right,
            &baseline_path,
        )
        .unwrap();
        let queue = OperationQueue::open(root.join("queue/queue.json")).unwrap();
        let entry = queue
            .enqueue_claimed(QueueOperation::Bidirectional {
                pair_id: PairId([83; 16]),
                pair_name: "Generischer Abschluss".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: prepared.preview().plan.baseline_digest,
                expected_plan_digest: prepared.preview().plan.plan_digest,
            })
            .unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        let before = queue.snapshot();

        let complete = queue
            .complete(&entry.operation_id, Progress::default())
            .unwrap_err();
        assert_eq!(
            crate::error_code(&complete),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(queue.snapshot(), before);

        let cancel = queue
            .cancel(&entry.operation_id, Progress::default())
            .unwrap_err();
        assert_eq!(crate::error_code(&cancel), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(queue.snapshot(), before);

        let fail = queue
            .fail(
                &entry.operation_id,
                Progress::default(),
                "darf Recovery nicht verbergen",
            )
            .unwrap_err();
        assert_eq!(crate::error_code(&fail), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(queue.snapshot(), before);
        drop(prepared);
    }

    #[test]
    fn direkter_bidirektionaler_eigentuemer_meldet_nur_ein_echtes_laufjournal_als_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline_path = root.join("state/paar.json");
        let pair_id = PairId([81; 16]);
        let prepared = crate::prepare_local_bidirectional(
            pair_id,
            "Direktpaar",
            &left,
            &right,
            &baseline_path,
        )
        .unwrap();
        let preview = prepared.preview().clone();
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let entry = queue
            .enqueue_claimed(QueueOperation::Bidirectional {
                pair_id,
                pair_name: "Direktpaar".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: preview.plan.baseline_digest,
                expected_plan_digest: preview.plan.plan_digest,
            })
            .unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        queue
            .interrupt_bidirectional(
                &entry.operation_id,
                Progress::default(),
                "simulierter Abbruch nach persistenter Eigentümeranlage",
            )
            .unwrap();

        let interrupted = &queue.snapshot().entries[0];
        assert_eq!(interrupted.state, QueueState::Interrupted);
        assert!(interrupted.recovery_available);
        assert!(
            serde_json::to_value(interrupted).unwrap()["recovery_available"]
                .as_bool()
                .unwrap()
        );
        drop(prepared);
    }

    /// Scheitert die Vorbereitung, bevor überhaupt ein Laufjournal entsteht, gab es
    /// keine Mutation. Der Eintrag muss deshalb ganz normal wiederholbar bleiben —
    /// sonst hält er als erster nicht-terminaler Eintrag die ganze Fensterqueue an,
    /// ohne dass die Anwendung noch einen Ausweg anbietet.
    #[test]
    fn bidirektionaler_abbruch_ohne_laufjournal_bleibt_wiederholbar() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        // Bewusst ein Pfad ohne jede Baseline: genau der Zustand nach einer
        // Vorbereitung, die schon an der Wurzelprüfung gescheitert ist.
        let baseline_path = root.join("state/nie-angelegt.json");
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        let entry = queue
            .enqueue_claimed(QueueOperation::Bidirectional {
                pair_id: PairId([91; 16]),
                pair_name: "Ohne Laufjournal".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: None,
                expected_plan_digest: [7; 32],
            })
            .unwrap();
        assert_eq!(
            bidirectional_recovery_kind(&entry.operation),
            crate::bidirectional::BidirectionalRunRecovery::Absent
        );
        queue
            .interrupt_bidirectional(
                &entry.operation_id,
                Progress::default(),
                "Wurzelprüfung scheiterte vor jeder Wirkung",
            )
            .unwrap();

        let interrupted = &queue.snapshot().entries[0];
        assert_eq!(interrupted.state, QueueState::Interrupted);
        assert!(!interrupted.recovery_available);
        assert!(interrupted.bidirectional_completion_recovery.is_none());
        assert_eq!(
            interrupted.error_code,
            Some(crate::ErrorCode::Interrupted),
            "ohne Laufjournal gibt es keine mögliche Teilwirkung"
        );

        queue.retry(&entry.operation_id).unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Pending);
        let claimed = queue.claim_next().unwrap().expect("Queue bleibt bedienbar");
        assert_eq!(claimed.operation_id, entry.operation_id);
    }

    /// Ein vor der Korrektur geschriebenes Journal trägt an genau dieser Stelle
    /// `OutcomeUnclear`. Das Öffnen muss den Eintrag reparieren, sonst bliebe er
    /// über jeden Neustart hinweg unwiederholbar.
    #[test]
    fn altes_unklares_bidirektionales_journal_ohne_laufjournal_wird_beim_oeffnen_wiederholbar() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline_path = root.join("state/nie-angelegt.json");
        let queue_path = root.join("state/queue.json");
        let operation_id = {
            let queue = OperationQueue::open(&queue_path).unwrap();
            let entry = queue
                .enqueue_claimed(QueueOperation::Bidirectional {
                    pair_id: PairId([92; 16]),
                    pair_name: "Altstand".into(),
                    left: local_bidirectional_endpoint(&left),
                    right: local_bidirectional_endpoint(&right),
                    baseline_path: baseline_path.to_string_lossy().into_owned(),
                    resolutions: Vec::new(),
                    expected_baseline_digest: None,
                    expected_plan_digest: [8; 32],
                })
                .unwrap();
            entry.operation_id
        };

        // Den alten Stand direkt im Journal nachbauen.
        let raw = std::fs::read_to_string(&queue_path).unwrap();
        let mut snapshot: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entry = &mut snapshot["entries"][0];
        entry["state"] = serde_json::json!("Interrupted");
        entry["error"] = serde_json::json!("Wurzelprüfung scheiterte vor jeder Wirkung");
        entry["error_code"] = serde_json::json!("outcome_unclear");
        entry["recovery_available"] = serde_json::json!(false);
        std::fs::write(&queue_path, serde_json::to_string(&snapshot).unwrap()).unwrap();

        let queue = OperationQueue::open(&queue_path).unwrap();
        let repariert = &queue.snapshot().entries[0];
        assert_eq!(repariert.state, QueueState::Interrupted);
        assert_eq!(repariert.error_code, Some(crate::ErrorCode::Interrupted));
        queue.retry(&operation_id).unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Pending);
    }

    #[test]
    fn bidirektionaler_terminalcommit_ohne_core_beleg_bleibt_unveraendert() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let operation = || QueueOperation::Bidirectional {
            pair_id: PairId([82; 16]),
            pair_name: "Terminalpaar".into(),
            left: local_bidirectional_endpoint(&left),
            right: local_bidirectional_endpoint(&right),
            baseline_path: root.join("state/paar.json").to_string_lossy().into_owned(),
            resolutions: Vec::new(),
            expected_baseline_digest: None,
            expected_plan_digest: [7; 32],
        };

        let queue = OperationQueue::open(root.join("queue/queue.json")).unwrap();
        let entry = queue.enqueue_claimed(operation()).unwrap();
        queue.mark_running(&entry.operation_id).unwrap();
        let before = queue.snapshot();
        for rolled_back in [false, true] {
            let error = queue
                .finish_bidirectional_after_effect(
                    &entry.operation_id,
                    rolled_back,
                    Progress::default(),
                )
                .unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert_eq!(queue.snapshot(), before);
        }
    }

    #[test]
    fn persistierter_bidirektionaler_eintrag_mit_unsicherer_baseline_wird_abgelehnt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let snapshot = QueueSnapshot {
            version: QUEUE_JOURNAL_VERSION,
            revision: 0,
            paused: false,
            stop_on_error: false,
            entries: vec![QueueEntry {
                operation_id: "ungueltig".into(),
                operation: QueueOperation::Bidirectional {
                    pair_id: PairId([80; 16]),
                    pair_name: "Unsicher".into(),
                    left: local_bidirectional_endpoint(Path::new("/tmp/links")),
                    right: local_bidirectional_endpoint(Path::new("/tmp/rechts")),
                    baseline_path: "relative-baseline.json".into(),
                    resolutions: Vec::new(),
                    expected_baseline_digest: None,
                    expected_plan_digest: [1; 32],
                },
                profile_bindings: Vec::new(),
                created_unix_ms: 0,
                state: QueueState::Pending,
                attempts: 0,
                progress: Progress::default(),
                attention: None,
                error: None,
                error_code: None,
                temp_recovery_paths: Vec::new(),
                retained_backup_paths: Vec::new(),
                recovery_available: false,
                bidirectional_completion_recovery: None,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        assert_eq!(
            OperationQueue::open(path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn persistiertes_journal_mit_doppelter_operations_id_wird_abgelehnt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        queue.enqueue(file_operation("erste")).unwrap();
        queue.enqueue(file_operation("zweite")).unwrap();
        drop(queue);

        let mut snapshot: QueueSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot.entries[1].operation_id = snapshot.entries[0].operation_id.clone();
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let error = OperationQueue::open(path)
            .err()
            .expect("doppelte IDs dürfen keinen mehrdeutigen Queuezustand öffnen");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn persistiertes_journal_mit_unaufloesbarer_bestaetigung_wird_abgelehnt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        queue.enqueue(file_operation("bestaetigung")).unwrap();
        drop(queue);

        let mut snapshot: QueueSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot.entries[0].state = QueueState::AwaitingConfirmation;
        snapshot.entries[0].attention = None;
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let error = OperationQueue::open(path)
            .err()
            .expect("Bestätigungszustand ohne Daten würde die gesamte Queue blockieren");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn bidirektionale_queue_drift_aktualisiert_nur_bestaetigte_digests() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline_path = root.join("state/paar.json");
        let pair_id = PairId([78; 16]);
        let draft =
            crate::prepare_local_bidirectional(pair_id, "Driftpaar", &left, &right, &baseline_path)
                .unwrap();
        let plan_digest = draft.preview().plan.plan_digest;
        draft.discard().unwrap();
        drop(draft);
        let queue = OperationQueue::open(root.join("state/queue.json")).unwrap();
        queue
            .enqueue(QueueOperation::Bidirectional {
                pair_id,
                pair_name: "Driftpaar".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: None,
                expected_plan_digest: plan_digest,
            })
            .unwrap();
        std::fs::write(left.join("neu"), b"drift").unwrap();
        let claimed = queue.claim_next().unwrap().unwrap();
        let BidirectionalQueuePreparation::AwaitingConfirmation { .. } =
            prepare_bidirectional_queue_entry(&queue, claimed, no_secrets()).unwrap()
        else {
            panic!("gedrifteter Queue-Plan wurde unerwartet vorbereitet");
        };
        let snapshot = queue.snapshot();
        let QueueAttention::BidirectionalPlanChanged {
            confirmation,
            current_plan_digest,
            ..
        } = snapshot.entries[0].attention.as_ref().unwrap()
        else {
            panic!("bidirektionale Drift besitzt falschen Aufmerksamkeitstyp");
        };
        let confirmation = confirmation.clone();
        let current_plan_digest = *current_plan_digest;
        queue
            .approve_attention(&snapshot.entries[0].operation_id, &confirmation)
            .unwrap();
        let QueueOperation::Bidirectional {
            expected_plan_digest,
            ..
        } = &queue.snapshot().entries[0].operation
        else {
            panic!("Queue-Eintrag verlor seinen Typ");
        };
        assert_eq!(*expected_plan_digest, current_plan_digest);
        assert!(crate::load_bidirectional_run_journal(&baseline_path)
            .unwrap()
            .is_none());
    }

    #[test]
    fn beschaedigtes_oder_falsches_journal_wird_fail_closed_abgelehnt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        std::fs::write(&path, b"{kaputt").unwrap();
        assert_eq!(
            OperationQueue::open(&path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(
            &path,
            br#"{"version":999,"paused":false,"stop_on_error":false,"entries":[]}"#,
        )
        .unwrap();
        assert_eq!(
            OperationQueue::open(&path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn baumelnder_queue_symlink_wird_nicht_als_fehlendes_journal_ersetzt() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let path = root.join("queue.json");
        symlink(root.join("nicht-vorhanden.json"), &path).unwrap();

        let _error = OperationQueue::open(&path)
            .err()
            .expect("ein unsicherer Journalname darf nicht durch eine leere Queue ersetzt werden");

        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn provider_ersetzen_parkt_genau_bestaetigte_kollision_im_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let target = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("datei.txt"), b"neu").unwrap();
        std::fs::write(target.join("datei.txt"), b"alt").unwrap();
        let vfs = QueueConflictRaceVfs::new(false);
        let conflict = target.join("datei.txt").to_string_lossy().into_owned();
        let proofs = capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();

        let mut recovery = None;
        let run = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            &[conflict],
            &proofs,
            &Cancel::new(),
            &mut recovery,
        )
        .unwrap();
        assert_eq!(recovery.as_ref().unwrap().parked_entries, 1);
        assert!(!target.join("datei.txt").exists());
        assert_eq!(std::fs::read(run.join("datei.txt")).unwrap(), b"alt");

        let selected = vec![source.join("datei.txt")];
        copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &selected,
                dst_dir: &target,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        complete_quarantine_run_if_matches(&vfs, &run, run.manifest_digest).unwrap();
        assert_eq!(std::fs::read(target.join("datei.txt")).unwrap(), b"neu");
        let runs =
            crate::list_quarantine_runs(&vfs, &target.join(".doppeldecker-quarantine"), &None)
                .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].items_total, 1);
        assert_eq!(runs[0].state, crate::QuarantineState::Complete);
    }

    #[test]
    fn review_quarantaene_eltern_binden_case_alias_nur_nach_anlageprobe() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let run = root.join(".doppeldecker-quarantine/lauf");
        std::fs::create_dir_all(&run).unwrap();
        let vfs = QueueConflictRaceVfs::new(false);
        let run_proof = vfs.listed_proof(&run).unwrap();
        // Auf case-sensitiven Zielen entstehen zwei Ordner. APFS mit
        // zusammenfassender Schreibweise bindet nach EEXIST denselben Ordner.
        for name in ["docs", "Docs", "Docs", "docs"] {
            let (parent, proof) =
                ensure_bound_park_parent(&vfs, &run, &run_proof, Path::new(name), None).unwrap();
            let actual = vfs.stat(&parent).unwrap();
            assert!(actual.is_dir);
            assert_eq!(proof.entry.device, actual.device);
            assert_eq!(proof.entry.inode, actual.inode);
        }
        assert!(run.join("docs").is_dir());
        assert!(run.join("Docs").is_dir());
        let parent = root.join("basis");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(parent.join(".Doppeldecker-Quarantine")).unwrap();
        let base = parent.join(".doppeldecker-quarantine");
        let mut recovery = None;
        let proof = ensure_provider_quarantine_base(&vfs, &parent, &base, &mut recovery).unwrap();
        let actual = vfs.stat(&base).unwrap();
        assert_eq!(proof.entry.inode, actual.inode);
    }

    #[test]
    fn provider_quarantaene_verlangt_beide_gebundenen_eltern_vor_erster_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let target = std::fs::canonicalize(temp.path()).unwrap().join("ziel");
        std::fs::create_dir(&target).unwrap();
        let conflict_path = target.join("datei.txt");
        std::fs::write(&conflict_path, b"bestaetigtes-altziel").unwrap();
        let vfs = QueueConflictRaceVfs::new(false);
        vfs.disable_two_parent_contract();
        let conflict = conflict_path.to_str().unwrap().to_owned();
        let proofs = capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();
        let mut recovery = None;

        let error = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            std::slice::from_ref(&conflict),
            &proofs,
            &Cancel::new(),
            &mut recovery,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            std::fs::read(&conflict_path).unwrap(),
            b"bestaetigtes-altziel"
        );
        assert!(!target.join(".doppeldecker-quarantine").exists());
        assert!(recovery.is_none());
        assert_eq!(vfs.conditional_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_quarantaene_verschiebt_keinen_ausgetauschten_konflikt() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel");
        std::fs::create_dir(&target).unwrap();
        let conflict_path = target.join("datei.txt");
        std::fs::write(&conflict_path, b"bestaetigtes-altziel").unwrap();
        let vfs = QueueConflictRaceVfs::new(true);
        vfs.arm_attack(&conflict_path);
        let conflict = conflict_path.to_string_lossy().into_owned();
        let proofs = capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();

        let mut recovery = None;
        let error = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            std::slice::from_ref(&conflict),
            &proofs,
            &Cancel::new(),
            &mut recovery,
        )
        .unwrap_err();

        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::OutcomeUnclear,
            "unerwarteter Quarantänefehler: {error}"
        );
        assert_eq!(std::fs::read(&conflict_path).unwrap(), b"fremder-konflikt");
        let displaced = vfs.displaced.lock().unwrap().clone().unwrap();
        assert_eq!(std::fs::read(displaced).unwrap(), b"bestaetigtes-altziel");
        assert_eq!(vfs.conditional_calls.load(Ordering::SeqCst), 1);
        let message = error.to_string();
        assert!(message.contains(&conflict_path.display().to_string()));
        assert!(message.contains(".doppeldecker-quarantine"));
        assert_eq!(recovery.as_ref().unwrap().parked_entries, 0);
    }

    #[test]
    fn provider_quarantaene_bindet_manifest_intent_in_dieselbe_rename_wirkung() {
        let temp = tempfile::tempdir().unwrap();
        let target = std::fs::canonicalize(temp.path()).unwrap().join("ziel");
        std::fs::create_dir(&target).unwrap();
        let conflict_path = target.join("datei.txt");
        std::fs::write(&conflict_path, b"bestaetigtes-altziel").unwrap();
        let vfs = QueueConflictRaceVfs::new(false);
        vfs.attack_manifest_before_next_rename();
        let conflict = conflict_path.to_str().unwrap().to_owned();
        let proofs = capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();
        let mut recovery = None;

        let error = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            std::slice::from_ref(&conflict),
            &proofs,
            &Cancel::new(),
            &mut recovery,
        )
        .unwrap_err();

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            std::fs::read(&conflict_path).unwrap(),
            b"bestaetigtes-altziel"
        );
        let recovery = recovery.expect("Manifestangriff braucht den sichtbaren Lauf");
        assert_eq!(recovery.parked_entries, 0);
        let run = Path::new(&recovery.quarantine_run);
        let sidecar = run.parent().unwrap().join(format!(
            "{}.json",
            run.file_name().unwrap().to_string_lossy()
        ));
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(sidecar).unwrap()).unwrap();
        assert_eq!(manifest["records"], serde_json::json!([]));
        assert_eq!(vfs.conditional_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_quarantaene_bindet_quell_basis_lauf_und_park_parent_bis_zum_rename() {
        for attack in [
            QuarantineParentAttack::Base,
            QuarantineParentAttack::Run,
            QuarantineParentAttack::BoundParent,
            QuarantineParentAttack::SourceParent,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let target = std::fs::canonicalize(temp.path()).unwrap().join("ziel");
            let conflict_path = target.join("ordner/datei.txt");
            std::fs::create_dir_all(conflict_path.parent().unwrap()).unwrap();
            std::fs::write(&conflict_path, b"bestaetigtes-altziel").unwrap();
            let vfs = QueueConflictRaceVfs::new(false);
            let conflict = conflict_path.to_str().unwrap().to_owned();
            let proofs =
                capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();
            vfs.arm_parent_attack(attack);
            let mut recovery = None;

            let error = quarantine_provider_file_conflicts(
                &vfs,
                &target,
                std::slice::from_ref(&conflict),
                &proofs,
                &Cancel::new(),
                &mut recovery,
            )
            .unwrap_err();

            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            let displaced = vfs.parent_displaced_path.lock().unwrap().clone().unwrap();
            assert!(displaced.is_dir());
            let replacement_parent = vfs.parent_replacement_path.lock().unwrap().clone().unwrap();
            assert_eq!(
                std::fs::read(replacement_parent.join("fremd.txt")).unwrap(),
                b"fremder-elternordner"
            );
            if matches!(attack, QuarantineParentAttack::SourceParent) {
                assert_eq!(
                    std::fs::read(displaced.join("datei.txt")).unwrap(),
                    b"bestaetigtes-altziel"
                );
                assert!(!conflict_path.exists());
            } else {
                assert_eq!(
                    std::fs::read(&conflict_path).unwrap(),
                    b"bestaetigtes-altziel"
                );
            }
            let recovery = recovery.expect("Parkfehler braucht den exakten Laufpfad");
            assert_eq!(recovery.parked_entries, 0);
            assert!(error
                .to_string()
                .contains(&conflict_path.display().to_string()));
            assert!(error.to_string().contains(&recovery.quarantine_run));
            assert_eq!(vfs.conditional_calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn provider_quarantaene_validiert_gesamte_konfliktmenge_vor_erster_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel");
        std::fs::create_dir(&target).unwrap();
        let valid = target.join("gueltig.txt");
        let outside = root.join("ausserhalb.txt");
        std::fs::write(&valid, b"alt-1").unwrap();
        std::fs::write(&outside, b"alt-2").unwrap();
        let vfs = QueueConflictRaceVfs::new(false);
        let conflicts = vec![
            valid.to_str().unwrap().to_owned(),
            outside.to_str().unwrap().to_owned(),
        ];
        let proofs = capture_file_conflict_proofs(&vfs, &conflicts).unwrap();
        let mut recovery = None;

        let error = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            &conflicts,
            &proofs,
            &Cancel::new(),
            &mut recovery,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&valid).unwrap(), b"alt-1");
        assert_eq!(std::fs::read(&outside).unwrap(), b"alt-2");
        assert!(!target.join(".doppeldecker-quarantine").exists());
        assert!(recovery.is_none());
        assert_eq!(vfs.conditional_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_cancel_vor_und_nach_quarantaenebeginn_bleibt_ehrlich() {
        let temp = tempfile::tempdir().unwrap();
        let target = std::fs::canonicalize(temp.path()).unwrap().join("ziel");
        std::fs::create_dir(&target).unwrap();
        let first = target.join("eins.txt");
        let second = target.join("zwei.txt");
        std::fs::write(&first, b"eins-alt").unwrap();
        std::fs::write(&second, b"zwei-alt").unwrap();
        let vfs = QueueConflictRaceVfs::new(false);
        let conflicts = vec![
            first.to_str().unwrap().to_owned(),
            second.to_str().unwrap().to_owned(),
        ];
        let proofs = capture_file_conflict_proofs(&vfs, &conflicts).unwrap();

        let cancelled_before = Cancel::new();
        cancelled_before.cancel();
        let mut no_recovery = None;
        let error = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            &conflicts,
            &proofs,
            &cancelled_before,
            &mut no_recovery,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(no_recovery.is_none());
        assert!(!target.join(".doppeldecker-quarantine").exists());

        let cancelled_during = Cancel::new();
        vfs.cancel_after_next_rename(&cancelled_during);
        let mut recovery = None;
        let run = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            &conflicts,
            &proofs,
            &cancelled_during,
            &mut recovery,
        )
        .unwrap();
        assert!(cancelled_during.is_cancelled());
        assert!(!first.exists() && !second.exists());
        assert_eq!(std::fs::read(run.join("eins.txt")).unwrap(), b"eins-alt");
        assert_eq!(std::fs::read(run.join("zwei.txt")).unwrap(), b"zwei-alt");
        assert_eq!(recovery.as_ref().unwrap().parked_entries, 2);

        let error = provider_cancel_after_parking_error(recovery.as_ref());
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(run.to_str().unwrap()));
    }

    #[test]
    fn cancel_nach_konflikthash_stoppt_vor_quarantaenenmutation() {
        let temp = tempfile::tempdir().unwrap();
        let target = std::fs::canonicalize(temp.path()).unwrap().join("ziel");
        std::fs::create_dir(&target).unwrap();
        let conflict_path = target.join("datei.txt");
        std::fs::write(&conflict_path, b"bestaetigtes-altziel").unwrap();
        let vfs = QueueConflictRaceVfs::new(false);
        let conflict = conflict_path.to_str().unwrap().to_owned();
        let proofs = capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();
        let cancel = Cancel::new();
        vfs.cancel_while_hashing_proof(&cancel);
        let mut recovery = None;

        let error = quarantine_provider_file_conflicts(
            &vfs,
            &target,
            std::slice::from_ref(&conflict),
            &proofs,
            &cancel,
            &mut recovery,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            std::fs::read(&conflict_path).unwrap(),
            b"bestaetigtes-altziel"
        );
        assert!(!target.join(".doppeldecker-quarantine").exists());
        assert!(recovery.is_none());
    }

    #[test]
    fn provider_quarantaenelauf_unterscheidet_unveraenderte_und_unklare_anlagefehler() {
        for (failure, outcome_unclear) in [
            (QuarantineRunCreateFailure::Unchanged, false),
            (QuarantineRunCreateFailure::OutcomeUnclear, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let target = std::fs::canonicalize(temp.path()).unwrap().join("ziel");
            std::fs::create_dir(&target).unwrap();
            std::fs::create_dir(target.join(".doppeldecker-quarantine")).unwrap();
            let conflict_path = target.join("datei.txt");
            std::fs::write(&conflict_path, b"bestaetigtes-altziel").unwrap();
            let vfs = QueueConflictRaceVfs::new(false);
            let conflict = conflict_path.to_str().unwrap().to_owned();
            let proofs =
                capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();
            vfs.fail_quarantine_run_create(failure);
            let mut recovery = None;

            let error = quarantine_provider_file_conflicts(
                &vfs,
                &target,
                std::slice::from_ref(&conflict),
                &proofs,
                &Cancel::new(),
                &mut recovery,
            )
            .unwrap_err();

            assert_eq!(
                std::fs::read(&conflict_path).unwrap(),
                b"bestaetigtes-altziel"
            );
            if outcome_unclear {
                assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                let recovery = recovery.expect("mögliche Anlage braucht den Recoverydatensatz");
                assert_eq!(recovery.parked_entries, 0);
                assert!(Path::new(&recovery.quarantine_run).is_dir());
                assert!(error.to_string().contains(&recovery.quarantine_run));
            } else {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                assert_ne!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                assert!(
                    recovery.is_none(),
                    "mutationsfreie Absage braucht kein Recovery"
                );
            }
        }
    }

    #[test]
    fn provider_quarantaenebasis_bindet_recovery_vor_create_und_parent_sync() {
        #[derive(Clone, Copy)]
        enum Case {
            Unchanged,
            OutcomeUnclear,
            SyncUnclear,
            RunUnchangedAfterFreshBase,
        }

        for case in [
            Case::Unchanged,
            Case::OutcomeUnclear,
            Case::SyncUnclear,
            Case::RunUnchangedAfterFreshBase,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let target = std::fs::canonicalize(temp.path()).unwrap().join("ziel");
            std::fs::create_dir(&target).unwrap();
            let base = target.join(".doppeldecker-quarantine");
            let conflict_path = target.join("datei.txt");
            std::fs::write(&conflict_path, b"bestaetigtes-altziel").unwrap();
            let vfs = QueueConflictRaceVfs::new(false);
            match case {
                Case::Unchanged => {
                    vfs.fail_quarantine_base_create(QuarantineRunCreateFailure::Unchanged)
                }
                Case::OutcomeUnclear => {
                    vfs.fail_quarantine_base_create(QuarantineRunCreateFailure::OutcomeUnclear)
                }
                Case::SyncUnclear => vfs.fail_sync_at(&target),
                Case::RunUnchangedAfterFreshBase => {
                    vfs.fail_quarantine_run_create(QuarantineRunCreateFailure::Unchanged)
                }
            }
            let conflict = conflict_path.to_str().unwrap().to_owned();
            let proofs =
                capture_file_conflict_proofs(&vfs, std::slice::from_ref(&conflict)).unwrap();
            let mut recovery = None;

            let error = quarantine_provider_file_conflicts(
                &vfs,
                &target,
                std::slice::from_ref(&conflict),
                &proofs,
                &Cancel::new(),
                &mut recovery,
            )
            .unwrap_err();

            assert_eq!(
                std::fs::read(&conflict_path).unwrap(),
                b"bestaetigtes-altziel"
            );
            match case {
                Case::Unchanged => {
                    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                    assert!(recovery.is_none());
                    assert!(!base.exists());
                }
                Case::OutcomeUnclear | Case::SyncUnclear => {
                    assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
                    let recovery =
                        recovery.expect("sichtbare Basis braucht strukturiertes Recovery");
                    assert_eq!(recovery.parked_entries, 0);
                    assert_eq!(recovery.quarantine_run, base.to_str().unwrap());
                    assert!(base.is_dir());
                    assert!(error.to_string().contains(&base.display().to_string()));
                    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
                }
                Case::RunUnchangedAfterFreshBase => {
                    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                    let recovery = recovery.expect(
                        "die zuvor sichtbar angelegte Basis bleibt eigener Recoveryzustand",
                    );
                    assert_eq!(recovery.parked_entries, 0);
                    assert_eq!(recovery.quarantine_run, base.to_str().unwrap());
                    assert!(base.is_dir());
                    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn queue_start_entfernt_keine_ungebundenen_passenden_tempnamen() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let foreign_file = temp.path().join(".operations-queue-fremd.tmp");
        let foreign_target = temp.path().join("fremdes-ziel");
        let foreign_symlink = temp.path().join(".operations-queue-fremd-link.tmp");
        std::fs::write(&foreign_file, b"fremde-datei").unwrap();
        std::fs::write(&foreign_target, b"fremdes-ziel").unwrap();
        symlink(&foreign_target, &foreign_symlink).unwrap();

        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();

        assert_eq!(std::fs::read(&foreign_file).unwrap(), b"fremde-datei");
        assert!(std::fs::symlink_metadata(&foreign_symlink)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&foreign_target).unwrap(), b"fremdes-ziel");
        drop(queue);
    }

    /// Eine abgelehnte Rückfrage darf die Fensterqueue nicht dauerhaft anhalten.
    ///
    /// Vor `discard_attention` führte aus `AwaitingConfirmation` einzig die
    /// Zustimmung heraus. Wer die neu entdeckte Lage nicht übernehmen wollte,
    /// hatte damit gar keine Wahl: Der Eintrag blieb vorne stehen, blockierte
    /// jeden nachfolgenden Eintrag und überstand auch einen Neustart.
    #[test]
    fn abgelehnte_bestaetigung_gibt_die_queue_frei_ohne_zuzustimmen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let queue = OperationQueue::open(&path).unwrap();
        let first = queue.enqueue(file_operation("eins")).unwrap();
        let second = queue.enqueue(file_operation("zwei")).unwrap();
        queue.claim_next().unwrap().unwrap();
        queue
            .require_confirmation(
                &first.operation_id,
                QueueAttention::Manual {
                    reason: "Der Plan hat sich geändert".into(),
                    confirmation: "phrase-eins".into(),
                },
            )
            .unwrap();

        // Kein bestehender Weg nimmt den Eintrag aus der Reihenfolge, und der
        // zweite Eintrag kommt dadurch nie an die Reihe.
        for blocked in [
            queue.remove_pending(&first.operation_id),
            queue.move_pending(&first.operation_id, 1),
            queue.retry(&first.operation_id),
            queue.cancel(&first.operation_id, Progress::default()),
            queue.fail(&first.operation_id, Progress::default(), "abgelehnt"),
        ] {
            assert_eq!(blocked.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        assert!(queue.claim_next().unwrap().is_none());
        assert!(!queue.has_runnable());

        queue.discard_attention(&first.operation_id).unwrap();

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries[0].state, QueueState::Cancelled);
        assert!(snapshot.entries[0].attention.is_none());
        assert_eq!(
            snapshot.entries[0].error.as_deref(),
            Some(DISCARDED_ATTENTION_ERROR)
        );
        // Der verworfene Eintrag blockiert nicht mehr: Der nächste läuft an.
        assert!(queue.has_runnable());
        assert_eq!(
            queue.claim_next().unwrap().unwrap().operation_id,
            second.operation_id
        );

        // Das Verwerfen ist terminal, aber keine Einbahnstraße: Wer es sich
        // anders überlegt, nimmt den Eintrag über den gewöhnlichen Retry-Weg
        // wieder auf — mit frischer Prüfung und frischer Rückfrage.
        queue.retry(&first.operation_id).unwrap();
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Pending);
    }

    /// Zwei Lagen halten Zustand außerhalb des Journals und dürfen deshalb nicht
    /// still verworfen werden.
    #[test]
    fn verwerfen_weist_remote_vorbereitung_und_offenes_laufjournal_ab() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();

        // Eine wartende Remote-Vorbereitung lebt als Permit und offene Verbindung
        // im Backend-RAM. Sie wird zuerst dort abgebaut; danach ist der Eintrag
        // wieder `Pending` und ganz normal entfernbar.
        let remote_queue = OperationQueue::open(root.join("remote/queue.json")).unwrap();
        let remote = remote_queue.enqueue(file_operation("remote")).unwrap();
        remote_queue.claim_next().unwrap().unwrap();
        remote_queue
            .require_confirmation(
                &remote.operation_id,
                QueueAttention::RemoteRun {
                    reason: "Realmodus bestätigen".into(),
                    confirmation: "laufphrase".into(),
                },
            )
            .unwrap();
        let refused = remote_queue
            .discard_attention(&remote.operation_id)
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            remote_queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        remote_queue
            .discard_remote_preparation(&remote.operation_id)
            .unwrap();
        remote_queue.remove_pending(&remote.operation_id).unwrap();
        assert!(remote_queue.snapshot().entries.is_empty());

        // Ein bidirektionaler Eintrag mit vorhandenem Laufjournal kann bereits
        // echte Teilprojektionen gebunden haben. Ein stilles `Cancelled` würde
        // den einzigen Hinweis darauf löschen.
        let left = root.join("links");
        let right = root.join("rechts");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        let baseline = root.join("state/paar.json");
        let prepared = crate::prepare_local_bidirectional(
            PairId([42; 16]),
            "Verwerfen mit Laufjournal",
            &left,
            &right,
            &baseline,
        )
        .unwrap();
        let bidirectional_queue = OperationQueue::open(root.join("bidi/queue.json")).unwrap();
        let entry = bidirectional_queue
            .enqueue_claimed(QueueOperation::Bidirectional {
                pair_id: PairId([42; 16]),
                pair_name: "Verwerfen mit Laufjournal".into(),
                left: local_bidirectional_endpoint(&left),
                right: local_bidirectional_endpoint(&right),
                baseline_path: baseline.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: prepared.preview().plan.baseline_digest,
                expected_plan_digest: prepared.preview().plan.plan_digest,
            })
            .unwrap();
        bidirectional_queue
            .require_confirmation(
                &entry.operation_id,
                QueueAttention::Manual {
                    reason: "Zweitscan weicht ab".into(),
                    confirmation: "phrase-bidi".into(),
                },
            )
            .unwrap();
        let refused = bidirectional_queue
            .discard_attention(&entry.operation_id)
            .unwrap_err();
        assert_eq!(
            crate::error::error_code(&refused),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(
            bidirectional_queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        drop(prepared);
    }

    // ------------------------------------------------------------------
    // Modellbasierter Test der Queue-Zustandsmaschine
    //
    // Die Zustandsmaschine liegt verteilt in vielen Methoden; jede prüft ihre
    // erlaubten Ausgangszustände selbst. Der folgende Test schreibt dieselben
    // Regeln noch einmal unabhängig als kleines Modell auf und lässt proptest
    // zufällige Bedienfolgen gegen Modell und echte Queue laufen. Verglichen
    // wird nach jedem einzelnen Schritt: ob die Handlung überhaupt erlaubt war
    // (Ok gegen Err), Reihenfolge und Zustand aller Einträge, Pause- und
    // Fehlerschalter, die Vorschau `has_runnable` und der tatsächlich auf Platte
    // liegende Journalstand.
    // ------------------------------------------------------------------

    /// Eine einzelne Bedienhandlung an der Queue. Der Index wird im Test modulo
    /// Listenlänge gerechnet, damit der Generator keine ungültigen Kombinationen
    /// erzeugen muss und ein Gegenbeispiel gut schrumpft.
    #[derive(Clone, Copy, Debug)]
    enum QueueStep {
        Enqueue,
        EnqueueClaimed,
        ClaimNext,
        MarkRunning(usize),
        Complete(usize),
        Cancel(usize),
        Fail(usize),
        FailCoded {
            index: usize,
            outcome_unclear: bool,
        },
        AcknowledgeUnclear {
            index: usize,
            reviewed: bool,
        },
        RequireConfirmation {
            index: usize,
            kind: ModelAttention,
        },
        Approve {
            index: usize,
            correct_phrase: bool,
        },
        DiscardAttention(usize),
        Retry(usize),
        RemovePending(usize),
        MovePending {
            index: usize,
            forward: bool,
        },
        SetPaused(bool),
        SetStopOnError(bool),
        /// Prozessende und Neustart: Journal schließen und erneut öffnen.
        Reopen,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ModelAttention {
        None,
        Manual,
        ConflictsChanged,
        RemoteRun,
    }

    /// Der Modellzustand eines Eintrags. Mehr braucht die Zustandsmaschine
    /// nicht: Operation, Fortschritt und Fehlertext ändern keinen Übergang.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ModelEntry {
        id: String,
        state: QueueState,
        attention: ModelAttention,
        error_code: Option<crate::ErrorCode>,
        attempts: u32,
    }

    #[derive(Clone, Debug, Default)]
    struct QueueModel {
        entries: Vec<ModelEntry>,
        paused: bool,
        stop_on_error: bool,
    }

    impl QueueModel {
        fn is_terminal(state: QueueState) -> bool {
            matches!(
                state,
                QueueState::Completed | QueueState::Failed | QueueState::Cancelled
            )
        }

        /// Der Scheduler nimmt immer nur den vordersten offenen Eintrag. Ein
        /// bestätigungspflichtiger oder unterbrochener Eintrag davor hält die
        /// Reihenfolge bewusst an, statt übersprungen zu werden.
        fn claimable(&self) -> Option<usize> {
            // Die fensterlokale Ausführungslease gilt für die ganze Queue. Auch
            // ein durch späteren Retry davor eingereihter Pending-Eintrag darf
            // deshalb keinen zweiten Lauf neben einem aktiven Eintrag starten.
            let execution_lease_held = self
                .entries
                .iter()
                .any(|entry| matches!(entry.state, QueueState::Validating | QueueState::Running));
            if self.paused || execution_lease_held {
                return None;
            }
            let index = self
                .entries
                .iter()
                .position(|entry| !Self::is_terminal(entry.state))?;
            (self.entries[index].state == QueueState::Pending).then_some(index)
        }

        /// Der Claim selbst: Er setzt den Eintrag auf `Validating` und zählt den
        /// Versuch hoch, bevor irgendetwas geprüft oder mutiert wird.
        fn claim_next(&mut self) -> Option<String> {
            let index = self.claimable()?;
            let entry = &mut self.entries[index];
            entry.state = QueueState::Validating;
            entry.attempts = entry.attempts.saturating_add(1);
            Some(entry.id.clone())
        }

        /// Ein unmittelbares CLI-Enqueue darf nur starten, wenn gar nichts
        /// Offenes davor liegt.
        fn accepts_immediate_claim(&self) -> bool {
            !self.paused
                && self
                    .entries
                    .iter()
                    .all(|entry| Self::is_terminal(entry.state))
        }

        fn index_of(&self, index: usize) -> Option<usize> {
            (!self.entries.is_empty()).then(|| index % self.entries.len())
        }
    }

    /// Die im Test verwendete Bestätigungsphrase. Sie hängt nur an der
    /// Operations-ID und übersteht damit auch einen simulierten Neustart.
    fn model_confirmation(id: &str) -> String {
        format!("bestaetige-{id}")
    }

    fn queue_step_strategy() -> impl proptest::strategy::Strategy<Value = QueueStep> {
        use proptest::strategy::{Just, Strategy};
        proptest::prop_oneof![
            3 => Just(QueueStep::Enqueue),
            1 => Just(QueueStep::EnqueueClaimed),
            4 => Just(QueueStep::ClaimNext),
            3 => (0_usize..6).prop_map(QueueStep::MarkRunning),
            2 => (0_usize..6).prop_map(QueueStep::Complete),
            1 => (0_usize..6).prop_map(QueueStep::Cancel),
            2 => (0_usize..6).prop_map(QueueStep::Fail),
            2 => (0_usize..6, proptest::bool::ANY)
                .prop_map(|(index, outcome_unclear)| QueueStep::FailCoded {
                    index,
                    outcome_unclear,
                }),
            1 => (0_usize..6, proptest::bool::ANY)
                .prop_map(|(index, reviewed)| QueueStep::AcknowledgeUnclear {
                    index,
                    reviewed,
                }),
            3 => (0_usize..6, proptest::sample::select(vec![
                    ModelAttention::Manual,
                    ModelAttention::ConflictsChanged,
                    ModelAttention::RemoteRun,
                ]))
                .prop_map(|(index, kind)| QueueStep::RequireConfirmation { index, kind }),
            3 => (0_usize..6, proptest::bool::ANY)
                .prop_map(|(index, correct_phrase)| QueueStep::Approve { index, correct_phrase }),
            2 => (0_usize..6).prop_map(QueueStep::DiscardAttention),
            2 => (0_usize..6).prop_map(QueueStep::Retry),
            2 => (0_usize..6).prop_map(QueueStep::RemovePending),
            2 => (0_usize..6, proptest::bool::ANY)
                .prop_map(|(index, forward)| QueueStep::MovePending { index, forward }),
            2 => proptest::bool::ANY.prop_map(QueueStep::SetPaused),
            1 => proptest::bool::ANY.prop_map(QueueStep::SetStopOnError),
            1 => Just(QueueStep::Reopen),
        ]
    }

    /// Vergleicht nach jedem Schritt den gesamten beobachtbaren Queue-Zustand mit
    /// dem Modell und prüft zusätzlich die Invarianten, die keine einzelne
    /// Methode allein zusichern kann.
    fn assert_queue_matches_model(
        queue: &OperationQueue,
        model: &QueueModel,
        path: &Path,
        step: &QueueStep,
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        let snapshot = queue.snapshot();
        proptest::prop_assert_eq!(
            snapshot.entries.len(),
            model.entries.len(),
            "Eintragszahl nach {:?}",
            step
        );
        for (real, expected) in snapshot.entries.iter().zip(&model.entries) {
            proptest::prop_assert_eq!(
                &real.operation_id,
                &expected.id,
                "Reihenfolge nach {:?}",
                step
            );
            proptest::prop_assert_eq!(real.state, expected.state, "Zustand nach {:?}", step);
            let real_attention = match real.attention.as_ref() {
                None => ModelAttention::None,
                Some(QueueAttention::Manual { .. }) => ModelAttention::Manual,
                Some(QueueAttention::ConflictsChanged { .. }) => ModelAttention::ConflictsChanged,
                Some(QueueAttention::RemoteRun { .. }) => ModelAttention::RemoteRun,
                Some(other) => {
                    return Err(proptest::test_runner::TestCaseError::fail(format!(
                        "unerwartete Bestätigungsart {other:?} nach {step:?}"
                    )))
                }
            };
            proptest::prop_assert_eq!(
                real_attention,
                expected.attention,
                "Bestätigungsart nach {:?}",
                step
            );
            proptest::prop_assert_eq!(
                real.error_code,
                expected.error_code,
                "Fehlercode nach {:?}",
                step
            );
            proptest::prop_assert_eq!(real.attempts, expected.attempts, "Versuche nach {:?}", step);
            // Der persistierte Datensatz muss in sich stimmig bleiben: Genau ein
            // `AwaitingConfirmation` trägt Bestätigungsdaten, jeder andere Zustand
            // keine. Das Öffnen weist alles andere als beschädigt ab.
            proptest::prop_assert_eq!(
                real.attention.is_some(),
                real.state == QueueState::AwaitingConfirmation,
                "Bestätigungsdaten passen nicht zum Zustand nach {:?}",
                step
            );
        }
        proptest::prop_assert_eq!(snapshot.paused, model.paused, "Pause nach {:?}", step);
        proptest::prop_assert_eq!(
            snapshot.stop_on_error,
            model.stop_on_error,
            "Stop-on-error nach {:?}",
            step
        );
        // Single-Flight: Nie darf mehr als ein Eintrag gleichzeitig geprüft oder
        // ausgeführt werden.
        let active = snapshot
            .entries
            .iter()
            .filter(|entry| matches!(entry.state, QueueState::Validating | QueueState::Running))
            .count();
        proptest::prop_assert!(active <= 1, "{active} aktive Einträge nach {step:?}");
        // Die Vorschau, die der Worker zum Anlaufen benutzt, darf nie etwas
        // anderes behaupten als der nachfolgende Claim tatsächlich tut.
        proptest::prop_assert_eq!(
            queue.has_runnable(),
            model.claimable().is_some(),
            "has_runnable nach {:?}",
            step
        );
        // In-Memory-Sicht und Journaldatei müssen nach jedem Schritt Byte für
        // Byte dieselbe Lage beschreiben.
        let persisted = read_snapshot(path).unwrap();
        proptest::prop_assert_eq!(persisted, snapshot, "Journaldatei nach {:?}", step);
        Ok(())
    }

    fn run_queue_state_machine(
        steps: &[QueueStep],
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("queue.json");
        let mut queue = OperationQueue::open(&path).unwrap();
        let mut model = QueueModel::default();
        let mut created = 0_usize;

        for step in steps {
            match *step {
                QueueStep::Enqueue => {
                    created += 1;
                    let entry = queue
                        .enqueue(file_operation(&format!("op{created}")))
                        .unwrap();
                    model.entries.push(ModelEntry {
                        id: entry.operation_id,
                        state: QueueState::Pending,
                        attention: ModelAttention::None,
                        error_code: None,
                        attempts: 0,
                    });
                }
                QueueStep::EnqueueClaimed => {
                    created += 1;
                    let allowed = model.accepts_immediate_claim();
                    let result = queue.enqueue_claimed(file_operation(&format!("cli{created}")));
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "enqueue_claimed erlaubt?");
                    match result {
                        Ok(entry) => model.entries.push(ModelEntry {
                            id: entry.operation_id,
                            state: QueueState::Validating,
                            attention: ModelAttention::None,
                            error_code: None,
                            attempts: 1,
                        }),
                        Err(error) => proptest::prop_assert_eq!(
                            error.kind(),
                            io::ErrorKind::WouldBlock,
                            "abgewiesenes CLI-Enqueue meldet WouldBlock"
                        ),
                    }
                }
                QueueStep::ClaimNext => {
                    let expected = model.claim_next();
                    let claimed = queue.claim_next().unwrap();
                    proptest::prop_assert_eq!(
                        claimed.map(|entry| entry.operation_id),
                        expected,
                        "claim_next wählt denselben Eintrag"
                    );
                }
                QueueStep::MarkRunning(index) => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = model.entries[index].state == QueueState::Validating;
                    let result = queue.mark_running(&model.entries[index].id);
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "mark_running erlaubt?");
                    if allowed {
                        model.entries[index].state = QueueState::Running;
                    }
                }
                QueueStep::Complete(index) | QueueStep::Cancel(index) => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let cancelled = matches!(step, QueueStep::Cancel(_));
                    let allowed = model.entries[index].state == QueueState::Running;
                    let id = model.entries[index].id.clone();
                    let result = if cancelled {
                        queue.cancel(&id, Progress::default())
                    } else {
                        queue.complete(&id, Progress::default())
                    };
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "Abschluss erlaubt?");
                    if allowed {
                        model.entries[index].state = if cancelled {
                            QueueState::Cancelled
                        } else {
                            QueueState::Completed
                        };
                    }
                }
                QueueStep::Fail(index) => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = matches!(
                        model.entries[index].state,
                        QueueState::Validating | QueueState::Running
                    );
                    let result =
                        queue.fail(&model.entries[index].id, Progress::default(), "Testfehler");
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "fail erlaubt?");
                    if allowed {
                        model.entries[index].state = QueueState::Failed;
                        model.entries[index].error_code = None;
                        // Stop-on-error hält die ganze Queue an, nicht nur den
                        // betroffenen Eintrag.
                        if model.stop_on_error {
                            model.paused = true;
                        }
                    }
                }
                QueueStep::FailCoded {
                    index,
                    outcome_unclear,
                } => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = matches!(
                        model.entries[index].state,
                        QueueState::Validating | QueueState::Running
                    );
                    let code = if outcome_unclear {
                        crate::ErrorCode::OutcomeUnclear
                    } else {
                        crate::ErrorCode::Io
                    };
                    let error = crate::coded_error(io::ErrorKind::Other, code, "Testfehler");
                    let result =
                        queue.fail_coded(&model.entries[index].id, Progress::default(), &error);
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "fail_coded erlaubt?");
                    if allowed {
                        model.entries[index].state = QueueState::Failed;
                        model.entries[index].error_code = Some(code);
                        if model.stop_on_error {
                            model.paused = true;
                        }
                    }
                }
                QueueStep::AcknowledgeUnclear { index, reviewed } => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = matches!(
                        model.entries[index].state,
                        QueueState::Failed | QueueState::Interrupted
                    ) && model.entries[index].error_code
                        == Some(crate::ErrorCode::OutcomeUnclear)
                        && reviewed;
                    let result =
                        queue.acknowledge_unclear_outcome(&model.entries[index].id, reviewed);
                    proptest::prop_assert_eq!(
                        result.is_ok(),
                        allowed,
                        "unklaren Ausgang schließen erlaubt?"
                    );
                    if allowed {
                        model.entries[index].state = QueueState::Cancelled;
                    }
                }
                QueueStep::RequireConfirmation { index, kind } => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = model.entries[index].state == QueueState::Validating;
                    let id = model.entries[index].id.clone();
                    let attention = match kind {
                        ModelAttention::Manual => QueueAttention::Manual {
                            reason: format!("Neue Lage für {id}"),
                            confirmation: model_confirmation(&id),
                        },
                        ModelAttention::ConflictsChanged => QueueAttention::ConflictsChanged {
                            reason: format!("Neue Konfliktlage für {id}"),
                            confirmation: model_confirmation(&id),
                            current_conflicts: Vec::new(),
                            current_conflict_proofs: Vec::new(),
                        },
                        ModelAttention::RemoteRun => QueueAttention::RemoteRun {
                            reason: format!("Remote-Lauf für {id}"),
                            confirmation: model_confirmation(&id),
                        },
                        ModelAttention::None => unreachable!("Generator erzeugt eine Rückfrage"),
                    };
                    let result = queue.require_confirmation(&id, attention);
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "Rückfrage erlaubt?");
                    if allowed {
                        model.entries[index].state = QueueState::AwaitingConfirmation;
                        model.entries[index].attention = kind;
                    }
                }
                QueueStep::Approve {
                    index,
                    correct_phrase,
                } => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let id = model.entries[index].id.clone();
                    let awaiting = model.entries[index].state == QueueState::AwaitingConfirmation;
                    let kind = model.entries[index].attention;
                    let phrase = if correct_phrase {
                        model_confirmation(&id)
                    } else {
                        format!("falsch-{id}")
                    };
                    let result = queue.approve_attention(&id, &phrase);
                    proptest::prop_assert_eq!(
                        result.is_ok(),
                        awaiting && correct_phrase && kind != ModelAttention::RemoteRun,
                        "Bestätigung erlaubt?"
                    );
                    match result {
                        Ok(()) => {
                            model.entries[index].state = QueueState::Pending;
                            model.entries[index].attention = ModelAttention::None;
                            model.entries[index].error_code = None;
                        }
                        Err(error) if awaiting && !correct_phrase => proptest::prop_assert_eq!(
                            crate::error::error_code(&error),
                            crate::ErrorCode::ConfirmationMismatch,
                            "falsche Phrase meldet den maschinenlesbaren Code"
                        ),
                        Err(_) => {}
                    }
                }
                QueueStep::DiscardAttention(index) => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = model.entries[index].state == QueueState::AwaitingConfirmation
                        && model.entries[index].attention != ModelAttention::RemoteRun;
                    let result = queue.discard_attention(&model.entries[index].id);
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "Verwerfen erlaubt?");
                    if allowed {
                        model.entries[index].state = QueueState::Cancelled;
                        model.entries[index].attention = ModelAttention::None;
                        model.entries[index].error_code = None;
                    }
                }
                QueueStep::Retry(index) => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = matches!(
                        model.entries[index].state,
                        QueueState::Failed | QueueState::Cancelled | QueueState::Interrupted
                    ) && model.entries[index].error_code
                        != Some(crate::ErrorCode::OutcomeUnclear);
                    let result = queue.retry(&model.entries[index].id);
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "retry erlaubt?");
                    if allowed {
                        model.entries[index].state = QueueState::Pending;
                        model.entries[index].attention = ModelAttention::None;
                        model.entries[index].error_code = None;
                    }
                }
                QueueStep::RemovePending(index) => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = model.entries[index].state == QueueState::Pending;
                    let result = queue.remove_pending(&model.entries[index].id);
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "Entfernen erlaubt?");
                    if allowed {
                        model.entries.remove(index);
                    }
                }
                QueueStep::MovePending { index, forward } => {
                    let Some(index) = model.index_of(index) else {
                        continue;
                    };
                    let allowed = model.entries[index].state == QueueState::Pending;
                    let delta = if forward { 1 } else { -1 };
                    let result = queue.move_pending(&model.entries[index].id, delta);
                    proptest::prop_assert_eq!(result.is_ok(), allowed, "Umordnen erlaubt?");
                    if allowed {
                        // Nur benachbarte wartende Plätze dürfen tauschen. Ein
                        // offener Prüf-/Recoveryeintrag begrenzt dieses Segment;
                        // abgeschlossene Historie darf dazwischen stehen.
                        let pending = model
                            .entries
                            .iter()
                            .enumerate()
                            .filter_map(|(position, entry)| {
                                (entry.state == QueueState::Pending).then_some(position)
                            })
                            .collect::<Vec<_>>();
                        let slot = pending
                            .iter()
                            .position(|position| *position == index)
                            .unwrap();
                        let other_slot = if forward {
                            slot.checked_add(1).filter(|slot| *slot < pending.len())
                        } else {
                            slot.checked_sub(1)
                        };
                        let other = other_slot.map(|slot| pending[slot]);
                        if let Some(other) = other {
                            let segment = &model.entries[index.min(other)..=index.max(other)];
                            if segment.iter().all(|entry| {
                                entry.state == QueueState::Pending
                                    || QueueModel::is_terminal(entry.state)
                            }) {
                                model.entries.swap(index, other);
                            }
                        }
                    }
                }
                QueueStep::SetPaused(paused) => {
                    queue.set_paused(paused).unwrap();
                    model.paused = paused;
                }
                QueueStep::SetStopOnError(stop) => {
                    queue.set_stop_on_error(stop).unwrap();
                    model.stop_on_error = stop;
                }
                QueueStep::Reopen => {
                    drop(queue);
                    queue = OperationQueue::open(&path).unwrap();
                    for entry in &mut model.entries {
                        // Ein Prozessende während Prüfung oder Ausführung ist die
                        // Crash-Grenze: Der Eintrag wird sichtbar unterbrochen und
                        // läuft nie still weiter.
                        if matches!(entry.state, QueueState::Validating | QueueState::Running) {
                            entry.state = QueueState::Interrupted;
                            entry.error_code = Some(crate::ErrorCode::Interrupted);
                        } else if entry.state == QueueState::AwaitingConfirmation
                            && entry.attention == ModelAttention::RemoteRun
                        {
                            entry.state = QueueState::Interrupted;
                            entry.attention = ModelAttention::None;
                            entry.error_code = Some(crate::ErrorCode::Interrupted);
                        }
                    }
                }
            }
            assert_queue_matches_model(&queue, &model, &path, step)?;
        }
        Ok(())
    }

    #[test]
    fn historische_queue_regressionen_laufen_als_exakte_folgen() {
        let cases = [
            vec![
                QueueStep::Enqueue,
                QueueStep::SetPaused(true),
                QueueStep::Enqueue,
                QueueStep::Enqueue,
                QueueStep::Enqueue,
            ],
            vec![
                QueueStep::Enqueue,
                QueueStep::Enqueue,
                QueueStep::Enqueue,
                QueueStep::ClaimNext,
                QueueStep::RequireConfirmation {
                    index: 0,
                    kind: ModelAttention::Manual,
                },
                QueueStep::Retry(3),
            ],
            vec![
                QueueStep::EnqueueClaimed,
                QueueStep::EnqueueClaimed,
                QueueStep::EnqueueClaimed,
                QueueStep::MarkRunning(0),
                QueueStep::Reopen,
            ],
            vec![
                QueueStep::EnqueueClaimed,
                QueueStep::MarkRunning(0),
                QueueStep::Cancel(0),
                QueueStep::EnqueueClaimed,
                QueueStep::Retry(2),
            ],
        ];
        for steps in cases {
            run_queue_state_machine(&steps).unwrap();
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 48,
            .. proptest::test_runner::Config::default()
        })]

        /// Zufällige Bedienfolgen führen Queue und unabhängiges Modell nie
        /// auseinander — weder im Zustand noch in der Frage, welche Handlung
        /// überhaupt erlaubt war.
        #[test]
        fn queue_zustandsmaschine_folgt_dem_modell(
            steps in proptest::collection::vec(queue_step_strategy(), 1..40),
        ) {
            run_queue_state_machine(&steps)?;
        }
    }
}
