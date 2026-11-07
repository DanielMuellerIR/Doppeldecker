//! Protokollneutrale Metadaten-DTOs und kanonische Manifest-Digests.
//!
//! Die ersten Stufe-2-Schnitte beschreiben Fähigkeiten, binden Metadaten eindeutig an
//! einen Digest und übertragen Owner, Sonderbits, Linux-xattrs sowie macOS-Ressourcen-
//! Forks, ACLs, Sparse-Extents und laufbezogene Hardlink-Gruppen mit exaktem Readback.

use std::io;
use std::path::Path;

/// Gewählter Umfang des Metadatenvertrags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MetadataProfile {
    #[default]
    Portable,
    NativeStrict,
    NativeBestEffort,
}

/// Separater Inhaltsstrom eines Eintrags. Weitere Fork-Arten werden erst ergänzt,
/// wenn ein Provider dafür eine eindeutige, verlustfreie Semantik besitzt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ForkKind {
    Resource,
}

/// Fähigkeit eines konkreten Providers beziehungsweise Ziel-Mounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Capability {
    Exact,
    ReadOnly,
    Unsupported,
    Unknown,
}

impl Capability {
    fn unknown() -> Self {
        Self::Unknown
    }

    fn is_unknown(&self) -> bool {
        *self == Self::Unknown
    }
}

/// ACL-Modelle sind nicht verlustfrei ineinander übersetzbar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AclModel {
    Posix,
    Nfs4,
    WindowsSid,
    Unsupported,
    Unknown,
}

/// Vollständig typisierte ACL. Die Varianten bleiben absichtlich getrennt, weil eine
/// POSIX-Maske und eine geordnete NFSv4-Allow/Deny-Liste nicht verlustfrei ineinander
/// übersetzt werden können.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "model", content = "value")]
pub enum Acl {
    Posix(PosixAcl),
    Nfs4(Nfs4Acl),
    WindowsDacl(crate::windows_security::WindowsDacl),
}

/// Vererbung am lokalen Ziel-Elternordner, getrennt von ausdrücklich zu
/// übertragenden Quellmetadaten. Der bestätigte Sync-Plan bindet diesen Zustand.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalParentInheritance {
    pub identity: crate::vfs::LocalRootIdentity,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub acl: Option<Acl>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Nfs4Acl {
    pub entries: Vec<Nfs4AclEntry>,
    /// ACL-weite Flags; derzeit kennt macOS `ACL_FLAG_NO_INHERIT` (`1 << 17`).
    pub flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PosixAcl {
    pub access: Vec<PosixAclEntry>,
    pub default: Vec<PosixAclEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PosixAclEntry {
    pub tag: PosixAclTag,
    /// Nur bei `User` und `Group` gesetzt; Namen sind kein stabiler Schlüssel.
    pub id: Option<u32>,
    /// Bit 2 = lesen, Bit 1 = schreiben, Bit 0 = ausführen.
    pub permissions: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PosixAclTag {
    UserObject,
    User,
    GroupObject,
    Group,
    Mask,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Nfs4AclEntry {
    /// macOS-ACLs identifizieren Nutzer und Gruppen stabil über ihre 128-Bit-UUID.
    pub qualifier: [u8; 16],
    pub kind: Nfs4AceKind,
    pub permissions: u64,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Nfs4AceKind {
    Allow,
    Deny,
}

impl Acl {
    pub fn model(&self) -> AclModel {
        match self {
            Self::Posix(_) => AclModel::Posix,
            Self::Nfs4(_) => AclModel::Nfs4,
            Self::WindowsDacl(_) => AclModel::WindowsSid,
        }
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        match self {
            Self::Posix(acl) => {
                validate_posix_entries(&acl.access, false)?;
                validate_posix_entries(&acl.default, true)
            }
            Self::WindowsDacl(acl) => acl.validate(),
            Self::Nfs4(acl) => {
                if acl.entries.len() > 128 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "NFSv4-ACL enthält mehr als 128 Einträge",
                    ));
                }
                const ACL_FLAGS: u32 = 1 << 17;
                const ENTRY_FLAGS: u32 = (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8);
                if acl.flags & !ACL_FLAGS != 0
                    || acl
                        .entries
                        .iter()
                        .any(|entry| entry.flags & !ENTRY_FLAGS != 0)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "NFSv4-ACL enthält unbekannte Vererbungsflags",
                    ));
                }
                Ok(())
            }
        }
    }
}

fn validate_posix_entries(entries: &[PosixAclEntry], default: bool) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if !default && entries.len() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "POSIX-Access-ACL ist unvollständig",
        ));
    }
    let mut previous = None;
    for entry in entries {
        if entry.permissions & !0o7 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "POSIX-ACL enthält unbekannte Rechtebits",
            ));
        }
        let needs_id = matches!(entry.tag, PosixAclTag::User | PosixAclTag::Group);
        if needs_id != entry.id.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "POSIX-ACL-Qualifier passt nicht zum Tag",
            ));
        }
        let key = (posix_tag_byte(entry.tag), entry.id.unwrap_or(0));
        if previous.is_some_and(|value| value >= key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "POSIX-ACL-Einträge sind nicht eindeutig kanonisch sortiert",
            ));
        }
        previous = Some(key);
    }
    for required in [
        PosixAclTag::UserObject,
        PosixAclTag::GroupObject,
        PosixAclTag::Other,
    ] {
        if !entries.iter().any(|entry| entry.tag == required) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "POSIX-ACL enthält nicht alle Basiseinträge",
            ));
        }
    }
    let named = entries
        .iter()
        .any(|entry| matches!(entry.tag, PosixAclTag::User | PosixAclTag::Group));
    if named && !entries.iter().any(|entry| entry.tag == PosixAclTag::Mask) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Erweiterte POSIX-ACL benötigt eine Maske",
        ));
    }
    Ok(())
}

/// Vor dem Scan ermittelte Metadatenfähigkeiten eines Providers am konkreten Root.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataCapabilities {
    pub owner: Capability,
    pub special_mode_bits: Capability,
    pub acl_model: AclModel,
    #[serde(
        default = "Capability::unknown",
        skip_serializing_if = "Capability::is_unknown"
    )]
    pub acl: Capability,
    pub xattrs: Capability,
    pub resource_fork: Capability,
    #[serde(
        default = "Capability::unknown",
        skip_serializing_if = "Capability::is_unknown"
    )]
    pub named_streams: Capability,
    pub sparse_extents: Capability,
    pub hardlinks: Capability,
}

