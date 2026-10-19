//! HTTPS-WebDAV mit gepinntem Zertifikat und strengem `Depth: 1`-Listing.
//!
//! Der Provider behandelt Multi-Status niemals als „best effort": Pflichtmetadaten
//! müssen erfolgreich, eindeutig und vollständig sein. Nur eine separat mit 404 als
//! fehlend gemeldete optionale `creationdate` wird als `None` übernommen. Dateien
//! benötigen eine starke ETag; schwache oder fehlende ETags sperren den Plan.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use percent_encoding::percent_decode_str;
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;
use reqwest::blocking::{Body, Client, RequestBuilder};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH, SERVER,
};
use reqwest::{Method, StatusCode, Url};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};

use crate::error::{coded_error, ErrorCode};
use crate::ftps::{tls_config, PinnedCertificateVerifier};
use crate::metadata::{AclModel, Capability, MetadataCapabilities};
use crate::remote::{
    validate_webdav_origin, RemoteCapabilities, WebDavCertificatePin, WebDavConnectionConfig,
    WebDavRootIdentity, WebDavServerIdentity,
};
use crate::sftp::{SecretResolver, SecretString};
#[path = "webdav_namespace.rs"]
mod interactive_namespace;
#[path = "webdav_rename.rs"]
mod interactive_rename;

pub(crate) fn validate_ordinary_rename_path(source: &Path, new_name: &str) -> io::Result<()> {
    interactive_rename::validate_request(source, new_name)
}
use crate::vfs::{entry_extension, CompleteDirectoryListing, Entry, ProofListEntry, Vfs};

#[path = "webdav_folder.rs"]
mod interactive_folder;
#[path = "webdav_upload.rs"]
mod interactive_upload;

pub(crate) const FILE_REPLACE_BLOCKER: &str = "WebDAV-Dateiersetzen ist hier nicht ausführbar: Ein einzelnes MOVE kann Temp-Version, bestätigte Altzielversion und sämtliche Elternversionen nicht gemeinsam mit UND binden. Getaggte If-Listen verschiedener Ressourcen gelten laut RFC 4918 als ODER; ein passender Elterntoken kann ein ausgetauschtes Ziel freigeben. Vorhandene Ziele bleiben erhalten; neue Dateien können separat additiv kopiert werden.";
pub(crate) const STRICT_MUTATION_BLOCKER: &str = "Strikte WebDAV-Mutationen sind nicht ausführbar: Standardbedingungen binden höchstens eine Elternressource zusätzlich zur Request-Version, nicht die vollständige Root-/Elternkette. Deshalb keine künstliche Schreib-/Löschprobe, allgemeine VFS-Schreibfreigabe, Quellentfernung oder Quarantäne. Gewöhnliche additive Uploads, Ordneranlage und Umbenennung behalten ihre eigene bestätigte Best-Effort-Klasse.";
pub(crate) const NAMESPACE_BINDING_LIMITATION: &str = "Eine einzelne WebDAV-If-Bedingung bindet den unmittelbaren Elternordner; weitere Elternlisten würden sie durch ODER abschwächen. Die übrige Kette bis zum Profilroot wird nur vor und nach jeder Wirkung geprüft. Ein später Wechsel eines Vorfahren kann bereits wirken, bevor der Readback den Ausgang als unklar meldet. Es gibt keinen automatischen Rücktausch oder Wiederholversuch.";

pub(crate) fn ensure_ordinary_upload_profile_current(
    queue: &Path,
    bindings: &[crate::RemoteProfileBinding],
    config: &WebDavConnectionConfig,
) -> io::Result<()> {
    interactive_upload::ensure_profile_current(queue, bindings, config)
}

pub(crate) fn ordinary_upload_capabilities() -> crate::ProviderFileCapabilities {
    interactive_upload::capabilities()
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TLS_IO_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_MULTISTATUS_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct WebDavSession {
    http: Arc<WebDavHttp>,
    identity: WebDavServerIdentity,
}

struct WebDavHttp {
    client: Client,
    mutation_client: Client,
    username: String,
    password: SecretString,
}

struct ProviderState {
    session: WebDavSession,
    generation: u64,
    capabilities: RemoteCapabilities,
}

struct BoundWebDavListing {
    session: WebDavSession,
    generation: u64,
    capabilities: RemoteCapabilities,
}

#[derive(Clone)]
struct BoundWebDavMutation {
    session: WebDavSession,
    generation: u64,
    capabilities: RemoteCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WebDavResourceIdentity {
    remote: String,
    etag: String,
}

impl BoundWebDavMutation {
    fn checked_session(&self) -> &WebDavSession {
        // Generation und Fähigkeiten gehören sichtbar zum selben Snapshot wie die
        // Sitzung. Die konkrete Mutation verwendet anschließend nur diese Sitzung.
        debug_assert!(self.generation > 0);
        let _captured_capabilities = self.capabilities;
        &self.session
    }
}

fn bind_mutation_snapshot_with<S: Clone>(
    snapshot: impl FnOnce() -> (S, String, u64, RemoteCapabilities),
    permitted: impl FnOnce(RemoteCapabilities) -> bool,
    _missing_capability: &str,
) -> io::Result<(S, String, u64, RemoteCapabilities)> {
    let (session, root, generation, capabilities) = snapshot();
    if !permitted(capabilities) {
        return require_strict_mutation_contract();
    }
    Ok((session, root, generation, capabilities))
}

/// An eine HTTPS-Origin und einen kanonischen Collection-Root gebundenes VFS.
/// Strikte Schreibfähigkeiten folgen ausschließlich ihren verhaltensgeprüften
/// Verträgen. Gewöhnliche Namespace- und Dateiwirkungen verwenden private Sichten.
pub struct WebDavVfs {
    config: WebDavConnectionConfig,
    secrets: Arc<dyn SecretResolver>,
    state: Mutex<ProviderState>,
    reconnect: Mutex<()>,
}

/// Ein interaktives Pane-Listing samt der Generation, unter der es entstanden ist.
/// Die Generation verhindert eine Vermischung mit einem parallelen Reconnect, ist
/// aber ausdrücklich kein Namespace- oder Identitätsbeleg.
#[derive(Clone, Debug)]
pub struct WebDavInteractiveDirectoryListing {
    pub entries: Vec<Entry>,
    pub session_generation: u64,
}

impl WebDavVfs {
    pub fn connect(
        config: WebDavConnectionConfig,
        secrets: Arc<dyn SecretResolver>,
    ) -> io::Result<Self> {
        config.validate()?;
        let session = connect_once(&config, secrets.as_ref())?;
        Ok(Self {
            config,
            secrets,
            state: Mutex::new(ProviderState {
                session,
                generation: 1,
                capabilities: RemoteCapabilities {
                    complete_listing: true,
                    ..RemoteCapabilities::default()
                },
            }),
            reconnect: Mutex::new(()),
        })
    }

    pub fn identity(&self) -> WebDavServerIdentity {
        lock(&self.state).session.identity.clone()
    }

    pub fn session_generation(&self) -> u64 {
        lock(&self.state).generation
    }

    pub fn capabilities(&self) -> RemoteCapabilities {
        lock(&self.state).capabilities
    }

    pub(crate) fn mark_quarantine_verified(
        &self,
        _expected_generation: u64,
    ) -> io::Result<RemoteCapabilities> {
        require_strict_mutation_contract()
    }

    pub fn reconnect(&self) -> io::Result<()> {
        let _claim = lock(&self.reconnect);
        let expected = self.identity();
        let session = connect_once(&self.config, self.secrets.as_ref())?;
        if session.identity != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "WebDAV-Identität änderte sich beim Reconnect",
            ));
        }
        let mut state = lock(&self.state);
        state.session = session;
        state.generation = state.generation.saturating_add(1);
        state.capabilities = RemoteCapabilities {
            complete_listing: true,
            ..RemoteCapabilities::default()
        };
        Ok(())
    }

    /// Standard-WebDAV beweist keine gemeinsame Root-/Elternbindung.
    /// Diese kompatiblen Abfragen bleiben passiv und erteilen keine Freigabe.
    pub fn probe_directory_create_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_directory_create_capability_at(Path::new("/"))
    }

    pub fn probe_directory_create_capability_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        interactive_namespace::validate_path(probe_root)?;
        require_strict_mutation_contract()
    }

    /// Passive Grenze des strikten Rename-Vertrags; gewöhnliches F2 ist getrennt.
    pub fn probe_rename_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_rename_capability_at(Path::new("/"))
    }

    pub fn probe_rename_capability_at(&self, probe_root: &Path) -> io::Result<RemoteCapabilities> {
        interactive_namespace::validate_path(probe_root)?;
        require_strict_mutation_contract()
    }

    /// Passive Absage: Bedingtes DELETE bindet keine vollständige Elternkette;
    /// Collection-DELETE ist zudem rekursiv und taugt nicht als Cleanup-Probe.
    pub fn probe_permanent_delete_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_permanent_delete_capability_at(Path::new("/"))
    }

    pub fn probe_permanent_delete_capability_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        interactive_namespace::validate_path(probe_root)?;
        require_strict_mutation_contract()
    }

    fn canonical_root(&self) -> String {
        self.identity().canonical_root
    }

    fn remote_path(&self, path: &Path) -> io::Result<String> {
        map_virtual_path(&self.canonical_root(), path)
    }

    fn bind_listing(&self, path: &Path) -> io::Result<(BoundWebDavListing, String)> {
        let state = lock(&self.state);
        // HTTP-Sitzung, kanonischer Root, Identität, Generation und Fähigkeiten
        // stammen aus genau demselben gesperrten Zustand. Getrennte Getter könnten
        // sonst ein altes PROPFIND mit einer neuen Generation tokenisieren.
        let identity = state.session.identity.clone();
        let remote = map_virtual_path(&identity.canonical_root, path)?;
        Ok((
            BoundWebDavListing {
                session: WebDavSession {
                    http: state.session.http.clone(),
                    identity,
                },
                generation: state.generation,
                capabilities: state.capabilities,
            },
            remote,
        ))
    }

    /// Listing, Fähigkeiten **und** die Generation, an die das Ergebnis gebunden ist.
    ///
    /// Die Generation getrennt nachzulesen wäre ein Rennen: Die Verbindungen sind
    /// prozessweit geteilt, und ein Reconnect aus einem anderen Fenster zählt sie
    /// zwischen beiden Zugriffen hoch. Die Einträge der alten Sitzung trügen dann
    /// das Etikett der neuen — und genau daran hängt die Driftbindung von Pane,
    /// Ordnergrösse und Folgeoperation. `complete_bound_webdav_listing_with` prüft
    /// die Generation nach dem Parsen ohnehin gegen genau diesen Wert.
    fn complete_list_bound(
        &self,
        path: &Path,
    ) -> io::Result<(CompleteDirectoryListing, RemoteCapabilities, u64)> {
        let (binding, remote) = self.bind_listing(path)?;
        let generation = binding.generation;
        let (listing, capabilities) = complete_bound_webdav_listing_with(
            &binding.session,
            binding.generation,
            &binding.session.identity,
            binding.capabilities,
            |session| {
                let response = propfind(session, &remote, "1")?;
                parse_collection_listing(&response, &session.identity.origin, &remote, true)
            },
            || {
                let state = lock(&self.state);
                (state.generation, state.session.identity.clone())
            },
        )?;
        Ok((listing, capabilities, generation))
    }

    fn complete_list(&self, path: &Path) -> io::Result<Vec<ProofListEntry>> {
        self.complete_list_bound(path)
            .map(|(listing, _, _)| listing.entries)
    }

    /// Interaktives Pane-Listing samt gebundener Generation.
    pub fn list_interactive(&self, path: &Path) -> io::Result<WebDavInteractiveDirectoryListing> {
        let (listing, _, session_generation) = self.complete_list_bound(path)?;
        Ok(WebDavInteractiveDirectoryListing {
            entries: listing.entries.into_iter().map(|item| item.entry).collect(),
            session_generation,
        })
    }

    fn bind_mutation(
        &self,
        path: &Path,
        permitted: impl FnOnce(RemoteCapabilities) -> bool,
        missing_capability: &str,
    ) -> io::Result<(BoundWebDavMutation, String)> {
        let (session, root, generation, capabilities) = bind_mutation_snapshot_with(
            || {
                let state = lock(&self.state);
                (
                    state.session.clone(),
                    state.session.identity.canonical_root.clone(),
                    state.generation,
                    state.capabilities,
                )
            },
            permitted,
            missing_capability,
        )?;
        let remote = map_virtual_path(&root, path)?;
        Ok((
            BoundWebDavMutation {
                session,
                generation,
                capabilities,
            },
            remote,
        ))
    }

    fn stat_proof_bound(binding: &BoundWebDavMutation, remote: &str) -> io::Result<ProofListEntry> {
        stat_proof_in_session(binding.checked_session(), remote)
    }

    fn resource_identity_bound(
        binding: &BoundWebDavMutation,
        remote: &str,
        expected_collection: bool,
    ) -> io::Result<WebDavResourceIdentity> {
        let proof = Self::stat_proof_bound(binding, remote)?;
        if proof.entry.is_symlink
            || (expected_collection && !proof.entry.is_dir)
            || (!expected_collection && !proof.entry.is_file)
        {
            return Err(invalid(if expected_collection {
                "WebDAV-Elternbindung benötigt eine echte Collection"
            } else {
                "WebDAV-Ressourcenbindung benötigt eine reguläre Datei"
            }));
        }
        let etag = proof
            .provider_token
            .and_then(|value| String::from_utf8(value).ok())
            .ok_or_else(|| invalid("WebDAV-Ressourcenbindung benötigt eine starke ETag"))?;
        validate_strong_etag(&etag)?;
        Ok(WebDavResourceIdentity {
            remote: remote.to_owned(),
            etag,
        })
    }

    fn ensure_mutation_binding_current(&self, binding: &BoundWebDavMutation) -> io::Result<()> {
        let state = lock(&self.state);
        if state.generation != binding.generation
            || state.session.identity != binding.session.identity
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "WebDAV-Sitzung wechselte während der gebundenen Operation",
            ));
        }
        Ok(())
    }

    fn stat_proof(&self, path: &Path) -> io::Result<ProofListEntry> {
        let remote = self.remote_path(path)?;
        let state = lock(&self.state);
        let response = propfind(&state.session, &remote, "0")?;
        let mut entries =
            parse_collection_listing(&response, &state.session.identity.origin, &remote, false)?;
        if entries.len() != 1 {
            return Err(invalid(
                "WebDAV-Depth-0 lieferte nicht genau eine Ressource",
            ));
        }
        Ok(entries.remove(0))
    }

    /// Passive Absage des strikten AddOnly-Vertrags; primitive Gegenproben
    /// können die erforderliche gemeinsame Root-/Elternbindung nicht beweisen.
    pub fn probe_add_only_capabilities(&self) -> io::Result<RemoteCapabilities> {
        self.probe_add_only_capabilities_at(Path::new("/"))
    }

    pub fn probe_add_only_capabilities_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        interactive_namespace::validate_path(probe_root)?;
        require_strict_mutation_contract()
    }
}

