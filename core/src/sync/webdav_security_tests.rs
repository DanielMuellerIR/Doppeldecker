//! Opake DAV-Sicherheit bleibt sichtbar und gebunden, während Inhalte wirklich fließen.
use super::*;
use crate::metadata::{AclModel, Capability, MetadataCapabilities};
use crate::vfs::{CompleteDirectoryListing, LocalVfs};
use crate::webdav_security::{WebDavPropertyObservation as Property, WebDavSecurityObservation};
use crate::{Cancel, ErrorCode};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

fn observed(value: u8, empty: bool) -> WebDavSecurityObservation {
    WebDavSecurityObservation {
        namespace: [value; 32],
        query_status: 207,
        resource_status: None,
        acl: Property::Observed {
            empty,
            digest: [value; 32],
        },
        owner: Property::Observed {
            empty,
            digest: [value.wrapping_add(1); 32],
        },
    }
}

struct DavVfs {
    inner: LocalVfs,
    observation: WebDavSecurityObservation,
    overrides: Mutex<BTreeMap<PathBuf, WebDavSecurityObservation>>,
    writes: AtomicUsize,
}
impl DavVfs {
    fn new(observation: WebDavSecurityObservation) -> Self {
        Self {
            inner: LocalVfs::new(),
            observation,
            overrides: Mutex::default(),
            writes: AtomicUsize::new(0),
        }
    }
    fn change(&self, path: &Path, observation: WebDavSecurityObservation) {
        self.overrides
            .lock()
            .unwrap()
            .insert(path.into(), observation);
    }
    fn effect(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }
}
impl Vfs for DavVfs {
    fn supports_mode_write(&self) -> bool {
        false
    }
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.inner.list(path)
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        self.inner.list_complete(path)
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.inner.stat(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.effect();
        self.inner.create_dir_all(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.effect();
        self.inner.create_dir(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.inner.open_read(path)
    }
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.effect();
        self.inner.open_write(path)
    }
    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.effect();
        self.inner.open_write_new(path)
    }
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn set_mtime(&self, path: &Path, value: i64) -> io::Result<()> {
        self.effect();
        self.inner.set_mtime(path, value)
    }
    fn set_mode(&self, path: &Path, value: u32) -> io::Result<()> {
        self.effect();
        self.inner.set_mode(path, value)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.effect();
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.effect();
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.effect();
        self.inner.rename(from, to)
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.effect();
        self.inner.rename_noreplace(from, to)
    }
    fn metadata_capabilities(&self, _: &Path) -> io::Result<MetadataCapabilities> {
        Ok(MetadataCapabilities {
            owner: Capability::Unsupported,
            special_mode_bits: Capability::Unsupported,
            acl_model: AclModel::Unsupported,
            acl: Capability::Unsupported,
            xattrs: Capability::Unsupported,
            resource_fork: Capability::Unsupported,
            named_streams: Capability::Unsupported,
            sparse_extents: Capability::Unsupported,
            hardlinks: Capability::Unsupported,
        })
    }
    fn metadata_manifest(&self, path: &Path) -> io::Result<MetadataManifest> {
        self.inner.stat(path)?;
        MetadataManifest::new(None, 0, None, vec![], None, None, None)?.with_webdav_security(Some(
            self.overrides
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .unwrap_or_else(|| self.observation.clone()),
        ))
    }
}

struct Fixture {
    source_root: tempfile::TempDir,
    target_root: tempfile::TempDir,
    source: DavVfs,
    target: DavVfs,
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
        Self {
            source_root,
            target_root,
            source: DavVfs::new(observed(1, false)),
            target: DavVfs::new(observed(2, false)),
        }
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

fn issues<'a>(
    report: &'a MetadataReport,
    side: &str,
    class: MetadataClass,
) -> Vec<&'a MetadataIssue> {
    report
        .issues
        .iter()
        .filter(|issue| issue.path == "a" && issue.class == class && issue.reason.starts_with(side))
        .collect()
}

