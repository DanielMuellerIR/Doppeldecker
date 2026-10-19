//! FTP und explizites FTPS mit vollständigem MLSD.
//!
//! Der Transport kann bewusst Klartext oder TLS (`AUTH TLS`, `PROT P`) verwenden.
//! Das sichtbare Profil benennt, ob die TLS-Identität öffentlich, extern oder nicht
//! geprüft wurde. Unabhängig davon bleiben UTF-8, vollständiges `MLSD` und alle
//! Schreibfähigkeiten an getrennte STOU-/Rename-/Readback-Proben gebunden; ein
//! gewöhnliches `STOR` darf nie als exklusives Create gelten.

#[cfg(test)]
#[path = "ftps_transport_tests.rs"]
mod transport_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};
use suppaftp::types::{FileType, Mode};
use suppaftp::{FtpError, RustlsConnector, RustlsFtpStream, Status};

use crate::error::{capability_probe_required, coded_error, ErrorCode};
use crate::metadata::{AclModel, Capability, MetadataCapabilities};
use crate::remote::{
    validate_host, FtpSecurityStatus, FtpTransportSecurity, FtpsCertificatePin,
    FtpsCertificatePolicy, FtpsConnectionConfig, FtpsRootIdentity, FtpsServerIdentity,
    RemoteCapabilities,
};
use crate::sftp::{SecretResolver, SecretString};
use crate::vfs::{entry_extension, CompleteDirectoryListing, Entry, ProofListEntry, Vfs};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MLSD_BYTES: usize = 64 * 1024 * 1024;
const MAX_WELCOME_BYTES: usize = 64 * 1024;
const REQUIRED_MLST_FACTS: &str = "Type;Size;Modify;";

struct FtpsSession {
    stream: RustlsFtpStream,
    identity: FtpsServerIdentity,
}

struct ProviderState {
    session: FtpsSession,
    generation: u64,
    capabilities: RemoteCapabilities,
    probe_revisions: [u64; 3],
    active_probes: BTreeMap<(usize, u64), ActiveProbe>,
    rename_contradictions: BTreeMap<(bool, String), u64>,
}

struct ActiveProbe {
    operation: Weak<FtpsOperation>,
    root: String,
    superseded: bool,
}

/// An einen kanonischen Server-Root gebundenes FTPS-Dateisystem. Virtuelle Pfade
/// beginnen immer bei `/`; `..`, Präfixe und NUL werden vor dem Netzwerk abgelehnt.
#[derive(Clone)]
pub struct FtpsVfs {
    config: FtpsConnectionConfig,
    secrets: Arc<dyn SecretResolver>,
    state: Arc<Mutex<ProviderState>>,
    reconnect: Arc<Mutex<()>>,
    /// Nur Namen, die diese Prozessinstanz nach abgeschlossenem STOU selbst sichtbar
    /// gemacht hat. Cleanup darf niemals allein anhand eines Temp-Präfixes löschen.
    owned_files: Arc<Mutex<BTreeSet<String>>>,
    operation: Option<Arc<FtpsOperation>>,
}

struct FtpsOperation {
    owner: crate::local_staging::StagingOwner,
    config: crate::local_fs::RootHandle,
    roots: Vec<FtpsRootIdentity>,
    account_digest: [u8; 32],
    _process_lock: Arc<crate::OperationQueueProcessLock>,
    intents: Mutex<Vec<Arc<crate::local_staging::RemoteTempIntent>>>,
    capabilities: Mutex<RemoteCapabilities>,
    rename_proofs: Mutex<[Option<RenameProof>; 2]>,
    /// Eine vollständig als inhaltsfrei geprüfte Auswahl darf keine spätere
    /// Dateiwirkung aus einem Typwechsel oder einem alten Probenflag ableiten.
    directories_only: AtomicBool,
    directory_proof: Mutex<Option<DirectoryProof>>,
    /// Stoppt neue Teilproben. Bereits begonnene Proben müssen ihren eigenen
    /// Transport und Cleanup weiter prüfen dürfen, sonst blieben nur neue Reste.
    preparation_cancel: Option<crate::Cancel>,
}

#[derive(Clone)]
struct RenameProof {
    root: String,
    revision: u64,
    no_replace: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct DirectoryProof {
    root: String,
    revision: u64,
}

/// Ein interaktives Pane-Listing samt der Generation, unter der es entstanden ist.
/// Die Generation verhindert eine Vermischung mit einem parallelen Reconnect, ist
/// aber ausdrücklich kein Namespace- oder Identitätsbeleg.
#[derive(Clone, Debug)]
pub struct FtpsInteractiveDirectoryListing {
    pub entries: Vec<Entry>,
    pub session_generation: u64,
}

impl FtpsVfs {
    pub fn connect(
        config: FtpsConnectionConfig,
        secrets: Arc<dyn SecretResolver>,
    ) -> io::Result<Self> {
        config.validate()?;
        let session = connect_once(&config, secrets.as_ref())?;
        Ok(Self {
            config,
            secrets,
            state: Arc::new(Mutex::new(ProviderState {
                session,
                generation: 1,
                capabilities: RemoteCapabilities {
                    complete_listing: true,
                    ..RemoteCapabilities::default()
                },
                probe_revisions: [0; 3],
                active_probes: BTreeMap::new(),
                rename_contradictions: BTreeMap::new(),
            })),
            reconnect: Arc::new(Mutex::new(())),
            owned_files: Arc::new(Mutex::new(BTreeSet::new())),
            operation: None,
        })
    }

    /// Eine konkrete Vorbereitung erhält eine eigene unveränderliche Besitzsicht.
    /// Der Verbindungspool und die Fensterjournale werden dabei nicht verschmolzen.
    pub fn for_queue_preparation(
        &self,
        queue: &crate::OperationQueue,
        root: &Path,
    ) -> io::Result<Self> {
        self.for_queue_operation(queue, None, root, &[])
    }

    pub(crate) fn for_queue_operation(
        &self,
        queue: &crate::OperationQueue,
        operation: Option<&str>,
        root: &Path,
        additional_roots: &[&Path],
    ) -> io::Result<Self> {
        self.for_queue_operation_with_cancel(queue, operation, root, additional_roots, None)
    }

    pub(crate) fn for_queue_operation_with_cancel(
        &self,
        queue: &crate::OperationQueue,
        operation: Option<&str>,
        root: &Path,
        additional_roots: &[&Path],
        cancel: Option<&crate::Cancel>,
    ) -> io::Result<Self> {
        let (owner, process_lock, config) = queue.ftps_owner(operation)?;
        let mut roots = Vec::new();
        for root in std::iter::once(root).chain(additional_roots.iter().copied()) {
            self.remote_path(root)?;
            let identity = FtpsRootIdentity {
                server: self.identity(),
                virtual_root: normalize_virtual_path(root)?,
                session_generation: self.session_generation(),
            };
            if !roots.contains(&identity) {
                roots.push(identity);
            }
        }
        if roots.len() > 2 {
            return Err(invalid("FTPS-Operation besitzt mehr als zwei Pane-Roots"));
        }
        let mut value = self.clone();
        value.operation = Some(Arc::new(FtpsOperation {
            owner,
            config,
            roots,
            account_digest: self.account_digest(),
            _process_lock: process_lock,
            intents: Mutex::new(Vec::new()),
            capabilities: Mutex::new(RemoteCapabilities {
                complete_listing: true,
                ..RemoteCapabilities::default()
            }),
            rename_proofs: Mutex::new([None, None]),
            directories_only: AtomicBool::new(false),
            directory_proof: Mutex::new(None),
            preparation_cancel: cancel.cloned(),
        }));
        Ok(value)
    }

    fn operation(&self) -> io::Result<&FtpsOperation> {
        self.operation_in_state(&lock(&self.state))
    }

    pub(crate) fn require_mutation_owner(&self) -> io::Result<()> {
        self.operation().map(|_| ())
    }

    pub(crate) fn shares_connection_mapping(&self, other: &Self) -> bool {
        self.config == other.config && self.identity() == other.identity()
    }

    pub(crate) fn adopt_rename_proofs(&self, target: &Self) -> io::Result<()> {
        let source_owner = self.operation()?;
        let target_owner = target.operation()?;
        if source_owner.owner.queue != target_owner.owner.queue
            || source_owner.owner.operation != target_owner.owner.operation
        {
            // Die Vorschau besitzt getrennte Vorbereitungs-IDs und führt nichts
            // aus. Erst der gemeinsame aktive Queueeintrag darf Belege weitergeben.
            return Ok(());
        }
        if !Arc::ptr_eq(&self.state, &target.state) || self.config != target.config {
            return Err(crate::real_mode::plan_drift(
                "FTPS-Move-Belege stammen nicht aus derselben gebundenen Sitzung",
            ));
        }
        let state = lock(&self.state);
        let source_owner = self.operation_in_state(&state)?;
        let target_owner = target.operation_in_state(&state)?;
        let proofs = lock(&target_owner.rename_proofs).clone();
        for (directory, proof) in proofs
            .iter()
            .enumerate()
            .filter_map(|(index, proof)| proof.as_ref().map(|proof| (index == 1, proof)))
        {
            if !source_owner.roots.iter().any(|root| {
                Path::new(&proof.root).starts_with(
                    Path::new(&root.server.canonical_root)
                        .join(root.virtual_root.trim_start_matches('/')),
                )
            }) {
                return Err(crate::real_mode::plan_drift(
                    "FTPS-Move-Zielbeleg liegt außerhalb der gebundenen Pane-Roots",
                ));
            }
            if proof.no_replace {
                self.validate_rename_proof(&state, directory, proof)?;
            }
        }
        *lock(&source_owner.rename_proofs) = proofs;
        let observed = *lock(&target_owner.capabilities);
        let mut capabilities = lock(&source_owner.capabilities);
        copy_probe_capabilities(&mut capabilities, ProbeKind::File, observed);
        Ok(())
    }

    fn operation_in_state(&self, state: &ProviderState) -> io::Result<&FtpsOperation> {
        let operation = self.operation.as_deref().ok_or_else(|| {
            capability_probe_required(
                "FTPS-Wirkung benötigt vor dem ersten Befehl einen dauerhaften Queueowner",
            )
        })?;
        if !operation.config.is_current()? {
            return Err(crate::real_mode::plan_drift(
                "FTPS-Queueordner wurde ausgetauscht",
            ));
        }
        if !operation.config.has_exclusive_namespace()? {
            return Err(coded_error(io::ErrorKind::PermissionDenied, ErrorCode::PermissionDenied,
                "FTPS-Proben und Schreibwirkungen benötigen einen gegen fremdes Schreiben geschützten Queueordner"));
        }
        if operation.roots.iter().any(|root| {
            root.server != state.session.identity || root.session_generation != state.generation
        }) {
            return Err(crate::real_mode::plan_drift(
                "FTPS-Intentbindung änderte sich vor der Wirkung",
            ));
        }
        Ok(operation)
    }

    fn intent(
        &self,
        target: &str,
        phase: &str,
        paths: &[&str],
    ) -> io::Result<Arc<crate::local_staging::RemoteTempIntent>> {
        let operation = self.operation()?;
        let intent = crate::local_staging::RemoteTempIntent::create_ftps(
            &operation.owner,
            &operation.config,
            Path::new(target),
            crate::local_staging::FtpsIntentBinding {
                roots: operation.roots.clone(),
                account_digest: operation.account_digest,
                phase: phase.into(),
                stou_name_unknown: phase == "stou",
                paths: paths.iter().map(Path::new).map(Path::to_path_buf).collect(),
            },
        )?;
        // Nach dem dauerhaften Write nochmals die laufende Bindung prüfen.
        self.operation()?;
        Ok(Arc::new(intent))
    }

    fn track_intent(&self, intent: Arc<crate::local_staging::RemoteTempIntent>) -> io::Result<()> {
        lock(&self.operation()?.intents).push(intent);
        Ok(())
    }

    fn effect<T>(&self, effect: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let state = lock(&self.state);
        self.operation_in_state(&state)?;
        effect()
    }

    pub(crate) fn finish_operation_intents(&self) -> io::Result<()> {
        let operation = self
            .operation
            .as_deref()
            .ok_or_else(|| capability_probe_required("FTPS-Intentowner fehlt"))?;
        let mut intents = lock(&operation.intents);
        while let Some(intent) = intents.last() {
            intent
                .finish()
                .map_err(|error| intent_unclear(intent, error))?;
            intents.pop();
        }
        Ok(())
    }

    fn operation_error(&self, error: io::Error) -> io::Error {
        let Some(operation) = &self.operation else {
            return error;
        };
        let paths = lock(&operation.intents)
            .iter()
            .flat_map(|intent| intent.ftps_recovery().0)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return error;
        }
        let mut all_paths = crate::error::recovery_paths(&error);
        all_paths.extend(paths);
        crate::error::with_recovery_paths(
            coded_error(
                error.kind(),
                ErrorCode::OutcomeUnclear,
                format!(
                    "Offene FTPS-Wirkungsintents bleiben zur manuellen Prüfung erhalten: {error}"
                ),
            ),
            &all_paths,
        )
    }

    pub(crate) fn finish_sync_result<T>(
        &self,
        result: io::Result<T>,
        stats: impl FnOnce(&T) -> &crate::SyncStats,
    ) -> io::Result<T> {
        let value = result.map_err(|error| self.operation_error(error))?;
        if self.operation.is_some() {
            let stats = stats(&value);
            if stats.cancelled || !stats.temp_recovery_paths.is_empty() {
                let error = self.operation_error(coded_error(
                    io::ErrorKind::Interrupted,
                    ErrorCode::Interrupted,
                    "FTPS-Lauf endete mit Abbruch oder erhaltenen Zwischenständen",
                ));
                if crate::error_code(&error) == ErrorCode::OutcomeUnclear {
                    return Err(error);
                }
            } else {
                self.finish_operation_intents()?;
            }
        }
        Ok(value)
    }

    pub fn identity(&self) -> FtpsServerIdentity {
        lock(&self.state).session.identity.clone()
    }

    pub fn security_status(&self) -> FtpSecurityStatus {
        self.config.security_status()
    }

    /// Dauerhafte Kontobindung ohne Benutzername oder Secret im Paarmanifest.
    pub fn account_digest(&self) -> [u8; 32] {
        *blake3::hash(self.config.username.as_bytes()).as_bytes()
    }

    pub fn capabilities(&self) -> RemoteCapabilities {
        let state = lock(&self.state);
        match self.operation.as_deref() {
            Some(operation)
                if operation.roots.iter().all(|root| {
                    root.session_generation == state.generation
                        && root.server == state.session.identity
                }) =>
            {
                *lock(&operation.capabilities)
            }
            Some(_) => RemoteCapabilities {
                complete_listing: true,
                ..RemoteCapabilities::default()
            },
            None => state.capabilities,
        }
    }

    fn publish_operation_capabilities(
        &self,
        kind: ProbeKind,
        observed: RemoteCapabilities,
    ) -> RemoteCapabilities {
        let mut capabilities = self.bound_capabilities();
        copy_probe_capabilities(&mut capabilities, kind, observed);
        *capabilities
    }