impl Vfs for WebDavVfs {
    fn safety_capabilities_digest(&self) -> io::Result<Option<[u8; 32]>> {
        crate::remote::capabilities_digest(self.capabilities()).map(Some)
    }

    fn reclaim_verification_session(&self) -> io::Result<()> {
        // Ausschließlich für lesende Zustandsklärung nach einer verlorenen
        // Mutationsantwort. `reconnect` bindet dieselbe Serveridentität und setzt
        // alle verhaltensgeprüften Schreibfähigkeiten zurück.
        self.reconnect()
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
            .into_iter()
            .map(|item| item.entry)
            .collect())
    }

    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        self.complete_list_bound(path)
            .map(|(listing, _, _)| listing)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        Ok(self.stat_proof(path)?.entry)
    }

    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }

    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
    }
    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let remote = self.remote_path(path)?;
        let proof = self.stat_proof(path)?;
        let etag = proof
            .provider_token
            .and_then(|value| String::from_utf8(value).ok())
            .ok_or_else(|| invalid("WebDAV-Datei besitzt keine starke ETag"))?;
        if !proof.entry.is_file || proof.entry.is_symlink {
            return Err(invalid("WebDAV-GET benötigt eine reguläre Datei"));
        }
        let state = lock(&self.state);
        let url = resource_url(&state.session.identity.origin, &remote)?;
        let response = authenticated(state.session.http.client.get(url), &state.session.http)
            .header(IF_MATCH, etag.as_str())
            .send()
            .map_err(http_error)?;
        require_status(response.status(), &[StatusCode::OK], "WebDAV-GET")?;
        require_matching_etag(response.headers(), &etag, "WebDAV-GET")?;
        // Die Antwort implementiert `Read` und streamt große Dateien ohne RAM-Puffer.
        Ok(Box::new(response))
    }

    fn open_write(&self, _path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Err(read_only())
    }
    fn open_write_new(&self, _path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        require_strict_mutation_contract()
    }
    fn sync_file(&self, _path: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let entry = self.stat(path)?;
        if entry.is_dir && !entry.is_symlink {
            Ok(())
        } else {
            Err(invalid(
                "WebDAV-Namespace-Readback benötigt eine Collection",
            ))
        }
    }
    fn set_mtime(&self, _path: &Path, _mtime: i64) -> io::Result<()> {
        Err(read_only())
    }
    fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(read_only())
    }
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
    }
    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
    }

    fn remove_dir(&self, _path: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
    }

    fn remove_if_matches(&self, _path: &Path, _expected: &ProofListEntry) -> io::Result<()> {
        require_strict_mutation_contract()
    }

    fn supports_remove_if_matches(&self) -> bool {
        false
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn rename_noreplace(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
    }

    fn rename_replace_plain_once(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        require_strict_mutation_contract()
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
        let (binding, remote) = self.bind_mutation(
            path,
            |_| true,
            "WebDAV-Root-Identität besitzt keine gebundene Sitzung",
        )?;
        let identity = Self::resource_identity_bound(&binding, &remote, true)?;
        self.ensure_mutation_binding_current(&binding)?;
        Ok(crate::real_mode::RootIdentity::WebDav(WebDavRootIdentity {
            server: binding.session.identity,
            virtual_root: normalize_virtual_path(path)?,
            root_etag: identity.etag,
            session_generation: binding.generation,
        }))
    }
}

/// Der synchrone VFS-Schreibvertrag reicht Bytes über ein lokales Socketpaar an den
/// PUT-Worker. Dadurch liegt auch bei einem Prozessabsturz kein Klartext-Spool auf Platte.
struct WebDavWrite {
    writer: Option<UnixStream>,
    task: Option<std::thread::JoinHandle<io::Result<(String, blake3::Hash)>>>,
    source_hash: blake3::Hasher,
    finished: bool,
}

impl WebDavWrite {
    fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "WebDAV-Pipe fehlt"))?;
        writer.flush()?;
        writer.shutdown(std::net::Shutdown::Write)?;
        drop(writer);
        let task = self
            .task
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "WebDAV-PUT-Worker fehlt"))?;
        let (_etag, target_hash) = task
            .join()
            .map_err(|_| io::Error::other("WebDAV-PUT-Worker ist abgestürzt"))??;
        if target_hash != self.source_hash.finalize() {
            return Err(invalid(
                "WebDAV-PUT-Readback stimmt nicht mit den Quelldaten überein",
            ));
        }
        self.finished = true;
        Ok(())
    }
}

impl Write for WebDavWrite {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "WebDAV-Schreibstrom ist bereits abgeschlossen",
            ));
        }
        let count = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "WebDAV-Pipe fehlt"))?
            .write(buffer)?;
        self.source_hash.update(&buffer[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.finish()
    }
}

impl Drop for WebDavWrite {
    fn drop(&mut self) {
        self.writer.take();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn put_stream_and_readback(
    session: WebDavSession,
    remote: String,
    parents: Vec<WebDavResourceIdentity>,
    reader: UnixStream,
    owned_files: Arc<Mutex<BTreeMap<String, String>>>,
    timeout: Duration,
) -> io::Result<(String, blake3::Hash)> {
    let response = with_parent_condition(
        authenticated(
            session
                .http
                .mutation_client
                .put(resource_url(&session.identity.origin, &remote)?),
            &session.http,
        ),
        &session.identity.origin,
        &parents,
        &remote,
    )?
    .header(IF_NONE_MATCH, "*")
    .timeout(timeout)
    .body(Body::new(reader))
    .send()
    .map_err(http_error)?;
    require_status(response.status(), &[StatusCode::CREATED], "WebDAV-PUT")?;
    let etag = require_strong_etag(response.headers(), "WebDAV-PUT")?;
    lock(&owned_files).insert(remote.clone(), etag.clone());
    let hash = get_hash_with_timeout(
        &session.http,
        &session.identity.origin,
        &remote,
        &etag,
        timeout,
    )?;
    Ok((etag, hash))
}

fn parent_server_path(child: &str) -> io::Result<String> {
    let child = child.trim_end_matches('/');
    let split = child
        .rfind('/')
        .ok_or_else(|| invalid("WebDAV-Kindpfad besitzt keinen Elternpfad"))?;
    if split == 0 {
        Ok("/".to_owned())
    } else {
        Ok(child[..split].to_owned())
    }
}

fn require_strict_mutation_contract<T>() -> io::Result<T> {
    Err(coded_error(
        io::ErrorKind::Unsupported,
        ErrorCode::Unsupported,
        STRICT_MUTATION_BLOCKER,
    ))
}

fn with_parent_condition(
    request: RequestBuilder,
    origin: &str,
    identities: &[WebDavResourceIdentity],
    child: &str,
) -> io::Result<RequestBuilder> {
    let parent = parent_server_path(child)?;
    let mut selected = None;
    for identity in identities {
        validate_strong_etag(&identity.etag)?;
        if identity.remote == parent {
            if selected.is_some_and(|etag| etag != identity.etag.as_str()) {
                return Err(coded_error(
                    io::ErrorKind::Interrupted,
                    ErrorCode::PlanDrift,
                    "Widersprüchliche WebDAV-Elternversionen; keine Mutation",
                ));
            }
            selected = Some(identity.etag.as_str());
        }
    }
    let etag = selected.ok_or_else(|| invalid("Der unmittelbare WebDAV-Elternbeleg fehlt"))?;
    // Genau eine Tagged-list: Weitere wahre Listen könnten eine falsche
    // Elternversion nach RFC 4918 §10.4.3 überdecken. Die restliche Kette bleibt
    // ausschließlich durch die getrennten Vorher-/Nachherprüfungen beobachtet.
    let value = format!("<{}> ([{etag}])", resource_url(origin, &parent)?);
    let value = HeaderValue::from_str(&value)
        .map_err(|_| invalid("WebDAV-Elternbedingung ließ sich nicht sicher kodieren"))?;
    Ok(request.header(HeaderName::from_static("if"), value))
}

fn get_hash_with_timeout(
    http: &WebDavHttp,
    origin: &str,
    path: &str,
    etag: &str,
    timeout: Duration,
) -> io::Result<blake3::Hash> {
    let mut response = authenticated(http.client.get(resource_url(origin, path)?), http)
        .header(IF_MATCH, etag)
        .timeout(timeout)
        .send()
        .map_err(http_error)?;
    require_status(
        response.status(),
        &[StatusCode::OK],
        "WebDAV-Inhaltsreadback",
    )?;
    require_matching_etag(response.headers(), etag, "WebDAV-Inhaltsreadback")?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize())
}

fn move_request(
    http: &WebDavHttp,
    origin: &str,
    from: &str,
    to: &str,
    etag: &str,
    parent_identities: &[WebDavResourceIdentity],
) -> io::Result<reqwest::blocking::Response> {
    let destination = resource_url(origin, to)?;
    let request = with_parent_condition(
        authenticated(
            http.mutation_client
                .request(custom_method(b"MOVE")?, resource_url(origin, from)?),
            http,
        ),
        origin,
        parent_identities,
        to,
    )?
    .header("Destination", destination.as_str())
    .header("Overwrite", "F")
    .header(IF_MATCH, etag);
    request.timeout(TLS_IO_TIMEOUT).send().map_err(http_error)
}

/// Entfernt ausschließlich die eigene, beim PUT per starkem ETag belegte Temp-Datei.
///
/// Der gespeicherte ETag geht zwingend als `If-Match` mit. Ohne ihn löschte das
/// Aufräumen eine serverseitig untergeschobene fremde Datei desselben Namens,
/// deren Identität nie geprüft wurde. HTTP 412 unterscheidet Datei- und
/// Elternbedingung nicht; daraus folgt keine bewiesene Fremddatei.
fn delete_owned(
    http: &WebDavHttp,
    origin: &str,
    path: &str,
    etag: &str,
    parent_identities: &[WebDavResourceIdentity],
    readback: impl FnOnce() -> io::Result<ProofListEntry>,
) -> io::Result<()> {
    let response = delete_request(http, origin, path, Some(etag), None, parent_identities)?;
    if response.status() == StatusCode::PRECONDITION_FAILED {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::PlanDrift,
            "WebDAV-Temp-DELETE abgelehnt: Datei- oder unmittelbare Elternversion passt nicht mehr; Bestand prüfen, nicht automatisch wiederholen",
        ));
    }
    require_status(
        response.status(),
        &[StatusCode::NO_CONTENT],
        "WebDAV-Temp-DELETE",
    )?;
    // 204 allein ist auch hier kein Wirkungsbeleg. Ohne den Readback gilt eine
    // Temp-Datei als entfernt, der Aufrufer streicht sie aus `owned_files` — und
    // wenn der Server sie doch stehen ließ, fehlt danach der ETag-Besitznachweis,
    // mit dem sie überhaupt noch aufgeräumt werden könnte. Jeder andere Löschweg
    // dieses Providers verlangt denselben Beleg.
    require_absent_after_delete_readback(readback(), path, "WebDAV-Temp-DELETE")
}

fn delete_request(
    http: &WebDavHttp,
    origin: &str,
    path: &str,
    etag: Option<&str>,
    depth: Option<&str>,
    parent_identities: &[WebDavResourceIdentity],
) -> io::Result<reqwest::blocking::Response> {
    let mut request = with_parent_condition(
        authenticated(
            http.mutation_client
                .request(custom_method(b"DELETE")?, resource_url(origin, path)?),
            http,
        ),
        origin,
        parent_identities,
        path,
    )?;
    if let Some(etag) = etag {
        request = request.header(IF_MATCH, etag);
    }
    if let Some(depth) = depth {
        request = request.header(HeaderName::from_static("depth"), depth);
    }
    request.timeout(TLS_IO_TIMEOUT).send().map_err(http_error)
}

/// Depth-0-Beleg genau einer Ressource. Die gebundene Fassung und der Abbruchweg
/// eines Schreibvorgangs brauchen dasselbe; nur die Herkunft der Sitzung
/// unterscheidet sich.
fn stat_proof_in_session(session: &WebDavSession, remote: &str) -> io::Result<ProofListEntry> {
    let response = propfind(session, remote, "0")?;
    let mut entries = parse_collection_listing(&response, &session.identity.origin, remote, false)?;
    if entries.len() != 1 {
        return Err(invalid(
            "WebDAV-Depth-0 lieferte nicht genau eine Ressource",
        ));
    }
    Ok(entries.remove(0))
}

fn require_absent_after_delete_readback(
    readback: io::Result<ProofListEntry>,
    remote: &str,
    label: &str,
) -> io::Result<()> {
    match readback {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "{label} meldete Erfolg, aber der Pfad ist im Readback weiter vorhanden: {remote}"
            ),
        )),
        Err(error) => Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "{label} meldete Erfolg, aber der verpflichtende Abwesenheitsbeleg scheiterte: {error}"
            ),
        )),
    }
}

fn require_strong_etag(headers: &HeaderMap, action: &str) -> io::Result<String> {
    let etag = required_header(headers, ETAG, "ETag")?;
    validate_strong_etag(&etag)
        .map_err(|_| invalid(format!("{action} lieferte keine starke ETag")))?;
    Ok(etag)
}

fn require_matching_etag(headers: &HeaderMap, expected: &str, action: &str) -> io::Result<()> {
    let actual = require_strong_etag(headers, action)?;
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{action} lieferte eine andere ETag"),
        ))
    }
}

