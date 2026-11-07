//! Forward-Bidirsync über echte HTTPS-/WebDAV-Pakete im künstlichen Loopbackroot.
#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::bidirectional::forward::*;
use doppeldecker_core::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn endpoint(config: &WebDavConnectionConfig) -> ProviderEndpoint {
    endpoint_at_root(config, "/pane")
}
fn endpoint_at_root(config: &WebDavConnectionConfig, root: &str) -> ProviderEndpoint {
    ProviderEndpoint::webdav(
        Arc::new(WebDavVfs::connect(config.clone(), Arc::new(server::TestSecrets)).unwrap()),
        Path::new(root),
    )
    .unwrap()
}

fn prepare(
    queue: &OperationQueue,
    pair: PairId,
    left: &Path,
    config: &WebDavConnectionConfig,
    baseline: &Path,
    recovery: bool,
) -> PreparedForwardBidirectional {
    prepare_forward_bidirectional_for_queue(
        queue,
        pair,
        ProviderEndpoint::local(left).unwrap(),
        endpoint(config),
        baseline,
        &[],
        recovery,
        None,
        &Cancel::new(),
    )
    .unwrap()
}
fn prepare_at_root(
    queue: &OperationQueue,
    pair: PairId,
    left: &Path,
    config: &WebDavConnectionConfig,
    baseline: &Path,
    recovery: bool,
    remote_root: &str,
) -> PreparedForwardBidirectional {
    prepare_forward_bidirectional_for_queue(
        queue,
        pair,
        ProviderEndpoint::local(left).unwrap(),
        endpoint_at_root(config, remote_root),
        baseline,
        &[],
        recovery,
        None,
        &Cancel::new(),
    )
    .unwrap()
}

fn execute(
    queue: &OperationQueue,
    prepared: PreparedForwardBidirectional,
    config: &WebDavConnectionConfig,
    baseline: &Path,
) {
    use doppeldecker_core::queue::{
        bind_prepared_bidirectional_forward_queue_entry,
        execute_prepared_bidirectional_forward_queue_entry,
    };
    let preview = prepared.preview().clone();
    queue
        .enqueue(QueueOperation::BidirectionalForward {
            pair_id: preview.plan.pair_id,
            pair_name: "WebDAV-B0".into(),
            left: Box::new(BidirectionalQueueEndpoint::Local {
                root: prepared
                    .left_endpoint()
                    .root()
                    .to_string_lossy()
                    .into_owned(),
            }),
            right: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(config.clone()),
                root: prepared
                    .right_endpoint()
                    .root()
                    .to_string_lossy()
                    .into_owned(),
            }),
            baseline_path: baseline.to_string_lossy().into_owned(),
            resolutions: vec![],
            expected_baseline_digest: preview.plan.baseline_digest,
            expected_plan_digest: preview.plan.plan_digest,
            expected_unresolved_digest: preview.unresolved_digest,
            expected_preview_digest: preview.digest,
        })
        .unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let prepared =
        bind_prepared_bidirectional_forward_queue_entry(queue, &entry, prepared, &Cancel::new())
            .unwrap();
    let digest = prepared.preview().digest;
    execute_prepared_bidirectional_forward_queue_entry(
        queue,
        prepared,
        digest,
        true,
        &[],
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
}
#[test]
fn webdav_forward_direct_mutation_requires_persistent_queue_owner() {
    let server = server::Server::new();
    server.state.lock().unwrap().allow_file_overwrite = true;
    let area = tempfile::tempdir().unwrap();
    let left = area.path().join("left");
    std::fs::create_dir(&left).unwrap();
    let baseline = area.path().join("baseline.json");
    let pair = PairId::generate().unwrap();
    let queue = OperationQueue::open(area.path().join("config/operations-queue.json")).unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(left.join(name), name).unwrap();
        std::fs::write(server.root.path().join("pane").join(name), name).unwrap();
    }
    let unowned = prepare(&queue, pair, &left, &server.config, &baseline, false);
    let digest = unowned.preview().digest;
    assert_eq!(
        error_code(
            &unowned
                .confirm_effects(digest, true)
                .err()
                .expect("Kein ownerloser B1-Commit")
        ),
        ErrorCode::Unsupported
    );
    assert!(!baseline.exists());
    execute(
        &queue,
        prepare(&queue, pair, &left, &server.config, &baseline, false),
        &server.config,
        &baseline,
    );
    let b0 = std::fs::read(&baseline).unwrap();
    std::fs::write(left.join("a"), b"new").unwrap();
    let prepared = prepare(&queue, pair, &left, &server.config, &baseline, false);
    assert!(!prepared.preview().effects.is_empty());
    let digest = prepared.preview().digest;
    let error = prepared
        .confirm_effects(digest, true)
        .err()
        .expect("Kein ownerloser WRITE");
    assert_eq!(error_code(&error), ErrorCode::Unsupported);
    assert_eq!(std::fs::read(&baseline).unwrap(), b0);
    assert_eq!(
        std::fs::read(server.root.path().join("pane/a")).unwrap(),
        b"a"
    );
    assert!(forward_bidirectional_journal_status(&baseline)
        .unwrap()
        .is_none());
}

