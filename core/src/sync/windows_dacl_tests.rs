//! Datei-DACLs an echten Temp-Inodes: Kopierabschluss, Auslassungen und Drift.
use super::*;
use crate::metadata::{
    Acl, AclModel, BlobSummary, Capability, MetadataCapabilities, NamedBlobSummary,
    WindowsSecurityObservation,
};
use crate::vfs::{CompleteDirectoryListing, LocalVfs};
use crate::windows_dacl::{
    self, WindowsDaclOmission, WindowsDaclTargetIssue, WindowsDaclTransfer,
    WindowsDaclWriteContext, WindowsDaclWriteSupport,
};
use crate::windows_security::{WindowsAce, WindowsDacl, WindowsDaclState, WindowsSid};
use crate::{Cancel, ErrorCode, Progress};
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read, Write},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

const NAMESPACE: [u8; 16] = [0x42; 16];
type Inode = (u64, u64);
type Streams = BTreeMap<Vec<u8>, Vec<u8>>;
type StreamStore = Arc<Mutex<BTreeMap<Inode, Streams>>>;
type DirectorySnapshot = (String, Vec<(String, Vec<u8>)>, Streams);
type DirectorySnapshots = Arc<Mutex<Vec<DirectorySnapshot>>>;

fn sid(rid: u32) -> WindowsSid {
    WindowsSid {
        revision: 1,
        authority: 5,
        sub_authorities: vec![21, 1, 2, 3, rid],
    }
}
fn dacl() -> WindowsDacl {
    WindowsDacl {
        namespace: NAMESPACE,
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![
                WindowsAce::Allow {
                    flags: 0,
                    mask: 0x1701ff,
                    sid: sid(1000),
                },
                WindowsAce::Deny {
                    flags: 0,
                    mask: 0x80000,
                    sid: sid(2000),
                },
                WindowsAce::Allow {
                    flags: 0,
                    mask: 0x120089,
                    sid: sid(3000),
                },
            ],
        },
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Security {
    acl: WindowsDacl,
    owner: WindowsSid,
    group: WindowsSid,
}
#[derive(Clone, Copy)]
enum WriteFailure {
    None,
    BeforeSet,
    AfterSet,
}
#[derive(Clone, Copy)]
enum ListingFault {
    Denied,
    Incomplete,
    Interrupted,
    Drift,
}
struct DaclVfs {
    inner: LocalVfs,
    default: Security,
    security: Arc<Mutex<BTreeMap<Inode, Security>>>,
    streams: StreamStore,
    support: WindowsDaclWriteSupport,
    support_reads: AtomicUsize,
    writes: AtomicUsize,
    acl_attempts: Arc<AtomicUsize>,
    acl_mutations: Arc<AtomicUsize>,
    directory_snapshots: DirectorySnapshots,
    directory_finishes: Arc<AtomicUsize>,
    directory_finish_failure: bool,
    created_dacl_ready: bool,
    fail_directory: Option<String>,
    created: Mutex<Vec<(PathBuf, Inode, Security)>>,
    failure: WriteFailure,
    listing_fault: Mutex<Option<(PathBuf, ListingFault, bool)>>,
    faulty_listing_attempts: AtomicUsize,
}
impl DaclVfs {
    fn new(owner: u32) -> Self {
        Self {
            inner: LocalVfs::new(),
            default: Security {
                acl: dacl(),
                owner: sid(owner),
                group: sid(owner + 1),
            },
            security: Arc::default(),
            streams: Arc::default(),
            support: WindowsDaclWriteSupport::Ready(WindowsDaclWriteContext {
                namespace: NAMESPACE,
                owner: sid(1000),
                group: Some(sid(1001)),
            }),
            support_reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            acl_attempts: Arc::default(),
            acl_mutations: Arc::default(),
            directory_snapshots: Arc::default(),
            directory_finishes: Arc::default(),
            directory_finish_failure: false,
            created_dacl_ready: false,
            fail_directory: None,
            created: Mutex::default(),
            failure: WriteFailure::None,
            listing_fault: Mutex::new(None),
            faulty_listing_attempts: AtomicUsize::new(0),
        }
    }
    fn key(path: &Path) -> io::Result<Inode> {
        let metadata = fs::metadata(path)?;
        Ok((metadata.dev(), metadata.ino()))
    }
    fn security(&self, path: &Path) -> Security {
        self.security
            .lock()
            .unwrap()
            .get(&Self::key(path).unwrap())
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }
    fn set_security(&self, path: &Path, value: Security) {
        self.security
            .lock()
            .unwrap()
            .insert(Self::key(path).unwrap(), value);
    }
    fn seed_stream(&self, path: &Path) {
        self.streams
            .lock()
            .unwrap()
            .entry(Self::key(path).unwrap())
            .or_default()
            .insert(b"metadata".to_vec(), b"ADS-content".to_vec());
    }
    fn streams(&self, path: &Path) -> Streams {
        self.streams
            .lock()
            .unwrap()
            .get(&Self::key(path).unwrap())
            .cloned()
            .unwrap_or_default()
    }
}
struct StreamWriter {
    store: StreamStore,
    key: Inode,
    name: Vec<u8>,
}
impl Write for StreamWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.store
            .lock()
            .unwrap()
            .get_mut(&self.key)
            .unwrap()
            .get_mut(&self.name)
            .unwrap()
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Vfs for DaclVfs {
    fn supports_mode_write(&self) -> bool {
        false
    }
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.inner.list(path)
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let fault = {
            let mut state = self.listing_fault.lock().unwrap();
            match state.as_ref() {
                Some((expected, fault, permanent)) if path == expected => {
                    self.faulty_listing_attempts.fetch_add(1, Ordering::SeqCst);
                    let fault = *fault;
                    if !permanent {
                        *state = None;
                    }
                    Some(fault)
                }
                _ => None,
            }
        };
        match fault {
            Some(ListingFault::Denied) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Listing verweigert",
            )),
            Some(ListingFault::Interrupted) => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Listing abgebrochen",
            )),
            Some(ListingFault::Drift) => Err(crate::real_mode::plan_drift("Bekannte Listingdrift")),
            Some(ListingFault::Incomplete) => self.inner.list_complete(path).map(|mut listing| {
                listing.complete = false;
                listing
            }),
            None => self.inner.list_complete(path),
        }
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.inner.stat(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.create_dir_all(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.create_dir(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.inner.open_read(path)
    }
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.open_write(path)
    }
    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        let writer = self.inner.open_write_new(path)?;
        let key = Self::key(path)?;
        self.security
            .lock()
            .unwrap()
            .insert(key, self.default.clone());
        self.streams.lock().unwrap().insert(key, BTreeMap::new());
        self.created
            .lock()
            .unwrap()
            .push((path.into(), key, self.default.clone()));
        Ok(writer)
    }
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn set_mtime(&self, path: &Path, value: i64) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set_mtime(path, value)
    }
    fn set_mode(&self, path: &Path, value: u32) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set_mode(path, value)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.rename(from, to)
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.rename_noreplace(from, to)
    }
    fn metadata_capabilities(&self, _: &Path) -> io::Result<MetadataCapabilities> {
        Ok(MetadataCapabilities {
            owner: Capability::Unsupported,
            special_mode_bits: Capability::Unsupported,
            acl_model: AclModel::WindowsSid,
            acl: Capability::ReadOnly,
            xattrs: Capability::Unsupported,
            resource_fork: Capability::Unsupported,
            named_streams: Capability::Exact,
            sparse_extents: Capability::Unsupported,
            hardlinks: Capability::Unsupported,
        })
    }
    fn metadata_manifest(&self, path: &Path) -> io::Result<MetadataManifest> {
        let security = self.security(path);
        MetadataManifest::new(
            None,
            self.inner.metadata_manifest(path)?.mode,
            Some(Acl::WindowsDacl(security.acl.clone())),
            vec![],
            None,
            None,
            None,
        )?
        .with_windows_security(Some(WindowsSecurityObservation {
            namespace: security.acl.namespace,
            owner: Some(security.owner),
            group: Some(security.group),
            issue: None,
            invalid_descriptor_digest: None,
        }))?
        .with_named_streams(
            if fs::metadata(path)?.is_file()
                || self.streams.lock().unwrap().contains_key(&Self::key(path)?)
            {
                Some(
                    self.streams(path)
                        .into_iter()
                        .map(|(name, bytes)| NamedBlobSummary {
                            name,
                            value: BlobSummary::from_bytes(&bytes),
                        })
                        .collect(),
                )
            } else {
                None
            },
        )
    }
    fn windows_dacl_write_support(&self) -> io::Result<WindowsDaclWriteSupport> {
        self.support_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.support.clone())
    }
    fn created_directory_dacl_write_support(&self) -> io::Result<WindowsDaclWriteSupport> {
        Ok(if self.created_dacl_ready {
            self.support.clone()
        } else {
            WindowsDaclWriteSupport::Unavailable(WindowsDaclTargetIssue::UnsupportedBackend)
        })
    }
    fn directory_dacl_write_support(&self) -> io::Result<WindowsDaclWriteSupport> {
        Ok(self.support.clone())
    }
    fn begin_directory_dacl_transaction(
        &self,
        path: &Path,
        plan: &crate::directory_dacl::DirectoryDaclPlan,
    ) -> io::Result<Box<dyn crate::directory_dacl::DirectoryDaclTransaction>> {
        plan.validate()?;
        let proof = plan.target_proof.as_ref().unwrap();
        if !proof.matches_directory_identity(&self.stat(path)?)
            || self.security(path).acl != plan.before
        {
            return Err(crate::coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::PlanDrift,
                "Verzeichnisbasis änderte sich",
            ));
        }
        let failure = if self
            .fail_directory
            .as_ref()
            .is_none_or(|name| path.file_name().unwrap() == name.as_str())
        {
            self.failure
        } else {
            WriteFailure::None
        };
        Ok(Box::new(DirectoryTransaction {
            path: path.into(),
            key: Self::key(path)?,
            plan: plan.clone(),
            security: self.security.clone(),
            report: Default::default(),
            failure,
            attempts: self.acl_attempts.clone(),
            mutations: self.acl_mutations.clone(),
            snapshots: self.directory_snapshots.clone(),
            finishes: self.directory_finishes.clone(),
            finish_failure: self.directory_finish_failure,
            streams: self.streams.clone(),
        }))
    }
    fn write_windows_dacl_exact(
        &self,
        path: &Path,
        acl: &WindowsDacl,
        context: &WindowsDaclWriteContext,
    ) -> io::Result<()> {
        self.acl_attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_directory.is_none() && matches!(self.failure, WriteFailure::BeforeSet) {
            return Err(windows_dacl::write_omission(
                WindowsDaclTargetIssue::RequiredAccessDenied,
            ));
        }
        let mut security = self.security(path);
        assert_eq!(context.owner, security.owner);
        assert_eq!(context.group.as_ref(), Some(&security.group));
        acl.encode_for_file_write(context.namespace, &context.owner)?;
        security.acl = acl.clone();
        self.set_security(path, security);
        self.acl_mutations.fetch_add(1, Ordering::SeqCst);
        if self.fail_directory.is_none() && matches!(self.failure, WriteFailure::AfterSet) {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "DACL-SET gesendet; Readback injiziert unterbrochen",
            ));
        }
        Ok(())
    }
    fn open_named_stream_read(&self, path: &Path, name: &[u8]) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(
            self.streams(path)
                .get(name)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Strom fehlt"))?,
        )))
    }
    fn open_named_stream_write_new(
        &self,
        path: &Path,
        name: &[u8],
    ) -> io::Result<Box<dyn Write + Send>> {
        let key = Self::key(path)?;
        let mut store = self.streams.lock().unwrap();
        let streams = store
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("Eigene Temp-Datei fehlt"))?;
        if streams.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Strom kollidiert",
            ));
        }
        streams.insert(name.to_vec(), vec![]);
        Ok(Box::new(StreamWriter {
            store: self.streams.clone(),
            key,
            name: name.to_vec(),
        }))
    }
}
struct DirectoryTransaction {
    path: PathBuf,
    key: Inode,
    plan: crate::directory_dacl::DirectoryDaclPlan,
    security: Arc<Mutex<BTreeMap<Inode, Security>>>,
    report: crate::directory_dacl::DirectoryDaclReport,
    failure: WriteFailure,
    attempts: Arc<AtomicUsize>,
    mutations: Arc<AtomicUsize>,
    snapshots: DirectorySnapshots,
    finishes: Arc<AtomicUsize>,
    finish_failure: bool,
    streams: StreamStore,
}
impl crate::directory_dacl::DirectoryDaclTransaction for DirectoryTransaction {
    fn apply(&mut self) -> io::Result<crate::directory_dacl::DirectoryDaclReport> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if matches!(self.failure, WriteFailure::BeforeSet) {
            return Err(windows_dacl::write_omission(
                WindowsDaclTargetIssue::RequiredAccessDenied,
            ));
        }
        assert_eq!(DaclVfs::key(&self.path)?, self.key);
        let mut children = Vec::new();
        let mut streams = Streams::new();
        for child in fs::read_dir(&self.path)? {
            let child = child?;
            if child.file_type()?.is_file() {
                let name = child.file_name().to_string_lossy().into_owned();
                children.push((name.clone(), fs::read(child.path())?));
                for (stream, value) in self
                    .streams
                    .lock()
                    .unwrap()
                    .get(&DaclVfs::key(&child.path())?)
                    .cloned()
                    .unwrap_or_default()
                {
                    let mut label = name.as_bytes().to_vec();
                    label.push(b':');
                    label.extend(stream);
                    streams.insert(label, value);
                }
            }
        }
        children.sort();
        self.snapshots.lock().unwrap().push((
            self.path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            children,
            streams,
        ));
        self.security
            .lock()
            .unwrap()
            .get_mut(&self.key)
            .unwrap()
            .acl = self.plan.after.clone();
        self.mutations.fetch_add(1, Ordering::SeqCst);
        let outcome = self.plan.outcome(&self.path.to_string_lossy())?;
        if matches!(self.failure, WriteFailure::AfterSet) {
            self.report.uncertain.push(outcome);
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "Verzeichnis-SET-Ausgang unklar",
            ));
        }
        self.report.completed.push(outcome);
        Ok(self.report.clone())
    }
    fn report(&self) -> crate::directory_dacl::DirectoryDaclReport {
        self.report.clone()
    }
    fn recovery_paths(&self) -> Vec<String> {
        vec![self.path.to_string_lossy().into_owned()]
    }
    fn finish(self: Box<Self>) -> io::Result<crate::directory_dacl::DirectoryDaclReport> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        if self.finish_failure {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "Intentabschluss nach bestätigtem SET unterbrochen",
            ));
        }
        Ok(self.report)
    }
}

