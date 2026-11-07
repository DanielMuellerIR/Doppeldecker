use super::*;
use crate::metadata::{Acl, MetadataManifest, WindowsSecurityObservation};
use crate::windows_security::{WindowsDacl, WindowsDaclState, WindowsSid};
use crate::Vfs;
use std::os::unix::fs::PermissionsExt;

fn identity() -> crate::RootIdentity {
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
        virtual_root: "/pane".into(),
        root_file_id: 1,
        tree_id: 1,
        session_generation: 1,
    })
}
fn sid() -> WindowsSid {
    WindowsSid {
        revision: 1,
        authority: 5,
        sub_authorities: vec![21, 42],
    }
}
fn acl(control: u16) -> WindowsDacl {
    WindowsDacl {
        namespace: [1; 16],
        control,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![],
        },
    }
}
fn binding() -> DirectoryDaclIntentBinding {
    DirectoryDaclIntentBinding {
        directory: "/pane/folder".into(),
        base_id: 42,
        namespace: [1; 16],
        before_digest: crate::directory_dacl::dacl_digest(&acl(0x1004)).unwrap(),
        after_digest: crate::directory_dacl::dacl_digest(&acl(0x100c)).unwrap(),
        owner: sid(),
        group: Some(sid()),
        phase: DirectoryDaclIntentPhase::Prepared,
    }
}
fn fixture() -> (tempfile::TempDir, StagingOwner) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = StagingOwner {
        queue: temp.path().canonicalize().unwrap().join("queue.json"),
        operation: "d".repeat(32),
    };
    (temp, owner)
}
fn intent(owner: &StagingOwner) -> RemoteDirectoryDaclIntent {
    RemoteDirectoryDaclIntent::create_with_root(owner, binding(), &identity(), [2; 32]).unwrap()
}
fn recovered_report(owner: &StagingOwner) -> crate::directory_dacl::DirectoryDaclReport {
    let mut report = crate::directory_dacl::DirectoryDaclReport::default();
    for recovered in recover_queue(&owner.queue).unwrap() {
        report.merge(&recovered.directory_dacl_report);
    }
    report
}
struct Observation {
    identity: crate::RootIdentity,
    entry: crate::Entry,
    manifest: Option<MetadataManifest>,
}
impl Observation {
    fn new(control: u16) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let mut entry = crate::local_fs::stat(&temp.path().canonicalize().unwrap()).unwrap();
        entry.inode = Some(42);
        let manifest = MetadataManifest::new(
            None,
            0,
            Some(Acl::WindowsDacl(acl(control))),
            vec![],
            None,
            None,
            None,
        )
        .unwrap()
        .with_windows_security(Some(WindowsSecurityObservation {
            namespace: [1; 16],
            owner: Some(sid()),
            group: Some(sid()),
            issue: None,
            invalid_descriptor_digest: None,
        }))
        .unwrap();
        Self {
            identity: identity(),
            entry,
            manifest: Some(manifest),
        }
    }
}
impl Vfs for Observation {
    fn root_identity(&self, _: &Path) -> io::Result<crate::RootIdentity> {
        Ok(self.identity.clone())
    }
    fn stat(&self, _: &Path) -> io::Result<crate::Entry> {
        panic!("DACL-Recovery darf Identität nicht getrennt vom Security-Readback lesen")
    }
    fn directory_dacl_recovery_observation(
        &self,
        _: &Path,
        expected_root: &crate::RootIdentity,
    ) -> io::Result<crate::windows_dacl::DirectoryDaclRecoveryObservation> {
        if expected_root != &self.identity
            || !self.entry.is_dir
            || self.entry.is_file
            || self.entry.is_symlink
        {
            return Err(io::Error::other("Kein gebundener echter Ordner"));
        }
        let manifest = self
            .manifest
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "unlesbar"))?;
        manifest.validate()?;
        let security = manifest
            .windows_security
            .as_ref()
            .ok_or_else(|| io::Error::other("Security fehlt"))?;
        let Some(Acl::WindowsDacl(dacl)) = &manifest.acl else {
            return Err(io::Error::other("DACL fehlt"));
        };
        Ok(crate::windows_dacl::DirectoryDaclRecoveryObservation {
            base_id: self
                .entry
                .inode
                .ok_or_else(|| io::Error::other("ID fehlt"))?,
            context: crate::windows_dacl::WindowsDaclWriteContext {
                namespace: security.namespace,
                owner: security
                    .owner
                    .clone()
                    .ok_or_else(|| io::Error::other("Owner fehlt"))?,
                group: security.group.clone(),
            },
            dacl: dacl.clone(),
        })
    }
    fn metadata_manifest(&self, _: &Path) -> io::Result<MetadataManifest> {
        self.manifest
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "unlesbar"))
    }
    fn list(&self, _: &Path) -> io::Result<Vec<crate::Entry>> {
        panic!("Recovery darf nicht listen")
    }
    fn exists(&self, _: &Path) -> bool {
        panic!("Recovery darf exists nicht verwenden")
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        panic!("Remote-Schreibzugriff")
    }
    fn open_read(&self, _: &Path) -> io::Result<Box<dyn Read + Send>> {
        panic!("Unerwarteter Datenzugriff")
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        panic!("Remote-Schreibzugriff")
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        panic!("Remote-Schreibzugriff")
    }
}