pub(crate) fn confirmed_delete_etag(token: Option<&[u8]>) -> io::Result<&str> {
    let etag = token
        .and_then(|value| std::str::from_utf8(value).ok())
        .ok_or_else(|| invalid("WebDAV-DELETE benötigt die bestätigte starke ETag"))?;
    validate_strong_etag(etag)
        .map_err(|_| invalid("WebDAV-DELETE benötigt die bestätigte starke ETag"))?;
    Ok(etag)
}

fn validate_strong_etag(etag: &str) -> io::Result<()> {
    let inner = etag
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'));
    if etag.starts_with("W/")
        || inner.is_none()
        || inner.is_some_and(|value| {
            value.is_empty()
                || !value
                    .bytes()
                    .all(|byte| byte == 0x21 || (0x23..=0x7e).contains(&byte))
        })
    {
        return Err(invalid("WebDAV-Datei benötigt eine starke ETag"));
    }
    Ok(())
}

fn custom_method(bytes: &[u8]) -> io::Result<Method> {
    Method::from_bytes(bytes).map_err(io::Error::other)
}

/// Liest nur das öffentliche Leaf-Zertifikat einer HTTPS-Origin. Es folgen weder
/// HTTP-Anmeldung noch WebDAV-Befehl; der Fingerabdruck muss extern geprüft werden.
pub fn discover_webdav_certificate(origin: &str) -> io::Result<WebDavCertificatePin> {
    let url = validate_webdav_origin(origin)?;
    let host = discovery_host(&url);
    let port = url.port_or_known_default().unwrap_or(443);
    let observed = Arc::new(Mutex::new(None));
    let verifier = Arc::new(PinnedCertificateVerifier::discovery(observed.clone()));
    let config = tls_config(verifier)?;
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| invalid("WebDAV-TLS-Hostname ist ungültig"))?;
    let tcp = connect_tcp(&host, port)?;
    let connection = ClientConnection::new(config, server_name).map_err(io::Error::other)?;
    let mut stream = StreamOwned::new(connection, tcp);
    stream.conn.complete_io(&mut stream.sock)?;
    let digest =
        lock(&observed).ok_or_else(|| invalid("WebDAV-TLS-Verbindung lieferte kein Zertifikat"))?;
    Ok(WebDavCertificatePin { sha256: digest })
}

fn discovery_host(url: &Url) -> String {
    match url.host().expect("Origin wurde vorher validiert") {
        url::Host::Domain(host) => host.to_owned(),
        url::Host::Ipv4(address) => address.to_string(),
        url::Host::Ipv6(address) => address.to_string(),
    }
}

fn connect_once(
    config: &WebDavConnectionConfig,
    secrets: &dyn SecretResolver,
) -> io::Result<WebDavSession> {
    config.validate()?;
    let verifier = Arc::new(PinnedCertificateVerifier::new(
        config.certificate.sha256,
        config.certificate_policy,
    )?);
    let tls = tls_config(verifier)?;
    let client_builder = || {
        Client::builder()
            .use_preconfigured_tls((*tls).clone())
            .https_only(true)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("Doppeldecker/", env!("CARGO_PKG_VERSION")))
    };
    // Leseverbindungen bleiben wiederverwendbar. Sonst würden die vielen frischen
    // Metadatenbelege jedes Mal einen TLS-Aufbau benötigen.
    let client = client_builder().build().map_err(http_error)?;
    // Neben reqwest kann Hyper eine ungestartete Anfrage auf einer verlorenen
    // Poolverbindung intern neu versenden. Für jede Mutation verhindert eine
    // frische HTTP/1-Verbindung auch diesen zweiten Versandweg.
    let mutation_client = client_builder()
        .http1_only()
        .pool_max_idle_per_host(0)
        .build()
        .map_err(http_error)?;
    let password = secrets.resolve(&config.password_reference)?;
    let http = Arc::new(WebDavHttp {
        client,
        mutation_client,
        username: config.username.clone(),
        password,
    });
    let origin = config.normalized_origin()?;
    let requested_root = normalize_server_path(&config.requested_root)?;
    let options_url = resource_url(&origin, &requested_root)?;
    let response = send_metadata_read(options_url, |url| {
        authenticated(http.client.request(Method::OPTIONS, url), &http)
    })?;
    require_status(
        response.status(),
        &[StatusCode::OK, StatusCode::NO_CONTENT],
        "WebDAV-OPTIONS",
    )?;
    let dav = required_header(response.headers(), HeaderName::from_static("dav"), "DAV")?;
    let dav = canonical_dav_header(&dav)?;
    if !dav.split(',').any(|token| token == "1") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WebDAV-Server weist DAV-Klasse 1 nicht aus",
        ));
    }
    let server_header_digest = server_header_identity_digest(response.headers());
    let legacy_server_header_digests = legacy_server_header_identity_digests(response.headers())?;
    let provisional = WebDavSession {
        http,
        identity: WebDavServerIdentity {
            origin: origin.clone(),
            certificate: config.certificate.clone(),
            account_digest: *blake3::hash(config.username.as_bytes()).as_bytes(),
            canonical_root: requested_root.clone(),
            dav_header_digest: *blake3::hash(dav.as_bytes()).as_bytes(),
            server_header_digest,
            legacy_server_header_digests,
        },
    };
    let body = propfind(&provisional, &requested_root, "0")?;
    // Der Profilroot stammt ausschließlich aus der geprüften Konfiguration. Ein
    // Server-href darf ihn nicht erweitern oder auf eine andere Collection
    // umbiegen. Derselbe strenge Depth:0-Parser wie bei späteren Stat-Aufrufen
    // bindet daher die Selbstressource exakt an `requested_root`.
    let resources = parse_collection_listing(&body, &origin, &requested_root, false)?;
    if resources.len() != 1 || !resources[0].entry.is_dir || resources[0].entry.is_symlink {
        return Err(invalid(
            "WebDAV-Root-PROPFIND lieferte keine einzelne Collection",
        ));
    }
    let identity = WebDavServerIdentity {
        canonical_root: requested_root,
        ..provisional.identity.clone()
    };
    identity.validate()?;
    Ok(WebDavSession {
        identity,
        ..provisional
    })
}

