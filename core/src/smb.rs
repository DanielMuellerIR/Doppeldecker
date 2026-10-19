//! Verschlüsselter SMB3-Provider mit vollständigem Paging und stabiler Identität.
//!
//! Der Provider verwendet keine Mounts und keine C-Bibliothek. Jeder Pfad bleibt in
//! der gebundenen Freigabe, DFS wird fail-closed abgelehnt und Reparse-Points werden
//! als Symlinks sichtbar gemacht. Reale Schreibfähigkeit entsteht erst nach einer
//! künstlichen Create-/FLUSH-/Rename-/Readback-Probe.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

use smb2::client::connection::Connection;
use smb2::msg::create::{
    CreateDisposition, CreateRequest, CreateResponse, ImpersonationLevel, ShareAccess,
};
use smb2::msg::flush::FlushRequest;
use smb2::msg::query_directory::{
    FileInformationClass, QueryDirectoryFlags, QueryDirectoryRequest, QueryDirectoryResponse,
};
use smb2::msg::query_info::{InfoType, QueryInfoRequest, QueryInfoResponse};
use smb2::msg::read::{ReadRequest, ReadResponse, SMB2_CHANNEL_NONE};
use smb2::msg::set_info::SetInfoRequest;
use smb2::msg::write::{WriteRequest, WriteResponse};
use smb2::pack::{FileTime, Guid, ReadCursor, Unpack};
use smb2::types::flags::FileAccessMask;
use smb2::types::status::NtStatus;
use smb2::types::{Command, CreditCharge, Dialect, FileId, OplockLevel};
use smb2::{Session, Tree};
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

use crate::capability_probe_required;
use crate::metadata::{AclModel, Capability, MetadataCapabilities};
use crate::remote::{RemoteCapabilities, SmbConnectionConfig, SmbRootIdentity, SmbServerIdentity};
use crate::sftp::SecretResolver;
use crate::vfs::{entry_extension, CompleteDirectoryListing, Entry, ProofListEntry, Vfs};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;
const FILE_BASIC_INFORMATION: u8 = 4;
const FILE_STANDARD_INFORMATION: u8 = 5;
const FILE_INTERNAL_INFORMATION: u8 = 6;
const FILE_RENAME_INFORMATION: u8 = 10;
const MAX_DIRECTORY_RESPONSE: u32 = 64 * 1024;
const MAX_IO_CHUNK: u32 = 8 * 1024 * 1024;
const MAX_PATH_UTF16_UNITS: usize = u16::MAX as usize / 2;

struct SmbSession {
    connection: Connection,
    tree: Arc<Tree>,
    identity: SmbServerIdentity,
}

struct ProviderState {
    session: SmbSession,
    generation: u64,
    capabilities: RemoteCapabilities,
}

/// Ein an genau eine verschlüsselte SMB3-Freigabe gebundenes VFS.
pub struct SmbVfs {
    runtime: Arc<Runtime>,
    config: SmbConnectionConfig,
    secrets: Arc<dyn SecretResolver>,
    state: Mutex<ProviderState>,
    reconnect: Mutex<()>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SmbServerDiscovery {
    pub server_guid: [u8; 16],
    pub dialect: u16,
    pub encryption_capable: bool,
    pub signing_required: bool,
}

/// Ermittelt ausschließlich die öffentliche NEGOTIATE-Identität. Es findet weder
/// Anmeldung noch Freigabe- oder Dateizugriff statt.
pub fn discover_smb_server(host: &str, port: u16) -> io::Result<SmbServerDiscovery> {
    crate::remote::validate_host(host)?;
    if port == 0 {
        return Err(invalid("SMB-Port 0 ist ungültig"));
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let runtime = RuntimeBuilder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut connection = Connection::connect(&format!("{host}:{port}"), CONNECT_TIMEOUT)
            .await
            .map_err(smb_error)?;
        connection.negotiate().await.map_err(smb_error)?;
        let params = connection
            .params()
            .ok_or_else(|| invalid("SMB-NEGOTIATE lieferte keine Parameter"))?;
        Ok(SmbServerDiscovery {
            server_guid: guid_bytes(params.server_guid),
            dialect: params.dialect as u16,
            // Bei SMB 3.1.1 beweist der vom Server ausgewählte
            // SMB2_ENCRYPTION_CAPABILITIES-Kontext die Verschlüsselung. Das alte
            // globale Bit ist im NEGOTIATE-Response nur für 3.0/3.0.2 definiert.
            encryption_capable: params.cipher.is_some(),
            signing_required: params.signing_required,
        })
    })
}

/// Ein interaktives Pane-Listing samt der Generation, unter der es entstanden ist.
/// Die Generation verhindert eine Vermischung mit einem parallelen Reconnect, ist
/// aber ausdrücklich kein Namespace- oder Identitätsbeleg.
#[derive(Clone, Debug)]
pub struct SmbInteractiveDirectoryListing {
    pub entries: Vec<Entry>,
    pub session_generation: u64,
}

