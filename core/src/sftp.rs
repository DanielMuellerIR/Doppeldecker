//! SFTP-Provider mit gepinnter Serveridentität und fail-closed Fähigkeiten.
//!
//! Die öffentliche [`Vfs`](crate::Vfs)-Schnittstelle ist synchron. `russh` arbeitet
//! asynchron; deshalb besitzt jede Verbindung eine kleine Tokio-Laufzeit und die
//! synchronen Aufrufe blockieren ausschließlich den aufrufenden Worker. Zugangsdaten
//! werden erst beim Verbindungsaufbau aus einer [`SecretReference`](crate::remote)
//! aufgelöst und anschließend durch [`SecretString`] beim Drop überschrieben.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::agent::client::AgentClient;
use russh::keys::ssh_key::{Fingerprint, HashAlg, PublicKey};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, StatusCode};
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};
use zeroize::Zeroizing;

use crate::capability_probe_required;
use crate::metadata::{AclModel, Capability, MetadataCapabilities};
use crate::remote::{
    validate_host, KeychainSecretReference, RemoteCapabilities, SftpAuthenticationReference,
    SftpConnectionConfig, SftpHostKeyPin, SftpServerIdentity,
};
use crate::vfs::{entry_extension, CompleteDirectoryListing, Entry, Vfs};

#[path = "sftp_directory.rs"]
mod directory_read;

#[path = "sftp_read.rs"]
mod interactive_read;
#[path = "sftp_size.rs"]
mod interactive_size;
pub use interactive_read::{
    validate_interactive_content_selection, SFTP_CONTENT_READ_LIMITATION,
    SFTP_MAX_INTERACTIVE_FILE_BYTES,
};
pub use interactive_size::SFTP_DIRECTORY_SIZE_LIMITATION;

/// Kurzlebiger Klartextwert. Debug-Ausgaben und Fehlermeldungen geben nur eine
/// Maskierung aus; der Speicher wird beim Drop durch `zeroize` überschrieben.
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: String) -> io::Result<Self> {
        if value.contains('\0') {
            return Err(invalid("Geheimnis enthält ein NUL-Zeichen"));
        }
        Ok(Self(Zeroizing::new(value)))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([MASKIERT])")
    }
}

/// App- oder CLI-Schicht zum Auflösen eines Schlüsselbund-Verweises. Implementierungen
/// dürfen Werte weder loggen noch cachen; der Kern hält sie nur für die Anmeldung.
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &KeychainSecretReference) -> io::Result<SecretString>;
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SftpPublicKeyIdentity {
    pub algorithm: String,
    pub sha256: [u8; 32],
    pub comment: String,
}

#[derive(Clone)]
struct HostKeyVerifier {
    expected: SftpHostKeyPin,
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let fingerprint = key.fingerprint(HashAlg::Sha256);
        Ok(key.algorithm().as_str() == self.expected.algorithm
            && fingerprint.sha256() == Some(self.expected.sha256))
    }
}

struct HostKeyDiscovery {
    observed: Arc<Mutex<Option<SftpHostKeyPin>>>,
}

impl client::Handler for HostKeyDiscovery {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let pin = SftpHostKeyPin {
            algorithm: key.algorithm().as_str().to_owned(),
            sha256: key
                .fingerprint(HashAlg::Sha256)
                .sha256()
                .expect("SHA-256-Fingerprint wurde ausdrücklich angefordert"),
        };
        *lock(&self.observed) = Some(pin);
        // Dieser Kanal dient nur der öffentlichen Ermittlung. Es folgt weder eine
        // Authentifizierung noch ein SFTP-Subsystem- oder Dateizugriff.
        Ok(true)
    }
}

pub fn discover_sftp_host_key(host: &str, port: u16) -> io::Result<SftpHostKeyPin> {
    validate_host(host)?;
    if port == 0 {
        return Err(invalid("SFTP-Port 0 ist ungültig"));
    }
    let runtime = RuntimeBuilder::new_current_thread().enable_all().build()?;
    let observed = Arc::new(Mutex::new(None));
    let handler = HostKeyDiscovery {
        observed: observed.clone(),
    };
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    // Dieselbe Frist wie beim eigentlichen Verbindungsaufbau: „Hostkey ermitteln"
    // ist der erste Klick im Profileditor und trifft naturgemäss oft einen
    // vertippten oder gefilterten Host.
    let handle = runtime.block_on(async {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(
                Arc::new(client::Config::default()),
                (normalized, port),
                handler,
            ),
        )
        .await
        {
            Ok(result) => result.map_err(ssh_error),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "SFTP-Hostkey-Ermittlung überschritt {} Sekunden",
                    CONNECT_TIMEOUT.as_secs()
                ),
            )),
        }
    })?;
    drop(handle);
    let pin = lock(&observed).clone().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "SSH-Server lieferte keinen Hostkey",
        )
    })?;
    pin.validate()?;
    Ok(pin)
}

/// Prüft einen bereits entdeckten Hostkey gegen OpenSSHs `known_hosts`, inklusive
/// gehashter Hostnamen. Ein vorhandener Schlüssel desselben Algorithmus mit anderem
/// Digest ist ein harter Fehler; ein vollständig unbekannter Host liefert `false`.
pub fn sftp_host_key_is_known(
    host: &str,
    port: u16,
    expected: &SftpHostKeyPin,
) -> io::Result<bool> {
    validate_host(host)?;
    expected.validate()?;
    let keys = russh::keys::known_hosts::known_host_keys(host, port).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "OpenSSH-known_hosts konnte nicht sicher ausgewertet werden",
        )
    })?;
    let mut same_algorithm = false;
    for (_, key) in keys {
        if key.algorithm().as_str() != expected.algorithm {
            continue;
        }
        same_algorithm = true;
        if key.fingerprint(HashAlg::Sha256).sha256() == Some(expected.sha256) {
            return Ok(true);
        }
    }
    if same_algorithm {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SFTP-Hostkey widerspricht OpenSSH-known_hosts",
        ));
    }
    Ok(false)
}

pub fn list_sftp_agent_keys() -> io::Result<Vec<SftpPublicKeyIdentity>> {
    let runtime = RuntimeBuilder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut agent = AgentClient::connect_env()
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::NotConnected, error.to_string()))?;
        let identities = agent
            .request_identities()
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
        identities
            .into_iter()
            .map(|identity| {
                let public = identity.public_key();
                let sha256 = public
                    .fingerprint(HashAlg::Sha256)
                    .sha256()
                    .expect("SHA-256-Fingerprint wurde ausdrücklich angefordert");
                Ok(SftpPublicKeyIdentity {
                    algorithm: public.algorithm().as_str().to_owned(),
                    sha256,
                    comment: identity.comment().to_owned(),
                })
            })
            .collect()
    })
}