struct Fixture {
    source_root: tempfile::TempDir,
    target_root: tempfile::TempDir,
    source: DaclVfs,
    target: DaclVfs,
}
impl Fixture {
    fn new(existing: Option<&[u8]>) -> Self {
        let root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let source_root = tempfile::tempdir_in(&root).unwrap();
        let target_root = tempfile::tempdir_in(root).unwrap();
        fs::write(source_root.path().join("a"), b"new-content").unwrap();
        if let Some(bytes) = existing {
            fs::write(target_root.path().join("a"), bytes).unwrap();
        }
        let fixture = Self {
            source_root,
            target_root,
            source: DaclVfs::new(4000),
            target: DaclVfs::new(1000),
        };
        fixture.source.seed_stream(&fixture.source_path());
        fixture
    }
    fn source_path(&self) -> PathBuf {
        self.source_root.path().join("a")
    }
    fn target_path(&self) -> PathBuf {
        self.target_root.path().join("a")
    }
    fn options() -> SyncOptions {
        SyncOptions {
            mode: SyncMode::Update,
            compare: Compare::ContentHash,
            metadata: MetadataProfile::NativeBestEffort,
            ..Default::default()
        }
    }
    fn plan(&self) -> SyncPlan {
        plan_content_with_cancel(
            &self.source,
            self.source_root.path(),
            &self.target,
            self.target_root.path(),
            &Self::options(),
            &Cancel::new(),
            false,
        )
        .unwrap()
    }
    fn run(&self, plan: &SyncPlan) -> io::Result<SyncStats> {
        execute(
            ExecuteRequest {
                src: &self.source,
                src_root: self.source_root.path(),
                dst: &self.target,
                dst_root: self.target_root.path(),
                plan,
                safety: &Safety::headless_test(),
            },
            &Cancel::new(),
            &mut |_| {},
        )
    }
}
fn transfer(plan: &SyncPlan) -> &MetadataTransfer {
    plan.actions
        .iter()
        .find_map(|action| match action {
            Action::CopyFile { metadata, .. } | Action::UpdateFile { metadata, .. } => {
                metadata.as_ref()
            }
            _ => None,
        })
        .unwrap()
}
fn acl_report(report: &MetadataReport) -> &MetadataClassStats {
    report
        .classes
        .iter()
        .find(|item| item.class == MetadataClass::Acl)
        .unwrap()
}