impl SmbVfs {
    pub fn connect(
        config: SmbConnectionConfig,
        secrets: Arc<dyn SecretResolver>,
    ) -> io::Result<Self> {
        config.validate()?;
        let runtime = Arc::new(
            RuntimeBuilder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("doppeldecker-smb")
                .build()?,
        );
        let session = runtime.block_on(connect_once(&config, secrets.as_ref()))?;
        Ok(Self {
            runtime,
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

    pub fn identity(&self) -> SmbServerIdentity {
        lock(&self.state).session.identity.clone()
    }

    pub fn session_generation(&self) -> u64 {
        lock(&self.state).generation
    }

    pub fn capabilities(&self) -> RemoteCapabilities {
        lock(&self.state).capabilities
    }

    pub fn reconnect(&self) -> io::Result<()> {
        let _claim = lock(&self.reconnect);
        let expected = self.identity();
        let session = self
            .runtime
            .block_on(connect_once(&self.config, self.secrets.as_ref()))?;
        if session.identity != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SMB-Identität änderte sich beim Reconnect",
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

    /// Prüft exklusives Verzeichnis-Create unabhängig vom Datei-FLUSH-/Rename-Vertrag.
    pub fn probe_directory_create_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_directory_create_capability_at(Path::new("/"))
    }

    pub fn probe_directory_create_capability_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        let generation = self.session_generation();
        let base = probe_root.join(format!(".doppeldecker-mkdir-probe-{}", random_hex()?));
        self.create_probe_directory(&base, generation)?;
        let result = (|| {
            match self.create_dir_unchecked(&base) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Ok(()) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "SMB-Verzeichnis-Create akzeptierte einen vorhandenen Namen",
                    ));
                }
                Err(error) => return Err(error),
            }
            let entry = self.stat_unchecked(&base)?;
            if !entry.is_dir || entry.is_symlink {
                return Err(invalid("SMB-mkdir-Readback meldet kein echtes Verzeichnis"));
            }
            Ok(())
        })();
        let cleanup = cleanup_after_probe(&result, &base, || self.remove_dir_unchecked(&base));
        self.finish_probe("SMB-mkdir", &base, generation, result, cleanup)?;
        let mut state = lock(&self.state);
        if state.generation != generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "SMB-Sitzung wechselte während der mkdir-Probe",
            ));
        }
        state.capabilities.exclusive_directory_create = true;
        Ok(state.capabilities)
    }

    /// Prüft No-Replace-Rename ausschließlich mit selbst angelegten Verzeichnissen.
    pub fn probe_rename_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_rename_capability_at(Path::new("/"))
    }

    pub fn probe_rename_capability_at(&self, probe_root: &Path) -> io::Result<RemoteCapabilities> {
        let generation = self.session_generation();
        let base = probe_root.join(format!(".doppeldecker-rename-probe-{}", random_hex()?));
        let first = base.join("first");
        let renamed = base.join("renamed");
        let source = base.join("source");
        let collision = base.join("collision");
        self.create_probe_directory(&base, generation)?;
        let result = (|| {
            self.create_dir_unchecked(&first)?;
            self.rename_noreplace_unchecked(&first, &renamed)?;
            self.create_dir_unchecked(&source)?;
            self.create_dir_unchecked(&collision)?;
            match self.rename_noreplace_unchecked(&source, &collision) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Ok(()) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "SMB-No-Replace-Rename ersetzte einen vorhandenen Eintrag",
                    ));
                }
                Err(error) => return Err(error),
            }
            if !self.stat_unchecked(&source)?.is_dir || !self.stat_unchecked(&collision)?.is_dir {
                return Err(invalid("SMB-Rename-Kollision veränderte den Bestand"));
            }
            Ok(())
        })();
        let cleanup = cleanup_after_probe(&result, &base, || {
            self.cleanup_probe(
                &base,
                &[
                    ("first", true),
                    ("renamed", true),
                    ("source", true),
                    ("collision", true),
                ],
            )
        });
        self.finish_probe("SMB-Rename", &base, generation, result, cleanup)?;
        let mut state = lock(&self.state);
        if state.generation != generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "SMB-Sitzung wechselte während der Rename-Probe",
            ));
        }
        state.capabilities.atomic_noreplace_rename = true;
        Ok(state.capabilities)
    }

    pub(crate) fn mark_quarantine_verified(
        &self,
        expected_generation: u64,
    ) -> io::Result<RemoteCapabilities> {
        let mut state = lock(&self.state);
        if state.generation != expected_generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "SMB-Sitzung wechselte während der Quarantäneprobe",
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
                "SMB-Quarantäne benötigt getrennt geprüfte Lösch- sowie AddOnly-/Replace-Fähigkeiten für ihr Manifest",
            ));
        }
        state.capabilities.quarantine = true;
        Ok(state.capabilities)
    }

    /// Prüft Datei- und Verzeichnis-Löschen mit ausschließlich künstlichen Namen.
    /// Der leere Dateihandle wird nur für die Löschprobe erzeugt und braucht weder
    /// Inhaltsübertragung noch FLUSH; der Uploadvertrag bleibt vollständig getrennt.
    pub fn probe_delete_capability(&self) -> io::Result<RemoteCapabilities> {
        self.probe_delete_capability_at(Path::new("/"))
    }

    pub fn probe_delete_capability_at(&self, probe_root: &Path) -> io::Result<RemoteCapabilities> {
        if !self.capabilities().permits_directory_create() {
            self.probe_directory_create_capability_at(probe_root)?;
        }
        let generation = self.session_generation();
        let base = probe_root.join(format!(".doppeldecker-delete-probe-{}", random_hex()?));
        let file = base.join("file");
        let directory = base.join("directory");
        self.create_probe_directory(&base, generation)?;

        let probe = (|| -> io::Result<()> {
            drop(self.open_write_new_unchecked(&file)?);
            self.create_dir_unchecked(&directory)?;
            let listing = self.list_complete(&base)?;
            if !listing.complete || listing.entries.len() != 2 {
                return Err(invalid(
                    "SMB-Löschprobe erhielt kein vollständiges Zweier-Listing",
                ));
            }
            let file_ok = listing.entries.iter().any(|listed| {
                listed.entry.name == "file"
                    && listed.entry.is_file
                    && !listed.entry.is_dir
                    && !listed.entry.is_symlink
            });
            let directory_ok = listing.entries.iter().any(|listed| {
                listed.entry.name == "directory" && listed.entry.is_dir && !listed.entry.is_symlink
            });
            if !file_ok || !directory_ok {
                return Err(invalid(
                    "SMB-Löschprobe konnte Datei und Verzeichnis nicht eindeutig binden",
                ));
            }

            self.remove_file_unchecked(&file)?;
            self.remove_dir_unchecked(&directory)?;
            for path in [&file, &directory] {
                match self.stat_unchecked(path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(invalid(
                            "SMB-Löschprobe ließ einen entfernten Eintrag sichtbar",
                        ))
                    }
                    Err(error) => return Err(error),
                }
            }
            self.remove_dir_unchecked(&base)
        })();

        // Nur die zufällig erzeugten Probenamen bereinigen; jeder Rest sperrt die
        // Fähigkeit und bleibt mit konkretem Pfad als Fehler sichtbar.
        // Die Fehler bleiben als `io::Error` erhalten, statt sofort zu Text zu
        // werden: Ein Cleanup-Delete kann `OutcomeUnclear` melden, und dieser Code
        // muss bis zum Aufrufer durchkommen — er verbietet den Wiederholversuch.
        let cleanup = cleanup_after_probe(&probe, &base, || {
            let mut cleanup_errors = Vec::new();
            for (path, is_dir) in [(&file, false), (&directory, true), (&base, true)] {
                match self.stat_unchecked(path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        let cleanup = if is_dir {
                            self.remove_dir_unchecked(path)
                        } else {
                            self.remove_file_unchecked(path)
                        };
                        if let Err(error) = cleanup {
                            cleanup_errors.push(crate::remote::error_with_path(path, error));
                        }
                    }
                    Err(error) => cleanup_errors.push(crate::remote::error_with_path(path, error)),
                }
            }
            crate::remote::fold_cleanup_errors(cleanup_errors)
        });
        self.finish_probe("SMB-Lösch", &base, generation, probe, cleanup)?;
        let mut state = lock(&self.state);
        if state.generation != generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "SMB-Sitzung wechselte während der Löschprobe",
            ));
        }
        state.capabilities.entry_delete = true;
        Ok(state.capabilities)
    }

    /// Prüft ausschließlich in einem zufälligen künstlichen Ordner die mutierenden
    /// Garantien. Ein Fehler vergibt keine neuen Schreibfähigkeiten.
    pub fn probe_add_only_capabilities(&self) -> io::Result<RemoteCapabilities> {
        self.probe_add_only_capabilities_at(Path::new("/"))
    }

    pub fn probe_add_only_capabilities_at(
        &self,
        probe_root: &Path,
    ) -> io::Result<RemoteCapabilities> {
        let generation = self.session_generation();
        let base = probe_root.join(format!(".doppeldecker-probe-{}", random_hex()?));
        let first = base.join("first");
        let renamed = base.join("renamed");
        let collision = base.join("collision");
        let replace_source = base.join("replace-source");
        let replace_target = base.join("replace-target");

        self.create_probe_directory(&base, generation)?;
        let result = (|| {
            write_probe(self, &first, b"eins")?;
            match self.open_write_new_unchecked(&first) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "SMB-Server ignoriert exklusives Create",
                    ));
                }
                Err(error) => return Err(error),
            }
            self.rename_noreplace_unchecked(&first, &renamed)?;
            require_bytes(self, &renamed, b"eins")?;

            write_probe(self, &collision, b"ziel")?;
            write_probe(self, &first, b"quelle")?;
            match self.rename_noreplace_unchecked(&first, &collision) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Ok(()) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "SMB-No-Replace-Rename ersetzte ein vorhandenes Ziel",
                    ));
                }
                Err(error) => return Err(error),
            }
            require_bytes(self, &first, b"quelle")?;
            require_bytes(self, &collision, b"ziel")?;

            write_probe(self, &replace_source, b"neu")?;
            write_probe(self, &replace_target, b"alt")?;
            self.rename_replace_unchecked(&replace_source, &replace_target)?;
            require_bytes(self, &replace_target, b"neu")?;
            match self.stat_unchecked(&replace_source) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => return Err(invalid("SMB-Replace-Rename ließ die Quelle zurück")),
                Err(error) => return Err(error),
            }

            let available = self.available_bytes_unchecked(&base)?;
            if available == 0 {
                return Err(invalid("SMB-Freigabe meldet keinen verfügbaren Speicher"));
            }
            let durable_directory_flush = self.flush_directory_unchecked(&base).is_ok();
            Ok(durable_directory_flush)
        })();

        let cleanup = cleanup_after_probe(&result, &base, || {
            self.cleanup_probe(
                &base,
                &[
                    ("first", false),
                    ("renamed", false),
                    ("collision", false),
                    ("replace-source", false),
                    ("replace-target", false),
                ],
            )
        });
        let durable_directory_flush =
            self.finish_probe("SMB-Fähigkeits", &base, generation, result, cleanup)?;
        let mut state = lock(&self.state);
        if state.generation != generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "SMB-Sitzung wechselte während der Fähigkeitsermittlung",
            ));
        }
        state.capabilities.exclusive_directory_create = true;
        state.capabilities.exclusive_create = true;
        state.capabilities.atomic_noreplace_rename = true;
        state.capabilities.atomic_replace_rename = true;
        state.capabilities.durable_file_flush = true;
        state.capabilities.durable_directory_flush = durable_directory_flush;
        state.capabilities.available_space = true;
        Ok(state.capabilities)
    }

    fn canonical_root(&self) -> String {
        self.identity().canonical_root
    }

    fn remote_path(&self, path: &Path) -> io::Result<String> {
        map_virtual_path(&self.canonical_root(), path)
    }

    fn require_write_capability(&self) -> io::Result<()> {
        if self.capabilities().permits_add_only() {
            Ok(())
        } else {
            Err(capability_probe_required(
                "SMB-Schreibfähigkeiten wurden für diese Sitzung nicht sicher geprüft",
            ))
        }
    }

    fn require_entry_delete(&self) -> io::Result<()> {
        if self.capabilities().permits_entry_delete() {
            Ok(())
        } else {
            Err(capability_probe_required(
                "SMB-Löschen wurde für diese Sitzung nicht getrennt geprüft",
            ))
        }
    }

    fn require_directory_create(&self) -> io::Result<()> {
        if self.capabilities().permits_directory_create() {
            Ok(())
        } else {
            Err(capability_probe_required(
                "SMB-Verzeichnis-Create wurde für diese Sitzung noch nicht sicher geprüft",
            ))
        }
    }

    fn require_rename(&self) -> io::Result<()> {
        if self.capabilities().atomic_noreplace_rename {
            Ok(())
        } else {
            Err(capability_probe_required(
                "SMB-No-Replace-Rename wurde für diese Sitzung noch nicht geprüft",
            ))
        }
    }

    /// Listing **und** die Generation, unter der es entstanden ist, aus demselben
    /// gehaltenen Zustand.
    ///
    /// Beides getrennt zu lesen wäre ein Rennen: Die Verbindungen sind prozessweit
    /// geteilt, und ein Reconnect aus einem anderen Fenster zählt die Generation
    /// zwischen beiden Zugriffen hoch. Die Einträge der alten Sitzung trügen dann
    /// das Etikett der neuen — und genau daran hängt die Driftbindung von
    /// Pane, Ordnergrösse und Folgeoperation.
    fn complete_list_unchecked(&self, path: &Path) -> io::Result<(Vec<SmbListedEntry>, u64)> {
        let remote = self.remote_path(path)?;
        let mut state = lock(&self.state);
        let generation = state.generation;
        let tree = state.session.tree.clone();
        let entries = self.runtime.block_on(async {
            query_directory_complete(&mut state.session.connection, tree.as_ref(), &remote).await
        })?;
        Ok((entries, generation))
    }

    /// Interaktives Pane-Listing samt gebundener Generation.
    pub fn list_interactive(&self, path: &Path) -> io::Result<SmbInteractiveDirectoryListing> {
        let (entries, session_generation) = self.complete_list_unchecked(path)?;
        Ok(SmbInteractiveDirectoryListing {
            entries: entries.into_iter().map(|entry| entry.entry()).collect(),
            session_generation,
        })
    }

    fn stat_unchecked(&self, path: &Path) -> io::Result<SmbListedEntry> {
        let remote = self.remote_path(path)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("/")
            .to_owned();
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(async {
            query_entry(&mut state.session.connection, tree.as_ref(), &remote, name).await
        })
    }

    fn create_dir_unchecked(&self, path: &Path) -> io::Result<()> {
        let remote = self.remote_path(path)?;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime
            .block_on(tree.create_directory(&mut state.session.connection, &remote))
            .map_err(smb_error)
    }

    fn open_write_new_unchecked(&self, path: &Path) -> io::Result<SmbWrite> {
        let remote = self.remote_path(path)?;
        let (tree, mut connection, max_write) = {
            let state = lock(&self.state);
            let max_write = state
                .session
                .connection
                .params()
                .map(|params| params.max_write_size.min(MAX_IO_CHUNK))
                .unwrap_or(64 * 1024);
            (
                state.session.tree.clone(),
                state.session.connection.clone(),
                max_write,
            )
        };
        let file_id = self.runtime.block_on(open_handle(
            &mut connection,
            tree.as_ref(),
            &remote,
            FileAccessMask::FILE_WRITE_DATA
                | FileAccessMask::FILE_WRITE_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
            CreateDisposition::FileCreate,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ))?;
        Ok(SmbWrite {
            runtime: self.runtime.clone(),
            tree,
            connection,
            file_id,
            offset: 0,
            max_write,
            finished: false,
        })
    }

    /// Benennt serverseitig um und belegt anschließend beide Seiten neu.
    ///
    /// `replace` entscheidet, ob ein vorhandenes Ziel ersetzt werden darf. Der
    /// Readback ist für beide Formen derselbe und nicht optional: Erst wenn die
    /// Quelle verschwunden und das Ziel vorhanden ist, gilt der Rename als
    /// vollzogen. Die Sitzungssperre wird vor dem Readback wieder freigegeben.
    fn rename_unchecked(&self, from: &Path, to: &Path, replace: bool) -> io::Result<()> {
        let remote_from = self.remote_path(from)?;
        let remote_to = self.remote_path(to)?;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(rename_path(
            &mut state.session.connection,
            tree.as_ref(),
            &remote_from,
            &remote_to,
            replace,
        ))?;
        drop(state);
        self.require_rename_readback(from, to)
    }

    fn rename_noreplace_unchecked(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename_unchecked(from, to, false)
    }

    fn rename_replace_unchecked(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename_unchecked(from, to, true)
    }

    fn require_rename_readback(&self, from: &Path, to: &Path) -> io::Result<()> {
        match self.stat_unchecked(from) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(invalid("SMB-Rename ließ die Quelle sichtbar")),
            Err(error) => return Err(error),
        }
        self.stat_unchecked(to).map(|_| ())
    }

    fn available_bytes_unchecked(&self, path: &Path) -> io::Result<u64> {
        let _ = self.stat_unchecked(path)?;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        let info = self
            .runtime
            .block_on(tree.fs_info(&mut state.session.connection))
            .map_err(smb_error)?;
        Ok(info.free_bytes)
    }

    fn flush_directory_unchecked(&self, path: &Path) -> io::Result<()> {
        let remote = self.remote_path(path)?;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(flush_path(
            &mut state.session.connection,
            tree.as_ref(),
            &remote,
            true,
        ))
    }

    /// Nach einem fehlgeschlagenen Create kann ein Probenrest existieren. Ohne
    /// bestätigte Anlage bereinigen wir nichts; beide Roots werden trotzdem geprüft.
    fn create_probe_directory(&self, base: &Path, generation: u64) -> io::Result<()> {
        match self.create_dir_unchecked(base) {
            Ok(()) => Ok(()),
            Err(error) => self.finish_probe("SMB-Create", base, generation, Err(error), Ok(())),
        }
    }

    fn finish_probe<T>(
        &self,
        provider: &str,
        base: &Path,
        generation: u64,
        result: io::Result<T>,
        cleanup: io::Result<()>,
    ) -> io::Result<T> {
        let result = crate::remote::combine_probe_outcome(
            provider,
            result,
            cleanup,
            &base.to_string_lossy(),
        );
        let readback = verify_probe_absence(base, |root| {
            let (entries, observed_generation) = self.complete_list_unchecked(root)?;
            if observed_generation != generation {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "SMB-Sitzung wechselte während der Restprüfung",
                ));
            }
            Ok(entries.into_iter().map(|entry| entry.name).collect())
        });
        crate::remote::combine_probe_outcome(provider, result, readback, &base.to_string_lossy())
    }

    fn cleanup_probe(&self, base: &Path, expected: &[(&str, bool)]) -> io::Result<()> {
        let (entries, _) = match self.complete_list_unchecked(base) {
            Ok(listing) => listing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        // Erst den gesamten Bestand prüfen: Ein fremdes Kind darf nicht erst
        // auffallen, nachdem davor bereits andere Einträge entfernt wurden.
        execute_probe_cleanup(entries, expected, base, |path, is_dir| {
            if is_dir {
                // Proben erzeugen ausschließlich leere Ordner. Neue Kinder sind
                // fremder Bestand; RMD muss sie erhalten und sichtbar scheitern.
                self.remove_dir_unchecked(path)
            } else {
                self.remove_file_unchecked(path)
            }
        })?;
        self.remove_dir_unchecked(base)
            .map_err(|error| probe_cleanup_recovery(base, error))
    }

    /// Führt genau eine Löschung aus und beweist danach die Abwesenheit.
    ///
    /// SMB löscht ein mit `FILE_DELETE_ON_CLOSE` geöffnetes Objekt erst, wenn das
    /// **letzte** Handle geschlossen ist. Doppeldecker öffnet selbst mit
    /// `FILE_SHARE_DELETE`, verursacht also keine Sharing-Violation: Hält ein anderer
    /// Client ein Handle, antwortet CLOSE mit Erfolg und die Datei bleibt sichtbar.
    /// Die Serverantwort allein ist deshalb kein Löschbeweis. Nur ein echtes
    /// `NotFound` im Readback zählt als Abwesenheit; jeder andere Statfehler bleibt
    /// ein unklarer Ausgang und darf nicht als Erfolg gemeldet werden.
    fn delete_with_readback(&self, path: &Path, directory: bool) -> io::Result<()> {
        let remote = self.remote_path(path)?;
        {
            let mut state = lock(&self.state);
            let tree = state.session.tree.clone();
            self.runtime.block_on(delete_path(
                &mut state.session.connection,
                tree.as_ref(),
                &remote,
                directory,
            ))?;
        }
        match self.stat_unchecked(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "SMB meldete die Löschung von {remote} als erfolgreich, der Pfad ist aber weiterhin sichtbar"
                ),
            )),
            Err(error) => Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!("SMB-Löschung von {remote} ist unbestätigt: {error}"),
            )),
        }
    }

    fn remove_file_unchecked(&self, path: &Path) -> io::Result<()> {
        match self.delete_with_readback(path, false) {
            Err(error) if error.kind() == io::ErrorKind::IsADirectory => {
                // Ein Symlink auf ein Verzeichnis ist selbst ein Verzeichniseintrag
                // und lässt sich nur mit `FILE_DIRECTORY_FILE` öffnen. Genau dafür —
                // und nur dafür — gibt es diesen zweiten Versuch. Ohne die Rückfrage
                // entfernte `remove_file` auch ein echtes Verzeichnis; jeder andere
                // Provider scheitert dort mit einem Typfehler, und der Move-Quellabbau
                // (`remove_move_leaf_best_effort`) verlässt sich genau darauf.
                let entry = self.stat_unchecked(path)?;
                if !entry.is_symlink {
                    return Err(error);
                }
                self.delete_with_readback(path, true)
            }
            result => result,
        }
    }

    fn remove_dir_unchecked(&self, path: &Path) -> io::Result<()> {
        self.delete_with_readback(path, true)
    }

    fn remove_dir_all_unchecked(&self, path: &Path) -> io::Result<()> {
        for entry in self.complete_list_unchecked(path)?.0 {
            let child = path.join(&entry.name);
            if entry.is_dir && !entry.is_symlink {
                self.remove_dir_all_unchecked(&child)?;
            } else {
                self.remove_file_unchecked(&child)?;
            }
        }
        self.remove_dir_unchecked(path)
    }
}