    fn bound_capabilities(&self) -> MutexGuard<'_, RemoteCapabilities> {
        lock(
            &self
                .operation
                .as_deref()
                .expect("Probe ist an einen Owner gebunden")
                .capabilities,
        )
    }

    pub fn session_generation(&self) -> u64 {
        lock(&self.state).generation
    }

    pub(crate) fn mark_quarantine_verified(
        &self,
        expected_generation: u64,
    ) -> io::Result<RemoteCapabilities> {
        let mut state = lock(&self.state);
        if state.generation != expected_generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FTPS-Sitzung wechselte während der Quarantäneprobe",
            ));
        }
        if !state.capabilities.permits_add_only()
            || !state.capabilities.permits_directory_create()
            || !state.capabilities.atomic_noreplace_rename
            || !state.capabilities.atomic_replace_rename
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "FTPS-Quarantäne benötigt zusätzlich atomare Manifest-Ersetzungen",
            ));
        }
        state.capabilities.quarantine = true;
        Ok(state.capabilities)
    }

    pub fn reconnect(&self) -> io::Result<()> {
        let _claim = lock(&self.reconnect);
        let expected = self.identity();
        let session = connect_once(&self.config, self.secrets.as_ref())?;
        if session.identity != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "FTPS-Identität änderte sich beim Reconnect",
            ));
        }
        let mut state = lock(&self.state);
        state.session = session;
        state.generation = state.generation.saturating_add(1);
        state.active_probes.clear();
        state.rename_contradictions.clear();
        state.capabilities = RemoteCapabilities {
            complete_listing: true,
            ..RemoteCapabilities::default()
        };
        Ok(())
    }

    /// Prüft MKD unabhängig von STOU, RNTO und dem Datei-Commit-Vertrag.
    ///
    /// Manche FTP-Server beantworten ein zweites `MKD` für ein bereits vorhandenes
    /// Verzeichnis erneut mit Erfolg. Das ist für diese reine AddOnly-Wirkung kein
    /// Sicherheitsproblem: Vor dem echten Anlegen prüft `create_dir` den vollständigen
    /// Elternbestand, und ein gleichnamiger Ordner kann auch bei einer späten Kollision
    /// keine Datei und keinen vorhandenen Verzeichnisinhalt ersetzen.
    pub fn probe_directory_create_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_directory_create_capability_at(Path::new("/"))
    }

    /// Führt die künstliche MKD-Probe ausschließlich unter dem gebundenen Pane-Root
    /// aus. Dadurch liegen selbst kurzlebige Testnamen nie neben fremden Profilinhalten.
    pub fn probe_directory_create_capability_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        let proof = self.begin_probe(ProbeKind::Directory, probe_root)?;
        let root = self.remote_path(probe_root)?;
        let base = join_server_path(
            &root,
            &format!(".doppeldecker-mkdir-probe-{}", random_hex()?),
        );
        let mut session = self.fresh_session()?;
        match stat_remote(&mut session.stream, &base) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Zufälliger FTPS-MKD-Probename existiert bereits",
                ))
            }
            Err(error) => return Err(error),
        }
        let intent = self.intent(&base, "probe", &[&base])?;
        self.create_probe_directory(&mut session, &root, &base, &intent)?;
        let result = (|| {
            let entry = stat_remote(&mut session.stream, &base)?;
            if !entry.is_dir || entry.is_symlink {
                return Err(invalid("FTPS-MKD-Readback meldet kein echtes Verzeichnis"));
            }
            Ok(())
        })();
        let cleanup = session.stream.rmdir(&base).map_err(ftp_error);
        self.finish_probe(
            &session,
            "FTPS-MKD",
            &root,
            &base,
            &intent,
            ProbeOutcome { result, cleanup },
        )?;
        let mut state = lock(&self.state);
        if !proof.matches(&state) || state.session.identity != session.identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FTPS-Sitzung oder Probenrevision wechselte während der mkdir-Probe",
            ));
        }
        let mut observed = *self.bound_capabilities();
        observed.exclusive_directory_create = true;
        *lock(&self.operation_in_state(&state)?.directory_proof) = Some(DirectoryProof {
            root: root.clone(),
            revision: proof.revision,
        });
        Ok(self.publish_probe_capabilities(&mut state, &proof, ProbeKind::Directory, observed))
    }

    /// Prüft RNFR/RNTO unabhängig von STOU und Datei-Commit — zweistufig:
    ///
    /// 1. Funktioniert ein gewöhnliches Rename samt Readback überhaupt? Das trägt
    ///    die interaktive Best-Effort-Umbenennung (`best_effort_rename`).
    /// 2. Weigert sich RNTO zusätzlich, ein vorhandenes Ziel zu ersetzen? Nur dann
    ///    ist der Rename auch als atomarer No-Replace-Commit für unbeaufsichtigte
    ///    Läufe beweisbar (`atomic_noreplace_rename`).
    ///
    /// Ein Server mit überschreibendem RNTO besteht Stufe 1 und fällt bei Stufe 2
    /// sichtbar zurück — die Probe scheitert dann nicht mehr, sondern liefert die
    /// ehrliche schwächere Fähigkeit.
    pub fn probe_rename_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_rename_capability_at(Path::new("/"))
    }

    pub fn probe_rename_capability_at(&self, probe_root: &Path) -> io::Result<RemoteCapabilities> {
        let proof = self.begin_probe(ProbeKind::Rename, probe_root)?;
        let root = self.remote_path(probe_root)?;
        let base = join_server_path(
            &root,
            &format!(".doppeldecker-rename-probe-{}", random_hex()?),
        );
        let first = join_server_path(&base, "first");
        let renamed = join_server_path(&base, "renamed");
        let source = join_server_path(&base, "source");
        let collision = join_server_path(&base, "collision");
        let mut session = self.fresh_session()?;
        let intent = self.intent(&base, "probe", &[&base])?;
        self.create_probe_directory(&mut session, &root, &base, &intent)?;
        // `Ok(no_replace)` beschreibt das Ergebnis der Kollisionsstufe; ein `Err`
        // bedeutet, dass schon das gewöhnliche Rename oder sein Readback scheiterte.
        let result: io::Result<bool> = (|| {
            session.stream.mkdir(&first).map_err(ftp_error)?;
            session.stream.rename(&first, &renamed).map_err(ftp_error)?;
            if !stat_remote(&mut session.stream, &renamed)?.is_dir
                || !matches!(
                    stat_remote(&mut session.stream, &first),
                    Err(error) if error.kind() == io::ErrorKind::NotFound
                )
            {
                return Err(invalid("FTPS-Rename-Readback ist unvollständig"));
            }
            session.stream.mkdir(&source).map_err(ftp_error)?;
            session.stream.mkdir(&collision).map_err(ftp_error)?;
            if session.stream.rename(&source, &collision).is_ok() {
                // Der Server ersetzte den vorhandenen Probeeintrag: kein No-Replace,
                // aber das gewöhnliche Rename selbst ist bewiesen. Manche Server
                // ersetzen dabei nicht, sondern verschieben die Quelle IN das
                // vorhandene Zielverzeichnis — dieser verschachtelte eigene
                // Probeordner muss hier weg, sonst scheitert unten das flache
                // Cleanup und der Basisordner bliebe als Rest unter dem Pane-Root.
                let nested = join_server_path(&collision, "source");
                if stat_remote(&mut session.stream, &nested).is_ok() {
                    session.stream.rmdir(&nested).map_err(ftp_error)?;
                }
                return Ok(false);
            }
            if !stat_remote(&mut session.stream, &source)?.is_dir
                || !stat_remote(&mut session.stream, &collision)?.is_dir
            {
                return Err(invalid("FTPS-Rename-Kollision veränderte den Bestand"));
            }
            Ok(true)
        })();
        let cleanup = (|| -> io::Result<()> {
            for path in [&first, &renamed, &source, &collision] {
                if stat_remote(&mut session.stream, path).is_ok() {
                    session.stream.rmdir(path).map_err(ftp_error)?;
                }
            }
            session.stream.rmdir(&base).map_err(ftp_error)
        })();
        let no_replace = self.finish_probe(
            &session,
            "FTPS-Rename",
            &root,
            &base,
            &intent,
            ProbeOutcome { result, cleanup },
        )?;
        let mut state = lock(&self.state);
        if !proof.matches(&state) || state.session.identity != session.identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FTPS-Sitzung oder Probenrevision wechselte während der Rename-Probe",
            ));
        }
        let mut observed = *self.bound_capabilities();
        observed.best_effort_rename = true;
        // Eine Ordnerprobe kann keine Datei-No-Replace-Garantie vergeben oder
        // reparieren. Ihr eigener Beleg bleibt nach Typ und Zielroot getrennt.
        self.record_rename_evidence(&mut state, &root, &proof, true, no_replace);
        Ok(self.publish_probe_capabilities(&mut state, &proof, ProbeKind::Rename, observed))
    }

    /// Prüft ausschließlich an selbst erzeugten Einträgen, dass `DELE` und `RMD`
    /// die benannten Objekte entfernen und der Readback das bestätigt. Daraus folgt
    /// keine Quarantäne- oder tokengebundene Dauerlöschgarantie. Die interaktive
    /// Schicht nutzt den Beleg nur für begrenzte Best-Effort-Move-Quellentfernung;
    /// der eigenständige DELETE-Befehl bleibt ohne atomare Tokenbindung gesperrt.
    pub fn probe_permanent_delete_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_permanent_delete_capability_at(Path::new("/"))
    }

    pub fn probe_permanent_delete_capability_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        let proof = self.begin_probe(ProbeKind::Delete, probe_root)?;
        let root = self.remote_path(probe_root)?;
        let base_name = format!(".doppeldecker-delete-probe-{}", random_hex()?);
        let base = join_server_path(&root, &base_name);
        let mut session = self.fresh_session()?;
        let intent = self.intent(&base, "probe", &[&base])?;
        self.create_probe_directory(&mut session, &root, &base, &intent)?;
        let mut owned = BTreeMap::new();
        let mut directories = BTreeSet::new();
        let result = (|| {
            session.stream.cwd(&base).map_err(ftp_error)?;
            let file = store_unique_bytes(
                &mut session.stream,
                b"delete-probe",
                &mut owned,
                Some((&intent, &base)),
            )?;
            require_names(&mut session.stream, ".", &[&file], &[])?;
            if read_remote_hash(&mut session.stream, &file)? != blake3::hash(b"delete-probe") {
                return Err(probe_cleanup_recovery(
                    &join_server_path(&base, &file),
                    "Löschprobe erhielt nicht ihren eigenen vollständigen Inhalt",
                ));
            }
            session.stream.rm(&file).map_err(ftp_error)?;
            require_names(&mut session.stream, ".", &[], &[&file])?;
            owned.remove(&file);
            session.stream.mkdir("empty").map_err(ftp_error)?;
            directories.insert("empty".to_owned());
            require_names(&mut session.stream, ".", &["empty"], &[])?;
            session.stream.rmdir("empty").map_err(ftp_error)?;
            require_names(&mut session.stream, ".", &[], &["empty"])?;
            directories.remove("empty");
            Ok(())
        })();
        let cleanup =
            cleanup_probe_entries(&mut session.stream, &root, &base, &owned, &directories);
        self.finish_probe(
            &session,
            "FTPS-Löschprobe",
            &root,
            &base,
            &intent,
            ProbeOutcome { result, cleanup },
        )?;
        let mut state = lock(&self.state);
        if !proof.matches(&state) || state.session.identity != session.identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FTPS-Sitzung oder Probenrevision wechselte während der Löschprobe",
            ));
        }
        let mut observed = *self.bound_capabilities();
        observed.permanent_delete = true;
        Ok(self.publish_probe_capabilities(&mut state, &proof, ProbeKind::Delete, observed))
    }

    fn canonical_root(&self) -> String {
        self.identity().canonical_root
    }

    fn remote_path(&self, path: &Path) -> io::Result<String> {
        map_virtual_path(&self.canonical_root(), path)
    }

    /// Öffnet eine zusätzliche Verbindung für Datenströme und Capability-Proben und
    /// verlangt von ihr dieselbe Serveridentität wie von der gehaltenen Sitzung.
    ///
    /// Bewusste Reichweite dieser Prüfung: `FtpsServerIdentity` bindet Host, Port,
    /// Zertifikat, kanonischen Root, FEAT-Antwort und Banner — also den Server, wie
    /// er sich meldet, nicht die konkrete Adresse oder die einzelne Verbindung.
    /// Steht hinter einem Hostnamen ein Verbund mehrerer gleich konfigurierter
    /// Knoten (Multi-Node-FTPS), akzeptiert die Prüfung deshalb auch einen anderen
    /// Knoten als den, auf dem eine frühere Probe lief. Die Folgen dieser Grenze
    /// stehen bei `probe_add_only_capabilities_at`.
    fn fresh_session(&self) -> io::Result<FtpsSession> {
        let session = connect_once(&self.config, self.secrets.as_ref())?;
        if session.identity != self.identity() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "FTPS-Identität änderte sich für den Datenstrom",
            ));
        }
        Ok(session)
    }

    /// Listing **und** die Generation, unter der es entstanden ist, aus demselben
    /// gehaltenen Zustand.
    ///
    /// Beides getrennt zu lesen wäre ein Rennen: Die Verbindungen sind prozessweit
    /// geteilt, und ein Reconnect aus einem anderen Fenster zählt die Generation
    /// zwischen beiden Zugriffen hoch. Die Einträge der alten Sitzung trügen dann
    /// das Etikett der neuen — und genau daran hängt die Driftbindung von Pane,
    /// Ordnergrösse und Folgeoperation.
    fn complete_list(&self, path: &Path) -> io::Result<(Vec<ProofListEntry>, u64)> {
        let remote = self.remote_path(path)?;
        let mut state = lock(&self.state);
        let generation = state.generation;
        let entries = list_remote_directory(&mut state.session.stream, &remote)?;
        Ok((entries, generation))
    }

    /// Interaktives Pane-Listing samt gebundener Generation.
    pub fn list_interactive(&self, path: &Path) -> io::Result<FtpsInteractiveDirectoryListing> {
        let (entries, session_generation) = self.complete_list(path)?;
        Ok(FtpsInteractiveDirectoryListing {
            entries: entries.into_iter().map(|item| item.entry).collect(),
            session_generation,
        })
    }

    /// Beweist alle für AddOnly nötigen Eigenschaften mit ausschließlich künstlichen
    /// Einträgen. STOU muss einen serverseitig eindeutigen Namen liefern, RNTO darf
    /// eine Kollision nicht ersetzen und jeder Schritt wird über MLSD/RETR rückgelesen.
    pub fn probe_add_only_capabilities(&self) -> io::Result<RemoteCapabilities> {
        self.probe_add_only_capabilities_at(Path::new("/"))
    }

    /// Dokumentierte Einschränkung des Beweises (Entscheidung 2026-08-03):
    ///
    /// Die Probe läuft über `fresh_session()` auf einer eigenen frischen Verbindung
    /// und schreibt ihr Ergebnis anschließend in den gehaltenen Zustand. Die spätere
    /// Mutation — etwa der STOU-Commit in `Vfs::open_write_new` — öffnet erneut eine
    /// eigene frische Verbindung. Beide müssen dieselbe `FtpsServerIdentity` melden,
    /// aber diese Identität beschreibt den Server und nicht die einzelne Verbindung.
    /// Hinter einem Hostnamen mit mehreren gleich konfigurierten Knoten kann deshalb
    /// Knoten A das No-Replace-Verhalten belegen, während Knoten B beim Commit
    /// ersetzt. Der gleiche Vorbehalt gilt für die drei anderen FTPS-Proben
    /// (Ordner anlegen, Umbenennen, interne Löschprimitive).
    ///
    /// Bewusst nicht abgesichert: Eine erneute Probe je Mutationssitzung kostet bei
    /// jedem Upload zusätzliche Umläufe, und Probe samt Mutation an genau dieselbe
    /// Verbindung zu binden wäre ein Umbau des Sitzungsmodells. Beides lohnt erst,
    /// wenn Multi-Node-FTPS real eingesetzt wird; bis dahin bleibt es bei dieser
    /// dokumentierten Einschränkung (siehe `docs/remote-providers-design.md`).
    pub fn probe_add_only_capabilities_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        let proof = self.begin_probe(ProbeKind::File, probe_root)?;
        let nonce = random_hex()?;
        let base_name = format!(".doppeldecker-probe-{nonce}");
        let root = self.remote_path(probe_root)?;
        let base = join_server_path(&root, &base_name);
        let mut session = self.fresh_session()?;
        let intent = self.intent(&base, "probe", &[&base])?;
        self.create_probe_directory(&mut session, &root, &base, &intent)?;
        // `Ok(no_replace)` beschreibt die Kollisionsstufe: STOU-Eindeutigkeit,
        // Inhalts-Readback und gewöhnliches Rename sind dann bereits bewiesen.
        // Ein `Err` bedeutet, dass schon diese Grundlage scheiterte.
        let mut owned = BTreeMap::new();
        let result: io::Result<bool> = (|| {
            session.stream.cwd(&base).map_err(ftp_error)?;
            let first = store_unique_bytes(
                &mut session.stream,
                b"eins",
                &mut owned,
                Some((&intent, &base)),
            )?;
            let first_hash = read_remote_hash(&mut session.stream, &first)?;
            if first_hash != blake3::hash(b"eins") {
                return Err(invalid(
                    "FTPS-STOU-Readback stimmt nicht mit den Schreibdaten überein",
                ));
            }
            session
                .stream
                .rename(first.as_str(), "renamed")
                .map_err(ftp_error)?;
            owned.remove(&first);
            owned.insert("renamed".to_owned(), blake3::hash(b"eins"));
            require_names(&mut session.stream, ".", &["renamed"], &[&first])?;

            let collision = store_unique_bytes(
                &mut session.stream,
                b"ziel",
                &mut owned,
                Some((&intent, &base)),
            )?;
            session
                .stream
                .rename(collision.as_str(), "collision")
                .map_err(ftp_error)?;
            owned.remove(&collision);
            owned.insert("collision".to_owned(), blake3::hash(b"ziel"));
            let source = store_unique_bytes(
                &mut session.stream,
                b"quelle",
                &mut owned,
                Some((&intent, &base)),
            )?;
            if session.stream.rename(source.as_str(), "collision").is_ok() {
                owned.remove(&source);
                owned.insert("collision".to_owned(), blake3::hash(b"quelle"));
                // Der Server ersetzte das vorhandene Ziel: kein No-Replace-Beweis.
                // STOU, Readback und Rename selbst sind trotzdem belegt — daraus
                // entsteht die schwächere Best-Effort-Commit-Fähigkeit.
                require_names(&mut session.stream, ".", &["collision"], &[&source])?;
                if read_remote_hash(&mut session.stream, "collision")? != blake3::hash(b"quelle") {
                    return Err(invalid(
                        "FTPS-Kollisionsprobe hinterließ einen unerwarteten Inhalt",
                    ));
                }
                // Genau diese Probe belegt zugleich, dass eine ausdrücklich
                // bestätigte vorhandene Datei fachlich ersetzt werden kann. Eine
                // Altversion stellt FTPS dabei nicht bereit.
                return Ok(false);
            }
            require_names(&mut session.stream, ".", &[&source, "collision"], &[])?;
            if read_remote_hash(&mut session.stream, "collision")? != blake3::hash(b"ziel") {
                return Err(invalid(
                    "FTPS-Kollisionsprobe veränderte das vorhandene Ziel",
                ));
            }
            Ok(true)
        })();
        // Die Registry liegt außerhalb der Probe-Closure, damit auch ein Teilfehler
        // nur konkret erzeugte Namen und deren erwarteten Inhalt bereinigen kann.
        let cleanup = cleanup_probe(&mut session.stream, &root, &base, &owned);
        let no_replace = self.finish_probe(
            &session,
            "FTPS-AddOnly",
            &root,
            &base,
            &intent,
            ProbeOutcome { result, cleanup },
        )?;
        let mut state = lock(&self.state);
        if !proof.matches(&state) || state.session.identity != session.identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "FTPS-Sitzung oder Probenrevision wechselte während der Capability-Probe",
            ));
        }
        let mut observed = *self.bound_capabilities();
        observed.exclusive_directory_create = true;
        // Das gewöhnliche Rename samt Listing-Readback ist in beiden Stufen belegt.
        observed.best_effort_rename = true;
        observed.best_effort_file_commit = true;
        observed.exclusive_create = no_replace;
        observed.atomic_noreplace_rename = no_replace;
        observed.verified_file_commit = no_replace;
        observed.best_effort_file_replace = !no_replace;
        self.record_rename_evidence(&mut state, &root, &proof, false, no_replace);
        Ok(self.publish_probe_capabilities(&mut state, &proof, ProbeKind::File, observed))
    }

    /// Schreibgate für neue Dateien: entweder der vollständige AddOnly-Beweis oder
    /// der ausdrücklich schwächere Best-Effort-Commit (STOU + Inhalts-Readback +
    /// gewöhnliches Rename bewiesen). Sync-Modi bleiben davon unberührt — sie
    /// prüfen weiterhin ausschließlich `permits_add_only()` über die Sync-Matrix.
    fn require_file_write(&self, path: &Path) -> io::Result<RenameProof> {
        let capabilities = self.capabilities();
        if capabilities.permits_add_only()
            || (capabilities.complete_listing && capabilities.best_effort_file_commit)
        {
            let target = self.remote_path(path)?;
            let state = lock(&self.state);
            let operation = self.operation_in_state(&state)?;
            let proof = lock(&operation.rename_proofs)[0].clone().ok_or_else(|| {
                capability_probe_required("FTPS-Dateiwirkung besitzt keinen eigenen Dateibeleg")
            })?;
            self.validate_file_proof(&state, &proof, &target)?;
            Ok(proof)
        } else {
            Err(capability_probe_required(
                "FTPS-Datei-Commit wurde für diese Sitzung nicht geprüft",
            ))
        }
    }

    fn require_directory_create(&self) -> io::Result<()> {
        if self.capabilities().permits_directory_create() {
            Ok(())
        } else {
            Err(capability_probe_required(
                "FTPS-MKD wurde für diese Sitzung noch nicht sicher geprüft",
            ))
        }
    }

    pub(crate) fn restrict_to_directories(&self) -> io::Result<()> {
        let operation = self.operation()?;
        operation.directories_only.store(true, Ordering::SeqCst);
        revoke_probe_capabilities(&mut lock(&operation.capabilities), ProbeKind::File);
        revoke_probe_capabilities(&mut lock(&operation.capabilities), ProbeKind::Delete);
        *lock(&operation.rename_proofs) = [None, None];
        Ok(())
    }

    pub(crate) fn directories_only(&self) -> bool {
        self.operation
            .as_ref()
            .is_some_and(|owner| owner.directories_only.load(Ordering::SeqCst))
    }

    pub(crate) fn check_preparation_cancel(&self) -> io::Result<()> {
        if self
            .operation()?
            .preparation_cancel
            .as_ref()
            .is_some_and(crate::Cancel::is_cancelled)
        {
            return Err(coded_error(
                io::ErrorKind::Interrupted,
                ErrorCode::Interrupted,
                "FTP-/FTPS-Vorbereitung wurde abgebrochen",
            ));
        }
        Ok(())
    }

    fn require_directory_proof(&self, path: &Path) -> io::Result<DirectoryProof> {
        let remote = self.remote_path(path)?;
        let state = lock(&self.state);
        let operation = self.operation_in_state(&state)?;
        let proof = lock(&operation.directory_proof).clone().ok_or_else(|| {
            capability_probe_required("FTPS-Ordnerwirkung benötigt einen eigenen MKD-/RMD-Beleg")
        })?;
        if remote == proof.root || !Path::new(&remote).starts_with(&proof.root) {
            return Err(capability_probe_required(
                "FTPS-Ordnerbeleg gilt nicht für diesen Root",
            ));
        }
        Ok(proof)
    }

    fn reject_directory_content(&self) -> io::Result<()> {
        if self.directories_only() {
            return Err(crate::real_mode::plan_drift(
                "Die vorbereitete FTP-/FTPS-Auswahl erlaubt nur Ordner; Dateiinhalt benötigt eine neue Vorbereitung"));
        }
        Ok(())
    }

    fn require_permanent_delete(&self) -> io::Result<()> {
        if self.capabilities().complete_listing && self.capabilities().permanent_delete {
            Ok(())
        } else {
            Err(capability_probe_required(
                "FTPS-Dauerlöschen wurde für diese Sitzung nicht geprüft",
            ))
        }
    }

    fn require_rename(&self, from: &Path, to: &Path) -> io::Result<(bool, RenameProof)> {
        self.reject_directory_content()?;
        let entry = self.stat(from)?;
        if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
            return Err(read_only());
        }
        let target = self.remote_path(to)?;
        let proof = self.require_rename_in_state(&lock(&self.state), entry.is_dir, &target)?;
        Ok((entry.is_dir, proof))
    }

    fn require_rename_in_state(
        &self,
        state: &ProviderState,
        directory: bool,
        target: &str,
    ) -> io::Result<RenameProof> {
        let operation = self.operation_in_state(state)?;
        let proofs = lock(&operation.rename_proofs);
        let proof = proofs[usize::from(directory)].as_ref().ok_or_else(|| {
            capability_probe_required(
                "FTPS-No-Replace benötigt einen Beleg für den tatsächlichen Eintragstyp",
            )
        })?;
        if !proof.no_replace
            || !Path::new(target)
                .parent()
                .is_some_and(|parent| parent.starts_with(&proof.root))
        {
            return Err(capability_probe_required(
                "FTPS-No-Replace ist für diesen Eintragstyp und Zielroot nicht bewiesen",
            ));
        }
        self.validate_rename_proof(state, directory, proof)?;
        Ok(proof.clone())
    }

    fn validate_file_proof(
        &self,
        state: &ProviderState,
        proof: &RenameProof,
        target: &str,
    ) -> io::Result<()> {
        self.operation_in_state(state)?;
        if !Path::new(target)
            .parent()
            .is_some_and(|parent| parent.starts_with(&proof.root))
        {
            return Err(capability_probe_required(
                "FTPS-Dateibeleg gilt nicht für diesen Zielroot",
            ));
        }
        if proof.no_replace {
            self.validate_rename_proof(state, false, proof)?;
        }
        Ok(())
    }

    fn validate_rename_proof(
        &self,
        state: &ProviderState,
        directory: bool,
        proof: &RenameProof,
    ) -> io::Result<()> {
        if state
            .rename_contradictions
            .iter()
            .any(|((kind, root), revision)| {
                *kind == directory
                    && *revision > proof.revision
                    && (Path::new(root).starts_with(&proof.root)
                        || Path::new(&proof.root).starts_with(root))
            })
        {
            return Err(crate::real_mode::plan_drift(
                "Frischer FTPS-Gegenbeleg widerspricht dem bestätigten No-Replace-Vertrag",
            ));
        }
        Ok(())
    }

    pub(crate) fn rename_assured_at(&self, root: &Path, directory: bool) -> io::Result<bool> {
        let root = self.remote_path(root)?;
        let state = lock(&self.state);
        let operation = self.operation_in_state(&state)?;
        let proofs = lock(&operation.rename_proofs);
        let Some(proof) = &proofs[usize::from(directory)] else {
            return Ok(false);
        };
        if proof.root != root {
            return Ok(false);
        }
        if proof.no_replace {
            self.validate_rename_proof(&state, directory, proof)?;
        }
        Ok(proof.no_replace)
    }

    pub(crate) fn file_capabilities_at(&self, root: &Path) -> io::Result<RemoteCapabilities> {
        let root = self.remote_path(root)?;
        let state = lock(&self.state);
        let operation = self.operation_in_state(&state)?;
        let proofs = lock(&operation.rename_proofs);
        let proof = proofs[0]
            .as_ref()
            .filter(|proof| proof.root == root)
            .ok_or_else(|| {
                capability_probe_required(
                    "Für diesen FTPS-Zielroot besteht kein eigener Dateibeleg",
                )
            })?;
        if proof.no_replace {
            self.validate_rename_proof(&state, false, proof)?;
        }
        Ok(*lock(&operation.capabilities))
    }

    pub(crate) fn probe_rename_kind_at(
        &self,
        root: &Path,
        directory: bool,
    ) -> io::Result<(bool, bool)> {
        if directory {
            let capabilities = self.probe_rename_capability_at(root)?;
            return Ok((
                self.rename_assured_at(root, true)?,
                capabilities.best_effort_rename,
            ));
        }
        match self.probe_add_only_capabilities_at(root) {
            Ok(capabilities) => Ok((
                self.rename_assured_at(root, false)?,
                capabilities.best_effort_rename,
            )),
            Err(error)
                if matches!(
                    crate::error_code(&error),
                    ErrorCode::ConnectionLost
                        | ErrorCode::OutcomeUnclear
                        | ErrorCode::PlanDrift
                        | ErrorCode::Interrupted
                ) =>
            {
                Err(error)
            }
            Err(_) => {
                // Ein sauber abgelehntes STOU verhindert keine gewöhnliche
                // Umbenennung. Die flache Ordnerprobe belegt dann nur den
                // Best-Effort-Befehl; sie gibt keinen Datei-No-Replace-Beleg.
                let capabilities = self.probe_rename_capability_at(root)?;
                Ok((false, capabilities.best_effort_rename))
            }
        }
    }

    fn require_plain_rename(&self) -> io::Result<()> {
        // Das gewöhnliche Rename genügt sich selbst nicht: Es läuft nur nach der
        // Verhaltensprobe, die RNFR/RNTO samt Readback an dieser Sitzung belegt hat.
        let capabilities = self.capabilities();
        if capabilities.best_effort_rename || capabilities.atomic_noreplace_rename {
            Ok(())
        } else {
            Err(capability_probe_required(
                "FTPS-Rename wurde für diese Sitzung noch nicht geprüft",
            ))
        }
    }

    fn require_file_replace(&self) -> io::Result<()> {
        if self.capabilities().best_effort_file_replace {
            Ok(())
        } else {
            Err(capability_probe_required(
                "FTPS-Ersetzen wurde für diese Sitzung nicht geprüft",
            ))
        }
    }
}