#[test]
fn v5_is_typed_and_preserves_legacy_json_and_digests() {
    let (_temp, owner) = fixture();
    let legacy = RemoteTempIntent::create(
        &owner,
        Path::new("/pane/file"),
        Path::new("/pane/.temporary"),
    )
    .unwrap();
    let bytes = std::fs::read(legacy.path()).unwrap();
    let expected = format!("{{\"version\":1,\"owner\":{{\"queue\":{},\"operation\":{}}},\"target\":\"/pane/file\",\"candidate\":\"/pane/.temporary\",\"remote\":true,\"stage_guard\":false}}",
        serde_json::to_string(&owner.queue).unwrap(), serde_json::to_string(&owner.operation).unwrap());
    assert_eq!(bytes, expected.as_bytes());
    let record: NeighborRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        record_stamp(&record, &bytes).unwrap(),
        *blake3::hash(&bytes).as_bytes()
    );
    legacy.finish().unwrap();
    let current = intent(&owner);
    let bytes = std::fs::read(current.path()).unwrap();
    let mut record: NeighborRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(record.version, 5);
    assert!(record.named_stream.is_none());
    assert!(record.candidate.is_none());
    validate_neighbor(&record, owner.queue.parent().unwrap()).unwrap();
    record.version = 4;
    assert!(validate_neighbor(&record, owner.queue.parent().unwrap()).is_err());
    record.version = 5;
    record.candidate = Some("/pane/.temporary".into());
    assert!(validate_neighbor(&record, owner.queue.parent().unwrap()).is_err());
    assert!(current.phase(DirectoryDaclIntentPhase::Applied).is_err());
    assert!(recover_queue(&owner.queue)
        .unwrap()
        .iter()
        .any(|entry| entry.directory_dacl_recovery));
    assert!(recovered_report(&owner).is_empty());
    current.finish_prepared().unwrap();
    assert!(recover_queue(&owner.queue)
        .unwrap()
        .iter()
        .all(|entry| !entry.directory_dacl_recovery));
    assert!(recovered_report(&owner).is_empty());
}

#[test]
fn prepared_and_crashed_applying_are_not_temps_or_forward_candidates() {
    let (_temp, owner) = fixture();
    let current = intent(&owner);
    let prepared = std::fs::read(current.path()).unwrap();
    assert!(current.finish().is_err());
    current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
    assert_eq!(current.report().uncertain.len(), 1);
    assert!(current.finish_prepared().is_err());
    let index = current.path().to_owned();
    drop(current);
    // Der Snapshot liegt hinter dem dauerhaft bestätigten WAL-Phasenwechsel.
    std::fs::write(&index, prepared).unwrap();
    assert_eq!(recovered_report(&owner).uncertain.len(), 1);
    let endpoint = crate::EndpointBinding::remote(&identity(), [2; 32]).unwrap();
    let view = namespace_staging_view(
        owner.queue.parent().unwrap(),
        &[(&endpoint, Some(&identity()), Path::new("/pane"))],
    )
    .unwrap();
    assert_eq!(view.candidates.len(), 1);
    assert!(matches!(
        view.candidates[0].object,
        StagingObject::DirectoryDacl(_)
    ));
    assert!(view.candidates[0]
        .object
        .is_absent(&Observation::new(0x1004))
        .is_err());
    assert!(forward_staging_candidates(&owner).is_err());
    let registry =
        crate::staging_guard::KnownStagingRegistry::at_private_root(owner.queue.parent().unwrap())
            .unwrap();
    let endpoints = [crate::staging_guard::StagingEndpoint {
        vfs: std::sync::Arc::new(Observation::new(0x1004)),
        root: "/pane".into(),
        binding: endpoint,
    }];
    assert!(registry.check(&endpoints, None).is_err());
    assert!(registry.check_both(&endpoints, None).is_err());
}