#[test]
fn new_and_updated_files_transfer_exact_dacl_without_translating_source_owner() {
    for existing in [None, Some(b"old-content".as_slice())] {
        let fixture = Fixture::new(existing);
        let previous = existing.map(|_| DaclVfs::key(&fixture.target_path()).unwrap());
        let plan = fixture.plan();
        let metadata = transfer(&plan);
        assert!(
            matches!(&metadata.windows_dacl, Some(WindowsDaclTransfer::Exact(context)) if context.owner == sid(1000))
        );
        assert!(metadata.acl);
        assert!(!metadata.owner);
        assert!(!metadata.normal_mode);
        let stats = fixture.run(&plan).unwrap();
        assert_eq!(fs::read(fixture.target_path()).unwrap(), b"new-content");
        assert_eq!(
            fixture.target.streams(&fixture.target_path()),
            fixture.source.streams(&fixture.source_path())
        );
        let result = fixture.target.security(&fixture.target_path());
        let created = fixture.target.created.lock().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(DaclVfs::key(&fixture.target_path()).unwrap(), created[0].1);
        assert_ne!(previous, Some(created[0].1));
        assert_eq!(
            result.acl,
            fixture.source.security(&fixture.source_path()).acl
        );
        assert_eq!(result.owner, created[0].2.owner);
        assert_eq!(result.group, created[0].2.group);
        assert_ne!(
            result.owner,
            fixture.source.security(&fixture.source_path()).owner
        );
        assert_eq!(fixture.target.acl_mutations.load(Ordering::SeqCst), 1);
        assert_eq!(acl_report(&stats.metadata_report).transferred, 1);
    }
}

#[test]
fn same_content_dacl_difference_does_not_replace_file_or_streams() {
    let fixture = Fixture::new(Some(b"new-content"));
    fixture.target.seed_stream(&fixture.target_path());
    let mut security = fixture.target.security(&fixture.target_path());
    security.acl.control = 4;
    fixture
        .target
        .set_security(&fixture.target_path(), security.clone());
    let inode = DaclVfs::key(&fixture.target_path()).unwrap();
    let plan = fixture.plan();
    assert!(plan.actions.is_empty());
    assert_eq!(plan.copy_count + plan.update_count, 0);
    assert!(plan
        .metadata_report
        .issues
        .iter()
        .any(|issue| issue.path == "a" && issue.class == MetadataClass::Acl));
    fixture.run(&plan).unwrap();
    assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.target.acl_attempts.load(Ordering::SeqCst), 0);
    assert_eq!(DaclVfs::key(&fixture.target_path()).unwrap(), inode);
    assert_eq!(fixture.target.security(&fixture.target_path()), security);
}

