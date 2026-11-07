//! Best-Effort-Vorwärtslauf mit Neuplanung nach Teilfehlern gegen die letzte
//! gemeinsame Baseline. Diese Klasse besitzt ausdrücklich keinen Universalrollback.
use super::*;
use crate::{
    Cancel, ContentSyncPreview, PreparedContentSync, Progress, ProviderEndpoint, SyncStats,
};

const FORWARD_VERSION: u32 = 1;
const FORWARD_CLASS: &str = "best_effort_forward";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ForwardJournal {
    version: u32,
    execution_class: String,
    active: Option<BidirectionalRunJournal>,
    history: Vec<BidirectionalRunJournal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery_owner: Option<crate::local_staging::StagingOwner>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    staging_bindings: BTreeMap<PathBuf, EndpointSide>,
    digest: [u8; 32],
}
impl ForwardJournal {
    fn refresh(&mut self) -> io::Result<()> {
        self.digest = [0; 32];
        self.digest = serializable_digest(self)?;
        Ok(())
    }
    fn validate(&self) -> io::Result<()> {
        let mut expected = self.clone();
        expected.refresh()?;
        if self.version != FORWARD_VERSION
            || self.execution_class != FORWARD_CLASS
            || self.digest != expected.digest
        {
            return Err(invalid_data(
                "Forwardjournal hat eine unbekannte Klasse oder einen ungültigen Digest",
            ));
        }
        for run in self.active.iter().chain(&self.history) {
            run.validate()?;
            if run.rollback_step.is_some()
                || run.type_swap_step.is_some()
                || !run.rollback_quarantines.is_empty()
                || matches!(
                    run.state,
                    BidirectionalRunState::RollingBack | BidirectionalRunState::RollbackCommitted
                )
                || run.projections.iter().any(|projection| {
                    matches!(
                        projection.projection,
                        BidirectionalProjectionKind::TypeSwap { .. }
                    )
                })
            {
                return Err(invalid_data(
                    "Rollback- oder Typwechselzustand gehört nicht zur Forwardklasse",
                ));
            }
        }
        Ok(())
    }
}
fn journal_path(store: &BaselineStore) -> PathBuf {
    store.path.with_extension("forward.json")
}
fn load(store: &BaselineStore) -> io::Result<Option<ForwardJournal>> {
    let Some((_, payload)) = read_store_payload(&journal_path(store), MAX_RUN_JOURNAL_BYTES)?
    else {
        return Ok(None);
    };
    let journal: ForwardJournal = serde_json::from_slice(&payload)
        .map_err(|error| invalid_data(format!("Forwardjournal ist beschädigt: {error}")))?;
    journal.validate()?;
    Ok(Some(journal))
}
pub(super) fn has_active_forward_journal(store: &BaselineStore) -> io::Result<bool> {
    Ok(load(store)?.is_some_and(|journal| journal.active.is_some()))
}
fn save(store: &BaselineStore, journal: &mut ForwardJournal) -> io::Result<()> {
    journal.refresh()?;
    journal.validate()?;
    let payload = serde_json::to_vec(journal).map_err(io::Error::other)?;
    if payload.len() as u64 > MAX_RUN_JOURNAL_BYTES {
        return Err(invalid_data(
            "Forwardjournal überschreitet die Größenbegrenzung",
        ));
    }
    let path = journal_path(store);
    let parent = path
        .parent()
        .ok_or_else(|| invalid_input("Forwardjournal braucht einen Elternordner"))?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
    let suffix: String = nonce.iter().map(|value| format!("{value:02x}")).collect();
    let temp = parent.join(format!(".bidirectional-forward-{suffix}.tmp"));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(&payload)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        File::open(parent)?.sync_all()
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        // Ein sichtbarer Rename allein genügt nicht. Erst erneute Barrieren und
        // exakter Readback erlauben die nächste Mutation.
        if load(store)?.as_ref() == Some(journal) {
            let (file, _) = read_store_payload(&path, MAX_RUN_JOURNAL_BYTES)?
                .ok_or_else(|| invalid_data("Forwardjournal verschwand"))?;
            file.sync_all()
                .and_then(|_| File::open(parent)?.sync_all())
                .map_err(|barrier| {
                    crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        format!("Forwardjournal bleibt undauerhaft: {error}; {barrier}"),
                    )
                })?;
            if load(store)?.as_ref() == Some(journal) {
                return Ok(());
            }
        }
        return Err(error);
    }
    Ok(())
}

fn forward_conflict_views(
    plan: &BidirectionalPlan,
    left: &TreeSnapshot,
    right: &TreeSnapshot,
) -> io::Result<Vec<BidirectionalConflictView>> {
    let mut conflict_details = bidirectional_conflict_views(plan, left, right)?;
    for view in &mut conflict_details {
        for choice in &mut view.choices {
            choice.explanation=match &choice.decision {
                ConflictDecision::KeepLeft=>"Linken Zustand am Originalpfad übernehmen. Wiederherstellbarkeit und dauerhafte Wirkungen stehen in der anschließenden Bilanz.",
                ConflictDecision::KeepRight=>"Rechten Zustand am Originalpfad übernehmen. Wiederherstellbarkeit und dauerhafte Wirkungen stehen in der anschließenden Bilanz.",
                ConflictDecision::KeepBoth {..}=>"Beide Fassungen unter den gebundenen Namen behalten; die anschließende Bilanz nennt die Originalpfadwirkung.",
                ConflictDecision::Skip=>"Beide Fassungen unverändert lassen; die gemeinsame Baseline dieses Unterbaums bleibt erhalten.",
            }.into();
        }
    }
    Ok(conflict_details)
}

pub fn bind_forward_bidirectional_endpoint(
    endpoint: &ProviderEndpoint,
    cancel: &Cancel,
) -> io::Result<EndpointBinding> {
    let (_, _, binding) = endpoint.content_sync_snapshot_context(cancel)?;
    Ok(binding)
}

pub fn bind_forward_bidirectional_endpoint_compatibility(
    endpoint: &ProviderEndpoint,
    cancel: &Cancel,
) -> io::Result<(EndpointBinding, Vec<EndpointBinding>)> {
    let (vfs, root, binding) = endpoint.content_sync_snapshot_context(cancel)?;
    let identity = vfs.root_identity(&root)?;
    let current = match binding {
        EndpointBinding::Local(_) => EndpointBinding::local(&identity)?,
        EndpointBinding::Sftp(value)
        | EndpointBinding::Ftps(value)
        | EndpointBinding::WebDav(value)
        | EndpointBinding::Smb(value) => EndpointBinding::remote(&identity, value.account_digest)?,
    };
    let legacy = match &current {
        EndpointBinding::Ftps(value) => Some(EndpointBinding::Ftps(RemoteEndpointBinding {
            schema_version: 1,
            ..value.clone()
        })),
        EndpointBinding::WebDav(value) => Some(EndpointBinding::WebDav(RemoteEndpointBinding {
            schema_version: 1,
            ..value.clone()
        })),
        _ => None,
    };
    let mut compatible = vec![current.clone()];
    if let Some(legacy) = legacy {
        for candidate in legacy_endpoint_bindings(&identity, &legacy)? {
            if !compatible.contains(&candidate) {
                compatible.push(candidate);
            }
        }
    }
    Ok((current, compatible))
}

#[derive(Clone, Debug, Serialize)]
pub struct ForwardBidirectionalJournalStatus {
    pub execution_class: String,
    pub run_id: [u8; 16],
    pub plan: BidirectionalPlan,
    pub projections: Vec<BidirectionalProjectionJournal>,
    pub baseline_matches_previous: bool,
    pub baseline_completion_available: bool,
    pub previous_runs: usize,
}