struct Connection {
    // Der SSH-Handle hält Transport und Authentifizierung für den SFTP-Kanal am Leben.
    _ssh: client::Handle<HostKeyVerifier>,
    sftp: Arc<SftpSession>,
    read_session: Arc<RawSftpSession>,
    identity: SftpServerIdentity,
}

struct ProviderState {
    connection: Connection,
    generation: u64,
    capabilities: RemoteCapabilities,
}

/// Ein an genau einen kanonischen Server-Root gebundenes SFTP-Dateisystem.
///
/// Pfade an der VFS-Grenze sind virtuelle absolute POSIX-Pfade (`/ordner/datei`).
/// `..`, Plattformpräfixe und nicht-UTF-8-kodierbare Komponenten werden abgelehnt,
/// bevor sie den Server erreichen.
pub struct SftpVfs {
    runtime: Arc<Runtime>,
    config: SftpConnectionConfig,
    secrets: Arc<dyn SecretResolver>,
    state: Mutex<ProviderState>,
    reconnect: Mutex<()>,
    lost_interactive_read_generation: AtomicU64,
}

/// Sicherheitsklasse des interaktiven SFTP-Listings.
///
/// SFTP v3 öffnet Verzeichnisse ausschließlich über Pfadtexte. Ein nach LSTAT und
/// REALPATH ausgetauschter Elternordner kann deshalb beim folgenden READDIR auf ein
/// anderes Ziel zeigen. Diese Klasse darf nur die interaktive Pane-Anzeige speisen;
/// Planner, Sync, Quarantäne und Root-Identitäten verwenden sie ausdrücklich nicht.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpReadSecurity {
    SftpPathBestEffort,
}

/// Ein interaktives Pane-Listing samt Generation aus demselben gehaltenen State.
/// Die Generation verhindert eine Vermischung mit einem parallelen Reconnect, ist
/// aber ausdrücklich kein Namespace- oder Identitätsbeleg.
#[derive(Clone, Debug)]
pub struct SftpInteractiveDirectoryListing {
    pub entries: Vec<crate::remote_real_mode::ProviderInteractiveEntry>,
    pub omitted: Vec<crate::InteractiveListingOmission>,
    pub session_generation: u64,
    pub security: SftpReadSecurity,
}

impl SftpVfs {
    pub fn connect(
        config: SftpConnectionConfig,
        secrets: Arc<dyn SecretResolver>,
    ) -> io::Result<Self> {
        config.validate()?;
        let runtime = Arc::new(
            RuntimeBuilder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("doppeldecker-sftp")
                .build()?,
        );
        let connection = runtime.block_on(connect_once_bounded(&config, secrets.as_ref()))?;
        let capabilities = initial_capabilities();
        Ok(Self {
            runtime,
            config,
            secrets,
            state: Mutex::new(ProviderState {
                connection,
                generation: 1,
                capabilities,
            }),
            reconnect: Mutex::new(()),
            lost_interactive_read_generation: AtomicU64::new(0),
        })
    }

    pub fn identity(&self) -> SftpServerIdentity {
        lock(&self.state).connection.identity.clone()
    }

    /// Dauerhafte Kontobindung ohne Benutzername oder Secret im Paarmanifest.
    pub fn account_digest(&self) -> [u8; 32] {
        *blake3::hash(self.config.username.as_bytes()).as_bytes()
    }

    pub fn capabilities(&self) -> RemoteCapabilities {
        lock(&self.state).capabilities
    }

    fn require_entry_delete(&self) -> io::Result<()> {
        if self.capabilities().permits_entry_delete() {
            Ok(())
        } else {
            Err(capability_probe_required(
                "SFTP-Löschen wurde für diese Sitzung nicht getrennt geprüft",
            ))
        }
    }

    pub fn session_generation(&self) -> u64 {
        lock(&self.state).generation
    }

    /// Listet ausschließlich für die interaktive Pane-Anzeige. SFTP v3 besitzt
    /// weder handle-relatives READDIR noch ein serverseitiges No-Follow für jede
    /// Pfadkomponente. Vorprüfungen reduzieren erkennbare Fehlbindungen, schließen
    /// den Austausch direkt vor READDIR aber nicht. Der typisierte Rückgabewert
    /// verhindert, dass dieses Ergebnis als vollständiger Planner-Beleg gilt.
    pub fn list_interactive_best_effort(
        &self,
        path: &Path,
    ) -> io::Result<SftpInteractiveDirectoryListing> {
        let components = virtual_components(path)?;
        let bounded =
            crate::Cancel::new().until(std::time::Instant::now() + Duration::from_secs(60));
        self.reconnect_lost_interactive_read()?;
        let state = lock(&self.state);
        let session = state.connection.sftp.clone();
        let identity = state.connection.identity.clone();
        let generation = state.generation;
        let raw = state.connection.read_session.clone();
        drop(state);
        let read = self.runtime.block_on(async {
            let remote = verify_sftp_directory_chain(
                &session,
                &identity.canonical_root,
                &components,
                Some(&bounded),
            )
            .await?;
            collect_interactive_entries(
                read_sftp_display_entries(self, generation, &raw, &remote, &bounded).await?,
            )
        });
        let read = self.record_interactive_read_failure(generation, read);
        let (entries, omitted) = read.map_err(|error| {
            if bounded.is_cancelled() {
                crate::coded_error(
                    io::ErrorKind::Unsupported,
                    crate::ErrorCode::Unsupported,
                    "SFTP-Anzeigelisting erreichte die Lesefrist von 60 Sekunden",
                )
            } else {
                error
            }
        })?;
        let current = lock(&self.state);
        finish_interactive_listing(
            entries,
            omitted,
            generation,
            &identity,
            current.generation,
            &current.connection.identity,
        )
    }

    fn record_interactive_read_failure<T>(
        &self,
        generation: u64,
        result: io::Result<T>,
    ) -> io::Result<T> {
        if result
            .as_ref()
            .is_err_and(|error| crate::error_code(error) == crate::ErrorCode::ConnectionLost)
        {
            let state = lock(&self.state);
            if state.generation == generation {
                self.lost_interactive_read_generation
                    .store(generation, Ordering::SeqCst);
            }
        }
        result
    }