impl Vfs for SmbVfs {
    fn safety_capabilities_digest(&self) -> io::Result<Option<[u8; 32]>> {
        crate::remote::capabilities_digest(self.capabilities()).map(Some)
    }

    fn supports_mode_write(&self) -> bool {
        false
    }

    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(self
            .complete_list_unchecked(path)?
            .0
            .into_iter()
            .map(|entry| entry.entry())
            .collect())
    }

    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let (mut entries, generation) = self.complete_list_unchecked(path)?;
        // SMB garantiert keine stabile Reihenfolge zwischen zwei gepagten Listings.
        // Der Proof muss deshalb nur vom Inhalt, nicht von der Serverreihenfolge abhängen.
        entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        let mut token = blake3::Hasher::new();
        token.update(b"doppeldecker-smb-list-v1\0");
        token.update(&generation.to_le_bytes());
        let mut listed = Vec::with_capacity(entries.len());
        for item in entries {
            let provider_token = item.provider_token();
            token.update(&(item.name.len() as u64).to_le_bytes());
            token.update(item.name.as_bytes());
            token.update(&provider_token);
            listed.push(ProofListEntry {
                raw_name: item.name.as_bytes().to_vec(),
                safely_decoded: true,
                provider_token: Some(provider_token),
                entry: item.entry(),
            });
        }
        Ok(CompleteDirectoryListing {
            entries: listed,
            provider_token: Some(token.finalize().as_bytes().to_vec()),
            complete: true,
        })
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        Ok(self.stat_unchecked(path)?.entry())
    }

    fn exists(&self, path: &Path) -> bool {
        self.stat_unchecked(path).is_ok()
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.require_directory_create()?;
        let mut current = Path::new("/").to_path_buf();
        for component in virtual_components(path)? {
            current.push(component);
            match self.create_dir_unchecked(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let entry = self.stat_unchecked(&current)?;
                    if !entry.is_dir || entry.is_symlink {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "SMB-Pfadkomponente ist kein echtes Verzeichnis",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.require_directory_create()?;
        self.create_dir_unchecked(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let remote = self.remote_path(path)?;
        let (tree, mut connection, max_read) = {
            let state = lock(&self.state);
            let max_read = state
                .session
                .connection
                .params()
                .map(|params| params.max_read_size.min(MAX_IO_CHUNK))
                .unwrap_or(64 * 1024);
            (
                state.session.tree.clone(),
                state.session.connection.clone(),
                max_read,
            )
        };
        let opened = self.runtime.block_on(open_handle_response(
            &mut connection,
            tree.as_ref(),
            &remote,
            FileAccessMask::FILE_READ_DATA
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
            CreateDisposition::FileOpen,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ))?;
        if opened.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            let _ = self
                .runtime
                .block_on(tree.close_handle(&mut connection, opened.file_id));
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SMB-Reparse-Point wird nicht als Datei geöffnet",
            ));
        }
        Ok(Box::new(SmbRead {
            runtime: self.runtime.clone(),
            tree,
            connection,
            file_id: opened.file_id,
            size: opened.end_of_file,
            offset: 0,
            max_read,
            closed: false,
        }))
    }

    fn open_write(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SMB schreibt ausschließlich neue Temp-Dateien",
        ))
    }

    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.require_write_capability()?;
        Ok(Box::new(self.open_write_new_unchecked(path)?))
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.require_write_capability()?;
        let remote = self.remote_path(path)?;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(flush_path(
            &mut state.session.connection,
            tree.as_ref(),
            &remote,
            false,
        ))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        if !self.capabilities().complete_listing {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SMB-Namespace-Readback benötigt ein vollständiges Listing",
            ));
        }
        if self.capabilities().durable_directory_flush {
            self.flush_directory_unchecked(path)
        } else {
            let entry = self.stat_unchecked(path)?;
            if entry.is_dir && !entry.is_symlink {
                Ok(())
            } else {
                Err(invalid("SMB-Namespace-Readback benötigt ein Verzeichnis"))
            }
        }
    }

    fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
        self.require_write_capability()?;
        let remote = self.remote_path(path)?;
        let system_time = if mtime >= 0 {
            UNIX_EPOCH + Duration::from_secs(mtime as u64)
        } else {
            UNIX_EPOCH
                .checked_sub(Duration::from_secs(mtime.unsigned_abs()))
                .ok_or_else(|| invalid("SMB-mtime liegt außerhalb des Zeitbereichs"))?
        };
        let raw = FileTime::from_system_time(system_time).0;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(set_mtime(
            &mut state.session.connection,
            tree.as_ref(),
            &remote,
            raw,
        ))?;
        drop(state);
        let observed = self.stat_unchecked(path)?.mtime;
        if observed != Some(mtime) {
            return Err(invalid("SMB-mtime-Readback stimmt nicht exakt überein"));
        }
        Ok(())
    }

    fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SMB besitzt in diesem Profil keine portable POSIX-Rechtesemantik",
        ))
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
        self.remove_file_unchecked(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.require_entry_delete()?;
        self.remove_dir_all_unchecked(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.require_entry_delete()?;
        self.remove_dir_unchecked(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.require_write_capability()?;
        if !self.capabilities().atomic_replace_rename {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SMB-Replace-Rename wurde nicht geprüft",
            ));
        }
        self.rename_replace_unchecked(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.require_rename()?;
        if !self.capabilities().atomic_noreplace_rename {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SMB-No-Replace-Rename wurde nicht geprüft",
            ));
        }
        self.rename_noreplace_unchecked(from, to)
    }

    fn available_bytes(&self, path: &Path) -> io::Result<u64> {
        if !self.capabilities().available_space {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SMB-Speicherabfrage wurde nicht geprüft",
            ));
        }
        self.available_bytes_unchecked(path)
    }

    fn root_identity(&self, path: &Path) -> io::Result<crate::real_mode::RootIdentity> {
        let entry = self.stat_unchecked(path)?;
        if !entry.is_dir || entry.is_symlink || entry.file_id == 0 {
            return Err(invalid(
                "SMB-Root-Identität benötigt ein echtes Verzeichnis",
            ));
        }
        let state = lock(&self.state);
        let identity = SmbRootIdentity {
            server: state.session.identity.clone(),
            virtual_root: normalize_virtual_path(path)?,
            root_file_id: entry.file_id,
            tree_id: state.session.tree.tree_id.0,
            session_generation: state.generation,
        };
        identity.validate()?;
        Ok(crate::real_mode::RootIdentity::Smb(identity))
    }
}

