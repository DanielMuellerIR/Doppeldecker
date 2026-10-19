//! Geheimnisfreie persistente Remote-Verbindungsprofile.
//!
//! Profile enthalten Host, Nutzer, Hostkey-Pin und ausschließlich Referenzen auf den
//! System-Schlüsselbund oder SSH-Agenten. Passwort-/Passphrasenwerte werden von diesem
//! Modul weder angenommen noch serialisiert.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::{
    coded_error, ErrorCode, FtpsConnectionConfig, SftpConnectionConfig, SmbConnectionConfig,
    WebDavConnectionConfig,
};

const PROFILE_VERSION: u32 = 1;
const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SftpProfile {
    pub id: String,
    pub name: String,
    pub connection: SftpConnectionConfig,
}

impl SftpProfile {
    pub fn validate(&self) -> io::Result<()> {
        validate_id(&self.id)?;
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err(invalid("SFTP-Profilname ist ungültig"));
        }
        self.connection.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FtpsProfile {
    pub id: String,
    pub name: String,
    pub connection: FtpsConnectionConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebDavProfile {
    pub id: String,
    pub name: String,
    pub connection: WebDavConnectionConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmbProfile {
    pub id: String,
    pub name: String,
    pub connection: SmbConnectionConfig,
}

impl SmbProfile {
    pub fn validate(&self) -> io::Result<()> {
        validate_profile_header(&self.id, &self.name, "SMB")?;
        self.connection.validate()
    }
}

impl WebDavProfile {
    pub fn validate(&self) -> io::Result<()> {
        validate_profile_header(&self.id, &self.name, "WebDAV")?;
        self.connection.validate()
    }
}

impl FtpsProfile {
    pub fn validate(&self) -> io::Result<()> {
        validate_profile_header(&self.id, &self.name, "FTPS")?;
        self.connection.validate()
    }
}

trait StoredProfile: Clone + serde::Serialize + DeserializeOwned {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn validate_stored(&self) -> io::Result<()>;
}

/// Bindet Profiltypen an die gemeinsame Ablage.
///
/// Alle Profile tragen dieselben Kopfdaten `id` und `name` und bringen ihre
/// eigene `validate`-Methode mit; die Umsetzung war deshalb für jeden Typ
/// wörtlich gleich. Ein neuer Provider wird hier eingetragen, statt den Block zu
/// kopieren — so kann keine Umsetzung still von den anderen abweichen.
macro_rules! impl_stored_profile {
    ($($profile:ty),+ $(,)?) => {
        $(
            impl StoredProfile for $profile {
                fn id(&self) -> &str {
                    &self.id
                }

                fn name(&self) -> &str {
                    &self.name
                }

                fn validate_stored(&self) -> io::Result<()> {
                    self.validate()
                }
            }
        )+
    };
}

impl_stored_profile!(SftpProfile, FtpsProfile, WebDavProfile, SmbProfile);

#[derive(serde::Serialize, serde::Deserialize)]
struct ProfileDocument<T> {
    version: u32,
    /// Prozessübergreifend monotone Quelle für Profilrevisionen. Die Zuordnung
    /// behält auch entfernte IDs, damit Entfernen und späteres Neuanlegen niemals
    /// wieder dieselbe Revision verwenden.
    #[serde(default)]
    revision_counter: u64,
    #[serde(default)]
    revisions: BTreeMap<String, u64>,
    profiles: Vec<T>,
}

impl<T> Default for ProfileDocument<T> {
    fn default() -> Self {
        Self {
            version: 0,
            revision_counter: 0,
            revisions: BTreeMap::new(),
            profiles: Vec::new(),
        }
    }
}

/// Geheimnisfreie Sicht eines Profils samt seiner dauerhaft monotonen Revision.
/// `flatten` hält das bisherige JSON-Format für Frontend und CLI kompatibel und
/// ergänzt lediglich das Feld `revision`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct VersionedProfile<T> {
    #[serde(flatten)]
    pub profile: T,
    pub revision: u64,
}

/// Ergebnis genau einer Profilmutation. `revision` bezeichnet auch beim Entfernen
/// den neuen Tombstone-Stand; `changed=false` kennzeichnet ein fehlendes Profil.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileMutation<T> {
    pub profiles: Vec<VersionedProfile<T>>,
    pub revision: u64,
    pub changed: bool,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum RemoteProfileProtocol {
    Sftp,
    Ftps,
    Webdav,
    Smb,
}

impl RemoteProfileProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Ftps => "ftps",
            Self::Webdav => "webdav",
            Self::Smb => "smb",
        }
    }
}