    pub(crate) fn mark_quarantine_verified(
        &self,
        expected_generation: u64,
    ) -> io::Result<RemoteCapabilities> {
        let mut state = lock(&self.state);
        if state.generation != expected_generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "SFTP-Sitzung wechselte während der Quarantäneprobe",
            ));
        }
        if !state.capabilities.permits_add_only()
            || !state.capabilities.permits_directory_create()
            || !state.capabilities.permits_entry_delete()
            || !state.capabilities.atomic_noreplace_rename
            || !state.capabilities.atomic_replace_rename
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SFTP-Quarantäne benötigt getrennt geprüfte Lösch- sowie AddOnly-/Replace-Fähigkeiten für ihr Manifest",
            ));
        }
        state.capabilities.quarantine = true;
        Ok(state.capabilities)
    }

    /// Stellt eine abgebrochene Sitzung neu her. Hostkey, kanonischer Root,
    /// Protokollversion und Erweiterungen müssen exakt mit der ersten Sitzung
    /// übereinstimmen; andernfalls bleibt die alte Sitzung unverändert erhalten.
    pub fn reconnect(&self) -> io::Result<()> {
        let _reconnect = lock(&self.reconnect);
        self.reconnect_locked()
    }

    fn reconnect_locked(&self) -> io::Result<()> {
        let expected = self.identity();
        let connection = self
            .runtime
            .block_on(connect_once_bounded(&self.config, self.secrets.as_ref()))?;
        if connection.identity != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SFTP-Identität änderte sich beim Reconnect",
            ));
        }
        let mut state = lock(&self.state);
        state.connection = connection;
        state.generation = state.generation.saturating_add(1);
        // Verhaltenstests gelten nur für die konkrete Sitzung. Nach einem Reconnect
        // müssen exklusive Create-/Rename-Garantien erneut bewiesen werden.
        state.capabilities = initial_capabilities();
        self.lost_interactive_read_generation
            .store(0, Ordering::SeqCst);
        Ok(())
    }

    fn reconnect_lost_interactive_read(&self) -> io::Result<()> {
        let _reconnect = lock(&self.reconnect);
        if self.lost_interactive_read_generation.load(Ordering::SeqCst) == self.session_generation()
        {
            self.reconnect_locked()?;
        }
        Ok(())
    }

    /// Prüft ausschließlich die für interaktives `mkdir` nötige Semantik. Die
    /// Probe hängt weder an Datei-Upload noch Replace, Quarantäne oder Mirror.
    pub fn probe_directory_create_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_directory_create_capability_at(Path::new("/"))
    }

    pub fn probe_directory_create_capability_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        self.passive_capabilities_at(probe_root)
    }

    /// SFTP v3 besitzt kein bedingtes REMOVE/RMDIR, das die zuvor gelesene Identität
    /// serverseitig bindet. Eine künstliche Löschprobe könnte deshalb zwischen
    /// Readback und Cleanup fremde Daten treffen und bleibt vollständig lesend.
    pub fn probe_delete_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_delete_capability_at(Path::new("/"))
    }

    pub fn probe_delete_capability_at(&self, probe_root: &Path) -> io::Result<RemoteCapabilities> {
        self.passive_capabilities_at(probe_root)
    }

    /// Die starke No-Replace-Eigenschaft lässt sich ohne sichere Entfernung der
    /// Probeobjekte nicht mutierend testen. Auch interaktive Umbenennung bleibt
    /// ohne elterngebundene Serverprimitive gesperrt.
    pub fn probe_rename_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_rename_capability_at(Path::new("/"))
    }

    pub fn probe_rename_capability_at(&self, probe_root: &Path) -> io::Result<RemoteCapabilities> {
        self.passive_capabilities_at(probe_root)
    }

    /// SFTP-v3-Pfadmutationen besitzen keine atomare Elternhandle-Bindung. Auch eine
    /// interaktive Best-Effort-Wirkung darf deshalb ohne eine spätere, stärkere
    /// Serverprimitive keine mutierende Probe freischalten.
    pub fn probe_add_only_capabilities(&self) -> io::Result<RemoteCapabilities> {
        self.probe_add_only_capabilities_at(Path::new("/"))
    }

    pub fn probe_add_only_capabilities_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        self.passive_capabilities_at(probe_root)
    }

    fn passive_capabilities_at(&self, probe_root: &Path) -> io::Result<RemoteCapabilities> {
        // Die Abfrage meldet ausschließlich die bereits bekannte Negativmatrix.
        // Ein pfadbasiertes Listing wäre selbst als „Probe" kein Sicherheitsbeleg.
        virtual_components(probe_root)?;
        Ok(self.capabilities())
    }
}

fn append_sftp_component(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

/// Interaktive Leser prüfen jede beobachtbare Komponente per LSTAT und REALPATH
/// in derselben Sitzung. Das verhindert wissentlich verfolgte Symlinks, bietet
/// aber keine atomare Rootbindung. Inhaltsleser prüfen ihren Abbruchtoken zwischen
/// den Requests; sie müssen nach Abbruch keine weitere Elternkette abwarten.
async fn verify_sftp_directory_chain(
    session: &SftpSession,
    canonical_root: &str,
    components: &[&str],
    cancel: Option<&crate::Cancel>,
) -> io::Result<String> {
    check_sftp_cancel(cancel)?;
    let real_root = session
        .canonicalize(canonical_root.to_owned())
        .await
        .map_err(sftp_error)?;
    check_sftp_cancel(cancel)?;
    if real_root != canonical_root {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Gebundener SFTP-Root wechselte seine REALPATH-Identität",
        ));
    }
    let root = session
        .symlink_metadata(canonical_root.to_owned())
        .await
        .map_err(sftp_error)?;
    check_sftp_cancel(cancel)?;
    if !root.is_dir() || root.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Gebundener SFTP-Root ist kein echtes Verzeichnis mehr",
        ));
    }

    let mut current = canonical_root.to_owned();
    for component in components {
        check_sftp_cancel(cancel)?;
        current = append_sftp_component(&current, component);
        let metadata = session
            .symlink_metadata(current.clone())
            .await
            .map_err(sftp_error)?;
        check_sftp_cancel(cancel)?;
        if !metadata.is_dir() || metadata.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "SFTP-Pfad enthält einen Symlink oder Nichtordner und bleibt gesperrt: {current}"
                ),
            ));
        }
        let real = session
            .canonicalize(current.clone())
            .await
            .map_err(sftp_error)?;
        check_sftp_cancel(cancel)?;
        let inside_root = if canonical_root == "/" {
            real.starts_with('/')
        } else {
            real == canonical_root
                || real
                    .strip_prefix(canonical_root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        };
        if real != current || !inside_root {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "SFTP-Elternpfad wechselte per REALPATH auf eine andere Identität oder außerhalb des Roots: {current}"
                ),
            ));
        }
    }
    Ok(current)
}

fn check_sftp_cancel(cancel: Option<&crate::Cancel>) -> io::Result<()> {
    if cancel.is_some_and(crate::Cancel::is_cancelled) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "SFTP-Lesen wurde abgebrochen",
        ))
    } else {
        Ok(())
    }
}

// Rohdaten bleiben hier Datenwerte. Erst collect_interactive_entries entscheidet,
// welche Namen und Typen bedienbar sind; kein Name wird dabei als Pfad geöffnet.
async fn read_sftp_display_entries(
    provider: &SftpVfs,
    generation: u64,
    raw: &RawSftpSession,
    remote: &str,
    cancel: &crate::Cancel,
) -> io::Result<Vec<(String, FileAttributes)>> {
    let mut result = directory_read::read_records(
        raw,
        remote,
        cancel,
        directory_read::DirectoryReadLimits {
            entries: 100_000,
            name_bytes: 16 * 1024 * 1024,
            pages: 100_003,
            duplicate_error_kind: io::ErrorKind::InvalidData,
        },
        |result| provider.record_interactive_read_failure(generation, result),
    )
    .await?;
    result.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(result)
}