#[test]
fn applied_report_survives_stale_index_failed_close_and_closed_index_reload() {
    let (_temp, owner) = fixture();
    let current = intent(&owner);
    let prepared = std::fs::read(current.path()).unwrap();
    current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
    current.phase(DirectoryDaclIntentPhase::Applied).unwrap();
    let index = current.path().to_owned();
    std::fs::write(&index, prepared).unwrap();
    assert_eq!(recovered_report(&owner).completed.len(), 1);
    // Der Ergebnisbeleg ist auch bei einem späteren lokalen Abschlussfehler da.
    std::fs::remove_file(&index).unwrap();
    assert!(current.finish().is_err());
    drop(current);
    assert_eq!(recovered_report(&owner).completed.len(), 1);
    assert!(recovered_report(&owner).uncertain.is_empty());
    let endpoint = crate::EndpointBinding::remote(&identity(), [2; 32]).unwrap();
    assert!(namespace_staging_view(
        owner.queue.parent().unwrap(),
        &[(&endpoint, Some(&identity()), Path::new("/pane"))]
    )
    .unwrap()
    .candidates
    .is_empty());
}

#[test]
fn explicit_recovery_accepts_before_and_after_without_remote_writes() {
    for (phase, control, completed) in [
        (DirectoryDaclIntentPhase::Prepared, 0x1004, 0),
        (DirectoryDaclIntentPhase::Applying, 0x1004, 0),
        (DirectoryDaclIntentPhase::Applying, 0x100c, 1),
        (DirectoryDaclIntentPhase::Applied, 0x100c, 1),
        (DirectoryDaclIntentPhase::Applied, 0x1004, 1),
    ] {
        let (_temp, owner) = fixture();
        let current = intent(&owner);
        if phase != DirectoryDaclIntentPhase::Prepared {
            current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
        }
        if phase == DirectoryDaclIntentPhase::Applied {
            current.phase(phase).unwrap();
        }
        let index = current.path().to_owned();
        drop(current);
        let mut observation = Observation::new(control);
        if let crate::RootIdentity::Smb(root) = &mut observation.identity {
            root.session_generation += 1;
            root.tree_id += 1;
        }
        let report =
            finish_directory_dacl_recovery(&owner, &observation, Path::new("/pane"), [2; 32])
                .unwrap();
        assert_eq!(report.completed.len(), completed);
        assert!(report.uncertain.is_empty());
        assert!(!index.exists());
        let report = recovered_report(&owner);
        assert_eq!(report.completed.len(), completed);
        assert!(report.uncertain.is_empty());
    }
}

#[test]
fn explicit_recovery_blocks_foreign_identity_third_acl_and_unreadable_security() {
    for mismatch in 0..7 {
        let (_temp, owner) = fixture();
        let current = intent(&owner);
        current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
        let index = current.path().to_owned();
        drop(current);
        let mut observation = Observation::new(0x1004);
        match mismatch {
            0 => observation = Observation::new(0x1104),
            1 => observation.entry.inode = Some(43),
            2 => observation.manifest = None,
            3 => {
                if let crate::RootIdentity::Smb(root) = &mut observation.identity {
                    root.root_file_id += 1;
                }
            }
            4 => {
                let manifest = observation.manifest.take().unwrap();
                let mut security = manifest.windows_security.clone().unwrap();
                security.owner = Some(WindowsSid {
                    sub_authorities: vec![99],
                    ..sid()
                });
                observation.manifest =
                    Some(manifest.with_windows_security(Some(security)).unwrap());
            }
            5 => {
                let manifest = observation.manifest.take().unwrap();
                let mut security = manifest.windows_security.clone().unwrap();
                security.group = None;
                observation.manifest =
                    Some(manifest.with_windows_security(Some(security)).unwrap());
            }
            6 => observation.entry.is_symlink = true,
            _ => unreachable!(),
        }
        let error =
            finish_directory_dacl_recovery(&owner, &observation, Path::new("/pane"), [2; 32])
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(index.exists());
        assert_eq!(recovered_report(&owner).uncertain.len(), 1);
    }
}

