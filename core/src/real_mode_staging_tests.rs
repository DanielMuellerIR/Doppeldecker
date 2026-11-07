use super::*;
use std::cell::RefCell;
use tempfile::TempDir;
thread_local! {static SCAN_HOOK:RefCell<Option<Box<dyn FnOnce()>>>=const {RefCell::new(None)};}
pub(super) fn take_scan_hook() -> Option<Box<dyn FnOnce()>> {
    SCAN_HOOK.with(|hook| hook.borrow_mut().take())
}
struct Fixture {
    area: TempDir,
    queue: crate::OperationQueue,
    other: crate::OperationQueue,
    source: PathBuf,
    target: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let area = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let root = area.path().canonicalize().unwrap();
        let source = root.join("source");
        let target = root.join("target");
        for path in [&source, &target] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(source.join("ordinary"), b"source").unwrap();
        std::fs::write(target.join("extra"), b"target").unwrap();
        let config = root.join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
        let queue = crate::OperationQueue::open(config.join("operations-queue-a.json")).unwrap();
        let (_, lock, _) = queue.ftps_owner(None).unwrap();
        let other = crate::OperationQueue::open_with_process_lock(
            config.join("operations-queue-b.json"),
            lock,
        )
        .unwrap();
        Self {
            area,
            queue,
            other,
            source,
            target,
        }
    }
    fn stage(&self, root: &Path) -> (Box<dyn Vfs>, PathBuf) {
        let (owner, _, _) = self.other.ftps_owner(None).unwrap();
        stage(root, &owner, true)
    }
}
fn stage(root: &Path, owner: &StagingOwner, body: bool) -> (Box<dyn Vfs>, PathBuf) {
    let vfs = LocalVfs::rooted(root)
        .unwrap()
        .with_staging_owner(&owner.queue, &owner.operation);
    let scope = vfs
        .staging_scope(&root.join("foreign-target"), None)
        .unwrap()
        .expect("privater lokaler Stagingkontext");
    assert!(scope.has_private_staging());
    let path = scope.staging_recovery_path().unwrap().to_path_buf();
    if body {
        std::fs::write(path.join("private-body"), b"private partial").unwrap();
    }
    (scope, path)
}
#[test]
fn registered_foreign_local_stage_blocks_source_and_mirror_target_active_and_crashed() {
    for active in [true, false] {
        for target_side in [false, true] {
            let f = Fixture::new();
            let (scope, path) = f.stage(if target_side { &f.target } else { &f.source });
            let mut live = Some(scope);
            if !active {
                drop(live.take());
            }
            let result = if target_side {
                prepare_local_real_run_for_queue(
                    &f.queue,
                    &f.source,
                    &f.target,
                    SyncOptions {
                        mode: SyncMode::Mirror,
                        compare: Compare::ContentHash,
                        ..Default::default()
                    },
                    None,
                )
                .map(|_| ())
            } else {
                prepare_local_sync_run_for_queue(
                    &f.queue,
                    &f.source,
                    &f.target,
                    SyncOptions::default(),
                    None,
                )
                .map(|_| ())
            };
            let error = result.unwrap_err();
            assert_eq!(
                crate::error_code(&error),
                ErrorCode::OutcomeUnclear,
                "{error}"
            );
            assert!(crate::error::recovery_paths(&error)
                .iter()
                .any(|entry| Path::new(entry) == path));
            assert!(!f.target.join("ordinary").exists());
            assert_eq!(std::fs::read(f.target.join("extra")).unwrap(), b"target");
            assert_eq!(
                std::fs::read(path.join("private-body")).unwrap(),
                b"private partial"
            );
            assert!(!f.area.path().join(".doppeldecker-quarantine").exists());
        }
    }
}
#[test]
fn unrelated_foreign_stage_does_not_serialize_local_sync() {
    let f = Fixture::new();
    let unrelated = f.source.parent().unwrap().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (_scope, path) = f.stage(&unrelated);
    let prepared = prepare_local_sync_run_for_queue(
        &f.queue,
        &f.source,
        &f.target,
        SyncOptions::default(),
        None,
    )
    .unwrap();
    let stats = prepared
        .confirm(true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(stats.files_copied, 1);
    assert_eq!(std::fs::read(f.target.join("ordinary")).unwrap(), b"source");
    assert!(path.join("private-body").exists());
}
#[test]
fn conscious_stage_recovery_requires_fresh_local_confirmation() {
    let f = Fixture::new();
    let prepared = prepare_local_sync_run_for_queue(
        &f.queue,
        &f.source,
        &f.target,
        SyncOptions::default(),
        None,
    )
    .unwrap();
    let (scope, path) = f.stage(&f.source);
    drop(scope);
    let saved = f.area.path().join("saved-private-stage");
    std::fs::rename(&path, &saved).unwrap();
    let error = prepared
        .confirm(true)
        .err()
        .expect("alter Epoch darf nach Restsicherung nicht gelten");
    assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
    let fresh = prepare_local_sync_run_for_queue(
        &f.queue,
        &f.source,
        &f.target,
        SyncOptions::default(),
        None,
    )
    .unwrap();
    let stats = fresh
        .confirm(true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(stats.files_copied, 1);
    assert_eq!(
        std::fs::read(saved.join("private-body")).unwrap(),
        b"private partial"
    );
}
#[test]
fn stage_aba_during_initial_scan_invalidates_before_effects() {
    let f = Fixture::new();
    let source = f.source.clone();
    let (owner, _, _) = f.other.ftps_owner(None).unwrap();
    SCAN_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let (scope, path) = stage(&source, &owner, false);
            scope.finish_staging().unwrap();
            drop(scope);
            assert!(!path.exists());
        }))
    });
    let error = prepare_local_sync_run_for_queue(
        &f.queue,
        &f.source,
        &f.target,
        SyncOptions::default(),
        None,
    )
    .err()
    .expect("neu entstandener und abgeschlossener Stage muss alten Scan invalidieren");
    assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
    assert!(!f.target.join("ordinary").exists());
}
#[test]
fn ownerless_prepare_stops_before_mutating_probes() {
    let f = Fixture::new();
    assert_eq!(
        crate::error_code(
            &prepare_local_sync_run(&f.source, &f.target, SyncOptions::default())
                .err()
                .unwrap()
        ),
        ErrorCode::Unsupported
    );
    assert_eq!(
        crate::error_code(
            &prepare_local_real_run(
                &f.source,
                &f.target,
                SyncOptions {
                    mode: SyncMode::Mirror,
                    compare: Compare::ContentHash,
                    ..Default::default()
                }
            )
            .err()
            .unwrap()
        ),
        ErrorCode::Unsupported
    );
    assert!(!f.area.path().join(".doppeldecker-quarantine").exists());
    assert!(!f.target.join("ordinary").exists());
}