fn propfind(session: &WebDavSession, path: &str, depth: &str) -> io::Result<Vec<u8>> {
    let url = resource_url(&session.identity.origin, path)?;
    let method = Method::from_bytes(b"PROPFIND").map_err(io::Error::other)?;
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/><D:getlastmodified/><D:creationdate/><D:getetag/></D:prop></D:propfind>"#;
    let mut response = send_metadata_read(url, |url| {
        authenticated(
            session.http.client.request(method.clone(), url),
            &session.http,
        )
        .header(HeaderName::from_static("depth"), depth)
        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(body)
    })?;
    require_status(
        response.status(),
        &[StatusCode::MULTI_STATUS],
        "WebDAV-PROPFIND",
    )?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut response)
        .take((MAX_MULTISTATUS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MULTISTATUS_BYTES {
        return Err(invalid(
            "WebDAV-Multi-Status überschreitet die Größenbegrenzung",
        ));
    }
    Ok(bytes)
}

fn authenticated(builder: RequestBuilder, http: &WebDavHttp) -> RequestBuilder {
    builder.basic_auth(&http.username, Some(http.password.expose()))
}

/// Collection-Server dürfen die äquivalente Slashform verlangen. Nur diese eine
/// Kanonisierung ist für OPTIONS/PROPFIND zulässig; Mutationen nutzen den Helfer nie.
fn send_metadata_read(
    url: Url,
    request: impl Fn(Url) -> RequestBuilder,
) -> io::Result<reqwest::blocking::Response> {
    let response = request(url.clone())
        .timeout(TLS_IO_TIMEOUT)
        .send()
        .map_err(http_error)?;
    if let Some(canonical) = slash_redirect_target(
        &url,
        response.status(),
        response.headers().get(reqwest::header::LOCATION),
    ) {
        drop(response);
        return request(canonical)
            .timeout(TLS_IO_TIMEOUT)
            .send()
            .map_err(http_error);
    }
    Ok(response)
}

fn slash_redirect_target(
    url: &Url,
    status: StatusCode,
    location: Option<&HeaderValue>,
) -> Option<Url> {
    if url.path().ends_with('/')
        || !matches!(
            status,
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        )
    {
        return None;
    }
    let target = url.join(location?.to_str().ok()?).ok()?;
    let mut expected = url.clone();
    expected.path_segments_mut().ok()?.push("");
    (target == expected).then_some(target)
}

#[derive(Default)]
struct DavResource {
    href: String,
    href_seen: bool,
    is_collection: bool,
    length: Option<u64>,
    mtime: Option<i64>,
    created: Option<i64>,
    etag: Option<String>,
    successful_propstats: usize,
    successful_resourcetype: Option<bool>,
}

fn direct_dav_parent(stack: &[(bool, Vec<u8>)], expected: &[u8]) -> bool {
    stack
        .last()
        .is_some_and(|(dav, name)| *dav && name.as_slice() == expected)
}

fn inside_foreign_property(stack: &[(bool, Vec<u8>)]) -> bool {
    stack.iter().any(|(dav, _)| !dav)
}

fn validate_nonfield_xml_value(value: &str, stack: &[(bool, Vec<u8>)]) -> io::Result<()> {
    if inside_foreign_property(stack) || value.chars().all(char::is_whitespace) {
        return Ok(());
    }
    Err(invalid(
        "WebDAV-Struktur enthält einen Nicht-Whitespace-Wert außerhalb eines Skalarfelds",
    ))
}

fn parse_multistatus(payload: &[u8]) -> io::Result<Vec<DavResource>> {
    let mut reader = NsReader::from_reader(payload);
    // XML-Whitespace gehört in Skalarfeldern zum Wert. Der Parser prüft
    // Struktur-Whitespace unten ausdrücklich, statt ihn global und damit auch aus
    // href, ETag oder Datumsfeldern zu entfernen.
    reader.config_mut().trim_text(false);
    let mut resources = Vec::new();
    let mut current: Option<DavResource> = None;
    let mut field = Vec::<u8>::new();
    let mut field_value = String::new();
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut root_seen = false;
    let mut root_closed = false;
    let mut element_stack = Vec::<(bool, Vec<u8>)>::new();
    let mut inside_propstat = false;
    let mut inside_prop = false;
    // RFC 4918 kennt `collection` ausschließlich als direktes Kind von
    // `resourcetype`. Ohne diese Verfolgung genügte ein gleichnamiges Element
    // irgendwo unter `prop`, um eine Datei als Ordner zu bilanzieren — und
    // `validate_resource` überspränge dann die für Dateien nötigen ETag-,
    // Größen- und Zeitprüfungen.
    // Die beiden Tiefenwerte unterscheiden ein direktes XML-Kind von einem nur
    // irgendwo darunter liegenden Element. Ein boolesches „innerhalb“ wäre hier
    // zu schwach: `<resourcetype><lockdiscovery><collection/>…` läge ebenfalls
    // innerhalb, bezeichnet nach RFC 4918 aber keine Collection.
    let mut prop_depth: Option<usize> = None;
    let mut resourcetype_depth: Option<usize> = None;
    let mut propstat_status: Option<u16> = None;
    let mut propstat_fields = Vec::<(Vec<u8>, String)>::new();
    let mut propstat_properties = BTreeSet::<Vec<u8>>::new();
    let mut propstat_collection = false;
    let mut propstat_prop_count = 0usize;
    let mut propstat_resourcetype_count = 0usize;
    let mut propstat_status_error: Option<String> = None;
    let mut propstat_index = 0usize;
    let mut response_semantic_errors = Vec::<String>::new();
    let mut multistatus_semantic_errors = Vec::<String>::new();
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|error| invalid(error.to_string()))?;
        let dav = is_dav_namespace(&namespace);
        match event {
            Event::Start(event) => {
                depth = depth.saturating_add(1);
                if depth > 64 {
                    return Err(invalid("WebDAV-XML ist zu tief verschachtelt"));
                }
                if !field.is_empty() {
                    return Err(invalid("WebDAV-Skalar enthält verschachteltes XML"));
                }
                let name = event.local_name().as_ref().to_vec();
                if root_closed {
                    return Err(invalid("WebDAV-XML enthält Inhalt nach multistatus"));
                }
                if !root_seen && (!dav || name.as_slice() != b"multistatus") {
                    return Err(invalid(
                        "WebDAV-XML besitzt Inhalt vor der multistatus-Wurzel",
                    ));
                }
                if !dav {
                    if direct_dav_parent(&element_stack, b"prop")
                        || inside_foreign_property(&element_stack)
                    {
                        // Zusätzliche fremde Properties sind laut WebDAV erlaubt.
                        // Ihr Teilbaum bleibt isoliert: Ein DAV-Pflichtfeld darin
                        // scheitert später an seiner falschen direkten Elternkante.
                        element_stack.push((false, name));
                        buffer.clear();
                        continue;
                    }
                    return Err(invalid("WebDAV-Struktur enthält einen fremden XML-Wrapper"));
                }
                match name.as_slice() {
                    b"multistatus" if !element_stack.is_empty() => {
                        return Err(invalid("WebDAV-multistatus steht nicht an der Wurzel"));
                    }
                    b"response" if !direct_dav_parent(&element_stack, b"multistatus") => {
                        return Err(invalid(
                            "WebDAV-response steht nicht direkt unter multistatus",
                        ));
                    }
                    b"href" if !direct_dav_parent(&element_stack, b"response") => {
                        return Err(invalid("WebDAV-href steht nicht direkt unter response"));
                    }
                    b"propstat" if !direct_dav_parent(&element_stack, b"response") => {
                        return Err(invalid("WebDAV-propstat steht nicht direkt unter response"));
                    }
                    b"prop" if !direct_dav_parent(&element_stack, b"propstat") => {
                        return Err(invalid("WebDAV-prop steht nicht direkt unter propstat"));
                    }
                    b"status" if !direct_dav_parent(&element_stack, b"propstat") => {
                        return Err(invalid("WebDAV-status steht nicht direkt unter propstat"));
                    }
                    b"resourcetype" | b"getcontentlength" | b"getlastmodified"
                    | b"creationdate" | b"getetag"
                        if !direct_dav_parent(&element_stack, b"prop") =>
                    {
                        return Err(invalid("WebDAV-Eigenschaft steht nicht direkt unter prop"));
                    }
                    b"collection" if !direct_dav_parent(&element_stack, b"resourcetype") => {
                        return Err(invalid(
                            "WebDAV-Collection steht nicht direkt unter resourcetype",
                        ));
                    }
                    b"responsedescription"
                        if direct_dav_parent(&element_stack, b"multistatus")
                            || direct_dav_parent(&element_stack, b"response")
                            || direct_dav_parent(&element_stack, b"propstat") => {}
                    b"responsedescription" => return Err(invalid(
                        "WebDAV-Antwortbeschreibung steht nicht an einer erlaubten Strukturkante",
                    )),
                    b"multistatus" | b"response" | b"href" | b"propstat" | b"prop" | b"status"
                    | b"resourcetype" | b"getcontentlength" | b"getlastmodified"
                    | b"creationdate" | b"getetag" | b"collection" => {}
                    _ if direct_dav_parent(&element_stack, b"resourcetype") => {
                        return Err(invalid(
                            "WebDAV-resourcetype enthält einen unbekannten DAV-Ressourcentyp",
                        ));
                    }
                    _ if direct_dav_parent(&element_stack, b"multistatus")
                        || direct_dav_parent(&element_stack, b"response")
                        || direct_dav_parent(&element_stack, b"propstat") =>
                    {
                        return Err(invalid(
                            "WebDAV-Struktur enthält einen unbekannten DAV-Wrapper",
                        ));
                    }
                    _ => {}
                }
                element_stack.push((true, name.clone()));
                match name.as_slice() {
                    b"multistatus" => {
                        if root_seen || depth != 1 {
                            return Err(invalid(
                                "WebDAV-Multi-Status besitzt keine eindeutige Wurzel",
                            ));
                        }
                        root_seen = true;
                    }
                    b"response" => {
                        if !root_seen || root_closed || current.is_some() {
                            return Err(invalid("WebDAV-Multi-Status verschachtelt response"));
                        }
                        current = Some(DavResource::default());
                        response_semantic_errors.clear();
                        propstat_index = 0;
                    }
                    b"propstat" => {
                        if current.is_none() || inside_propstat {
                            return Err(invalid("WebDAV-propstat steht an falscher Stelle"));
                        }
                        inside_propstat = true;
                        prop_depth = None;
                        resourcetype_depth = None;
                        propstat_status = None;
                        propstat_fields.clear();
                        propstat_properties.clear();
                        propstat_collection = false;
                        propstat_prop_count = 0;
                        propstat_resourcetype_count = 0;
                        propstat_status_error = None;
                    }
                    b"prop" => {
                        if !inside_propstat || inside_prop {
                            return Err(invalid("WebDAV-prop steht an falscher Stelle"));
                        }
                        propstat_prop_count = propstat_prop_count.saturating_add(1);
                        if propstat_prop_count != 1 {
                            return Err(invalid(
                                "WebDAV-propstat enthält mehr als ein direktes prop",
                            ));
                        }
                        inside_prop = true;
                        prop_depth = Some(depth);
                    }
                    b"responsedescription" => {
                        field = name;
                        field_value.clear();
                    }
                    b"href" if current.is_some() && !inside_propstat => {
                        field = name;
                        field_value.clear();
                    }
                    b"status" if inside_propstat => {
                        field = name;
                        field_value.clear();
                    }
                    b"resourcetype" if inside_prop && prop_depth == Some(depth - 1) => {
                        propstat_properties.insert(name);
                        propstat_resourcetype_count = propstat_resourcetype_count.saturating_add(1);
                        resourcetype_depth = Some(depth);
                    }
                    b"resourcetype" => {
                        return Err(invalid("WebDAV-resourcetype steht nicht direkt unter prop"));
                    }
                    b"getcontentlength" | b"getlastmodified" | b"creationdate" | b"getetag"
                        if inside_prop && prop_depth == Some(depth - 1) =>
                    {
                        propstat_properties.insert(name.clone());
                        field = name;
                        field_value.clear();
                    }
                    b"getcontentlength" | b"getlastmodified" | b"creationdate" | b"getetag" => {
                        return Err(invalid("WebDAV-Pflichtfeld steht nicht direkt unter prop"));
                    }
                    b"collection" => {
                        if resourcetype_depth != Some(depth - 1) {
                            return Err(invalid(
                                "WebDAV-Collection steht nicht direkt unter resourcetype",
                            ));
                        }
                        propstat_collection = true;
                    }
                    _ => {}
                }
            }
            Event::Empty(event) => {
                if !field.is_empty() {
                    return Err(invalid("WebDAV-Skalar enthält verschachteltes XML"));
                }
                let name = event.local_name().as_ref().to_vec();
                if !root_seen || root_closed {
                    return Err(invalid(
                        "WebDAV-XML enthält ein leeres Element außerhalb von multistatus",
                    ));
                }
                if !dav {
                    if direct_dav_parent(&element_stack, b"prop")
                        || inside_foreign_property(&element_stack)
                    {
                        buffer.clear();
                        continue;
                    }
                    return Err(invalid(
                        "WebDAV-Struktur enthält einen leeren fremden XML-Wrapper",
                    ));
                }
                match name.as_slice() {
                    b"multistatus" | b"response" | b"propstat" | b"prop" | b"status" => {
                        return Err(invalid(
                            "WebDAV-Strukturknoten darf kein leeres Element sein",
                        ));
                    }
                    b"href" => {
                        if !direct_dav_parent(&element_stack, b"response") {
                            return Err(invalid("WebDAV-href steht nicht direkt unter response"));
                        }
                        let resource = current
                            .as_mut()
                            .ok_or_else(|| invalid("WebDAV-href steht außerhalb einer response"))?;
                        // `<href/>` ist dieselbe vorhandene, aber leere Angabe wie
                        // `<href></href>`. Der Vorkommenszähler muss deshalb gesetzt
                        // werden. Ein weiteres href bleibt ein semantischer Fehler,
                        // damit der Parser spätere Responses noch sicher bilanzieren
                        // kann, statt die Multi-Status-Antwort vorzeitig abzubrechen.
                        if let Err(error) = apply_resource_field(resource, b"href", String::new()) {
                            response_semantic_errors.push(error.to_string());
                        }
                    }
                    b"collection" => {
                        if !direct_dav_parent(&element_stack, b"resourcetype") {
                            return Err(invalid(
                                "WebDAV-Collection steht nicht direkt unter resourcetype",
                            ));
                        }
                        propstat_collection = true;
                    }
                    b"resourcetype" => {
                        if !direct_dav_parent(&element_stack, b"prop") {
                            return Err(invalid(
                                "WebDAV-resourcetype steht nicht direkt unter prop",
                            ));
                        }
                        propstat_properties.insert(name);
                        propstat_resourcetype_count = propstat_resourcetype_count.saturating_add(1);
                    }
                    b"getcontentlength" | b"getlastmodified" | b"creationdate" | b"getetag" => {
                        if !direct_dav_parent(&element_stack, b"prop") {
                            return Err(invalid(
                                "WebDAV-Pflichtfeld steht nicht direkt unter prop",
                            ));
                        }
                        propstat_properties.insert(name.clone());
                        propstat_fields.push((name, String::new()));
                    }
                    b"responsedescription" if direct_dav_parent(&element_stack, b"multistatus")
                        || direct_dav_parent(&element_stack, b"response")
                        || direct_dav_parent(&element_stack, b"propstat") => {}
                    b"responsedescription" => return Err(invalid(
                        "Leere WebDAV-Antwortbeschreibung steht nicht an einer erlaubten Strukturkante")),
                    _ if direct_dav_parent(&element_stack, b"resourcetype") => {
                        return Err(invalid(
                            "WebDAV-resourcetype enthält einen leeren unbekannten DAV-Ressourcentyp",
                        ));
                    }
                    _ if direct_dav_parent(&element_stack, b"multistatus")
                        || direct_dav_parent(&element_stack, b"response")
                        || direct_dav_parent(&element_stack, b"propstat") =>
                    {
                        return Err(invalid(
                            "WebDAV-Struktur enthält einen leeren unbekannten DAV-Wrapper",
                        ));
                    }
                    _ => {}
                }
            }
            Event::Text(text) => {
                let value = text
                    .decode()
                    .map_err(|error| invalid(error.to_string()))?
                    .into_owned();
                if field.is_empty() {
                    validate_nonfield_xml_value(&value, &element_stack)?;
                    buffer.clear();
                    continue;
                }
                if current.is_none() && field.as_slice() != b"responsedescription" {
                    return Err(invalid("WebDAV-Text außerhalb einer response"));
                }
                field_value.push_str(&value);
                if field_value.len() > 8192 {
                    return Err(invalid("WebDAV-Skalar überschreitet die Größenbegrenzung"));
                }
            }
            Event::CData(text) => {
                let value = text
                    .decode()
                    .map_err(|error| invalid(error.to_string()))?
                    .into_owned();
                if field.is_empty() {
                    validate_nonfield_xml_value(&value, &element_stack)?;
                    buffer.clear();
                    continue;
                }
                if current.is_none() && field.as_slice() != b"responsedescription" {
                    return Err(invalid("WebDAV-CDATA steht außerhalb einer response"));
                }
                // CDATA ändert nur die XML-Schreibweise, nicht den Wert des direkt
                // gebundenen WebDAV-Skalarfelds. Deshalb gelten dieselbe sichere
                // Dekodierung und dieselbe harte Größenbegrenzung wie für Text.
                field_value.push_str(&value);
                if field_value.len() > 8192 {
                    return Err(invalid("WebDAV-Skalar überschreitet die Größenbegrenzung"));
                }
            }
            Event::GeneralRef(reference) => {
                let value = reference
                    .decode()
                    .map_err(|error| invalid(error.to_string()))?;
                let character = match value.as_ref() {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    _ => reference
                        .resolve_char_ref()
                        .map_err(|error| invalid(error.to_string()))?
                        .ok_or_else(|| invalid("WebDAV-XML enthält eine unbekannte Entität"))?,
                };
                if character.is_control() && !matches!(character, '\t' | '\n' | '\r') {
                    return Err(invalid("WebDAV-XML enthält ein unzulässiges Steuerzeichen"));
                }
                if field.is_empty() {
                    validate_nonfield_xml_value(&character.to_string(), &element_stack)?;
                    buffer.clear();
                    continue;
                }
                if current.is_none() && field.as_slice() != b"responsedescription" {
                    return Err(invalid("WebDAV-Entität steht außerhalb einer response"));
                }
                field_value.push(character);
                if field_value.len() > 8192 {
                    return Err(invalid("WebDAV-Skalar überschreitet die Größenbegrenzung"));
                }
            }
            Event::End(event) => {
                let closing_name = event.local_name().as_ref().to_vec();
                let Some((opened_dav, opened_name)) = element_stack.last() else {
                    return Err(invalid("WebDAV-XML schließt ein Element ohne Anfang"));
                };
                if *opened_dav != dav || *opened_name != closing_name {
                    return Err(invalid(
                        "WebDAV-XML schließt einen anderen Strukturknoten als geöffnet",
                    ));
                }
                if dav {
                    match closing_name.as_slice() {
                        b"responsedescription" => {
                            field_value.clear();
                            field.clear();
                        }
                        b"href" => {
                            let resource = current.as_mut().ok_or_else(|| {
                                invalid("WebDAV-Skalar endet außerhalb einer response")
                            })?;
                            if let Err(error) = apply_resource_field(
                                resource,
                                &field,
                                std::mem::take(&mut field_value),
                            ) {
                                // Ein doppeltes href ist semantisch falsch, aber die
                                // XML-Struktur bleibt sicher weiter parsebar. Der
                                // erste gebundene href bezeichnet die Fehlermeldung;
                                // spätere Responses werden ebenfalls ausgewertet.
                                response_semantic_errors.push(error.to_string());
                            }
                            field.clear();
                        }
                        b"status" => {
                            match status_line_code(&field_value) {
                                Ok(status) => {
                                    if propstat_status.replace(status).is_some() {
                                        propstat_status_error = Some(
                                            "WebDAV-propstat enthält mehrere Status".to_owned(),
                                        );
                                    }
                                }
                                Err(error) => {
                                    propstat_status_error = Some(error.to_string());
                                }
                            }
                            field_value.clear();
                            field.clear();
                        }
                        b"getcontentlength" | b"getlastmodified" | b"creationdate" | b"getetag" => {
                            propstat_fields.push((field.clone(), std::mem::take(&mut field_value)));
                            field.clear();
                        }
                        b"resourcetype" if resourcetype_depth == Some(depth) => {
                            resourcetype_depth = None;
                        }
                        b"prop" => {
                            inside_prop = false;
                            prop_depth = None;
                            resourcetype_depth = None;
                        }
                        b"propstat" => {
                            propstat_index = propstat_index.saturating_add(1);
                            let resource = current
                                .as_mut()
                                .ok_or_else(|| invalid("WebDAV-propstat-Ende ohne response"))?;
                            let mut semantic_errors = Vec::new();
                            let had_status_error = propstat_status_error.is_some();
                            if let Some(error) = propstat_status_error.take() {
                                semantic_errors.push(error);
                            }
                            match propstat_status {
                                Some(200) => {
                                    if propstat_prop_count != 1 {
                                        semantic_errors.push(format!(
                                            "erfolgreicher propstat besitzt {} direkte prop statt genau einem",
                                            propstat_prop_count
                                        ));
                                    }
                                    if propstat_resourcetype_count > 1 {
                                        semantic_errors.push(format!(
                                            "erfolgreicher propstat besitzt {} direkte resourcetype statt höchstens einem",
                                            propstat_resourcetype_count
                                        ));
                                    } else if propstat_resourcetype_count == 1 {
                                        if let Some(previous) =
                                            resource.successful_resourcetype
                                        {
                                            if previous != propstat_collection {
                                                semantic_errors.push(
                                                    "resourcetype widerspricht einem früheren erfolgreichen propstat"
                                                        .to_owned(),
                                                );
                                            } else {
                                                semantic_errors.push(
                                                    "resourcetype steht in mehreren erfolgreichen propstat-Blöcken"
                                                        .to_owned(),
                                                );
                                            }
                                        } else {
                                            resource.successful_resourcetype =
                                                Some(propstat_collection);
                                            resource.is_collection = propstat_collection;
                                        }
                                    }
                                    for (field, value) in &propstat_fields {
                                        if let Err(error) =
                                            apply_resource_field(resource, field, value.clone())
                                        {
                                            semantic_errors.push(error.to_string());
                                        }
                                    }
                                    resource.successful_propstats =
                                        resource.successful_propstats.saturating_add(1);
                                }
                                Some(404)
                                    if !propstat_properties.is_empty()
                                        && propstat_properties.iter().all(|property| {
                                            property.as_slice() == b"creationdate"
                                        }) => {}
                                Some(status) => semantic_errors.push(format!(
                                    "Status {status} ist für eine Pflichtmetadatenklasse nicht erfolgreich"
                                )),
                                None if !had_status_error => {
                                    semantic_errors.push(
                                        "WebDAV-propstat besitzt keinen Status".to_owned(),
                                    );
                                }
                                None => {}
                            }
                            if !semantic_errors.is_empty() {
                                response_semantic_errors.push(format!(
                                    "propstat {propstat_index}: {}",
                                    semantic_errors.join(", ")
                                ));
                            }
                            inside_propstat = false;
                            prop_depth = None;
                            resourcetype_depth = None;
                            propstat_status = None;
                            propstat_fields.clear();
                            propstat_properties.clear();
                            propstat_collection = false;
                            propstat_prop_count = 0;
                            propstat_resourcetype_count = 0;
                            propstat_status_error = None;
                        }
                        b"response" => {
                            if inside_propstat || inside_prop {
                                return Err(invalid(
                                    "WebDAV-response endet innerhalb von propstat",
                                ));
                            }
                            let resource = current
                                .take()
                                .ok_or_else(|| invalid("WebDAV-response-Ende ohne Anfang"))?;
                            let href = if resource.href.is_empty() {
                                "<fehlender href>".to_owned()
                            } else {
                                resource.href.clone()
                            };
                            if let Err(error) = validate_resource(&resource) {
                                response_semantic_errors.push(error.to_string());
                            }
                            if !response_semantic_errors.is_empty() {
                                multistatus_semantic_errors.push(format!(
                                    "WebDAV-response {href}: {}",
                                    response_semantic_errors.join("; ")
                                ));
                            } else {
                                resources.push(resource);
                            }
                        }
                        b"multistatus" => root_closed = true,
                        _ => {}
                    }
                }
                depth = depth.saturating_sub(1);
                element_stack.pop();
            }
            Event::Eof => break,
            Event::DocType(_) => {
                return Err(invalid("WebDAV-XML darf keine DTD enthalten"));
            }
            _ => {}
        }
        buffer.clear();
    }
    if current.is_some() || !root_seen || !root_closed || depth != 0 || !element_stack.is_empty() {
        return Err(invalid("WebDAV-Multi-Status ist unvollständig oder leer"));
    }
    if !multistatus_semantic_errors.is_empty() {
        return Err(invalid(multistatus_semantic_errors.join("; ")));
    }
    if resources.is_empty() {
        return Err(invalid("WebDAV-Multi-Status ist leer"));
    }
    Ok(resources)
}

