//! Lokale Einzeldateikopie mit dauerhaft bestätigter Byte-Grenze.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{
    fchmod, futimens, openat, renameat_with, unlinkat, AtFlags, Mode, OFlags, RenameFlags,
    Timespec, Timestamps,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ftps::resume::{BoundFtpsResume, FtpsFileVersion};
use crate::local_fs::{open_read_beneath, root_identity_beneath, RootHandle};
use crate::local_read::FileVersion;
use crate::sftp::{BoundSftpResume, SftpFileVersion};
use crate::vfs::{FileCommitClass, LocalRootIdentity};
use crate::webdav::resume::WebDavFileVersion;
use crate::{
    coded_error, BidirectionalQueueEndpoint, Cancel, ErrorCode, FtpsServerIdentity, FtpsVfs,
    LocalVfs, Progress, SftpServerIdentity, SftpVfs, Vfs, WebDavServerIdentity, WebDavVfs,
};

const CHUNK: usize = 1024 * 1024;
const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
const PAYLOAD: &str = "daten";
const STAGE_PREFIX: &str = ".doppeldecker-teilkopie-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeCopyPlan {
    pub source: PathBuf,
    pub target: PathBuf,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_root: Option<LocalRootIdentity>,
    target_root: LocalRootIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_version: Option<FileVersion>,
    mtime: Option<(i64, i64)>,
    mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    webdav: Option<Box<WebDavResumeSource>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ftps: Option<Box<FtpsResumeSource>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sftp: Option<Box<SftpResumeSource>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderResumeSource<Identity, Version> {
    endpoint: BidirectionalQueueEndpoint,
    identity: Identity,
    version: Version,
}

type WebDavResumeSource = ProviderResumeSource<WebDavServerIdentity, WebDavFileVersion>;
type FtpsResumeSource = ProviderResumeSource<FtpsServerIdentity, FtpsFileVersion>;
type SftpResumeSource = ProviderResumeSource<SftpServerIdentity, SftpFileVersion>;

enum PreparedSource<'a> {
    Local {
        root: &'a LocalRootIdentity,
        version: FileVersion,
        bytes: u64,
        mode: u32,
        mtime: (i64, i64),
    },
    WebDav(Box<WebDavResumeSource>),
    Ftps(Box<FtpsResumeSource>),
    Sftp(Box<SftpResumeSource>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum CopyCheckpoint {
    Creating,
    Bytes {
        stage: LocalRootIdentity,
        file_id: (u64, u64),
        written: u64,
        digest: [u8; 32],
        sealed: bool,
    },
}

impl CopyCheckpoint {
    pub fn bytes(&self) -> u64 {
        match self {
            Self::Creating => 0,
            Self::Bytes { written, .. } => *written,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CopyOutcome {
    pub paused: bool,
    pub bytes: u64,
    pub recovery: Option<String>,
}

fn input(message: &str) -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}

fn drift(message: &str) -> io::Error {
    crate::real_mode::plan_drift(message)
}

fn pause_requested(cancel: &Cancel, error: &io::Error) -> bool {
    cancel.is_cancelled() && crate::error_code(error) == ErrorCode::Interrupted
}

fn unclear(message: &str, stage: &Path, target: &Path) -> io::Error {
    crate::error::with_recovery_paths(
        coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear, message),
        &[
            stage.to_string_lossy().into_owned(),
            target.to_string_lossy().into_owned(),
        ],
    )
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| input("Datei benötigt einen Elternordner"))
}

fn check_target(root: &RootHandle, target: &Path) -> io::Result<bool> {
    match open_read_beneath(root, target) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn current_source(plan: &ResumeCopyPlan, root: &RootHandle, held: &File) -> io::Result<()> {
    if !root.is_current()?
        || Some(FileVersion::of(&held.metadata()?)) != plan.source_version
        || Some(FileVersion::of(
            &open_read_beneath(root, &plan.source)?.metadata()?,
        )) != plan.source_version
    {
        return Err(drift("Quelldatei hat sich geändert. Teilkopie prüfen."));
    }
    Ok(())
}

fn check_target_root(root: &RootHandle) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if std::fs::read("/sys/fs/selinux/enforce").is_ok() {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Fortsetzbares Kopieren kann bei aktivem SELinux den Zielkontext nicht sicher übernehmen.",
        ));
    }
    if !root.has_exclusive_namespace()?
        || LocalVfs::rooted(root.path())?.file_commit_class_at(root.path())?
            != FileCommitClass::Assured
    {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Fortsetzbares Kopieren benötigt einen eigenen lokalen Zielordner ohne fremde Schreibrechte und mit atomarem Neuanlegen.",
        ));
    }
    Ok(())
}

pub fn prepare_resume_copies(
    sources: &[PathBuf],
    target_root: &Path,
    sandbox: Option<&Path>,
) -> io::Result<Vec<ResumeCopyPlan>> {
    let first = sources
        .first()
        .ok_or_else(|| input("Keine Dateien für fortsetzbare Kopie ausgewählt"))?;
    let source_root = crate::validate_file_operation_root(parent(first)?)?;
    let sandbox = sandbox.map(Path::to_path_buf);
    crate::fileops::guard(&sandbox, &source_root)?;
    let src = RootHandle::open_nofollow(&source_root)?;
    let source_identity = root_identity_beneath(&src, src.path())?;
    prepare_copies(
        sources,
        &source_root,
        target_root,
        sandbox.as_deref(),
        |source| {
            crate::fileops::guard(&sandbox, source)?;
            let file = open_read_beneath(&src, source)?;
            let meta = file.metadata()?;
            if !meta.is_file() {
                return Err(input("Fortsetzbares Kopieren benötigt reguläre Dateien"));
            }
            if meta.mode() & 0o400 == 0 {
                return Err(input("Die Quelle benötigt ein Leserecht für ihren Eigentümer, damit eine abgeschlossene Teilkopie nach einem Neustart geprüft werden kann."));
            }
            Ok(PreparedSource::Local {
                root: &source_identity,
                version: FileVersion::of(&meta),
                bytes: meta.len(),
                mode: meta.mode() & 0o777,
                mtime: (meta.mtime(), meta.mtime_nsec()),
            })
        },
    )
}

pub(crate) fn prepare_webdav_resume_copies(
    vfs: &WebDavVfs,
    endpoint: &BidirectionalQueueEndpoint,
    sources: &[PathBuf],
    target_root: &Path,
    sandbox: Option<&Path>,
    cancel: &Cancel,
) -> io::Result<Vec<ResumeCopyPlan>> {
    let BidirectionalQueueEndpoint::WebDav { connection, root } = endpoint else {
        return Err(input(
            "Fortsetzbarer Download benötigt einen WebDAV-Endpunkt",
        ));
    };
    endpoint.validate()?;
    if vfs.connection_config() != connection.as_ref() {
        return Err(drift("WebDAV-Verbindung passt nicht zum gewählten Profil"));
    }
    let identity = vfs.identity();
    prepare_copies(sources, Path::new(root), target_root, sandbox, |source| {
        let version = vfs.resume_file_version(source)?;
        let mut probe = [0; 1];
        vfs.read_resume_range(
            source,
            &version,
            0,
            &mut probe[..usize::from(version.size > 0)],
            cancel,
        )?;
        Ok(PreparedSource::WebDav(Box::new(WebDavResumeSource {
            version,
            endpoint: endpoint.clone(),
            identity: identity.clone(),
        })))
    })
}