#[derive(Clone, Copy)]
enum ProbeKind {
    Directory,
    Rename,
    File,
    Delete,
}

struct ProbeOutcome<T> {
    result: io::Result<T>,
    cleanup: io::Result<()>,
}

struct ProbeTicket {
    state: Arc<Mutex<ProviderState>>,
    generation: u64,
    family: usize,
    revision: u64,
}

impl ProbeTicket {
    fn matches(&self, state: &ProviderState) -> bool {
        state.generation == self.generation
            && state
                .active_probes
                .get(&(self.family, self.revision))
                .is_some_and(|probe| !probe.superseded)
    }
}

impl Drop for ProbeTicket {
    fn drop(&mut self) {
        lock(&self.state)
            .active_probes
            .remove(&(self.family, self.revision));
    }
}

impl FtpsVfs {
    fn record_rename_evidence(
        &self,
        state: &mut ProviderState,
        root: &str,
        proof: &ProbeTicket,
        directory: bool,
        no_replace: bool,
    ) {
        let operation = self.operation.as_deref().expect("Owner vor Probe geprüft");
        lock(&operation.rename_proofs)[usize::from(directory)] = Some(RenameProof {
            root: root.into(),
            revision: proof.revision,
            no_replace,
        });
        if !no_replace {
            let revision = state
                .rename_contradictions
                .entry((directory, root.into()))
                .or_default();
            *revision = (*revision).max(proof.revision);
        }
    }

    fn publish_probe_capabilities(
        &self,
        state: &mut ProviderState,
        proof: &ProbeTicket,
        kind: ProbeKind,
        observed: RemoteCapabilities,
    ) -> RemoteCapabilities {
        // Ein unabhängiger Root darf später fertig werden. Seine eigene Klasse
        // bleibt gültig, der gemeinsame Anzeigecache erhält dagegen niemals das
        // ältere Ergebnis über die neueste begonnene Probe hinweg.
        if state.probe_revisions[proof.family] == proof.revision {
            copy_probe_capabilities(&mut state.capabilities, kind, observed);
        }
        self.publish_operation_capabilities(kind, observed)
    }

    fn begin_probe(&self, kind: ProbeKind, root: &Path) -> io::Result<ProbeTicket> {
        self.check_preparation_cancel()?;
        if self.directories_only() && !matches!(kind, ProbeKind::Directory) {
            return Err(crate::real_mode::plan_drift(
                "Die reine FTPS-Ordnerwirkung benötigt für andere Proben eine neue Vorbereitung",
            ));
        }
        let root = self.remote_path(root)?;
        let mut state = lock(&self.state);
        self.operation_in_state(&state)?;
        let family = match kind {
            ProbeKind::Directory => 0,
            ProbeKind::Rename | ProbeKind::File => 1,
            ProbeKind::Delete => 2,
        };
        state.probe_revisions[family] = state.probe_revisions[family]
            .checked_add(1)
            .ok_or_else(|| io::Error::other("FTPS-Probenrevision ist erschöpft"))?;
        let proof = ProbeTicket {
            state: self.state.clone(),
            generation: state.generation,
            family,
            revision: state.probe_revisions[family],
        };
        // Nur gleichzeitig offene Tickets benötigen eine Rootzuordnung. Eine
        // neuere überlappende Probe oder eine Neuprobe derselben Operationssicht
        // verdrängt das ältere Ticket dauerhaft, auch wenn sie selbst scheitert.
        // Getrennte Vorbereitungen verschiedener Pane-Roots bleiben unabhängig.
        let operation = Arc::downgrade(self.operation.as_ref().expect("Owner vor Probe geprüft"));
        for ((active_family, _), probe) in &mut state.active_probes {
            if *active_family == family
                && (probe.operation.ptr_eq(&operation)
                    || Path::new(&root).starts_with(&probe.root)
                    || Path::new(&probe.root).starts_with(&root))
            {
                probe.superseded = true;
            }
        }
        state.active_probes.insert(
            (family, proof.revision),
            ActiveProbe {
                operation,
                root,
                superseded: false,
            },
        );
        // Globale Beobachtung und diese konkrete Vorbereitung verlieren nur ihre
        // eigenen alten Belege. Bereits laufende andere Fenster behalten ihre
        // erfolgreich gebundene Klasse bis zum Abschluss derselben Operation.
        revoke_probe_capabilities(&mut state.capabilities, kind);
        let operation = self.operation.as_deref().expect("Owner vor Probe geprüft");
        match kind {
            ProbeKind::Directory => *lock(&operation.directory_proof) = None,
            ProbeKind::Rename => lock(&operation.rename_proofs)[1] = None,
            ProbeKind::File => lock(&operation.rename_proofs)[0] = None,
            _ => {}
        }
        revoke_probe_capabilities(&mut lock(&operation.capabilities), kind);
        if lock(&operation.rename_proofs).iter().any(Option::is_some) {
            lock(&operation.capabilities).best_effort_rename = true;
        }
        Ok(proof)
    }
}