impl MetadataCapabilities {
    /// Sicherer Standard für Provider, die noch keine konkrete Probe implementieren.
    pub fn unknown() -> Self {
        Self {
            owner: Capability::Unknown,
            special_mode_bits: Capability::Unknown,
            acl_model: AclModel::Unknown,
            acl: Capability::Unknown,
            xattrs: Capability::Unknown,
            resource_fork: Capability::Unknown,
            named_streams: Capability::Unknown,
            sparse_extents: Capability::Unknown,
            hardlinks: Capability::Unknown,
        }
    }
}

impl Default for MetadataCapabilities {
    fn default() -> Self {
        Self::unknown()
    }
}

/// Numerischer Eigentümer; Namen wären kein stabiler, rechnerübergreifender Schlüssel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// Größe und Inhaltsdigest eines potenziell großen Metadaten-Blobs.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobSummary {
    pub size: u64,
    pub digest: [u8; 32],
}

impl BlobSummary {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            size: bytes.len() as u64,
            digest: *blake3::hash(bytes).as_bytes(),
        }
    }
}

/// Bytegenauer Name und Zusammenfassung eines xattrs oder benannten Datenstroms.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NamedBlobSummary {
    pub name: Vec<u8>,
    pub value: BlobSummary,
}

/// Bytegenauer xattr-Wert für den descriptor-relativen VFS-Datenpfad. Im UI-Plan wird
/// daraus nur `NamedBlobSummary`; große Werte werden dort niemals eingebettet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XattrValue {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// Ein belegter Datenbereich einer Sparse-Datei.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DataExtent {
    pub offset: u64,
    pub length: u64,
}

/// Beobachteter Windows-Sicherheitskontext, getrennt vom numerischen UID/GID-Owner.
/// Die SID-Namensräume werden weder übersetzt noch als Schreibfreigabe verwendet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowsSecurityObservation {
    pub namespace: [u8; 16],
    pub owner: Option<crate::windows_security::WindowsSid>,
    pub group: Option<crate::windows_security::WindowsSid>,
    pub issue: Option<WindowsSecurityReadIssue>,
    pub invalid_descriptor_digest: Option<[u8; 32]>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WindowsSecurityReadIssue {
    ReadControlDenied,
    QueryUnsupported,
    InvalidDescriptor,
}
impl WindowsSecurityReadIssue {
    pub fn reason(self) -> &'static str {
        match self {
            Self::ReadControlDenied => {
                "READ_CONTROL für den Windows-Sicherheitsdeskriptor wurde verweigert"
            }
            Self::QueryUnsupported => {
                "Der Server unterstützt die abgefragten Windows-Sicherheitsinformationen nicht"
            }
            Self::InvalidDescriptor => {
                "Der gelieferte Windows-Sicherheitsdeskriptor ist strukturell ungültig"
            }
        }
    }
}
impl WindowsSecurityObservation {
    pub fn validate(&self) -> io::Result<()> {
        if self.namespace == [0; 16]
            || (self.issue.is_some() && (self.owner.is_some() || self.group.is_some()))
            || (self.issue == Some(WindowsSecurityReadIssue::InvalidDescriptor))
                != self.invalid_descriptor_digest.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ungültige Windows-Sicherheitsbeobachtung",
            ));
        }
        for sid in self.owner.iter().chain(&self.group) {
            sid.validate()?;
        }
        Ok(())
    }
}

/// Konkrete Auslassung der lesenden SMB-Sparse-Abfrage, niemals eine leere Liste.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SparseReadIssue {
    ServerStatus(u32),
    BufferLimit,
    InvalidResponse,
}

impl SparseReadIssue {
    pub fn validate(self) -> io::Result<()> {
        if matches!(self, Self::ServerStatus(status) if !matches!(status,
            0xc000_0002 | 0xc000_0003 | 0xc000_0010 | 0xc000_000d | 0xc000_0022 | 0xc000_00bb))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ungültiger optionaler Sparse-Abfragestatus",
            ));
        }
        Ok(())
    }
}

/// Kanonisches Manifest eines Eintrags. Große Werte werden nur über Größe und Digest
/// gebunden; der eigentliche Datenstrom bleibt außerhalb des UI-Plans.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataManifest {
    pub owner: Option<Owner>,
    /// Vollständige POSIX-Rechte einschließlich setuid/setgid/sticky (`0o7777`).
    pub mode: u32,
    pub acl: Option<Acl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_security: Option<WindowsSecurityObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webdav_security: Option<crate::webdav_security::WebDavSecurityObservation>,
    pub xattrs: Vec<NamedBlobSummary>,
    pub resource_fork: Option<BlobSummary>,
    /// `None` ist unbeobachtet; `Some([])` belegt die vollständige leere Menge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named_streams: Option<Vec<NamedBlobSummary>>,
    pub sparse_extents: Option<Vec<DataExtent>>,
    /// Laufbezogene Gruppe; erst der spätere Scanner weist sie anhand Gerät/Inode zu.
    pub hardlink_group: Option<u64>,
    /// Gemeldete Linkzahl einer regulären Datei; kein Beleg für konkrete Aliasnamen
    /// oder eine vollständige, volumengebundene Hardlink-Gruppe. SMB meldet u32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_hard_links: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_read_issue: Option<SparseReadIssue>,
    pub digest: [u8; 32],
}

