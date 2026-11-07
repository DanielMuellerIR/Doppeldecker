//! Gewöhnlicher, wirkungsgebundener Inhaltssync über die privaten Dateikontexte.
//!
//! Diese Klasse erteilt keine strikten Providerfähigkeiten und verspricht keine
//! Quarantäne. Planer, Transfer, Metadatenbilanz und Commit bleiben gemeinsame Kerne.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::real_mode::{confirmation_mismatch, plan_drift, ListingProof, REAL_PERMIT_LIFETIME};
use crate::{
    Cancel, Compare, Progress, ProviderEndpoint, ProviderKind, RootIdentity, Safety, SyncMode,
    SyncOptions, SyncPlan, SyncStats, Vfs,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ContentSyncPreview {
    pub execution_class: crate::sync::SyncExecutionClass,
    pub source_root: String,
    pub target_root: String,
    pub source_provider: ProviderKind,
    pub source_identity: RootIdentity,
    pub target_identity: RootIdentity,
    pub target_provider: ProviderKind,
    pub options: SyncOptions,
    pub plan: SyncPlan,
    pub digest: [u8; 32],
    pub expires_unix_ms: u64,
    pub limitations: Vec<String>,
    pub quarantined_replacements: u64,
    pub quarantined_removals: u64,
    pub quarantine_run: Option<String>,
    pub restore_probe_ok: bool,
    pub local_execution: Option<crate::real_mode::LocalSyncExecution>,
    pub irreversible_replacements: u64,
    pub irreversible_removals: u64,
    pub irreversible_removal_bytes: u64,
    pub blocked_removals: u64,
    pub removal_confirmation: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub directory_stream_effects: Vec<crate::directory_streams::DirectoryStreamOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directory_dacl_effects: Vec<crate::directory_dacl::DirectoryDaclOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_dacl_effects: Vec<crate::file_dacl::FileDaclOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_directory_dacl_effects:
        Vec<crate::created_directory_dacl::CreatedDirectoryDaclPreviewEffect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_directory_before_children_effects:
        Vec<crate::created_directory_dacl::CreatedDirectoryBeforeChildrenPreviewEffect>,
}

pub struct PreparedContentSync {
    source_endpoint: ProviderEndpoint,
    target_endpoint: ProviderEndpoint,
    source: Arc<dyn Vfs>,
    source_root: PathBuf,
    real_source: Option<RealContentSource>,
    target: Arc<dyn Vfs>,
    preview: ContentSyncPreview,
    quarantine: Option<LocalContentQuarantine>,
    removal_plans: Vec<crate::RemoteDeletePlan>,
    owner: Option<crate::remote_real_mode::InteractiveFileOwner>,
    staging: Option<ContentStaging>,
    queue_execution_bound: bool,
    source_listing: ListingProof,
    target_listing: ListingProof,
    created: Instant,
    cancel: Cancel,
}

pub struct ContentSyncRun {
    prepared: PreparedContentSync,
}

/// Geschlossenes Einmalrecht ausschließlich für den bereits bestätigten Plan.
pub struct ContentSyncPermit {
    source_root: PathBuf,
    target_root: PathBuf,
    source_identity: RootIdentity,
    target_identity: RootIdentity,
    plan: SyncPlan,
    created: Instant,
    consumed: AtomicBool,
    source_listing: ListingProof,
    target_endpoint: ProviderEndpoint,
    quarantine: Option<LocalContentQuarantine>,
    local_execution: Option<crate::real_mode::LocalSyncExecution>,
    irreversible_replacements: bool,
    metadata_profile: crate::MetadataProfile,
    real_source: Option<RealContentSource>,
    removal_plans: Vec<crate::RemoteDeletePlan>,
    owner: Option<crate::remote_real_mode::InteractiveFileOwner>,
    staging: Option<ContentStaging>,
}

#[derive(Clone)]
struct ContentStaging {
    registry: crate::staging_guard::KnownStagingRegistry,
    endpoints: Vec<crate::staging_guard::StagingEndpoint>,
    epoch: [u8; 32],
}
impl ContentStaging {
    fn adopt_after_probe(
        &mut self,
        owner: Option<&crate::local_staging::StagingOwner>,
        foreign_epoch: Option<[u8; 32]>,
        drift_message: &str,
    ) -> io::Result<()> {
        // Die gemeinsame Epoche vor der letzten Fremdprüfung lesen. Andernfalls
        // könnte ein fremder Begin-/Finish-Zyklus zwischen Prüfung und Snapshot
        // unbemerkt in die neue Freigabe aufgenommen werden.
        let epoch = self.registry.check(&self.endpoints, None)?;
        #[cfg(test)]
        staging_rebind_hook(StagingRebindPoint::AfterGeneralSnapshot);
        if Some(self.registry.check(&self.endpoints, owner)?) != foreign_epoch {
            return Err(plan_drift(drift_message));
        }
        #[cfg(test)]
        staging_rebind_hook(StagingRebindPoint::AfterForeignCheck);
        // Spätere Änderungen weichen von diesem bereits gelesenen Snapshot ab
        // und werden durch die anschließende validate-Prüfung erkannt.
        self.epoch = epoch;
        Ok(())
    }

    fn complete_directory_dacl(
        &mut self,
        owner: Option<&crate::local_staging::StagingOwner>,
    ) -> io::Result<()> {
        // Nur vollständig geschlossene eigene DACL-Intents sind aus der
        // Fremdepoche ausgenommen. Offene/unklare Intents bleiben auch mit Owner
        // durch die Registry gesperrt. Die neue gemeinsame Epoche erst nach dem
        // Vergleich mit dem vor dem Lauf gebundenen Fremdstand übernehmen.
        self.adopt_after_probe(
            owner,
            Some(self.epoch),
            "Fremde Zwischenstände änderten sich während des Verzeichnis-DACL-Abschlusses",
        )?;
        self.validate(None)
    }

    fn validate(&self, owner: Option<&crate::local_staging::StagingOwner>) -> io::Result<()> {
        if self.registry.check(&self.endpoints, owner)? != self.epoch {
            return Err(plan_drift(
                "Bekannte fremde Zwischenstände änderten sich seit der Inhaltssync-Planung",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum StagingRebindPoint {
    AfterGeneralSnapshot,
    AfterForeignCheck,
}
#[cfg(test)]
type StagingRebindHook = (StagingRebindPoint, Box<dyn FnOnce()>);
#[cfg(test)]
thread_local! {
    static STAGING_REBIND_HOOK: std::cell::RefCell<Option<StagingRebindHook>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn staging_rebind_hook(point: StagingRebindPoint) {
    let hook = STAGING_REBIND_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|(expected, _)| *expected == point)
        {
            slot.take().map(|(_, hook)| hook)
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        hook();
    }
}

#[derive(Clone)]
struct RealContentSource {
    vfs: Arc<dyn Vfs>,
    root: PathBuf,
    identity: RootIdentity,
    listing: ListingProof,
}
impl RealContentSource {
    fn validate(&self, cancel: &Cancel, full: bool) -> io::Result<()> {
        if !self.vfs.anchor_is_current()? || self.vfs.root_identity(&self.root)? != self.identity {
            return Err(plan_drift(
                "Echte Projektionsquelle änderte ihre Root- oder Providersitzungsbindung",
            ));
        }
        if full {
            let current =
                complete_proof(self.vfs.as_ref(), &self.root, self.identity.clone(), cancel)?;
            if !same_listing(&current, &self.listing) {
                return Err(plan_drift(
                    "Echter Quellbestand der Projektion änderte sich seit der Vorbereitung",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct LocalContentQuarantine {
    base: PathBuf,
    identity: RootIdentity,
    run: PathBuf,
}

impl std::fmt::Debug for ContentSyncPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentSyncPermit")
            .field("source_root", &self.source_root)
            .field("target_root", &self.target_root)
            .finish_non_exhaustive()
    }
}
impl ContentSyncPermit {
    pub(crate) fn execute_removals(
        &self,
        source: &dyn Vfs,
        target: &dyn Vfs,
        cancel: &Cancel,
        completed: &mut u64,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<bool> {
        if self.plan.delete_count == 0 {
            return Ok(false);
        }
        self.validate_roots(source, target)?;
        if let Some(real) = &self.real_source {
            real.validate(cancel, true)?;
        } else {
            let fresh = complete_proof(
                source,
                &self.source_root,
                self.source_identity.clone(),
                cancel,
            )?;
            if !same_listing(&fresh, &self.source_listing) {
                return Err(plan_drift("Quelle änderte sich nach den Kopien; sämtliche Mirror-Entfernungen bleiben gesperrt"));
            }
        }
        if self.quarantine.is_some() {
            return Ok(false);
        }
        let mut removed = *completed;
        let mut removed_bytes = 0u64;
        let total_bytes = self.removal_plans.iter().map(|plan| plan.bytes).sum();
        for plan in &self.removal_plans {
            if cancel.is_cancelled() {
                return Ok(true);
            }
            self.validate_roots(source, target)?;
            // Eigene vorherige Kindkopien dürfen Collectionversionen ändern. Der
            // neue Löschkontext bindet diese aktuelle Parentkette; die Liste der
            // erlaubten Löschobjekte stammt ausschließlich aus der Bestätigung.
            let deletion = self
                .target_endpoint
                .content_sync_delete_context(cancel, self.owner.clone())?;
            let stats = crate::remote_delete::execute_content_sync_permanent(
                deletion.as_ref(), plan, cancel, &mut |progress| {
                    let mut cumulative=progress.clone();
                    cumulative.files_total=self.plan.delete_count;
                    cumulative.bytes_total=total_bytes;
                    cumulative.files_done=removed.saturating_add(progress.files_done);
                    cumulative.bytes_done=removed_bytes.saturating_add(progress.bytes_done);
                    *completed=cumulative.files_done;
                    on_progress(&cumulative);
                },
            ).map_err(|error| if *completed>0 {crate::error::with_context(error,format!("Bereits {} Mirror-Einträge wurden nachweislich dauerhaft entfernt; diese Teilwirkung bleibt erhalten",*completed))} else {error})?;
            removed = removed.saturating_add(stats.entries);
            *completed = removed;
            removed_bytes = removed_bytes.saturating_add(stats.bytes);
            if stats.entries > 0 {
                target.observe_content_sync_removals(Path::new(&plan.root))?;
            }
            if stats.cancelled {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn quarantine_run(&self) -> Option<PathBuf> {
        self.quarantine
            .as_ref()
            .map(|quarantine| quarantine.run.clone())
    }
    pub(crate) fn local_execution(&self) -> Option<&crate::real_mode::LocalSyncExecution> {
        self.local_execution.as_ref()
    }
    pub(crate) fn irreversible_replacements(&self) -> bool {
        self.irreversible_replacements
    }
    pub(crate) fn allows_metadata_omission(&self) -> bool {
        self.metadata_profile != crate::MetadataProfile::NativeStrict
    }

    pub(crate) fn mtime_omission<'a>(
        &self,
        error: &'a io::Error,
    ) -> Option<&'a KnownMtimeOmission> {
        mtime_omission_reason(error)
            .filter(|omission| omission.allowed_in_profile(self.metadata_profile))
    }

    pub(crate) fn validate_roots(&self, source: &dyn Vfs, target: &dyn Vfs) -> io::Result<()> {
        if let Some(staging) = &self.staging {
            staging.validate(self.owner.as_ref().map(|owner| &owner.staging))?;
        }

        if let Some(quarantine) = &self.quarantine {
            if target.root_identity(&quarantine.base)? != quarantine.identity {
                return Err(plan_drift(
                    "Lokale Inhaltssync-Quarantäne wurde ausgetauscht",
                ));
            }
        }
        if let Some(expected) = &self.local_execution {
            let mut current =
                crate::real_mode::LocalSyncExecution::read(target, &self.target_root)?;
            current.irreversible_replacements = expected.irreversible_replacements;
            if &current != expected {
                return Err(plan_drift(
                    "Lokale Inhaltssync-Zielfähigkeiten änderten sich",
                ));
            }
        }
        if let Some(real) = &self.real_source {
            real.validate(&Cancel::new(), false)?;
        }
        let current_source = source.root_identity(&self.source_root)?;
        let source_matches = if self.real_source.is_some() {
            same_target_identity(&current_source, &self.source_identity)
        } else {
            current_source == self.source_identity
        };
        if !source_matches
            || !same_target_identity(
                &target.root_identity(&self.target_root)?,
                &self.target_identity,
            )
            || !source.anchor_is_current()?
            || !target.anchor_is_current()?
        {
            return Err(plan_drift(
                "Inhaltssync: Root oder Providersitzung änderte sich",
            ));
        }
        Ok(())
    }
    pub(crate) fn consume(
        &self,
        source: &dyn Vfs,
        source_root: &Path,
        target: &dyn Vfs,
        target_root: &Path,
        plan: &SyncPlan,
    ) -> io::Result<()> {
        if self.created.elapsed() > REAL_PERMIT_LIFETIME {
            return Err(plan_drift(
                "Inhaltssync-Vorbereitung ist abgelaufen; neu prüfen",
            ));
        }
        if source_root != self.source_root || target_root != self.target_root || plan != &self.plan
        {
            return Err(plan_drift(
                "Inhaltssync-Permit gehört zu einem anderen Plan",
            ));
        }
        self.validate_roots(source, target)?;
        if self.consumed.swap(true, Ordering::SeqCst) {
            return Err(confirmation_mismatch(
                "Inhaltssync-Permit wurde bereits verbraucht",
            ));
        }
        Ok(())
    }
}

pub fn validate_content_sync_options(options: &SyncOptions) -> io::Result<()> {
    options.validate()?;
    if (matches!(options.mode, SyncMode::Update | SyncMode::Mirror)
        || options.metadata != crate::MetadataProfile::Portable)
        && options.compare != Compare::ContentHash
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Update, Mirror und native Metadaten benötigen vollständigen Inhaltsvergleich",
        ));
    }
    Ok(())
}

pub fn prepare_content_sync(
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    options: SyncOptions,
    cancel: &Cancel,
) -> io::Result<PreparedContentSync> {
    prepare_content_sync_owned(source, target, options, cancel, None)
}

/// Vorschau im privaten Stagingbereich der verursachenden Fensterqueue.
/// Hier entsteht weder ein Queueeintrag noch ein Schreibpermit.
pub fn prepare_content_sync_for_queue(
    queue: &crate::OperationQueue,
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    options: SyncOptions,
    cancel: &Cancel,
) -> io::Result<PreparedContentSync> {
    validate_content_sync_options(&options)?;
    let registry = crate::staging_guard::KnownStagingRegistry::for_queue(queue)?;
    let (staging, _, _) = queue.ftps_owner(None)?;
    let source = queue.bind_provider_preparation_with_cancel(&source, cancel)?;
    let target = queue.bind_provider_preparation_with_cancel(&target, cancel)?;
    let mut prepared = prepare_content_sync_contextual(
        source,
        target,
        options,
        cancel,
        Some(crate::remote_real_mode::InteractiveFileOwner {
            staging,
            profiles: Vec::new(),
        }),
        None,
        Some(registry),
    )?;
    prepared.queue_execution_bound = false;
    Ok(prepared)
}

pub(crate) fn prepare_content_sync_owned(
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    options: SyncOptions,
    cancel: &Cancel,
    owner: Option<crate::remote_real_mode::InteractiveFileOwner>,
) -> io::Result<PreparedContentSync> {
    let registry = owner
        .as_ref()
        .map(|owner| crate::staging_guard::KnownStagingRegistry::for_owner(&owner.staging))
        .transpose()?;
    prepare_content_sync_contextual(source, target, options, cancel, owner, None, registry)
}

pub(crate) fn prepare_projected_content_sync(
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    projection: Arc<dyn Vfs>,
    options: SyncOptions,
    cancel: &Cancel,
) -> io::Result<PreparedContentSync> {
    prepare_projected_content_sync_owned(source, target, projection, options, cancel, None)
}

pub(crate) fn prepare_projected_content_sync_owned(
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    projection: Arc<dyn Vfs>,
    options: SyncOptions,
    cancel: &Cancel,
    owner: Option<crate::remote_real_mode::InteractiveFileOwner>,
) -> io::Result<PreparedContentSync> {
    let registry = owner
        .as_ref()
        .map(|owner| crate::staging_guard::KnownStagingRegistry::for_owner(&owner.staging))
        .transpose()?;
    prepare_content_sync_contextual(
        source,
        target,
        options,
        cancel,
        owner,
        Some(projection),
        registry,
    )
}

fn prepare_content_sync_contextual(
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    options: SyncOptions,
    cancel: &Cancel,
    owner: Option<crate::remote_real_mode::InteractiveFileOwner>,
    projection: Option<Arc<dyn Vfs>>,
    registry: Option<crate::staging_guard::KnownStagingRegistry>,
) -> io::Result<PreparedContentSync> {
    validate_content_sync_options(&options)?;
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Inhaltssync-Vorbereitung abgebrochen",
        ));
    }
    if registry.is_none() && target.kind() == ProviderKind::Local {
        return Err(crate::coded_error(io::ErrorKind::Unsupported, crate::ErrorCode::Unsupported, "Lokaler Inhaltssync benötigt vor Capability-/Restoreproben den privaten Queuekontext; prepare_content_sync_for_queue verwenden."));
    }
    let queue_execution_bound = owner.is_some();
    let created = Instant::now();
    let source_kind = source.kind();
    let target_kind = target.kind();
    let display_source_root = source.root().to_path_buf();
    let mut source_root = display_source_root.clone();
    let target_root = target.root().to_path_buf();
    if target_root == Path::new("/") && target_kind == ProviderKind::Local {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Die lokale Dateisystemwurzel ist kein Ganzbaum-Syncziel",
        ));
    }
    // Identität und Überlappung werden vor jeglicher Temp-/Recoveryanlage geprüft.
    source.shares_runtime_namespace(&target)?;
    let actual_source_vfs = source.content_sync_context(false, cancel, owner.clone())?;
    let source_identity = actual_source_vfs.root_identity(&display_source_root)?;
    let readonly_target = target.content_sync_context(false, cancel, None)?;
    let target_identity = readonly_target.root_identity(&target_root)?;
    let shared_owner_domain = matches!(&source_identity, RootIdentity::Local(_))
        && matches!(&target_identity, RootIdentity::Local(_));
    crate::remote_real_mode::reject_overlapping_roots(
        &source_identity,
        &display_source_root,
        &target_identity,
        &target_root,
    )?;
    let mut staging = registry
        .map(|registry| -> io::Result<ContentStaging> {
            let endpoints = vec![
                crate::staging_guard::StagingEndpoint {
                    vfs: actual_source_vfs.clone(),
                    root: display_source_root.clone(),
                    binding: source.content_sync_endpoint_binding(&source_identity)?,
                },
                crate::staging_guard::StagingEndpoint {
                    vfs: readonly_target.clone(),
                    root: target_root.clone(),
                    binding: target.content_sync_endpoint_binding(&target_identity)?,
                },
            ];
            let epoch = registry.check(&endpoints, None)?;
            Ok(ContentStaging {
                registry,
                endpoints,
                epoch,
            })
        })
        .transpose()?;
    let preparation_epoch = staging
        .as_ref()
        .map(|staging| {
            staging.registry.check(
                &staging.endpoints,
                owner.as_ref().map(|owner| &owner.staging),
            )
        })
        .transpose()?;
    let real_source = if projection.is_some() {
        Some(RealContentSource {
            vfs: actual_source_vfs.clone(),
            root: display_source_root.clone(),
            identity: source_identity.clone(),
            listing: complete_proof(
                actual_source_vfs.as_ref(),
                &display_source_root,
                source_identity.clone(),
                cancel,
            )?,
        })
    } else {
        None
    };
    let source_vfs = if let Some(projection) = projection {
        source_root = target_root.clone();
        projection
    } else {
        actual_source_vfs
    };
    let mut target_vfs = target.content_sync_context(true, cancel, owner.clone())?;
    if options.metadata == crate::metadata::MetadataProfile::NativeBestEffort {
        let source_metadata = source_vfs.metadata_capabilities(&source_root)?;
        if source_metadata.acl_model == crate::metadata::AclModel::WindowsSid {
            target_vfs.prepare_windows_dacl_write()?;
            target_vfs.prepare_directory_dacl_write()?;
            if matches!(
                options.mode,
                crate::SyncMode::Update | crate::SyncMode::Mirror
            ) {
                target_vfs.prepare_file_dacl_write()?;
            }
        }
        if matches!(
            source_metadata.sparse_extents,
            crate::Capability::Exact | crate::Capability::ReadOnly
        ) {
            target_vfs.prepare_sparse_write(source_metadata.acl_model)?;
        }
    }
    let source_listing_identity = source_vfs.root_identity(&source_root)?;
    let source_listing = complete_proof(
        source_vfs.as_ref(),
        &source_root,
        source_listing_identity,
        cancel,
    )?;
    let target_listing =
        complete_proof(target_vfs.as_ref(), &target_root, target_identity, cancel)?;
    let mut plan = crate::sync::plan_content_with_cancel(
        source_vfs.as_ref(),
        &source_root,
        target_vfs.as_ref(),
        &target_root,
        &options,
        cancel,
        shared_owner_domain,
    )?;
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Inhaltssync-Vorbereitung abgebrochen",
        ));
    }
    annotate_unreadable_metadata(
        &mut plan,
        source_vfs.as_ref(),
        &source_root,
        source_kind,
        &options,
    )?;
    let mut limitations = source_vfs.interactive_write_limitations();
    if target_kind == ProviderKind::Local {
        limitations.extend(crate::real_mode::target_scope_warnings(&target_root)?);
    }

    limitations.extend(target_vfs.interactive_write_limitations());
    if options.metadata != crate::MetadataProfile::Portable {
        for (role, identity) in [
            ("Quelle", &source_identity),
            ("Ziel", &target_listing.root_identity),
        ] {
            if let RootIdentity::Smb(root) = identity {
                if root.server.canonical_root != "/" || root.virtual_root != "/" {
                    limitations.push(format!(
                        "SMB-{role}: Zugriffe auf benannte Datenströme halten die gewählte Wurzel und ihre Unterordner. Darüberliegende Ordner werden frisch geprüft; ihre serverseitige Pfadauflösung bleibt Best Effort."
                    ));
                }
            }
        }
    }
    limitations.push("Best Effort: frischer Bestandscheck, gebundener Transferhash und Inhaltsreadback. Unklare Mutationen werden nicht wiederholt.".into());
    limitations.sort();
    limitations.dedup();
    for row in &plan.comparison.rows {
        if row.action != crate::sync::ComparisonAction::Skip {
            continue;
        }
        let (Some(source), Some(target)) = (&row.source, &row.target) else {
            continue;
        };
        if source.kind == crate::sync::ComparisonEntryKind::File
            && target.kind == crate::sync::ComparisonEntryKind::File
        {
            limitations.push(format!("{}: Versionskonflikt. {} Der Zielinhalt bleibt erhalten; diesen Unterschied vor einem erneuten Update auflösen.",row.path,if source.mtime.is_none() || target.mtime.is_none() {"Mindestens eine überprüfbare Dateizeit fehlt."} else {"Das Ziel hat eine neuere Dateizeit als die abweichende Quelle."}));
        }
    }
    if !target_vfs.supports_mtime_write() {
        limitations.push(match target_kind {
            ProviderKind::WebDav => "WebDAV meldet getlastmodified lesend; diese Verbindung besitzt keinen ausgehandelten, exakt zurücklesbaren Schreibvertrag für die Dateizeit. Dateizeiten werden pfadgenau als ausgelassen bilanziert.",
            ProviderKind::Ftps => "FTP/FTPS meldet Dateizeiten im Listing; diese Sitzung besitzt keinen ausgehandelten, exakt zurücklesbaren Zeit-Schreibvertrag. Dateizeiten werden pfadgenau als ausgelassen bilanziert.",
            ProviderKind::Smb => "SMB liest Dateizeiten, aber der verwendete SMB-Schreibvertrag setzt keine gebundenen FILE_BASIC_INFORMATION-Zeitwerte. Dateizeiten werden pfadgenau als ausgelassen bilanziert.",
            _ => "Das Ziel besitzt keinen exakt rücklesbaren Schreibvertrag für Dateizeiten; die Auslassung wird pfadgenau bilanziert.",
        }.into());
    }
    let mut omitted_times = 0u64;
    let mut transferred_times = 0u64;
    for action in &plan.actions {
        let (path, source_mtime) = match action {
            crate::sync::Action::CopyFile {
                path, source_mtime, ..
            }
            | crate::sync::Action::UpdateFile {
                path, source_mtime, ..
            }
            | crate::sync::Action::CreateDir {
                path, source_mtime, ..
            }
            | crate::sync::Action::SetDirectoryMetadata {
                path, source_mtime, ..
            } => (path, source_mtime),
            _ => continue,
        };
        if source_mtime.is_some() && target_vfs.supports_mtime_write() {
            transferred_times = transferred_times.saturating_add(1);
        } else {
            omitted_times = omitted_times.saturating_add(1);
            plan.metadata_report.issues.push(crate::sync::MetadataIssue {path:path.clone(),class:crate::sync::MetadataClass::Mtime,kind:crate::sync::MetadataIssueKind::Unsupported,reason:if source_mtime.is_none() {"Die Quelle liefert keine überprüfbare Dateizeit; es wird kein Wert erfunden".into()} else {format!("{:?}: Die Zielverbindung besitzt keinen ausgehandelten exakt rücklesbaren Zeit-Schreibvertrag",target_kind)} });
        }
    }
    if omitted_times > 0 || transferred_times > 0 {
        plan.metadata_report
            .classes
            .push(crate::sync::MetadataClassStats {
                class: crate::sync::MetadataClass::Mtime,
                transferred: transferred_times,
                omitted: omitted_times,
            });
        plan.metadata_report.unsupported_metadata = plan
            .metadata_report
            .unsupported_metadata
            .saturating_add(omitted_times);
    }
    let technical_removal_skips = plan
        .actions
        .iter()
        .filter_map(|action| {
            let crate::sync::Action::SkipUnsupported { path } = action else {
                return None;
            };
            let row = plan.comparison.rows.iter().find(|row| row.path == *path)?;
            if row.source.is_some()
                || row.target.as_ref()?.kind != crate::sync::ComparisonEntryKind::Directory
            {
                return None;
            }
            target_vfs
                .best_effort_permanent_delete_blocker(true)
                .map(|reason| format!("{path}: {reason}"))
        })
        .collect::<Vec<_>>();
    limitations.extend(technical_removal_skips.iter().cloned());
    let blocked_removals = if plan.skip_count > technical_removal_skips.len() as u64 {
        plan.delete_count
    } else {
        0
    };
    if blocked_removals > 0 {
        for action in &mut plan.actions {
            let path = match action {
                crate::sync::Action::DeleteFile { path, .. }
                | crate::sync::Action::DeleteDir { path, .. } => Some(path.clone()),
                _ => None,
            };
            if let Some(path) = path {
                *action = crate::sync::Action::SkipConflict { path };
                plan.skip_count = plan.skip_count.saturating_add(1);
            }
        }
        for row in &mut plan.comparison.rows {
            if row.action == crate::sync::ComparisonAction::Quarantine {
                row.action = crate::sync::ComparisonAction::Skip;
                row.affected_side = crate::sync::ComparisonSide::Neither;
            }
        }
        plan.comparison.categories.skipped = plan
            .comparison
            .categories
            .skipped
            .saturating_add(plan.comparison.categories.quarantines);
        plan.comparison.categories.quarantines = 0;
        plan.delete_count = 0;
        plan.delete_bytes = 0;
        limitations.push(format!("{blocked_removals} Mirror-Entfernungen bleiben wegen ausgelassener Einträge oder Konflikte gesperrt; unabhängige Kopien werden ausgeführt."));
    }
    let mut local_execution = if target_kind == ProviderKind::Local {
        Some(crate::real_mode::LocalSyncExecution::read(
            target_vfs.as_ref(),
            &target_root,
        )?)
    } else {
        None
    };
    let mut quarantine = None;
    if let Some(execution) = &mut local_execution {
        if (plan.update_count > 0 || plan.delete_count > 0) && !execution.is_best_effort() {
            let attempt = (|| -> io::Result<(Arc<dyn Vfs>, LocalContentQuarantine)> {
                let context = target.content_sync_local_context(
                    &plan,
                    execution.clone(),
                    true,
                    owner.clone(),
                )?;
                let internal = crate::real_mode::is_home_root(&target_root)?;
                let parent = if internal {
                    target_root.as_path()
                } else {
                    target_root
                        .parent()
                        .ok_or_else(|| io::Error::other("Lokales Ziel ohne Eltern"))?
                };
                let base = parent.join(".doppeldecker-quarantine");
                if source_kind == ProviderKind::Local
                    && (base.starts_with(&display_source_root)
                        || display_source_root.starts_with(&base))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Der Quarantänebereich überlappt die Quelle",
                    ));
                }
                crate::real_mode::ensure_quarantine_root(context.as_ref(), parent, &base)?;
                if internal {
                    crate::real_mode::protect_internal_quarantine(context.as_ref(), &base)?;
                }
                let identity = context.root_identity(&base)?;
                crate::real_mode::ensure_same_mount(&target_listing.root_identity, &identity)?;
                if cancel.is_cancelled() {
                    return Err(crate::coded_error(
                        io::ErrorKind::Interrupted,
                        crate::ErrorCode::Interrupted,
                        "Inhaltssync-Restoreprüfung abgebrochen",
                    ));
                }
                crate::real_mode::run_restore_probe(context.as_ref(), &target_root, &base)?;
                let run = crate::real_mode::reserve_quarantine_run_name(context.as_ref(), &base)?;
                Ok((
                    context,
                    LocalContentQuarantine {
                        base,
                        identity,
                        run,
                    },
                ))
            })();
            match attempt {Ok((context,binding))=>{target_vfs=context;quarantine=Some(binding);},Err(error) if crate::error_code(&error)!=crate::ErrorCode::OutcomeUnclear && matches!(error.kind(),io::ErrorKind::Unsupported|io::ErrorKind::PermissionDenied|io::ErrorKind::CrossesDevices)=>limitations.push(format!("Lokale Quarantäne ist für diese Wirkung technisch nicht verfügbar: {error}. Bestätigte Ersetzungen und Entfernungen erfolgen dauerhaft.")),Err(error)=>return Err(error)}
        }
        if quarantine.is_none() {
            execution.irreversible_replacements = plan.update_count;
            target_vfs = target.content_sync_local_context(
                &plan,
                execution.clone(),
                false,
                owner.clone(),
            )?;
        }
    }
    let mut removal_plans = Vec::new();
    if plan.delete_count > 0 && quarantine.is_none() {
        let deletion = target.content_sync_delete_context(cancel, owner.clone())?;
        let mut grouped = std::collections::BTreeMap::<PathBuf, Vec<PathBuf>>::new();
        for action in &plan.actions {
            let path = match action {
                crate::sync::Action::DeleteFile { path, .. }
                | crate::sync::Action::DeleteDir { path, .. } => path,
                _ => continue,
            };
            let path = crate::path::checked_join(&target_root, path)?;
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("Mirrorziel ohne Eltern"))?
                .to_path_buf();
            grouped.entry(parent).or_default().push(path);
        }
        for (parent, selected) in grouped {
            removal_plans.push(crate::remote_delete::plan_content_sync_delete(
                deletion.as_ref(),
                &parent,
                &selected,
                cancel,
            )?);
        }
    }
    let quarantined_replacements = if quarantine.is_some() {
        plan.update_count
    } else {
        0
    };
    let quarantined_removals = if quarantine.is_some() {
        plan.delete_count
    } else {
        0
    };
    let irreversible_replacements = if quarantine.is_none() {
        plan.update_count
    } else {
        0
    };
    let irreversible_removals = if quarantine.is_none() {
        plan.delete_count
    } else {
        0
    };
    let irreversible_removal_bytes = if quarantine.is_none() {
        plan.delete_bytes
    } else {
        0
    };
    if irreversible_replacements > 0 || irreversible_removals > 0 {
        limitations.push(format!("{irreversible_replacements} vorhandene Dateiversionen werden dauerhaft ersetzt, {irreversible_removals} Zieleinträge dauerhaft entfernt; für diese Wirkungen gibt es keine Quarantäne oder Wiederherstellung."));
    }
    let removal_confirmation = (irreversible_removals > 1000
        || irreversible_removal_bytes > 10 * 1024 * 1024 * 1024)
        .then(|| {
            format!("DAUERHAFT ENTFERNEN {irreversible_removals} {irreversible_removal_bytes}")
        });
    let directory_stream_effects = plan
        .actions
        .iter()
        .flat_map(|action| {
            let (path, metadata) = match action {
                crate::Action::CreateDir {
                    path,
                    metadata: Some(metadata),
                    ..
                }
                | crate::Action::SetDirectoryMetadata { path, metadata, .. } => (path, metadata),
                _ => return Vec::new(),
            };
            metadata
                .directory_streams
                .as_ref()
                .map_or_else(Vec::new, |streams| {
                    streams
                        .effects
                        .iter()
                        .map(|effect| crate::directory_streams::DirectoryStreamOutcome {
                            directory: target_root.join(path).to_string_lossy().into_owned(),
                            effect: effect.clone(),
                        })
                        .collect()
                })
        })
        .collect::<Vec<_>>();
    if !directory_stream_effects.is_empty() {
        limitations.push("Benannte Verzeichnisströme werden als einzeln bestätigte Wirkungen übertragen. Ersetzungen und Entfernungen sind dauerhaft, ohne Quarantäne. Zwischen vollständiger Zielprüfung und einzelnem Rename bleibt ein Best-Effort-Fenster für parallele Änderungen; ein atomarer Versionsvergleich ist nicht verfügbar. Teilwirkungen bleiben nach Fehlern sichtbar.".into());
    }
    let file_dacl_effects = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            crate::Action::SetFileDacl { path, plan, .. } => {
                Some(plan.outcome(&target_root.join(path).to_string_lossy()))
            }
            _ => None,
        })
        .collect::<io::Result<Vec<_>>>()?;
    if !file_dacl_effects.is_empty() {
        limitations.push("Mehrere Dateinamen können auf dasselbe Dateiobjekt verweisen (Hardlinks). Die Rechteänderung gilt für alle bestehenden und neu angelegten Namen dieses Objekts, auch außerhalb der ausgewählten Ordner. Die Liste zeigt nur die hier bekannten Namen; eine beobachtete Anzahl von 1 begrenzt die Wirkung nicht. Die Änderung ist dauerhaft, ohne automatische Wiederherstellung. Inhalte, benannte Datenströme, Eigentümer und Gruppe bleiben erhalten. Ein unklarer Ausgang bleibt bis zur bewussten Rechteprüfung gesperrt.".into());
    }
    let directory_dacl_effects = plan
        .actions
        .iter()
        .filter_map(|action| {
            let crate::Action::SetDirectoryMetadata { path, metadata, .. } = action else {
                return None;
            };
            let Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(dacl)) =
                &metadata.windows_dacl
            else {
                return None;
            };
            Some(dacl.outcome(&target_root.join(path).to_string_lossy()))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if !directory_dacl_effects.is_empty() {
        limitations.push("DACLs vorhandener Verzeichnisse werden nach allen Inhalten und Metadaten als dauerhafte Einzelwirkungen gesetzt. Ein unklarer SET-Ausgang bleibt mit dem tatsächlichen Verzeichnispfad zur bewussten Prüfung erhalten.".into());
    }
    let created_directory_dacl_effects = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            crate::Action::CreateDir {
                path,
                metadata: Some(m),
                ..
            } => match &m.windows_dacl {
                Some(crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryExact(
                    selection,
                )) => Some(
                    crate::created_directory_dacl::CreatedDirectoryDaclPreviewEffect {
                        directory: target_root.join(path).to_string_lossy().into_owned(),
                        selection: (**selection).clone(),
                    },
                ),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    if !created_directory_dacl_effects.is_empty() {
        limitations.push("Nur die hier benannten, tatsächlich neu angelegten leeren Ordner können die bestätigten Quellrechte erhalten. Anfangsrechte und Objektidentität werden erst bei der Anlage beobachtet. Die Rechteänderung ist dauerhaft, ohne automatische Wiederherstellung. Fehlen zusätzliche Zugriffe oder passende Beobachtungen, bleiben Anlage und übrige Inhalte verfügbar; die Rechteänderung wird konkret ausgelassen.".into());
    }
    let created_directory_before_children_effects = plan
        .actions
        .iter()
        .filter_map(|action| {
            let crate::Action::CreateDir {
                path,
                metadata: Some(metadata),
                ..
            } = action
            else {
                return None;
            };
            let Some(
                crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(
                    selection,
                ),
            ) = &metadata.windows_dacl
            else {
                return None;
            };
            Some(
                crate::created_directory_dacl::CreatedDirectoryBeforeChildrenPreviewEffect {
                    directory: target_root.join(path).to_string_lossy().into_owned(),
                    selection: (**selection).clone(),
                },
            )
        })
        .collect::<Vec<_>>();
    if !created_directory_before_children_effects.is_empty() {
        limitations.push("Die benannten neuen nichtleeren Ordner erhalten die bestätigten Rechte dauerhaft VOR den Kindern. Kinder und gewöhnliche Metadaten bleiben danach noch auszuführen; ein Fehler setzt die bereits bestätigten Rechte nicht zurück.".into());
    }
    let mut preview = ContentSyncPreview {
        execution_class: crate::sync::SyncExecutionClass::BestEffort,
        source_root: display_source_root.to_string_lossy().into_owned(),
        target_root: target_root.to_string_lossy().into_owned(),
        source_provider: source_kind,
        target_provider: target_kind,
        source_identity: source_identity.clone(),
        target_identity: target_listing.root_identity.clone(),
        options,
        plan,
        digest: [0; 32],
        expires_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
            + REAL_PERMIT_LIFETIME.as_millis() as u64,
        limitations,
        quarantined_replacements,
        quarantined_removals,
        quarantine_run: quarantine
            .as_ref()
            .map(|binding| binding.run.to_string_lossy().into_owned()),
        restore_probe_ok: quarantine.is_some(),
        local_execution,
        irreversible_replacements,
        irreversible_removals,
        irreversible_removal_bytes,
        blocked_removals,
        removal_confirmation,
        directory_stream_effects,
        directory_dacl_effects,
        file_dacl_effects,
        created_directory_dacl_effects,
        created_directory_before_children_effects,
    };
    preview.digest = binding_digest(
        &preview,
        &source_listing,
        &target_listing,
        &removal_plans,
        quarantine.as_ref(),
        real_source.as_ref(),
    )?;
    if let Some(staging) = &mut staging {
        staging.endpoints[1].vfs = target_vfs.clone();
        staging.adopt_after_probe(
            owner.as_ref().map(|owner| &owner.staging),
            preparation_epoch,
            "Fremde Zwischenstände änderten sich während der Inhaltssync-Vorbereitung",
        )?;
    }
    let prepared = PreparedContentSync {
        source_endpoint: source,
        target_endpoint: target,
        source: source_vfs,
        source_root,
        real_source,
        target: target_vfs,
        preview,
        quarantine,
        removal_plans,
        owner,
        staging,
        queue_execution_bound,
        source_listing,
        target_listing,
        created,
        cancel: cancel.clone(),
    };
    prepared.validate()?;
    Ok(prepared)
}

fn annotate_unreadable_metadata(
    plan: &mut SyncPlan,
    source: &dyn Vfs,
    root: &Path,
    kind: ProviderKind,
    options: &SyncOptions,
) -> io::Result<()> {
    if options.metadata == crate::MetadataProfile::Portable {
        return Ok(());
    }
    let capabilities = source.metadata_capabilities(root)?;
    annotate_unreadable_metadata_with_capabilities(plan, &capabilities, kind, options)
}

fn annotate_unreadable_metadata_with_capabilities(
    plan: &mut SyncPlan,
    capabilities: &crate::metadata::MetadataCapabilities,
    kind: ProviderKind,
    options: &SyncOptions,
) -> io::Result<()> {
    if options.metadata == crate::MetadataProfile::Portable {
        return Ok(());
    }
    use crate::metadata::{AclModel, Capability};
    use crate::sync::{MetadataClass as C, MetadataIssueKind as K};
    let readable = |value| matches!(value, Capability::Exact | Capability::ReadOnly);
    let classes = [
        (C::Owner, readable(capabilities.owner)),
        (
            C::Acl,
            matches!(capabilities.acl_model, AclModel::Posix | AclModel::Nfs4),
        ),
        (C::NormalMode, readable(capabilities.special_mode_bits)),
        (C::SpecialModeBits, readable(capabilities.special_mode_bits)),
        (C::Xattrs, readable(capabilities.xattrs)),
        (C::ResourceFork, readable(capabilities.resource_fork)),
        (C::NamedStreams, readable(capabilities.named_streams)),
        (C::SparseExtents, readable(capabilities.sparse_extents)),
        (C::Hardlinks, readable(capabilities.hardlinks)),
    ];
    let observed_modes = plan
        .comparison
        .rows
        .iter()
        .map(|row| {
            (
                row.path.as_str(),
                row.source
                    .as_ref()
                    .is_some_and(|entry| entry.mode.is_some()),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    for action in &plan.actions {
        let (path, metadata, directory) = match action {
            crate::sync::Action::CreateDir { path, metadata, .. } => {
                (path, metadata.as_ref(), true)
            }
            crate::sync::Action::CopyFile { path, metadata, .. }
            | crate::sync::Action::UpdateFile { path, metadata, .. } => {
                (path, metadata.as_ref(), false)
            }
            crate::sync::Action::SetDirectoryMetadata { path, metadata, .. } => {
                (path, Some(metadata), true)
            }
            crate::sync::Action::CreateHardlink { path, metadata, .. } => {
                (path, Some(metadata), false)
            }
            _ => continue,
        };
        let windows_source = metadata.map(|value| &value.source).filter(|source| {
            source.windows_security.is_some()
                || matches!(source.acl, Some(crate::metadata::Acl::WindowsDacl(_)))
        });
        if let Some(source) = windows_source {
            // Nur der gültige Objektbeleg bestätigt die ACL-Lesesicht. Optionale
            // Owner-/Group-SIDs und die Schreibfähigkeit bleiben davon getrennt.
            source.validate()?;
        }
        for (class, known) in classes {
            // Die konkrete Linkzahl bilanziert der Planer auch ohne vollständige
            // Gruppenidentität. Die grobe Rootfähigkeit darf weder die Zahl noch
            // eine bekannte Einzeldatei wieder in eine pauschale Auslassung ändern.
            if class == C::SparseExtents
                && metadata.is_some_and(|value| value.source.sparse_read_issue.is_some())
            {
                continue;
            }
            if class == C::Hardlinks
                && metadata.is_some_and(|value| value.source.reported_hard_links.is_some())
            {
                continue;
            }
            // Der Planer bilanziert konkrete Windows-Abfragefehler bereits je Objekt.
            // Eine grobe Rootfähigkeit darf deren Grund und Fehlerklasse nicht ersetzen.
            if matches!(class, C::Owner | C::Acl)
                && windows_source
                    .and_then(|source| source.windows_security.as_ref())
                    .is_some_and(|observation| observation.issue.is_some())
                && plan
                    .metadata_report
                    .issues
                    .iter()
                    .any(|issue| issue.path == *path && issue.class == class)
            {
                continue;
            }
            // DAV beobachtet diese Klassen je Objekt. Deren präzise Statusbilanz
            // darf nicht durch die gröbere Rootfähigkeit überschrieben werden.
            if matches!(class, C::Owner | C::Acl)
                && metadata.is_some_and(|value| value.source.webdav_security.is_some())
            {
                continue;
            }
            let known = if class == C::Acl
                && windows_source.is_some_and(|source| {
                    matches!(source.acl, Some(crate::metadata::Acl::WindowsDacl(_)))
                }) {
                true
            } else if class == C::Owner {
                known && metadata.is_some_and(|value| value.source.owner.is_some())
            } else if class == C::NamedStreams {
                // Lokale Adapter kennen diese zusätzliche Klasse explizit nicht.
                // SMB-Verzeichnisströme bleiben dagegen eine benannte Schnittgrenze.
                kind == ProviderKind::Local
                    || metadata.is_some_and(|value| value.source.named_streams.is_some())
            } else if class == C::NormalMode
                && options.metadata != crate::MetadataProfile::NativeStrict
            {
                observed_modes.get(path.as_str()).copied().unwrap_or(false)
            } else {
                known
            };
            if known {
                continue;
            }
            let reason = if class == C::NamedStreams
                && kind == ProviderKind::Smb
                && capabilities.named_streams == Capability::Unsupported
            {
                "SMB: Der Server meldet für diesen Root keine Unterstützung benannter Datenströme (FILE_NAMED_STREAMS fehlt)".into()
            } else if class == C::NamedStreams && directory && kind == ProviderKind::Smb {
                "SMB: Der Server unterstützt die vollständige Streamabfrage dieser Verzeichnisbasis nicht; fehlende Beobachtung ist kein Beleg für Abwesenheit".into()
            } else {
                format!("{kind:?}: Die Quelle stellt für {class:?} keine exakte Lesesicht bereit; fehlende Beobachtung ist kein Beleg für Abwesenheit")
            };
            if let Some(issue) = plan
                .metadata_report
                .issues
                .iter_mut()
                .find(|issue| issue.path == *path && issue.class == class)
            {
                if class != C::NamedStreams
                    || (directory
                        && metadata.is_some_and(|value| value.source.named_streams.is_none()))
                {
                    issue.reason = reason;
                }
                continue;
            }
            plan.metadata_report
                .issues
                .push(crate::sync::MetadataIssue {
                    path: path.clone(),
                    class,
                    kind: K::Unsupported,
                    reason,
                });
            if let Some(count) = plan
                .metadata_report
                .classes
                .iter_mut()
                .find(|count| count.class == class)
            {
                count.omitted = count.omitted.saturating_add(1);
            } else {
                plan.metadata_report
                    .classes
                    .push(crate::sync::MetadataClassStats {
                        class,
                        transferred: 0,
                        omitted: 1,
                    });
            }
            plan.metadata_report.unsupported_metadata =
                plan.metadata_report.unsupported_metadata.saturating_add(1);
        }
    }
    Ok(())
}

fn complete_proof(
    vfs: &dyn Vfs,
    root: &Path,
    identity: RootIdentity,
    cancel: &Cancel,
) -> io::Result<ListingProof> {
    let proof =
        crate::real_mode::build_listing_proof_with_cancel(vfs, root, identity, Some(cancel));
    if cancel.is_cancelled() {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Inhaltssync-Listing abgebrochen",
        ));
    }
    if !proof.complete {
        return Err(plan_drift(format!(
            "Inhaltssync-Listing unvollständig: {}",
            proof
                .errors
                .iter()
                .map(|error| error.reason.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    Ok(proof)
}
fn same_target_identity(current: &RootIdentity, expected: &RootIdentity) -> bool {
    match (current, expected) {
        // Die private Uploadsicht prüft und erneuert Collectionversionen nach
        // eigenen Wirkungen. Ein Kindcommit darf die Root-ETag regulär ändern.
        (RootIdentity::WebDav(current), RootIdentity::WebDav(expected)) => {
            current.server == expected.server
                && current.virtual_root == expected.virtual_root
                && current.session_generation == expected.session_generation
        }
        _ => current == expected,
    }
}
fn same_listing(left: &ListingProof, right: &ListingProof) -> bool {
    left.root_identity == right.root_identity
        && left.complete
        && right.complete
        && left.listing_digest == right.listing_digest
        && left.provider_token == right.provider_token
}
fn binding_digest(
    preview: &ContentSyncPreview,
    source: &ListingProof,
    target: &ListingProof,
    removals: &[crate::RemoteDeletePlan],
    quarantine: Option<&LocalContentQuarantine>,
    real_source: Option<&RealContentSource>,
) -> io::Result<[u8; 32]> {
    let bytes = serde_json::to_vec(&(
        2u32,
        (
            &preview.source_root,
            &preview.target_root,
            preview.execution_class,
            &preview.options,
            &preview.plan,
        ),
        (
            &preview.limitations,
            preview.irreversible_replacements,
            preview.irreversible_removals,
            preview.irreversible_removal_bytes,
            preview.quarantined_replacements,
            preview.quarantined_removals,
            &preview.local_execution,
            preview.restore_probe_ok,
            preview.blocked_removals,
            &preview.removal_confirmation,
            &preview.directory_stream_effects,
        ),
        (
            source,
            target,
            removals,
            quarantine,
            real_source.map(|real| (&real.root, &real.identity, &real.listing)),
        ),
    ))
    .map_err(io::Error::other)?;
    let digest = if preview.directory_dacl_effects.is_empty() {
        *blake3::hash(&bytes).as_bytes()
    } else {
        let bytes = serde_json::to_vec(&(bytes, &preview.directory_dacl_effects))
            .map_err(io::Error::other)?;
        *blake3::hash(&bytes).as_bytes()
    };
    let digest = if preview.file_dacl_effects.is_empty() {
        digest
    } else {
        let mut hash = blake3::Hasher::new();
        hash.update(b"doppeldecker-content-file-dacl-v6\0");
        hash.update(&digest);
        hash.update(&serde_json::to_vec(&preview.file_dacl_effects).map_err(io::Error::other)?);
        *hash.finalize().as_bytes()
    };
    let digest = if preview.created_directory_dacl_effects.is_empty() {
        digest
    } else {
        let mut hash = blake3::Hasher::new();
        hash.update(b"doppeldecker-content-created-directory-v7\0");
        hash.update(&digest);
        hash.update(
            &serde_json::to_vec(&preview.created_directory_dacl_effects)
                .map_err(io::Error::other)?,
        );
        *hash.finalize().as_bytes()
    };
    if preview.created_directory_before_children_effects.is_empty() {
        Ok(digest)
    } else {
        let mut hash = blake3::Hasher::new();
        hash.update(b"doppeldecker-content-created-directory-before-children-v8\0");
        hash.update(&digest);
        hash.update(
            &serde_json::to_vec(&preview.created_directory_before_children_effects)
                .map_err(io::Error::other)?,
        );
        Ok(*hash.finalize().as_bytes())
    }
}

impl ContentSyncPermit {
    pub(crate) fn metadata_profile(&self) -> crate::MetadataProfile {
        self.metadata_profile
    }
}

// V5 benötigt weiterhin exakt denselben vorbereiteten Kontext. V7 darf bei
// fehlender neuer Probe nur seine optionale Rechtewirkung auslassen; begin()
// vergleicht deren bestätigten Kontext erneut am noch gehaltenen CREATE-Objekt.
fn validate_directory_dacl_queue_reprobe(
    expected_existing: &[&crate::windows_dacl::WindowsDaclWriteContext],
    observed: crate::windows_dacl::WindowsDaclWriteSupport,
) -> io::Result<()> {
    if expected_existing.is_empty() {
        return Ok(());
    }
    match observed {
        crate::windows_dacl::WindowsDaclWriteSupport::Ready(current)
            if expected_existing
                .iter()
                .all(|expected| **expected == current) =>
        {
            Ok(())
        }
        _ => Err(plan_drift(
            "Der bestätigte Verzeichnis-DACL-Zielkontext änderte sich bei der Queuezuordnung",
        )),
    }
}

#[cfg(test)]
#[test]
fn created_directory_queue_reprobe_unavailable_is_optional_but_v5_context_stays_bound() {
    use crate::windows_dacl::{
        WindowsDaclTargetIssue as Issue, WindowsDaclWriteContext,
        WindowsDaclWriteSupport as Support,
    };
    let context = WindowsDaclWriteContext {
        namespace: [1; 16],
        owner: crate::windows_security::WindowsSid {
            revision: 1,
            authority: 5,
            sub_authorities: vec![21, 1],
        },
        group: None,
    };
    for observed in [
        Support::Unavailable(Issue::RequiredAccessDenied),
        Support::Unavailable(Issue::NotPrepared),
        Support::Ready(context.clone()),
    ] {
        validate_directory_dacl_queue_reprobe(&[], observed.clone()).unwrap();
        let existing = validate_directory_dacl_queue_reprobe(&[&context], observed.clone());
        if observed == Support::Ready(context.clone()) {
            existing.unwrap();
        } else {
            assert_eq!(
                crate::error_code(&existing.unwrap_err()),
                crate::ErrorCode::PlanDrift
            );
        }
    }
    let mut changed = context.clone();
    changed.namespace = [2; 16];
    validate_directory_dacl_queue_reprobe(&[], Support::Ready(changed.clone())).unwrap();
    assert_eq!(
        crate::error_code(
            &validate_directory_dacl_queue_reprobe(&[&context], Support::Ready(changed))
                .unwrap_err()
        ),
        crate::ErrorCode::PlanDrift
    );
}

impl PreparedContentSync {
    pub fn target_endpoint(&self) -> &ProviderEndpoint {
        &self.target_endpoint
    }
    pub fn preview(&self) -> &ContentSyncPreview {
        &self.preview
    }
    pub(crate) fn matches_queue_endpoints(
        &self,
        source: &crate::BidirectionalQueueEndpoint,
        target: &crate::BidirectionalQueueEndpoint,
    ) -> bool {
        self.source_endpoint.matches_queue_endpoint(source)
            && self.target_endpoint.matches_queue_endpoint(target)
    }

    pub fn queue_operation(&self) -> crate::QueueOperation {
        crate::QueueOperation::ContentSync {
            source: Box::new(self.source_endpoint.queue_reference()),
            target: Box::new(self.target_endpoint.queue_reference()),
            options: self.preview.options.clone(),
            expected_plan: Box::new(self.preview.plan.clone()),
            binding_digest: self.preview.digest,
        }
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
        if !self.source.anchor_is_current()? || !self.target.anchor_is_current()? {
            return Err(plan_drift(
                "Gehaltene Wurzel oder Elternbindung änderte sich vor Queuezuordnung",
            ));
        }
        let registry = crate::staging_guard::KnownStagingRegistry::for_queue(queue)?;
        let actual_source = self
            .real_source
            .as_ref()
            .map(|real| real.vfs.clone())
            .unwrap_or_else(|| self.source.clone());
        let actual_source_root = self.source_endpoint.root().to_path_buf();
        let endpoints = vec![
            crate::staging_guard::StagingEndpoint {
                vfs: actual_source.clone(),
                root: actual_source_root.clone(),
                binding: self.source_endpoint.content_sync_endpoint_binding(
                    &actual_source.root_identity(&actual_source_root)?,
                )?,
            },
            crate::staging_guard::StagingEndpoint {
                vfs: self.target.clone(),
                root: self.target_endpoint.root().to_path_buf(),
                binding: self.target_endpoint.content_sync_endpoint_binding(
                    &self.target.root_identity(self.target_endpoint.root())?,
                )?,
            },
        ];
        if let Some(staging) = &self.staging {
            staging.validate(None)?;
        }
        let epoch = registry.check(&endpoints, None)?;
        self.staging = Some(ContentStaging {
            registry,
            endpoints,
            epoch,
        });
        let source = self.source_endpoint.with_queue_owner_with_cancel(
            queue,
            Some(operation),
            &[],
            Some(cancel),
        )?;
        let target = self.target_endpoint.with_queue_owner_with_cancel(
            queue,
            Some(operation),
            &[],
            Some(cancel),
        )?;
        let (staging, _, _) = queue.ftps_owner(Some(operation))?;
        let owner = crate::remote_real_mode::InteractiveFileOwner {
            staging,
            profiles: profiles.to_vec(),
        };
        let actual_source = source.content_sync_context(false, cancel, Some(owner.clone()))?;
        if let Some(real) = &mut self.real_source {
            real.vfs = actual_source;
        } else {
            self.source = actual_source;
        }
        self.target = if let Some(execution) = &self.preview.local_execution {
            target.content_sync_local_context(
                &self.preview.plan,
                execution.clone(),
                self.quarantine.is_some(),
                Some(owner.clone()),
            )?
        } else {
            target.content_sync_context(true, cancel, Some(owner.clone()))?
        };
        if let Some(staging) = &mut self.staging {
            staging.endpoints[0].vfs = self
                .real_source
                .as_ref()
                .map(|real| real.vfs.clone())
                .unwrap_or_else(|| self.source.clone());
            staging.endpoints[1].vfs = self.target.clone();
        }
        let expected_dacl_contexts: Vec<_> = self
            .preview
            .plan
            .actions
            .iter()
            .filter_map(|action| {
                let metadata = match action {
                    crate::Action::CopyFile { metadata, .. }
                    | crate::Action::UpdateFile { metadata, .. } => metadata.as_ref()?,
                    _ => return None,
                };
                match &metadata.windows_dacl {
                    Some(crate::windows_dacl::WindowsDaclTransfer::Exact(context)) => Some(context),
                    _ => None,
                }
            })
            .collect();
        if !expected_dacl_contexts.is_empty() {
            // Der Queueowner erzeugt eine neue private Providersicht. Ihre eigene
            // Probe muss denselben bestätigten Kontext belegen; ein serialisierter
            // Plan überträgt weder die alte Backendfreigabe noch einen neuen Owner.
            let foreign_epoch = self
                .staging
                .as_ref()
                .map(|staging| {
                    staging
                        .registry
                        .check(&staging.endpoints, Some(&owner.staging))
                })
                .transpose()?;
            self.target.prepare_windows_dacl_write()?;
            match self.target.windows_dacl_write_support()? {
                crate::windows_dacl::WindowsDaclWriteSupport::Ready(current)
                    if expected_dacl_contexts
                        .iter()
                        .all(|expected| **expected == current) => {}
                _ => return Err(plan_drift(
                    "Der bestätigte Windows-DACL-Zielkontext änderte sich bei der Queuezuordnung",
                )),
            }
            if let Some(staging) = &mut self.staging {
                staging.adopt_after_probe(
                    Some(&owner.staging),
                    foreign_epoch,
                    "Fremde Zwischenstände änderten sich während der DACL-Probe bei Queuezuordnung",
                )?;
            }
        }
        if !self.preview.file_dacl_effects.is_empty() {
            self.target.prepare_file_dacl_write()?;
            match self.target.file_dacl_write_support()? {
                crate::windows_dacl::WindowsDaclWriteSupport::Ready(context)
                    if self.preview.plan.actions.iter().all(|action| match action {
                        crate::Action::SetFileDacl { plan, .. } => {
                            plan.context.namespace == context.namespace
                        }
                        _ => true,
                    }) => {}
                _ => {
                    return Err(plan_drift(
                        "Der Datei-DACL-Zielnamensraum änderte sich bei Queuezuordnung",
                    ))
                }
            }
        }
        let expected_directory_contexts: Vec<_> = self
            .preview
            .plan
            .actions
            .iter()
            .filter_map(|action| {
                let crate::Action::SetDirectoryMetadata { metadata, .. } = action else {
                    return None;
                };
                match &metadata.windows_dacl {
                    Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(plan)) => {
                        Some(&plan.context)
                    }
                    _ => None,
                }
            })
            .collect();
        if !expected_directory_contexts.is_empty()
            || !self.preview.created_directory_dacl_effects.is_empty()
            || !self
                .preview
                .created_directory_before_children_effects
                .is_empty()
        {
            let foreign_epoch = self
                .staging
                .as_ref()
                .map(|staging| {
                    staging
                        .registry
                        .check(&staging.endpoints, Some(&owner.staging))
                })
                .transpose()?;
            self.target.prepare_directory_dacl_write()?;
            validate_directory_dacl_queue_reprobe(
                &expected_directory_contexts,
                self.target.directory_dacl_write_support()?,
            )?;
            if let Some(staging) = &mut self.staging {
                staging.adopt_after_probe(Some(&owner.staging), foreign_epoch,
                    "Fremde Zwischenstände änderten sich während der Verzeichnis-DACL-Probe bei Queuezuordnung")?;
            }
        }
        if self.preview.plan.actions.iter().any(|action| {
            matches!(action,
                crate::Action::CopyFile { metadata: Some(metadata), .. }
                | crate::Action::UpdateFile { metadata: Some(metadata), .. }
                if metadata.sparse_extents)
        }) {
            let foreign_epoch = self
                .staging
                .as_ref()
                .map(|staging| {
                    staging
                        .registry
                        .check(&staging.endpoints, Some(&owner.staging))
                })
                .transpose()?;
            let source_acl = self
                .source
                .metadata_capabilities(&self.source_root)?
                .acl_model;
            if source_acl == crate::metadata::AclModel::WindowsSid {
                self.target.prepare_windows_dacl_write()?;
            }
            self.target.prepare_sparse_write(source_acl)?;
            if self
                .target
                .metadata_capabilities(self.target_endpoint.root())?
                .sparse_extents
                != crate::Capability::Exact
            {
                return Err(plan_drift("Die bestätigte Sparse-Zielschreibfähigkeit änderte sich bei der Queuezuordnung"));
            }
            if let Some(staging) = &mut self.staging {
                staging.adopt_after_probe(Some(&owner.staging), foreign_epoch,
                    "Fremde Zwischenstände änderten sich während der Sparse-Probe bei Queuezuordnung")?;
            }
        }
        self.source_endpoint = source;
        self.target_endpoint = target;
        self.cancel = cancel.clone();
        self.owner = Some(owner);
        self.queue_execution_bound = true;
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> io::Result<()> {
        if let Some(staging) = &self.staging {
            staging.validate(None)?;
        }
        if self.created.elapsed() > REAL_PERMIT_LIFETIME {
            return Err(plan_drift(
                "Inhaltssync-Vorbereitung ist abgelaufen; neu prüfen",
            ));
        }
        let source_root = self.source_root.as_path();
        if let Some(real) = &self.real_source {
            real.validate(&self.cancel, true)?;
        }
        let target_root = Path::new(&self.preview.target_root);
        if !self.source.anchor_is_current()? || !self.target.anchor_is_current()? {
            return Err(plan_drift(
                "Inhaltssync-Wurzel oder gehaltener Elternordner wurde ausgetauscht",
            ));
        }
        if let Some(binding) = &self.quarantine {
            if self.target.root_identity(&binding.base)? != binding.identity {
                return Err(plan_drift(
                    "Lokale Inhaltssync-Quarantäne wurde ausgetauscht",
                ));
            }
        }
        if let Some(expected) = &self.preview.local_execution {
            let mut current =
                crate::real_mode::LocalSyncExecution::read(self.target.as_ref(), target_root)?;
            current.irreversible_replacements = expected.irreversible_replacements;
            if &current != expected {
                return Err(plan_drift(
                    "Lokale Inhaltssync-Zielfähigkeiten änderten sich",
                ));
            }
            let required =
                crate::real_mode::impact_for_plan(&self.preview.plan).required_available_bytes;
            if self.target.available_bytes(target_root)? < required {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "Zu wenig freier Platz für den bestätigten Inhaltssync",
                ));
            }
        }

        let source = complete_proof(
            self.source.as_ref(),
            source_root,
            self.source_listing.root_identity.clone(),
            &self.cancel,
        )?;
        let target = complete_proof(
            self.target.as_ref(),
            target_root,
            self.target_listing.root_identity.clone(),
            &self.cancel,
        )?;
        if !same_listing(&source, &self.source_listing)
            || !same_listing(&target, &self.target_listing)
        {
            return Err(plan_drift(
                "Inhaltssync-Bestand änderte sich seit der Vorschau",
            ));
        }
        if let Some(staging) = &self.staging {
            staging.validate(None)?;
        }
        crate::sync::verify_metadata_observations(
            self.source.as_ref(),
            source_root,
            self.target.as_ref(),
            target_root,
            &self.preview.plan,
        )?;
        // Inhaltsänderungen bleiben an den einzelnen Wirkungen gebunden: Der
        // gemeinsame Transfer vergleicht den tatsächlichen Stream mit dem
        // bestätigten Quellhash und prüft den Altzielhash unmittelbar vor Commit.
        // Ein erneuter Vollhash bei jedem Weiterreichen würde dieselben Remote-
        // Inhalte mehrfach lesen, ohne eine spätere Änderung auszuschließen.
        Ok(())
    }
    pub fn confirm_effects(
        self,
        digest: [u8; 32],
        effects_confirmed: bool,
    ) -> io::Result<ContentSyncRun> {
        self.confirm_effects_with_removal_phrase(digest, effects_confirmed, None)
    }
    pub fn confirm_effects_with_removal_phrase(
        self,
        digest: [u8; 32],
        effects_confirmed: bool,
        removal_phrase: Option<&str>,
    ) -> io::Result<ContentSyncRun> {
        if !self.queue_execution_bound
            && self.preview.plan.actions.iter().any(|action| {
                !matches!(
                    action,
                    crate::sync::Action::SkipConflict { .. }
                        | crate::sync::Action::SkipSymlink { .. }
                        | crate::sync::Action::SkipUnsupported { .. }
                )
            })
        {
            return Err(crate::coded_error(io::ErrorKind::Unsupported, crate::ErrorCode::Unsupported, "Schreibender Inhaltssync benötigt den dauerhaft gebundenen Queueowner; über die Fensterqueue bestätigen."));
        }
        if self
            .preview
            .removal_confirmation
            .as_deref()
            .is_some_and(|expected| Some(expected) != removal_phrase)
        {
            return Err(confirmation_mismatch(
                "Große dauerhafte Mirror-Entfernung benötigt die exakte angezeigte Phrase",
            ));
        }
        if digest != self.preview.digest || !effects_confirmed {
            return Err(confirmation_mismatch(
                "Die genaue Inhaltssync-Wirkung wurde nicht bestätigt",
            ));
        }
        self.validate()?;
        Ok(ContentSyncRun { prepared: self })
    }
}
impl ContentSyncRun {
    pub fn execute(
        mut self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<SyncStats> {
        if cancel.is_cancelled() {
            return Ok(SyncStats {
                execution_class: Some(crate::sync::SyncExecutionClass::BestEffort),
                cancelled: true,
                metadata_report: self.prepared.preview.plan.metadata_report.clone(),
                local_execution: self.prepared.preview.local_execution.clone(),
                ..SyncStats::default()
            });
        }
        self.prepared.cancel = cancel.clone();
        self.prepared.validate()?;
        let prepared = self.prepared;
        let mut runtime_staging = prepared.staging.clone();
        if let Some(staging) = &mut runtime_staging {
            staging.epoch = staging.registry.check(
                &staging.endpoints,
                prepared.owner.as_ref().map(|owner| &owner.staging),
            )?;
        }
        let completion_staging = runtime_staging.clone();
        let completion_owner = prepared.owner.clone();
        let completion_endpoint = prepared.target_endpoint.clone();
        let permit = ContentSyncPermit {
            source_root: prepared.source_root.clone(),
            target_root: PathBuf::from(&prepared.preview.target_root),
            source_identity: prepared.source_listing.root_identity.clone(),
            target_identity: prepared.target_listing.root_identity,
            plan: prepared.preview.plan.clone(),
            created: prepared.created,
            consumed: AtomicBool::new(false),
            source_listing: prepared.source_listing.clone(),
            target_endpoint: prepared.target_endpoint,
            quarantine: prepared.quarantine,
            local_execution: prepared.preview.local_execution.clone(),
            irreversible_replacements: prepared.preview.irreversible_replacements > 0,
            metadata_profile: prepared.preview.options.metadata,
            real_source: prepared.real_source,
            removal_plans: prepared.removal_plans,
            owner: prepared.owner,
            staging: runtime_staging,
        };
        let safety = Safety::content_sync(permit);
        let result = crate::sync::execute(
            crate::sync::ExecuteRequest {
                src: prepared.source.as_ref(),
                src_root: prepared.source_root.as_path(),
                dst: prepared.target.as_ref(),
                dst_root: Path::new(&prepared.preview.target_root),
                plan: &prepared.preview.plan,
                safety: &safety,
            },
            cancel,
            on_progress,
        );
        // FTPS hält die eigenen Intents bis zum bestätigten Gesamtergebnis.
        // Erst danach darf die vollständige Prüfung alle Owner einschließlich
        // dieses abgeschlossenen Laufs wieder gleich behandeln.
        let stats = completion_endpoint.finish_content_sync_result(result)?;
        if stats.cancelled {
            return Ok(stats);
        }
        if let Some(mut staging) = completion_staging {
            let owner = completion_owner.as_ref().map(|owner| &owner.staging);
            let checked = if !stats.directory_dacl_report.completed.is_empty()
                || !stats.file_dacl_report.completed.is_empty()
                || !stats.created_directory_dacl_report.completed.is_empty()
            {
                staging.complete_directory_dacl(owner)
            } else {
                staging
                    .validate(owner)
                    .and_then(|_| staging.registry.check(&staging.endpoints, None).map(|_| ()))
            };
            checked.map_err(|error| {
                crate::error::with_directory_dacl_report(
                    crate::error::with_directory_stream_report(
                        crate::error::with_file_dacl_report(
                            crate::error::with_created_directory_dacl_report(
                                crate::error::with_recovery_paths(
                                    error,
                                    &stats.temp_recovery_paths,
                                ),
                                &stats.created_directory_dacl_report,
                            ),
                            &stats.file_dacl_report,
                        ),
                        &stats.directory_stream_report,
                    ),
                    &stats.directory_dacl_report,
                )
            })?;
        }
        Ok(stats)
    }
}

#[cfg(test)]
#[path = "content_sync_tests.rs"]
mod tests;

pub(crate) fn local_delete_context(root: &Path) -> io::Result<Arc<dyn Vfs>> {
    Ok(Arc::new(LocalContentDelete {
        inner: crate::LocalVfs::rooted(root)?,
    }))
}
struct LocalContentDelete {
    inner: crate::LocalVfs,
}
impl Vfs for LocalContentDelete {
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht legt nichts an",
        ))
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht schreibt keine Inhalte",
        ))
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht schreibt keine Inhalte",
        ))
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht verlangt bestätigten Einzelrecord",
        ))
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht entfernt keine ungeplanten Bäume",
        ))
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht benennt nichts um",
        ))
    }

    fn root_identity(&self, path: &Path) -> io::Result<RootIdentity> {
        self.inner.root_identity(path)
    }
    fn list(&self, path: &Path) -> io::Result<Vec<crate::Entry>> {
        self.inner.list(path)
    }
    fn list_complete(&self, path: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
        self.inner.list_complete(path)
    }
    fn stat(&self, path: &Path) -> io::Result<crate::Entry> {
        self.inner.stat(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        self.inner.open_read(path)
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht legt nichts an",
        ))
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht schreibt keine Inhalte",
        ))
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht verändert keine Rechte",
        ))
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht verändert keine Zeiten",
        ))
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Löschsicht benennt nichts um",
        ))
    }
    fn supports_best_effort_permanent_delete(&self) -> bool {
        true
    }
    fn remove_permanent_best_effort_if_matches(
        &self,
        path: &Path,
        expected: &crate::vfs::ProofListEntry,
    ) -> io::Result<()> {
        let current = self.inner.stat(path)?;
        let old = &expected.entry;
        if current.device != old.device
            || current.inode != old.inode
            || current.is_dir != old.is_dir
            || current.is_file != old.is_file
            || current.is_symlink
            || current.size != old.size
            || current.mtime != old.mtime
            || current.mode != old.mode
        {
            return Err(plan_drift(
                "Lokales Mirror-Löschobjekt änderte sich vor der Einzelwirkung",
            ));
        }
        if current.is_dir {
            self.inner.remove_dir(path)?;
        } else {
            self.inner.remove_file(path)?;
        }
        self.inner.sync_dir(
            path.parent()
                .ok_or_else(|| io::Error::other("Löschziel ohne Eltern"))?,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_audit(
    path: &Path,
    operation: &str,
    status: &str,
    preview: &ContentSyncPreview,
    progress: &Progress,
    stats: Option<&SyncStats>,
    error: Option<&io::Error>,
    known_deleted: u64,
) -> io::Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Inhaltssync-Auditpfad besitzt keinen Elternordner",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut safe_progress = progress.clone();
    safe_progress.current.clear();
    let record = serde_json::json!({"version":1,"operation":"content_sync","operation_id":operation,"unix_ms":SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_millis(),"status":status,"plan_digest":preview.digest,"source_provider":preview.source_provider,"target_provider":preview.target_provider,"execution_class":"best_effort","created_directory_dacl_effects_planned":preview.created_directory_dacl_effects,"created_directory_before_children_effects_planned":preview.created_directory_before_children_effects,"created_directory_dacl_report":stats.map(|stats|stats.created_directory_dacl_report.as_ref().clone()).unwrap_or_else(||error.map(crate::error::created_directory_dacl_report).unwrap_or_else(||progress.created_directory_dacl_report.clone())),"file_dacl_effects_planned":preview.file_dacl_effects,"file_dacl_report":stats.map(|stats|stats.file_dacl_report.clone()).unwrap_or_else(||error.map(crate::error::file_dacl_report).unwrap_or_else(||progress.file_dacl_report.clone())),"directory_dacl_effects_planned":preview.directory_dacl_effects,"directory_dacl_report":stats.map(|stats|stats.directory_dacl_report.clone()).unwrap_or_else(||error.map(crate::error::directory_dacl_report).unwrap_or_else(||progress.directory_dacl_report.clone())),"directory_stream_effects_planned":preview.directory_stream_effects,"directory_stream_report":stats.map(|stats|stats.directory_stream_report.clone()).unwrap_or_else(||error.map(crate::error::directory_stream_report).unwrap_or_else(||progress.directory_stream_report.clone())),"irreversible_replacements_planned":preview.irreversible_replacements,"irreversible_removals_planned":preview.irreversible_removals,"blocked_removals":preview.blocked_removals,"quarantined_replacements_planned":preview.quarantined_replacements,"quarantined_removals_planned":preview.quarantined_removals,"restore_probe_ok":preview.restore_probe_ok,"progress":safe_progress,"metadata_report":stats.map(|stats|&stats.metadata_report),"files_copied":stats.map(|stats|stats.files_copied),"files_deleted":stats.map_or(known_deleted,|stats|stats.files_deleted),"irreversible_replacements_actual":stats.map(|stats|stats.irreversible_replacements),"error_code":error.map(crate::error_code)});
    let (mut file, created) = crate::real_mode::open_audit_append(path)?;
    serde_json::to_writer(&mut file, &record).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    if created {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent)?
            .sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct KnownMtimeOmission {
    pub(crate) kind: crate::sync::MetadataIssueKind,
    reason: String,
}
impl std::fmt::Display for KnownMtimeOmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}
impl std::error::Error for KnownMtimeOmission {}

pub(crate) fn mtime_omission(reason: impl Into<String>) -> io::Error {
    known_mtime_omission(crate::sync::MetadataIssueKind::Unsupported, reason)
}
pub(crate) fn mtime_privilege_omission(reason: impl Into<String>) -> io::Error {
    known_mtime_omission(crate::sync::MetadataIssueKind::PrivilegeFailure, reason)
}
fn known_mtime_omission(
    kind: crate::sync::MetadataIssueKind,
    reason: impl Into<String>,
) -> io::Error {
    io::Error::new(
        if kind == crate::sync::MetadataIssueKind::PrivilegeFailure {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::Unsupported
        },
        KnownMtimeOmission {
            kind,
            reason: reason.into(),
        },
    )
}
pub(crate) fn mtime_omission_reason(error: &io::Error) -> Option<&KnownMtimeOmission> {
    error.get_ref()?.downcast_ref::<KnownMtimeOmission>()
}
impl KnownMtimeOmission {
    pub(crate) fn allowed_in_profile(&self, profile: crate::MetadataProfile) -> bool {
        profile != crate::MetadataProfile::NativeStrict
            && (self.kind != crate::sync::MetadataIssueKind::PrivilegeFailure
                || profile == crate::MetadataProfile::NativeBestEffort)
    }
}

pub(crate) fn record_mtime_omission(
    report: &mut crate::sync::MetadataReport,
    path: &str,
    omission: KnownMtimeOmission,
) {
    record_metadata_omission_kind(
        report,
        path,
        crate::sync::MetadataClass::Mtime,
        omission.kind,
        omission.reason,
    );
}

#[derive(Debug)]
struct KnownNormalModeOmission(String);
impl std::fmt::Display for KnownNormalModeOmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for KnownNormalModeOmission {}

pub(crate) fn normal_mode_omission(reason: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        KnownNormalModeOmission(reason.into()),
    )
}
pub(crate) fn normal_mode_omission_reason(error: &io::Error) -> Option<&str> {
    error
        .get_ref()?
        .downcast_ref::<KnownNormalModeOmission>()
        .map(|omission| omission.0.as_str())
}
pub(crate) fn record_normal_mode_omission(
    report: &mut crate::sync::MetadataReport,
    path: &str,
    reason: String,
) {
    record_metadata_omission(report, path, crate::sync::MetadataClass::NormalMode, reason);
}
pub(crate) fn record_windows_dacl_omission(
    report: &mut crate::sync::MetadataReport,
    path: &str,
    issue: crate::windows_dacl::WindowsDaclTargetIssue,
) {
    use crate::sync::{MetadataClass, MetadataClassStats, MetadataIssue, MetadataIssueKind};
    let kind = if issue.is_access_denied() {
        report.privilege_failures = report.privilege_failures.saturating_add(1);
        MetadataIssueKind::PrivilegeFailure
    } else {
        report.unsupported_metadata = report.unsupported_metadata.saturating_add(1);
        MetadataIssueKind::Unsupported
    };
    if let Some(count) = report
        .classes
        .iter_mut()
        .find(|count| count.class == MetadataClass::Acl)
    {
        count.transferred = count.transferred.saturating_sub(1);
        count.omitted = count.omitted.saturating_add(1);
    } else {
        report.classes.push(MetadataClassStats {
            class: MetadataClass::Acl,
            transferred: 0,
            omitted: 1,
        });
    }
    report.issues.push(MetadataIssue {
        path: path.into(),
        class: MetadataClass::Acl,
        kind,
        reason: issue.reason().into(),
    });
}

