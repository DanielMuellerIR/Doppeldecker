//! Eigene Queueklasse für Forward-Bidirsync. Ein offener Teilauftrag bleibt
//! nicht-terminal, bis eine frische Drei-Wege-Entscheidung ihn bewusst ersetzt.
use super::*;
use crate::bidirectional::forward as engine;

pub struct PreparedBidirectionalForwardQueueRun {
    entry: QueueEntry,
    prepared: engine::PreparedForwardBidirectional,
    resolutions: Vec<ConflictResolution>,
}
impl PreparedBidirectionalForwardQueueRun {
    pub fn operation_id(&self) -> &str {
        &self.entry.operation_id
    }
    pub fn preview(&self) -> &engine::ForwardBidirectionalPreview {
        self.prepared.preview()
    }
    pub fn left_endpoint(&self) -> &ProviderEndpoint {
        self.prepared.left_endpoint()
    }
    pub fn right_endpoint(&self) -> &ProviderEndpoint {
        self.prepared.right_endpoint()
    }

    pub fn conflict_choices_after_decisions(
        &self,
        resolutions: &[ConflictResolution],
        expected_digest: [u8; 32],
    ) -> io::Result<Vec<crate::bidirectional::BidirectionalConflictView>> {
        self.prepared
            .conflict_choices_after_decisions(resolutions, expected_digest)
    }
}
fn confirmed_operation(prepared: &PreparedBidirectionalForwardQueueRun) -> QueueOperation {
    let mut operation = prepared.entry.operation.clone();
    if let QueueOperation::BidirectionalForward {
        expected_baseline_digest,
        expected_plan_digest,
        expected_unresolved_digest,
        expected_preview_digest,
        resolutions,
        ..
    } = &mut operation
    {
        let preview = prepared.preview();
        *expected_baseline_digest = preview.plan.baseline_digest;
        *expected_plan_digest = preview.plan.plan_digest;
        *expected_unresolved_digest = preview.unresolved_digest;
        *expected_preview_digest = preview.digest;
        *resolutions = prepared.resolutions.clone();
    }
    operation
}
impl OperationQueue {
    /// Stellt dieselbe RAM-Vorbereitung für den normalen Worker bereit.
    /// Der persistierte Pendingzustand enthält weiterhin kein Permit.
    pub fn confirm_bidirectional_forward_preparation(
        &self,
        id: &str,
        prepared: &PreparedBidirectionalForwardQueueRun,
    ) -> io::Result<()> {
        if prepared.operation_id() != id || prepared.preview().stage_blocker.is_some() {
            return Err(crate::real_mode::confirmation_mismatch(
                "Keine ausführbare passende Forwardvorbereitung",
            ));
        }
        let operation = confirmed_operation(prepared);
        self.update(id,|entry| {
            require_state(entry,QueueState::AwaitingConfirmation)?;
            if entry.profile_bindings!=prepared.entry.profile_bindings || (entry.operation!=prepared.entry.operation && entry.operation!=operation)
                || !matches!(&entry.attention,Some(QueueAttention::RemoteRun {confirmation,..}) if confirmation==&digest_text(&prepared.preview().digest)) {
                return Err(crate::real_mode::plan_drift("Forwardqueue änderte sich vor der Workerfreigabe"));
            }
            entry.operation=operation;entry.state=QueueState::Pending;entry.attention=None;entry.error=None;entry.error_code=None;
            Ok(())
        })
    }
}

fn digest_text(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn append_forward_audit(
    queue: &OperationQueue,
    id: &str,
    status: &str,
    preview: &engine::ForwardBidirectionalPreview,
    progress: &Progress,
) -> io::Result<()> {
    for effect in &preview.effects {
        let context = ProviderAuditContext {
            source_provider: Some(effect.source_provider),
            target_provider: effect.target_provider,
            execution_class: ProviderAuditExecutionClass::BestEffort,
            source_removal_best_effort_expected: false,
            target_copy_new_best_effort_expected: effect.plan.copy_count > 0,
            target_replace_best_effort_expected: effect.irreversible_replacements > 0,
            source_removal_best_effort_actual: None,
            target_copy_new_best_effort_actual: None,
            target_replace_best_effort_actual: None,
        };
        queue.append_file_audit(
            id,
            "bidirectional_forward",
            status,
            progress,
            &context,
            None,
        )?;
    }
    Ok(())
}
fn current_entry(queue: &OperationQueue, id: &str) -> io::Result<QueueEntry> {
    queue
        .snapshot()
        .entries
        .into_iter()
        .find(|entry| entry.operation_id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Forward-Queueeintrag fehlt"))
}
fn forward_baseline(operation: &QueueOperation) -> io::Result<&Path> {
    let QueueOperation::BidirectionalForward { baseline_path, .. } = operation else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Keine bidirektionale Forwardqueue",
        ));
    };
    Ok(Path::new(baseline_path))
}
fn prepare_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
    resolutions: Vec<ConflictResolution>,
    expected_unresolved: Option<[u8; 32]>,
    recovery: bool,
    cancel: &Cancel,
) -> io::Result<PreparedBidirectionalForwardQueueRun> {
    validate_operation(&entry.operation)?;
    let current = current_entry(queue, &entry.operation_id)?;
    if current.operation != entry.operation
        || current.profile_bindings != entry.profile_bindings
        || current.state != QueueState::Validating
    {
        return Err(crate::real_mode::plan_drift(
            "Forwardqueue änderte sich vor der Vorbereitung",
        ));
    }
    let QueueOperation::BidirectionalForward {
        pair_id,
        left,
        right,
        baseline_path,
        ..
    } = &entry.operation
    else {
        unreachable!()
    };
    let left = left
        .connect(secrets.clone())?
        .with_queue_owner_with_cancel(queue, Some(&entry.operation_id), &[], Some(cancel))?;
    let right = right.connect(secrets)?.with_queue_owner_with_cancel(
        queue,
        Some(&entry.operation_id),
        &[],
        Some(cancel),
    )?;
    let (staging, _, _) = queue.ftps_owner(Some(&entry.operation_id))?;
    let owner = crate::remote_real_mode::InteractiveFileOwner {
        staging,
        profiles: entry.profile_bindings.clone(),
    };
    let prepared = engine::prepare_forward_bidirectional_owned(
        *pair_id,
        left,
        right,
        Path::new(baseline_path),
        &resolutions,
        recovery,
        expected_unresolved,
        cancel,
        Some(owner),
    )?;
    let reason = if recovery {
        "Teilauftrag wurde gegen die unveränderte letzte gemeinsame Baseline frisch gelesen. Alte Konfliktentscheidungen werden nicht übernommen; neue Wirkungen ausdrücklich bestätigen."
    } else {
        "Forward-Bidirsync ist frisch gelesen. Beide Richtungen, irreversible Wirkungen und verfügbare Quarantäne ausdrücklich bestätigen."
    };
    if let Err(error) = queue.require_confirmation(
        &entry.operation_id,
        QueueAttention::RemoteRun {
            reason: reason.into(),
            confirmation: digest_text(&prepared.preview().digest),
        },
    ) {
        let _ = prepared.discard();
        return Err(error);
    }
    Ok(PreparedBidirectionalForwardQueueRun {
        entry,
        prepared,
        resolutions,
    })
}