impl MetadataManifest {
    /// Validiert variable Listen, sortiert xattrs byteweise und erzeugt den stabilen
    /// Digest. Dadurch ergibt dieselbe Semantik unabhängig von Provider-Reihenfolgen
    /// immer denselben Planwert.
    pub fn new(
        owner: Option<Owner>,
        mode: u32,
        acl: Option<Acl>,
        mut xattrs: Vec<NamedBlobSummary>,
        resource_fork: Option<BlobSummary>,
        sparse_extents: Option<Vec<DataExtent>>,
        hardlink_group: Option<u64>,
    ) -> io::Result<Self> {
        if mode & !0o7777 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Metadatenmodus enthält Dateityp- oder unbekannte Bits",
            ));
        }
        xattrs.sort_by(|left, right| left.name.cmp(&right.name));

        let mut manifest = Self {
            owner,
            mode: mode & 0o7777,
            acl,
            windows_security: None,
            webdav_security: None,
            xattrs,
            resource_fork,
            named_streams: None,
            sparse_extents,
            hardlink_group,
            reported_hard_links: None,
            sparse_read_issue: None,
            digest: [0; 32],
        };
        manifest.validate_structure()?;
        manifest.digest = manifest.canonical_digest();
        Ok(manifest)
    }

    pub fn with_windows_security(
        mut self,
        observation: Option<WindowsSecurityObservation>,
    ) -> io::Result<Self> {
        self.windows_security = observation;
        self.validate_structure()?;
        self.digest = self.canonical_digest();
        Ok(self)
    }

    pub fn with_webdav_security(
        mut self,
        observation: Option<crate::webdav_security::WebDavSecurityObservation>,
    ) -> io::Result<Self> {
        self.webdav_security = observation;
        self.validate_structure()?;
        self.digest = self.canonical_digest();
        Ok(self)
    }

    /// Bindet eine vollständig beobachtete, byteweise kanonische Strommenge.
    /// Unbeobachtete Legacy-Manifeste behalten ihren bisherigen V2-Digest.
    pub fn with_named_streams(
        mut self,
        mut streams: Option<Vec<NamedBlobSummary>>,
    ) -> io::Result<Self> {
        if let Some(streams) = &mut streams {
            streams.sort_by(|left, right| left.name.cmp(&right.name));
        }
        self.named_streams = streams;
        self.validate_structure()?;
        self.digest = self.canonical_digest();
        Ok(self)
    }

    /// Bindet die vom laufbezogenen Scanner vergebene Hardlink-Gruppe neu an den
    /// kanonischen Digest. Die lokale Einzeldatei-Probe kennt diese Gruppe bewusst
    /// noch nicht; erst der vollständige Baumscan kann gleiche Inodes zusammenführen.
    pub fn with_hardlink_group(mut self, group: Option<u64>) -> io::Result<Self> {
        self.hardlink_group = group;
        self.validate_structure()?;
        self.digest = self.canonical_digest();
        Ok(self)
    }

    pub fn with_reported_hard_links(mut self, count: Option<u32>) -> io::Result<Self> {
        self.reported_hard_links = count;
        self.validate_structure()?;
        self.digest = self.canonical_digest();
        Ok(self)
    }

    pub fn with_sparse_observation(
        mut self,
        extents: Option<Vec<DataExtent>>,
        issue: Option<SparseReadIssue>,
    ) -> io::Result<Self> {
        self.sparse_extents = extents;
        self.sparse_read_issue = issue;
        self.validate_structure()?;
        self.digest = self.canonical_digest();
        Ok(self)
    }

    /// Prüft nach Deserialisierung, ob Felder und gespeicherter Digest zusammenpassen.
    pub fn has_valid_digest(&self) -> bool {
        self.digest == self.canonical_digest()
    }

    /// Vollständige Eingangsprüfung nach Deserialisierung. Ein korrekt neu berechneter
    /// Digest allein darf keine strukturell ungültigen Listen legitimieren.
    pub fn validate(&self) -> io::Result<()> {
        self.validate_structure()?;
        if !self.has_valid_digest() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Metadatenmanifest-Digest stimmt nicht mit den Feldern überein",
            ));
        }
        Ok(())
    }

    fn validate_structure(&self) -> io::Result<()> {
        if let Some(issue) = self.sparse_read_issue {
            issue.validate()?;
            if self.sparse_extents.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Sparse-Bereiche und fehlende Beobachtung widersprechen sich",
                ));
            }
        }
        if self.hardlink_group.is_some() && self.reported_hard_links.is_some_and(|count| count < 2)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Hardlink-Gruppe widerspricht der gemeldeten Linkzahl",
            ));
        }
        if self.mode & !0o7777 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Metadatenmodus enthält Dateityp- oder unbekannte Bits",
            ));
        }
        if self
            .xattrs
            .iter()
            .any(|item| item.name.is_empty() || item.name.contains(&0))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "xattr-Name darf weder leer sein noch ein Nullbyte enthalten",
            ));
        }
        if self
            .xattrs
            .windows(2)
            .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "xattr-Namen sind nicht eindeutig byteweise sortiert",
            ));
        }
        if let Some(streams) = &self.named_streams {
            for stream in streams {
                validate_named_stream_name(&stream.name)?;
            }
            if streams.windows(2).any(|pair| pair[0].name >= pair[1].name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Datenstromnamen sind nicht eindeutig byteweise sortiert",
                ));
            }
        }
        if let Some(acl) = &self.acl {
            acl.validate()?;
        }
        if let Some(observation) = &self.webdav_security {
            observation.validate()?;
            if self.windows_security.is_some() || self.acl.is_some() || self.owner.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "DAV-Sicherheitsbeobachtung darf keine fremde ACL- oder Owner-Abbildung enthalten",
                ));
            }
        }
        if let Some(observation) = &self.windows_security {
            observation.validate()?;
            if let Some(Acl::WindowsDacl(acl)) = &self.acl {
                if acl.namespace != observation.namespace || observation.issue.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Windows-DACL und Sicherheitsbeobachtung widersprechen einander",
                    ));
                }
            } else if self.acl.is_some() || observation.issue.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Vollständige Windows-Beobachtung benötigt den beobachteten DACL-Zustand",
                ));
            }
        }
        if let Some(extents) = &self.sparse_extents {
            validate_extents(extents)?;
        }
        Ok(())
    }

    fn canonical_digest(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        // Nur eine konkrete Sparse-Auslassung wählt V7. Ohne neue Felder bleiben
        // sämtliche bisherigen V2/V3/V4/V5/V6-Belege bytekompatibel.
        let link_count_format = self.reported_hard_links.is_some();
        let sparse_issue_format = self.sparse_read_issue.is_some();
        hash.update(&[if sparse_issue_format {
            7
        } else if link_count_format {
            6
        } else if self.webdav_security.is_some() {
            5
        } else if self.windows_security.is_some() || matches!(self.acl, Some(Acl::WindowsDacl(_))) {
            4
        } else if self.named_streams.is_some() {
            3
        } else {
            2
        }]);
        encode_owner(&mut hash, self.owner);
        hash.update(&(self.mode & 0o7777).to_be_bytes());
        encode_acl(&mut hash, self.acl.as_ref());
        encode_u64(&mut hash, self.xattrs.len() as u64);
        for item in &self.xattrs {
            encode_bytes(&mut hash, &item.name);
            encode_blob(&mut hash, &item.value);
        }
        encode_optional_blob(&mut hash, self.resource_fork.as_ref());
        match &self.sparse_extents {
            Some(extents) => {
                hash.update(&[1]);
                encode_u64(&mut hash, extents.len() as u64);
                for extent in extents {
                    encode_u64(&mut hash, extent.offset);
                    encode_u64(&mut hash, extent.length);
                }
            }
            None => {
                hash.update(&[0]);
            }
        }
        match self.hardlink_group {
            Some(group) => {
                hash.update(&[1]);
                encode_u64(&mut hash, group);
            }
            None => {
                hash.update(&[0]);
            }
        }
        let windows_format =
            self.windows_security.is_some() || matches!(self.acl, Some(Acl::WindowsDacl(_)));
        let extended_format = windows_format
            || self.webdav_security.is_some()
            || link_count_format
            || sparse_issue_format;
        if extended_format {
            hash.update(&[u8::from(self.named_streams.is_some())]);
        }
        if let Some(streams) = &self.named_streams {
            encode_u64(&mut hash, streams.len() as u64);
            for stream in streams {
                encode_bytes(&mut hash, &stream.name);
                encode_blob(&mut hash, &stream.value);
            }
        }
        if extended_format {
            hash.update(&[u8::from(self.windows_security.is_some())]);
        }
        if let Some(observation) = &self.windows_security {
            hash.update(&observation.namespace);
            encode_windows_sid(&mut hash, observation.owner.as_ref());
            encode_windows_sid(&mut hash, observation.group.as_ref());
            hash.update(&[match observation.issue {
                None => 0,
                Some(WindowsSecurityReadIssue::ReadControlDenied) => 1,
                Some(WindowsSecurityReadIssue::QueryUnsupported) => 2,
                Some(WindowsSecurityReadIssue::InvalidDescriptor) => 3,
            }]);
            if let Some(digest) = observation.invalid_descriptor_digest {
                hash.update(&digest);
            }
        }
        if self.webdav_security.is_some() || link_count_format || sparse_issue_format {
            hash.update(&[u8::from(self.webdav_security.is_some())]);
        }
        if let Some(observation) = &self.webdav_security {
            hash.update(&observation.namespace);
            hash.update(&observation.query_status.to_be_bytes());
            match observation.resource_status {
                Some(status) => {
                    hash.update(&[1]);
                    hash.update(&status.to_be_bytes());
                }
                None => {
                    hash.update(&[0]);
                }
            }
            encode_webdav_property(&mut hash, &observation.acl);
            encode_webdav_property(&mut hash, &observation.owner);
        }
        if sparse_issue_format {
            hash.update(&[u8::from(self.reported_hard_links.is_some())]);
        }
        if let Some(count) = self.reported_hard_links {
            hash.update(&count.to_be_bytes());
        }
        if let Some(issue) = self.sparse_read_issue {
            match issue {
                SparseReadIssue::ServerStatus(status) => {
                    hash.update(&[1]);
                    hash.update(&status.to_be_bytes());
                }
                SparseReadIssue::BufferLimit => {
                    hash.update(&[2]);
                }
                SparseReadIssue::InvalidResponse => {
                    hash.update(&[3]);
                }
            }
        }
        *hash.finalize().as_bytes()
    }
}