fn copy_probe_capabilities(
    target: &mut RemoteCapabilities,
    kind: ProbeKind,
    observed: RemoteCapabilities,
) {
    match kind {
        ProbeKind::Directory => {
            target.exclusive_directory_create = observed.exclusive_directory_create
        }
        ProbeKind::Delete => target.permanent_delete = observed.permanent_delete,
        ProbeKind::Rename => target.best_effort_rename = observed.best_effort_rename,
        ProbeKind::File => {
            target.best_effort_rename = observed.best_effort_rename;
            target.atomic_noreplace_rename = observed.atomic_noreplace_rename;
            target.exclusive_create = observed.exclusive_create;
            target.verified_file_commit = observed.verified_file_commit;
            target.best_effort_file_commit = observed.best_effort_file_commit;
            target.best_effort_file_replace = observed.best_effort_file_replace;
            if matches!(kind, ProbeKind::File) {
                target.exclusive_directory_create = observed.exclusive_directory_create;
            }
        }
    }
}

fn revoke_probe_capabilities(capabilities: &mut RemoteCapabilities, kind: ProbeKind) {
    capabilities.quarantine = false;
    match kind {
        ProbeKind::Directory => capabilities.exclusive_directory_create = false,
        ProbeKind::Delete => capabilities.permanent_delete = false,
        ProbeKind::Rename => capabilities.best_effort_rename = false,
        ProbeKind::File => {
            capabilities.best_effort_rename = false;
            capabilities.atomic_noreplace_rename = false;
            capabilities.exclusive_create = false;
            capabilities.verified_file_commit = false;
            capabilities.best_effort_file_commit = false;
            capabilities.best_effort_file_replace = false;
        }
    }
}

impl Vfs for FtpsVfs {
    fn safety_capabilities_digest(&self) -> io::Result<Option<[u8; 32]>> {
        crate::remote::capabilities_digest(self.capabilities()).map(Some)
    }

    fn reclaim_verification_session(&self) -> io::Result<()> {
        // Identitätsgeprüfter Neuaufbau derselben Verbindung; setzt die
        // verhaltensgeprüften Fähigkeiten zurück. Nur für lesende Zustandsklärung.
        self.reconnect()
    }

    fn file_commit_class(&self) -> crate::vfs::FileCommitClass {
        if self.directories_only() {
            return crate::vfs::FileCommitClass::BestEffort;
        }
        // Die Klasse folgt den verhaltensgeprüften Fähigkeiten dieser Sitzung:
        // bewiesenes No-Replace bleibt abgesichert; sonst trägt der schwächere
        // Best-Effort-Beweis den sichtbaren Commit mit frischer Bestandsprüfung.
        let capabilities = self.capabilities();
        if capabilities.atomic_noreplace_rename {
            crate::vfs::FileCommitClass::Assured
        } else if capabilities.best_effort_file_commit {
            crate::vfs::FileCommitClass::BestEffort
        } else {
            // Ohne jeden Beweis sperrt `require_file_write` das Schreiben ohnehin;
            // die Klasse bleibt dann bedeutungslos konservativ.
            crate::vfs::FileCommitClass::Assured
        }
    }

    fn file_replace_class(&self) -> crate::vfs::FileReplaceClass {
        if self.capabilities().best_effort_file_replace {
            crate::vfs::FileReplaceClass::BestEffort
        } else {
            crate::vfs::FileReplaceClass::Assured
        }
    }

    fn supports_mode_write(&self) -> bool {
        false
    }

    fn supports_mtime_write(&self) -> bool {
        false
    }

    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(self
            .complete_list(path)?
            .0
            .into_iter()
            .map(|item| item.entry)
            .collect())
    }

    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let (entries, generation) = self.complete_list(path)?;
        Ok(CompleteDirectoryListing {
            provider_token: Some(listing_token(&entries, generation)?),
            entries,
            complete: true,
        })
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let normalized = normalize_virtual_path(path)?;
        if normalized == "/" {
            return Ok(Entry {
                name: "/".into(),
                is_dir: true,
                is_file: false,
                is_symlink: false,
                size: 0,
                mtime: None,
                created: None,
                owner: None,
                group: None,
                extension: None,
                mode: None,
                device: None,
                inode: None,
                hard_links: None,
            });
        }
        let parent = Path::new(&normalized)
            .parent()
            .ok_or_else(|| invalid("FTPS-Pfad besitzt keinen Elternpfad"))?;
        let name = Path::new(&normalized)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("FTPS-Pfadname ist nicht UTF-8"))?;
        self.complete_list(parent)?
            .0
            .into_iter()
            .find(|item| item.entry.name == name)
            .map(|item| item.entry)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "FTPS-Pfad fehlt"))
    }

    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut current = String::from("/");
        for part in virtual_components(path)? {
            current = if current == "/" {
                format!("/{part}")
            } else {
                format!("{current}/{part}")
            };
            match self.stat(Path::new(&current)) {
                Ok(entry) if entry.is_dir && !entry.is_symlink => {}
                Ok(_) => return Err(invalid("FTPS-Pfadkomponente ist kein echtes Verzeichnis")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.create_dir(Path::new(&current))?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.require_directory_create()?;
        let directory_proof = self
            .directories_only()
            .then(|| self.require_directory_proof(path))
            .transpose()?;
        let remote = self.remote_path(path)?;
        let mut state = lock(&self.state);
        match stat_remote(&mut state.session.stream, &remote) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "FTPS-Zielname existiert bereits",
                ))
            }
            Err(error) => return Err(error),
        }
        drop(state);
        let intent = self.intent(&remote, "mkdir", &[&remote])?;
        self.track_intent(intent.clone())?;
        let mut state = lock(&self.state);
        let operation = self.operation_in_state(&state)?;
        if directory_proof
            .as_ref()
            .is_some_and(|proof| lock(&operation.directory_proof).as_ref() != Some(proof))
        {
            return Err(crate::real_mode::plan_drift(
                "FTPS-Ordnerbeleg wechselte vor MKD",
            ));
        }
        state
            .session
            .stream
            .mkdir(&remote)
            .map_err(ftp_error)
            .map_err(|error| intent_unclear(&intent, error))?;
        drop(state);
        let entry = self
            .stat(path)
            .map_err(|error| intent_unclear(&intent, error))?;
        if entry.is_dir && !entry.is_symlink {
            Ok(())
        } else {
            Err(intent_unclear(
                &intent,
                invalid("FTPS-MKD-Readback meldet kein echtes Verzeichnis"),
            ))
        }
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.reject_directory_content()?;
        let remote = self.remote_path(path)?;
        let mut session = self.fresh_session()?;
        let stream = session.stream.retr_as_stream(remote).map_err(ftp_error)?;
        Ok(Box::new(FtpsRead {
            session: Some(session.stream),
            data: Some(Box::new(stream)),
            finished: false,
        }))
    }

    fn open_write(&self, _path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Err(read_only())
    }

    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        self.reject_directory_content()?;
        let file_proof = self.require_file_write(path)?;
        let remote = self.remote_path(path)?;
        let (parent, target) = split_server_path(&remote)?;
        let mut session = self.fresh_session()?;
        // Ohne No-Replace-Beweis würde schon das interne STOU→Zielnamen-Rename in
        // `finish` einen gleichnamigen Bestandseintrag still ersetzen. Deshalb wird
        // der Name hier vorab geprüft: Ein Treffer liefert `AlreadyExists`, worauf
        // der Temp-Namensgenerator des Aufrufers den nächsten Kandidaten wählt.
        // Das kleine Restfenster bis zum Rename trifft praktisch nur eigene, mit
        // PID und Zähler gebildete Doppeldecker-Tempnamen.
        if !file_proof.no_replace {
            match stat_remote(&mut session.stream, &remote) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "FTPS-Zielname für den neuen Datenstrom ist bereits belegt",
                    ))
                }
                Err(error) => return Err(error),
            }
            session.stream.cwd(&parent).map_err(ftp_error)?;
        } else {
            session.stream.cwd(&parent).map_err(ftp_error)?;
        }
        let intent = self.intent(&remote, "stou", &[&remote])?;
        self.track_intent(intent.clone())?;
        let (unique, data) = self
            .effect(|| start_stou(&mut session.stream))
            .map_err(|error| intent_unclear(&intent, error))?;
        intent
            .ftps_stou_name(Path::new(&join_server_path(&parent, &unique)))
            .map_err(|error| intent_unclear(&intent, error))?;
        Ok(Box::new(FtpsWrite {
            session: Some(session.stream),
            data: Some(data),
            unique,
            target,
            owned_path: remote,
            owned_files: self.owned_files.clone(),
            finished: false,
            failed: false,
            intent: Some(intent),
            binding: Some(self.clone()),
            file_proof: Some(file_proof),
        }))
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.reject_directory_content()?;
        self.require_file_write(path)?;
        let remote = self.remote_path(path)?;
        let mut session = self.fresh_session()?;
        let entry = stat_remote(&mut session.stream, &remote)?;
        if !entry.is_file || entry.is_symlink {
            return Err(invalid("FTPS-Datei-Readback meldet keine reguläre Datei"));
        }
        let mut stream = session.stream.retr_as_stream(&remote).map_err(ftp_error)?;
        io::copy(&mut stream, &mut io::sink())?;
        session
            .stream
            .finalize_retr_stream(stream)
            .map_err(ftp_error)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let entry = self.stat(path)?;
        if entry.is_dir && !entry.is_symlink {
            Ok(())
        } else {
            Err(invalid("FTPS-Namespace-Readback benötigt ein Verzeichnis"))
        }
    }

    fn set_mtime(&self, _path: &Path, _mtime: i64) -> io::Result<()> {
        Err(read_only())
    }

    fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(read_only())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.reject_directory_content()?;
        self.operation()?;
        if self.capabilities().permanent_delete {
            let remote = self.remote_path(path)?;
            let mut session = self.fresh_session()?;
            let intent = self.intent(&remote, "delete", &[&remote])?;
            self.track_intent(intent.clone())?;
            self.effect(|| session.stream.rm(&remote).map_err(ftp_error))
                .map_err(|error| intent_unclear(&intent, error))?;
            return require_absent_after_delete(&mut session.stream, &remote)
                .map_err(|error| intent_unclear(&intent, error));
        }
        let remote = self.remote_path(path)?;
        if !lock(&self.owned_files).contains(&remote) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "FTPS-Cleanup darf nur eine in diesem Prozess angelegte Temp-Datei entfernen",
            ));
        }
        let mut session = self.fresh_session()?;
        let intent = self.intent(&remote, "delete", &[&remote])?;
        self.track_intent(intent.clone())?;
        self.effect(|| session.stream.rm(&remote).map_err(ftp_error))
            .map_err(|error| intent_unclear(&intent, error))?;
        require_absent_after_delete(&mut session.stream, &remote)
            .map_err(|error| intent_unclear(&intent, error))?;
        lock(&self.owned_files).remove(&remote);
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.reject_directory_content()?;
        self.operation()?;
        self.require_permanent_delete()?;
        for listed in self.list_complete(path)?.entries {
            let child = path.join(&listed.entry.name);
            if listed.entry.is_dir && !listed.entry.is_symlink {
                self.remove_dir_all(&child)?;
            } else if listed.entry.is_file && !listed.entry.is_symlink {
                self.remove_file(&child)?;
            } else {
                return Err(invalid(
                    "FTPS-Dauerlöschen traf einen Symlink oder Spezialeintrag",
                ));
            }
        }
        self.remove_dir(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.require_permanent_delete()?;
        let remote = self.remote_path(path)?;
        let mut session = self.fresh_session()?;
        let intent = self.intent(&remote, "delete", &[&remote])?;
        self.track_intent(intent.clone())?;
        self.effect(|| session.stream.rmdir(&remote).map_err(ftp_error))
            .map_err(|error| intent_unclear(&intent, error))?;
        require_absent_after_delete(&mut session.stream, &remote)
            .map_err(|error| intent_unclear(&intent, error))
    }

    fn remove_move_empty_dir_best_effort(&self, path: &Path) -> io::Result<()> {
        if !self.directories_only() {
            return self.remove_dir(path);
        }
        let proof = self.require_directory_proof(path)?;
        let remote = self.remote_path(path)?;
        let mut session = self.fresh_session()?;
        let entry = stat_remote(&mut session.stream, &remote)?;
        if !entry.is_dir || entry.is_symlink {
            return Err(crate::real_mode::plan_drift(
                "FTPS-Move-Quelle ist kein echter Ordner mehr",
            ));
        }
        if !list_remote_directory(&mut session.stream, &remote)?.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                "FTPS-Move-Quellordner ist nicht mehr leer",
            ));
        }
        let intent = self.intent(&remote, "delete", &[&remote])?;
        self.track_intent(intent.clone())?;
        let state = lock(&self.state);
        let operation = self.operation_in_state(&state)?;
        if lock(&operation.directory_proof).as_ref() != Some(&proof) {
            return Err(crate::real_mode::plan_drift(
                "FTPS-Ordnerbeleg wechselte vor RMD",
            ));
        }
        session
            .stream
            .rmdir(&remote)
            .map_err(ftp_error)
            .map_err(|error| intent_unclear(&intent, error))?;
        drop(state);
        require_absent_after_delete(&mut session.stream, &remote)
            .map_err(|error| intent_unclear(&intent, error))
    }

    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(read_only())
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (directory, proof) = self.require_rename(from, to)?;
        let from = self.remote_path(from)?;
        let to = self.remote_path(to)?;
        let intent = self.intent(&to, "rename", &[&from, &to])?;
        self.track_intent(intent.clone())?;
        let mut state = lock(&self.state);
        self.operation_in_state(&state)?;
        self.validate_rename_proof(&state, directory, &proof)?;
        if let Err(error) = state.session.stream.rename(&from, &to) {
            // Der Aufrufer unterscheidet eine späte Kollision an `AlreadyExists` und
            // bietet dann den Kollisionsdialog an. FTP hat dafür keinen eigenen Code:
            // Ein abgelehntes RNTO kommt als 550 oder 553 zurück und würde sonst als
            // unspezifische Pfadablehnung erscheinen. Ein frisches Elternlisting
            // klärt den Unterschied positiv, statt ihn aus einem Fehlertext zu raten.
            let error = ftp_error(error);
            if collision_at(&mut state.session.stream, &to).unwrap_or(false) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "FTPS-Zielname entstand zwischen Vorschau und Umbenennung",
                ));
            }
            return Err(error);
        }
        let mut owned = lock(&self.owned_files);
        if owned.remove(&from) {
            owned.insert(to);
        }
        Ok(())
    }

    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.reject_directory_content()?;
        // Best-Effort-Umbenennung: exakt ein RNFR/RNTO auf der gehaltenen Sitzung.
        // Die Kollisionsprüfung (frisches Elternlisting) und der Readback liegen
        // beim Aufrufer; hier gibt es bewusst weder Wiederholung noch Fallback.
        self.require_plain_rename()?;
        let from = self.remote_path(from)?;
        let to = self.remote_path(to)?;
        let intent = self.intent(&to, "rename", &[&from, &to])?;
        self.track_intent(intent.clone())?;
        let mut state = lock(&self.state);
        self.operation_in_state(&state)?;
        state
            .session
            .stream
            .rename(&from, &to)
            .map_err(ftp_error)
            .map_err(|error| {
                crate::error::with_recovery_paths(
                    error,
                    &[
                        from.clone(),
                        to.clone(),
                        intent.path().to_string_lossy().into_owned(),
                    ],
                )
            })?;
        let mut owned = lock(&self.owned_files);
        if owned.remove(&from) {
            owned.insert(to);
        }
        Ok(())
    }

    fn rename_replace_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.require_file_replace()?;
        let from = self.remote_path(from)?;
        let to = self.remote_path(to)?;
        let intent = self.intent(&to, "rename", &[&from, &to])?;
        self.track_intent(intent.clone())?;
        let mut state = lock(&self.state);
        self.operation_in_state(&state)?;
        state
            .session
            .stream
            .rename(&from, &to)
            .map_err(ftp_error)
            .map_err(|error| intent_unclear(&intent, error))?;
        let mut owned = lock(&self.owned_files);
        owned.remove(&from);
        Ok(())
    }

    fn metadata_capabilities(&self, _root: &Path) -> io::Result<MetadataCapabilities> {
        Ok(MetadataCapabilities {
            owner: Capability::Unsupported,
            special_mode_bits: Capability::Unsupported,
            acl_model: AclModel::Unsupported,
            xattrs: Capability::Unsupported,
            resource_fork: Capability::Unsupported,
            sparse_extents: Capability::Unsupported,
            hardlinks: Capability::Unsupported,
        })
    }

    fn root_identity(&self, path: &Path) -> io::Result<crate::real_mode::RootIdentity> {
        let entry = self.stat(path)?;
        if !entry.is_dir || entry.is_symlink {
            return Err(invalid("FTPS-Root-Identität benötigt ein Verzeichnis"));
        }
        Ok(crate::real_mode::RootIdentity::Ftps(FtpsRootIdentity {
            server: self.identity(),
            virtual_root: normalize_virtual_path(path)?,
            session_generation: self.session_generation(),
        }))
    }
}

