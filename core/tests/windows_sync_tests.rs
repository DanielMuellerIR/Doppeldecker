//! Der öffentliche Planer bilanziert lesbare und ungeprüfte Windows-Metadaten,
//! ohne daraus eine Schreibfähigkeit oder einen unnötigen Basistausch abzuleiten.

use doppeldecker_core::metadata::{
    Acl, AclModel, Capability, MetadataCapabilities, MetadataManifest, MetadataProfile, PosixAcl,
    PosixAclEntry, PosixAclTag, WindowsSecurityObservation, WindowsSecurityReadIssue,
};
use doppeldecker_core::windows_security::{WindowsAce, WindowsDacl, WindowsDaclState, WindowsSid};
use doppeldecker_core::{
    plan, Action, Compare, Entry, LocalVfs, MetadataClass, MetadataIssueKind, SyncMode,
    SyncOptions, SyncPlan, Vfs,
};
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

const NAMESPACE: [u8; 16] = [0x21; 16];

fn sid(authority: u64, sub_authorities: &[u32]) -> WindowsSid {
    WindowsSid {
        revision: 1,
        authority,
        sub_authorities: sub_authorities.to_vec(),
    }
}

fn security(mask: u32) -> MetadataManifest {
    MetadataManifest::new(
        None,
        0o640,
        Some(Acl::WindowsDacl(WindowsDacl {
            namespace: NAMESPACE,
            control: 4 | 0x1000,
            state: WindowsDaclState::Acl {
                revision: 2,
                aces: vec![WindowsAce::Allow {
                    flags: 0,
                    mask,
                    sid: sid(5, &[21, 1, 2, 3, 1000]),
                }],
            },
        })),
        vec![],
        None,
        None,
        None,
    )
    .unwrap()
    .with_windows_security(Some(WindowsSecurityObservation {
        namespace: NAMESPACE,
        owner: Some(sid(5, &[21, 1, 2, 3, 1000])),
        group: Some(sid(22, &[2, 100])),
        issue: None,
        invalid_descriptor_digest: None,
    }))
    .unwrap()
}

fn unreadable() -> MetadataManifest {
    MetadataManifest::new(None, 0o640, None, vec![], None, None, None)
        .unwrap()
        .with_windows_security(Some(WindowsSecurityObservation {
            namespace: NAMESPACE,
            owner: None,
            group: None,
            issue: Some(WindowsSecurityReadIssue::ReadControlDenied),
            invalid_descriptor_digest: None,
        }))
        .unwrap()
}

struct WindowsVfs {
    local: LocalVfs,
    manifest: MetadataManifest,
}

impl WindowsVfs {
    fn new(manifest: MetadataManifest) -> Self {
        Self {
            local: LocalVfs::new(),
            manifest,
        }
    }
}

fn mutation<T>() -> io::Result<T> {
    panic!("Die lesende Planung darf keine Mutation am Testadapter auslösen")
}

impl Vfs for WindowsVfs {
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.local.list(path)
    }
    fn list_complete(
        &self,
        path: &Path,
    ) -> io::Result<doppeldecker_core::vfs::CompleteDirectoryListing> {
        self.local.list_complete(path)
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.local.stat(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.local.exists(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.local.open_read(path)
    }
    fn supports_mode_write(&self) -> bool {
        false
    }
    fn supports_mtime_write(&self) -> bool {
        false
    }
    fn metadata_capabilities(&self, _: &Path) -> io::Result<MetadataCapabilities> {
        Ok(MetadataCapabilities {
            owner: Capability::ReadOnly,
            special_mode_bits: Capability::Unsupported,
            acl_model: match self.manifest.acl.as_ref() {
                Some(Acl::Posix(_)) => AclModel::Posix,
                Some(Acl::Nfs4(_)) => AclModel::Nfs4,
                Some(Acl::WindowsDacl(_)) => AclModel::WindowsSid,
                None if self.manifest.windows_security.is_some() => AclModel::WindowsSid,
                None => AclModel::Unsupported,
            },
            acl: Capability::ReadOnly,
            xattrs: Capability::Unsupported,
            resource_fork: Capability::Unsupported,
            named_streams: Capability::Unsupported,
            sparse_extents: Capability::Unsupported,
            hardlinks: Capability::Unsupported,
        })
    }
    fn metadata_manifest(&self, path: &Path) -> io::Result<MetadataManifest> {
        self.local.stat(path)?;
        self.manifest.validate()?;
        Ok(self.manifest.clone())
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        mutation()
    }
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        mutation()
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        mutation()
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        mutation()
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        mutation()
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        mutation()
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        mutation()
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        mutation()
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        mutation()
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        mutation()
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        mutation()
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        mutation()
    }
}