/// Geheimnisfreie Bindung einer Laufzeit- oder Queuevorbereitung an exakt den
/// Profilstand, aus dem Verbindung und öffentliche Konfiguration entstanden.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProfileBinding {
    pub protocol: RemoteProfileProtocol,
    pub profile_id: String,
    pub revision: u64,
}

impl RemoteProfileBinding {
    pub fn new(
        protocol: RemoteProfileProtocol,
        profile_id: impl Into<String>,
        revision: u64,
    ) -> io::Result<Self> {
        let profile_id = profile_id.into();
        validate_id(&profile_id)?;
        Ok(Self {
            protocol,
            profile_id,
            revision,
        })
    }

    pub fn validate(&self) -> io::Result<()> {
        validate_id(&self.profile_id)
    }
}

pub fn load_sftp_profiles(path: &Path) -> io::Result<Vec<SftpProfile>> {
    load_profiles(path, "SFTP")
}

pub fn load_ftps_profiles(path: &Path) -> io::Result<Vec<FtpsProfile>> {
    load_profiles(path, "FTPS")
}

pub fn load_webdav_profiles(path: &Path) -> io::Result<Vec<WebDavProfile>> {
    load_profiles(path, "WebDAV")
}

pub fn load_smb_profiles(path: &Path) -> io::Result<Vec<SmbProfile>> {
    load_profiles(path, "SMB")
}

fn load_profiles<T: StoredProfile>(path: &Path, protocol: &str) -> io::Result<Vec<T>> {
    Ok(load_profile_document(path, protocol)?.profiles)
}

fn load_profile_document<T: StoredProfile>(
    path: &Path,
    protocol: &str,
) -> io::Result<ProfileDocument<T>> {
    let file = match open_profile_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProfileDocument {
                version: PROFILE_VERSION,
                ..ProfileDocument::default()
            })
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata.len() > MAX_PROFILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{protocol}-Profildatei ist zu groß"),
        ));
    }
    // Auch ein außerhalb Doppeldeckers schreibender Prozess darf die Datei nach
    // der Metadatenprüfung nicht unbegrenzt wachsen lassen. `take` bindet dieselbe
    // harte Grenze an den bereits ohne Symlink-Folgen geöffneten Deskriptor.
    let mut payload = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PROFILE_BYTES + 1).read_to_end(&mut payload)?;
    if payload.len() as u64 > MAX_PROFILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{protocol}-Profildatei ist zu groß"),
        ));
    }
    let document: ProfileDocument<T> = serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if document.version != PROFILE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{protocol}-Profilversion {} wird nicht unterstützt",
                document.version
            ),
        ));
    }
    for profile in &document.profiles {
        profile.validate_stored()?;
    }
    let mut ids = document
        .profiles
        .iter()
        .map(StoredProfile::id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{protocol}-Profildatei enthält doppelte IDs"),
        ));
    }
    for id in document.revisions.keys() {
        validate_id(id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{protocol}-Profildatei enthält eine ungültige Revisions-ID"),
            )
        })?;
    }
    if document
        .revisions
        .values()
        .any(|revision| *revision > document.revision_counter)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{protocol}-Profildatei enthält eine widersprüchliche Revision"),
        ));
    }
    Ok(document)
}

fn open_profile_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(
        libc::O_NOFOLLOW
            // Ein FIFO oder Gerät darf den Profil-Loader nicht schon beim
            // Öffnen blockieren; die Deskriptormetadaten lehnen es danach ab.
            | libc::O_NONBLOCK,
    );
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Remote-Profildatei ist keine alleinstehende reguläre Datei",
        ));
    }
    Ok(file)
}

fn versioned_profiles<T: StoredProfile>(document: &ProfileDocument<T>) -> Vec<VersionedProfile<T>> {
    document
        .profiles
        .iter()
        .cloned()
        .map(|profile| VersionedProfile {
            revision: document.revisions.get(profile.id()).copied().unwrap_or(0),
            profile,
        })
        .collect()
}