fn encode_webdav_property(
    hash: &mut blake3::Hasher,
    property: &crate::webdav_security::WebDavPropertyObservation,
) {
    use crate::webdav_security::WebDavPropertyObservation;
    match property {
        WebDavPropertyObservation::Missing => {
            hash.update(&[0]);
        }
        WebDavPropertyObservation::Unavailable { status } => {
            hash.update(&[1]);
            hash.update(&status.to_be_bytes());
        }
        WebDavPropertyObservation::Observed { empty, digest } => {
            hash.update(&[2, u8::from(*empty)]);
            hash.update(digest);
        }
    }
}

/// Prüft den vom Basispfad getrennten, protokollneutralen Stromnamen.
/// Provider müssen zusätzlich Kollisionen ihrer eigenen Namensauflösung abweisen.
pub fn validate_named_stream_name(name: &[u8]) -> io::Result<()> {
    if name.is_empty()
        || std::str::from_utf8(name).is_err()
        || name
            .iter()
            .any(|byte| matches!(*byte, 0 | b'/' | b'\\' | b':'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Datenstromname muss nichtleeres UTF-8 ohne Nullbyte, Slash, Backslash oder Doppelpunkt sein",
        ));
    }
    Ok(())
}

fn validate_extents(extents: &[DataExtent]) -> io::Result<()> {
    let mut previous_end = 0u64;
    for (index, extent) in extents.iter().enumerate() {
        if extent.length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Sparse-Extent darf nicht leer sein",
            ));
        }
        let end = extent.offset.checked_add(extent.length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Sparse-Extent läuft über")
        })?;
        if index > 0 && extent.offset < previous_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Sparse-Extents sind überlappend oder unsortiert",
            ));
        }
        previous_end = end;
    }
    Ok(())
}

fn encode_u64(hash: &mut blake3::Hasher, value: u64) {
    hash.update(&value.to_be_bytes());
}

fn encode_bytes(hash: &mut blake3::Hasher, bytes: &[u8]) {
    encode_u64(hash, bytes.len() as u64);
    hash.update(bytes);
}

fn encode_owner(hash: &mut blake3::Hasher, owner: Option<Owner>) {
    match owner {
        Some(owner) => {
            hash.update(&[1]);
            hash.update(&owner.uid.to_be_bytes());
            hash.update(&owner.gid.to_be_bytes());
        }
        None => {
            hash.update(&[0]);
        }
    }
}