fn apply_resource_field(resource: &mut DavResource, field: &[u8], value: String) -> io::Result<()> {
    match field {
        b"href" => {
            // Ein leerer Text ist trotzdem eine vorhandene XML-Angabe. Würden wir
            // Einmaligkeit am String prüfen, könnte ein zweites href das leere
            // erste still „heilen“ und die Ressourcenidentität austauschen.
            if resource.href_seen {
                return Err(invalid("WebDAV-href ist doppelt"));
            }
            resource.href_seen = true;
            resource.href = value;
            Ok(())
        }
        b"getcontentlength" => {
            let length = value
                .parse()
                .map_err(|_| invalid("WebDAV-Länge ist ungültig"))?;
            if resource.length.replace(length).is_some() {
                return Err(invalid("WebDAV-Länge ist doppelt"));
            }
            Ok(())
        }
        b"getlastmodified" => {
            let time = httpdate::parse_http_date(&value)
                .map_err(|_| invalid("WebDAV-Änderungszeit ist ungültig"))?;
            let timestamp = time
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| invalid("WebDAV-Änderungszeit liegt vor der Unix-Epoche"))?;
            let timestamp = i64::try_from(timestamp.as_secs())
                .map_err(|_| invalid("WebDAV-Änderungszeit ist zu groß"))?;
            if resource.mtime.replace(timestamp).is_some() {
                return Err(invalid("WebDAV-Änderungszeit ist doppelt"));
            }
            Ok(())
        }
        b"creationdate" => {
            let time =
                time::OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339)
                    .map_err(|_| invalid("WebDAV-Erstellungszeit ist ungültig"))?;
            if resource.created.replace(time.unix_timestamp()).is_some() {
                return Err(invalid("WebDAV-Erstellungszeit ist doppelt"));
            }
            Ok(())
        }
        b"getetag" => {
            if resource.etag.replace(value).is_some() {
                return Err(invalid("WebDAV-ETag ist doppelt"));
            }
            Ok(())
        }
        _ => Err(invalid("WebDAV-Skalarfeld ist unbekannt")),
    }
}

fn validate_resource(resource: &DavResource) -> io::Result<()> {
    let mut errors = Vec::new();
    if resource.href.is_empty() {
        errors.push("WebDAV-Ressource besitzt keinen href".to_owned());
    }
    if resource.successful_propstats == 0 {
        errors
            .push("WebDAV-Ressource besitzt keinen vollständig erfolgreichen propstat".to_owned());
    }
    if resource.successful_resourcetype.is_none() {
        errors.push(
            "WebDAV-Ressource besitzt kein erfolgreiches resourcetype direkt unter prop".to_owned(),
        );
    }
    if !resource.is_collection {
        if resource.length.is_none() {
            errors.push("WebDAV-Datei enthält keine Länge".to_owned());
        }
        if resource.mtime.is_none() {
            errors.push("WebDAV-Datei enthält keine Änderungszeit".to_owned());
        }
        match resource.etag.as_deref() {
            Some(etag) => {
                if let Err(error) = validate_strong_etag(etag) {
                    errors.push(error.to_string());
                }
            }
            None => errors.push("WebDAV-Datei enthält keine ETag".to_owned()),
        }
    }
    if !errors.is_empty() {
        return Err(invalid(errors.join(", ")));
    }
    Ok(())
}

fn is_dav_namespace(namespace: &ResolveResult<'_>) -> bool {
    matches!(namespace, ResolveResult::Bound(Namespace(value)) if *value == b"DAV:")
}

fn parse_collection_listing(
    payload: &[u8],
    origin: &str,
    requested: &str,
    include_children: bool,
) -> io::Result<Vec<ProofListEntry>> {
    let resources = parse_multistatus(payload)?;
    let requested = normalize_server_path(requested)?;
    let requested_trimmed = requested.trim_end_matches('/');
    let mut saw_self = false;
    let mut names = BTreeSet::new();
    let mut result = Vec::new();
    for resource in resources {
        let path = href_path(origin, &resource.href)?;
        if path.trim_end_matches('/') == requested_trimmed {
            if saw_self {
                return Err(invalid("WebDAV-PROPFIND enthält die Collection doppelt"));
            }
            saw_self = true;
            if include_children && !resource.is_collection {
                return Err(invalid(
                    "WebDAV-PROPFIND-Selbstressource ist keine Collection",
                ));
            }
            if !include_children {
                let name = requested_trimmed
                    .rsplit('/')
                    .next()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("/");
                result.push(resource_entry(name, resource)?);
            }
            continue;
        }
        if !include_children {
            return Err(invalid(
                "WebDAV-Depth-0 enthält eine unerwartete Kindressource",
            ));
        }
        let prefix = format!("{requested_trimmed}/");
        let relative = path
            .trim_end_matches('/')
            .strip_prefix(&prefix)
            .ok_or_else(|| invalid("WebDAV-href liegt außerhalb der angefragten Collection"))?;
        if relative.is_empty() || relative.contains('/') {
            return Err(invalid(
                "WebDAV-Depth-1 enthält keine direkte Kindressource",
            ));
        }
        // `href_path` hat bereits genau einmal dekodiert.
        let name = relative.to_owned();
        validate_name(&name)?;
        if !names.insert(name.clone()) {
            return Err(invalid("WebDAV-PROPFIND enthält einen doppelten Namen"));
        }
        result.push(resource_entry(&name, resource)?);
    }
    if !saw_self {
        return Err(invalid(
            "WebDAV-PROPFIND enthält die angefragte Collection nicht",
        ));
    }
    Ok(result)
}

fn resource_entry(name: &str, resource: DavResource) -> io::Result<ProofListEntry> {
    let is_dir = resource.is_collection;
    let is_file = !is_dir;
    let etag = resource.etag.unwrap_or_default();
    Ok(ProofListEntry {
        raw_name: name.as_bytes().to_vec(),
        safely_decoded: true,
        entry: Entry {
            extension: entry_extension(name, is_file),
            name: name.to_owned(),
            is_dir,
            is_file,
            is_symlink: false,
            size: resource.length.unwrap_or(0),
            mtime: resource.mtime,
            created: resource.created,
            owner: None,
            group: None,
            mode: None,
            device: None,
            inode: None,
            hard_links: None,
        },
        provider_token: Some(etag.into_bytes()),
    })
}

fn status_line_code(value: &str) -> io::Result<u16> {
    let mut parts = value.split_whitespace();
    let version = parts
        .next()
        .ok_or_else(|| invalid("WebDAV-propstat-Status ist unvollständig"))?;
    if version != "HTTP/1.1" {
        return Err(invalid(
            "WebDAV-propstat-Status verwendet keine HTTP/1.1-Statuszeile",
        ));
    }
    let code = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| invalid("WebDAV-propstat-Status ist ungültig"))?;
    Ok(code)
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
            (
                &item.raw_name,
                &item.provider_token,
                item.entry.size,
                item.entry.mtime,
                item.entry.is_dir,
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    let payload = serde_json::to_vec(&(1u32, generation, rows))
        .map_err(|error| invalid(format!("WebDAV-Listingtoken ist nicht bildbar: {error}")))?;
    Ok(blake3::hash(&payload).as_bytes().to_vec())
}

fn finish_bound_webdav_listing(
    entries: Vec<ProofListEntry>,
    generation: u64,
    identity: &WebDavServerIdentity,
    capabilities: RemoteCapabilities,
    current_generation: u64,
    current_identity: &WebDavServerIdentity,
) -> io::Result<(CompleteDirectoryListing, RemoteCapabilities)> {
    if generation != current_generation || identity != current_identity {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "WebDAV-Sitzung oder Root-Identität wechselte während des vollständigen Listings",
        ));
    }
    Ok((
        CompleteDirectoryListing {
            provider_token: Some(listing_token(&entries, generation)?),
            entries,
            complete: true,
        },
        capabilities,
    ))
}

fn complete_bound_webdav_listing_with<S>(
    session: &S,
    generation: u64,
    identity: &WebDavServerIdentity,
    capabilities: RemoteCapabilities,
    read_listing: impl FnOnce(&S) -> io::Result<Vec<ProofListEntry>>,
    current_binding: impl FnOnce() -> (u64, WebDavServerIdentity),
) -> io::Result<(CompleteDirectoryListing, RemoteCapabilities)> {
    // Der Leseaufruf erhält genau die atomar erfasste Sitzung. Erst nach dem
    // vollständigen XML-Parsing wird der aktuelle Providerzustand erneut geprüft.
    let entries = read_listing(session)?;
    let (current_generation, current_identity) = current_binding();
    finish_bound_webdav_listing(
        entries,
        generation,
        identity,
        capabilities,
        current_generation,
        &current_identity,
    )
}

fn href_path(origin: &str, href: &str) -> io::Result<String> {
    let base =
        Url::parse(&format!("{origin}/")).map_err(|_| invalid("WebDAV-Origin ist ungültig"))?;
    let url = base
        .join(href)
        .map_err(|_| invalid("WebDAV-href ist ungültig"))?;
    if url.scheme() != base.scheme()
        || url.host_str() != base.host_str()
        || url.port_or_known_default() != base.port_or_known_default()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("WebDAV-href wechselt die gebundene Origin"));
    }
    let decoded = percent_decode_str(url.path())
        .decode_utf8()
        .map_err(|_| invalid("WebDAV-href-Pfad ist nicht UTF-8"))?;
    normalize_server_path(&decoded)
}

fn resource_url(origin: &str, path: &str) -> io::Result<Url> {
    let mut url =
        Url::parse(&format!("{origin}/")).map_err(|_| invalid("WebDAV-Origin ist ungültig"))?;
    let path = normalize_server_path(path)?;
    {
        // Der interne Pfad ist bereits genau einmal dekodiert. `Url::set_path`
        // würde darin vorhandene Sequenzen wie `%2F` als fertige URL-Kodierung
        // übernehmen und damit einen wörtlichen Dateinamen in einen Pfadtrenner
        // umdeuten. `PathSegmentsMut::push` kodiert dagegen jedes Prozentzeichen
        // und jeden Slash innerhalb einer einzelnen Komponente erneut.
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| invalid("WebDAV-Origin erlaubt keine Pfadsegmente"))?;
        segments.clear();
        for segment in path.split('/').filter(|segment| !segment.is_empty()) {
            segments.push(segment);
        }
    }
    Ok(url)
}

fn normalize_server_path(path: &str) -> io::Result<String> {
    normalize_absolute_text(path)
}

fn normalize_virtual_path(path: &Path) -> io::Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("WebDAV-Pfad ist nicht UTF-8"))?;
    normalize_absolute_text(text)
}