macro_rules! versioned_profile_api {
    (
        $load:ident,
        $save:ident,
        $save_if_current:ident,
        $remove:ident,
        $remove_if_current:ident,
        $latest:ident,
        $profile:ty,
        $protocol:literal
    ) => {
        pub fn $load(path: &Path) -> io::Result<Vec<VersionedProfile<$profile>>> {
            let document = load_profile_document(path, $protocol)?;
            Ok(versioned_profiles(&document))
        }

        pub fn $save(path: &Path, profile: $profile) -> io::Result<ProfileMutation<$profile>> {
            save_profile_versioned(path, profile, $protocol)
        }

        pub fn $save_if_current(
            path: &Path,
            profile: $profile,
            expected_revision: Option<u64>,
        ) -> io::Result<ProfileMutation<$profile>> {
            save_profile_versioned_if_current(path, profile, expected_revision, $protocol)
        }

        pub fn $remove(path: &Path, id: &str) -> io::Result<ProfileMutation<$profile>> {
            remove_profile_versioned(path, id, $protocol)
        }

        pub fn $remove_if_current(
            path: &Path,
            id: &str,
            expected_revision: u64,
        ) -> io::Result<ProfileMutation<$profile>> {
            remove_profile_versioned_if_current(path, id, expected_revision, $protocol)
        }

        pub fn $latest(path: &Path, id: &str) -> io::Result<Option<u64>> {
            validate_id(id)?;
            let document: ProfileDocument<$profile> = load_profile_document(path, $protocol)?;
            Ok(document.revisions.get(id).copied().or_else(|| {
                // Profildateien vor W2b besitzen noch keine Revisionskarte. Ein
                // darin vorhandenes Profil hat dieselbe sichtbare Revision 0 wie
                // `load_*_profiles_versioned`; nur ein wirklich unbekannter Name
                // liefert `None`.
                document
                    .profiles
                    .iter()
                    .any(|profile| profile.id() == id)
                    .then_some(0)
            }))
        }
    };
}

versioned_profile_api!(
    load_sftp_profiles_versioned,
    save_sftp_profile_versioned,
    save_sftp_profile_versioned_if_current,
    remove_sftp_profile_versioned,
    remove_sftp_profile_versioned_if_current,
    latest_sftp_profile_revision,
    SftpProfile,
    "SFTP"
);
versioned_profile_api!(
    load_ftps_profiles_versioned,
    save_ftps_profile_versioned,
    save_ftps_profile_versioned_if_current,
    remove_ftps_profile_versioned,
    remove_ftps_profile_versioned_if_current,
    latest_ftps_profile_revision,
    FtpsProfile,
    "FTPS"
);
versioned_profile_api!(
    load_webdav_profiles_versioned,
    save_webdav_profile_versioned,
    save_webdav_profile_versioned_if_current,
    remove_webdav_profile_versioned,
    remove_webdav_profile_versioned_if_current,
    latest_webdav_profile_revision,
    WebDavProfile,
    "WebDAV"
);
versioned_profile_api!(
    load_smb_profiles_versioned,
    save_smb_profile_versioned,
    save_smb_profile_versioned_if_current,
    remove_smb_profile_versioned,
    remove_smb_profile_versioned_if_current,
    latest_smb_profile_revision,
    SmbProfile,
    "SMB"
);

pub fn save_sftp_profile(path: &Path, profile: SftpProfile) -> io::Result<Vec<SftpProfile>> {
    save_profile(path, profile, "SFTP")
}

pub fn save_ftps_profile(path: &Path, profile: FtpsProfile) -> io::Result<Vec<FtpsProfile>> {
    save_profile(path, profile, "FTPS")
}

pub fn save_webdav_profile(path: &Path, profile: WebDavProfile) -> io::Result<Vec<WebDavProfile>> {
    save_profile(path, profile, "WebDAV")
}

pub fn save_smb_profile(path: &Path, profile: SmbProfile) -> io::Result<Vec<SmbProfile>> {
    save_profile(path, profile, "SMB")
}

fn save_profile<T: StoredProfile>(path: &Path, profile: T, protocol: &str) -> io::Result<Vec<T>> {
    Ok(save_profile_versioned(path, profile, protocol)?
        .profiles
        .into_iter()
        .map(|profile| profile.profile)
        .collect())
}

fn save_profile_versioned<T: StoredProfile>(
    path: &Path,
    profile: T,
    protocol: &str,
) -> io::Result<ProfileMutation<T>> {
    save_profile_versioned_checked(path, profile, RevisionCheck::Unconditional, protocol)
}