fn finish_interactive_listing(
    entries: Vec<crate::remote_real_mode::ProviderInteractiveEntry>,
    omitted: Vec<crate::InteractiveListingOmission>,
    generation: u64,
    identity: &SftpServerIdentity,
    current_generation: u64,
    current_identity: &SftpServerIdentity,
) -> io::Result<SftpInteractiveDirectoryListing> {
    if current_generation != generation || current_identity != identity {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "SFTP-Sitzung oder Root-Identität wechselte während des interaktiven Listings",
        ));
    }
    Ok(SftpInteractiveDirectoryListing {
        entries,
        omitted,
        session_generation: generation,
        security: SftpReadSecurity::SftpPathBestEffort,
    })
}

fn unsupported_sftp_directory_create(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "SFTP v3 kann MKDIR nicht atomar an den zuvor geprüften Elternordner und Root binden; Ordneranlage bleibt gesperrt: {}",
            path.display()
        ),
    )
}

fn reject_unbound_sftp_mutation<T>(action: &str, paths: &[&Path]) -> io::Result<T> {
    for path in paths {
        virtual_components(path)?;
    }
    let paths = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" → ");
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "SFTP v3 kann {action} nicht atomar an den gebundenen Pane-Root und Elternordner binden; Mutation bleibt gesperrt: {paths}"
        ),
    ))
}

fn reject_unbound_sftp_read<T>(action: &str, paths: &[&Path]) -> io::Result<T> {
    let rendered = paths
        .iter()
        .map(|path| {
            virtual_components(path)?;
            normalize_virtual_path(path)
        })
        .collect::<io::Result<Vec<_>>>()?;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "SFTP-v3-{action} bleibt für Sicherheitsplanung und Dateiinhalte gesperrt: Der Server bindet Pfadkomponenten nicht atomar an einen gehaltenen Root; Pfad/Pfade {}",
            rendered.join(", ")
        ),
    ))
}

impl Vfs for SftpVfs {
    fn safety_capabilities_digest(&self) -> io::Result<Option<[u8; 32]>> {
        crate::remote::capabilities_digest(self.capabilities()).map(Some)
    }

    fn reclaim_verification_session(&self) -> io::Result<()> {
        // Nur für die lesende Zustandsklärung nach einer verlorenen Rename-Antwort.
        // `reconnect` bindet denselben Hostkey, Root und Capability-Digest neu und
        // setzt alle sitzungsgebundenen Mutationsbelege zurück.
        self.reconnect()
    }

    fn file_commit_class(&self) -> crate::vfs::FileCommitClass {
        // Schreiben ist bereits am Provider-Gate gesperrt. `Assured` verhindert,
        // dass ein Aufrufer aus einer nur nominalen Best-Effort-Klasse eine
        // tatsächlich nicht vorhandene SFTP-Mutation ableitet.
        crate::vfs::FileCommitClass::Assured
    }

    fn supports_mode_write(&self) -> bool {
        false
    }

    fn supports_mtime_write(&self) -> bool {
        false
    }

    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        reject_unbound_sftp_read("LIST", &[path])
    }

    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        reject_unbound_sftp_read("vollständiges Planner-Listing", &[path])
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        reject_unbound_sftp_read("LSTAT", &[path])
    }

    fn exists(&self, path: &Path) -> bool {
        virtual_components(path).is_ok_and(|_| false)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        reject_unbound_sftp_mutation("MKDIR", &[path])
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let components = virtual_components(path)?;
        if components.is_empty() {
            return Err(invalid(
                "SFTP-mkdir darf den gebundenen Root nicht neu anlegen",
            ));
        }
        // SFTP v3 besitzt keine Operation, die den geprüften Elternordner bis zum
        // MKDIR hält. Ein sichtbarer Best-Effort-Weg wäre hier unehrlich: Ein
        // Symlink-Tausch kann die Wirkung aus dem gewählten Root hinaus verschieben.
        Err(unsupported_sftp_directory_create(path))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        reject_unbound_sftp_read("OPEN READ", &[path])
    }

    fn open_write(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SFTP-Overwrite ist vor dem Update-Vertrag gesperrt",
        ))
    }

    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        reject_unbound_sftp_mutation("OPEN CREATE/WRITE", &[path])
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        reject_unbound_sftp_mutation("OPEN/fsync", &[path])
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        reject_unbound_sftp_read("Namespace-Readback", &[path])
    }

    fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
        let _ = mtime;
        reject_unbound_sftp_mutation("SETSTAT mtime", &[path])
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        let _ = mode;
        reject_unbound_sftp_mutation("SETSTAT mode", &[path])
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

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.require_entry_delete()?;
        reject_unbound_sftp_mutation("REMOVE", &[path])
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.require_entry_delete()?;
        reject_unbound_sftp_mutation("rekursives REMOVE/RMDIR", &[path])
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.require_entry_delete()?;
        reject_unbound_sftp_mutation("RMDIR", &[path])
    }

    fn remove_move_leaf_best_effort(&self, path: &Path) -> io::Result<()> {
        reject_unbound_sftp_mutation("Best-Effort-REMOVE", &[path])
    }

    fn remove_move_empty_dir_best_effort(&self, path: &Path) -> io::Result<()> {
        reject_unbound_sftp_mutation("Best-Effort-RMDIR", &[path])
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        reject_unbound_sftp_mutation("Replace-RENAME", &[from, to])
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        reject_unbound_sftp_mutation("No-Replace-RENAME", &[from, to])
    }

    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        reject_unbound_sftp_mutation("Standard-RENAME", &[from, to])
    }

    fn available_bytes(&self, path: &Path) -> io::Result<u64> {
        reject_unbound_sftp_read("STATVFS", &[path])
    }

    fn root_identity(&self, path: &Path) -> io::Result<crate::real_mode::RootIdentity> {
        reject_unbound_sftp_read("Root-Identität", &[path])
    }
}
// SFTP-VFS-BLOCK-ENDE — der Guard-Test unten schneidet die letzte Methode an dieser
// Marke ab. Sie darf nur einmal im Produktionsteil vorkommen.