pub(crate) fn prepare_ftps_resume_copies(
    vfs: &FtpsVfs,
    endpoint: &BidirectionalQueueEndpoint,
    sources: &[PathBuf],
    target_root: &Path,
    sandbox: Option<&Path>,
    cancel: &Cancel,
) -> io::Result<Vec<ResumeCopyPlan>> {
    let BidirectionalQueueEndpoint::Ftps { connection, root } = endpoint else {
        return Err(input("Fortsetzbarer Download benötigt einen FTPS-Endpunkt"));
    };
    endpoint.validate()?;
    if vfs.connection_config() != connection.as_ref() {
        return Err(drift("FTPS-Verbindung passt nicht zum gewählten Profil"));
    }
    let identity = vfs.identity();
    prepare_copies(sources, Path::new(root), target_root, sandbox, |source| {
        let (version, _) = vfs.bind_resume_file(source, cancel)?;
        Ok(PreparedSource::Ftps(Box::new(FtpsResumeSource {
            version,
            endpoint: endpoint.clone(),
            identity: identity.clone(),
        })))
    })
}

pub(crate) fn prepare_sftp_resume_copies(
    vfs: &Arc<SftpVfs>,
    endpoint: &BidirectionalQueueEndpoint,
    sources: &[PathBuf],
    target_root: &Path,
    sandbox: Option<&Path>,
    cancel: &Cancel,
) -> io::Result<Vec<ResumeCopyPlan>> {
    let BidirectionalQueueEndpoint::Sftp { connection, root } = endpoint else {
        return Err(input("Fortsetzbarer Download benötigt einen SFTP-Endpunkt"));
    };
    endpoint.validate()?;
    if vfs.connection_config() != connection.as_ref() {
        return Err(drift("SFTP-Verbindung passt nicht zum gewählten Profil"));
    }
    let identity = vfs.identity();
    prepare_copies(sources, Path::new(root), target_root, sandbox, |source| {
        let (version, _) = vfs.bind_resume_file(source, cancel)?;
        Ok(PreparedSource::Sftp(Box::new(SftpResumeSource {
            version,
            endpoint: endpoint.clone(),
            identity: identity.clone(),
        })))
    })
}

fn prepare_copies<'a>(
    sources: &[PathBuf],
    source_root: &Path,
    target_root: &Path,
    sandbox: Option<&Path>,
    mut bind_source: impl FnMut(&Path) -> io::Result<PreparedSource<'a>>,
) -> io::Result<Vec<ResumeCopyPlan>> {
    if sources.is_empty() {
        return Err(input("Keine Dateien für fortsetzbare Kopie ausgewählt"));
    }
    let target_root = crate::validate_file_operation_root(target_root)?;
    let sandbox = sandbox.map(Path::to_path_buf);
    crate::fileops::guard(&sandbox, &target_root)?;
    let dst = RootHandle::open_nofollow(&target_root)?;
    check_target_root(&dst)?;
    let target_identity = root_identity_beneath(&dst, dst.path())?;
    let mut names = std::collections::HashSet::<PathBuf>::new();
    let mut paths = Vec::with_capacity(sources.len());
    for path in sources {
        if crate::validate_file_operation_root(parent(path)?)? != source_root {
            return Err(input(
                "Fortsetzbare Kopien dieser Auswahl benötigen denselben Quellordner",
            ));
        }
        let name = path.file_name().ok_or_else(|| input("Quellname fehlt"))?;
        if name
            .to_str()
            .is_none_or(|name| !crate::archive_read::path_is_safe(name))
        {
            return Err(input(
                "Unsicherer oder nicht eindeutig dekodierbarer Dateiname",
            ));
        }
        let source = source_root.join(name);
        let target = target_root.join(name);
        if !names.insert(target.clone()) {
            return Err(input(
                "Mehrere ausgewählte Quellen ergeben denselben Zielnamen",
            ));
        }
        crate::fileops::guard(&sandbox, &target)?;
        if check_target(&dst, &target)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Am Ziel ist dieser Name bereits belegt: {}",
                    name.to_string_lossy()
                ),
            ));
        }
        paths.push((source, target));
    }
    if paths.len() > 1 {
        check_target_name_collisions(&dst, &paths)?;
    }
    let mut plans = Vec::with_capacity(paths.len());
    for (source, target) in paths {
        let prepared = bind_source(&source)?;
        let mut plan = ResumeCopyPlan {
            source,
            target,
            bytes: 0,
            mode: 0o600,
            mtime: None,
            source_root: None,
            source_version: None,
            target_root: target_identity.clone(),
            webdav: None,
            ftps: None,
            sftp: None,
        };
        match prepared {
            PreparedSource::Local {
                root,
                version,
                bytes,
                mode,
                mtime,
            } => {
                plan.source_root = Some(root.clone());
                plan.source_version = Some(version);
                plan.bytes = bytes;
                plan.mode = mode;
                plan.mtime = Some(mtime);
            }
            PreparedSource::WebDav(binding) => {
                plan.bytes = binding.version.size;
                plan.mtime = binding.version.mtime.map(|time| (time, 0));
                plan.webdav = Some(binding);
            }
            PreparedSource::Ftps(binding) => {
                plan.bytes = binding.version.size;
                plan.mtime = binding.version.mtime.map(|time| (time, 0));
                plan.ftps = Some(binding);
            }
            PreparedSource::Sftp(binding) => {
                plan.bytes = binding.version.size;
                plan.mtime = binding.version.mtime.map(|time| (time, 0));
                plan.sftp = Some(binding);
            }
        }
        plans.push(plan);
    }
    Ok(plans)
}

fn check_target_name_collisions(root: &RootHandle, paths: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    let path = root.path().join(format!(
        "{STAGE_PREFIX}{}",
        crate::queue::new_operation_id()?
    ));
    let stage = root.create_private_stage(&path)?;
    let stage_id = root_identity_beneath(&stage, &path)?;
    let mut created = Vec::with_capacity(paths.len());
    // Das Zielvolume entscheidet über die Gleichheit, etwa für Unicode und APFS.
    // Die Probe verwendet ausschließlich leere Dateien unter einem privaten FD.
    let result = paths.iter().try_for_each(|(_, target)| {
        let name = target.file_name().ok_or_else(|| input("Zielname fehlt"))?;
        match openat(
            stage.fd(),
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(_) => {
                created.push(name);
                Ok(())
            }
            Err(rustix::io::Errno::EXIST) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Mehrere ausgewählte Quellen ergeben am Ziel denselben Namen: {}",
                    name.to_string_lossy()
                ),
            )),
            Err(error) => Err(error.into()),
        }
    });
    let mut cleaned = true;
    for name in created {
        cleaned &= unlinkat(stage.fd(), name, AtFlags::empty()).is_ok();
    }
    if !cleaned || remove_empty_stage(root, path.clone(), &stage_id).is_err() {
        return Err(crate::error::with_recovery_paths(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "Zielnamenprobe konnte nicht vollständig entfernt werden.",
            ),
            &[path.to_string_lossy().into_owned()],
        ));
    }
    result
}

impl ResumeCopyPlan {
    pub fn webdav_endpoint(&self) -> Option<&BidirectionalQueueEndpoint> {
        self.webdav.as_ref().map(|binding| &binding.endpoint)
    }