struct Fixture {
    source_root: tempfile::TempDir,
    target_root: tempfile::TempDir,
    source: WindowsVfs,
    target: WindowsVfs,
}

impl Fixture {
    fn new(
        source: MetadataManifest,
        target: MetadataManifest,
        target_bytes: Option<&[u8]>,
        directory: bool,
    ) -> Self {
        let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
        let source_root = tempfile::tempdir_in(&temporary_root).unwrap();
        let target_root = tempfile::tempdir_in(temporary_root).unwrap();
        if directory {
            fs::create_dir(source_root.path().join("entry")).unwrap();
            fs::create_dir(target_root.path().join("entry")).unwrap();
        } else {
            fs::write(source_root.path().join("entry"), b"source content").unwrap();
            if let Some(bytes) = target_bytes {
                fs::write(target_root.path().join("entry"), bytes).unwrap();
            }
        }
        Self {
            source_root,
            target_root,
            source: WindowsVfs::new(source),
            target: WindowsVfs::new(target),
        }
    }

    fn preview(&self, profile: MetadataProfile) -> io::Result<SyncPlan> {
        plan(
            &self.source,
            self.source_root.path(),
            &self.target,
            self.target_root.path(),
            &SyncOptions {
                mode: SyncMode::Update,
                compare: Compare::ContentHash,
                metadata: profile,
                ..Default::default()
            },
        )
    }

    fn best_effort(&self) -> SyncPlan {
        self.preview(MetadataProfile::NativeBestEffort).unwrap()
    }
}

fn issue_count(plan: &SyncPlan, class: MetadataClass, kind: MetadataIssueKind) -> usize {
    plan.metadata_report
        .issues
        .iter()
        .filter(|issue| issue.path == "entry" && issue.class == class && issue.kind == kind)
        .count()
}

fn assert_no_content_or_metadata_writes(plan: &SyncPlan) {
    assert!(plan.actions.is_empty(), "actions: {:?}", plan.actions);
    assert_eq!(plan.copy_count, 0);
    assert_eq!(plan.update_count, 0);
    assert_eq!(plan.metadata_count, 0);
    assert_eq!(plan.bytes_to_transfer, 0);
    assert_eq!(plan.metadata_report.content_changes, 0);
    assert_eq!(plan.metadata_report.metadata_changes, 0);
}

#[test]
fn equal_file_content_with_different_dacl_reports_omission_without_copy() {
    let fixture = Fixture::new(
        security(0x12019f),
        security(0x120089),
        Some(b"source content"),
        false,
    );
    let preview = fixture.best_effort();
    assert_no_content_or_metadata_writes(&preview);
    assert_eq!(
        issue_count(&preview, MetadataClass::Acl, MetadataIssueKind::Unsupported),
        1
    );
    assert_eq!(
        issue_count(
            &preview,
            MetadataClass::Owner,
            MetadataIssueKind::Unsupported
        ),
        0
    );
    assert!(preview
        .metadata_report
        .classes
        .iter()
        .any(|class| class.class == MetadataClass::Acl
            && class.omitted == 1
            && class.transferred == 0));
    assert_eq!(
        fs::read(fixture.target_root.path().join("entry")).unwrap(),
        b"source content"
    );
}

