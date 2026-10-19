//! Persistente Baseline, Drei-Wege-Planung und Ausführung für bidirektionalen Sync.
//!
//! Dieses Modul erstellt vollständige, inhaltsgebundene Baumsichten, leitet daraus
//! gebundene Projektionen ab und führt sie über die atomare Einweg-Engine aus. Das
//! Laufjournal hält Teilcommits, Resume, Rollback und atomare Typwechsel so fest,
//! dass ein neuer Normalplan keinen unvollständigen Lauf überschreiben kann.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::metadata::MetadataProfile;
use crate::metadata::{ForkKind, MetadataCapabilities, MetadataManifest, XattrValue};
use crate::path::validate_relative_path;
use crate::real_mode::RootIdentity;
use crate::vfs::{
    CompleteDirectoryListing, Entry, LocalRootIdentity, ProofListEntry, SparseRead, Vfs,
};

const LEGACY_BASELINE_VERSION: u32 = 1;
const BASELINE_VERSION: u32 = 2;
const MAX_BASELINE_BYTES: u64 = 64 * 1024 * 1024;
const LEGACY_RUN_JOURNAL_VERSION: u32 = 7;
const RUN_JOURNAL_VERSION: u32 = 8;
const MAX_RUN_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 128 * 1024;

/// Schreibgeschützte Sicht einer Gewinnerauswahl im Namespace des Ziels. Nicht
/// ausgewählte Pfade stammen aus dem Ziel und erscheinen der Einweg-Engine deshalb
/// bereits identisch; ausgewählte Pfade werden bytegenau aus der Gegenseite gelesen.
struct LocalProjectedVfs {
    source: Arc<dyn Vfs>,
    source_root: PathBuf,
    target: Arc<dyn Vfs>,
    target_root: PathBuf,
    selected: BTreeSet<PathBuf>,
    deleted: BTreeSet<PathBuf>,
    aliases: BTreeMap<PathBuf, PathBuf>,
    selection_digest: [u8; 32],
}

impl LocalProjectedVfs {
    fn new(
        source: Arc<dyn Vfs>,
        source_root: PathBuf,
        target: Arc<dyn Vfs>,
        target_root: PathBuf,
        selected: BTreeSet<PathBuf>,
        deleted: BTreeSet<PathBuf>,
    ) -> io::Result<Self> {
        Self::new_with_aliases(
            source,
            source_root,
            target,
            target_root,
            selected,
            deleted,
            BTreeMap::new(),
        )
    }

    fn new_with_aliases(
        source: Arc<dyn Vfs>,
        source_root: PathBuf,
        target: Arc<dyn Vfs>,
        target_root: PathBuf,
        selected: BTreeSet<PathBuf>,
        deleted: BTreeSet<PathBuf>,
        aliases: BTreeMap<PathBuf, PathBuf>,
    ) -> io::Result<Self> {
        if selected.is_empty() && deleted.is_empty() {
            return Err(invalid_input(
                "Projektion benötigt mindestens einen Wirkungspfad",
            ));
        }
        if selected.iter().any(|path| deleted.contains(path)) {
            return Err(invalid_input(
                "Projektionspfad darf nicht zugleich gewählt und gelöscht sein",
            ));
        }
        if aliases.keys().any(|path| !selected.contains(path)) {
            return Err(invalid_input(
                "Alias muss zugleich ein ausgewählter Projektionspfad sein",
            ));
        }
        let mut hash = blake3::Hasher::new();
        hash.update(&[1]);
        for path in &selected {
            validate_projected_relative(path)?;
            let bytes = path.as_os_str().as_encoded_bytes();
            hash.update(&(bytes.len() as u64).to_be_bytes());
            hash.update(bytes);
        }
        for path in &deleted {
            validate_projected_relative(path)?;
            let bytes = path.as_os_str().as_encoded_bytes();
            hash.update(&[2]);
            hash.update(&(bytes.len() as u64).to_be_bytes());
            hash.update(bytes);
        }
        for (target, source) in &aliases {
            validate_projected_relative(source)?;
            hash.update(&[3]);
            for path in [target, source] {
                let bytes = path.as_os_str().as_encoded_bytes();
                hash.update(&(bytes.len() as u64).to_be_bytes());
                hash.update(bytes);
            }
        }
        Ok(Self {
            source,
            source_root,
            target,
            target_root,
            selected,
            deleted,
            aliases,
            selection_digest: *hash.finalize().as_bytes(),
        })
    }

    fn relative<'a>(&self, path: &'a Path) -> io::Result<&'a Path> {
        path.strip_prefix(&self.target_root)
            .map_err(|_| invalid_input("Pfad liegt außerhalb der projizierten Zielwurzel"))
    }

    fn source_path(&self, relative: &Path) -> PathBuf {
        self.source_root.join(
            self.aliases
                .get(relative)
                .map(PathBuf::as_path)
                .unwrap_or(relative),
        )
    }

    fn uses_source(&self, relative: &Path) -> bool {
        self.selected.contains(relative)
    }

    fn has_selected_descendant(&self, relative: &Path) -> bool {
        self.selected
            .iter()
            .any(|selected| selected.starts_with(relative) && selected != relative)
    }

    fn is_deleted(&self, relative: &Path) -> bool {
        self.deleted.contains(relative)
    }

    fn has_deleted_descendant(&self, relative: &Path) -> bool {
        self.deleted
            .iter()
            .any(|deleted| deleted.starts_with(relative) && deleted != relative)
    }

    fn read_vfs_and_path<'a>(&'a self, path: &Path) -> io::Result<(&'a dyn Vfs, PathBuf)> {
        let relative = self.relative(path)?;
        if self.is_deleted(relative) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Pfad fehlt absichtlich in der Löschprojektion",
            ));
        }
        if self.uses_source(relative) {
            Ok((self.source.as_ref(), self.source_path(relative)))
        } else {
            Ok((self.target.as_ref(), path.to_path_buf()))
        }
    }

    fn read_only() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ProjectedVfs ist strikt schreibgeschützt",
        )
    }
}

impl Vfs for LocalProjectedVfs {
    fn supports_mode_write(&self) -> bool {
        false
    }

    fn supports_mtime_write(&self) -> bool {
        false
    }

    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(self
            .list_complete(path)?
            .entries
            .into_iter()
            .map(|listed| listed.entry)
            .collect())
    }

    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let relative = self.relative(path)?;
        let uses_source = self.uses_source(relative);
        let has_selected_descendant = self.has_selected_descendant(relative);
        let has_deleted_descendant = self.has_deleted_descendant(relative);
        if !uses_source && !has_selected_descendant && !has_deleted_descendant {
            let target_listing = self.target.list_complete(path)?;
            if !target_listing.complete {
                return Err(invalid_data("Projiziertes Ziellisting ist unvollständig"));
            }
            return Ok(target_listing);
        }
        let source_listing = if uses_source || has_selected_descendant {
            let source_path = self.source_path(relative);
            let listing = self.source.list_complete(&source_path)?;
            if !listing.complete {
                return Err(invalid_data("Projiziertes Quelllisting ist unvollständig"));
            }
            Some(listing)
        } else {
            None
        };

        let target_listing = match self.target.list_complete(path) {
            Ok(listing) if listing.complete => Some(listing),
            Ok(_) => return Err(invalid_data("Projiziertes Ziellisting ist unvollständig")),
            Err(error) if error.kind() == io::ErrorKind::NotFound && uses_source => None,
            Err(error) => return Err(error),
        };

        let mut merged: BTreeMap<Vec<u8>, ProofListEntry> = target_listing
            .as_ref()
            .into_iter()
            .flat_map(|listing| listing.entries.iter().cloned())
            .map(|listed| (listed.raw_name.clone(), listed))
            .collect();
        let mut source_names = BTreeSet::new();
        if let Some(source_listing) = &source_listing {
            for listed in &source_listing.entries {
                let child = relative.join(&listed.entry.name);
                source_names.insert(listed.raw_name.clone());
                if self.uses_source(&child) || self.has_selected_descendant(&child) {
                    merged.insert(listed.raw_name.clone(), listed.clone());
                }
            }
        }
        for (target, source) in &self.aliases {
            if target.parent().unwrap_or_else(|| Path::new("")) != relative {
                continue;
            }
            let source_parent = source.parent().unwrap_or_else(|| Path::new(""));
            if source_parent != relative {
                // Unterhalb eines umbenannten Alias-Roots bleiben die Kindnamen
                // gleich. Das bereits auf den Quellordner abgebildete Listing fügt
                // sie normal ein; nur der umbenannte Root selbst braucht Injektion.
                if source.file_name() == target.file_name() {
                    continue;
                }
                return Err(invalid_data(
                    "Verschachtelter Konfliktalias ändert einen Kindnamen",
                ));
            }
            let source_name = source
                .file_name()
                .ok_or_else(|| invalid_data("Konfliktalias besitzt keinen Quellnamen"))?;
            let target_name = target
                .file_name()
                .ok_or_else(|| invalid_data("Konfliktalias besitzt keinen Zielnamen"))?;
            let source_raw = source_name.as_encoded_bytes();
            let mut listed = source_listing
                .as_ref()
                .and_then(|listing| {
                    listing
                        .entries
                        .iter()
                        .find(|listed| listed.raw_name == source_raw)
                })
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Quelle des Konfliktalias fehlt im vollständigen Listing",
                    )
                })?;
            let target_name = target_name
                .to_str()
                .ok_or_else(|| invalid_data("Konfliktalias ist nicht verlustfrei UTF-8"))?;
            listed.entry.name = target_name.to_owned();
            listed.raw_name = target_name.as_bytes().to_vec();
            source_names.insert(listed.raw_name.clone());
            merged.insert(listed.raw_name.clone(), listed);
        }
        merged.retain(|_, listed| !self.is_deleted(&relative.join(&listed.entry.name)));
        for selected in self
            .selected
            .iter()
            .filter(|item| item.starts_with(relative))
        {
            let remainder = selected
                .strip_prefix(relative)
                .map_err(|_| invalid_data("Projektionsauswahl verlor ihren Elternpfad"))?;
            let Some(first) = remainder.components().next() else {
                continue;
            };
            let raw = first.as_os_str().as_encoded_bytes();
            if !source_names.contains(raw) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Ausgewählter Quellpfad fehlt im vollständigen Listing",
                ));
            }
        }

        let mut token_hash = blake3::Hasher::new();
        token_hash.update(&[1]);
        token_hash.update(&self.selection_digest);
        token_hash.update(relative.as_os_str().as_encoded_bytes());
        encode_optional_bytes(
            &mut token_hash,
            target_listing
                .as_ref()
                .and_then(|listing| listing.provider_token.as_deref()),
        );
        encode_optional_bytes(
            &mut token_hash,
            source_listing
                .as_ref()
                .and_then(|listing| listing.provider_token.as_deref()),
        );
        Ok(CompleteDirectoryListing {
            entries: merged.into_values().collect(),
            provider_token: Some(token_hash.finalize().as_bytes().to_vec()),
            complete: true,
        })
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let (vfs, mapped) = self.read_vfs_and_path(path)?;
        vfs.stat(&mapped)
    }

    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }

    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let (vfs, mapped) = self.read_vfs_and_path(path)?;
        vfs.open_read(&mapped)
    }

    fn open_write(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(Self::read_only())
    }

    fn open_write_new(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(Self::read_only())
    }

    fn open_sparse_read(&self, path: &Path) -> io::Result<Box<dyn SparseRead>> {
        let (vfs, mapped) = self.read_vfs_and_path(path)?;
        vfs.open_sparse_read(&mapped)
    }

    fn sync_file(&self, _path: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn sync_dir(&self, _path: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn set_mtime(&self, _path: &Path, _mtime: i64) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn metadata_capabilities(&self, root: &Path) -> io::Result<MetadataCapabilities> {
        let (vfs, mapped) = self.read_vfs_and_path(root)?;
        vfs.metadata_capabilities(&mapped)
    }

    fn metadata_manifest(&self, path: &Path) -> io::Result<MetadataManifest> {
        let (vfs, mapped) = self.read_vfs_and_path(path)?;
        vfs.metadata_manifest(&mapped)
    }

    fn read_xattrs(&self, path: &Path) -> io::Result<Vec<XattrValue>> {
        let (vfs, mapped) = self.read_vfs_and_path(path)?;
        vfs.read_xattrs(&mapped)
    }

    fn open_fork_read(&self, path: &Path, kind: ForkKind) -> io::Result<Box<dyn Read + Send>> {
        let (vfs, mapped) = self.read_vfs_and_path(path)?;
        vfs.open_fork_read(&mapped, kind)
    }

    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn rename_noreplace(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(Self::read_only())
    }

    fn anchored_root(&self) -> Option<&Path> {
        self.target.anchored_root()
    }

    fn anchored_identity(&self) -> Option<(u64, u64)> {
        self.target.anchored_identity()
    }

    fn anchor_is_current(&self) -> io::Result<bool> {
        Ok(self.source.anchor_is_current()? && self.target.anchor_is_current()?)
    }

    fn local_root_identity(&self, path: &Path) -> io::Result<LocalRootIdentity> {
        self.target.local_root_identity(path)
    }

    fn root_identity(&self, path: &Path) -> io::Result<RootIdentity> {
        // Die Projektion lebt im Namespace des Ziels. Der Selektionsdigest steckt
        // zusätzlich im vollständigen Listingtoken; die Provider-/Sitzungsidentität
        // selbst muss deshalb exakt die echte Zielwurzel bleiben.
        self.target.root_identity(path)
    }

    fn available_bytes(&self, path: &Path) -> io::Result<u64> {
        self.target.available_bytes(path)
    }
}

fn validate_projected_relative(path: &Path) -> io::Result<()> {
    validate_relative_path(path)
}

/// Grenzen verhindern, dass ein unerwartet großer oder zyklisch wirkender Provider
/// den Prozess während einer vermeintlich lesenden Vorschau erschöpft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotLimits {
    pub max_entries: u64,
    pub max_depth: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_entries: 2_000_000,
            max_depth: 256,
            max_file_bytes: 16 * 1024 * 1024 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024 * 1024 * 1024,
        }
    }
}

/// Zufällige, dauerhaft stabile Identität eines Sync-Paars.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PairId(pub [u8; 16]);

impl PairId {
    pub fn generate() -> io::Result<Self> {
        let mut value = [0_u8; 16];
        getrandom::fill(&mut value).map_err(|error| io::Error::other(error.to_string()))?;
        if value == [0; 16] {
            return Err(invalid_data("Zufallsgenerator lieferte eine leere Paar-ID"));
        }
        Ok(Self(value))
    }

    fn validate(self) -> io::Result<()> {
        if self.0 == [0; 16] {
            Err(invalid_data("Paar-ID darf nicht leer sein"))
        } else {
            Ok(())
        }
    }
}

/// Dauerhafte, geheimnisfreie Bindung eines Endpunkts. Kurzlebige Sitzungswerte sind
/// absichtlich nur Teil von [`TreeSnapshot::root_identity`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", content = "identity", rename_all = "snake_case")]
pub enum EndpointBinding {
    Local(LocalEndpointBinding),
    Sftp(RemoteEndpointBinding),
    Ftps(RemoteEndpointBinding),
    WebDav(RemoteEndpointBinding),
    Smb(RemoteEndpointBinding),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalEndpointBinding {
    pub canonical_root: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub fsid: Vec<u8>,
    pub volume_id: Option<[u8; 16]>,
    pub fs_type: String,
}

/// Provider-spezifische öffentliche Felder werden kanonisch serialisiert und als
/// Digest gebunden. Das DTO nimmt absichtlich keine Zugangsdatenreferenz an.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEndpointBinding {
    #[serde(default = "legacy_endpoint_binding_schema")]
    pub schema_version: u32,
    pub canonical_root: String,
    pub account_digest: [u8; 32],
    pub public_identity_digest: [u8; 32],
}

/// Exakte Serialisierungsform der vor Schemafeld-Einführung geschriebenen
/// Endpunktbindungen. Serde-Defaults reparieren das Lesen, dürfen aber den
/// historischen Digest nicht nachträglich um ein neues Feld erweitern.
#[derive(Serialize)]
#[serde(tag = "provider", content = "identity", rename_all = "snake_case")]
enum LegacyEndpointBindingRef<'a> {
    Local(&'a LocalEndpointBinding),
    Sftp(LegacyRemoteEndpointBindingRef<'a>),
    Ftps(LegacyRemoteEndpointBindingRef<'a>),
    WebDav(LegacyRemoteEndpointBindingRef<'a>),
    Smb(LegacyRemoteEndpointBindingRef<'a>),
}

#[derive(Clone, Copy, Serialize)]
struct LegacyRemoteEndpointBindingRef<'a> {
    canonical_root: &'a str,
    account_digest: [u8; 32],
    public_identity_digest: [u8; 32],
}

impl<'a> LegacyEndpointBindingRef<'a> {
    fn new(binding: &'a EndpointBinding) -> Self {
        let remote = |value: &'a RemoteEndpointBinding| LegacyRemoteEndpointBindingRef {
            canonical_root: &value.canonical_root,
            account_digest: value.account_digest,
            public_identity_digest: value.public_identity_digest,
        };
        match binding {
            EndpointBinding::Local(value) => Self::Local(value),
            EndpointBinding::Sftp(value) => Self::Sftp(remote(value)),
            EndpointBinding::Ftps(value) => Self::Ftps(remote(value)),
            EndpointBinding::WebDav(value) => Self::WebDav(remote(value)),
            EndpointBinding::Smb(value) => Self::Smb(remote(value)),
        }
    }
}

const fn legacy_endpoint_binding_schema() -> u32 {
    1
}

impl EndpointBinding {
    pub fn local(identity: &RootIdentity) -> io::Result<Self> {
        let RootIdentity::Local(identity) = identity else {
            return Err(invalid_input("Root-Identität ist nicht lokal"));
        };
        Ok(Self::Local(LocalEndpointBinding {
            canonical_root: identity.display_path.clone(),
            device: identity.device,
            inode: identity.inode,
            fsid: identity.mount.fsid.clone(),
            volume_id: identity.mount.volume_id,
            fs_type: identity.mount.fs_type.clone(),
        }))
    }

    /// Baut die dauerhafte, geheimnisfreie Bindung aus einer frischen Remote-
    /// Rootidentität. Sitzungsgeneration und kurzlebige Tree-IDs bleiben bewusst
    /// draußen; Konto, Serveridentität und konkreter virtueller Root bleiben gebunden.
    pub fn remote(identity: &RootIdentity, account_digest: [u8; 32]) -> io::Result<Self> {
        if account_digest == [0; 32] {
            return Err(invalid_data("Remote-Kontodigest darf nicht leer sein"));
        }
        let (schema_version, canonical_root, public_identity_digest) = match identity {
            RootIdentity::Sftp(value) => {
                value.validate()?;
                let public = serializable_digest(&(1_u32, "sftp", &value.server))?;
                (1, value.virtual_root.clone(), public)
            }
            RootIdentity::Ftps(value) => {
                value.validate()?;
                let public = serializable_digest(&(2_u32, "ftps", &value.server))?;
                (2, value.virtual_root.clone(), public)
            }
            RootIdentity::WebDav(value) => {
                value.validate()?;
                if value.server.account_digest != account_digest {
                    return Err(invalid_data(
                        "WebDAV-Kontodigest passt nicht zur Serveridentität",
                    ));
                }
                let public = serializable_digest(&(2_u32, "webdav", &value.server))?;
                (2, value.virtual_root.clone(), public)
            }
            RootIdentity::Smb(value) => {
                value.validate()?;
                if value.server.account_digest != account_digest {
                    return Err(invalid_data(
                        "SMB-Kontodigest passt nicht zur Serveridentität",
                    ));
                }
                let public = serializable_digest(&(1_u32, "smb", &value.server))?;
                (1, value.virtual_root.clone(), public)
            }
            RootIdentity::Local(_) => {
                return Err(invalid_input("Root-Identität ist nicht remote"));
            }
        };
        let value = RemoteEndpointBinding {
            schema_version,
            canonical_root,
            account_digest,
            public_identity_digest,
        };
        let result = match identity {
            RootIdentity::Sftp(_) => Self::Sftp(value),
            RootIdentity::Ftps(_) => Self::Ftps(value),
            RootIdentity::WebDav(_) => Self::WebDav(value),
            RootIdentity::Smb(_) => Self::Smb(value),
            RootIdentity::Local(_) => unreachable!("oben bereits abgelehnt"),
        };
        result.validate()?;
        Ok(result)
    }

    fn validate(&self) -> io::Result<()> {
        match self {
            Self::Local(value) => {
                if !value.canonical_root.is_absolute()
                    || value.fsid.is_empty()
                    || value.fs_type.is_empty()
                {
                    return Err(invalid_data("Lokale Endpunktbindung ist unvollständig"));
                }
            }
            Self::Sftp(value) | Self::Ftps(value) | Self::WebDav(value) | Self::Smb(value) => {
                if value.canonical_root.is_empty()
                    || !matches!(value.schema_version, 1 | 2)
                    || value.account_digest == [0; 32]
                    || value.public_identity_digest == [0; 32]
                {
                    return Err(invalid_data("Remote-Endpunktbindung ist unvollständig"));
                }
            }
        }
        Ok(())
    }
}

fn serializable_digest(value: &impl Serialize) -> io::Result<[u8; 32]> {
    serde_json::to_vec(value)
        .map(|payload| *blake3::hash(&payload).as_bytes())
        .map_err(io::Error::other)
}

/// Bytegenauer relativer Pfad. Die Komponentenform schließt absolute Pfade und
/// Mehrdeutigkeiten durch Trennzeichen aus.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BinaryRelativePath(pub Vec<Vec<u8>>);

impl BinaryRelativePath {
    fn validate(&self) -> io::Result<()> {
        if self.0.is_empty() || self.0.len() > 256 {
            return Err(invalid_data("Relativer Pfad hat eine ungültige Tiefe"));
        }
        for component in &self.0 {
            if component.is_empty()
                || component == b"."
                || component == b".."
                || component.contains(&0)
                || component.contains(&b'/')
            {
                return Err(invalid_data(
                    "Relativer Pfad enthält eine ungültige Komponente",
                ));
            }
        }
        Ok(())
    }

    pub fn display_lossy(&self) -> String {
        self.0
            .iter()
            .map(|part| String::from_utf8_lossy(part))
            .collect::<Vec<_>>()
            .join("/")
    }

    fn to_path_buf(&self) -> io::Result<PathBuf> {
        self.validate()?;
        let mut path = PathBuf::new();
        for component in &self.0 {
            let component = std::str::from_utf8(component)
                .map_err(|_| invalid_data("Projektionspfad ist nicht verlustfrei UTF-8"))?;
            path.push(component);
        }
        validate_projected_relative(&path)?;
        Ok(path)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotEntryKind {
    File,
    Directory,
}

/// Vollständiger semantischer Zustand eines Pfads. `mtime` und Provider-ID dienen
/// der Diagnose bzw. zusätzlichen Drift-Erkennung, nicht der Gewinnerwahl.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub kind: SnapshotEntryKind,
    pub size: u64,
    pub mtime: Option<i64>,
    pub content_digest: Option<[u8; 32]>,
    pub metadata_digest: [u8; 32],
    pub provider_id: Option<Vec<u8>>,
    pub entry_digest: [u8; 32],
}

impl SnapshotEntry {
    fn validate(&self) -> io::Result<()> {
        if (self.kind == SnapshotEntryKind::File) != self.content_digest.is_some() {
            return Err(invalid_data("Inhaltsdigest passt nicht zum Eintragstyp"));
        }
        if self.entry_digest != self.canonical_digest() {
            return Err(invalid_data("Eintragsdigest ist ungültig"));
        }
        Ok(())
    }

    fn semantically_equal(&self, other: &Self) -> bool {
        self.kind == other.kind
            // Verzeichnisgrößen sind provider- und dateisystemspezifische
            // Verwaltungswerte. Nur die rekursiv gebundenen Kinder bestimmen den
            // Inhalt eines Ordners; Dateigrößen bleiben dagegen semantisch.
            && (self.kind == SnapshotEntryKind::Directory || self.size == other.size)
            && self.content_digest == other.content_digest
            && self.metadata_digest == other.metadata_digest
    }

    fn canonical_digest(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(&[
            1,
            match self.kind {
                SnapshotEntryKind::File => 1,
                SnapshotEntryKind::Directory => 2,
            },
        ]);
        hash.update(&self.size.to_be_bytes());
        encode_optional_bytes(
            &mut hash,
            self.content_digest.as_ref().map(|value| value.as_slice()),
        );
        hash.update(&self.metadata_digest);
        *hash.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub path: BinaryRelativePath,
    pub entry: SnapshotEntry,
}

/// Unveränderliche, vollständig gelesene Baumsicht eines Endpunkts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSnapshot {
    pub root_identity: RootIdentity,
    pub metadata_profile: MetadataProfile,
    pub entries: Vec<SnapshotRecord>,
    pub total_file_bytes: u64,
    pub snapshot_digest: [u8; 32],
}

impl TreeSnapshot {
    fn validate(&self) -> io::Result<()> {
        if self.metadata_profile != MetadataProfile::Portable {
            return Err(invalid_data(
                "Bidirektionaler Snapshot verwendet kein portables Metadatenprofil",
            ));
        }
        validate_records(&self.entries)?;
        let total_file_bytes = self.entries.iter().try_fold(0_u64, |total, record| {
            if record.entry.kind == SnapshotEntryKind::File {
                total
                    .checked_add(record.entry.size)
                    .ok_or_else(|| invalid_data("Snapshot-Dateisumme läuft über"))
            } else {
                Ok(total)
            }
        })?;
        if self.total_file_bytes != total_file_bytes {
            return Err(invalid_data("Snapshot-Dateisumme ist ungültig"));
        }
        if self.snapshot_digest != snapshot_digest(self.metadata_profile, &self.entries) {
            return Err(invalid_data("Snapshot-Digest ist ungültig"));
        }
        Ok(())
    }

    fn map(&self) -> BTreeMap<&BinaryRelativePath, &SnapshotEntry> {
        self.entries
            .iter()
            .map(|record| (&record.path, &record.entry))
            .collect()
    }
}

/// Erstellt einen vollständigen lesenden Snapshot. Jeder unsichere Eintrag bricht den
/// gesamten Scan ab; eine partielle Sicht wird niemals als Planbasis ausgegeben.
pub fn build_tree_snapshot(
    vfs: &dyn Vfs,
    root: &Path,
    metadata_profile: MetadataProfile,
    limits: SnapshotLimits,
) -> io::Result<TreeSnapshot> {
    if metadata_profile != MetadataProfile::Portable {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Native bidirektionale Profile benötigen den symmetrischen Fähigkeitsvertrag",
        ));
    }
    let identity_before = vfs.root_identity(root).map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::error_code(&error),
                format!("Root-Identität vor dem Scan konnte nicht gelesen werden: {error}"),
            ),
            &[&error],
            &[],
        )
    })?;
    let mut records = Vec::new();
    let mut total_file_bytes = 0_u64;
    scan_directory(vfs, root, &[], limits, &mut records, &mut total_file_bytes)?;
    let identity_after = vfs.root_identity(root).map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::error_code(&error),
                format!("Root-Identität nach dem Scan konnte nicht gelesen werden: {error}"),
            ),
            &[&error],
            &[],
        )
    })?;
    if identity_before != identity_after {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Root-Identität änderte sich während des bidirektionalen Scans",
        ));
    }
    records.sort_by(|left, right| left.path.cmp(&right.path));
    validate_records(&records)?;
    let digest = snapshot_digest(metadata_profile, &records);
    Ok(TreeSnapshot {
        root_identity: identity_before,
        metadata_profile,
        entries: records,
        total_file_bytes,
        snapshot_digest: digest,
    })
}

fn scan_directory(
    vfs: &dyn Vfs,
    physical: &Path,
    relative: &[Vec<u8>],
    limits: SnapshotLimits,
    records: &mut Vec<SnapshotRecord>,
    total_file_bytes: &mut u64,
) -> io::Result<()> {
    if relative.len() > limits.max_depth {
        return Err(invalid_data(
            "Bidirektionaler Scan überschreitet die maximale Pfadtiefe",
        ));
    }
    let listing = vfs.list_complete(physical).map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::error_code(&error),
                format!("Verzeichnisliste konnte nicht vollständig gelesen werden: {error}"),
            ),
            &[&error],
            &[],
        )
    })?;
    if !listing.complete {
        return Err(invalid_data(
            "Provider bestätigte die Verzeichnisliste nicht als vollständig",
        ));
    }
    let listing_token_before = listing.provider_token;
    let mut entries = listing.entries;
    entries.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
    if entries
        .windows(2)
        .any(|pair| pair[0].raw_name == pair[1].raw_name)
    {
        return Err(invalid_data("Verzeichnisliste enthält doppelte rohe Namen"));
    }
    let listing_before = canonical_listing_digest(&entries);
    for listed in entries {
        validate_listed_entry(&listed)?;
        if records.len() as u64 >= limits.max_entries {
            return Err(invalid_data(
                "Bidirektionaler Scan überschreitet die Pfadgrenze",
            ));
        }
        let mut child_relative = relative.to_vec();
        child_relative.push(listed.raw_name.clone());
        let relative_path = BinaryRelativePath(child_relative.clone());
        relative_path.validate()?;
        let child = physical.join(&listed.entry.name);
        let content_digest = if listed.entry.is_file {
            if listed.entry.size > limits.max_file_bytes {
                return Err(invalid_data(
                    "Datei überschreitet die bidirektionale Scangrenze",
                ));
            }
            *total_file_bytes = total_file_bytes
                .checked_add(listed.entry.size)
                .ok_or_else(|| invalid_data("Dateisumme läuft über"))?;
            if *total_file_bytes > limits.max_total_bytes {
                return Err(invalid_data(
                    "Bidirektionaler Scan überschreitet die Bytegrenze",
                ));
            }
            Some(hash_file_stable(vfs, &child, &listed.entry)?)
        } else {
            None
        };
        let kind = if listed.entry.is_file {
            SnapshotEntryKind::File
        } else {
            SnapshotEntryKind::Directory
        };
        let metadata_digest = portable_metadata_digest(listed.entry.mode);
        let mut snapshot = SnapshotEntry {
            kind,
            size: listed.entry.size,
            mtime: listed.entry.mtime,
            content_digest,
            metadata_digest,
            provider_id: listed.provider_token,
            entry_digest: [0; 32],
        };
        snapshot.entry_digest = snapshot.canonical_digest();
        records.push(SnapshotRecord {
            path: relative_path,
            entry: snapshot,
        });
        if listed.entry.is_dir {
            scan_directory(
                vfs,
                &child,
                &child_relative,
                limits,
                records,
                total_file_bytes,
            )?;
        }
    }
    // Ein zweites Listing nach allen Kindscans schließt die Lücke, in der parallel
    // ein neuer Name auftauchen oder verschwinden könnte. Die spätere Ausführung
    // wiederholt zusätzlich den ganzen Snapshot und bindet dessen Digest.
    let final_listing = vfs.list_complete(physical)?;
    if !final_listing.complete {
        return Err(invalid_data(
            "Provider bestätigte das abschließende Verzeichnislisting nicht",
        ));
    }
    let mut final_entries = final_listing.entries;
    final_entries.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
    if final_entries
        .windows(2)
        .any(|pair| pair[0].raw_name == pair[1].raw_name)
    {
        return Err(invalid_data(
            "Abschließendes Verzeichnislisting enthält doppelte rohe Namen",
        ));
    }
    for listed in &final_entries {
        validate_listed_entry(listed)?;
    }
    if final_listing.provider_token != listing_token_before
        || canonical_listing_digest(&final_entries) != listing_before
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!(
                "Verzeichnis änderte sich während des Snapshots: {}",
                physical.display()
            ),
        ));
    }
    Ok(())
}

fn canonical_listing_digest(entries: &[ProofListEntry]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(&[1]);
    for listed in entries {
        hash.update(&(listed.raw_name.len() as u64).to_be_bytes());
        hash.update(&listed.raw_name);
        hash.update(&[
            u8::from(listed.entry.is_file),
            u8::from(listed.entry.is_dir),
            u8::from(listed.entry.is_symlink),
        ]);
        hash.update(&listed.entry.size.to_be_bytes());
        hash.update(&listed.entry.mtime.unwrap_or(i64::MIN).to_be_bytes());
        hash.update(&listed.entry.mode.unwrap_or(u32::MAX).to_be_bytes());
        encode_optional_bytes(&mut hash, listed.provider_token.as_deref());
    }
    *hash.finalize().as_bytes()
}

fn validate_listed_entry(listed: &ProofListEntry) -> io::Result<()> {
    if !listed.safely_decoded || listed.entry.name.as_bytes() != listed.raw_name {
        return Err(invalid_data(
            "Dateiname ist nicht verlustfrei als UTF-8 gebunden",
        ));
    }
    if listed.entry.is_symlink || listed.entry.is_dir == listed.entry.is_file {
        return Err(invalid_data(
            "Symlink, Spezialdatei oder mehrdeutiger Dateityp im Baum",
        ));
    }
    Ok(())
}

fn hash_file_stable(
    vfs: &dyn Vfs,
    path: &Path,
    expected: &crate::vfs::Entry,
) -> io::Result<[u8; 32]> {
    let first = hash_file(vfs, path, expected.size)?;
    let second = hash_file(vfs, path, expected.size)?;
    let after = vfs.stat(path)?;
    if first != second
        || after.is_symlink
        || !after.is_file
        || after.size != expected.size
        || after.mtime != expected.mtime
        || after.mode != expected.mode
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!(
                "Datei änderte sich während des Snapshots: {}",
                path.display()
            ),
        ));
    }
    Ok(first)
}

fn hash_file(vfs: &dyn Vfs, path: &Path, expected_size: u64) -> io::Result<[u8; 32]> {
    let mut reader = vfs.open_read(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut remaining = expected_size;
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        let read = reader.read(&mut buffer[..chunk])?;
        if read == 0 {
            return Err(snapshot_size_drift(path));
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    // Das Listing begrenzt auch den tatsächlichen Stream. Ein falsches oder während
    // des Lesens wachsendes Providerobjekt darf das Scanbudget nicht umgehen.
    if reader.read(&mut [0_u8; 1])? != 0 {
        return Err(snapshot_size_drift(path));
    }
    Ok(*hasher.finalize().as_bytes())
}

fn snapshot_size_drift(path: &Path) -> io::Error {
    crate::coded_error(
        io::ErrorKind::Interrupted,
        crate::ErrorCode::PlanDrift,
        format!(
            "Dateistream entspricht nicht der gebundenen Größe: {}",
            path.display()
        ),
    )
}

fn portable_metadata_digest(mode: Option<u32>) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(&[1]);
    hash.update(&(mode.map(|value| value & 0o777).unwrap_or(u32::MAX)).to_be_bytes());
    *hash.finalize().as_bytes()
}

fn snapshot_digest(profile: MetadataProfile, records: &[SnapshotRecord]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(&[1, metadata_profile_byte(profile)]);
    for record in records {
        encode_path(&mut hash, &record.path);
        hash.update(&record.entry.entry_digest);
        hash.update(&record.entry.mtime.unwrap_or(i64::MIN).to_be_bytes());
        encode_optional_bytes(&mut hash, record.entry.provider_id.as_deref());
    }
    *hash.finalize().as_bytes()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineRecord {
    pub path: BinaryRelativePath,
    pub entry: SnapshotEntry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionDirection {
    LeftToRight,
    RightToLeft,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub path: BinaryRelativePath,
    pub previous_entry_digest: [u8; 32],
    pub baseline_generation: u64,
    pub direction: DeletionDirection,
    pub committed_unix_ms: u64,
}

/// Dauerhafte letzte gemeinsame Sicht. Records sind statt einer JSON-Map gespeichert,
/// damit binäre Pfade ohne verlustbehaftete Stringschlüssel serialisierbar bleiben.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalBaseline {
    pub version: u32,
    pub pair_id: PairId,
    pub generation: u64,
    pub left_binding: EndpointBinding,
    pub right_binding: EndpointBinding,
    pub metadata_profile: MetadataProfile,
    pub entries: Vec<BaselineRecord>,
    pub tombstones: Vec<Tombstone>,
    pub body_digest: [u8; 32],
}

impl BidirectionalBaseline {
    pub fn from_identical_snapshots(
        pair_id: PairId,
        left_binding: EndpointBinding,
        right_binding: EndpointBinding,
        left: &TreeSnapshot,
        right: &TreeSnapshot,
    ) -> io::Result<Self> {
        left.validate()?;
        right.validate()?;
        validate_binding_matches_snapshot(&left_binding, left)?;
        validate_binding_matches_snapshot(&right_binding, right)?;
        if left.metadata_profile != right.metadata_profile
            || !snapshots_semantically_equal(left, right)
        {
            return Err(invalid_input(
                "Erstbaseline benötigt zwei semantisch identische Bäume",
            ));
        }
        let mut baseline = Self {
            version: BASELINE_VERSION,
            pair_id,
            generation: 1,
            left_binding,
            right_binding,
            metadata_profile: left.metadata_profile,
            entries: left
                .entries
                .iter()
                .map(|record| BaselineRecord {
                    path: record.path.clone(),
                    entry: record.entry.clone(),
                })
                .collect(),
            tombstones: Vec::new(),
            body_digest: [0; 32],
        };
        baseline.body_digest = baseline.canonical_digest()?;
        baseline.validate()?;
        Ok(baseline)
    }

    pub fn validate(&self) -> io::Result<()> {
        if !matches!(self.version, LEGACY_BASELINE_VERSION | BASELINE_VERSION) {
            return Err(invalid_data("Unbekannte bidirektionale Baseline-Version"));
        }
        self.pair_id.validate()?;
        if self.generation == 0 || self.metadata_profile == MetadataProfile::NativeBestEffort {
            return Err(invalid_data(
                "Baseline-Generation oder Metadatenprofil ist ungültig",
            ));
        }
        self.left_binding.validate()?;
        self.right_binding.validate()?;
        validate_baseline_records(&self.entries)?;
        validate_tombstones(&self.tombstones, &self.entries)?;
        if self.body_digest != self.canonical_digest()? {
            return Err(invalid_data("Baseline-Digest ist ungültig"));
        }
        Ok(())
    }

    fn advance_from_resolved_snapshots(
        &self,
        left_binding: EndpointBinding,
        right_binding: EndpointBinding,
        left: &TreeSnapshot,
        right: &TreeSnapshot,
        plan: &BidirectionalPlan,
    ) -> io::Result<Self> {
        self.validate()?;
        left.validate()?;
        right.validate()?;
        validate_binding_matches_snapshot(&self.left_binding, left)?;
        validate_binding_matches_snapshot(&self.right_binding, right)?;
        validate_binding_matches_snapshot(&left_binding, left)?;
        validate_binding_matches_snapshot(&right_binding, right)?;
        if left.metadata_profile != self.metadata_profile
            || right.metadata_profile != self.metadata_profile
            || !snapshots_equal_except_skipped(left, right, plan)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Finale Bäume stimmen nicht mit dem Baseline-Vertrag überein",
            ));
        }
        if plan.pair_id != self.pair_id || plan.baseline_digest != Some(self.body_digest) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Finaler Plan gehört nicht zur fortzuschreibenden Baseline",
            ));
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid_data("Baseline-Generation läuft über"))?;
        let committed_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let skipped = skipped_paths(plan);
        let mut tombstones = self.tombstones.clone();
        for action in &plan.actions {
            let tombstone = match action {
                BidirectionalAction::Delete {
                    path,
                    direction,
                    expected_target,
                } => Some(Tombstone {
                    path: path.clone(),
                    previous_entry_digest: *expected_target,
                    baseline_generation: generation,
                    direction: match direction {
                        SyncDirection::LeftToRight => DeletionDirection::LeftToRight,
                        SyncDirection::RightToLeft => DeletionDirection::RightToLeft,
                    },
                    committed_unix_ms,
                }),
                BidirectionalAction::Tombstone {
                    path,
                    previous_entry_digest,
                } => Some(Tombstone {
                    path: path.clone(),
                    previous_entry_digest: *previous_entry_digest,
                    baseline_generation: generation,
                    direction: DeletionDirection::Both,
                    committed_unix_ms,
                }),
                _ => None,
            };
            if let Some(tombstone) = tombstone {
                tombstones.retain(|existing| existing.path != tombstone.path);
                tombstones.push(tombstone);
            }
        }
        let entries = resolved_baseline_entries(Some(self), left, right, &skipped)?;
        // Eine ausdrücklich wiederbelebte und nun wieder gleiche Datei darf nicht
        // gleichzeitig als gelöscht markiert bleiben. Übersprungene Pfade behalten
        // dagegen ihren alten Tombstone unverändert.
        tombstones.retain(|item| {
            path_is_skipped(&item.path, &skipped)
                || !entries.iter().any(|record| record.path == item.path)
        });
        tombstones.sort_by(|left, right| left.path.cmp(&right.path));
        let mut baseline = Self {
            version: BASELINE_VERSION,
            pair_id: self.pair_id,
            generation,
            left_binding,
            right_binding,
            metadata_profile: self.metadata_profile,
            entries,
            tombstones,
            body_digest: [0; 32],
        };
        baseline.body_digest = baseline.canonical_digest()?;
        baseline.validate()?;
        Ok(baseline)
    }

    fn canonical_digest(&self) -> io::Result<[u8; 32]> {
        #[derive(Serialize)]
        struct Body<'a> {
            version: u32,
            pair_id: PairId,
            generation: u64,
            left_binding: &'a EndpointBinding,
            right_binding: &'a EndpointBinding,
            metadata_profile: MetadataProfile,
            entries: &'a [BaselineRecord],
            tombstones: &'a [Tombstone],
        }
        let bytes = if self.version == LEGACY_BASELINE_VERSION {
            #[derive(Serialize)]
            struct LegacyBody<'a> {
                version: u32,
                pair_id: PairId,
                generation: u64,
                left_binding: LegacyEndpointBindingRef<'a>,
                right_binding: LegacyEndpointBindingRef<'a>,
                metadata_profile: MetadataProfile,
                entries: &'a [BaselineRecord],
                tombstones: &'a [Tombstone],
            }
            serde_json::to_vec(&LegacyBody {
                version: self.version,
                pair_id: self.pair_id,
                generation: self.generation,
                left_binding: LegacyEndpointBindingRef::new(&self.left_binding),
                right_binding: LegacyEndpointBindingRef::new(&self.right_binding),
                metadata_profile: self.metadata_profile,
                entries: &self.entries,
                tombstones: &self.tombstones,
            })
        } else {
            serde_json::to_vec(&Body {
                version: self.version,
                pair_id: self.pair_id,
                generation: self.generation,
                left_binding: &self.left_binding,
                right_binding: &self.right_binding,
                metadata_profile: self.metadata_profile,
                entries: &self.entries,
                tombstones: &self.tombstones,
            })
        }
        .map_err(io::Error::other)?;
        Ok(*blake3::hash(&bytes).as_bytes())
    }
}

/// Wendet ausschließlich explizite, snapshotgebundene Konfliktentscheidungen an.
/// Nicht genannte Konflikte bleiben offen; unbekannte oder doppelte Entscheidungen
/// werden abgelehnt, damit eine veraltete UI-Auswahl nie auf einen anderen Plan fällt.
pub fn resolve_conflicts(
    plan: &BidirectionalPlan,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
    resolutions: &[ConflictResolution],
) -> io::Result<BidirectionalPlan> {
    left.validate()?;
    right.validate()?;
    validate_plan_derived_fields(plan)?;
    if plan.left_snapshot_digest != left.snapshot_digest
        || plan.right_snapshot_digest != right.snapshot_digest
        || plan.plan_digest != plan_digest(plan)?
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Konfliktentscheidungen gehören nicht zu diesen Snapshots",
        ));
    }

    let mut decisions = BTreeMap::new();
    for resolution in resolutions {
        resolution.path.validate()?;
        if decisions
            .insert(&resolution.path, &resolution.decision)
            .is_some()
        {
            return Err(invalid_input("Konfliktpfad wurde doppelt entschieden"));
        }
    }
    let decision_paths: Vec<_> = decisions.keys().copied().collect();
    for (index, path) in decision_paths.iter().enumerate() {
        if decision_paths[index + 1..]
            .iter()
            .any(|other| binary_path_contains(path, other) || binary_path_contains(other, path))
        {
            return Err(invalid_input(
                "Eltern- und Kindkonflikt dürfen nicht gemeinsam entschieden werden",
            ));
        }
    }
    // Neue Konfliktnamen stehen noch in keinem Snapshot. Auch untereinander
    // dürfen sie auf case-insensitiven oder normalisierenden Zielen nicht kollidieren.
    // Die Kopien beider Seiten gehören zur selben Reservierung. Sie bleibt auch
    // erhalten, wenn weitere Entscheidungen auf dem teilweise aufgelösten Plan folgen.
    let mut conflict_names: BTreeSet<_> = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            BidirectionalAction::ConflictCopy { target_path, .. } => Some(target_path),
            _ => None,
        })
        .collect();
    for (original, decision) in &decisions {
        if let ConflictDecision::KeepBoth { conflict_path, .. } = decision {
            conflict_path.validate()?;
            if !conflict_names.insert(conflict_path)
                || decisions.keys().any(|path| {
                    *path == conflict_path
                        || binary_path_contains(path, conflict_path)
                        || binary_path_contains(conflict_path, path)
                })
                || conflict_names.iter().any(|other| {
                    *other != conflict_path
                        && (sibling_paths_may_collide(other, conflict_path)
                            || binary_path_contains(other, conflict_path)
                            || binary_path_contains(conflict_path, other))
                })
            {
                return Err(invalid_input(
                    "Konfliktname kollidiert mit einer anderen Pfadentscheidung",
                ));
            }
            if *original == conflict_path {
                return Err(invalid_input("Konfliktname entspricht dem Originalpfad"));
            }
        }
    }

    let left_map = left.map();
    let right_map = right.map();
    let mut used = BTreeSet::new();
    let mut actions = Vec::with_capacity(plan.actions.len());
    for action in &plan.actions {
        let action_path = bidirectional_action_path(action);
        if decisions
            .keys()
            .any(|root| binary_path_contains(root, action_path))
        {
            // Eine Elternentscheidung bilanziert den ganzen Unterbaum. Die alten
            // Kindaktionen dürfen deshalb weder doppelt laufen noch offen bleiben.
            continue;
        }
        let BidirectionalAction::Conflict {
            path,
            baseline,
            left: expected_left,
            right: expected_right,
            ..
        } = action
        else {
            actions.push(action.clone());
            continue;
        };
        let Some(decision) = decisions.get(path).copied() else {
            actions.push(action.clone());
            continue;
        };
        used.insert(path);
        let left_entry = left_map.get(path).copied();
        let right_entry = right_map.get(path).copied();
        if left_entry.map(|entry| entry.entry_digest) != *expected_left
            || right_entry.map(|entry| entry.entry_digest) != *expected_right
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Konflikteintrag änderte sich seit der Planung",
            ));
        }
        match decision {
            ConflictDecision::Skip => actions.push(BidirectionalAction::SkippedConflict {
                path: path.clone(),
                baseline: *baseline,
                left: *expected_left,
                right: *expected_right,
            }),
            ConflictDecision::KeepLeft => actions.extend(resolve_subtree_winner(
                path,
                EndpointSide::Left,
                left,
                right,
            )?),
            ConflictDecision::KeepRight => actions.extend(resolve_subtree_winner(
                path,
                EndpointSide::Right,
                left,
                right,
            )?),
            ConflictDecision::KeepBoth {
                primary,
                conflict_path,
            } => actions.extend(resolve_keep_both(
                path,
                *primary,
                conflict_path,
                left_entry,
                right_entry,
                left,
                right,
            )?),
        }
    }
    if used.len() != decisions.len() {
        return Err(invalid_input(
            "Entscheidung verweist nicht auf einen offenen Konflikt",
        ));
    }
    validate_internal_resolution_paths(&actions)?;
    let conflicts = actions
        .iter()
        .filter(|action| matches!(action, BidirectionalAction::Conflict { .. }))
        .count() as u64;
    let mut resolved = plan.clone();
    resolved.actions = actions;
    resolved.conflicts = conflicts;
    resolved.executable = conflicts == 0;
    resolved.plan_digest = [0; 32];
    resolved.plan_digest = plan_digest(&resolved)?;
    validate_plan_derived_fields(&resolved)?;
    Ok(resolved)
}

fn validate_plan_derived_fields(plan: &BidirectionalPlan) -> io::Result<()> {
    let conflicts = plan
        .actions
        .iter()
        .filter(|action| matches!(action, BidirectionalAction::Conflict { .. }))
        .count() as u64;
    if plan.conflicts != conflicts || plan.executable != (conflicts == 0) {
        return Err(invalid_data(
            "Konfliktzahl oder Ausführbarkeit passt nicht zu den Planaktionen",
        ));
    }
    Ok(())
}

fn validate_internal_resolution_paths(actions: &[BidirectionalAction]) -> io::Result<()> {
    let conflict_targets: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            BidirectionalAction::ConflictCopy { target_path, .. } => Some(target_path),
            _ => None,
        })
        .collect();
    let mut staging_paths = BTreeSet::new();
    for action in actions {
        let BidirectionalAction::TypeSwap { staging_path, .. } = action else {
            continue;
        };
        if !staging_paths.insert(staging_path)
            || conflict_targets.iter().any(|target| {
                *target == staging_path
                    || binary_path_contains(target, staging_path)
                    || binary_path_contains(staging_path, target)
            })
        {
            return Err(invalid_input(
                "Interner Typwechsel-Stagingpfad kollidiert mit einem Konfliktziel",
            ));
        }
    }
    Ok(())
}

fn resolve_subtree_winner(
    root: &BinaryRelativePath,
    winner: EndpointSide,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
) -> io::Result<Vec<BidirectionalAction>> {
    let left_map = left.map();
    let right_map = right.map();
    let (winner_map, target_map, direction) = match winner {
        EndpointSide::Left => (&left_map, &right_map, SyncDirection::LeftToRight),
        EndpointSide::Right => (&right_map, &left_map, SyncDirection::RightToLeft),
    };
    let paths: BTreeSet<_> = winner_map
        .keys()
        .chain(target_map.keys())
        .copied()
        .filter(|path| *path == root || binary_path_contains(root, path))
        .collect();
    if paths.is_empty() {
        return Err(invalid_data(
            "Konfliktunterbaum besitzt auf keiner Seite Einträge",
        ));
    }
    let mut actions = Vec::new();
    let mut absorbed_type_roots = Vec::new();
    for path in paths {
        if absorbed_type_roots
            .iter()
            .any(|root| binary_path_contains(root, path))
        {
            continue;
        }
        let source = winner_map.get(path).copied();
        let target = target_map.get(path).copied();
        if let Some((source, target)) = source
            .zip(target)
            .filter(|(source, target)| source.kind != target.kind)
        {
            let staging_path = type_swap_staging_path(path, direction, source, target)?;
            if left_map.contains_key(&staging_path) || right_map.contains_key(&staging_path) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Gebundener Typwechsel-Stagingname ist bereits belegt",
                ));
            }
            let mut subtree = Vec::new();
            if source.kind == SnapshotEntryKind::Directory {
                for (source_path, source_entry) in winner_map
                    .iter()
                    .filter(|(candidate, _)| binary_path_contains(path, candidate))
                {
                    subtree.push(ConflictCopyPath {
                        source_path: (*source_path).clone(),
                        target_path: rebase_binary_path(source_path, path, &staging_path)?,
                        expected_source: source_entry.entry_digest,
                    });
                }
            }
            actions.push(BidirectionalAction::TypeSwap {
                path: path.clone(),
                staging_path,
                direction,
                expected_source: source.entry_digest,
                expected_target: target.entry_digest,
                subtree,
            });
            absorbed_type_roots.push(path.clone());
            continue;
        }
        match (source, target) {
            (Some(source), Some(target)) if !source.semantically_equal(target) => {
                actions.push(copy(path, direction, source, Some(target)));
            }
            (Some(source), None) => actions.push(copy(path, direction, source, None)),
            (None, Some(target)) => actions.push(BidirectionalAction::Delete {
                path: path.clone(),
                direction,
                expected_target: target.entry_digest,
            }),
            _ => {}
        }
    }
    if actions.is_empty() {
        return Err(invalid_data(
            "Konfliktentscheidung erzeugte keine gebundene Unterbaumwirkung",
        ));
    }
    Ok(actions)
}

fn bidirectional_action_path(action: &BidirectionalAction) -> &BinaryRelativePath {
    match action {
        BidirectionalAction::ConflictCopy { source_path, .. } => source_path,
        BidirectionalAction::Copy { path, .. }
        | BidirectionalAction::TypeSwap { path, .. }
        | BidirectionalAction::Delete { path, .. }
        | BidirectionalAction::Converged { path, .. }
        | BidirectionalAction::Tombstone { path, .. }
        | BidirectionalAction::Conflict { path, .. }
        | BidirectionalAction::SkippedConflict { path, .. } => path,
    }
}

fn resolve_keep_both(
    original: &BinaryRelativePath,
    primary: EndpointSide,
    conflict_path: &BinaryRelativePath,
    left_entry: Option<&SnapshotEntry>,
    right_entry: Option<&SnapshotEntry>,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
) -> io::Result<Vec<BidirectionalAction>> {
    conflict_path.validate()?;
    let conflict_fs_path = conflict_path.to_path_buf()?;
    let original_fs_path = original.to_path_buf()?;
    if conflict_path == original
        || conflict_fs_path.parent() != original_fs_path.parent()
        || left.map().contains_key(conflict_path)
        || right.map().contains_key(conflict_path)
        || !snapshot_name_available(conflict_path, left, right)
    {
        return Err(invalid_input(
            "Konfliktname kollidiert oder liegt nicht neben dem Original",
        ));
    }
    let (Some(left_entry), Some(right_entry)) = (left_entry, right_entry) else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Beide behalten benötigt zwei vorhandene Versionen",
        ));
    };
    let losing_side = match primary {
        EndpointSide::Left => EndpointSide::Right,
        EndpointSide::Right => EndpointSide::Left,
    };
    let losing_entry = match losing_side {
        EndpointSide::Left => left_entry,
        EndpointSide::Right => right_entry,
    };
    let losing_map = match losing_side {
        EndpointSide::Left => left.map(),
        EndpointSide::Right => right.map(),
    };
    let left_map = left.map();
    let right_map = right.map();
    let mut subtree = Vec::new();
    if losing_entry.kind == SnapshotEntryKind::Directory {
        for (source_path, source_entry) in losing_map
            .iter()
            .filter(|(path, _)| binary_path_contains(original, path))
        {
            let target_path = rebase_binary_path(source_path, original, conflict_path)?;
            if left_map.contains_key(&target_path) || right_map.contains_key(&target_path) {
                return Err(invalid_input(
                    "Ein Pfad des Konflikt-Unterbaums ist bereits belegt",
                ));
            }
            subtree.push(ConflictCopyPath {
                source_path: (*source_path).clone(),
                target_path,
                expected_source: source_entry.entry_digest,
            });
        }
    }
    let mut actions = vec![
        BidirectionalAction::ConflictCopy {
            source_path: original.clone(),
            target_path: conflict_path.clone(),
            source_side: losing_side,
            target_side: EndpointSide::Left,
            expected_source: losing_entry.entry_digest,
            subtree: subtree.clone(),
        },
        BidirectionalAction::ConflictCopy {
            source_path: original.clone(),
            target_path: conflict_path.clone(),
            source_side: losing_side,
            target_side: EndpointSide::Right,
            expected_source: losing_entry.entry_digest,
            subtree,
        },
    ];
    actions.extend(resolve_subtree_winner(original, primary, left, right)?);
    Ok(actions)
}

fn type_swap_staging_path(
    original: &BinaryRelativePath,
    direction: SyncDirection,
    source: &SnapshotEntry,
    target: &SnapshotEntry,
) -> io::Result<BinaryRelativePath> {
    original.validate()?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"doppeldecker-type-swap-v1");
    hash.update(&[match direction {
        SyncDirection::LeftToRight => 1,
        SyncDirection::RightToLeft => 2,
    }]);
    for component in &original.0 {
        hash.update(&(component.len() as u64).to_be_bytes());
        hash.update(component);
    }
    hash.update(&source.entry_digest);
    hash.update(&target.entry_digest);
    let suffix = hash.finalize().to_hex()[..20].to_string();
    let mut components = original.0.clone();
    *components
        .last_mut()
        .ok_or_else(|| invalid_data("Typwechselpfad besitzt keinen Namen"))? =
        format!(".doppeldecker-swap-{suffix}").into_bytes();
    let staging = BinaryRelativePath(components);
    staging.validate()?;
    Ok(staging)
}

fn rebase_binary_path(
    path: &BinaryRelativePath,
    old_root: &BinaryRelativePath,
    new_root: &BinaryRelativePath,
) -> io::Result<BinaryRelativePath> {
    if path == old_root || !binary_path_contains(old_root, path) {
        return Err(invalid_input(
            "Unterbaumpfad liegt nicht unter seinem Alias-Root",
        ));
    }
    let mut components = new_root.0.clone();
    components.extend_from_slice(&path.0[old_root.0.len()..]);
    let rebased = BinaryRelativePath(components);
    rebased.validate()?;
    Ok(rebased)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncDirection {
    LeftToRight,
    RightToLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    InitialDifference,
    BothChanged,
    ChangedAgainstDeletion,
    TypeChanged,
    RecreatedAfterDeletion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSide {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ConflictDecision {
    KeepLeft,
    KeepRight,
    KeepBoth {
        primary: EndpointSide,
        conflict_path: BinaryRelativePath,
    },
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictResolution {
    pub path: BinaryRelativePath,
    pub decision: ConflictDecision,
}

/// Erzeugt den bei „Beide behalten“ sichtbaren Namen bereits für die Vorschau.
/// Der Name bindet Verliererseite, UTC-Zeit und Zufall; seine tatsächliche
/// Kollisionsfreiheit wird anschließend gegen beide vollständigen Snapshots geprüft.
pub fn generate_conflict_path(
    original: &BinaryRelativePath,
    losing_side: EndpointSide,
) -> io::Result<BinaryRelativePath> {
    let label = match losing_side {
        EndpointSide::Left => "Konflikt links",
        EndpointSide::Right => "Konflikt rechts",
    };
    generate_named_copy_path(original, label)
}

pub(crate) fn generate_file_copy_path(
    original: &BinaryRelativePath,
) -> io::Result<BinaryRelativePath> {
    generate_named_copy_path(original, "Kopie")
}

fn generate_named_copy_path(
    original: &BinaryRelativePath,
    label: &str,
) -> io::Result<BinaryRelativePath> {
    original.validate()?;
    let mut parts = original.0.clone();
    let raw_name = parts
        .pop()
        .ok_or_else(|| invalid_input("Originalpfad besitzt keinen Dateinamen"))?;
    let name = std::str::from_utf8(&raw_name)
        .map_err(|_| invalid_input("Konfliktname benötigt einen verlustfreien UTF-8-Namen"))?;
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem, format!(".{extension}"))
        }
        _ => (name, String::new()),
    };
    let seconds: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs()
        .try_into()
        .map_err(|_| invalid_data("UTC-Zeit liegt außerhalb des unterstützten Bereichs"))?;
    let timestamp = utc_filename_timestamp(seconds)?;
    let mut nonce = [0_u8; 4];
    getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
    let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let suffix = format!(" ({label} {timestamp} {suffix})");
    let generated = shortened_conflict_name(stem, &extension, &suffix);
    parts.push(generated.into_bytes());
    let path = BinaryRelativePath(parts);
    path.validate()?;
    Ok(path)
}

/// Kürzt nur an UTF-8-Grenzen. Die übliche Erweiterung bleibt vollständig;
/// extrem lange Erweiterungen teilen sich das begrenzte Namensbudget mit dem Stamm.
fn shortened_conflict_name(stem: &str, extension: &str, suffix: &str) -> String {
    fn prefix(value: &str, limit: usize) -> &str {
        let mut end = value.len().min(limit);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        &value[..end]
    }
    let extension = prefix(extension, 240 - suffix.len() - 4);
    let budget = 240 - suffix.len() - extension.len();
    format!("{}{}{}", prefix(stem, budget), suffix, extension)
}

#[cfg(unix)]
fn utc_filename_timestamp(seconds: i64) -> io::Result<String> {
    let raw: libc::time_t = seconds;
    let mut result = std::mem::MaybeUninit::<libc::tm>::uninit();
    // `gmtime_r` schreibt ausschließlich in unseren lokalen Puffer und verwendet
    // damit keinen prozessweiten, nebenläufig veränderlichen Zeitzustand.
    let value = unsafe { libc::gmtime_r(&raw, result.as_mut_ptr()) };
    if value.is_null() {
        return Err(invalid_data("UTC-Zeit konnte nicht formatiert werden"));
    }
    let value = unsafe { result.assume_init() };
    Ok(format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        value.tm_year + 1900,
        value.tm_mon + 1,
        value.tm_mday,
        value.tm_hour,
        value.tm_min,
        value.tm_sec
    ))
}

#[cfg(not(unix))]
fn utc_filename_timestamp(_seconds: i64) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "UTC-Konfliktnamen sind auf dieser Plattform nicht implementiert",
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BidirectionalAction {
    ConflictCopy {
        source_path: BinaryRelativePath,
        target_path: BinaryRelativePath,
        source_side: EndpointSide,
        target_side: EndpointSide,
        expected_source: [u8; 32],
        #[serde(default)]
        subtree: Vec<ConflictCopyPath>,
    },
    Copy {
        path: BinaryRelativePath,
        direction: SyncDirection,
        expected_source: [u8; 32],
        expected_target: Option<[u8; 32]>,
    },
    /// Ein Gewinner-Unterbaum ersetzt am Originalpfad einen Eintrag anderen Typs.
    /// Der vollständige Gewinner wird zuerst unter `staging_path` materialisiert;
    /// erst eine eigene Journalprojektion tauscht beide Namen atomar aus.
    TypeSwap {
        path: BinaryRelativePath,
        staging_path: BinaryRelativePath,
        direction: SyncDirection,
        expected_source: [u8; 32],
        expected_target: [u8; 32],
        #[serde(default)]
        subtree: Vec<ConflictCopyPath>,
    },
    Delete {
        path: BinaryRelativePath,
        direction: SyncDirection,
        expected_target: [u8; 32],
    },
    Converged {
        path: BinaryRelativePath,
        entry_digest: [u8; 32],
    },
    Tombstone {
        path: BinaryRelativePath,
        previous_entry_digest: [u8; 32],
    },
    Conflict {
        path: BinaryRelativePath,
        kind: ConflictKind,
        baseline: Option<[u8; 32]>,
        left: Option<[u8; 32]>,
        right: Option<[u8; 32]>,
    },
    SkippedConflict {
        path: BinaryRelativePath,
        baseline: Option<[u8; 32]>,
        left: Option<[u8; 32]>,
        right: Option<[u8; 32]>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictCopyPath {
    pub source_path: BinaryRelativePath,
    pub target_path: BinaryRelativePath,
    pub expected_source: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalPlan {
    pub pair_id: PairId,
    pub baseline_generation: Option<u64>,
    pub baseline_digest: Option<[u8; 32]>,
    pub left_root_identity: RootIdentity,
    pub right_root_identity: RootIdentity,
    pub left_snapshot_digest: [u8; 32],
    pub right_snapshot_digest: [u8; 32],
    pub actions: Vec<BidirectionalAction>,
    pub conflicts: u64,
    pub executable: bool,
    pub plan_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidirectionalRunState {
    Prepared,
    Running,
    RollingBack,
    /// Alle Datenwirkungen des Rollbacks sind anhand zweier stabiler Scans auf
    /// den ursprünglichen Stand zurückgeführt. Das Journal bleibt absichtlich
    /// bestehen, bis die Queue ihren terminalen Rollbackzustand persistiert hat.
    RollbackCommitted,
    BaselineCommitted,
}

fn reject_completed_run_recovery(journal: &BidirectionalRunJournal) -> io::Result<()> {
    if matches!(
        journal.state,
        BidirectionalRunState::BaselineCommitted | BidirectionalRunState::RollbackCommitted
    ) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Datenwirkung ist bereits abgeschlossen; Journal braucht ausschließlich Abschluss-Recovery",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidirectionalProjectionState {
    Prepared,
    Running,
    Committed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BidirectionalProjectionKind {
    ConflictCopy {
        source_path: BinaryRelativePath,
        target_path: BinaryRelativePath,
        source_side: EndpointSide,
        target_side: EndpointSide,
        subtree: Vec<ConflictCopyPath>,
    },
    Copy {
        direction: SyncDirection,
        replacement: bool,
        paths: Vec<BinaryRelativePath>,
    },
    TypeSwap {
        path: BinaryRelativePath,
        staging_path: BinaryRelativePath,
        direction: SyncDirection,
    },
    Delete {
        direction: SyncDirection,
        paths: Vec<BinaryRelativePath>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalProjectionJournal {
    pub ordinal: u64,
    pub projection: BidirectionalProjectionKind,
    pub state: BidirectionalProjectionState,
    pub quarantine: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalRollbackQuarantine {
    pub side: EndpointSide,
    pub run_root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidirectionalRollbackPhase {
    QuarantineCurrent,
    RestorePrevious,
    RestoreTypePreviousToStaging,
    ExchangeTypeNames,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalRollbackStep {
    pub ordinal: u64,
    #[serde(default)]
    pub unit_index: u64,
    pub side: EndpointSide,
    pub path: BinaryRelativePath,
    pub phase: BidirectionalRollbackPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidirectionalTypeSwapPhase {
    Exchange,
    QuarantinePrevious,
}

/// Dauerhafter Intent rund um den atomaren Namensaustausch. So kann Recovery auch
/// einen Prozessabbruch zwischen Kernel-Swap und Quarantäne-Rename sicher erkennen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalTypeSwapStep {
    pub ordinal: u64,
    pub side: EndpointSide,
    pub path: BinaryRelativePath,
    pub staging_path: BinaryRelativePath,
    pub quarantine_root: PathBuf,
    pub phase: BidirectionalTypeSwapPhase,
}

/// Geheimnisfreies Crash-Journal eines Mehrprojektionslaufs. Handles, Sitzungen und
/// Permits bleiben ausschließlich im RAM; dauerhaft sind nur Identitäten und Digests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalRunJournal {
    pub version: u32,
    pub run_id: [u8; 16],
    pub pair_id: PairId,
    pub plan_digest: [u8; 32],
    pub baseline_digest: Option<[u8; 32]>,
    pub left_binding: EndpointBinding,
    pub right_binding: EndpointBinding,
    pub left_snapshot_digest: [u8; 32],
    pub right_snapshot_digest: [u8; 32],
    /// Erwarteter Digest der bereits vollständig berechneten Folgebaseline. Er
    /// wird vor deren atomarem Schreiben journalisiert. So lässt sich ein Fehler
    /// beim letzten Zustandswechsel sicher von einer offenen Datenprojektion
    /// unterscheiden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_baseline_digest: Option<[u8; 32]>,
    pub plan: BidirectionalPlan,
    pub left_snapshot: TreeSnapshot,
    pub right_snapshot: TreeSnapshot,
    pub state: BidirectionalRunState,
    pub projections: Vec<BidirectionalProjectionJournal>,
    pub rollback_quarantines: Vec<BidirectionalRollbackQuarantine>,
    pub rollback_step: Option<BidirectionalRollbackStep>,
    pub type_swap_step: Option<BidirectionalTypeSwapStep>,
    pub body_digest: [u8; 32],
}

impl BidirectionalRunJournal {
    fn new(
        plan: &BidirectionalPlan,
        left_binding: EndpointBinding,
        right_binding: EndpointBinding,
        left_snapshot: &TreeSnapshot,
        right_snapshot: &TreeSnapshot,
    ) -> io::Result<Self> {
        let mut run_id = [0_u8; 16];
        getrandom::fill(&mut run_id).map_err(|error| io::Error::other(error.to_string()))?;
        if run_id == [0; 16] {
            return Err(invalid_data("Zufallsgenerator lieferte eine leere Lauf-ID"));
        }
        let projections = planned_projections(plan)?;
        let mut journal = Self {
            version: RUN_JOURNAL_VERSION,
            run_id,
            pair_id: plan.pair_id,
            plan_digest: plan.plan_digest,
            baseline_digest: plan.baseline_digest,
            left_binding,
            right_binding,
            left_snapshot_digest: plan.left_snapshot_digest,
            right_snapshot_digest: plan.right_snapshot_digest,
            committed_baseline_digest: None,
            plan: plan.clone(),
            left_snapshot: left_snapshot.clone(),
            right_snapshot: right_snapshot.clone(),
            state: BidirectionalRunState::Prepared,
            projections,
            rollback_quarantines: Vec::new(),
            rollback_step: None,
            type_swap_step: None,
            body_digest: [0; 32],
        };
        journal.body_digest = journal.canonical_digest()?;
        journal.validate()?;
        Ok(journal)
    }

    pub fn validate(&self) -> io::Result<()> {
        if !matches!(
            self.version,
            LEGACY_RUN_JOURNAL_VERSION | RUN_JOURNAL_VERSION
        ) || self.run_id == [0; 16]
            || self.plan_digest == [0; 32]
        {
            return Err(invalid_data("Bidirektionales Laufjournal ist ungültig"));
        }
        self.pair_id.validate()?;
        self.left_binding.validate()?;
        self.right_binding.validate()?;
        validate_persisted_remote_root_identity(&self.plan.left_root_identity)?;
        validate_persisted_remote_root_identity(&self.plan.right_root_identity)?;
        validate_persisted_remote_root_identity(&self.left_snapshot.root_identity)?;
        validate_persisted_remote_root_identity(&self.right_snapshot.root_identity)?;
        self.left_snapshot.validate()?;
        self.right_snapshot.validate()?;
        validate_plan_derived_fields(&self.plan)?;
        if self.plan.plan_digest != self.plan_digest
            || self.plan.plan_digest != plan_digest(&self.plan)?
            || self.plan.pair_id != self.pair_id
            || self.plan.baseline_digest != self.baseline_digest
            || self.plan.left_snapshot_digest != self.left_snapshot_digest
            || self.plan.right_snapshot_digest != self.right_snapshot_digest
            || self.left_snapshot.snapshot_digest != self.left_snapshot_digest
            || self.right_snapshot.snapshot_digest != self.right_snapshot_digest
        {
            return Err(invalid_data(
                "Plan oder Snapshots passen nicht zum Laufjournal",
            ));
        }
        if self.committed_baseline_digest.is_some()
            && (self
                .projections
                .iter()
                .any(|projection| projection.state != BidirectionalProjectionState::Committed)
                || !matches!(
                    self.state,
                    BidirectionalRunState::Running | BidirectionalRunState::BaselineCommitted
                )
                || self.rollback_step.is_some()
                || self.type_swap_step.is_some())
        {
            return Err(invalid_data(
                "Folgebaseline-Digest gehört nicht zu vollständig committed Projektionen",
            ));
        }
        if self.state == BidirectionalRunState::BaselineCommitted
            && (self
                .projections
                .iter()
                .any(|projection| projection.state != BidirectionalProjectionState::Committed)
                || self.rollback_step.is_some()
                || self.type_swap_step.is_some())
        {
            return Err(invalid_data(
                "Baseline-Abschluss besitzt noch eine offene Datenwirkung",
            ));
        }
        if self.state == BidirectionalRunState::RollbackCommitted
            && (self.committed_baseline_digest.is_some()
                || self
                    .projections
                    .iter()
                    .any(|projection| projection.state != BidirectionalProjectionState::Prepared)
                || self.rollback_step.is_some()
                || self.type_swap_step.is_some())
        {
            return Err(invalid_data(
                "Rollback-Abschluss besitzt noch eine offene Datenwirkung",
            ));
        }
        for (index, projection) in self.projections.iter().enumerate() {
            if projection.ordinal != index as u64 {
                return Err(invalid_data(
                    "Projektionsreihenfolge im Journal ist ungültig",
                ));
            }
            validate_projection_kind(&projection.projection)?;
        }
        if self.rollback_quarantines.len() > 2
            || self
                .rollback_quarantines
                .windows(2)
                .any(|pair| pair[0].side == pair[1].side)
            || self
                .rollback_quarantines
                .iter()
                .any(|item| !item.run_root.is_absolute())
        {
            return Err(invalid_data(
                "Rollback-Quarantänen im Journal sind ungültig",
            ));
        }
        if let Some(step) = &self.rollback_step {
            step.path.validate()?;
            if step.ordinal as usize >= self.projections.len()
                || self.state != BidirectionalRunState::RollingBack
            {
                return Err(invalid_data("Rollback-Schritt im Journal ist ungültig"));
            }
        }
        if let Some(step) = &self.type_swap_step {
            step.path.validate()?;
            step.staging_path.validate()?;
            if step.ordinal as usize >= self.projections.len()
                || !step.quarantine_root.is_absolute()
                || self.state != BidirectionalRunState::Running
                || self
                    .projections
                    .get(step.ordinal as usize)
                    .is_none_or(|projection| {
                        projection.state != BidirectionalProjectionState::Running
                            || !matches!(
                                &projection.projection,
                                BidirectionalProjectionKind::TypeSwap {
                                    path,
                                    staging_path,
                                    ..
                                } if path == &step.path && staging_path == &step.staging_path
                            )
                    })
            {
                return Err(invalid_data("Typwechsel-Schritt im Journal ist ungültig"));
            }
        }
        let expected = planned_projections(&self.plan)?;
        if expected.len() != self.projections.len()
            || expected
                .iter()
                .zip(&self.projections)
                .any(|(expected, actual)| expected.projection != actual.projection)
        {
            return Err(invalid_data(
                "Projektionsliste passt nicht zum gebundenen Plan",
            ));
        }
        let mut seen_running = false;
        let mut seen_prepared = false;
        for projection in &self.projections {
            match projection.state {
                BidirectionalProjectionState::Committed if seen_running || seen_prepared => {
                    return Err(invalid_data(
                        "Committed-Projektionen bilden keinen geordneten Präfix",
                    ));
                }
                BidirectionalProjectionState::Running if seen_running || seen_prepared => {
                    return Err(invalid_data(
                        "Journal besitzt mehrere laufende Projektionen",
                    ));
                }
                BidirectionalProjectionState::Running => seen_running = true,
                BidirectionalProjectionState::Prepared => seen_prepared = true,
                BidirectionalProjectionState::Committed => {}
            }
        }
        if self.body_digest != self.canonical_digest()? {
            return Err(invalid_data("Laufjournal-Digest ist ungültig"));
        }
        Ok(())
    }

    fn canonical_digest(&self) -> io::Result<[u8; 32]> {
        #[derive(Serialize)]
        struct Body<'a> {
            version: u32,
            run_id: [u8; 16],
            pair_id: PairId,
            plan_digest: [u8; 32],
            baseline_digest: Option<[u8; 32]>,
            left_binding: &'a EndpointBinding,
            right_binding: &'a EndpointBinding,
            left_snapshot_digest: [u8; 32],
            right_snapshot_digest: [u8; 32],
            #[serde(skip_serializing_if = "Option::is_none")]
            committed_baseline_digest: Option<[u8; 32]>,
            plan: &'a BidirectionalPlan,
            left_snapshot: &'a TreeSnapshot,
            right_snapshot: &'a TreeSnapshot,
            state: BidirectionalRunState,
            projections: &'a [BidirectionalProjectionJournal],
            rollback_quarantines: &'a [BidirectionalRollbackQuarantine],
            rollback_step: &'a Option<BidirectionalRollbackStep>,
            type_swap_step: &'a Option<BidirectionalTypeSwapStep>,
        }
        let payload = if self.version == LEGACY_RUN_JOURNAL_VERSION {
            #[derive(Serialize)]
            struct LegacyBody<'a> {
                version: u32,
                run_id: [u8; 16],
                pair_id: PairId,
                plan_digest: [u8; 32],
                baseline_digest: Option<[u8; 32]>,
                left_binding: LegacyEndpointBindingRef<'a>,
                right_binding: LegacyEndpointBindingRef<'a>,
                left_snapshot_digest: [u8; 32],
                right_snapshot_digest: [u8; 32],
                #[serde(skip_serializing_if = "Option::is_none")]
                committed_baseline_digest: Option<[u8; 32]>,
                plan: &'a BidirectionalPlan,
                left_snapshot: &'a TreeSnapshot,
                right_snapshot: &'a TreeSnapshot,
                state: BidirectionalRunState,
                projections: &'a [BidirectionalProjectionJournal],
                rollback_quarantines: &'a [BidirectionalRollbackQuarantine],
                rollback_step: &'a Option<BidirectionalRollbackStep>,
                type_swap_step: &'a Option<BidirectionalTypeSwapStep>,
            }
            serde_json::to_vec(&LegacyBody {
                version: self.version,
                run_id: self.run_id,
                pair_id: self.pair_id,
                plan_digest: self.plan_digest,
                baseline_digest: self.baseline_digest,
                left_binding: LegacyEndpointBindingRef::new(&self.left_binding),
                right_binding: LegacyEndpointBindingRef::new(&self.right_binding),
                left_snapshot_digest: self.left_snapshot_digest,
                right_snapshot_digest: self.right_snapshot_digest,
                committed_baseline_digest: self.committed_baseline_digest,
                plan: &self.plan,
                left_snapshot: &self.left_snapshot,
                right_snapshot: &self.right_snapshot,
                state: self.state,
                projections: &self.projections,
                rollback_quarantines: &self.rollback_quarantines,
                rollback_step: &self.rollback_step,
                type_swap_step: &self.type_swap_step,
            })
        } else {
            serde_json::to_vec(&Body {
                version: self.version,
                run_id: self.run_id,
                pair_id: self.pair_id,
                plan_digest: self.plan_digest,
                baseline_digest: self.baseline_digest,
                left_binding: &self.left_binding,
                right_binding: &self.right_binding,
                left_snapshot_digest: self.left_snapshot_digest,
                right_snapshot_digest: self.right_snapshot_digest,
                committed_baseline_digest: self.committed_baseline_digest,
                plan: &self.plan,
                left_snapshot: &self.left_snapshot,
                right_snapshot: &self.right_snapshot,
                state: self.state,
                projections: &self.projections,
                rollback_quarantines: &self.rollback_quarantines,
                rollback_step: &self.rollback_step,
                type_swap_step: &self.type_swap_step,
            })
        }
        .map_err(io::Error::other)?;
        Ok(*blake3::hash(&payload).as_bytes())
    }

    fn refresh_digest(&mut self) -> io::Result<()> {
        self.body_digest = self.canonical_digest()?;
        self.validate()
    }
}

fn validate_persisted_remote_root_identity(identity: &RootIdentity) -> io::Result<()> {
    match identity {
        RootIdentity::Local(_) => Ok(()),
        RootIdentity::Sftp(identity) => identity.validate(),
        RootIdentity::Ftps(identity) => identity.validate(),
        RootIdentity::WebDav(identity) => identity.validate(),
        RootIdentity::Smb(identity) => identity.validate(),
    }
}

fn contains_legacy_webdav_root_identity(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(values) => values.iter().any(contains_legacy_webdav_root_identity),
        serde_json::Value::Object(fields) => {
            let is_unbound_webdav_root = fields.get("provider").and_then(serde_json::Value::as_str)
                == Some("WebDav")
                && fields
                    .get("identity")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|identity| !identity.contains_key("root_etag"));
            is_unbound_webdav_root || fields.values().any(contains_legacy_webdav_root_identity)
        }
        _ => false,
    }
}

fn legacy_webdav_journal_may_have_effect(value: &serde_json::Value) -> bool {
    if value.get("state").and_then(serde_json::Value::as_str) != Some("prepared") {
        return true;
    }
    let Some(projections) = value
        .get("projections")
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    projections.iter().any(|projection| {
        projection.get("state").and_then(serde_json::Value::as_str) != Some("prepared")
    }) || value
        .get("rollback_quarantines")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|items| !items.is_empty())
        || !value
            .get("rollback_step")
            .is_none_or(serde_json::Value::is_null)
        || !value
            .get("type_swap_step")
            .is_none_or(serde_json::Value::is_null)
}

/// Reine Drei-Wege-Planung. Das Ergebnis enthält keine implizite Konfliktentscheidung
/// und kann deshalb gefahrlos in CLI, Tauri und Tests identisch verwendet werden.
pub fn plan_bidirectional(
    pair_id: PairId,
    left_binding: &EndpointBinding,
    right_binding: &EndpointBinding,
    baseline: Option<&BidirectionalBaseline>,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
) -> io::Result<BidirectionalPlan> {
    pair_id.validate()?;
    left_binding.validate()?;
    right_binding.validate()?;
    left.validate()?;
    right.validate()?;
    validate_binding_matches_snapshot(left_binding, left)?;
    validate_binding_matches_snapshot(right_binding, right)?;
    if left.metadata_profile != right.metadata_profile {
        return Err(invalid_input(
            "Snapshots verwenden verschiedene Metadatenprofile",
        ));
    }
    if let Some(baseline) = baseline {
        baseline.validate()?;
        if baseline.pair_id != pair_id
            || !bindings_match_same_snapshot(&baseline.left_binding, left_binding, left)?
            || !bindings_match_same_snapshot(&baseline.right_binding, right_binding, right)?
            || baseline.metadata_profile != left.metadata_profile
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Baseline gehört nicht zu diesem Paar oder diesen Endpunkten",
            ));
        }
    }

    let left_map = left.map();
    let right_map = right.map();
    let base_map: BTreeMap<_, _> = baseline
        .map(|value| {
            value
                .entries
                .iter()
                .map(|record| (&record.path, &record.entry))
                .collect()
        })
        .unwrap_or_default();
    let tombstones: BTreeSet<_> = baseline
        .map(|value| value.tombstones.iter().map(|item| &item.path).collect())
        .unwrap_or_default();
    let paths: BTreeSet<_> = left_map
        .keys()
        .chain(right_map.keys())
        .chain(base_map.keys())
        .chain(tombstones.iter())
        .copied()
        .collect();
    let mut actions = Vec::new();
    for path in paths {
        let left_entry = left_map.get(path).copied();
        let right_entry = right_map.get(path).copied();
        if baseline.is_none() {
            match (left_entry, right_entry) {
                (Some(left), Some(right)) if left.semantically_equal(right) => {
                    actions.push(BidirectionalAction::Converged {
                        path: path.clone(),
                        entry_digest: left.entry_digest,
                    })
                }
                _ => actions.push(conflict(
                    path,
                    ConflictKind::InitialDifference,
                    None,
                    left_entry,
                    right_entry,
                )),
            }
            continue;
        }
        if tombstones.contains(path) {
            if left_entry.is_some() || right_entry.is_some() {
                actions.push(conflict(
                    path,
                    ConflictKind::RecreatedAfterDeletion,
                    None,
                    left_entry,
                    right_entry,
                ));
            }
            continue;
        }
        let base = base_map.get(path).copied();
        actions.extend(plan_path(path, base, left_entry, right_entry));
    }
    let conflicts = actions
        .iter()
        .filter(|action| matches!(action, BidirectionalAction::Conflict { .. }))
        .count() as u64;
    let executable = conflicts == 0;
    let mut plan = BidirectionalPlan {
        pair_id,
        baseline_generation: baseline.map(|value| value.generation),
        baseline_digest: baseline.map(|value| value.body_digest),
        left_root_identity: left.root_identity.clone(),
        right_root_identity: right.root_identity.clone(),
        left_snapshot_digest: left.snapshot_digest,
        right_snapshot_digest: right.snapshot_digest,
        actions,
        conflicts,
        executable,
        plan_digest: [0; 32],
    };
    plan.plan_digest = plan_digest(&plan)?;
    validate_plan_derived_fields(&plan)?;
    Ok(plan)
}

/// Vorschau der gebundenen Projektionen mit konkreten Planblockern und den
/// erforderlichen Bestätigungen für Konflikte, Ersetzungen und Löschungen.
#[derive(Clone, Debug, Serialize)]
pub struct LocalBidirectionalPreview {
    pub conflict_details: Vec<BidirectionalConflictView>,
    pub plan: BidirectionalPlan,
    pub confirmation_phrase: String,
    pub expires_at_unix_ms: u128,
    pub backup_confirmation_required: bool,
    pub stage_blocker: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BidirectionalConflictView {
    pub path: BinaryRelativePath,
    pub relative_name: String,
    pub left: crate::conflict_view::ConflictEntryView,
    pub right: crate::conflict_view::ConflictEntryView,
    pub choices: Vec<BidirectionalConflictChoice>,
    pub skip_blocker: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BidirectionalConflictChoice {
    pub decision: ConflictDecision,
    pub explanation: String,
    pub generated_name: Option<String>,
    pub blocker: Option<String>,
}

fn snapshot_root_text(identity: &RootIdentity) -> String {
    match identity {
        RootIdentity::Local(value) => format!("Lokal · {}", value.display_path.display()),
        RootIdentity::Sftp(value) => format!(
            "SFTP · {}:{} · {}",
            value.server.host, value.server.port, value.virtual_root
        ),
        RootIdentity::Ftps(value) => format!(
            "{} · {}:{} · {}",
            if value.server.transport_security == crate::FtpTransportSecurity::Plaintext {
                "FTP"
            } else {
                "FTPS"
            },
            value.server.host,
            value.server.port,
            value.virtual_root
        ),
        RootIdentity::WebDav(value) => {
            format!("WebDAV · {} · {}", value.server.origin, value.virtual_root)
        }
        RootIdentity::Smb(value) => format!(
            "SMB · {}:{}/{} · {}",
            value.server.host, value.server.port, value.server.share, value.virtual_root
        ),
    }
}

fn snapshot_conflict_entry(
    snapshot: &TreeSnapshot,
    path: &BinaryRelativePath,
) -> crate::conflict_view::ConflictEntryView {
    use crate::conflict_view::ConflictEntryView;
    let endpoint = snapshot_root_text(&snapshot.root_identity);
    match snapshot.map().get(path) {
        None => ConflictEntryView::absent(endpoint),
        Some(entry) => ConflictEntryView {
            endpoint,
            exists: true,
            kind: Some(
                match entry.kind {
                    SnapshotEntryKind::File => "file",
                    SnapshotEntryKind::Directory => "directory",
                }
                .into(),
            ),
            size: (entry.kind == SnapshotEntryKind::File).then_some(entry.size),
            mtime: entry.mtime,
        },
    }
}

/// Die UI erhält nur Entscheidungen, die der Kern für genau diese Snapshots
/// auflösen kann. Auch generierte Namen werden vor der ersten Anzeige geprüft.
fn bidirectional_conflict_views(
    plan: &BidirectionalPlan,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
) -> io::Result<Vec<BidirectionalConflictView>> {
    let mut views = Vec::new();
    for action in &plan.actions {
        let BidirectionalAction::Conflict {
            path,
            left: left_entry,
            right: right_entry,
            ..
        } = action
        else {
            continue;
        };
        // Eine Elternentscheidung umfasst die Kinder; dafür erscheint genau ein Dialog.
        if views
            .iter()
            .any(|view: &BidirectionalConflictView| binary_path_contains(&view.path, path))
        {
            continue;
        }
        let mut choices = Vec::new();
        let skip_blocker = (plan.baseline_generation.is_none() && left_entry.is_some() != right_entry.is_some())
            .then(|| "Die Erstbaseline kann diesen einseitigen Unterschied nicht als offenen Konflikt speichern. Eine Seite muss übernommen werden.".into());
        for (decision, explanation) in [
            (ConflictDecision::KeepLeft, if left_entry.is_none() { "Links fehlt: Den rechten Eintrag einschließlich Unterbaum in Quarantäne entfernen." } else { "Linke Fassung am Originalpfad in beiden Bäumen übernehmen; abweichende rechte Fassung wird gesichert." }),
            (ConflictDecision::KeepRight, if right_entry.is_none() { "Rechts fehlt: Den linken Eintrag einschließlich Unterbaum in Quarantäne entfernen." } else { "Rechte Fassung am Originalpfad in beiden Bäumen übernehmen; abweichende linke Fassung wird gesichert." }),
            (ConflictDecision::Skip, "Beide Fassungen unverändert lassen. Eine bestehende Baseline bleibt für diesen Unterbaum erhalten."),
        ] {
            if matches!(decision, ConflictDecision::Skip) && skip_blocker.is_some() { continue; }
            if let Ok(resolved) = resolve_conflicts(plan, left, right, &[ConflictResolution { path: path.clone(), decision: decision.clone() }]) {
                let blocker = conflict_choice_stage_blocker(&resolved, left, right);
                choices.push(BidirectionalConflictChoice { decision, explanation: explanation.into(), generated_name: None, blocker });
            }
        }
        if left_entry.is_some() && right_entry.is_some() {
            for primary in [EndpointSide::Left, EndpointSide::Right] {
                let losing = if primary == EndpointSide::Left {
                    EndpointSide::Right
                } else {
                    EndpointSide::Left
                };
                for _ in 0..32 {
                    let candidate = generate_conflict_path(path, losing)?;
                    let planned_collision = plan.actions.iter().any(|action| {
                        matches!(action, BidirectionalAction::ConflictCopy { target_path, .. }
                            if sibling_paths_may_collide(target_path, &candidate))
                    });
                    if planned_collision || !snapshot_name_available(&candidate, left, right) {
                        continue;
                    }
                    let decision = ConflictDecision::KeepBoth {
                        primary,
                        conflict_path: candidate.clone(),
                    };
                    if let Ok(resolved) = resolve_conflicts(
                        plan,
                        left,
                        right,
                        &[ConflictResolution {
                            path: path.clone(),
                            decision: decision.clone(),
                        }],
                    ) {
                        choices.push(BidirectionalConflictChoice {
                            decision, explanation: "Beide vollständigen Fassungen in beiden Bäumen erhalten. Die zweite Fassung bekommt den angezeigten neuen Namen.".into(),
                            generated_name: Some(crate::conflict_view::display_relative_name(&candidate.0)),
                            blocker: conflict_choice_stage_blocker(&resolved, left, right),
                        });
                    }
                    break;
                }
            }
        }
        views.push(BidirectionalConflictView {
            path: path.clone(),
            relative_name: crate::conflict_view::display_relative_name(&path.0),
            left: snapshot_conflict_entry(left, path),
            right: snapshot_conflict_entry(right, path),
            choices,
            skip_blocker,
        });
    }
    Ok(views)
}

/// Verwendet dieselben Mengengrenzen wie die spätere lokale Vorbereitung.
/// Typwechsel zählen gemeinsam; bei Entfernungen bilden die unveränderten
/// Zielpfade die virtuelle Mirror-Quelle. So ist die Absage vor der Auswahl sichtbar.
fn conflict_choice_stage_blocker(
    plan: &BidirectionalPlan,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
) -> Option<String> {
    if matches!(
        (&left.root_identity, &right.root_identity),
        (RootIdentity::Local(_), RootIdentity::Local(_))
    ) {
        if let Some(reason) = local_type_swap_stage_blocker(plan) {
            return Some(reason);
        }
    }
    for direction in [SyncDirection::LeftToRight, SyncDirection::RightToLeft] {
        let deletions = deleted_binary_paths(plan, direction);
        if deletions.is_empty() {
            continue;
        }
        let target = if direction == SyncDirection::LeftToRight {
            right
        } else {
            left
        };
        let target_count = target.entries.len() as u64;
        let delete_count = target
            .entries
            .iter()
            .filter(|record| {
                deletions
                    .iter()
                    .any(|path| *path == record.path || binary_path_contains(path, &record.path))
            })
            .count() as u64;
        let summary = crate::SyncPlan {
            source_count: target_count.saturating_sub(delete_count),
            target_count,
            delete_count,
            ..Default::default()
        };
        let options = crate::SyncOptions {
            mode: crate::SyncMode::Mirror,
            compare: crate::Compare::ContentHash,
            ..Default::default()
        };
        if let Err(error) = crate::real_mode::validate_real_plan(&summary, &options) {
            return Some(error.to_string());
        }
    }
    None
}

fn sibling_paths_may_collide(first: &BinaryRelativePath, second: &BinaryRelativePath) -> bool {
    let Some((first_name, first_parent)) = first.0.split_last() else {
        return false;
    };
    let Some((second_name, second_parent)) = second.0.split_last() else {
        return false;
    };
    first_parent == second_parent
        && match (
            std::str::from_utf8(first_name),
            std::str::from_utf8(second_name),
        ) {
            (Ok(first), Ok(second)) => crate::remote_rename::names_may_collide(first, second),
            _ => first_name == second_name,
        }
}

fn snapshot_name_available(
    candidate: &BinaryRelativePath,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
) -> bool {
    let Some(name) = candidate
        .0
        .last()
        .and_then(|part| std::str::from_utf8(part).ok())
    else {
        return false;
    };
    left.entries.iter().chain(&right.entries).all(|record| {
        record.path.0.len() != candidate.0.len()
            || record.path.0[..record.path.0.len() - 1] != candidate.0[..candidate.0.len() - 1]
            || record
                .path
                .0
                .last()
                .and_then(|part| std::str::from_utf8(part).ok())
                .is_some_and(|existing| !crate::remote_rename::names_may_collide(existing, name))
    })
}

/// Reine providerneutrale Vorschau der Remote-Freigabestufe. Sie beweist bereits
/// dauerhafte Bindungen, vollständige Snapshots und Konfliktentscheidungen, enthält
/// aber absichtlich noch kein Permit und kann daher keine Nutzerdaten verändern.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderBidirectionalPlanPreview {
    pub plan: BidirectionalPlan,
    pub left_binding: EndpointBinding,
    pub right_binding: EndpointBinding,
    pub stage_blocker: String,
}

/// Backend-gehaltene Vorbereitung. VFS-Handles, Einweg-Permits und Baseline-Lock
/// sind absichtlich weder serialisierbar noch klonbar.
pub struct PreparedLocalBidirectionalRun {
    pair_id: PairId,
    left_root: PathBuf,
    right_root: PathBuf,
    left_vfs: Arc<dyn Vfs>,
    right_vfs: Arc<dyn Vfs>,
    left_endpoint: crate::ProviderEndpoint,
    right_endpoint: crate::ProviderEndpoint,
    left_binding: EndpointBinding,
    right_binding: EndpointBinding,
    left_snapshot: TreeSnapshot,
    right_snapshot: TreeSnapshot,
    baseline: Option<BidirectionalBaseline>,
    store: BaselineStore,
    prepared_runs: Vec<crate::PreparedProviderRealRun>,
    prepared_run_ordinals: Vec<usize>,
    deferred_run_ordinals: Vec<usize>,
    preview: LocalBidirectionalPreview,
    expires_at: std::time::SystemTime,
    journal_required: bool,
    discard_allowed: bool,
}

pub struct LocalBidirectionalRun {
    pair_id: PairId,
    left_root: PathBuf,
    right_root: PathBuf,
    left_vfs: Arc<dyn Vfs>,
    right_vfs: Arc<dyn Vfs>,
    left_endpoint: crate::ProviderEndpoint,
    right_endpoint: crate::ProviderEndpoint,
    left_binding: EndpointBinding,
    right_binding: EndpointBinding,
    baseline: Option<BidirectionalBaseline>,
    plan: BidirectionalPlan,
    store: BaselineStore,
    runs: Vec<crate::ProviderRealRun>,
    run_ordinals: Vec<usize>,
    deferred_run_ordinals: Vec<usize>,
    commit_required: bool,
    journal_required: bool,
    backup_confirmed: bool,
}

pub type PreparedProviderBidirectionalRun = PreparedLocalBidirectionalRun;
pub type ProviderBidirectionalRun = LocalBidirectionalRun;
pub type ProviderBidirectionalStats = LocalBidirectionalStats;

/// Liefert die dauerhafte, geheimnisfreie Paarbindung eines bereits validierten
/// Backend-Endpunkts. Die Tauri-Registry darf lokale Alias-Pfade oder bloße
/// Remote-Profilnamen nie selbst als Namespace-Identität erfinden.
pub fn bind_bidirectional_endpoint(
    endpoint: &crate::ProviderEndpoint,
) -> io::Result<EndpointBinding> {
    let (_, _, binding) = endpoint.bidirectional_snapshot_context()?;
    Ok(binding)
}

/// Liefert zusätzlich alle historischen Bindungen, die derselbe frisch
/// authentisierte Backend-Snapshot belegt. Die App-Registry darf damit einen alten
/// Schlüssel umschlüsseln, ohne eine neue Pair-ID oder eine Identität aus Pfadtext
/// abzuleiten.
pub fn bind_bidirectional_endpoint_compatibility(
    endpoint: &crate::ProviderEndpoint,
) -> io::Result<(EndpointBinding, Vec<EndpointBinding>)> {
    let (vfs, root, first_binding) = endpoint.bidirectional_snapshot_context()?;
    let identity = vfs.root_identity(&root)?;
    // `bidirectional_snapshot_context()` prüft den Root bereits einmal. Ein
    // lesender Reconnect zwischen beiden Prüfungen darf aber nicht die aktuelle
    // Bindung aus der ersten Identität mit Legacy-Kandidaten aus der zweiten
    // vermischen. Beide Rückgabeklassen werden daher aus demselben letzten
    // Root-Snapshot abgeleitet; vom ersten Beleg übernehmen wir nur das Konto.
    let current = match &first_binding {
        EndpointBinding::Local(_) => EndpointBinding::local(&identity)?,
        EndpointBinding::Sftp(value)
        | EndpointBinding::Ftps(value)
        | EndpointBinding::WebDav(value)
        | EndpointBinding::Smb(value) => EndpointBinding::remote(&identity, value.account_digest)?,
    };
    let legacy_expected = match &current {
        EndpointBinding::Ftps(value) => Some(EndpointBinding::Ftps(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: value.canonical_root.clone(),
            account_digest: value.account_digest,
            public_identity_digest: value.public_identity_digest,
        })),
        EndpointBinding::WebDav(value) => Some(EndpointBinding::WebDav(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: value.canonical_root.clone(),
            account_digest: value.account_digest,
            public_identity_digest: value.public_identity_digest,
        })),
        _ => None,
    };
    let mut compatible = vec![current.clone()];
    if let Some(expected) = legacy_expected {
        compatible.extend(legacy_endpoint_bindings(&identity, &expected)?);
    }
    let mut unique = Vec::with_capacity(compatible.len());
    for binding in compatible {
        if !unique.contains(&binding) {
            unique.push(binding);
        }
    }
    Ok((current, unique))
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalBidirectionalStats {
    pub runs: Vec<crate::SyncStats>,
    pub baseline_generation: u64,
    pub baseline_committed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalBidirectionalRollbackPreview {
    pub pair_id: PairId,
    pub committed_projections: u64,
    pub confirmation_phrase: String,
    pub expires_at_unix_ms: u128,
}

pub struct PreparedLocalBidirectionalRollback {
    left_root: PathBuf,
    right_root: PathBuf,
    left_vfs: Arc<dyn Vfs>,
    right_vfs: Arc<dyn Vfs>,
    left_snapshot: TreeSnapshot,
    right_snapshot: TreeSnapshot,
    store: BaselineStore,
    journal: BidirectionalRunJournal,
    preview: LocalBidirectionalRollbackPreview,
    expires_at: std::time::SystemTime,
}

pub struct LocalBidirectionalRollback {
    left_root: PathBuf,
    right_root: PathBuf,
    left_vfs: Arc<dyn Vfs>,
    right_vfs: Arc<dyn Vfs>,
    store: BaselineStore,
    journal: BidirectionalRunJournal,
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalBidirectionalRollbackStats {
    pub projections_rolled_back: u64,
    pub rollback_quarantines: Vec<String>,
}

pub type PreparedProviderBidirectionalRollback = PreparedLocalBidirectionalRollback;
pub type ProviderBidirectionalRollback = LocalBidirectionalRollback;
pub type ProviderBidirectionalRollbackStats = LocalBidirectionalRollbackStats;

/// Bereitet additive und ersetzende lokale Gewinnerpfade vor. Die Funktion schreibt
/// nur exklusive, sofort wieder entfernte Realmodus-Proben; Nutzerdaten bleiben bis
/// zur Bestätigung unverändert.
pub fn prepare_local_bidirectional(
    pair_id: PairId,
    pair_name: &str,
    left: &Path,
    right: &Path,
    baseline_path: &Path,
) -> io::Result<PreparedLocalBidirectionalRun> {
    prepare_local_bidirectional_with_decisions(pair_id, pair_name, left, right, baseline_path, &[])
}

/// Plant ein beliebiges bereits verbundenes Providerpaar vollständig lesend.
/// Schreibproben, Quarantäne und Recovery werden erst in den folgenden
/// providerweisen Freigaben zu einer nicht serialisierbaren Vorbereitung kombiniert.
pub fn plan_provider_bidirectional(
    pair_id: PairId,
    left: &crate::ProviderEndpoint,
    right: &crate::ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
) -> io::Result<ProviderBidirectionalPlanPreview> {
    pair_id.validate()?;
    let (left_vfs, left_root, left_binding) = left.bidirectional_snapshot_context()?;
    let (right_vfs, right_root, right_binding) = right.bidirectional_snapshot_context()?;
    let (left_snapshot, right_snapshot) = final_local_snapshots(
        left_vfs.as_ref(),
        &left_root,
        right_vfs.as_ref(),
        &right_root,
    )?;
    crate::remote_real_mode::reject_overlapping_roots(
        &left_snapshot.root_identity,
        &left_root,
        &right_snapshot.root_identity,
        &right_root,
    )?;
    let store = BaselineStore::open(baseline_path)?;
    if store.load_run_journal()?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Offenes bidirektionales Laufjournal verlangt zuerst Recovery",
        ));
    }
    let baseline = store.load()?;
    let unresolved = plan_bidirectional(
        pair_id,
        &left_binding,
        &right_binding,
        baseline.as_ref(),
        &left_snapshot,
        &right_snapshot,
    )?;
    let plan = resolve_conflicts(&unresolved, &left_snapshot, &right_snapshot, resolutions)?;
    Ok(ProviderBidirectionalPlanPreview {
        plan,
        left_binding,
        right_binding,
        stage_blocker:
            "Remote-Ausführung wartet auf den providerweisen Quarantäne- und Recovery-Vertrag"
                .into(),
    })
}

/// Kompatibler SFTP-Einstieg für die providerneutrale Remote-Matrix.
pub fn prepare_sftp_bidirectional_with_decisions(
    pair_id: PairId,
    pair_name: &str,
    left: crate::ProviderEndpoint,
    right: crate::ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
) -> io::Result<PreparedProviderBidirectionalRun> {
    validate_sftp_pair(&left, &right)?;
    prepare_provider_bidirectional_with_decisions(
        pair_id,
        pair_name,
        left,
        right,
        baseline_path,
        resolutions,
    )
}

/// Bereitet ausschließlich die Wirkungsrichtungen vor, deren konkretes Ziel den
/// benötigten Quarantäne- und Dauerhaftigkeitsvertrag beweisen kann.
pub fn prepare_provider_bidirectional_with_decisions(
    pair_id: PairId,
    pair_name: &str,
    left: crate::ProviderEndpoint,
    right: crate::ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
) -> io::Result<PreparedProviderBidirectionalRun> {
    prepare_provider_bidirectional_with_decisions_bound(
        pair_id,
        pair_name,
        left,
        right,
        baseline_path,
        resolutions,
        None,
    )
}

pub fn prepare_provider_bidirectional_with_decisions_bound(
    pair_id: PairId,
    pair_name: &str,
    left: crate::ProviderEndpoint,
    right: crate::ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
    expected_unresolved_digest: Option<[u8; 32]>,
) -> io::Result<PreparedProviderBidirectionalRun> {
    validate_pair_name(pair_name)?;
    validate_remote_pair(&left, &right)?;
    let (left_vfs, left_root, left_binding) = left.bidirectional_snapshot_context()?;
    let (right_vfs, right_root, right_binding) = right.bidirectional_snapshot_context()?;
    let (left_snapshot, right_snapshot) = final_local_snapshots(
        left_vfs.as_ref(),
        &left_root,
        right_vfs.as_ref(),
        &right_root,
    )?;
    crate::remote_real_mode::reject_overlapping_roots(
        &left_snapshot.root_identity,
        &left_root,
        &right_snapshot.root_identity,
        &right_root,
    )?;
    let store = BaselineStore::open(baseline_path)?;
    if store.load_run_journal()?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Offenes bidirektionales Laufjournal verlangt zuerst Recovery",
        ));
    }
    let baseline = store.load()?;
    let unresolved = plan_bidirectional(
        pair_id,
        &left_binding,
        &right_binding,
        baseline.as_ref(),
        &left_snapshot,
        &right_snapshot,
    )?;
    if expected_unresolved_digest.is_some_and(|digest| digest != unresolved.plan_digest) {
        return Err(crate::error::coded_error(std::io::ErrorKind::Interrupted,
            crate::error::ErrorCode::PlanDrift,
            "Beide Bäume oder die Baseline haben sich seit dem Konfliktdialog geändert; bitte neu planen"));
    }
    let plan = resolve_conflicts(&unresolved, &left_snapshot, &right_snapshot, resolutions)?;
    // Konflikte und feste Mengengrenzen benötigen keine Schreibprobe. Besonders
    // FTPS/WebDAV-Capability-Proben können kurzlebige Objekte anlegen; ein bereits
    // gesperrter Plan darf sie daher gar nicht erst starten.
    let stage_blocker = match provider_plan_stage_blocker(&plan) {
        Some(reason) => Some(reason),
        None => {
            let left_effects = provider_effect_capabilities(&left, &plan, EndpointSide::Left);
            let right_effects = provider_effect_capabilities(&right, &plan, EndpointSide::Right);
            provider_stage_blocker(
                &plan,
                &left_snapshot,
                &right_snapshot,
                &left_effects,
                &right_effects,
            )
        }
    };
    let journal_required = stage_blocker.is_none()
        && (baseline.is_none()
            || plan
                .actions
                .iter()
                .any(|action| !matches!(action, BidirectionalAction::SkippedConflict { .. })));
    if journal_required {
        store.save_run_journal(&BidirectionalRunJournal::new(
            &plan,
            left_binding.clone(),
            right_binding.clone(),
            &left_snapshot,
            &right_snapshot,
        )?)?;
    }
    let (prepared_runs, prepared_run_ordinals, deferred_run_ordinals) = if stage_blocker.is_none() {
        let context = LocalProjectionContext::new(
            left_vfs.clone(),
            right_vfs.clone(),
            left.clone(),
            right.clone(),
            left_root.clone(),
            right_root.clone(),
        )?;
        match prepare_projection_runs(&plan, &context, None, true) {
            Ok(value) => value,
            Err(error) => {
                if journal_required {
                    store.remove_run_journal()?;
                }
                return Err(error);
            }
        }
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    let now = std::time::SystemTime::now();
    let expires_at = now + crate::REAL_PERMIT_LIFETIME;
    let preview = LocalBidirectionalPreview {
        conflict_details: bidirectional_conflict_views(&plan, &left_snapshot, &right_snapshot)?,
        plan,
        confirmation_phrase: bidirectional_phrase(pair_name)?,
        expires_at_unix_ms: expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
        // Ein SFTP-Ziel wird für die rollbackfähige Additionsstufe absichtlich
        // durch den stärkeren Update-/Quarantänevertrag vorbereitet.
        backup_confirmation_required: true,
        stage_blocker,
    };
    Ok(PreparedLocalBidirectionalRun {
        pair_id,
        left_root,
        right_root,
        left_vfs,
        right_vfs,
        left_endpoint: left,
        right_endpoint: right,
        left_binding,
        right_binding,
        left_snapshot,
        right_snapshot,
        baseline,
        store,
        prepared_runs,
        prepared_run_ordinals,
        deferred_run_ordinals,
        preview,
        expires_at,
        journal_required,
        discard_allowed: true,
    })
}

/// Bereitet denselben lokalen Lauf mit expliziten Konfliktentscheidungen vor. Die
/// Entscheidungen werden erst nach zwei vollständigen Snapshots in den Plan gebunden.
pub fn prepare_local_bidirectional_with_decisions(
    pair_id: PairId,
    pair_name: &str,
    left: &Path,
    right: &Path,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
) -> io::Result<PreparedLocalBidirectionalRun> {
    prepare_local_bidirectional_with_decisions_bound(
        pair_id,
        pair_name,
        left,
        right,
        baseline_path,
        resolutions,
        None,
    )
}

pub fn prepare_local_bidirectional_with_decisions_bound(
    pair_id: PairId,
    pair_name: &str,
    left: &Path,
    right: &Path,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
    expected_unresolved_digest: Option<[u8; 32]>,
) -> io::Result<PreparedLocalBidirectionalRun> {
    validate_pair_name(pair_name)?;
    let (left_root, right_root) = crate::sync::validate_local_sync_roots(left, right)?;
    crate::real_mode::validate_real_target(&left_root)?;
    crate::real_mode::validate_real_target(&right_root)?;
    let left_parent = std::fs::canonicalize(
        left_root
            .parent()
            .ok_or_else(|| invalid_input("Linker Root besitzt keinen Elternordner"))?,
    )?;
    let right_parent = std::fs::canonicalize(
        right_root
            .parent()
            .ok_or_else(|| invalid_input("Rechter Root besitzt keinen Elternordner"))?,
    )?;
    let left_vfs = Arc::new(crate::LocalVfs::rooted(&left_parent)?);
    let right_vfs = Arc::new(crate::LocalVfs::rooted(&right_parent)?);
    let left_snapshot = build_tree_snapshot(
        left_vfs.as_ref(),
        &left_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    let right_snapshot = build_tree_snapshot(
        right_vfs.as_ref(),
        &right_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    let left_binding = EndpointBinding::local(&left_snapshot.root_identity)?;
    let right_binding = EndpointBinding::local(&right_snapshot.root_identity)?;
    let store = BaselineStore::open(baseline_path)?;
    if store.load_run_journal()?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Offenes bidirektionales Laufjournal verlangt zuerst Recovery",
        ));
    }
    let baseline = store.load()?;
    let unresolved_plan = plan_bidirectional(
        pair_id,
        &left_binding,
        &right_binding,
        baseline.as_ref(),
        &left_snapshot,
        &right_snapshot,
    )?;
    if expected_unresolved_digest.is_some_and(|digest| digest != unresolved_plan.plan_digest) {
        return Err(crate::error::coded_error(std::io::ErrorKind::Interrupted,
            crate::error::ErrorCode::PlanDrift,
            "Beide Bäume oder die Baseline haben sich seit dem Konfliktdialog geändert; bitte neu planen"));
    }
    let plan = resolve_conflicts(
        &unresolved_plan,
        &left_snapshot,
        &right_snapshot,
        resolutions,
    )?;

    let stage_blocker = local_stage_blocker(&plan);
    let backup_confirmation_required = plan.actions.iter().any(|action| {
        matches!(
            action,
            BidirectionalAction::Copy {
                expected_target: Some(_),
                ..
            } | BidirectionalAction::TypeSwap { .. }
                | BidirectionalAction::Delete { .. }
                | BidirectionalAction::Tombstone { .. }
        )
    });
    let journal_required = stage_blocker.is_none()
        && (baseline.is_none()
            || plan
                .actions
                .iter()
                .any(|action| !matches!(action, BidirectionalAction::SkippedConflict { .. })));
    if journal_required {
        let journal = BidirectionalRunJournal::new(
            &plan,
            left_binding.clone(),
            right_binding.clone(),
            &left_snapshot,
            &right_snapshot,
        )?;
        store.save_run_journal(&journal)?;
    }
    let (prepared_runs, prepared_run_ordinals, deferred_run_ordinals) = if stage_blocker.is_none() {
        prepare_projection_runs(
            &plan,
            &LocalProjectionContext::local(
                left_vfs.clone(),
                right_vfs.clone(),
                left_root.clone(),
                right_root.clone(),
            )?,
            None,
            true,
        )?
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    let now = std::time::SystemTime::now();
    let expires_at = now + crate::REAL_PERMIT_LIFETIME;
    let preview = LocalBidirectionalPreview {
        conflict_details: bidirectional_conflict_views(&plan, &left_snapshot, &right_snapshot)?,
        plan,
        confirmation_phrase: bidirectional_phrase(pair_name)?,
        expires_at_unix_ms: expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
        backup_confirmation_required,
        stage_blocker,
    };
    let left_endpoint = crate::ProviderEndpoint::local(&left_root)?;
    let right_endpoint = crate::ProviderEndpoint::local(&right_root)?;
    Ok(PreparedLocalBidirectionalRun {
        pair_id,
        left_root,
        right_root,
        left_vfs,
        right_vfs,
        left_endpoint,
        right_endpoint,
        left_binding,
        right_binding,
        left_snapshot,
        right_snapshot,
        baseline,
        store,
        prepared_runs,
        prepared_run_ordinals,
        deferred_run_ordinals,
        preview,
        expires_at,
        journal_required,
        discard_allowed: true,
    })
}

/// Schreibt den offenen Journalsuffix eines Resume-Laufs auf `Prepared` zurück,
/// sichert das Journal und bereitet genau diesen Suffix erneut vor.
///
/// Lokales und Provider-Resume teilen sich diese Mechanik vollständig; sie
/// unterscheiden sich nur im Projektionskontext und im Fehlertext. Damit die
/// bisherige Reihenfolge erhalten bleibt — erst den Wiederaufsetzpunkt
/// festschreiben, dann den Kontext für die Vorbereitung binden —, wird der
/// Kontext bewusst erst nach dem Journalschreiben über `build_context` erzeugt.
///
/// Alles vor dem Präfix gilt als endgültig committed. Alles danach wird wieder
/// als vorbereitet geführt; die Quarantänebindung dieser Projektionen wird
/// gelöst, weil sie neu vorbereitet werden. Nur ein Typwechsel behält sie, weil
/// dort das bereits beiseite gelegte Original der einzige Wiederherstellungsweg
/// ist. Am Ende muss jede offene Projektion entweder vorbereitet oder bewusst
/// zurückgestellt sein: Eine verlorene Projektion wäre stille Datenwirkung und
/// bricht deshalb fail-closed ab.
fn resume_projection_runs(
    store: &BaselineStore,
    journal: &mut BidirectionalRunJournal,
    prefix: usize,
    build_context: impl FnOnce() -> io::Result<LocalProjectionContext>,
    lost_projection_message: &'static str,
) -> io::Result<(Vec<crate::PreparedProviderRealRun>, Vec<usize>, Vec<usize>)> {
    for (index, projection) in journal.projections.iter_mut().enumerate() {
        if index < prefix {
            projection.state = BidirectionalProjectionState::Committed;
        } else {
            projection.state = BidirectionalProjectionState::Prepared;
            if !matches!(
                projection.projection,
                BidirectionalProjectionKind::TypeSwap { .. }
            ) {
                projection.quarantine = None;
            }
        }
    }
    journal.state = BidirectionalRunState::Prepared;
    journal.refresh_digest()?;
    store.save_run_journal(journal)?;

    let allowed: BTreeSet<_> = (prefix..journal.projections.len()).collect();
    let context = build_context()?;
    let (prepared_runs, prepared_run_ordinals, deferred_run_ordinals) =
        prepare_projection_runs(&journal.plan, &context, Some(&allowed), true)?;
    let mut covered = prepared_run_ordinals.clone();
    covered.extend(&deferred_run_ordinals);
    covered.sort_unstable();
    if covered != allowed.iter().copied().collect::<Vec<_>>() {
        return Err(invalid_data(lost_projection_message));
    }
    Ok((prepared_runs, prepared_run_ordinals, deferred_run_ordinals))
}

/// Bereitet ausschließlich die noch nicht committed Projektionen eines offenen
/// lokalen Journals neu vor. Der aktuelle Zustand muss exakt einem vollständigen
/// Projektionspräfix entsprechen; Mischzustände oder fremde Änderungen stoppen.
pub fn prepare_resume_local_bidirectional(
    pair_name: &str,
    left: &Path,
    right: &Path,
    baseline_path: &Path,
) -> io::Result<PreparedLocalBidirectionalRun> {
    validate_pair_name(pair_name)?;
    // Ein terminales Journal bindet bereits die vollständige Datenwirkung. Dieser
    // lokale Check kommt vor Root-Scans, damit Resume keinen alten Plan mehr öffnet.
    let store = BaselineStore::open(baseline_path)?;
    let mut journal = store
        .load_run_journal()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Kein Laufjournal vorhanden"))?;
    reject_completed_run_recovery(&journal)?;
    // Alte Journale können einen Plan enthalten, den die aktuelle Version wegen
    // eines einseitig übersprungenen Erstkonflikts nicht mehr ausführen darf. Die
    // Schranke wird unmittelbar aus dem bereits validierten Journal gelesen, bevor
    // eine Typwechselnormalisierung, Restore-Probe oder Projektion mutieren kann.
    let stage_blocker = local_stage_blocker(&journal.plan);
    let (left_root, right_root) = crate::sync::validate_local_sync_roots(left, right)?;
    crate::real_mode::validate_real_target(&left_root)?;
    crate::real_mode::validate_real_target(&right_root)?;
    let left_parent = std::fs::canonicalize(
        left_root
            .parent()
            .ok_or_else(|| invalid_input("Linker Root besitzt keinen Elternordner"))?,
    )?;
    let right_parent = std::fs::canonicalize(
        right_root
            .parent()
            .ok_or_else(|| invalid_input("Rechter Root besitzt keinen Elternordner"))?,
    )?;
    let left_vfs = Arc::new(crate::LocalVfs::rooted(&left_parent)?);
    let right_vfs = Arc::new(crate::LocalVfs::rooted(&right_parent)?);
    let (mut left_snapshot, mut right_snapshot) = final_local_snapshots(
        left_vfs.as_ref(),
        &left_root,
        right_vfs.as_ref(),
        &right_root,
    )?;
    let baseline = store.load()?;
    if baseline.as_ref().map(|value| value.body_digest) != journal.baseline_digest
        || EndpointBinding::local(&left_snapshot.root_identity)? != journal.left_binding
        || EndpointBinding::local(&right_snapshot.root_identity)? != journal.right_binding
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Baseline oder Endpunktbindung änderte sich seit dem Lauf",
        ));
    }
    if stage_blocker.is_none()
        && normalize_type_swap_step(
            &store,
            &mut journal,
            &left_snapshot,
            &right_snapshot,
            LocalEndpointPair {
                left_vfs: left_vfs.as_ref(),
                left_root: &left_root,
                right_vfs: right_vfs.as_ref(),
                right_root: &right_root,
            },
        )?
    {
        (left_snapshot, right_snapshot) = final_local_snapshots(
            left_vfs.as_ref(),
            &left_root,
            right_vfs.as_ref(),
            &right_root,
        )?;
    }
    let prefix = recovery_prefix(&journal, &left_snapshot, &right_snapshot)?;
    // Ein gesperrter Plan darf beim Resume nichts anfassen: weder das Journal
    // umschreiben noch über die Vorbereitung eine Schreibprobe auslösen.
    let (prepared_runs, prepared_run_ordinals, deferred_run_ordinals) = if stage_blocker.is_none() {
        resume_projection_runs(
            &store,
            &mut journal,
            prefix,
            || {
                LocalProjectionContext::local(
                    left_vfs.clone(),
                    right_vfs.clone(),
                    left_root.clone(),
                    right_root.clone(),
                )
            },
            "Resume-Vorbereitung verlor eine offene Journalprojektion",
        )?
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    let backup_confirmation_required = journal.plan.actions.iter().any(|action| {
        matches!(
            action,
            BidirectionalAction::ConflictCopy { .. }
                | BidirectionalAction::Copy {
                    expected_target: Some(_),
                    ..
                }
                | BidirectionalAction::TypeSwap { .. }
                | BidirectionalAction::Delete { .. }
                | BidirectionalAction::Tombstone { .. }
        )
    });
    let now = std::time::SystemTime::now();
    let expires_at = now + crate::REAL_PERMIT_LIFETIME;
    let preview = LocalBidirectionalPreview {
        conflict_details: bidirectional_conflict_views(
            &journal.plan,
            &left_snapshot,
            &right_snapshot,
        )?,
        plan: journal.plan.clone(),
        confirmation_phrase: bidirectional_phrase(pair_name)?,
        expires_at_unix_ms: expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
        backup_confirmation_required,
        stage_blocker,
    };
    let left_endpoint = crate::ProviderEndpoint::local(&left_root)?;
    let right_endpoint = crate::ProviderEndpoint::local(&right_root)?;
    Ok(PreparedLocalBidirectionalRun {
        pair_id: journal.pair_id,
        left_root,
        right_root,
        left_vfs,
        right_vfs,
        left_endpoint,
        right_endpoint,
        left_binding: journal.left_binding,
        right_binding: journal.right_binding,
        left_snapshot,
        right_snapshot,
        baseline,
        store,
        prepared_runs,
        prepared_run_ordinals,
        deferred_run_ordinals,
        preview,
        expires_at,
        journal_required: true,
        discard_allowed: false,
    })
}

/// Bereitet den rückwärts laufenden Rollback bestätigter Teilprojektionen vor.
/// Dateien, Verzeichnisunterbäume und Typwechsel bleiben an ihre Journalzustände gebunden.
pub fn prepare_local_bidirectional_rollback(
    pair_name: &str,
    left: &Path,
    right: &Path,
    baseline_path: &Path,
) -> io::Result<PreparedLocalBidirectionalRollback> {
    validate_pair_name(pair_name)?;
    // RollbackCommitted darf auch bei inzwischen gedrifteten Roots niemals als
    // zweiter Rollback interpretiert werden. Offen ist ausschließlich Cleanup.
    let store = BaselineStore::open(baseline_path)?;
    let mut journal = store
        .load_run_journal()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Kein Laufjournal vorhanden"))?;
    reject_completed_run_recovery(&journal)?;
    let (left_root, right_root) = crate::sync::validate_local_sync_roots(left, right)?;
    let left_parent = std::fs::canonicalize(
        left_root
            .parent()
            .ok_or_else(|| invalid_input("Linker Root besitzt keinen Elternordner"))?,
    )?;
    let right_parent = std::fs::canonicalize(
        right_root
            .parent()
            .ok_or_else(|| invalid_input("Rechter Root besitzt keinen Elternordner"))?,
    )?;
    let left_vfs = Arc::new(crate::LocalVfs::rooted(&left_parent)?);
    let right_vfs = Arc::new(crate::LocalVfs::rooted(&right_parent)?);
    let (left_snapshot, right_snapshot) = final_local_snapshots(
        left_vfs.as_ref(),
        &left_root,
        right_vfs.as_ref(),
        &right_root,
    )?;
    if store.load()?.map(|value| value.body_digest) != journal.baseline_digest
        || EndpointBinding::local(&left_snapshot.root_identity)? != journal.left_binding
        || EndpointBinding::local(&right_snapshot.root_identity)? != journal.right_binding
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Rollback-Baseline oder Endpunktbindung ist nicht mehr gültig",
        ));
    }
    let committed = journal
        .projections
        .iter()
        .take_while(|projection| projection.state == BidirectionalProjectionState::Committed)
        .count();
    let running = journal.state != BidirectionalRunState::RollingBack
        && journal
            .projections
            .get(committed)
            .is_some_and(|projection| projection.state == BidirectionalProjectionState::Running);
    let maximum = committed + usize::from(running);
    let prefix = if journal.rollback_step.is_some() {
        normalize_rollback_step(
            &mut journal,
            &left_snapshot,
            &right_snapshot,
            maximum,
            LocalEndpointPair {
                left_vfs: left_vfs.as_ref(),
                left_root: &left_root,
                right_vfs: right_vfs.as_ref(),
                right_root: &right_root,
            },
        )?
    } else {
        observed_projection_prefix(&journal, &left_snapshot, &right_snapshot, 0, maximum)?
    };
    if prefix == 0 && journal.state != BidirectionalRunState::RollingBack {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Rollback besitzt keinen committed Teilzustand; unverändert verwerfen verwenden",
        ));
    }
    for index in 0..prefix {
        projection_rollback_units(&journal, index)?;
    }
    for (index, projection) in journal.projections.iter_mut().enumerate() {
        projection.state = if index < prefix {
            BidirectionalProjectionState::Committed
        } else {
            BidirectionalProjectionState::Prepared
        };
        if index >= prefix {
            projection.quarantine = None;
        }
    }
    journal.state = BidirectionalRunState::RollingBack;
    journal.refresh_digest()?;
    store.save_run_journal(&journal)?;
    let now = std::time::SystemTime::now();
    let expires_at = now + crate::REAL_PERMIT_LIFETIME;
    let preview = LocalBidirectionalRollbackPreview {
        pair_id: journal.pair_id,
        committed_projections: prefix as u64,
        confirmation_phrase: rollback_phrase(pair_name)?,
        expires_at_unix_ms: expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
    };
    Ok(PreparedLocalBidirectionalRollback {
        left_root,
        right_root,
        left_vfs,
        right_vfs,
        left_snapshot,
        right_snapshot,
        store,
        journal,
        preview,
        expires_at,
    })
}

/// Kompatibler SFTP-Einstieg für providerneutrales Resume.
pub fn prepare_resume_sftp_bidirectional(
    pair_name: &str,
    left: crate::ProviderEndpoint,
    right: crate::ProviderEndpoint,
    baseline_path: &Path,
) -> io::Result<PreparedProviderBidirectionalRun> {
    validate_sftp_pair(&left, &right)?;
    prepare_resume_provider_bidirectional(pair_name, left, right, baseline_path)
}

/// Verbindet einen offenen Providerlauf mit frischen Sitzungen und bereitet nur den
/// noch fehlenden, capability-geprüften Projektionssuffix neu vor.
pub fn prepare_resume_provider_bidirectional(
    pair_name: &str,
    left: crate::ProviderEndpoint,
    right: crate::ProviderEndpoint,
    baseline_path: &Path,
) -> io::Result<PreparedProviderBidirectionalRun> {
    validate_pair_name(pair_name)?;
    validate_remote_pair(&left, &right)?;
    // Ein bereits abgeschlossener Lauf braucht nur noch lokalen Journalabbau.
    // Deshalb lesen wir diesen Zustand vor jedem neuen Providerkontakt: Resume darf
    // weder eine neue Sitzung öffnen noch die ursprünglichen Datenwirkungen starten.
    let store = BaselineStore::open(baseline_path)?;
    let mut journal = store
        .load_run_journal()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Kein Laufjournal vorhanden"))?;
    reject_completed_run_recovery(&journal)?;
    let (left_vfs, left_root, left_binding) = left.bidirectional_snapshot_context()?;
    let (right_vfs, right_root, right_binding) = right.bidirectional_snapshot_context()?;
    let (mut left_snapshot, mut right_snapshot) = final_local_snapshots(
        left_vfs.as_ref(),
        &left_root,
        right_vfs.as_ref(),
        &right_root,
    )?;
    if provider_plan_stage_blocker(&journal.plan).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Laufjournal gehört nicht zu einer offenen Remote-Providerfreigabe",
        ));
    }
    let left_effects = provider_effect_capabilities(&left, &journal.plan, EndpointSide::Left);
    let right_effects = provider_effect_capabilities(&right, &journal.plan, EndpointSide::Right);
    if provider_stage_blocker(
        &journal.plan,
        &journal.left_snapshot,
        &journal.right_snapshot,
        &left_effects,
        &right_effects,
    )
    .is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Laufjournal gehört nicht zu einer offenen Remote-Providerfreigabe",
        ));
    }
    let baseline = store.load()?;
    if baseline.as_ref().map(|value| value.body_digest) != journal.baseline_digest
        || !bindings_match_same_snapshot(&journal.left_binding, &left_binding, &left_snapshot)?
        || !bindings_match_same_snapshot(&journal.right_binding, &right_binding, &right_snapshot)?
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Baseline oder Remote-Endpunktbindung änderte sich seit dem Lauf",
        ));
    }
    if normalize_type_swap_step(
        &store,
        &mut journal,
        &left_snapshot,
        &right_snapshot,
        LocalEndpointPair {
            left_vfs: left_vfs.as_ref(),
            left_root: &left_root,
            right_vfs: right_vfs.as_ref(),
            right_root: &right_root,
        },
    )? {
        (left_snapshot, right_snapshot) = final_local_snapshots(
            left_vfs.as_ref(),
            &left_root,
            right_vfs.as_ref(),
            &right_root,
        )?;
    }
    let prefix = recovery_prefix(&journal, &left_snapshot, &right_snapshot)?;
    let (prepared_runs, prepared_run_ordinals, deferred_run_ordinals) = resume_projection_runs(
        &store,
        &mut journal,
        prefix,
        || {
            LocalProjectionContext::new(
                left_vfs.clone(),
                right_vfs.clone(),
                left.clone(),
                right.clone(),
                left_root.clone(),
                right_root.clone(),
            )
        },
        "Remote-Resume verlor eine offene Journalprojektion",
    )?;
    let now = std::time::SystemTime::now();
    let expires_at = now + crate::REAL_PERMIT_LIFETIME;
    let preview = LocalBidirectionalPreview {
        conflict_details: bidirectional_conflict_views(
            &journal.plan,
            &left_snapshot,
            &right_snapshot,
        )?,
        plan: journal.plan.clone(),
        confirmation_phrase: bidirectional_phrase(pair_name)?,
        expires_at_unix_ms: expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
        backup_confirmation_required: true,
        stage_blocker: None,
    };
    Ok(PreparedLocalBidirectionalRun {
        pair_id: journal.pair_id,
        left_root,
        right_root,
        left_vfs,
        right_vfs,
        left_endpoint: left,
        right_endpoint: right,
        // Ein authentisierter Legacy-Vergleich autorisiert diesen Recoverylauf;
        // der nächste Baseline-Commit schreibt danach ausschließlich Schema 2.
        left_binding,
        right_binding,
        left_snapshot,
        right_snapshot,
        baseline,
        store,
        prepared_runs,
        prepared_run_ordinals,
        deferred_run_ordinals,
        preview,
        expires_at,
        journal_required: true,
        discard_allowed: false,
    })
}

/// Kompatibler SFTP-Einstieg für providerneutralen Rollback.
pub fn prepare_sftp_bidirectional_rollback(
    pair_name: &str,
    left: crate::ProviderEndpoint,
    right: crate::ProviderEndpoint,
    baseline_path: &Path,
) -> io::Result<PreparedProviderBidirectionalRollback> {
    validate_sftp_pair(&left, &right)?;
    prepare_provider_bidirectional_rollback(pair_name, left, right, baseline_path)
}

/// Bereitet den vollständigen Rückweg eines capability-geprüften Remote-Teilcommits
/// vor. Aktuelle Versionen wandern zuerst in eine manifestierte Quarantäne; ein
/// unbekannter Name wird niemals ersetzt oder gelöscht.
pub fn prepare_provider_bidirectional_rollback(
    pair_name: &str,
    left: crate::ProviderEndpoint,
    right: crate::ProviderEndpoint,
    baseline_path: &Path,
) -> io::Result<PreparedProviderBidirectionalRollback> {
    validate_pair_name(pair_name)?;
    validate_remote_pair(&left, &right)?;
    // RollbackCommitted ist bereits vollständig zurückgerollt. Auch hier muss die
    // lokale Abschluss-Recovery vor jedem neuen Providerkontakt gewinnen.
    let store = BaselineStore::open(baseline_path)?;
    let mut journal = store
        .load_run_journal()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Kein Laufjournal vorhanden"))?;
    reject_completed_run_recovery(&journal)?;
    let (left_vfs, left_root, left_binding) = left.bidirectional_snapshot_context()?;
    let (right_vfs, right_root, right_binding) = right.bidirectional_snapshot_context()?;
    let (left_snapshot, right_snapshot) = final_local_snapshots(
        left_vfs.as_ref(),
        &left_root,
        right_vfs.as_ref(),
        &right_root,
    )?;
    if provider_plan_stage_blocker(&journal.plan).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Remote-Rollback-Plan besitzt einen unbeantworteten Planblocker",
        ));
    }
    let left_effects = provider_effect_capabilities(&left, &journal.plan, EndpointSide::Left);
    let right_effects = provider_effect_capabilities(&right, &journal.plan, EndpointSide::Right);
    if provider_stage_blocker(
        &journal.plan,
        &journal.left_snapshot,
        &journal.right_snapshot,
        &left_effects,
        &right_effects,
    )
    .is_some()
        || store.load()?.map(|value| value.body_digest) != journal.baseline_digest
        || !bindings_match_same_snapshot(&journal.left_binding, &left_binding, &left_snapshot)?
        || !bindings_match_same_snapshot(&journal.right_binding, &right_binding, &right_snapshot)?
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Remote-Rollback-Baseline oder Endpunktbindung ist nicht mehr gültig",
        ));
    }
    let committed = journal
        .projections
        .iter()
        .take_while(|projection| projection.state == BidirectionalProjectionState::Committed)
        .count();
    let running = journal.state != BidirectionalRunState::RollingBack
        && journal
            .projections
            .get(committed)
            .is_some_and(|projection| projection.state == BidirectionalProjectionState::Running);
    let maximum = committed + usize::from(running);
    let prefix = if journal.rollback_step.is_some() {
        normalize_rollback_step(
            &mut journal,
            &left_snapshot,
            &right_snapshot,
            maximum,
            LocalEndpointPair {
                left_vfs: left_vfs.as_ref(),
                left_root: &left_root,
                right_vfs: right_vfs.as_ref(),
                right_root: &right_root,
            },
        )?
    } else {
        observed_projection_prefix(&journal, &left_snapshot, &right_snapshot, 0, maximum)?
    };
    if prefix == 0 && journal.state != BidirectionalRunState::RollingBack {
        return Err(invalid_input(
            "Remote-Rollback besitzt keinen committed Teilzustand",
        ));
    }
    for index in 0..prefix {
        projection_rollback_units(&journal, index)?;
    }
    for (index, projection) in journal.projections.iter_mut().enumerate() {
        projection.state = if index < prefix {
            BidirectionalProjectionState::Committed
        } else {
            BidirectionalProjectionState::Prepared
        };
        if index >= prefix {
            projection.quarantine = None;
        }
    }
    journal.state = BidirectionalRunState::RollingBack;
    journal.refresh_digest()?;
    store.save_run_journal(&journal)?;
    let now = std::time::SystemTime::now();
    let expires_at = now + crate::REAL_PERMIT_LIFETIME;
    let preview = LocalBidirectionalRollbackPreview {
        pair_id: journal.pair_id,
        committed_projections: prefix as u64,
        confirmation_phrase: rollback_phrase(pair_name)?,
        expires_at_unix_ms: expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
    };
    Ok(PreparedLocalBidirectionalRollback {
        left_root,
        right_root,
        left_vfs,
        right_vfs,
        left_snapshot,
        right_snapshot,
        store,
        journal,
        preview,
        expires_at,
    })
}

impl PreparedLocalBidirectionalRollback {
    pub fn preview(&self) -> &LocalBidirectionalRollbackPreview {
        &self.preview
    }

    pub fn confirm(self, phrase: &str) -> io::Result<LocalBidirectionalRollback> {
        if phrase != self.preview.confirmation_phrase {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Rollback-Bestätigungsphrase stimmt nicht exakt überein",
            ));
        }
        if std::time::SystemTime::now() > self.expires_at {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Rollback-Vorbereitung ist älter als zehn Minuten",
            ));
        }
        if self.store.load_run_journal()? != Some(self.journal.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Rollback-Journal änderte sich seit der Vorschau",
            ));
        }
        let (left, right) = final_local_snapshots(
            self.left_vfs.as_ref(),
            &self.left_root,
            self.right_vfs.as_ref(),
            &self.right_root,
        )?;
        if left != self.left_snapshot || right != self.right_snapshot {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Rollback-Bäume änderten sich seit der Vorschau",
            ));
        }
        Ok(LocalBidirectionalRollback {
            left_root: self.left_root,
            right_root: self.right_root,
            left_vfs: self.left_vfs,
            right_vfs: self.right_vfs,
            store: self.store,
            journal: self.journal,
        })
    }
}

impl PreparedLocalBidirectionalRun {
    /// Berechnet nach jeder Einzelwahl die verbleibenden Optionen aus denselben
    /// gehaltenen Snapshots. Frühere Entscheidungen zählen bei Mengenlimits mit.
    pub fn conflict_choices_after_decisions(
        &self,
        resolutions: &[ConflictResolution],
        expected_digest: [u8; 32],
    ) -> io::Result<Vec<BidirectionalConflictView>> {
        if self.preview.plan.plan_digest != expected_digest {
            return Err(crate::real_mode::plan_drift(
                "Die Konfliktentscheidungen gehören zu einer anderen Vorbereitung",
            ));
        }
        let resolved = resolve_conflicts(
            &self.preview.plan,
            &self.left_snapshot,
            &self.right_snapshot,
            resolutions,
        )?;
        bidirectional_conflict_views(&resolved, &self.left_snapshot, &self.right_snapshot)
    }

    pub fn preview(&self) -> &LocalBidirectionalPreview {
        &self.preview
    }

    fn reject_before_mutation<T>(&self, error: io::Error) -> io::Result<T> {
        if self.journal_required && self.discard_allowed {
            self.store.remove_run_journal()?;
        }
        Err(error)
    }

    /// Eine noch nicht bestätigte Vorbereitung bewusst verwerfen. Queue und UI
    /// brauchen diesen Weg, wenn ein frischer Scan andere Digests als der gespeicherte
    /// Eintrag liefert; ein mutationsloses Laufjournal darf dann nicht zurückbleiben.
    pub fn discard(&self) -> io::Result<()> {
        if !self.discard_allowed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Eine Resume-Vorbereitung darf das offene Recovery-Journal nicht verwerfen",
            ));
        }
        if self.journal_required {
            // Die Primitive behandelt ein bereits entferntes Journal idempotent,
            // synchronisiert den Elternordner aber auch bei diesem Retry erneut.
            self.store.remove_run_journal()
        } else {
            Ok(())
        }
    }

    fn matches_prepared_journal(&self, journal: &BidirectionalRunJournal) -> bool {
        journal.pair_id == self.pair_id
            && journal.plan_digest == self.preview.plan.plan_digest
            && journal.baseline_digest == self.preview.plan.baseline_digest
            && journal.left_binding == self.left_binding
            && journal.right_binding == self.right_binding
            && journal.left_snapshot_digest == self.left_snapshot.snapshot_digest
            && journal.right_snapshot_digest == self.right_snapshot.snapshot_digest
            && journal.state == BidirectionalRunState::Prepared
            && journal
                .projections
                .iter()
                .all(|projection| projection.state == BidirectionalProjectionState::Prepared)
            && journal.rollback_quarantines.is_empty()
            && journal.rollback_step.is_none()
            && journal.type_swap_step.is_none()
    }

    /// Schließt ausschließlich den Übergang einer bereits nachweislich persistierten
    /// Queueoperation ab. Vor dem Queuecommit darf dieser Weg nie aufgerufen werden.
    /// Meldet das Unlink erst nach sichtbarer Wirkung einen Fehler, ist der Übergang
    /// trotzdem in diesem Prozess abgeschlossen: Ein möglicher Rückfall nach einem
    /// Absturz hinterlässt höchstens das alte, mutationslose Journal und blockiert den
    /// Queue-Lauf beim Neustart fail-closed.
    pub fn finish_enqueue_transition(&self) -> io::Result<()> {
        self.finish_enqueue_transition_using(
            |journal| std::fs::remove_file(journal),
            |parent| File::open(parent)?.sync_all(),
        )
    }

    fn finish_enqueue_transition_using(
        &self,
        remove_file: impl FnOnce(&Path) -> io::Result<()>,
        sync_parent: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        if !self.discard_allowed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Eine Resume-Vorbereitung kann nicht in eine neue Queueoperation übergehen",
            ));
        }
        if !self.journal_required {
            return Ok(());
        }
        let before = self
            .store
            .load_run_journal()?
            .ok_or_else(|| invalid_data("Erwartetes Laufjournal fehlt vor dem Queueübergang"))?;
        if !self.matches_prepared_journal(&before) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Laufjournal änderte sich vor dem Queueübergang",
            ));
        }

        match remove_file_and_sync_parent_using(
            &self.store.run_journal_path(),
            remove_file,
            sync_parent,
        ) {
            Ok(()) => Ok(()),
            Err(remove_error) => match self.store.load_run_journal() {
                Ok(None) => Ok(()),
                Ok(Some(after)) if self.matches_prepared_journal(&after) => Err(remove_error),
                Ok(Some(_)) => Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!(
                        "Laufjournal änderte sich während des Queueübergangs: {remove_error}"
                    ),
                )),
                Err(read_error) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Queueübergang schlug fehl und das Laufjournal ist nicht lesbar: {remove_error}; {read_error}"
                    ),
                )),
            },
        }
    }

    pub fn confirm(
        self,
        phrase: &str,
        backup_confirmed: bool,
    ) -> io::Result<LocalBidirectionalRun> {
        if let Some(reason) = &self.preview.stage_blocker {
            return self.reject_before_mutation(io::Error::new(
                io::ErrorKind::Unsupported,
                reason.clone(),
            ));
        }
        if phrase != self.preview.confirmation_phrase {
            return self.reject_before_mutation(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Bidirektionale Bestätigungsphrase stimmt nicht exakt überein",
            ));
        }
        if self.preview.backup_confirmation_required && !backup_confirmed {
            return self.reject_before_mutation(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Bidirektionale Ersetzungen benötigen ein bestätigtes unabhängiges Backup",
            ));
        }
        if std::time::SystemTime::now() > self.expires_at {
            return self.reject_before_mutation(io::Error::new(
                io::ErrorKind::TimedOut,
                "Bidirektionale Vorbereitung ist älter als zehn Minuten",
            ));
        }
        if self.store.load()? != self.baseline {
            return self.reject_before_mutation(io::Error::new(
                io::ErrorKind::Interrupted,
                "Baseline änderte sich seit der Vorschau",
            ));
        }
        match (self.journal_required, self.store.load_run_journal()?) {
            (true, Some(journal))
                if journal.pair_id == self.pair_id
                    && journal.plan_digest == self.preview.plan.plan_digest
                    && journal.baseline_digest == self.preview.plan.baseline_digest
                    && journal.state == BidirectionalRunState::Prepared
                    && self.prepared_run_ordinals.len() == self.prepared_runs.len()
                    && self
                        .prepared_run_ordinals
                        .iter()
                        .chain(&self.deferred_run_ordinals)
                        .all(|ordinal| {
                            journal.projections.get(*ordinal).is_some_and(|projection| {
                                projection.state == BidirectionalProjectionState::Prepared
                            })
                        }) => {}
            (false, None) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Laufjournal änderte sich seit der Vorschau",
                ));
            }
        }
        let (fresh_left, fresh_right) = final_local_snapshots(
            self.left_vfs.as_ref(),
            &self.left_root,
            self.right_vfs.as_ref(),
            &self.right_root,
        )?;
        if fresh_left != self.left_snapshot || fresh_right != self.right_snapshot {
            return self.reject_before_mutation(io::Error::new(
                io::ErrorKind::Interrupted,
                "Linker oder rechter Baum änderte sich seit der Vorschau",
            ));
        }
        let mut runs = Vec::with_capacity(self.prepared_runs.len());
        for prepared in self.prepared_runs {
            let internal_phrase = prepared.preview().confirmation_phrase.clone();
            runs.push(prepared.confirm(&internal_phrase, backup_confirmed)?);
        }
        let commit_required = self.baseline.is_none()
            || self.baseline.as_ref().is_some_and(|baseline| {
                baseline.version != BASELINE_VERSION
                    || baseline.left_binding != self.left_binding
                    || baseline.right_binding != self.right_binding
            })
            || self
                .preview
                .plan
                .actions
                .iter()
                .any(|action| !matches!(action, BidirectionalAction::SkippedConflict { .. }));
        Ok(LocalBidirectionalRun {
            pair_id: self.pair_id,
            left_root: self.left_root,
            right_root: self.right_root,
            left_vfs: self.left_vfs,
            right_vfs: self.right_vfs,
            left_endpoint: self.left_endpoint,
            right_endpoint: self.right_endpoint,
            left_binding: self.left_binding,
            right_binding: self.right_binding,
            baseline: self.baseline,
            plan: self.preview.plan,
            store: self.store,
            runs,
            run_ordinals: self.prepared_run_ordinals,
            deferred_run_ordinals: self.deferred_run_ordinals,
            commit_required,
            journal_required: self.journal_required,
            backup_confirmed,
        })
    }
}

fn cancelled_projection_error(message: &str, result: &crate::SyncStats) -> io::Error {
    crate::error::with_recovery_paths(
        io::Error::new(io::ErrorKind::Interrupted, message),
        &result.temp_recovery_paths,
    )
}

impl LocalBidirectionalRun {
    pub fn execute(
        self,
        cancel: &crate::Cancel,
        on_progress: &mut dyn FnMut(&crate::Progress),
    ) -> io::Result<LocalBidirectionalStats> {
        let mut stats = Vec::with_capacity(self.runs.len() + self.deferred_run_ordinals.len());
        // Folgeprüfungen und Journalfehler dürfen Reste bereits ausgeführter
        // Projektionen nicht verdecken. Der aktuelle Syncfehler bringt eigene
        // Pfade mit; die Fehlerhülle ergänzt die früheren ohne Duplikate.
        let (baseline_generation, baseline_committed) = self
            .execute_with_stats(cancel, on_progress, &mut stats)
            .map_err(|error| {
                let paths = stats
                    .iter()
                    .flat_map(|result| result.temp_recovery_paths.iter().cloned())
                    .collect::<Vec<_>>();
                crate::error::with_recovery_paths(error, &paths)
            })?;
        Ok(LocalBidirectionalStats {
            runs: stats,
            baseline_generation,
            baseline_committed,
        })
    }

    fn execute_with_stats(
        self,
        cancel: &crate::Cancel,
        on_progress: &mut dyn FnMut(&crate::Progress),
        stats: &mut Vec<crate::SyncStats>,
    ) -> io::Result<(u64, bool)> {
        let LocalBidirectionalRun {
            pair_id,
            left_root,
            right_root,
            left_vfs,
            right_vfs,
            left_endpoint,
            right_endpoint,
            left_binding,
            right_binding,
            baseline,
            plan,
            store,
            runs,
            run_ordinals,
            deferred_run_ordinals,
            commit_required,
            journal_required,
            backup_confirmed,
        } = self;
        if store.load()? != baseline {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Baseline änderte sich nach der Permit-Ausgabe",
            ));
        }
        for (index, run) in run_ordinals.into_iter().zip(runs) {
            if journal_required {
                store.update_run_journal(|journal| {
                    let projection = journal
                        .projections
                        .get_mut(index)
                        .ok_or_else(|| invalid_data("Journalprojektion fehlt"))?;
                    if projection.state != BidirectionalProjectionState::Prepared {
                        return Err(invalid_data(
                            "Journalprojektion besitzt vor dem Start falschen Zustand",
                        ));
                    }
                    projection.state = BidirectionalProjectionState::Running;
                    journal.state = BidirectionalRunState::Running;
                    Ok(())
                })?;
            }
            let result_index = stats.len();
            stats.push(run.execute(cancel, on_progress)?);
            let result = &stats[result_index];
            if result.cancelled {
                return Err(cancelled_projection_error(
                    "Bidirektionaler Lauf wurde vor dem Baseline-Commit abgebrochen",
                    result,
                ));
            }
            if journal_required {
                store.update_run_journal(|journal| {
                    let projection = journal
                        .projections
                        .get_mut(index)
                        .ok_or_else(|| invalid_data("Journalprojektion fehlt"))?;
                    if projection.state != BidirectionalProjectionState::Running {
                        return Err(invalid_data(
                            "Journalprojektion besitzt nach dem Lauf falschen Zustand",
                        ));
                    }
                    projection.state = BidirectionalProjectionState::Committed;
                    projection.quarantine = result.quarantine.clone();
                    Ok(())
                })?;
            }
        }
        if !deferred_run_ordinals.is_empty() {
            let context = LocalProjectionContext::new(
                left_vfs.clone(),
                right_vfs.clone(),
                left_endpoint.clone(),
                right_endpoint.clone(),
                left_root.clone(),
                right_root.clone(),
            )?;
            for index in deferred_run_ordinals {
                if cancel.is_cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Bidirektionaler Lauf wurde vor der aufgeschobenen Projektion abgebrochen",
                    ));
                }
                let journal = store
                    .load_run_journal()?
                    .ok_or_else(|| invalid_data("Aufgeschobene Projektion besitzt kein Journal"))?;
                let (fresh_left, fresh_right) = final_local_snapshots(
                    left_vfs.as_ref(),
                    &left_root,
                    right_vfs.as_ref(),
                    &right_root,
                )?;
                if !snapshots_match_projection_prefix(&journal, &fresh_left, &fresh_right, index)? {
                    let (expected_left, expected_right) =
                        expected_maps_after_prefix(&journal, index)?;
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        format!(
                            "Aufgeschobene Projektion driftete außerhalb der gebundenen Vorwirkungen: {}",
                            first_pair_map_mismatch(
                                &expected_left,
                                &expected_right,
                                &owned_snapshot_map(&fresh_left),
                                &owned_snapshot_map(&fresh_right),
                            )
                        ),
                    ));
                }
                store.update_run_journal(|journal| {
                    let projection = journal
                        .projections
                        .get_mut(index)
                        .ok_or_else(|| invalid_data("Aufgeschobene Journalprojektion fehlt"))?;
                    if projection.state != BidirectionalProjectionState::Prepared {
                        return Err(invalid_data(
                            "Aufgeschobene Projektion besitzt falschen Zustand",
                        ));
                    }
                    projection.state = BidirectionalProjectionState::Running;
                    journal.state = BidirectionalRunState::Running;
                    Ok(())
                })?;
                let projection = planned_projections(&plan)?
                    .get(index)
                    .map(|entry| entry.projection.clone())
                    .ok_or_else(|| invalid_data("Aufgeschobene Projektion fehlt im Plan"))?;
                if matches!(projection, BidirectionalProjectionKind::TypeSwap { .. }) {
                    execute_local_type_swap_projection(
                        &store,
                        index,
                        LocalEndpointPair {
                            left_vfs: left_vfs.as_ref(),
                            left_root: &left_root,
                            right_vfs: right_vfs.as_ref(),
                            right_root: &right_root,
                        },
                    )?;
                    continue;
                }
                let allowed = BTreeSet::from([index]);
                let (prepared, ordinals, deferred) =
                    prepare_projection_runs(&plan, &context, Some(&allowed), false)?;
                if ordinals != vec![index] || !deferred.is_empty() || prepared.len() != 1 {
                    return Err(invalid_data(
                        "Aufgeschobene Einwegprojektion wurde nicht eindeutig vorbereitet",
                    ));
                }
                let prepared = prepared
                    .into_iter()
                    .next()
                    .ok_or_else(|| invalid_data("Aufgeschobene Löschprojektion fehlt"))?;
                let phrase = prepared.preview().confirmation_phrase.clone();
                let result_index = stats.len();
                stats.push(
                    prepared
                        .confirm(&phrase, backup_confirmed)?
                        .execute(cancel, on_progress)?,
                );
                let result = &stats[result_index];
                if result.cancelled {
                    return Err(cancelled_projection_error(
                        "Bidirektionale Löschprojektion wurde abgebrochen",
                        result,
                    ));
                }
                store.update_run_journal(|journal| {
                    let projection = journal
                        .projections
                        .get_mut(index)
                        .ok_or_else(|| invalid_data("Aufgeschobene Journalprojektion fehlt"))?;
                    if projection.state != BidirectionalProjectionState::Running {
                        return Err(invalid_data(
                            "Aufgeschobene Projektion verlor ihren Laufzustand",
                        ));
                    }
                    projection.state = BidirectionalProjectionState::Committed;
                    projection.quarantine = result.quarantine.clone();
                    Ok(())
                })?;
            }
        }

        let (first_left, first_right) = final_local_snapshots(
            left_vfs.as_ref(),
            &left_root,
            right_vfs.as_ref(),
            &right_root,
        )?;
        let (second_left, second_right) = final_local_snapshots(
            left_vfs.as_ref(),
            &left_root,
            right_vfs.as_ref(),
            &right_root,
        )?;
        if first_left != second_left
            || first_right != second_right
            || !snapshots_equal_except_skipped(&second_left, &second_right, &plan)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!(
                    "Finale Doppelprüfung ergab keine zwei stabil identischen Bäume: {}",
                    first_semantic_mismatch(&second_left, &second_right)
                ),
            ));
        }

        let generation = if commit_required {
            if store.load()? != baseline {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Baseline änderte sich während der Ausführung",
                ));
            }
            let next = match &baseline {
                Some(previous) => previous.advance_from_resolved_snapshots(
                    left_binding,
                    right_binding,
                    &second_left,
                    &second_right,
                    &plan,
                )?,
                None => baseline_from_resolved_snapshots(
                    pair_id,
                    left_binding,
                    right_binding,
                    &second_left,
                    &second_right,
                    &plan,
                )?,
            };
            let generation = next.generation;
            if journal_required {
                store.update_run_journal(|journal| {
                    if journal.projections.iter().any(|projection| {
                        projection.state != BidirectionalProjectionState::Committed
                    }) {
                        return Err(invalid_data(
                            "Folgebaseline darf erst nach allen Projektionen gebunden werden",
                        ));
                    }
                    journal.state = BidirectionalRunState::Running;
                    journal.committed_baseline_digest = Some(next.body_digest);
                    Ok(())
                })?;
            }
            store.save(&next)?;
            if journal_required {
                store.update_run_journal(|journal| {
                    journal.state = BidirectionalRunState::BaselineCommitted;
                    Ok(())
                })?;
            }
            generation
        } else {
            baseline
                .as_ref()
                .ok_or_else(|| invalid_data("No-op-Lauf besitzt keine Baseline"))?
                .generation
        };
        Ok((generation, commit_required))
    }
}

#[derive(Clone, Copy)]
struct LocalEndpointPair<'a> {
    left_vfs: &'a dyn Vfs,
    left_root: &'a Path,
    right_vfs: &'a dyn Vfs,
    right_root: &'a Path,
}

fn execute_local_type_swap_projection(
    store: &BaselineStore,
    ordinal: usize,
    endpoints: LocalEndpointPair<'_>,
) -> io::Result<()> {
    let LocalEndpointPair {
        left_vfs,
        left_root,
        right_vfs,
        right_root,
    } = endpoints;
    let journal = store
        .load_run_journal()?
        .ok_or_else(|| invalid_data("Typwechsel besitzt kein Laufjournal"))?;
    let projection = journal
        .projections
        .get(ordinal)
        .ok_or_else(|| invalid_data("Typwechselprojektion fehlt"))?;
    let BidirectionalProjectionKind::TypeSwap {
        path,
        staging_path,
        direction,
    } = &projection.projection
    else {
        return Err(invalid_data("Aufgeschobene Projektion ist kein Typwechsel"));
    };
    if projection.state != BidirectionalProjectionState::Running || journal.type_swap_step.is_some()
    {
        return Err(invalid_data(
            "Typwechselprojektion besitzt vor dem Austausch falschen Journalzustand",
        ));
    }
    let (_, target_side) = direction_sides(*direction);
    let (vfs, root) = match target_side {
        EndpointSide::Left => (left_vfs, left_root),
        EndpointSide::Right => (right_vfs, right_root),
    };
    let (current_left, current_right) =
        final_local_snapshots(left_vfs, left_root, right_vfs, right_root)?;
    if !snapshots_match_projection_prefix(&journal, &current_left, &current_right, ordinal)? {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Typwechsel-Ausgangszustand driftete vor dem atomaren Austausch",
        ));
    }
    let (expected_left, expected_right) = expected_maps_after_prefix(&journal, ordinal)?;
    let previous = match target_side {
        EndpointSide::Left => expected_left.get(path),
        EndpointSide::Right => expected_right.get(path),
    }
    .ok_or_else(|| invalid_data("Typwechsel-Vorzustand fehlt"))?;

    let relative = path.to_path_buf()?;
    let staging_relative = staging_path.to_path_buf()?;
    if relative.parent() != staging_relative.parent() {
        return Err(invalid_data(
            "Typwechsel-Stagingpfad liegt nicht neben dem Original",
        ));
    }
    let original = root.join(&relative);
    let staging = root.join(&staging_relative);
    let target_parent = root
        .parent()
        .ok_or_else(|| invalid_input("Typwechsel-Root besitzt keinen Elternordner"))?;
    let base = target_parent.join(".doppeldecker-quarantine");
    crate::real_mode::ensure_quarantine_root(vfs, target_parent, &base)?;
    let run_root = match projection.quarantine.as_deref() {
        Some(path) => PathBuf::from(path),
        None => crate::real_mode::reserve_quarantine_run_name(vfs, &base)?,
    };
    store.update_run_journal(|journal| {
        let projection = journal
            .projections
            .get_mut(ordinal)
            .ok_or_else(|| invalid_data("Typwechselprojektion fehlt"))?;
        if projection.state != BidirectionalProjectionState::Running
            || journal.type_swap_step.is_some()
        {
            return Err(invalid_data(
                "Typwechseljournal änderte sich vor dem Intent-Commit",
            ));
        }
        projection.quarantine = Some(run_root.to_string_lossy().into_owned());
        journal.type_swap_step = Some(BidirectionalTypeSwapStep {
            ordinal: ordinal as u64,
            side: target_side,
            path: path.clone(),
            staging_path: staging_path.clone(),
            quarantine_root: run_root.clone(),
            phase: BidirectionalTypeSwapPhase::Exchange,
        });
        Ok(())
    })?;
    let _quarantine_lease = ensure_type_swap_quarantine_started(vfs, &run_root, root)?;

    vfs.rename_exchange(&original, &staging)
        .map_err(|error| bidirectional_effect_error(error, &[&original, &staging]))?;
    sync_rollback_parents(vfs, &original, &staging)?;
    store.update_run_journal(|journal| {
        let step = journal
            .type_swap_step
            .as_mut()
            .ok_or_else(|| invalid_data("Typwechsel-Intent verschwand"))?;
        if step.ordinal as usize != ordinal {
            return Err(invalid_data(
                "Typwechsel-Intent gehört zu anderer Projektion",
            ));
        }
        step.phase = BidirectionalTypeSwapPhase::QuarantinePrevious;
        Ok(())
    })?;

    let backup = run_root.join(&relative);
    if vfs.exists(&backup) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Typwechsel-Quarantänepfad existiert bereits",
        ));
    }
    if let Some(parent) = backup.parent() {
        vfs.create_dir_all(parent)?;
    }
    crate::quarantine::bind_quarantine_run_identity(vfs, &run_root)?;
    crate::quarantine::record_quarantine_intent_for_source(
        vfs,
        &run_root,
        &relative,
        &staging,
        previous.kind == SnapshotEntryKind::Directory,
        crate::quarantine::QuarantineReason::Replaced,
    )?;
    vfs.rename_noreplace(&staging, &backup)?;
    sync_rollback_parents(vfs, &staging, &backup)?;

    let (final_left, final_right) =
        final_local_snapshots(left_vfs, left_root, right_vfs, right_root)?;
    let latest = store
        .load_run_journal()?
        .ok_or_else(|| invalid_data("Typwechseljournal verschwand"))?;
    if !snapshots_match_projection_prefix(&latest, &final_left, &final_right, ordinal + 1)? {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Typwechsel-Nachzustand entspricht nicht dem gebundenen Gewinner-Unterbaum",
        ));
    }
    crate::quarantine::complete_quarantine_run(vfs, &run_root)?;
    store.update_run_journal(|journal| {
        let step = journal
            .type_swap_step
            .as_ref()
            .ok_or_else(|| invalid_data("Typwechsel-Intent fehlt beim Commit"))?;
        if step.ordinal as usize != ordinal
            || step.phase != BidirectionalTypeSwapPhase::QuarantinePrevious
        {
            return Err(invalid_data(
                "Typwechsel-Intent besitzt falsche Commitphase",
            ));
        }
        journal.type_swap_step = None;
        let projection = journal
            .projections
            .get_mut(ordinal)
            .ok_or_else(|| invalid_data("Typwechselprojektion fehlt beim Commit"))?;
        projection.state = BidirectionalProjectionState::Committed;
        projection.quarantine = Some(run_root.to_string_lossy().into_owned());
        Ok(())
    })
}

fn ensure_type_swap_quarantine_started(
    vfs: &dyn Vfs,
    run_root: &Path,
    target_root: &Path,
) -> io::Result<crate::quarantine::QuarantineLease> {
    let base = run_root
        .parent()
        .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keinen Basisordner"))?;
    let run_id = run_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keine Lauf-ID"))?;
    match crate::quarantine::list_quarantine_entries_for_target(
        vfs,
        base,
        run_id,
        &None,
        Some(target_root),
    ) {
        Ok(_) => crate::quarantine::lease_quarantine_run(vfs, run_root, target_root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            crate::quarantine::start_quarantine_run(vfs, run_root, target_root)
        }
        Err(error) => Err(error),
    }
}

impl LocalBidirectionalRollback {
    pub fn execute(self, cancel: &crate::Cancel) -> io::Result<LocalBidirectionalRollbackStats> {
        let LocalBidirectionalRollback {
            left_root,
            right_root,
            left_vfs,
            right_vfs,
            store,
            mut journal,
        } = self;
        if store.load_run_journal()? != Some(journal.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Rollback-Journal änderte sich nach der Permit-Ausgabe",
            ));
        }
        let committed = journal
            .projections
            .iter()
            .take_while(|projection| projection.state == BidirectionalProjectionState::Committed)
            .count();
        let mut required_quarantines = journal
            .projections
            .iter()
            .take(committed)
            .filter_map(|projection| {
                projection.quarantine.as_ref().map(|run| {
                    (
                        projection_target_side(&projection.projection),
                        PathBuf::from(run),
                    )
                })
            })
            .chain(
                journal
                    .rollback_quarantines
                    .iter()
                    .map(|quarantine| (quarantine.side, quarantine.run_root.clone())),
            )
            .collect::<Vec<_>>();
        required_quarantines.sort_by(|left, right| {
            endpoint_side_order(left.0)
                .cmp(&endpoint_side_order(right.0))
                .then_with(|| left.1.cmp(&right.1))
        });
        required_quarantines.dedup();
        let mut quarantine_leases = Vec::with_capacity(required_quarantines.len());
        for (side, run_root) in required_quarantines {
            let (vfs, root) = match side {
                EndpointSide::Left => (left_vfs.as_ref(), &left_root),
                EndpointSide::Right => (right_vfs.as_ref(), &right_root),
            };
            let lease = crate::quarantine::lease_quarantine_run(vfs, &run_root, root)?;
            quarantine_leases.push(HeldRollbackQuarantine {
                side,
                run_root,
                lease,
            });
        }
        let mut rolled_back = 0_u64;
        for index in (0..committed).rev() {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Bidirektionaler Rollback wurde abgebrochen",
                ));
            }
            let units = projection_rollback_units(&journal, index)?;
            rollback_projection_units(
                &store,
                &mut journal,
                index,
                &units,
                LocalEndpointPair {
                    left_vfs: left_vfs.as_ref(),
                    left_root: &left_root,
                    right_vfs: right_vfs.as_ref(),
                    right_root: &right_root,
                },
                &mut quarantine_leases,
            )?;
            rolled_back += 1;
        }

        let (first_left, first_right) = final_local_snapshots(
            left_vfs.as_ref(),
            &left_root,
            right_vfs.as_ref(),
            &right_root,
        )?;
        let (second_left, second_right) = final_local_snapshots(
            left_vfs.as_ref(),
            &left_root,
            right_vfs.as_ref(),
            &right_root,
        )?;
        if first_left != second_left
            || first_right != second_right
            || !snapshots_semantically_equal(&second_left, &journal.left_snapshot)
            || !snapshots_semantically_equal(&second_right, &journal.right_snapshot)
            || store.load()?.map(|value| value.body_digest) != journal.baseline_digest
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Rollback-Abschluss entspricht nicht den stabilen Ausgangsbäumen",
            ));
        }
        for quarantine in &journal.rollback_quarantines {
            let vfs = match quarantine.side {
                EndpointSide::Left => left_vfs.as_ref(),
                EndpointSide::Right => right_vfs.as_ref(),
            };
            crate::quarantine::complete_quarantine_run(vfs, &quarantine.run_root)?;
        }
        let quarantine_paths = journal
            .rollback_quarantines
            .iter()
            .map(|item| item.run_root.to_string_lossy().into_owned())
            .collect();
        // Die Bäume sind bereits vollständig zurückgesetzt. Das Journal darf nun
        // trotzdem noch nicht verschwinden: Erst sein eigener atomarer Commit
        // beweist einem neu gestarteten Prozess, dass ausschließlich der
        // mutationslose Queue-/Journalabschluss offen ist.
        store.update_run_journal(|persisted| {
            if *persisted != journal {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Rollback-Journal änderte sich vor dem Abschlusscommit",
                ));
            }
            persisted.state = BidirectionalRunState::RollbackCommitted;
            Ok(())
        })?;
        Ok(LocalBidirectionalRollbackStats {
            projections_rolled_back: rolled_back,
            rollback_quarantines: quarantine_paths,
        })
    }
}

struct HeldRollbackQuarantine {
    side: EndpointSide,
    run_root: PathBuf,
    lease: crate::quarantine::QuarantineLease,
}

fn endpoint_side_order(side: EndpointSide) -> u8 {
    match side {
        EndpointSide::Left => 0,
        EndpointSide::Right => 1,
    }
}

fn projection_target_side(projection: &BidirectionalProjectionKind) -> EndpointSide {
    match projection {
        BidirectionalProjectionKind::ConflictCopy { target_side, .. } => *target_side,
        BidirectionalProjectionKind::Copy { direction, .. }
        | BidirectionalProjectionKind::TypeSwap { direction, .. }
        | BidirectionalProjectionKind::Delete { direction, .. } => direction_sides(*direction).1,
    }
}

fn held_rollback_quarantine<'lease>(
    leases: &'lease [HeldRollbackQuarantine],
    side: EndpointSide,
    run_root: &Path,
) -> io::Result<&'lease crate::quarantine::QuarantineLease> {
    leases
        .iter()
        .find(|held| held.side == side && held.run_root == run_root)
        .map(|held| &held.lease)
        .ok_or_else(|| invalid_data("Rollback besitzt keine vorab gehaltene Quarantäne-Lease"))
}

#[derive(Clone)]
struct ProjectionDelta {
    side: EndpointSide,
    path: BinaryRelativePath,
    before: Option<SnapshotEntry>,
    after: Option<SnapshotEntry>,
}

fn projection_deltas(
    journal: &BidirectionalRunJournal,
    ordinal: usize,
) -> io::Result<Vec<ProjectionDelta>> {
    let mut before_left = owned_snapshot_map(&journal.left_snapshot);
    let mut before_right = owned_snapshot_map(&journal.right_snapshot);
    for projection in journal.projections.iter().take(ordinal) {
        apply_projection_state(&projection.projection, &mut before_left, &mut before_right)?;
    }
    let mut after_left = before_left.clone();
    let mut after_right = before_right.clone();
    let projection = journal
        .projections
        .get(ordinal)
        .ok_or_else(|| invalid_data("Rollback-Projektion fehlt"))?;
    apply_projection_state(&projection.projection, &mut after_left, &mut after_right)?;
    let target_side = match projection.projection {
        BidirectionalProjectionKind::ConflictCopy { target_side, .. } => target_side,
        BidirectionalProjectionKind::Copy { direction, .. }
        | BidirectionalProjectionKind::Delete { direction, .. }
        | BidirectionalProjectionKind::TypeSwap { direction, .. } => match direction {
            SyncDirection::LeftToRight => EndpointSide::Right,
            SyncDirection::RightToLeft => EndpointSide::Left,
        },
    };
    let (before, after) = match target_side {
        EndpointSide::Left => (&before_left, &after_left),
        EndpointSide::Right => (&before_right, &after_right),
    };
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let old = before.get(&path).cloned();
            let new = after.get(&path).cloned();
            let unchanged = match (&old, &new) {
                (Some(old), Some(new)) => old.semantically_equal(new),
                (None, None) => true,
                _ => false,
            };
            (!unchanged).then_some(ProjectionDelta {
                side: target_side,
                path,
                before: old,
                after: new,
            })
        })
        .collect())
}

fn projection_rollback_units(
    journal: &BidirectionalRunJournal,
    ordinal: usize,
) -> io::Result<Vec<ProjectionDelta>> {
    if let Some(BidirectionalProjectionKind::TypeSwap { path, .. }) = journal
        .projections
        .get(ordinal)
        .map(|projection| &projection.projection)
    {
        let delta = projection_deltas(journal, ordinal)?
            .into_iter()
            .find(|delta| &delta.path == path)
            .ok_or_else(|| invalid_data("Typwechsel-Rollback besitzt keine Originalwirkung"))?;
        return Ok(vec![delta]);
    }
    let mut deltas = projection_deltas(journal, ordinal)?;
    deltas.sort_by(|left, right| {
        left.path
            .0
            .len()
            .cmp(&right.path.0.len())
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut absorbing = Vec::new();
    let mut units = Vec::new();
    for delta in deltas {
        if absorbing
            .iter()
            .any(|root| binary_path_contains(root, &delta.path))
        {
            continue;
        }
        if delta.before.is_none()
            || delta.after.is_none()
            || delta
                .before
                .as_ref()
                .zip(delta.after.as_ref())
                .is_some_and(|(before, after)| before.kind != after.kind)
        {
            absorbing.push(delta.path.clone());
        }
        units.push(delta);
    }
    units.sort_by(|left, right| {
        right
            .path
            .0
            .len()
            .cmp(&left.path.0.len())
            .then_with(|| left.path.cmp(&right.path))
    });
    if units.is_empty() {
        return Err(invalid_data(
            "Rollback-Projektion besitzt keine Wirkungseinheit",
        ));
    }
    Ok(units)
}

fn rollback_projection_units(
    store: &BaselineStore,
    journal: &mut BidirectionalRunJournal,
    ordinal: usize,
    units: &[ProjectionDelta],
    endpoints: LocalEndpointPair<'_>,
    quarantine_leases: &mut Vec<HeldRollbackQuarantine>,
) -> io::Result<()> {
    let LocalEndpointPair {
        left_vfs,
        left_root,
        right_vfs,
        right_root,
    } = endpoints;
    let side = units
        .first()
        .ok_or_else(|| invalid_data("Rollback-Projektion besitzt keine Einheiten"))?
        .side;
    if units.iter().any(|unit| unit.side != side) {
        return Err(invalid_data("Rollback-Projektion mischt Zielseiten"));
    }
    let (vfs, root) = match side {
        EndpointSide::Left => (left_vfs, left_root),
        EndpointSide::Right => (right_vfs, right_root),
    };
    let parent = root
        .parent()
        .ok_or_else(|| invalid_input("Rollback-Ziel besitzt keinen Elternordner"))?;
    loop {
        if journal.rollback_step.is_none() {
            journal.rollback_step = Some(rollback_step_for_unit(ordinal, 0, &units[0])?);
            journal.refresh_digest()?;
            store.save_run_journal(journal)?;
        }
        let step = journal
            .rollback_step
            .clone()
            .ok_or_else(|| invalid_data("Rollback-Schritt fehlt"))?;
        let unit_index = step.unit_index as usize;
        let delta = units
            .get(unit_index)
            .ok_or_else(|| invalid_data("Rollback-Einheitenindex ist ungültig"))?;
        if step.ordinal as usize != ordinal || step.side != side || step.path != delta.path {
            return Err(invalid_data(
                "Offener Rollback-Schritt gehört nicht zur nächsten Unterbaumeinheit",
            ));
        }
        let (current_left, current_right) =
            final_local_snapshots(left_vfs, left_root, right_vfs, right_root)?;
        let matches_before = snapshots_match_rollback_units(
            journal,
            &current_left,
            &current_right,
            ordinal,
            unit_index,
        )?;
        let matches_restore_gap = step.phase == BidirectionalRollbackPhase::RestorePrevious
            && delta.before.is_some()
            && snapshots_match_rollback_without_unit(
                journal,
                &current_left,
                &current_right,
                ordinal,
                unit_index,
                side,
                &delta.path,
            )?;
        let matches_type_intermediate = type_mismatch_delta(delta)
            && match step.phase {
                BidirectionalRollbackPhase::RestoreTypePreviousToStaging => {
                    snapshots_match_type_rollback_intermediate(
                        journal,
                        &current_left,
                        &current_right,
                        ordinal,
                        false,
                    )?
                }
                BidirectionalRollbackPhase::ExchangeTypeNames => {
                    snapshots_match_type_rollback_intermediate(
                        journal,
                        &current_left,
                        &current_right,
                        ordinal,
                        false,
                    )? || snapshots_match_type_rollback_intermediate(
                        journal,
                        &current_left,
                        &current_right,
                        ordinal,
                        true,
                    )?
                }
                BidirectionalRollbackPhase::QuarantineCurrent => {
                    snapshots_match_type_rollback_intermediate(
                        journal,
                        &current_left,
                        &current_right,
                        ordinal,
                        true,
                    )?
                }
                BidirectionalRollbackPhase::RestorePrevious => false,
            };
        if !matches_before && !matches_restore_gap && !matches_type_intermediate {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Rollback-Unterbaum driftete vor der nächsten Einheit",
            ));
        }

        if type_mismatch_delta(delta) {
            rollback_type_swap_unit(
                store,
                journal,
                (ordinal, unit_index),
                delta,
                vfs,
                root,
                quarantine_leases,
            )?;
        } else if metadata_only_delta(delta) {
            restore_portable_mode(vfs, &root.join(delta.path.to_path_buf()?), delta)?;
        } else {
            if step.phase == BidirectionalRollbackPhase::QuarantineCurrent {
                let after = delta
                    .after
                    .as_ref()
                    .ok_or_else(|| invalid_data("Rollback-Nachzustand fehlt"))?;
                let run_root = ensure_rollback_quarantine(
                    store,
                    journal,
                    side,
                    vfs,
                    parent,
                    root,
                    quarantine_leases,
                )?;
                let (expected_left, expected_right) =
                    expected_maps_after_rollback_units(journal, ordinal, unit_index)?;
                let expected_subtree = match side {
                    EndpointSide::Left => &expected_left,
                    EndpointSide::Right => &expected_right,
                };
                ensure_current_quarantined(
                    vfs,
                    root,
                    &run_root,
                    &delta.path,
                    after,
                    delta.before.as_ref(),
                    expected_subtree,
                )?;
                journal
                    .rollback_step
                    .as_mut()
                    .ok_or_else(|| invalid_data("Rollback-Schritt verschwand"))?
                    .phase = BidirectionalRollbackPhase::RestorePrevious;
                journal.refresh_digest()?;
                store.save_run_journal(journal)?;
            }

            if let Some(before) = &delta.before {
                ensure_previous_restored(
                    vfs,
                    root,
                    side,
                    &delta.path,
                    before,
                    &journal.projections[ordinal],
                    quarantine_leases,
                )?;
            } else if vfs.exists(&root.join(delta.path.to_path_buf()?)) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Additives Rollback-Ziel entstand vor dem Abschluss erneut",
                ));
            }
        }

        let (after_left, after_right) =
            final_local_snapshots(left_vfs, left_root, right_vfs, right_root)?;
        if !snapshots_match_rollback_units(
            journal,
            &after_left,
            &after_right,
            ordinal,
            unit_index + 1,
        )? {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Rollback-Einheit erreichte nicht den gebundenen Vorzustand",
            ));
        }
        if unit_index + 1 == units.len() {
            journal.rollback_step = None;
            journal.projections[ordinal].state = BidirectionalProjectionState::Prepared;
            journal.refresh_digest()?;
            return store.save_run_journal(journal);
        }
        journal.rollback_step = Some(rollback_step_for_unit(
            ordinal,
            unit_index + 1,
            &units[unit_index + 1],
        )?);
        journal.refresh_digest()?;
        store.save_run_journal(journal)?;
    }
}

fn rollback_type_swap_unit(
    store: &BaselineStore,
    journal: &mut BidirectionalRunJournal,
    position: (usize, usize),
    delta: &ProjectionDelta,
    vfs: &dyn Vfs,
    root: &Path,
    quarantine_leases: &[HeldRollbackQuarantine],
) -> io::Result<()> {
    // Typwechsel bilden genau eine Rollbackeinheit. Projektion und Einheit gehören
    // deshalb als gemeinsame Position zusammen und werden unten getrennt geprüft.
    let (ordinal, unit_index) = position;
    let projection = journal
        .projections
        .get(ordinal)
        .ok_or_else(|| invalid_data("Typwechsel-Rollbackprojektion fehlt"))?;
    let BidirectionalProjectionKind::TypeSwap {
        path, staging_path, ..
    } = &projection.projection
    else {
        return Err(invalid_data(
            "Typunterschied stammt nicht aus einer Typwechselprojektion",
        ));
    };
    if path != &delta.path || unit_index != 0 {
        return Err(invalid_data(
            "Typwechsel-Rollbackeinheit ist nicht eindeutig",
        ));
    }
    let before = delta
        .before
        .as_ref()
        .ok_or_else(|| invalid_data("Typwechsel-Rollback besitzt keinen Vorzustand"))?;
    delta
        .after
        .as_ref()
        .ok_or_else(|| invalid_data("Typwechsel-Rollback besitzt keinen Nachzustand"))?;
    let relative = path.to_path_buf()?;
    let staging_relative = staging_path.to_path_buf()?;
    let original = root.join(&relative);
    let staging = root.join(&staging_relative);

    let phase = journal
        .rollback_step
        .as_ref()
        .ok_or_else(|| invalid_data("Typwechsel-Rollbackschritt fehlt"))?
        .phase;
    if phase == BidirectionalRollbackPhase::RestoreTypePreviousToStaging {
        let (before_maps, _) = type_swap_before_after_maps(journal, ordinal, delta.side)?;
        restore_type_previous_to_staging(
            vfs,
            root,
            delta.side,
            projection,
            before,
            &before_maps,
            quarantine_leases,
        )?;
        journal
            .rollback_step
            .as_mut()
            .ok_or_else(|| invalid_data("Typwechsel-Rollbackschritt verschwand"))?
            .phase = BidirectionalRollbackPhase::ExchangeTypeNames;
        journal.refresh_digest()?;
        store.save_run_journal(journal)?;
    }

    if journal
        .rollback_step
        .as_ref()
        .is_some_and(|step| step.phase == BidirectionalRollbackPhase::ExchangeTypeNames)
    {
        vfs.rename_exchange(&original, &staging)
            .map_err(|error| bidirectional_effect_error(error, &[&original, &staging]))?;
        sync_rollback_parents(vfs, &original, &staging)?;
        journal
            .rollback_step
            .as_mut()
            .ok_or_else(|| invalid_data("Typwechsel-Rollbackschritt verschwand"))?
            .phase = BidirectionalRollbackPhase::RestorePrevious;
        journal.refresh_digest()?;
        store.save_run_journal(journal)?;
    }
    if journal
        .rollback_step
        .as_ref()
        .is_none_or(|step| step.phase != BidirectionalRollbackPhase::RestorePrevious)
    {
        return Err(invalid_data(
            "Typwechsel-Rollback erreichte keine abgeschlossene Rücktauschphase",
        ));
    }
    Ok(())
}

fn rollback_step_for_unit(
    ordinal: usize,
    unit_index: usize,
    delta: &ProjectionDelta,
) -> io::Result<BidirectionalRollbackStep> {
    if delta.before.is_none() && delta.after.is_none() {
        return Err(invalid_data("Rollback-Einheit besitzt keine Wirkung"));
    }
    Ok(BidirectionalRollbackStep {
        ordinal: ordinal as u64,
        unit_index: unit_index as u64,
        side: delta.side,
        path: delta.path.clone(),
        phase: if type_mismatch_delta(delta) {
            BidirectionalRollbackPhase::RestoreTypePreviousToStaging
        } else if delta.after.is_some() && !metadata_only_delta(delta) {
            BidirectionalRollbackPhase::QuarantineCurrent
        } else {
            BidirectionalRollbackPhase::RestorePrevious
        },
    })
}

fn type_mismatch_delta(delta: &ProjectionDelta) -> bool {
    delta
        .before
        .as_ref()
        .zip(delta.after.as_ref())
        .is_some_and(|(before, after)| before.kind != after.kind)
}

fn metadata_only_delta(delta: &ProjectionDelta) -> bool {
    delta
        .before
        .as_ref()
        .zip(delta.after.as_ref())
        .is_some_and(|(before, after)| {
            before.kind == after.kind
                && before.size == after.size
                && before.content_digest == after.content_digest
                && before.metadata_digest != after.metadata_digest
        })
}

fn restore_portable_mode(vfs: &dyn Vfs, path: &Path, delta: &ProjectionDelta) -> io::Result<()> {
    let before = delta
        .before
        .as_ref()
        .ok_or_else(|| invalid_data("Metadaten-Rollback besitzt keinen Vorzustand"))?;
    let mode = (0_u32..=0o777)
        .find(|mode| portable_metadata_digest(Some(*mode)) == before.metadata_digest)
        .ok_or_else(|| invalid_data("Portabler Modus lässt sich nicht aus dem Digest ableiten"))?;
    vfs.set_mode(path, mode)
        .map_err(|error| bidirectional_effect_error(error, &[path]))?;
    let result = if before.kind == SnapshotEntryKind::Directory {
        vfs.sync_dir(path)
    } else {
        vfs.sync_file(path)
    };
    result.map_err(|error| bidirectional_effect_error(error, &[path]))?;
    let restored = snapshot_local_entry(vfs, path)
        .map_err(|error| bidirectional_effect_error(error, &[path]))?;
    if !restored.semantically_equal(before) {
        return Err(bidirectional_effect_error(
            io::Error::new(
                io::ErrorKind::Interrupted,
                "Portabler Modus entspricht nach Rollback nicht dem Vorzustand",
            ),
            &[path],
        ));
    }
    Ok(())
}

fn ensure_current_quarantined(
    vfs: &dyn Vfs,
    target_root: &Path,
    run_root: &Path,
    relative: &BinaryRelativePath,
    after: &SnapshotEntry,
    before: Option<&SnapshotEntry>,
    expected_subtree: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> io::Result<()> {
    let relative_path = relative.to_path_buf()?;
    let target = target_root.join(&relative_path);
    let backup = run_root.join(&relative_path);
    match snapshot_local_entry(vfs, &target) {
        Ok(actual) if rollback_entry_matches(&actual, after) => quarantine_rollback_entry(
            vfs,
            target_root,
            run_root,
            relative,
            after,
            expected_subtree,
        ),
        Ok(actual)
            if before.is_some_and(|expected| rollback_entry_matches(&actual, expected))
                && snapshot_local_entry(vfs, &backup)
                    .is_ok_and(|moved| rollback_entry_matches(&moved, after)) =>
        {
            sync_rollback_parents(vfs, &target, &backup)?;
            verify_quarantined_subtree(vfs, &backup, relative, expected_subtree)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let moved = snapshot_local_entry(vfs, &backup)?;
            if rollback_entry_matches(&moved, after) {
                sync_rollback_parents(vfs, &target, &backup)?;
                verify_quarantined_subtree(vfs, &backup, relative, expected_subtree)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Rollback-Quarantäne enthält nicht den gebundenen Nachzustand",
                ))
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Rollback-Ziel entspricht weder Vor- noch Nachzustand",
        )),
        Err(error) => Err(error),
    }
}

fn ensure_previous_restored(
    vfs: &dyn Vfs,
    target_root: &Path,
    side: EndpointSide,
    relative: &BinaryRelativePath,
    before: &SnapshotEntry,
    projection: &BidirectionalProjectionJournal,
    quarantine_leases: &[HeldRollbackQuarantine],
) -> io::Result<()> {
    let relative_path = relative.to_path_buf()?;
    let target = target_root.join(&relative_path);
    match snapshot_local_entry(vfs, &target) {
        Ok(actual) if rollback_entry_matches(&actual, before) => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Rollback-Wiederherstellungsziel enthält eine fremde Datei",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let run_root = projection
        .quarantine
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| invalid_data("Vorheriger Zustand besitzt keine Quarantänebindung"))?;
    let base = run_root
        .parent()
        .ok_or_else(|| invalid_data("Vorwärtsquarantäne besitzt keinen Basisordner"))?;
    let run_id = run_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("Vorwärtsquarantäne besitzt keine Lauf-ID"))?;
    let relative_text = relative_path
        .to_str()
        .ok_or_else(|| invalid_data("Rollback-Pfad ist nicht verlustfrei UTF-8"))?;
    let lease = held_rollback_quarantine(quarantine_leases, side, &run_root)?;
    crate::quarantine::restore_quarantine_entry_for_target_with_lease(
        lease,
        vfs,
        base,
        run_id,
        relative_text,
        target_root,
    )?;
    let restored = snapshot_local_entry(vfs, &target)
        .map_err(|error| bidirectional_effect_error(error, &[&target, &run_root]))?;
    if !rollback_entry_matches(&restored, before) {
        return Err(bidirectional_effect_error(
            io::Error::new(
                io::ErrorKind::Interrupted,
                "Wiederhergestellte Datei entspricht nicht dem gebundenen Vorzustand",
            ),
            &[&target, &run_root],
        ));
    }
    Ok(())
}

fn restore_type_previous_to_staging(
    vfs: &dyn Vfs,
    target_root: &Path,
    side: EndpointSide,
    projection: &BidirectionalProjectionJournal,
    before: &SnapshotEntry,
    expected_before: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    quarantine_leases: &[HeldRollbackQuarantine],
) -> io::Result<()> {
    let BidirectionalProjectionKind::TypeSwap {
        path, staging_path, ..
    } = &projection.projection
    else {
        return Err(invalid_data(
            "Typwechsel-Staging-Restore gehört zu keiner Typwechselprojektion",
        ));
    };
    let original_relative = path.to_path_buf()?;
    let staging_relative = staging_path.to_path_buf()?;
    let staging = target_root.join(&staging_relative);
    if vfs.exists(&staging) {
        let actual = snapshot_local_entry(vfs, &staging)?;
        if rollback_entry_matches(&actual, before) {
            return verify_quarantined_subtree(vfs, &staging, path, expected_before);
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Typwechsel-Stagingname enthält vor dem Rollback einen fremden Eintrag",
        ));
    }
    let run_root = projection
        .quarantine
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| invalid_data("Typwechsel-Vorzustand besitzt keine Quarantänebindung"))?;
    let base = run_root
        .parent()
        .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keinen Basisordner"))?;
    let run_id = run_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keine Lauf-ID"))?;
    let original_text = original_relative
        .to_str()
        .ok_or_else(|| invalid_data("Typwechselpfad ist nicht verlustfrei UTF-8"))?;
    let lease = held_rollback_quarantine(quarantine_leases, side, &run_root)?;
    crate::quarantine::restore_quarantine_entry_to_for_target_with_lease(
        lease,
        vfs,
        base,
        run_id,
        original_text,
        &staging_relative,
        target_root,
    )?;
    let restored = snapshot_local_entry(vfs, &staging)
        .map_err(|error| bidirectional_effect_error(error, &[&staging, &run_root]))?;
    if !rollback_entry_matches(&restored, before) {
        return Err(bidirectional_effect_error(
            io::Error::new(
                io::ErrorKind::Interrupted,
                "Typwechsel-Stagingversion entspricht nicht dem gebundenen Vorzustand",
            ),
            &[&staging, &run_root],
        ));
    }
    verify_quarantined_subtree(vfs, &staging, path, expected_before)
}

fn type_swap_before_after_maps(
    journal: &BidirectionalRunJournal,
    ordinal: usize,
    side: EndpointSide,
) -> io::Result<(
    BTreeMap<BinaryRelativePath, SnapshotEntry>,
    BTreeMap<BinaryRelativePath, SnapshotEntry>,
)> {
    let (before_left, before_right) = expected_maps_after_prefix(journal, ordinal)?;
    let (after_left, after_right) = expected_maps_after_prefix(journal, ordinal + 1)?;
    Ok(match side {
        EndpointSide::Left => (before_left, after_left),
        EndpointSide::Right => (before_right, after_right),
    })
}

fn ensure_rollback_quarantine(
    store: &BaselineStore,
    journal: &mut BidirectionalRunJournal,
    side: EndpointSide,
    vfs: &dyn Vfs,
    target_parent: &Path,
    target_root: &Path,
    quarantine_leases: &mut Vec<HeldRollbackQuarantine>,
) -> io::Result<PathBuf> {
    if let Some(existing) = journal
        .rollback_quarantines
        .iter()
        .find(|item| item.side == side)
    {
        let base = existing
            .run_root
            .parent()
            .ok_or_else(|| invalid_data("Rollback-Quarantäne besitzt keinen Basisordner"))?;
        let id = existing
            .run_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid_data("Rollback-Quarantäne besitzt keine Lauf-ID"))?;
        crate::quarantine::list_quarantine_entries_for_target(
            vfs,
            base,
            id,
            &None,
            Some(target_root),
        )?;
        return Ok(existing.run_root.clone());
    }
    let base = target_parent.join(".doppeldecker-quarantine");
    crate::real_mode::ensure_quarantine_root(vfs, target_parent, &base)?;
    let run_root = crate::real_mode::reserve_quarantine_run_name(vfs, &base)?;
    journal
        .rollback_quarantines
        .push(BidirectionalRollbackQuarantine {
            side,
            run_root: run_root.clone(),
        });
    journal
        .rollback_quarantines
        .sort_by_key(|item| match item.side {
            EndpointSide::Left => 0,
            EndpointSide::Right => 1,
        });
    journal.refresh_digest()?;
    store.save_run_journal(journal)?;
    let lease = crate::quarantine::start_quarantine_run(vfs, &run_root, target_root)?;
    quarantine_leases.push(HeldRollbackQuarantine {
        side,
        run_root: run_root.clone(),
        lease,
    });
    Ok(run_root)
}

fn quarantine_rollback_entry(
    vfs: &dyn Vfs,
    target_root: &Path,
    run_root: &Path,
    relative: &BinaryRelativePath,
    expected: &SnapshotEntry,
    expected_subtree: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> io::Result<()> {
    let relative_path = relative.to_path_buf()?;
    let target = target_root.join(&relative_path);
    let actual = snapshot_local_entry(vfs, &target)?;
    if !rollback_entry_matches(&actual, expected) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Rollback-Zieldatei änderte sich vor dem Quarantäne-Claim",
        ));
    }
    let backup = run_root.join(&relative_path);
    if vfs.exists(&backup) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Rollback-Quarantänepfad existiert bereits",
        ));
    }
    if let Some(parent) = backup.parent() {
        vfs.create_dir_all(parent)?;
    }
    crate::quarantine::bind_quarantine_run_identity(vfs, run_root)?;
    crate::quarantine::record_quarantine_intent(
        vfs,
        run_root,
        &relative_path,
        expected.kind == SnapshotEntryKind::Directory,
        crate::quarantine::QuarantineReason::MirrorRemoval,
    )?;
    vfs.rename_noreplace(&target, &backup)
        .map_err(|error| bidirectional_effect_error(error, &[&target, &backup]))?;
    (|| {
        sync_rollback_parents(vfs, &target, &backup)?;
        let moved = snapshot_local_entry(vfs, &backup)?;
        if !rollback_entry_matches(&moved, expected) {
            vfs.rename_noreplace(&backup, &target)?;
            sync_rollback_parents(vfs, &backup, &target)?;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Rollback-Claim traf nicht mehr den gebundenen Eintrag",
            ));
        }
        verify_quarantined_subtree(vfs, &backup, relative, expected_subtree)
    })()
    .map_err(|error| bidirectional_effect_error(error, &[&target, &backup]))
}

fn verify_quarantined_subtree(
    vfs: &dyn Vfs,
    backup: &Path,
    logical_root: &BinaryRelativePath,
    expected: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> io::Result<()> {
    let mut actual = BTreeMap::new();
    collect_local_subtree(vfs, backup, logical_root, &mut actual)?;
    let expected: BTreeMap<_, _> = expected
        .iter()
        .filter(|(path, _)| *path == logical_root || binary_path_contains(logical_root, path))
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect();
    if expected.len() != actual.len()
        || expected.iter().any(|(path, expected)| {
            !actual
                .get(path)
                .is_some_and(|actual| rollback_entry_matches(actual, expected))
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Rollback-Quarantäne enthält nicht den vollständig gebundenen Unterbaum",
        ));
    }
    Ok(())
}

fn collect_local_subtree(
    vfs: &dyn Vfs,
    path: &Path,
    logical: &BinaryRelativePath,
    output: &mut BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> io::Result<()> {
    let entry = snapshot_local_entry(vfs, path)?;
    let is_dir = entry.kind == SnapshotEntryKind::Directory;
    if output.insert(logical.clone(), entry).is_some() {
        return Err(invalid_data(
            "Rollback-Unterbaum enthält einen doppelten Pfad",
        ));
    }
    if !is_dir {
        return Ok(());
    }
    let listing = vfs.list_complete(path)?;
    if !listing.complete {
        return Err(invalid_data("Rollback-Unterbaumlisting ist unvollständig"));
    }
    let mut seen = BTreeSet::new();
    for listed in listing.entries {
        validate_listed_entry(&listed)?;
        if !seen.insert(listed.raw_name.clone()) {
            return Err(invalid_data(
                "Rollback-Unterbaum enthält keinen eindeutig dekodierbaren Namen",
            ));
        }
        let mut components = logical.0.clone();
        components.push(listed.raw_name);
        let child_logical = BinaryRelativePath(components);
        child_logical.validate()?;
        collect_local_subtree(vfs, &path.join(&listed.entry.name), &child_logical, output)?;
    }
    Ok(())
}

// Nur der im Journal ausdrücklich ausgelassene Modus bleibt beim lokalen
// Readback ungebunden. Inhalt, Typ, Größe und gebundene Modi werden weiter geprüft.
fn rollback_entry_matches(actual: &SnapshotEntry, expected: &SnapshotEntry) -> bool {
    let mut actual = actual.clone();
    if expected.metadata_digest == portable_metadata_digest(None) {
        actual.metadata_digest = expected.metadata_digest;
    }
    actual.semantically_equal(expected)
}

fn snapshot_local_entry(vfs: &dyn Vfs, path: &Path) -> io::Result<SnapshotEntry> {
    let entry = vfs.stat(path)?;
    if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
        return Err(invalid_data(
            "Rollback-Pfad ist weder reguläre Datei noch Verzeichnis",
        ));
    }
    let mut snapshot = SnapshotEntry {
        kind: if entry.is_dir {
            SnapshotEntryKind::Directory
        } else {
            SnapshotEntryKind::File
        },
        size: if entry.is_dir { 0 } else { entry.size },
        mtime: entry.mtime,
        content_digest: if entry.is_file {
            Some(hash_file_stable(vfs, path, &entry)?)
        } else {
            None
        },
        metadata_digest: portable_metadata_digest(entry.mode),
        provider_id: None,
        entry_digest: [0; 32],
    };
    snapshot.entry_digest = snapshot.canonical_digest();
    Ok(snapshot)
}

fn bidirectional_effect_error(error: io::Error, paths: &[&Path]) -> io::Error {
    crate::error::with_recovery_context(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!("Bidirektionale Teilwirkung ist nicht dauerhaft bestätigt: {error}"),
        ),
        &[&error],
        paths,
    )
}

fn sync_rollback_parents(vfs: &dyn Vfs, from: &Path, to: &Path) -> io::Result<()> {
    let from_parent = from
        .parent()
        .ok_or_else(|| invalid_input("Rollback-Quelle besitzt keinen Elternordner"))?;
    let to_parent = to
        .parent()
        .ok_or_else(|| invalid_input("Rollback-Ziel besitzt keinen Elternordner"))?;
    (|| {
        vfs.sync_dir(from_parent)?;
        if to_parent != from_parent {
            vfs.sync_dir(to_parent)?;
        }
        Ok(())
    })()
    .map_err(|error| bidirectional_effect_error(error, &[from, to]))
}

fn sync_recovered_rollback_unit(
    vfs: &dyn Vfs,
    root: &Path,
    journal: &BidirectionalRunJournal,
    ordinal: usize,
    delta: &ProjectionDelta,
) -> io::Result<()> {
    let relative = delta.path.to_path_buf()?;
    let target = root.join(&relative);
    // Ein sichtbarer Nachzustand darf nach einem fsync-Fehler erst dann eine
    // Journalphase abschließen, wenn alle möglichen Rename-Eltern erneut halten.
    if metadata_only_delta(delta) {
        let result = match delta.before.as_ref().map(|entry| entry.kind) {
            Some(SnapshotEntryKind::Directory) => vfs.sync_dir(&target),
            _ => vfs.sync_file(&target),
        };
        result.map_err(|error| bidirectional_effect_error(error, &[&target]))?;
    }
    sync_rollback_parents(vfs, &target, &target)?;
    if let Some(run) = journal.rollback_quarantines.iter().find(|run| {
        run.side == delta.side
            && delta.after.is_some()
            && !metadata_only_delta(delta)
            && !type_mismatch_delta(delta)
    }) {
        sync_rollback_parents(vfs, &target, &run.run_root.join(&relative))?;
    }
    if let Some(run) = journal.projections[ordinal]
        .quarantine
        .as_ref()
        .filter(|_| delta.before.is_some() && !metadata_only_delta(delta))
    {
        sync_rollback_parents(vfs, &target, &Path::new(run).join(&relative))?;
    }
    Ok(())
}

struct LocalProjectionContext {
    left_vfs: Arc<dyn Vfs>,
    right_vfs: Arc<dyn Vfs>,
    left_endpoint: crate::ProviderEndpoint,
    right_endpoint: crate::ProviderEndpoint,
    left_root: PathBuf,
    right_root: PathBuf,
}

fn additive_projection_mode(target: &crate::ProviderEndpoint) -> io::Result<crate::SyncMode> {
    if target.kind() == crate::ProviderKind::Local {
        return Ok(crate::SyncMode::AddOnly);
    }
    // Eine bidirektionale Addition muss nach einem Teilcommit wieder zurückrollbar
    // sein. Nicht die Providerart, sondern die konkrete Parken-/Restore-Probe
    // entscheidet deshalb, ob der Update-Vertrag verlangt und angeboten wird.
    let capabilities = target.probe_delete_capabilities()?;
    if capabilities.quarantine {
        Ok(crate::SyncMode::Update)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Bidirektionale Addition benötigt Doppeldecker-Quarantäne für Recovery: {}",
                capabilities.blockers.join("; ")
            ),
        ))
    }
}

fn prepare_projection_runs(
    plan: &BidirectionalPlan,
    context: &LocalProjectionContext,
    allowed_ordinals: Option<&BTreeSet<usize>>,
    allow_deferred_deletions: bool,
) -> io::Result<(Vec<crate::PreparedProviderRealRun>, Vec<usize>, Vec<usize>)> {
    let projections = planned_projections(plan)?;
    let first_type_swap = projections.iter().position(|entry| {
        matches!(
            &entry.projection,
            BidirectionalProjectionKind::TypeSwap { .. }
        )
    });
    let has_prior_copy = projections.iter().any(|entry| {
        !matches!(
            &entry.projection,
            BidirectionalProjectionKind::Delete { .. }
        )
    });
    let mut runs = Vec::new();
    let mut ordinals = Vec::new();
    let mut deferred = Vec::new();
    for entry in projections {
        let ordinal = entry.ordinal as usize;
        if allowed_ordinals.is_some_and(|allowed| !allowed.contains(&ordinal)) {
            continue;
        }
        if matches!(
            &entry.projection,
            BidirectionalProjectionKind::TypeSwap { .. }
        ) {
            // Der atomare Swap hat einen eigenen Journalzustandsautomaten und darf
            // nicht als gewöhnlicher Einweg-Providerlauf vorbereitet werden.
            deferred.push(ordinal);
            continue;
        }
        if allow_deferred_deletions && first_type_swap.is_some_and(|first| ordinal > first) {
            // Nach dem ersten Typcommit muss die globale Journalreihenfolge erhalten
            // bleiben; spätere normale Ersetzungen werden deshalb ebenfalls erst
            // nach dem atomaren Swap frisch vorbereitet.
            deferred.push(ordinal);
            continue;
        }
        if allow_deferred_deletions
            && has_prior_copy
            && matches!(
                &entry.projection,
                BidirectionalProjectionKind::Delete { .. }
            )
        {
            deferred.push(ordinal);
            continue;
        }
        let prepared = match entry.projection {
            BidirectionalProjectionKind::ConflictCopy {
                source_path,
                target_path,
                source_side,
                target_side,
                subtree,
            } => {
                let source_relative = source_path.to_path_buf()?;
                let target_relative = target_path.to_path_buf()?;
                let mut selected = BTreeSet::from([target_relative.clone()]);
                let mut aliases = BTreeMap::from([(target_relative, source_relative)]);
                for entry in subtree {
                    let source = entry.source_path.to_path_buf()?;
                    let target = entry.target_path.to_path_buf()?;
                    selected.insert(target.clone());
                    aliases.insert(target, source);
                }
                let (source_vfs, source_root) = context.endpoint(source_side);
                let (target_vfs, target_root, target_endpoint) =
                    context.target_endpoint(target_side);
                let projection = Arc::new(LocalProjectedVfs::new_with_aliases(
                    source_vfs,
                    source_root,
                    target_vfs.clone(),
                    target_root.clone(),
                    selected,
                    BTreeSet::new(),
                    aliases,
                )?);
                let mode = additive_projection_mode(&target_endpoint)?;
                crate::remote_real_mode::prepare_projected_provider_transfer(
                    projection,
                    target_root.clone(),
                    target_endpoint,
                    crate::SyncOptions {
                        mode,
                        compare: crate::Compare::ContentHash,
                        metadata: MetadataProfile::Portable,
                        mtime_tolerance: 2,
                        exclude: Vec::new(),
                    },
                )?
            }
            BidirectionalProjectionKind::Copy {
                direction,
                replacement,
                paths,
            } => {
                let selected: BTreeSet<_> = paths
                    .iter()
                    .map(BinaryRelativePath::to_path_buf)
                    .collect::<io::Result<_>>()?;
                let source_side = match direction {
                    SyncDirection::LeftToRight => EndpointSide::Left,
                    SyncDirection::RightToLeft => EndpointSide::Right,
                };
                let target_side = match direction {
                    SyncDirection::LeftToRight => EndpointSide::Right,
                    SyncDirection::RightToLeft => EndpointSide::Left,
                };
                let (source_vfs, source_root) = context.endpoint(source_side);
                let (target_vfs, target_root, target_endpoint) =
                    context.target_endpoint(target_side);
                let projection = Arc::new(LocalProjectedVfs::new(
                    source_vfs,
                    source_root,
                    target_vfs.clone(),
                    target_root.clone(),
                    selected,
                    BTreeSet::new(),
                )?);
                let mode = if replacement {
                    crate::SyncMode::Mirror
                } else {
                    additive_projection_mode(&target_endpoint)?
                };
                crate::remote_real_mode::prepare_projected_provider_transfer(
                    projection,
                    target_root.clone(),
                    target_endpoint,
                    crate::SyncOptions {
                        mode,
                        compare: crate::Compare::ContentHash,
                        metadata: MetadataProfile::Portable,
                        mtime_tolerance: 2,
                        exclude: Vec::new(),
                    },
                )?
            }
            BidirectionalProjectionKind::Delete { direction, paths } => {
                let deleted: BTreeSet<_> = paths
                    .iter()
                    .map(BinaryRelativePath::to_path_buf)
                    .collect::<io::Result<_>>()?;
                let source_side = match direction {
                    SyncDirection::LeftToRight => EndpointSide::Left,
                    SyncDirection::RightToLeft => EndpointSide::Right,
                };
                let target_side = match direction {
                    SyncDirection::LeftToRight => EndpointSide::Right,
                    SyncDirection::RightToLeft => EndpointSide::Left,
                };
                let (source_vfs, source_root) = context.endpoint(source_side);
                let (target_vfs, target_root, target_endpoint) =
                    context.target_endpoint(target_side);
                let projection = Arc::new(LocalProjectedVfs::new(
                    source_vfs,
                    source_root,
                    target_vfs.clone(),
                    target_root.clone(),
                    BTreeSet::new(),
                    deleted,
                )?);
                crate::remote_real_mode::prepare_projected_provider_transfer(
                    projection,
                    target_root.clone(),
                    target_endpoint,
                    crate::SyncOptions {
                        mode: crate::SyncMode::Mirror,
                        compare: crate::Compare::ContentHash,
                        metadata: MetadataProfile::Portable,
                        mtime_tolerance: 2,
                        exclude: Vec::new(),
                    },
                )?
            }
            BidirectionalProjectionKind::TypeSwap { .. } => {
                return Err(invalid_data(
                    "Typwechselprojektion gelangte in den Einweg-Vorbereiter",
                ));
            }
        };
        runs.push(prepared);
        ordinals.push(ordinal);
    }
    Ok((runs, ordinals, deferred))
}

impl LocalProjectionContext {
    fn new(
        left_vfs: Arc<dyn Vfs>,
        right_vfs: Arc<dyn Vfs>,
        left_endpoint: crate::ProviderEndpoint,
        right_endpoint: crate::ProviderEndpoint,
        left_root: PathBuf,
        right_root: PathBuf,
    ) -> io::Result<Self> {
        if left_endpoint.root() != left_root || right_endpoint.root() != right_root {
            return Err(invalid_input(
                "Projektionsendpunkt passt nicht zur gebundenen Wurzel",
            ));
        }
        Ok(Self {
            left_vfs,
            right_vfs,
            left_endpoint,
            right_endpoint,
            left_root,
            right_root,
        })
    }

    fn local(
        left_vfs: Arc<dyn Vfs>,
        right_vfs: Arc<dyn Vfs>,
        left_root: PathBuf,
        right_root: PathBuf,
    ) -> io::Result<Self> {
        Self::new(
            left_vfs,
            right_vfs,
            crate::ProviderEndpoint::local(&left_root)?,
            crate::ProviderEndpoint::local(&right_root)?,
            left_root,
            right_root,
        )
    }

    fn endpoint(&self, side: EndpointSide) -> (Arc<dyn Vfs>, PathBuf) {
        match side {
            EndpointSide::Left => (self.left_vfs.clone(), self.left_root.clone()),
            EndpointSide::Right => (self.right_vfs.clone(), self.right_root.clone()),
        }
    }

    fn target_endpoint(
        &self,
        side: EndpointSide,
    ) -> (Arc<dyn Vfs>, PathBuf, crate::ProviderEndpoint) {
        match side {
            EndpointSide::Left => (
                self.left_vfs.clone(),
                self.left_root.clone(),
                self.left_endpoint.clone(),
            ),
            EndpointSide::Right => (
                self.right_vfs.clone(),
                self.right_root.clone(),
                self.right_endpoint.clone(),
            ),
        }
    }
}

fn final_local_snapshots(
    left_vfs: &dyn Vfs,
    left_root: &Path,
    right_vfs: &dyn Vfs,
    right_root: &Path,
) -> io::Result<(TreeSnapshot, TreeSnapshot)> {
    let mut left = build_tree_snapshot(
        left_vfs,
        left_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    let mut right = build_tree_snapshot(
        right_vfs,
        right_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    normalize_pair_portable_metadata(&mut left, &mut right);
    Ok((left, right))
}

/// POSIX-Modi gehören nur dann zur bidirektionalen Baseline, wenn beide Endpunkte
/// sie zuverlässig lesen und schreiben können. FTPS, WebDAV und das SMB-Profil ohne
/// POSIX-Erweiterungen besitzen dafür keinen standardisierten Vertrag; bei solchen
/// Paaren wird deshalb auf beiden Seiten derselbe explizite „nicht gebunden“-Digest
/// verwendet.
fn normalize_pair_portable_metadata(left: &mut TreeSnapshot, right: &mut TreeSnapshot) {
    let omits_mode = |identity: &RootIdentity| {
        matches!(
            identity,
            RootIdentity::Ftps(_) | RootIdentity::WebDav(_) | RootIdentity::Smb(_)
        )
    };
    if !omits_mode(&left.root_identity) && !omits_mode(&right.root_identity) {
        return;
    }
    for snapshot in [left, right] {
        for record in &mut snapshot.entries {
            record.entry.metadata_digest = portable_metadata_digest(None);
            record.entry.entry_digest = record.entry.canonical_digest();
        }
        snapshot.snapshot_digest = snapshot_digest(snapshot.metadata_profile, &snapshot.entries);
    }
}

fn planned_projections(
    plan: &BidirectionalPlan,
) -> io::Result<Vec<BidirectionalProjectionJournal>> {
    let mut kinds = Vec::new();
    for action in &plan.actions {
        if let BidirectionalAction::ConflictCopy {
            source_path,
            target_path,
            source_side,
            target_side,
            subtree,
            ..
        } = action
        {
            kinds.push(BidirectionalProjectionKind::ConflictCopy {
                source_path: source_path.clone(),
                target_path: target_path.clone(),
                source_side: *source_side,
                target_side: *target_side,
                subtree: subtree.clone(),
            });
        }
    }
    // Der vollständige Gewinner-Unterbaum muss wie jede andere additive Kopie vor
    // irgendeiner Ersetzung dauerhaft unter seinem gebundenen Stagingnamen liegen.
    for action in &plan.actions {
        if let BidirectionalAction::TypeSwap {
            path,
            staging_path,
            direction,
            subtree,
            ..
        } = action
        {
            let (source_side, target_side) = direction_sides(*direction);
            kinds.push(BidirectionalProjectionKind::ConflictCopy {
                source_path: path.clone(),
                target_path: staging_path.clone(),
                source_side,
                target_side,
                subtree: subtree.clone(),
            });
        }
    }
    for direction in [SyncDirection::LeftToRight, SyncDirection::RightToLeft] {
        let replacement = false;
        let paths = selected_binary_paths(plan, direction, replacement);
        if !paths.is_empty() {
            kinds.push(BidirectionalProjectionKind::Copy {
                direction,
                replacement,
                paths,
            });
        }
    }
    // Der atomare Typcommit folgt allen additiven Kopien, aber liegt vor normalen
    // Ersetzungen. So sieht eine Verzeichnis-Metadatenprojektion darunter bereits
    // den entschiedenen Typ und kann nicht am alten Kindtyp hängenbleiben.
    for action in &plan.actions {
        if let BidirectionalAction::TypeSwap {
            path,
            staging_path,
            direction,
            ..
        } = action
        {
            kinds.push(BidirectionalProjectionKind::TypeSwap {
                path: path.clone(),
                staging_path: staging_path.clone(),
                direction: *direction,
            });
        }
    }
    for direction in [SyncDirection::LeftToRight, SyncDirection::RightToLeft] {
        let replacement = true;
        let paths = selected_binary_paths(plan, direction, replacement);
        if !paths.is_empty() {
            kinds.push(BidirectionalProjectionKind::Copy {
                direction,
                replacement,
                paths,
            });
        }
    }
    for direction in [SyncDirection::LeftToRight, SyncDirection::RightToLeft] {
        let paths = deleted_binary_paths(plan, direction);
        if !paths.is_empty() {
            kinds.push(BidirectionalProjectionKind::Delete { direction, paths });
        }
    }
    kinds
        .into_iter()
        .enumerate()
        .map(|(index, projection)| {
            validate_projection_kind(&projection)?;
            Ok(BidirectionalProjectionJournal {
                ordinal: index as u64,
                projection,
                state: BidirectionalProjectionState::Prepared,
                quarantine: None,
            })
        })
        .collect()
}

fn validate_projection_kind(projection: &BidirectionalProjectionKind) -> io::Result<()> {
    let paths: Vec<&BinaryRelativePath> = match projection {
        BidirectionalProjectionKind::ConflictCopy {
            source_path,
            target_path,
            subtree,
            ..
        } => {
            let mut paths = vec![source_path, target_path];
            for entry in subtree {
                paths.push(&entry.source_path);
                paths.push(&entry.target_path);
            }
            paths
        }
        BidirectionalProjectionKind::Copy { paths, .. }
        | BidirectionalProjectionKind::Delete { paths, .. } => {
            if paths.is_empty() {
                return Err(invalid_data("Journalprojektion besitzt keine Pfade"));
            }
            paths.iter().collect()
        }
        BidirectionalProjectionKind::TypeSwap {
            path, staging_path, ..
        } => vec![path, staging_path],
    };
    for path in paths {
        path.validate()?;
    }
    Ok(())
}

fn direction_sides(direction: SyncDirection) -> (EndpointSide, EndpointSide) {
    match direction {
        SyncDirection::LeftToRight => (EndpointSide::Left, EndpointSide::Right),
        SyncDirection::RightToLeft => (EndpointSide::Right, EndpointSide::Left),
    }
}

fn selected_binary_paths(
    plan: &BidirectionalPlan,
    direction: SyncDirection,
    replacement: bool,
) -> Vec<BinaryRelativePath> {
    plan.actions
        .iter()
        .filter_map(|action| match action {
            BidirectionalAction::Copy {
                path,
                direction: actual,
                expected_target,
                ..
            } if *actual == direction && expected_target.is_some() == replacement => {
                Some(path.clone())
            }
            _ => None,
        })
        .collect()
}

fn deleted_binary_paths(
    plan: &BidirectionalPlan,
    direction: SyncDirection,
) -> Vec<BinaryRelativePath> {
    plan.actions
        .iter()
        .filter_map(|action| match action {
            BidirectionalAction::Delete {
                path,
                direction: actual,
                ..
            } if *actual == direction => Some(path.clone()),
            _ => None,
        })
        .collect()
}

fn local_stage_blocker(plan: &BidirectionalPlan) -> Option<String> {
    if plan.conflicts > 0 {
        return Some(format!(
            "{} Konflikt(e) benötigen eine ausdrückliche Entscheidung",
            plan.conflicts
        ));
    }
    if let Some(reason) = initial_one_sided_skip_blocker(plan) {
        return Some(reason);
    }
    local_type_swap_stage_blocker(plan)
}

fn local_type_swap_stage_blocker(plan: &BidirectionalPlan) -> Option<String> {
    let type_swaps = plan
        .actions
        .iter()
        .filter(|action| matches!(action, BidirectionalAction::TypeSwap { .. }))
        .count();
    if type_swaps > 100 {
        return Some(format!(
            "{type_swaps} Typwechsel überschreiten die feste Grenze von 100 Ersetzungen"
        ));
    }
    None
}

#[derive(Default)]
struct ProviderEffectCapabilities {
    additions: bool,
    replacements: bool,
    directory_replacements: bool,
    deletions: bool,
    type_swaps: bool,
    blocker: String,
}

fn provider_effect_capabilities(
    endpoint: &crate::ProviderEndpoint,
    plan: &BidirectionalPlan,
    side: EndpointSide,
) -> ProviderEffectCapabilities {
    // Eine leere Baseline oder reine Quelle braucht keine Schreibfähigkeit.
    // Gerade FTPS-Proben können eigene Namen anlegen und bei einem unklaren
    // Ausgang erhalten. Nach dem Snapshot würden sie dann selbst berechtigt die
    // Driftabsage auslösen. Auch Resume und Rollback verändern nur die ursprüngliche
    // Zielseite; alle dort benötigten Garantien bleiben unverändert erforderlich.
    let mutates = plan.actions.iter().any(|action| match action {
        BidirectionalAction::ConflictCopy { target_side, .. } => *target_side == side,
        BidirectionalAction::Copy { direction, .. }
        | BidirectionalAction::Delete { direction, .. }
        | BidirectionalAction::TypeSwap { direction, .. } => match direction {
            SyncDirection::LeftToRight => side == EndpointSide::Right,
            SyncDirection::RightToLeft => side == EndpointSide::Left,
        },
        BidirectionalAction::Conflict { .. }
        | BidirectionalAction::Converged { .. }
        | BidirectionalAction::Tombstone { .. }
        | BidirectionalAction::SkippedConflict { .. } => false,
    });
    if !mutates {
        return ProviderEffectCapabilities::default();
    }
    if endpoint.kind() == crate::ProviderKind::Local {
        return ProviderEffectCapabilities {
            additions: true,
            replacements: true,
            directory_replacements: true,
            deletions: true,
            type_swaps: true,
            blocker: String::new(),
        };
    }
    let delete = endpoint.probe_delete_capabilities();
    let mut reasons = Vec::new();
    let quarantine = match delete {
        Ok(value) => {
            reasons.extend(value.blockers);
            value.quarantine
        }
        Err(error) => {
            reasons.push(format!("Quarantäne: {error}"));
            false
        }
    };
    // Die passive Absage ist vor jeder schreibenden Dateiprobe bekannt. Ohne
    // Rückrollvertrag benötigt auch eine Addition keine mutierende Vorbereitung.
    if !quarantine {
        return ProviderEffectCapabilities {
            blocker: reasons.join("; "),
            ..ProviderEffectCapabilities::default()
        };
    }
    let (copy_new, rename_noreplace) = match endpoint.probe_file_target_capabilities() {
        Ok(value) => {
            reasons.extend(value.blockers);
            (value.copy_new, value.rename_noreplace)
        }
        Err(error) => {
            reasons.push(format!("Dateiwirkung: {error}"));
            (false, false)
        }
    };
    // Jede bidirektionale Mutation muss nach einem Teilcommit rückrollbar sein.
    // Additionen benötigen daher ebenso Quarantäne wie Ersetzungen und Löschungen.
    let transfer = endpoint.current_transfer_capabilities();
    let additions = copy_new && quarantine;
    let replacements = additions && rename_noreplace && transfer.update;
    ProviderEffectCapabilities {
        additions,
        replacements,
        directory_replacements: replacements && transfer.durable_directory_flush,
        deletions: quarantine && transfer.mirror,
        type_swaps: replacements && transfer.durable_directory_flush,
        blocker: if reasons.is_empty() {
            "Verbindung besitzt keinen vollständigen Recoveryvertrag".into()
        } else {
            reasons.join("; ")
        },
    }
}

fn provider_stage_blocker(
    plan: &BidirectionalPlan,
    left_snapshot: &TreeSnapshot,
    right_snapshot: &TreeSnapshot,
    left: &ProviderEffectCapabilities,
    right: &ProviderEffectCapabilities,
) -> Option<String> {
    if let Some(reason) = provider_plan_stage_blocker(plan) {
        return Some(reason);
    }
    let target = |direction: SyncDirection| match direction {
        SyncDirection::LeftToRight => right,
        SyncDirection::RightToLeft => left,
    };
    let side = |side: EndpointSide| match side {
        EndpointSide::Left => left,
        EndpointSide::Right => right,
    };
    let left_entries = left_snapshot.map();
    let right_entries = right_snapshot.map();
    let blocked = plan.actions.iter().find_map(|action| match action {
        BidirectionalAction::ConflictCopy { target_side, .. } => {
            (!side(*target_side).additions).then_some(side(*target_side))
        }
        BidirectionalAction::Copy {
            path,
            direction,
            expected_target: Some(_),
            ..
        } => {
            let source = match direction {
                SyncDirection::LeftToRight => left_entries.get(path).copied(),
                SyncDirection::RightToLeft => right_entries.get(path).copied(),
            };
            let existing = match direction {
                SyncDirection::LeftToRight => right_entries.get(path).copied(),
                SyncDirection::RightToLeft => left_entries.get(path).copied(),
            };
            let needs_directory_durability = matches!(
                (source, existing),
                (
                    Some(SnapshotEntry {
                        kind: SnapshotEntryKind::Directory,
                        ..
                    }),
                    _
                ) | (
                    _,
                    Some(SnapshotEntry {
                        kind: SnapshotEntryKind::Directory,
                        ..
                    })
                )
            );
            let permitted = if needs_directory_durability {
                target(*direction).directory_replacements
            } else {
                target(*direction).replacements
            };
            (!permitted).then_some(target(*direction))
        }
        BidirectionalAction::Copy {
            direction,
            expected_target: None,
            ..
        } => (!target(*direction).additions).then_some(target(*direction)),
        BidirectionalAction::Delete { direction, .. } => {
            (!target(*direction).deletions).then_some(target(*direction))
        }
        BidirectionalAction::TypeSwap { direction, .. } => {
            (!target(*direction).type_swaps).then_some(target(*direction))
        }
        BidirectionalAction::Conflict { .. } => Some(left),
        BidirectionalAction::Converged { .. }
        | BidirectionalAction::Tombstone { .. }
        | BidirectionalAction::SkippedConflict { .. } => None,
    });
    blocked.map(|capabilities| {
        format!(
            "Mindestens eine Zielwirkung ist für diese konkrete Verbindung gesperrt: {}",
            capabilities.blocker
        )
    })
}

fn provider_plan_stage_blocker(plan: &BidirectionalPlan) -> Option<String> {
    if plan.conflicts > 0 {
        return Some(format!(
            "{} Konflikt(e) benötigen eine vollständige Entscheidung",
            plan.conflicts
        ));
    }
    if let Some(reason) = initial_one_sided_skip_blocker(plan) {
        return Some(reason);
    }
    let replacements = plan
        .actions
        .iter()
        .filter(|action| {
            matches!(
                action,
                BidirectionalAction::Copy {
                    expected_target: Some(_),
                    ..
                }
            )
        })
        .count();
    if replacements > 100 {
        return Some(format!(
            "{replacements} Dateiersetzungen überschreiten die feste Grenze von 100 Ersetzungen"
        ));
    }
    None
}

fn initial_one_sided_skipped_path(plan: &BidirectionalPlan) -> Option<&BinaryRelativePath> {
    if plan.baseline_generation.is_some() || plan.baseline_digest.is_some() {
        return None;
    }
    plan.actions.iter().find_map(|action| match action {
        BidirectionalAction::SkippedConflict {
            path, left, right, ..
        } if left.is_some() != right.is_some() => Some(path),
        _ => None,
    })
}

fn initial_one_sided_skip_blocker(plan: &BidirectionalPlan) -> Option<String> {
    initial_one_sided_skipped_path(plan).map(|path| {
        format!(
            "Die Erstbaseline kann den einseitig übersprungenen Pfad {} nicht dauerhaft als offenen Konflikt speichern; links oder rechts behalten ist erforderlich",
            path.display_lossy()
        )
    })
}

fn validate_remote_pair(
    left: &crate::ProviderEndpoint,
    right: &crate::ProviderEndpoint,
) -> io::Result<()> {
    if left.kind() == crate::ProviderKind::Local && right.kind() == crate::ProviderKind::Local {
        Err(invalid_input(
            "Die Remote-Providerfreigabe benötigt mindestens einen Remote-Endpunkt",
        ))
    } else {
        Ok(())
    }
}

fn validate_sftp_pair(
    left: &crate::ProviderEndpoint,
    right: &crate::ProviderEndpoint,
) -> io::Result<()> {
    let kinds = (left.kind(), right.kind());
    if matches!(
        kinds.0,
        crate::ProviderKind::Local | crate::ProviderKind::Sftp
    ) && matches!(
        kinds.1,
        crate::ProviderKind::Local | crate::ProviderKind::Sftp
    ) && matches!(
        kinds,
        (crate::ProviderKind::Sftp, _) | (_, crate::ProviderKind::Sftp)
    ) {
        Ok(())
    } else {
        Err(invalid_input(
            "Diese Freigabestufe benötigt mindestens einen SFTP-Endpunkt und sonst nur lokale Endpunkte",
        ))
    }
}

fn validate_pair_name(name: &str) -> io::Result<()> {
    if name.trim() != name
        || name.is_empty()
        || name.len() > 80
        || name.chars().any(char::is_control)
    {
        return Err(invalid_input("Bidirektionaler Paarname ist ungültig"));
    }
    Ok(())
}

fn bidirectional_phrase(pair_name: &str) -> io::Result<String> {
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
    let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("BEIDE {pair_name} {nonce}"))
}

fn rollback_phrase(pair_name: &str) -> io::Result<String> {
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
    let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("ZURÜCK {pair_name} {nonce}"))
}

fn plan_path(
    path: &BinaryRelativePath,
    base: Option<&SnapshotEntry>,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
) -> Vec<BidirectionalAction> {
    let Some(base) = base else {
        return match (left, right) {
            (Some(left), None) => vec![copy(path, SyncDirection::LeftToRight, left, None)],
            (None, Some(right)) => vec![copy(path, SyncDirection::RightToLeft, right, None)],
            (Some(left), Some(right)) if left.semantically_equal(right) => {
                vec![BidirectionalAction::Converged {
                    path: path.clone(),
                    entry_digest: left.entry_digest,
                }]
            }
            (Some(_), Some(_)) => {
                vec![conflict(path, ConflictKind::BothChanged, None, left, right)]
            }
            (None, None) => Vec::new(),
        };
    };
    let left_same = left.is_some_and(|entry| entry.semantically_equal(base));
    let right_same = right.is_some_and(|entry| entry.semantically_equal(base));
    match (left, right) {
        (Some(_), Some(_)) if left_same && right_same => Vec::new(),
        (Some(left), Some(right)) if !left_same && right_same => {
            if left.kind != base.kind {
                vec![conflict(
                    path,
                    ConflictKind::TypeChanged,
                    Some(base.entry_digest),
                    Some(left),
                    Some(right),
                )]
            } else {
                vec![copy(path, SyncDirection::LeftToRight, left, Some(right))]
            }
        }
        (Some(left), Some(right)) if left_same && !right_same => {
            if right.kind != base.kind {
                vec![conflict(
                    path,
                    ConflictKind::TypeChanged,
                    Some(base.entry_digest),
                    Some(left),
                    Some(right),
                )]
            } else {
                vec![copy(path, SyncDirection::RightToLeft, right, Some(left))]
            }
        }
        (Some(left), Some(right)) if left.semantically_equal(right) => {
            vec![BidirectionalAction::Converged {
                path: path.clone(),
                entry_digest: left.entry_digest,
            }]
        }
        (Some(left), Some(right)) => vec![conflict(
            path,
            ConflictKind::BothChanged,
            Some(base.entry_digest),
            Some(left),
            Some(right),
        )],
        (None, None) => vec![BidirectionalAction::Tombstone {
            path: path.clone(),
            previous_entry_digest: base.entry_digest,
        }],
        (None, Some(right)) if right_same => vec![BidirectionalAction::Delete {
            path: path.clone(),
            direction: SyncDirection::LeftToRight,
            expected_target: right.entry_digest,
        }],
        (Some(left), None) if left_same => vec![BidirectionalAction::Delete {
            path: path.clone(),
            direction: SyncDirection::RightToLeft,
            expected_target: left.entry_digest,
        }],
        (None, Some(right)) => vec![conflict(
            path,
            ConflictKind::ChangedAgainstDeletion,
            Some(base.entry_digest),
            None,
            Some(right),
        )],
        (Some(left), None) => vec![conflict(
            path,
            ConflictKind::ChangedAgainstDeletion,
            Some(base.entry_digest),
            Some(left),
            None,
        )],
    }
}

fn copy(
    path: &BinaryRelativePath,
    direction: SyncDirection,
    source: &SnapshotEntry,
    target: Option<&SnapshotEntry>,
) -> BidirectionalAction {
    BidirectionalAction::Copy {
        path: path.clone(),
        direction,
        expected_source: source.entry_digest,
        expected_target: target.map(|entry| entry.entry_digest),
    }
}

fn conflict(
    path: &BinaryRelativePath,
    kind: ConflictKind,
    baseline: Option<[u8; 32]>,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
) -> BidirectionalAction {
    BidirectionalAction::Conflict {
        path: path.clone(),
        kind,
        baseline,
        left: left.map(|entry| entry.entry_digest),
        right: right.map(|entry| entry.entry_digest),
    }
}

fn plan_digest(plan: &BidirectionalPlan) -> io::Result<[u8; 32]> {
    #[derive(Serialize)]
    struct Body<'a> {
        pair_id: PairId,
        baseline_generation: Option<u64>,
        baseline_digest: Option<[u8; 32]>,
        left_root_identity: &'a RootIdentity,
        right_root_identity: &'a RootIdentity,
        left_snapshot_digest: [u8; 32],
        right_snapshot_digest: [u8; 32],
        actions: &'a [BidirectionalAction],
    }
    let payload = serde_json::to_vec(&Body {
        pair_id: plan.pair_id,
        baseline_generation: plan.baseline_generation,
        baseline_digest: plan.baseline_digest,
        left_root_identity: &plan.left_root_identity,
        right_root_identity: &plan.right_root_identity,
        left_snapshot_digest: plan.left_snapshot_digest,
        right_snapshot_digest: plan.right_snapshot_digest,
        actions: &plan.actions,
    })
    .map_err(io::Error::other)?;
    Ok(*blake3::hash(&payload).as_bytes())
}

/// Exklusiv gesperrter Baseline-Speicher. Der Lock bleibt für die gesamte Lebensdauer
/// offen, sodass zwei Prozesse niemals dasselbe Paar parallel fortschreiben.
pub struct BaselineStore {
    path: PathBuf,
    process_lock: File,
    lock_owner: u32,
}

impl Drop for BaselineStore {
    fn drop(&mut self) {
        // `close` allein beendet flock erst am letzten geerbten FD. Ein parallel
        // gestartetes Kind kann ihn bis exec halten und den nächsten eigenen
        // Recovery-Schritt mit EAGAIN blockieren. Der ursprüngliche Besitzer gibt
        // deshalb ausdrücklich frei; ein nach fork geerbtes Objekt darf dagegen
        // den noch laufenden Besitz seines Elternprozesses nicht freigeben.
        if self.lock_owner == std::process::id() {
            let _ = rustix::fs::flock(&self.process_lock, rustix::fs::FlockOperation::Unlock);
        }
    }
}

// Die Pfadprüfung allein bindet die später geöffnete Datei nicht: Dazwischen
// könnte ein Symlink, FIFO oder größeres Objekt eingewechselt werden. Der lokale
// Öffnungsweg lehnt Sonderdateien ohne Blockieren ab und hält alle Eltern offen.
fn read_store_payload(path: &Path, maximum: u64) -> io::Result<Option<(File, Vec<u8>)>> {
    use std::os::unix::fs::MetadataExt;
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !before.is_file() || before.len() > maximum {
        return Err(invalid_data(
            "Persistierter Paarzustand ist ungültig oder zu groß",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_input("Paarzustand braucht einen Elternordner"))?;
    let root = crate::local_fs::RootHandle::open(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid_input("Paarzustand braucht einen Dateinamen"))?;
    let file = crate::local_fs::open_read_beneath(&root, &root.path().join(name))?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.len() > maximum
        || (before.dev(), before.ino()) != (opened.dev(), opened.ino())
    {
        return Err(invalid_data(
            "Paarzustand wurde beim Öffnen ausgetauscht oder vergrößert",
        ));
    }
    let mut payload = Vec::with_capacity(opened.len() as usize);
    (&file).take(maximum + 1).read_to_end(&mut payload)?;
    let after = file.metadata()?;
    let visible = std::fs::symlink_metadata(path)?;
    if payload.len() as u64 > maximum
        || after.len() != payload.len() as u64
        || !visible.is_file()
        || (visible.dev(), visible.ino()) != (opened.dev(), opened.ino())
        || !root.is_current()?
    {
        return Err(invalid_data("Paarzustand änderte sich während des Lesens"));
    }
    Ok(Some((file, payload)))
}

impl BaselineStore {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let lock_path = baseline_lock_path(&path)?;
        let parent = lock_path
            .parent()
            .ok_or_else(|| invalid_input("Baseline-Lock braucht einen Elternordner"))?;
        std::fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options.open(lock_path)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(io::Error::from)?;
        Ok(Self {
            path,
            process_lock: lock,
            lock_owner: std::process::id(),
        })
    }

    pub fn load(&self) -> io::Result<Option<BidirectionalBaseline>> {
        let Some((_, payload)) = read_store_payload(&self.path, MAX_BASELINE_BYTES)? else {
            return Ok(None);
        };
        let baseline: BidirectionalBaseline = serde_json::from_slice(&payload)
            .map_err(|error| invalid_data(format!("Baseline ist beschädigt: {error}")))?;
        baseline.validate()?;
        Ok(Some(baseline))
    }

    pub fn save(&self, baseline: &BidirectionalBaseline) -> io::Result<()> {
        self.save_baseline_using(baseline, Self::write_baseline, Self::sync_baseline)
    }

    fn save_baseline_using(
        &self,
        baseline: &BidirectionalBaseline,
        write: impl FnOnce(&Self, &BidirectionalBaseline) -> io::Result<()>,
        sync: impl FnOnce(&Self, &BidirectionalBaseline) -> io::Result<()>,
    ) -> io::Result<()> {
        baseline.validate()?;
        let before = self.load()?;
        let write_error = match write(self, baseline) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        match self.load() {
            Ok(Some(actual)) if actual == *baseline => sync(self, baseline).map_err(|error| {
                crate::error::with_recovery_context(
                    bidirectional_effect_error(error, &[&self.path]),
                    &[&write_error],
                    &[],
                )
            }),
            Ok(actual) if actual == before => Err(write_error),
            Ok(_) => Err(bidirectional_effect_error(write_error, &[&self.path])),
            Err(error) => Err(crate::error::with_recovery_context(
                bidirectional_effect_error(error, &[&self.path]),
                &[&write_error],
                &[],
            )),
        }
    }

    fn sync_baseline(&self, expected: &BidirectionalBaseline) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid_input("Baseline braucht einen Elternordner"))?;
        let root = crate::local_fs::RootHandle::open(parent)?;
        let (file, payload) = read_store_payload(&self.path, MAX_BASELINE_BYTES)?
            .ok_or_else(|| invalid_data("Baseline verschwand vor dem erneuten fsync"))?;
        let actual: BidirectionalBaseline = serde_json::from_slice(&payload)
            .map_err(|error| invalid_data(format!("Baseline ist beschädigt: {error}")))?;
        actual.validate()?;
        if actual != *expected {
            return Err(invalid_data("Baseline änderte sich vor dem erneuten fsync"));
        }
        file.sync_all()?;
        crate::local_fs::sync_dir_beneath(&root, root.path())?;
        if !root.is_current()? || self.load()? != Some(expected.clone()) {
            return Err(invalid_data(
                "Baseline änderte sich während des erneuten fsync",
            ));
        }
        Ok(())
    }

    fn write_baseline(&self, baseline: &BidirectionalBaseline) -> io::Result<()> {
        baseline.validate()?;
        let payload = serde_json::to_vec_pretty(baseline).map_err(io::Error::other)?;
        if payload.len() as u64 > MAX_BASELINE_BYTES {
            return Err(invalid_data("Baseline überschreitet die Größenbegrenzung"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid_input("Baseline braucht einen Elternordner"))?;
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let temp = parent.join(format!(".bidirectional-baseline-{suffix}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let result = (|| {
            file.write_all(&payload)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn run_journal_path(&self) -> PathBuf {
        self.path.with_extension("run.json")
    }

    pub fn load_run_journal(&self) -> io::Result<Option<BidirectionalRunJournal>> {
        let path = self.run_journal_path();
        let Some((_, payload)) = read_store_payload(&path, MAX_RUN_JOURNAL_BYTES)? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(&payload)
            .map_err(|error| invalid_data(format!("Laufjournal ist beschädigt: {error}")))?;
        if contains_legacy_webdav_root_identity(&value) {
            // Vor Journalversion 7 fehlte die starke Collection-ETag. Sie darf
            // weder aus dem Pfad noch aus einer anderen Sitzung erfunden werden.
            return Err(invalid_data(
                if legacy_webdav_journal_may_have_effect(&value) {
                    "Älteres WebDAV-Laufjournal besitzt keine gebundene Root-ETag und kann bereits Teilwirkungen enthalten; weder fortsetzen noch neu planen, sondern Journal und beide Endpunkte manuell prüfen"
                } else {
                    "Älteres WebDAV-Laufjournal besitzt keine gebundene Root-ETag, wurde aber vor der ersten Projektion beendet; Journal nach Sichtprüfung verwerfen und erst danach neu planen"
                },
            ));
        }
        let journal: BidirectionalRunJournal = serde_json::from_value(value)
            .map_err(|error| invalid_data(format!("Laufjournal ist beschädigt: {error}")))?;
        journal.validate()?;
        Ok(Some(journal))
    }

    fn save_run_journal(&self, journal: &BidirectionalRunJournal) -> io::Result<()> {
        journal.validate()?;
        let payload = serde_json::to_vec_pretty(journal).map_err(io::Error::other)?;
        if payload.len() as u64 > MAX_RUN_JOURNAL_BYTES {
            return Err(invalid_data(
                "Laufjournal überschreitet die Größenbegrenzung",
            ));
        }
        let path = self.run_journal_path();
        let parent = path
            .parent()
            .ok_or_else(|| invalid_input("Laufjournal braucht einen Elternordner"))?;
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let temp = parent.join(format!(".bidirectional-run-{suffix}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let result = (|| {
            file.write_all(&payload)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn update_run_journal(
        &self,
        change: impl FnOnce(&mut BidirectionalRunJournal) -> io::Result<()>,
    ) -> io::Result<()> {
        self.update_run_journal_using(
            change,
            |store, journal| store.save_run_journal(journal),
            Self::sync_run_journal,
        )
    }

    fn update_run_journal_using(
        &self,
        change: impl FnOnce(&mut BidirectionalRunJournal) -> io::Result<()>,
        write: impl FnOnce(&Self, &BidirectionalRunJournal) -> io::Result<()>,
        sync: impl FnOnce(&Self, &BidirectionalRunJournal) -> io::Result<()>,
    ) -> io::Result<()> {
        let before = self
            .load_run_journal()?
            .ok_or_else(|| invalid_data("Erwartetes Laufjournal fehlt"))?;
        let mut next = before.clone();
        change(&mut next)?;
        next.refresh_digest()?;
        let write_error = match write(self, &next) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        // Sichtbarkeit beweist keine Dauerhaftigkeit: Ein nach dem Rename
        // gemeldeter Fehler darf erst nach erneutem Datei-/Parent-fsync und
        // frischem Readback als Commit gelten. Sonst startet keine Folgeprojektion.
        match self.load_run_journal() {
            Ok(Some(actual)) if actual == next => sync(self, &next).map_err(|sync_error| {
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Laufjournal ist sichtbar, aber die Dauerhaftigkeit blieb unklar: {write_error}; {sync_error}"
                    ),
                )
            }),
            Ok(Some(actual)) if actual == before => Err(write_error),
            Ok(actual) => Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Laufjournal-Schreibfehler mit unerwartetem Readback: {write_error}; vorhanden={}",
                    actual.is_some()
                ),
            )),
            Err(read_error) => Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Laufjournal-Schreibfehler und Readback fehlgeschlagen: {write_error}; {read_error}"
                ),
            )),
        }
    }

    fn sync_run_journal(&self, expected: &BidirectionalRunJournal) -> io::Result<()> {
        let path = self.run_journal_path();
        let parent = path
            .parent()
            .ok_or_else(|| invalid_input("Laufjournal braucht einen Elternordner"))?;
        let parent = crate::local_fs::RootHandle::open(parent)?;
        let (file, payload) = read_store_payload(&path, MAX_RUN_JOURNAL_BYTES)?
            .ok_or_else(|| invalid_data("Laufjournal verschwand vor dem erneuten fsync"))?;
        let actual: BidirectionalRunJournal = serde_json::from_slice(&payload)
            .map_err(|error| invalid_data(format!("Laufjournal ist beschädigt: {error}")))?;
        actual.validate()?;
        if actual != *expected {
            return Err(invalid_data(
                "Laufjournal änderte sich vor dem erneuten fsync",
            ));
        }
        file.sync_all()?;
        crate::local_fs::sync_dir_beneath(&parent, parent.path())?;
        if !parent.is_current()? || self.load_run_journal()? != Some(expected.clone()) {
            return Err(invalid_data(
                "Laufjournal änderte sich während des erneuten fsync",
            ));
        }
        Ok(())
    }

    fn remove_run_journal(&self) -> io::Result<()> {
        let path = self.run_journal_path();
        remove_file_and_sync_parent_using(
            &path,
            |journal| std::fs::remove_file(journal),
            |parent| File::open(parent)?.sync_all(),
        )
    }

    fn remove_run_journal_reconciled(&self, expected: &BidirectionalRunJournal) -> io::Result<()> {
        self.remove_run_journal_reconciled_using(
            expected,
            |journal| std::fs::remove_file(journal),
            |parent| File::open(parent)?.sync_all(),
        )
    }

    fn remove_run_journal_reconciled_using(
        &self,
        expected: &BidirectionalRunJournal,
        remove_file: impl FnOnce(&Path) -> io::Result<()>,
        mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        let path = self.run_journal_path();
        let parent = path
            .parent()
            .ok_or_else(|| invalid_input("Laufjournal braucht einen Elternordner"))?;
        let effect_error = match remove_file(&path) {
            Ok(()) => sync_parent(parent).err(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => sync_parent(parent).err(),
            Err(error) => Some(error),
        };
        let Some(effect_error) = effect_error else {
            return Ok(());
        };

        match self.load_run_journal() {
            Ok(None) => sync_parent(parent).map_err(|sync_error| {
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Laufjournal ist entfernt, aber der Elternordner blieb nach erneutem fsync unklar: {effect_error}; {sync_error}"
                    ),
                )
            }),
            Ok(Some(actual)) if actual == *expected => Err(effect_error),
            Ok(Some(_)) => Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Laufjournal änderte sich während des Abschluss-Cleanups: {effect_error}"
                ),
            )),
            Err(read_error) => Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Abschluss-Cleanup und Journal-Readback schlugen fehl: {effect_error}; {read_error}"
                ),
            )),
        }
    }
}

/// Der Lock liegt als zusätzliche Erweiterung direkt neben seiner Baseline. Damit
/// löst das Dateisystem `pair.json` und `Pair.json` genauso als gleich oder getrennt
/// auf wie die eigentliche Baseline. `with_extension` verarbeitet auch nicht als
/// UTF-8 darstellbare Namen ohne verlustbehaftete Umwandlung.
fn baseline_lock_path(path: &Path) -> io::Result<PathBuf> {
    path.parent()
        .ok_or_else(|| invalid_input("Baseline braucht einen Elternordner"))?;
    path.file_name()
        .ok_or_else(|| invalid_input("Baseline braucht einen Dateinamen"))?;
    if path.extension() != Some(std::ffi::OsStr::new("json"))
        || path.file_stem().is_none_or(std::ffi::OsStr::is_empty)
    {
        return Err(invalid_input("Baseline-Dateiname muss auf .json enden"));
    }
    Ok(path.with_extension("json.lock"))
}

/// Ein erfolgreiches `unlink` ist erst nach dem `fsync` des Elternordners dauerhaft.
/// Meldet dieser späte Schritt einen Fehler, findet der Wiederholversuch die Datei
/// bereits nicht mehr. Auch dieser erwartete `NotFound`-Fall muss den Elternordner
/// deshalb erneut synchronisieren.
fn remove_file_and_sync_parent_using(
    path: &Path,
    remove_file: impl FnOnce(&Path) -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_input("Laufjournal braucht einen Elternordner"))?;
    match remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    sync_parent(parent)
}

pub fn load_bidirectional_run_journal(
    baseline_path: &Path,
) -> io::Result<Option<BidirectionalRunJournal>> {
    BaselineStore::open(baseline_path)?.load_run_journal()
}

/// Maschinenlesbare Einordnung eines persistenten Laufjournals. Nach vollständig
/// bestätigter Daten- und Baselinewirkung darf die Queue ausschließlich das
/// mutationslose Journal-Cleanup anbieten, niemals Resume oder Rollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BidirectionalRunRecovery {
    Absent,
    ResumeOrRollback,
    CompletionCleanup,
    RollbackCompletionCleanup,
}

fn run_completion_shape_is_bound(journal: &BidirectionalRunJournal) -> bool {
    journal
        .projections
        .iter()
        .all(|projection| projection.state == BidirectionalProjectionState::Committed)
        && matches!(
            journal.state,
            BidirectionalRunState::Running | BidirectionalRunState::BaselineCommitted
        )
        && journal.rollback_step.is_none()
        && journal.type_swap_step.is_none()
}

fn run_completion_recovery_is_bound(
    journal: &BidirectionalRunJournal,
    baseline: Option<&BidirectionalBaseline>,
) -> bool {
    if !run_completion_shape_is_bound(journal) {
        return false;
    }
    match journal.committed_baseline_digest {
        Some(expected) => baseline.is_some_and(|baseline| baseline.body_digest == expected),
        None => {
            // Version 7 schrieb den Folgedigest noch nicht. Der erst nach dem
            // Baseline-Rename persistierte Zustand `BaselineCommitted` beweist
            // trotzdem, dass kein Datenplan mehr fortgesetzt werden darf. Die
            // aktuelle Baseline wird beim tatsächlichen Cleanup zusätzlich an
            // Paar und beide Endpunkte gebunden.
            journal.state == BidirectionalRunState::BaselineCommitted
        }
    }
}

fn rollback_completion_shape_is_bound(journal: &BidirectionalRunJournal) -> bool {
    journal.state == BidirectionalRunState::RollbackCommitted
        && journal.committed_baseline_digest.is_none()
        && journal
            .projections
            .iter()
            .all(|projection| projection.state == BidirectionalProjectionState::Prepared)
        && journal.rollback_step.is_none()
        && journal.type_swap_step.is_none()
}

fn rollback_completion_recovery_is_bound(
    journal: &BidirectionalRunJournal,
    baseline: Option<&BidirectionalBaseline>,
) -> bool {
    rollback_completion_shape_is_bound(journal)
        && match journal.baseline_digest {
            Some(expected) => baseline.is_some_and(|baseline| baseline.body_digest == expected),
            None => baseline.is_none(),
        }
}

fn legacy_run_completion_cleanup_is_safe(
    journal: &BidirectionalRunJournal,
    baseline: Option<&BidirectionalBaseline>,
) -> bool {
    let Some(baseline) = baseline else {
        return false;
    };
    journal.committed_baseline_digest.is_none()
        && journal.state == BidirectionalRunState::BaselineCommitted
        && baseline.pair_id == journal.pair_id
        && baseline.left_binding == journal.left_binding
        && baseline.right_binding == journal.right_binding
        && journal.baseline_digest != Some(baseline.body_digest)
}

pub fn classify_bidirectional_run_recovery(
    baseline_path: &Path,
) -> io::Result<BidirectionalRunRecovery> {
    let store = BaselineStore::open(baseline_path)?;
    let Some(journal) = store.load_run_journal()? else {
        return Ok(BidirectionalRunRecovery::Absent);
    };
    let baseline = store.load()?;
    let completion_is_bound = run_completion_recovery_is_bound(&journal, baseline.as_ref());
    let rollback_completion_is_bound =
        rollback_completion_recovery_is_bound(&journal, baseline.as_ref());
    Ok(if completion_is_bound {
        BidirectionalRunRecovery::CompletionCleanup
    } else if rollback_completion_is_bound {
        BidirectionalRunRecovery::RollbackCompletionCleanup
    } else {
        BidirectionalRunRecovery::ResumeOrRollback
    })
}

/// Entfernt ausschließlich das Journal eines exakt zurückgelesenen vollständigen
/// Baselineabschlusses. Der Aufruf ist idempotent, damit ein später Queuecommit
/// nach Prozessabbruch gefahrlos erneut versucht werden kann.
pub fn finish_bidirectional_run_completion(baseline_path: &Path) -> io::Result<()> {
    finish_bidirectional_run_completion_using(baseline_path, BaselineStore::sync_baseline)
}

fn finish_bidirectional_run_completion_using(
    baseline_path: &Path,
    sync: impl FnOnce(&BaselineStore, &BidirectionalBaseline) -> io::Result<()>,
) -> io::Result<()> {
    let store = BaselineStore::open(baseline_path)?;
    let Some(journal) = store.load_run_journal()? else {
        return Ok(());
    };
    let baseline = store.load()?;
    let completion_is_bound = run_completion_recovery_is_bound(&journal, baseline.as_ref())
        && (journal.committed_baseline_digest.is_some()
            || legacy_run_completion_cleanup_is_safe(&journal, baseline.as_ref()));
    if !completion_is_bound {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Laufjournal besitzt keinen vollständig gebundenen Baselineabschluss",
        ));
    }
    // Der Digest beweist nur Sichtbarkeit. Das noch vorhandene Journal bleibt
    // erhalten, bis auch die Baseline nach einem früheren Savefehler dauerhaft ist.
    sync(
        &store,
        baseline
            .as_ref()
            .ok_or_else(|| invalid_data("Abschluss-Baseline fehlt"))?,
    )
    .map_err(|error| {
        bidirectional_effect_error(error, &[&store.path, &store.run_journal_path()])
    })?;
    store.remove_run_journal_reconciled(&journal)
}

/// Entfernt ausschließlich den bereits vollständig zurückgerollten Laufmarker.
/// Dieser Schritt öffnet keine Endpunkte und führt keine Datenprojektion aus. Er
/// bindet den Abschluss erneut an die alte Baseline und darf danach bei einem
/// unklaren Unlink-/fsync-Ausgang beliebig wiederholt werden, sobald die Queue
/// ihren terminalen Rollbackzustand bewiesen hat.
pub fn finish_bidirectional_rollback_completion(baseline_path: &Path) -> io::Result<()> {
    let store = BaselineStore::open(baseline_path)?;
    let Some(journal) = store.load_run_journal()? else {
        return Ok(());
    };
    let baseline = store.load()?;
    let rollback_completion_is_bound =
        rollback_completion_recovery_is_bound(&journal, baseline.as_ref());
    if !rollback_completion_is_bound {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Laufjournal besitzt keinen vollständig gebundenen Rollbackabschluss",
        ));
    }
    store.remove_run_journal_reconciled(&journal)
}

/// Entfernt ein offenes Journal nur dann, wenn keine Projektion als committed gilt
/// und beide Bäume bytegenau wieder den gebundenen Ausgangssnapshots entsprechen.
/// Dieser Beweis ist für Fehler vor dem ersten Teilcommit der sichere Recovery-Weg.
pub fn discard_unchanged_bidirectional_run(
    baseline_path: &Path,
    left: &Path,
    right: &Path,
) -> io::Result<()> {
    let store = BaselineStore::open(baseline_path)?;
    let journal = store
        .load_run_journal()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Kein Laufjournal vorhanden"))?;
    if matches!(
        journal.state,
        BidirectionalRunState::BaselineCommitted | BidirectionalRunState::RollbackCommitted
    ) || journal
        .projections
        .iter()
        .any(|projection| projection.state == BidirectionalProjectionState::Committed)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Teilcommits dürfen nicht als unveränderter Lauf verworfen werden",
        ));
    }
    let baseline_digest = store.load()?.map(|baseline| baseline.body_digest);
    if baseline_digest != journal.baseline_digest {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Baseline änderte sich seit dem unterbrochenen Lauf",
        ));
    }
    let (left_root, right_root) = crate::sync::validate_local_sync_roots(left, right)?;
    let left_parent = std::fs::canonicalize(
        left_root
            .parent()
            .ok_or_else(|| invalid_input("Linker Root besitzt keinen Elternordner"))?,
    )?;
    let right_parent = std::fs::canonicalize(
        right_root
            .parent()
            .ok_or_else(|| invalid_input("Rechter Root besitzt keinen Elternordner"))?,
    )?;
    let left_vfs = crate::LocalVfs::rooted(&left_parent)?;
    let right_vfs = crate::LocalVfs::rooted(&right_parent)?;
    let left_snapshot = build_tree_snapshot(
        &left_vfs,
        &left_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    let right_snapshot = build_tree_snapshot(
        &right_vfs,
        &right_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    if EndpointBinding::local(&left_snapshot.root_identity)? != journal.left_binding
        || EndpointBinding::local(&right_snapshot.root_identity)? != journal.right_binding
        || left_snapshot.snapshot_digest != journal.left_snapshot_digest
        || right_snapshot.snapshot_digest != journal.right_snapshot_digest
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Aktuelle Bäume entsprechen nicht dem unveränderten Journal-Ausgang",
        ));
    }
    store.remove_run_journal()
}

fn recovery_prefix(
    journal: &BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
) -> io::Result<usize> {
    let committed = journal
        .projections
        .iter()
        .take_while(|projection| projection.state == BidirectionalProjectionState::Committed)
        .count();
    let running = journal
        .projections
        .get(committed)
        .is_some_and(|projection| projection.state == BidirectionalProjectionState::Running);
    let maximum = committed + usize::from(running);
    observed_projection_prefix(journal, current_left, current_right, committed, maximum)
}

fn normalize_type_swap_step(
    store: &BaselineStore,
    journal: &mut BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
    endpoints: LocalEndpointPair<'_>,
) -> io::Result<bool> {
    let LocalEndpointPair {
        left_vfs,
        left_root,
        right_vfs,
        right_root,
    } = endpoints;
    let Some(step) = journal.type_swap_step.clone() else {
        return Ok(false);
    };
    let ordinal = step.ordinal as usize;
    let projection = journal
        .projections
        .get(ordinal)
        .ok_or_else(|| invalid_data("Offener Typwechsel verweist auf keine Projektion"))?;
    let BidirectionalProjectionKind::TypeSwap {
        path,
        staging_path,
        direction,
    } = &projection.projection
    else {
        return Err(invalid_data(
            "Offener Typwechsel verweist auf falsche Projektion",
        ));
    };
    let (_, target_side) = direction_sides(*direction);
    if projection.state != BidirectionalProjectionState::Running
        || step.side != target_side
        || step.path != *path
        || step.staging_path != *staging_path
    {
        return Err(invalid_data(
            "Offener Typwechsel ist nicht konsistent gebunden",
        ));
    }
    let (vfs, root) = match target_side {
        EndpointSide::Left => (left_vfs, left_root),
        EndpointSide::Right => (right_vfs, right_root),
    };
    let relative = path.to_path_buf()?;
    let staging_relative = staging_path.to_path_buf()?;

    if snapshots_match_projection_prefix(journal, current_left, current_right, ordinal + 1)? {
        let _quarantine_lease =
            crate::quarantine::lease_quarantine_run(vfs, &step.quarantine_root, root)?;
        let base = step
            .quarantine_root
            .parent()
            .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keinen Basisordner"))?;
        let run_id = step
            .quarantine_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keine Lauf-ID"))?;
        let relative_text = relative
            .to_str()
            .ok_or_else(|| invalid_data("Typwechselpfad ist nicht verlustfrei UTF-8"))?;
        let entries = crate::quarantine::list_quarantine_entries_for_target(
            vfs,
            base,
            run_id,
            &None,
            Some(root),
        )?;
        if !entries
            .iter()
            .any(|entry| entry.relative_path == relative_text && entry.available)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Typwechsel-Nachzustand besitzt keinen gebundenen Quarantäne-Eintrag",
            ));
        }
        sync_rollback_parents(
            vfs,
            &root.join(&staging_relative),
            &step.quarantine_root.join(&relative),
        )?;
        crate::quarantine::complete_quarantine_run(vfs, &step.quarantine_root)?;
        journal.type_swap_step = None;
        let projection = journal
            .projections
            .get_mut(ordinal)
            .ok_or_else(|| invalid_data("Typwechselprojektion verschwand"))?;
        projection.state = BidirectionalProjectionState::Committed;
        projection.quarantine = Some(step.quarantine_root.to_string_lossy().into_owned());
        journal.refresh_digest()?;
        store.save_run_journal(journal)?;
        return Ok(false);
    }

    if snapshots_match_projection_prefix(journal, current_left, current_right, ordinal)? {
        let quarantine_lease =
            ensure_type_swap_quarantine_started(vfs, &step.quarantine_root, root)?;
        let base = step
            .quarantine_root
            .parent()
            .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keinen Basisordner"))?;
        let run_id = step
            .quarantine_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid_data("Typwechsel-Quarantäne besitzt keine Lauf-ID"))?;
        // Vor der ersten Datenwirkung besitzt dieser Recoverypfad nur ein leeres
        // Pending-Manifest. Der Purge behält Manifest und Intent absichtlich als
        // sichtbaren Tombstone; ohne eine atomare bedingte Entfernung darf selbst
        // dieses recordlose Sidecar nicht pfadbasiert verschwinden.
        crate::quarantine::purge_quarantine_run_for_target_with_lease(
            &quarantine_lease,
            vfs,
            base,
            run_id,
            root,
        )?;
        journal.type_swap_step = None;
        journal.projections[ordinal].quarantine = None;
        journal.refresh_digest()?;
        store.save_run_journal(journal)?;
        return Ok(false);
    }

    let (mut expected_left, mut expected_right) = expected_maps_after_prefix(journal, ordinal)?;
    let target = match target_side {
        EndpointSide::Left => &mut expected_left,
        EndpointSide::Right => &mut expected_right,
    };
    apply_type_swap_exchange_state(target, path, staging_path)?;
    let current_left_map = owned_snapshot_map(current_left);
    let current_right_map = owned_snapshot_map(current_right);
    if !semantic_maps_equal(&expected_left, &current_left_map)
        || !semantic_maps_equal(&expected_right, &current_right_map)
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Offener Typwechsel entspricht keinem sicheren Vor-, Zwischen- oder Nachzustand",
        ));
    }
    let _quarantine_lease =
        crate::quarantine::lease_quarantine_run(vfs, &step.quarantine_root, root)?;

    // Im Zwischenzustand liegt der Gewinner bereits am Original und der Verlierer
    // am Stagingnamen. Ein zweiter atomarer Swap stellt den bestätigungsfähigen
    // Vorzustand wieder her, ohne dass der Originalname jemals fehlt.
    let original = root.join(relative);
    let staging = root.join(staging_relative);
    vfs.rename_exchange(&original, &staging)
        .map_err(|error| bidirectional_effect_error(error, &[&original, &staging]))?;
    sync_rollback_parents(vfs, &original, &staging)?;
    let (restored_left, restored_right) =
        final_local_snapshots(left_vfs, left_root, right_vfs, right_root)?;
    if !snapshots_match_projection_prefix(journal, &restored_left, &restored_right, ordinal)? {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Typwechsel-Zwischenzustand ließ sich nicht exakt zurücktauschen",
        ));
    }
    journal.type_swap_step = None;
    journal.projections[ordinal].quarantine =
        Some(step.quarantine_root.to_string_lossy().into_owned());
    journal.refresh_digest()?;
    store.save_run_journal(journal)?;
    Ok(true)
}

fn apply_type_swap_exchange_state(
    target: &mut BTreeMap<BinaryRelativePath, SnapshotEntry>,
    path: &BinaryRelativePath,
    staging_path: &BinaryRelativePath,
) -> io::Result<()> {
    let original: Vec<_> = target
        .iter()
        .filter(|(candidate, _)| *candidate == path || binary_path_contains(path, candidate))
        .map(|(candidate, entry)| (candidate.clone(), entry.clone()))
        .collect();
    let staged: Vec<_> = target
        .iter()
        .filter(|(candidate, _)| {
            *candidate == staging_path || binary_path_contains(staging_path, candidate)
        })
        .map(|(candidate, entry)| (candidate.clone(), entry.clone()))
        .collect();
    if original.is_empty() || staged.is_empty() {
        return Err(invalid_data(
            "Typwechsel-Zwischenzustand besitzt keinen vollständigen Namenstausch",
        ));
    }
    target.retain(|candidate, _| {
        candidate != path
            && !binary_path_contains(path, candidate)
            && candidate != staging_path
            && !binary_path_contains(staging_path, candidate)
    });
    for (old, entry) in original {
        let new = if old == *path {
            staging_path.clone()
        } else {
            rebase_binary_path(&old, path, staging_path)?
        };
        target.insert(new, entry);
    }
    for (old, entry) in staged {
        let new = if old == *staging_path {
            path.clone()
        } else {
            rebase_binary_path(&old, staging_path, path)?
        };
        if target.insert(new, entry).is_some() {
            return Err(invalid_data("Typwechsel-Zwischenzustand kollidiert intern"));
        }
    }
    Ok(())
}

fn normalize_rollback_step(
    journal: &mut BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
    maximum: usize,
    endpoints: LocalEndpointPair<'_>,
) -> io::Result<usize> {
    let LocalEndpointPair {
        left_vfs,
        left_root,
        right_vfs,
        right_root,
    } = endpoints;
    let step = journal
        .rollback_step
        .clone()
        .ok_or_else(|| invalid_data("Rollback-Schritt fehlt"))?;
    let ordinal = step.ordinal as usize;
    if ordinal >= maximum {
        return Err(invalid_data(
            "Rollback-Schritt liegt außerhalb des committed Präfixes",
        ));
    }
    let units = projection_rollback_units(journal, ordinal)?;
    let unit_index = step.unit_index as usize;
    let delta = units
        .get(unit_index)
        .ok_or_else(|| invalid_data("Rollback-Schritt verweist auf keine Unterbaumeinheit"))?;
    if delta.path != step.path || delta.side != step.side {
        return Err(invalid_data("Rollback-Schritt passt nicht zur Projektion"));
    }
    let (vfs, root) = match delta.side {
        EndpointSide::Left => (left_vfs, left_root),
        EndpointSide::Right => (right_vfs, right_root),
    };
    if snapshots_match_rollback_units(journal, current_left, current_right, ordinal, unit_index)? {
        return Ok(ordinal + 1);
    }
    if snapshots_match_rollback_units(
        journal,
        current_left,
        current_right,
        ordinal,
        unit_index + 1,
    )? {
        sync_recovered_rollback_unit(vfs, root, journal, ordinal, delta)?;
        if unit_index + 1 == units.len() {
            journal.rollback_step = None;
            journal.projections[ordinal].state = BidirectionalProjectionState::Prepared;
            return Ok(ordinal);
        }
        journal.rollback_step = Some(rollback_step_for_unit(
            ordinal,
            unit_index + 1,
            &units[unit_index + 1],
        )?);
        return Ok(ordinal + 1);
    }
    if type_mismatch_delta(delta) {
        if snapshots_match_type_rollback_intermediate(
            journal,
            current_left,
            current_right,
            ordinal,
            false,
        )? {
            sync_recovered_rollback_unit(vfs, root, journal, ordinal, delta)?;
            journal
                .rollback_step
                .as_mut()
                .ok_or_else(|| invalid_data("Typwechsel-Rollbackschritt verschwand"))?
                .phase = BidirectionalRollbackPhase::ExchangeTypeNames;
            return Ok(ordinal + 1);
        }
        if snapshots_match_type_rollback_intermediate(
            journal,
            current_left,
            current_right,
            ordinal,
            true,
        )? {
            sync_recovered_rollback_unit(vfs, root, journal, ordinal, delta)?;
            journal
                .rollback_step
                .as_mut()
                .ok_or_else(|| invalid_data("Typwechsel-Rollbackschritt verschwand"))?
                .phase = BidirectionalRollbackPhase::QuarantineCurrent;
            return Ok(ordinal + 1);
        }
    }
    if !type_mismatch_delta(delta)
        && delta.before.is_some()
        && snapshots_match_rollback_without_unit(
            journal,
            current_left,
            current_right,
            ordinal,
            unit_index,
            delta.side,
            &delta.path,
        )?
    {
        let vfs = match delta.side {
            EndpointSide::Left => left_vfs,
            EndpointSide::Right => right_vfs,
        };
        let run_root = journal
            .rollback_quarantines
            .iter()
            .find(|quarantine| quarantine.side == delta.side)
            .map(|quarantine| &quarantine.run_root)
            .ok_or_else(|| invalid_data("Rollback-Zwischenzustand besitzt keine Quarantäne"))?;
        let backup = run_root.join(delta.path.to_path_buf()?);
        let (expected_left, expected_right) =
            expected_maps_after_rollback_units(journal, ordinal, unit_index)?;
        let expected = match delta.side {
            EndpointSide::Left => &expected_left,
            EndpointSide::Right => &expected_right,
        };
        verify_quarantined_subtree(vfs, &backup, &delta.path, expected)?;
        sync_recovered_rollback_unit(vfs, root, journal, ordinal, delta)?;
        journal
            .rollback_step
            .as_mut()
            .ok_or_else(|| invalid_data("Rollback-Schritt verschwand"))?
            .phase = BidirectionalRollbackPhase::RestorePrevious;
        return Ok(ordinal + 1);
    }
    Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "Offener Rollback-Schritt entspricht keinem sicheren Zwischenzustand",
    ))
}

fn snapshots_match_rollback_without_unit(
    journal: &BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
    ordinal: usize,
    completed_units: usize,
    side: EndpointSide,
    path: &BinaryRelativePath,
) -> io::Result<bool> {
    let (mut expected_left, mut expected_right) =
        expected_maps_after_rollback_units(journal, ordinal, completed_units)?;
    let expected = match side {
        EndpointSide::Left => &mut expected_left,
        EndpointSide::Right => &mut expected_right,
    };
    expected.retain(|candidate, _| candidate != path && !binary_path_contains(path, candidate));
    Ok(
        semantic_maps_equal(&expected_left, &owned_snapshot_map(current_left))
            && semantic_maps_equal(&expected_right, &owned_snapshot_map(current_right)),
    )
}

fn snapshots_match_projection_prefix(
    journal: &BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
    prefix: usize,
) -> io::Result<bool> {
    let (expected_left, expected_right) = expected_maps_after_prefix(journal, prefix)?;
    Ok(
        semantic_maps_equal(&expected_left, &owned_snapshot_map(current_left))
            && semantic_maps_equal(&expected_right, &owned_snapshot_map(current_right)),
    )
}

fn snapshots_match_rollback_units(
    journal: &BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
    ordinal: usize,
    completed_units: usize,
) -> io::Result<bool> {
    let (expected_left, expected_right) =
        expected_maps_after_rollback_units(journal, ordinal, completed_units)?;
    Ok(
        semantic_maps_equal(&expected_left, &owned_snapshot_map(current_left))
            && semantic_maps_equal(&expected_right, &owned_snapshot_map(current_right)),
    )
}

fn snapshots_match_type_rollback_intermediate(
    journal: &BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
    ordinal: usize,
    exchanged: bool,
) -> io::Result<bool> {
    let projection = journal
        .projections
        .get(ordinal)
        .ok_or_else(|| invalid_data("Typwechsel-Rollbackprojektion fehlt"))?;
    let BidirectionalProjectionKind::TypeSwap {
        path,
        staging_path,
        direction,
    } = &projection.projection
    else {
        return Ok(false);
    };
    let (_, side) = direction_sides(*direction);
    let (before, after) = type_swap_before_after_maps(journal, ordinal, side)?;
    let (mut expected_left, mut expected_right) = expected_maps_after_prefix(journal, ordinal + 1)?;
    let target = match side {
        EndpointSide::Left => &mut expected_left,
        EndpointSide::Right => &mut expected_right,
    };
    if exchanged {
        replace_map_subtree(target, &before, path);
        insert_rebased_map_subtree(target, &after, path, staging_path)?;
    } else {
        insert_rebased_map_subtree(target, &before, path, staging_path)?;
    }
    Ok(
        semantic_maps_equal(&expected_left, &owned_snapshot_map(current_left))
            && semantic_maps_equal(&expected_right, &owned_snapshot_map(current_right)),
    )
}

fn insert_rebased_map_subtree(
    target: &mut BTreeMap<BinaryRelativePath, SnapshotEntry>,
    source: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    old_root: &BinaryRelativePath,
    new_root: &BinaryRelativePath,
) -> io::Result<()> {
    for (path, entry) in source
        .iter()
        .filter(|(path, _)| *path == old_root || binary_path_contains(old_root, path))
    {
        let rebased = if path == old_root {
            new_root.clone()
        } else {
            rebase_binary_path(path, old_root, new_root)?
        };
        if target.insert(rebased, entry.clone()).is_some() {
            return Err(invalid_data(
                "Typwechsel-Rollback-Staging kollidiert mit einem vorhandenen Pfad",
            ));
        }
    }
    Ok(())
}

fn expected_maps_after_rollback_units(
    journal: &BidirectionalRunJournal,
    ordinal: usize,
    completed_units: usize,
) -> io::Result<(
    BTreeMap<BinaryRelativePath, SnapshotEntry>,
    BTreeMap<BinaryRelativePath, SnapshotEntry>,
)> {
    let units = projection_rollback_units(journal, ordinal)?;
    if completed_units > units.len() {
        return Err(invalid_data("Rollback-Einheitenpräfix ist zu groß"));
    }
    if matches!(
        journal
            .projections
            .get(ordinal)
            .map(|projection| &projection.projection),
        Some(BidirectionalProjectionKind::TypeSwap { .. })
    ) {
        return expected_maps_after_prefix(
            journal,
            if completed_units == 0 {
                ordinal + 1
            } else {
                ordinal
            },
        );
    }
    let (before_left, before_right) = expected_maps_after_prefix(journal, ordinal)?;
    let (mut current_left, mut current_right) = expected_maps_after_prefix(journal, ordinal + 1)?;
    let side = units
        .first()
        .ok_or_else(|| invalid_data("Rollback-Projektion besitzt keine Einheiten"))?
        .side;
    let (before, current) = match side {
        EndpointSide::Left => (&before_left, &mut current_left),
        EndpointSide::Right => (&before_right, &mut current_right),
    };
    for unit in units.iter().take(completed_units) {
        replace_map_subtree(current, before, &unit.path);
    }
    Ok((current_left, current_right))
}

fn replace_map_subtree(
    target: &mut BTreeMap<BinaryRelativePath, SnapshotEntry>,
    source: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    root: &BinaryRelativePath,
) {
    target.retain(|path, _| path != root && !binary_path_contains(root, path));
    for (path, entry) in source
        .iter()
        .filter(|(path, _)| *path == root || binary_path_contains(root, path))
    {
        target.insert(path.clone(), entry.clone());
    }
}

fn expected_maps_after_prefix(
    journal: &BidirectionalRunJournal,
    prefix: usize,
) -> io::Result<(
    BTreeMap<BinaryRelativePath, SnapshotEntry>,
    BTreeMap<BinaryRelativePath, SnapshotEntry>,
)> {
    if prefix > journal.projections.len() {
        return Err(invalid_data("Projektionspräfix ist zu groß"));
    }
    let mut left = owned_snapshot_map(&journal.left_snapshot);
    let mut right = owned_snapshot_map(&journal.right_snapshot);
    for projection in journal.projections.iter().take(prefix) {
        apply_projection_state(&projection.projection, &mut left, &mut right)?;
    }
    Ok((left, right))
}

fn observed_projection_prefix(
    journal: &BidirectionalRunJournal,
    current_left: &TreeSnapshot,
    current_right: &TreeSnapshot,
    minimum: usize,
    maximum: usize,
) -> io::Result<usize> {
    if minimum > maximum || maximum > journal.projections.len() {
        return Err(invalid_data("Recovery-Präfixgrenze ist ungültig"));
    }
    let mut expected_left = owned_snapshot_map(&journal.left_snapshot);
    let mut expected_right = owned_snapshot_map(&journal.right_snapshot);
    let current_left = owned_snapshot_map(current_left);
    let current_right = owned_snapshot_map(current_right);
    let mut matches = Vec::new();
    if minimum == 0
        && semantic_maps_equal(&expected_left, &current_left)
        && semantic_maps_equal(&expected_right, &current_right)
    {
        matches.push(0);
    }
    for (index, projection) in journal.projections.iter().enumerate() {
        apply_projection_state(
            &projection.projection,
            &mut expected_left,
            &mut expected_right,
        )?;
        let prefix = index + 1;
        if prefix >= minimum
            && prefix <= maximum
            && semantic_maps_equal(&expected_left, &current_left)
            && semantic_maps_equal(&expected_right, &current_right)
        {
            matches.push(prefix);
        }
        if prefix >= maximum {
            break;
        }
    }
    matches.into_iter().max().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Interrupted,
            "Recovery-Bäume entsprechen weder dem gebundenen Vor- noch Nachzustand",
        )
    })
}

fn owned_snapshot_map(snapshot: &TreeSnapshot) -> BTreeMap<BinaryRelativePath, SnapshotEntry> {
    snapshot
        .entries
        .iter()
        .map(|record| (record.path.clone(), record.entry.clone()))
        .collect()
}

fn semantic_maps_equal(
    expected: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    actual: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> bool {
    expected.len() == actual.len()
        && expected.iter().all(|(path, entry)| {
            actual
                .get(path)
                .is_some_and(|actual| entry.semantically_equal(actual))
        })
}

fn first_pair_map_mismatch(
    expected_left: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    expected_right: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    actual_left: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    actual_right: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> String {
    first_map_mismatch("links", expected_left, actual_left)
        .or_else(|| first_map_mismatch("rechts", expected_right, actual_right))
        .unwrap_or_else(|| "Snapshots drifteten zwischen den Prüfungen".into())
}

fn first_map_mismatch(
    side: &str,
    expected: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
    actual: &BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> Option<String> {
    for path in expected.keys().chain(actual.keys()) {
        match (expected.get(path), actual.get(path)) {
            (Some(expected), Some(actual)) if !expected.semantically_equal(actual) => {
                return Some(format!(
                    "{side} Pfad {}, Typ gleich {}, Größe gleich {}, Inhalt gleich {}, Metadaten gleich {}",
                    path.display_lossy(),
                    expected.kind == actual.kind,
                    expected.size == actual.size,
                    expected.content_digest == actual.content_digest,
                    expected.metadata_digest == actual.metadata_digest
                ));
            }
            (Some(_), None) => {
                return Some(format!("{side} Pfad {} fehlt", path.display_lossy()));
            }
            (None, Some(_)) => {
                return Some(format!(
                    "{side} Pfad {} ist unerwartet",
                    path.display_lossy()
                ));
            }
            (Some(_), Some(_)) | (None, None) => {}
        }
    }
    None
}

fn apply_projection_state(
    projection: &BidirectionalProjectionKind,
    left: &mut BTreeMap<BinaryRelativePath, SnapshotEntry>,
    right: &mut BTreeMap<BinaryRelativePath, SnapshotEntry>,
) -> io::Result<()> {
    match projection {
        BidirectionalProjectionKind::ConflictCopy {
            source_path,
            target_path,
            source_side,
            target_side,
            subtree,
        } => {
            for (source_path, target_path) in std::iter::once((source_path, target_path)).chain(
                subtree
                    .iter()
                    .map(|entry| (&entry.source_path, &entry.target_path)),
            ) {
                let source = match source_side {
                    EndpointSide::Left => left.get(source_path),
                    EndpointSide::Right => right.get(source_path),
                }
                .cloned()
                .ok_or_else(|| invalid_data("Journal-Konfliktquelle fehlt"))?;
                match target_side {
                    EndpointSide::Left => left.insert(target_path.clone(), source),
                    EndpointSide::Right => right.insert(target_path.clone(), source),
                };
            }
        }
        BidirectionalProjectionKind::Copy {
            direction, paths, ..
        } => {
            for path in paths {
                let source = match direction {
                    SyncDirection::LeftToRight => left.get(path),
                    SyncDirection::RightToLeft => right.get(path),
                }
                .cloned()
                .ok_or_else(|| invalid_data("Journal-Kopierquelle fehlt"))?;
                match direction {
                    SyncDirection::LeftToRight => right.insert(path.clone(), source),
                    SyncDirection::RightToLeft => left.insert(path.clone(), source),
                };
            }
        }
        BidirectionalProjectionKind::Delete { direction, paths } => {
            for path in paths {
                match direction {
                    SyncDirection::LeftToRight => right.remove(path),
                    SyncDirection::RightToLeft => left.remove(path),
                };
            }
        }
        BidirectionalProjectionKind::TypeSwap {
            path,
            staging_path,
            direction,
        } => {
            let target = match direction {
                SyncDirection::LeftToRight => right,
                SyncDirection::RightToLeft => left,
            };
            apply_type_swap_final_state(target, path, staging_path)?;
        }
    }
    Ok(())
}

fn apply_type_swap_final_state(
    target: &mut BTreeMap<BinaryRelativePath, SnapshotEntry>,
    path: &BinaryRelativePath,
    staging_path: &BinaryRelativePath,
) -> io::Result<()> {
    let staged: Vec<_> = target
        .iter()
        .filter(|(candidate, _)| {
            *candidate == staging_path || binary_path_contains(staging_path, candidate)
        })
        .map(|(candidate, entry)| (candidate.clone(), entry.clone()))
        .collect();
    if staged.is_empty() {
        return Err(invalid_data(
            "Journal-Typwechsel besitzt keinen Staging-Unterbaum",
        ));
    }
    target.retain(|candidate, _| {
        candidate != path
            && !binary_path_contains(path, candidate)
            && candidate != staging_path
            && !binary_path_contains(staging_path, candidate)
    });
    for (staged_path, entry) in staged {
        let final_path = if staged_path == *staging_path {
            path.clone()
        } else {
            rebase_binary_path(&staged_path, staging_path, path)?
        };
        if target.insert(final_path, entry).is_some() {
            return Err(invalid_data(
                "Journal-Typwechsel kollidiert mit einem gebundenen Zielpfad",
            ));
        }
    }
    Ok(())
}

fn snapshots_semantically_equal(left: &TreeSnapshot, right: &TreeSnapshot) -> bool {
    left.entries.len() == right.entries.len()
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| {
                left.path == right.path && left.entry.semantically_equal(&right.entry)
            })
}

fn first_semantic_mismatch(left: &TreeSnapshot, right: &TreeSnapshot) -> String {
    let left = left.map();
    let right = right.map();
    for path in left.keys().chain(right.keys()) {
        match (left.get(path), right.get(path)) {
            (Some(left), Some(right)) if !left.semantically_equal(right) => {
                return format!(
                    "Pfad {}, Typ gleich {}, Größe gleich {}, Inhalt gleich {}, Metadaten gleich {}",
                    path.display_lossy(),
                    left.kind == right.kind,
                    left.size == right.size,
                    left.content_digest == right.content_digest,
                    left.metadata_digest == right.metadata_digest
                );
            }
            (Some(_), None) => return format!("Pfad {} fehlt rechts", path.display_lossy()),
            (None, Some(_)) => return format!("Pfad {} fehlt links", path.display_lossy()),
            (Some(_), Some(_)) | (None, None) => {}
        }
    }
    "Snapshots drifteten zwischen den beiden finalen Scans".into()
}

fn skipped_paths(plan: &BidirectionalPlan) -> BTreeSet<BinaryRelativePath> {
    plan.actions
        .iter()
        .filter_map(|action| match action {
            BidirectionalAction::SkippedConflict { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

fn binary_path_contains(parent: &BinaryRelativePath, child: &BinaryRelativePath) -> bool {
    parent != child && parent.0.len() < child.0.len() && child.0.starts_with(parent.0.as_slice())
}

fn path_is_skipped(path: &BinaryRelativePath, skipped: &BTreeSet<BinaryRelativePath>) -> bool {
    skipped
        .iter()
        .any(|root| root == path || binary_path_contains(root, path))
}

fn snapshots_equal_except_skipped(
    left: &TreeSnapshot,
    right: &TreeSnapshot,
    plan: &BidirectionalPlan,
) -> bool {
    let skipped = skipped_paths(plan);
    let left_map = left.map();
    let right_map = right.map();
    left_map
        .keys()
        .chain(right_map.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .all(|path| {
            path_is_skipped(path, &skipped)
                || match (left_map.get(path), right_map.get(path)) {
                    (Some(left), Some(right)) => left.semantically_equal(right),
                    (None, None) => true,
                    _ => false,
                }
        })
}

fn resolved_baseline_entries(
    previous: Option<&BidirectionalBaseline>,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
    skipped: &BTreeSet<BinaryRelativePath>,
) -> io::Result<Vec<BaselineRecord>> {
    let left_map = left.map();
    let right_map = right.map();
    let mut entries: BTreeMap<BinaryRelativePath, SnapshotEntry> = BTreeMap::new();
    for path in left_map
        .keys()
        .chain(right_map.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        if path_is_skipped(path, skipped) {
            continue;
        }
        match (left_map.get(path), right_map.get(path)) {
            (Some(left), Some(right)) if left.semantically_equal(right) => {
                entries.insert(path.clone(), (*left).clone());
            }
            (None, None) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Nicht übersprungener Pfad ist nach dem Lauf nicht identisch",
                ));
            }
        }
    }
    if let Some(previous) = previous {
        // Skip bedeutet echte partielle Fortschreibung: Für diesen Unterbaum bleibt
        // exakt die alte gemeinsame Wahrheit bestehen, nicht eine der Konfliktseiten.
        for record in &previous.entries {
            if path_is_skipped(&record.path, skipped) {
                entries.insert(record.path.clone(), record.entry.clone());
            }
        }
    }
    Ok(entries
        .into_iter()
        .map(|(path, entry)| BaselineRecord { path, entry })
        .collect())
}

fn baseline_from_resolved_snapshots(
    pair_id: PairId,
    left_binding: EndpointBinding,
    right_binding: EndpointBinding,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
    plan: &BidirectionalPlan,
) -> io::Result<BidirectionalBaseline> {
    left.validate()?;
    right.validate()?;
    validate_binding_matches_snapshot(&left_binding, left)?;
    validate_binding_matches_snapshot(&right_binding, right)?;
    if initial_one_sided_skipped_path(plan).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Einseitig übersprungene Erstkonflikte können keine sichere Baseline bilden",
        ));
    }
    if left.metadata_profile != right.metadata_profile
        || !snapshots_equal_except_skipped(left, right, plan)
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Erstbaseline enthält einen nicht entschiedenen Unterschied",
        ));
    }
    let skipped = skipped_paths(plan);
    let mut baseline = BidirectionalBaseline {
        version: BASELINE_VERSION,
        pair_id,
        generation: 1,
        left_binding,
        right_binding,
        metadata_profile: left.metadata_profile,
        entries: resolved_baseline_entries(None, left, right, &skipped)?,
        tombstones: Vec::new(),
        body_digest: [0; 32],
    };
    baseline.body_digest = baseline.canonical_digest()?;
    baseline.validate()?;
    Ok(baseline)
}

fn validate_binding_matches_snapshot(
    binding: &EndpointBinding,
    snapshot: &TreeSnapshot,
) -> io::Result<()> {
    let actual = match binding {
        EndpointBinding::Local(_) => EndpointBinding::local(&snapshot.root_identity)?,
        EndpointBinding::Sftp(value)
        | EndpointBinding::Ftps(value)
        | EndpointBinding::WebDav(value)
        | EndpointBinding::Smb(value) => {
            EndpointBinding::remote(&snapshot.root_identity, value.account_digest)?
        }
    };
    let legacy_match = legacy_endpoint_bindings(&snapshot.root_identity, binding)?
        .iter()
        .any(|candidate| candidate == binding);
    if &actual != binding && !legacy_match {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Endpunktbindung passt nicht zur gehaltenen Snapshot-Wurzel",
        ));
    }
    Ok(())
}

fn legacy_endpoint_bindings(
    identity: &RootIdentity,
    expected: &EndpointBinding,
) -> io::Result<Vec<EndpointBinding>> {
    let account_digest = match expected {
        EndpointBinding::Ftps(value) | EndpointBinding::WebDav(value)
            if value.schema_version == 1 =>
        {
            value.account_digest
        }
        _ => return Ok(Vec::new()),
    };
    let candidates = match identity {
        RootIdentity::Ftps(value) => {
            let mut out = vec![EndpointBinding::Ftps(RemoteEndpointBinding {
                schema_version: 1,
                canonical_root: value.virtual_root.clone(),
                account_digest,
                public_identity_digest: serializable_digest(&(1_u32, "ftps", &value.server))?,
            })];
            if value.server.legacy_welcome_digest != [0; 32]
                && value.server.legacy_welcome_digest != value.server.welcome_digest
            {
                let mut server = value.server.clone();
                server.welcome_digest = server.legacy_welcome_digest;
                out.push(EndpointBinding::Ftps(RemoteEndpointBinding {
                    schema_version: 1,
                    canonical_root: value.virtual_root.clone(),
                    account_digest,
                    public_identity_digest: serializable_digest(&(1_u32, "ftps", &server))?,
                }));
            }
            out
        }
        RootIdentity::WebDav(value) => {
            if value.server.account_digest != account_digest {
                return Ok(Vec::new());
            }
            let mut out = vec![EndpointBinding::WebDav(RemoteEndpointBinding {
                schema_version: 1,
                canonical_root: value.virtual_root.clone(),
                account_digest,
                public_identity_digest: serializable_digest(&(1_u32, "webdav", &value.server))?,
            })];
            for digest in &value.server.legacy_server_header_digests {
                if *digest == value.server.server_header_digest {
                    continue;
                }
                let mut server = value.server.clone();
                server.server_header_digest = *digest;
                out.push(EndpointBinding::WebDav(RemoteEndpointBinding {
                    schema_version: 1,
                    canonical_root: value.virtual_root.clone(),
                    account_digest,
                    public_identity_digest: serializable_digest(&(1_u32, "webdav", &server))?,
                }));
            }
            out
        }
        _ => Vec::new(),
    };
    Ok(candidates)
}

fn bindings_match_same_snapshot(
    persisted: &EndpointBinding,
    current: &EndpointBinding,
    snapshot: &TreeSnapshot,
) -> io::Result<bool> {
    if persisted == current {
        return Ok(true);
    }

    // Nur FTPS und WebDAV haben ihr Bindungsschema geändert. Konto und Root
    // bleiben dabei unverändert; ausschließlich der Digest derselben frisch
    // authentisierten Serveridentität darf zwischen Schema 1 und 2 abweichen.
    // Würden wir hier bloß dieselbe Enum-Variante verlangen, wären insbesondere
    // SFTP- und FTPS-Bindungen verschiedener Konten jeweils für sich gegen den
    // Server-Snapshot gültig und würden anschließend fälschlich als gleich gelten.
    let is_supported_migration = match (persisted, current) {
        (EndpointBinding::Ftps(old), EndpointBinding::Ftps(new))
        | (EndpointBinding::WebDav(old), EndpointBinding::WebDav(new)) => {
            old.schema_version == 1
                && new.schema_version == 2
                && old.canonical_root == new.canonical_root
                && old.account_digest == new.account_digest
        }
        _ => false,
    };
    if !is_supported_migration {
        return Ok(false);
    }
    validate_binding_matches_snapshot(persisted, snapshot)?;
    validate_binding_matches_snapshot(current, snapshot)?;
    Ok(true)
}

fn validate_records(records: &[SnapshotRecord]) -> io::Result<()> {
    for record in records {
        record.path.validate()?;
        record.entry.validate()?;
    }
    if records.windows(2).any(|pair| pair[0].path >= pair[1].path) {
        return Err(invalid_data("Snapshot-Pfade sind nicht eindeutig sortiert"));
    }
    Ok(())
}

fn validate_baseline_records(records: &[BaselineRecord]) -> io::Result<()> {
    for record in records {
        record.path.validate()?;
        record.entry.validate()?;
    }
    if records.windows(2).any(|pair| pair[0].path >= pair[1].path) {
        return Err(invalid_data("Baseline-Pfade sind nicht eindeutig sortiert"));
    }
    Ok(())
}

fn validate_tombstones(tombstones: &[Tombstone], entries: &[BaselineRecord]) -> io::Result<()> {
    for tombstone in tombstones {
        tombstone.path.validate()?;
        if tombstone.baseline_generation == 0 || tombstone.previous_entry_digest == [0; 32] {
            return Err(invalid_data("Tombstone ist unvollständig"));
        }
    }
    if tombstones
        .windows(2)
        .any(|pair| pair[0].path >= pair[1].path)
        || tombstones.iter().any(|item| {
            entries
                .binary_search_by(|entry| entry.path.cmp(&item.path))
                .is_ok()
        })
    {
        return Err(invalid_data(
            "Tombstones sind doppelt oder überlappen Baseline-Einträge",
        ));
    }
    Ok(())
}

fn encode_path(hash: &mut blake3::Hasher, path: &BinaryRelativePath) {
    hash.update(&(path.0.len() as u64).to_be_bytes());
    for component in &path.0 {
        hash.update(&(component.len() as u64).to_be_bytes());
        hash.update(component);
    }
}

fn encode_optional_bytes(hash: &mut blake3::Hasher, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            hash.update(&[1]);
            hash.update(&(value.len() as u64).to_be_bytes());
            hash.update(value);
        }
        None => {
            hash.update(&[0]);
        }
    }
}

fn metadata_profile_byte(profile: MetadataProfile) -> u8 {
    match profile {
        MetadataProfile::Portable => 1,
        MetadataProfile::NativeStrict => 2,
        MetadataProfile::NativeBestEffort => 3,
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{SftpHostKeyPin, SftpRootIdentity, SftpServerIdentity};
    use crate::vfs::LocalVfs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    struct SnapshotFaultVfs {
        inner: LocalVfs,
        payload: Option<Vec<u8>>,
        bytes_read: Arc<std::sync::atomic::AtomicUsize>,
        failed_sync: Option<PathBuf>,
        scan_failure: Option<&'static str>,
    }

    struct CountedReader {
        reader: std::io::Cursor<Vec<u8>>,
        bytes_read: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Read for CountedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.reader.read(buffer)?;
            self.bytes_read
                .fetch_add(count, std::sync::atomic::Ordering::SeqCst);
            Ok(count)
        }
    }

    impl Vfs for SnapshotFaultVfs {
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }
        fn is_local(&self) -> bool {
            true
        }
        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            match &self.payload {
                Some(payload) => Ok(Box::new(CountedReader {
                    reader: std::io::Cursor::new(payload.clone()),
                    bytes_read: self.bytes_read.clone(),
                })),
                None => self.inner.open_read(path),
            }
        }
        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            if self.failed_sync.as_deref() == Some(path) {
                return Err(io::Error::other("erzwungener Namespace-fsync-Fehler"));
            }
            self.inner.sync_dir(path)
        }
        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }
        fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
            if self.scan_failure == Some("listing") {
                return Err(crate::error::with_recovery_paths(
                    crate::coded_error(
                        io::ErrorKind::UnexpectedEof,
                        crate::ErrorCode::ConnectionLost,
                        "Testverbindung abgebrochen",
                    ),
                    &["fixture/recovery".to_owned()],
                ));
            }
            self.inner.list_complete(path)
        }
        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.inner.stat(path)
        }
        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }
        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir(path)
        }
        fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(path)
        }
        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write_new(path)
        }
        fn sync_file(&self, path: &Path) -> io::Result<()> {
            if self.failed_sync.as_deref() == Some(path) {
                return Err(io::Error::other("erzwungener Datei-fsync-Fehler"));
            }
            self.inner.sync_file(path)
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
        fn remove_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
        }
        fn root_identity(&self, path: &Path) -> io::Result<RootIdentity> {
            if self.scan_failure == Some("root") {
                return Err(crate::error::with_recovery_paths(
                    crate::coded_error(
                        io::ErrorKind::UnexpectedEof,
                        crate::ErrorCode::ConnectionLost,
                        "Testverbindung abgebrochen",
                    ),
                    &["fixture/recovery".to_owned()],
                ));
            }
            self.inner.root_identity(path)
        }
        fn local_root_identity(&self, path: &Path) -> io::Result<LocalRootIdentity> {
            self.inner.local_root_identity(path)
        }
    }

    #[test]
    fn review_rollback_recovery_bestaetigt_keinen_undauerhaften_claim() {
        let fixture = Fixture::new();
        let baseline = fixture._temp.path().join("state/pair.json");
        create_interrupted_sftp_addition(&fixture, PairId([116; 16]), &baseline);
        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared =
            prepare_sftp_bidirectional_rollback("Recovery-fsync", left, right, &baseline).unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let LocalBidirectionalRollback {
            left_root,
            right_root,
            left_vfs,
            right_vfs,
            store,
            mut journal,
        } = prepared.confirm(&phrase).unwrap();
        let delta = projection_rollback_units(&journal, 0).unwrap().remove(0);
        assert_eq!(delta.side, EndpointSide::Right);
        journal.rollback_step = Some(rollback_step_for_unit(0, 0, &delta).unwrap());
        let mut leases = Vec::new();
        let run = ensure_rollback_quarantine(
            &store,
            &mut journal,
            delta.side,
            right_vfs.as_ref(),
            right_root.parent().unwrap(),
            &right_root,
            &mut leases,
        )
        .unwrap();
        let (_, expected) = expected_maps_after_prefix(&journal, 1).unwrap();
        let vfs = SnapshotFaultVfs {
            inner: LocalVfs::rooted(right_root.parent().unwrap()).unwrap(),
            payload: None,
            bytes_read: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failed_sync: Some(right_root.clone()),
            scan_failure: None,
        };
        assert!(quarantine_rollback_entry(
            &vfs,
            &right_root,
            &run,
            &delta.path,
            delta.after.as_ref().unwrap(),
            &expected
        )
        .is_err());
        let (current_left, current_right) =
            final_local_snapshots(left_vfs.as_ref(), &left_root, &vfs, &right_root).unwrap();
        let error = normalize_rollback_step(
            &mut journal,
            &current_left,
            &current_right,
            1,
            LocalEndpointPair {
                left_vfs: left_vfs.as_ref(),
                left_root: &left_root,
                right_vfs: &vfs,
                right_root: &right_root,
            },
        )
        .expect_err("Readback allein beweist keinen dauerhaften Rename");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(journal.rollback_step.is_some());
        assert_eq!(
            journal.projections[0].state,
            BidirectionalProjectionState::Committed
        );
    }

    #[test]
    fn review_baseline_rename_braucht_nachgeholtes_fsync() {
        let fixture = Fixture::new();
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let baseline = BidirectionalBaseline::from_identical_snapshots(
            PairId([117; 16]),
            bindings.0,
            bindings.1,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        let store = BaselineStore::open(fixture._temp.path().join("state/pair.json")).unwrap();
        let retry = std::cell::Cell::new(false);
        let error = store
            .save_baseline_using(
                &baseline,
                |store, next| {
                    let temp = store.path.with_extension("test-tmp");
                    std::fs::write(&temp, serde_json::to_vec(next).unwrap())?;
                    std::fs::rename(temp, &store.path)?;
                    Err(io::Error::other("Fehler nach wirkendem Baseline-Rename"))
                },
                |_, _| {
                    retry.set(true);
                    Err(io::Error::other("Nachgeholter fsync verweigert"))
                },
            )
            .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(retry.get());
        assert_eq!(store.load().unwrap(), Some(baseline.clone()));
        store
            .save_baseline_using(
                &baseline,
                |_, _| Err(io::Error::other("Antwortverlust")),
                BaselineStore::sync_baseline,
            )
            .unwrap();
    }

    #[test]
    fn review_abschlusscleanup_behaelt_journal_bei_baseline_fsync_fehler() {
        let fixture = Fixture::new();
        let baseline = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional(
            PairId([118; 16]),
            "Abschluss-fsync",
            &fixture.left,
            &fixture.right,
            &baseline,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase, false)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        let before = load_bidirectional_run_journal(&baseline).unwrap().unwrap();
        let error = finish_bidirectional_run_completion_using(&baseline, |_, _| {
            Err(io::Error::other("Baseline-fsync verweigert"))
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            load_bidirectional_run_journal(&baseline).unwrap(),
            Some(before)
        );
        finish_bidirectional_run_completion(&baseline).unwrap();
        assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
    }

    #[test]
    fn review_snapshot_begrenzt_den_tatsaechlichen_datenstrom() {
        for payload in [vec![b'x'; COPY_BUFFER_BYTES * 4], Vec::new()] {
            let fixture = Fixture::new();
            write(&fixture.left.join("a"), "x");
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let vfs = SnapshotFaultVfs {
                inner: LocalVfs::rooted(&fixture.left).unwrap(),
                payload: Some(payload),
                bytes_read: count.clone(),
                failed_sync: None,
                scan_failure: None,
            };
            let result = build_tree_snapshot(
                &vfs,
                &fixture.left,
                MetadataProfile::Portable,
                SnapshotLimits {
                    max_file_bytes: 1,
                    ..SnapshotLimits::default()
                },
            );
            assert!(
                result.is_err(),
                "Streamgröße darf der gebundenen Dateigröße nicht widersprechen"
            );
            assert!(
                count.load(std::sync::atomic::Ordering::SeqCst) <= 2,
                "höchstens die gebundene Größe plus ein Prüfbyte lesen"
            );
        }
    }

    #[test]
    fn review_rollback_achtet_auf_ausgelassene_und_gebundene_modi() {
        for directory in [false, true] {
            let fixture = Fixture::new();
            let target = fixture.left.join("a");
            if directory {
                std::fs::create_dir(&target).unwrap();
                write(&target.join("child"), "inhalt");
            } else {
                write(&target, "inhalt");
            }
            let relative = BinaryRelativePath(vec![b"a".to_vec()]);
            let mut expected = BTreeMap::new();
            collect_local_subtree(&fixture.left_vfs, &target, &relative, &mut expected).unwrap();
            let bound = expected.clone();
            for entry in expected.values_mut() {
                entry.metadata_digest = portable_metadata_digest(None);
                entry.entry_digest = entry.canonical_digest();
            }
            let parent = fixture.left.parent().unwrap();
            let vfs = LocalVfs::rooted(parent).unwrap();
            let base = parent.join(".doppeldecker-quarantine");
            crate::real_mode::ensure_quarantine_root(&vfs, parent, &base).unwrap();
            let run = crate::real_mode::reserve_quarantine_run_name(&vfs, &base).unwrap();
            let _lease =
                crate::quarantine::start_quarantine_run(&vfs, &run, &fixture.left).unwrap();
            ensure_current_quarantined(
                &vfs,
                &fixture.left,
                &run,
                &relative,
                &expected[&relative],
                None,
                &expected,
            )
            .unwrap();
            ensure_current_quarantined(
                &vfs,
                &fixture.left,
                &run,
                &relative,
                &expected[&relative],
                None,
                &expected,
            )
            .unwrap();
            assert!(!target.exists());
            verify_quarantined_subtree(&vfs, &run.join("a"), &relative, &expected).unwrap();
            vfs.set_mode(&run.join("a"), 0o700).unwrap();
            let mut changed = bound.clone();
            changed.get_mut(&relative).unwrap().metadata_digest =
                portable_metadata_digest(Some(0o600));
            assert!(verify_quarantined_subtree(&vfs, &run.join("a"), &relative, &changed).is_err());
        }
    }

    #[test]
    fn review_rollback_fsync_fehler_erhaelt_wirkung_und_pfade() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "inhalt");
        let parent = fixture.left.parent().unwrap();
        let relative = BinaryRelativePath(vec![b"a".to_vec()]);
        let expected = snapshot_local_entry(&fixture.left_vfs, &fixture.left.join("a")).unwrap();
        let vfs = SnapshotFaultVfs {
            inner: LocalVfs::rooted(parent).unwrap(),
            payload: None,
            bytes_read: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failed_sync: Some(fixture.left.clone()),
            scan_failure: None,
        };
        let base = parent.join(".doppeldecker-quarantine");
        crate::real_mode::ensure_quarantine_root(&vfs, parent, &base).unwrap();
        let run = crate::real_mode::reserve_quarantine_run_name(&vfs, &base).unwrap();
        let _lease = crate::quarantine::start_quarantine_run(&vfs, &run, &fixture.left).unwrap();
        let error = quarantine_rollback_entry(
            &vfs,
            &fixture.left,
            &run,
            &relative,
            &expected,
            &BTreeMap::from([(relative.clone(), expected.clone())]),
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(crate::error::recovery_paths(&error)
            .contains(&run.join("a").to_string_lossy().into_owned()));
        assert_eq!(std::fs::read(run.join("a")).unwrap(), b"inhalt");
    }

    #[test]
    fn review_scan_kontext_erhaelt_code_und_recoverypfade() {
        for fault in ["root", "listing"] {
            let fixture = Fixture::new();
            let vfs = SnapshotFaultVfs {
                inner: LocalVfs::rooted(&fixture.left).unwrap(),
                payload: None,
                bytes_read: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                failed_sync: None,
                scan_failure: Some(fault),
            };
            let error = build_tree_snapshot(
                &vfs,
                &fixture.left,
                MetadataProfile::Portable,
                SnapshotLimits::default(),
            )
            .unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::ConnectionLost);
            assert_eq!(
                crate::error::recovery_paths(&error),
                vec!["fixture/recovery"]
            );
        }
    }

    #[test]
    fn review_modus_rollback_bestaetigt_erst_nach_fsync() {
        for directory in [false, true] {
            let fixture = Fixture::new();
            let target = fixture.left.join("a");
            if directory {
                std::fs::create_dir(&target).unwrap();
            } else {
                write(&target, "inhalt");
            }
            fixture.left_vfs.set_mode(&target, 0o700).unwrap();
            let before = snapshot_local_entry(&fixture.left_vfs, &target).unwrap();
            fixture.left_vfs.set_mode(&target, 0o600).unwrap();
            let after = snapshot_local_entry(&fixture.left_vfs, &target).unwrap();
            let vfs = SnapshotFaultVfs {
                inner: LocalVfs::rooted(&fixture.left).unwrap(),
                payload: None,
                bytes_read: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                failed_sync: Some(target.clone()),
                scan_failure: None,
            };
            let delta = ProjectionDelta {
                side: EndpointSide::Left,
                path: BinaryRelativePath(vec![b"a".to_vec()]),
                before: Some(before.clone()),
                after: Some(after),
            };
            let error = restore_portable_mode(&vfs, &target, &delta).unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert!(snapshot_local_entry(&fixture.left_vfs, &target)
                .unwrap()
                .semantically_equal(&before));
            restore_portable_mode(&fixture.left_vfs, &target, &delta).unwrap();
        }
    }

    struct Fixture {
        _temp: TempDir,
        left: PathBuf,
        right: PathBuf,
        left_vfs: LocalVfs,
        right_vfs: LocalVfs,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let left = temp.path().join("left");
            let right = temp.path().join("right");
            std::fs::create_dir_all(&left).unwrap();
            std::fs::create_dir_all(&right).unwrap();
            // macOS kanonisiert `/var` zu `/private/var`; das gehaltene VFS erwartet
            // deshalb denselben kanonischen Anzeigenpfad wie der Produktionsassistent.
            let left = std::fs::canonicalize(left).unwrap();
            let right = std::fs::canonicalize(right).unwrap();
            let left_vfs = LocalVfs::rooted(&left).unwrap();
            let right_vfs = LocalVfs::rooted(&right).unwrap();
            Self {
                _temp: temp,
                left,
                right,
                left_vfs,
                right_vfs,
            }
        }

        fn snapshots(&self) -> (TreeSnapshot, TreeSnapshot) {
            (
                build_tree_snapshot(
                    &self.left_vfs,
                    &self.left,
                    MetadataProfile::Portable,
                    SnapshotLimits::default(),
                )
                .unwrap(),
                build_tree_snapshot(
                    &self.right_vfs,
                    &self.right,
                    MetadataProfile::Portable,
                    SnapshotLimits::default(),
                )
                .unwrap(),
            )
        }

        fn bindings(
            &self,
            snapshots: &(TreeSnapshot, TreeSnapshot),
        ) -> (EndpointBinding, EndpointBinding) {
            (
                EndpointBinding::local(&snapshots.0.root_identity).unwrap(),
                EndpointBinding::local(&snapshots.1.root_identity).unwrap(),
            )
        }
    }

    #[test]
    fn ap02_jede_angebotene_erstentscheidung_ist_ausfuehrbar() {
        for scenario in ["one-sided", "files", "type-swap", "equal"] {
            let make = || {
                let fixture = Fixture::new();
                write(&fixture.left.join("konflikt"), "links");
                match scenario {
                    "one-sided" => {}
                    "files" => write(&fixture.right.join("konflikt"), "rechts"),
                    "type-swap" => {
                        std::fs::create_dir(fixture.right.join("konflikt")).unwrap();
                        write(&fixture.right.join("konflikt/kind"), "rechts");
                    }
                    "equal" => write(&fixture.right.join("konflikt"), "links"),
                    _ => unreachable!(),
                }
                fixture
            };
            let fixture = make();
            let pair = PairId([81; 16]);
            let baseline = fixture._temp.path().join("state/pair.json");
            let prepared =
                prepare_local_bidirectional(pair, "AP02", &fixture.left, &fixture.right, &baseline)
                    .unwrap();
            let preview = prepared.preview().clone();
            if scenario == "equal" {
                assert!(preview.conflict_details.is_empty());
            } else {
                assert_eq!(preview.conflict_details.len(), 1);
            }
            if scenario == "one-sided" {
                let view = &preview.conflict_details[0];
                assert!(!view.right.exists);
                assert!(view.skip_blocker.is_some());
                assert!(!view.choices.iter().any(|choice| matches!(
                    choice.decision,
                    ConflictDecision::Skip | ConflictDecision::KeepBoth { .. }
                )));
            }
            prepared.discard().unwrap();
            drop(prepared);
            for choice in preview
                .conflict_details
                .iter()
                .flat_map(|view| &view.choices)
                .filter(|choice| choice.blocker.is_none())
            {
                let fixture = make();
                let baseline = fixture._temp.path().join("state/pair.json");
                let prepared = prepare_local_bidirectional_with_decisions(
                    pair,
                    "AP02",
                    &fixture.left,
                    &fixture.right,
                    &baseline,
                    &[resolution("konflikt", choice.decision.clone())],
                )
                .unwrap();
                assert!(
                    prepared.preview().stage_blocker.is_none(),
                    "{} {:?}: {:?}",
                    scenario,
                    choice.decision,
                    prepared.preview().stage_blocker
                );
                let phrase = prepared.preview().confirmation_phrase.clone();
                prepared
                    .confirm(&phrase, true)
                    .unwrap()
                    .execute(&crate::Cancel::new(), &mut ignore_progress)
                    .unwrap();
                finish_bidirectional_run_completion(&baseline).unwrap();
                assert!(BaselineStore::open(&baseline)
                    .unwrap()
                    .load()
                    .unwrap()
                    .is_some());
            }
        }
    }

    #[test]
    fn ap02_spaetere_konfliktentscheidung_beruecksichtigt_fruehere_mengengrenzen() {
        let fixture = Fixture::new();
        for index in 0..20 {
            let name = format!("gleich-{index}");
            write(&fixture.left.join(&name), "gleich");
            write(&fixture.right.join(&name), "gleich");
        }
        write(&fixture.left.join("A.txt"), "A");
        write(&fixture.left.join("B.txt"), "B");
        let baseline = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional(
            PairId([83; 16]),
            "AP02",
            &fixture.left,
            &fixture.right,
            &baseline,
        )
        .unwrap();
        let before = &prepared.preview().conflict_details;
        assert!(before
            .iter()
            .all(|view| view.choices.iter().any(|choice| matches!(
                choice.decision,
                ConflictDecision::KeepRight
            ) && choice.blocker.is_none())));
        let after = prepared
            .conflict_choices_after_decisions(
                &[resolution("A.txt", ConflictDecision::KeepRight)],
                prepared.preview().plan.plan_digest,
            )
            .unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0].choices.iter().any(|choice| matches!(
            choice.decision,
            ConflictDecision::KeepRight
        ) && choice
            .blocker
            .as_ref()
            .is_some_and(|reason| reason.contains("5 %"))));
        assert!(prepared
            .conflict_choices_after_decisions(&[], [0; 32])
            .is_err());
        prepared.discard().unwrap();
    }

    #[test]
    fn spaetere_konfliktwahl_zeigt_die_erreichte_typwechselgrenze() {
        let fixture = Fixture::new();
        let mut resolutions = Vec::new();
        for index in 0..101 {
            let name = format!("konflikt-{index:03}");
            write(&fixture.left.join(&name), "links");
            std::fs::create_dir(fixture.right.join(&name)).unwrap();
            resolutions.push(resolution(&name, ConflictDecision::KeepLeft));
        }
        let baseline = fixture._temp.path().join("state/pair.json");
        let pair = PairId([84; 16]);
        let prepared = prepare_local_bidirectional(
            pair,
            "Typwechselgrenze",
            &fixture.left,
            &fixture.right,
            &baseline,
        )
        .unwrap();
        let choices = prepared
            .conflict_choices_after_decisions(
                &resolutions[..100],
                prepared.preview().plan.plan_digest,
            )
            .unwrap();
        prepared.discard().unwrap();
        drop(prepared);

        let over_limit = prepare_local_bidirectional_with_decisions(
            pair,
            "Typwechselgrenze",
            &fixture.left,
            &fixture.right,
            &baseline,
            &resolutions,
        )
        .unwrap();
        assert!(over_limit.preview().stage_blocker.is_some());
        over_limit.discard().unwrap();
        drop(over_limit);
        assert_eq!(choices.len(), 1);
        assert!(choices[0]
            .choices
            .iter()
            .any(|choice| matches!(choice.decision, ConflictDecision::Skip)));
        assert!(choices[0]
            .choices
            .iter()
            .any(|choice| matches!(choice.decision, ConflictDecision::KeepLeft)));
        for choice in &choices[0].choices {
            if matches!(choice.decision, ConflictDecision::Skip) {
                assert!(choice.blocker.is_none());
            } else {
                assert!(
                    choice.blocker.is_some(),
                    "Nicht ausführbare Typwechselwahl bleibt angeboten: {:?}",
                    choice.decision
                );
            }
        }
        assert!(BaselineStore::open(&baseline)
            .unwrap()
            .load_run_journal()
            .unwrap()
            .is_none());
        for index in 0..101 {
            let name = format!("konflikt-{index:03}");
            assert_eq!(std::fs::read(fixture.left.join(&name)).unwrap(), b"links");
            assert!(fixture.right.join(&name).is_dir());
        }
    }

    #[test]
    fn ap02_geaenderte_konfliktsnapshots_werden_vor_wirkung_abgelehnt() {
        let fixture = Fixture::new();
        write(&fixture.left.join("bericht.txt"), "links");
        write(&fixture.right.join("bericht.txt"), "rechts");
        let baseline = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional(
            PairId([82; 16]),
            "AP02",
            &fixture.left,
            &fixture.right,
            &baseline,
        )
        .unwrap();
        let digest = prepared.preview().plan.plan_digest;
        prepared.discard().unwrap();
        drop(prepared);
        write(&fixture.right.join("bericht.txt"), "spaeter");
        let error = prepare_local_bidirectional_with_decisions_bound(
            PairId([82; 16]),
            "AP02",
            &fixture.left,
            &fixture.right,
            &baseline,
            &[resolution("bericht.txt", ConflictDecision::KeepLeft)],
            Some(digest),
        )
        .err()
        .unwrap();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(
            std::fs::read(fixture.right.join("bericht.txt")).unwrap(),
            b"spaeter"
        );
        assert!(BaselineStore::open(&baseline)
            .unwrap()
            .load_run_journal()
            .unwrap()
            .is_none());
    }

    #[test]
    fn remote_bindung_ignoriert_sitzung_aber_bindet_konto_server_und_root() {
        let identity = |generation| {
            RootIdentity::Sftp(SftpRootIdentity {
                server: SftpServerIdentity {
                    host: "sftp.example.test".into(),
                    port: 22,
                    host_key: SftpHostKeyPin {
                        algorithm: "ssh-ed25519".into(),
                        sha256: [1; 32],
                    },
                    canonical_root: "/daten".into(),
                    sftp_version: 3,
                    extensions_digest: [2; 32],
                },
                virtual_root: "/daten/projekt".into(),
                session_generation: generation,
            })
        };
        let first = EndpointBinding::remote(&identity(1), [3; 32]).unwrap();
        let reconnected = EndpointBinding::remote(&identity(2), [3; 32]).unwrap();
        let other_account = EndpointBinding::remote(&identity(2), [4; 32]).unwrap();
        assert_eq!(first, reconnected);
        assert_ne!(first, other_account);

        let snapshot = TreeSnapshot {
            root_identity: identity(2),
            metadata_profile: MetadataProfile::Portable,
            entries: Vec::new(),
            total_file_bytes: 0,
            snapshot_digest: snapshot_digest(MetadataProfile::Portable, &[]),
        };
        assert!(!bindings_match_same_snapshot(&first, &other_account, &snapshot).unwrap());

        let RootIdentity::Sftp(mut changed_server) = identity(3) else {
            unreachable!()
        };
        changed_server.server.host_key.sha256 = [5; 32];
        assert_ne!(
            first,
            EndpointBinding::remote(&RootIdentity::Sftp(changed_server), [3; 32]).unwrap()
        );
    }

    #[test]
    fn authentisierte_snapshots_migrieren_alte_ftps_und_webdav_bindungen() {
        let account = [3; 32];
        let ftps_identity = RootIdentity::Ftps(crate::remote::FtpsRootIdentity {
            server: crate::remote::FtpsServerIdentity {
                host: "ftp.example.test".into(),
                port: 21,
                transport_security: crate::remote::FtpTransportSecurity::Plaintext,
                certificate: None,
                canonical_root: "/daten".into(),
                features_digest: [4; 32],
                welcome_digest: [5; 32],
                legacy_welcome_digest: [6; 32],
            },
            virtual_root: "/daten/projekt".into(),
            session_generation: 1,
        });
        let mut legacy_ftps_server = match &ftps_identity {
            RootIdentity::Ftps(value) => value.server.clone(),
            _ => unreachable!(),
        };
        legacy_ftps_server.welcome_digest = legacy_ftps_server.legacy_welcome_digest;
        let legacy_ftps = EndpointBinding::Ftps(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: "/daten/projekt".into(),
            account_digest: account,
            public_identity_digest: serializable_digest(&(1_u32, "ftps", legacy_ftps_server))
                .unwrap(),
        });
        let current_ftps = EndpointBinding::remote(&ftps_identity, account).unwrap();
        let other_ftps_account = EndpointBinding::remote(&ftps_identity, [11; 32]).unwrap();
        let immediate_legacy_ftps = EndpointBinding::Ftps(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: "/daten/projekt".into(),
            account_digest: account,
            public_identity_digest: serializable_digest(&(
                1_u32,
                "ftps",
                match &ftps_identity {
                    RootIdentity::Ftps(value) => &value.server,
                    _ => unreachable!(),
                },
            ))
            .unwrap(),
        });
        let ftps_snapshot = TreeSnapshot {
            root_identity: ftps_identity,
            metadata_profile: MetadataProfile::Portable,
            entries: Vec::new(),
            total_file_bytes: 0,
            snapshot_digest: snapshot_digest(MetadataProfile::Portable, &[]),
        };
        assert!(bindings_match_same_snapshot(&legacy_ftps, &current_ftps, &ftps_snapshot).unwrap());
        assert!(bindings_match_same_snapshot(
            &immediate_legacy_ftps,
            &current_ftps,
            &ftps_snapshot,
        )
        .unwrap());
        let other_account_matches =
            bindings_match_same_snapshot(&legacy_ftps, &other_ftps_account, &ftps_snapshot)
                .unwrap();
        assert!(!other_account_matches);

        let webdav_identity = RootIdentity::WebDav(crate::remote::WebDavRootIdentity {
            server: crate::remote::WebDavServerIdentity {
                origin: "https://dav.example.test".into(),
                certificate: crate::remote::WebDavCertificatePin { sha256: [7; 32] },
                account_digest: account,
                canonical_root: "/daten".into(),
                dav_header_digest: [8; 32],
                server_header_digest: [9; 32],
                legacy_server_header_digests: vec![[10; 32]],
            },
            virtual_root: "/daten/projekt".into(),
            root_etag: "\"root-1\"".into(),
            session_generation: 1,
        });
        let mut legacy_webdav_server = match &webdav_identity {
            RootIdentity::WebDav(value) => value.server.clone(),
            _ => unreachable!(),
        };
        legacy_webdav_server.server_header_digest = [10; 32];
        let legacy_webdav = EndpointBinding::WebDav(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: "/daten/projekt".into(),
            account_digest: account,
            public_identity_digest: serializable_digest(&(1_u32, "webdav", legacy_webdav_server))
                .unwrap(),
        });
        let current_webdav = EndpointBinding::remote(&webdav_identity, account).unwrap();
        let immediate_legacy_webdav = EndpointBinding::WebDav(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: "/daten/projekt".into(),
            account_digest: account,
            public_identity_digest: serializable_digest(&(
                1_u32,
                "webdav",
                match &webdav_identity {
                    RootIdentity::WebDav(value) => &value.server,
                    _ => unreachable!(),
                },
            ))
            .unwrap(),
        });
        let webdav_snapshot = TreeSnapshot {
            root_identity: webdav_identity,
            metadata_profile: MetadataProfile::Portable,
            entries: Vec::new(),
            total_file_bytes: 0,
            snapshot_digest: snapshot_digest(MetadataProfile::Portable, &[]),
        };
        assert!(
            bindings_match_same_snapshot(&legacy_webdav, &current_webdav, &webdav_snapshot)
                .unwrap()
        );
        assert!(bindings_match_same_snapshot(
            &immediate_legacy_webdav,
            &current_webdav,
            &webdav_snapshot,
        )
        .unwrap());
        assert!(matches!(current_ftps, EndpointBinding::Ftps(value) if value.schema_version == 2));
        assert!(
            matches!(current_webdav, EndpointBinding::WebDav(value) if value.schema_version == 2)
        );
    }

    #[test]
    fn provider_plan_bleibt_ohne_schreibfreigabe_rein_lesend() {
        let fixture = Fixture::new();
        let baseline = fixture._temp.path().join("state/provider-pair.json");
        let preview = plan_provider_bidirectional(
            PairId([81; 16]),
            &crate::ProviderEndpoint::local(&fixture.left).unwrap(),
            &crate::ProviderEndpoint::local(&fixture.right).unwrap(),
            &baseline,
            &[],
        )
        .unwrap();

        assert!(preview.plan.executable);
        assert!(matches!(preview.left_binding, EndpointBinding::Local(_)));
        assert!(matches!(preview.right_binding, EndpointBinding::Local(_)));
        assert!(preview.stage_blocker.contains("providerweisen"));
        assert!(!baseline.exists());
        assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
    }

    fn sftp_test_endpoints(
        fixture: &Fixture,
    ) -> (crate::ProviderEndpoint, crate::ProviderEndpoint) {
        (
            crate::ProviderEndpoint::local(&fixture.left).unwrap(),
            crate::ProviderEndpoint::test_sftp_local(&fixture.right).unwrap(),
        )
    }

    fn remote_test_endpoints(
        fixture: &Fixture,
        kind: crate::ProviderKind,
    ) -> (crate::ProviderEndpoint, crate::ProviderEndpoint) {
        (
            crate::ProviderEndpoint::local(&fixture.left).unwrap(),
            crate::ProviderEndpoint::test_remote_local(kind, &fixture.right).unwrap(),
        )
    }

    fn initialize_remote_test_pair(
        fixture: &Fixture,
        kind: crate::ProviderKind,
        pair: PairId,
        baseline: &Path,
    ) {
        let (left, right) = remote_test_endpoints(fixture, kind);
        let prepared = prepare_provider_bidirectional_with_decisions(
            pair,
            "Remote-Testpaar",
            left,
            right,
            baseline,
            &[],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(baseline).unwrap();
    }

    #[test]
    fn ftps_und_webdav_duerfen_nur_in_ein_starkes_lokales_ziel_wirken() {
        for (index, kind) in [crate::ProviderKind::Ftps, crate::ProviderKind::WebDav]
            .into_iter()
            .enumerate()
        {
            let fixture = Fixture::new();
            let pair = PairId([100 + index as u8; 16]);
            let baseline = fixture._temp.path().join("state/source-only.json");
            initialize_remote_test_pair(&fixture, kind, pair, &baseline);
            write(&fixture.right.join("remote.txt"), "remote");

            let (left, right) = remote_test_endpoints(&fixture, kind);
            let prepared = prepare_provider_bidirectional_with_decisions(
                pair,
                "Remote-Testpaar",
                left,
                right,
                &baseline,
                &[],
            )
            .unwrap();
            assert!(prepared.preview().stage_blocker.is_none());
            let phrase = prepared.preview().confirmation_phrase.clone();
            prepared
                .confirm(&phrase, true)
                .unwrap()
                .execute(&crate::Cancel::new(), &mut ignore_progress)
                .unwrap();
            finish_bidirectional_run_completion(&baseline).unwrap();
            assert_eq!(
                std::fs::read(fixture.left.join("remote.txt")).unwrap(),
                b"remote"
            );

            write(&fixture.left.join("lokal.txt"), "lokal");
            let (left, right) = remote_test_endpoints(&fixture, kind);
            let blocked = prepare_provider_bidirectional_with_decisions(
                pair,
                "Remote-Testpaar",
                left,
                right,
                &baseline,
                &[],
            )
            .unwrap();
            assert!(blocked.preview().stage_blocker.is_some());
            assert!(blocked.confirm("beliebig", true).is_err());
            assert!(!fixture.right.join("lokal.txt").exists());
            assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
        }
    }

    #[test]
    fn smb_matrix_erlaubt_additionen_und_dateiersetzungen_in_beide_richtungen() {
        let fixture = Fixture::new();
        let pair = PairId([102; 16]);
        let baseline = fixture._temp.path().join("state/smb-pair.json");
        initialize_remote_test_pair(&fixture, crate::ProviderKind::Smb, pair, &baseline);
        write(&fixture.left.join("links"), "links eins");
        write(&fixture.right.join("rechts"), "rechts eins");

        let run = |fixture: &Fixture, baseline: &Path| {
            let (left, right) = remote_test_endpoints(fixture, crate::ProviderKind::Smb);
            let prepared = prepare_provider_bidirectional_with_decisions(
                pair,
                "SMB-Testpaar",
                left,
                right,
                baseline,
                &[],
            )
            .unwrap();
            assert!(prepared.preview().stage_blocker.is_none());
            let phrase = prepared.preview().confirmation_phrase.clone();
            prepared
                .confirm(&phrase, true)
                .unwrap()
                .execute(&crate::Cancel::new(), &mut ignore_progress)
                .unwrap();
            finish_bidirectional_run_completion(baseline).unwrap();
        };
        run(&fixture, &baseline);
        write(&fixture.left.join("links"), "links zwei");
        write(&fixture.right.join("rechts"), "rechts zwei");
        run(&fixture, &baseline);
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(std::fs::read(root.join("links")).unwrap(), b"links zwei");
            assert_eq!(std::fs::read(root.join("rechts")).unwrap(), b"rechts zwei");
        }
    }

    #[test]
    fn smb_paar_bindet_ohne_posix_erweiterung_keine_modusunterschiede() {
        let fixture = Fixture::new();
        write(&fixture.left.join("gleich"), "inhalt");
        write(&fixture.right.join("gleich"), "inhalt");
        std::fs::set_permissions(
            fixture.left.join("gleich"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("gleich"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let (mut left, mut right) = fixture.snapshots();
        right.root_identity = RootIdentity::Smb(crate::remote::SmbRootIdentity {
            server: crate::remote::SmbServerIdentity {
                host: "smb.example.test".into(),
                port: 445,
                server_guid: [1; 16],
                share: "Test".into(),
                account_digest: [2; 32],
                canonical_root: "/".into(),
                dialect: 0x0311,
                signing_active: true,
                encryption_active: true,
                signing_algorithm: "AES-GMAC".into(),
                encryption_cipher: "AES-128-GCM".into(),
            },
            virtual_root: "/".into(),
            root_file_id: 1,
            tree_id: 1,
            session_generation: 1,
        });
        assert_ne!(
            left.entries[0].entry.metadata_digest,
            right.entries[0].entry.metadata_digest
        );

        normalize_pair_portable_metadata(&mut left, &mut right);

        assert_eq!(
            left.entries[0].entry.metadata_digest,
            portable_metadata_digest(None)
        );
        assert!(left.entries[0]
            .entry
            .semantically_equal(&right.entries[0].entry));
    }

    #[test]
    fn remote_typwechsel_darf_nur_ein_lokales_ziel_atomar_tauschen() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            write(&root.join("wechsel"), "alt");
        }
        let pair = PairId([103; 16]);
        let baseline = fixture._temp.path().join("state/remote-type-local.json");
        initialize_remote_test_pair(&fixture, crate::ProviderKind::Ftps, pair, &baseline);
        std::fs::remove_file(fixture.right.join("wechsel")).unwrap();
        std::fs::create_dir(fixture.right.join("wechsel")).unwrap();
        write(&fixture.right.join("wechsel/remote.txt"), "remote");

        let (left, right) = remote_test_endpoints(&fixture, crate::ProviderKind::Ftps);
        let prepared = prepare_provider_bidirectional_with_decisions(
            pair,
            "Remote-Typpaar",
            left,
            right,
            &baseline,
            &[resolution("wechsel", ConflictDecision::KeepRight)],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline).unwrap();
        assert!(fixture.left.join("wechsel").is_dir());
        assert_eq!(
            std::fs::read(fixture.left.join("wechsel/remote.txt")).unwrap(),
            b"remote"
        );

        let blocked = Fixture::new();
        for root in [&blocked.left, &blocked.right] {
            write(&root.join("wechsel"), "alt");
        }
        let blocked_pair = PairId([104; 16]);
        let blocked_baseline = blocked._temp.path().join("state/remote-type-blocked.json");
        initialize_remote_test_pair(
            &blocked,
            crate::ProviderKind::Ftps,
            blocked_pair,
            &blocked_baseline,
        );
        std::fs::remove_file(blocked.left.join("wechsel")).unwrap();
        std::fs::create_dir(blocked.left.join("wechsel")).unwrap();
        write(&blocked.left.join("wechsel/lokal.txt"), "lokal");
        let (left, right) = remote_test_endpoints(&blocked, crate::ProviderKind::Ftps);
        let prepared = prepare_provider_bidirectional_with_decisions(
            blocked_pair,
            "Remote-Typpaar",
            left,
            right,
            &blocked_baseline,
            &[resolution("wechsel", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_some());
        assert!(prepared.confirm("beliebig", true).is_err());
        assert!(blocked.right.join("wechsel").is_file());
        assert!(load_bidirectional_run_journal(&blocked_baseline)
            .unwrap()
            .is_none());
    }

    fn create_interrupted_remote_type_swap(fixture: &Fixture, pair: PairId, baseline: &Path) {
        for root in [&fixture.left, &fixture.right] {
            write(&root.join("wechsel"), "alt");
        }
        initialize_remote_test_pair(fixture, crate::ProviderKind::Ftps, pair, baseline);
        std::fs::remove_file(fixture.right.join("wechsel")).unwrap();
        std::fs::create_dir(fixture.right.join("wechsel")).unwrap();
        write(&fixture.right.join("wechsel/remote.txt"), "remote");

        let (left, right) = remote_test_endpoints(fixture, crate::ProviderKind::Ftps);
        let prepared = prepare_provider_bidirectional_with_decisions(
            pair,
            "Remote-Typ-Recovery",
            left,
            right,
            baseline,
            &[resolution("wechsel", ConflictDecision::KeepRight)],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            left_root,
            right_root,
            left_vfs,
            right_vfs,
            store,
            runs,
            run_ordinals,
            deferred_run_ordinals,
            ..
        } = run;
        assert_eq!(run_ordinals, vec![0]);
        assert_eq!(deferred_run_ordinals, vec![1]);

        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                journal.projections[1].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        execute_local_type_swap_projection(
            &store,
            1,
            LocalEndpointPair {
                left_vfs: left_vfs.as_ref(),
                left_root: &left_root,
                right_vfs: right_vfs.as_ref(),
                right_root: &right_root,
            },
        )
        .unwrap();
        // Der simulierte Prozessabbruch liegt nach dem atomaren Tausch, aber vor dem
        // Commit des Projektionszustands. Die alte Baseline bleibt daher unverändert.
        drop(store);
        assert_eq!(
            std::fs::read(fixture.left.join("wechsel/remote.txt")).unwrap(),
            b"remote"
        );
        assert_eq!(
            BaselineStore::open(baseline)
                .unwrap()
                .load()
                .unwrap()
                .unwrap()
                .generation,
            1
        );
    }

    #[test]
    fn remote_typwechsel_resume_verbindet_frisch_und_committet_erst_am_ende() {
        let fixture = Fixture::new();
        let pair = PairId([105; 16]);
        let baseline = fixture._temp.path().join("state/remote-type-resume.json");
        create_interrupted_remote_type_swap(&fixture, pair, &baseline);

        let (left, right) = remote_test_endpoints(&fixture, crate::ProviderKind::Ftps);
        let resumed =
            prepare_resume_provider_bidirectional("Remote-Typ-Recovery", left, right, &baseline)
                .unwrap();
        let phrase = resumed.preview().confirmation_phrase.clone();
        let stats = resumed
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline).unwrap();

        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(
            std::fs::read(fixture.left.join("wechsel/remote.txt")).unwrap(),
            b"remote"
        );
        assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
    }

    #[test]
    fn remote_typwechsel_rollback_stellt_lokalen_vorzustand_vollstaendig_her() {
        let fixture = Fixture::new();
        let pair = PairId([106; 16]);
        let baseline = fixture._temp.path().join("state/remote-type-rollback.json");
        create_interrupted_remote_type_swap(&fixture, pair, &baseline);

        let (left, right) = remote_test_endpoints(&fixture, crate::ProviderKind::Ftps);
        let prepared =
            prepare_provider_bidirectional_rollback("Remote-Typ-Recovery", left, right, &baseline)
                .unwrap();
        assert_eq!(prepared.preview().committed_projections, 2);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();

        assert_eq!(stats.projections_rolled_back, 2);
        assert_eq!(std::fs::read(fixture.left.join("wechsel")).unwrap(), b"alt");
        assert_eq!(
            std::fs::read(fixture.right.join("wechsel/remote.txt")).unwrap(),
            b"remote"
        );
        assert_eq!(
            BaselineStore::open(&baseline)
                .unwrap()
                .load()
                .unwrap()
                .unwrap()
                .generation,
            1
        );
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline).unwrap(),
            BidirectionalRunRecovery::RollbackCompletionCleanup
        );
        let (left, right) = remote_test_endpoints(&fixture, crate::ProviderKind::Ftps);
        let resume_error =
            prepare_resume_provider_bidirectional("Remote-Typ-Recovery", left, right, &baseline)
                .err()
                .expect("Provider-Resume darf RollbackCommitted nicht öffnen");
        assert_eq!(resume_error.kind(), io::ErrorKind::AlreadyExists);
        let (left, right) = remote_test_endpoints(&fixture, crate::ProviderKind::Ftps);
        let rollback_error =
            prepare_provider_bidirectional_rollback("Remote-Typ-Recovery", left, right, &baseline)
                .err()
                .expect("Provider-Rollback darf RollbackCommitted nicht erneut ausführen");
        assert_eq!(rollback_error.kind(), io::ErrorKind::AlreadyExists);
        finish_bidirectional_rollback_completion(&baseline).unwrap();
        assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
    }

    fn initialize_sftp_test_pair(
        fixture: &Fixture,
        pair: PairId,
        baseline: &Path,
    ) -> LocalBidirectionalStats {
        let (left, right) = sftp_test_endpoints(fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Testpaar",
            left,
            right,
            baseline,
            &[],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(baseline).unwrap();
        stats
    }

    #[test]
    fn additive_sftp_stufe_konvergiert_und_committet_baseline() {
        let fixture = Fixture::new();
        let pair = PairId([82; 16]);
        let baseline = fixture._temp.path().join("state/sftp-pair.json");
        assert_eq!(
            initialize_sftp_test_pair(&fixture, pair, &baseline).baseline_generation,
            1
        );
        write(&fixture.left.join("von-links"), "links");
        write(&fixture.right.join("von-rechts"), "rechts");

        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Testpaar",
            left,
            right,
            &baseline,
            &[],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_none());
        assert!(prepared.preview().backup_confirmation_required);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline).unwrap();

        assert_eq!(stats.baseline_generation, 2);
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(std::fs::read(root.join("von-links")).unwrap(), b"links");
            assert_eq!(std::fs::read(root.join("von-rechts")).unwrap(), b"rechts");
        }
    }

    #[test]
    fn sftp_dateiersetzungen_laufen_in_beiden_richtungen_ueber_quarantaene() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            write(&root.join("links-gewinnt"), "alt-links");
            write(&root.join("rechts-gewinnt"), "alt-rechts");
        }
        let pair = PairId([85; 16]);
        let baseline = fixture._temp.path().join("state/sftp-update.json");
        initialize_sftp_test_pair(&fixture, pair, &baseline);
        write(&fixture.left.join("links-gewinnt"), "neu-links");
        write(&fixture.right.join("rechts-gewinnt"), "neu-rechts");

        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Testpaar",
            left,
            right,
            &baseline,
            &[],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline).unwrap();
        assert_eq!(stats.baseline_generation, 2);
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(
                std::fs::read(root.join("links-gewinnt")).unwrap(),
                b"neu-links"
            );
            assert_eq!(
                std::fs::read(root.join("rechts-gewinnt")).unwrap(),
                b"neu-rechts"
            );
        }
        assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
    }

    #[test]
    fn sftp_verzeichnisersetzung_bleibt_ohne_starke_namespace_dauerhaftigkeit_gesperrt() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::create_dir(root.join("ordner")).unwrap();
            std::fs::set_permissions(root.join("ordner"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let pair = PairId([86; 16]);
        let baseline = fixture
            ._temp
            .path()
            .join("state/sftp-directory-blocked.json");
        initialize_sftp_test_pair(&fixture, pair, &baseline);
        std::fs::set_permissions(
            fixture.left.join("ordner"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("ordner"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();

        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Testpaar",
            left,
            right,
            &baseline,
            &[resolution("ordner", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_some());
        assert!(prepared.confirm("beliebig", true).is_err());
        assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
    }

    #[test]
    fn sftp_verzeichnisersetzung_darf_in_ein_starkes_lokales_ziel_laufen() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::create_dir(root.join("ordner")).unwrap();
            std::fs::set_permissions(root.join("ordner"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            write(&root.join("ordner/inhalt"), "gleich");
        }
        let pair = PairId([107; 16]);
        let baseline = fixture._temp.path().join("state/sftp-directory-local.json");
        initialize_sftp_test_pair(&fixture, pair, &baseline);
        std::fs::set_permissions(
            fixture.right.join("ordner"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();

        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Testpaar",
            left,
            right,
            &baseline,
            &[],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline).unwrap();
        assert_eq!(
            std::fs::metadata(fixture.left.join("ordner"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(fixture.right.join("ordner"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    fn fill_sftp_deletion_fixture(fixture: &Fixture) {
        for index in 0..24 {
            let name = format!("bestand-{index:02}");
            for root in [&fixture.left, &fixture.right] {
                write(&root.join(&name), "gleich");
            }
        }
    }

    #[test]
    fn sftp_loeschung_wird_ausschliesslich_in_starkes_lokales_ziel_propagiert() {
        let fixture = Fixture::new();
        fill_sftp_deletion_fixture(&fixture);
        let pair = PairId([87; 16]);
        let baseline = fixture._temp.path().join("state/sftp-delete-local.json");
        initialize_sftp_test_pair(&fixture, pair, &baseline);
        std::fs::remove_file(fixture.right.join("bestand-00")).unwrap();

        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Löschpaar",
            left,
            right,
            &baseline,
            &[],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline).unwrap();
        assert!(!fixture.left.join("bestand-00").exists());
        assert_eq!(stats.runs.len(), 1);
        assert_eq!(stats.runs[0].files_deleted, 1);
        let quarantine = PathBuf::from(stats.runs[0].quarantine.as_ref().unwrap());
        assert_eq!(
            std::fs::read(quarantine.join("bestand-00")).unwrap(),
            b"gleich"
        );
        let committed = BaselineStore::open(&baseline)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert!(committed.tombstones.iter().any(|record| {
            record.path.display_lossy() == "bestand-00"
                && record.direction == DeletionDirection::RightToLeft
        }));
    }

    #[test]
    fn lokale_loeschung_bleibt_in_richtung_sftp_ohne_verzeichnis_fsync_gesperrt() {
        let fixture = Fixture::new();
        fill_sftp_deletion_fixture(&fixture);
        let pair = PairId([88; 16]);
        let baseline = fixture._temp.path().join("state/sftp-delete-blocked.json");
        initialize_sftp_test_pair(&fixture, pair, &baseline);
        std::fs::remove_file(fixture.left.join("bestand-00")).unwrap();

        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Löschpaar",
            left,
            right,
            &baseline,
            &[],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_some());
        assert!(prepared.confirm("beliebig", true).is_err());
        assert!(fixture.right.join("bestand-00").exists());
        assert!(load_bidirectional_run_journal(&baseline).unwrap().is_none());
    }

    fn create_interrupted_sftp_addition(fixture: &Fixture, pair: PairId, baseline: &Path) {
        initialize_sftp_test_pair(fixture, pair, baseline);
        write(&fixture.left.join("von-links"), "links");
        write(&fixture.right.join("von-rechts"), "rechts");
        let (left, right) = sftp_test_endpoints(fixture);
        let prepared = prepare_sftp_bidirectional_with_decisions(
            pair,
            "SFTP-Recovery",
            left,
            right,
            baseline,
            &[],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        write(&fixture.left.join("von-rechts"), "fremd");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        assert_eq!(
            std::fs::read(fixture.right.join("von-links")).unwrap(),
            b"links"
        );
        std::fs::remove_file(fixture.left.join("von-rechts")).unwrap();
    }

    #[test]
    fn additiver_sftp_teilcommit_wird_mit_frischen_endpunkten_fortgesetzt() {
        let fixture = Fixture::new();
        let pair = PairId([83; 16]);
        let baseline = fixture._temp.path().join("state/sftp-resume.json");
        create_interrupted_sftp_addition(&fixture, pair, &baseline);
        let (left, right) = sftp_test_endpoints(&fixture);
        let prepared =
            prepare_resume_sftp_bidirectional("SFTP-Recovery", left, right, &baseline).unwrap();
        assert_eq!(prepared.prepared_run_ordinals, vec![1]);
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline).unwrap();
        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(
            std::fs::read(fixture.left.join("von-rechts")).unwrap(),
            b"rechts"
        );
    }

    #[test]
    fn additiver_sftp_teilcommit_rollt_in_manifestierte_quarantaene_zurueck() {
        let fixture = Fixture::new();
        let pair = PairId([84; 16]);
        let baseline = fixture._temp.path().join("state/sftp-rollback.json");
        create_interrupted_sftp_addition(&fixture, pair, &baseline);
        let (left, right) = sftp_test_endpoints(&fixture);
        let rollback =
            prepare_sftp_bidirectional_rollback("SFTP-Recovery", left, right, &baseline).unwrap();
        assert_eq!(rollback.preview().committed_projections, 1);
        let phrase = rollback.preview().confirmation_phrase.clone();
        let stats = rollback
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert_eq!(stats.projections_rolled_back, 1);
        assert!(!fixture.right.join("von-links").exists());
        assert_eq!(
            std::fs::read(Path::new(&stats.rollback_quarantines[0]).join("von-links")).unwrap(),
            b"links"
        );
        assert_eq!(
            BaselineStore::open(&baseline)
                .unwrap()
                .load()
                .unwrap()
                .unwrap()
                .generation,
            1
        );
    }

    fn write(path: &Path, value: &str) {
        std::fs::write(path, value).unwrap();
    }

    fn baseline(
        fixture: &Fixture,
        pair: PairId,
    ) -> (BidirectionalBaseline, EndpointBinding, EndpointBinding) {
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        (
            BidirectionalBaseline::from_identical_snapshots(
                pair,
                bindings.0.clone(),
                bindings.1.clone(),
                &snapshots.0,
                &snapshots.1,
            )
            .unwrap(),
            bindings.0,
            bindings.1,
        )
    }

    #[test]
    fn identische_erstbaseline_ist_digestgebunden() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "gleich");
        write(&fixture.right.join("a"), "gleich");
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let baseline = BidirectionalBaseline::from_identical_snapshots(
            PairId([1; 16]),
            bindings.0,
            bindings.1,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        baseline.validate().unwrap();
        assert_eq!(baseline.generation, 1);
    }

    fn legacy_test_remote_binding(identity_byte: u8) -> EndpointBinding {
        EndpointBinding::Sftp(RemoteEndpointBinding {
            schema_version: 1,
            canonical_root: format!("/daten/{identity_byte}"),
            account_digest: [3; 32],
            public_identity_digest: [identity_byte; 32],
        })
    }

    fn remove_binding_schema(value: &mut serde_json::Value, field: &str) {
        value[field]["identity"]
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
    }

    #[test]
    fn echte_schemafreie_version_eins_baseline_bleibt_ladbar() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/legacy-baseline.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut baseline = BidirectionalBaseline {
            version: LEGACY_BASELINE_VERSION,
            pair_id: PairId([91; 16]),
            generation: 4,
            left_binding: legacy_test_remote_binding(4),
            right_binding: legacy_test_remote_binding(5),
            metadata_profile: MetadataProfile::Portable,
            entries: Vec::new(),
            tombstones: Vec::new(),
            body_digest: [0; 32],
        };
        baseline.body_digest = baseline.canonical_digest().unwrap();
        let mut payload = serde_json::to_value(&baseline).unwrap();
        remove_binding_schema(&mut payload, "left_binding");
        remove_binding_schema(&mut payload, "right_binding");
        std::fs::write(&path, serde_json::to_vec_pretty(&payload).unwrap()).unwrap();

        let loaded = BaselineStore::open(path).unwrap().load().unwrap().unwrap();
        assert_eq!(loaded, baseline);
        loaded.validate().unwrap();
    }

    #[test]
    fn echtes_schemafreies_version_sieben_laufjournal_bleibt_ladbar() {
        let fixture = Fixture::new();
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let plan = plan_bidirectional(
            PairId([92; 16]),
            &bindings.0,
            &bindings.1,
            None,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        let mut journal = BidirectionalRunJournal::new(
            &plan,
            legacy_test_remote_binding(6),
            legacy_test_remote_binding(7),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        journal.version = LEGACY_RUN_JOURNAL_VERSION;
        journal.body_digest = journal.canonical_digest().unwrap();
        let mut payload = serde_json::to_value(&journal).unwrap();
        remove_binding_schema(&mut payload, "left_binding");
        remove_binding_schema(&mut payload, "right_binding");
        let baseline_path = fixture._temp.path().join("state/legacy-journal.json");
        let store = BaselineStore::open(&baseline_path).unwrap();
        std::fs::write(
            store.run_journal_path(),
            serde_json::to_vec_pretty(&payload).unwrap(),
        )
        .unwrap();

        let loaded = store.load_run_journal().unwrap().unwrap();
        assert_eq!(loaded, journal);
        loaded.validate().unwrap();
    }

    #[test]
    fn erstbaseline_lehnt_vertauschte_endpunktbindungen_ab() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "gleich");
        write(&fixture.right.join("a"), "gleich");
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);

        assert!(BidirectionalBaseline::from_identical_snapshots(
            PairId([17; 16]),
            bindings.1,
            bindings.0,
            &snapshots.0,
            &snapshots.1,
        )
        .is_err());
    }

    #[test]
    fn snapshot_validation_bindet_dateisumme_und_portables_profil() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "inhalt");
        let mut snapshot = fixture.snapshots().0;

        snapshot.total_file_bytes += 1;
        assert!(snapshot.validate().is_err());

        snapshot.total_file_bytes -= 1;
        snapshot.metadata_profile = MetadataProfile::NativeStrict;
        snapshot.snapshot_digest = snapshot_digest(snapshot.metadata_profile, &snapshot.entries);
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn unterschiedlicher_erster_lauf_bleibt_konflikt() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let plan = plan_bidirectional(
            PairId([2; 16]),
            &bindings.0,
            &bindings.1,
            None,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert_eq!(plan.conflicts, 1);
        assert!(!plan.executable);
    }

    #[test]
    fn journal_lehnt_widerspruechliche_abgeleitete_planfelder_ab() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let plan = plan_bidirectional(
            PairId([18; 16]),
            &bindings.0,
            &bindings.1,
            None,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        let mut journal =
            BidirectionalRunJournal::new(&plan, bindings.0, bindings.1, &snapshots.0, &snapshots.1)
                .unwrap();
        journal.plan.conflicts = 0;
        journal.plan.executable = true;
        journal.body_digest = journal.canonical_digest().unwrap();

        assert!(journal.validate().is_err());
    }

    #[test]
    fn einseitige_aenderung_wird_in_richtige_richtung_geplant() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([3; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        write(&fixture.left.join("a"), "neu");
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Copy {
                direction: SyncDirection::LeftToRight,
                ..
            }]
        ));
    }

    #[test]
    fn gleiche_beidseitige_aenderung_konvergiert_ohne_kopie() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([4; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        write(&fixture.left.join("a"), "neu");
        write(&fixture.right.join("a"), "neu");
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Converged { .. }]
        ));
    }

    #[test]
    fn verschiedene_beidseitige_aenderungen_sind_konflikt() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([5; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Conflict {
                kind: ConflictKind::BothChanged,
                ..
            }]
        ));
    }

    #[test]
    fn loeschung_gegen_unveraendert_und_aenderung_werden_getrennt() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([6; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        std::fs::remove_file(fixture.left.join("a")).unwrap();
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Delete {
                direction: SyncDirection::LeftToRight,
                ..
            }]
        ));
        write(&fixture.right.join("a"), "rechts-neu");
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Conflict {
                kind: ConflictKind::ChangedAgainstDeletion,
                ..
            }]
        ));
    }

    #[test]
    fn beidseitige_loeschung_erzeugt_tombstone_aktion() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([7; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        std::fs::remove_file(fixture.left.join("a")).unwrap();
        std::fs::remove_file(fixture.right.join("a")).unwrap();
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Tombstone { .. }]
        ));
    }

    #[test]
    fn typwechsel_wird_nicht_automatisch_uebertragen() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([8; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        std::fs::remove_file(fixture.left.join("a")).unwrap();
        std::fs::create_dir(fixture.left.join("a")).unwrap();
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Conflict {
                kind: ConflictKind::TypeChanged,
                ..
            }]
        ));
    }

    #[test]
    fn portable_rechte_zaehlen_als_aenderung() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "gleich");
        write(&fixture.right.join("a"), "gleich");
        let pair = PairId([9; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        std::fs::set_permissions(
            fixture.left.join("a"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Copy {
                direction: SyncDirection::LeftToRight,
                ..
            }]
        ));
    }

    #[test]
    fn mtime_allein_aendert_snapshotbindung_aber_nicht_semantik() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "gleich");
        write(&fixture.right.join("a"), "gleich");
        let pair = PairId([11; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        let before = fixture.snapshots();
        filetime::set_file_mtime(
            fixture.left.join("a"),
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let after = fixture.snapshots();
        assert_ne!(before.0.snapshot_digest, after.0.snapshot_digest);
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &after.0,
            &after.1,
        )
        .unwrap();
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn neuer_pfad_nach_baseline_wird_kopiert_aber_beim_ersten_lauf_nicht() {
        let fixture = Fixture::new();
        let pair = PairId([12; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        write(&fixture.left.join("neu"), "inhalt");
        let snapshots = fixture.snapshots();
        let initial = plan_bidirectional(
            PairId([13; 16]),
            &left_binding,
            &right_binding,
            None,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert_eq!(initial.conflicts, 1);
        let continued = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            continued.actions.as_slice(),
            [BidirectionalAction::Copy {
                direction: SyncDirection::LeftToRight,
                ..
            }]
        ));
    }

    #[test]
    fn tombstone_verhindert_unbemerkte_wiederbelebung() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([14; 16]);
        let (mut base, left_binding, right_binding) = baseline(&fixture, pair);
        let previous = base.entries.remove(0);
        base.tombstones.push(Tombstone {
            path: previous.path.clone(),
            previous_entry_digest: previous.entry.entry_digest,
            baseline_generation: base.generation,
            direction: DeletionDirection::Both,
            committed_unix_ms: 1_700_000_000_000,
        });
        base.body_digest = base.canonical_digest().unwrap();
        std::fs::remove_file(fixture.right.join("a")).unwrap();
        let snapshots = fixture.snapshots();
        let plan = plan_bidirectional(
            pair,
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(matches!(
            plan.actions.as_slice(),
            [BidirectionalAction::Conflict {
                kind: ConflictKind::RecreatedAfterDeletion,
                ..
            }]
        ));
    }

    #[test]
    fn falsche_paar_oder_rootbindung_stoppt_vor_planung() {
        let fixture = Fixture::new();
        let pair = PairId([15; 16]);
        let (base, left_binding, right_binding) = baseline(&fixture, pair);
        let snapshots = fixture.snapshots();
        assert!(plan_bidirectional(
            PairId([16; 16]),
            &left_binding,
            &right_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .is_err());
        assert!(plan_bidirectional(
            pair,
            &right_binding,
            &left_binding,
            Some(&base),
            &snapshots.0,
            &snapshots.1,
        )
        .is_err());
    }

    #[test]
    fn baseline_speicher_ist_atomisch_gesperrt_und_prueft_digest() {
        let fixture = Fixture::new();
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let baseline = BidirectionalBaseline::from_identical_snapshots(
            PairId([10; 16]),
            bindings.0,
            bindings.1,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        let path = fixture._temp.path().join("state/pair.json");
        let store = BaselineStore::open(&path).unwrap();
        assert!(BaselineStore::open(&path).is_err());
        store.save(&baseline).unwrap();
        assert_eq!(store.load().unwrap(), Some(baseline.clone()));
        #[cfg(unix)]
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut damaged = baseline;
        damaged.generation += 1;
        std::fs::write(&path, serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(store.load().is_err());
        damaged.version = 99;
        std::fs::write(&path, serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(store.load().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn baseline_und_laufjournal_werten_symlinkfehler_nicht_als_abwesenheit() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let baseline_path = temp.path().join("state/pair.json");
        let store = BaselineStore::open(&baseline_path).unwrap();

        symlink("pair.json", &baseline_path).unwrap();
        assert_eq!(store.load().unwrap_err().kind(), io::ErrorKind::InvalidData);
        std::fs::remove_file(&baseline_path).unwrap();

        let journal_path = store.run_journal_path();
        symlink("pair.run.json", &journal_path).unwrap();
        assert_eq!(
            store.load_run_journal().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn verschiedene_baselines_im_selben_ordner_blockieren_einander_nicht() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let first = BaselineStore::open(state.join("pair-a.json")).unwrap();
        let second = BaselineStore::open(state.join("pair-b.json"))
            .expect("verschiedene Paare brauchen unabhängige Baseline-Sperren");

        assert_ne!(first.path, second.path);
    }

    #[test]
    fn baseline_lockpfad_folgt_case_alias_und_bleibt_non_utf8_sicher() {
        let parent = Path::new("/simuliert-case-insensitiv");
        let lower = baseline_lock_path(&parent.join("pair.json")).unwrap();
        let upper = baseline_lock_path(&parent.join("Pair.json")).unwrap();
        let other = baseline_lock_path(&parent.join("anderes.json")).unwrap();
        let simulated_alias = |path: &Path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .to_ascii_lowercase()
        };
        assert_eq!(simulated_alias(&lower), simulated_alias(&upper));
        assert_ne!(simulated_alias(&lower), simulated_alias(&other));

        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::{OsStrExt, OsStringExt};

            let name = OsString::from_vec(vec![0xff, b'.', b'j', b's', b'o', b'n']);
            let lock = baseline_lock_path(&parent.join(name)).unwrap();
            assert!(lock.as_os_str().as_bytes().ends_with(b".json.lock"));
        }
    }

    #[test]
    fn baseline_lock_wird_bei_geerbtem_fd_am_besitzerende_frei() {
        const CHILD_MODE: &str = "DOPPELDECKER_BASELINE_FORK_TEST";
        if std::env::var_os(CHILD_MODE).is_none() {
            // Ein eigener Testprozess verhindert, dass der Fork FDs anderer
            // parallel laufender Tests erbt und deren Locks vorübergehend hält.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bidirectional::tests::baseline_lock_wird_bei_geerbtem_fd_am_besitzerende_frei",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD_MODE, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pair.json");
        let store = BaselineStore::open(&path).unwrap();
        let mut ready = [0; 2];
        let mut release = [0; 2];
        // Nach dem Fork führt das Kind ausschließlich async-signal-sichere
        // Syscalls aus und hält die geerbten FDs bis zur Freigabe offen.
        unsafe {
            assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
            assert_eq!(libc::pipe(release.as_mut_ptr()), 0);
            let child = libc::fork();
            assert!(child >= 0);
            if child == 0 {
                libc::alarm(5);
                libc::close(ready[0]);
                libc::close(release[1]);
                let byte = [1_u8];
                libc::write(ready[1], byte.as_ptr().cast(), 1);
                let mut buffer = [0_u8];
                libc::read(release[0], buffer.as_mut_ptr().cast(), 1);
                libc::_exit(0);
            }
            struct ForkGuard {
                pid: libc::pid_t,
                ready: i32,
                release: i32,
            }
            impl Drop for ForkGuard {
                fn drop(&mut self) {
                    unsafe {
                        libc::close(self.ready);
                        // EOF beendet genau unser noch unreaptes Kind.
                        libc::close(self.release);
                        while libc::waitpid(self.pid, std::ptr::null_mut(), 0) == -1 {
                            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                                break;
                            }
                        }
                    }
                }
            }
            libc::close(ready[1]);
            libc::close(release[0]);
            let _child = ForkGuard {
                pid: child,
                ready: ready[0],
                release: release[1],
            };
            let mut byte = [0_u8];
            assert_eq!(libc::read(ready[0], byte.as_mut_ptr().cast(), 1), 1);
            assert_eq!(
                BaselineStore::open(&path).err().unwrap().kind(),
                io::ErrorKind::WouldBlock
            );
            drop(store);
            assert!(
                BaselineStore::open(&path).is_ok(),
                "Geerbter FD darf den beendeten Baselinebesitz nicht verlängern"
            );
        }
    }

    #[test]
    fn geerbter_baseline_besitzer_gibt_den_elternlock_nicht_frei() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pair.json");
        let mut inherited = BaselineStore::open(&path).unwrap();
        let parent_descriptor = inherited.process_lock.try_clone().unwrap();
        // Simuliert die abweichende PID eines geerbten Objekts, während ein
        // Deskriptor des ursprünglichen Besitzers dieselbe flock-Bindung hält.
        inherited.lock_owner ^= 1;
        drop(inherited);
        assert_eq!(
            BaselineStore::open(&path).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(parent_descriptor);
        assert!(BaselineStore::open(&path).is_ok());
    }

    #[test]
    fn baseline_lock_bleibt_von_vorbereitung_bis_zum_lauf_gehalten() {
        let fixture = Fixture::new();
        let baseline_path = fixture._temp.path().join("state/held-pair.json");
        let prepared = prepare_local_bidirectional(
            PairId([93; 16]),
            "Gehaltenes Paar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert!(BaselineStore::open(&baseline_path).is_err());

        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        assert!(BaselineStore::open(&baseline_path).is_err());
        drop(run);
        assert!(BaselineStore::open(&baseline_path).is_ok());
    }

    fn ignore_progress(_: &crate::Progress) {}

    fn prepare_and_run(
        fixture: &Fixture,
        pair: PairId,
        baseline_path: &Path,
        backup: bool,
    ) -> LocalBidirectionalStats {
        let prepared = prepare_local_bidirectional(
            pair,
            "Testpaar",
            &fixture.left,
            &fixture.right,
            baseline_path,
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, backup)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(baseline_path).unwrap();
        stats
    }

    fn prepare_and_run_with_decisions(
        fixture: &Fixture,
        pair: PairId,
        baseline_path: &Path,
        backup: bool,
        resolutions: &[ConflictResolution],
    ) -> LocalBidirectionalStats {
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Konfliktpaar",
            &fixture.left,
            &fixture.right,
            baseline_path,
            resolutions,
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, backup)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(baseline_path).unwrap();
        stats
    }

    fn resolution(path: &str, decision: ConflictDecision) -> ConflictResolution {
        ConflictResolution {
            path: BinaryRelativePath(vec![path.as_bytes().to_vec()]),
            decision,
        }
    }

    /// Testinhalt fester Länge.
    ///
    /// Alle erzeugten Dateien sind gleich groß, damit sich zwei Bäume
    /// ausschließlich im Inhalt unterscheiden. Sonst würde schon der Größenvergleich
    /// die Projektionen auseinanderhalten und der Test bliebe blind dafür, ob die
    /// Recovery wirklich den Inhalt heranzieht.
    fn content(side: &str, index: usize) -> String {
        format!("{side}{index:04}")
    }

    /// Wie eine Pfadsituation vor dem Lauf aussieht. Der Produktionsplan darf
    /// gleichgerichtete Pfade zu einer Projektion bündeln; jede Lage erzeugt aber
    /// mindestens eine sichtbare Wirkung in einem der beiden Bäume.
    #[derive(Clone, Copy, Debug)]
    enum PathSituation {
        OnlyLeft,
        OnlyRight,
        ConflictKeepLeft,
        ConflictKeepRight,
    }

    fn path_situation() -> impl proptest::strategy::Strategy<Value = PathSituation> {
        proptest::prop_oneof![
            proptest::strategy::Just(PathSituation::OnlyLeft),
            proptest::strategy::Just(PathSituation::OnlyRight),
            proptest::strategy::Just(PathSituation::ConflictKeepLeft),
            proptest::strategy::Just(PathSituation::ConflictKeepRight),
        ]
    }

    /// Baut beide Bäume und die passenden Konfliktentscheidungen.
    fn build_situations(
        fixture: &Fixture,
        situations: &[PathSituation],
    ) -> Vec<ConflictResolution> {
        let mut resolutions = Vec::new();
        for (index, situation) in situations.iter().enumerate() {
            let name = format!("datei-{index}");
            match situation {
                PathSituation::OnlyLeft => {
                    write(&fixture.left.join(&name), &content("L", index));
                    // Ohne Baseline ist auch ein einseitig neuer Eintrag ein
                    // Erstkonflikt und braucht eine ausdrückliche Entscheidung.
                    resolutions.push(resolution(&name, ConflictDecision::KeepLeft));
                }
                PathSituation::OnlyRight => {
                    write(&fixture.right.join(&name), &content("R", index));
                    resolutions.push(resolution(&name, ConflictDecision::KeepRight));
                }
                PathSituation::ConflictKeepLeft | PathSituation::ConflictKeepRight => {
                    write(&fixture.left.join(&name), &content("L", index));
                    write(&fixture.right.join(&name), &content("R", index));
                    let decision = if matches!(situation, PathSituation::ConflictKeepLeft) {
                        ConflictDecision::KeepLeft
                    } else {
                        ConflictDecision::KeepRight
                    };
                    resolutions.push(resolution(&name, decision));
                }
            }
        }
        resolutions
    }

    /// So viele unberührte Dateien hält der Zweitlauf-Test vor, damit zwei
    /// Löschungen unter der 5-%-Grenze des Real-Mirrors bleiben.
    const ANCHOR_FILES: usize = 48;

    /// Was im zweiten Lauf mit einem Pfad passiert.
    #[derive(Clone, Copy, Debug)]
    enum SecondRunChange {
        Untouched,
        DeleteLeft,
        DeleteRight,
        ModifyLeft,
        ModifyRight,
    }

    fn second_run_change() -> impl proptest::strategy::Strategy<Value = SecondRunChange> {
        proptest::prop_oneof![
            proptest::strategy::Just(SecondRunChange::Untouched),
            proptest::strategy::Just(SecondRunChange::DeleteLeft),
            proptest::strategy::Just(SecondRunChange::DeleteRight),
            proptest::strategy::Just(SecondRunChange::ModifyLeft),
            proptest::strategy::Just(SecondRunChange::ModifyRight),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            // Jeder Fall legt zwei vollständige Bäume an und fährt zwei echte Läufe.
            cases: 16,
            .. proptest::test_runner::Config::default()
        })]

        /// Auch nach einem Teillauf MIT Baseline muss der Recovery-Präfix exakt
        /// die committeten Projektionen treffen.
        ///
        /// Erst der zweite Lauf erzeugt Lösch- und Ersetzungsprojektionen. Genau
        /// die tragen eine Quarantänewirkung: Würde das Resume eine davon
        /// fälschlich als erledigt überspringen, bliebe eine Löschung oder
        /// Ersetzung dauerhaft aus, ohne dass es jemand meldet.
        #[test]
        fn recovery_praefix_bleibt_nach_baseline_exakt(
            changes in proptest::collection::vec(second_run_change(), 1..5),
            committed_share in 0usize..=4,
            running_applied in proptest::bool::ANY,
        ) {
            // Mehr als zwei Löschungen sprengen die 5-%-Grenze des Real-Mirrors;
            // die ist eine eigene Regel und hier nicht der Prüfgegenstand.
            let deletions = changes
                .iter()
                .filter(|change| {
                    matches!(
                        change,
                        SecondRunChange::DeleteLeft | SecondRunChange::DeleteRight
                    )
                })
                .count();
            proptest::prop_assume!(deletions <= 2);

            let fixture = Fixture::new();
            let baseline_path = fixture._temp.path().join("state/property-second.json");

            // Erster Lauf: alle Pfade entstehen links und werden nach rechts
            // gespiegelt. Danach steht eine Baseline, gegen die der zweite Lauf
            // einseitige Änderungen ohne Konflikt einordnen kann.
            let mut first_resolutions = Vec::new();
            // Nie angetastete Ankerdateien. Sie halten beide Bäume nichtleer und
            // den Zielbestand groß genug, damit die getesteten Löschungen unter der
            // 5-%-Mirror-Grenze bleiben. Beide Schranken sind eigene, bereits
            // abgedeckte Sicherheitsregeln und nicht das Thema dieses Tests.
            for anchor in 0..ANCHOR_FILES {
                let name = format!("anker-{anchor}");
                write(&fixture.left.join(&name), "anker");
                first_resolutions.push(resolution(&name, ConflictDecision::KeepLeft));
            }
            for index in 0..changes.len() {
                let name = format!("datei-{index}");
                write(&fixture.left.join(&name), &content("A", index));
                first_resolutions.push(resolution(&name, ConflictDecision::KeepLeft));
            }
            let prepared = prepare_local_bidirectional_with_decisions(
                PairId([202; 16]),
                "Property-Zweitlauf",
                &fixture.left,
                &fixture.right,
                &baseline_path,
                &first_resolutions,
            )
            .unwrap();
            let phrase = prepared.preview().confirmation_phrase.clone();
            prepared
                .confirm(&phrase, true)
                .unwrap()
                .execute(&crate::Cancel::new(), &mut ignore_progress)
                .unwrap();
            // Ein abgeschlossener Lauf lässt sein Journal als gebundenen
            // Abschlussmarker stehen; erst dieser Aufruf gibt den Pfad frei.
            finish_bidirectional_run_completion(&baseline_path).unwrap();

            // Zweiter Lauf: einseitige Änderungen gegen die Baseline.
            for (index, change) in changes.iter().enumerate() {
                let name = format!("datei-{index}");
                match change {
                    SecondRunChange::Untouched => {}
                    SecondRunChange::DeleteLeft => {
                        std::fs::remove_file(fixture.left.join(&name)).unwrap()
                    }
                    SecondRunChange::DeleteRight => {
                        std::fs::remove_file(fixture.right.join(&name)).unwrap()
                    }
                    SecondRunChange::ModifyLeft => {
                        write(&fixture.left.join(&name), &content("L", index))
                    }
                    SecondRunChange::ModifyRight => {
                        write(&fixture.right.join(&name), &content("R", index))
                    }
                }
            }

            let prepared = prepare_local_bidirectional_with_decisions(
                PairId([202; 16]),
                "Property-Zweitlauf",
                &fixture.left,
                &fixture.right,
                &baseline_path,
                &[],
            )
            .unwrap();
            proptest::prop_assert!(
                prepared.preview().stage_blocker.is_none(),
                "die erzeugte Zweitlauflage darf nicht gesperrt sein: {:?}",
                prepared.preview().stage_blocker
            );
            let phrase = prepared.preview().confirmation_phrase.clone();
            let run = prepared.confirm(&phrase, true).unwrap();
            let LocalBidirectionalRun {
                store,
                runs,
                run_ordinals,
                ..
            } = run;
            let total = runs.len();
            proptest::prop_assume!(total > 0);
            // Es bleibt absichtlich immer eine nächste Projektion übrig. Nur dann
            // muss Recovery zwischen `Running` vor und nach deren Wirkung
            // unterscheiden; ein vollständig committeter Lauf besitzt diese
            // Mehrdeutigkeit nicht.
            let committed = committed_share % total;
            let mut pending_runs = runs.into_iter().enumerate();

            for (index, single) in pending_runs.by_ref().take(committed) {
                let ordinal = run_ordinals[index];
                store
                    .update_run_journal(|journal| {
                        journal.state = BidirectionalRunState::Running;
                        journal.projections[ordinal].state =
                            BidirectionalProjectionState::Running;
                        Ok(())
                    })
                    .unwrap();
                let result = single.execute(&crate::Cancel::new(), &mut ignore_progress).unwrap();
                store
                    .update_run_journal(|journal| {
                        journal.projections[ordinal].state =
                            BidirectionalProjectionState::Committed;
                        journal.projections[ordinal].quarantine = result.quarantine.clone();
                        Ok(())
                    })
                    .unwrap();
            }

            let (running_index, running) = pending_runs.next().unwrap();
            let running_ordinal = run_ordinals[running_index];
            store
                .update_run_journal(|journal| {
                    journal.state = BidirectionalRunState::Running;
                    journal.projections[running_ordinal].state =
                        BidirectionalProjectionState::Running;
                    Ok(())
                })
                .unwrap();
            if running_applied {
                // Simuliert den Crash nach der Datenwirkung, aber vor dem
                // Journalcommit. Der Quarantänepfad ist zu diesem Zeitpunkt
                // ebenfalls noch nicht in die Projektion zurückgeschrieben.
                running
                    .execute(&crate::Cancel::new(), &mut ignore_progress)
                    .unwrap();
            }

            let journal = store.load_run_journal().unwrap().unwrap();
            let (left_snapshot, right_snapshot) = fixture.snapshots();
            let prefix = recovery_prefix(&journal, &left_snapshot, &right_snapshot).unwrap();
            let expected = committed + usize::from(running_applied);

            proptest::prop_assert_eq!(
                prefix,
                expected,
                "Recovery-Präfix {} weicht vom erwarteten Präfix {} ab ({} committet, Running-Wirkung {}, Änderungen: {:?})",
                prefix,
                expected,
                committed,
                running_applied,
                changes
            );
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 48,
            // Die Fälle legen echte Verzeichnisbäume an; mehr Fälle kosten spürbar Zeit.
            .. proptest::test_runner::Config::default()
        })]

        /// Der Recovery-Präfix muss exakt die Zahl der tatsächlich committeten
        /// Projektionen treffen.
        ///
        /// `observed_projection_prefix` wählt unter allen passenden Präfixen das
        /// **größte**. Wäre eine geplante Projektion für den Baumvergleich
        /// unsichtbar, träfen zwei Präfixe gleichermaßen zu und das Resume würde
        /// eine nie ausgeführte Projektion als erledigt überspringen — also stillen
        /// Datenverlust erzeugen. Dieser Test führt eine zufällige Anzahl echter
        /// Projektionen aus und verlangt, dass das Resume genau dort wieder
        /// aufsetzt.
        #[test]
        fn recovery_praefix_trifft_genau_die_committeten_projektionen(
            situations in proptest::collection::vec(path_situation(), 1..5),
            committed_share in 0usize..=4,
            running_applied in proptest::bool::ANY,
        ) {
            let fixture = Fixture::new();
            let resolutions = build_situations(&fixture, &situations);
            let baseline_path = fixture._temp.path().join("state/property-resume.json");
            let prepared = prepare_local_bidirectional_with_decisions(
                PairId([201; 16]),
                "Property-Resume",
                &fixture.left,
                &fixture.right,
                &baseline_path,
                &resolutions,
            )
            .unwrap();
            proptest::prop_assert!(
                prepared.preview().stage_blocker.is_none(),
                "die erzeugte Lage darf nicht gesperrt sein"
            );
            let phrase = prepared.preview().confirmation_phrase.clone();
            let run = prepared.confirm(&phrase, true).unwrap();
            let LocalBidirectionalRun {
                store,
                runs,
                run_ordinals,
                ..
            } = run;
            let total = runs.len();
            proptest::prop_assume!(total > 0);
            let committed = committed_share % total;
            let mut pending_runs = runs.into_iter().enumerate();

            for (index, single) in pending_runs.by_ref().take(committed) {
                let ordinal = run_ordinals[index];
                store
                    .update_run_journal(|journal| {
                        journal.state = BidirectionalRunState::Running;
                        journal.projections[ordinal].state =
                            BidirectionalProjectionState::Running;
                        Ok(())
                    })
                    .unwrap();
                let result = single.execute(&crate::Cancel::new(), &mut ignore_progress).unwrap();
                store
                    .update_run_journal(|journal| {
                        journal.projections[ordinal].state =
                            BidirectionalProjectionState::Committed;
                        journal.projections[ordinal].quarantine = result.quarantine.clone();
                        Ok(())
                    })
                    .unwrap();
            }

            let (running_index, running) = pending_runs.next().unwrap();
            let running_ordinal = run_ordinals[running_index];
            store
                .update_run_journal(|journal| {
                    journal.state = BidirectionalRunState::Running;
                    journal.projections[running_ordinal].state =
                        BidirectionalProjectionState::Running;
                    Ok(())
                })
                .unwrap();
            if running_applied {
                running
                    .execute(&crate::Cancel::new(), &mut ignore_progress)
                    .unwrap();
            }

            let journal = store.load_run_journal().unwrap().unwrap();
            let (left_snapshot, right_snapshot) = fixture.snapshots();
            let prefix = recovery_prefix(&journal, &left_snapshot, &right_snapshot).unwrap();
            let expected = committed + usize::from(running_applied);

            proptest::prop_assert_eq!(
                prefix,
                expected,
                "Recovery-Präfix {} weicht vom erwarteten Präfix {} ab ({} committet, Running-Wirkung {}, Lage: {:?})",
                prefix,
                expected,
                committed,
                running_applied,
                situations
            );
        }
    }

    #[test]
    fn fehlgeschlagenes_discard_bleibt_mit_derselben_vorbereitung_wiederholbar() {
        let fixture = Fixture::new();
        let baseline_path = fixture._temp.path().join("state/discard-retry.json");
        let prepared = prepare_local_bidirectional(
            PairId([91; 16]),
            "Discard-Retry",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let journal = baseline_path.with_extension("run.json");
        let saved_journal = journal.with_extension("retry-backup");
        std::fs::rename(&journal, &saved_journal).unwrap();
        std::fs::create_dir(&journal).unwrap();

        assert!(prepared.discard().is_err());
        std::fs::remove_dir(&journal).unwrap();
        std::fs::rename(&saved_journal, &journal).unwrap();
        prepared.discard().unwrap();
        assert!(!journal.exists());
    }

    #[test]
    fn discard_retry_synct_den_elternordner_auch_nach_bereits_wirkendem_unlink() {
        use std::cell::Cell;

        let journal_exists = Cell::new(true);
        let sync_attempts = Cell::new(0_u8);
        let journal = Path::new("/test/pair.run.json");

        let first = remove_file_and_sync_parent_using(
            journal,
            |_| {
                journal_exists.set(false);
                Ok(())
            },
            |_| {
                sync_attempts.set(sync_attempts.get() + 1);
                Err(io::Error::other("Eltern-fsync meldet einen späten Fehler"))
            },
        );
        assert!(first.is_err());

        remove_file_and_sync_parent_using(
            journal,
            |_| {
                assert!(!journal_exists.get());
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Unlink hatte bereits gewirkt",
                ))
            },
            |_| {
                sync_attempts.set(sync_attempts.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(sync_attempts.get(), 2);
    }

    #[test]
    fn sichtbarer_laufjournalcommit_braucht_nachgeholte_durability() {
        use std::cell::Cell;

        let fixture = Fixture::new();
        write(&fixture.left.join("neu.txt"), "gebundener Inhalt");
        let baseline_path = fixture._temp.path().join("state/durability.json");
        let prepared = prepare_local_bidirectional_with_decisions(
            PairId([93; 16]),
            "Durability",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution("neu.txt", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        let before = prepared.store.load_run_journal().unwrap().unwrap();
        assert!(!before.projections.is_empty());
        let sync_attempted = Cell::new(false);
        let result = prepared.store.update_run_journal_using(
            |journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            },
            |store, next| {
                let temp = store.run_journal_path().with_extension("test-tmp");
                std::fs::write(&temp, serde_json::to_vec_pretty(next).unwrap())?;
                std::fs::rename(temp, store.run_journal_path())?;
                Err(io::Error::other("Fehler nach sichtbarem Journal-Rename"))
            },
            |_, _| {
                sync_attempted.set(true);
                Err(io::Error::other("Nachgeholter fsync verweigert"))
            },
        );
        let error = result.unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(sync_attempted.get());
        assert!(!fixture.right.join("neu.txt").exists());
        assert!(prepared.store.load().unwrap().is_none());
        let visible = prepared.store.load_run_journal().unwrap().unwrap();
        assert_eq!(visible.state, BidirectionalRunState::Running);
        drop(prepared);
        assert!(prepare_local_bidirectional(
            PairId([93; 16]),
            "Durability",
            &fixture.left,
            &fixture.right,
            &baseline_path
        )
        .is_err());
        let reopened = BaselineStore::open(&baseline_path).unwrap();
        assert_eq!(reopened.load_run_journal().unwrap(), Some(visible));
    }

    #[test]
    fn baselineabschluss_versoehnt_wirkenden_journalcommit_und_unlinkfehler() {
        use std::cell::Cell;

        let fixture = Fixture::new();
        let baseline_path = fixture._temp.path().join("state/abschluss.json");
        let prepared = prepare_local_bidirectional(
            PairId([94; 16]),
            "Abschlusspaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let baseline = baseline_from_resolved_snapshots(
            prepared.pair_id,
            prepared.left_binding.clone(),
            prepared.right_binding.clone(),
            &prepared.left_snapshot,
            &prepared.right_snapshot,
            &prepared.preview.plan,
        )
        .unwrap();
        prepared.store.save(&baseline).unwrap();

        prepared
            .store
            .update_run_journal_using(
                |journal| {
                    journal.state = BidirectionalRunState::Running;
                    journal.committed_baseline_digest = Some(baseline.body_digest);
                    Ok(())
                },
                |store, next| {
                    store.save_run_journal(next)?;
                    Err(io::Error::other(
                        "simulierter Fehler nach atomarem Journal-Rename",
                    ))
                },
                BaselineStore::sync_run_journal,
            )
            .unwrap();
        drop(prepared);

        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::CompletionCleanup
        );

        let queue =
            crate::OperationQueue::open(fixture._temp.path().join("state/abschluss-queue.json"))
                .unwrap();
        let queued = queue
            .enqueue_claimed(crate::QueueOperation::Bidirectional {
                pair_id: PairId([94; 16]),
                pair_name: "Abschlusspaar".into(),
                left: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: fixture.left.to_string_lossy().into_owned(),
                }),
                right: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: fixture.right.to_string_lossy().into_owned(),
                }),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: None,
                expected_plan_digest: [7; 32],
            })
            .unwrap();
        queue.mark_running(&queued.operation_id).unwrap();
        queue
            .interrupt_bidirectional(
                &queued.operation_id,
                crate::Progress::default(),
                "simulierter Fehler nach Baselinewirkung",
            )
            .unwrap();
        let interrupted = &queue.snapshot().entries[0];
        assert!(!interrupted.recovery_available);
        assert_eq!(
            interrupted.bidirectional_completion_recovery,
            Some(crate::queue::BidirectionalCompletionRecovery::Complete)
        );
        queue
            .interrupt_bidirectional(
                &queued.operation_id,
                crate::Progress::default(),
                "simulierter Abschlussfehler eines Resume-Laufs",
            )
            .unwrap();
        assert_eq!(
            queue.snapshot().entries[0].bidirectional_completion_recovery,
            Some(crate::queue::BidirectionalCompletionRecovery::Complete)
        );

        let store = BaselineStore::open(&baseline_path).unwrap();
        let expected = store.load_run_journal().unwrap().unwrap();
        let syncs = Cell::new(0_u8);
        store
            .remove_run_journal_reconciled_using(
                &expected,
                |path| {
                    std::fs::remove_file(path)?;
                    Err(io::Error::other("simulierter Fehler nach Unlink"))
                },
                |_| {
                    syncs.set(syncs.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(syncs.get(), 1);
        drop(store);
        queue
            .finish_bidirectional_completion_recovery(&queued.operation_id)
            .unwrap();
        assert_eq!(
            queue.snapshot().entries[0].state,
            crate::QueueState::Completed
        );
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::Absent
        );
    }

    #[test]
    fn baselineabschluss_bleibt_bis_terminalem_queuecommit_journalgebunden() {
        let fixture = Fixture::new();
        let pair = PairId([95; 16]);
        let baseline_path = fixture._temp.path().join("state/queue-abschluss.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("neu.txt"), "neuer Inhalt");

        let queue_path = fixture._temp.path().join("state/normal-queue.json");
        let queue = crate::OperationQueue::open(&queue_path).unwrap();
        let queued = queue
            .enqueue_claimed(crate::QueueOperation::Bidirectional {
                pair_id: pair,
                pair_name: "Queue-Abschlusspaar".into(),
                left: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: fixture.left.to_string_lossy().into_owned(),
                }),
                right: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: fixture.right.to_string_lossy().into_owned(),
                }),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: None,
                expected_plan_digest: [8; 32],
            })
            .unwrap();
        queue.mark_running(&queued.operation_id).unwrap();

        let prepared = prepare_local_bidirectional(
            pair,
            "Queue-Abschlusspaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase, false)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();

        // Die Baseline ist schon wirksam. Bis zum terminalen Queuecommit muss das
        // Core-Journal genau diese abgeschlossene Wirkung weiter binden.
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::CompletionCleanup
        );

        let writes = std::cell::Cell::new(0_u8);
        let error = queue
            .finish_bidirectional_after_effect_using(
                &queued.operation_id,
                false,
                crate::Progress::default(),
                |_, _| {
                    writes.set(writes.get() + 1);
                    Err(io::Error::other("simulierter Fehler vor Queue-Rename"))
                },
            )
            .unwrap_err();
        assert_eq!(writes.get(), 2);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let live = &queue.snapshot().entries[0];
        assert_eq!(live.state, crate::QueueState::Interrupted);
        assert_eq!(
            live.bidirectional_completion_recovery,
            Some(crate::queue::BidirectionalCompletionRecovery::Complete)
        );
        assert_eq!(
            crate::error_code(&queue.retry(&queued.operation_id).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        drop(queue);

        let queue = crate::OperationQueue::open(&queue_path).unwrap();
        let recovered = &queue.snapshot().entries[0];
        assert_eq!(recovered.state, crate::QueueState::Interrupted);
        assert_eq!(
            recovered.bidirectional_completion_recovery,
            Some(crate::queue::BidirectionalCompletionRecovery::Complete)
        );
        assert_eq!(
            crate::error_code(&queue.retry(&queued.operation_id).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        queue
            .finish_bidirectional_completion_recovery(&queued.operation_id)
            .unwrap();
        let completed = &queue.snapshot().entries[0];
        assert_eq!(completed.state, crate::QueueState::Completed);
        assert!(completed.bidirectional_completion_recovery.is_none());
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::Absent
        );
    }

    #[test]
    fn queueuebergang_akzeptiert_nur_bewiesen_entferntes_oder_erhaltenes_journal() {
        let fixture = Fixture::new();
        let baseline_path = fixture._temp.path().join("state/queue-transition.json");
        let prepared = prepare_local_bidirectional(
            PairId([92; 16]),
            "Queue-Übergang",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let journal = baseline_path.with_extension("run.json");

        let error = prepared
            .finish_enqueue_transition_using(
                |_| {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "vor Unlink",
                    ))
                },
                |_| Ok(()),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            journal.exists(),
            "Retry braucht dasselbe vorbereitete Journal"
        );

        prepared
            .finish_enqueue_transition_using(
                |path| {
                    std::fs::remove_file(path)?;
                    Err(io::Error::other("Unlink wirkte, Antwort ging verloren"))
                },
                |_| Ok(()),
            )
            .unwrap();
        assert!(
            !journal.exists(),
            "Nach bewiesenem Queuecommit ist ein sichtbar entferntes Journal abgeschlossen"
        );
    }

    #[test]
    fn lokale_erstbaseline_schreibt_nur_generation_eins() {
        let fixture = Fixture::new();
        write(&fixture.left.join("gleich"), "inhalt");
        write(&fixture.right.join("gleich"), "inhalt");
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let stats = prepare_and_run(&fixture, PairId([21; 16]), &baseline_path, false);
        assert_eq!(stats.baseline_generation, 1);
        assert!(stats.baseline_committed);
        assert!(stats.runs.is_empty());
        assert_eq!(
            std::fs::read(fixture.left.join("gleich")).unwrap(),
            b"inhalt"
        );
        assert_eq!(
            std::fs::read(fixture.right.join("gleich")).unwrap(),
            b"inhalt"
        );
    }

    #[test]
    fn projektion_liest_nur_auswahl_aus_quelle_und_verweigert_jeden_schreibweg() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "quelle");
        write(&fixture.right.join("a"), "ziel");
        write(&fixture.left.join("nur-links"), "verdeckt");
        let projection = LocalProjectedVfs::new(
            Arc::new(fixture.left_vfs.clone()),
            fixture.left.clone(),
            Arc::new(fixture.right_vfs.clone()),
            fixture.right.clone(),
            BTreeSet::from([PathBuf::from("a")]),
            BTreeSet::new(),
        )
        .unwrap();
        let names: Vec<_> = projection
            .list(&fixture.right)
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, vec!["a"]);
        let mut bytes = Vec::new();
        projection
            .open_read(&fixture.right.join("a"))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"quelle");
        assert!(projection.open_write(&fixture.right.join("a")).is_err());
        assert!(projection.remove_file(&fixture.right.join("a")).is_err());
    }

    #[test]
    fn lokale_additionen_laufen_in_beide_richtungen_und_werden_idempotent() {
        let fixture = Fixture::new();
        let pair = PairId([22; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("von-links"), "links");
        write(&fixture.right.join("von-rechts"), "rechts");
        let stats = prepare_and_run(&fixture, pair, &baseline_path, false);
        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(stats.runs.len(), 2);
        assert_eq!(
            std::fs::read(fixture.right.join("von-links")).unwrap(),
            b"links"
        );
        assert_eq!(
            std::fs::read(fixture.left.join("von-rechts")).unwrap(),
            b"rechts"
        );

        let no_op = prepare_and_run(&fixture, pair, &baseline_path, false);
        assert_eq!(no_op.baseline_generation, 2);
        assert!(!no_op.baseline_committed);
        assert!(no_op.runs.is_empty());
    }

    #[test]
    fn no_op_schreibt_eine_legacy_baseline_auf_das_aktuelle_schema_um() {
        let fixture = Fixture::new();
        write(&fixture.left.join("gleich"), "inhalt");
        write(&fixture.right.join("gleich"), "inhalt");
        let pair = PairId([29; 16]);
        let baseline_path = fixture._temp.path().join("state/legacy-no-op.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);

        let store = BaselineStore::open(&baseline_path).unwrap();
        let mut legacy = store.load().unwrap().unwrap();
        legacy.version = LEGACY_BASELINE_VERSION;
        legacy.body_digest = legacy.canonical_digest().unwrap();
        store.save(&legacy).unwrap();
        drop(store);

        let upgraded = prepare_and_run(&fixture, pair, &baseline_path, false);
        assert!(upgraded.runs.is_empty());
        assert!(upgraded.baseline_committed);
        assert_eq!(upgraded.baseline_generation, 2);
        let persisted = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(persisted.version, BASELINE_VERSION);
        assert_eq!(persisted.generation, 2);
    }

    #[test]
    fn gemischte_ordner_additionen_und_ersetzung_bleiben_geordnet() {
        let fixture = Fixture::new();
        write(&fixture.left.join("bestand"), "alt");
        write(&fixture.right.join("bestand"), "alt");
        let pair = PairId([28; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("bestand"), "neu");
        std::fs::create_dir(fixture.left.join("links-ordner")).unwrap();
        write(&fixture.left.join("links-ordner/datei"), "links");
        std::fs::create_dir(fixture.right.join("rechts-ordner")).unwrap();
        write(&fixture.right.join("rechts-ordner/datei"), "rechts");

        let stats = prepare_and_run(&fixture, pair, &baseline_path, true);
        assert_eq!(stats.runs.len(), 3);
        assert_eq!(
            std::fs::read(fixture.right.join("links-ordner/datei")).unwrap(),
            b"links"
        );
        assert_eq!(
            std::fs::read(fixture.left.join("rechts-ordner/datei")).unwrap(),
            b"rechts"
        );
        assert_eq!(
            std::fs::read(fixture.right.join("bestand")).unwrap(),
            b"neu"
        );
    }

    #[test]
    fn lokale_ersetzungen_ignorieren_mtime_als_gewinnerregel_aber_brauchen_backup() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt-a");
        write(&fixture.right.join("a"), "alt-a");
        write(&fixture.left.join("b"), "alt-b");
        write(&fixture.right.join("b"), "alt-b");
        let pair = PairId([23; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);

        write(&fixture.left.join("a"), "links-neu");
        write(&fixture.right.join("b"), "rechts-neu");
        filetime::set_file_mtime(
            fixture.left.join("a"),
            filetime::FileTime::from_unix_time(1_600_000_000, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            fixture.right.join("a"),
            filetime::FileTime::from_unix_time(1_800_000_000, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            fixture.right.join("b"),
            filetime::FileTime::from_unix_time(1_600_000_000, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            fixture.left.join("b"),
            filetime::FileTime::from_unix_time(1_800_000_000, 0),
        )
        .unwrap();

        let denied = prepare_local_bidirectional(
            pair,
            "Testpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert!(denied.preview().backup_confirmation_required);
        let phrase = denied.preview().confirmation_phrase.clone();
        assert!(denied.confirm(&phrase, false).is_err());
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"alt-a");
        assert_eq!(std::fs::read(fixture.left.join("b")).unwrap(), b"alt-b");

        let stats = prepare_and_run(&fixture, pair, &baseline_path, true);
        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(stats.runs.len(), 2);
        assert_eq!(
            std::fs::read(fixture.right.join("a")).unwrap(),
            b"links-neu"
        );
        assert_eq!(
            std::fs::read(fixture.left.join("b")).unwrap(),
            b"rechts-neu"
        );
    }

    #[test]
    fn lokale_drift_stoppt_vor_mutation_und_baseline_commit() {
        let fixture = Fixture::new();
        let pair = PairId([24; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("geplant"), "eins");
        let prepared = prepare_local_bidirectional(
            pair,
            "Testpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        write(&fixture.left.join("spaet"), "zwei");
        let phrase = prepared.preview().confirmation_phrase.clone();
        assert!(prepared.confirm(&phrase, false).is_err());
        assert!(!fixture.right.join("geplant").exists());
        let store = BaselineStore::open(&baseline_path).unwrap();
        assert_eq!(store.load().unwrap().unwrap().generation, 1);
    }

    #[test]
    fn spaete_bidirektionale_fehler_erhalten_alle_projektionsreste() {
        use std::os::unix::fs::PermissionsExt;
        for failure in ["journal", "followup", "cancel", "projection"] {
            let fixture = Fixture::new();
            let fixture_root = std::fs::canonicalize(fixture._temp.path()).unwrap();
            for root in [fixture_root.as_path(), &fixture.left, &fixture.right] {
                std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
                LocalVfs::new().write_acl_exact(root, None).unwrap();
            }
            let pair = PairId([97; 16]);
            let baseline_path = fixture._temp.path().join("state/pair.json");
            prepare_and_run(&fixture, pair, &baseline_path, false);
            std::fs::write(fixture.left.join("a"), vec![7; 262_144]).unwrap();
            std::fs::write(fixture.right.join("b"), vec![8; 262_144]).unwrap();
            let prepared = prepare_local_bidirectional(
                pair,
                "Recoverytest",
                &fixture.left,
                &fixture.right,
                &baseline_path,
            )
            .unwrap();
            let phrase = prepared.preview().confirmation_phrase.clone();
            let run = prepared.confirm(&phrase, false).unwrap();
            let cancel = crate::Cancel::new();
            let mut finished = 0;
            let error = run
                .execute(&cancel, &mut |progress| {
                    if progress.bytes_done > 0 {
                        for root in [fixture_root.as_path(), &fixture.left, &fixture.right] {
                            for stage in crate::atomic_file::staging_fault_tests::stages(root) {
                                std::fs::set_permissions(
                                    stage,
                                    std::fs::Permissions::from_mode(0o755),
                                )
                                .unwrap();
                            }
                        }
                    }
                    if progress.phase == "done" {
                        finished += 1;
                        if failure == "journal" && finished == 1 {
                            std::fs::write(baseline_path.with_extension("run.json"), b"{").unwrap();
                        }
                        if failure == "followup" && finished == 2 {
                            std::fs::write(fixture.left.join("spaete-drift"), b"spaete Aenderung")
                                .unwrap();
                        }
                    }
                    if finished == 1 && progress.phase == "copy" {
                        if failure == "cancel" && progress.bytes_done > 0 {
                            cancel.cancel();
                        }
                        if failure == "projection" {
                            let source = if progress.current == "a" {
                                &fixture.left
                            } else {
                                &fixture.right
                            };
                            let path = source.join(&progress.current);
                            if path.is_file() {
                                std::fs::remove_file(path).unwrap();
                            }
                        }
                    }
                })
                .expect_err(failure);
            let stages: Vec<_> = [fixture_root.as_path(), &fixture.left, &fixture.right]
                .into_iter()
                .flat_map(crate::atomic_file::staging_fault_tests::stages)
                .collect();
            assert!(!stages.is_empty(), "{failure}: künstlicher Rest fehlt");
            let paths = crate::error::recovery_paths(&error);
            for stage in &stages {
                let path = stage.to_string_lossy().into_owned();
                assert!(
                    paths
                        .iter()
                        .any(|known| Path::new(known).starts_with(stage)),
                    "{failure}: verlorener Rest {path}; {error}"
                );
            }
            assert!(paths.iter().all(|path| Path::new(path).exists()));
            assert_eq!(
                paths.len(),
                stages.len(),
                "{failure}: doppelte oder fremde Pfade"
            );
            assert!(fixture.right.join("a").is_file() || fixture.left.join("b").is_file());
            if failure == "followup" {
                assert_eq!(finished, 2);
                assert_eq!(stages.len(), 2);
                assert!(fixture.right.join("a").is_file() && fixture.left.join("b").is_file());
            }
            let store = BaselineStore::open(&baseline_path).unwrap();
            assert_eq!(store.load().unwrap().unwrap().generation, 1);
        }
    }

    #[test]
    fn projektionsabbruch_erhaelt_die_sync_recoverypfade() {
        let result = crate::SyncStats {
            cancelled: true,
            files_copied: 1,
            temp_recovery_paths: vec!["/isoliert/stage/rest".into()],
            ..crate::SyncStats::default()
        };
        for message in [
            "Bidirektionaler Lauf wurde vor dem Baseline-Commit abgebrochen",
            "Bidirektionale Löschprojektion wurde abgebrochen",
        ] {
            let error = cancelled_projection_error(message, &result);
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_eq!(crate::error_code(&error), crate::ErrorCode::Interrupted);
            assert_eq!(
                crate::error::recovery_paths(&error),
                result.temp_recovery_paths
            );
            assert!(error.to_string().contains("/isoliert/stage/rest"));
        }
    }

    #[test]
    fn lokaler_abbruch_laesst_alte_baseline_und_ziel_unveraendert() {
        let fixture = Fixture::new();
        let pair = PairId([27; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        std::fs::write(fixture.left.join("gross"), vec![0x5a; 1024 * 1024]).unwrap();
        let prepared = prepare_local_bidirectional(
            pair,
            "Abbruchtest",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        let cancel = crate::Cancel::new();
        cancel.cancel();
        assert!(run.execute(&cancel, &mut ignore_progress).is_err());
        assert!(!fixture.right.join("gross").exists());
        let store = BaselineStore::open(&baseline_path).unwrap();
        assert_eq!(store.load().unwrap().unwrap().generation, 1);
    }

    #[test]
    fn spaete_kollision_und_quell_drift_stoppen_vor_baseline_commit() {
        let fixture = Fixture::new();
        let pair = PairId([29; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("kollision"), "quelle");
        let prepared = prepare_local_bidirectional(
            pair,
            "Kollisionstest",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        write(&fixture.right.join("kollision"), "spaet");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        assert_eq!(
            std::fs::read(fixture.right.join("kollision")).unwrap(),
            b"spaet"
        );
        let store = BaselineStore::open(&baseline_path).unwrap();
        assert_eq!(store.load().unwrap().unwrap().generation, 1);
        drop(store);

        std::fs::remove_file(fixture.right.join("kollision")).unwrap();
        discard_unchanged_bidirectional_run(&baseline_path, &fixture.left, &fixture.right).unwrap();
        let prepared = prepare_local_bidirectional(
            pair,
            "Drifttest",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        write(&fixture.left.join("kollision"), "nach permit");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        assert!(!fixture.right.join("kollision").exists());
        let store = BaselineStore::open(&baseline_path).unwrap();
        assert_eq!(store.load().unwrap().unwrap().generation, 1);
    }

    #[test]
    fn konflikt_und_loeschung_bleiben_in_stufe_drei_ohne_mutation() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([25; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let conflict = prepare_local_bidirectional(
            pair,
            "Testpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert!(conflict.preview().stage_blocker.is_some());
        let phrase = conflict.preview().confirmation_phrase.clone();
        assert!(conflict.confirm(&phrase, true).is_err());
        assert_eq!(std::fs::read(fixture.left.join("a")).unwrap(), b"links");
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"rechts");

        let deleted = Fixture::new();
        write(&deleted.left.join("a"), "alt");
        write(&deleted.right.join("a"), "alt");
        let deleted_pair = PairId([26; 16]);
        let deleted_baseline = deleted._temp.path().join("state/pair.json");
        prepare_and_run(&deleted, deleted_pair, &deleted_baseline, false);
        std::fs::remove_file(deleted.left.join("a")).unwrap();
        let deletion = prepare_local_bidirectional(
            deleted_pair,
            "Löschtest",
            &deleted.left,
            &deleted.right,
            &deleted_baseline,
        );
        assert!(deletion.is_err());
        assert_eq!(std::fs::read(deleted.right.join("a")).unwrap(), b"alt");
    }

    #[test]
    fn bestaetigte_loeschung_quarantaenisiert_und_schreibt_tombstone() {
        let fixture = Fixture::new();
        for index in 0..20 {
            let name = format!("bestand-{index:02}");
            write(&fixture.left.join(&name), "bleibt");
            write(&fixture.right.join(&name), "bleibt");
        }
        write(&fixture.left.join("weg"), "original");
        write(&fixture.right.join("weg"), "original");
        let pair = PairId([30; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        std::fs::remove_file(fixture.left.join("weg")).unwrap();

        let prepared = prepare_local_bidirectional(
            pair,
            "Löschpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert!(prepared.preview().backup_confirmation_required);
        assert!(prepared.preview().stage_blocker.is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        assert!(!fixture.right.join("weg").exists());
        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(stats.runs.len(), 1);
        assert_eq!(stats.runs[0].files_deleted, 1);
        let quarantine = PathBuf::from(stats.runs[0].quarantine.as_ref().unwrap());
        assert_eq!(std::fs::read(quarantine.join("weg")).unwrap(), b"original");
        let store = BaselineStore::open(&baseline_path).unwrap();
        let baseline = store.load().unwrap().unwrap();
        assert!(baseline
            .entries
            .iter()
            .all(|record| record.path.display_lossy() != "weg"));
        assert!(baseline
            .tombstones
            .iter()
            .any(|record| record.path.display_lossy() == "weg"
                && record.direction == DeletionDirection::LeftToRight));
    }

    #[test]
    fn beidseitige_loeschung_braucht_keine_dateimutation_aber_schreibt_tombstone() {
        let fixture = Fixture::new();
        write(&fixture.left.join("weg"), "original");
        write(&fixture.right.join("weg"), "original");
        let pair = PairId([31; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        std::fs::remove_file(fixture.left.join("weg")).unwrap();
        std::fs::remove_file(fixture.right.join("weg")).unwrap();
        let stats = prepare_and_run(&fixture, pair, &baseline_path, true);
        assert!(stats.runs.is_empty());
        assert_eq!(stats.baseline_generation, 2);
        let store = BaselineStore::open(&baseline_path).unwrap();
        let baseline = store.load().unwrap().unwrap();
        assert!(baseline
            .tombstones
            .iter()
            .any(|record| record.path.display_lossy() == "weg"
                && record.direction == DeletionDirection::Both));
    }

    #[test]
    fn unvollstaendiges_listing_und_scanlimit_scheitern_fail_closed() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "x");
        let limits = SnapshotLimits {
            max_entries: 0,
            ..SnapshotLimits::default()
        };
        assert!(build_tree_snapshot(
            &fixture.left_vfs,
            &fixture.left,
            MetadataProfile::Portable,
            limits
        )
        .is_err());
        assert!(build_tree_snapshot(
            &fixture.left_vfs,
            &fixture.left,
            MetadataProfile::NativeBestEffort,
            SnapshotLimits::default()
        )
        .is_err());
    }

    #[test]
    fn explizite_gewinnerseiten_ersetzen_ohne_mtime_heuristik() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([32; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);

        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        filetime::set_file_mtime(
            fixture.left.join("a"),
            filetime::FileTime::from_unix_time(1_600_000_000, 0),
        )
        .unwrap();
        filetime::set_file_mtime(
            fixture.right.join("a"),
            filetime::FileTime::from_unix_time(1_800_000_000, 0),
        )
        .unwrap();
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            true,
            &[resolution("a", ConflictDecision::KeepLeft)],
        );
        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"links");

        write(&fixture.left.join("a"), "wieder-links");
        write(&fixture.right.join("a"), "wieder-rechts");
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            true,
            &[resolution("a", ConflictDecision::KeepRight)],
        );
        assert_eq!(stats.baseline_generation, 3);
        assert_eq!(
            std::fs::read(fixture.left.join("a")).unwrap(),
            b"wieder-rechts"
        );
    }

    #[test]
    fn verzeichnisentscheidung_bilanziert_den_vollstaendigen_unterbaum() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.left.join("projekt")).unwrap();
        std::fs::create_dir(fixture.right.join("projekt")).unwrap();
        std::fs::set_permissions(
            fixture.left.join("projekt"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("projekt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        for index in 0..20 {
            let name = format!("gleich-{index:02}");
            write(&fixture.left.join("projekt").join(&name), "gleich");
            write(&fixture.right.join("projekt").join(&name), "gleich");
        }
        write(&fixture.left.join("projekt/nur-links"), "links");
        write(&fixture.right.join("projekt/nur-rechts"), "rechts");
        let pair = PairId([50; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            true,
            &[resolution("projekt", ConflictDecision::KeepLeft)],
        );
        assert_eq!(stats.baseline_generation, 1);
        assert_eq!(
            std::fs::read(fixture.right.join("projekt/nur-links")).unwrap(),
            b"links"
        );
        assert!(!fixture.right.join("projekt/nur-rechts").exists());
        assert_eq!(
            std::fs::metadata(fixture.right.join("projekt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn verzeichnis_skip_friert_alle_kindkonflikte_ohne_baseline_ein() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.left.join("projekt")).unwrap();
        std::fs::create_dir(fixture.right.join("projekt")).unwrap();
        std::fs::set_permissions(
            fixture.left.join("projekt"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("projekt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write(&fixture.left.join("projekt/a"), "links");
        write(&fixture.right.join("projekt/a"), "rechts");
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let plan = plan_bidirectional(
            PairId([51; 16]),
            &bindings.0,
            &bindings.1,
            None,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        assert!(plan.conflicts >= 2);
        let resolved = resolve_conflicts(
            &plan,
            &snapshots.0,
            &snapshots.1,
            &[resolution("projekt", ConflictDecision::Skip)],
        )
        .unwrap();
        assert!(resolved.executable);
        assert!(matches!(
            resolved.actions.as_slice(),
            [BidirectionalAction::SkippedConflict { path, .. }]
                if path.display_lossy() == "projekt"
        ));
    }

    #[test]
    fn verzeichnisgewinner_ersetzt_datei_atomar_im_entschiedenen_unterbaum() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.left.join("projekt/knoten")).unwrap();
        std::fs::create_dir(fixture.right.join("projekt")).unwrap();
        write(&fixture.left.join("projekt/knoten/datei"), "inhalt");
        write(&fixture.right.join("projekt/knoten"), "datei");
        std::fs::set_permissions(
            fixture.left.join("projekt"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("projekt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let pair = PairId([52; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            true,
            &[resolution("projekt", ConflictDecision::KeepLeft)],
        );
        assert_eq!(stats.baseline_generation, 1);
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(
                std::fs::read(root.join("projekt/knoten/datei")).unwrap(),
                b"inhalt"
            );
            assert!(std::fs::read_dir(root).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("swap")));
        }
    }

    #[test]
    fn dateigewinner_ersetzt_nichtleeres_verzeichnis_atomar() {
        let fixture = Fixture::new();
        write(&fixture.left.join("knoten"), "gewinner");
        std::fs::create_dir(fixture.right.join("knoten")).unwrap();
        write(&fixture.right.join("knoten/kind"), "verlierer");
        let pair = PairId([54; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");

        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            true,
            &[resolution("knoten", ConflictDecision::KeepLeft)],
        );

        assert_eq!(stats.baseline_generation, 1);
        assert_eq!(
            std::fs::read(fixture.left.join("knoten")).unwrap(),
            b"gewinner"
        );
        assert_eq!(
            std::fs::read(fixture.right.join("knoten")).unwrap(),
            b"gewinner"
        );
        let sandbox = fixture.right.parent().unwrap().to_path_buf();
        let base = sandbox.join(".doppeldecker-quarantine");
        let runs = crate::quarantine::list_quarantine_runs(&LocalVfs::new(), &base, &Some(sandbox))
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].items_available, 1);
    }

    #[test]
    fn beide_behalten_sichert_verzeichnis_vor_typwechsel_auf_beiden_seiten() {
        let fixture = Fixture::new();
        write(&fixture.left.join("knoten"), "gewinner");
        std::fs::create_dir(fixture.right.join("knoten")).unwrap();
        write(&fixture.right.join("knoten/kind"), "verlierer");
        let baseline_path = fixture._temp.path().join("state/pair.json");

        prepare_and_run_with_decisions(
            &fixture,
            PairId([55; 16]),
            &baseline_path,
            true,
            &[resolution(
                "knoten",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(vec![b"knoten-rechts".to_vec()]),
                },
            )],
        );

        for root in [&fixture.left, &fixture.right] {
            assert_eq!(std::fs::read(root.join("knoten")).unwrap(), b"gewinner");
            assert_eq!(
                std::fs::read(root.join("knoten-rechts/kind")).unwrap(),
                b"verlierer"
            );
        }
    }

    #[test]
    fn resume_normalisiert_manifestlosen_typwechsel_vor_erster_mutation() {
        let fixture = Fixture::new();
        write(&fixture.left.join("knoten"), "gewinner");
        std::fs::create_dir(fixture.right.join("knoten")).unwrap();
        write(&fixture.right.join("knoten/kind"), "verlierer");
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional_with_decisions(
            PairId([65; 16]),
            "Typresume-vor-manifest",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution("knoten", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            left_root,
            right_root,
            right_vfs,
            store,
            runs,
            ..
        } = run;
        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                journal.projections[1].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let journal = store.load_run_journal().unwrap().unwrap();
        let BidirectionalProjectionKind::TypeSwap {
            path, staging_path, ..
        } = &journal.projections[1].projection
        else {
            panic!("zweite Projektion ist kein Typwechsel");
        };
        let base = right_root
            .parent()
            .unwrap()
            .join(".doppeldecker-quarantine");
        crate::real_mode::ensure_quarantine_root(
            right_vfs.as_ref(),
            right_root.parent().unwrap(),
            &base,
        )
        .unwrap();
        let run_root =
            crate::real_mode::reserve_quarantine_run_name(right_vfs.as_ref(), &base).unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[1].quarantine = Some(run_root.to_string_lossy().into_owned());
                journal.type_swap_step = Some(BidirectionalTypeSwapStep {
                    ordinal: 1,
                    side: EndpointSide::Right,
                    path: path.clone(),
                    staging_path: staging_path.clone(),
                    quarantine_root: run_root.clone(),
                    phase: BidirectionalTypeSwapPhase::Exchange,
                });
                Ok(())
            })
            .unwrap();
        assert!(!base
            .join(format!(
                "{}.json",
                run_root.file_name().unwrap().to_string_lossy()
            ))
            .exists());
        drop(store);

        prepare_resume_local_bidirectional(
            "Typresume-vor-manifest",
            &left_root,
            &right_root,
            &baseline_path,
        )
        .unwrap();
        let sidecar = base.join(format!(
            "{}.json",
            run_root.file_name().unwrap().to_string_lossy()
        ));
        assert!(sidecar.exists());
        assert!(base
            .join(format!(
                "{}.purge-intent",
                run_root.file_name().unwrap().to_string_lossy()
            ))
            .exists());
        assert!(!run_root.exists());
        let run_id = run_root.file_name().unwrap().to_string_lossy();
        let runs =
            crate::quarantine::list_quarantine_runs(right_vfs.as_ref(), &base, &None).unwrap();
        assert_eq!(
            runs.iter().find(|run| run.id == run_id).unwrap().status,
            crate::quarantine::QuarantineRunStatus::Purged
        );
        assert!(fixture.right.join("knoten").is_dir());
    }

    #[test]
    fn resume_tauscht_abgebrochenen_typwechsel_zuerst_atomar_zurueck() {
        let fixture = Fixture::new();
        write(&fixture.left.join("knoten"), "gewinner");
        std::fs::create_dir(fixture.right.join("knoten")).unwrap();
        write(&fixture.right.join("knoten/kind"), "verlierer");
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional_with_decisions(
            PairId([56; 16]),
            "Typresume",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution("knoten", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            left_root,
            right_root,
            left_vfs: _,
            right_vfs,
            store,
            runs,
            run_ordinals,
            deferred_run_ordinals,
            ..
        } = run;
        assert_eq!(run_ordinals, vec![0]);
        assert_eq!(deferred_run_ordinals, vec![1]);
        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                journal.projections[1].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let journal = store.load_run_journal().unwrap().unwrap();
        let BidirectionalProjectionKind::TypeSwap {
            path, staging_path, ..
        } = &journal.projections[1].projection
        else {
            panic!("zweite Projektion ist kein Typwechsel");
        };
        let relative = path.to_path_buf().unwrap();
        let staging_relative = staging_path.to_path_buf().unwrap();
        let base = right_root
            .parent()
            .unwrap()
            .join(".doppeldecker-quarantine");
        crate::real_mode::ensure_quarantine_root(
            right_vfs.as_ref(),
            right_root.parent().unwrap(),
            &base,
        )
        .unwrap();
        let run_root =
            crate::real_mode::reserve_quarantine_run_name(right_vfs.as_ref(), &base).unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[1].quarantine = Some(run_root.to_string_lossy().into_owned());
                journal.type_swap_step = Some(BidirectionalTypeSwapStep {
                    ordinal: 1,
                    side: EndpointSide::Right,
                    path: path.clone(),
                    staging_path: staging_path.clone(),
                    quarantine_root: run_root.clone(),
                    phase: BidirectionalTypeSwapPhase::Exchange,
                });
                Ok(())
            })
            .unwrap();
        ensure_type_swap_quarantine_started(right_vfs.as_ref(), &run_root, &right_root).unwrap();
        right_vfs
            .rename_exchange(
                &right_root.join(&relative),
                &right_root.join(&staging_relative),
            )
            .unwrap();
        right_vfs
            .sync_dir(right_root.join(&relative).parent().unwrap())
            .unwrap();
        drop(store);

        let resumed = prepare_resume_local_bidirectional(
            "Typresume",
            &left_root,
            &right_root,
            &baseline_path,
        )
        .unwrap();
        assert!(resumed.prepared_run_ordinals.is_empty());
        assert_eq!(resumed.deferred_run_ordinals, vec![1]);
        // Die Recovery-Vorbereitung hat den Zwischenzustand ohne fehlenden
        // Originalnamen wieder vor den Swap zurückgesetzt.
        assert!(fixture.right.join("knoten").is_dir());
        let phrase = resumed.preview().confirmation_phrase.clone();
        resumed
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        assert_eq!(
            std::fs::read(fixture.right.join("knoten")).unwrap(),
            b"gewinner"
        );
    }

    #[test]
    fn typwechsel_rollback_setzt_nach_staging_crash_atomar_fort() {
        let fixture = Fixture::new();
        write(&fixture.left.join("knoten"), "gewinner");
        std::fs::create_dir(fixture.right.join("knoten")).unwrap();
        write(&fixture.right.join("knoten/kind"), "verlierer");
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional_with_decisions(
            PairId([60; 16]),
            "Typrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution("knoten", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            left_root,
            right_root,
            left_vfs,
            right_vfs,
            store,
            runs,
            run_ordinals,
            deferred_run_ordinals,
            ..
        } = run;
        assert_eq!(run_ordinals, vec![0]);
        assert_eq!(deferred_run_ordinals, vec![1]);
        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                journal.projections[1].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        execute_local_type_swap_projection(
            &store,
            1,
            LocalEndpointPair {
                left_vfs: left_vfs.as_ref(),
                left_root: &left_root,
                right_vfs: right_vfs.as_ref(),
                right_root: &right_root,
            },
        )
        .unwrap();
        drop(store);

        let prepared = prepare_local_bidirectional_rollback(
            "Typrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let rollback = prepared.confirm(&phrase).unwrap();
        let LocalBidirectionalRollback {
            right_root,
            right_vfs,
            store,
            mut journal,
            ..
        } = rollback;
        let delta = projection_rollback_units(&journal, 1).unwrap().remove(0);
        journal.rollback_step = Some(rollback_step_for_unit(1, 0, &delta).unwrap());
        journal.refresh_digest().unwrap();
        store.save_run_journal(&journal).unwrap();
        let projection = journal.projections[1].clone();
        if !matches!(
            &projection.projection,
            BidirectionalProjectionKind::TypeSwap { .. }
        ) {
            panic!("zweite Projektion ist kein Typwechsel");
        }
        let (before_maps, _) =
            type_swap_before_after_maps(&journal, 1, EndpointSide::Right).unwrap();
        let run_root = PathBuf::from(projection.quarantine.as_ref().unwrap());
        let lease =
            crate::quarantine::lease_quarantine_run(right_vfs.as_ref(), &run_root, &right_root)
                .unwrap();
        let held = vec![HeldRollbackQuarantine {
            side: EndpointSide::Right,
            run_root,
            lease,
        }];
        restore_type_previous_to_staging(
            right_vfs.as_ref(),
            &right_root,
            EndpointSide::Right,
            &projection,
            delta.before.as_ref().unwrap(),
            &before_maps,
            &held,
        )
        .unwrap();
        // Simulation: Prozess endet nach dem No-Replace-Restore an den Stagingnamen,
        // aber vor dem Phasencommit.
        drop(held);
        drop(store);

        let resumed = prepare_local_bidirectional_rollback(
            "Typrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = resumed.preview().confirmation_phrase.clone();
        let stats = resumed
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert_eq!(stats.projections_rolled_back, 2);
        assert_eq!(
            std::fs::read(fixture.left.join("knoten")).unwrap(),
            b"gewinner"
        );
        assert_eq!(
            std::fs::read(fixture.right.join("knoten/kind")).unwrap(),
            b"verlierer"
        );
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::RollbackCompletionCleanup
        );
        finish_bidirectional_rollback_completion(&baseline_path).unwrap();
        assert!(load_bidirectional_run_journal(&baseline_path)
            .unwrap()
            .is_none());
    }

    #[test]
    fn beide_behalten_materialisiert_zwei_vollstaendige_verzeichnisbaeume() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.left.join("projekt")).unwrap();
        std::fs::create_dir(fixture.right.join("projekt")).unwrap();
        std::fs::set_permissions(
            fixture.left.join("projekt"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("projekt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        for index in 0..20 {
            let name = format!("gleich-{index:02}");
            write(&fixture.left.join("projekt").join(&name), "gleich");
            write(&fixture.right.join("projekt").join(&name), "gleich");
        }
        write(&fixture.left.join("projekt/nur-links"), "links");
        write(&fixture.right.join("projekt/nur-rechts"), "rechts");
        let pair = PairId([53; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let conflict = BinaryRelativePath(vec![b"projekt-rechts".to_vec()]);
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            true,
            &[resolution(
                "projekt",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: conflict,
                },
            )],
        );
        assert_eq!(stats.baseline_generation, 1);
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(
                std::fs::read(root.join("projekt/nur-links")).unwrap(),
                b"links"
            );
            assert!(!root.join("projekt/nur-rechts").exists());
            assert_eq!(
                std::fs::read(root.join("projekt-rechts/nur-rechts")).unwrap(),
                b"rechts"
            );
            assert!(!root.join("projekt-rechts/nur-links").exists());
            assert_eq!(
                std::fs::metadata(root.join("projekt-rechts"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn skip_friert_nur_seinen_baseline_pfad_ein() {
        let fixture = Fixture::new();
        write(&fixture.left.join("konflikt"), "alt");
        write(&fixture.right.join("konflikt"), "alt");
        let pair = PairId([33; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        let old_baseline = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();

        write(&fixture.left.join("konflikt"), "links");
        write(&fixture.right.join("konflikt"), "rechts");
        write(&fixture.left.join("neu"), "additiv");
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            false,
            &[resolution("konflikt", ConflictDecision::Skip)],
        );
        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(
            std::fs::read(fixture.right.join("neu")).unwrap(),
            b"additiv"
        );
        assert_eq!(
            std::fs::read(fixture.left.join("konflikt")).unwrap(),
            b"links"
        );
        assert_eq!(
            std::fs::read(fixture.right.join("konflikt")).unwrap(),
            b"rechts"
        );

        let current = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        let old = old_baseline
            .entries
            .iter()
            .find(|record| record.path.display_lossy() == "konflikt")
            .unwrap();
        let preserved = current
            .entries
            .iter()
            .find(|record| record.path.display_lossy() == "konflikt")
            .unwrap();
        assert_eq!(preserved.entry, old.entry);
        let prepared = prepare_local_bidirectional(
            pair,
            "Konfliktpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert_eq!(prepared.preview().plan.conflicts, 1);
    }

    #[test]
    fn initiales_skip_schreibt_keine_falsche_gemeinsame_wahrheit() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let pair = PairId([34; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            false,
            &[resolution("a", ConflictDecision::Skip)],
        );
        assert_eq!(stats.baseline_generation, 1);
        let baseline = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert!(baseline.entries.is_empty());
        let prepared = prepare_local_bidirectional(
            pair,
            "Konfliktpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert_eq!(prepared.preview().plan.conflicts, 1);
    }

    #[test]
    fn einseitiges_initiales_skip_bleibt_vor_journal_und_wirkung_gesperrt() {
        let fixture = Fixture::new();
        write(&fixture.left.join("nur-links"), "bleibt links");
        let pair = PairId([113; 16]);
        let baseline_path = fixture._temp.path().join("state/one-sided-skip.json");
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Einseitiges Skip",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution("nur-links", ConflictDecision::Skip)],
        )
        .unwrap();

        assert!(prepared
            .preview()
            .stage_blocker
            .as_deref()
            .is_some_and(|reason| reason.contains("Erstbaseline")));
        assert!(prepared.store.load_run_journal().unwrap().is_none());
        let phrase = prepared.preview().confirmation_phrase.clone();
        let error = prepared
            .confirm(&phrase, false)
            .err()
            .expect("Einseitiges initiales Skip muss vor dem Lauf blockieren");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(!baseline_path.exists());
        assert!(!fixture.right.join("nur-links").exists());

        let unresolved = prepare_local_bidirectional(
            pair,
            "Einseitiges Skip",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert_eq!(unresolved.preview().plan.conflicts, 1);
    }

    #[cfg(unix)]
    #[test]
    fn gesperrtes_legacy_resume_startet_keine_restore_probe() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        write(&fixture.left.join("skip"), "bleibt links");
        write(&fixture.left.join("kopie"), "gewinnt links");
        write(&fixture.right.join("kopie"), "verliert rechts");
        let pair = PairId([116; 16]);
        let baseline_path = fixture._temp.path().join("state/legacy-blocked.json");
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Legacy-Sperre",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[
                resolution("skip", ConflictDecision::Skip),
                resolution("kopie", ConflictDecision::KeepLeft),
            ],
        )
        .unwrap();
        assert!(prepared.preview().stage_blocker.is_some());

        // Simuliert ein Journal einer älteren Version, die den heute gesperrten
        // Plan bereits persistiert hatte. Ein schreibgeschütztes Ziel macht jede
        // versehentlich gestartete Restore-Probe zuverlässig sichtbar.
        let journal = BidirectionalRunJournal::new(
            &prepared.preview().plan,
            prepared.left_binding.clone(),
            prepared.right_binding.clone(),
            &prepared.left_snapshot,
            &prepared.right_snapshot,
        )
        .unwrap();
        prepared.store.save_run_journal(&journal).unwrap();
        drop(prepared);
        let original_mode = std::fs::metadata(&fixture.right)
            .unwrap()
            .permissions()
            .mode();
        std::fs::set_permissions(&fixture.right, std::fs::Permissions::from_mode(0o555)).unwrap();

        let resumed = prepare_resume_local_bidirectional(
            "Legacy-Sperre",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        );

        std::fs::set_permissions(
            &fixture.right,
            std::fs::Permissions::from_mode(original_mode),
        )
        .unwrap();
        let resumed = resumed.expect("die Sperrvorschau darf keine Schreibprobe starten");
        assert!(resumed.preview().stage_blocker.is_some());
        assert!(resumed.prepared_runs.is_empty());
        assert_eq!(
            resumed.store.load_run_journal().unwrap().unwrap(),
            journal,
            "ein gesperrtes Resume darf das alte Journal nicht umschreiben"
        );
    }

    #[test]
    fn provider_sperrt_einseitiges_initiales_skip_ebenfalls_vor_dem_journal() {
        let fixture = Fixture::new();
        write(&fixture.left.join("nur-links"), "bleibt links");
        let pair = PairId([115; 16]);
        let baseline_path = fixture
            ._temp
            .path()
            .join("state/provider-one-sided-skip.json");
        let (left, right) = remote_test_endpoints(&fixture, crate::ProviderKind::Ftps);
        let prepared = prepare_provider_bidirectional_with_decisions(
            pair,
            "Provider Skip",
            left,
            right,
            &baseline_path,
            &[resolution("nur-links", ConflictDecision::Skip)],
        )
        .unwrap();

        assert!(prepared
            .preview()
            .stage_blocker
            .as_deref()
            .is_some_and(|reason| reason.contains("Erstbaseline")));
        assert!(prepared.store.load_run_journal().unwrap().is_none());
    }

    #[test]
    fn tombstone_wiederbelebung_kann_uebersprungen_oder_bestaetigt_werden() {
        let fixture = Fixture::new();
        for index in 0..20 {
            let name = format!("bestand-{index:02}");
            write(&fixture.left.join(&name), "bleibt");
            write(&fixture.right.join(&name), "bleibt");
        }
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([35; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        std::fs::remove_file(fixture.left.join("a")).unwrap();
        std::fs::remove_file(fixture.right.join("a")).unwrap();
        prepare_and_run(&fixture, pair, &baseline_path, true);
        write(&fixture.left.join("a"), "wiederbelebt");

        let skipped = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            false,
            &[resolution("a", ConflictDecision::Skip)],
        );
        assert!(!skipped.baseline_committed);
        assert!(!fixture.right.join("a").exists());
        let preserved = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert!(preserved
            .tombstones
            .iter()
            .any(|item| item.path.display_lossy() == "a"));

        let accepted = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            false,
            &[resolution("a", ConflictDecision::KeepLeft)],
        );
        assert_eq!(accepted.baseline_generation, 3);
        assert_eq!(
            std::fs::read(fixture.right.join("a")).unwrap(),
            b"wiederbelebt"
        );
        let baseline = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert!(baseline
            .tombstones
            .iter()
            .all(|item| item.path.display_lossy() != "a"));
    }

    #[test]
    fn loeschung_gegen_aenderung_folgt_nur_expliziter_entscheidung() {
        let keep_change = Fixture::new();
        write(&keep_change.left.join("a"), "alt");
        write(&keep_change.right.join("a"), "alt");
        let pair = PairId([36; 16]);
        let baseline_path = keep_change._temp.path().join("state/pair.json");
        prepare_and_run(&keep_change, pair, &baseline_path, false);
        std::fs::remove_file(keep_change.left.join("a")).unwrap();
        write(&keep_change.right.join("a"), "geändert");
        prepare_and_run_with_decisions(
            &keep_change,
            pair,
            &baseline_path,
            false,
            &[resolution("a", ConflictDecision::KeepRight)],
        );
        assert_eq!(
            std::fs::read(keep_change.left.join("a")).unwrap(),
            "geändert".as_bytes()
        );

        let keep_deletion = Fixture::new();
        for index in 0..20 {
            let name = format!("bestand-{index:02}");
            write(&keep_deletion.left.join(&name), "bleibt");
            write(&keep_deletion.right.join(&name), "bleibt");
        }
        write(&keep_deletion.left.join("a"), "alt");
        write(&keep_deletion.right.join("a"), "alt");
        let pair = PairId([37; 16]);
        let baseline_path = keep_deletion._temp.path().join("state/pair.json");
        prepare_and_run(&keep_deletion, pair, &baseline_path, false);
        std::fs::remove_file(keep_deletion.left.join("a")).unwrap();
        write(&keep_deletion.right.join("a"), "geändert");
        let stats = prepare_and_run_with_decisions(
            &keep_deletion,
            pair,
            &baseline_path,
            true,
            &[resolution("a", ConflictDecision::KeepLeft)],
        );
        assert!(!keep_deletion.right.join("a").exists());
        assert_eq!(stats.runs[0].files_deleted, 1);
    }

    #[test]
    fn veraltete_und_doppelte_entscheidungen_scheitern_fail_closed() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let plan = plan_bidirectional(
            PairId([38; 16]),
            &bindings.0,
            &bindings.1,
            None,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        let duplicate = vec![
            resolution("a", ConflictDecision::KeepLeft),
            resolution("a", ConflictDecision::KeepRight),
        ];
        assert!(resolve_conflicts(&plan, &snapshots.0, &snapshots.1, &duplicate).is_err());
        assert!(resolve_conflicts(
            &plan,
            &snapshots.0,
            &snapshots.1,
            &[resolution("fremd", ConflictDecision::Skip)]
        )
        .is_err());
        let keep_both = resolve_conflicts(
            &plan,
            &snapshots.0,
            &snapshots.1,
            &[resolution(
                "a",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(vec![b"a-konflikt".to_vec()]),
                },
            )],
        )
        .unwrap();
        assert!(keep_both.executable);
        assert_eq!(keep_both.actions.len(), 3);
    }

    #[test]
    fn beide_behalten_materialisiert_verlierer_vor_dem_originalersatz() {
        let fixture = Fixture::new();
        write(&fixture.left.join("bericht.txt"), "links");
        write(&fixture.right.join("bericht.txt"), "rechts");
        let pair = PairId([39; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let stats = prepare_and_run_with_decisions(
            &fixture,
            pair,
            &baseline_path,
            true,
            &[resolution(
                "bericht.txt",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(
                        vec![b"bericht-rechts-konflikt.txt".to_vec()],
                    ),
                },
            )],
        );
        assert_eq!(stats.runs.len(), 3);
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(std::fs::read(root.join("bericht.txt")).unwrap(), b"links");
            assert_eq!(
                std::fs::read(root.join("bericht-rechts-konflikt.txt")).unwrap(),
                b"rechts"
            );
        }
        let baseline = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(baseline.entries.len(), 2);
    }

    #[test]
    fn beide_behalten_bindet_kollisionsfreien_namen() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        write(&fixture.left.join("belegt"), "x");
        let pair = PairId([40; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let result = prepare_local_bidirectional_with_decisions(
            pair,
            "Konfliktpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution(
                "a",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Right,
                    conflict_path: BinaryRelativePath(vec![b"belegt".to_vec()]),
                },
            )],
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(fixture.left.join("a")).unwrap(), b"links");
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"rechts");
    }

    #[test]
    fn beide_behalten_verweigert_namensalias_zwischen_neuen_konfliktzielen() {
        let fixture = Fixture::new();
        for name in ["a", "b"] {
            write(&fixture.left.join(name), "links");
            write(&fixture.right.join(name), "rechts");
        }
        let snapshots = fixture.snapshots();
        let bindings = fixture.bindings(&snapshots);
        let plan = plan_bidirectional(
            PairId([85; 16]),
            &bindings.0,
            &bindings.1,
            None,
            &snapshots.0,
            &snapshots.1,
        )
        .unwrap();
        let decisions = |first: &str, second: &str| {
            [("a", first), ("b", second)].map(|(original, target)| {
                resolution(
                    original,
                    ConflictDecision::KeepBoth {
                        primary: EndpointSide::Left,
                        conflict_path: BinaryRelativePath(vec![target.as_bytes().to_vec()]),
                    },
                )
            })
        };
        assert!(
            resolve_conflicts(
                &plan,
                &snapshots.0,
                &snapshots.1,
                &decisions("kopie-a.txt", "kopie-b.txt"),
            )
            .unwrap()
            .executable
        );
        let distinct = decisions("kopie-a.txt", "kopie-b.txt");
        let partial = resolve_conflicts(&plan, &snapshots.0, &snapshots.1, &distinct[..1]).unwrap();
        assert!(
            resolve_conflicts(&partial, &snapshots.0, &snapshots.1, &distinct[1..])
                .unwrap()
                .executable
        );
        for (first, second) in [
            ("kopie.txt", "Kopie.txt"),
            ("Änderung.txt", "A\u{308}nderung.txt"),
        ] {
            assert!(
                resolve_conflicts(&plan, &snapshots.0, &snapshots.1, &decisions(first, second),)
                    .is_err(),
                "Zwei geplante Konfliktkopien kollidieren: {first:?}, {second:?}"
            );
            let choices = decisions(first, second);
            let partial =
                resolve_conflicts(&plan, &snapshots.0, &snapshots.1, &choices[..1]).unwrap();
            assert!(
                resolve_conflicts(&partial, &snapshots.0, &snapshots.1, &choices[1..]).is_err(),
                "Schrittweise geplante Konfliktkopien kollidieren: {first:?}, {second:?}"
            );
        }
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(std::fs::read_dir(root).unwrap().count(), 2);
            let expected: &[u8] = if root == &fixture.left {
                b"links"
            } else {
                b"rechts"
            };
            for name in ["a", "b"] {
                assert_eq!(std::fs::read(root.join(name)).unwrap(), expected);
            }
        }
    }

    #[test]
    fn konfliktname_bindet_seite_utc_und_zufall_neben_dem_original() {
        let original = BinaryRelativePath(vec![b"ordner".to_vec(), b"bericht.txt".to_vec()]);
        let generated = generate_conflict_path(&original, EndpointSide::Right).unwrap();
        assert_eq!(generated.0[0], b"ordner");
        let name = std::str::from_utf8(&generated.0[1]).unwrap();
        assert!(name.starts_with("bericht (Konflikt rechts "));
        assert!(name.ends_with(".txt"));
        assert!(name.contains('T') && name.contains("Z "));
        assert_ne!(
            generated,
            generate_conflict_path(&original, EndpointSide::Right).unwrap()
        );
    }

    #[test]
    fn journal_existiert_vor_bestaetigung_und_falsche_phrase_verwirft_es() {
        let fixture = Fixture::new();
        let pair = PairId([41; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional(
            pair,
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let journal = prepared.store.load_run_journal().unwrap().unwrap();
        assert_eq!(journal.state, BidirectionalRunState::Prepared);
        assert!(journal.projections.is_empty());
        assert!(prepared.confirm("falsch", false).is_err());
        let store = BaselineStore::open(&baseline_path).unwrap();
        assert!(store.load_run_journal().unwrap().is_none());
    }

    #[test]
    fn teilcommit_bleibt_im_journal_und_blockiert_neuen_normalplan() {
        let fixture = Fixture::new();
        write(&fixture.left.join("bericht.txt"), "links");
        write(&fixture.right.join("bericht.txt"), "rechts");
        let pair = PairId([42; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let conflict_name = "bericht-rechts-konflikt.txt";
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution(
                "bericht.txt",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(vec![conflict_name.as_bytes().to_vec()]),
                },
            )],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        // Die Kollision entsteht erst nach der Permit-Ausgabe auf der zweiten
        // Zielseite. Die erste Konfliktkopie ist dann bereits ein echter Teilcommit.
        write(&fixture.right.join(conflict_name), "fremd");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        assert_eq!(
            std::fs::read(fixture.left.join(conflict_name)).unwrap(),
            b"rechts"
        );
        assert_eq!(
            std::fs::read(fixture.right.join(conflict_name)).unwrap(),
            b"fremd"
        );
        let store = BaselineStore::open(&baseline_path).unwrap();
        let journal = store.load_run_journal().unwrap().unwrap();
        assert_eq!(journal.state, BidirectionalRunState::Running);
        assert_eq!(
            journal.projections[0].state,
            BidirectionalProjectionState::Committed
        );
        assert_eq!(
            journal.projections[1].state,
            BidirectionalProjectionState::Running
        );
        assert_eq!(
            journal.projections[2].state,
            BidirectionalProjectionState::Prepared
        );
        drop(store);
        assert!(prepare_local_bidirectional(
            pair,
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .is_err());
        assert!(
            discard_unchanged_bidirectional_run(&baseline_path, &fixture.left, &fixture.right)
                .is_err()
        );

        std::fs::remove_file(fixture.right.join(conflict_name)).unwrap();
        let resumed = prepare_resume_local_bidirectional(
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert_eq!(resumed.prepared_run_ordinals, vec![1, 2]);
        assert!(resumed.confirm("falsch", true).is_err());
        assert!(load_bidirectional_run_journal(&baseline_path)
            .unwrap()
            .is_some());
        let resumed = prepare_resume_local_bidirectional(
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = resumed.preview().confirmation_phrase.clone();
        let stats = resumed
            .confirm(&phrase, true)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        finish_bidirectional_run_completion(&baseline_path).unwrap();
        assert_eq!(stats.baseline_generation, 1);
        for root in [&fixture.left, &fixture.right] {
            assert_eq!(std::fs::read(root.join("bericht.txt")).unwrap(), b"links");
            assert_eq!(std::fs::read(root.join(conflict_name)).unwrap(), b"rechts");
        }
        let store = BaselineStore::open(&baseline_path).unwrap();
        assert!(store.load_run_journal().unwrap().is_none());
    }

    #[test]
    fn resume_lehnt_einen_dritten_ungebundenen_zustand_ab() {
        let fixture = Fixture::new();
        let pair = PairId([44; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("neu"), "quelle");
        let prepared = prepare_local_bidirectional(
            pair,
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        write(&fixture.right.join("neu"), "fremd");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        write(&fixture.right.join("neu"), "noch-anders");
        assert!(prepare_resume_local_bidirectional(
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .is_err());
    }

    #[test]
    fn rollbackabschluss_bleibt_bis_terminalem_queuecommit_journalgebunden() {
        let fixture = Fixture::new();
        write(&fixture.left.join("bericht.txt"), "links");
        write(&fixture.right.join("bericht.txt"), "rechts");
        let pair = PairId([45; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let conflict_name = "bericht-rechts-rollback.txt";
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Rollbackpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution(
                "bericht.txt",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(vec![conflict_name.as_bytes().to_vec()]),
                },
            )],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        write(&fixture.right.join(conflict_name), "fremd");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        std::fs::remove_file(fixture.right.join(conflict_name)).unwrap();

        let queue_path = fixture._temp.path().join("state/rollback-queue.json");
        let queue = crate::OperationQueue::open(&queue_path).unwrap();
        let queued = queue
            .enqueue_claimed(crate::QueueOperation::Bidirectional {
                pair_id: pair,
                pair_name: "Rollbackpaar".into(),
                left: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: fixture.left.to_string_lossy().into_owned(),
                }),
                right: Box::new(crate::BidirectionalQueueEndpoint::Local {
                    root: fixture.right.to_string_lossy().into_owned(),
                }),
                baseline_path: baseline_path.to_string_lossy().into_owned(),
                resolutions: Vec::new(),
                expected_baseline_digest: None,
                expected_plan_digest: [7; 32],
            })
            .unwrap();
        queue.mark_running(&queued.operation_id).unwrap();
        queue
            .interrupt_bidirectional(
                &queued.operation_id,
                crate::Progress::default(),
                "Teilwirkung wartet auf Rollback",
            )
            .unwrap();

        let rollback = prepare_local_bidirectional_rollback(
            "Rollbackpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert_eq!(rollback.preview().committed_projections, 1);
        let phrase = rollback.preview().confirmation_phrase.clone();
        let stats = rollback
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert_eq!(stats.projections_rolled_back, 1);
        assert_eq!(stats.rollback_quarantines.len(), 1);
        assert!(!fixture.left.join(conflict_name).exists());
        assert!(!fixture.right.join(conflict_name).exists());
        assert_eq!(
            std::fs::read(fixture.left.join("bericht.txt")).unwrap(),
            b"links"
        );
        assert_eq!(
            std::fs::read(fixture.right.join("bericht.txt")).unwrap(),
            b"rechts"
        );
        assert_eq!(
            std::fs::read(Path::new(&stats.rollback_quarantines[0]).join(conflict_name)).unwrap(),
            b"rechts"
        );
        let store = BaselineStore::open(&baseline_path).unwrap();
        assert!(store.load().unwrap().is_none());
        // Der vollständig rückgängig gemachte Lauf bleibt bis zum bewiesenen
        // terminalen Queuecommit als mutationsloser Abschluss gebunden. Würde
        // das Journal hier schon fehlen, könnte ein Queue-Schreibfehler den alten
        // Eintrag später fälschlich als normalen Retry anbieten.
        let expected = store.load_run_journal().unwrap().unwrap();
        assert_eq!(expected.state, BidirectionalRunState::RollbackCommitted);
        drop(store);
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::RollbackCompletionCleanup
        );

        // Beide Queue-Schreibversuche scheitern sicher vor dem Rename. Das
        // weiterhin vorhandene Core-Journal muss schon den lebenden Prozess auf
        // mutationslosen Rollback-Abschluss statt normalen Retry lenken.
        let writes = std::cell::Cell::new(0_u8);
        let error = queue
            .finish_bidirectional_after_effect_using(
                &queued.operation_id,
                true,
                crate::Progress::default(),
                |_, _| {
                    writes.set(writes.get() + 1);
                    Err(io::Error::other("simulierter Fehler vor Queue-Rename"))
                },
            )
            .unwrap_err();
        assert_eq!(writes.get(), 2);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);

        let live = &queue.snapshot().entries[0];
        assert_eq!(live.state, crate::QueueState::Interrupted);
        assert_eq!(
            live.bidirectional_completion_recovery,
            Some(crate::queue::BidirectionalCompletionRecovery::RolledBack)
        );
        assert_eq!(
            crate::error_code(&queue.retry(&queued.operation_id).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );

        let resume_error = prepare_resume_local_bidirectional(
            "Rollbackpaar",
            Path::new("darf-nicht-geoeffnet-werden-links"),
            Path::new("darf-nicht-geoeffnet-werden-rechts"),
            &baseline_path,
        )
        .err()
        .expect("RollbackCommitted darf nie als normaler Resume-Lauf starten");
        assert_eq!(resume_error.kind(), io::ErrorKind::AlreadyExists);
        let rollback_error = prepare_local_bidirectional_rollback(
            "Rollbackpaar",
            Path::new("darf-nicht-geoeffnet-werden-links"),
            Path::new("darf-nicht-geoeffnet-werden-rechts"),
            &baseline_path,
        )
        .err()
        .expect("RollbackCommitted darf keinen zweiten Rollback starten");
        assert_eq!(rollback_error.kind(), io::ErrorKind::AlreadyExists);
        drop(queue);

        let queue = crate::OperationQueue::open(&queue_path).unwrap();
        let recovered = &queue.snapshot().entries[0];
        assert_eq!(recovered.state, crate::QueueState::Interrupted);
        assert!(!recovered.recovery_available);
        assert_eq!(
            recovered.bidirectional_completion_recovery,
            Some(crate::queue::BidirectionalCompletionRecovery::RolledBack)
        );
        assert_eq!(
            crate::error_code(&queue.retry(&queued.operation_id).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );

        // Auch der wiederaufgenommene Abschluss schreibt zuerst den terminalen
        // Queuezustand samt Marker. Erst danach simulieren wir ein wirksames
        // Unlink, dessen Parent-fsync weiterhin fehlschlägt.
        queue
            .finish_bidirectional_after_effect(
                &queued.operation_id,
                true,
                crate::Progress::default(),
            )
            .unwrap();
        let terminal = &queue.snapshot().entries[0];
        assert_eq!(terminal.state, crate::QueueState::Cancelled);
        assert_eq!(
            terminal.bidirectional_completion_recovery,
            Some(crate::queue::BidirectionalCompletionRecovery::RolledBack)
        );

        let store = BaselineStore::open(&baseline_path).unwrap();
        let expected = store.load_run_journal().unwrap().unwrap();
        let syncs = std::cell::Cell::new(0_u8);
        let cleanup_error = store
            .remove_run_journal_reconciled_using(
                &expected,
                |path| std::fs::remove_file(path),
                |_| {
                    syncs.set(syncs.get() + 1);
                    Err(io::Error::other("simulierter Parent-fsync-Fehler"))
                },
            )
            .unwrap_err();
        assert_eq!(
            crate::error_code(&cleanup_error),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(syncs.get(), 2);
        assert!(store.load_run_journal().unwrap().is_none());
        drop(store);
        drop(queue);

        let queue = crate::OperationQueue::open(&queue_path).unwrap();
        assert_eq!(
            crate::error_code(&queue.retry(&queued.operation_id).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        queue
            .finish_bidirectional_completion_recovery(&queued.operation_id)
            .unwrap();
        let finished = &queue.snapshot().entries[0];
        assert_eq!(finished.state, crate::QueueState::Cancelled);
        assert!(finished.bidirectional_completion_recovery.is_none());
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::Absent
        );
    }

    #[test]
    fn rollbackabschluss_bleibt_an_die_alte_baseline_gebunden() {
        let fixture = Fixture::new();
        write(&fixture.left.join("bericht.txt"), "alt");
        write(&fixture.right.join("bericht.txt"), "alt");
        let pair = PairId([114; 16]);
        let baseline_path = fixture._temp.path().join("state/bound-rollback.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("bericht.txt"), "links");
        write(&fixture.right.join("bericht.txt"), "rechts");

        let conflict_name = "bericht-rechts-rollback.txt";
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Gebundener Rollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution(
                "bericht.txt",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(vec![conflict_name.as_bytes().to_vec()]),
                },
            )],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        write(&fixture.right.join(conflict_name), "spaete Kollision");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        std::fs::remove_file(fixture.right.join(conflict_name)).unwrap();

        let rollback = prepare_local_bidirectional_rollback(
            "Gebundener Rollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = rollback.preview().confirmation_phrase.clone();
        rollback
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();

        let store = BaselineStore::open(&baseline_path).unwrap();
        let original = store.load().unwrap().unwrap();
        let mut changed = original.clone();
        changed.generation += 1;
        changed.body_digest = changed.canonical_digest().unwrap();
        store.save(&changed).unwrap();
        drop(store);

        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::ResumeOrRollback
        );
        let error = finish_bidirectional_rollback_completion(&baseline_path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(load_bidirectional_run_journal(&baseline_path)
            .unwrap()
            .is_some());

        let store = BaselineStore::open(&baseline_path).unwrap();
        store.save(&original).unwrap();
        drop(store);
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::RollbackCompletionCleanup
        );
        finish_bidirectional_rollback_completion(&baseline_path).unwrap();
        assert!(load_bidirectional_run_journal(&baseline_path)
            .unwrap()
            .is_none());
    }

    #[test]
    fn legacy_v7_baselineabschluss_ohne_folgedigest_bleibt_nur_cleanup() {
        let fixture = Fixture::new();
        let pair = PairId([96; 16]);
        let baseline_path = fixture._temp.path().join("state/legacy-v7.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("neu.txt"), "legacy");

        let prepared = prepare_local_bidirectional(
            pair,
            "Legacy-v7-Paar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase, false)
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();

        let store = BaselineStore::open(&baseline_path).unwrap();
        store
            .update_run_journal(|journal| {
                assert_eq!(journal.version, RUN_JOURNAL_VERSION);
                assert_eq!(journal.state, BidirectionalRunState::BaselineCommitted);
                journal.committed_baseline_digest = None;
                Ok(())
            })
            .unwrap();
        let serialized = std::fs::read_to_string(store.run_journal_path()).unwrap();
        assert!(!serialized.contains("committed_baseline_digest"));
        drop(store);

        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::CompletionCleanup
        );
        let resume = prepare_resume_local_bidirectional(
            "Legacy-v7-Paar",
            Path::new("darf-nicht-geoeffnet-werden-links"),
            Path::new("darf-nicht-geoeffnet-werden-rechts"),
            &baseline_path,
        )
        .err()
        .expect("Legacy-BaselineCommitted darf nie Resume anbieten");
        assert_eq!(resume.kind(), io::ErrorKind::AlreadyExists);
        finish_bidirectional_run_completion(&baseline_path).unwrap();
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::Absent
        );
    }

    #[test]
    fn additiver_verzeichnis_teilcommit_rollt_als_ganzer_unterbaum_zurueck() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.left.join("projekt")).unwrap();
        std::fs::create_dir(fixture.right.join("projekt")).unwrap();
        std::fs::set_permissions(
            fixture.left.join("projekt"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("projekt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write(&fixture.left.join("projekt/links"), "links");
        write(&fixture.right.join("projekt/rechts"), "rechts");
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let conflict_name = "projekt-rechts-rollback";
        let prepared = prepare_local_bidirectional_with_decisions(
            PairId([57; 16]),
            "Ordnerrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution(
                "projekt",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(vec![conflict_name.as_bytes().to_vec()]),
                },
            )],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        std::fs::create_dir(fixture.right.join(conflict_name)).unwrap();
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        std::fs::remove_dir(fixture.right.join(conflict_name)).unwrap();
        assert_eq!(
            std::fs::read(fixture.left.join(conflict_name).join("rechts")).unwrap(),
            b"rechts"
        );

        let prepared = prepare_local_bidirectional_rollback(
            "Ordnerrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert!(!fixture.left.join(conflict_name).exists());
        assert_eq!(stats.projections_rolled_back, 1);
        assert_eq!(
            std::fs::read(
                Path::new(&stats.rollback_quarantines[0])
                    .join(conflict_name)
                    .join("rechts")
            )
            .unwrap(),
            b"rechts"
        );
    }

    #[test]
    fn verzeichnisprojektion_rollt_addition_und_modus_pfadgetreu_zurueck() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.left.join("projekt")).unwrap();
        std::fs::create_dir(fixture.right.join("projekt")).unwrap();
        // Die Baseline darf nicht von der pro Rechner konfigurierten umask abhängen.
        // Nur die spätere linke 0700-Änderung soll als echte Metadatenwirkung gelten.
        for root in [&fixture.left, &fixture.right] {
            std::fs::set_permissions(root.join("projekt"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        for index in 0..48 {
            let name = format!("gleich-{index:02}");
            write(&fixture.left.join("projekt").join(&name), "gleich");
            write(&fixture.right.join("projekt").join(&name), "gleich");
        }
        write(&fixture.left.join("projekt/alt"), "alt");
        write(&fixture.right.join("projekt/alt"), "alt");
        write(&fixture.left.join("spaeter-loeschen"), "gebunden");
        write(&fixture.right.join("spaeter-loeschen"), "gebunden");
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let pair = PairId([58; 16]);
        prepare_and_run(&fixture, pair, &baseline_path, false);

        std::fs::set_permissions(
            fixture.left.join("projekt"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            fixture.right.join("projekt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write(&fixture.left.join("projekt/neu"), "neu");
        std::fs::remove_file(fixture.left.join("spaeter-loeschen")).unwrap();
        let prepared = prepare_local_bidirectional(
            pair,
            "Unterbaumrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        // Die aufgeschobene Löschung muss den fremden Inhalt erkennen. Davor sind
        // die gebundenen Projekt-Unterbaumprojektionen bereits committed.
        write(&fixture.right.join("spaeter-loeschen"), "fremd");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        write(&fixture.right.join("spaeter-loeschen"), "gebunden");
        assert!(fixture.right.join("projekt/neu").exists());
        assert!(fixture.right.join("projekt/alt").exists());

        let prepared = prepare_local_bidirectional_rollback(
            "Unterbaumrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert!(!fixture.right.join("projekt/neu").exists());
        assert_eq!(
            std::fs::read(fixture.right.join("projekt/alt")).unwrap(),
            b"alt"
        );
        assert_eq!(
            std::fs::metadata(fixture.right.join("projekt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read(fixture.right.join("spaeter-loeschen")).unwrap(),
            b"gebunden"
        );
    }

    #[test]
    fn geloeschter_verzeichnis_teilcommit_wird_mit_unterbaum_wiederhergestellt() {
        let fixture = Fixture::new();
        for index in 0..48 {
            let name = format!("bestand-{index:02}");
            write(&fixture.left.join(&name), "bleibt");
            write(&fixture.right.join(&name), "bleibt");
        }
        std::fs::create_dir(fixture.left.join("weg")).unwrap();
        std::fs::create_dir(fixture.right.join("weg")).unwrap();
        write(&fixture.left.join("weg/kind"), "original");
        write(&fixture.right.join("weg/kind"), "original");
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let pair = PairId([59; 16]);
        prepare_and_run(&fixture, pair, &baseline_path, false);
        std::fs::remove_dir_all(fixture.left.join("weg")).unwrap();
        let prepared = prepare_local_bidirectional(
            pair,
            "Ordnerlöschrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            store,
            runs,
            run_ordinals,
            ..
        } = run;
        assert_eq!(run_ordinals, vec![0]);
        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                Ok(())
            })
            .unwrap();
        drop(store);
        assert!(!fixture.right.join("weg").exists());

        let prepared = prepare_local_bidirectional_rollback(
            "Ordnerlöschrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        prepared
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert!(!fixture.left.join("weg").exists());
        assert_eq!(
            std::fs::read(fixture.right.join("weg/kind")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn rollback_crash_nach_claim_wird_aus_dem_baumzustand_abgeschlossen() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let pair = PairId([46; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let conflict_name = "a-rollback";
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Crashrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution(
                "a",
                ConflictDecision::KeepBoth {
                    primary: EndpointSide::Left,
                    conflict_path: BinaryRelativePath(vec![conflict_name.as_bytes().to_vec()]),
                },
            )],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        write(&fixture.right.join(conflict_name), "fremd");
        assert!(run
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .is_err());
        std::fs::remove_file(fixture.right.join(conflict_name)).unwrap();

        let prepared = prepare_local_bidirectional_rollback(
            "Crashrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let rollback = prepared.confirm(&phrase).unwrap();
        let LocalBidirectionalRollback {
            left_root,
            left_vfs,
            store,
            mut journal,
            ..
        } = rollback;
        let delta = projection_rollback_units(&journal, 0).unwrap().remove(0);
        let (expected_left, _) = expected_maps_after_prefix(&journal, 1).unwrap();
        let parent = left_root.parent().unwrap();
        let mut quarantine_leases = Vec::new();
        let run_root = ensure_rollback_quarantine(
            &store,
            &mut journal,
            EndpointSide::Left,
            left_vfs.as_ref(),
            parent,
            &left_root,
            &mut quarantine_leases,
        )
        .unwrap();
        quarantine_rollback_entry(
            left_vfs.as_ref(),
            &left_root,
            &run_root,
            &delta.path,
            delta.after.as_ref().unwrap(),
            &expected_left,
        )
        .unwrap();
        // Simulation: Prozess endet nach dem Rename, aber vor dem Status-Commit.
        drop(quarantine_leases);
        drop(store);

        let resumed = prepare_local_bidirectional_rollback(
            "Crashrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        assert_eq!(resumed.preview().committed_projections, 0);
        let phrase = resumed.preview().confirmation_phrase.clone();
        let stats = resumed
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert_eq!(stats.projections_rolled_back, 0);
        assert_eq!(
            std::fs::read(Path::new(&stats.rollback_quarantines[0]).join(conflict_name)).unwrap(),
            b"rechts"
        );
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::RollbackCompletionCleanup
        );
        finish_bidirectional_rollback_completion(&baseline_path).unwrap();
        assert!(load_bidirectional_run_journal(&baseline_path)
            .unwrap()
            .is_none());
    }

    #[test]
    fn ersetzungs_teilcommit_quarantaenisiert_neuversion_und_stellt_altversion_wieder_her() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "baseline");
        write(&fixture.right.join("a"), "baseline");
        let pair = PairId([47; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("a"), "links-neu");
        write(&fixture.right.join("a"), "rechts-neu");
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Ersetzungsrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution("a", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            store,
            runs,
            run_ordinals,
            right_root,
            right_vfs,
            ..
        } = run;
        assert_eq!(run_ordinals, vec![0]);
        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                Ok(())
            })
            .unwrap();
        drop(store);
        assert_eq!(
            std::fs::read(fixture.right.join("a")).unwrap(),
            b"links-neu"
        );

        let forward_run = PathBuf::from(result.quarantine.as_ref().unwrap());
        let foreign_lease =
            crate::quarantine::lease_quarantine_run(right_vfs.as_ref(), &forward_run, &right_root)
                .unwrap();
        let blocked = prepare_local_bidirectional_rollback(
            "Ersetzungsrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let blocked_phrase = blocked.preview().confirmation_phrase.clone();
        let error = blocked
            .confirm(&blocked_phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::QuarantineBusy);
        assert_eq!(
            std::fs::read(fixture.right.join("a")).unwrap(),
            b"links-neu",
            "Lease-Konflikt muss vor der ersten Rollback-Dateimutation wirken"
        );
        drop(foreign_lease);

        let prepared = prepare_local_bidirectional_rollback(
            "Ersetzungsrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let stats = prepared
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert_eq!(stats.projections_rolled_back, 1);
        assert_eq!(std::fs::read(fixture.left.join("a")).unwrap(), b"links-neu");
        assert_eq!(
            std::fs::read(fixture.right.join("a")).unwrap(),
            b"rechts-neu"
        );
        assert_eq!(
            std::fs::read(Path::new(&stats.rollback_quarantines[0]).join("a")).unwrap(),
            b"links-neu"
        );
        let baseline = BaselineStore::open(&baseline_path)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(baseline.generation, 1);
    }

    #[test]
    fn ersetzungsrollback_setzt_nach_crash_zwischen_beiden_renames_fort() {
        let fixture = Fixture::new();
        write(&fixture.left.join("a"), "alt");
        write(&fixture.right.join("a"), "alt");
        let pair = PairId([48; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        write(&fixture.left.join("a"), "links");
        write(&fixture.right.join("a"), "rechts");
        let prepared = prepare_local_bidirectional_with_decisions(
            pair,
            "Crashrestore",
            &fixture.left,
            &fixture.right,
            &baseline_path,
            &[resolution("a", ConflictDecision::KeepLeft)],
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            store,
            runs,
            run_ordinals,
            ..
        } = run;
        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        assert_eq!(run_ordinals, vec![0]);
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                Ok(())
            })
            .unwrap();
        drop(store);

        let prepared = prepare_local_bidirectional_rollback(
            "Crashrestore",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let rollback = prepared.confirm(&phrase).unwrap();
        let LocalBidirectionalRollback {
            right_root,
            right_vfs,
            store,
            mut journal,
            ..
        } = rollback;
        let delta = projection_rollback_units(&journal, 0).unwrap().remove(0);
        let (_, expected_right) = expected_maps_after_prefix(&journal, 1).unwrap();
        journal.rollback_step = Some(BidirectionalRollbackStep {
            ordinal: 0,
            unit_index: 0,
            side: EndpointSide::Right,
            path: delta.path.clone(),
            phase: BidirectionalRollbackPhase::QuarantineCurrent,
        });
        journal.refresh_digest().unwrap();
        store.save_run_journal(&journal).unwrap();
        let mut quarantine_leases = Vec::new();
        let run_root = ensure_rollback_quarantine(
            &store,
            &mut journal,
            EndpointSide::Right,
            right_vfs.as_ref(),
            right_root.parent().unwrap(),
            &right_root,
            &mut quarantine_leases,
        )
        .unwrap();
        ensure_current_quarantined(
            right_vfs.as_ref(),
            &right_root,
            &run_root,
            &delta.path,
            delta.after.as_ref().unwrap(),
            delta.before.as_ref(),
            &expected_right,
        )
        .unwrap();
        journal.rollback_step.as_mut().unwrap().phase = BidirectionalRollbackPhase::RestorePrevious;
        journal.refresh_digest().unwrap();
        store.save_run_journal(&journal).unwrap();
        assert!(!fixture.right.join("a").exists());
        drop(quarantine_leases);
        drop(store);

        let resumed = prepare_local_bidirectional_rollback(
            "Crashrestore",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = resumed.preview().confirmation_phrase.clone();
        resumed
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"rechts");
        assert_eq!(
            classify_bidirectional_run_recovery(&baseline_path).unwrap(),
            BidirectionalRunRecovery::RollbackCompletionCleanup
        );
        finish_bidirectional_rollback_completion(&baseline_path).unwrap();
        assert!(load_bidirectional_run_journal(&baseline_path)
            .unwrap()
            .is_none());
    }

    #[test]
    fn loesch_teilcommit_wird_ohne_ueberschreiben_wiederhergestellt() {
        let fixture = Fixture::new();
        for index in 0..20 {
            let name = format!("bestand-{index:02}");
            write(&fixture.left.join(&name), "bleibt");
            write(&fixture.right.join(&name), "bleibt");
        }
        write(&fixture.left.join("a"), "original");
        write(&fixture.right.join("a"), "original");
        let pair = PairId([49; 16]);
        let baseline_path = fixture._temp.path().join("state/pair.json");
        prepare_and_run(&fixture, pair, &baseline_path, false);
        std::fs::remove_file(fixture.left.join("a")).unwrap();
        let prepared = prepare_local_bidirectional(
            pair,
            "Löschrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, true).unwrap();
        let LocalBidirectionalRun {
            store,
            runs,
            run_ordinals,
            ..
        } = run;
        store
            .update_run_journal(|journal| {
                journal.state = BidirectionalRunState::Running;
                journal.projections[0].state = BidirectionalProjectionState::Running;
                Ok(())
            })
            .unwrap();
        let result = runs
            .into_iter()
            .next()
            .unwrap()
            .execute(&crate::Cancel::new(), &mut ignore_progress)
            .unwrap();
        assert_eq!(run_ordinals, vec![0]);
        store
            .update_run_journal(|journal| {
                journal.projections[0].state = BidirectionalProjectionState::Committed;
                journal.projections[0].quarantine = result.quarantine.clone();
                Ok(())
            })
            .unwrap();
        drop(store);
        assert!(!fixture.right.join("a").exists());

        let rollback = prepare_local_bidirectional_rollback(
            "Löschrollback",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let phrase = rollback.preview().confirmation_phrase.clone();
        let stats = rollback
            .confirm(&phrase)
            .unwrap()
            .execute(&crate::Cancel::new())
            .unwrap();
        assert_eq!(stats.projections_rolled_back, 1);
        assert!(stats.rollback_quarantines.is_empty());
        assert!(!fixture.left.join("a").exists());
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"original");
    }

    #[test]
    fn journal_korruption_und_unbekannte_version_scheitern_fail_closed() {
        let fixture = Fixture::new();
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional(
            PairId([43; 16]),
            "Journalpaar",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let path = prepared.store.run_journal_path();
        drop(prepared);
        let mut journal: BidirectionalRunJournal =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        journal.version = 99;
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(load_bidirectional_run_journal(&baseline_path).is_err());
        journal.version = RUN_JOURNAL_VERSION;
        journal.body_digest[0] ^= 0xff;
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(load_bidirectional_run_journal(&baseline_path).is_err());
    }

    #[test]
    fn altes_webdav_laufjournal_ohne_root_etag_verlangt_neuplanung() {
        let fixture = Fixture::new();
        let baseline_path = fixture._temp.path().join("state/pair.json");
        let prepared = prepare_local_bidirectional(
            PairId([44; 16]),
            "Altes WebDAV-Journal",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let path = prepared.store.run_journal_path();
        drop(prepared);
        let webdav_root = RootIdentity::WebDav(crate::remote::WebDavRootIdentity {
            server: crate::remote::WebDavServerIdentity {
                origin: "https://dav.example.test".into(),
                certificate: crate::remote::WebDavCertificatePin { sha256: [1; 32] },
                account_digest: [2; 32],
                canonical_root: "/daten".into(),
                dav_header_digest: [3; 32],
                server_header_digest: [4; 32],
                legacy_server_header_digests: Vec::new(),
            },
            virtual_root: "/daten".into(),
            root_etag: "\"root-1\"".into(),
            session_generation: 1,
        });
        let mut legacy_root = serde_json::to_value(webdav_root).unwrap();
        legacy_root["identity"]
            .as_object_mut()
            .unwrap()
            .remove("root_etag");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        journal["plan"]["left_root_identity"] = legacy_root;
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let error = load_bidirectional_run_journal(&baseline_path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("Älteres WebDAV-Laufjournal"));
        assert!(error.to_string().contains("vor der ersten Projektion"));
        assert!(
            path.exists(),
            "Laufjournal muss für Sichtprüfung erhalten bleiben"
        );
    }

    #[test]
    fn altes_webdav_laufjournal_mit_teilwirkung_verlangt_manuelle_recovery() {
        let fixture = Fixture::new();
        let baseline_path = fixture._temp.path().join("state/partial-webdav.json");
        let pair = PairId([45; 16]);
        prepare_and_run(&fixture, pair, &baseline_path, false);
        std::fs::write(fixture.left.join("neu.txt"), b"neu").unwrap();
        let prepared = prepare_local_bidirectional(
            pair,
            "Partielles WebDAV-Journal",
            &fixture.left,
            &fixture.right,
            &baseline_path,
        )
        .unwrap();
        let path = prepared.store.run_journal_path();
        drop(prepared);
        let webdav_root = RootIdentity::WebDav(crate::remote::WebDavRootIdentity {
            server: crate::remote::WebDavServerIdentity {
                origin: "https://dav.example.test".into(),
                certificate: crate::remote::WebDavCertificatePin { sha256: [1; 32] },
                account_digest: [2; 32],
                canonical_root: "/daten".into(),
                dav_header_digest: [3; 32],
                server_header_digest: [4; 32],
                legacy_server_header_digests: Vec::new(),
            },
            virtual_root: "/daten".into(),
            root_etag: "\"root-1\"".into(),
            session_generation: 1,
        });
        let mut legacy_root = serde_json::to_value(webdav_root).unwrap();
        legacy_root["identity"]
            .as_object_mut()
            .unwrap()
            .remove("root_etag");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        journal["plan"]["left_root_identity"] = legacy_root;
        journal["state"] = serde_json::Value::String("running".into());
        let projections = journal["projections"].as_array_mut().unwrap();
        assert!(!projections.is_empty());
        projections[0]["state"] = serde_json::Value::String("committed".into());
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let error = load_bidirectional_run_journal(&baseline_path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("Teilwirkungen"));
        assert!(error.to_string().contains("manuell prüfen"));
        assert!(error
            .to_string()
            .contains("weder fortsetzen noch neu planen"));
        assert!(path.exists());
    }
}