#[test]
fn acknowledged_before_checkpoint_clears_stale_queue_uncertainty_after_reload() {
    let (_temp, owner) = fixture();
    let current = intent(&owner);
    current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
    let mut stale_queue_report = current.report();
    drop(current);
    assert_eq!(stale_queue_report.uncertain.len(), 1);
    finish_directory_dacl_recovery(
        &owner,
        &Observation::new(0x1004),
        Path::new("/pane"),
        [2; 32],
    )
    .unwrap();
    // Der Queuewrite sei ausgefallen. Erst der neue Registryread sieht den
    // vollständig gespeicherten leeren Bericht aus recovery_before.
    let recovered = recover_queue(&owner.queue).unwrap();
    let checkpoint = recovered
        .iter()
        .find(|entry| entry.directory_dacl_acknowledged)
        .unwrap();
    stale_queue_report = checkpoint.directory_dacl_report.clone();
    assert!(stale_queue_report.is_empty());
    assert!(checkpoint.paths.is_empty());
    assert!(!checkpoint.directory_dacl_recovery);
    assert!(checkpoint
        .directory_dacl_acknowledged_paths
        .contains(&"/pane/folder".to_string()));
    assert!(!checkpoint
        .directory_dacl_acknowledged_paths
        .iter()
        .any(|path| path.ends_with("unrelated.temp")));
    assert!(finish_directory_dacl_recovery(
        &owner,
        &Observation::new(0x1004),
        Path::new("/pane"),
        [2; 32]
    )
    .unwrap()
    .is_empty());
}

#[test]
fn interrupted_phase_tail_keeps_typed_path_until_explicit_acknowledgement() {
    let (_temp, owner) = fixture();
    let current = intent(&owner);
    let mut record = current.record.lock().unwrap().clone();
    record.directory_dacl.as_mut().unwrap().phase = DirectoryDaclIntentPhase::Applying;
    let mut tail = serde_json::to_vec(&("update", &record)).unwrap();
    tail.truncate(tail.len() - 4);
    let config_path = owner.queue.parent().unwrap();
    let mut events = std::fs::OpenOptions::new()
        .append(true)
        .open(config_path.join(INTENT_EVENTS))
        .unwrap();
    events.write_all(&tail).unwrap();
    events.sync_all().unwrap();
    drop(events);
    drop(current);
    assert_eq!(recovered_report(&owner).uncertain.len(), 1);
    let endpoint = crate::EndpointBinding::remote(&identity(), [2; 32]).unwrap();
    let scope = [(&endpoint, Some(&identity()), Path::new("/pane"))];
    assert!(
        namespace_staging_view_excluding_both(config_path, &scope, Some(&owner))
            .unwrap()
            .candidates
            .iter()
            .any(|entry| matches!(entry.object, StagingObject::DirectoryDacl(_)))
    );
    finish_directory_dacl_recovery(
        &owner,
        &Observation::new(0x1004),
        Path::new("/pane"),
        [2; 32],
    )
    .unwrap();
    assert!(namespace_staging_view(config_path, &scope)
        .unwrap()
        .candidates
        .is_empty());
    assert!(recovered_report(&owner).is_empty());
}

#[test]
fn queue_acknowledgement_uses_the_held_config_and_rejects_path_replacement() {
    let (_temp, owner) = fixture();
    let current = intent(&owner);
    current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
    let index = current.path().to_owned();
    drop(current);
    let config_path = owner.queue.parent().unwrap();
    let held = RootHandle::open_nofollow(config_path).unwrap();
    let displaced = config_path.with_extension("displaced");
    std::fs::rename(config_path, &displaced).unwrap();
    std::fs::create_dir(config_path).unwrap();
    std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = finish_directory_dacl_recovery_beneath(
        &owner,
        &Observation::new(0x1004),
        Path::new("/pane"),
        [2; 32],
        &held,
    );
    std::fs::remove_dir(config_path).unwrap();
    std::fs::rename(&displaced, config_path).unwrap();
    assert!(result.is_err());
    assert!(index.exists());
    assert_eq!(recovered_report(&owner).uncertain.len(), 1);
}

#[test]
fn durable_close_keeps_failed_local_index_cleanup_as_forensics_without_reblocking() {
    let (_temp, owner) = fixture();
    let current = intent(&owner);
    current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
    current.phase(DirectoryDaclIntentPhase::Applied).unwrap();
    let index = current.path().to_owned();
    {
        let record = current.record.lock().unwrap();
        let _lock = lock_intent_events(&current.config).unwrap();
        // Absturzschnitt nach dem dauerhaften Abschluss, vor Indexentfernung.
        append_intent_event_locked(&current.config, &record, "finish").unwrap();
    }
    drop(current);
    assert!(index.exists());
    let endpoint = crate::EndpointBinding::remote(&identity(), [2; 32]).unwrap();
    assert!(namespace_staging_view(
        owner.queue.parent().unwrap(),
        &[(&endpoint, Some(&identity()), Path::new("/pane"))]
    )
    .unwrap()
    .candidates
    .is_empty());
    assert!(forward_staging_candidates(&owner).unwrap().is_empty());
    let recovered = recover_queue(&owner.queue).unwrap();
    let checkpoint = recovered
        .iter()
        .find(|entry| entry.directory_dacl_acknowledged)
        .unwrap();
    assert!(!checkpoint.directory_dacl_recovery);
    assert!(checkpoint
        .directory_dacl_acknowledged_paths
        .contains(&index.to_string_lossy().into_owned()));
    assert_eq!(checkpoint.directory_dacl_report.completed.len(), 1);
}