/// Geheimnisfreier Startupstatus ohne Providerzugriff oder Journal-/Baselinemutation.
pub fn forward_bidirectional_journal_status(
    baseline_path: &Path,
) -> io::Result<Option<ForwardBidirectionalJournalStatus>> {
    let store = BaselineStore::open(baseline_path)?;
    let Some(journal) = load(&store)? else {
        return Ok(None);
    };
    let Some(active) = journal.active else {
        return Ok(None);
    };
    let baseline_digest = store.load()?.map(|baseline| baseline.body_digest);
    Ok(Some(ForwardBidirectionalJournalStatus {
        execution_class: FORWARD_CLASS.into(),
        run_id: active.run_id,
        plan: active.plan,
        projections: active.projections,
        baseline_matches_previous: baseline_digest == active.baseline_digest,
        baseline_completion_available: active.committed_baseline_digest.is_some()
            && active.committed_baseline_digest == baseline_digest,
        previous_runs: journal.history.len(),
    }))
}

#[derive(Clone, Debug, Serialize)]
pub struct ForwardRecoveryObservation {
    pub previous_run_id: [u8; 16],
    pub changed_left: Vec<BinaryRelativePath>,
    pub changed_right: Vec<BinaryRelativePath>,
    pub running_projections: Vec<u64>,
    pub warning: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ForwardBidirectionalPreview {
    pub execution_class: String,
    pub plan: BidirectionalPlan,
    pub unresolved_digest: [u8; 32],
    pub conflict_details: Vec<BidirectionalConflictView>,
    pub stage_blocker: Option<String>,
    pub scope_warnings: Vec<String>,
    pub expires_at_unix_ms: u128,
    pub digest: [u8; 32],
    pub recovery: Option<ForwardRecoveryObservation>,
    pub limitations: Vec<String>,
    pub effects: Vec<ContentSyncPreview>,
}
pub struct PreparedForwardBidirectional {
    store: BaselineStore,
    left: ProviderEndpoint,
    right: ProviderEndpoint,
    left_vfs: Arc<dyn Vfs>,
    right_vfs: Arc<dyn Vfs>,
    left_binding: EndpointBinding,
    right_binding: EndpointBinding,
    left_snapshot: TreeSnapshot,
    right_snapshot: TreeSnapshot,
    baseline: Option<BidirectionalBaseline>,
    journal: ForwardJournal,
    previous_digest: Option<[u8; 32]>,
    owns_preparation: bool,
    owner: Option<crate::remote_real_mode::InteractiveFileOwner>,
    queue_execution_bound: bool,
    unresolved: BidirectionalPlan,
    preview: ForwardBidirectionalPreview,
    created: std::time::Instant,
}
pub struct ForwardBidirectionalRun {
    prepared: PreparedForwardBidirectional,
}
#[derive(Debug, Serialize)]
pub struct ForwardBidirectionalStats {
    pub runs: Vec<SyncStats>,
    pub baseline_generation: u64,
    pub baseline_committed: bool,
}

fn changed_paths(before: &TreeSnapshot, after: &TreeSnapshot) -> Vec<BinaryRelativePath> {
    let before = before.map();
    let after = after.map();
    before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| match (before.get(path), after.get(path)) {
            (Some(a), Some(b)) => !a.semantically_equal(b),
            (None, None) => false,
            _ => true,
        })
        .cloned()
        .collect()
}
fn staging_blocked(message: &str, paths: &[String]) -> io::Error {
    crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::WouldBlock, crate::ErrorCode::OutcomeUnclear,
        format!("{message} Reste am angegebenen Provider/Konto prüfen und über normale Dateiaktionen bewusst außerhalb beider Paarwurzeln sichern; danach frisch planen. Keine automatische Löschung oder Wiederholung.")), paths)
}

fn projection_target(kind: &BidirectionalProjectionKind) -> EndpointSide {
    match kind {
        BidirectionalProjectionKind::Copy { direction, .. }
        | BidirectionalProjectionKind::Delete { direction, .. }
        | BidirectionalProjectionKind::TypeSwap { direction, .. } => direction_sides(*direction).1,
        BidirectionalProjectionKind::ConflictCopy { target_side, .. } => *target_side,
    }
}

fn staging_epoch(journal: Option<&ForwardJournal>) -> io::Result<[u8; 32]> {
    let Some(owner) = journal.and_then(|journal| journal.recovery_owner.as_ref()) else {
        return Ok([0; 32]);
    };
    let mut records = crate::local_staging::forward_staging_candidates(owner)?
        .into_iter()
        .map(|candidate| (candidate.index, candidate.stamp))
        .collect::<Vec<_>>();
    records.sort();
    records.dedup();
    serializable_digest(&records)
}