#[test]
fn webdav_forward_queue_child_exits_after_first_committed_file() {
    use doppeldecker_core::queue::{
        execute_prepared_bidirectional_forward_queue_entry,
        prepare_bidirectional_forward_queue_entry,
    };
    let Some(root) = std::env::var_os("DOPPELDECKER_WEBDAV_FORWARD_QUEUE_CRASH") else {
        return;
    };
    let root = PathBuf::from(root);
    let queue = OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let prepared = prepare_bidirectional_forward_queue_entry(
        &queue,
        entry,
        Arc::new(server::TestSecrets),
        &Cancel::new(),
    )
    .unwrap();
    let digest = prepared.preview().digest;
    execute_prepared_bidirectional_forward_queue_entry(
        &queue,
        prepared,
        digest,
        true,
        &[],
        &Cancel::new(),
        &mut |progress| {
            if progress.files_done == 1 {
                std::process::exit(17);
            }
        },
    )
    .unwrap();
    panic!("Erster Queuecommit wurde nicht erreicht");
}

#[test]
fn webdav_forward_queue_owner_survives_crash_and_requires_fresh_recovery() {
    run_webdav_forward_queue_case("/pane");
}
#[test]
fn webdav_forward_queue_profile_namespace_root_survives_crash_and_recovery() {
    run_webdav_forward_queue_case("/");
}
fn run_webdav_forward_queue_case(remote_root: &str) {
    use doppeldecker_core::queue::{
        execute_prepared_bidirectional_forward_queue_entry,
        prepare_bidirectional_forward_queue_recovery,
    };
    let server = server::Server::new();
    {
        let mut state = server.state.lock().unwrap();
        state.allow_file_overwrite = true;
        state.changing_collection_tokens = true;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let left = root.join("left");
    std::fs::create_dir(&left).unwrap();
    let baseline = root.join("baseline.json");
    let pair = PairId::generate().unwrap();
    let remote = if remote_root == "/" {
        std::fs::remove_dir(server.root.path().join("pane")).unwrap();
        server.root.path().to_path_buf()
    } else {
        server.root.path().join("pane")
    };
    for name in ["a", "b", "c"] {
        std::fs::write(left.join(name), name).unwrap();
        std::fs::write(remote.join(name), name).unwrap();
    }
    let queue = OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
    execute(
        &queue,
        prepare_at_root(
            &queue,
            pair,
            &left,
            &server.config,
            &baseline,
            false,
            remote_root,
        ),
        &server.config,
        &baseline,
    );
    let b0 = std::fs::read(&baseline).unwrap();
    std::fs::write(left.join("a"), "a-new").unwrap();
    std::fs::write(left.join("b"), "b-new").unwrap();
    std::fs::write(remote.join("c"), "c-new").unwrap();
    let prepared = prepare_at_root(
        &queue,
        pair,
        &left,
        &server.config,
        &baseline,
        false,
        remote_root,
    );
    let preview = prepared.preview().clone();
    prepared.discard().unwrap();
    let entry = queue
        .enqueue(QueueOperation::BidirectionalForward {
            pair_id: pair,
            pair_name: "WebDAV-Forwardqueue".into(),
            left: Box::new(BidirectionalQueueEndpoint::Local {
                root: left.to_string_lossy().into_owned(),
            }),
            right: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(server.config.clone()),
                root: remote_root.into(),
            }),
            baseline_path: baseline.to_string_lossy().into_owned(),
            resolutions: vec![],
            expected_baseline_digest: preview.plan.baseline_digest,
            expected_plan_digest: preview.plan.plan_digest,
            expected_unresolved_digest: preview.unresolved_digest,
            expected_preview_digest: preview.digest,
        })
        .unwrap();
    let id = entry.operation_id;
    drop(queue);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "webdav_forward_queue_child_exits_after_first_committed_file",
            "--nocapture",
        ])
        .env("DOPPELDECKER_WEBDAV_FORWARD_QUEUE_CRASH", &root)
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(17),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(std::fs::read(&baseline).unwrap(), b0);
    assert_eq!(std::fs::read(remote.join("a")).unwrap(), b"a-new");
    assert_eq!(std::fs::read(remote.join("b")).unwrap(), b"b");
    let queue = OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
    let entry = queue
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.operation_id == id)
        .unwrap();
    assert_eq!(entry.state, QueueState::Interrupted);
    assert!(entry.recovery_available);
    assert!(queue.retry(&id).is_err());
    assert!(queue.claim_next().unwrap().is_none());
    let prepared = prepare_bidirectional_forward_queue_recovery(
        &queue,
        &id,
        &[],
        None,
        Arc::new(server::TestSecrets),
        &Cancel::new(),
    )
    .unwrap();
    assert!(prepared
        .preview()
        .recovery
        .as_ref()
        .unwrap()
        .changed_right
        .iter()
        .any(|path| path.display_lossy() == "a"));
    let digest = prepared.preview().digest;
    let QueueExecutionResult::Bidirectional(stats) =
        execute_prepared_bidirectional_forward_queue_entry(
            &queue,
            prepared,
            digest,
            true,
            &[],
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap()
    else {
        panic!("Forwardbilanz fehlt")
    };
    assert_eq!(stats.baseline_generation, 2);
    assert_eq!(
        stats.execution_class,
        Some(doppeldecker_core::sync::SyncExecutionClass::BestEffort)
    );
    let entry = queue
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.operation_id == id)
        .unwrap();
    assert_eq!(entry.state, QueueState::Completed);
    assert!(entry.bidirectional_completion_recovery.is_none());
    assert!(forward_bidirectional_journal_status(&baseline)
        .unwrap()
        .is_none());
    for name in ["a", "b", "c"] {
        assert_eq!(
            std::fs::read(left.join(name)).unwrap(),
            std::fs::read(remote.join(name)).unwrap()
        );
    }
    let audit = std::fs::read_to_string(root.join("config/operations-audit.jsonl")).unwrap();
    assert!(audit
        .lines()
        .any(|line| line.contains("bidirectional_forward") && line.contains("started")));
    assert!(audit
        .lines()
        .any(|line| line.contains("bidirectional_forward") && line.contains("completed")));
    let no_op = prepare_at_root(
        &queue,
        pair,
        &left,
        &server.config,
        &baseline,
        false,
        remote_root,
    );
    assert!(no_op.preview().effects.is_empty());
    let before_no_op = std::fs::read(&baseline).unwrap();
    execute(&queue, no_op, &server.config, &baseline);
    assert_eq!(std::fs::read(&baseline).unwrap(), before_no_op);
}