struct FtpsRead {
    session: Option<RustlsFtpStream>,
    data: Option<Box<dyn Read + Send>>,
    finished: bool,
}

impl FtpsRead {
    fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let data = self
            .data
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "FTPS-Datenstrom fehlt"))?;
        let session = self.session.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "FTPS-Kontrollsitzung fehlt")
        })?;
        session.finalize_retr_stream(data).map_err(ftp_error)?;
        self.finished = true;
        Ok(())
    }
}

impl Read for FtpsRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.finished {
            return Ok(0);
        }
        let count = self
            .data
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "FTPS-Datenstrom fehlt"))?
            .read(buffer)?;
        if count == 0 {
            self.finish()?;
        }
        Ok(count)
    }
}

impl Drop for FtpsRead {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

struct FtpsWrite {
    session: Option<RustlsFtpStream>,
    data: Option<Box<dyn FtpsDataStream>>,
    unique: String,
    target: String,
    owned_path: String,
    owned_files: Arc<Mutex<BTreeSet<String>>>,
    finished: bool,
    failed: bool,
    intent: Option<Arc<crate::local_staging::RemoteTempIntent>>,
    binding: Option<FtpsVfs>,
    file_proof: Option<RenameProof>,
}

impl FtpsWrite {
    fn unclear(&self, error: io::Error) -> io::Error {
        let parent = Path::new(&self.owned_path)
            .parent()
            .expect("geprüfter Zielpfad");
        let unique = parent.join(&self.unique).to_string_lossy().into_owned();
        let mut paths = vec![unique.clone(), self.owned_path.clone()];
        if let Some(intent) = &self.intent {
            paths.push(intent.path().to_string_lossy().into_owned());
        }
        crate::error::with_recovery_paths(
            coded_error(error.kind(), ErrorCode::OutcomeUnclear, format!(
                "FTPS-Uploadabschluss nicht bestätigt; STOU-Rest {unique}, angeforderter Name {}: {error}",
                self.owned_path
            )),
            &paths,
        )
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let data = self
            .data
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "FTPS-Datenstrom fehlt"))?;
        let session = self.session.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "FTPS-Kontrollsitzung fehlt")
        })?;
        if let Err(error) = finish_put(session, data) {
            return Err(self.unclear(error));
        }
        let session = self.session.as_mut().expect("Kontrollsitzung oben geprüft");
        // Erst nach abgeschlossenem Datenkanal wird der serverseitig eindeutige
        // STOU-Name an den angeforderten Namen gehängt. Bei bewiesenem No-Replace
        // kann dieses RNTO nichts ersetzen; auf Best-Effort-Servern hat
        // `open_write_new` den angeforderten Namen unmittelbar vorher als frei
        // geprüft, und der Aufrufer trägt den eigentlichen Ziel-Commit getrennt
        // über den Best-Effort-Weg mit frischem Listing und Readback.
        let rename = if let Some(binding) = &self.binding {
            let state = lock(&binding.state);
            binding.operation_in_state(&state).and_then(|_| {
                let proof = self.file_proof.as_ref().ok_or_else(|| {
                    capability_probe_required("FTPS-Datenstrom verlor seinen Dateibeleg")
                })?;
                binding.validate_file_proof(&state, proof, &self.owned_path)?;
                session
                    .rename(&self.unique, &self.target)
                    .map_err(ftp_error)
            })
        } else {
            session
                .rename(&self.unique, &self.target)
                .map_err(ftp_error)
        };
        if let Err(error) = rename {
            // Eine verlorene RNTO-Antwort kann bereits den Zielnamen erzeugt
            // haben. Weder ihn noch einen inzwischen fremden STOU-Namen löschen.
            return Err(self.unclear(error));
        }
        lock(&self.owned_files).insert(self.owned_path.clone());
        self.finished = true;
        Ok(())
    }
}

impl Write for FtpsWrite {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(self.unclear(io::Error::other("Schreibstrom hatte bereits einen Fehler")));
        }
        if self.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "FTPS-Schreibstrom ist bereits abgeschlossen",
            ));
        }
        let result = self
            .data
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "FTPS-Datenstrom fehlt"))?
            .write(buffer);
        match result {
            Ok(count) => Ok(count),
            Err(error) => {
                self.failed = true;
                Err(self.unclear(error))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(self.unclear(io::Error::other("Schreibstrom hatte bereits einen Fehler")));
        }
        let result = self.finish();
        self.failed |= result.is_err();
        result
    }
}

impl Drop for FtpsWrite {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // ABOR darf einen begonnenen Stream beenden, gibt aber kein Löschrecht
        // für einen anschließend möglicherweise ausgetauschten STOU-Namen.
        // Unvollständige Inhalte bleiben für die Recovery erhalten.
        if let (Some(session), Some(data)) = (self.session.as_mut(), self.data.take()) {
            let _ = session.abort(data);
        }
    }
}

trait FtpsDataStream: Read + Write + Send {
    fn shutdown_write(&mut self) -> io::Result<()>;

    #[cfg(test)]
    fn socket(&self) -> &TcpStream;
}

impl<T> FtpsDataStream for suppaftp::DataStream<T>
where
    T: suppaftp::TlsStream<InnerStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>>
        + Send,
{
    fn shutdown_write(&mut self) -> io::Result<()> {
        self.flush()?;
        if let suppaftp::DataStream::Ssl(tls) = self {
            let stream = tls.mut_ref();
            stream.conn.send_close_notify();
            while stream.conn.wants_write() {
                if stream.conn.write_tls(&mut stream.sock)? == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
            }
            stream.sock.flush()?;
        }
        self.get_ref().shutdown(std::net::Shutdown::Write)
    }

    #[cfg(test)]
    fn socket(&self) -> &TcpStream {
        self.get_ref()
    }
}

/// Ermittelt ausschließlich das öffentliche Zertifikat des expliziten TLS-Kanals.
/// Es folgen weder Anmeldung noch Dateibefehl. Der Aufrufer muss den Fingerabdruck
/// außerhalb dieser Verbindung prüfen, bevor er ihn in ein Profil übernimmt.
pub fn discover_ftps_certificate(host: &str, port: u16) -> io::Result<FtpsCertificatePin> {
    validate_endpoint(host, port)?;
    let observed = Arc::new(Mutex::new(None));
    let verifier = Arc::new(PinnedCertificateVerifier::discovery(observed.clone()));
    let config = tls_config(verifier)?;
    let (plain, _welcome) = connect_plain(std::iter::once(host), port)?;
    let mut secured = plain
        .into_secure(RustlsConnector::from(config), host)
        .map_err(ftp_error)?;
    secured.noop().map_err(ftp_error)?;
    let digest = lock(&observed)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "FTPS-Zertifikat fehlt"))?;
    Ok(FtpsCertificatePin { sha256: digest })
}

fn connect_once(
    config: &FtpsConnectionConfig,
    secrets: &dyn SecretResolver,
) -> io::Result<FtpsSession> {
    config.validate()?;
    let (plain, welcome) = connect_plain(config.candidate_hosts(), config.port)?;
    let mut stream = match config.transport_security {
        FtpTransportSecurity::Plaintext => plain,
        FtpTransportSecurity::ExplicitTls => {
            let certificate = config
                .certificate
                .as_ref()
                .ok_or_else(|| invalid("FTPS-Zertifikat fehlt"))?;
            let policy = config
                .certificate_policy
                .ok_or_else(|| invalid("FTPS-Zertifikatsrichtlinie fehlt"))?;
            let verifier = Arc::new(PinnedCertificateVerifier::new(certificate.sha256, policy)?);
            let tls = tls_config(verifier.clone())?;
            match plain.into_secure(RustlsConnector::from(tls), &config.host) {
                Ok(stream) => stream,
                Err(_) if verifier.public_identity_rejected() => {
                    return Err(coded_error(
                        io::ErrorKind::InvalidData,
                        ErrorCode::TlsIdentityUnverified,
                        "FTPS-Zertifikat bestätigt den angesprochenen Server nicht öffentlich",
                    ));
                }
                Err(error) => return Err(ftp_error(error)),
            }
        }
    };
    let password: SecretString = secrets.resolve(&config.password_reference)?;
    stream
        .login(config.username.as_str(), password.expose())
        .map_err(ftp_error)?;
    stream.set_mode(Mode::ExtendedPassive);
    stream.transfer_type(FileType::Binary).map_err(ftp_error)?;
    let features = stream.feat().map_err(ftp_error)?;
    if !features.contains_key("MLST") || !features.contains_key("UTF8") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FTPS-Server muss MLST/MLSD und UTF8 über FEAT ausweisen",
        ));
    }
    stream.opts("UTF8", Some("ON")).map_err(ftp_error)?;
    // RFC 3659 garantiert nicht, dass alle angebotenen MLST-Fakten standardmäßig
    // aktiv sind. Ohne diese Auswahl liefern manche korrekten Server nur Namen,
    // obwohl sie Typ, Größe und Änderungszeit unterstützen. Server ohne Faktenliste
    // (darunter libunftp) liefern dagegen bereits vollständige Standardwerte, kennen
    // OPTS MLST aber nicht. Deshalb senden wir die Auswahl nur, wenn FEAT alle
    // benötigten Fakten ausdrücklich anbietet und mindestens eine davon deaktiviert.
    // `OPTS MLST` bleibt dabei ein optionaler Befehl: Eine reine Protokollablehnung
    // (z. B. 500/502 „not implemented") darf den Verbindungsaufbau nicht scheitern
    // lassen — der strenge MLSD-Parser erzwingt Typ, Größe und Modify weiterhin
    // fail-closed je Zeile, fehlende Fakten scheitern also sichtbar beim ersten
    // Listing. Nur Transport-/Verbindungsfehler bleiben harte Fehler.
    if mlst_facts_need_selection(&features) {
        if let Err(error) = stream.opts("MLST", Some(REQUIRED_MLST_FACTS)) {
            if !opts_mlst_rejection_is_tolerable(&error) {
                return Err(ftp_error(error));
            }
        }
    }
    stream.cwd(&config.requested_root).map_err(ftp_error)?;
    let canonical_root = normalize_server_root(&stream.pwd().map_err(ftp_error)?)?;
    let identity = FtpsServerIdentity {
        host: config.normalized_host(),
        port: config.port,
        transport_security: config.transport_security,
        certificate: config.certificate.clone(),
        canonical_root,
        features_digest: features_digest(&features)?,
        welcome_digest: welcome_identity_digest(&welcome),
        legacy_welcome_digest: legacy_welcome_identity_digest(&welcome),
    };
    identity.validate()?;
    Ok(FtpsSession { stream, identity })
}

fn connect_plain<'host>(
    hosts: impl IntoIterator<Item = &'host str>,
    port: u16,
) -> io::Result<(RustlsFtpStream, Vec<u8>)> {
    let mut last = None;
    for host in hosts {
        validate_endpoint(host, port)?;
        let addresses = match (host, port).to_socket_addrs() {
            Ok(addresses) => addresses,
            Err(error) => {
                last = Some(error);
                continue;
            }
        };
        for address in addresses {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(tcp) => {
                    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
                    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
                    // `suppaftp` speichert den Willkommenstext nur als UTF-8-
                    // `String`. RFC-konformes obs-text würde dort zu `None` und
                    // damit früher zum Digest der leeren Eingabe. `peek` liest die
                    // vollständige Antwort dagegen roh und ohne sie dem eigentlichen
                    // FTP-Parser wegzunehmen.
                    let welcome = peek_welcome_response(&tcp)?;
                    let stream = RustlsFtpStream::connect_with_stream(tcp).map_err(ftp_error)?;
                    return Ok((stream, welcome));
                }
                Err(error) => last = Some(error),
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "FTPS-Host fehlt")))
}

/// Liest die erste vollständige FTP-Antwort mit `MSG_PEEK`, also ohne Bytes aus
/// dem Kontrollstrom zu verbrauchen. Einzeilige und mehrzeilige RFC-959-Antworten
/// werden begrenzt; unvollständige oder übergroße Banner stoppen vor der Anmeldung.
fn peek_welcome_response(stream: &TcpStream) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut buffer = vec![0_u8; MAX_WELCOME_BYTES];
    loop {
        let read = stream.peek(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "FTP-Server schloss die Verbindung vor dem Willkommenstext",
            ));
        }
        if let Some(end) = ftp_response_end(&buffer[..read]) {
            return Ok(buffer[..end].to_vec());
        }
        if read == buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FTP-Willkommenstext überschreitet die Größenbegrenzung",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "FTP-Willkommenstext blieb unvollständig",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn ftp_response_end(bytes: &[u8]) -> Option<usize> {
    let first_end = bytes.iter().position(|byte| *byte == b'\n')? + 1;
    if bytes.len() < 4 || !bytes[..3].iter().all(u8::is_ascii_digit) || bytes[3] != b'-' {
        return Some(first_end);
    }
    let status = &bytes[..3];
    let mut start = first_end;
    while start < bytes.len() {
        let end = start + bytes[start..].iter().position(|byte| *byte == b'\n')? + 1;
        if end - start >= 4 && &bytes[start..start + 3] == status && bytes[start + 3] == b' ' {
            return Some(end);
        }
        start = end;
    }
    None
}

fn welcome_identity_digest(raw: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"doppeldecker:ftp-welcome:v2\0");
    hasher.update(&(raw.len() as u64).to_le_bytes());
    hasher.update(raw);
    *hasher.finalize().as_bytes()
}

fn legacy_welcome_identity_digest(raw: &[u8]) -> [u8; 32] {
    // Bis Schema 1 verwendete `suppaftp` den als UTF-8 gelesenen, am Ende
    // getrimmten vollständigen Antworttext; ungültiges UTF-8 fiel auf den leeren
    // Text zurück. Der Beleg wird nur zur Migration bereits authentisierter alter
    // Bindungen berechnet, nie als aktuelle Serveridentität.
    let legacy = std::str::from_utf8(raw)
        .map(str::trim_end)
        .unwrap_or_default();
    *blake3::hash(legacy.as_bytes()).as_bytes()
}