#[test]
fn unobserved_directory_streams_omit_new_directory_acl_while_child_content_is_copied() {
    let fixture = Fixture::new(None);
    fs::create_dir(fixture.source_root.path().join("folder")).unwrap();
    fs::write(fixture.source_root.path().join("folder/child"), b"child").unwrap();
    let plan = fixture.plan();
    let directory = plan
        .actions
        .iter()
        .find_map(|action| match action {
            Action::CreateDir {
                path,
                metadata: Some(metadata),
                ..
            } if path == "folder" => Some(metadata),
            _ => None,
        })
        .unwrap();
    assert!(directory.source.named_streams.is_none());
    assert_eq!(
        directory.windows_dacl,
        Some(WindowsDaclTransfer::Omitted(
            WindowsDaclOmission::CreatedDirectoryStreamsUnobserved
        ))
    );
    assert!(!directory.acl);
    fixture.run(&plan).unwrap();
    assert_eq!(
        fs::read(fixture.target_root.path().join("folder/child")).unwrap(),
        b"child"
    );
    assert_eq!(fixture.target.acl_mutations.load(Ordering::SeqCst), 2);
}

#[test]
fn unavailable_target_and_ineligible_source_keep_content_and_ads_independent() {
    for case in [
        "default",
        "normalized",
        "foreign_namespace",
        "opaque",
        "inherited",
        "relative_sid",
    ] {
        let mut fixture = Fixture::new(None);
        match case {
            "default" => {
                fixture.target.support =
                    WindowsDaclWriteSupport::Unavailable(WindowsDaclTargetIssue::NotPrepared)
            }
            "normalized" => {
                fixture.target.support =
                    WindowsDaclWriteSupport::Unavailable(WindowsDaclTargetIssue::Normalized)
            }
            _ => {
                let mut security = fixture.source.security(&fixture.source_path());
                match case {
                    "foreign_namespace" => security.acl.namespace = [0x43; 16],
                    "opaque" => {
                        if let WindowsDaclState::Acl { aces, .. } = &mut security.acl.state {
                            aces.push(WindowsAce::Opaque {
                                ace_type: 9,
                                flags: 0,
                                data: vec![0; 4],
                            });
                        }
                    }
                    "inherited" => {
                        if let WindowsDaclState::Acl { aces, .. } = &mut security.acl.state {
                            if let WindowsAce::Allow { flags, .. } = &mut aces[0] {
                                *flags = 0x10;
                            }
                        }
                    }
                    "relative_sid" => {
                        if let WindowsDaclState::Acl { aces, .. } = &mut security.acl.state {
                            aces.push(WindowsAce::Allow {
                                flags: 0,
                                mask: 0,
                                sid: WindowsSid {
                                    revision: 1,
                                    authority: 3,
                                    sub_authorities: vec![4],
                                },
                            });
                        }
                    }
                    _ => unreachable!(),
                }
                fixture
                    .source
                    .set_security(&fixture.source_path(), security);
            }
        }
        let plan = fixture.plan();
        assert!(
            matches!(
                transfer(&plan).windows_dacl,
                Some(WindowsDaclTransfer::Omitted(_))
            ),
            "{case}"
        );
        assert!(!transfer(&plan).acl, "{case}");
        let stats = fixture.run(&plan).unwrap();
        assert_eq!(
            fs::read(fixture.target_path()).unwrap(),
            b"new-content",
            "{case}"
        );
        assert_eq!(
            fixture.target.streams(&fixture.target_path()),
            fixture.source.streams(&fixture.source_path()),
            "{case}"
        );
        assert_eq!(
            fixture.target.acl_attempts.load(Ordering::SeqCst),
            0,
            "{case}"
        );
        assert_eq!(acl_report(&stats.metadata_report).omitted, 1, "{case}");
    }
}

#[test]
fn known_pre_set_denial_is_omitted_only_with_explicit_runtime_permission() {
    for allowed in [false, true] {
        let mut fixture = Fixture::new(Some(b"old-content"));
        fixture.target.failure = WriteFailure::BeforeSet;
        let plan = fixture.plan();
        let temp = fixture.target_root.path().join("owned-test-temp");
        fixture
            .target
            .open_write_new(&temp)
            .unwrap()
            .write_all(b"new-content")
            .unwrap();
        let before = fixture.target.security(&temp);
        let mut omitted = None;
        let result = transfer_planned_metadata(
            PlannedMetadataRequest {
                src: &fixture.source,
                src_path: &fixture.source_path(),
                dst: &fixture.target,
                temp_path: &temp,
                expected: Some(transfer(&plan)),
                allow_windows_dacl_omission: allowed,
                windows_dacl_omission: &mut omitted,
            },
            &Cancel::new(),
            &mut Progress::default(),
            &mut |_| {},
        );
        if allowed {
            assert!(result.unwrap());
            assert_eq!(omitted, Some(WindowsDaclTargetIssue::RequiredAccessDenied));
            let mut report = plan.metadata_report.clone();
            let before = acl_report(&report).clone();
            crate::content_sync::record_windows_dacl_omission(&mut report, "a", omitted.unwrap());
            assert_eq!(acl_report(&report).transferred, before.transferred - 1);
            assert_eq!(acl_report(&report).omitted, before.omitted + 1);
            assert!(report.issues.iter().any(|issue| issue.path == "a"
                && issue.class == MetadataClass::Acl
                && issue.kind == MetadataIssueKind::PrivilegeFailure));
        } else {
            assert!(result.is_err());
            assert_eq!(omitted, None);
        }
        assert_eq!(fixture.target.security(&temp), before);
        assert_eq!(
            fixture.target.streams(&temp),
            fixture.source.streams(&fixture.source_path())
        );
        assert_eq!(fs::read(fixture.target_path()).unwrap(), b"old-content");
        assert_eq!(fixture.target.acl_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.target.acl_mutations.load(Ordering::SeqCst), 0);
    }
    let mut fixture = Fixture::new(Some(b"old-content"));
    fixture.target.failure = WriteFailure::BeforeSet;
    let plan = fixture.plan();
    assert!(fixture.run(&plan).is_err());
    assert_eq!(fixture.target.acl_mutations.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read(fixture.target_path()).unwrap(), b"old-content");
}

#[test]
fn outcome_unclear_after_set_retains_owned_temp_and_old_target() {
    let mut fixture = Fixture::new(Some(b"old-content"));
    fixture.target.failure = WriteFailure::AfterSet;
    let before = DaclVfs::key(&fixture.target_path()).unwrap();
    let security = fixture.target.security(&fixture.target_path());
    let plan = fixture.plan();
    let error = fixture.run(&plan).unwrap_err();
    assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
    assert_eq!(fs::read(fixture.target_path()).unwrap(), b"old-content");
    assert_eq!(DaclVfs::key(&fixture.target_path()).unwrap(), before);
    assert_eq!(fixture.target.security(&fixture.target_path()), security);
    assert_eq!(fixture.target.acl_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.target.acl_mutations.load(Ordering::SeqCst), 1);
    let created = fixture.target.created.lock().unwrap();
    assert_eq!(created.len(), 1);
    assert!(created[0].0.exists());
    assert_eq!(fs::read(&created[0].0).unwrap(), b"new-content");
    assert!(
        crate::error::recovery_paths(&error).contains(&created[0].0.to_string_lossy().into_owned())
    );
}