    pub fn ftps_endpoint(&self) -> Option<&BidirectionalQueueEndpoint> {
        self.ftps.as_ref().map(|binding| &binding.endpoint)
    }

    pub fn sftp_endpoint(&self) -> Option<&BidirectionalQueueEndpoint> {
        self.sftp.as_ref().map(|binding| &binding.endpoint)
    }

    pub fn remote_endpoint(&self) -> Option<&BidirectionalQueueEndpoint> {
        self.webdav_endpoint()
            .or_else(|| self.ftps_endpoint())
            .or_else(|| self.sftp_endpoint())
    }

    pub(crate) fn validate(&self, checkpoint: Option<&CopyCheckpoint>) -> io::Result<()> {
        for path in [&self.source, &self.target] {
            if !path.is_absolute()
                || path.components().any(|c| {
                    !matches!(
                        c,
                        std::path::Component::Normal(_) | std::path::Component::RootDir
                    )
                })
                || path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_none_or(|name| !crate::archive_read::path_is_safe(name))
            {
                return Err(input("Unsicherer Pfad im Auftrag für Teilkopie"));
            }
        }
        if self.remote_endpoint().is_none() && self.source == self.target
            || self.source.file_name() != self.target.file_name()
            || self.mode > 0o777
            || self.mode & 0o400 == 0
            || self
                .mtime
                .is_some_and(|mtime| !(0..1_000_000_000).contains(&mtime.1))
            || matches!(checkpoint, Some(CopyCheckpoint::Bytes { written, sealed, .. }) if *written > self.bytes || *sealed && *written != self.bytes)
        {
            return Err(input("Ungültiger Plan oder Checkpoint für Teilkopie"));
        }
        let (endpoint, bytes, mtime) = match (
            &self.source_root,
            &self.source_version,
            &self.webdav,
            &self.ftps,
            &self.sftp,
        ) {
            (Some(_), Some(_), None, None, None) if self.mtime.is_some() => return Ok(()),
            (None, None, Some(binding), None, None)
                if matches!(binding.endpoint, BidirectionalQueueEndpoint::WebDav { .. }) =>
            {
                binding.version.validate()?;
                (
                    &binding.endpoint,
                    binding.version.size,
                    binding.version.mtime,
                )
            }
            (None, None, None, Some(binding), None) => {
                use crate::FtpSecurityStatus::{
                    TlsIdentityExternallyVerified, TlsIdentityVerified,
                };
                binding.identity.validate()?;
                let BidirectionalQueueEndpoint::Ftps { connection, .. } = &binding.endpoint else {
                    return Err(input("Auftrag für Teilkopie benötigt einen FTPS-Endpunkt"));
                };
                if !matches!(
                    connection.security_status(),
                    TlsIdentityVerified | TlsIdentityExternallyVerified
                ) {
                    return Err(input(
                        "FTPS-Auftrag für Teilkopie benötigt ein geprüftes Zertifikat",
                    ));
                }
                (
                    &binding.endpoint,
                    binding.version.size,
                    binding.version.mtime,
                )
            }
            (None, None, None, None, Some(binding))
                if matches!(binding.endpoint, BidirectionalQueueEndpoint::Sftp { .. }) =>
            {
                binding.identity.validate()?;
                (
                    &binding.endpoint,
                    binding.version.size,
                    binding.version.mtime,
                )
            }
            _ => return Err(input("Ungültige Quellbindung für Teilkopie")),
        };
        endpoint.validate()?;
        if self.source.parent() != Some(Path::new(endpoint.root()))
            || self.bytes != bytes
            || self.mtime != mtime.map(|time| (time, 0))
            || self.mode != 0o600
        {
            return Err(input("Remotequelle passt nicht zum Auftrag für Teilkopie"));
        }
        Ok(())
    }
}

pub(crate) fn stage_path(plan: &ResumeCopyPlan, operation: &str) -> io::Result<PathBuf> {
    if operation.is_empty()
        || operation.len() > 128
        || !operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(input("Ungültige Auftragskennung für Teilkopie"));
    }
    Ok(parent(&plan.target)?.join(format!("{STAGE_PREFIX}{operation}")))
}

pub(crate) fn is_stage_name(name: &str) -> bool {
    name.strip_prefix(STAGE_PREFIX).is_some_and(|id| {
        // Die Queue erzeugt 16 Zufallsbytes als 32 Kleinbuchstaben/Hexziffern.
        id.len() == 32
            && id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

pub(crate) fn is_stage_path(path: &Path) -> bool {
    path.iter()
        .any(|name| name.to_str().is_some_and(is_stage_name))
}

fn identity(file: &File) -> io::Result<(u64, u64)> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(input("Teilkopie ist keine reguläre Datei"));
    }
    Ok((meta.dev(), meta.ino()))
}

fn open_payload(stage: &RootHandle, create: bool, sealed: bool) -> io::Result<File> {
    let access = if sealed { OFlags::RDONLY } else { OFlags::RDWR };
    let create = if create {
        OFlags::CREATE | OFlags::EXCL
    } else {
        OFlags::empty()
    };
    Ok(File::from(openat(
        stage.fd(),
        PAYLOAD,
        access | create | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?))
}

pub(crate) struct RunContext<'a> {
    pub(crate) sandbox: Option<&'a Path>,
    pub(crate) cancel: &'a Cancel,
    pub(crate) connect: Option<Box<dyn FnOnce() -> io::Result<ResumeProvider> + 'a>>,
}

pub(crate) enum ResumeProvider {
    WebDav(Box<WebDavVfs>),
    Ftps(Box<FtpsVfs>),
    Sftp(Arc<SftpVfs>),
}

enum ActiveSource<'a> {
    Local {
        root: RootHandle,
        file: File,
    },
    WebDav {
        binding: &'a WebDavResumeSource,
        vfs: &'a WebDavVfs,
    },
    Ftps {
        binding: &'a FtpsResumeSource,
        vfs: &'a FtpsVfs,
        bound: Option<Box<BoundFtpsResume>>,
        reader: Option<Box<dyn Read + Send>>,
    },
    Sftp {
        binding: &'a SftpResumeSource,
        vfs: &'a SftpVfs,
        bound: Option<Box<BoundSftpResume>>,
        reader: Option<Box<dyn Read + Send>>,
    },
}