fn normalize_absolute_text(path: &str) -> io::Result<String> {
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err(invalid("WebDAV-Pfad muss sicher und absolut sein"));
    }
    let mut components = Vec::new();
    let parts = path.split('/').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            if index + 1 == parts.len() {
                continue;
            }
            return Err(invalid("WebDAV-Pfad enthält eine leere Komponente"));
        }
        validate_name(part)?;
        components.push(*part);
    }
    if components.is_empty() {
        Ok("/".into())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

fn virtual_components(path: &Path) -> io::Result<Vec<String>> {
    Ok(normalize_virtual_path(path)?
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect())
}

fn map_virtual_path(root: &str, path: &Path) -> io::Result<String> {
    let virtual_path = normalize_virtual_path(path)?;
    if virtual_path == "/" {
        Ok(root.to_owned())
    } else if root == "/" {
        Ok(virtual_path)
    } else {
        Ok(format!("{}{}", root.trim_end_matches('/'), virtual_path))
    }
}

fn join_server_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        return Err(invalid("WebDAV-Name ist ungültig"));
    }
    Ok(())
}

fn connect_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last = None;
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(TLS_IO_TIMEOUT))?;
                stream.set_write_timeout(Some(TLS_IO_TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "WebDAV-Host fehlt")))
}

fn required_header(
    headers: &HeaderMap,
    name: reqwest::header::HeaderName,
    label: &str,
) -> io::Result<String> {
    let values = headers
        .get_all(name)
        .iter()
        .map(|value| value.to_str())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid(format!("WebDAV-{label}-Header ist ungültig")))?;
    if values.is_empty() {
        return Err(invalid(format!("WebDAV-{label}-Header fehlt")));
    }
    Ok(values.join(","))
}

/// Bindet alle rohen `Server`-Headerwerte eindeutig an die Provideridentität.
///
/// HTTP erlaubt in Headerwerten auch Bytes außerhalb von UTF-8. Der freiwillige
/// Identitätshinweis darf die Verbindung deshalb nicht allein wegen einer
/// Textdekodierung sperren. Anzahl und Länge jedes Rohwerts sind Teil des Digests;
/// damit bleiben „fehlt“, „leer“, mehrere Werte und ein einzelner Kommawert
/// voneinander getrennt. Das Versionspräfix verhindert außerdem Kollisionen mit
/// Belegtexten anderer Identitätsfelder.
fn server_header_identity_digest(headers: &HeaderMap) -> [u8; 32] {
    let values = headers.get_all(SERVER).iter().collect::<Vec<_>>();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"doppeldecker:webdav-server-header:v2\0");
    hasher.update(&(values.len() as u64).to_le_bytes());
    for value in values {
        let raw = value.as_bytes();
        hasher.update(&(raw.len() as u64).to_le_bytes());
        hasher.update(raw);
    }
    *hasher.finalize().as_bytes()
}

fn legacy_server_header_identity_digests(headers: &HeaderMap) -> io::Result<Vec<[u8; 32]>> {
    let values = match headers
        .get_all(SERVER)
        .iter()
        .map(|value| value.to_str())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(values) => values,
        // Das alte Schema konnte HTTP-`obs-text` nicht darstellen. Das ist kein
        // Fehler des aktuellen Rohbyte-Schemas: Es gibt hier schlicht keinen
        // authentisierbaren Legacy-Kandidaten.
        Err(_) => return Ok(Vec::new()),
    };
    let joined = values.join(",");
    let direct = *blake3::hash(joined.as_bytes()).as_bytes();
    let compatibility = if values.is_empty() {
        *blake3::hash(b"webdav-server-header:absent").as_bytes()
    } else if joined.is_empty() {
        *blake3::hash(b"webdav-server-header:empty").as_bytes()
    } else {
        direct
    };
    let mut digests = vec![direct];
    if compatibility != direct {
        digests.push(compatibility);
    }
    Ok(digests)
}

fn canonical_dav_header(raw: &str) -> io::Result<String> {
    let mut tokens = raw
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty()
        || tokens
            .iter()
            .any(|token| token.chars().any(char::is_control))
    {
        return Err(invalid("WebDAV-DAV-Header ist ungültig"));
    }
    tokens.sort_unstable();
    tokens.dedup();
    Ok(tokens.join(","))
}

fn require_status(actual: StatusCode, expected: &[StatusCode], action: &str) -> io::Result<()> {
    if expected.contains(&actual) {
        return Ok(());
    }
    let kind = match actual.as_u16() {
        401 | 403 => io::ErrorKind::PermissionDenied,
        404 => io::ErrorKind::NotFound,
        409 | 412 => io::ErrorKind::AlreadyExists,
        507 => io::ErrorKind::StorageFull,
        _ => io::ErrorKind::Other,
    };
    Err(io::Error::new(
        kind,
        format!("{action} antwortete mit HTTP {}", actual.as_u16()),
    ))
}

fn http_error(error: reqwest::Error) -> io::Error {
    if error.is_timeout() {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "WebDAV-Anfrage lief in ein Timeout",
        )
    } else if error.is_connect() {
        io::Error::new(io::ErrorKind::NotConnected, "WebDAV-Verbindung schlug fehl")
    } else if error.is_request() || error.is_body() {
        // Abbruch während einer bereits laufenden Anfrage oder beim Streamen des
        // Bodys: Der Server kann die Mutation (MOVE/PUT/DELETE) schon ausgeführt
        // haben, bevor die Antwort verlorenging. Konservativ als
        // Verbindungsverlust (`ConnectionAborted` → `ErrorCode::ConnectionLost`)
        // einstufen, damit die Exakt-einmal-Klärung den Zustand per Readback
        // klärt, statt dass Aufrufer die Mutation irrtümlich wiederholen.
        io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("WebDAV-Anfrage brach ab: {error}"),
        )
    } else {
        io::Error::other(error.to_string())
    }
}