#[test]
fn equal_windows_security_and_content_is_noop_without_acl_or_owner_issue() {
    let manifest = security(0x12019f);
    let fixture = Fixture::new(manifest.clone(), manifest, Some(b"source content"), false);
    let preview = fixture.best_effort();
    assert_no_content_or_metadata_writes(&preview);
    assert!(
        preview.metadata_report.issues.is_empty(),
        "{:?}",
        preview.metadata_report.issues
    );
    assert_eq!(preview.metadata_report.unsupported_metadata, 0);
    assert_eq!(preview.metadata_report.privilege_failures, 0);
}

#[test]
fn denied_read_control_keeps_readable_equal_content_and_reports_two_privilege_failures() {
    let fixture = Fixture::new(
        unreadable(),
        security(0x12019f),
        Some(b"source content"),
        false,
    );
    let preview = fixture.best_effort();
    assert_no_content_or_metadata_writes(&preview);
    assert_eq!(
        issue_count(
            &preview,
            MetadataClass::Acl,
            MetadataIssueKind::PrivilegeFailure
        ),
        1
    );
    assert_eq!(
        issue_count(
            &preview,
            MetadataClass::Owner,
            MetadataIssueKind::PrivilegeFailure
        ),
        1
    );
    assert_eq!(preview.metadata_report.privilege_failures, 2);
    assert_eq!(preview.metadata_report.issues.len(), 2);
}

#[test]
fn new_file_reports_dacl_and_sid_owner_omissions_without_claiming_write_capability() {
    let manifest = security(0x12019f);
    let fixture = Fixture::new(manifest.clone(), manifest, None, false);
    let preview = fixture.best_effort();
    assert_eq!(preview.copy_count, 1);
    assert_eq!(preview.update_count, 0);
    assert_eq!(preview.metadata_report.content_changes, 1);
    assert_eq!(
        issue_count(&preview, MetadataClass::Acl, MetadataIssueKind::Unsupported),
        1
    );
    assert_eq!(
        issue_count(
            &preview,
            MetadataClass::Owner,
            MetadataIssueKind::Unsupported
        ),
        1
    );
    assert!(preview.actions.iter().any(|action| matches!(action, Action::CopyFile { metadata: Some(metadata), .. } if !metadata.acl && !metadata.owner)));
    assert!(!fixture.target_root.path().join("entry").exists());
}

#[test]
fn content_replacement_reports_dacl_and_sid_owner_omissions_even_for_equal_old_security() {
    let manifest = security(0x12019f);
    let fixture = Fixture::new(
        manifest.clone(),
        manifest,
        Some(b"old target content"),
        false,
    );
    let preview = fixture.best_effort();
    assert_eq!(preview.copy_count, 0);
    assert_eq!(preview.update_count, 1);
    assert_eq!(preview.metadata_report.content_changes, 1);
    assert_eq!(
        issue_count(&preview, MetadataClass::Acl, MetadataIssueKind::Unsupported),
        1
    );
    assert_eq!(
        issue_count(
            &preview,
            MetadataClass::Owner,
            MetadataIssueKind::Unsupported
        ),
        1
    );
    assert!(preview.actions.iter().any(|action| matches!(action, Action::UpdateFile { metadata_only: false, metadata: Some(metadata), .. } if !metadata.acl && !metadata.owner)));
    assert_eq!(
        fs::read(fixture.target_root.path().join("entry")).unwrap(),
        b"old target content"
    );
}

#[test]
fn unchanged_directory_reports_dacl_difference_without_creating_metadata_action() {
    let fixture = Fixture::new(security(0x12019f), security(0x120089), None, true);
    let preview = fixture.best_effort();
    assert_no_content_or_metadata_writes(&preview);
    assert_eq!(
        issue_count(&preview, MetadataClass::Acl, MetadataIssueKind::Unsupported),
        1
    );
    assert_eq!(
        issue_count(
            &preview,
            MetadataClass::Owner,
            MetadataIssueKind::Unsupported
        ),
        0
    );
    assert_eq!(preview.metadata_report.unsupported_metadata, 1);
}