#[test]
fn foreign_stage_after_terminal_queue_commit_belongs_to_next_run() {
    let f = Fixture::new();
    let audit = f.area.path().join("audit.jsonl");
    let prepared = prepare_local_sync_run_for_queue(
        &f.queue,
        &f.source,
        &f.target,
        SyncOptions::default(),
        None,
    )
    .unwrap();
    let entry = f
        .queue
        .enqueue(crate::QueueOperation::LocalSync {
            source: f.source.to_string_lossy().into_owned(),
            target: f.target.to_string_lossy().into_owned(),
            options: SyncOptions::default(),
            sandbox: None,
            expected_plan: Box::new(prepared.preview().plan.clone()),
            execution_hint: prepared.preview().local_execution.clone(),
        })
        .unwrap();
    f.queue.claim_next().unwrap().unwrap();
    let run = prepared
        .confirm(true)
        .unwrap()
        .with_queue_staging_owner(&f.queue, &entry.operation_id)
        .unwrap();
    f.queue.mark_running(&entry.operation_id).unwrap();
    let mut latest = Progress::default();
    let success = run
        .execute_with_audit_detailed(&audit, &Cancel::new(), &mut |progress| {
            latest = progress.clone()
        })
        .unwrap();
    success.validate_staging_completion().unwrap();
    f.queue.complete(&entry.operation_id, latest).unwrap();
    let (_scope, path) = f.stage(&f.source);
    let stats = success.append_completion_audit().expect(
        "später fremder Stage darf abgeschlossene Wirkung und terminalen Audit nicht zurücknehmen",
    );
    assert_eq!(stats.files_copied, 1);
    assert_eq!(
        f.queue.snapshot().entries[0].state,
        crate::QueueState::Completed
    );
    assert!(std::fs::read_to_string(audit)
        .unwrap()
        .contains("\"status\":\"completed\""));
    assert!(path.exists());
}