#[derive(Clone, Debug)]
struct SmbListedEntry {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mtime: Option<i64>,
    created: Option<i64>,
    change_time: u64,
    file_id: u64,
}

impl SmbListedEntry {
    fn entry(&self) -> Entry {
        let is_file = !self.is_dir && !self.is_symlink;
        Entry {
            extension: entry_extension(&self.name, is_file),
            name: self.name.clone(),
            is_dir: self.is_dir,
            is_file,
            is_symlink: self.is_symlink,
            size: if self.is_dir { 0 } else { self.size },
            mtime: self.mtime,
            created: self.created,
            owner: None,
            group: None,
            mode: None,
            device: None,
            inode: (self.file_id != 0).then_some(self.file_id),
            hard_links: None,
        }
    }

    fn provider_token(&self) -> Vec<u8> {
        let mut token = Vec::with_capacity(16);
        token.extend_from_slice(&self.file_id.to_le_bytes());
        token.extend_from_slice(&self.change_time.to_le_bytes());
        token
    }
}

struct SmbRead {
    runtime: Arc<Runtime>,
    tree: Arc<Tree>,
    connection: Connection,
    file_id: FileId,
    size: u64,
    offset: u64,
    max_read: u32,
    closed: bool,
}

impl Read for SmbRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.offset >= self.size {
            self.close()?;
            return Ok(0);
        }
        let length = (buffer.len() as u64)
            .min(self.size - self.offset)
            .min(self.max_read as u64) as u32;
        let request = ReadRequest {
            padding: 0x50,
            flags: 0,
            length,
            offset: self.offset,
            file_id: self.file_id,
            minimum_count: 0,
            channel: SMB2_CHANNEL_NONE,
            remaining_bytes: 0,
            read_channel_info: Vec::new(),
        };
        let frame = self
            .runtime
            .block_on(self.connection.execute_with_credits(
                Command::Read,
                &request,
                Some(self.tree.tree_id),
                CreditCharge((u64::from(length).div_ceil(65_536).max(1)) as u16),
            ))
            .map_err(smb_error)?;
        require_status(&frame, Command::Read)?;
        let response =
            ReadResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
        if response.data.len() > length as usize || response.data.len() > buffer.len() {
            return Err(invalid("SMB-READ lieferte mehr Bytes als angefordert"));
        }
        if response.data.is_empty() && self.offset < self.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "SMB-Datei endete vor der beim Öffnen gebundenen Größe",
            ));
        }
        let count = response.data.len();
        buffer[..count].copy_from_slice(&response.data);
        self.offset += count as u64;
        Ok(count)
    }
}

impl SmbRead {
    fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.runtime
            .block_on(self.tree.close_handle(&mut self.connection, self.file_id))
            .map_err(smb_error)
    }
}

impl Drop for SmbRead {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct SmbWrite {
    runtime: Arc<Runtime>,
    tree: Arc<Tree>,
    connection: Connection,
    file_id: FileId,
    offset: u64,
    max_write: u32,
    finished: bool,
}

impl Write for SmbWrite {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SMB-Schreibstrom wurde bereits abgeschlossen",
            ));
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        let count = buffer.len().min(self.max_write as usize);
        let request = WriteRequest {
            data_offset: 0x70,
            offset: self.offset,
            file_id: self.file_id,
            channel: 0,
            remaining_bytes: 0,
            write_channel_info_offset: 0,
            write_channel_info_length: 0,
            flags: 0,
            data: buffer[..count].to_vec(),
        };
        let frame = self
            .runtime
            .block_on(self.connection.execute_with_credits(
                Command::Write,
                &request,
                Some(self.tree.tree_id),
                CreditCharge((count as u64).div_ceil(65_536).max(1) as u16),
            ))
            .map_err(smb_error)?;
        require_status(&frame, Command::Write)?;
        let response =
            WriteResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
        if response.count == 0 || response.count as usize > count {
            return Err(invalid("SMB-WRITE meldete eine ungültige Bytezahl"));
        }
        self.offset += u64::from(response.count);
        Ok(response.count as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.runtime.block_on(flush_handle(
            &mut self.connection,
            self.tree.as_ref(),
            self.file_id,
        ))?;
        self.runtime
            .block_on(self.tree.close_handle(&mut self.connection, self.file_id))
            .map_err(smb_error)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for SmbWrite {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self
                .runtime
                .block_on(self.tree.close_handle(&mut self.connection, self.file_id));
            self.finished = true;
        }
    }
}

async fn connect_once(
    config: &SmbConnectionConfig,
    secrets: &dyn SecretResolver,
) -> io::Result<SmbSession> {
    let host = config.normalized_host();
    // Der High-Level-Client authentifiziert bereits während connect(). Die
    // öffentliche Identität muss aber auf genau derselben Verbindung feststehen,
    // bevor ein Secret aufgelöst oder eine NTLM-Anmeldung gesendet wird.
    let mut connection = Connection::connect(&format!("{host}:{}", config.port), CONNECT_TIMEOUT)
        .await
        .map_err(smb_error)?;
    connection.set_compression_requested(false);
    connection.negotiate().await.map_err(smb_error)?;
    let params = connection
        .params()
        .cloned()
        .ok_or_else(|| invalid("SMB-Verhandlung lieferte keine Parameter"))?;
    if params.dialect != Dialect::Smb3_1_1 || params.cipher.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Doppeldecker verlangt SMB 3.1.1 mit Preauth-Integrität und Verschlüsselung",
        ));
    }
    let observed_guid = guid_bytes(params.server_guid);
    if observed_guid != config.server_guid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Server-GUID stimmt nicht mit dem Profil-Pin überein",
        ));
    }
    let password = secrets.resolve(&config.password_reference)?;
    let session = Session::setup(
        &mut connection,
        &config.username,
        password.expose(),
        &config.domain,
    )
    .await
    .map_err(smb_error)?;
    drop(password);
    if !session.should_sign {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Sitzung besitzt keine aktive Signatur",
        ));
    }
    let encryption_key = session
        .encryption_key
        .clone()
        .ok_or_else(|| invalid("SMB3-Sitzung lieferte keinen Verschlüsselungsschlüssel"))?;
    let decryption_key = session
        .decryption_key
        .clone()
        .ok_or_else(|| invalid("SMB3-Sitzung lieferte keinen Entschlüsselungsschlüssel"))?;
    let cipher = params
        .cipher
        .expect("SMB-3.1.1-Cipher wurde unmittelbar zuvor geprüft");
    connection.activate_encryption(encryption_key, decryption_key, cipher);
    let tree = Arc::new(
        Tree::connect(&mut connection, &config.share)
            .await
            .map_err(smb_error)?,
    );
    if tree.is_dfs {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "DFS-Freigaben bleiben bis zur identitätsgebundenen Referral-Unterstützung gesperrt",
        ));
    }
    let diagnostics = connection.diagnostics();
    if !diagnostics.signing.active || !diagnostics.encryption.active {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB3-Signing oder -Verschlüsselung ist nicht aktiv",
        ));
    }
    let canonical_root = normalize_configured_root(&config.requested_root)?;
    let account_digest =
        *blake3::hash(format!("{}\0{}", config.domain, config.username).as_bytes()).as_bytes();
    let identity = SmbServerIdentity {
        host,
        port: config.port,
        server_guid: observed_guid,
        share: config.share.clone(),
        account_digest,
        canonical_root,
        dialect: params.dialect as u16,
        signing_active: true,
        encryption_active: true,
        signing_algorithm: format!("{:?}", session.signing_algorithm),
        encryption_cipher: format!("{cipher:?}"),
    };
    identity.validate()?;
    Ok(SmbSession {
        connection,
        tree,
        identity,
    })
}