/// Harte Frist für den gesamten SFTP-Verbindungsaufbau.
///
/// SFTP war das einzige Protokoll ohne diese Grenze; FTPS, WebDAV und SMB haben je
/// dieselben 15 Sekunden. Ein vertippter oder gefilterter Host liess die Oberfläche
/// deshalb bis zum TCP-Default des Systems stehen — auf macOS rund 75 Sekunden —,
/// und ein Reconnect nach einem Netzwechsel konnte diese Wartezeit verdoppeln.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Bindet Verbindungsaufbau, Schlüsseltausch, Anmeldung und SFTP-Init gemeinsam an
/// [`CONNECT_TIMEOUT`]. Jede dieser Phasen kann hängen, nicht nur die TCP-Verbindung.
async fn connect_once_bounded(
    config: &SftpConnectionConfig,
    secrets: &dyn SecretResolver,
) -> io::Result<Connection> {
    match tokio::time::timeout(CONNECT_TIMEOUT, connect_once(config, secrets)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "SFTP-Verbindungsaufbau überschritt {} Sekunden",
                CONNECT_TIMEOUT.as_secs()
            ),
        )),
    }
}

async fn connect_once(
    config: &SftpConnectionConfig,
    secrets: &dyn SecretResolver,
) -> io::Result<Connection> {
    let ssh_config = Arc::new(client::Config::default());
    let mut ssh = connect_transport(config, ssh_config).await?;
    authenticate(&mut ssh, config, secrets).await?;

    let probe_channel = ssh.channel_open_session().await.map_err(ssh_error)?;
    probe_channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(ssh_error)?;
    let raw = Arc::new(RawSftpSession::new(probe_channel.into_stream()));
    raw.set_timeout(15);
    let version = raw.init().await.map_err(sftp_error)?;
    let extensions_digest = hash_extensions(&version.extensions);
    let channel = ssh.channel_open_session().await.map_err(ssh_error)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(ssh_error)?;
    let sftp = Arc::new(
        SftpSession::new(channel.into_stream())
            .await
            .map_err(sftp_error)?,
    );
    let canonical_root = sftp
        .canonicalize(&config.requested_root)
        .await
        .map_err(sftp_error)?;
    let identity = SftpServerIdentity {
        host: config.normalized_host(),
        port: config.port,
        host_key: config.host_key.clone(),
        canonical_root,
        sftp_version: version.version,
        extensions_digest,
    };
    identity.validate()?;
    let root_metadata = sftp
        .symlink_metadata(&identity.canonical_root)
        .await
        .map_err(sftp_error)?;
    if !root_metadata.is_dir() || root_metadata.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kanonischer SFTP-Root ist kein echtes Verzeichnis",
        ));
    }
    Ok(Connection {
        _ssh: ssh,
        sftp,
        read_session: raw,
        identity,
    })
}

async fn connect_transport(
    config: &SftpConnectionConfig,
    ssh_config: Arc<client::Config>,
) -> io::Result<client::Handle<HostKeyVerifier>> {
    connect_candidate_hosts(
        config.candidate_hosts().map(str::to_owned).collect(),
        CONNECT_TIMEOUT,
        |host| {
            let verifier = HostKeyVerifier {
                expected: config.host_key.clone(),
            };
            client::connect(
                ssh_config.clone(),
                (host.trim_end_matches('.').to_ascii_lowercase(), config.port),
                verifier,
            )
        },
    )
    .await
}

async fn connect_candidate_hosts<T, F, Fut>(
    hosts: Vec<String>,
    timeout: Duration,
    mut connect: F,
) -> io::Result<T>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<T, russh::Error>>,
{
    let mut last = None;
    // Jeder Kandidat erhält einen Anteil der bestehenden Gesamtfrist. Sonst
    // verbraucht ein gefilterter Primärhost sie vollständig vor der Alternative.
    let candidate_timeout = timeout / u32::try_from(hosts.len().max(1)).unwrap_or(u32::MAX);
    for host in hosts {
        let result = tokio::time::timeout(candidate_timeout, connect(host))
            .await
            .unwrap_or(Err(russh::Error::ConnectionTimeout));
        match result {
            Ok(handle) => return Ok(handle),
            Err(error) if retry_alternate_host(&error) => last = Some(error),
            Err(error) => return Err(ssh_error(error)),
        }
    }
    Err(last
        .map(ssh_error)
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SFTP-Endpunkt fehlt")))
}

fn retry_alternate_host(error: &russh::Error) -> bool {
    matches!(
        error,
        russh::Error::IO(_)
            | russh::Error::Disconnect
            | russh::Error::HUP
            | russh::Error::ConnectionTimeout
            | russh::Error::KeepaliveTimeout
            | russh::Error::InactivityTimeout
    )
}

async fn authenticate(
    ssh: &mut client::Handle<HostKeyVerifier>,
    config: &SftpConnectionConfig,
    secrets: &dyn SecretResolver,
) -> io::Result<()> {
    let success = match &config.authentication {
        SftpAuthenticationReference::Password { secret } => {
            let password = secrets.resolve(secret)?;
            ssh.authenticate_password(&config.username, password.expose())
                .await
                .map_err(ssh_error)?
                .success()
        }
        SftpAuthenticationReference::PrivateKey {
            path,
            passphrase,
            public_key_sha256,
        } => {
            let passphrase = passphrase
                .as_ref()
                .map(|reference| secrets.resolve(reference))
                .transpose()?;
            let key = load_secret_key(path, passphrase.as_ref().map(SecretString::expose))
                .map_err(|error| {
                    io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
                })?;
            require_public_key(
                &key.public_key().fingerprint(HashAlg::Sha256),
                public_key_sha256,
            )?;
            let hash = ssh
                .best_supported_rsa_hash()
                .await
                .map_err(ssh_error)?
                .flatten();
            ssh.authenticate_publickey(
                &config.username,
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .map_err(ssh_error)?
            .success()
        }
        SftpAuthenticationReference::SshAgent { public_key_sha256 } => {
            let mut agent = AgentClient::connect_env()
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::NotConnected, error.to_string()))?;
            let identities = agent.request_identities().await.map_err(|error| {
                io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
            })?;
            let public = identities
                .iter()
                .map(|identity| identity.public_key().into_owned())
                .find(|key| key.fingerprint(HashAlg::Sha256).sha256() == Some(*public_key_sha256))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "Gebundener öffentlicher Schlüssel fehlt im SSH-Agenten",
                    )
                })?;
            let hash = ssh
                .best_supported_rsa_hash()
                .await
                .map_err(ssh_error)?
                .flatten();
            ssh.authenticate_publickey_with(&config.username, public, hash, &mut agent)
                .await
                .map_err(|error| {
                    io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
                })?
                .success()
        }
    };
    if !success {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SFTP-Authentifizierung wurde abgelehnt",
        ));
    }
    Ok(())
}