fn save_profile_versioned_if_current<T: StoredProfile>(
    path: &Path,
    profile: T,
    expected_revision: Option<u64>,
    protocol: &str,
) -> io::Result<ProfileMutation<T>> {
    save_profile_versioned_checked(
        path,
        profile,
        RevisionCheck::Current(expected_revision),
        protocol,
    )
}

#[derive(Clone, Copy)]
enum RevisionCheck {
    Unconditional,
    /// `None` bindet eine Neuanlage an einen weiterhin freien Profilnamen.
    Current(Option<u64>),
}

fn save_profile_versioned_checked<T: StoredProfile>(
    path: &Path,
    profile: T,
    revision_check: RevisionCheck,
    protocol: &str,
) -> io::Result<ProfileMutation<T>> {
    profile.validate_stored()?;
    // GUI und CLI dürfen dieselbe Profilablage parallel verwenden. Der Lock umfasst
    // deshalb den vollständigen Read-modify-write-Zyklus; nur ein atomarer Rename am
    // Ende würde sonst zwei gleichzeitig ergänzte Profile gegenseitig verlieren.
    let _lock = acquire_process_lock(path)?;
    let mut document: ProfileDocument<T> = load_profile_document(path, protocol)?;
    let profile_id = profile.id().to_owned();
    ensure_revision_current(&document, &profile_id, revision_check, protocol)?;
    if let Some(existing) = document
        .profiles
        .iter_mut()
        .find(|existing| existing.id() == profile_id)
    {
        *existing = profile;
    } else {
        document.profiles.push(profile);
    }
    document.profiles.sort_by(|left, right| {
        left.name()
            .cmp(right.name())
            .then(left.id().cmp(right.id()))
    });
    let revision = next_revision(&document, protocol)?;
    document.revision_counter = revision;
    document.revisions.insert(profile_id, revision);
    write_profiles(path, &document, protocol)?;
    Ok(ProfileMutation {
        profiles: versioned_profiles(&document),
        revision,
        changed: true,
    })
}

pub fn remove_sftp_profile(path: &Path, id: &str) -> io::Result<Vec<SftpProfile>> {
    remove_profile(path, id, "SFTP")
}

pub fn remove_ftps_profile(path: &Path, id: &str) -> io::Result<Vec<FtpsProfile>> {
    remove_profile(path, id, "FTPS")
}

pub fn remove_webdav_profile(path: &Path, id: &str) -> io::Result<Vec<WebDavProfile>> {
    remove_profile(path, id, "WebDAV")
}

pub fn remove_smb_profile(path: &Path, id: &str) -> io::Result<Vec<SmbProfile>> {
    remove_profile(path, id, "SMB")
}

fn remove_profile<T: StoredProfile>(path: &Path, id: &str, protocol: &str) -> io::Result<Vec<T>> {
    Ok(remove_profile_versioned(path, id, protocol)?
        .profiles
        .into_iter()
        .map(|profile| profile.profile)
        .collect())
}

fn remove_profile_versioned<T: StoredProfile>(
    path: &Path,
    id: &str,
    protocol: &str,
) -> io::Result<ProfileMutation<T>> {
    remove_profile_versioned_checked(path, id, RevisionCheck::Unconditional, protocol)
}

fn remove_profile_versioned_if_current<T: StoredProfile>(
    path: &Path,
    id: &str,
    expected_revision: u64,
    protocol: &str,
) -> io::Result<ProfileMutation<T>> {
    remove_profile_versioned_checked(
        path,
        id,
        RevisionCheck::Current(Some(expected_revision)),
        protocol,
    )
}

fn remove_profile_versioned_checked<T: StoredProfile>(
    path: &Path,
    id: &str,
    revision_check: RevisionCheck,
    protocol: &str,
) -> io::Result<ProfileMutation<T>> {
    validate_id(id)?;
    let _lock = acquire_process_lock(path)?;
    let mut document: ProfileDocument<T> = load_profile_document(path, protocol)?;
    ensure_revision_current(&document, id, revision_check, protocol)?;
    let before = document.profiles.len();
    document.profiles.retain(|profile| profile.id() != id);
    if document.profiles.len() == before {
        return Ok(ProfileMutation {
            profiles: versioned_profiles(&document),
            revision: document.revisions.get(id).copied().unwrap_or(0),
            changed: false,
        });
    }
    let revision = next_revision(&document, protocol)?;
    document.revision_counter = revision;
    document.revisions.insert(id.to_owned(), revision);
    write_profiles(path, &document, protocol)?;
    Ok(ProfileMutation {
        profiles: versioned_profiles(&document),
        revision,
        changed: true,
    })
}