fn bind_staging_candidates(journal: &mut ForwardJournal) -> io::Result<()> {
    let Some(owner) = &journal.recovery_owner else {
        return Ok(());
    };
    let candidates = crate::local_staging::forward_staging_candidates(owner)?;
    let running = journal
        .active
        .iter()
        .flat_map(|run| &run.projections)
        .filter(|projection| projection.state == BidirectionalProjectionState::Running)
        .collect::<Vec<_>>();
    if running.len() == 1 {
        let side = projection_target(&running[0].projection);
        for candidate in candidates {
            journal
                .staging_bindings
                .entry(candidate.index)
                .or_insert(side);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn reject_unresolved_staging(
    journal: &ForwardJournal,
    left: &ProviderEndpoint,
    left_vfs: &Arc<dyn Vfs>,
    left_binding: &EndpointBinding,
    right: &ProviderEndpoint,
    right_vfs: &Arc<dyn Vfs>,
    right_binding: &EndpointBinding,
) -> io::Result<()> {
    let run = journal.active.as_ref().or_else(|| journal.history.last());
    let Some(run) = run else {
        return Ok(());
    };
    if run.left_binding != *left_binding || run.right_binding != *right_binding {
        return Err(crate::real_mode::plan_drift(
            "Forward-Endpunktidentitäten änderten sich vor der Zwischenstandsprüfung",
        ));
    }
    let Some(owner) = &journal.recovery_owner else {
        if journal.active.is_some()
            && run
                .projections
                .iter()
                .any(|projection| projection.state == BidirectionalProjectionState::Running)
        {
            return Err(staging_blocked("Der unterbrochene direkte oder ältere Forwardlauf besitzt keinen nachweisbaren Temp-Intentowner; sicherer Neuabgleich ist gesperrt.", &[]));
        }
        return Ok(());
    };
    let candidates = crate::local_staging::forward_staging_candidates(owner).map_err(|error| {
        staging_blocked(
            &format!("Temp-Intents sind nicht eindeutig lesbar: {error}."),
            &[],
        )
    })?;
    if candidates.is_empty() {
        return Ok(());
    }
    let running = run
        .projections
        .iter()
        .filter(|projection| projection.state == BidirectionalProjectionState::Running)
        .collect::<Vec<_>>();
    for candidate in candidates {
        if let (Some(binding), Some(identity)) = (&candidate.endpoint, &candidate.endpoint_root) {
            let mut relevant = false;
            for (endpoint, vfs, current_binding) in [
                (left, left_vfs, left_binding),
                (right, right_vfs, right_binding),
            ] {
                if crate::local_staging::remote_account(binding)
                    != crate::local_staging::remote_account(current_binding)
                {
                    continue;
                }
                let current = vfs.root_identity(endpoint.root())?;
                relevant |= crate::local_staging::translate_namespace_path(
                    identity,
                    candidate.object.base_path(),
                    &current,
                    endpoint.root(),
                )
                .is_some();
            }
            if !relevant {
                continue;
            }
        }
        if candidate.ftps.is_none()
            && candidate.endpoint_root.is_none()
            && ![left, right].iter().any(|endpoint| {
                candidate.remote == (endpoint.kind() != crate::ProviderKind::Local)
                    && candidate.object.base_path().starts_with(endpoint.root())
            })
        {
            continue;
        }
        // Alte Intents behalten bei einem neuen Auftrag ihre ursprüngliche Seite.
        let side = journal
            .staging_bindings
            .get(&candidate.index)
            .copied()
            .or_else(|| (running.len() == 1).then(|| projection_target(&running[0].projection)))
            .ok_or_else(|| {
                staging_blocked(
                    "Temp-Intent besitzt keinen eindeutig gebundenen Projektionsendpunkt.",
                    &[candidate.object.recovery_hint()],
                )
            })?;
        let (endpoint, vfs) = if side == EndpointSide::Left {
            (left, left_vfs)
        } else {
            (right, right_vfs)
        };
        if candidate.remote != (endpoint.kind() != crate::ProviderKind::Local) {
            continue;
        }
        let mut object = candidate.object.clone();
        let endpoint_binding = if side == EndpointSide::Left {
            left_binding
        } else {
            right_binding
        };
        if let (Some(binding), Some(identity)) = (&candidate.endpoint, &candidate.endpoint_root) {
            if crate::local_staging::remote_account(binding)
                != crate::local_staging::remote_account(endpoint_binding)
            {
                continue;
            }
            let current = vfs.root_identity(endpoint.root())?;
            let Some(path) = crate::local_staging::translate_namespace_path(
                identity,
                object.base_path(),
                &current,
                endpoint.root(),
            ) else {
                continue;
            };
            object = object.with_base_path(path);
        }
        let mut path = object.base_path().to_path_buf();
        if let Some(binding) = candidate.ftps {
            if binding.stou_name_unknown {
                return Err(staging_blocked(
                    "Ein FTP-/FTPS-Tempname blieb unbekannt; sein Fehlen ist nicht belegt.",
                    &[path.to_string_lossy().into_owned()],
                ));
            }
            let endpoint_binding = if side == EndpointSide::Left {
                left_binding
            } else {
                right_binding
            };
            let root = binding
                .roots
                .iter()
                .find(|root| {
                    EndpointBinding::remote(
                        &RootIdentity::Ftps((*root).clone()),
                        binding.account_digest,
                    )
                    .is_ok_and(|identity| identity == *endpoint_binding)
                })
                .ok_or_else(|| {
                    staging_blocked(
                        "FTP-/FTPS-Intent passt nicht zum gebundenen Konto und Root.",
                        &[path.to_string_lossy().into_owned()],
                    )
                })?;
            let physical = Path::new(&root.server.canonical_root)
                .join(root.virtual_root.trim_start_matches('/'));
            if let Ok(relative) = path.strip_prefix(physical) {
                path = endpoint.root().join(relative);
            } else {
                continue;
            }
        }
        if !path.starts_with(endpoint.root()) {
            continue;
        }
        object = object.with_base_path(path);
        let paths = vec![object.recovery_hint()];
        if candidate.active {
            return Err(staging_blocked(
                "Ein eigener Transfer hält noch eine aktive Registry-Lease.",
                &paths,
            ));
        }
        if !candidate.requires_absence {
            continue;
        }
        match object.is_absent(vfs.as_ref()) {
            Ok(true) => {}
            Ok(false) => {
                return Err(staging_blocked(
                    &format!(
                        "Eigener Zwischenstand auf {:?} ({side:?}) ist noch vorhanden.",
                        endpoint.kind()
                    ),
                    &paths,
                ))
            }
            Err(error) => {
                return Err(staging_blocked(
                    &format!(
                    "Eigener Zwischenstand auf {:?} ({side:?}) ist nicht sicher prüfbar: {error}.",
                    endpoint.kind()
                ),
                    &paths,
                ))
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn namespace_guard(
    config: Option<&Path>,
    left: &ProviderEndpoint,
    left_vfs: &Arc<dyn Vfs>,
    left_binding: &EndpointBinding,
    right: &ProviderEndpoint,
    right_vfs: &Arc<dyn Vfs>,
    right_binding: &EndpointBinding,
) -> io::Result<[u8; 32]> {
    let Some(config) = config else {
        return Ok([0; 32]);
    };
    let registry = crate::staging_guard::KnownStagingRegistry::at_private_root(config)?;
    registry.check(
        &[
            crate::staging_guard::StagingEndpoint {
                vfs: left_vfs.clone(),
                root: left.root().to_path_buf(),
                binding: left_binding.clone(),
            },
            crate::staging_guard::StagingEndpoint {
                vfs: right_vfs.clone(),
                root: right.root().to_path_buf(),
                binding: right_binding.clone(),
            },
        ],
        None,
    )
}

struct ForwardSnapshots {
    left_vfs: Arc<dyn Vfs>,
    right_vfs: Arc<dyn Vfs>,
    left_binding: EndpointBinding,
    right_binding: EndpointBinding,
    left: TreeSnapshot,
    right: TreeSnapshot,
    namespace_epoch: [u8; 32],
}
fn snapshots(
    left: &ProviderEndpoint,
    right: &ProviderEndpoint,
    cancel: &Cancel,
    journal: Option<&ForwardJournal>,
    owner: Option<&crate::remote_real_mode::InteractiveFileOwner>,
) -> io::Result<ForwardSnapshots> {
    let (left_vfs, _, left_binding) = left.content_sync_snapshot_context(cancel)?;
    let (right_vfs, _, right_binding) = right.content_sync_snapshot_context(cancel)?;
    let config = owner
        .map(|owner| &owner.staging)
        .or_else(|| journal.and_then(|journal| journal.recovery_owner.as_ref()))
        .and_then(|owner| owner.queue.parent());
    let namespace_epoch = namespace_guard(
        config,
        left,
        &left_vfs,
        &left_binding,
        right,
        &right_vfs,
        &right_binding,
    )?;
    let before_intents = staging_epoch(journal)?;
    if let Some(journal) = journal {
        reject_unresolved_staging(
            journal,
            left,
            &left_vfs,
            &left_binding,
            right,
            &right_vfs,
            &right_binding,
        )?;
    }
    let (left_snapshot, right_snapshot) = final_local_snapshots(
        left_vfs.as_ref(),
        left.root(),
        right_vfs.as_ref(),
        right.root(),
    )?;
    if let Some(journal) = journal {
        reject_unresolved_staging(
            journal,
            left,
            &left_vfs,
            &left_binding,
            right,
            &right_vfs,
            &right_binding,
        )?;
    }
    if namespace_guard(
        config,
        left,
        &left_vfs,
        &left_binding,
        right,
        &right_vfs,
        &right_binding,
    )? != namespace_epoch
    {
        return Err(crate::real_mode::plan_drift(
            "Relevante Temp-Intents änderten sich während des Forwardscans; frisch vorbereiten",
        ));
    }
    if staging_epoch(journal)? != before_intents {
        return Err(crate::real_mode::plan_drift(
            "Temp-Intents änderten sich während des Forwardscans; frisch vorbereiten",
        ));
    }
    crate::remote_real_mode::reject_overlapping_roots(
        &left_snapshot.root_identity,
        left.root(),
        &right_snapshot.root_identity,
        right.root(),
    )?;
    Ok(ForwardSnapshots {
        left_vfs,
        right_vfs,
        left_binding,
        right_binding,
        left: left_snapshot,
        right: right_snapshot,
        namespace_epoch,
    })
}

/// Fenstergebundene Vorschau einschließlich registrierter lokaler Proben, ohne Queueeintrag oder Ausführungsrecht.
#[allow(clippy::too_many_arguments)]
pub fn prepare_forward_bidirectional_for_queue(
    queue: &crate::OperationQueue,
    pair_id: PairId,
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
    recovery: bool,
    expected_unresolved_digest: Option<[u8; 32]>,
    cancel: &Cancel,
) -> io::Result<PreparedForwardBidirectional> {
    let (staging, _, _) = queue.ftps_owner(None)?;
    let source = queue.bind_provider_preparation_with_cancel(&source, cancel)?;
    let target = queue.bind_provider_preparation_with_cancel(&target, cancel)?;
    let mut prepared = prepare_forward_bidirectional_owned(
        pair_id,
        source,
        target,
        baseline_path,
        resolutions,
        recovery,
        expected_unresolved_digest,
        cancel,
        Some(crate::remote_real_mode::InteractiveFileOwner {
            staging,
            profiles: Vec::new(),
        }),
    )?;
    prepared.queue_execution_bound = false;
    Ok(prepared)
}

/// `recovery` muss für ein offenes Forwardjournal ausdrücklich gesetzt sein.
/// Frühere Konfliktentscheidungen werden dabei nie übernommen.
pub fn prepare_forward_bidirectional(
    pair_id: PairId,
    left: ProviderEndpoint,
    right: ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
    recovery: bool,
    cancel: &Cancel,
) -> io::Result<PreparedForwardBidirectional> {
    prepare_forward_bidirectional_bound(
        pair_id,
        left,
        right,
        baseline_path,
        resolutions,
        recovery,
        None,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_forward_bidirectional_bound(
    pair_id: PairId,
    left: ProviderEndpoint,
    right: ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
    recovery: bool,
    expected_unresolved_digest: Option<[u8; 32]>,
    cancel: &Cancel,
) -> io::Result<PreparedForwardBidirectional> {
    prepare_forward_bidirectional_owned(
        pair_id,
        left,
        right,
        baseline_path,
        resolutions,
        recovery,
        expected_unresolved_digest,
        cancel,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_forward_bidirectional_owned(
    pair_id: PairId,
    left: ProviderEndpoint,
    right: ProviderEndpoint,
    baseline_path: &Path,
    resolutions: &[ConflictResolution],
    recovery: bool,
    expected_unresolved_digest: Option<[u8; 32]>,
    cancel: &Cancel,
    owner: Option<crate::remote_real_mode::InteractiveFileOwner>,
) -> io::Result<PreparedForwardBidirectional> {
    pair_id.validate()?;
    let queue_execution_bound = owner.is_some();
    let store = BaselineStore::open(baseline_path)?;
    if store.load_strict_run_journal()?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Ein bestehendes Strictjournal behält seinen Resume-/Rollbackvertrag",
        ));
    }
    let mut previous = load(&store)?;
    if let Some(journal) = &mut previous {
        bind_staging_candidates(journal)?;
    }
    let active = previous
        .as_ref()
        .and_then(|journal| journal.active.as_ref());
    if active.is_some() && !recovery {
        return Err(crate::coded_error(
            io::ErrorKind::WouldBlock,
            crate::ErrorCode::ReplanRequired,
            "Teilwirkung verlangt ausdrücklich eine frische Forward-Recoveryplanung",
        ));
    }
    let baseline = store.load()?;
    if active.is_some_and(|run| {
        run.pair_id != pair_id
            || run.baseline_digest != baseline.as_ref().map(|baseline| baseline.body_digest)
    }) {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "Forwardjournal und gemeinsame Baseline stimmen nicht überein; gegebenenfalls Abschluss-Recovery ausführen"));
    }
    let ForwardSnapshots {
        left_vfs,
        right_vfs,
        left_binding,
        right_binding,
        left: left_snapshot,
        right: right_snapshot,
        ..
    } = snapshots(&left, &right, cancel, previous.as_ref(), owner.as_ref())?;
    if active
        .is_some_and(|run| run.left_binding != left_binding || run.right_binding != right_binding)
    {
        return Err(invalid_data(
            "Forward-Recovery hat andere Endpunktidentitäten",
        ));
    }
    let unresolved = plan_bidirectional(
        pair_id,
        &left_binding,
        &right_binding,
        baseline.as_ref(),
        &left_snapshot,
        &right_snapshot,
    )?;
    if expected_unresolved_digest.is_some_and(|digest| digest != unresolved.plan_digest) {
        return Err(crate::real_mode::plan_drift(
            "Forward-Konfliktbestand änderte sich seit der Entscheidung",
        ));
    }
    if recovery && !resolutions.is_empty() && expected_unresolved_digest.is_none() {
        return Err(crate::real_mode::confirmation_mismatch(
            "Recovery übernimmt Konfliktentscheidungen nur mit dem Digest der frischen Planung",
        ));
    }
    let mut skipped_types = Vec::new();
    for action in &unresolved.actions {
        if let BidirectionalAction::Conflict {
            path,
            kind: ConflictKind::TypeChanged,
            ..
        } = action
        {
            if !skipped_types
                .iter()
                .any(|parent| binary_path_contains(parent, path))
            {
                skipped_types.push(path.clone());
            }
        }
    }
    let mut decisions = resolutions
        .iter()
        .filter(|decision| {
            !skipped_types
                .iter()
                .any(|path| path == &decision.path || binary_path_contains(path, &decision.path))
        })
        .cloned()
        .collect::<Vec<_>>();
    decisions.extend(skipped_types.iter().map(|path| ConflictResolution {
        path: path.clone(),
        decision: ConflictDecision::Skip,
    }));
    let plan = resolve_conflicts(&unresolved, &left_snapshot, &right_snapshot, &decisions)?;
    let observation = active.map(|run| ForwardRecoveryObservation {
        previous_run_id:run.run_id,
        changed_left:changed_paths(&run.left_snapshot,&left_snapshot), changed_right:changed_paths(&run.right_snapshot,&right_snapshot),
        running_projections:run.projections.iter().filter(|projection| projection.state == BidirectionalProjectionState::Running).map(|projection|projection.ordinal).collect(),
        warning:"Vollständig frisch beobachtete Änderungen; ein laufender Schritt kann einen unklaren Ausgang haben. Kein automatischer Retry und keine Übernahme alter Konfliktentscheidungen. Der neue Drei-Wege-Plan verwendet weiterhin die letzte gemeinsame Baseline.".into(),
    });
    let previous_digest = previous.as_ref().map(|journal| journal.digest);
    let journal = previous.unwrap_or(ForwardJournal {
        version: FORWARD_VERSION,
        execution_class: FORWARD_CLASS.into(),
        active: None,
        history: Vec::new(),
        recovery_owner: None,
        staging_bindings: BTreeMap::new(),
        digest: [0; 32],
    });
    let conflict_details = forward_conflict_views(&plan, &left_snapshot, &right_snapshot)?;
    let scope_warnings = bidirectional_scope_warnings(
        &plan,
        &left_snapshot,
        left.root(),
        &right_snapshot,
        right.root(),
    )?;
    let expires_at_unix_ms = (std::time::SystemTime::now() + crate::REAL_PERMIT_LIFETIME)
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    let mut prepared = PreparedForwardBidirectional {
        store, left, right, left_vfs, right_vfs, left_binding, right_binding,
        left_snapshot, right_snapshot, baseline, journal, previous_digest,
        owns_preparation: false,
        owner,
        queue_execution_bound,
        unresolved: unresolved.clone(),
        preview: ForwardBidirectionalPreview {
            execution_class: FORWARD_CLASS.into(),
            unresolved_digest: unresolved.plan_digest,
            conflict_details,
            stage_blocker: local_stage_blocker(&plan),
            scope_warnings,
            expires_at_unix_ms,
            plan,
            digest: [0;32],
            recovery: observation,
            limitations: vec!["Best Effort: Kein universeller Rollback. Lokale Quarantäne bleibt verfügbar; entfernte Ersetzungen und Löschungen können dauerhaft sein. Teilfehler sperren das Paar bis zu einer frischen Wirkungsbestätigung.".into()],
            effects: Vec::new(),
        },
        created: std::time::Instant::now(),
    };
    for path in skipped_types {
        prepared.preview.limitations.push(format!("{}: Datei-/Ordner-Typwechsel wird in dieser Forwardklasse ausgelassen; die alte gemeinsame Baseline dieses Unterbaums bleibt erhalten.",path.display_lossy()));
    }
    if prepared.journal.active.is_none()
        && prepared.preview.stage_blocker.is_none()
        && !planned_projections(&prepared.preview.plan)?.is_empty()
    {
        prepared.journal.active = Some(BidirectionalRunJournal::new(
            &prepared.preview.plan,
            prepared.left_binding.clone(),
            prepared.right_binding.clone(),
            &prepared.left_snapshot,
            &prepared.right_snapshot,
        )?);
        prepared.journal.recovery_owner =
            prepared.owner.as_ref().map(|owner| owner.staging.clone());
        save(&prepared.store, &mut prepared.journal)?;
        prepared.previous_digest = Some(prepared.journal.digest);
        prepared.owns_preparation = true;
    }
    // Vorschau vollständig vorbereiten; beim Ausführen werden die einzelnen
    // Projektionen nach den gebundenen Vorgängerwirkungen erneut vorbereitet.
    for projection in if prepared.preview.stage_blocker.is_none() {
        planned_projections(&prepared.preview.plan)?
    } else {
        Vec::new()
    } {
        let transfer = prepared.projection(&projection.projection, cancel)?;
        prepared
            .preview
            .limitations
            .extend(transfer.preview().limitations.clone());
        prepared.preview.effects.push(transfer.preview().clone());
    }
    prepared.preview.limitations.sort();
    prepared.preview.limitations.dedup();
    prepared.preview.digest = serializable_digest(&prepared.preview)?;
    Ok(prepared)
}
impl PreparedForwardBidirectional {
    fn commits_baseline(&self) -> bool {
        self.baseline.is_none()
            || self.baseline.as_ref().is_some_and(|baseline| {
                baseline.version != BASELINE_VERSION
                    || baseline.left_binding != self.left_binding
                    || baseline.right_binding != self.right_binding
            })
            || self
                .preview
                .plan
                .actions
                .iter()
                .any(|action| !matches!(action, BidirectionalAction::SkippedConflict { .. }))
    }
    pub fn preview(&self) -> &ForwardBidirectionalPreview {
        &self.preview
    }
    pub fn left_endpoint(&self) -> &ProviderEndpoint {
        &self.left
    }
    pub fn right_endpoint(&self) -> &ProviderEndpoint {
        &self.right
    }

    pub fn conflict_choices_after_decisions(
        &self,
        resolutions: &[ConflictResolution],
        expected_digest: [u8; 32],
    ) -> io::Result<Vec<BidirectionalConflictView>> {
        if expected_digest != self.preview.unresolved_digest {
            return Err(crate::real_mode::plan_drift(
                "Konfliktentscheidungen gehören zu einer anderen Forwardvorbereitung",
            ));
        }
        let resolved = resolve_conflicts(
            &self.unresolved,
            &self.left_snapshot,
            &self.right_snapshot,
            resolutions,
        )?;
        forward_conflict_views(&resolved, &self.left_snapshot, &self.right_snapshot)
    }
    pub(crate) fn with_queue_owner(
        mut self,
        queue: &crate::OperationQueue,
        operation: &str,
        profiles: &[crate::RemoteProfileBinding],
        cancel: &Cancel,
    ) -> io::Result<Self> {
        let (preparation_owner, _, _) = queue.ftps_owner(None)?;
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.staging.queue != preparation_owner.queue)
        {
            return Err(crate::real_mode::plan_drift(
                "Die Vorbereitung gehört zu einer anderen Fensterqueue; bitte dort neu vorbereiten",
            ));
        }
        let left =
            self.left
                .with_queue_owner_with_cancel(queue, Some(operation), &[], Some(cancel))?;
        let right =
            self.right
                .with_queue_owner_with_cancel(queue, Some(operation), &[], Some(cancel))?;
        let (left_vfs, _, left_binding) = left.content_sync_snapshot_context(cancel)?;
        let (right_vfs, _, right_binding) = right.content_sync_snapshot_context(cancel)?;
        if left_binding != self.left_binding
            || right_binding != self.right_binding
            || left_vfs.root_identity(left.root())? != self.left_snapshot.root_identity
            || right_vfs.root_identity(right.root())? != self.right_snapshot.root_identity
        {
            return Err(crate::real_mode::plan_drift(
                "Forwardendpunkte änderten sich beim Binden an die Fensterqueue",
            ));
        }
        let (staging, _, _) = queue.ftps_owner(Some(operation))?;
        self.owner = Some(crate::remote_real_mode::InteractiveFileOwner {
            staging,
            profiles: profiles.to_vec(),
        });
        if self.owns_preparation {
            self.journal.recovery_owner = self.owner.as_ref().map(|owner| owner.staging.clone());
            save(&self.store, &mut self.journal)?;
            self.previous_digest = Some(self.journal.digest);
        }
        self.queue_execution_bound = true;
        self.left = left;
        self.right = right;
        self.left_vfs = left_vfs;
        self.right_vfs = right_vfs;
        Ok(self)
    }
    pub(crate) fn matches_queue_endpoints(
        &self,
        left: &crate::BidirectionalQueueEndpoint,
        right: &crate::BidirectionalQueueEndpoint,
    ) -> bool {
        self.left.matches_queue_endpoint(left) && self.right.matches_queue_endpoint(right)
    }

    /// Verwirft ausschließlich den eigenen noch nicht ausgeführten Auftrag.
    /// Ein bestehender Recoveryfall bleibt unverändert gesperrt.
    pub fn discard(mut self) -> io::Result<()> {
        if !self.owns_preparation {
            return Ok(());
        }
        if load(&self.store)?.as_ref().map(|journal| journal.digest) != self.previous_digest {
            return Err(crate::real_mode::plan_drift(
                "Forwardjournal änderte sich vor dem Verwerfen",
            ));
        }
        let active = self
            .journal
            .active
            .take()
            .ok_or_else(|| invalid_data("Vorbereitungsjournal fehlt"))?;
        if active.state != BidirectionalRunState::Prepared
            || active
                .projections
                .iter()
                .any(|projection| projection.state != BidirectionalProjectionState::Prepared)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Bereits ausgeführte Forwardwirkung darf nicht verworfen werden",
            ));
        }
        self.journal.history.push(active);
        save(&self.store, &mut self.journal)
    }
    fn projection(
        &self,
        kind: &BidirectionalProjectionKind,
        cancel: &Cancel,
    ) -> io::Result<PreparedContentSync> {
        let (source_side, target_side, selected, deleted, aliases, mode) = match kind {
            BidirectionalProjectionKind::Copy {
                direction,
                replacement,
                paths,
            } => {
                let (a, b) = direction_sides(*direction);
                (
                    a,
                    b,
                    paths
                        .iter()
                        .map(BinaryRelativePath::to_path_buf)
                        .collect::<io::Result<BTreeSet<_>>>()?,
                    BTreeSet::new(),
                    BTreeMap::new(),
                    if *replacement {
                        crate::SyncMode::Mirror
                    } else {
                        crate::SyncMode::AddOnly
                    },
                )
            }
            BidirectionalProjectionKind::Delete { direction, paths } => {
                let (a, b) = direction_sides(*direction);
                (
                    a,
                    b,
                    BTreeSet::new(),
                    paths
                        .iter()
                        .map(BinaryRelativePath::to_path_buf)
                        .collect::<io::Result<BTreeSet<_>>>()?,
                    BTreeMap::new(),
                    crate::SyncMode::Mirror,
                )
            }
            BidirectionalProjectionKind::ConflictCopy {
                source_path,
                target_path,
                source_side,
                target_side,
                subtree,
            } => {
                let mut selected = BTreeSet::from([target_path.to_path_buf()?]);
                let mut aliases =
                    BTreeMap::from([(target_path.to_path_buf()?, source_path.to_path_buf()?)]);
                for path in subtree {
                    selected.insert(path.target_path.to_path_buf()?);
                    aliases.insert(
                        path.target_path.to_path_buf()?,
                        path.source_path.to_path_buf()?,
                    );
                }
                (
                    *source_side,
                    *target_side,
                    selected,
                    BTreeSet::new(),
                    aliases,
                    crate::SyncMode::AddOnly,
                )
            }
            BidirectionalProjectionKind::TypeSwap { .. } => {
                return Err(invalid_input("Typwechsel ist kein Forwardtransfer"))
            }
        };
        let endpoint = |side| {
            if side == EndpointSide::Left {
                (&self.left, &self.left_vfs)
            } else {
                (&self.right, &self.right_vfs)
            }
        };
        let (source, source_vfs) = endpoint(source_side);
        let (target, target_vfs) = endpoint(target_side);
        let projection = Arc::new(LocalProjectedVfs::new_with_aliases(
            source_vfs.clone(),
            source.root().to_path_buf(),
            target_vfs.clone(),
            target.root().to_path_buf(),
            selected,
            deleted,
            aliases,
        )?);
        let options = crate::SyncOptions {
            mode,
            compare: crate::Compare::ContentHash,
            metadata: MetadataProfile::Portable,
            mtime_tolerance: 2,
            exclude: Vec::new(),
        };
        match &self.owner {
            Some(owner) => crate::content_sync::prepare_projected_content_sync_owned(
                source.clone(),
                target.clone(),
                projection,
                options,
                cancel,
                Some(owner.clone()),
            ),
            None => crate::content_sync::prepare_projected_content_sync(
                source.clone(),
                target.clone(),
                projection,
                options,
                cancel,
            ),
        }
    }
    pub fn confirm_effects(
        self,
        digest: [u8; 32],
        effects_confirmed: bool,
    ) -> io::Result<ForwardBidirectionalRun> {
        self.confirm_effects_with_removal_phrases(digest, effects_confirmed, &[])
    }
    pub fn confirm_effects_with_removal_phrases(
        mut self,
        digest: [u8; 32],
        effects_confirmed: bool,
        removal_phrases: &[String],
    ) -> io::Result<ForwardBidirectionalRun> {
        if !self.queue_execution_bound
            && (!self.preview.effects.is_empty() || self.commits_baseline())
        {
            self.discard()?;
            return Err(crate::coded_error(io::ErrorKind::Unsupported, crate::ErrorCode::Unsupported,
                "Mutierender Forwardlauf benötigt den dauerhaft gebundenen Queueowner; über die Forwardqueue vorbereiten und bestätigen."));
        }
        for phrase in self
            .preview
            .effects
            .iter()
            .filter_map(|effect| effect.removal_confirmation.as_ref())
        {
            if !removal_phrases.contains(phrase) {
                return Err(crate::real_mode::confirmation_mismatch(
                    "Große dauerhafte Forwardentfernung verlangt die genaue angezeigte Phrase",
                ));
            }
        }
        if let Some(reason) = &self.preview.stage_blocker {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, reason.clone()));
        }
        if self.preview.digest != digest || !effects_confirmed {
            return Err(crate::real_mode::confirmation_mismatch(
                "Die frischen Forwardwirkungen wurden nicht bestätigt",
            ));
        }
        if self.created.elapsed() > crate::REAL_PERMIT_LIFETIME {
            return Err(crate::real_mode::plan_drift(
                "Forwardvorbereitung ist abgelaufen",
            ));
        }
        if self.store.load()? != self.baseline
            || load(&self.store)?.as_ref().map(|journal| journal.digest) != self.previous_digest
        {
            return Err(crate::real_mode::plan_drift(
                "Baseline oder Forwardjournal änderten sich",
            ));
        }
        let ForwardSnapshots {
            left_binding,
            right_binding,
            left,
            right,
            ..
        } = snapshots(
            &self.left,
            &self.right,
            &Cancel::new(),
            Some(&self.journal),
            self.owner.as_ref(),
        )?;
        if left_binding != self.left_binding
            || right_binding != self.right_binding
            || left != self.left_snapshot
            || right != self.right_snapshot
        {
            return Err(crate::real_mode::plan_drift(
                "Forwardbestand änderte sich seit der Vorschau",
            ));
        }
        if !self.owns_preparation {
            if let Some(previous) = self.journal.active.take() {
                self.journal.history.push(previous);
            }
            self.journal.active = Some(BidirectionalRunJournal::new(
                &self.preview.plan,
                self.left_binding.clone(),
                self.right_binding.clone(),
                &left,
                &right,
            )?);
        }
        self.journal.recovery_owner = self.owner.as_ref().map(|owner| owner.staging.clone());
        save(&self.store, &mut self.journal)?;
        Ok(ForwardBidirectionalRun { prepared: self })
    }
}
impl ForwardBidirectionalRun {
    pub fn execute(
        self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<ForwardBidirectionalStats> {
        self.execute_internal(cancel, on_progress, false)
    }
    pub(crate) fn execute_retaining_journal(
        self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<ForwardBidirectionalStats> {
        self.execute_internal(cancel, on_progress, true)
    }
    fn execute_internal(
        mut self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
        retain_completion: bool,
    ) -> io::Result<ForwardBidirectionalStats> {
        let mut results = Vec::new();
        let (baseline_generation, baseline_committed) = self
            .execute_with_stats(cancel, on_progress, &mut results, retain_completion)
            .map_err(|error| {
                let paths = results
                    .iter()
                    .flat_map(|result| result.temp_recovery_paths.iter().cloned())
                    .collect::<Vec<_>>();
                crate::error::with_recovery_paths(error, &paths)
            })?;
        Ok(ForwardBidirectionalStats {
            runs: results,
            baseline_generation,
            baseline_committed,
        })
    }
    fn execute_with_stats(
        &mut self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
        results: &mut Vec<SyncStats>,
        retain_completion: bool,
    ) -> io::Result<(u64, bool)> {
        let prepared = &mut self.prepared;
        if prepared.created.elapsed() > crate::REAL_PERMIT_LIFETIME {
            return Err(crate::real_mode::plan_drift(
                "Forwardpermit ist abgelaufen; frisch planen und bestätigen",
            ));
        }
        for (index, projection) in planned_projections(&prepared.preview.plan)?
            .into_iter()
            .enumerate()
        {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Forwardlauf abgebrochen; gemeinsame Baseline bleibt erhalten",
                ));
            }
            if prepared.created.elapsed() > crate::REAL_PERMIT_LIFETIME {
                return Err(crate::real_mode::plan_drift(
                    "Forwardpermit lief vor der nächsten Projektion ab",
                ));
            }
            let ForwardSnapshots {
                left_binding,
                right_binding,
                left,
                right,
                ..
            } = snapshots(
                &prepared.left,
                &prepared.right,
                cancel,
                Some(&prepared.journal),
                prepared.owner.as_ref(),
            )?;
            if left_binding != prepared.left_binding || right_binding != prepared.right_binding {
                return Err(crate::real_mode::plan_drift(
                    "Forward-Endpunktidentität änderte sich",
                ));
            }
            let active = prepared
                .journal
                .active
                .as_ref()
                .ok_or_else(|| invalid_data("Forwardintent fehlt"))?;
            if !snapshots_match_projection_prefix(active, &left, &right, index)? {
                return Err(crate::real_mode::plan_drift(
                    "Forwardbestand driftete außerhalb der bestätigten Vorwirkungen",
                ));
            }
            let transfer = prepared.projection(&projection.projection, cancel)?;
            let expected = &prepared.preview.effects[index];
            let actual = transfer.preview();
            if actual.plan.actions != expected.plan.actions
                || actual.irreversible_replacements > expected.irreversible_replacements
                || actual.irreversible_removals > expected.irreversible_removals
                || actual.removal_confirmation != expected.removal_confirmation
            {
                return Err(crate::real_mode::plan_drift(
                    "Forwardwirkung oder Wiederherstellbarkeit änderte sich; neu bestätigen",
                ));
            }
            let digest = transfer.preview().digest;
            let phrase = transfer.preview().removal_confirmation.clone();
            let run =
                transfer.confirm_effects_with_removal_phrase(digest, true, phrase.as_deref())?;
            let active = prepared.journal.active.as_mut().unwrap();
            active.state = BidirectionalRunState::Running;
            active.projections[index].state = BidirectionalProjectionState::Running;
            active.refresh_digest()?;
            save(&prepared.store, &mut prepared.journal)?;
            let result = run.execute(cancel, on_progress)?;
            // Die letzte eindeutige Running-Zuordnung vor dem Richtungswechsel
            // behalten; ein offener Zwischenstand darf nicht der Folgeseite gelten.
            bind_staging_candidates(&mut prepared.journal)?;
            reject_unresolved_staging(
                &prepared.journal,
                &prepared.left,
                &prepared.left_vfs,
                &prepared.left_binding,
                &prepared.right,
                &prepared.right_vfs,
                &prepared.right_binding,
            )?;
            if result.cancelled {
                return Err(cancelled_projection_error(
                    "Forwardteilwirkung bleibt bis zur frischen Recoveryplanung gesperrt",
                    &result,
                ));
            }
            let quarantine = result.quarantine.clone();
            results.push(result);
            let active = prepared.journal.active.as_mut().unwrap();
            active.projections[index].state = BidirectionalProjectionState::Committed;
            active.projections[index].quarantine = quarantine;
            active.refresh_digest()?;
            save(&prepared.store, &mut prepared.journal)?;
        }
        let ForwardSnapshots {
            left_binding,
            right_binding,
            left,
            right,
            ..
        } = snapshots(
            &prepared.left,
            &prepared.right,
            cancel,
            Some(&prepared.journal),
            prepared.owner.as_ref(),
        )?;
        let ForwardSnapshots {
            left: second_left,
            right: second_right,
            namespace_epoch: completion_epoch,
            ..
        } = snapshots(
            &prepared.left,
            &prepared.right,
            cancel,
            Some(&prepared.journal),
            prepared.owner.as_ref(),
        )?;
        if left_binding != prepared.left_binding || right_binding != prepared.right_binding {
            return Err(crate::real_mode::plan_drift(
                "Forward-Endpunktidentität änderte sich vor dem Abschluss",
            ));
        }
        if left != second_left
            || right != second_right
            || !snapshots_equal_except_skipped(&left, &right, &prepared.preview.plan)
        {
            return Err(crate::real_mode::plan_drift(
                "Forwardabschluss besitzt keine stabilen gemeinsamen Bäume",
            ));
        }
        if prepared.store.load()? != prepared.baseline {
            return Err(crate::real_mode::plan_drift(
                "Gemeinsame Baseline änderte sich während des Forwardlaufs",
            ));
        }
        let commit = prepared.commits_baseline();
        let generation = if commit {
            let next = match &prepared.baseline {
                Some(previous) => previous.advance_from_resolved_snapshots(
                    left_binding,
                    right_binding,
                    &left,
                    &right,
                    &prepared.preview.plan,
                )?,
                None => baseline_from_resolved_snapshots(
                    prepared.preview.plan.pair_id,
                    left_binding,
                    right_binding,
                    &left,
                    &right,
                    &prepared.preview.plan,
                )?,
            };
            let active = prepared.journal.active.as_mut().unwrap();
            active.state = BidirectionalRunState::Running;
            active.committed_baseline_digest = Some(next.body_digest);
            active.refresh_digest()?;
            save(&prepared.store, &mut prepared.journal)?;
            reject_unresolved_staging(
                &prepared.journal,
                &prepared.left,
                &prepared.left_vfs,
                &prepared.left_binding,
                &prepared.right,
                &prepared.right_vfs,
                &prepared.right_binding,
            )?;
            let config = prepared
                .owner
                .as_ref()
                .and_then(|owner| owner.staging.queue.parent());
            if namespace_guard(
                config,
                &prepared.left,
                &prepared.left_vfs,
                &prepared.left_binding,
                &prepared.right,
                &prepared.right_vfs,
                &prepared.right_binding,
            )? != completion_epoch
            {
                return Err(crate::real_mode::plan_drift(
                    "Relevante Tempwirkungen änderten sich vor dem Baselinecommit",
                ));
            }
            prepared.store.save(&next)?;
            next.generation
        } else {
            prepared.baseline.as_ref().unwrap().generation
        };
        let active = prepared
            .journal
            .active
            .as_mut()
            .ok_or_else(|| invalid_data("Forwardabschlussintent fehlt"))?;
        active.state = BidirectionalRunState::BaselineCommitted;
        if active.committed_baseline_digest.is_none() {
            active.committed_baseline_digest = prepared
                .baseline
                .as_ref()
                .map(|baseline| baseline.body_digest);
        }
        active.refresh_digest()?;
        if !retain_completion {
            let active = prepared.journal.active.take().unwrap();
            prepared.journal.history.push(active);
        }
        save(&prepared.store, &mut prepared.journal)?;
        Ok((generation, commit))
    }
}