pub fn prepare_bidirectional_forward_queue_entry(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
) -> io::Result<PreparedBidirectionalForwardQueueRun> {
    let id = entry.operation_id.clone();
    match prepare_bidirectional_forward_queue_entry_inner(queue, entry, secrets, cancel) {
        Ok(prepared) => Ok(prepared),
        Err(error) => Err(settle_forward_error(queue, &id, error)),
    }
}
fn prepare_bidirectional_forward_queue_entry_inner(
    queue: &OperationQueue,
    entry: QueueEntry,
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
) -> io::Result<PreparedBidirectionalForwardQueueRun> {
    let QueueOperation::BidirectionalForward {
        resolutions,
        expected_unresolved_digest,
        ..
    } = &entry.operation
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kein Forwardqueueauftrag",
        ));
    };
    let resolutions = resolutions.clone();
    let expected = Some(*expected_unresolved_digest);
    match prepare_entry(
        queue,
        entry.clone(),
        secrets.clone(),
        resolutions,
        expected,
        false,
        cancel,
    ) {
        Ok(prepared) => Ok(prepared),
        Err(error) if crate::error_code(&error) == crate::ErrorCode::PlanDrift => {
            prepare_entry(queue, entry, secrets, Vec::new(), None, false, cancel)
        }
        Err(error) => Err(error),
    }
}

/// Bindet eine bereits angezeigte und bestätigte RAM-Vorbereitung ohne neue
/// Wirkungsfrage an den exakt dazu geclaimten Fensterqueueauftrag.
pub fn bind_prepared_bidirectional_forward_queue_entry(
    queue: &OperationQueue,
    entry: &QueueEntry,
    prepared: engine::PreparedForwardBidirectional,
    cancel: &Cancel,
) -> io::Result<PreparedBidirectionalForwardQueueRun> {
    match bind_prepared_bidirectional_forward_queue_entry_inner(queue, entry, prepared, cancel) {
        Ok(prepared) => Ok(prepared),
        Err(error) => Err(settle_forward_error(queue, &entry.operation_id, error)),
    }
}
fn bind_prepared_bidirectional_forward_queue_entry_inner(
    queue: &OperationQueue,
    entry: &QueueEntry,
    prepared: engine::PreparedForwardBidirectional,
    cancel: &Cancel,
) -> io::Result<PreparedBidirectionalForwardQueueRun> {
    validate_operation(&entry.operation)?;
    let current = current_entry(queue, &entry.operation_id)?;
    if current.operation != entry.operation
        || current.profile_bindings != entry.profile_bindings
        || current.state != QueueState::Validating
    {
        return Err(crate::real_mode::plan_drift(
            "Forwardqueue änderte sich vor der RAM-Übernahme",
        ));
    }
    let QueueOperation::BidirectionalForward {
        pair_id,
        left,
        right,
        resolutions,
        expected_baseline_digest,
        expected_plan_digest,
        expected_unresolved_digest,
        expected_preview_digest,
        ..
    } = &entry.operation
    else {
        return Err(invalid_transition(entry.state));
    };
    let preview = prepared.preview();
    if preview.plan.pair_id != *pair_id
        || preview.plan.baseline_digest != *expected_baseline_digest
        || preview.plan.plan_digest != *expected_plan_digest
        || preview.unresolved_digest != *expected_unresolved_digest
        || preview.digest != *expected_preview_digest
        || !prepared.matches_queue_endpoints(left, right)
    {
        return Err(crate::real_mode::plan_drift(
            "Forward-RAM-Vorbereitung passt nicht zur exakt angezeigten Queueabsicht",
        ));
    }
    let prepared =
        prepared.with_queue_owner(queue, &entry.operation_id, &entry.profile_bindings, cancel)?;
    queue.require_confirmation(&entry.operation_id,QueueAttention::RemoteRun {reason:"Die angezeigte Forwardwirkung ist an diese Fensterqueue gebunden; dieselbe Wirkungsbestätigung gilt weiter.".into(),confirmation:digest_text(&prepared.preview().digest)})?;
    Ok(PreparedBidirectionalForwardQueueRun {
        entry: entry.clone(),
        prepared,
        resolutions: resolutions.clone(),
    })
}

/// Ein ausschließlich menschlich ausgelöster Recoveryeinstieg. Er setzt den alten
/// Auftrag nicht Pending und erteilt kein neues Ausführungsrecht.
pub fn prepare_bidirectional_forward_queue_recovery(
    queue: &OperationQueue,
    id: &str,
    resolutions: &[ConflictResolution],
    expected_unresolved_digest: Option<[u8; 32]>,
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
) -> io::Result<PreparedBidirectionalForwardQueueRun> {
    let entry = current_entry(queue, id)?;
    let baseline = forward_baseline(&entry.operation)?;
    let status = engine::forward_bidirectional_journal_status(baseline)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Kein aktiver Forwardauftrag für Recovery",
        )
    })?;
    if status.baseline_completion_available {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Datenwirkung ist abgeschlossen; ausschließlich Forwardabschluss ausführen",
        ));
    }
    queue.transact(|journal| {
        if journal.entries.iter().any(|entry| {
            entry.operation_id != id
                && matches!(entry.state, QueueState::Validating | QueueState::Running)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Eine andere Fensteroperation läuft",
            ));
        }
        let entry = find_mut(journal, id)?;
        if !matches!(
            entry.state,
            QueueState::Interrupted | QueueState::AwaitingConfirmation
        ) || !matches!(entry.operation, QueueOperation::BidirectionalForward { .. })
        {
            return Err(invalid_transition(entry.state));
        }
        entry.state = QueueState::Validating;
        entry.attention = None;
        entry.recovery_available = true;
        Ok(())
    })?;
    let entry = current_entry(queue, id)?;
    match prepare_entry(
        queue,
        entry,
        secrets,
        resolutions.to_vec(),
        expected_unresolved_digest,
        true,
        cancel,
    ) {
        Ok(prepared) => Ok(prepared),
        Err(error) => Err(interrupt_forward_error(
            queue,
            id,
            Progress::default(),
            error,
        )),
    }
}