fn ensure_revision_current<T: StoredProfile>(
    document: &ProfileDocument<T>,
    id: &str,
    check: RevisionCheck,
    protocol: &str,
) -> io::Result<()> {
    let RevisionCheck::Current(expected) = check else {
        return Ok(());
    };
    let current = document
        .profiles
        .iter()
        .any(|profile| profile.id() == id)
        .then(|| document.revisions.get(id).copied().unwrap_or(0));
    if current == expected {
        return Ok(());
    }
    Err(coded_error(
        io::ErrorKind::Other,
        ErrorCode::PlanDrift,
        format!("{protocol}-Profil wurde seit dem Öffnen des Editors geändert oder entfernt"),
    ))
}

fn next_revision<T>(document: &ProfileDocument<T>, protocol: &str) -> io::Result<u64> {
    document.revision_counter.checked_add(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{protocol}-Profilrevision ist ausgeschöpft"),
        )
    })
}

fn write_profiles<T: StoredProfile>(
    path: &Path,
    document: &ProfileDocument<T>,
    protocol: &str,
) -> io::Result<()> {
    path.parent()
        .ok_or_else(|| invalid(format!("{protocol}-Profildatei ohne Elternordner")))?;
    let payload = serde_json::to_vec_pretty(document).map_err(io::Error::other)?;
    if payload.len() as u64 > MAX_PROFILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{protocol}-Profildokument überschreitet das Leselimit"),
        ));
    }
    crate::atomic_write::atomic_write_local(
        path,
        &payload,
        ".remote-profiles-",
        crate::atomic_write::AtomicWriteMode::Replace,
    )
}

fn acquire_process_lock(path: &Path) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Remote-Profildatei ohne Elternordner"))?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("Remote-Profildateiname ist ungültig"))?
        .to_string_lossy();
    let lock_path = parent.join(format!(".{name}.lock"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(lock_path)?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).map_err(io::Error::from)?;
    Ok(lock)
}

fn validate_id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid("Remote-Profil-ID ist ungültig"));
    }
    Ok(())
}

