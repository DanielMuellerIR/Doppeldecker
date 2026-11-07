use super::*;
use crate::windows_security::{WindowsAce, WindowsDaclState, WindowsSid};
use crate::{Entry, Vfs};
use std::io::{Read, Write};
use std::path::Path;

pub(crate) fn context() -> WindowsDaclWriteContext {
    WindowsDaclWriteContext {
        namespace: [1; 16],
        owner: WindowsSid {
            revision: 1,
            authority: 5,
            sub_authorities: vec![21, 42],
        },
        group: Some(WindowsSid {
            revision: 1,
            authority: 5,
            sub_authorities: vec![21, 43],
        }),
    }
}
pub(crate) fn acl(after: bool) -> WindowsDacl {
    let context = context();
    WindowsDacl {
        namespace: context.namespace,
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![
                WindowsAce::Allow {
                    flags: 0,
                    mask: 0x00160081,
                    sid: context.owner,
                },
                WindowsAce::Allow {
                    flags: 0,
                    mask: if after { 0x81 } else { 0x80 },
                    sid: WindowsSid {
                        revision: 1,
                        authority: 1,
                        sub_authorities: vec![0],
                    },
                },
            ],
        },
    }
}
pub(crate) fn identity(root: &str) -> crate::RootIdentity {
    crate::RootIdentity::Smb(crate::remote::SmbRootIdentity {
        server: crate::remote::SmbServerIdentity {
            host: "server".into(),
            port: 445,
            server_guid: [1; 16],
            share: "share".into(),
            account_digest: [2; 32],
            canonical_root: "/vault".into(),
            dialect: 0x0311,
            signing_active: true,
            encryption_active: true,
            signing_algorithm: "AES-GMAC".into(),
            encryption_cipher: "AES-128-GCM".into(),
        },
        virtual_root: root.into(),
        root_file_id: 1,
        tree_id: 1,
        session_generation: 1,
    })
}
pub(crate) fn proof(name: &str, id: u64) -> crate::sync::SyncTargetProof {
    serde_json::from_value(serde_json::json!({"raw_name":name.as_bytes(),"provider_token":null,"is_dir":false,"is_file":true,"is_symlink":false,"size":4,"mtime":null,"mode":null,"device":null,"inode":id})).unwrap()
}
pub(crate) fn plan() -> FileDaclPlan {
    FileDaclPlan {
        before: acl(false),
        after: acl(true),
        context: context(),
        target_proof: proof("a", 42),
        file_id: 42,
        object_namespace: crate::file_dacl_registry::object_namespace(&identity("/pane")).unwrap(),
        content: BlobSummary::from_bytes(b"data"),
        streams: vec![NamedBlobSummary {
            name: b"ads".to_vec(),
            value: BlobSummary::from_bytes(b"extra"),
        }],
        alias_scope: FileDaclAliasScope {
            selected_paths: vec!["a".into()],
            observed_link_count: Some(1),
            all_existing_and_future_names_including_outside_root: true,
        },
    }
}
pub(crate) fn fixture() -> (
    tempfile::TempDir,
    crate::local_staging::StagingOwner,
    crate::local_fs::RootHandle,
) {
    use std::os::unix::fs::PermissionsExt;
    let area = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let path = area.path().canonicalize().unwrap();
    crate::LocalVfs::new().write_acl_exact(&path, None).unwrap();
    let owner = crate::local_staging::StagingOwner {
        queue: path.join("queue.json"),
        operation: "f".repeat(32),
    };
    let config = crate::local_fs::RootHandle::open_nofollow(&path).unwrap();
    (area, owner, config)
}
pub(crate) struct Readback {
    pub(crate) identity: crate::RootIdentity,
    pub(crate) observed: Option<FileDaclRecoveryObservation>,
}
impl Readback {
    pub(crate) fn new(after: bool) -> Self {
        Self {
            identity: identity("/pane"),
            observed: Some(FileDaclRecoveryObservation {
                file_id: 42,
                object_namespace: plan().object_namespace,
                context: context(),
                dacl: acl(after),
            }),
        }
    }
}
impl Vfs for Readback {
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        panic!("Keine Anlage")
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        panic!("Kein Datenwrite")
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        panic!("Kein Rename")
    }
    fn root_identity(&self, _: &Path) -> io::Result<crate::RootIdentity> {
        Ok(self.identity.clone())
    }
    fn created_directory_dacl_recovery_observation(
        &self,
        _: &Path,
        expected: &crate::RootIdentity,
    ) -> io::Result<crate::created_directory_dacl::CreatedDirectoryDaclRecoveryObservation> {
        assert_eq!(expected, &self.identity);
        let observed = self.observed.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::PermissionDenied, "synthetisch unlesbar")
        })?;
        Ok(
            crate::created_directory_dacl::CreatedDirectoryDaclRecoveryObservation {
                file_id: observed.file_id,
                object_namespace: crate::created_directory_dacl_registry::object_namespace(
                    expected,
                )?,
                context: observed.context.clone(),
                dacl: observed.dacl.clone(),
            },
        )
    }
    fn file_dacl_recovery_observation(
        &self,
        _: &Path,
        expected: &crate::RootIdentity,
    ) -> io::Result<FileDaclRecoveryObservation> {
        assert_eq!(expected, &self.identity);
        self.observed
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "synthetisch unlesbar"))
    }
    fn stat(&self, _: &Path) -> io::Result<Entry> {
        panic!("ID und Security müssen aus demselben gehaltenen Handle stammen")
    }
    fn list(&self, _: &Path) -> io::Result<Vec<Entry>> {
        panic!("Recovery darf nicht scannen")
    }
    fn exists(&self, _: &Path) -> bool {
        panic!("Recovery darf Fehler nicht in Abwesenheit verwandeln")
    }
    fn open_read(&self, _: &Path) -> io::Result<Box<dyn Read + Send>> {
        panic!("Recovery liest nur Identität und Security")
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        panic!("Kein Datenwrite")
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        panic!("Keine Anlage")
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Löschen")
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Löschen")
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        panic!("Kein Rename")
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        panic!("Keine Zeitänderung")
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        panic!("Kein Moduswrite")
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Dateischreiben")
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Ordnerschreiben")
    }
}
#[test]
fn file_dacl_scope_and_protected_revision_are_mandatory() {
    let mut value = plan();
    value.validate().unwrap();
    assert_eq!(value.content.size, 4);
    value
        .alias_scope
        .all_existing_and_future_names_including_outside_root = false;
    assert!(value.validate().is_err());
    value = plan();
    value.alias_scope.selected_paths = vec!["b".into(), "a".into()];
    assert!(value.validate().is_err());
    value = plan();
    value.alias_scope.selected_paths = vec!["../escape".into()];
    assert!(value.validate().is_err());
    value = plan();
    value.after.control = 4;
    assert!(value.validate().is_err());
    value = plan();
    let WindowsDaclState::Acl { revision, .. } = &mut value.after.state else {
        panic!()
    };
    *revision = 4;
    assert!(value.validate().is_err());
}
#[test]
fn file_dacl_digest_domain_is_separate_from_directory() {
    assert_ne!(
        dacl_digest(&acl(false)).unwrap(),
        crate::directory_dacl::dacl_digest(&acl(false)).unwrap()
    );
}
#[test]
fn file_dacl_report_keeps_confirmed_and_uncertain_objects_separate() {
    let effect = plan().outcome("/pane/a").unwrap();
    let mut report = FileDaclReport {
        completed: vec![],
        uncertain: vec![effect.clone()],
    };
    report.merge(&FileDaclReport {
        completed: vec![effect.clone()],
        uncertain: vec![],
    });
    assert_eq!(report.completed, vec![effect]);
    assert!(report.uncertain.is_empty());
    let error = crate::error::with_file_dacl_report(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            "offen",
        ),
        &report,
    );
    let wrapped = crate::error::with_recovery_context(
        io::Error::other("Abschluss"),
        &[&error],
        &[Path::new("/pane/a")],
    );
    assert_eq!(crate::error::file_dacl_report(&wrapped), report);
}