fn start_stou(stream: &mut RustlsFtpStream) -> io::Result<(String, Box<dyn FtpsDataStream>)> {
    let (response, data) = stream
        .custom_data_command("STOU", &[Status::AboutToSend, Status::AlreadyOpen])
        .map_err(ftp_error)?;
    let body = std::str::from_utf8(&response.body)
        .map_err(|_| invalid("FTPS-STOU-Antwort ist nicht UTF-8"))?
        .trim();
    // Manche Server liefern im Response-Body zusätzlich den dreistelligen FTP-
    // Statuscode. Er gehört nicht zum ausgehandelten eindeutigen Dateinamen.
    let body = body
        .split_once(' ')
        .filter(|(prefix, _)| prefix.len() == 3 && prefix.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(body, |(_, remainder)| remainder.trim());
    let body = body
        .strip_prefix("FILE:")
        .or_else(|| body.strip_prefix("File:"))
        .unwrap_or(body)
        .trim();
    let name = body
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(body);
    validate_name(name)?;
    Ok((name.to_owned(), Box::new(data)))
}

fn finish_put(stream: &mut RustlsFtpStream, mut data: Box<dyn FtpsDataStream>) -> io::Result<()> {
    // Ein Drop mit ungelesenen TLS-Sessiontickets kann TCP-RST senden und dabei
    // sogar das schon geschriebene close_notify verwerfen. Erst das Schreibende
    // ausdrücklich melden, dann die Kontrollantwort bei gehaltenem Lesekanal
    // abwarten. finalize_put_stream darf hier nur die Leihgabe verwerfen: Der
    // Datenstrom bleibt bis zur Antwort erhalten, ohne einen Upload zu wiederholen.
    data.shutdown_write()?;
    stream.finalize_put_stream(&mut data).map_err(ftp_error)
}

fn store_unique_bytes(
    stream: &mut RustlsFtpStream,
    bytes: &[u8],
    owned: &mut BTreeMap<String, blake3::Hash>,
    intent: Option<(&crate::local_staging::RemoteTempIntent, &str)>,
) -> io::Result<String> {
    if let Some((intent, _)) = intent {
        intent.ftps_stou_pending()?;
    }
    let (name, data) = start_stou(stream)?;
    // Der bestätigte eindeutige Name wird vor dem ersten Inhaltsbyte vermerkt.
    // Auch ein Teiltransfer darf nur bei exakt passendem vollständigem Hash
    // bereinigt werden; ein verlorener STOU-Name erzeugt kein Löschrecht.
    owned.insert(name.clone(), blake3::hash(bytes));
    if let Some((intent, base)) = intent {
        intent
            .ftps_stou_name(Path::new(&join_server_path(base, &name)))
            .map_err(|error| intent_unclear(intent, error))?;
    }
    write_unique_contents(stream, &name, data, bytes)?;
    Ok(name)
}

fn write_unique_contents(
    stream: &mut RustlsFtpStream,
    name: &str,
    mut data: Box<dyn FtpsDataStream>,
    bytes: &[u8],
) -> io::Result<()> {
    let write = data.write_all(bytes).and_then(|_| data.flush());
    let close = finish_put(stream, data);
    let errors = [("Schreiben/Flush", write), ("Abschluss", close)]
        .into_iter()
        .filter_map(|(phase, result)| {
            result.err().map(|error| {
                coded_error(
                    error.kind(),
                    crate::error_code(&error),
                    format!("FTPS-STOU {phase}, Recoveryname {name}: {error}"),
                )
            })
        })
        .collect();
    crate::remote::fold_cleanup_errors(errors)
}

fn read_remote_hash(stream: &mut RustlsFtpStream, path: &str) -> io::Result<blake3::Hash> {
    let mut data = stream.retr_as_stream(path).map_err(ftp_error)?;
    let mut hasher = blake3::Hasher::new();
    let read = io::copy(&mut data, &mut hasher_writer(&mut hasher));
    let close = stream.finalize_retr_stream(data).map_err(ftp_error);
    let errors = [("Inhalt", read.map(|_| ())), ("Abschluss", close)]
        .into_iter()
        .filter_map(|(phase, result)| {
            result.err().map(|error| {
                coded_error(
                    error.kind(),
                    crate::error_code(&error),
                    format!("FTPS-RETR {phase}, Pfad {path}: {error}"),
                )
            })
        })
        .collect();
    crate::remote::fold_cleanup_errors(errors)?;
    Ok(hasher.finalize())
}

fn hasher_writer(hasher: &mut blake3::Hasher) -> impl Write + '_ {
    struct Writer<'a>(&'a mut blake3::Hasher);
    impl Write for Writer<'_> {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.update(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    Writer(hasher)
}

fn require_names(
    stream: &mut RustlsFtpStream,
    path: &str,
    present: &[&str],
    absent: &[&str],
) -> io::Result<()> {
    let names = list_remote_directory(stream, path)?
        .into_iter()
        .map(|item| item.entry.name)
        .collect::<BTreeSet<_>>();
    if present.iter().any(|name| !names.contains(*name))
        || absent.iter().any(|name| names.contains(*name))
    {
        return Err(invalid(format!(
            "FTPS-Namespace-Readback widerspricht dem erwarteten Ergebnis \
             (vorhanden: {present:?}, abwesend: {absent:?}, gelesen: {names:?})"
        )));
    }
    Ok(())
}

fn probe_cleanup_recovery(path: &str, error: impl std::fmt::Display) -> io::Error {
    crate::error::with_recovery_paths(coded_error(
        io::ErrorKind::Other,
        ErrorCode::OutcomeUnclear,
        format!("FTPS-Probe-Cleanup erhält nicht eindeutig eigenen Bestand; Recoverypfad {path}: {error}"),
    ), &[path.to_owned()])
}

fn intent_unclear(intent: &crate::local_staging::RemoteTempIntent, error: io::Error) -> io::Error {
    let mut paths = crate::error::recovery_paths(&error);
    let (intent_paths, unknown) = intent.ftps_recovery();
    paths.extend(intent_paths);
    crate::error::with_recovery_paths(
        coded_error(
            error.kind(),
            ErrorCode::OutcomeUnclear,
            format!(
                "FTPS-Wirkung oder Intentabschluss nicht bestätigt{}: {error}",
                if unknown { "; STOU-Name unbekannt" } else { "" }
            ),
        ),
        &paths,
    )
}

fn plan_probe_cleanup(
    entries: Vec<ProofListEntry>,
    owned: &BTreeMap<String, blake3::Hash>,
    base: &str,
) -> io::Result<Vec<ProofListEntry>> {
    for item in &entries {
        if !item.entry.is_file || item.entry.is_symlink || !owned.contains_key(&item.entry.name) {
            return Err(probe_cleanup_recovery(
                &join_server_path(base, &item.entry.name),
                "Unerwarteter Name oder Typ wurde nicht entfernt",
            ));
        }
    }
    Ok(entries)
}

impl FtpsVfs {
    /// Eine verlorene MKD-Antwort kann schon Zustand hinterlassen haben. Ohne
    /// bestätigte Anlage entfernen wir nichts, lesen aber beide Wurzeln zurück.
    fn create_probe_directory(
        &self,
        session: &mut FtpsSession,
        root: &str,
        base: &str,
        intent: &crate::local_staging::RemoteTempIntent,
    ) -> io::Result<()> {
        match session.stream.mkdir(base).map_err(ftp_error) {
            Ok(()) => Ok(()),
            Err(error) => self.finish_probe(
                session,
                "FTPS-MKD",
                root,
                base,
                intent,
                ProbeOutcome {
                    result: Err(error),
                    cleanup: Ok(()),
                },
            ),
        }
    }

    /// Der Readback läuft auch nach Probe-/Cleanup-Fehlern. Fähigkeiten werden erst
    /// anschließend veröffentlicht; ein Fehler in der Profilwurzel darf den schon
    /// erfolgreichen Test im gewählten Ordner nicht als vollständig erscheinen lassen.
    fn finish_probe<T>(
        &self,
        session: &FtpsSession,
        provider: &str,
        root: &str,
        base: &str,
        intent: &crate::local_staging::RemoteTempIntent,
        outcome: ProbeOutcome<T>,
    ) -> io::Result<T> {
        let ProbeOutcome { result, cleanup } = outcome;
        let cleanup_succeeded = cleanup.is_ok();
        let result = crate::remote::combine_probe_outcome(provider, result, cleanup, base);
        let readback = self.verify_probe_absence(root, &session.identity, base);
        let cleared = cleanup_succeeded && readback.is_ok();
        let result = crate::remote::combine_probe_outcome(provider, result, readback, base);
        if cleared {
            let finish = intent
                .finish()
                .map_err(|error| intent_unclear(intent, error));
            let retired = finish.is_ok();
            let result = crate::remote::combine_probe_outcome(provider, result, finish, base);
            if retired {
                return result;
            }
            return result.map_err(|error| intent_unclear(intent, error));
        }
        result.map_err(|error| intent_unclear(intent, error))
    }

    fn verify_probe_absence(
        &self,
        root: &str,
        identity: &FtpsServerIdentity,
        base: &str,
    ) -> io::Result<()> {
        let profile_root = identity.canonical_root.as_str();
        let name = base.rsplit('/').next().unwrap_or(base);
        let mut errors = Vec::new();
        // Beide Listings werden unabhängig versucht. Ein Stat-/Listingfehler ist
        // kein Abwesenheitsbeweis, und Reste im Profilroot werden niemals blind gelöscht.
        for (label, path) in [("gewählter Root", root), ("Profilroot", profile_root)] {
            // Eine abgelehnte Datenverbindung kann den alten Kontrollkanal versetzen.
            // Jede Wurzel erhält deshalb eine eigene identitätsgebundene Lesesitzung.
            let listing = (|| {
                let mut readback = self.fresh_session()?;
                if readback.identity != *identity {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "FTPS-Identität wechselte vor der Restprüfung",
                    ));
                }
                list_remote_directory(&mut readback.stream, path)
            })();
            match listing {
                Ok(entries) if entries.iter().any(|item| item.entry.name == name) => {
                    errors.push(coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "FTPS-Probenrest im {label}: {}",
                            join_server_path(path, name)
                        ),
                    ));
                }
                Ok(_) => {}
                Err(error) => errors.push(coded_error(
                    error.kind(),
                    crate::error_code(&error),
                    format!("{label} {path}: {error}"),
                )),
            }
        }
        crate::remote::fold_cleanup_errors(errors)
    }
}

fn cleanup_probe(
    stream: &mut RustlsFtpStream,
    root: &str,
    base: &str,
    owned: &BTreeMap<String, blake3::Hash>,
) -> io::Result<()> {
    cleanup_probe_entries(stream, root, base, owned, &BTreeSet::new())
}

fn cleanup_probe_entries(
    stream: &mut RustlsFtpStream,
    root: &str,
    base: &str,
    owned: &BTreeMap<String, blake3::Hash>,
    directories: &BTreeSet<String>,
) -> io::Result<()> {
    stream.cwd(base).map_err(ftp_error)?;
    let (owned_directories, files): (Vec<_>, Vec<_>) =
        raw_mlsd_current(stream)?.into_iter().partition(|item| {
            item.entry.is_dir && !item.entry.is_symlink && directories.contains(&item.entry.name)
        });
    let entries = plan_probe_cleanup(files, owned, base)?;
    // Auch bekannte Namen müssen noch den eigenen Probeinhalt tragen. Sämtliche
    // Prüfungen laufen vor dem ersten DELETE; ein fremder Eintrag bleibt erhalten.
    for item in &entries {
        let actual = read_remote_hash(stream, &item.entry.name)?;
        if Some(&actual) != owned.get(&item.entry.name) {
            return Err(probe_cleanup_recovery(
                &join_server_path(base, &item.entry.name),
                "Probeinhalt wurde ausgetauscht; nichts entfernt",
            ));
        }
    }
    for item in &owned_directories {
        let path = join_server_path(base, &item.entry.name);
        if !list_remote_directory(stream, &path)?.is_empty() {
            return Err(probe_cleanup_recovery(
                &path,
                "Probeordner enthält fremden Bestand",
            ));
        }
    }
    stream.cwd(base).map_err(ftp_error)?;
    for item in entries {
        stream.rm(&item.entry.name).map_err(|error| {
            probe_cleanup_recovery(&join_server_path(base, &item.entry.name), ftp_error(error))
        })?;
    }
    for item in owned_directories {
        stream.rmdir(&item.entry.name).map_err(|error| {
            probe_cleanup_recovery(&join_server_path(base, &item.entry.name), ftp_error(error))
        })?;
    }
    stream.cwd(root).map_err(ftp_error)?;
    stream
        .rmdir(base)
        .map_err(|error| probe_cleanup_recovery(base, ftp_error(error)))
}

fn stat_remote(stream: &mut RustlsFtpStream, path: &str) -> io::Result<Entry> {
    let (parent, name) = split_server_path(path)?;
    list_remote_directory(stream, &parent)?
        .into_iter()
        .find(|item| item.entry.name == name)
        .map(|item| item.entry)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "FTPS-Pfad fehlt"))
}

fn split_server_path(path: &str) -> io::Result<(String, String)> {
    let path = Path::new(path);
    let parent = path
        .parent()
        .and_then(Path::to_str)
        .ok_or_else(|| invalid("FTPS-Pfad besitzt keinen Elternpfad"))?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid("FTPS-Pfad besitzt keinen UTF-8-Namen"))?;
    validate_name(name)?;
    Ok((
        if parent.is_empty() { "/" } else { parent }.into(),
        name.into(),
    ))
}

/// Liegt am Zielnamen inzwischen wirklich ein Eintrag?
///
/// Nur ein vollständiges Elternlisting beantwortet das. Ein Fehler beim Listen
/// beantwortet es gar nicht — dann bleibt der ursprüngliche Fehler stehen.
fn collision_at(stream: &mut RustlsFtpStream, remote: &str) -> io::Result<bool> {
    let trimmed = remote.trim_end_matches('/');
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((parent, name)) => (parent, name),
        None => ("/", trimmed),
    };
    Ok(list_remote_directory(stream, parent)?
        .into_iter()
        .any(|item| item.entry.name == name))
}

/// Beweist die Abwesenheit eines Namens **positiv** über das vollständige Listing
/// seines Elternordners.
///
/// Ein Statfehler taugt dafür nicht: FTP beantwortet mit 550 sowohl „gibt es nicht"
/// als auch „darf ich nicht" (RFC 959 §4.2.1). Ein Konto, das im Elternordner löschen,
/// ihn aber nicht listen darf — eine verbreitete ACL für Ablageordner —, hätte die
/// Löschung sonst als geprüft gemeldet, ohne dass je ein Listing gesehen wurde.
fn require_absent_after_delete(stream: &mut RustlsFtpStream, remote: &str) -> io::Result<()> {
    let trimmed = remote.trim_end_matches('/');
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((parent, name)) => (parent, name),
        None => ("/", trimmed),
    };
    if name.is_empty() {
        return Err(invalid("FTPS-Abwesenheitsbeweis braucht einen Namen"));
    }
    require_names(stream, parent, &[], &[name])
}

fn join_server_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Wählt das Verzeichnis zuerst per `CWD` und listet danach ohne MLSD-Argument.
/// Nur die argumentlose Form für das aktuelle Verzeichnis ist nach RFC 3659
/// überall definiert; `MLSD .` oder ein absoluter Pfad sind Servererweiterungen.
fn list_remote_directory(
    stream: &mut RustlsFtpStream,
    path: &str,
) -> io::Result<Vec<ProofListEntry>> {
    stream.cwd(path).map_err(ftp_error)?;
    raw_mlsd_current(stream)
}

fn raw_mlsd_current(stream: &mut RustlsFtpStream) -> io::Result<Vec<ProofListEntry>> {
    let (_, mut data) = stream
        .custom_data_command("MLSD", &[Status::AboutToSend, Status::AlreadyOpen])
        .map_err(ftp_error)?;
    let mut bytes = Vec::new();
    let read = Read::by_ref(&mut data)
        .take((MAX_MLSD_BYTES + 1) as u64)
        .read_to_end(&mut bytes);
    let close = stream.close_data_connection(data).map_err(ftp_error);
    read?;
    close?;
    if bytes.len() > MAX_MLSD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FTPS-MLSD überschreitet die Größenbegrenzung",
        ));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "FTPS-MLSD enthält trotz UTF8-Aushandlung ungültige Bytes",
        )
    })?;
    let mut names = BTreeSet::new();
    let mut result = Vec::new();
    for raw_line in text.split_terminator('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        let (name, entry) = mlsd_line_entry(line)?;
        if !names.insert(name) {
            return Err(invalid("FTPS-MLSD enthält einen doppelten Namen"));
        }
        if let Some(entry) = entry {
            result.push(entry);
        }
    }
    Ok(result)
}