fn validate_profile_header(id: &str, name: &str, protocol: &str) -> io::Result<()> {
    validate_id(id)?;
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(invalid(format!("{protocol}-Profilname ist ungültig")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FtpTransportSecurity, FtpsCertificatePin, FtpsCertificatePolicy, KeychainSecretReference,
        SftpAuthenticationReference, SftpHostKeyPin,
    };
    use std::os::unix::fs::symlink;

    fn profile(id: &str, name: &str) -> SftpProfile {
        SftpProfile {
            id: id.into(),
            name: name.into(),
            connection: SftpConnectionConfig {
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
                    secret: KeychainSecretReference::new("Doppeldecker", id).unwrap(),
                },
            },
        }
    }

    #[test]
    fn profile_roundtrip_update_und_remove_bleiben_geheimnisfrei() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        save_sftp_profile(&path, profile("eins", "Eins")).unwrap();
        save_sftp_profile(&path, profile("zwei", "Zwei")).unwrap();
        save_sftp_profile(&path, profile("eins", "Aktualisiert")).unwrap();
        let loaded = load_sftp_profiles(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "Aktualisiert");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("password\":"));
        assert_eq!(remove_sftp_profile(&path, "eins").unwrap().len(), 1);
    }

    #[test]
    fn parallele_profilergaenzungen_verlieren_keinen_eintrag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        std::thread::scope(|scope| {
            for index in 0..12 {
                let path = path.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let id = format!("parallel-{index}");
                    barrier.wait();
                    save_sftp_profile(&path, profile(&id, &id)).unwrap();
                });
            }
        });
        let loaded = load_sftp_profiles(&path).unwrap();
        assert_eq!(loaded.len(), 12);
    }

    #[test]
    fn profile_revision_bleibt_ueber_entfernen_und_neuanlegen_monoton() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");

        let first = save_sftp_profile_versioned(&path, profile("eins", "Eins")).unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(first.profiles[0].revision, 1);

        let updated = save_sftp_profile_versioned(&path, profile("eins", "Aktualisiert")).unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.profiles[0].revision, 2);

        let removed = remove_sftp_profile_versioned(&path, "eins").unwrap();
        assert!(removed.profiles.is_empty());
        assert_eq!(removed.revision, 3);
        assert!(removed.changed);

        let recreated = save_sftp_profile_versioned(&path, profile("eins", "Neu")).unwrap();
        assert_eq!(recreated.revision, 4);
        assert_eq!(recreated.profiles[0].revision, 4);
        assert_eq!(
            latest_sftp_profile_revision(&path, "eins").unwrap(),
            Some(4)
        );
    }

    #[test]
    fn profilmutation_ist_unter_dem_dateilock_revisionsgebunden() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");

        let first =
            save_sftp_profile_versioned_if_current(&path, profile("geteilt", "Erster Stand"), None)
                .unwrap();
        assert_eq!(first.revision, 1);

        let second = save_sftp_profile_versioned_if_current(
            &path,
            profile("geteilt", "Zweites Fenster"),
            Some(first.revision),
        )
        .unwrap();
        let stale_save = save_sftp_profile_versioned_if_current(
            &path,
            profile("geteilt", "Veralteter Editor"),
            Some(first.revision),
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&stale_save), ErrorCode::PlanDrift);
        assert_eq!(
            load_sftp_profiles(&path).unwrap()[0].name,
            "Zweites Fenster"
        );

        let stale_remove =
            remove_sftp_profile_versioned_if_current(&path, "geteilt", first.revision).unwrap_err();
        assert_eq!(crate::error_code(&stale_remove), ErrorCode::PlanDrift);
        assert_eq!(load_sftp_profiles(&path).unwrap().len(), 1);

        let removed =
            remove_sftp_profile_versioned_if_current(&path, "geteilt", second.revision).unwrap();
        assert!(removed.changed);
        assert!(removed.profiles.is_empty());

        let stale_create =
            save_sftp_profile_versioned_if_current(&path, profile("anderes", "Neu"), Some(0))
                .unwrap_err();
        assert_eq!(crate::error_code(&stale_create), ErrorCode::PlanDrift);
        assert!(load_sftp_profiles(&path).unwrap().is_empty());
    }

    #[test]
    fn altes_profildokument_beginnt_bei_revision_null_und_migriert_beim_speichern() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let payload = serde_json::json!({
            "version": 1,
            "profiles": [profile("alt", "Alt")]
        });
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();

        let legacy = load_sftp_profiles_versioned(&path).unwrap();
        assert_eq!(legacy[0].revision, 0);
        assert_eq!(latest_sftp_profile_revision(&path, "alt").unwrap(), Some(0));
        let saved = save_sftp_profile_versioned(&path, profile("alt", "Neu")).unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.profiles[0].revision, 1);
    }

    #[test]
    fn doppelte_ids_und_traversal_werden_abgelehnt() {
        assert!(profile("../ausbruch", "Nein").validate().is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let payload = serde_json::json!({
            "version": 1,
            "profiles": [profile("gleich", "A"), profile("gleich", "B")]
        });
        std::fs::write(path.clone(), serde_json::to_vec(&payload).unwrap()).unwrap();
        assert!(load_sftp_profiles(&path).is_err());
    }

    #[test]
    fn profildatei_folgt_keinem_finalen_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fremdes-profil.json");
        let link = dir.path().join("profiles.json");
        save_sftp_profile(&target, profile("fremd", "Fremd")).unwrap();
        symlink(&target, &link).unwrap();

        load_sftp_profiles(&link)
            .expect_err("Ein Profildatei-Symlink darf nicht als eigener Zustand gelten");
    }

    #[test]
    fn uebergrosses_profildokument_wird_nicht_committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(&path, b"bleibt").unwrap();
        let mut document = ProfileDocument {
            version: PROFILE_VERSION,
            revision_counter: 1_100,
            revisions: BTreeMap::new(),
            profiles: Vec::new(),
        };
        for index in 0..1_100_u64 {
            let id = format!("profil-{index}");
            let mut item = profile(&id, &id);
            item.connection.requested_root = format!("/{}", "x".repeat(4_000));
            document.revisions.insert(id, index + 1);
            document.profiles.push(item);
        }

        let error = write_profiles(&path, &document, "SFTP")
            .expect_err("Der Writer darf sein eigenes Leselimit nicht überschreiten");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(path).unwrap(), b"bleibt");
    }

    #[test]
    fn ftps_profile_persistiert_nur_pin_und_referenz() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ftps-profiles.json");
        let profile = FtpsProfile {
            id: "ftps_eins".into(),
            name: "FTPS Eins".into(),
            connection: FtpsConnectionConfig {
                host: "ftps.example.test".into(),
                alternate_hosts: Vec::new(),
                port: 21,
                username: "test".into(),
                requested_root: "/daten".into(),
                transport_security: FtpTransportSecurity::ExplicitTls,
                certificate: Some(FtpsCertificatePin { sha256: [7; 32] }),
                certificate_policy: Some(FtpsCertificatePolicy::WebPkiAndPin),
                password_reference: KeychainSecretReference::new("Doppeldecker", "ftps_eins")
                    .unwrap(),
            },
        };
        save_ftps_profile(&path, profile.clone()).unwrap();
        assert_eq!(load_ftps_profiles(&path).unwrap(), vec![profile]);
        let payload = std::fs::read_to_string(&path).unwrap();
        assert!(!payload.contains("secret_value"));
        assert!(payload.contains("password_reference"));
        assert!(payload.contains("certificate_policy"));
        assert!(remove_ftps_profile(&path, "ftps_eins").unwrap().is_empty());
    }

    #[test]
    fn altes_ftps_profil_ohne_transportfeld_bleibt_tls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ftps-profiles.json");
        let payload = serde_json::json!({
            "version": 1,
            "profiles": [{
                "id": "legacy_ftps",
                "name": "Legacy FTPS",
                "connection": {
                    "host": "ftps.example.test",
                    "port": 21,
                    "username": "test",
                    "requested_root": "/",
                    "certificate": { "sha256": vec![7_u8; 32] },
                    "certificate_policy": "web_pki_and_pin",
                    "password_reference": {
                        "service": "Doppeldecker",
                        "account": "legacy_ftps"
                    }
                }
            }]
        });
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
        let profiles = load_ftps_profiles(&path).unwrap();
        assert_eq!(
            profiles[0].connection.transport_security,
            FtpTransportSecurity::ExplicitTls
        );
        assert!(profiles[0].connection.certificate.is_some());
        assert_eq!(
            profiles[0].connection.certificate_policy,
            Some(FtpsCertificatePolicy::WebPkiAndPin)
        );
    }

    #[test]
    fn webdav_profile_persistiert_nur_origin_pin_und_referenz() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webdav-profiles.json");
        let profile = WebDavProfile {
            id: "webdav_eins".into(),
            name: "WebDAV Eins".into(),
            connection: WebDavConnectionConfig {
                origin: "https://dav.example.test".into(),
                username: "test".into(),
                requested_root: "/daten".into(),
                certificate: FtpsCertificatePin { sha256: [8; 32] },
                certificate_policy: FtpsCertificatePolicy::WebPkiAndPin,
                password_reference: KeychainSecretReference::new("Doppeldecker", "webdav_eins")
                    .unwrap(),
            },
        };
        save_webdav_profile(&path, profile.clone()).unwrap();
        assert_eq!(load_webdav_profiles(&path).unwrap(), vec![profile]);
        let payload = std::fs::read_to_string(&path).unwrap();
        assert!(!payload.contains("secret_value"));
        assert!(!payload.contains("password\":"));
        assert!(payload.contains("password_reference"));
        assert!(payload.contains("https://dav.example.test"));
        assert!(remove_webdav_profile(&path, "webdav_eins")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn smb_profile_persistiert_nur_guid_und_referenz() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("smb-profiles.json");
        let profile = SmbProfile {
            id: "smb_eins".into(),
            name: "SMB Eins".into(),
            connection: SmbConnectionConfig {
                host: "smb.example.test".into(),
                port: 445,
                server_guid: [6; 16],
                share: "Daten".into(),
                username: "test".into(),
                domain: "WORKGROUP".into(),
                requested_root: "/archiv".into(),
                password_reference: KeychainSecretReference::new("Doppeldecker", "smb_eins")
                    .unwrap(),
            },
        };
        save_smb_profile(&path, profile.clone()).unwrap();
        assert_eq!(load_smb_profiles(&path).unwrap(), vec![profile]);
        let payload = std::fs::read_to_string(&path).unwrap();
        assert!(!payload.contains("password\":"));
        assert!(payload.contains("server_guid"));
        assert!(payload.contains("password_reference"));
        assert!(remove_smb_profile(&path, "smb_eins").unwrap().is_empty());
    }
}