#[test]
fn noop_security_observation_drift_stops_before_sibling_copy_or_empty_plan_success() {
    for sibling in [false, true] {
        let fixture = Fixture::new(Some(b"new-content"));
        fixture.target.seed_stream(&fixture.target_path());
        if sibling {
            fs::write(fixture.source_root.path().join("b"), b"new-sibling").unwrap();
        }
        let plan = fixture.plan();
        assert_eq!(plan.copy_count, u64::from(sibling));
        let mut changed = fixture.source.security(&fixture.source_path());
        changed.owner = sid(5000);
        fixture.source.set_security(&fixture.source_path(), changed);
        let error = fixture.run(&plan).unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.target.acl_attempts.load(Ordering::SeqCst), 0);
        assert!(!fixture.target_root.path().join("b").exists());
    }
}

#[test]
fn public_strict_planner_never_activates_prepared_windows_writer() {
    let fixture = Fixture::new(None);
    let plan = plan(
        &fixture.source,
        fixture.source_root.path(),
        &fixture.target,
        fixture.target_root.path(),
        &Fixture::options(),
    )
    .unwrap();
    assert!(!transfer(&plan).acl);
    assert!(!matches!(
        transfer(&plan).windows_dacl,
        Some(WindowsDaclTransfer::Exact(_))
    ));
    assert_eq!(fixture.target.support_reads.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn legacy_transfer_and_plan_omit_new_empty_windows_fields() {
    let source = MetadataManifest::new(None, 0o644, None, vec![], None, None, None).unwrap();
    let legacy: MetadataTransfer = serde_json::from_value(
        serde_json::json!({ "source": source, "xattrs": false, "resource_fork": false }),
    )
    .unwrap();
    assert_eq!(legacy.windows_dacl, None);
    assert!(!serde_json::to_string(&legacy)
        .unwrap()
        .contains("windows_dacl"));
    let mut encoded = serde_json::to_value(SyncPlan::default()).unwrap();
    encoded
        .as_object_mut()
        .unwrap()
        .remove("metadata_observations");
    let legacy_plan: SyncPlan = serde_json::from_value(encoded.clone()).unwrap();
    assert!(legacy_plan.metadata_observations.is_empty());
    assert_eq!(serde_json::to_value(legacy_plan).unwrap(), encoded);
}

fn directory_fixture(names: &[&str]) -> Fixture {
    let fixture = Fixture::new(None);
    fs::remove_file(fixture.source_path()).unwrap();
    for name in names {
        let source = fixture.source_root.path().join(name);
        let target = fixture.target_root.path().join(name);
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        let mut security = fixture.target.security(&target);
        if let WindowsDaclState::Acl { aces, .. } = &mut security.acl.state {
            aces.pop();
        }
        fixture.target.set_security(&target, security);
        let time = fixture.source.stat(&source).unwrap().mtime.unwrap();
        fixture.target.inner.set_mtime(&target, time).unwrap();
    }
    fixture
}

#[test]
fn existing_directory_acl_only_preserves_identity_children_owner_group_and_ads() {
    let fixture = directory_fixture(&["folder"]);
    let source = fixture.source_root.path().join("folder");
    let target = fixture.target_root.path().join("folder");
    for root in [&source, &target] {
        fs::write(root.join("child"), b"unchanged").unwrap();
    }
    fixture.source.seed_stream(&source.join("child"));
    fixture.target.seed_stream(&target.join("child"));
    let security = fixture.target.security(&target);
    let identity = DaclVfs::key(&target).unwrap();
    let child_identity = DaclVfs::key(&target.join("child")).unwrap();
    let plan = fixture.plan();
    assert_eq!(
        plan.copy_count + plan.update_count + plan.bytes_to_transfer,
        0
    );
    assert!(plan.actions.iter().any(|action| matches!(action, Action::SetDirectoryMetadata { path, metadata, .. } if path == "folder" && matches!(metadata.windows_dacl, Some(WindowsDaclTransfer::DirectoryExact(_))))));
    let stats = fixture.run(&plan).unwrap();
    assert_eq!(stats.directory_dacl_report.completed.len(), 1);
    assert_eq!(DaclVfs::key(&target).unwrap(), identity);
    assert_eq!(DaclVfs::key(&target.join("child")).unwrap(), child_identity);
    assert_eq!(fixture.target.security(&target).owner, security.owner);
    assert_eq!(fixture.target.security(&target).group, security.group);
    assert_eq!(
        fixture.target.security(&target).acl,
        fixture.source.security(&source).acl
    );
    assert_eq!(fs::read(target.join("child")).unwrap(), b"unchanged");
    assert_eq!(
        fixture.target.streams(&target.join("child")),
        fixture.source.streams(&source.join("child"))
    );
}

#[test]
fn same_directory_acl_has_no_effect_and_no_write() {
    let fixture = directory_fixture(&["folder"]);
    let target = fixture.target_root.path().join("folder");
    let mut security = fixture.target.security(&target);
    security.acl = dacl();
    fixture.target.set_security(&target, security);
    let plan = fixture.plan();
    assert!(plan.actions.is_empty());
    let stats = fixture.run(&plan).unwrap();
    assert!(stats.directory_dacl_report.is_empty());
    assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.target.acl_attempts.load(Ordering::SeqCst), 0);
}

#[test]
fn directory_acl_source_target_and_identity_drift_prevent_all_sibling_effects() {
    for drift in ["source", "target", "identity"] {
        let fixture = directory_fixture(&["folder"]);
        fs::write(fixture.source_root.path().join("sibling"), b"new").unwrap();
        let plan = fixture.plan();
        let target = fixture.target_root.path().join("folder");
        if drift == "identity" {
            fs::rename(&target, fixture.target_root.path().join("old")).unwrap();
            fs::create_dir(&target).unwrap();
            let mut security = fixture.target.security(&target);
            if let WindowsDaclState::Acl { aces, .. } = &mut security.acl.state {
                aces.pop();
            }
            fixture.target.set_security(&target, security);
        } else {
            let (vfs, path) = if drift == "source" {
                (&fixture.source, fixture.source_root.path().join("folder"))
            } else {
                (&fixture.target, target)
            };
            let mut security = vfs.security(&path);
            if let WindowsDaclState::Acl { aces, .. } = &mut security.acl.state {
                aces.remove(0);
            }
            vfs.set_security(&path, security);
        }
        assert!(fixture.run(&plan).is_err(), "{drift}");
        assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0, "{drift}");
        assert_eq!(
            fixture.target.acl_attempts.load(Ordering::SeqCst),
            0,
            "{drift}"
        );
        assert!(!fixture.target_root.path().join("sibling").exists());
    }
}