/// Beendet ausschließlich einen bereits dauerhaft committed Baselineabschluss.
/// Es werden keine Providerdaten verändert und keine offene Mutation wiederholt.
pub fn finish_forward_bidirectional_completion(baseline_path: &Path) -> io::Result<()> {
    let store = BaselineStore::open(baseline_path)?;
    let mut journal = load(&store)?.ok_or_else(|| invalid_data("Forwardjournal fehlt"))?;
    let active = journal
        .active
        .as_ref()
        .ok_or_else(|| invalid_input("Kein offener Forwardabschluss"))?;
    let baseline = store
        .load()?
        .ok_or_else(|| invalid_data("Gemeinsame Baseline fehlt"))?;
    if active.committed_baseline_digest != Some(baseline.body_digest) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Forwarddaten sind noch nicht vollständig committed",
        ));
    }
    store.sync_baseline(&baseline)?;
    let mut active = journal.active.take().unwrap();
    active.state = BidirectionalRunState::BaselineCommitted;
    active.refresh_digest()?;
    journal.history.push(active);
    save(&store, &mut journal)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        _temp: tempfile::TempDir,
        left: PathBuf,
        right: PathBuf,
        baseline: PathBuf,
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
                pair: PairId::generate().unwrap(),
            }
        }
        fn prepare(&self, recovery: bool) -> io::Result<PreparedForwardBidirectional> {
            let queue = crate::OperationQueue::open(
                self.baseline
                    .parent()
                    .unwrap()
                    .join("config/operations-queue.json"),
            )?;
            let (staging, _, _) = queue.ftps_owner(None)?;
            prepare_forward_bidirectional_owned(
                self.pair,
                ProviderEndpoint::local(&self.left)?,
                ProviderEndpoint::local(&self.right)?,
                &self.baseline,
                &[],
                recovery,
                None,
                &Cancel::new(),
                Some(crate::remote_real_mode::InteractiveFileOwner {
                    staging,
                    profiles: Vec::new(),
                }),
            )
        }
        fn execute(&self) -> ForwardBidirectionalStats {
            let prepared = self.prepare(false).unwrap();
            let digest = prepared.preview().digest;
            prepared
                .confirm_effects(digest, true)
                .unwrap()
                .execute(&Cancel::new(), &mut |_| {})
                .unwrap()
        }
    }
    #[test]
    fn forward_child_exits_after_first_committed_file() {
        let Some(root) = std::env::var_os("DOPPELDECKER_FORWARD_CRASH_FIXTURE") else {
            return;
        };
        let root = PathBuf::from(root);
        let pair: PairId =
            serde_json::from_str(&std::env::var("DOPPELDECKER_FORWARD_CRASH_PAIR").unwrap())
                .unwrap();
        let queue = crate::OperationQueue::open(root.join("config/operations-queue.json")).unwrap();
        let (staging, _, _) = queue.ftps_owner(None).unwrap();
        let prepared = prepare_forward_bidirectional_owned(
            pair,
            ProviderEndpoint::local(&root.join("left")).unwrap(),
            ProviderEndpoint::local(&root.join("right")).unwrap(),
            &root.join("baseline.json"),
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
            .execute(&Cancel::new(), &mut |progress| {
                if progress.files_done == 1 {
                    std::process::exit(17);
                }
            })
            .unwrap();
        panic!("Der erste Dateicommit wurde nicht erreicht");
    }
    #[test]
    fn forward_partial_first_direction_replans_against_unchanged_baseline() {
        let fixture = Fixture::new();
        for name in ["a", "b", "c"] {
            std::fs::write(fixture.left.join(name), name).unwrap();
            std::fs::write(fixture.right.join(name), name).unwrap();
        }
        assert_eq!(fixture.execute().baseline_generation, 1);
        let b0 = std::fs::read(&fixture.baseline).unwrap();
        std::fs::write(fixture.left.join("a"), "a-new").unwrap();
        std::fs::write(fixture.left.join("b"), "b-new").unwrap();
        std::fs::write(fixture.right.join("c"), "c-new").unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bidirectional::forward::tests::forward_child_exits_after_first_committed_file",
                "--nocapture",
            ])
            .env(
                "DOPPELDECKER_FORWARD_CRASH_FIXTURE",
                fixture.baseline.parent().unwrap(),
            )
            .env(
                "DOPPELDECKER_FORWARD_CRASH_PAIR",
                serde_json::to_string(&fixture.pair).unwrap(),
            )
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(17),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(std::fs::read(&fixture.baseline).unwrap(), b0);
        let status = forward_bidirectional_journal_status(&fixture.baseline)
            .unwrap()
            .unwrap();
        assert!(status.baseline_matches_previous);
        assert!(!status.baseline_completion_available);
        assert_eq!(
            std::fs::read_to_string(fixture.right.join("a")).unwrap(),
            "a-new"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.right.join("b")).unwrap(),
            "b"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.left.join("c")).unwrap(),
            "c"
        );
        assert_eq!(
            fixture.prepare(false).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        let store = BaselineStore::open(&fixture.baseline).unwrap();
        assert_eq!(
            store.load_run_journal().err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        let journal = load(&store).unwrap().unwrap();
        assert_eq!(
            journal
                .active
                .as_ref()
                .unwrap()
                .projections
                .iter()
                .filter(|projection| projection.state == BidirectionalProjectionState::Running)
                .count(),
            1
        );
        drop(store);
        let recovery = fixture.prepare(true).unwrap();
        let observed = recovery.preview().recovery.as_ref().unwrap();
        assert!(observed
            .changed_right
            .iter()
            .any(|path| path.display_lossy() == "a"));
        assert_eq!(recovery.preview().plan.baseline_generation, Some(1));
        let digest = recovery.preview().digest;
        let result = recovery
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(result.baseline_generation, 2);
        for name in ["a", "b", "c"] {
            assert_eq!(
                std::fs::read(fixture.left.join(name)).unwrap(),
                std::fs::read(fixture.right.join(name)).unwrap()
            );
        }
        let prepared = fixture.prepare(false).unwrap();
        assert!(planned_projections(&prepared.preview().plan)
            .unwrap()
            .is_empty());
        let digest = prepared.preview().digest;
        let result = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert!(!result.baseline_committed);
        assert_eq!(result.baseline_generation, 2);
        let store = BaselineStore::open(&fixture.baseline).unwrap();
        let journal = load(&store).unwrap().unwrap();
        assert!(journal.active.is_none());
        assert_eq!(journal.history.len(), 4);
    }
    #[test]
    fn forward_replacement_and_deletion_keep_local_quarantine() {
        let fixture = Fixture::new();
        for name in ["replace", "remove"] {
            std::fs::write(fixture.left.join(name), "old").unwrap();
            std::fs::write(fixture.right.join(name), "old").unwrap();
        }
        fixture.execute();
        std::fs::write(fixture.left.join("replace"), "new").unwrap();
        std::fs::remove_file(fixture.left.join("remove")).unwrap();
        let prepared = fixture.prepare(false).unwrap();
        assert!(prepared.preview().effects.iter().all(
            |effect| effect.irreversible_removals == 0 && effect.irreversible_replacements == 0
        ));
        assert!(prepared
            .preview()
            .effects
            .iter()
            .any(|effect| effect.quarantined_removals > 0));
        let digest = prepared.preview().digest;
        let result = prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert!(result.runs.iter().any(|run| run.quarantine.is_some()));
        let store = BaselineStore::open(&fixture.baseline).unwrap();
        let baseline = store.load().unwrap().unwrap();
        assert!(baseline
            .tombstones
            .iter()
            .any(|stone| stone.path.display_lossy() == "remove"));
    }
    #[test]
    fn forward_type_skip_preserves_baseline_subtree_and_independent_effects() {
        let fixture = Fixture::new();
        for root in [&fixture.left, &fixture.right] {
            std::fs::write(root.join("type"), "baseline").unwrap();
        }
        fixture.execute();
        let original = {
            let store = BaselineStore::open(&fixture.baseline).unwrap();
            store.load().unwrap().unwrap().entries
        };
        std::fs::remove_file(fixture.left.join("type")).unwrap();
        std::fs::create_dir(fixture.left.join("type")).unwrap();
        std::fs::write(fixture.left.join("type/child"), "changed").unwrap();
        std::fs::write(fixture.right.join("independent"), "copy").unwrap();
        let prepared = fixture.prepare(false).unwrap();
        assert!(prepared
            .preview()
            .limitations
            .iter()
            .any(
                |limitation| limitation.starts_with("type:") && limitation.contains("ausgelassen")
            ));
        assert!(prepared.preview().plan.actions.iter().any(|action|matches!(action,BidirectionalAction::SkippedConflict {path,..} if path.display_lossy()=="type")));
        let digest = prepared.preview().digest;
        prepared
            .confirm_effects(digest, true)
            .unwrap()
            .execute(&Cancel::new(), &mut |_| {})
            .unwrap();
        assert!(fixture.left.join("type").is_dir());
        assert!(fixture.right.join("type").is_file());
        assert_eq!(
            std::fs::read(fixture.left.join("independent")).unwrap(),
            b"copy"
        );
        let store = BaselineStore::open(&fixture.baseline).unwrap();
        let baseline = store.load().unwrap().unwrap();
        assert!(baseline.entries.contains(&original[0]));
        assert!(!baseline
            .entries
            .iter()
            .any(|entry| entry.path.display_lossy() == "type/child"));
    }
}