/// Neue Konfliktentscheidungen aus derselben frisch angezeigten Dreierplanung.
/// Funktioniert auch bei einem normalen Queue-Claim ohne aktives Recoveryjournal.
pub fn reprepare_bidirectional_forward_queue_entry_decisions(
    queue: &OperationQueue,
    id: &str,
    resolutions: &[ConflictResolution],
    expected_unresolved_digest: [u8; 32],
    secrets: Arc<dyn SecretResolver>,
    cancel: &Cancel,
) -> io::Result<PreparedBidirectionalForwardQueueRun> {
    let entry = current_entry(queue, id)?;
    let status = engine::forward_bidirectional_journal_status(forward_baseline(&entry.operation)?)?;
    if status
        .as_ref()
        .is_some_and(|status| status.baseline_completion_available)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Nur der mutationslose Forwardabschluss ist noch offen",
        ));
    }
    let recovery = status.is_some();
    queue.transact(|journal| {
        if journal.entries.iter().any(|entry| {
            entry.operation_id != id
                && matches!(entry.state, QueueState::Validating | QueueState::Running)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Eine andere Fensteroperation läuft",
            ));
        }
        let entry = find_mut(journal, id)?;
        if !matches!(
            entry.state,
            QueueState::Pending | QueueState::AwaitingConfirmation | QueueState::Interrupted
        ) {
            return Err(invalid_transition(entry.state));
        }
        entry.state = QueueState::Validating;
        entry.attention = None;
        Ok(())
    })?;
    match prepare_entry(
        queue,
        current_entry(queue, id)?,
        secrets,
        resolutions.to_vec(),
        Some(expected_unresolved_digest),
        recovery,
        cancel,
    ) {
        Ok(prepared) => Ok(prepared),
        Err(error) => Err(settle_forward_error(queue, id, error)),
    }
}

pub fn discard_prepared_bidirectional_forward_queue_entry(
    queue: &OperationQueue,
    prepared: PreparedBidirectionalForwardQueueRun,
) -> io::Result<()> {
    cleanup_prepared_forward(queue, prepared, false)
}
pub fn cancel_prepared_bidirectional_forward_queue_entry(
    queue: &OperationQueue,
    prepared: PreparedBidirectionalForwardQueueRun,
) -> io::Result<()> {
    cleanup_prepared_forward(queue, prepared, true)
}
fn cleanup_prepared_forward(
    queue: &OperationQueue,
    prepared: PreparedBidirectionalForwardQueueRun,
    cancel: bool,
) -> io::Result<()> {
    let id = prepared.operation_id().to_owned();
    let result = (|| {
        let baseline = forward_baseline(&prepared.entry.operation)?.to_path_buf();
        prepared.prepared.discard()?;
        let active = engine::forward_bidirectional_journal_status(&baseline)?.is_some();
        queue.update(&id, |entry| {
            if !matches!(
                entry.state,
                QueueState::AwaitingConfirmation | QueueState::Pending
            ) {
                return Err(invalid_transition(entry.state));
            }
            entry.state = if active {
                QueueState::Interrupted
            } else if cancel {
                QueueState::Cancelled
            } else {
                QueueState::Pending
            };
            entry.attention = None;
            entry.recovery_available = active;
            entry.error = active.then(|| {
                "Forwardteilauftrag bleibt bis zu einer frischen Recoveryentscheidung gesperrt"
                    .into()
            });
            entry.error_code = active.then_some(crate::ErrorCode::OutcomeUnclear);
            Ok(())
        })
    })();
    result.map_err(|error| settle_forward_error(queue, &id, error))
}

impl OperationQueue {
    pub fn interrupt_bidirectional_forward(
        &self,
        id: &str,
        progress: Progress,
        error: &io::Error,
    ) -> io::Result<()> {
        self.interrupt_bidirectional_forward_using(id, progress, error, |path, snapshot| {
            write_snapshot_bound(
                &self._process_lock.root,
                path,
                snapshot,
                AtomicWriteMode::Replace,
            )
        })
    }
    fn interrupt_bidirectional_forward_using(
        &self,
        id: &str,
        mut progress: Progress,
        error: &io::Error,
        write: impl FnOnce(&Path, &QueueSnapshot) -> io::Result<()>,
    ) -> io::Result<()> {
        let operation = current_entry(self, id)?.operation;
        let baseline = forward_baseline(&operation)?;
        // Auch ein unlesbares Journal ist kein Beweis für Abwesenheit.
        let status = engine::forward_bidirectional_journal_status(baseline);
        let active = !matches!(&status, Ok(None));
        let completion = matches!(&status,Ok(Some(status)) if status.baseline_completion_available);
        let paths = crate::error::recovery_paths(error);
        self.transact_using(
            |journal| {
                let entry = find_mut(journal, id)?;
                if !matches!(
                    entry.state,
                    QueueState::Pending
                        | QueueState::Validating
                        | QueueState::Running
                        | QueueState::Interrupted
                        | QueueState::AwaitingConfirmation
                ) {
                    return Err(invalid_transition(entry.state));
                }
                entry.state = QueueState::Interrupted;
                progress.current.clear();
                progress.phase = if completion {
                    "completion_unclear"
                } else if active {
                    "recovery"
                } else {
                    "interrupted"
                }
                .into();
                entry.progress = progress;
                entry.error = Some(error.to_string());
                entry.error_code = Some(if active {
                    crate::ErrorCode::OutcomeUnclear
                } else {
                    crate::error_code(error)
                });
                entry.attention = None;
                entry.recovery_available = active && !completion;
                entry.bidirectional_completion_recovery =
                    completion.then_some(BidirectionalCompletionRecovery::Complete);
                for path in paths {
                    if !entry.temp_recovery_paths.contains(&path) {
                        entry.temp_recovery_paths.push(path);
                    }
                }
                if journal.stop_on_error {
                    journal.paused = true;
                }
                Ok(())
            },
            write,
        )
    }
}

fn settle_forward_failure(queue: &OperationQueue, id: &str, error: &io::Error) -> io::Result<()> {
    let entry = current_entry(queue, id)?;
    if matches!(entry.operation, QueueOperation::BidirectionalForward { .. })
        && matches!(
            entry.state,
            QueueState::Pending
                | QueueState::Validating
                | QueueState::Running
                | QueueState::AwaitingConfirmation
                | QueueState::Interrupted
        )
    {
        queue.interrupt_bidirectional_forward(id, entry.progress, error)?;
    }
    Ok(())
}