/// Wertet eine MLSD-Zeile vollständig aus eigenen, streng validierten Fakten aus.
/// Rückgabe: der Name (für die Duplikatprüfung, auch für `.`/`..`) und der fertige
/// Eintrag (`None` für `.`/`..`).
///
/// Bewusst KEIN Fremdparser mehr: suppaftps `ListParser::parse_mlsd` lehnte reale
/// Serverzeilen ab (z. B. ProFTPD/Plesk mit `UNIX.mode=01777`, fünfstellig wegen
/// Sticky-/Setuid-Bits) und war zugleich nachgiebiger als unsere Prüfungen.
fn mlsd_line_entry(line: &str) -> io::Result<(String, Option<ProofListEntry>)> {
    let (facts, name) = line
        .split_once(' ')
        .ok_or_else(|| invalid("FTPS-MLSD-Zeile trennt Fakten und Namen nicht"))?;
    let values = validate_mlsd_facts(facts, name)?;
    let kind = mlsd_entry_kind(values["type"])?;
    // RFC 3659 erlaubt für cdir/pdir echte Pfadnamen statt nur `.` und `..`.
    // Diese beiden Zeilen beschreiben das gelistete Verzeichnis beziehungsweise
    // dessen Elternverzeichnis und sind keine Kindeinträge der Pane.
    if matches!(
        kind,
        MlsdEntryKind::CurrentDirectory | MlsdEntryKind::ParentDirectory
    ) {
        return Ok((name.to_owned(), None));
    }
    validate_name(name)?;
    // validate_mlsd_facts hat Typ, Größe und Zeit bereits erzwungen.
    let is_dir = kind == MlsdEntryKind::Directory;
    let is_file = kind == MlsdEntryKind::File;
    let is_symlink = kind == MlsdEntryKind::Symlink;
    let size = if is_file {
        let raw = values["size"];
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid("FTPS-Dateigröße ist nicht darstellbar"));
        }
        raw.parse::<u64>()
            .map_err(|_| invalid("FTPS-Dateigröße ist nicht darstellbar"))?
    } else {
        0
    };
    let mtime = Some(mlsd_epoch_seconds(values["modify"])?);
    let created = values
        .get("create")
        .map(|value| mlsd_epoch_seconds(value))
        .transpose()?;
    let owner = mlsd_principal(&values, "unix.ownername", "unix.owner");
    let group = mlsd_principal(&values, "unix.groupname", "unix.group");
    Ok((
        name.to_owned(),
        Some(ProofListEntry {
            raw_name: name.as_bytes().to_vec(),
            safely_decoded: true,
            provider_token: None,
            entry: Entry {
                extension: entry_extension(name, is_file),
                name: name.to_owned(),
                is_dir,
                is_file,
                is_symlink,
                size,
                mtime,
                created,
                owner,
                group,
                mode: mlsd_mode(facts),
                device: None,
                inode: None,
                hard_links: None,
            },
        }),
    ))
}

fn mlsd_principal(
    values: &BTreeMap<String, &str>,
    preferred: &str,
    fallback: &str,
) -> Option<String> {
    values
        .get(preferred)
        .or_else(|| values.get(fallback))
        .copied()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && !value.chars().any(|character| character.is_control())
        })
        .map(str::to_owned)
}

fn validate_mlsd_facts<'line>(
    facts: &'line str,
    name: &str,
) -> io::Result<BTreeMap<String, &'line str>> {
    if name.is_empty() {
        return Err(invalid("FTPS-MLSD enthält einen leeren Namen"));
    }
    let mut values = BTreeMap::new();
    for fact in facts.split(';').filter(|fact| !fact.is_empty()) {
        let (key, value) = fact
            .split_once('=')
            .ok_or_else(|| invalid("FTPS-MLSD-Fakt besitzt kein Gleichheitszeichen"))?;
        let key = key.to_ascii_lowercase();
        if values.insert(key, value).is_some() {
            return Err(invalid("FTPS-MLSD enthält einen doppelten Fakt"));
        }
    }
    let raw_kind = values
        .get("type")
        .ok_or_else(|| invalid("FTPS-MLSD enthält keinen Typ"))?;
    let kind = mlsd_entry_kind(raw_kind)?;
    if kind == MlsdEntryKind::File && !values.contains_key("size") {
        return Err(invalid("FTPS-MLSD-Datei enthält keine Größe"));
    }
    let modified = values
        .get("modify")
        .ok_or_else(|| invalid("FTPS-MLSD enthält keine Änderungszeit"))?;
    if modified.len() < 14 || !modified.as_bytes()[..14].iter().all(u8::is_ascii_digit) {
        return Err(invalid("FTPS-MLSD-Änderungszeit ist ungültig"));
    }
    Ok(values)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MlsdEntryKind {
    File,
    Directory,
    CurrentDirectory,
    ParentDirectory,
    Symlink,
    Special,
}

/// Ordnet den case-insensitiven RFC-3659-Typ ein. Neben den vier Basistypen
/// erlaubt der Standard `OS.name=type`; solche Einträge müssen als sichtbare
/// Spezialdateien bilanziert werden, statt das vollständige Listing zu verwerfen.
/// Den im RFC selbst gezeigten Unix-Typ `slink[:Ziel]` erkennen wir als Symlink,
/// verwenden sein ungebundenes Ziel aber bewusst nie für Navigation oder Wirkung.
fn mlsd_entry_kind(raw: &str) -> io::Result<MlsdEntryKind> {
    let normalized = raw.to_ascii_lowercase();
    match normalized.as_str() {
        "file" => return Ok(MlsdEntryKind::File),
        "dir" => return Ok(MlsdEntryKind::Directory),
        "cdir" => return Ok(MlsdEntryKind::CurrentDirectory),
        "pdir" => return Ok(MlsdEntryKind::ParentDirectory),
        // Einige verbreitete Server liefern noch den Typ früherer MLST-Entwürfe.
        // Er bleibt sicher darstellbar, solange wir das Ziel niemals verfolgen.
        "link" => return Ok(MlsdEntryKind::Symlink),
        _ => {}
    }

    let Some(os_type) = normalized.strip_prefix("os.") else {
        return Err(invalid("FTPS-MLSD enthält einen unbekannten Typ"));
    };
    let Some((os_name, os_kind)) = os_type.split_once('=') else {
        return Err(invalid("FTPS-MLSD enthält einen unbekannten Typ"));
    };
    if !mlsd_token(os_name) || !mlsd_token(os_kind) {
        return Err(invalid("FTPS-MLSD enthält einen unbekannten Typ"));
    }
    if os_name.eq_ignore_ascii_case("unix")
        && (os_kind.eq_ignore_ascii_case("slink")
            || os_kind
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("slink:")))
    {
        Ok(MlsdEntryKind::Symlink)
    } else {
        Ok(MlsdEntryKind::Special)
    }
}

/// `token = 1*RCHAR` aus RFC 3659 Abschnitt 2.1. Insbesondere sind Leerraum,
/// Semikolon und Gleichheitszeichen ausgeschlossen; so kann ein OS-Typ nicht in
/// weitere Fakten oder den Dateinamen hineinreichen.
fn mlsd_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b",.:!@#$%^&()-_+?/\\'\"".contains(&byte))
}

/// Rechnet eine MLSD-`modify`-Zeit (`YYYYMMDDHHMMSS`, optional `.Sekundenbruch`)
/// streng validiert in UTC-Epochensekunden um. Kalenderfehler (Monat 13,
/// 30. Februar, Stunde 24 …) sind fail-closed ein Fehler statt einer stillen
/// Näherung; der optionale Sekundenbruch wird verworfen.
fn mlsd_epoch_seconds(modify: &str) -> io::Result<i64> {
    let error = || invalid("FTPS-MLSD-Zeit ist ungültig");
    // `modify` wird bereits im Pflichtfeld-Parser geprüft. `create` ist dagegen
    // optional und erreicht diese gemeinsame Funktion direkt; sie muss deshalb
    // selbst gegen kurze oder nicht-ASCII-kodierte Providerwerte abgesichert sein.
    if modify.len() < 14
        || !modify.is_ascii()
        || !modify.as_bytes()[..14].iter().all(u8::is_ascii_digit)
    {
        return Err(error());
    }
    let (stamp, fraction) = modify.split_at(14);
    if !fraction.is_empty() {
        let digits = fraction.strip_prefix('.').ok_or_else(error)?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(error());
        }
    }
    let number = |range: std::ops::Range<usize>| -> i64 {
        stamp[range].parse::<i64>().expect("nur ASCII-Ziffern")
    };
    let (year, month, day) = (number(0..4), number(4..6), number(6..8));
    let (hour, minute, second) = (number(8..10), number(10..12), number(12..14));
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(error()),
    };
    if day < 1 || day > days_in_month || hour > 23 || minute > 59 || second > 59 {
        return Err(error());
    }
    // Tage seit 1970-01-01 über den proleptischen gregorianischen Kalender
    // (Algorithmus „days_from_civil", Howard Hinnant) — bewusst ohne Zeitzonen,
    // MLSD-Zeiten sind laut RFC 3659 immer UTC.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    Ok(days_since_epoch * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn mlsd_mode(facts: &str) -> Option<u32> {
    facts
        .split(';')
        .filter_map(|fact| fact.split_once('='))
        .find(|(key, _)| key.eq_ignore_ascii_case("unix.mode"))
        .and_then(|(_, value)| u32::from_str_radix(value, 8).ok())
        .map(|mode| mode & 0o777)
}

/// Bindet ein vollständiges Listing an einen Token dieser Sitzung.
///
/// Der Digest muss berechenbar sein: Ein Rückfall auf einen festen Ersatzwert
/// würde zwei verschiedene Listings gleich aussehen lassen und damit die
/// Identitätsbindung stillschweigend aufheben. Deshalb wird ein
/// Serialisierungsfehler gemeldet statt ersetzt.
fn listing_token(entries: &[ProofListEntry], generation: u64) -> io::Result<Vec<u8>> {
    let mut rows = entries
        .iter()
        .map(|item| {
            let entry = &item.entry;
            (
                item.raw_name.clone(),
                entry.is_dir,
                entry.is_file,
                entry.is_symlink,
                entry.size,
                entry.mtime,
                entry.mode,
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    let payload = serde_json::to_vec(&(1u32, generation, rows))
        .map_err(|error| invalid(format!("FTPS-Listingtoken ist nicht bildbar: {error}")))?;
    Ok(blake3::hash(&payload).as_bytes().to_vec())
}

/// Bindet die gemeldeten Serverfähigkeiten an einen Digest der Serveridentität.
///
/// Wie beim Listingtoken darf ein Serialisierungsfehler hier keinen festen
/// Ersatzwert erzeugen: Zwei verschiedene Server bekämen sonst denselben Digest
/// und ein Reconnect würde einen Wechsel nicht mehr bemerken.
fn features_digest(features: &suppaftp::types::Features) -> io::Result<[u8; 32]> {
    let mut rows = features
        .iter()
        .map(|(key, value)| (key.to_ascii_uppercase(), value.clone()))
        .collect::<Vec<_>>();
    rows.sort();
    let payload = serde_json::to_vec(&rows)
        .map_err(|error| invalid(format!("FTP-Fähigkeitsdigest ist nicht bildbar: {error}")))?;
    Ok(*blake3::hash(&payload).as_bytes())
}

/// True, wenn die Ablehnung des optionalen `OPTS MLST` toleriert werden darf.
/// Toleriert werden ausschließlich echte Protokollantworten des Servers (etwa
/// 500/501/502 „Befehl nicht implementiert"). Verbindungs- und Datenkanalabbrüche
/// (421/425/426) sowie Transportfehler (EOF, I/O) bleiben harte Fehler, weil dort
/// gar keine belastbare Serverentscheidung vorliegt.
fn opts_mlst_rejection_is_tolerable(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response)
        if matches!(response.status.code(), 500 | 501 | 502 | 504))
}

fn mlst_facts_need_selection(features: &suppaftp::types::Features) -> bool {
    let Some(Some(raw)) = features.get("MLST") else {
        return false;
    };
    let facts = raw
        .split(';')
        .filter(|fact| !fact.is_empty())
        .map(|fact| {
            let enabled = fact.ends_with('*');
            (fact.trim_end_matches('*').to_ascii_lowercase(), enabled)
        })
        .collect::<BTreeMap<_, _>>();
    ["type", "size", "modify"]
        .iter()
        .all(|fact| facts.contains_key(*fact))
        && ["type", "size", "modify"]
            .iter()
            .any(|fact| facts.get(*fact) == Some(&false))
}

fn normalize_server_root(raw: &str) -> io::Result<String> {
    let path = normalize_virtual_path(Path::new(raw))?;
    if !path.starts_with('/') {
        return Err(invalid("FTPS-PWD ist nicht absolut"));
    }
    Ok(path)
}

fn normalize_virtual_path(path: &Path) -> io::Result<String> {
    if !path.is_absolute() {
        return Err(invalid("FTPS-Pfad muss absolut sein"));
    }
    let components = virtual_components(path)?;
    if components.is_empty() {
        Ok("/".into())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

fn virtual_components(path: &Path) -> io::Result<Vec<&str>> {
    let mut result = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| invalid("FTPS-Pfad ist nicht UTF-8"))?;
                validate_name(value)?;
                result.push(value);
            }
            _ => return Err(invalid("FTPS-Pfad enthält Traversal oder Präfix")),
        }
    }
    Ok(result)
}

pub(crate) fn map_virtual_path(root: &str, path: &Path) -> io::Result<String> {
    let virtual_path = normalize_virtual_path(path)?;
    if virtual_path == "/" {
        Ok(root.to_owned())
    } else if root == "/" {
        Ok(virtual_path)
    } else {
        Ok(format!("{root}{virtual_path}"))
    }
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        return Err(invalid("FTPS-Name ist ungültig"));
    }
    Ok(())
}

fn validate_endpoint(host: &str, port: u16) -> io::Result<()> {
    validate_host(host)?;
    if port == 0 {
        return Err(invalid("FTPS-Endpunkt ist ungültig"));
    }
    Ok(())
}