async fn open_handle(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    access: u32,
    disposition: CreateDisposition,
    options: u32,
) -> io::Result<FileId> {
    Ok(
        open_handle_response(connection, tree, path, access, disposition, options)
            .await?
            .file_id,
    )
}

async fn open_handle_response(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    access: u32,
    disposition: CreateDisposition,
    options: u32,
) -> io::Result<CreateResponse> {
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(access),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access: ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | ShareAccess::FILE_SHARE_WRITE
                | ShareAccess::FILE_SHARE_DELETE,
        ),
        create_disposition: disposition,
        create_options: options,
        name: path.to_owned(),
        create_contexts: Vec::new(),
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::Create)?;
    CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)
}

async fn query_directory_complete(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
) -> io::Result<Vec<SmbListedEntry>> {
    let file_id = open_handle(
        connection,
        tree,
        path,
        FileAccessMask::FILE_READ_DATA
            | FileAccessMask::FILE_READ_ATTRIBUTES
            | FileAccessMask::SYNCHRONIZE,
        CreateDisposition::FileOpen,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
    )
    .await?;
    let result = async {
        let mut entries = Vec::new();
        let mut names = BTreeSet::new();
        let mut first = true;
        let mut pages = 0u64;
        loop {
            pages += 1;
            if pages > 1_000_000 {
                return Err(invalid("SMB-Paging überschreitet das Sicherheitslimit"));
            }
            let request = QueryDirectoryRequest {
                file_information_class: FileInformationClass::FileIdBothDirectoryInformation,
                flags: QueryDirectoryFlags(if first {
                    QueryDirectoryFlags::RESTART_SCANS
                } else {
                    0
                }),
                file_index: 0,
                file_id,
                output_buffer_length: MAX_DIRECTORY_RESPONSE,
                file_name: "*".to_owned(),
            };
            first = false;
            let frame = connection
                .execute(Command::QueryDirectory, &request, Some(tree.tree_id))
                .await
                .map_err(smb_error)?;
            if frame.header.status == NtStatus::NO_MORE_FILES {
                break;
            }
            require_status(&frame, Command::QueryDirectory)?;
            let response = QueryDirectoryResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(smb_error)?;
            let page = parse_file_id_both_directory_info(&response.output_buffer)?;
            // Ein leeres Nutzresultat ist erlaubt, wenn die validierte Seite nur
            // `.` und `..` enthielt. Erst STATUS_NO_MORE_FILES beendet das Listing;
            // ein Server, der endlos solche Seiten sendet, trifft das Seitenlimit.
            for entry in page {
                if !names.insert(entry.name.clone()) {
                    return Err(invalid("SMB-Listing enthält einen doppelten Namen"));
                }
                entries.push(entry);
                if entries.len() > 10_000_000 {
                    return Err(invalid(
                        "SMB-Verzeichnis überschreitet das Sicherheitslimit",
                    ));
                }
            }
        }
        Ok(entries)
    }
    .await;
    let close = tree
        .close_handle(connection, file_id)
        .await
        .map_err(smb_error);
    result.and_then(|entries| close.map(|()| entries))
}

async fn query_entry(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    name: String,
) -> io::Result<SmbListedEntry> {
    let opened = open_handle_response(
        connection,
        tree,
        path,
        FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
        CreateDisposition::FileOpen,
        FILE_OPEN_REPARSE_POINT,
    )
    .await?;
    let result = async {
        let basic =
            query_info(connection, tree, opened.file_id, FILE_BASIC_INFORMATION, 40).await?;
        let standard = query_info(
            connection,
            tree,
            opened.file_id,
            FILE_STANDARD_INFORMATION,
            24,
        )
        .await?;
        let internal = query_info(
            connection,
            tree,
            opened.file_id,
            FILE_INTERNAL_INFORMATION,
            8,
        )
        .await?;
        if basic.len() < 36 || standard.len() < 22 || internal.len() < 8 {
            return Err(invalid("SMB-QUERY_INFO-Antwort ist zu kurz"));
        }
        let attributes = read_u32(&basic, 32)?;
        Ok(SmbListedEntry {
            name,
            is_dir: standard[21] != 0 || attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            is_symlink: attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
            size: read_u64(&standard, 8)?,
            mtime: filetime_to_unix(read_u64(&basic, 16)?),
            created: filetime_to_unix(read_u64(&basic, 0)?),
            change_time: read_u64(&basic, 24)?,
            file_id: read_u64(&internal, 0)?,
        })
    }
    .await;
    let close = tree
        .close_handle(connection, opened.file_id)
        .await
        .map_err(smb_error);
    result.and_then(|entry| close.map(|()| entry))
}

async fn query_info(
    connection: &mut Connection,
    tree: &Tree,
    file_id: FileId,
    class: u8,
    length: u32,
) -> io::Result<Vec<u8>> {
    let request = QueryInfoRequest {
        info_type: InfoType::File,
        file_info_class: class,
        output_buffer_length: length,
        additional_information: 0,
        flags: 0,
        file_id,
        input_buffer: Vec::new(),
    };
    let frame = connection
        .execute(Command::QueryInfo, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::QueryInfo)?;
    Ok(QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
        .map_err(smb_error)?
        .output_buffer)
}

async fn flush_path(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    directory: bool,
) -> io::Result<()> {
    let file_id = open_handle(
        connection,
        tree,
        path,
        if directory {
            FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE
        } else {
            FileAccessMask::FILE_WRITE_DATA | FileAccessMask::SYNCHRONIZE
        },
        CreateDisposition::FileOpen,
        (if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        }) | FILE_OPEN_REPARSE_POINT,
    )
    .await?;
    let result = flush_handle(connection, tree, file_id).await;
    let close = tree
        .close_handle(connection, file_id)
        .await
        .map_err(smb_error);
    result.and(close)
}

async fn flush_handle(connection: &mut Connection, tree: &Tree, file_id: FileId) -> io::Result<()> {
    let frame = connection
        .execute(
            Command::Flush,
            &FlushRequest { file_id },
            Some(tree.tree_id),
        )
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::Flush)
}

async fn rename_path(
    connection: &mut Connection,
    tree: &Tree,
    from: &str,
    to: &str,
    replace: bool,
) -> io::Result<()> {
    let file_id = open_handle(
        connection,
        tree,
        from,
        FileAccessMask::DELETE | FileAccessMask::FILE_READ_ATTRIBUTES,
        CreateDisposition::FileOpen,
        FILE_OPEN_REPARSE_POINT,
    )
    .await?;
    let request = SetInfoRequest {
        info_type: InfoType::File,
        file_info_class: FILE_RENAME_INFORMATION,
        additional_information: 0,
        file_id,
        buffer: rename_buffer(to, replace),
    };
    set_info_and_close(connection, tree, request, file_id).await
}

/// Sendet genau eine `SetInfo`-Anfrage und schließt das Handle in jedem Fall.
///
/// Ein offen gelassenes Handle würde den Namen serverseitig weiter binden, auch
/// nach einem Fehler. Der Fehler der Anfrage hat Vorrang; scheitert nur das
/// Schließen, wird dessen Fehler gemeldet.
async fn set_info_and_close(
    connection: &mut Connection,
    tree: &Tree,
    request: SetInfoRequest,
    file_id: FileId,
) -> io::Result<()> {
    let result = async {
        let frame = connection
            .execute(Command::SetInfo, &request, Some(tree.tree_id))
            .await
            .map_err(smb_error)?;
        require_status(&frame, Command::SetInfo)
    }
    .await;
    let close = tree
        .close_handle(connection, file_id)
        .await
        .map_err(smb_error);
    result.and(close)
}

async fn delete_path(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    directory: bool,
) -> io::Result<()> {
    let file_id = open_handle(
        connection,
        tree,
        path,
        FileAccessMask::DELETE | FileAccessMask::FILE_READ_ATTRIBUTES,
        CreateDisposition::FileOpen,
        (if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        }) | FILE_OPEN_REPARSE_POINT
            | FILE_DELETE_ON_CLOSE,
    )
    .await?;
    tree.close_handle(connection, file_id)
        .await
        .map_err(smb_error)
}

async fn set_mtime(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    mtime: u64,
) -> io::Result<()> {
    let file_id = open_handle(
        connection,
        tree,
        path,
        FileAccessMask::FILE_WRITE_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
        CreateDisposition::FileOpen,
        FILE_OPEN_REPARSE_POINT,
    )
    .await?;
    let mut buffer = vec![0u8; 40];
    buffer[16..24].copy_from_slice(&mtime.to_le_bytes());
    let request = SetInfoRequest {
        info_type: InfoType::File,
        file_info_class: FILE_BASIC_INFORMATION,
        additional_information: 0,
        file_id,
        buffer,
    };
    set_info_and_close(connection, tree, request, file_id).await
}