fn preserve_forward_error(error: io::Error, followup: io::Result<()>, context: &str) -> io::Error {
    match followup {
        Ok(()) => error,
        Err(followup) => failed_queue_error_commit(&error, &followup, context),
    }
}
fn settle_forward_error(queue: &OperationQueue, id: &str, error: io::Error) -> io::Error {
    let followup = settle_forward_failure(queue, id, &error);
    preserve_forward_error(error, followup, "Forward-Recoverycommit")
}
fn interrupt_forward_error(
    queue: &OperationQueue,
    id: &str,
    progress: Progress,
    error: io::Error,
) -> io::Error {
    let followup = queue.interrupt_bidirectional_forward(id, progress, &error);
    preserve_forward_error(error, followup, "Forward-Interrupted-Commit")
}
fn forward_stats_paths(stats: &engine::ForwardBidirectionalStats) -> Vec<String> {
    stats
        .runs
        .iter()
        .flat_map(|run| run.temp_recovery_paths.iter().cloned())
        .collect()
}
fn forward_completion_error(error: io::Error, paths: &[String], context: &str) -> io::Error {
    crate::error::with_recovery_paths(
        crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            format!("Forwarddatenwirkung ist abgeschlossen; {context}: {error}"),
        ),
        &[paths, &crate::error::recovery_paths(&error)].concat(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_prepared_bidirectional_forward_queue_entry(
    queue: &OperationQueue,
    prepared: PreparedBidirectionalForwardQueueRun,
    digest: [u8; 32],
    effects_confirmed: bool,
    removal_phrases: &[String],
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    let id = prepared.entry.operation_id.clone();
    match execute_prepared_bidirectional_forward_queue_entry_inner(
        queue,
        prepared,
        digest,
        effects_confirmed,
        removal_phrases,
        cancel,
        on_progress,
    ) {
        Ok(result) => Ok(result),
        Err(error) => Err(settle_forward_error(queue, &id, error)),
    }
}
#[allow(clippy::too_many_arguments)]
fn execute_prepared_bidirectional_forward_queue_entry_inner(
    queue: &OperationQueue,
    prepared: PreparedBidirectionalForwardQueueRun,
    digest: [u8; 32],
    effects_confirmed: bool,
    removal_phrases: &[String],
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<QueueExecutionResult> {
    let id = prepared.entry.operation_id.clone();
    let current = current_entry(queue, &id)?;
    let confirmed_operation = confirmed_operation(&prepared);
    if (current.operation != prepared.entry.operation && current.operation != confirmed_operation)
        || current.profile_bindings != prepared.entry.profile_bindings
        || !matches!(
            current.state,
            QueueState::AwaitingConfirmation | QueueState::Validating
        )
        || (current.state == QueueState::Validating && current.operation != confirmed_operation)
    {
        return Err(crate::real_mode::plan_drift(
            "Forwardqueue oder Profilbindung änderte sich seit der Vorschau",
        ));
    }
    let QueueOperation::BidirectionalForward { left, right, .. } = &current.operation else {
        return Err(invalid_transition(current.state));
    };
    if !prepared.prepared.matches_queue_endpoints(left, right) {
        return Err(crate::real_mode::plan_drift(
            "Forwardendpunkte passen nicht zum Queueauftrag",
        ));
    }
    let preview = prepared.preview().clone();
    let run = prepared.prepared.confirm_effects_with_removal_phrases(
        digest,
        effects_confirmed,
        removal_phrases,
    )?;
    let resolutions = prepared.resolutions;
    if let Err(error)=queue.transact(|journal| {
        if journal.paused {return Err(io::Error::new(io::ErrorKind::Interrupted,"Queue ist pausiert; Forwardwirkungen wurden nicht gestartet"));}
        let entry=find_mut(journal,&id)?;
        if !matches!(entry.state,QueueState::AwaitingConfirmation|QueueState::Validating) {return Err(invalid_transition(entry.state));}
        if entry.state==QueueState::AwaitingConfirmation && !matches!(&entry.attention,Some(QueueAttention::RemoteRun {confirmation,..}) if confirmation==&digest_text(&digest)) {return Err(crate::real_mode::confirmation_mismatch("Forwardqueuerückfrage gehört zu einer anderen Vorschau"));}
        let QueueOperation::BidirectionalForward {expected_baseline_digest,expected_plan_digest,expected_unresolved_digest,expected_preview_digest,resolutions:stored,..}=&mut entry.operation else{return Err(invalid_transition(entry.state));};
        *expected_baseline_digest=preview.plan.baseline_digest;*expected_plan_digest=preview.plan.plan_digest;*expected_unresolved_digest=preview.unresolved_digest;*expected_preview_digest=preview.digest;*stored=resolutions;
        entry.state=QueueState::Running;entry.attention=None;entry.error=None;entry.error_code=None;entry.recovery_available=false;entry.bidirectional_completion_recovery=None;
        Ok(())
    }) {drop(run);return Err(interrupt_forward_error(queue,&id,Progress::default(),error));}
    if let Err(error) = append_forward_audit(queue, &id, "started", &preview, &Progress::default())
    {
        drop(run);
        return Err(error);
    }
    let mut progress = QueueProgress::new(queue, &id, cancel, on_progress);
    let result = run.execute_retaining_journal(cancel, &mut |value| progress.report(value));
    let (latest, persistence_error) = progress.finish();
    if let Some(error) = persistence_error {
        let paths = result.as_ref().map(forward_stats_paths).unwrap_or_default();
        let error = crate::error::with_recovery_paths(
            possible_mutation_progress_error(error, &result),
            &paths,
        );
        return Err(interrupt_forward_error(
            queue,
            &id,
            recovery_progress(latest),
            error,
        ));
    }
    match result {
        Err(error) => {
            let audit = append_forward_audit(queue, &id, "interrupted", &preview, &latest);
            let error = preserve_forward_error(error, audit, "Forward-Unterbrechungsaudit");
            Err(interrupt_forward_error(
                queue,
                &id,
                recovery_progress(latest),
                error,
            ))
        }
        Ok(stats) => complete_forward_queue_success(queue, &id, stats, latest, |progress| {
            append_forward_audit(queue, &id, "completed", &preview, progress)
        }),
    }
}

fn complete_forward_queue_success(
    queue: &OperationQueue,
    id: &str,
    stats: engine::ForwardBidirectionalStats,
    mut latest: Progress,
    completed_audit: impl FnOnce(&Progress) -> io::Result<()>,
) -> io::Result<QueueExecutionResult> {
    let paths = forward_stats_paths(&stats);
    latest.phase = "done".into();
    latest.current.clear();
    if let Err(error) = queue.update(id, |entry| {
        require_state(entry, QueueState::Running)?;
        entry.state = QueueState::Completed;
        entry.progress = latest.clone();
        entry.error = None;
        entry.error_code = None;
        entry.attention = None;
        entry.recovery_available = false;
        entry.bidirectional_completion_recovery = Some(BidirectionalCompletionRecovery::Complete);
        for path in &paths {
            if !entry.temp_recovery_paths.contains(path) {
                entry.temp_recovery_paths.push(path.clone());
            }
        }
        Ok(())
    }) {
        let error = forward_completion_error(error, &paths, "Terminaler Queuecommit scheiterte");
        return Err(interrupt_forward_error(queue, id, latest, error));
    }
    // Der dauerhafte Queueabschluss liegt vor dem Audit: Ein Auditfehler darf B1
    // nicht als wiederholbaren Dateilauf erscheinen lassen. Das Corejournal bleibt
    // bis zum anschließenden mutationslosen Cleanup gebunden.
    completed_audit(&latest)
        .map_err(|error| forward_completion_error(error, &paths, "Abschlussaudit scheiterte"))?;
    finish_bidirectional_forward_queue_completion(queue, id).map_err(|error| {
        forward_completion_error(error, &paths, "Abschluss-Recovery scheiterte")
    })?;
    Ok(QueueExecutionResult::Bidirectional(
        LocalBidirectionalStats {
            execution_class: Some(crate::sync::SyncExecutionClass::BestEffort),
            runs: stats.runs,
            baseline_generation: stats.baseline_generation,
            baseline_committed: stats.baseline_committed,
        },
    ))
}

/// Der Coreabschluss beweist B1; erst danach wird der Queueabschlussmarker entfernt.
pub fn finish_bidirectional_forward_queue_completion(
    queue: &OperationQueue,
    id: &str,
) -> io::Result<()> {
    let entry = current_entry(queue, id)?;
    let baseline = forward_baseline(&entry.operation)?;
    if !matches!(entry.state, QueueState::Interrupted | QueueState::Completed) {
        return Err(invalid_transition(entry.state));
    }
    let status = engine::forward_bidirectional_journal_status(baseline)?;
    let core_completion = match status {
        Some(status) if status.baseline_completion_available => true,
        None if entry.bidirectional_completion_recovery
            == Some(BidirectionalCompletionRecovery::Complete) =>
        {
            false
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Forwarddatenwirkung ist noch nicht vollständig committed",
            ))
        }
    };
    queue.update(id, |entry| {
        entry.bidirectional_completion_recovery = Some(BidirectionalCompletionRecovery::Complete);
        entry.recovery_available = false;
        Ok(())
    })?;
    if core_completion {
        engine::finish_forward_bidirectional_completion(baseline)?;
    }
    queue.update(id, |entry| {
        entry.state = QueueState::Completed;
        entry.progress.phase = "done".into();
        entry.progress.current.clear();
        entry.attention = None;
        entry.error = None;
        entry.error_code = None;
        entry.recovery_available = false;
        entry.bidirectional_completion_recovery = None;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoSecrets;
    impl SecretResolver for NoSecrets {
        fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Lokaler Test fragt keine Secrets ab",
            ))
        }
    }
    struct Fixture {
        _temp: tempfile::TempDir,
        left: PathBuf,
        right: PathBuf,
        baseline: PathBuf,
        queue: PathBuf,
        pair: PairId,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let left = root.join("left");
            let right = root.join("right");
            std::fs::create_dir(&left).unwrap();
            std::fs::create_dir(&right).unwrap();
            Self {
                _temp: temp,
                left,
                right,
                baseline: root.join("baseline.json"),
                queue: root.join("config/operations-queue.json"),
                pair: PairId::generate().unwrap(),
            }
        }
        fn prepare_owner(&self) -> crate::remote_real_mode::InteractiveFileOwner {
            crate::remote_real_mode::InteractiveFileOwner {
                staging: crate::local_staging::StagingOwner {
                    queue: self.queue.clone(),
                    operation: "e".repeat(32),
                },
                profiles: Vec::new(),
            }
        }
        fn prepare(&self) -> engine::PreparedForwardBidirectional {
            engine::prepare_forward_bidirectional_owned(
                self.pair,
                ProviderEndpoint::local(&self.left).unwrap(),
                ProviderEndpoint::local(&self.right).unwrap(),
                &self.baseline,
                &[],
                false,
                None,
                &Cancel::new(),
                Some(self.prepare_owner()),
            )
            .unwrap()
        }
        fn baseline(&self) {
            let queue = OperationQueue::open(&self.queue).unwrap();
            let (staging, _, _) = queue.ftps_owner(None).unwrap();
            let prepared = engine::prepare_forward_bidirectional_owned(
                self.pair,
                ProviderEndpoint::local(&self.left).unwrap(),
                ProviderEndpoint::local(&self.right).unwrap(),
                &self.baseline,
                &[],
                false,
                None,
                &Cancel::new(),
                Some(crate::remote_real_mode::InteractiveFileOwner {
                    staging,
                    profiles: Vec::new(),
                }),
            )
            .unwrap();
            let digest = prepared.preview().digest;
            prepared
                .confirm_effects(digest, true)
                .unwrap()
                .execute(&Cancel::new(), &mut |_| {})
                .unwrap();
        }
        fn operation(&self, prepared: &engine::PreparedForwardBidirectional) -> QueueOperation {
            let preview = prepared.preview();
            QueueOperation::BidirectionalForward {
                pair_id: self.pair,
                pair_name: "Forwardtest".into(),
                left: Box::new(BidirectionalQueueEndpoint::Local {
                    root: self.left.to_string_lossy().into_owned(),
                }),
                right: Box::new(BidirectionalQueueEndpoint::Local {
                    root: self.right.to_string_lossy().into_owned(),
                }),
                baseline_path: self.baseline.to_string_lossy().into_owned(),
                resolutions: vec![],
                expected_baseline_digest: preview.plan.baseline_digest,
                expected_plan_digest: preview.plan.plan_digest,
                expected_unresolved_digest: preview.unresolved_digest,
                expected_preview_digest: preview.digest,
            }
        }
        fn bind(&self, queue: &OperationQueue) -> PreparedBidirectionalForwardQueueRun {
            let prepared = self.prepare();
            let entry = queue.enqueue_claimed(self.operation(&prepared)).unwrap();
            bind_prepared_bidirectional_forward_queue_entry(queue, &entry, prepared, &Cancel::new())
                .unwrap()
        }
    }
    fn run(
        queue: &OperationQueue,
        prepared: PreparedBidirectionalForwardQueueRun,
    ) -> QueueExecutionResult {
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
        .unwrap()
    }
    #[test]
    fn forwardqueue_preparation_cannot_change_window_queue() {
        let fixture = Fixture::new();
        std::fs::write(fixture.left.join("a"), b"expected").unwrap();
        let first = OperationQueue::open(&fixture.queue).unwrap();
        let (_, lock, _) = first.ftps_owner(None).unwrap();
        let second = OperationQueue::open_with_process_lock(
            fixture.queue.with_file_name("operations-queue-other.json"),
            lock,
        )
        .unwrap();
        let prepared = engine::prepare_forward_bidirectional_for_queue(
            &first,
            fixture.pair,
            ProviderEndpoint::local(&fixture.left).unwrap(),
            ProviderEndpoint::local(&fixture.right).unwrap(),
            &fixture.baseline,
            &[],
            false,
            None,
            &Cancel::new(),
        )
        .unwrap();
        let entry = second
            .enqueue_claimed(fixture.operation(&prepared))
            .unwrap();
        let error = match bind_prepared_bidirectional_forward_queue_entry(
            &second,
            &entry,
            prepared,
            &Cancel::new(),
        ) {
            Ok(_) => panic!("Andere Fensterqueue darf Vorbereitung nicht übernehmen"),
            Err(error) => error,
        };
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(second.snapshot().entries[0].state, QueueState::Interrupted);
        assert!(!fixture.right.join("a").exists());
        assert!(!fixture.baseline.exists());
    }

    #[test]
    fn forwardqueue_binds_initial_ram_and_requires_fresh_recovery_after_reopen() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("a"), "old").unwrap();
        }
        fixture.baseline();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        std::fs::write(fixture.left.join("a"), "first").unwrap();
        let prepared = fixture.bind(&queue);
        let id = prepared.operation_id().to_owned();
        assert_eq!(
            queue.snapshot().entries[0].state,
            QueueState::AwaitingConfirmation
        );
        assert!(queue.claim_next().unwrap().is_none());
        queue
            .confirm_bidirectional_forward_preparation(&id, &prepared)
            .unwrap();
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Pending
        );
        assert_eq!(queue.claim_next().unwrap().unwrap().operation_id, id);
        let QueueExecutionResult::Bidirectional(stats) = run(&queue, prepared) else {
            panic!("Bidirbilanz fehlt")
        };
        assert_eq!(
            stats.execution_class,
            Some(crate::sync::SyncExecutionClass::BestEffort)
        );
        assert_eq!(stats.baseline_generation, 2);
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
        assert!(
            engine::forward_bidirectional_journal_status(&fixture.baseline)
                .unwrap()
                .is_none()
        );
        std::fs::write(fixture.left.join("a"), "second").unwrap();
        let prepared = fixture.bind(&queue);
        let next = prepared.operation_id().to_owned();
        queue
            .confirm_bidirectional_forward_preparation(&next, &prepared)
            .unwrap();
        drop(prepared);
        drop(queue);
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let entry = current_entry(&queue, &next).unwrap();
        assert_eq!(entry.state, QueueState::Interrupted);
        assert!(entry.recovery_available);
        assert!(queue.retry(&next).is_err());
        assert!(queue
            .fail_coded(&next, Progress::default(), &io::Error::other("generic"))
            .is_err());
        let prepared = prepare_bidirectional_forward_queue_recovery(
            &queue,
            &next,
            &[],
            None,
            Arc::new(NoSecrets),
            &Cancel::new(),
        )
        .unwrap();
        assert!(prepared.preview().recovery.is_some());
        run(&queue, prepared);
        assert_eq!(
            current_entry(&queue, &next).unwrap().state,
            QueueState::Completed
        );
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"second");
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Completed
        );
    }
    #[test]
    fn forwardqueue_cancel_keeps_b0_and_reopen_never_retries_partial_projection() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            for name in ["a", "b", "c"] {
                std::fs::write(root.join(name), name).unwrap();
            }
        }
        fixture.baseline();
        let b0 = std::fs::read(&fixture.baseline).unwrap();
        std::fs::write(fixture.left.join("a"), "a-new").unwrap();
        std::fs::write(fixture.left.join("b"), "b-new").unwrap();
        std::fs::write(fixture.right.join("c"), "c-new").unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        queue.set_stop_on_error(true).unwrap();
        let prepared = fixture.bind(&queue);
        let id = prepared.operation_id().to_owned();
        let digest = prepared.preview().digest;
        let cancel = Cancel::new();
        execute_prepared_bidirectional_forward_queue_entry(
            &queue,
            prepared,
            digest,
            true,
            &[],
            &cancel,
            &mut |progress| {
                if progress.files_done == 1 {
                    cancel.cancel();
                }
            },
        )
        .unwrap_err();
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b0);
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"a-new");
        assert_eq!(std::fs::read(fixture.right.join("b")).unwrap(), b"b");
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Interrupted
        );
        assert!(queue.snapshot().paused);
        drop(queue);
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        assert!(queue.claim_next().unwrap().is_none());
        assert!(current_entry(&queue, &id).unwrap().recovery_available);
        queue.set_paused(false).unwrap();
        let prepared = prepare_bidirectional_forward_queue_recovery(
            &queue,
            &id,
            &[],
            None,
            Arc::new(NoSecrets),
            &Cancel::new(),
        )
        .unwrap();
        run(&queue, prepared);
        for name in ["a", "b", "c"] {
            assert_eq!(
                std::fs::read(fixture.left.join(name)).unwrap(),
                std::fs::read(fixture.right.join(name)).unwrap()
            );
        }
    }
    #[test]
    fn forwardqueue_schema_34_rejects_new_variant_in_version_33() {
        let fixture = Fixture::new();
        fixture.baseline();
        let prepared = fixture.prepare();
        let operation = fixture.operation(&prepared);
        prepared.discard().unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        queue.enqueue(operation).unwrap();
        let mut snapshot = queue.snapshot();
        snapshot.version = 33;
        drop(queue);
        std::fs::write(&fixture.queue, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(
            OperationQueue::open(&fixture.queue).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn forwardqueue_reopen_after_b1_allows_only_mutation_free_completion() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("a"), "old").unwrap();
        }
        fixture.baseline();
        std::fs::write(fixture.left.join("a"), "new").unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let prepared = fixture.bind(&queue);
        let id = prepared.operation_id().to_owned();
        let digest = prepared.preview().digest;
        let run = prepared.prepared.confirm_effects(digest, true).unwrap();
        queue
            .mark_remote_running(&id, &digest_text(&digest))
            .unwrap();
        run.execute_retaining_journal(&Cancel::new(), &mut |_| {})
            .unwrap();
        let b1 = std::fs::read(&fixture.baseline).unwrap();
        assert!(
            engine::forward_bidirectional_journal_status(&fixture.baseline)
                .unwrap()
                .unwrap()
                .baseline_completion_available
        );
        drop(queue);
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let entry = current_entry(&queue, &id).unwrap();
        assert_eq!(entry.state, QueueState::Interrupted);
        assert!(!entry.recovery_available);
        assert_eq!(
            entry.bidirectional_completion_recovery,
            Some(BidirectionalCompletionRecovery::Complete)
        );
        assert!(queue.retry(&id).is_err());
        finish_bidirectional_forward_queue_completion(&queue, &id).unwrap();
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Completed
        );
        assert!(
            engine::forward_bidirectional_journal_status(&fixture.baseline)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b1);
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"new");
    }
    #[test]
    fn forwardqueue_drift_discards_old_conflict_choices_and_binds_new_choices() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("a"), "old").unwrap();
        }
        fixture.baseline();
        let b0 = std::fs::read(&fixture.baseline).unwrap();
        std::fs::write(fixture.left.join("a"), "left").unwrap();
        std::fs::write(fixture.right.join("a"), "right").unwrap();
        let resolution = |decision| ConflictResolution {
            path: crate::BinaryRelativePath(vec![b"a".to_vec()]),
            decision,
        };
        let old = vec![resolution(crate::ConflictDecision::KeepLeft)];
        let prepared = engine::prepare_forward_bidirectional_owned(
            fixture.pair,
            ProviderEndpoint::local(&fixture.left).unwrap(),
            ProviderEndpoint::local(&fixture.right).unwrap(),
            &fixture.baseline,
            &old,
            false,
            None,
            &Cancel::new(),
            Some(fixture.prepare_owner()),
        )
        .unwrap();
        let mut operation = fixture.operation(&prepared);
        if let QueueOperation::BidirectionalForward { resolutions, .. } = &mut operation {
            *resolutions = old;
        }
        prepared.discard().unwrap();
        std::fs::write(fixture.right.join("a"), "right-later").unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let entry = queue.enqueue_claimed(operation).unwrap();
        let id = entry.operation_id.clone();
        let prepared = prepare_bidirectional_forward_queue_entry(
            &queue,
            entry,
            Arc::new(NoSecrets),
            &Cancel::new(),
        )
        .unwrap();
        assert!(!prepared.preview().plan.executable);
        assert_eq!(prepared.preview().conflict_details.len(), 1);
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b0);
        let expected = prepared.preview().unresolved_digest;
        discard_prepared_bidirectional_forward_queue_entry(&queue, prepared).unwrap();
        let prepared = reprepare_bidirectional_forward_queue_entry_decisions(
            &queue,
            &id,
            &[resolution(crate::ConflictDecision::KeepRight)],
            expected,
            Arc::new(NoSecrets),
            &Cancel::new(),
        )
        .unwrap();
        queue
            .confirm_bidirectional_forward_preparation(&id, &prepared)
            .unwrap();
        queue.claim_entry(&id).unwrap().unwrap();
        run(&queue, prepared);
        assert_eq!(
            std::fs::read(fixture.left.join("a")).unwrap(),
            b"right-later"
        );
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Completed
        );
    }
    #[test]
    fn forwardqueue_partial_error_keeps_code_and_paths_when_recovery_commit_has_enospc() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("a"), "old").unwrap();
        }
        fixture.baseline();
        let b0 = std::fs::read(&fixture.baseline).unwrap();
        std::fs::write(fixture.left.join("a"), "new").unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let prepared = fixture.bind(&queue);
        let id = prepared.operation_id().to_owned();
        let digest = prepared.preview().digest;
        let run = prepared.prepared.confirm_effects(digest, true).unwrap();
        queue
            .mark_remote_running(&id, &digest_text(&digest))
            .unwrap();
        let cancel = Cancel::new();
        let execution = run
            .execute_retaining_journal(&cancel, &mut |progress| {
                if progress.files_done == 1 {
                    cancel.cancel();
                }
            })
            .unwrap_err();
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"new");
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b0);
        let original_path = fixture
            .left
            .parent()
            .unwrap()
            .join("retained-upload.tmp")
            .display()
            .to_string();
        let journal_path = fixture.queue.display().to_string();
        let original = crate::error::with_recovery_paths(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!("Antwortverlust nach Teilwirkung: {execution}"),
            ),
            std::slice::from_ref(&original_path),
        );
        let followup = queue.interrupt_bidirectional_forward_using(
            &id,
            Progress::default(),
            &original,
            |_, snapshot| {
                let entry = &snapshot.entries[0];
                assert_eq!(entry.state, QueueState::Interrupted);
                assert!(entry.temp_recovery_paths.contains(&original_path));
                Err(crate::error::with_recovery_paths(
                    io::Error::from_raw_os_error(libc::ENOSPC),
                    std::slice::from_ref(&journal_path),
                ))
            },
        );
        let error = preserve_forward_error(original, followup, "Forward-Recoverycommit");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(error
            .to_string()
            .contains("Antwortverlust nach Teilwirkung"));
        assert!(error.to_string().contains("Forward-Recoverycommit"));
        let paths = crate::error::recovery_paths(&error);
        assert!(paths.contains(&original_path));
        assert!(paths.contains(&journal_path));
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Running
        );
        assert_eq!(
            read_snapshot_bound(&queue._process_lock.root, &fixture.queue)
                .unwrap()
                .entries[0]
                .state,
            QueueState::Running
        );
        assert!(
            engine::forward_bidirectional_journal_status(&fixture.baseline)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn forwardqueue_audit_failure_after_b1_keeps_stats_paths_and_terminal_commit() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("a"), "old").unwrap();
        }
        fixture.baseline();
        std::fs::write(fixture.left.join("a"), "new").unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let prepared = fixture.bind(&queue);
        let id = prepared.operation_id().to_owned();
        let digest = prepared.preview().digest;
        let run = prepared.prepared.confirm_effects(digest, true).unwrap();
        queue
            .mark_remote_running(&id, &digest_text(&digest))
            .unwrap();
        let mut stats = run
            .execute_retaining_journal(&Cancel::new(), &mut |_| {})
            .unwrap();
        let b1 = std::fs::read(&fixture.baseline).unwrap();
        let root = fixture.left.parent().unwrap();
        let first = root.join("first-retained.tmp").display().to_string();
        let second = root.join("second-retained.tmp").display().to_string();
        let audit_path = root.join("audit-failure").display().to_string();
        stats.runs[0].temp_recovery_paths.push(first.clone());
        stats.runs.push(crate::SyncStats {
            temp_recovery_paths: vec![second.clone()],
            ..Default::default()
        });
        let error = complete_forward_queue_success(&queue, &id, stats, Progress::default(), |_| {
            let disk = read_snapshot_bound(&queue._process_lock.root, &fixture.queue).unwrap();
            let entry = &disk.entries[0];
            assert_eq!(entry.state, QueueState::Completed);
            assert_eq!(
                entry.bidirectional_completion_recovery,
                Some(BidirectionalCompletionRecovery::Complete)
            );
            assert!(entry.temp_recovery_paths.contains(&first));
            assert!(entry.temp_recovery_paths.contains(&second));
            Err(crate::error::with_recovery_paths(
                io::Error::from_raw_os_error(libc::ENOSPC),
                std::slice::from_ref(&audit_path),
            ))
        })
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let paths = crate::error::recovery_paths(&error);
        for path in [&first, &second, &audit_path] {
            assert!(paths.contains(path));
        }
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Completed
        );
        assert!(
            engine::forward_bidirectional_journal_status(&fixture.baseline)
                .unwrap()
                .unwrap()
                .baseline_completion_available
        );
        finish_bidirectional_forward_queue_completion(&queue, &id).unwrap();
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b1);
        assert_eq!(std::fs::read(fixture.right.join("a")).unwrap(), b"new");
        assert_eq!(
            current_entry(&queue, &id).unwrap().temp_recovery_paths,
            vec![first, second]
        );
    }

    #[test]
    fn forwardqueue_pending_preparation_is_discarded_before_removal_or_cancel() {
        for cancel in [false, true] {
            let fixture = Fixture::new();
            fixture.baseline();
            std::fs::write(fixture.left.join("a"), "new").unwrap();
            let queue = OperationQueue::open(&fixture.queue).unwrap();
            let prepared = fixture.bind(&queue);
            let id = prepared.operation_id().to_owned();
            queue
                .confirm_bidirectional_forward_preparation(&id, &prepared)
                .unwrap();
            if cancel {
                cancel_prepared_bidirectional_forward_queue_entry(&queue, prepared).unwrap();
                assert_eq!(
                    current_entry(&queue, &id).unwrap().state,
                    QueueState::Cancelled
                );
            } else {
                discard_prepared_bidirectional_forward_queue_entry(&queue, prepared).unwrap();
                assert_eq!(
                    current_entry(&queue, &id).unwrap().state,
                    QueueState::Pending
                );
                queue.remove_pending(&id).unwrap();
            }
            assert!(
                engine::forward_bidirectional_journal_status(&fixture.baseline)
                    .unwrap()
                    .is_none()
            );
            fixture.prepare().discard().unwrap();
            assert!(!fixture.right.join("a").exists());
        }
    }

    #[test]
    fn forwardqueue_cancel_of_recovery_preserves_old_partial_journal() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("a"), "old").unwrap();
        }
        fixture.baseline();
        let b0 = std::fs::read(&fixture.baseline).unwrap();
        std::fs::write(fixture.left.join("a"), "new").unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let prepared = fixture.bind(&queue);
        let id = prepared.operation_id().to_owned();
        let digest = prepared.preview().digest;
        let cancel = Cancel::new();
        execute_prepared_bidirectional_forward_queue_entry(
            &queue,
            prepared,
            digest,
            true,
            &[],
            &cancel,
            &mut |progress| {
                if progress.files_done == 1 {
                    cancel.cancel();
                }
            },
        )
        .unwrap_err();
        let prepared = prepare_bidirectional_forward_queue_recovery(
            &queue,
            &id,
            &[],
            None,
            Arc::new(NoSecrets),
            &Cancel::new(),
        )
        .unwrap();
        cancel_prepared_bidirectional_forward_queue_entry(&queue, prepared).unwrap();
        let entry = current_entry(&queue, &id).unwrap();
        assert_eq!(entry.state, QueueState::Interrupted);
        assert!(entry.recovery_available);
        assert!(
            engine::forward_bidirectional_journal_status(&fixture.baseline)
                .unwrap()
                .is_some()
        );
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b0);
        assert!(queue.retry(&id).is_err());
    }
    #[test]
    fn forwardqueue_pending_raw_cleanup_keeps_old_partial_recovery_visible() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("a"), "old").unwrap();
        }
        fixture.baseline();
        let b0 = std::fs::read(&fixture.baseline).unwrap();
        std::fs::write(fixture.left.join("a"), "new").unwrap();
        let queue = OperationQueue::open(&fixture.queue).unwrap();
        let prepared = fixture.bind(&queue);
        let id = prepared.operation_id().to_owned();
        let digest = prepared.preview().digest;
        let cancel = Cancel::new();
        execute_prepared_bidirectional_forward_queue_entry(
            &queue,
            prepared,
            digest,
            true,
            &[],
            &cancel,
            &mut |progress| {
                if progress.files_done == 1 {
                    cancel.cancel();
                }
            },
        )
        .unwrap_err();
        let prepared = prepare_bidirectional_forward_queue_recovery(
            &queue,
            &id,
            &[],
            None,
            Arc::new(NoSecrets),
            &Cancel::new(),
        )
        .unwrap();
        queue
            .confirm_bidirectional_forward_preparation(&id, &prepared)
            .unwrap();
        assert_eq!(
            current_entry(&queue, &id).unwrap().state,
            QueueState::Pending
        );
        prepared.prepared.discard().unwrap();
        let error =
            crate::real_mode::plan_drift("Profilbindung wurde nach der RAMfreigabe ungueltig");
        queue
            .interrupt_bidirectional_forward(&id, Progress::default(), &error)
            .unwrap();
        let entry = current_entry(&queue, &id).unwrap();
        assert_eq!(entry.state, QueueState::Interrupted);
        assert!(entry.recovery_available);
        assert_eq!(entry.error_code, Some(crate::ErrorCode::OutcomeUnclear));
        assert!(
            engine::forward_bidirectional_journal_status(&fixture.baseline)
                .unwrap()
                .is_some()
        );
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b0);
        assert!(queue.remove_pending(&id).is_err());
    }
}