#[test]
fn native_strict_rejects_windows_sid_roots_before_claiming_verified_transfer() {
    let manifest = security(0x12019f);
    let fixture = Fixture::new(manifest.clone(), manifest, Some(b"source content"), false);
    let error = fixture.preview(MetadataProfile::NativeStrict).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    let control = Fixture::new(
        local_security(false),
        local_security(false),
        Some(b"source content"),
        false,
    );
    assert!(
        control
            .preview(MetadataProfile::NativeStrict)
            .unwrap()
            .metadata_report
            .strict_verified
    );
    assert_eq!(
        fs::read(fixture.target_root.path().join("entry")).unwrap(),
        b"source content"
    );
}

fn local_security(posix: bool) -> MetadataManifest {
    let acl = posix.then(|| {
        Acl::Posix(PosixAcl {
            access: [
                PosixAclTag::UserObject,
                PosixAclTag::GroupObject,
                PosixAclTag::Other,
            ]
            .into_iter()
            .map(|tag| PosixAclEntry {
                tag,
                id: None,
                permissions: 4,
            })
            .collect(),
            default: vec![],
        })
    });
    MetadataManifest::new(None, 0o640, acl, vec![], None, None, None).unwrap()
}

#[test]
fn local_source_noop_reports_target_read_control_denial_for_absent_and_posix_acl() {
    for posix in [false, true] {
        let fixture = Fixture::new(
            local_security(posix),
            unreadable(),
            Some(b"source content"),
            false,
        );
        let preview = fixture.best_effort();
        assert_no_content_or_metadata_writes(&preview);
        assert_eq!(
            issue_count(
                &preview,
                MetadataClass::Acl,
                MetadataIssueKind::PrivilegeFailure
            ),
            1
        );
        assert_eq!(preview.metadata_report.privilege_failures, 1);
        assert_eq!(
            issue_count(&preview, MetadataClass::Acl, MetadataIssueKind::Unsupported),
            0
        );
        assert_eq!(
            issue_count(
                &preview,
                MetadataClass::Acl,
                MetadataIssueKind::IncompatibleAclModel
            ),
            usize::from(posix)
        );
        assert_eq!(preview.metadata_report.issues.len(), 1 + usize::from(posix));
        assert!(preview
            .metadata_report
            .classes
            .iter()
            .any(|class| class.class == MetadataClass::Acl
                && class.omitted == 1 + u64::from(posix)
                && class.transferred == 0));
    }
}

#[test]
fn local_source_content_replacement_reports_target_denial_separately_from_source_acl() {
    for posix in [false, true] {
        let fixture = Fixture::new(
            local_security(posix),
            unreadable(),
            Some(b"old target content"),
            false,
        );
        let preview = fixture.best_effort();
        assert_eq!(preview.update_count, 1);
        assert_eq!(preview.metadata_report.content_changes, 1);
        assert_eq!(
            issue_count(
                &preview,
                MetadataClass::Acl,
                MetadataIssueKind::PrivilegeFailure
            ),
            1
        );
        assert_eq!(preview.metadata_report.privilege_failures, 1);
        assert_eq!(
            issue_count(&preview, MetadataClass::Acl, MetadataIssueKind::Unsupported),
            0
        );
        assert_eq!(
            issue_count(
                &preview,
                MetadataClass::Acl,
                MetadataIssueKind::IncompatibleAclModel
            ),
            usize::from(posix)
        );
        assert!(preview
            .metadata_report
            .classes
            .iter()
            .any(|class| class.class == MetadataClass::Acl
                && class.omitted == 1 + u64::from(posix)
                && class.transferred == 0));
        assert_eq!(
            fs::read(fixture.target_root.path().join("entry")).unwrap(),
            b"old target content"
        );
    }
}