#[test]
fn child_contents_and_ads_finish_before_directory_acl_and_known_denial_keeps_content() {
    for failure in [WriteFailure::None, WriteFailure::BeforeSet] {
        let mut fixture = directory_fixture(&["folder"]);
        fixture.target.failure = failure;
        fixture.target.fail_directory = Some("folder".into());
        let source = fixture.source_root.path().join("folder/child");
        fs::write(&source, b"new child").unwrap();
        fixture.source.seed_stream(&source);
        let plan = fixture.plan();
        let stats = fixture.run(&plan).unwrap();
        assert_eq!(
            fs::read(fixture.target_root.path().join("folder/child")).unwrap(),
            b"new child"
        );
        if matches!(failure, WriteFailure::None) {
            let snapshots = fixture.target.directory_snapshots.lock().unwrap();
            assert_eq!(
                snapshots[0].1,
                vec![("child".into(), b"new child".to_vec())]
            );
            assert_eq!(
                snapshots[0].2.get(b"child:metadata".as_slice()).unwrap(),
                b"ADS-content"
            );
            assert_eq!(stats.directory_dacl_report.completed.len(), 1);
        } else {
            assert!(stats.directory_dacl_report.is_empty());
            assert!(stats
                .metadata_report
                .issues
                .iter()
                .any(|issue| issue.path == "folder"
                    && issue.class == MetadataClass::Acl
                    && issue.kind == MetadataIssueKind::PrivilegeFailure));
        }
    }
}

#[test]
fn directory_acl_partial_completed_and_uncertain_reports_survive_error() {
    let mut fixture = directory_fixture(&["a", "b"]);
    fixture.target.failure = WriteFailure::AfterSet;
    fixture.target.fail_directory = Some("b".into());
    let plan = fixture.plan();
    let mut events = Vec::new();
    let error = execute(
        ExecuteRequest {
            src: &fixture.source,
            src_root: fixture.source_root.path(),
            dst: &fixture.target,
            dst_root: fixture.target_root.path(),
            plan: &plan,
            safety: &Safety::headless_test(),
        },
        &Cancel::new(),
        &mut |progress| events.push(progress.directory_dacl_report.clone()),
    )
    .unwrap_err();
    assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
    let report = crate::error::directory_dacl_report(&error);
    assert_eq!(report.completed.len(), 1);
    assert_eq!(report.uncertain.len(), 1);
    assert!(report.completed[0].directory.ends_with("/a"));
    assert!(report.uncertain[0].directory.ends_with("/b"));
    assert!(events.iter().any(|event| event == &report));
    assert!(crate::error::recovery_paths(&error)
        .iter()
        .any(|path| path.ends_with("/b")));
}

#[test]
fn directory_exact_is_rejected_on_files_new_directories_and_strict_before_effects() {
    for class in ["file", "new_directory", "strict"] {
        let fixture = directory_fixture(&["folder"]);
        let mut plan = fixture.plan();
        let index = plan
            .actions
            .iter()
            .position(|action| matches!(action, Action::SetDirectoryMetadata { .. }))
            .unwrap();
        let Action::SetDirectoryMetadata {
            path,
            source_mtime,
            metadata,
            ..
        } = plan.actions[index].clone()
        else {
            unreachable!();
        };
        if class == "strict" {
            plan.metadata_report.profile = MetadataProfile::NativeStrict;
        } else if class == "file" {
            plan.actions[index] = Action::CopyFile {
                path,
                size: 0,
                source_mtime,
                source_mode: None,
                source_hash: None,
                metadata: Some(metadata),
            };
        } else {
            plan.actions[index] = Action::CreateDir {
                path,
                source_mtime,
                source_mode: None,
                metadata: Some(metadata),
            };
        }
        assert!(fixture.run(&plan).is_err(), "{class}");
        assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0, "{class}");
        assert_eq!(
            fixture.target.acl_attempts.load(Ordering::SeqCst),
            0,
            "{class}"
        );
    }
}

#[test]
fn inherited_before_directory_acl_is_omitted_and_children_still_copy() {
    let fixture = directory_fixture(&["folder"]);
    let target = fixture.target_root.path().join("folder");
    let mut security = fixture.target.security(&target);
    security.acl.control = 4;
    fixture.target.set_security(&target, security.clone());
    fs::write(fixture.source_root.path().join("folder/child"), b"child").unwrap();
    let plan = fixture.plan();
    let metadata = plan
        .actions
        .iter()
        .find_map(|action| match action {
            Action::SetDirectoryMetadata { metadata, .. } => Some(metadata),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        metadata.windows_dacl,
        Some(WindowsDaclTransfer::Omitted(
            WindowsDaclOmission::TargetDacl(_)
        ))
    ));
    fixture.run(&plan).unwrap();
    assert_eq!(fs::read(target.join("child")).unwrap(), b"child");
    assert_eq!(fixture.target.security(&target), security);
}