fn read_only() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Diese FTPS-VFS-Operation ist nicht verfügbar",
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn ftp_error(error: FtpError) -> io::Error {
    match error {
        FtpError::ConnectionError(error) => error,
        // suppaftp meldet auch ein leeres EOF auf einer vom Server wegen Leerlauf
        // geschlossenen Kontrollsitzung als `BadResponse`. Als `UnexpectedEof`
        // erreicht der Fehler die bestehende einmalige Reconnect-Grenze der App.
        // Eine tatsächlich dauerhaft ungültige Antwort wird nach dem frischen Login
        // erneut abgelehnt; sie wird dadurch niemals als gültiges Listing behandelt.
        FtpError::BadResponse => io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "FTP-/FTPS-Kontrollsitzung lieferte keine vollständige Antwort",
        ),
        FtpError::UnexpectedResponse(response)
            if matches!(response.status.code(), 421 | 425 | 426) =>
        {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "FTP-/FTPS-Server beendete die Kontroll- oder Datenverbindung",
            )
        }
        FtpError::UnexpectedResponse(response)
            if matches!(response.status.code(), 430 | 530 | 532) =>
        {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "FTP-/FTPS-Anmeldung wurde abgelehnt",
            )
        }
        // 450 heißt „Aktion nicht ausgeführt, Datei nicht verfügbar (z. B. belegt)"
        // (RFC 959 §4.2.1). Das ist ausdrücklich ein vorübergehender Zustand und
        // gerade keine Abwesenheit; als `NotFound` hätte ein Abwesenheits-Readback
        // ihn als Löschbeweis gewertet.
        FtpError::UnexpectedResponse(response) if response.status.code() == 450 => {
            io::Error::other(
                "FTP-/FTPS-Aktion wurde abgelehnt; das Objekt ist vorübergehend nicht verfügbar",
            )
        }
        // 550 benutzen Server sowohl für „gibt es nicht" als auch für „darf ich
        // nicht". Der Stat-Weg braucht das `NotFound`, deshalb bleibt die Zuordnung —
        // ein Abwesenheitsbeweis darf sich aber nie darauf stützen, sondern nur auf
        // ein vollständiges Elternlisting (siehe `require_absent_after_delete`).
        FtpError::UnexpectedResponse(response) if response.status.code() == 550 => io::Error::new(
            io::ErrorKind::NotFound,
            "FTP-/FTPS-Pfad oder Aktion wurde abgelehnt",
        ),
        other => io::Error::other(other.to_string()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct PinnedCertificateVerifier {
    expected: Option<[u8; 32]>,
    observed: Arc<Mutex<Option<[u8; 32]>>>,
    webpki: Option<Arc<WebPkiServerVerifier>>,
    public_identity_rejected: AtomicBool,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedCertificateVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedCertificateVerifier")
            .field("expected", &self.expected.map(|_| "[PIN]"))
            .field("webpki", &self.webpki.is_some())
            .field(
                "public_identity_rejected",
                &self.public_identity_rejected.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl PinnedCertificateVerifier {
    pub(crate) fn discovery(observed: Arc<Mutex<Option<[u8; 32]>>>) -> Self {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        Self {
            expected: None,
            observed,
            webpki: None,
            public_identity_rejected: AtomicBool::new(false),
            algorithms,
        }
    }

    pub(crate) fn new(expected: [u8; 32], policy: FtpsCertificatePolicy) -> io::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let webpki = if policy == FtpsCertificatePolicy::WebPkiAndPin {
            let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Some(
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                    .build()
                    .map_err(|error| io::Error::other(error.to_string()))?,
            )
        } else {
            None
        };
        Ok(Self {
            expected: Some(expected),
            observed: Arc::new(Mutex::new(None)),
            webpki,
            public_identity_rejected: AtomicBool::new(false),
            algorithms: provider.signature_verification_algorithms,
        })
    }

    fn public_identity_rejected(&self) -> bool {
        self.public_identity_rejected.load(Ordering::Relaxed)
    }
}

impl ServerCertVerifier for PinnedCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        *lock(&self.observed) = Some(digest);
        if self.expected.is_some_and(|expected| expected != digest) {
            return Err(rustls::Error::General(
                "FTPS-Serverzertifikat stimmt nicht mit dem Pin überein".into(),
            ));
        }
        if let Some(webpki) = &self.webpki {
            match webpki.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            ) {
                Ok(verified) => Ok(verified),
                Err(error) => {
                    self.public_identity_rejected.store(true, Ordering::Relaxed);
                    Err(error)
                }
            }
        } else {
            Ok(ServerCertVerified::assertion())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub(crate) fn tls_config(
    verifier: Arc<dyn ServerCertVerifier>,
) -> io::Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(Arc::new(
        builder
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftp_probe_cleanup_sendet_fuer_fremden_bestand_keinen_delete() {
        use std::io::BufRead;
        for kind in ["file", "dir"] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut control, _) = listener.accept().unwrap();
                control
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(control.try_clone().unwrap());
                control.write_all(b"220 Cleanup test\r\n").unwrap();
                let mut data_listener = None;
                let mut mutations = Vec::new();
                loop {
                    let mut command = String::new();
                    if reader.read_line(&mut command).unwrap() == 0 {
                        break;
                    }
                    let command = command.trim_end_matches(['\r', '\n']);
                    match command {
                        "CWD /probe" => control.write_all(b"250 Directory changed\r\n").unwrap(),
                        "EPSV" => {
                            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                            let port = listener.local_addr().unwrap().port();
                            control
                                .write_all(
                                    format!("229 Extended passive (|||{port}|)\r\n").as_bytes(),
                                )
                                .unwrap();
                            data_listener = Some(listener);
                        }
                        "MLSD" => {
                            control
                                .write_all(b"150 Opening data connection\r\n")
                                .unwrap();
                            let (mut data, _) = data_listener.take().unwrap().accept().unwrap();
                            data.write_all(format!("Type=file;Size=4;Modify=20260728060000; own\r\nType={kind};Size=4;Modify=20260728060000; behalten\r\n").as_bytes()).unwrap();
                            drop(data);
                            control.write_all(b"226 Transfer complete\r\n").unwrap();
                        }
                        other => {
                            mutations.push(other.to_owned());
                            control.write_all(b"250 Command accepted\r\n").unwrap();
                        }
                    }
                }
                mutations
            });
            let tcp = TcpStream::connect(address).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut stream = RustlsFtpStream::connect_with_stream(tcp).unwrap();
            stream.set_mode(Mode::ExtendedPassive);
            let owned = BTreeMap::from([("own".to_owned(), blake3::hash(b"eins"))]);
            let error = cleanup_probe(&mut stream, "/", "/probe", &owned).unwrap_err();
            assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
            assert!(error.to_string().contains("/probe/behalten"));
            drop(stream);
            assert!(
                server.join().unwrap().is_empty(),
                "Fremdbestand darf keinen DELETE auslösen"
            );
        }
    }

    #[test]
    fn probe_cleanup_erhaelt_unbekannte_blaetter_und_ordner() {
        let owned = BTreeMap::from([("own".to_owned(), blake3::hash(b"eins"))]);
        for kind in ["file", "dir"] {
            let line = format!("Type={kind};Size=4;Modify=20260728060000; behalten");
            let foreign = mlsd_line_entry(&line).unwrap().1.unwrap();
            let error = plan_probe_cleanup(vec![foreign], &owned, "/probe").unwrap_err();
            assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
            assert!(error.to_string().contains("/probe/behalten"));
        }
        let own = mlsd_line_entry("Type=file;Size=4;Modify=20260728060000; own")
            .unwrap()
            .1
            .unwrap();
        assert_eq!(
            plan_probe_cleanup(vec![own], &owned, "/probe")
                .unwrap()
                .len(),
            1
        );
    }

    /// 450 ist laut RFC 959 §4.2.1 ein vorübergehender Zustand („file busy"), keine
    /// Abwesenheit. Als `NotFound` hätte ein Abwesenheits-Readback ihn als
    /// Löschbeweis gewertet.
    #[test]
    fn temporaere_ablehnung_ist_keine_abwesenheit() {
        let temporaer = ftp_error(FtpError::UnexpectedResponse(
            suppaftp::types::Response::new(
                suppaftp::Status::RequestFileActionIgnored,
                b"450 file busy".to_vec(),
            ),
        ));
        assert_ne!(temporaer.kind(), io::ErrorKind::NotFound);

        let abgelehnt = ftp_error(FtpError::UnexpectedResponse(
            suppaftp::types::Response::new(
                suppaftp::Status::FileUnavailable,
                b"550 no such file".to_vec(),
            ),
        ));
        assert_eq!(abgelehnt.kind(), io::ErrorKind::NotFound);
    }

    /// Der Abwesenheitsbeweis darf sich nie auf einen Statfehler stützen: 550 heißt
    /// bei FTP sowohl „gibt es nicht" als auch „darf ich nicht".
    #[test]
    fn loeschpfade_beweisen_abwesenheit_ueber_das_elternlisting() {
        let quelle = include_str!("ftps.rs");
        for (funktion, ende) in [
            ("fn remove_file(&self, path: &Path)", "fn remove_dir_all("),
            ("fn remove_dir(&self, path: &Path)", "fn rename(&self,"),
        ] {
            let start = quelle
                .find(funktion)
                .unwrap_or_else(|| panic!("{funktion} fehlt"));
            let rest = &quelle[start..];
            let rumpf = &rest[..rest.find(ende).unwrap_or_else(|| panic!("{ende} fehlt"))];
            assert!(
                rumpf.contains("require_absent_after_delete"),
                "{funktion} beweist die Abwesenheit nicht über das Elternlisting"
            );
            assert!(
                !rumpf.contains("io::ErrorKind::NotFound => Ok(())"),
                "{funktion} wertet wieder einen Statfehler als Löschbeweis"
            );
        }
    }
    use std::net::TcpListener;

    #[test]
    fn virtuelle_pfade_bleiben_im_kanonischen_root() {
        assert_eq!(
            map_virtual_path("/srv/daten", Path::new("/ordner/datei")).unwrap(),
            "/srv/daten/ordner/datei"
        );
        assert!(map_virtual_path("/srv/daten", Path::new("/../ausbruch")).is_err());
        assert!(map_virtual_path("/srv/daten", Path::new("relativ")).is_err());
    }

    #[test]
    fn willkommenstext_bindet_rohbytes_auch_ohne_utf8() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"220 Gruss-\xff\r\n").unwrap();
        });

        let (stream, raw) = connect_plain(std::iter::once("127.0.0.1"), port).unwrap();
        server.join().unwrap();

        assert_eq!(raw, b"220 Gruss-\xff\r\n");
        assert!(
            stream.get_welcome_msg().is_none(),
            "die Bibliothek verwirft den nicht als UTF-8 dekodierbaren Text"
        );
        assert_ne!(welcome_identity_digest(&raw), *blake3::hash(&[]).as_bytes());
        assert_ne!(
            welcome_identity_digest(b"220 Gruss-\xfe\r\n"),
            welcome_identity_digest(&raw),
            "verschiedene Rohbanner müssen verschiedene Identitäten bilden"
        );
    }

    #[test]
    fn mehrzeiliger_willkommenstext_endet_erst_an_der_statuszeile() {
        let raw = b"220-erste Zeile\r\nZwischenzeile\r\n220 fertig\r\nREST";
        assert_eq!(
            ftp_response_end(raw),
            Some(b"220-erste Zeile\r\nZwischenzeile\r\n220 fertig\r\n".len())
        );
    }

    #[test]
    fn ftps_namen_mit_steuerzeichen_bleiben_aus_listing_und_befehlen() {
        for name in ["zeile\nneu", "spalte\tneu", "escape\u{1b}name"] {
            let error = validate_name(name)
                .expect_err("Steuerzeichen dürfen keinen FTP-/FTPS-Namen bilden");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn opts_mlst_ablehnung_ist_tolerierbar_transportfehler_nicht() {
        // Regression: Ein standardkonformer Server darf den optionalen Befehl
        // `OPTS MLST` ablehnen (z. B. 500/502), ohne dass die gesamte Verbindung
        // scheitert — die Pflichtfakten prüft der strenge MLSD-Parser je Zeile.
        for code in [500_u32, 501, 502, 504] {
            assert!(opts_mlst_rejection_is_tolerable(
                &FtpError::UnexpectedResponse(suppaftp::types::Response {
                    status: suppaftp::Status::from(code),
                    body: Vec::new(),
                })
            ));
        }
        // Verbindungs-/Datenkanalabbrüche und Transportfehler bleiben hart.
        for code in [421_u32, 425, 426] {
            assert!(!opts_mlst_rejection_is_tolerable(
                &FtpError::UnexpectedResponse(suppaftp::types::Response {
                    status: suppaftp::Status::from(code),
                    body: Vec::new(),
                })
            ));
        }
        for code in [430_u32, 450, 451, 530, 532, 550, 553] {
            assert!(!opts_mlst_rejection_is_tolerable(
                &FtpError::UnexpectedResponse(suppaftp::types::Response {
                    status: suppaftp::Status::from(code),
                    body: Vec::new(),
                })
            ));
        }
        assert!(!opts_mlst_rejection_is_tolerable(&FtpError::BadResponse));
    }

    #[test]
    fn mlsd_nullmodus_bleibt_von_fehlender_modusangabe_unterscheidbar() {
        let (_, entry) =
            mlsd_line_entry("type=file;size=1;modify=20260716213817;UNIX.mode=0000; leer").unwrap();
        assert_eq!(entry.unwrap().entry.mode, Some(0));
    }

    #[test]
    fn mlsd_verlangt_typ_groesse_zeit_und_eindeutige_fakten() {
        assert!(
            validate_mlsd_facts("type=file;size=12;modify=20260712220000;", "datei.txt").is_ok()
        );
        assert!(validate_mlsd_facts("type=file;modify=20260712220000;", "x").is_err());
        assert!(
            validate_mlsd_facts("type=file;type=dir;size=1;modify=20260712220000;", "x").is_err()
        );
    }

    #[test]
    fn mlsd_unicode_in_zeitangabe_liefert_fehler_statt_panic() {
        for modified in ["2026091012345é", "202609101234é5", "20260910123456.é"] {
            let line = format!("type=file;size=1;modify={modified}; datei.txt");
            assert!(mlsd_line_entry(&line).is_err(), "{modified}");
        }
    }

    #[test]
    fn mlst_faktenauswahl_erfolgt_nur_fuer_angebotene_inaktive_pflichtfakten() {
        let mut features = suppaftp::types::Features::new();
        features.insert("MLST".into(), Some("Type;Size;Modify;UNIX.mode;".into()));
        assert!(mlst_facts_need_selection(&features));

        features.insert("MLST".into(), Some("Type*;Size*;Modify*;".into()));
        assert!(!mlst_facts_need_selection(&features));

        features.insert("MLST".into(), None);
        assert!(!mlst_facts_need_selection(&features));

        features.insert("MLST".into(), Some("Type;Size;".into()));
        assert!(!mlst_facts_need_selection(&features));
    }

    #[test]
    fn unvollstaendige_kontrollantwort_aktiviert_reconnect() {
        let error = ftp_error(FtpError::BadResponse);
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(!error.to_string().contains("Syntax"));
    }

    /// Regression: Verbreitete FTPS-Server (ProFTPD/Plesk) liefern für Sticky-/
    /// Setuid-Verzeichnisse einen fünfstelligen Modus (`UNIX.mode=01777`). Der
    /// frühere Fremdparser lehnte die ganze Zeile als „Syntax error: invalid line"
    /// ab. Zeile einem realen Serverlisting nachgebildet.
    #[test]
    fn mlsd_akzeptiert_fuenfstelligen_sticky_modus() {
        let line =
            "create=19700101000000;modify=20251007064848;perm=flcdmpe;type=dir;unique=803U5C30E4;\
                    UNIX.group=0;UNIX.groupname=users;UNIX.mode=01777;UNIX.owner=0;\
                    UNIX.ownername=user; tmp";
        let (name, entry) = mlsd_line_entry(line).unwrap();
        let entry = entry.unwrap();
        assert_eq!(name, "tmp");
        assert!(entry.entry.is_dir);
        // mlsd_mode blendet Sonderbits bewusst aus (Anzeige-Modus, & 0o777).
        assert_eq!(entry.entry.mode, Some(0o777));
        assert_eq!(entry.entry.created, Some(0));
        assert_eq!(entry.entry.owner.as_deref(), Some("user"));
        assert_eq!(entry.entry.group.as_deref(), Some("users"));
        assert_eq!(entry.entry.extension, None);
    }

    #[test]
    fn mlsd_zeile_liefert_typ_groesse_zeit_und_ueberspringt_punktordner() {
        let (_, file) =
            mlsd_line_entry("type=file;size=42;modify=20260716213817.123; bericht.txt").unwrap();
        let file = file.unwrap();
        assert!(file.entry.is_file && !file.entry.is_dir && !file.entry.is_symlink);
        assert_eq!(file.entry.size, 42);
        assert_eq!(file.entry.mtime, Some(1_784_237_897));
        assert_eq!(file.entry.extension.as_deref(), Some("txt"));

        let (_, link) = mlsd_line_entry("type=link;modify=19700101000000; verweis").unwrap();
        assert!(link.unwrap().entry.is_symlink);

        let (dot, skipped) = mlsd_line_entry("type=cdir;modify=20260716213817; .").unwrap();
        assert_eq!(dot, ".");
        assert!(skipped.is_none());
    }

    #[test]
    fn mlsd_bilanziert_rfc_os_typen_und_ueberspringt_cdir_pdir_nach_typ() {
        let (_, link) =
            mlsd_line_entry("Type=OS.Unix=SLINK:/nicht-folgen;Modify=20260716213817; verweis")
                .unwrap();
        let link = link.unwrap().entry;
        assert!(link.is_symlink);
        assert!(!link.is_file && !link.is_dir);

        let (_, special) =
            mlsd_line_entry("type=OS.unix=chr-13/29;modify=20260716213817; geraet").unwrap();
        let special = special.unwrap().entry;
        assert!(!special.is_symlink && !special.is_file && !special.is_dir);

        assert!(mlsd_line_entry("type=cdir;modify=20260716213817; /wurzel")
            .unwrap()
            .1
            .is_none());
        assert!(mlsd_line_entry("type=pdir;modify=20260716213817; eltern")
            .unwrap()
            .1
            .is_none());
    }

    #[test]
    fn mlsd_os_typ_bleibt_syntaktisch_streng() {
        for invalid_type in ["OS.=slink", "OS.unix=", "OS.unix=slink;boese", "unbekannt"] {
            let line = format!("type={invalid_type};modify=20260716213817; eintrag");
            assert!(mlsd_line_entry(&line).is_err(), "{invalid_type}");
        }
    }

    /// Kalender- und Formatfehler bleiben fail-closed Fehler statt stiller Näherung.
    #[test]
    fn mlsd_zeit_und_groesse_bleiben_streng() {
        assert_eq!(mlsd_epoch_seconds("19700101000000").unwrap(), 0);
        assert_eq!(mlsd_epoch_seconds("20000229120000").unwrap(), 951_825_600);
        // Kein Schaltjahr, Monat 13, Stunde 24, kaputter Sekundenbruch.
        assert!(mlsd_epoch_seconds("20250229120000").is_err());
        assert!(mlsd_epoch_seconds("20251301120000").is_err());
        assert!(mlsd_epoch_seconds("20250101240000").is_err());
        assert!(mlsd_epoch_seconds("20250101120000x").is_err());
        assert!(mlsd_epoch_seconds("20250101120000.").is_err());
        assert!(mlsd_epoch_seconds("kurz").is_err());
        assert!(
            mlsd_line_entry("type=file;size=1;modify=20250101120000;create=kurz; datei").is_err()
        );
        // Größen sind reine Ziffern.
        assert!(mlsd_line_entry("type=file;size=1x;modify=20250101120000; datei").is_err());
    }

    #[test]
    fn zertifikats_pin_ist_sha256_und_debug_bleibt_knapp() {
        let verifier =
            PinnedCertificateVerifier::new([7; 32], FtpsCertificatePolicy::PinnedCertificate)
                .unwrap();
        let debug = format!("{verifier:?}");
        assert!(debug.contains("[PIN]"));
        assert!(!debug.contains("070707"));
    }
}