fn record_metadata_omission(
    report: &mut crate::sync::MetadataReport,
    path: &str,
    class: crate::sync::MetadataClass,
    reason: String,
) {
    record_metadata_omission_kind(
        report,
        path,
        class,
        crate::sync::MetadataIssueKind::Unsupported,
        reason,
    );
}
fn record_metadata_omission_kind(
    report: &mut crate::sync::MetadataReport,
    path: &str,
    class: crate::sync::MetadataClass,
    kind: crate::sync::MetadataIssueKind,
    reason: String,
) {
    use crate::sync::{MetadataClassStats, MetadataIssue, MetadataIssueKind};
    if report
        .issues
        .iter()
        .any(|issue| issue.path == path && issue.class == class)
    {
        return;
    }
    if let Some(count) = report.classes.iter_mut().find(|count| count.class == class) {
        count.transferred = count.transferred.saturating_sub(1);
        count.omitted = count.omitted.saturating_add(1);
    } else {
        report.classes.push(MetadataClassStats {
            class,
            transferred: 0,
            omitted: 1,
        });
    }
    if kind == MetadataIssueKind::PrivilegeFailure {
        report.privilege_failures = report.privilege_failures.saturating_add(1);
    } else {
        report.unsupported_metadata = report.unsupported_metadata.saturating_add(1);
    }
    report.issues.push(MetadataIssue {
        path: path.to_owned(),
        class,
        kind,
        reason,
    });
}