#[test]
fn unsupported_source_directory_acl_stays_visible_even_when_target_is_equal() {
    let fixture = directory_fixture(&["folder"]);
    for (vfs, root) in [
        (&fixture.source, fixture.source_root.path()),
        (&fixture.target, fixture.target_root.path()),
    ] {
        let path = root.join("folder");
        let mut security = vfs.security(&path);
        security.acl.control = 4;
        vfs.set_security(&path, security);
    }
    let plan = fixture.plan();
    assert!(plan.actions.is_empty());
    assert!(plan
        .metadata_report
        .issues
        .iter()
        .any(|issue| issue.path == "folder"
            && issue.class == MetadataClass::Acl
            && issue.reason
                == crate::windows_security::WindowsDaclWriteIssue::UnsupportedControl.reason()));
    fixture.run(&plan).unwrap();
    assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn mirror_child_deletions_finish_before_directory_acl() {
    let fixture = directory_fixture(&["folder"]);
    fs::write(fixture.target_root.path().join("folder/obsolete"), b"old").unwrap();
    let mut options = Fixture::options();
    options.mode = SyncMode::Mirror;
    let plan = plan_content_with_cancel(
        &fixture.source,
        fixture.source_root.path(),
        &fixture.target,
        fixture.target_root.path(),
        &options,
        &Cancel::new(),
        false,
    )
    .unwrap();
    let mut safety = Safety::headless_test();
    safety.max_delete_fraction = Some(1.0);
    let stats = execute(
        ExecuteRequest {
            src: &fixture.source,
            src_root: fixture.source_root.path(),
            dst: &fixture.target,
            dst_root: fixture.target_root.path(),
            plan: &plan,
            safety: &safety,
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.files_deleted, 1);
    assert!(!fixture.target_root.path().join("folder/obsolete").exists());
    assert!(fixture.target.directory_snapshots.lock().unwrap()[0]
        .1
        .is_empty());
    assert_eq!(stats.directory_dacl_report.completed.len(), 1);
}

#[test]
fn directory_acl_queue_persistence_cancel_retains_completed_effect_and_open_intent() {
    let fixture = directory_fixture(&["folder"]);
    let plan = fixture.plan();
    let cancel = Cancel::new();
    let stats = execute(
        ExecuteRequest {
            src: &fixture.source,
            src_root: fixture.source_root.path(),
            dst: &fixture.target,
            dst_root: fixture.target_root.path(),
            plan: &plan,
            safety: &Safety::headless_test(),
        },
        &cancel,
        &mut |progress| {
            if !progress.directory_dacl_report.completed.is_empty() {
                cancel.cancel();
            }
        },
    )
    .unwrap();
    assert!(stats.cancelled);
    assert_eq!(stats.metadata_updated, 1);
    assert_eq!(stats.directory_dacl_report.completed.len(), 1);
    assert!(stats.directory_dacl_report.uncertain.is_empty());
    assert_eq!(fixture.target.acl_mutations.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.target.directory_finishes.load(Ordering::SeqCst), 0);
    assert_eq!(
        stats.temp_recovery_paths,
        vec![fixture
            .target_root
            .path()
            .join("folder")
            .to_string_lossy()
            .into_owned()]
    );
}

#[test]
fn directory_acl_finish_failure_keeps_known_completed_effect_and_actual_recovery_path() {
    let mut fixture = directory_fixture(&["folder"]);
    fixture.target.directory_finish_failure = true;
    let error = fixture.run(&fixture.plan()).unwrap_err();
    let report = crate::error::directory_dacl_report(&error);
    assert_eq!(report.completed.len(), 1);
    assert!(report.uncertain.is_empty());
    assert_eq!(fixture.target.acl_mutations.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.target.directory_finishes.load(Ordering::SeqCst), 1);
    assert_eq!(
        crate::error::recovery_paths(&error),
        vec![fixture
            .target_root
            .path()
            .join("folder")
            .to_string_lossy()
            .into_owned()]
    );
}

#[test]
fn created_directory_plan_distinguishes_unfiltered_children_and_complete_empty_streams() {
    for case in ["empty", "excluded-child", "streams", "unobserved"] {
        let mut fixture = Fixture::new(None);
        fixture.target.created_dacl_ready = true;
        let removed = DaclVfs::key(&fixture.source_path()).unwrap();
        fs::remove_file(fixture.source_path()).unwrap();
        // Linux kann den Inode sofort für den Ordner wiederverwenden. Die
        // künstlichen Beobachtungen der gelöschten Datei gehören nicht dorthin.
        fixture.source.streams.lock().unwrap().remove(&removed);
        fixture.source.security.lock().unwrap().remove(&removed);
        let folder = fixture.source_root.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let mut options = Fixture::options();
        match case {
            "excluded-child" => {
                fs::write(folder.join("hidden"), b"still-content").unwrap();
                options.exclude = vec!["**/hidden".into()];
            }
            "streams" => fixture.source.seed_stream(&folder),
            "unobserved" => {}
            _ => {}
        }
        // Ein leerer Fixtureeintrag ist eine ausdrücklich beobachtete leere Menge;
        // ein fehlender Eintrag bleibt eine unbekannte Beobachtung.
        if case != "unobserved" && case != "streams" {
            fixture
                .source
                .streams
                .lock()
                .unwrap()
                .insert(DaclVfs::key(&folder).unwrap(), BTreeMap::new());
        }
        let plan = plan_content_with_cancel(
            &fixture.source,
            fixture.source_root.path(),
            &fixture.target,
            fixture.target_root.path(),
            &options,
            &Cancel::new(),
            false,
        )
        .unwrap();
        let metadata = plan
            .actions
            .iter()
            .find_map(|a| match a {
                Action::CreateDir { metadata, .. } => metadata.as_ref(),
                _ => None,
            })
            .unwrap();
        let effect = metadata.windows_dacl.as_ref().unwrap();
        match case {
            "empty" => assert!(matches!(
                effect,
                WindowsDaclTransfer::CreatedDirectoryExact(_)
            )),
            "excluded-child" => {
                let WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(selection) = effect
                else {
                    panic!("Ungefilterte Kinder müssen V8 statt Leerheit begründen: {effect:?}")
                };
                assert_eq!(selection.source_children.entries, 1);
                assert!(!plan
                    .actions
                    .iter()
                    .any(|action| matches!(action, Action::CopyFile { .. })));
            }
            "streams" => assert_eq!(
                effect,
                &WindowsDaclTransfer::Omitted(WindowsDaclOmission::CreatedDirectoryStreamsNonempty)
            ),
            "unobserved" => assert_eq!(
                effect,
                &WindowsDaclTransfer::Omitted(
                    WindowsDaclOmission::CreatedDirectoryStreamsUnobserved
                )
            ),
            _ => unreachable!(),
        }
    }
}

#[test]
fn created_directory_optional_observations_omit_but_known_drift_and_interrupt_stop() {
    use super::execute::{
        validate_created_empty_source, CreatedSourceFault, CREATED_SOURCE_CHECKS,
        CREATED_SOURCE_FAULT,
    };
    for stage in [1, 2, 3] {
        for fault in [
            CreatedSourceFault::ListingDenied,
            CreatedSourceFault::ListingIncomplete,
            CreatedSourceFault::StreamsDenied,
            CreatedSourceFault::KnownStream,
            CreatedSourceFault::Interrupted,
        ] {
            let mut fixture = Fixture::new(None);
            fixture.target.created_dacl_ready = true;
            fs::remove_file(fixture.source_path()).unwrap();
            let folder = fixture.source_root.path().join("folder");
            fs::create_dir(&folder).unwrap();
            fixture
                .source
                .streams
                .lock()
                .unwrap()
                .insert(DaclVfs::key(&folder).unwrap(), BTreeMap::new());
            let plan = fixture.plan();
            let (metadata, mtime) = plan
                .actions
                .iter()
                .find_map(|action| match action {
                    Action::CreateDir {
                        metadata: Some(m),
                        source_mtime,
                        ..
                    } => Some((m, *source_mtime)),
                    _ => None,
                })
                .unwrap();
            CREATED_SOURCE_CHECKS.with(|count| count.set(0));
            CREATED_SOURCE_FAULT.with(|value| value.set(Some((stage, fault))));
            for _ in 1..stage {
                assert!(
                    validate_created_empty_source(&fixture.source, &folder, mtime, metadata)
                        .unwrap()
                        .is_none()
                );
            }
            let result = validate_created_empty_source(&fixture.source, &folder, mtime, metadata);
            match fault {
                CreatedSourceFault::KnownStream => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::PlanDrift
                ),
                CreatedSourceFault::Interrupted => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::Interrupted
                ),
                _ => assert!(result.unwrap().is_some()),
            }
            assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
            CREATED_SOURCE_FAULT.with(|value| value.set(None));
            fs::write(folder.join("known-child"), b"child").unwrap();
            if let Some(mtime) = mtime {
                fixture.source.inner.set_mtime(&folder, mtime).unwrap();
            }
            assert_eq!(
                crate::error_code(
                    &validate_created_empty_source(&fixture.source, &folder, mtime, metadata)
                        .unwrap_err()
                ),
                ErrorCode::PlanDrift
            );
        }
    }
}