fn encode_acl(hash: &mut blake3::Hasher, acl: Option<&Acl>) {
    match acl {
        Some(Acl::Posix(acl)) => {
            hash.update(&[1, acl_model_byte(AclModel::Posix)]);
            encode_posix_entries(hash, &acl.access);
            encode_posix_entries(hash, &acl.default);
        }
        Some(Acl::Nfs4(acl)) => {
            hash.update(&[1, acl_model_byte(AclModel::Nfs4)]);
            hash.update(&acl.flags.to_be_bytes());
            encode_u64(hash, acl.entries.len() as u64);
            for entry in &acl.entries {
                hash.update(&entry.qualifier);
                hash.update(&[match entry.kind {
                    Nfs4AceKind::Allow => 1,
                    Nfs4AceKind::Deny => 2,
                }]);
                encode_u64(hash, entry.permissions);
                hash.update(&entry.flags.to_be_bytes());
            }
        }
        Some(Acl::WindowsDacl(acl)) => {
            use crate::windows_security::{WindowsAce, WindowsDaclState};
            hash.update(&[1, acl_model_byte(AclModel::WindowsSid)]);
            hash.update(&acl.namespace);
            hash.update(&acl.control.to_be_bytes());
            match &acl.state {
                WindowsDaclState::Absent => {
                    hash.update(&[0]);
                }
                WindowsDaclState::Null => {
                    hash.update(&[1]);
                }
                WindowsDaclState::Acl { revision, aces } => {
                    hash.update(&[2, *revision]);
                    encode_u64(hash, aces.len() as u64);
                    for ace in aces {
                        match ace {
                            WindowsAce::Allow { flags, mask, sid }
                            | WindowsAce::Deny { flags, mask, sid } => {
                                hash.update(&[
                                    if matches!(ace, WindowsAce::Allow { .. }) {
                                        0
                                    } else {
                                        1
                                    },
                                    *flags,
                                ]);
                                hash.update(&mask.to_be_bytes());
                                encode_windows_sid(hash, Some(sid));
                            }
                            WindowsAce::Opaque {
                                ace_type,
                                flags,
                                data,
                            } => {
                                hash.update(&[255, *ace_type, *flags]);
                                encode_bytes(hash, data);
                            }
                        }
                    }
                }
            }
        }
        None => {
            hash.update(&[0]);
        }
    }
}

fn encode_windows_sid(
    hash: &mut blake3::Hasher,
    sid: Option<&crate::windows_security::WindowsSid>,
) {
    match sid {
        Some(sid) => {
            hash.update(&[1, sid.revision]);
            encode_u64(hash, sid.authority);
            encode_u64(hash, sid.sub_authorities.len() as u64);
            for value in &sid.sub_authorities {
                hash.update(&value.to_be_bytes());
            }
        }
        None => {
            hash.update(&[0]);
        }
    }
}

fn encode_posix_entries(hash: &mut blake3::Hasher, entries: &[PosixAclEntry]) {
    encode_u64(hash, entries.len() as u64);
    for entry in entries {
        hash.update(&[posix_tag_byte(entry.tag), entry.permissions]);
        match entry.id {
            Some(id) => {
                hash.update(&[1]);
                hash.update(&id.to_be_bytes());
            }
            None => {
                hash.update(&[0]);
            }
        }
    }
}

/// Feste Reihenfolge- und Kodierungsnummer eines POSIX-ACL-Tags.
///
/// Dieselbe Nummer bestimmt zwei Dinge, die zusammenpassen müssen: die
/// kanonische Sortierung der Einträge und ihre Kodierung im Manifest-Digest.
/// Zwei getrennte Tabellen könnten auseinanderlaufen und dieselbe ACL je nach
/// Weg unterschiedlich hashen — deshalb gibt es diese eine.
pub(crate) fn posix_tag_byte(tag: PosixAclTag) -> u8 {
    match tag {
        PosixAclTag::UserObject => 1,
        PosixAclTag::User => 2,
        PosixAclTag::GroupObject => 3,
        PosixAclTag::Group => 4,
        PosixAclTag::Mask => 5,
        PosixAclTag::Other => 6,
    }
}

fn acl_model_byte(model: AclModel) -> u8 {
    match model {
        AclModel::Posix => 1,
        AclModel::Nfs4 => 2,
        AclModel::Unsupported => 3,
        AclModel::Unknown => 4,
        AclModel::WindowsSid => 5,
    }
}

fn encode_blob(hash: &mut blake3::Hasher, blob: &BlobSummary) {
    encode_u64(hash, blob.size);
    hash.update(&blob.digest);
}

fn encode_optional_blob(hash: &mut blake3::Hasher, blob: Option<&BlobSummary>) {
    match blob {
        Some(blob) => {
            hash.update(&[1]);
            encode_blob(hash, blob);
        }
        None => {
            hash.update(&[0]);
        }
    }
}