impl<'a> ActiveSource<'a> {
    fn bind(
        plan: &'a ResumeCopyPlan,
        provider: Option<&'a ResumeProvider>,
        sandbox: &Option<PathBuf>,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        let source = if let Some(binding) = plan.webdav.as_deref() {
            let Some(ResumeProvider::WebDav(vfs)) = provider else {
                return Err(input("Auftrag benötigt seine WebDAV-Verbindung"));
            };
            Self::WebDav { binding, vfs }
        } else if let Some(binding) = plan.ftps.as_deref() {
            let Some(ResumeProvider::Ftps(vfs)) = provider else {
                return Err(input("Auftrag benötigt seine FTPS-Verbindung"));
            };
            if vfs.identity() != binding.identity {
                return Err(drift(
                    "FTPS-Serveridentität hat sich geändert. Teilkopie bleibt erhalten.",
                ));
            }
            let (version, bound) = vfs.bind_resume_file(&plan.source, cancel)?;
            if version != binding.version {
                return Err(drift("FTPS-Dateiinhalt oder Quellversion hat sich geändert. Teilkopie bleibt erhalten."));
            }
            Self::Ftps {
                binding,
                vfs,
                bound: Some(Box::new(bound)),
                reader: None,
            }
        } else if let Some(binding) = plan.sftp.as_deref() {
            let Some(ResumeProvider::Sftp(vfs)) = provider else {
                return Err(input("Auftrag benötigt seine SFTP-Verbindung"));
            };
            if vfs.identity() != binding.identity {
                return Err(drift(
                    "SFTP-Serveridentität hat sich geändert. Teilkopie bleibt erhalten.",
                ));
            }
            let (version, bound) = vfs.bind_resume_file(&plan.source, cancel)?;
            if version != binding.version {
                return Err(drift("SFTP-Dateiinhalt oder Quellversion hat sich geändert. Teilkopie bleibt erhalten."));
            }
            Self::Sftp {
                binding,
                vfs,
                bound: Some(Box::new(bound)),
                reader: None,
            }
        } else {
            crate::fileops::guard(sandbox, &plan.source)?;
            let root = RootHandle::open_nofollow(parent(&plan.source)?)?;
            if plan.source_root.as_ref() != Some(&root_identity_beneath(&root, root.path())?) {
                return Err(drift("Quellordner hat sich geändert. Teilkopie prüfen."));
            }
            let file = open_read_beneath(&root, &plan.source)?;
            Self::Local { root, file }
        };
        source.verify(plan)?;
        Ok(source)
    }

    fn verify(&self, plan: &ResumeCopyPlan) -> io::Result<()> {
        match self {
            Self::Local { root, file } => current_source(plan, root, file),
            Self::WebDav { binding, vfs } => {
                if vfs.identity() != binding.identity
                    || vfs.resume_file_version(&plan.source)? != binding.version
                {
                    return Err(drift("WebDAV-Quelle oder Serveridentität hat sich geändert. Teilkopie bleibt erhalten."));
                }
                Ok(())
            }
            Self::Ftps { binding, vfs, .. } => {
                if vfs.identity() != binding.identity {
                    return Err(drift(
                        "FTPS-Serveridentität hat sich geändert. Teilkopie bleibt erhalten.",
                    ));
                }
                Ok(())
            }
            Self::Sftp { binding, vfs, .. } => {
                if vfs.identity() != binding.identity {
                    return Err(drift(
                        "SFTP-Serveridentität hat sich geändert. Teilkopie bleibt erhalten.",
                    ));
                }
                Ok(())
            }
        }
    }

    fn local_file(&mut self) -> Option<&mut File> {
        match self {
            Self::Local { file, .. } => Some(file),
            Self::WebDav { .. } | Self::Ftps { .. } | Self::Sftp { .. } => None,
        }
    }

    fn read_chunk(
        &mut self,
        plan: &ResumeCopyPlan,
        offset: u64,
        buffer: &mut [u8],
        cancel: &Cancel,
    ) -> io::Result<()> {
        match self {
            Self::Local { file, .. } => file.read_exact(buffer),
            Self::WebDav { binding, vfs } => {
                vfs.read_resume_range(&plan.source, &binding.version, offset, buffer, cancel)
            }
            Self::Ftps { bound, reader, .. } => {
                if let Some(source) = bound.take() {
                    *reader = Some(source.open_at(offset)?);
                }
                read_bound_suffix(reader, buffer, cancel)
            }
            Self::Sftp { bound, reader, .. } => {
                if let Some(source) = bound.take() {
                    *reader = Some(source.open_at(offset)?);
                }
                read_bound_suffix(reader, buffer, cancel)
            }
        }
    }

    fn verify_complete(&mut self, file: &mut File, cancel: &Cancel) -> io::Result<()> {
        if let Self::Ftps {
            binding,
            bound,
            reader,
            ..
        } = self
        {
            if let Some(source) = bound.take() {
                // Leere Dateien brauchen ebenfalls RETR. Bei älteren Vollbelegen
                // den letzten Bytebereich lesen, da REST am EOF nicht überall geht.
                let size = binding.version.size;
                let offset = size.saturating_sub(1);
                *reader = Some(source.open_at(offset)?);
                if size > 0 {
                    let mut actual = [0; 1];
                    let mut expected = [0; 1];
                    read_bound_suffix(reader, &mut actual, cancel)?;
                    file.seek(SeekFrom::Start(offset))?;
                    file.read_exact(&mut expected)?;
                    if actual != expected {
                        return Err(drift(
                            "Der letzte FTPS-Dateiabschnitt weicht von der Teilkopie ab.",
                        ));
                    }
                }
            }
        }
        let (protocol, size, expected, reader) = match self {
            Self::Ftps {
                binding, reader, ..
            } => ("FTPS", binding.version.size, binding.version.sha256, reader),
            Self::Sftp {
                binding, reader, ..
            } => ("SFTP", binding.version.size, binding.version.sha256, reader),
            _ => return Ok(()),
        };
        if let Some(reader) = reader {
            if reader.read(&mut [0; 1])? != 0 {
                return Err(drift(&format!(
                    "{protocol} lieferte mehr Bytes als bestätigt. Teilkopie prüfen."
                )));
            }
        }
        file.seek(SeekFrom::Start(0))?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0; CHUNK];
        let mut left = size;
        while left > 0 {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Prüfsummenprüfung pausiert",
                ));
            }
            let len = left.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..len])?;
            digest.update(&buffer[..len]);
            left -= len as u64;
        }
        let actual: [u8; 32] = digest.finalize().into();
        if actual != expected || file.read(&mut [0; 1])? != 0 {
            return Err(drift(&format!("Die vollständige Teilkopie weicht vom bestätigten {protocol}-Dateiinhalt ab. Teilkopie verwerfen und neu kopieren.")));
        }
        Ok(())
    }
}

fn read_bound_suffix(
    reader: &mut Option<Box<dyn Read + Send>>,
    buffer: &mut [u8],
    cancel: &Cancel,
) -> io::Result<()> {
    let reader = reader
        .as_mut()
        .ok_or_else(|| input("Gebundener Datenkanal fehlt"))?;
    let mut remaining = buffer;
    while !remaining.is_empty() {
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Download pausiert",
            ));
        }
        let limit = remaining.len().min(64 * 1024);
        match reader.read(&mut remaining[..limit]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Download endete vor der bestätigten Größe",
                ))
            }
            Ok(read) => remaining = &mut remaining[read..],
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn verify_prefix(
    mut source: Option<&mut File>,
    partial: &mut File,
    written: u64,
    digest: [u8; 32],
    cancel: &Cancel,
) -> io::Result<blake3::Hasher> {
    let mut hash = blake3::Hasher::new();
    let mut source_buf = vec![0; CHUNK];
    let mut partial_buf = vec![0; CHUNK];
    let mut left = written;
    while left != 0 {
        if cancel.is_cancelled() {
            return Err(coded_error(
                io::ErrorKind::Interrupted,
                ErrorCode::Interrupted,
                "Prüfung der Teilkopie unterbrochen",
            ));
        }
        let take = left.min(CHUNK as u64) as usize;
        partial.read_exact(&mut partial_buf[..take])?;
        if let Some(source) = source.as_deref_mut() {
            source.read_exact(&mut source_buf[..take])?;
            if source_buf[..take] != partial_buf[..take] {
                return Err(drift(
                    "Teilkopie stimmt nicht mehr mit dem bestätigten Quellabschnitt überein.",
                ));
            }
        }
        hash.update(&partial_buf[..take]);
        left -= take as u64;
    }
    if *hash.finalize().as_bytes() != digest {
        return Err(drift(
            "Die Prüfsumme des bestätigten Quellabschnitts hat sich geändert.",
        ));
    }
    Ok(hash)
}