#[test]
fn created_directory_optional_plan_listing_does_not_replace_required_recursion() {
    for (fault, permanent) in [
        (ListingFault::Denied, false),
        (ListingFault::Incomplete, false),
        (ListingFault::Denied, true),
        (ListingFault::Incomplete, true),
        (ListingFault::Interrupted, false),
        (ListingFault::Drift, false),
    ] {
        let mut fixture = Fixture::new(None);
        fixture.target.created_dacl_ready = true;
        fs::remove_file(fixture.source_path()).unwrap();
        let folder = fixture.source_root.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(fixture.source_root.path().join("independent"), b"content").unwrap();
        fixture
            .source
            .streams
            .lock()
            .unwrap()
            .insert(DaclVfs::key(&folder).unwrap(), BTreeMap::new());
        *fixture.source.listing_fault.lock().unwrap() = Some((folder, fault, permanent));
        let result = plan_content_with_cancel(
            &fixture.source,
            fixture.source_root.path(),
            &fixture.target,
            fixture.target_root.path(),
            &Fixture::options(),
            &Cancel::new(),
            false,
        );
        if permanent || matches!(fault, ListingFault::Interrupted | ListingFault::Drift) {
            let error = result.unwrap_err();
            match fault {
                ListingFault::Interrupted => {
                    assert_eq!(crate::error_code(&error), ErrorCode::Interrupted)
                }
                ListingFault::Drift => assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift),
                ListingFault::Denied => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
                ListingFault::Incomplete => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidData)
                }
            }
            assert_eq!(
                fixture
                    .source
                    .faulty_listing_attempts
                    .load(Ordering::SeqCst),
                if permanent { 2 } else { 1 }
            );
        } else {
            let plan = result.unwrap();
            assert!(plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::CopyFile {path, ..} if path == "independent")));
            assert!(plan.actions.iter().any(|a| matches!(a, Action::CreateDir { metadata: Some(m), .. } if m.windows_dacl == Some(WindowsDaclTransfer::Omitted(WindowsDaclOmission::CreatedDirectoryListingUnobserved)))));
            assert_eq!(
                fixture
                    .source
                    .faulty_listing_attempts
                    .load(Ordering::SeqCst),
                1
            );
            assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn before_children_raw_names_and_types_drift_despite_same_directory_time() {
    use super::execute::validate_created_nonempty_source;
    for change in ["name", "type", "added", "empty"] {
        let mut fixture = Fixture::new(None);
        fixture.target.created_dacl_ready = true;
        fs::remove_file(fixture.source_path()).unwrap();
        let folder = fixture.source_root.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("child"), b"same-content").unwrap();
        fixture
            .source
            .streams
            .lock()
            .unwrap()
            .insert(DaclVfs::key(&folder).unwrap(), BTreeMap::new());
        let plan = fixture.plan();
        let (metadata, mtime) = plan
            .actions
            .iter()
            .find_map(|action| match action {
                Action::CreateDir {
                    metadata: Some(metadata),
                    source_mtime,
                    ..
                } => Some((metadata, *source_mtime)),
                _ => None,
            })
            .unwrap();
        let Some(WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(selection)) =
            &metadata.windows_dacl
        else {
            panic!()
        };
        assert!(validate_created_nonempty_source(
            &fixture.source,
            &folder,
            mtime,
            metadata,
            &selection.source_children
        )
        .unwrap()
        .is_none());
        match change {
            "name" => fs::rename(folder.join("child"), folder.join("renamed")).unwrap(),
            "type" => {
                fs::remove_file(folder.join("child")).unwrap();
                fs::create_dir(folder.join("child")).unwrap();
            }
            "added" => fs::write(folder.join("other"), b"new").unwrap(),
            "empty" => fs::remove_file(folder.join("child")).unwrap(),
            _ => unreachable!(),
        }
        fixture
            .source
            .inner
            .set_mtime(&folder, mtime.unwrap())
            .unwrap();
        assert_eq!(fixture.source.stat(&folder).unwrap().mtime, mtime);
        let error = validate_created_nonempty_source(
            &fixture.source,
            &folder,
            mtime,
            metadata,
            &selection.source_children,
        )
        .unwrap_err();
        assert_eq!(
            crate::error_code(&error),
            ErrorCode::PlanDrift,
            "{change}: {error}"
        );
        assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn before_children_optional_observations_keep_interrupt_and_known_stream_drift() {
    use super::execute::{
        validate_created_nonempty_source, CreatedSourceFault, CREATED_SOURCE_CHECKS,
        CREATED_SOURCE_FAULT,
    };
    for stage in [1, 2] {
        for fault in [
            CreatedSourceFault::ListingDenied,
            CreatedSourceFault::ListingIncomplete,
            CreatedSourceFault::StreamsDenied,
            CreatedSourceFault::KnownStream,
            CreatedSourceFault::Interrupted,
        ] {
            let mut fixture = Fixture::new(None);
            fixture.target.created_dacl_ready = true;
            fs::remove_file(fixture.source_path()).unwrap();
            let folder = fixture.source_root.path().join("folder");
            fs::create_dir(&folder).unwrap();
            fs::write(folder.join("child"), b"content").unwrap();
            fixture
                .source
                .streams
                .lock()
                .unwrap()
                .insert(DaclVfs::key(&folder).unwrap(), BTreeMap::new());
            let plan = fixture.plan();
            let (metadata, mtime) = plan
                .actions
                .iter()
                .find_map(|action| match action {
                    Action::CreateDir {
                        metadata: Some(metadata),
                        source_mtime,
                        ..
                    } => Some((metadata, *source_mtime)),
                    _ => None,
                })
                .unwrap();
            let Some(WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(selection)) =
                &metadata.windows_dacl
            else {
                panic!()
            };
            CREATED_SOURCE_CHECKS.with(|count| count.set(0));
            CREATED_SOURCE_FAULT.with(|value| value.set(Some((stage, fault))));
            for _ in 1..stage {
                assert!(validate_created_nonempty_source(
                    &fixture.source,
                    &folder,
                    mtime,
                    metadata,
                    &selection.source_children
                )
                .unwrap()
                .is_none());
            }
            let result = validate_created_nonempty_source(
                &fixture.source,
                &folder,
                mtime,
                metadata,
                &selection.source_children,
            );
            match fault {
                CreatedSourceFault::KnownStream => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::PlanDrift
                ),
                CreatedSourceFault::Interrupted => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::Interrupted
                ),
                _ => assert!(result.unwrap().is_some()),
            }
            CREATED_SOURCE_FAULT.with(|value| value.set(None));
            assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
        }
    }
}