#[test]
fn foreign_epoch_excludes_only_exact_own_durably_closed_directory_dacl_history() {
    let (_temp, owner) = fixture();
    let config = owner.queue.parent().unwrap();
    let target_identity = identity();
    let mut source_identity = identity();
    if let crate::RootIdentity::Smb(root) = &mut source_identity {
        root.virtual_root = "/source".into();
    }
    let target_binding = crate::EndpointBinding::remote(&target_identity, [2; 32]).unwrap();
    let source_binding = crate::EndpointBinding::remote(&source_identity, [2; 32]).unwrap();
    let scopes = [
        (
            &source_binding,
            Some(&source_identity),
            Path::new("/source"),
        ),
        (&target_binding, Some(&target_identity), Path::new("/pane")),
    ];
    let general_before = namespace_staging_view(config, &scopes).unwrap().epoch;
    let foreign_before = namespace_staging_view_excluding(config, &scopes, Some(&owner))
        .unwrap()
        .epoch;
    let current = intent(&owner);
    for phase in [
        DirectoryDaclIntentPhase::Prepared,
        DirectoryDaclIntentPhase::Applying,
        DirectoryDaclIntentPhase::Applied,
    ] {
        if phase != DirectoryDaclIntentPhase::Prepared {
            current.phase(phase).unwrap();
        }
        let open = namespace_staging_view_excluding(config, &scopes, Some(&owner)).unwrap();
        assert!(
            !open.candidates.is_empty(),
            "Offener eigener Intent darf nie ausgeblendet werden"
        );
        assert_ne!(open.epoch, foreign_before);
        assert!(forward_staging_candidates(&owner).is_err());
    }
    current.finish().unwrap();
    drop(current);
    let closed = namespace_staging_view_excluding(config, &scopes, Some(&owner)).unwrap();
    assert!(closed.candidates.is_empty());
    assert_eq!(closed.epoch, foreign_before);
    assert_ne!(
        namespace_staging_view(config, &scopes).unwrap().epoch,
        general_before
    );
    // Derselbe Operationsname in einer anderen Fensterqueue ist ein anderer Owner.
    let other = StagingOwner {
        queue: config.join("other-window.json"),
        operation: owner.operation.clone(),
    };
    let foreign = intent(&other);
    foreign.phase(DirectoryDaclIntentPhase::Applying).unwrap();
    foreign.phase(DirectoryDaclIntentPhase::Applied).unwrap();
    foreign.finish().unwrap();
    drop(foreign);
    assert_ne!(
        namespace_staging_view_excluding(config, &scopes, Some(&owner))
            .unwrap()
            .epoch,
        foreign_before
    );
}

#[test]
fn open_intent_query_is_read_only_and_counts_prepared_without_report() {
    let (_temp, owner) = fixture();
    let root = RootHandle::open_nofollow(owner.queue.parent().unwrap()).unwrap();
    let before = std::fs::read_dir(root.path()).unwrap().count();
    assert!(!has_open_directory_dacl_intent_beneath(&owner, &root).unwrap());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
    let current = intent(&owner);
    assert!(current.report().is_empty());
    assert!(has_open_directory_dacl_intent_beneath(&owner, &root).unwrap());
    let other = StagingOwner {
        operation: "e".repeat(32),
        ..owner.clone()
    };
    assert!(!has_open_directory_dacl_intent_beneath(&other, &root).unwrap());
    current.finish_prepared().unwrap();
    drop(current);
    assert!(!has_open_directory_dacl_intent_beneath(&owner, &root).unwrap());
    let current = intent(&owner);
    current.phase(DirectoryDaclIntentPhase::Applying).unwrap();
    assert!(has_open_directory_dacl_intent_beneath(&owner, &root).unwrap());
    let mut events = std::fs::OpenOptions::new()
        .append(true)
        .open(root.path().join(INTENT_EVENTS))
        .unwrap();
    events.write_all(b"[\"update\",").unwrap();
    events.sync_all().unwrap();
    assert!(has_open_directory_dacl_intent_beneath(&owner, &root).is_err());
}