fn check_installed(
    file: &mut File,
    file_id: (u64, u64),
    bytes: u64,
    digest: &[u8; 32],
) -> io::Result<bool> {
    if identity(file)? != file_id || file.metadata()?.len() != bytes {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut hash = blake3::Hasher::new();
    let read = io::copy(&mut (&mut *file).take(bytes.saturating_add(1)), &mut hash)?;
    Ok(read == bytes && file.metadata()?.len() == bytes && hash.finalize().as_bytes() == digest)
}

fn remove_empty_stage(
    root: &RootHandle,
    stage: PathBuf,
    expected: &LocalRootIdentity,
) -> io::Result<()> {
    if !root.is_current()?
        || !root.has_exclusive_namespace()?
        || root_identity_beneath(root, &stage)? != *expected
    {
        return Err(drift("Ort des Teilkopie-Ordners hat sich geändert"));
    }
    let name = stage
        .file_name()
        .ok_or_else(|| input("Ungültiger Teilkopie-Ordner"))?;
    unlinkat(root.fd(), name, AtFlags::REMOVEDIR)?;
    File::from(rustix::io::dup(root.fd())?).sync_all()?;
    Ok(())
}

pub(crate) fn discard(
    plan: &ResumeCopyPlan,
    operation: &str,
    checkpoint: &CopyCheckpoint,
    sandbox: Option<&Path>,
) -> io::Result<()> {
    let path = stage_path(plan, operation)?;
    crate::fileops::guard(&sandbox.map(Path::to_path_buf), &path)?;
    let root = RootHandle::open_nofollow(parent(&plan.target)?)?;
    if !root.has_exclusive_namespace()?
        || root_identity_beneath(&root, root.path())? != plan.target_root
    {
        return Err(drift("Zielordner der Teilkopie hat sich geändert."));
    }
    let sealed = matches!(checkpoint, CopyCheckpoint::Bytes { sealed: true, .. });
    let possible_commit = || {
        unclear(
            "Die vollständige Teilkopie fehlt am Arbeitsnamen. Vor dem Schließen das Ziel prüfen.",
            &path,
            &plan.target,
        )
    };
    let stage = match RootHandle::open_beneath(&root, &path) {
        Ok(stage) => stage,
        Err(error) if sealed && error.kind() == io::ErrorKind::NotFound => {
            return Err(possible_commit());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return File::from(rustix::io::dup(root.fd())?).sync_all();
        }
        Err(error) => return Err(error),
    };
    let CopyCheckpoint::Bytes {
        stage: expected,
        file_id,
        ..
    } = checkpoint
    else {
        return Err(unclear(
            "Anlage der Teilkopie war nicht bestätigt. Arbeitsordner manuell prüfen.",
            &path,
            &plan.target,
        ));
    };
    if !stage.is_private()? || root_identity_beneath(&stage, &path)? != *expected {
        return Err(drift("Arbeitsordner der Teilkopie hat sich geändert."));
    }
    match open_payload(&stage, false, true) {
        Ok(payload) => {
            if identity(&payload)? != *file_id || !stage.is_current()? {
                return Err(drift("Datei der Teilkopie hat sich geändert."));
            }
            unlinkat(stage.fd(), PAYLOAD, AtFlags::empty())?;
        }
        Err(error) if sealed && error.kind() == io::ErrorKind::NotFound => {
            return Err(possible_commit());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    File::from(rustix::io::dup(stage.fd())?).sync_all()?;
    remove_empty_stage(&root, path, expected)
}

pub(crate) fn execute_with_source(
    plan: &ResumeCopyPlan,
    operation: &str,
    checkpoint: Option<&CopyCheckpoint>,
    context: RunContext<'_>,
    mut persist: impl FnMut(CopyCheckpoint) -> io::Result<()>,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<CopyOutcome> {
    plan.validate(checkpoint)?;
    let stage_path = stage_path(plan, operation)?;
    let sandbox = context.sandbox.map(Path::to_path_buf);
    let cancel = context.cancel;
    crate::fileops::guard(&sandbox, &plan.target)?;
    crate::fileops::guard(&sandbox, &stage_path)?;
    let dst = RootHandle::open_nofollow(parent(&plan.target)?)?;
    check_target_root(&dst)?;
    if root_identity_beneath(&dst, dst.path())? != plan.target_root {
        return Err(drift("Zielordner hat sich geändert. Teilkopie prüfen."));
    }
    let target_name = plan
        .target
        .file_name()
        .ok_or_else(|| input("Zielname fehlt"))?;
    if let Some(CopyCheckpoint::Bytes {
        stage,
        file_id,
        written,
        digest,
        sealed: true,
    }) = checkpoint
    {
        match open_read_beneath(&dst, &plan.target) {
            Ok(mut installed) => {
                if *written != plan.bytes
                    || !check_installed(&mut installed, *file_id, *written, digest)?
                {
                    return Err(unclear(
                        "Am Ziel liegt eine andere Fassung. Teilkopie und Ziel prüfen.",
                        &stage_path,
                        &plan.target,
                    ));
                }
                File::from(rustix::io::dup(dst.fd())?).sync_all()?;
                let recovery = remove_empty_stage(&dst, stage_path.clone(), stage)
                    .err()
                    .filter(|error| error.kind() != io::ErrorKind::NotFound)
                    .map(|_| stage_path.to_string_lossy().into_owned());
                return Ok(CopyOutcome {
                    paused: false,
                    bytes: *written,
                    recovery,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    } else if check_target(&dst, &plan.target)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Am Ziel ist dieser Name bereits belegt.",
        ));
    }
    let paused = |bytes| CopyOutcome {
        paused: true,
        bytes,
        recovery: checkpoint.map(|_| stage_path.to_string_lossy().into_owned()),
    };
    let saved = checkpoint.map_or(0, CopyCheckpoint::bytes);
    if cancel.is_cancelled() {
        return Ok(paused(saved));
    }
    let provider = context.connect.map(|connect| connect()).transpose()?;
    let mut source = match ActiveSource::bind(plan, provider.as_ref(), &sandbox, cancel) {
        Err(error) if pause_requested(cancel, &error) => {
            return Ok(paused(saved));
        }
        result => result?,
    };

    let (stage, stage_id, mut file, file_id, mut written, digest, sealed) = match checkpoint {
        Some(CopyCheckpoint::Bytes {
            stage,
            file_id,
            written,
            digest,
            sealed,
        }) => {
            let held = RootHandle::open_beneath(&dst, &stage_path)?;
            if !held.is_private()?
                || root_identity_beneath(&held, &stage_path)? != *stage
                || *written > plan.bytes
            {
                return Err(drift(
                    "Beleg der Teilkopie passt nicht mehr zum Arbeitsordner.",
                ));
            }
            let file = open_payload(&held, false, *sealed)?;
            if identity(&file)? != *file_id || file.metadata()?.len() < *written {
                return Err(drift("Teilkopie hat sich geändert."));
            }
            (
                held,
                stage.clone(),
                file,
                *file_id,
                *written,
                *digest,
                *sealed,
            )
        }
        None | Some(CopyCheckpoint::Creating) => {
            persist(CopyCheckpoint::Creating)?;
            let stage = match dst.create_private_stage(&stage_path) {
                Ok(stage) => stage,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(unclear(
                        "Teilkopie-Ordner ist vorhanden; seine Anlage ist nicht bestätigt.",
                        &stage_path,
                        &plan.target,
                    ))
                }
                Err(error) => return Err(error),
            };
            let file = open_payload(&stage, true, false)?;
            let stage_id = root_identity_beneath(&stage, &stage_path)?;
            let file_id = identity(&file)?;
            let digest = *blake3::hash(&[]).as_bytes();
            file.sync_all()?;
            File::from(rustix::io::dup(stage.fd())?).sync_all()?;
            persist(CopyCheckpoint::Bytes {
                stage: stage_id.clone(),
                file_id,
                written: 0,
                digest,
                sealed: false,
            })?;
            (stage, stage_id, file, file_id, 0, digest, false)
        }
    };
    let mut report = Progress {
        files_total: 1,
        bytes_total: plan.bytes,
        bytes_done: written,
        current: target_name.to_string_lossy().into_owned(),
        phase: if written == 0 {
            "copy"
        } else {
            "resumable_verify"
        }
        .into(),
        ..Default::default()
    };
    progress(&report);
    let paused = |bytes| CopyOutcome {
        paused: true,
        bytes,
        recovery: Some(stage_path.to_string_lossy().into_owned()),
    };
    let mut hash = match verify_prefix(source.local_file(), &mut file, written, digest, cancel) {
        Err(error) if pause_requested(cancel, &error) => {
            return Ok(paused(written));
        }
        result => result?,
    };
    source.verify(plan)?;
    report.phase = "copy".into();
    if !sealed {
        // Ein Prozessabbruch kann Bytes hinter dem letzten dauerhaften Beleg
        // hinterlassen. Erst nach Prüfen des belegten Prefix am gebundenen FD kürzen.
        if file.metadata()?.len() > written {
            file.set_len(written)?;
        }
        let chunk = if plan.webdav.is_some() {
            CHECKPOINT_BYTES as usize
        } else {
            CHUNK
        };
        let mut buffer = vec![0; chunk];
        let mut confirmed = written;
        let mut confirmed_digest = digest;
        let ftps = plan.ftps.is_some();
        while written < plan.bytes && !cancel.is_cancelled() {
            let len = (plan.bytes - written).min(chunk as u64) as usize;
            match source.read_chunk(plan, written, &mut buffer[..len], cancel) {
                Err(error) if pause_requested(cancel, &error) => break,
                result => result?,
            };
            file.write_all(&buffer[..len])?;
            hash.update(&buffer[..len]);
            written += len as u64;
            if written - confirmed >= CHECKPOINT_BYTES && (!ftps || written < plan.bytes) {
                if plan.remote_endpoint().is_none() {
                    source.verify(plan)?;
                }
                file.sync_all()?;
                confirmed_digest = *hash.finalize().as_bytes();
                persist(CopyCheckpoint::Bytes {
                    stage: stage_id.clone(),
                    file_id,
                    written,
                    digest: confirmed_digest,
                    sealed: false,
                })?;
                confirmed = written;
            }
            report.bytes_done = written;
            progress(&report);
        }
        if !cancel.is_cancelled() || plan.remote_endpoint().is_none() {
            source.verify(plan)?;
        }
        if !cancel.is_cancelled() {
            match source.verify_complete(&mut file, cancel) {
                Err(error) if pause_requested(cancel, &error) => {}
                result => result?,
            }
        }
        let sealed = !cancel.is_cancelled();
        let mut checkpoint_digest = *hash.finalize().as_bytes();
        if ftps && !sealed && written == plan.bytes {
            written = confirmed;
            checkpoint_digest = confirmed_digest;
        }
        file.sync_all()?;
        persist(CopyCheckpoint::Bytes {
            stage: stage_id.clone(),
            file_id,
            written,
            digest: checkpoint_digest,
            sealed,
        })?;
    }
    if cancel.is_cancelled() {
        return Ok(paused(written));
    }

    let final_digest = *hash.finalize().as_bytes();
    if written != plan.bytes || !check_installed(&mut file, file_id, written, &final_digest)? {
        return Err(drift(
            "Vollständige Teilkopie hat die Inhaltsprüfung nicht bestanden.",
        ));
    }
    if let Some((seconds, nanos)) = plan.mtime {
        futimens(
            &file,
            &Timestamps {
                last_access: Timespec {
                    tv_sec: 0,
                    tv_nsec: rustix::fs::UTIME_OMIT,
                },
                last_modification: Timespec {
                    tv_sec: seconds,
                    tv_nsec: nanos as _,
                },
            },
        )?;
    }
    fchmod(&file, Mode::from_bits_truncate(plan.mode as _))?;
    file.sync_all()?;
    source.verify(plan)?;
    if !dst.is_current()?
        || !dst.has_exclusive_namespace()?
        || !stage.is_current()?
        || !stage.is_private()?
    {
        return Err(drift("Zielordner oder Teilkopie-Ordner hat sich geändert."));
    }
    if cancel.is_cancelled() {
        return Ok(paused(written));
    }
    if identity(&open_payload(&stage, false, true)?)? != file_id {
        return Err(drift("Teilkopie am Arbeitsnamen hat sich geändert."));
    }
    if let Err(error) = renameat_with(
        stage.fd(),
        PAYLOAD,
        dst.fd(),
        target_name,
        RenameFlags::NOREPLACE,
    ) {
        if matches!(
            error,
            rustix::io::Errno::IO | rustix::io::Errno::TIMEDOUT | rustix::io::Errno::INTR
        ) {
            return Err(unclear(
                "Ausgang der Umbenennung ist unklar.",
                &stage_path,
                &plan.target,
            ));
        }
        return Err(error.into());
    }
    File::from(rustix::io::dup(stage.fd())?)
        .sync_all()
        .map_err(|_| {
            unclear(
                "Datei sichtbar, Dauerhaftigkeit der Umbenennung ungeklärt.",
                &stage_path,
                &plan.target,
            )
        })?;

    File::from(rustix::io::dup(dst.fd())?)
        .sync_all()
        .map_err(|_| {
            unclear(
                "Datei sichtbar, Dauerhaftigkeit am Ziel ungeklärt.",
                &stage_path,
                &plan.target,
            )
        })?;
    let mut installed = open_read_beneath(&dst, &plan.target).map_err(|_| {
        unclear(
            "Datei sichtbar, Zielprüfung fehlgeschlagen.",
            &stage_path,
            &plan.target,
        )
    })?;
    if !check_installed(&mut installed, file_id, written, &final_digest).map_err(|_| {
        unclear(
            "Die abschließende Prüflesung ist fehlgeschlagen.",
            &stage_path,
            &plan.target,
        )
    })? {
        return Err(unclear(
            "Die abschließende Prüflesung passt nicht zur Teilkopie.",
            &stage_path,
            &plan.target,
        ));
    }
    let recovery = remove_empty_stage(&dst, stage_path.clone(), &stage_id)
        .err()
        .map(|_| stage_path.to_string_lossy().into_owned());
    Ok(CopyOutcome {
        paused: false,
        bytes: written,
        recovery,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{
        execute_local_queue_entry, LocalQueueExecution, OperationQueue, QueueOperation, QueueState,
    };
    use std::os::unix::fs::PermissionsExt;

    fn execute(
        plan: &ResumeCopyPlan,
        operation: &str,
        checkpoint: Option<&CopyCheckpoint>,
        sandbox: Option<&Path>,
        cancel: &Cancel,
        persist: impl FnMut(CopyCheckpoint) -> io::Result<()>,
        progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<CopyOutcome> {
        execute_with_source(
            plan,
            operation,
            checkpoint,
            RunContext {
                sandbox,
                cancel,
                connect: None,
            },
            persist,
            progress,
        )
    }

    fn prepare_resume_copy(
        source: &Path,
        target: &Path,
        sandbox: Option<&Path>,
    ) -> io::Result<ResumeCopyPlan> {
        let mut plans = prepare_resume_copies(&[source.to_path_buf()], target, sandbox)?;
        Ok(plans.remove(0))
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Vec<u8>) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let src = root.join("probe.bin");
        let dst = root.join("ziel");
        std::fs::create_dir(&dst).unwrap();
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();
        let data: Vec<_> = (0..CHUNK * 3)
            .map(|i| (i.rotate_left(3) % 251) as u8)
            .collect();
        std::fs::write(&src, &data).unwrap();
        (temp, src, dst, data)
    }

    #[test]
    fn resumable_copy_batch_uses_target_name_equality() {
        let (_temp, source, target, _) = fixture();
        let paths = [
            (source.clone(), target.join("probe.txt")),
            (source, target.join("PROBE.txt")),
        ];
        std::fs::write(&paths[0].1, b"canary").unwrap();
        let folds_case = paths[1].1.exists();
        std::fs::remove_file(&paths[0].1).unwrap();
        let target = RootHandle::open_nofollow(&target).unwrap();
        let result = check_target_name_collisions(&target, &paths);
        if folds_case {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        } else {
            result.unwrap();
        }
        assert!(std::fs::read_dir(target.path()).unwrap().next().is_none());
    }

    #[test]
    fn resumable_copy_does_not_retry_a_paused_sftp_reader() {
        struct InterruptedReader(Cancel, bool);
        impl Read for InterruptedReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert!(
                    !self.0.is_cancelled(),
                    "Der SFTP-Reader ist nach der Unterbrechung nicht wiederverwendbar"
                );
                if self.1 {
                    self.0.cancel();
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.1 = true;
                buffer[0] = 0x5a;
                Ok(1)
            }
        }
        let cancel = Cancel::new();
        let mut reader: Option<Box<dyn Read + Send>> =
            Some(Box::new(InterruptedReader(cancel.clone(), false)));
        let error = read_bound_suffix(&mut reader, &mut [0; 2], &cancel).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn resumable_copy_restores_queue_prefix_and_ignores_unconfirmed_tail() {
        let (temp, source, destination, original) = fixture();
        let journal = temp.path().join("queue.json");
        let plan = prepare_resume_copy(&source, &destination, None).unwrap();
        let stage;
        let id;
        let execution = LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 0,
            max_delete_fraction: 0.0,
        };
        {
            let queue = OperationQueue::open(&journal).unwrap();
            id = queue
                .enqueue(QueueOperation::ResumableCopy {
                    plan: Box::new(plan.clone()),
                    checkpoint: None,
                })
                .unwrap()
                .operation_id;
            let entry = queue.claim_next().unwrap().unwrap();
            let stop = Cancel::new();
            execute_local_queue_entry(&queue, entry, &execution, &stop, &mut |progress| {
                if progress.bytes_done >= CHUNK as u64 {
                    stop.cancel();
                }
            })
            .unwrap();
            let paused = queue.snapshot().entries.remove(0);
            assert_eq!(paused.state, QueueState::Interrupted);
            assert!(!plan.target.exists());
            stage = stage_path(&plan, &id).unwrap();
            assert_eq!(paused.temp_recovery_paths, [stage.to_string_lossy()]);
            assert_eq!(
                std::fs::metadata(stage.join(PAYLOAD)).unwrap().len(),
                CHUNK as u64
            );
        }
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        legacy["version"] = 45.into();
        std::fs::write(&journal, serde_json::to_vec(&legacy).unwrap()).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(stage.join(PAYLOAD))
            .unwrap()
            .write_all(b"nicht bestaetigter Rest")
            .unwrap();
        let queue = OperationQueue::open(journal).unwrap();
        queue.retry(&id).unwrap();
        assert!(queue.remove_pending(&id).is_err());
        std::fs::write(&plan.target, b"zwischenzeitlich belegt").unwrap();
        let error = execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::AlreadyExists);
        let failed = queue.snapshot().entries.remove(0);
        assert_eq!(failed.state, QueueState::Failed);
        assert_eq!(failed.temp_recovery_paths, [stage.to_string_lossy()]);
        assert_eq!(
            std::fs::read(&plan.target).unwrap(),
            b"zwischenzeitlich belegt"
        );
        std::fs::remove_file(&plan.target).unwrap();
        queue.retry(&id).unwrap();
        let mut observations = Vec::new();
        execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution,
            &Cancel::new(),
            &mut |progress| observations.push(progress.bytes_done),
        )
        .unwrap();
        assert_eq!(observations.first(), Some(&(CHUNK as u64)));
        let completed = queue.snapshot().entries.remove(0);
        assert_eq!(completed.state, QueueState::Completed);
        assert!(completed.temp_recovery_paths.is_empty());
        assert!(!stage.exists());
        assert_eq!(std::fs::read(plan.target).unwrap(), original);
        assert_eq!(std::fs::read(source).unwrap(), original);
    }

    #[test]
    fn resumable_copy_batch_stops_at_paused_file_and_never_recopies_completion() {
        let (temp, second, target, original) = fixture();
        let parent = second.parent().unwrap();
        let first = parent.join("1-klein.txt");
        let third = parent.join("3-klein.txt");
        let first_target = target.join("1-klein.txt");
        let second_target = target.join(second.file_name().unwrap());
        let third_target = target.join("3-klein.txt");
        std::fs::write(&first, b"zuerst").unwrap();
        std::fs::write(&third, b"zuletzt").unwrap();
        let paths = [first, second, third];
        let journal = temp.path().join("queue.json");
        let execution = LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 0,
            max_delete_fraction: 0.0,
        };
        let ids;
        let first_inode;
        {
            let queue = OperationQueue::open(&journal).unwrap();
            std::fs::write(&second_target, b"belegt").unwrap();
            assert_eq!(
                prepare_resume_copies(&paths, &target, None)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
            assert!(queue.snapshot().entries.is_empty());
            std::fs::remove_file(&second_target).unwrap();
            let plans = prepare_resume_copies(&paths, &target, None).unwrap();
            let revision = queue.snapshot().revision;
            ids = queue.enqueue_resumable_copies(plans, &[]).unwrap();
            assert_eq!(ids.len(), 3);
            assert_eq!(queue.snapshot().revision, revision + 1);
            let first = queue.claim_next().unwrap().unwrap();
            assert_eq!(first.operation_id, ids[0]);
            execute_local_queue_entry(&queue, first, &execution, &Cancel::new(), &mut |_| {})
                .unwrap();
            first_inode = std::fs::metadata(&first_target).unwrap().ino();

            let second = queue.claim_next().unwrap().unwrap();
            assert_eq!(second.operation_id, ids[1]);
            let stop = Cancel::new();
            execute_local_queue_entry(&queue, second, &execution, &stop, &mut |progress| {
                if progress.bytes_done >= CHUNK as u64 {
                    stop.cancel();
                }
            })
            .unwrap();
            assert!(!queue.has_runnable());
            assert!(!second_target.exists() && !third_target.exists());
        }
        let queue = OperationQueue::open(&journal).unwrap();
        queue.retry(&ids[1]).unwrap();
        for id in &ids[1..] {
            let entry = queue.claim_next().unwrap().unwrap();
            assert_eq!(&entry.operation_id, id);
            execute_local_queue_entry(&queue, entry, &execution, &Cancel::new(), &mut |_| {})
                .unwrap();
        }
        assert_eq!(std::fs::metadata(&first_target).unwrap().ino(), first_inode);
        assert_eq!(std::fs::read(first_target).unwrap(), b"zuerst");
        assert_eq!(std::fs::read(second_target).unwrap(), original);
        assert_eq!(std::fs::read(third_target).unwrap(), b"zuletzt");
        assert_eq!(
            queue
                .snapshot()
                .entries
                .iter()
                .map(|entry| entry.attempts)
                .collect::<Vec<_>>(),
            [1, 2, 1]
        );
    }

    #[test]
    fn resumable_copy_observes_sealed_commit_without_recopying() {
        let (_temp, source, destination, original) = fixture();
        let plan = prepare_resume_copy(&source, &destination, None).unwrap();
        let mut sealed = None;
        assert!(execute(
            &plan,
            "seal",
            None,
            None,
            &Cancel::new(),
            |next| {
                if matches!(next, CopyCheckpoint::Bytes { sealed: true, .. }) {
                    sealed = Some(next);
                    return Err(io::Error::other("simuliertes Prozessende"));
                }
                Ok(())
            },
            &mut |_| {},
        )
        .is_err());
        let stage = stage_path(&plan, "seal").unwrap();
        std::fs::rename(stage.join(PAYLOAD), &plan.target).unwrap();
        std::fs::write(source, b"inzwischen geaendert").unwrap();
        let installed = std::fs::metadata(&plan.target).unwrap().ino();
        let discard_error = discard(&plan, "seal", sealed.as_ref().unwrap(), None).unwrap_err();
        assert_eq!(crate::error_code(&discard_error), ErrorCode::OutcomeUnclear);
        assert_eq!(
            crate::error::recovery_paths(&discard_error),
            [stage.to_string_lossy(), plan.target.to_string_lossy()]
        );
        assert!(stage.is_dir());
        let resumed = execute(
            &plan,
            "seal",
            sealed.as_ref(),
            None,
            &Cancel::new(),
            |_| panic!("Abgeschlossenen Commit nur abgleichen"),
            &mut |_| {},
        )
        .unwrap();
        assert!(!resumed.paused);
        assert_eq!(resumed.bytes, original.len() as u64);
        assert!(!stage.exists());
        assert_eq!(std::fs::metadata(&plan.target).unwrap().ino(), installed);
        assert_eq!(std::fs::read(plan.target).unwrap(), original);
    }

    #[test]
    fn resumable_copy_work_directory_survives_mirror_without_user_patterns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let target = root.join("target");
        let parent = target.join("nur-am-ziel");
        let stage = parent.join(format!("{STAGE_PREFIX}0123456789abcdef0123456789abcdef"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join(PAYLOAD), b"nur ein Teil").unwrap();
        std::fs::write(parent.join("entfernbar.txt"), b"Altbestand").unwrap();
        std::fs::write(
            source.join(".doppeldecker-teilkopie-eigene-notiz"),
            b"Nutzdaten",
        )
        .unwrap();
        let vfs = LocalVfs::new();
        let preview = crate::plan(
            &vfs,
            &source,
            &vfs,
            &target,
            &crate::SyncOptions {
                mode: crate::SyncMode::Mirror,
                compare: crate::Compare::ContentHash,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(preview.exclusions.target_count, 1);
        assert_eq!(preview.delete_count, 1);
        assert!(preview.actions.iter().any(|action| matches!(
            action,
            crate::Action::DeleteFile { path, .. } if path == "nur-am-ziel/entfernbar.txt"
        )));
        assert!(preview.actions.iter().any(|action| matches!(
            action,
            crate::Action::CopyFile { path, .. } if path == ".doppeldecker-teilkopie-eigene-notiz"
        )));
        let opposite =
            crate::plan(&vfs, &target, &vfs, &source, &crate::SyncOptions::default()).unwrap();
        assert_eq!(opposite.exclusions.source_count, 1);
        assert!(opposite.actions.iter().all(|action| !matches!(
            action,
            crate::Action::CopyFile { path, .. } if path.ends_with("/daten")
        )));
        assert!(crate::plan(&vfs, &stage, &vfs, &source, &crate::SyncOptions::default()).is_err());
    }

    #[test]
    fn resumable_copy_discards_only_bound_working_payload() {
        let (temp, source, destination, original) = fixture();
        let plan = prepare_resume_copy(&source, &destination, None).unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let id = queue
            .enqueue(QueueOperation::ResumableCopy {
                plan: Box::new(plan.clone()),
                checkpoint: None,
            })
            .unwrap()
            .operation_id;
        let stop = Cancel::new();
        let entry = queue.claim_next().unwrap().unwrap();
        execute_local_queue_entry(
            &queue,
            entry,
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 0,
                max_delete_fraction: 0.0,
            },
            &stop,
            &mut |progress| {
                if progress.bytes_done >= CHUNK as u64 {
                    stop.cancel();
                }
            },
        )
        .unwrap();
        let stage = stage_path(&plan, &id).unwrap();
        assert!(stage.join(PAYLOAD).is_file());
        std::fs::write(&plan.target, b"fremdes Ziel").unwrap();
        queue.discard_resumable_copy(&id, None).unwrap();
        let entry = queue.snapshot().entries.remove(0);
        assert_eq!(entry.state, QueueState::Cancelled);
        assert!(entry.temp_recovery_paths.is_empty());
        assert!(queue.retry(&id).is_err());
        assert!(!stage.exists());
        assert_eq!(std::fs::read(plan.target).unwrap(), b"fremdes Ziel");
        assert_eq!(std::fs::read(source).unwrap(), original);
    }

    #[test]
    fn resumable_copy_refuses_changed_prefix_and_existing_target() {
        let (_temp, source, destination, _) = fixture();
        let plan = prepare_resume_copy(&source, &destination, None).unwrap();
        let stop = Cancel::new();
        let mut checkpoint = None;
        execute(
            &plan,
            "probe",
            None,
            None,
            &stop,
            |next| {
                checkpoint = Some(next);
                Ok(())
            },
            &mut |progress| {
                if progress.bytes_done >= CHUNK as u64 {
                    stop.cancel();
                }
            },
        )
        .unwrap();
        let payload = stage_path(&plan, "probe").unwrap().join(PAYLOAD);
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .open(&payload)
            .unwrap();
        target.write_all(b"fremd").unwrap();
        let before = std::fs::read(&payload).unwrap();
        let error = execute(
            &plan,
            "probe",
            checkpoint.as_ref(),
            None,
            &Cancel::new(),
            |_| panic!("Beschädigten Beleg niemals weiterführen"),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert!(!plan.target.exists());
        assert_eq!(std::fs::read(&payload).unwrap(), before);

        std::fs::write(&plan.target, b"bereits am Ziel").unwrap();
        let error = prepare_resume_copy(&source, &destination, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&plan.target).unwrap(), b"bereits am Ziel");
    }
}