/// Liest ein Manifest unmittelbar erneut und vergleicht es mit dem bestätigten Wert.
/// Spätere Sync-Aktionen verwenden dieselbe Schranke direkt vor jeder Metadatenmutation.
pub fn verify_metadata_manifest(
    vfs: &dyn crate::vfs::Vfs,
    path: &Path,
    expected: &MetadataManifest,
) -> io::Result<()> {
    expected.validate()?;
    let current = vfs.metadata_manifest(path)?;
    current.validate()?;
    if current.digest != expected.digest {
        // Derselbe Anlass — die Quellmetadaten änderten sich zwischen Vorschau und
        // Ausführung — meldet der Nachbarzweig in `sync::metadata` über
        // `plan_changed` den Code `plan_drift`. Ohne den Code hier bekäme dieselbe
        // Ursache je nach Zweig zwei verschiedene Maschinencodes, und ein Aufrufer,
        // der laut `crate::error` bei `plan_drift` eine neue Vorschau anbieten soll,
        // sähe nur ein unspezifisches `io`.
        return Err(crate::error::coded_error(
            io::ErrorKind::Interrupted,
            crate::error::ErrorCode::PlanDrift,
            format!(
                "Metadaten-Planbindung verletzt: {} änderte sich seit der Vorschau",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use crate::vfs::{LocalVfs, Vfs};

    use super::*;

    fn xattr(name: &[u8], value: &[u8]) -> NamedBlobSummary {
        NamedBlobSummary {
            name: name.to_vec(),
            value: BlobSummary::from_bytes(value),
        }
    }

    fn posix_acl() -> Acl {
        Acl::Posix(PosixAcl {
            access: vec![
                PosixAclEntry {
                    tag: PosixAclTag::UserObject,
                    id: None,
                    permissions: 7,
                },
                PosixAclEntry {
                    tag: PosixAclTag::GroupObject,
                    id: None,
                    permissions: 5,
                },
                PosixAclEntry {
                    tag: PosixAclTag::Other,
                    id: None,
                    permissions: 0,
                },
            ],
            default: vec![],
        })
    }

    /// macOS stellt `/var` als Symlink bereit; lokale VFS-Tests beginnen deshalb wie
    /// die Sync-Suite unter der kanonischen Temp-Wurzel.
    fn canonical_tempdir() -> tempfile::TempDir {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn named_streams_vfs_defaults_sind_explizit_unsupported() {
        let vfs = LocalVfs::new();
        let path = Path::new("nicht-geoeffnete-basisdatei");
        assert_eq!(
            vfs.open_named_stream_read(path, b"strom")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported,
        );
        assert_eq!(
            vfs.open_named_stream_write_new(path, b"strom")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported,
        );
    }

    #[test]
    fn named_streams_legacy_json_und_v2_digest_bleiben_bytegleich() {
        let bytes = include_str!("../../fuzz/seeds/metadata_manifest/valid.json");
        let legacy: MetadataManifest = serde_json::from_str(bytes).unwrap();
        legacy.validate().unwrap();
        assert_eq!(legacy.named_streams, None);
        assert_eq!(serde_json::to_string(&legacy).unwrap(), bytes);
        let rebuilt = MetadataManifest::new(None, 0o640, None, vec![], None, None, None).unwrap();
        assert_eq!(legacy, rebuilt);
        assert_eq!(legacy.clone().with_named_streams(None).unwrap(), legacy);
        let empty = legacy.clone().with_named_streams(Some(vec![])).unwrap();
        empty.validate().unwrap();
        assert_ne!(empty.digest, legacy.digest);
        assert_eq!(empty.named_streams, Some(vec![]));
        assert!(serde_json::to_string(&empty)
            .unwrap()
            .contains("\"named_streams\":[]"));
    }

    #[test]
    fn named_streams_kanonische_menge_und_roundtrip() {
        let base = MetadataManifest::new(None, 0o644, None, vec![], None, None, None).unwrap();
        let streams = vec![xattr("ä".as_bytes(), b""), xattr(b"alpha", b"inhalt")];
        let observed = base
            .clone()
            .with_named_streams(Some(streams.clone()))
            .unwrap();
        let mut reversed = streams;
        reversed.reverse();
        assert_eq!(observed, base.with_named_streams(Some(reversed)).unwrap());
        assert_eq!(observed.named_streams.as_ref().unwrap()[0].name, b"alpha");
        let encoded = serde_json::to_string(&observed).unwrap();
        let decoded: MetadataManifest = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded, observed);
        let mut changed = decoded;
        changed.named_streams.as_mut().unwrap()[0].value.size += 1;
        assert!(!changed.has_valid_digest());
        assert!(changed.validate().is_err());
        changed.digest = changed.canonical_digest();
        changed.validate().unwrap();
        changed.named_streams.as_mut().unwrap().reverse();
        changed.digest = changed.canonical_digest();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn named_streams_namen_und_duplikate_werden_abgewiesen() {
        let base = MetadataManifest::new(None, 0o644, None, vec![], None, None, None).unwrap();
        for name in [b"".as_slice(), b"a\0b", b"a/b", b"a\\b", b"a:b", &[0xff]] {
            assert!(base
                .clone()
                .with_named_streams(Some(vec![xattr(name, b"")]))
                .is_err());
        }
        let duplicate = xattr(b"gleich", b"");
        assert!(base
            .clone()
            .with_named_streams(Some(vec![duplicate.clone(), duplicate.clone()]))
            .is_err());
        let mut forged = base;
        forged.named_streams = Some(vec![duplicate.clone(), duplicate]);
        forged.digest = forged.canonical_digest();
        assert!(forged.validate().is_err());
    }

    #[test]
    fn named_streams_fehlende_capability_bleibt_unknown_ohne_json_aenderung() {
        let capabilities = MetadataCapabilities::unknown();
        let encoded = serde_json::to_string(&capabilities).unwrap();
        assert!(!encoded.contains("named_streams"));
        let decoded: MetadataCapabilities = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.named_streams, Capability::Unknown);
        assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
    }

    #[test]
    fn xattr_reihenfolge_aendert_digest_nicht() {
        let first = MetadataManifest::new(
            Some(Owner { uid: 1, gid: 2 }),
            0o1755,
            None,
            vec![xattr(b"user.z", b"z"), xattr(b"user.a", b"a")],
            None,
            None,
            None,
        )
        .unwrap();
        let second = MetadataManifest::new(
            Some(Owner { uid: 1, gid: 2 }),
            0o1755,
            None,
            vec![xattr(b"user.a", b"a"), xattr(b"user.z", b"z")],
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(first, second);
        first.validate().unwrap();
        let mut tampered = first.clone();
        tampered.mode = 0o700;
        assert!(tampered.validate().is_err());
    }

    #[test]
    fn jede_metadatenklasse_bindet_den_digest() {
        let base = MetadataManifest::new(None, 0o644, None, vec![], None, None, None).unwrap();
        let owner = MetadataManifest::new(
            Some(Owner { uid: 1, gid: 2 }),
            0o644,
            None,
            vec![],
            None,
            None,
            None,
        )
        .unwrap();
        let mode = MetadataManifest::new(None, 0o755, None, vec![], None, None, None).unwrap();
        let acl = MetadataManifest::new(None, 0o644, Some(posix_acl()), vec![], None, None, None)
            .unwrap();
        let attrs = MetadataManifest::new(
            None,
            0o644,
            None,
            vec![xattr(b"user.test", b"wert")],
            None,
            None,
            None,
        )
        .unwrap();
        let fork = MetadataManifest::new(
            None,
            0o644,
            None,
            vec![],
            Some(BlobSummary::from_bytes(b"fork")),
            None,
            None,
        )
        .unwrap();
        let sparse = MetadataManifest::new(
            None,
            0o644,
            None,
            vec![],
            None,
            Some(vec![DataExtent {
                offset: 1024,
                length: 512,
            }]),
            None,
        )
        .unwrap();
        let hardlink =
            MetadataManifest::new(None, 0o644, None, vec![], None, None, Some(7)).unwrap();

        for changed in [owner, mode, acl, attrs, fork, sparse, hardlink] {
            assert_ne!(base.digest, changed.digest);
        }
    }

    #[test]
    fn binaerer_xattr_name_ueberlebt_json_roundtrip() {
        let manifest = MetadataManifest::new(
            None,
            0o600,
            None,
            vec![xattr(&[b'u', b's', b'e', b'r', b'.', 0xff], b"wert")],
            None,
            None,
            None,
        )
        .unwrap();
        let json = serde_json::to_string(&manifest).unwrap();
        let decoded: MetadataManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, manifest);
        decoded.validate().unwrap();
    }

    #[test]
    fn ungueltige_listen_werden_abgelehnt() {
        assert!(MetadataManifest::new(
            None,
            0o644,
            None,
            vec![xattr(b"doppelt", b"1"), xattr(b"doppelt", b"2")],
            None,
            None,
            None,
        )
        .is_err());
        assert!(MetadataManifest::new(None, 0o100644, None, vec![], None, None, None,).is_err());
        assert!(MetadataManifest::new(
            None,
            0o644,
            Some(Acl::Posix(PosixAcl {
                access: vec![PosixAclEntry {
                    tag: PosixAclTag::User,
                    id: None,
                    permissions: 7,
                }],
                default: vec![],
            })),
            vec![],
            None,
            None,
            None,
        )
        .is_err());
        assert!(MetadataManifest::new(
            None,
            0o644,
            None,
            vec![],
            None,
            Some(vec![
                DataExtent {
                    offset: 10,
                    length: 5,
                },
                DataExtent {
                    offset: 14,
                    length: 2,
                },
            ]),
            None,
        )
        .is_err());
    }

    #[test]
    fn lokales_portable_manifest_liest_owner_modus_und_identitaet() {
        let dir = canonical_tempdir();
        let path = dir.path().join("datei");
        std::fs::write(&path, b"inhalt").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let expected = std::fs::symlink_metadata(&path).unwrap();
        let vfs = LocalVfs::new();

        let manifest = vfs.metadata_manifest(&path).unwrap();
        let entry = vfs.stat(&path).unwrap();

        assert_eq!(
            manifest.owner,
            Some(Owner {
                uid: expected.uid(),
                gid: expected.gid(),
            })
        );
        assert_eq!(manifest.mode, 0o751);
        manifest.validate().unwrap();
        assert_eq!(entry.device, Some(expected.dev()));
        assert_eq!(entry.inode, Some(expected.ino()));
        assert_eq!(entry.hard_links, Some(expected.nlink()));
    }

    #[test]
    fn lokale_faehigkeiten_melden_nur_verifizierte_schreibwege() {
        let dir = canonical_tempdir();
        let vfs = LocalVfs::new();
        let capabilities = vfs.metadata_capabilities(dir.path()).unwrap();

        assert_eq!(capabilities.owner, Capability::Exact);
        assert_eq!(capabilities.special_mode_bits, Capability::Exact);
        assert_eq!(capabilities.sparse_extents, Capability::Exact);
        assert_eq!(
            capabilities.xattrs,
            if cfg!(target_os = "linux") {
                Capability::Exact
            } else {
                Capability::ReadOnly
            }
        );
        assert_eq!(
            capabilities.resource_fork,
            if cfg!(target_os = "macos") {
                Capability::Exact
            } else {
                Capability::Unsupported
            }
        );
        assert_eq!(capabilities.hardlinks, Capability::Exact);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn lokale_xattrs_roundtrip_und_exaktes_entfernen() {
        let dir = canonical_tempdir();
        let path = dir.path().join("datei");
        std::fs::write(&path, b"inhalt").unwrap();
        let vfs = LocalVfs::new();
        let mut large = vec![0u8; 2048];
        for (index, byte) in large.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        let desired = vec![
            XattrValue {
                name: b"user.doppeldecker.leer".to_vec(),
                value: Vec::new(),
            },
            XattrValue {
                name: b"user.doppeldecker.klein".to_vec(),
                value: b"wert".to_vec(),
            },
            XattrValue {
                name: b"user.doppeldecker.gross".to_vec(),
                value: large,
            },
            XattrValue {
                name: b"user.doppeldecker.\xc3\xa4".to_vec(),
                value: vec![0, 1, 2, 255],
            },
        ];

        vfs.write_xattrs_exact(&path, &desired).unwrap();
        let mut expected = desired;
        expected.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(vfs.read_xattrs(&path).unwrap(), expected);
        let manifest = vfs.metadata_manifest(&path).unwrap();
        assert_eq!(manifest.xattrs.len(), 4);
        manifest.validate().unwrap();

        let remaining = vec![XattrValue {
            name: b"user.doppeldecker.klein".to_vec(),
            value: b"neu".to_vec(),
        }];
        vfs.write_xattrs_exact(&path, &remaining).unwrap();
        assert_eq!(vfs.read_xattrs(&path).unwrap(), remaining);
    }

    #[test]
    fn xattr_aenderung_verletzt_manifest_planbindung() {
        let dir = canonical_tempdir();
        let path = dir.path().join("datei");
        std::fs::write(&path, b"inhalt").unwrap();
        let vfs = LocalVfs::new();
        crate::local_fs::set_xattr_for_test(&path, b"user.doppeldecker.plan", b"vorher").unwrap();
        let expected = vfs.metadata_manifest(&path).unwrap();
        verify_metadata_manifest(&vfs, &path, &expected).unwrap();

        crate::local_fs::set_xattr_for_test(&path, b"user.doppeldecker.plan", b"nachher").unwrap();

        let drift = verify_metadata_manifest(&vfs, &path, &expected).unwrap_err();
        assert_eq!(
            crate::error_code(&drift),
            crate::ErrorCode::PlanDrift,
            "Derselbe Anlass muss denselben Maschinencode tragen wie der Nachbarzweig"
        );
    }

    #[test]
    fn rooted_vfs_blockt_xattr_zugriff_ausserhalb() {
        let parent = canonical_tempdir();
        let root = parent.path().join("wurzel");
        let outside = parent.path().join("ausserhalb");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&outside, b"inhalt").unwrap();
        let vfs = LocalVfs::rooted(&root).unwrap();
        let xattrs = vec![XattrValue {
            name: b"user.doppeldecker.nein".to_vec(),
            value: b"nein".to_vec(),
        }];

        assert!(vfs.write_xattrs_exact(&outside, &xattrs).is_err());
        assert!(vfs.read_xattrs(&outside).is_err());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn ressourcen_fork_streamt_descriptor_relativ_und_bindet_manifest() {
        let dir = canonical_tempdir();
        let path = dir.path().join("datei");
        std::fs::write(&path, b"daten-fork").unwrap();
        let vfs = LocalVfs::new();
        let mut expected = vec![0u8; 400_000];
        for (index, byte) in expected.iter_mut().enumerate() {
            *byte = (index % 239) as u8;
        }

        let mut writer = vfs.open_fork_write_new(&path, ForkKind::Resource).unwrap();
        for chunk in expected.chunks(17_003) {
            writer.write_all(chunk).unwrap();
        }
        writer.flush().unwrap();
        drop(writer);

        let mut actual = Vec::new();
        vfs.open_fork_read(&path, ForkKind::Resource)
            .unwrap()
            .read_to_end(&mut actual)
            .unwrap();
        assert_eq!(actual, expected);
        assert!(vfs
            .read_xattrs(&path)
            .unwrap()
            .iter()
            .all(|item| item.name != b"com.apple.ResourceFork"));

        let manifest = vfs.metadata_manifest(&path).unwrap();
        assert_eq!(
            manifest.resource_fork,
            Some(BlobSummary::from_bytes(&expected))
        );
        verify_metadata_manifest(&vfs, &path, &manifest).unwrap();
        assert!(vfs.open_fork_write_new(&path, ForkKind::Resource).is_err());

        crate::local_fs::set_xattr_for_test(&path, b"com.apple.ResourceFork", b"geaendert")
            .unwrap();
        assert!(verify_metadata_manifest(&vfs, &path, &manifest).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_meldet_ressourcen_fork_explizit_unsupported() {
        let dir = canonical_tempdir();
        let path = dir.path().join("datei");
        std::fs::write(&path, b"daten").unwrap();
        let vfs = LocalVfs::new();

        let read_error = match vfs.open_fork_read(&path, ForkKind::Resource) {
            Ok(_) => panic!("Linux darf keinen Ressourcen-Fork-Reader liefern"),
            Err(error) => error,
        };
        assert_eq!(read_error.kind(), io::ErrorKind::Unsupported);
        let write_error = match vfs.open_fork_write_new(&path, ForkKind::Resource) {
            Ok(_) => panic!("Linux darf keinen Ressourcen-Fork-Writer liefern"),
            Err(error) => error,
        };
        assert_eq!(write_error.kind(), io::ErrorKind::Unsupported);
        assert!(vfs
            .metadata_manifest(&path)
            .unwrap()
            .resource_fork
            .is_none());
    }

    #[test]
    fn owner_normale_rechte_und_sonderbits_laufen_in_getrennten_phasen() {
        let dir = canonical_tempdir();
        let file = dir.path().join("programm");
        let folder = dir.path().join("ordner");
        std::fs::write(&file, b"nicht ausfuehren").unwrap();
        std::fs::create_dir(&folder).unwrap();
        let vfs = LocalVfs::new();
        let owner = vfs.metadata_manifest(&file).unwrap().owner.unwrap();

        vfs.set_metadata_owner(&file, owner).unwrap();
        vfs.set_metadata_normal_mode(&file, 0o751).unwrap();
        vfs.set_metadata_special_bits(&file, 0o6000).unwrap();
        let file_manifest = vfs.metadata_manifest(&file).unwrap();
        assert_eq!(file_manifest.owner, Some(owner));
        assert_eq!(file_manifest.mode, 0o6751);

        vfs.set_metadata_normal_mode(&folder, 0o700).unwrap();
        vfs.set_metadata_special_bits(&folder, 0o1000).unwrap();
        assert_eq!(vfs.metadata_manifest(&folder).unwrap().mode, 0o1700);
    }

    #[test]
    fn ungueltige_owner_und_moduswerte_aendern_nichts() {
        let dir = canonical_tempdir();
        let path = dir.path().join("datei");
        std::fs::write(&path, b"inhalt").unwrap();
        let vfs = LocalVfs::new();
        vfs.set_metadata_normal_mode(&path, 0o640).unwrap();
        let before = vfs.metadata_manifest(&path).unwrap();

        assert!(vfs
            .set_metadata_owner(
                &path,
                Owner {
                    uid: u32::MAX,
                    gid: u32::MAX,
                },
            )
            .is_err());
        assert!(vfs.set_metadata_normal_mode(&path, 0o4640).is_err());
        assert!(vfs.set_metadata_special_bits(&path, 0o755).is_err());

        assert_eq!(vfs.metadata_manifest(&path).unwrap().digest, before.digest);
    }

    /// Die Tagnummern bestimmen sowohl die kanonische Sortierung der ACL-Einträge
    /// als auch ihre Kodierung im Manifest-Digest. Wer sie ändert, ändert beides
    /// und macht bestehende Digests ungültig — deshalb sind sie hier festgenagelt.
    #[test]
    fn posix_tagnummern_bleiben_geordnet_und_stabil() {
        let order = [
            (PosixAclTag::UserObject, 1),
            (PosixAclTag::User, 2),
            (PosixAclTag::GroupObject, 3),
            (PosixAclTag::Group, 4),
            (PosixAclTag::Mask, 5),
            (PosixAclTag::Other, 6),
        ];

        for (tag, expected) in order {
            assert_eq!(posix_tag_byte(tag), expected);
        }
        assert!(order
            .windows(2)
            .all(|pair| posix_tag_byte(pair[0].0) < posix_tag_byte(pair[1].0)));
    }
}