fn provider_preparation(f: &Fixture) -> io::Result<crate::PreparedProviderRealRun> {
    crate::prepare_provider_transfer_for_queue(
        &f.queue,
        crate::ProviderEndpoint::local(&f.source)?,
        crate::ProviderEndpoint::test_sftp_local(&f.target)?,
        SyncOptions {
            compare: Compare::ContentHash,
            ..Default::default()
        },
    )
}
#[test]
fn provider_factory_blocks_registered_foreign_stage_before_probe_or_plan() {
    let f = Fixture::new();
    let (_scope, path) = f.stage(&f.source);
    let error = provider_preparation(&f)
        .err()
        .expect("registrierter SourceStage muss vor Providerproben sperren");
    assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
    assert!(crate::error::recovery_paths(&error)
        .iter()
        .any(|value| Path::new(value) == path));
    assert!(!f.target.join("ordinary").exists());
    assert!(!f.area.path().join(".doppeldecker-quarantine").exists());
    let error = crate::prepare_provider_transfer(
        crate::ProviderEndpoint::local(&f.source).unwrap(),
        crate::ProviderEndpoint::test_sftp_local(&f.target).unwrap(),
        SyncOptions::default(),
    )
    .err()
    .expect("roher Providerstart ohne Kontext muss vor Probe stoppen");
    assert_eq!(crate::error_code(&error), ErrorCode::Unsupported);
}
#[test]
fn provider_factory_late_intent_aba_invalidates_confirmation() {
    let f = Fixture::new();
    let prepared = provider_preparation(&f).unwrap();
    let (owner, _, _) = f.other.ftps_owner(None).unwrap();
    let (scope, path) = stage(&f.source, &owner, false);
    scope.finish_staging().unwrap();
    drop(scope);
    assert!(!path.exists());
    let error = prepared
        .confirm_effects(true)
        .err()
        .expect("später vollständig abgeschlossener Intent muss die alte Vorschau invalidieren");
    assert_eq!(crate::error_code(&error), ErrorCode::PlanDrift);
    assert!(!f.target.join("ordinary").exists());
}
#[test]
fn provider_factory_foreign_stage_after_first_copy_stops_second_effect() {
    let f = Fixture::new();
    std::fs::write(f.source.join("second"), b"second").unwrap();
    let prepared = provider_preparation(&f).unwrap();
    let mut live = None;
    let mut latest = Progress::default();
    let error = prepared
        .confirm_effects(true)
        .unwrap()
        .execute(&Cancel::new(), &mut |progress| {
            latest = progress.clone();
            if progress.files_done == 1 && live.is_none() {
                live = Some(f.stage(&f.source));
            }
        })
        .unwrap_err();
    assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
    assert!(live.is_some());
    assert_eq!(
        ["ordinary", "second"]
            .iter()
            .filter(|path| f.target.join(path).exists())
            .count(),
        1
    );
    assert_eq!(latest.files_done, 1);
}
#[test]
fn provider_factory_success_keeps_full_terminal_audit_and_settles_own_stage() {
    let f = Fixture::new();
    let audit = f.area.path().join("provider-audit.jsonl");
    let prepared = provider_preparation(&f).unwrap();
    let stats = prepared
        .confirm_effects(true)
        .unwrap()
        .execute_with_audit(&audit, &Cancel::new(), &mut |_| {})
        .unwrap();
    assert_eq!(stats.files_copied, 1);
    assert_eq!(std::fs::read(f.target.join("ordinary")).unwrap(), b"source");
    assert!(std::fs::read_to_string(audit)
        .unwrap()
        .contains("\"status\":\"completed\""));
}

#[test]
fn direct_local_audit_checks_own_remaining_stage_before_completion() {
    let f = Fixture::new();
    let audit = f.area.path().join("direct-local-audit.jsonl");
    let prepared = prepare_local_sync_run_for_queue(
        &f.queue,
        &f.source,
        &f.target,
        SyncOptions::default(),
        None,
    )
    .unwrap();
    let run = prepared.confirm(true).unwrap();
    let owner = run
        .safety
        .real_permit()
        .unwrap()
        .staging
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .owner
        .clone();
    let mut live = None;
    let error = run
        .execute_with_audit(&audit, &Cancel::new(), &mut |progress| {
            if progress.files_done == 1 && live.is_none() {
                live = Some(stage(&f.target, &owner, true));
            }
        })
        .unwrap_err();
    assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
    assert!(live.is_some());
    assert!(f.target.join("ordinary").exists());
    assert!(!std::fs::read_to_string(audit)
        .unwrap()
        .contains("\"status\":\"completed\""));
}