fn read_only() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Diese WebDAV-VFS-Operation ist nicht verfügbar",
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const VALID: &str = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">
      <D:response><D:href>/dav/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
      <D:response><D:href>/dav/datei.txt</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>4</D:getcontentlength><D:getlastmodified>Sun, 12 Jul 2026 20:00:00 GMT</D:getlastmodified><D:creationdate>2026-07-12T19:30:00Z</D:creationdate><D:getetag>"stark"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    </D:multistatus>"#;

    const OPTIONAL_CREATIONDATE_404: &str = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">
      <D:response><D:href>/dav/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
      <D:response><D:href>/dav/datei.txt</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>4</D:getcontentlength><D:getlastmodified>Sun, 12 Jul 2026 20:00:00 GMT</D:getlastmodified><D:getetag>"stark"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><D:creationdate/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response>
    </D:multistatus>"#;

    #[test]
    fn slash_redirect_bleibt_exakt_an_origin_und_kodierten_pfad_gebunden() {
        let url = resource_url("https://dav.test", "/literal%2F name").unwrap();
        for location in [
            "/literal%252F%20name/",
            "https://dav.test/literal%252F%20name/",
        ] {
            let header = HeaderValue::from_str(location).unwrap();
            let target =
                slash_redirect_target(&url, StatusCode::MOVED_PERMANENTLY, Some(&header)).unwrap();
            assert_eq!(target.path(), "/literal%252F%20name/");
        }
        for location in [
            "https://foreign.test/literal%252F%20name/",
            "/anderer/",
            "/literal%2F%20name/",
            "/literal%252F%20name/?x=1",
            "/literal%252F%20name/#x",
        ] {
            assert!(slash_redirect_target(
                &url,
                StatusCode::TEMPORARY_REDIRECT,
                Some(&HeaderValue::from_str(location).unwrap())
            )
            .is_none());
        }
        assert!(slash_redirect_target(
            &url,
            StatusCode::SEE_OTHER,
            Some(&HeaderValue::from_static("/literal%252F%20name/"))
        )
        .is_none());
    }

    #[test]
    fn zertifikatsdiscovery_verwendet_nackte_ip_adressen() {
        for (origin, host) in [
            ("https://[::1]:443", "::1"),
            ("https://127.0.0.1", "127.0.0.1"),
            ("https://dav.test", "dav.test"),
        ] {
            let actual = discovery_host(&validate_webdav_origin(origin).unwrap());
            assert_eq!(actual, host);
            assert!(ServerName::try_from(actual).is_ok());
        }
    }

    #[test]
    fn standardoptionale_antwortbeschreibung_veraendert_keinen_listingeintrag() {
        let expected =
            parse_collection_listing(VALID.as_bytes(), "https://dav.test", "/dav", true).unwrap();
        for closing in ["</D:multistatus>", "</D:response>", "</D:propstat>"] {
            for description in [
                "<D:responsedescription>OK &amp; fertig</D:responsedescription>",
                "<D:responsedescription><![CDATA[OK <weiter>]]>&#32;fertig</D:responsedescription>",
                "<D:responsedescription/>",
            ] {
                let payload = VALID.replace(closing, &format!("{description}{closing}"));
                let actual =
                    parse_collection_listing(payload.as_bytes(), "https://dav.test", "/dav", true)
                        .unwrap();
                assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
            }
        }
    }

    #[test]
    fn antwortbeschreibung_erlaubt_keine_fremde_struktur_oder_unbegrenzten_text() {
        for payload in [
            VALID.replace("</D:prop>", "<D:responsedescription>OK</D:responsedescription></D:prop>"),
            VALID.replace("</D:multistatus>",
                "<D:responsedescription><D:href>/fremd</D:href></D:responsedescription></D:multistatus>"),
            VALID.replace("</D:multistatus>",
                &format!("<D:responsedescription>{}</D:responsedescription></D:multistatus>", "x".repeat(8193))),
        ] {
            assert!(parse_collection_listing(payload.as_bytes(), "https://dav.test", "/dav", true).is_err());
        }
    }

    #[test]
    fn depth_eins_bindet_selbstressource_name_und_starke_etag() {
        let entries =
            parse_collection_listing(VALID.as_bytes(), "https://dav.test", "/dav", true).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry.name, "datei.txt");
        assert_eq!(entries[0].entry.extension.as_deref(), Some("txt"));
        assert_eq!(
            entries[0].entry.created,
            Some(
                time::OffsetDateTime::parse(
                    "2026-07-12T19:30:00Z",
                    &time::format_description::well_known::Rfc3339,
                )
                .unwrap()
                .unix_timestamp()
            )
        );
        assert_eq!(entries[0].entry.owner, None);
        assert_eq!(entries[0].entry.group, None);
        assert_eq!(
            entries[0].provider_token.as_deref(),
            Some(b"\"stark\"".as_slice())
        );
        let escaped = parse_collection_listing(
            VALID.replace("datei.txt", "datei&amp;mehr.txt").as_bytes(),
            "https://dav.test",
            "/dav",
            true,
        )
        .unwrap();
        assert_eq!(escaped[0].entry.name, "datei&mehr.txt");
    }

    #[test]
    fn optionale_creationdate_darf_in_eigenem_404_propstat_fehlen() {
        let entries = parse_collection_listing(
            OPTIONAL_CREATIONDATE_404.as_bytes(),
            "https://dav.test",
            "/dav",
            true,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry.created, None);

        let missing_etag = OPTIONAL_CREATIONDATE_404
            .replace("<D:getetag>\"stark\"</D:getetag>", "")
            .replace("<D:creationdate/>", "<D:getetag/>");
        assert!(parse_multistatus(missing_etag.as_bytes()).is_err());
    }

    #[test]
    fn schwache_etag_propstat_fehler_und_fremde_origin_stoppen() {
        assert!(parse_multistatus(VALID.replace("\"stark\"", "W/\"schwach\"").as_bytes()).is_err());
        assert!(
            parse_multistatus(VALID.replace("\"stark\"", "\"eins\",\"zwei\"").as_bytes()).is_err()
        );
        assert!(parse_multistatus(VALID.replace("200 OK", "403 Forbidden").as_bytes()).is_err());
        assert!(parse_multistatus(
            VALID
                .replace("2026-07-12T19:30:00Z", "kein-datum")
                .as_bytes()
        )
        .is_err());
        assert!(href_path("https://dav.test", "https://fremd.test/dav/x").is_err());
        assert!(href_path("https://dav.test", "https://nutzer@dav.test/dav/x").is_err());
        assert!(parse_multistatus(
            VALID
                .replace("xmlns:D=\"DAV:\"", "xmlns:D=\"urn:falsch\"")
                .as_bytes()
        )
        .is_err());
        assert!(parse_multistatus(
            VALID
                .replace("\"stark\"", "<X:wert xmlns:X=\"urn:x\">\"stark\"</X:wert>")
                .as_bytes()
        )
        .is_err());
    }

    /// `collection` zählt nur als direktes Kind von `resourcetype`. Sonst könnte
    /// ein Server eine Datei über eine verschachtelte oder namensgleiche fremde
    /// Eigenschaft als Ordner ausgeben — und `validate_resource` überspränge dann
    /// die für Dateien verbindlichen ETag-, Größen- und Zeitprüfungen.
    #[test]
    fn collection_zaehlt_nur_direkt_unter_resourcetype() {
        let datei_mit = |xml: &str| {
            VALID.replace(
                "<D:resourcetype/><D:getcontentlength>",
                &format!("{xml}<D:getcontentlength>"),
            )
        };

        // `collection` darf weder neben `resourcetype` noch unter einer anderen
        // DAV-Eigenschaft stehen. Beide XML-Schreibweisen müssen fail-closed sein.
        for xml in [
            "<D:resourcetype/><D:lockdiscovery><D:collection/></D:lockdiscovery>",
            "<D:resourcetype/><D:lockdiscovery><D:collection></D:collection></D:lockdiscovery>",
        ] {
            assert!(parse_multistatus(datei_mit(xml).as_bytes()).is_err());
        }

        // Ein bloß gesetztes `inside_resourcetype` reicht nicht: Zwischen
        // `resourcetype` und `collection` darf exakt keine weitere XML-Stufe liegen.
        for xml in [
            "<D:resourcetype><D:lockdiscovery><D:collection/></D:lockdiscovery></D:resourcetype>",
            "<D:resourcetype><D:lockdiscovery><D:collection></D:collection></D:lockdiscovery></D:resourcetype>",
        ] {
            assert!(parse_multistatus(datei_mit(xml).as_bytes()).is_err());
        }

        // Umgekehrt zählt auch ein unter `lockdiscovery` verschachteltes
        // `resourcetype` nicht als die zugehörige direkte Dateieigenschaft. Das gilt
        // für ein Start-/Ende-Paar ebenso wie für das leere Element.
        for xml in [
            "<D:lockdiscovery><D:resourcetype><D:collection/></D:resourcetype></D:lockdiscovery>",
            "<D:lockdiscovery><D:resourcetype/></D:lockdiscovery>",
        ] {
            assert!(parse_multistatus(datei_mit(xml).as_bytes()).is_err());
        }

        // Der reguläre Fall bleibt unverändert gültig.
        let entries =
            parse_collection_listing(VALID.as_bytes(), "https://dav.test", "/dav", true).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].entry.is_file);
    }

    #[test]
    fn pflichtfelder_zaehlen_nur_direkt_unter_prop() {
        let felder = [
            ("getcontentlength", "4"),
            ("getlastmodified", "Sun, 12 Jul 2026 20:00:00 GMT"),
            ("creationdate", "2026-07-12T19:30:00Z"),
            ("getetag", "\"stark\""),
        ];
        for (name, wert) in felder {
            let direkt = format!("<D:{name}>{wert}</D:{name}>");

            // Start-/Ende-Element: Ein syntaktisch gültiger Wert unter einer
            // anderen DAV-Eigenschaft darf nicht als Pflichtmetadatum durchrutschen.
            let verschachtelt = VALID.replace(
                &direkt,
                &format!("<D:lockdiscovery>{direkt}</D:lockdiscovery>"),
            );
            let error = parse_multistatus(verschachtelt.as_bytes())
                .err()
                .expect("verschachteltes Pflichtfeld muss scheitern");
            assert!(
                error.to_string().contains("nicht direkt unter prop"),
                "{name}: {error}"
            );

            // Empty-Element: Der direkte gültige Wert bleibt zusätzlich stehen.
            // Damit belegt ausschließlich die falsche XML-Tiefe den Fehler, nicht
            // erst ein leerer Zahlen-, Datums- oder ETag-Wert.
            let leer_verschachtelt = VALID.replace(
                &direkt,
                &format!("<D:lockdiscovery><D:{name}/></D:lockdiscovery>{direkt}"),
            );
            let error = parse_multistatus(leer_verschachtelt.as_bytes())
                .err()
                .expect("verschachteltes leeres Pflichtfeld muss scheitern");
            assert!(
                error.to_string().contains("nicht direkt unter prop"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn jede_ressource_braucht_erfolgreiches_direktes_resourcetype() {
        let datei_prefix = "<D:response><D:href>/dav/datei.txt</D:href><D:propstat>";
        let ohne_resourcetype = VALID.replace(
            &format!("{datei_prefix}<D:prop><D:resourcetype/>"),
            &format!("{datei_prefix}<D:prop>"),
        );
        assert!(parse_multistatus(ohne_resourcetype.as_bytes()).is_err());

        // Andere Eigenschaften dürfen auf mehrere erfolgreiche propstat-Blöcke
        // verteilt sein. Das direkte resourcetype gilt dabei für die Ressource.
        let geteilt = VALID.replace(
            "<D:creationdate>2026-07-12T19:30:00Z</D:creationdate><D:getetag>",
            "<D:getetag>",
        ).replace(
            "</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n    </D:multistatus>",
            "</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><D:creationdate>2026-07-12T19:30:00Z</D:creationdate></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n    </D:multistatus>",
        );
        assert!(parse_multistatus(geteilt.as_bytes()).is_ok());
        let geteilt_mit_konsistentem_typ = geteilt.replace(
            "<D:prop><D:creationdate>",
            "<D:prop><D:resourcetype/><D:creationdate>",
        );
        assert!(parse_multistatus(geteilt_mit_konsistentem_typ.as_bytes()).is_err());

        // `resourcetype` außerhalb von `prop` zählt weder als Start-/Ende-Paar
        // noch als Empty-Element und wird als Strukturfehler abgelehnt.
        for ausserhalb in [
            "<D:resourcetype></D:resourcetype><D:prop>",
            "<D:resourcetype/><D:prop>",
        ] {
            let payload = VALID.replace(
                &format!("{datei_prefix}<D:prop><D:resourcetype/>"),
                &format!("{datei_prefix}{ausserhalb}"),
            );
            let error = parse_multistatus(payload.as_bytes())
                .err()
                .expect("resourcetype außerhalb von prop muss scheitern");
            assert!(
                error.to_string().contains("nicht direkt unter prop"),
                "{error}"
            );
        }
    }

    #[test]
    fn strukturknoten_muessen_direkte_dav_kinder_sein() {
        let response = "<D:response><D:href>/dav/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>";
        for wrapped in [
            format!("<D:lockdiscovery>{response}</D:lockdiscovery>"),
            format!("<X:wrapper xmlns:X=\"urn:test\">{response}</X:wrapper>"),
            format!("<D:lockdiscovery/>{response}"),
            format!("<X:wrapper xmlns:X=\"urn:test\"/>{response}"),
        ] {
            let payload = format!("<D:multistatus xmlns:D=\"DAV:\">{wrapped}</D:multistatus>");
            assert!(parse_multistatus(payload.as_bytes()).is_err(), "{payload}");
        }

        for payload in [
            VALID.replace(
                "<D:href>/dav/</D:href>",
                "<D:lockdiscovery><D:href>/dav/</D:href></D:lockdiscovery>",
            ),
            VALID.replace(
                "<D:href>/dav/</D:href>",
                "<X:wrapper xmlns:X=\"urn:test\"><D:href>/dav/</D:href></X:wrapper>",
            ),
            VALID
                .replace(
                    "<D:propstat><D:prop>",
                    "<D:lockdiscovery><D:propstat><D:prop>",
                )
                .replace(
                    "</D:propstat></D:response>",
                    "</D:propstat></D:lockdiscovery></D:response>",
                ),
            VALID
                .replace(
                    "<D:propstat><D:prop>",
                    "<X:wrapper xmlns:X=\"urn:test\"><D:propstat><D:prop>",
                )
                .replace(
                    "</D:propstat></D:response>",
                    "</D:propstat></X:wrapper></D:response>",
                ),
            VALID
                .replace(
                    "<D:prop><D:resourcetype>",
                    "<D:lockdiscovery><D:prop><D:resourcetype>",
                )
                .replace(
                    "</D:prop><D:status>",
                    "</D:prop></D:lockdiscovery><D:status>",
                ),
            VALID
                .replace(
                    "<D:prop><D:resourcetype>",
                    "<X:wrapper xmlns:X=\"urn:test\"><D:prop><D:resourcetype>",
                )
                .replace("</D:prop><D:status>", "</D:prop></X:wrapper><D:status>"),
            VALID.replace("<D:response><D:href>", "<D:response/><D:response><D:href>"),
            VALID.replace("<D:href>/dav/</D:href>", "<D:href/><D:href>/dav/</D:href>"),
            VALID.replace("<D:propstat><D:prop>", "<D:propstat/><D:propstat><D:prop>"),
            VALID.replace(
                "<D:prop><D:resourcetype>",
                "<D:prop/><D:prop><D:resourcetype>",
            ),
        ] {
            assert!(parse_multistatus(payload.as_bytes()).is_err(), "{payload}");
        }
    }

    #[test]
    fn response_braucht_genau_ein_erfolgreiches_resourcetype() {
        let duplicate = VALID.replace(
            "<D:resourcetype/><D:getcontentlength>",
            "<D:resourcetype/><D:resourcetype/><D:getcontentlength>",
        );
        assert!(parse_multistatus(duplicate.as_bytes()).is_err());

        let contradictory = VALID.replace(
            "<D:creationdate>2026-07-12T19:30:00Z</D:creationdate><D:getetag>",
            "<D:getetag>",
        ).replace(
            "</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n    </D:multistatus>",
            "</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:creationdate>2026-07-12T19:30:00Z</D:creationdate></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n    </D:multistatus>",
        );
        assert!(parse_multistatus(contradictory.as_bytes()).is_err());
    }

    #[test]
    fn erfolgreicher_propstat_besitzt_genau_ein_direktes_prop() {
        // Ohne die direkte Kardinalitätsprüfung würden beide Blöcke in denselben
        // Zwischenspeicher laufen und zusammen wie ein einziges gültiges `prop`
        // aussehen. Die XML-Struktur selbst muss den zweiten Block stoppen.
        let split = VALID.replace(
            "<D:resourcetype/><D:getcontentlength>4</D:getcontentlength>",
            "<D:resourcetype/></D:prop><D:prop><D:getcontentlength>4</D:getcontentlength>",
        );
        let error = parse_multistatus(split.as_bytes())
            .err()
            .expect("zweites direktes prop muss ein Strukturfehler sein");
        assert!(error.to_string().contains("prop"), "{error}");
    }

    #[test]
    fn semantische_propstat_fehler_werden_mit_href_und_index_gesammelt() {
        let payload = r#"<D:multistatus xmlns:D="DAV:"><D:response>
          <D:href>/dav/kaputt.txt</D:href>
          <D:propstat><D:prop><D:resourcetype/><D:getcontentlength>4</D:getcontentlength></D:prop><D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat>
          <D:propstat><D:prop><D:resourcetype/><D:getlastmodified>Sun, 12 Jul 2026 20:00:00 GMT</D:getlastmodified><D:getetag>"stark"</D:getetag></D:prop><D:status>HTTP/1.1 500 Internal Server Error</D:status></D:propstat>
        </D:response><D:response>
          <D:href>/dav/zweiter-fehler.txt</D:href>
          <D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 409 Conflict</D:status></D:propstat>
        </D:response></D:multistatus>"#;
        let error = parse_multistatus(payload.as_bytes())
            .err()
            .expect("semantisch fehlerhafte propstats müssen scheitern");
        let message = error.to_string();
        for expected in [
            "/dav/kaputt.txt",
            "propstat 1",
            "403",
            "propstat 2",
            "500",
            "keinen vollständig erfolgreichen propstat",
            "/dav/zweiter-fehler.txt",
            "409",
        ] {
            assert!(message.contains(expected), "fehlte {expected:?}: {message}");
        }
    }

    #[test]
    fn doppelte_href_und_pflichtfehler_werden_ueber_alle_responses_gesammelt() {
        let payload = r#"<D:multistatus xmlns:D="DAV:">
          <D:response>
            <D:href>/dav/erste.txt</D:href><D:href>/dav/fremde-zweite.txt</D:href>
            <D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
          </D:response>
          <D:response>
            <D:href>/dav/spaeter.txt</D:href>
            <D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 409 Conflict</D:status></D:propstat>
          </D:response>
        </D:multistatus>"#;
        let error = parse_multistatus(payload.as_bytes())
            .err()
            .expect("alle semantischen Response-Fehler müssen gemeinsam scheitern");
        let message = error.to_string();
        for expected in [
            "/dav/erste.txt",
            "href",
            "Länge",
            "Änderungszeit",
            "ETag",
            "/dav/spaeter.txt",
            "409",
        ] {
            assert!(message.contains(expected), "fehlte {expected:?}: {message}");
        }
    }

    #[test]
    fn leeres_href_bleibt_beim_zweiten_href_eine_doppelte_angabe() {
        let payload = r#"<D:multistatus xmlns:D="DAV:">
          <D:response>
            <D:href></D:href><D:href>/dav/darf-nicht-uebernehmen.txt</D:href>
            <D:propstat><D:prop>
              <D:resourcetype/><D:getcontentlength>1</D:getcontentlength>
              <D:getlastmodified>Wed, 21 Oct 2015 07:28:00 GMT</D:getlastmodified>
              <D:getetag>"erste"</D:getetag>
            </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
          </D:response>
          <D:response>
            <D:href>/dav/spaeter.txt</D:href>
            <D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 409 Conflict</D:status></D:propstat>
          </D:response>
        </D:multistatus>"#;
        let error = parse_multistatus(payload.as_bytes())
            .err()
            .expect("ein zweites href darf ein leeres erstes href nicht heilen");
        let message = error.to_string();
        for expected in [
            "<fehlender href>",
            "href ist doppelt",
            "keinen href",
            "/dav/spaeter.txt",
            "409",
        ] {
            assert!(message.contains(expected), "fehlte {expected:?}: {message}");
        }
        assert!(
            !message.contains("darf-nicht-uebernehmen"),
            "das zweite href wurde trotz erster Angabe als Identität übernommen: {message}"
        );
    }

    #[test]
    fn leeres_empty_href_sammelt_duplikat_und_spaetere_responsefehler() {
        let payload = r#"<D:multistatus xmlns:D="DAV:">
          <D:response>
            <D:href/><D:href>/dav/darf-nicht-uebernehmen.txt</D:href>
            <D:propstat><D:prop>
              <D:resourcetype/><D:getcontentlength>1</D:getcontentlength>
              <D:getlastmodified>Wed, 21 Oct 2015 07:28:00 GMT</D:getlastmodified>
              <D:getetag>"erste"</D:getetag>
            </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
          </D:response>
          <D:response>
            <D:href>/dav/spaeter-409.txt</D:href>
            <D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 409 Conflict</D:status></D:propstat>
          </D:response>
          <D:response>
            <D:href>/dav/spaeter-unvollstaendig.txt</D:href>
            <D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
          </D:response>
        </D:multistatus>"#;
        let error = parse_multistatus(payload.as_bytes())
            .err()
            .expect("ein leeres Empty-href und alle späteren Semantikfehler müssen scheitern");
        let message = error.to_string();
        for expected in [
            "<fehlender href>",
            "href ist doppelt",
            "keinen href",
            "/dav/spaeter-409.txt",
            "409",
            "/dav/spaeter-unvollstaendig.txt",
            "Länge",
            "Änderungszeit",
            "ETag",
        ] {
            assert!(message.contains(expected), "fehlte {expected:?}: {message}");
        }
        assert!(
            !message.contains("darf-nicht-uebernehmen"),
            "das zweite href wurde trotz erster Empty-Angabe übernommen: {message}"
        );
    }

    #[test]
    fn cdata_skalarwerte_binden_href_status_und_pflichtfelder() {
        let payload = r#"<D:multistatus xmlns:D="DAV:">
          <D:response>
            <D:href><![CDATA[/dav/cdata.txt]]></D:href>
            <D:propstat><D:prop>
              <D:resourcetype/>
              <D:getcontentlength><![CDATA[17]]></D:getcontentlength>
              <D:getlastmodified><![CDATA[Wed, 21 Oct 2015 07:28:00 GMT]]></D:getlastmodified>
              <D:getetag><![CDATA["cdata-etag"]]></D:getetag>
            </D:prop><D:status><![CDATA[HTTP/1.1 200 OK]]></D:status></D:propstat>
          </D:response>
        </D:multistatus>"#;
        let resources = parse_multistatus(payload.as_bytes())
            .expect("CDATA muss in direkten WebDAV-Skalarfeldern wie Text gelten");
        assert_eq!(resources.len(), 1);
        let resource = &resources[0];
        assert_eq!(resource.href, "/dav/cdata.txt");
        assert_eq!(resource.length, Some(17));
        assert_eq!(resource.mtime, Some(1_445_412_480));
        assert_eq!(resource.etag.as_deref(), Some("\"cdata-etag\""));
    }

    #[test]
    fn gemischte_xml_skalarfragmente_bewahren_alle_leerzeichen() {
        let payload = VALID.replace("/dav/datei.txt", "/dav/a<![CDATA[ b ]]>&amp; c.txt");
        let resources = parse_multistatus(payload.as_bytes())
            .expect("Text, CDATA und Referenzen dürfen den href nicht umformatieren");
        assert_eq!(resources[1].href, "/dav/a b & c.txt");
    }

    #[test]
    fn xml_referenzen_brauchen_response_und_bleiben_auf_8192_bytes_begrenzt() {
        let outside_response = VALID.replacen("<D:response>", "&amp;<D:response>", 1);
        assert!(
            parse_multistatus(outside_response.as_bytes()).is_err(),
            "Eine Entität außerhalb von response darf nicht still verschwinden"
        );

        let oversized_href = VALID.replace("/dav/", &format!("/dav/{}", "&amp;".repeat(8193)));
        let error = parse_multistatus(oversized_href.as_bytes())
            .err()
            .expect("Auch einzeln dekodierte Referenzen müssen die Skalargrenze einhalten");
        assert!(
            error.to_string().contains("Größenbegrenzung"),
            "unerwarteter Fehler: {error}"
        );
    }

    /// Regression: Der `Server`-Header ist laut HTTP freiwillig. Fehlte er, bildete
    /// die Sitzung ihren Digest über einen leeren Text — und genau den lehnt die
    /// Identitätsprüfung als „bindet nichts" ab. Ein ansonsten gültiger WebDAV-Server
    /// war dadurch überhaupt nicht mehr verbindbar.
    #[test]
    fn fehlender_server_header_bleibt_eine_gueltige_identitaet() {
        let empty_input = *blake3::hash(&[]).as_bytes();

        let mut headers = HeaderMap::new();
        let absent = server_header_identity_digest(&headers);
        assert_ne!(
            absent, empty_input,
            "die Abwesenheit des Headers braucht einen eigenen Beleg"
        );

        headers.insert(SERVER, HeaderValue::from_static(""));
        let empty_value = server_header_identity_digest(&headers);
        assert_ne!(empty_value, empty_input);
        assert_ne!(
            empty_value, absent,
            "ein leerer Headerwert ist etwas anderes als ein fehlender Header"
        );

        let mut named_headers = HeaderMap::new();
        named_headers.insert(SERVER, HeaderValue::from_static("Apache/2.4"));
        let named = server_header_identity_digest(&named_headers);
        assert_ne!(named, absent);
        assert_ne!(named, empty_value);

        let mut literal_absent = HeaderMap::new();
        literal_absent.insert(
            SERVER,
            HeaderValue::from_static("webdav-server-header:absent"),
        );
        assert_ne!(
            server_header_identity_digest(&literal_absent),
            absent,
            "ein Headerwert darf nicht mit dem Beleg für Abwesenheit kollidieren"
        );

        let mut two_empty = HeaderMap::new();
        two_empty.append(SERVER, HeaderValue::from_static(""));
        two_empty.append(SERVER, HeaderValue::from_static(""));
        let mut comma = HeaderMap::new();
        comma.insert(SERVER, HeaderValue::from_static(","));
        assert_ne!(
            server_header_identity_digest(&two_empty),
            server_header_identity_digest(&comma),
            "Anzahl und Grenzen mehrerer Headerwerte müssen gebunden bleiben"
        );

        let mut obs_text = HeaderMap::new();
        obs_text.insert(SERVER, HeaderValue::from_bytes(b"Server-\xff").unwrap());
        let obs_digest = server_header_identity_digest(&obs_text);
        assert_ne!(obs_digest, absent);
        assert_ne!(obs_digest, empty_input);
        assert!(legacy_server_header_identity_digests(&obs_text)
            .expect("ein nicht als Text dekodierbarer Alt-Header darf die aktuelle Identität nicht sperren")
            .is_empty());

        let identity = WebDavServerIdentity {
            origin: "https://dav.test".to_owned(),
            certificate: WebDavCertificatePin { sha256: [1; 32] },
            account_digest: [2; 32],
            canonical_root: "/dav".to_owned(),
            dav_header_digest: [3; 32],
            server_header_digest: absent,
            legacy_server_header_digests: Vec::new(),
        };
        identity
            .validate()
            .expect("ein Server ohne Server-Header muss verbindbar bleiben");
    }

    #[test]
    fn legacy_server_header_digests_rekonstruieren_die_ausgelieferten_schema_eins_bytes() {
        let absent = HeaderMap::new();
        let absent_digests = legacy_server_header_identity_digests(&absent).unwrap();
        assert_eq!(absent_digests.len(), 2);
        assert_eq!(
            blake3::Hash::from_bytes(absent_digests[0])
                .to_hex()
                .as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            blake3::Hash::from_bytes(absent_digests[1])
                .to_hex()
                .as_str(),
            "a1ab5ee3753b33d2b6ac53bce398db5766f4f63a56a8db08172a4665c1c8c0c6"
        );

        let mut empty = HeaderMap::new();
        empty.insert(SERVER, HeaderValue::from_static(""));
        let empty_digests = legacy_server_header_identity_digests(&empty).unwrap();
        assert_eq!(empty_digests.len(), 2);
        assert_eq!(
            blake3::Hash::from_bytes(empty_digests[0]).to_hex().as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            blake3::Hash::from_bytes(empty_digests[1]).to_hex().as_str(),
            "669fe1e5fc4f01e2cdafa48199b7c41b8bb66ab23409bccbe1b60964c0cda2ec"
        );
    }

    #[test]
    fn webdav_listing_verwirft_reconnect_vor_der_tokenbildung() {
        let identity = WebDavServerIdentity {
            origin: "https://dav.test".to_owned(),
            certificate: WebDavCertificatePin { sha256: [1; 32] },
            account_digest: [2; 32],
            canonical_root: "/dav".to_owned(),
            dav_header_digest: [3; 32],
            server_header_digest: [4; 32],
            legacy_server_header_digests: Vec::new(),
        };
        let capabilities = RemoteCapabilities {
            complete_listing: true,
            ..RemoteCapabilities::default()
        };
        let state = Mutex::new((7, identity.clone()));

        let error = complete_bound_webdav_listing_with(
            &17,
            7,
            &identity,
            capabilities,
            |session| {
                assert_eq!(*session, 17, "PROPFIND erhielt nicht die gebundene Sitzung");
                *lock(&state) = (8, identity.clone());
                Ok(Vec::new())
            },
            || {
                let state = lock(&state);
                (state.0, state.1.clone())
            },
        )
        .expect_err("Ein Reconnect zwischen Listing und Token muss das Ergebnis verwerfen");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);

        let mut changed_identity = identity.clone();
        changed_identity.canonical_root = "/anderer-root".to_owned();
        let error = complete_bound_webdav_listing_with(
            &23,
            7,
            &identity,
            capabilities,
            |session| {
                assert_eq!(*session, 23);
                Ok(Vec::new())
            },
            || (7, changed_identity),
        )
        .expect_err("Eine geänderte WebDAV-Identität muss das Listing verwerfen");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn mutationsbindung_verwendet_gate_session_root_und_generation_aus_einem_snapshot() {
        let capabilities = RemoteCapabilities {
            exclusive_directory_create: true,
            complete_listing: true,
            ..RemoteCapabilities::default()
        };
        let snapshots = Mutex::new(VecDeque::from([
            (17_u8, "/gebunden".to_owned(), 4_u64, capabilities),
            (
                23_u8,
                "/neu".to_owned(),
                5_u64,
                RemoteCapabilities::default(),
            ),
        ]));
        let (session, root, generation, captured_capabilities) = bind_mutation_snapshot_with(
            || {
                lock(&snapshots)
                    .pop_front()
                    .expect("nur ein Snapshot erlaubt")
            },
            RemoteCapabilities::permits_directory_create,
            "WebDAV-MKCOL fehlt",
        )
        .unwrap();
        assert_eq!(session, 17, "Mutation erhielt eine ungeprüfte neue Sitzung");
        assert_eq!(root, "/gebunden");
        assert_eq!(generation, 4);
        assert_eq!(captured_capabilities, capabilities);
        assert_eq!(lock(&snapshots).len(), 1, "State wurde mehrmals gelesen");
    }

    #[test]
    fn delete_204_braucht_einen_abwesenheitsbeleg() {
        let still_present = ProofListEntry {
            entry: Entry {
                name: "datei.txt".into(),
                is_dir: false,
                is_file: true,
                is_symlink: false,
                size: 4,
                mtime: None,
                created: None,
                owner: None,
                group: None,
                extension: Some("txt".into()),
                mode: None,
                device: None,
                inode: None,
                hard_links: None,
            },
            raw_name: b"datei.txt".to_vec(),
            safely_decoded: true,
            provider_token: Some(b"\"etag\"".to_vec()),
        };

        let error = require_absent_after_delete_readback(
            Ok(still_present),
            "/dav/datei.txt",
            "WebDAV-Datei-DELETE",
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(require_absent_after_delete_readback(
            Err(io::Error::new(io::ErrorKind::NotFound, "fehlt")),
            "/dav/datei.txt",
            "WebDAV-Datei-DELETE",
        )
        .is_ok());
    }

    #[test]
    fn nichtfeld_xmlwerte_sind_nur_als_whitespace_oder_fremdproperty_erlaubt() {
        for injected in ["Text", "<![CDATA[Text]]>", "&amp;"] {
            let payload = VALID.replacen(
                "<D:href>/dav/</D:href>",
                &format!("{injected}<D:href>/dav/</D:href>"),
                1,
            );
            assert!(
                parse_multistatus(payload.as_bytes()).is_err(),
                "Nichtfeldwert wurde außerhalb einer fremden Property verworfen: {injected}"
            );
        }

        let harmless_foreign = VALID.replacen(
            "<D:resourcetype>",
            "<X:extra xmlns:X=\"urn:test\">Text&amp;<![CDATA[mehr]]></X:extra><D:resourcetype>",
            1,
        );
        assert!(parse_multistatus(harmless_foreign.as_bytes()).is_ok());

        let unknown_reference = VALID.replacen(
            "<D:resourcetype>",
            "<X:extra xmlns:X=\"urn:test\">&unbekannt;</X:extra><D:resourcetype>",
            1,
        );
        assert!(
            parse_multistatus(unknown_reference.as_bytes()).is_err(),
            "Auch in einer fremden Property muss eine Referenz zuerst aufgelöst werden"
        );
    }

    #[test]
    fn fremde_zusatzproperty_bleibt_isoliert() {
        let harmless = VALID.replace(
            "<D:resourcetype>",
            "<X:extra xmlns:X=\"urn:test\"><X:wert/></X:extra><D:resourcetype>",
        );
        assert!(parse_multistatus(harmless.as_bytes()).is_ok());

        let harmless_empty = VALID.replace(
            "<D:resourcetype>",
            "<X:extra xmlns:X=\"urn:test\"/><D:resourcetype>",
        );
        assert!(parse_multistatus(harmless_empty.as_bytes()).is_ok());

        let smuggled = VALID.replace(
            "<D:resourcetype/>",
            "<X:extra xmlns:X=\"urn:test\"><D:getetag>\"fremd\"</D:getetag></X:extra><D:resourcetype/>",
        );
        assert!(parse_multistatus(smuggled.as_bytes()).is_err());

        let foreign_type = VALID.replace(
            "<D:resourcetype><D:collection/></D:resourcetype>",
            "<D:resourcetype><X:type xmlns:X=\"urn:test\"/><D:collection/></D:resourcetype>",
        );
        assert!(parse_multistatus(foreign_type.as_bytes()).is_err());

        let ambiguous_dav_type = VALID.replace(
            "<D:resourcetype><D:collection/></D:resourcetype>",
            "<D:resourcetype><D:lockdiscovery/><D:collection/></D:resourcetype>",
        );
        assert!(parse_multistatus(ambiguous_dav_type.as_bytes()).is_err());
    }

    #[test]
    fn virtuelle_pfade_bleiben_im_kanonischen_root() {
        assert_eq!(
            map_virtual_path("/dav", Path::new("/ordner/datei")).unwrap(),
            "/dav/ordner/datei"
        );
        assert!(map_virtual_path("/dav", Path::new("/../ausbruch")).is_err());
        assert!(map_virtual_path("/dav", Path::new("/./mehrdeutig")).is_err());
        assert!(normalize_server_path("/dav//mehrdeutig").is_err());
        assert_eq!(canonical_dav_header("2, 1, 2").unwrap(), "1,2");
    }

    #[test]
    fn ausgehende_url_kodiert_prozentsequenz_als_wortlaut() {
        // Ein Server liefert den wörtlichen Namen `literal%2F.txt` im href als
        // `%252F`. Nach genau einmaligem Dekodieren muss der ausgehende Request
        // das Prozentzeichen erneut kodieren; `%2F` wäre dort ein Pfadtrenner.
        let path = href_path("https://dav.test", "https://dav.test/dav/literal%252F.txt").unwrap();
        assert_eq!(path, "/dav/literal%2F.txt");

        let url = resource_url("https://dav.test", &path).unwrap();
        assert_eq!(url.as_str(), "https://dav.test/dav/literal%252F.txt");
    }
}