fn initial_capabilities() -> RemoteCapabilities {
    RemoteCapabilities {
        // READDIR ist in SFTP v3 ebenso pfadbasiert wie jede Mutation. Das separat
        // typisierte Pane-Listing darf daher niemals als vollständiger Planner-
        // Beleg oder Grundlage einer Root-Identität erscheinen.
        complete_listing: false,
        // SFTP v3 bindet OPEN/SETSTAT/RENAME/REMOVE ausschließlich an Pfadtexte.
        // Ein zuvor geprüftes Elternverzeichnis kann bis zum Request gegen einen
        // Symlink ausgetauscht werden. Deshalb bleiben auch die früher sichtbaren
        // Best-Effort-Flags aus: Ein Readback erkennt die Außenwirkung nur nachträglich.
        best_effort_file_commit: false,
        best_effort_rename: false,
        // Die Serveransage allein ist kein Beleg: OpenSSH kann die Erweiterung
        // anbieten und den konkreten Request per `internal-sftp -P` verweigern.
        // Ohne sicheren Probe-Cleanup bleibt dauerhaftes Flush stets gesperrt.
        durable_file_flush: false,
        available_space: false,
        ..RemoteCapabilities::default()
    }
}

fn collect_interactive_entries(
    raw: Vec<(String, FileAttributes)>,
) -> io::Result<(
    Vec<crate::ProviderInteractiveEntry>,
    Vec<crate::InteractiveListingOmission>,
)> {
    let mut entries = Vec::new();
    let mut omitted = Vec::new();
    for (name, attributes) in raw {
        let reason = if validate_remote_name(&name).is_err() {
            Some("Kein sicher bedienbarer einzelner Dateiname")
        } else if !attributes.permissions.is_some_and(|mode| {
            matches!(
                mode & 0o170000,
                0o010000 | 0o020000 | 0o040000 | 0o060000 | 0o100000 | 0o120000 | 0o140000
            )
        }) {
            Some("SFTP-Listing liefert keinen belegten Dateityp")
        } else {
            None
        };
        if let Some(reason) = reason {
            omitted.push(crate::InteractiveListingOmission {
                display_name: name.escape_debug().to_string(),
                is_hidden: name.starts_with('.'),
                reason: reason.into(),
            });
        } else {
            entries.push(interactive_entry_from_attributes(name, &attributes)?);
        }
    }
    Ok((entries, omitted))
}

fn interactive_entry_from_attributes(
    name: String,
    attributes: &FileAttributes,
) -> io::Result<crate::remote_real_mode::ProviderInteractiveEntry> {
    Ok(crate::remote_real_mode::ProviderInteractiveEntry {
        entry: entry_from_attributes(name, attributes)?,
        safely_decoded: true,
        size_known: attributes.size.is_some(),
    })
}

fn entry_from_attributes(name: String, attributes: &FileAttributes) -> io::Result<Entry> {
    let file_type = attributes.file_type();
    if attributes.permissions.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SFTP-Eintrag enthält keinen verlässlichen Dateityp",
        ));
    }
    let is_file = file_type.is_file();
    Ok(Entry {
        extension: entry_extension(&name, is_file),
        name,
        is_dir: file_type.is_dir(),
        is_file,
        is_symlink: file_type.is_symlink(),
        size: attributes.size.unwrap_or(0),
        mtime: attributes.mtime.map(i64::from),
        created: None,
        owner: attributes
            .user
            .clone()
            .or_else(|| attributes.uid.map(|value| value.to_string())),
        group: attributes
            .group
            .clone()
            .or_else(|| attributes.gid.map(|value| value.to_string())),
        mode: attributes.permissions.map(|mode| mode & 0o777),
        device: None,
        inode: None,
        hard_links: None,
    })
}

fn normalize_virtual_path(path: &Path) -> io::Result<String> {
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
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| invalid("SFTP-Pfad ist nicht gültiges UTF-8"))?;
                validate_remote_name(value)?;
                result.push(value);
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "SFTP-Pfad darf den gebundenen Root nicht verlassen",
                ));
            }
        }
    }
    Ok(result)
}

fn validate_remote_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SFTP-Server lieferte einen unsicheren Namen",
        ));
    }
    Ok(())
}

fn hash_extensions(extensions: &HashMap<String, String>) -> [u8; 32] {
    let mut sorted = extensions.iter().collect::<Vec<_>>();
    sorted.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"doppeldecker-sftp-extensions-v1");
    for (name, value) in sorted {
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn require_public_key(fingerprint: &Fingerprint, expected: &[u8; 32]) -> io::Result<()> {
    if fingerprint.sha256() == Some(*expected) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Privater Schlüssel passt nicht zur gebundenen öffentlichen Identität",
        ))
    }
}

fn sftp_error(error: russh_sftp::client::error::Error) -> io::Error {
    match &error {
        russh_sftp::client::error::Error::Status(status)
            if status.status_code == StatusCode::NoSuchFile =>
        {
            io::Error::new(io::ErrorKind::NotFound, error.to_string())
        }
        russh_sftp::client::error::Error::Status(status)
            if status.status_code == StatusCode::PermissionDenied =>
        {
            io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
        }
        russh_sftp::client::error::Error::Status(status)
            if matches!(
                status.status_code,
                StatusCode::NoConnection | StatusCode::ConnectionLost
            ) =>
        {
            io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
        }
        russh_sftp::client::error::Error::Timeout => {
            io::Error::new(io::ErrorKind::TimedOut, error.to_string())
        }
        // Ein I/O-Abbruch während einer laufenden SFTP-Anfrage heißt: Der Befehl
        // kann den Server bereits erreicht haben, nur die Antwort ging verloren.
        // Konservativ als Verbindungsverlust (`ConnectionAborted` →
        // `ErrorCode::ConnectionLost`) einstufen, damit die Exakt-einmal-Klärung
        // (identitätsgeprüfter Readback statt blinder Wiederholung) greift. Als
        // `Other` würde der Fehler wie ein gewöhnlicher Fehlschlag aussehen und
        // Aufrufer könnten die Mutation irrtümlich wiederholen.
        // russh-sftp verwendet denselben Variant auch für beendete interne
        // Requestkanäle (verlorener Sender/Empfänger). SFTP-Mutationen sind hier
        // gesperrt; die Einstufung erlaubt ausschließlich frische Leseversuche.
        russh_sftp::client::error::Error::IO(_)
        | russh_sftp::client::error::Error::UnexpectedBehavior(_) => {
            io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
        }
        _ => io::Error::other(error.to_string()),
    }
}