fn probe_cleanup_recovery(path: &Path, error: impl std::fmt::Display) -> io::Error {
    crate::error::with_recovery_paths(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!(
            "SMB-Probe-Cleanup erhält nicht eindeutig eigenen Bestand; Recoverypfad {}: {error}",
            path.display()
        ),
        ),
        &[path.to_string_lossy().into_owned()],
    )
}

fn cleanup_after_probe<T>(
    result: &io::Result<T>,
    base: &Path,
    cleanup: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if result.is_err() {
        return Err(probe_cleanup_recovery(
            base,
            "Fehlgeschlagene Probe: kollidierte oder unklare Kinder bleiben erhalten",
        ));
    }
    cleanup()
}

fn plan_probe_cleanup(
    entries: Vec<SmbListedEntry>,
    expected: &[(&str, bool)],
    base: &Path,
) -> io::Result<Vec<SmbListedEntry>> {
    for entry in &entries {
        if entry.is_symlink
            || !expected
                .iter()
                .any(|(name, is_dir)| *name == entry.name && *is_dir == entry.is_dir)
        {
            return Err(probe_cleanup_recovery(
                &base.join(&entry.name),
                "Unerwarteter Name oder Typ wurde nicht entfernt",
            ));
        }
    }
    Ok(entries)
}

/// `/` bezeichnet hier den virtuellen Profilroot, nicht den gesamten Share.
/// Beide vollständigen Listings sind nötig: Ein früher Fehler darf den Blick auf
/// falsch platzierte Reste in der anderen Wurzel nicht verhindern.
fn verify_probe_absence(
    base: &Path,
    mut list: impl FnMut(&Path) -> io::Result<Vec<String>>,
) -> io::Result<()> {
    let root = base
        .parent()
        .ok_or_else(|| invalid("SMB-Probe besitzt keinen Root"))?;
    let name = base
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("SMB-Probename ist ungültig"))?;
    let mut errors = Vec::new();
    for (label, path) in [("gewählter Root", root), ("Profilroot", Path::new("/"))] {
        match list(path) {
            Ok(names) if names.iter().any(|entry| entry == name) => {
                errors.push(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!("SMB-Probenrest im {label}: {}", path.join(name).display()),
                ));
            }
            Ok(_) => {}
            Err(error) => errors.push(crate::coded_error(
                error.kind(),
                crate::error_code(&error),
                format!("{label} {}: {error}", path.display()),
            )),
        }
    }
    crate::remote::fold_cleanup_errors(errors)
}

fn execute_probe_cleanup(
    entries: Vec<SmbListedEntry>,
    expected: &[(&str, bool)],
    base: &Path,
    mut remove: impl FnMut(&Path, bool) -> io::Result<()>,
) -> io::Result<()> {
    let entries = plan_probe_cleanup(entries, expected, base)?;
    for entry in entries {
        let path = base.join(&entry.name);
        remove(&path, entry.is_dir).map_err(|error| probe_cleanup_recovery(&path, error))?;
    }
    Ok(())
}

fn parse_file_id_both_directory_info(bytes: &[u8]) -> io::Result<Vec<SmbListedEntry>> {
    let mut result = Vec::new();
    let mut offset = 0usize;
    loop {
        if bytes.len().saturating_sub(offset) < 104 {
            return Err(invalid("SMB-FileIdBothDirectoryInformation ist zu kurz"));
        }
        let next = read_u32(bytes, offset)? as usize;
        let name_bytes = read_u32(bytes, offset + 60)? as usize;
        if !name_bytes.is_multiple_of(2) || name_bytes > 65_510 {
            return Err(invalid("SMB-Dateiname besitzt eine ungültige UTF-16-Länge"));
        }
        let end = offset
            .checked_add(104)
            .and_then(|start| start.checked_add(name_bytes))
            .ok_or_else(|| invalid("SMB-Verzeichniseintrag läuft über"))?;
        if end > bytes.len() {
            return Err(invalid(
                "SMB-Verzeichniseintrag endet außerhalb der Antwort",
            ));
        }
        if next != 0 && end > offset.saturating_add(next) {
            return Err(invalid(
                "SMB-Verzeichniseintrag überlappt den nächsten Eintrag",
            ));
        }
        let name = decode_utf16(&bytes[offset + 104..end])?;
        if name != "." && name != ".." {
            validate_name(&name)?;
            let attributes = read_u32(bytes, offset + 56)?;
            result.push(SmbListedEntry {
                name,
                is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                is_symlink: attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                size: read_u64(bytes, offset + 40)?,
                mtime: filetime_to_unix(read_u64(bytes, offset + 24)?),
                created: filetime_to_unix(read_u64(bytes, offset + 8)?),
                change_time: read_u64(bytes, offset + 32)?,
                file_id: read_u64(bytes, offset + 96)?,
            });
        }
        if next == 0 {
            break;
        }
        if next < 104 || !next.is_multiple_of(8) {
            return Err(invalid("SMB-NextEntryOffset ist ungültig"));
        }
        offset = offset
            .checked_add(next)
            .ok_or_else(|| invalid("SMB-NextEntryOffset läuft über"))?;
        if offset >= bytes.len() {
            return Err(invalid("SMB-NextEntryOffset zeigt außerhalb der Antwort"));
        }
    }
    Ok(result)
}

fn decode_utf16(bytes: &[u8]) -> io::Result<String> {
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| invalid("SMB-Dateiname enthält ungültiges UTF-16"))
}

fn rename_buffer(path: &str, replace: bool) -> Vec<u8> {
    let units = path.encode_utf16().collect::<Vec<_>>();
    let mut buffer = Vec::with_capacity(20 + units.len() * 2);
    buffer.push(u8::from(replace));
    buffer.extend_from_slice(&[0; 7]);
    buffer.extend_from_slice(&0u64.to_le_bytes());
    buffer.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
    for unit in units {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    buffer
}

fn write_probe(vfs: &SmbVfs, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut writer = vfs.open_write_new_unchecked(path)?;
    writer.write_all(bytes)?;
    writer.flush()
}

fn require_bytes(vfs: &SmbVfs, path: &Path, expected: &[u8]) -> io::Result<()> {
    let mut reader = Vfs::open_read(vfs, path)?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes != expected {
        return Err(invalid(
            "SMB-Probe-Readback stimmt nicht mit den Schreibdaten überein",
        ));
    }
    Ok(())
}

fn require_status(frame: &smb2::client::connection::Frame, command: Command) -> io::Result<()> {
    if frame.header.status == NtStatus::SUCCESS {
        Ok(())
    } else {
        Err(smb_error(smb2::Error::Protocol {
            status: frame.header.status,
            command,
        }))
    }
}

fn smb_error(error: smb2::Error) -> io::Error {
    let kind = match error.kind() {
        smb2::ErrorKind::NotFound => io::ErrorKind::NotFound,
        smb2::ErrorKind::AlreadyExists => io::ErrorKind::AlreadyExists,
        smb2::ErrorKind::AccessDenied
        | smb2::ErrorKind::AuthRequired
        | smb2::ErrorKind::SigningRequired => io::ErrorKind::PermissionDenied,
        smb2::ErrorKind::IsADirectory => io::ErrorKind::IsADirectory,
        smb2::ErrorKind::NotADirectory => io::ErrorKind::NotADirectory,
        smb2::ErrorKind::DiskFull => io::ErrorKind::StorageFull,
        smb2::ErrorKind::ConnectionLost | smb2::ErrorKind::SessionExpired => {
            io::ErrorKind::NotConnected
        }
        smb2::ErrorKind::TimedOut => io::ErrorKind::TimedOut,
        smb2::ErrorKind::Cancelled => io::ErrorKind::Interrupted,
        smb2::ErrorKind::Unsupported => io::ErrorKind::Unsupported,
        smb2::ErrorKind::InvalidData => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error.to_string())
}

pub(crate) fn map_virtual_path(root: &str, path: &Path) -> io::Result<String> {
    let mut parts = root
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    parts.extend(virtual_components(path)?.map(str::to_owned));
    let mapped = parts.join("\\");
    // CREATE kodiert die UTF-16-Bytelänge des vollständigen Pfads in einem u16.
    if mapped.encode_utf16().count() > MAX_PATH_UTF16_UNITS {
        return Err(invalid("SMB-Pfad überschreitet das Protokoll-Längenfeld"));
    }
    Ok(mapped)
}

fn virtual_components(path: &Path) -> io::Result<impl Iterator<Item = &str>> {
    let mut values = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| invalid("SMB-Pfad ist nicht UTF-8"))?;
                validate_name(value)?;
                values.push(value);
            }
            _ => return Err(invalid("SMB-Pfad enthält Traversal oder ein Präfix")),
        }
    }
    Ok(values.into_iter())
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.len() > 255
        || name.contains(['/', '\\', '\0'])
        || name.chars().any(char::is_control)
    {
        return Err(invalid("SMB-Name ist ungültig"));
    }
    Ok(())
}