#[test]
fn file_dacl_v6_phase_durability_and_cancel_after_applied() {
    use crate::file_dacl_registry::{recover_queue, Intent, Phase};
    let (_area, owner, config) = fixture();
    let intent = Intent::create(
        &owner,
        Path::new("/pane/a"),
        &plan(),
        &identity("/pane"),
        [2; 32],
    )
    .unwrap();
    let initial = recover_queue(&owner.queue, &config).unwrap();
    assert!(initial[0].file_dacl_report.is_empty());
    assert!(initial[0].file_dacl_recovery);
    intent.phase(Phase::Applying).unwrap();
    assert_eq!(
        recover_queue(&owner.queue, &config).unwrap()[0]
            .file_dacl_report
            .uncertain
            .len(),
        1
    );
    intent.phase(Phase::Applied).unwrap();
    let report = intent.report();
    assert_eq!(report.completed.len(), 1);
    assert!(intent.phase(Phase::Applying).is_err());
    // Abbruch nach Applied und vor finish darf den belegten Ausgang nicht abbauen.
    drop(intent);
    let recovered = recover_queue(&owner.queue, &config).unwrap();
    assert!(recovered[0].file_dacl_recovery);
    assert_eq!(recovered[0].file_dacl_report, report);
    let observed = Readback::new(true);
    let ack = crate::file_dacl_registry::acknowledge(
        &owner,
        &observed,
        Path::new("/pane"),
        [2; 32],
        &config,
    )
    .unwrap();
    assert_eq!(ack.report, report);
    let closed = recover_queue(&owner.queue, &config).unwrap();
    assert!(closed[0].file_dacl_acknowledged);
    assert!(!closed[0].file_dacl_recovery);
    assert_eq!(closed[0].file_dacl_report, report);
    assert!(closed[0].paths.is_empty());
}
#[test]
fn file_dacl_recovery_accepts_only_known_before_after_states_and_is_read_only() {
    use crate::file_dacl_registry::{Intent, Phase};
    for (phase, after, accepted) in [
        (Phase::Prepared, false, true),
        (Phase::Prepared, true, false),
        (Phase::Applying, false, true),
        (Phase::Applying, true, true),
        (Phase::Applied, false, false),
        (Phase::Applied, true, true),
    ] {
        let (_area, owner, config) = fixture();
        let intent = Intent::create(
            &owner,
            Path::new("/pane/a"),
            &plan(),
            &identity("/pane"),
            [2; 32],
        )
        .unwrap();
        if phase != Phase::Prepared {
            intent.phase(Phase::Applying).unwrap();
        }
        if phase == Phase::Applied {
            intent.phase(Phase::Applied).unwrap();
        }
        drop(intent);
        let result = crate::file_dacl_registry::acknowledge(
            &owner,
            &Readback::new(after),
            Path::new("/pane"),
            [2; 32],
            &config,
        );
        assert_eq!(result.is_ok(), accepted, "{phase:?} after={after}");
        if accepted {
            assert_eq!(result.unwrap().report.completed.len(), usize::from(after));
        }
    }
}
#[test]
fn file_dacl_recovery_blocks_active_writer_identity_sid_and_unknown_security() {
    use crate::file_dacl_registry::{Intent, Phase};
    let (_area, owner, config) = fixture();
    let intent = Intent::create(
        &owner,
        Path::new("/pane/a"),
        &plan(),
        &identity("/pane"),
        [2; 32],
    )
    .unwrap();
    intent.phase(Phase::Applying).unwrap();
    assert!(crate::file_dacl_registry::acknowledge(
        &owner,
        &Readback::new(true),
        Path::new("/pane"),
        [2; 32],
        &config
    )
    .is_err());
    drop(intent);
    for case in [
        "fileid",
        "namespace",
        "owner",
        "group",
        "third",
        "unreadable",
    ] {
        let mut readback = Readback::new(true);
        let observed = readback.observed.as_mut().unwrap();
        match case {
            "fileid" => observed.file_id += 1,
            "namespace" => observed.object_namespace = [9; 32],
            "owner" => observed.context.owner.sub_authorities.push(99),
            "group" => observed.context.group = None,
            "third" => observed.dacl.control = 4,
            "unreadable" => readback.observed = None,
            _ => unreachable!(),
        }
        assert!(
            crate::file_dacl_registry::acknowledge(
                &owner,
                &readback,
                Path::new("/pane"),
                [2; 32],
                &config
            )
            .is_err(),
            "{case}"
        );
        assert!(crate::file_dacl_registry::has_open(&owner, &config).unwrap());
    }
}
#[test]
fn file_dacl_registry_blocks_same_share_outside_root_and_detects_closed_aba() {
    use crate::file_dacl_registry::{check_registry, Intent};
    let (_area, owner, config) = fixture();
    let outside = identity("/unrelated");
    assert!(
        check_registry(&config, std::slice::from_ref(&outside), None)
            .unwrap()
            .is_none()
    );
    let intent = Intent::create(
        &owner,
        Path::new("/pane/a"),
        &plan(),
        &identity("/pane"),
        [2; 32],
    )
    .unwrap();
    assert!(check_registry(&config, std::slice::from_ref(&outside), None).is_err());
    assert!(check_registry(&config, std::slice::from_ref(&outside), Some(&owner)).is_err());
    intent.finish(false).unwrap();
    let first = check_registry(&config, std::slice::from_ref(&outside), None)
        .unwrap()
        .unwrap();
    assert_eq!(
        check_registry(&config, std::slice::from_ref(&outside), Some(&owner)).unwrap(),
        None
    );
    let second = Intent::create(
        &owner,
        Path::new("/pane/a"),
        &plan(),
        &identity("/pane"),
        [2; 32],
    )
    .unwrap();
    second.finish(false).unwrap();
    let next = check_registry(&config, std::slice::from_ref(&outside), None)
        .unwrap()
        .unwrap();
    assert_ne!(first, next);
    let mut other = outside;
    if let crate::RootIdentity::Smb(root) = &mut other {
        root.server.share = "other".into();
    }
    assert_eq!(check_registry(&config, &[other], None).unwrap(), None);
}
#[test]
fn file_dacl_corrupt_or_missing_intent_never_becomes_empty_registry() {
    use crate::file_dacl_registry::Intent;
    let (_area, owner, config) = fixture();
    let intent = Intent::create(
        &owner,
        Path::new("/pane/a"),
        &plan(),
        &identity("/pane"),
        [2; 32],
    )
    .unwrap();
    let path = intent.path().to_owned();
    drop(intent);
    std::fs::remove_file(&path).unwrap();
    assert!(crate::file_dacl_registry::has_open(&owner, &config).unwrap());
    std::fs::write(
        config.path().join(".file-dacl-v6-ledger.json"),
        b"{truncated",
    )
    .unwrap();
    assert!(crate::file_dacl_registry::has_open(&owner, &config).is_err());
    assert!(
        crate::file_dacl_registry::check_registry(&config, &[identity("/pane")], None).is_err()
    );
}

