//! Öffentliche Core-E2Es binden echte, isolierte Queueeinträge vor Schreibpermits.
thread_local! {
    static FIXTURES: std::cell::RefCell<Vec<tempfile::TempDir>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[allow(dead_code)]
pub fn prepare(
    source: doppeldecker_core::ProviderEndpoint,
    target: doppeldecker_core::ProviderEndpoint,
    options: doppeldecker_core::SyncOptions,
    cancel: &doppeldecker_core::Cancel,
) -> std::io::Result<doppeldecker_core::PreparedContentSync> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let root = fixture.path().canonicalize()?;
    let queue = doppeldecker_core::OperationQueue::open(root.join("operations-queue.json"))?;
    let prepared =
        doppeldecker_core::prepare_content_sync_for_queue(&queue, source, target, options, cancel)?;
    queue.enqueue(prepared.queue_operation())?;
    let entry = queue
        .claim_next()?
        .ok_or_else(|| std::io::Error::other("Isolierter Testauftrag fehlt"))?;
    let prepared = doppeldecker_core::queue::bind_prepared_content_sync_queue_entry(
        &queue, &entry, prepared, cancel,
    )?;
    FIXTURES.with(|fixtures| fixtures.borrow_mut().push(fixture));
    Ok(prepared)
}

#[allow(dead_code, clippy::too_many_arguments)]
pub fn forward(
    pair: doppeldecker_core::PairId,
    source: doppeldecker_core::ProviderEndpoint,
    target: doppeldecker_core::ProviderEndpoint,
    baseline: &std::path::Path,
    resolutions: &[doppeldecker_core::ConflictResolution],
    recovery: bool,
    expected: Option<[u8; 32]>,
    cancel: &doppeldecker_core::Cancel,
) -> std::io::Result<doppeldecker_core::bidirectional::forward::PreparedForwardBidirectional> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let root = fixture.path().canonicalize()?;
    let queue = doppeldecker_core::OperationQueue::open(root.join("operations-queue.json"))?;
    let prepared =
        doppeldecker_core::bidirectional::forward::prepare_forward_bidirectional_for_queue(
            &queue,
            pair,
            source,
            target,
            baseline,
            resolutions,
            recovery,
            expected,
            cancel,
        )?;
    FIXTURES.with(|fixtures| fixtures.borrow_mut().push(fixture));
    Ok(prepared)
}