fn ssh_error(error: russh::Error) -> io::Error {
    let kind = match &error {
        russh::Error::UnknownKey
        | russh::Error::WrongServerSig
        | russh::Error::KeyChanged { .. }
        | russh::Error::PacketAuth
        | russh::Error::NotAuthenticated
        | russh::Error::NoAuthMethod
        | russh::Error::UnsupportedAuthMethod => io::ErrorKind::PermissionDenied,
        russh::Error::IO(inner) => inner.kind(),
        russh::Error::ConnectionTimeout
        | russh::Error::KeepaliveTimeout
        | russh::Error::InactivityTimeout => io::ErrorKind::TimedOut,
        russh::Error::Disconnect | russh::Error::HUP | russh::Error::SendError => {
            io::ErrorKind::NotConnected
        }
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error.to_string())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haengender_primaerhost_verhindert_funktionierende_alternative_nicht() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let attempts = Mutex::new(Vec::new());
        runtime.block_on(async {
            let result = tokio::time::timeout(
                Duration::from_millis(200),
                connect_candidate_hosts(
                    vec!["primaer".into(), "alternative".into()],
                    Duration::from_millis(40),
                    |host| {
                        attempts.lock().unwrap().push(host.clone());
                        async move {
                            if host == "primaer" {
                                std::future::pending::<Result<(), russh::Error>>().await
                            } else {
                                Ok(())
                            }
                        }
                    },
                ),
            )
            .await;
            assert!(
                matches!(result, Ok(Ok(()))),
                "Alternative wurde nicht rechtzeitig erreicht: {result:?}"
            );
        });
        assert_eq!(*attempts.lock().unwrap(), ["primaer", "alternative"]);
    }

    #[test]
    fn ssh_identitaetsfehler_versucht_keinen_alternativen_server() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let attempts = Mutex::new(Vec::new());
        let result = runtime.block_on(connect_candidate_hosts(
            vec!["primaer".into(), "alternative".into()],
            Duration::from_secs(1),
            |host| {
                attempts.lock().unwrap().push(host);
                async { Err::<(), _>(russh::Error::UnknownKey) }
            },
        ));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(*attempts.lock().unwrap(), ["primaer"]);
    }

    #[test]
    fn ssh_identitaetsfehler_sind_keine_verbindungsverluste() {
        for error in [
            russh::Error::UnknownKey,
            russh::Error::WrongServerSig,
            russh::Error::KeyChanged { line: 1 },
        ] {
            let error = ssh_error(error);
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_ne!(crate::error_code(&error), crate::ErrorCode::ConnectionLost);
        }
        assert_eq!(
            crate::error_code(&ssh_error(russh::Error::ConnectionTimeout)),
            crate::ErrorCode::ConnectionLost
        );
    }

    /// SFTP war das einzige Protokoll ohne Frist für den Verbindungsaufbau; FTPS,
    /// WebDAV und SMB haben je dieselben 15 Sekunden. Ohne sie stand die Oberfläche
    /// bei einem vertippten oder gefilterten Host bis zum TCP-Default des Systems.
    #[test]
    fn jeder_verbindungsaufbau_traegt_die_frist() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));

        let quelle = include_str!("sftp.rs");
        // Zusammengesetzt, damit die Suchzeichenkette nicht selbst als Treffer zählt.
        let ungebunden = format!("connect_{}(config", "once");
        let treffer: Vec<&str> = quelle
            .lines()
            .filter(|zeile| zeile.contains(&ungebunden))
            .collect();
        assert_eq!(
            treffer.len(),
            1,
            "connect_once darf nur aus connect_once_bounded aufgerufen werden, gefunden: {treffer:?}"
        );
        assert!(
            treffer[0].contains("tokio::time::timeout"),
            "der einzige Aufruf muss unter der Frist stehen: {}",
            treffer[0]
        );
        assert!(
            quelle.contains("SFTP-Hostkey-Ermittlung überschritt"),
            "auch die Hostkey-Ermittlung braucht die Frist"
        );
    }

    #[test]
    fn sicherheitskritische_lesewege_starten_keine_ungebundene_sftp_operation() {
        fn method<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
            let start = source.find(start).expect("VFS-Methode fehlt");
            let rest = &source[start..];
            let end = rest.find(end).expect("nächste VFS-Methode fehlt");
            &rest[..end]
        }

        // Nur der Produktionsteil zählt. Sonst findet ein Marker, den es im Code gar
        // nicht mehr gibt, sich selbst in einem Testliteral weiter unten: Das
        // Schnittfenster wurde dann mehrere hundert Zeilen lang und der Assert griff
        // nur noch zufällig, weil zufällig genau ein passender Aufruf darin lag.
        let full = include_str!("sftp.rs");
        let source = &full[..full
            .find("#[cfg(test)]\nmod tests {")
            .expect("Testmodul fehlt")];
        for (start, end) in [
            (
                "fn list(&self, path: &Path)",
                "fn list_complete(&self, path: &Path)",
            ),
            (
                "fn list_complete(&self, path: &Path)",
                "fn stat(&self, path: &Path)",
            ),
            (
                "fn stat(&self, path: &Path)",
                "fn exists(&self, path: &Path)",
            ),
            (
                "fn open_read(&self, path: &Path)",
                "fn open_write(&self, _path: &Path)",
            ),
            (
                "fn sync_dir(&self, path: &Path)",
                "fn set_mtime(&self, path: &Path",
            ),
            (
                "fn available_bytes(&self, path: &Path)",
                "fn root_identity(&self, path: &Path)",
            ),
            (
                "fn root_identity(&self, path: &Path)",
                "// SFTP-VFS-BLOCK-ENDE",
            ),
        ] {
            let body = method(source, start, end);
            assert!(
                body.contains("reject_unbound_sftp_read"),
                "{start} gab eine pfadbasierte SFTP-v3-Lesewirkung als streng gebunden aus"
            );
        }
    }

    #[test]
    fn pfadbasierte_mutation_laeuft_nie_nach_elterntausch_im_hook() {
        let outside_was_mutated = std::cell::Cell::new(false);

        let result = reject_unbound_sftp_mutation::<()>(
            "OPEN/SETSTAT/RENAME/REMOVE",
            &[Path::new("/pane/ausgetauscht/ziel")],
        );
        if result.is_ok() {
            outside_was_mutated.set(true);
        }
        let error =
            result.expect_err("Ohne atomare Elternbindung darf der Mutationshook nie laufen");

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(!outside_was_mutated.get());
    }

    #[test]
    fn sftp_failure_ist_kein_eindeutiger_kollisionscode() {
        let error = russh_sftp::client::error::Error::Status(russh_sftp::protocol::Status {
            id: 7,
            status_code: StatusCode::Failure,
            error_message: "generischer Serverfehler".to_owned(),
            language_tag: String::new(),
        });
        assert_eq!(sftp_error(error).kind(), io::ErrorKind::Other);
    }

    #[test]
    fn io_abbruch_waehrend_einer_anfrage_zaehlt_als_verbindungsverlust() {
        // Regression: Ein Antwortverlust nach bereits gesendeter Mutation muss die
        // Exakt-einmal-Klärung erreichen (`ErrorCode::ConnectionLost`), statt als
        // gewöhnlicher Fehler eine blinde Wiederholung zu erlauben.
        let error = sftp_error(russh_sftp::client::error::Error::IO(
            "Kanal wurde geschlossen".into(),
        ));
        assert_eq!(
            crate::error::error_code(&error),
            crate::error::ErrorCode::ConnectionLost
        );
    }

    #[test]
    fn virtuelle_pfade_bleiben_normalisiert_ohne_elternausbruch() {
        assert_eq!(
            normalize_virtual_path(Path::new("/ordner/./datei.txt")).unwrap(),
            "/ordner/datei.txt"
        );
        assert!(normalize_virtual_path(Path::new("../ausbruch")).is_err());
    }

    #[test]
    fn geheimnis_wird_in_debug_immer_maskiert() {
        let secret = SecretString::new("nicht-ausgeben".into()).unwrap();
        let debug = format!("{secret:?}");
        assert_eq!(debug, "SecretString([MASKIERT])");
        assert!(!debug.contains("nicht-ausgeben"));
    }

    #[test]
    fn erweiterungsdigest_ist_reihenfolgeunabhaengig() {
        let a = HashMap::from([
            ("fsync@openssh.com".into(), "1".into()),
            ("statvfs@openssh.com".into(), "2".into()),
        ]);
        let b = HashMap::from([
            ("statvfs@openssh.com".into(), "2".into()),
            ("fsync@openssh.com".into(), "1".into()),
        ]);
        assert_eq!(hash_extensions(&a), hash_extensions(&b));
    }

    #[test]
    fn fsync_ansage_allein_schaltet_keine_dauerhaftigkeit_frei() {
        let capabilities = initial_capabilities();
        assert!(!capabilities.durable_file_flush);
        assert!(!capabilities.permits_add_only());
    }

    #[test]
    fn sftp_v3_startet_ohne_ungebundene_mutationsfaehigkeit() {
        let capabilities = initial_capabilities();

        assert!(!capabilities.complete_listing);
        assert!(!capabilities.best_effort_file_commit);
        assert!(!capabilities.best_effort_rename);
        assert!(!capabilities.exclusive_directory_create);
        assert!(!capabilities.exclusive_create);
        assert!(!capabilities.atomic_noreplace_rename);
        assert!(!capabilities.atomic_replace_rename);
        assert!(!capabilities.entry_delete);
        assert!(!capabilities.quarantine);
        assert!(!capabilities.permits_directory_create());
        assert!(!capabilities.permits_add_only());
        assert!(!capabilities.permits_entry_delete());
    }

    #[test]
    fn lesende_faehigkeitsabfrage_schaltet_keine_starke_faehigkeit_frei() {
        let capabilities = initial_capabilities();
        assert!(!capabilities.complete_listing);
        assert!(!capabilities.permits_add_only());
        assert!(!capabilities.permits_entry_delete());
    }

    #[test]
    fn eintrag_ohne_dateityp_stoppt_fail_closed() {
        let attributes = FileAttributes::empty();
        assert!(entry_from_attributes("unbekannt".into(), &attributes).is_err());
    }

    #[test]
    fn sftp_namen_mit_steuerzeichen_bleiben_aus_listing_und_pfad() {
        for name in ["zeile\nneu", "spalte\tneu", "escape\u{1b}name"] {
            let error = validate_remote_name(name)
                .expect_err("Steuerzeichen dürfen keinen SFTP-Namen bilden");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn interaktives_listing_bilanziert_auch_bestand_ohne_bedienbare_eintraege() {
        let mut file = FileAttributes::empty();
        file.set_regular(true);
        let (entries, omitted) = collect_interactive_entries(vec![
            ("schlecht/Name".into(), file.clone()),
            (".ohneTyp".into(), FileAttributes::empty()),
        ])
        .unwrap();
        assert!(entries.is_empty());
        assert_eq!(omitted.len(), 2);
        assert!(omitted[1].is_hidden);
        assert!(!omitted[0].is_hidden);
        file.permissions = Some(0o644);
        let (entries, omitted) =
            collect_interactive_entries(vec![("RechteOhneTyp".into(), file)]).unwrap();
        assert!(entries.is_empty());
        assert_eq!(omitted.len(), 1);
    }

    #[test]
    fn interaktive_dateigroesse_unterscheidet_unbekannt_von_leer() {
        let mut attributes = FileAttributes::empty();
        attributes.set_regular(true);
        for (size, expected) in [
            (None, serde_json::Value::Null),
            (Some(0), 0.into()),
            (Some(42), 42.into()),
        ] {
            attributes.size = size;
            let listed =
                interactive_entry_from_attributes("datei.txt".into(), &attributes).unwrap();
            assert_eq!(listed.display_entry().unwrap()["size"], expected);
            assert_eq!(listed.size_known, size.is_some());
            assert!(listed.entry.is_file);
        }
    }

    #[test]
    fn eintrag_liefert_sftp_darstellungsmetadaten() {
        let mut attributes = FileAttributes::empty();
        attributes.set_regular(true);
        attributes.permissions = attributes.permissions.map(|mode| mode | 0o640);
        attributes.size = Some(42);
        attributes.mtime = Some(7);
        attributes.uid = Some(501);
        attributes.user = Some("alice".into());
        attributes.gid = Some(20);
        attributes.group = Some("staff".into());

        let entry = entry_from_attributes("bericht.txt".into(), &attributes).unwrap();
        assert!(entry.is_file);
        assert_eq!(entry.extension.as_deref(), Some("txt"));
        assert_eq!(entry.owner.as_deref(), Some("alice"));
        assert_eq!(entry.group.as_deref(), Some("staff"));
        assert_eq!(entry.mode, Some(0o640));
        assert_eq!(entry.created, None);
    }

    #[test]
    fn interaktives_listing_verwirft_generation_und_identitaetsdrift() {
        let identity = SftpServerIdentity {
            host: "sftp.test".to_owned(),
            port: 22,
            host_key: SftpHostKeyPin {
                algorithm: "ssh-ed25519".to_owned(),
                sha256: [7; 32],
            },
            canonical_root: "/gebunden".to_owned(),
            sftp_version: 3,
            extensions_digest: [9; 32],
        };
        let reconnect_error =
            finish_interactive_listing(Vec::new(), Vec::new(), 4, &identity, 5, &identity)
                .expect_err("Ein Reconnect darf das alte interaktive Listing nicht etikettieren");
        assert_eq!(reconnect_error.kind(), io::ErrorKind::Interrupted);

        let mut changed_identity = identity.clone();
        changed_identity.canonical_root = "/ausgetauscht".to_owned();
        let identity_error =
            finish_interactive_listing(Vec::new(), Vec::new(), 4, &identity, 4, &changed_identity)
                .expect_err("Eine geänderte Root-Identität muss das Listing verwerfen");
        assert_eq!(identity_error.kind(), io::ErrorKind::Interrupted);

        let listing =
            finish_interactive_listing(Vec::new(), Vec::new(), 4, &identity, 4, &identity).unwrap();
        assert_eq!(listing.session_generation, 4);
        assert_eq!(listing.security, SftpReadSecurity::SftpPathBestEffort);
        assert!(listing.entries.is_empty());
    }

    #[test]
    fn secret_string_akzeptiert_keine_nul_werte() {
        let mut value = String::from("teil");
        value.push('\0');
        value.push_str("rest");
        assert!(SecretString::new(value).is_err());
    }
}