fn assert_omissions(report: &MetadataReport, target: bool) {
    for side in if target {
        vec!["Quelle: ", "Ziel: "]
    } else {
        vec!["Quelle: "]
    } {
        for class in [MetadataClass::Acl, MetadataClass::Owner] {
            assert_eq!(issues(report, side, class).len(), 1, "{side} {class:?}");
        }
    }
    assert_eq!(
        report
            .issues
            .iter()
            .filter(|issue| {
                issue.path == "a"
                    && matches!(issue.class, MetadataClass::Acl | MetadataClass::Owner)
            })
            .count(),
        if target { 4 } else { 2 }
    );
}

#[test]
fn new_and_updated_content_transfers_with_endpoint_specific_webdav_security_omissions() {
    for existing in [None, Some(b"old-content".as_slice())] {
        let fixture = Fixture::new(existing);
        let plan = fixture.plan();
        assert_eq!(plan.copy_count, u64::from(existing.is_none()));
        assert_eq!(plan.update_count, u64::from(existing.is_some()));
        assert_omissions(&plan.metadata_report, existing.is_some());
        let transfer = plan
            .actions
            .iter()
            .find_map(|action| match action {
                Action::CopyFile { metadata, .. } | Action::UpdateFile { metadata, .. } => {
                    metadata.as_ref()
                }
                _ => None,
            })
            .unwrap();
        assert!(!transfer.acl);
        assert!(!transfer.owner);
        assert!(transfer.source.acl.is_none());
        assert!(transfer.source.owner.is_none());
        let stats = fixture.run(&plan).unwrap();
        assert_eq!(
            fs::read(fixture.target_root.path().join("a")).unwrap(),
            b"new-content"
        );
        assert!(fixture.target.writes.load(Ordering::SeqCst) > 0);
        assert_eq!(fixture.source.writes.load(Ordering::SeqCst), 0);
        assert_omissions(&stats.metadata_report, existing.is_some());
    }
}

#[test]
fn noop_keeps_http_resource_and_property_omissions_distinct_without_dummy_copy() {
    let missing = Property::Missing;
    let mut cases = Vec::new();
    for status in [403, 404] {
        cases.push((
            WebDavSecurityObservation {
                namespace: [3; 32],
                query_status: status,
                resource_status: None,
                acl: missing.clone(),
                owner: missing.clone(),
            },
            format!("HTTP {status}"),
            status == 403,
        ));
        cases.push((
            WebDavSecurityObservation {
                namespace: [3; 32],
                query_status: 207,
                resource_status: Some(status),
                acl: missing.clone(),
                owner: missing.clone(),
            },
            format!("Ressourcenstatus {status}"),
            status == 403,
        ));
        cases.push((
            WebDavSecurityObservation {
                namespace: [3; 32],
                query_status: 207,
                resource_status: None,
                acl: Property::Unavailable { status },
                owner: Property::Unavailable { status },
            },
            format!("Property-Status {status}"),
            status == 403,
        ));
    }
    cases.push((
        WebDavSecurityObservation {
            namespace: [3; 32],
            query_status: 207,
            resource_status: None,
            acl: missing.clone(),
            owner: missing,
        },
        "nicht zurückgegeben".into(),
        false,
    ));
    cases.push((observed(3, true), "ausdrücklich leer".into(), false));
    cases.push((observed(3, false), "opaker Wert".into(), false));
    for (observation, detail, denied) in cases {
        let mut fixture = Fixture::new(Some(b"new-content"));
        fixture.source.observation = observation.clone();
        fixture.target.observation = observation;
        fixture.target.observation.namespace = [4; 32];
        for property in [
            &mut fixture.target.observation.acl,
            &mut fixture.target.observation.owner,
        ] {
            if let Property::Observed { digest, .. } = property {
                *digest = [8; 32];
            }
        }
        let plan = fixture.plan();
        assert!(plan.actions.is_empty(), "{detail}");
        assert_eq!(plan.copy_count + plan.update_count + plan.metadata_count, 0);
        assert_eq!(plan.metadata_observations.len(), 1);
        assert_omissions(&plan.metadata_report, true);
        for side in ["Quelle: ", "Ziel: "] {
            for class in [MetadataClass::Acl, MetadataClass::Owner] {
                let issue = issues(&plan.metadata_report, side, class)[0];
                assert!(issue.reason.contains(&detail), "{}", issue.reason);
                assert_eq!(
                    issue.kind,
                    if denied {
                        MetadataIssueKind::PrivilegeFailure
                    } else {
                        MetadataIssueKind::Unsupported
                    }
                );
            }
        }
        fixture.run(&plan).unwrap();
        assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0, "{detail}");
        assert_eq!(fixture.source.writes.load(Ordering::SeqCst), 0, "{detail}");
        assert_eq!(
            fs::read(fixture.target_root.path().join("a")).unwrap(),
            b"new-content"
        );
    }
}