struct PlannerVfs {
    root: std::path::PathBuf,
    acls: std::collections::BTreeMap<String, WindowsDacl>,
    target: bool,
    temp_dacl_ready: bool,
    forbidden_manifest: Option<String>,
    mtimes: std::collections::BTreeMap<String, Option<i64>>,
    reported_links: std::collections::BTreeMap<String, u32>,
    sparse_capability: crate::metadata::Capability,
    sparse_observations: std::collections::BTreeMap<
        String,
        (
            Option<Vec<crate::metadata::DataExtent>>,
            Option<crate::metadata::SparseReadIssue>,
        ),
    >,
}
impl PlannerVfs {
    fn entry(&self, mut entry: Entry) -> Entry {
        entry.mode = None;
        entry.device = None;
        if let Some(time) = self.mtimes.get(&entry.name) {
            entry.mtime = *time;
        }
        entry
    }
}
impl Vfs for PlannerVfs {
    fn root_identity(&self, path: &Path) -> io::Result<crate::RootIdentity> {
        Ok(identity(&path.to_string_lossy()))
    }
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(crate::LocalVfs::new()
            .list(path)?
            .into_iter()
            .map(|entry| self.entry(entry))
            .collect())
    }
    fn list_complete(&self, path: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
        Ok(crate::vfs::CompleteDirectoryListing {
            entries: self
                .list(path)?
                .into_iter()
                .map(|entry| crate::vfs::ProofListEntry {
                    raw_name: entry.name.as_bytes().to_vec(),
                    entry,
                    safely_decoded: true,
                    provider_token: None,
                })
                .collect(),
            provider_token: None,
            complete: true,
        })
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        Ok(self.entry(crate::LocalVfs::new().stat(path)?))
    }
    fn exists(&self, _: &Path) -> bool {
        panic!("Keine exists-Abkürzung")
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        crate::LocalVfs::new().open_read(path)
    }
    fn metadata_capabilities(&self, _: &Path) -> io::Result<crate::metadata::MetadataCapabilities> {
        use crate::metadata::{AclModel, Capability};
        let mut value = crate::metadata::MetadataCapabilities::unknown();
        value.sparse_extents = self.sparse_capability;
        value.acl_model = AclModel::WindowsSid;
        value.acl = Capability::ReadOnly;
        value.named_streams = if self.target {
            Capability::Exact
        } else {
            Capability::ReadOnly
        };
        Ok(value)
    }
    fn metadata_manifest(&self, path: &Path) -> io::Result<crate::MetadataManifest> {
        let name = path.file_name().unwrap().to_string_lossy();
        assert_ne!(
            self.forbidden_manifest.as_deref(),
            Some(name.as_ref()),
            "Vorhandenes AddOnly-Manifest darf nicht gelesen werden"
        );
        let dacl = self
            .acls
            .get(name.as_ref())
            .cloned()
            .unwrap_or_else(|| acl(false));
        let context = context();
        crate::MetadataManifest::new(
            None,
            0,
            Some(crate::metadata::Acl::WindowsDacl(dacl)),
            vec![],
            None,
            None,
            None,
        )?
        .with_windows_security(Some(crate::metadata::WindowsSecurityObservation {
            namespace: context.namespace,
            owner: Some(context.owner),
            group: context.group,
            issue: None,
            invalid_descriptor_digest: None,
        }))?
        .with_named_streams(Some(vec![]))?
        .with_reported_hard_links(self.reported_links.get(name.as_ref()).copied())?
        .with_sparse_observation(
            self.sparse_observations
                .get(name.as_ref())
                .and_then(|value| value.0.clone()),
            self.sparse_observations
                .get(name.as_ref())
                .and_then(|value| value.1),
        )
    }
    fn windows_dacl_write_support(
        &self,
    ) -> io::Result<crate::windows_dacl::WindowsDaclWriteSupport> {
        Ok(if self.temp_dacl_ready {
            crate::windows_dacl::WindowsDaclWriteSupport::Ready(context())
        } else {
            crate::windows_dacl::WindowsDaclWriteSupport::Unavailable(
                crate::windows_dacl::WindowsDaclTargetIssue::UnsupportedBackend,
            )
        })
    }
    fn file_dacl_write_support(&self) -> io::Result<crate::windows_dacl::WindowsDaclWriteSupport> {
        Ok(crate::windows_dacl::WindowsDaclWriteSupport::Ready(
            context(),
        ))
    }
    fn supports_directory_stream_updates(&self, _: &Path) -> io::Result<bool> {
        Ok(false)
    }
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        panic!("Plan darf nichts anlegen")
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        panic!("Plan darf nichts anlegen")
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        panic!("Kein Datenwrite")
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        panic!("Kein Tempwrite")
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Löschen")
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Löschen")
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        panic!("Kein Rename")
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        panic!("Kein Rename")
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        panic!("Kein Zeitwrite")
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        panic!("Kein Moduswrite")
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Syncwrite")
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        panic!("Kein Syncwrite")
    }
}
fn planner_fixture() -> (tempfile::TempDir, PlannerVfs, PlannerVfs) {
    let area = tempfile::tempdir().unwrap();
    let base = area.path().canonicalize().unwrap();
    let source = base.join("source");
    let target = base.join("target");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("a"), b"data").unwrap();
    std::fs::write(source.join("b"), b"data").unwrap();
    std::fs::write(target.join("a"), b"data").unwrap();
    std::fs::hard_link(target.join("a"), target.join("b")).unwrap();
    let source = PlannerVfs {
        root: source,
        acls: [("a".into(), acl(true)), ("b".into(), acl(true))].into(),
        target: false,
        temp_dacl_ready: false,
        forbidden_manifest: None,
        mtimes: [("a".into(), Some(200)), ("b".into(), Some(200))].into(),
        reported_links: Default::default(),
        sparse_capability: crate::metadata::Capability::Unknown,
        sparse_observations: Default::default(),
    };
    let target = PlannerVfs {
        root: target,
        acls: Default::default(),
        target: true,
        temp_dacl_ready: false,
        forbidden_manifest: None,
        mtimes: [("a".into(), Some(200)), ("b".into(), Some(200))].into(),
        reported_links: Default::default(),
        sparse_capability: crate::metadata::Capability::Unknown,
        sparse_observations: Default::default(),
    };
    (area, source, target)
}
fn planned(source: &PlannerVfs, target: &PlannerVfs) -> crate::SyncPlan {
    crate::sync::plan_content_with_cancel(
        source,
        &source.root,
        target,
        &target.root,
        &crate::SyncOptions {
            mode: crate::SyncMode::Update,
            compare: crate::Compare::ContentHash,
            metadata: crate::MetadataProfile::NativeBestEffort,
            ..Default::default()
        },
        &crate::Cancel::new(),
        false,
    )
    .unwrap()
}
#[test]
fn file_dacl_plan_deduplicates_existing_object_before_preview_without_copy_bytes() {
    let (_area, source, target) = planner_fixture();
    let plan = planned(&source, &target);
    assert_eq!(plan.actions.len(), 1, "{:#?}", plan.actions);
    let crate::Action::SetFileDacl { plan: effect, .. } = &plan.actions[0] else {
        panic!("{:#?}", plan.actions)
    };
    assert_eq!(effect.alias_scope.selected_paths, vec!["a", "b"]);
    assert_eq!(effect.alias_scope.observed_link_count, Some(2));
    assert_eq!(plan.metadata_count, 1);
    assert_eq!(plan.bytes_to_transfer, 0);
    assert_eq!(plan.copy_count, 0);
    assert_eq!(plan.update_count, 0);
    assert_eq!(plan.comparison.rows.len(), 2);
    assert_eq!(plan.comparison.categories.updates, 2);
    assert_eq!(
        plan.metadata_report
            .classes
            .iter()
            .find(|class| class.class == crate::sync::MetadataClass::Acl)
            .unwrap()
            .transferred,
        1
    );
    let stats = crate::execute(
        crate::ExecuteRequest {
            src: &source,
            src_root: &source.root,
            dst: &target,
            dst_root: &target.root,
            plan: &plan,
            safety: &crate::Safety::default(),
        },
        &crate::Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.files_copied, 0);
    assert_eq!(stats.bytes_copied, 0);
}
#[test]
fn file_dacl_conflicting_alias_desires_are_visible_before_any_effect() {
    let (_area, mut source, target) = planner_fixture();
    let mut conflict = acl(true);
    let WindowsDaclState::Acl { aces, .. } = &mut conflict.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[1] else {
        panic!()
    };
    *mask = 0x89;
    source.acls.insert("b".into(), conflict);
    let plan = planned(&source, &target);
    assert_eq!(plan.skip_count, 2);
    assert_eq!(plan.metadata_count, 0);
    assert!(plan
        .actions
        .iter()
        .all(|action| matches!(action, crate::Action::SkipConflict { .. })));
    assert_eq!(plan.comparison.categories.skipped, 2);
    assert_eq!(plan.comparison.categories.updates, 0);
    crate::execute(
        crate::ExecuteRequest {
            src: &source,
            src_root: &source.root,
            dst: &target,
            dst_root: &target.root,
            plan: &plan,
            safety: &crate::Safety::default(),
        },
        &crate::Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
}
#[test]
fn file_dacl_portable_and_old_strict_plans_do_not_gain_inplace_writes() {
    let (_area, source, target) = planner_fixture();
    for ordinary in [false, true] {
        for profile in [
            crate::MetadataProfile::Portable,
            crate::MetadataProfile::NativeBestEffort,
        ] {
            if ordinary && profile == crate::MetadataProfile::NativeBestEffort {
                continue;
            }
            let opts = crate::SyncOptions {
                mode: crate::SyncMode::Update,
                compare: crate::Compare::ContentHash,
                metadata: profile,
                ..Default::default()
            };
            let plan = if ordinary {
                crate::sync::plan_content_with_cancel(
                    &source,
                    &source.root,
                    &target,
                    &target.root,
                    &opts,
                    &crate::Cancel::new(),
                    false,
                )
            } else {
                crate::plan(&source, &source.root, &target, &target.root, &opts)
            }
            .unwrap();
            assert!(!plan
                .actions
                .iter()
                .any(|action| matches!(action, crate::Action::SetFileDacl { .. })));
        }
    }
}

#[test]
fn file_dacl_noop_alias_has_a_vote_in_object_desire_before_preview() {
    let (_area, mut source, target) = planner_fixture();
    source.acls.insert("a".into(), acl(false));
    let plan = planned(&source, &target);
    assert!(
        plan.actions
            .iter()
            .all(|action| matches!(action, crate::Action::SkipConflict { .. })),
        "{:#?}",
        plan.actions
    );
    assert_eq!(plan.skip_count, 2);
    assert_eq!(plan.metadata_count, 0);
    assert_eq!(plan.comparison.categories.skipped, 2);
    assert_eq!(plan.comparison.categories.updates, 0);
    crate::execute(
        crate::ExecuteRequest {
            src: &source,
            src_root: &source.root,
            dst: &target,
            dst_root: &target.root,
            plan: &plan,
            safety: &crate::Safety::default(),
        },
        &crate::Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
}
#[test]
fn file_dacl_ack_keeps_previously_closed_object_when_second_returns_before() {
    use crate::file_dacl_registry::{Intent, Phase};
    for after in [false, true] {
        let (_area, owner, config) = fixture();
        let first = Intent::create(
            &owner,
            Path::new("/pane/a"),
            &plan(),
            &identity("/pane"),
            [2; 32],
        )
        .unwrap();
        first.phase(Phase::Applying).unwrap();
        first.phase(Phase::Applied).unwrap();
        first.finish(true).unwrap();
        drop(first);
        let mut second_plan = plan();
        second_plan.file_id = 43;
        second_plan.target_proof = proof("b", 43);
        second_plan.alias_scope.selected_paths = vec!["b".into()];
        let second = Intent::create(
            &owner,
            Path::new("/pane/b"),
            &second_plan,
            &identity("/pane"),
            [2; 32],
        )
        .unwrap();
        second.phase(Phase::Applying).unwrap();
        drop(second);
        let mut observed = Readback::new(after);
        observed.observed.as_mut().unwrap().file_id = 43;
        let ack = crate::file_dacl_registry::acknowledge(
            &owner,
            &observed,
            Path::new("/pane"),
            [2; 32],
            &config,
        )
        .unwrap();
        assert_eq!(ack.report.completed.len(), 1 + usize::from(after));
        assert!(ack.report.uncertain.is_empty());
        assert!(ack
            .report
            .completed
            .iter()
            .any(|effect| effect.file_id == 42));
    }
}
#[test]
fn file_dacl_first_registration_is_serialized_between_windows() {
    use crate::file_dacl_registry::Intent;
    let (_area, owner, config) = fixture();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = (0..2)
        .map(|index| {
            let mut owner = owner.clone();
            owner.operation = format!("window-{index}");
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut effect = plan();
                effect.file_id += index;
                effect.target_proof = proof("a", effect.file_id);
                barrier.wait();
                let intent = Intent::create(
                    &owner,
                    Path::new("/pane/a"),
                    &effect,
                    &identity("/pane"),
                    [2; 32],
                )?;
                intent.finish(false)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert!(
        crate::file_dacl_registry::check_registry(&config, &[identity("/pane")], None)
            .unwrap()
            .is_some()
    );
}
#[test]
fn file_dacl_lost_private_namespace_blocks_queue_recovery() {
    use crate::file_dacl_registry::{Intent, Phase};
    use std::os::unix::fs::PermissionsExt;
    let (_area, owner, config) = fixture();
    let intent = Intent::create(
        &owner,
        Path::new("/pane/a"),
        &plan(),
        &identity("/pane"),
        [2; 32],
    )
    .unwrap();
    intent.phase(Phase::Applying).unwrap();
    drop(intent);
    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    let result = crate::local_staging::recover_queue_beneath(&owner.queue, &config);
    assert_eq!(
        result.err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(crate::file_dacl_registry::has_open(&owner, &config).unwrap());
}

#[test]
fn file_dacl_optional_payloads_leave_legacy_empty_shapes_unchanged() {
    let plan = crate::SyncPlan::default();
    let encoded = serde_json::to_value(&plan).unwrap();
    assert!(encoded.get("file_dacl_selections").is_none());
    let decoded: crate::SyncPlan = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, plan);
    let progress = serde_json::to_value(crate::Progress::default()).unwrap();
    assert!(progress.get("file_dacl_report").is_none());
    let stats = serde_json::to_value(crate::SyncStats::default()).unwrap();
    assert!(stats.get("file_dacl_report").is_none());
}

#[test]
fn file_dacl_add_only_does_not_read_existing_manifest() {
    let (_area, mut source, mut target) = planner_fixture();
    std::fs::remove_file(source.root.join("b")).unwrap();
    std::fs::remove_file(target.root.join("b")).unwrap();
    std::fs::write(source.root.join("new"), b"addition").unwrap();
    source.forbidden_manifest = Some("a".into());
    target.forbidden_manifest = Some("a".into());
    let plan = crate::sync::plan_content_with_cancel(
        &source,
        &source.root,
        &target,
        &target.root,
        &crate::SyncOptions {
            mode: crate::SyncMode::AddOnly,
            compare: crate::Compare::ContentHash,
            metadata: crate::MetadataProfile::NativeBestEffort,
            ..Default::default()
        },
        &crate::Cancel::new(),
        false,
    )
    .unwrap();
    assert!(plan.file_dacl_selections.is_empty());
    assert!(!plan
        .actions
        .iter()
        .any(|action| matches!(action, crate::Action::SetFileDacl { .. })));
    assert_eq!(plan.copy_count, 1);
    assert_eq!(std::fs::read(target.root.join("a")).unwrap(), b"data");
}

#[test]
fn file_dacl_update_requires_ordered_source_time_and_preserves_other_modes() {
    use crate::{Action, SyncMode};
    for mode in [SyncMode::Update, SyncMode::Mirror, SyncMode::AddOnly] {
        for (source_time, target_time, expected_update) in [
            (Some(100), Some(200), false),
            (Some(197), Some(200), false),
            (Some(198), Some(200), true),
            (Some(200), Some(200), true),
            (Some(300), Some(200), true),
            (None, Some(200), false),
            (Some(200), None, false),
            (None, None, false),
        ] {
            let (_area, mut source, mut target) = planner_fixture();
            for name in ["a", "b"] {
                source.mtimes.insert(name.into(), source_time);
                target.mtimes.insert(name.into(), target_time);
            }
            let plan = crate::sync::plan_content_with_cancel(
                &source,
                &source.root,
                &target,
                &target.root,
                &crate::SyncOptions {
                    mode,
                    compare: crate::Compare::ContentHash,
                    metadata: crate::MetadataProfile::NativeBestEffort,
                    ..Default::default()
                },
                &crate::Cancel::new(),
                false,
            )
            .unwrap();
            let permitted =
                mode == SyncMode::Mirror || (mode == SyncMode::Update && expected_update);
            assert_eq!(
                plan.actions
                    .iter()
                    .filter(|a| matches!(a, Action::SetFileDacl { .. }))
                    .count(),
                usize::from(permitted),
                "{mode:?} source={source_time:?} target={target_time:?}: {:?}",
                plan.actions
            );
            let conflicts = if mode == SyncMode::Update && !expected_update {
                2
            } else {
                0
            };
            assert_eq!(plan.skip_count, conflicts);
            assert_eq!(plan.comparison.categories.skipped, conflicts);
            assert_eq!(plan.metadata_count, u64::from(permitted));
            assert_eq!(
                plan.comparison.categories.updates,
                if permitted { 2 } else { 0 }
            );
            assert_eq!(plan.copy_count, 0);
            assert_eq!(plan.update_count, 0);
            assert_eq!(plan.bytes_to_transfer, 0);
            assert_eq!(
                plan.actions
                    .iter()
                    .filter(|a| matches!(a, Action::SkipConflict { .. }))
                    .count() as u64,
                conflicts
            );
            if mode == SyncMode::AddOnly {
                assert!(plan.file_dacl_selections.is_empty());
            }
            for row in &plan.comparison.rows {
                assert_eq!(row.source.as_ref().unwrap().mtime, source_time);
                assert_eq!(row.target.as_ref().unwrap().mtime, target_time);
            }
        }
    }
    // Eine ältere, bereits gleiche DACL ist kein offener Versionskonflikt.
    let (_area, mut source, mut target) = planner_fixture();
    for name in ["a", "b"] {
        source.acls.insert(name.into(), acl(false));
        source.mtimes.insert(name.into(), Some(100));
        target.mtimes.insert(name.into(), Some(200));
    }
    let unchanged = planned(&source, &target);
    assert!(unchanged.actions.is_empty());
    assert_eq!(unchanged.skip_count, 0);
}

#[test]
fn file_dacl_blocked_alias_time_prevents_allowed_alias_object_effect() {
    for blocked_time in [Some(100), None] {
        let (_area, mut source, mut target) = planner_fixture();
        source.mtimes.insert("a".into(), Some(200));
        source.mtimes.insert("b".into(), blocked_time);
        for name in ["a", "b"] {
            target.mtimes.insert(name.into(), Some(200));
        }
        let plan = planned(&source, &target);
        assert_eq!(plan.actions.len(), 2, "{:?}", plan.actions);
        assert!(plan
            .actions
            .iter()
            .all(|a| matches!(a, crate::Action::SkipConflict { .. })));
        assert_eq!(plan.skip_count, 2);
        assert_eq!(plan.metadata_count, 0);
        assert_eq!(plan.comparison.categories.skipped, 2);
        assert_eq!(plan.comparison.categories.updates, 0);
        assert_eq!(plan.bytes_to_transfer, 0);
        assert!(plan
            .metadata_report
            .issues
            .iter()
            .any(|i| i.path == "a" && i.class == crate::sync::MetadataClass::Acl));
        assert!(plan.file_dacl_selections.is_empty());
        // Die Fixture würde jede tatsächliche Mutation mit panic verweigern.
        let stats = crate::execute(
            crate::ExecuteRequest {
                src: &source,
                src_root: &source.root,
                dst: &target,
                dst_root: &target.root,
                plan: &plan,
                safety: &crate::Safety::default(),
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(stats.files_copied, 0);
        assert_eq!(stats.bytes_copied, 0);
    }
}

#[test]
fn file_dacl_supported_concrete_deny_noop_has_no_false_acl_omission() {
    for mask in [0, 2, 4, 6, 8, 0x10, 0x20, 0x100, 0x10000, 0x0009_013e] {
        for temp_ready in [false, true] {
            let (_area, mut source, mut target) = planner_fixture();
            target.temp_dacl_ready = temp_ready;
            let mut desired = acl(true);
            let WindowsDaclState::Acl { aces, .. } = &mut desired.state else {
                panic!()
            };
            if mask != 0 {
                aces.insert(
                    0,
                    WindowsAce::Deny {
                        flags: 0,
                        mask,
                        sid: context().owner,
                    },
                );
            }
            for name in ["a", "b"] {
                source.acls.insert(name.into(), desired.clone());
                target.acls.insert(name.into(), desired.clone());
            }
            let plan = planned(&source, &target);
            assert!(plan.actions.is_empty());
            assert!(
                plan.metadata_report
                    .issues
                    .iter()
                    .all(|issue| issue.class != crate::sync::MetadataClass::Acl),
                "temp_ready={temp_ready}, mask={mask:#x}: {:#?}",
                plan.metadata_report.issues
            );
        }
    }
}

#[test]
fn reported_link_omissions_survive_noops_without_inventing_groups() {
    for count in [None, Some(0), Some(1), Some(2), Some(u32::MAX)] {
        let (_area, mut source, mut target) = planner_fixture();
        target.acls = source.acls.clone();
        if let Some(count) = count {
            source.reported_links = [("a".into(), count), ("b".into(), count)].into();
        }
        let plan = planned(&source, &target);
        assert!(plan.actions.is_empty());
        let issues: Vec<_> = plan
            .metadata_report
            .issues
            .iter()
            .filter(|issue| issue.class == crate::sync::MetadataClass::Hardlinks)
            .collect();
        assert_eq!(
            issues.len(),
            if count.is_some_and(|value| value != 1) {
                2
            } else {
                0
            }
        );
        assert!(plan.metadata_report.hardlink_actions == 0);
        if count.is_some() {
            assert_eq!(plan.metadata_observations.len(), 2);
        }
    }
}

#[test]
fn one_selected_name_does_not_invent_a_link_relationship() {
    for count in [0, 2, u32::MAX] {
        let (_area, mut source, mut target) = planner_fixture();
        target.acls = source.acls.clone();
        std::fs::remove_file(source.root.join("b")).unwrap();
        source.reported_links.insert("a".into(), count);
        let plan = planned(&source, &target);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.metadata_observations.len(), 1);
        assert_eq!(
            plan.metadata_report
                .issues
                .iter()
                .filter(|issue| issue.class == crate::sync::MetadataClass::Hardlinks)
                .count(),
            usize::from(count == 0)
        );
    }
}

#[test]
fn reported_link_drift_stops_before_an_independent_copy() {
    let (_area, mut source, mut target) = planner_fixture();
    target.acls = source.acls.clone();
    source.reported_links.insert("a".into(), 2);
    std::fs::write(source.root.join("independent"), b"must-not-copy").unwrap();
    let plan = planned(&source, &target);
    assert_eq!(plan.copy_count, 1);
    source.reported_links.insert("a".into(), 3);
    let error = crate::execute(
        crate::ExecuteRequest {
            src: &source,
            src_root: &source.root,
            dst: &target,
            dst_root: &target.root,
            plan: &plan,
            safety: &crate::Safety::headless_test(),
        },
        &crate::Cancel::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
    assert!(!target.root.join("independent").exists());
}

#[test]
fn portable_noop_does_not_read_reported_link_metadata() {
    let (_area, mut source, mut target) = planner_fixture();
    source.forbidden_manifest = Some("a".into());
    target.forbidden_manifest = Some("a".into());
    let plan = crate::sync::plan_content_with_cancel(
        &source,
        &source.root,
        &target,
        &target.root,
        &crate::SyncOptions {
            mode: crate::SyncMode::Update,
            compare: crate::Compare::ContentHash,
            metadata: crate::MetadataProfile::Portable,
            ..Default::default()
        },
        &crate::Cancel::new(),
        false,
    )
    .unwrap();
    assert!(plan.actions.is_empty());
    assert!(plan.metadata_report.issues.is_empty());
    assert!(plan.metadata_observations.is_empty());
}

#[test]
fn file_dacl_unsupported_deny_or_inheritance_stays_visible_when_already_equal() {
    for (flags, mask) in [(0, 1), (0, 0x40), (0, 0x4000_0000), (0x10, 6)] {
        let (_area, mut source, mut target) = planner_fixture();
        target.temp_dacl_ready = true;
        let mut desired = acl(true);
        let WindowsDaclState::Acl { aces, .. } = &mut desired.state else {
            panic!()
        };
        aces.insert(
            0,
            WindowsAce::Deny {
                flags,
                mask,
                sid: context().owner,
            },
        );
        for name in ["a", "b"] {
            source.acls.insert(name.into(), desired.clone());
            target.acls.insert(name.into(), desired.clone());
        }
        let plan = planned(&source, &target);
        assert!(plan.actions.is_empty());
        assert!(plan
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == crate::sync::MetadataClass::Acl));
    }
}

#[test]
fn sparse_capability_without_object_observation_never_enables_sparse_transfer() {
    use crate::metadata::Capability;
    let (_area, mut source, mut target) = planner_fixture();
    target.acls = source.acls.clone();
    source.sparse_capability = Capability::ReadOnly;
    target.sparse_capability = Capability::Exact;
    std::fs::write(source.root.join("new-file"), b"new").unwrap();
    let plan = planned(&source, &target);
    assert_eq!(plan.copy_count, 1);
    for action in plan.actions {
        if let crate::Action::CopyFile {
            metadata: Some(value),
            ..
        } = action
        {
            assert!(!value.sparse_extents);
        }
    }
}

#[test]
fn sparse_read_omissions_survive_noop_and_dacl_alias_dedup_with_typed_denial() {
    use crate::metadata::{Capability, SparseReadIssue};
    for dacl_change in [false, true] {
        for issue in [
            SparseReadIssue::ServerStatus(0xc000_0022),
            SparseReadIssue::ServerStatus(0xc000_00bb),
            SparseReadIssue::BufferLimit,
            SparseReadIssue::InvalidResponse,
        ] {
            for names in [vec!["a"], vec!["b"], vec!["a", "b"]] {
                let (_area, mut source, mut target) = planner_fixture();
                if !dacl_change {
                    target.acls = source.acls.clone();
                }
                source.sparse_capability = Capability::ReadOnly;
                for name in &names {
                    source
                        .sparse_observations
                        .insert((*name).into(), (None, Some(issue)));
                }
                let plan = planned(&source, &target);
                let issues: Vec<_> = plan
                    .metadata_report
                    .issues
                    .iter()
                    .filter(|value| value.class == crate::MetadataClass::SparseExtents)
                    .collect();
                assert_eq!(issues.len(), names.len());
                for issue_report in issues {
                    assert!(names.contains(&issue_report.path.as_str()));
                    assert_eq!(
                        issue_report.kind,
                        if issue == SparseReadIssue::ServerStatus(0xc000_0022) {
                            crate::MetadataIssueKind::PrivilegeFailure
                        } else {
                            crate::MetadataIssueKind::Unsupported
                        }
                    );
                }
                assert_eq!(plan.actions.is_empty(), !dacl_change);
            }
        }
    }
}

#[test]
fn known_sparse_difference_stays_visible_without_a_target_writer_in_noop_or_dacl() {
    use crate::metadata::{Capability, DataExtent};
    for dacl_change in [false, true] {
        let (_area, mut source, mut target) = planner_fixture();
        if !dacl_change {
            target.acls = source.acls.clone();
        }
        for name in ["a", "b"] {
            std::fs::write(source.root.join(name), [0; 4]).unwrap();
        }
        std::fs::write(target.root.join("a"), [0; 4]).unwrap();
        source.sparse_capability = Capability::ReadOnly;
        target.sparse_capability = Capability::Unsupported;
        source
            .sparse_observations
            .insert("a".into(), (Some(vec![]), None));
        target.sparse_observations.insert(
            "a".into(),
            (
                Some(vec![DataExtent {
                    offset: 0,
                    length: 4,
                }]),
                None,
            ),
        );
        let plan = planned(&source, &target);
        assert_eq!(plan.copy_count + plan.update_count, 0);
        assert_eq!(plan.actions.is_empty(), !dacl_change);
        assert_eq!(
            plan.metadata_report
                .issues
                .iter()
                .filter(|issue| issue.class == crate::MetadataClass::SparseExtents)
                .count(),
            1
        );
    }
}