fn normalize_configured_root(root: &str) -> io::Result<String> {
    let components = virtual_components(Path::new(root))?.collect::<Vec<_>>();
    if components.is_empty() {
        Ok("/".to_owned())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

fn normalize_virtual_path(path: &Path) -> io::Result<String> {
    normalize_configured_root(
        path.to_str()
            .ok_or_else(|| invalid("SMB-Root ist nicht UTF-8"))?,
    )
}

fn guid_bytes(guid: Guid) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&guid.data1.to_le_bytes());
    bytes[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    bytes[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    bytes[8..16].copy_from_slice(&guid.data4);
    bytes
}

fn filetime_to_unix(raw: u64) -> Option<i64> {
    if raw == 0 {
        return None;
    }
    // Der Vendor-Adapter verwirft alle Werte vor 1970, obwohl FILETIME bis 1601
    // zurückreicht und set_mtime negative Unix-Zeiten schreibt. Division mit Rest
    // nach unten erhält auch die Sekunde unmittelbar vor der Unix-Epoche korrekt.
    let ticks = i128::from(raw) - 116_444_736_000_000_000;
    i64::try_from(ticks.div_euclid(10_000_000)).ok()
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("SMB-Antwort ist zu kurz"))?;
    Ok(u32::from_le_bytes(
        value.try_into().expect("Länge wurde geprüft"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| invalid("SMB-Antwort ist zu kurz"))?;
    Ok(u64::from_le_bytes(
        value.try_into().expect("Länge wurde geprüft"),
    ))
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("Zufallsquelle fehlgeschlagen: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letzte_smb_verbindungsreferenz_gibt_transport_und_empfaenger_frei() {
        use smb2::transport::MockTransport;
        let runtime = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let transport = Arc::new(MockTransport::new());
            let connection = Connection::from_transport(
                Box::new(transport.clone()),
                Box::new(transport.clone()),
                "kuenstlicher-smb-server",
            );
            let other = connection.clone();
            drop(connection);
            assert!(Arc::strong_count(&transport) > 1);
            drop(other);
            let released = tokio::time::timeout(Duration::from_secs(2), async {
                while Arc::strong_count(&transport) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(
                released.is_ok(),
                "Transportreferenzen nach letztem Connection-Drop: {}",
                Arc::strong_count(&transport)
            );
        });
    }

    #[test]
    fn dateifaehigkeitsprobe_erhaelt_unklaren_ausgang_und_stoppt_folgeproben() {
        use smb2::msg::close::CloseResponse;
        use smb2::msg::create::CreateAction;
        use smb2::msg::header::{ErrorResponse, Header};
        use smb2::pack::{Pack, WriteCursor};
        use smb2::transport::MockTransport;
        use smb2::types::TreeId;

        struct NoSecrets;
        impl SecretResolver for NoSecrets {
            fn resolve(
                &self,
                _: &crate::KeychainSecretReference,
            ) -> io::Result<crate::SecretString> {
                panic!("Die bereits verbundene Testsitzung benötigt kein Secret")
            }
        }
        let packet = |command, status, body: &dyn Pack| {
            let mut header = Header::new_request(command);
            header.flags.set_response();
            header.credits = 16;
            header.status = status;
            let mut cursor = WriteCursor::new();
            header.pack(&mut cursor);
            body.pack(&mut cursor);
            cursor.into_inner()
        };
        let created = CreateResponse {
            oplock_level: OplockLevel::None,
            flags: 0,
            create_action: CreateAction::FileCreated,
            creation_time: FileTime(0),
            last_access_time: FileTime(0),
            last_write_time: FileTime(0),
            change_time: FileTime(0),
            allocation_size: 0,
            end_of_file: 0,
            file_attributes: FILE_ATTRIBUTE_DIRECTORY,
            file_id: FileId {
                persistent: 1,
                volatile: 2,
            },
            create_contexts: Vec::new(),
        };
        let closed = CloseResponse {
            flags: 0,
            creation_time: FileTime(0),
            last_access_time: FileTime(0),
            last_write_time: FileTime(0),
            change_time: FileTime(0),
            allocation_size: 0,
            end_of_file: 0,
            file_attributes: FILE_ATTRIBUTE_DIRECTORY,
        };
        let mock = Arc::new(MockTransport::new());
        mock.enable_auto_rewrite_msg_id();
        mock.queue_response(packet(Command::Create, NtStatus::SUCCESS, &created));
        mock.queue_response(packet(Command::Close, NtStatus::SUCCESS, &closed));
        for _ in 0..6 {
            mock.queue_response(packet(
                Command::Create,
                NtStatus::ACCESS_DENIED,
                &ErrorResponse {
                    error_context_count: 0,
                    error_data: Vec::new(),
                },
            ));
        }
        let runtime = Arc::new(
            RuntimeBuilder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap(),
        );
        let connection = runtime.block_on(async {
            Connection::from_transport(
                Box::new(mock.clone()),
                Box::new(mock.clone()),
                "test-server",
            )
        });
        let timeout = {
            let mock = mock.clone();
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                mock.close();
            })
        };
        let config = SmbConnectionConfig {
            host: "127.0.0.1".into(),
            port: 445,
            server_guid: [9; 16],
            share: "DDTEST".into(),
            username: "test".into(),
            domain: String::new(),
            requested_root: "/".into(),
            password_reference: crate::KeychainSecretReference::new("Test", "smb").unwrap(),
        };
        let identity = SmbServerIdentity {
            host: config.host.clone(),
            port: config.port,
            server_guid: config.server_guid,
            share: config.share.clone(),
            account_digest: [8; 32],
            canonical_root: "/".into(),
            dialect: 0x0311,
            signing_active: true,
            encryption_active: true,
            signing_algorithm: "AES-CMAC".into(),
            encryption_cipher: "AES-128-GCM".into(),
        };
        let vfs = Arc::new(SmbVfs {
            runtime,
            config,
            secrets: Arc::new(NoSecrets),
            reconnect: Mutex::new(()),
            state: Mutex::new(ProviderState {
                session: SmbSession {
                    connection,
                    tree: Arc::new(Tree {
                        tree_id: TreeId(1),
                        share_name: "DDTEST".into(),
                        server: "test-server".into(),
                        is_dfs: false,
                        encrypt_data: false,
                    }),
                    identity,
                },
                generation: 1,
                capabilities: RemoteCapabilities {
                    complete_listing: true,
                    ..RemoteCapabilities::default()
                },
            }),
        });
        let endpoint = crate::ProviderEndpoint::smb(vfs, Path::new("/")).unwrap();
        let result = endpoint.probe_file_capabilities();
        timeout.abort();
        mock.close();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!(
                "Unklarer Probenrest wurde zu einer gewöhnlichen Fähigkeitsabsage herabgestuft"
            ),
        };
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(!crate::error::recovery_paths(&error).is_empty());
        assert_eq!(
            mock.sent_count(),
            5,
            "Nach unklarem Ausgang darf keine weitere Probe beginnen"
        );
    }

    #[test]
    fn verschluesselte_sitzung_verwirft_unverschluesselte_antworten_am_empfaenger() {
        use smb2::crypto::encryption::{encrypt_message, Cipher};
        use smb2::crypto::signing::{sign_message, verify_signature, SigningAlgorithm};
        use smb2::msg::echo::{EchoRequest, EchoResponse};
        use smb2::msg::header::Header;
        use smb2::pack::{Pack, WriteCursor};
        use smb2::transport::MockTransport;
        use smb2::types::SessionId;

        let runtime = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut accepted_unprotected = Vec::new();
        runtime.block_on(async {
            for kind in [
                "plain",
                "signed-plain",
                "pending-plain",
                "encrypted",
                "bad-tag",
            ] {
                let mock = Arc::new(MockTransport::new());
                let mut connection = Connection::from_transport(
                    Box::new(mock.clone()),
                    Box::new(mock.clone()),
                    "kuenstlicher-smb-server",
                );
                connection.set_session_id(SessionId(7));
                connection.activate_signing(vec![0x11; 16], SigningAlgorithm::HmacSha256);
                connection.activate_encryption(vec![0x22; 16], vec![0x33; 16], Cipher::Aes128Gcm);
                let mut header = Header::new_request(Command::Echo);
                header.flags.set_response();
                header.session_id = SessionId(7);
                header.credits = 60_000;
                if kind == "signed-plain" {
                    header.flags.set_signed();
                }
                let mut cursor = WriteCursor::new();
                header.pack(&mut cursor);
                EchoResponse.pack(&mut cursor);
                let mut reply = cursor.into_inner();
                if kind == "signed-plain" {
                    sign_message(
                        &mut reply,
                        &[0x11; 16],
                        SigningAlgorithm::HmacSha256,
                        0,
                        false,
                    )
                    .unwrap();
                    verify_signature(&reply, &[0x11; 16], SigningAlgorithm::HmacSha256, 0, false)
                        .unwrap();
                }
                let (mut envelope, ciphertext) =
                    encrypt_message(&reply, &[0x33; 16], Cipher::Aes128Gcm, &[0x44; 16], 7)
                        .unwrap();
                envelope.extend(ciphertext);
                if kind == "encrypted" || kind == "bad-tag" {
                    reply = envelope.clone();
                    if kind == "bad-tag" {
                        reply[4] ^= 1;
                    }
                } else if kind == "pending-plain" {
                    reply[8..12].copy_from_slice(&NtStatus::PENDING.0.to_le_bytes());
                }
                let sender = connection.clone();
                let request =
                    tokio::spawn(
                        async move { sender.execute(Command::Echo, &EchoRequest, None).await },
                    );
                tokio::time::timeout(Duration::from_secs(2), async {
                    while mock.sent_count() == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("Künstliche Anfrage wurde nicht gesendet");
                assert_eq!(mock.sent_message(0).unwrap()[0], 0xfd);
                mock.queue_response(reply);
                if kind == "pending-plain" {
                    mock.queue_response(envelope);
                }
                let result = tokio::time::timeout(Duration::from_secs(2), request)
                    .await
                    .expect("Wartende Anfrage wurde nicht abgeschlossen")
                    .unwrap();
                if kind == "encrypted" {
                    EchoResponse::unpack(&mut ReadCursor::new(&result.unwrap().body)).unwrap();
                } else if result.is_ok() {
                    accepted_unprotected.push(kind);
                } else {
                    assert!(connection.diagnostics().disconnected);
                    assert_eq!(connection.credits(), 1);
                    assert!(connection
                        .execute(Command::Echo, &EchoRequest, None)
                        .await
                        .is_err());
                    assert_eq!(mock.sent_count(), 1);
                }
                mock.close();
            }
        });
        assert!(accepted_unprotected.is_empty(),
            "Als verschlüsselt gebundene Sitzung akzeptierte ungeschützte Antworten: {accepted_unprotected:?}");
    }

    #[test]
    fn restpruefung_versucht_beide_roots_und_erhaelt_rest_nach_listingfehler() {
        let base = Path::new("/pane/.doppeldecker-probe-owned");
        let mut calls = Vec::new();
        let error = verify_probe_absence(base, |root| {
            calls.push(root.to_owned());
            if root == Path::new("/pane") {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Listing verweigert",
                ))
            } else {
                Ok(vec![".doppeldecker-probe-owned".into(), "behalten".into()])
            }
        })
        .unwrap_err();
        assert_eq!(calls, [Path::new("/pane"), Path::new("/")]);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains("Listing verweigert"));
        assert!(error
            .to_string()
            .contains("Profilroot: /.doppeldecker-probe-owned"));
        let mut calls = Vec::new();
        verify_probe_absence(base, |root| {
            calls.push(root.to_owned());
            Ok(vec!["behalten".into()])
        })
        .unwrap();
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn filetime_liest_auch_zeitpunkte_vor_1970() {
        assert_eq!(filetime_to_unix(0), None);
        assert_eq!(filetime_to_unix(116_444_735_990_000_000), Some(-1));
        assert_eq!(filetime_to_unix(116_444_735_999_999_999), Some(-1));
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), Some(0));
        assert_eq!(
            filetime_to_unix(133_485_408_000_000_000),
            Some(1_704_067_200)
        );
        let before_epoch = UNIX_EPOCH - Duration::from_secs(86_400);
        assert_eq!(
            filetime_to_unix(FileTime::from_system_time(before_epoch).0),
            Some(-86_400)
        );
    }

    /// Die SMB-Löschung darf nur über `delete_with_readback` laufen.
    ///
    /// Ohne Readback wäre die Serverantwort der einzige Beleg — und die ist keiner:
    /// SMB löscht ein mit `FILE_DELETE_ON_CLOSE` geöffnetes Objekt erst, wenn das
    /// letzte Handle geschlossen ist. Hält ein anderer Client eines, antwortet CLOSE
    /// mit Erfolg, während die Datei sichtbar bleibt. Ein echter Beweis braucht einen
    /// Samba-Server; diese Grenze lässt sich aber an der Quelle festhalten.
    #[test]
    fn jede_smb_loeschung_laeuft_ueber_den_readback() {
        let quelle = include_str!("smb.rs");
        // Zusammengesetzt, damit die Suchzeichenkette nicht selbst als Treffer zählt.
        let aufruf = format!("delete_{}(", "path");
        let treffer: Vec<&str> = quelle
            .lines()
            .filter(|zeile| zeile.contains(&aufruf) && !zeile.trim_start().starts_with("//"))
            .collect();
        assert_eq!(
            treffer.len(),
            2,
            "Erwartet: genau die Definition und der eine Aufruf in delete_with_readback, gefunden: {treffer:?}"
        );
        assert!(treffer.iter().any(|zeile| zeile.contains("async fn")));

        // Der Aufruf muss im Rumpf von `delete_with_readback` liegen.
        let start = quelle
            .find("fn delete_with_readback(")
            .expect("delete_with_readback fehlt");
        let rest = &quelle[start..];
        let ende = rest
            .find("\n    fn remove_file_unchecked(")
            .expect("Folgefunktion fehlt");
        let rumpf = &rest[..ende];
        assert!(rumpf.contains(&aufruf));
        assert!(
            rumpf.contains("ErrorCode::OutcomeUnclear"),
            "Ein unbestätigter Readback muss als unklarer Ausgang enden"
        );
        assert!(
            rumpf.contains("io::ErrorKind::NotFound => Ok(())"),
            "Nur ein echtes NotFound darf als Abwesenheit gelten"
        );
    }

    fn cleanup_entry(name: &str, is_dir: bool) -> SmbListedEntry {
        SmbListedEntry {
            name: name.into(),
            is_dir,
            is_symlink: false,
            size: 0,
            mtime: None,
            created: None,
            change_time: 0,
            file_id: 1,
        }
    }

    #[test]
    fn fehlgeschlagene_probe_entfernt_keine_kollidierte_fremde_datei() {
        let root = tempfile::tempdir().unwrap();
        let foreign = root.path().join("first");
        std::fs::write(&foreign, b"fremde Bytes erhalten").unwrap();
        let collision = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&foreign)
            .unwrap_err();
        assert_eq!(collision.kind(), io::ErrorKind::AlreadyExists);
        let result = Err::<(), _>(collision);
        let cleanup = cleanup_after_probe(&result, root.path(), || {
            execute_probe_cleanup(
                vec![cleanup_entry("first", false)],
                &[("first", false)],
                root.path(),
                |path, _| std::fs::remove_file(path),
            )
        });
        assert_eq!(
            std::fs::read(&foreign).ok(),
            Some(b"fremde Bytes erhalten".to_vec())
        );
        let error = cleanup.unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(crate::error::recovery_paths(&error)
            .contains(&root.path().to_string_lossy().into_owned()));
    }

    #[test]
    fn probe_cleanup_entfernt_vor_unbekanntem_bestand_nichts() {
        let mut removed = Vec::new();
        let error = execute_probe_cleanup(
            vec![
                cleanup_entry("first", false),
                cleanup_entry("behalten", true),
            ],
            &[("first", false)],
            Path::new("/probe"),
            |path, _| {
                removed.push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(removed.is_empty());
    }

    #[test]
    fn probe_cleanup_erhaelt_fremde_kinder_bekannter_probeordner() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("first");
        std::fs::create_dir(&directory).unwrap();
        let foreign = directory.join("behalten.txt");
        std::fs::write(&foreign, b"fremde Bytes").unwrap();
        let error = execute_probe_cleanup(
            vec![cleanup_entry("first", true)],
            &[("first", true)],
            root.path(),
            |path, is_dir| {
                if is_dir {
                    std::fs::remove_dir(path)
                } else {
                    std::fs::remove_file(path)
                }
            },
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(foreign).unwrap(), b"fremde Bytes");
    }

    #[test]
    fn probe_cleanup_erhaelt_unbekannte_blaetter_und_ordner() {
        for is_dir in [false, true] {
            let error = plan_probe_cleanup(
                vec![
                    cleanup_entry("first", false),
                    cleanup_entry("behalten", is_dir),
                ],
                &[("first", false)],
                Path::new("/probe"),
            )
            .unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert!(error.to_string().contains("/probe/behalten"));
        }
        let error = plan_probe_cleanup(
            vec![cleanup_entry("first", true)],
            &[("first", false)],
            Path::new("/probe"),
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            plan_probe_cleanup(
                vec![cleanup_entry("first", false)],
                &[("first", false)],
                Path::new("/probe"),
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn virtuelle_pfade_bleiben_in_der_freigabe() {
        assert_eq!(
            map_virtual_path("/Archiv", Path::new("/Fotos/2026")).unwrap(),
            "Archiv\\Fotos\\2026"
        );
        assert!(map_virtual_path("/Archiv", Path::new("../fremd")).is_err());
        assert!(map_virtual_path("/Archiv", Path::new("/a\\b")).is_err());
    }

    #[test]
    fn gemappter_smb_pfad_passt_vollstaendig_in_das_utf16_laengenfeld() {
        let boundary = format!("/{}", vec!["a".repeat(255); 128].join("/"));
        let mapped = map_virtual_path("/", Path::new(&boundary)).unwrap();
        assert_eq!(mapped.encode_utf16().count(), 32_767);
        assert!(map_virtual_path("/", Path::new(&format!("{boundary}/a"))).is_err(),
            "CREATE darf den vollständigen Pfad nicht durch ein überlaufendes NameLength-Feld abschneiden");
        assert!(
            map_virtual_path("/profil", Path::new(&boundary)).is_err(),
            "Der Profilroot gehört zum protokollseitigen Längenbudget"
        );
    }

    #[test]
    fn file_id_listing_parser_bindet_paging_und_reparse() {
        let name = "ziel.txt".encode_utf16().collect::<Vec<_>>();
        let mut bytes = vec![0u8; 104 + name.len() * 2];
        bytes[8..16].copy_from_slice(&FileTime::from_system_time(UNIX_EPOCH).0.to_le_bytes());
        bytes[24..32].copy_from_slice(&FileTime::from_system_time(UNIX_EPOCH).0.to_le_bytes());
        bytes[32..40].copy_from_slice(&77u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&123u64.to_le_bytes());
        bytes[56..60].copy_from_slice(&FILE_ATTRIBUTE_REPARSE_POINT.to_le_bytes());
        bytes[60..64].copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        bytes[96..104].copy_from_slice(&99u64.to_le_bytes());
        for (index, unit) in name.into_iter().enumerate() {
            bytes[104 + index * 2..106 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        let parsed = parse_file_id_both_directory_info(&bytes).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "ziel.txt");
        assert!(parsed[0].is_symlink);
        assert_eq!(parsed[0].file_id, 99);
        assert_eq!(parsed[0].created, Some(0));
        // Reparse-Ziele gelten nicht als normale Dateien und erhalten daher keine
        // möglicherweise irreführende Dateiendung.
        assert_eq!(parsed[0].entry().extension, None);

        bytes[60..64].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse_file_id_both_directory_info(&bytes).is_err());
    }

    #[test]
    fn listing_parser_lehnt_ueberlappung_und_kaputtes_utf16_ab() {
        let mut overlap = vec![0u8; 208];
        overlap[0..4].copy_from_slice(&104u32.to_le_bytes());
        overlap[60..64].copy_from_slice(&2u32.to_le_bytes());
        overlap[104..106].copy_from_slice(&(b'a' as u16).to_le_bytes());
        assert!(parse_file_id_both_directory_info(&overlap).is_err());

        let mut utf16 = vec![0u8; 106];
        utf16[60..64].copy_from_slice(&2u32.to_le_bytes());
        utf16[104..106].copy_from_slice(&0xd800u16.to_le_bytes());
        assert!(parse_file_id_both_directory_info(&utf16).is_err());
    }

    #[test]
    fn replace_puffer_setzt_replace_if_exists_explizit() {
        let replace = rename_buffer("ziel", true);
        let no_replace = rename_buffer("ziel", false);
        assert_eq!(replace[0], 1);
        assert_eq!(no_replace[0], 0);
        assert_eq!(&replace[16..20], &8u32.to_le_bytes());
    }
}