#[test]
fn equal_successful_webdav_properties_remain_four_omissions_without_writes() {
    for empty in [false, true] {
        let mut fixture = Fixture::new(Some(b"new-content"));
        fixture.source.observation = observed(1, empty);
        fixture.target.observation = fixture.source.observation.clone();
        let plan = fixture.plan();
        assert!(plan.actions.is_empty());
        assert_omissions(&plan.metadata_report, true);
        let stats = fixture.run(&plan).unwrap();
        assert_omissions(&stats.metadata_report, true);
        for class in [MetadataClass::Acl, MetadataClass::Owner] {
            let stats = stats
                .metadata_report
                .classes
                .iter()
                .find(|item| item.class == class)
                .unwrap();
            assert_eq!(stats.transferred, 0);
            assert_eq!(stats.omitted, 2);
        }
        assert_eq!(fixture.source.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn noop_source_or_target_security_drift_stops_before_sibling_write_or_empty_plan_success() {
    for source in [true, false] {
        for sibling in [true, false] {
            let fixture = Fixture::new(Some(b"new-content"));
            if sibling {
                fs::write(fixture.source_root.path().join("0-first"), b"sibling").unwrap();
            }
            let plan = fixture.plan();
            assert_eq!(plan.copy_count, u64::from(sibling));
            assert_eq!(plan.metadata_observations.len(), 1);
            let (vfs, root) = if source {
                (&fixture.source, fixture.source_root.path())
            } else {
                (&fixture.target, fixture.target_root.path())
            };
            let mut changed = vfs.observation.clone();
            changed.owner = Property::Unavailable { status: 403 };
            vfs.change(&root.join("a"), changed);
            let error = fixture.run(&plan).unwrap_err();
            assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
            assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.source.writes.load(Ordering::SeqCst), 0);
            assert!(!fixture.target_root.path().join("0-first").exists());
        }
    }
}

#[test]
fn action_bound_security_drift_stops_before_any_sibling_effect() {
    for existing in [None, Some(b"old-content".as_slice())] {
        for source in [true, false] {
            if existing.is_none() && !source {
                continue;
            }
            let fixture = Fixture::new(existing);
            fs::write(fixture.source_root.path().join("0-first"), b"sibling").unwrap();
            let plan = fixture.plan();
            assert_eq!(plan.copy_count + plan.update_count, 2);
            let (vfs, root) = if source {
                (&fixture.source, fixture.source_root.path())
            } else {
                (&fixture.target, fixture.target_root.path())
            };
            let mut changed = vfs.observation.clone();
            changed.acl = Property::Observed {
                empty: true,
                digest: [9; 32],
            };
            vfs.change(&root.join("a"), changed);
            let error = fixture.run(&plan).unwrap_err();
            assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
            assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.source.writes.load(Ordering::SeqCst), 0);
            assert!(!fixture.target_root.path().join("0-first").exists());
            assert_eq!(
                fs::read(fixture.target_root.path().join("a"))
                    .ok()
                    .as_deref(),
                existing
            );
        }
    }
}

#[test]
fn successful_opaque_webdav_properties_never_grant_native_strict_transfer() {
    let mut fixture = Fixture::new(Some(b"new-content"));
    fixture.source.observation = observed(1, true);
    fixture.target.observation = observed(1, true);
    let mut options = Fixture::options();
    options.metadata = MetadataProfile::NativeStrict;
    assert!(plan_content_with_cancel(
        &fixture.source,
        fixture.source_root.path(),
        &fixture.target,
        fixture.target_root.path(),
        &options,
        &Cancel::new(),
        false,
    )
    .is_err());
    assert_eq!(fixture.source.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.target.writes.load(Ordering::SeqCst), 0);
}
