//! Orchestrierung eines bestätigten Plans einschließlich aller Preflight-Prüfungen.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::atomic_file::sync_parent;
use crate::path::checked_join;
use crate::progress::{Cancel, Progress};
use crate::quarantine::complete_quarantine_run;
use crate::tree_measure::measure_entry;
use crate::vfs::Vfs;

use super::{
    copy_one, create_hardlink_one, current_optional_copy_snapshot, guard_path, hash_file_hex,
    hash_tree, optional_stat, plan_changed, planned_fork_bytes, quarantine_label,
    regular_copy_snapshot, remove_or_quarantine, start_sync_quarantine_run, summarize_xattrs,
    validate_anchor_identity, validate_metadata_transfer, validate_safety, verify_planned_metadata,
    verify_selected_metadata, verify_sync_quarantine_run, Action, CopyFileRequest, CopyResult,
    HardlinkRequest, MetadataTransfer, RemoveOrQuarantineRequest, Safety, SyncPlan, SyncStats,
};

// ---------------------------------------------------------------------------
// Schritt 2: Plan ausführen
// ---------------------------------------------------------------------------

/// Nach dem Inhaltslauf anzuwendende portable Rechte. Die Planung bindet die Werte;
/// der Executor prüft unmittelbar vor dem Setzen erneut den erwarteten Zielzustand.
struct PendingMode<'a> {
    path: &'a str,
    source_mode: u32,
    target_mode: Option<u32>,
    is_dir: bool,
    is_new: bool,
    content_hash: Option<&'a str>,
}

/// Verzeichnismetadaten werden erst post-order gesetzt, damit restriktive Rechte das
/// Anlegen der zuvor geplanten Kinder nicht verhindern.
struct PendingDirectoryMetadata<'a> {
    path: &'a str,
    source_mtime: Option<i64>,
    target_mtime: Option<i64>,
    metadata: &'a MetadataTransfer,
    is_new: bool,
}

/// Unveränderlicher Ausführungskontext eines vollständig bestätigten Sync-Plans.
/// Die Bündelung verhindert, dass Roots, VFS-Instanzen, Plan und Safety-Vertrag an
/// einer Aufrufstelle versehentlich in unterschiedlicher Reihenfolge übergeben werden.
#[derive(Clone, Copy)]
pub struct ExecuteRequest<'a> {
    pub src: &'a dyn Vfs,
    pub src_root: &'a Path,
    pub dst: &'a dyn Vfs,
    pub dst_root: &'a Path,
    pub plan: &'a SyncPlan,
    pub safety: &'a Safety,
}

#[derive(Default, PartialEq, Eq)]
struct PlanTotals {
    copy_count: u64,
    update_count: u64,
    metadata_count: u64,
    delete_count: u64,
    skip_count: u64,
    delete_bytes: u64,
    bytes_to_transfer: u64,
    additions: u64,
    updates: u64,
    quarantines: u64,
    skipped: u64,
}

/// Bindet die sicherheitsrelevanten Summen erneut an ihre Aktionen. `SyncPlan` ist ein
/// serialisierbares DTO und wird auch in Queue-Journalen gespeichert; seine Zähler
/// dürfen deshalb weder versehentlich noch durch einen alten/manipulierten Datensatz
/// die Löschgates, den Mirror-Zweitscan oder die Quarantäneanlage umgehen.
fn validate_plan_consistency(plan: &SyncPlan) -> io::Result<()> {
    let mut observed_paths = HashSet::new();
    for observation in &plan.metadata_observations {
        crate::path::validate_relative_path(Path::new(&observation.path))?;
        if observation.target_digest.is_none() || !observed_paths.insert(&observation.path) {
            return Err(invalid_plan(
                "Metadatenbeobachtung benötigt einen eindeutigen vorhandenen Zielpfad",
            ));
        }
    }
    let mut totals = PlanTotals::default();
    let mut paths = HashSet::with_capacity(plan.actions.len());
    let mut all_paths = Vec::with_capacity(plan.actions.len());
    let mut delete_directories = Vec::new();

    for (action_index, action) in plan.actions.iter().enumerate() {
        let path = match action {
            Action::CreateDir { path, .. }
            | Action::CopyFile { path, .. }
            | Action::UpdateFile { path, .. }
            | Action::CreateHardlink { path, .. }
            | Action::SetMode { path, .. }
            | Action::SetFileDacl { path, .. }
            | Action::SetDirectoryMetadata { path, .. }
            | Action::DeleteFile { path, .. }
            | Action::DeleteDir { path, .. }
            | Action::SkipSymlink { path }
            | Action::SkipConflict { path }
            | Action::SkipUnsupported { path } => path,
        };
        crate::path::validate_relative_path(Path::new(path))?;
        all_paths.push(path.as_str());
        // Zwei nicht sicher dekodierbare Namen dürfen denselben verlustbehafteten
        // Anzeige-String ergeben. Skip-Aktionen adressieren keinen Providerpfad und
        // sind deshalb von der Eindeutigkeitsprüfung ausgenommen.
        let addresses_target = !matches!(
            action,
            Action::SkipSymlink { .. }
                | Action::SkipConflict { .. }
                | Action::SkipUnsupported { .. }
        );
        if addresses_target && !paths.insert(path.as_str()) {
            return Err(invalid_plan(format!(
                "mehrere Aktionen adressieren denselben Pfad: {path}"
            )));
        }

        let dacl_metadata = match action {
            Action::CreateDir { metadata, .. }
            | Action::CopyFile { metadata, .. }
            | Action::UpdateFile { metadata, .. } => metadata.as_ref(),
            Action::CreateHardlink { metadata, .. }
            | Action::SetDirectoryMetadata { metadata, .. } => Some(metadata),
            _ => None,
        };
        if let Some(crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryExact(selection)) =
            dacl_metadata.and_then(|m| m.windows_dacl.as_ref())
        {
            selection.validate()?;
            let metadata = dacl_metadata.unwrap();
            if !matches!(action, Action::CreateDir { .. })
                || plan.metadata_report.profile != crate::MetadataProfile::NativeBestEffort
                || !metadata.acl
                || metadata.target.is_some()
                || !metadata
                    .source
                    .named_streams
                    .as_ref()
                    .is_some_and(Vec::is_empty)
                || metadata.owner
                || metadata.xattrs
                || metadata.normal_mode
                || metadata.special_mode_bits
                || metadata.resource_fork
                || metadata.directory_streams.is_some()
            {
                return Err(invalid_plan("V7 benötigt einen neuen leeren Ordner ohne weitere mutierende Metadatenklassen"));
            }
        }
        if let Some(
            crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(
                selection,
            ),
        ) = dacl_metadata.and_then(|m| m.windows_dacl.as_ref())
        {
            selection.validate()?;
            let metadata = dacl_metadata.unwrap();
            if !matches!(action, Action::CreateDir { .. })
                || plan.metadata_report.profile != crate::MetadataProfile::NativeBestEffort
                || !metadata.acl
                || metadata.target.is_some()
                || !metadata
                    .source
                    .named_streams
                    .as_ref()
                    .is_some_and(Vec::is_empty)
                || metadata.owner
                || metadata.xattrs
                || metadata.normal_mode
                || metadata.special_mode_bits
                || metadata.resource_fork
                || metadata.directory_streams.is_some()
            {
                return Err(invalid_plan("V8 benötigt einen neuen nichtleeren Ordner ohne weitere mutierende Metadatenklassen"));
            }
        }
        if let Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(dacl)) =
            dacl_metadata.and_then(|metadata| metadata.windows_dacl.as_ref())
        {
            if !matches!(action, Action::SetDirectoryMetadata { .. })
                || plan.metadata_report.profile != crate::MetadataProfile::NativeBestEffort
                || !dacl.target_proof.as_ref().is_some_and(|proof| {
                    proof.validates_directory_action(path) && proof.inode.is_some()
                })
            {
                return Err(invalid_plan("Verzeichnis-DACL benötigt NativeBestEffort und einen vorhandenen gebundenen Zielordner"));
            }
        }

        if let Action::CopyFile {
            metadata: Some(metadata),
            ..
        }
        | Action::UpdateFile {
            metadata: Some(metadata),
            ..
        }
        | Action::CreateHardlink { metadata, .. } = action
        {
            if metadata.directory_streams.is_some() {
                return Err(invalid_plan(
                    "Dateiaktion enthält Verzeichnisstreamwirkungen",
                ));
            }
        }

        match action {
            Action::CreateDir { metadata, .. } => {
                if let Some(metadata) = metadata {
                    validate_directory_metadata_transfer(metadata)?;
                    if metadata.target.is_some() {
                        return Err(invalid_plan(
                            "CreateDir darf kein vorhandenes Zielmanifest enthalten",
                        ));
                    }
                }
                totals.bytes_to_transfer = totals.bytes_to_transfer.saturating_add(
                    super::plan::planned_directory_stream_bytes(metadata.as_ref()),
                );
                totals.additions = totals.additions.saturating_add(1);
            }
            Action::CopyFile { size, metadata, .. } => {
                if let Some(metadata) = metadata {
                    validate_metadata_transfer(metadata)?;
                    if metadata.target.is_some() {
                        return Err(invalid_plan(
                            "CopyFile darf kein vorhandenes Zielmanifest enthalten",
                        ));
                    }
                }
                totals.copy_count = totals.copy_count.saturating_add(1);
                totals.bytes_to_transfer = totals
                    .bytes_to_transfer
                    .saturating_add(size.saturating_add(planned_fork_bytes(metadata.as_ref())));
                totals.additions = totals.additions.saturating_add(1);
            }
            Action::UpdateFile {
                path,
                size,
                target_size,
                target_mtime,
                target_mode,
                target_proof,
                metadata_only,
                metadata,
                ..
            } => {
                if !target_proof.validates_file_action(
                    path,
                    *target_size,
                    *target_mtime,
                    *target_mode,
                ) {
                    return Err(invalid_plan(
                        "UpdateFile-Zielbeleg widerspricht Pfad oder Zielmetadaten",
                    ));
                }
                if let Some(metadata) = metadata {
                    validate_metadata_transfer(metadata)?;
                    if metadata.target.is_none() {
                        return Err(invalid_plan(
                            "UpdateFile benötigt das bestätigte Zielmanifest",
                        ));
                    }
                } else if *metadata_only {
                    return Err(invalid_plan(
                        "Reines Metadaten-Update enthält keinen Metadatenvertrag",
                    ));
                }
                totals.update_count = totals.update_count.saturating_add(1);
                totals.bytes_to_transfer = totals
                    .bytes_to_transfer
                    .saturating_add(size.saturating_add(planned_fork_bytes(metadata.as_ref())));
                if *metadata_only {
                    totals.metadata_count = totals.metadata_count.saturating_add(1);
                }
                totals.updates = totals.updates.saturating_add(1);
            }
            Action::CreateHardlink {
                path,
                link_to,
                target_size,
                target_mtime,
                target_mode,
                target_proof,
                metadata,
                ..
            } => {
                crate::path::validate_relative_path(Path::new(link_to))?;
                validate_metadata_transfer(metadata)?;
                if matches!(
                    metadata.windows_dacl,
                    Some(crate::windows_dacl::WindowsDaclTransfer::Exact(_))
                ) {
                    return Err(invalid_plan(
                        "Windows-DACL-Transfer benötigt eine neue eigene Basisdatei",
                    ));
                }
                if !metadata.hardlinks || metadata.source.hardlink_group.is_none() {
                    return Err(invalid_plan(
                        "CreateHardlink enthält keine gebundene Hardlink-Gruppe",
                    ));
                }
                if target_size.is_some() != metadata.target.is_some() {
                    return Err(invalid_plan(
                        "Hardlink-Zielzustand und Zielmanifest widersprechen einander",
                    ));
                }
                if target_size.is_some() != target_proof.is_some()
                    || target_proof.as_ref().is_some_and(|proof| {
                        !proof.validates_file_action(
                            path,
                            target_size.unwrap_or_default(),
                            *target_mtime,
                            *target_mode,
                        )
                    })
                {
                    return Err(invalid_plan(
                        "Hardlink-Zielbeleg widerspricht Pfad oder Zielzustand",
                    ));
                }
                if target_size.is_some() {
                    totals.update_count = totals.update_count.saturating_add(1);
                    totals.updates = totals.updates.saturating_add(1);
                } else {
                    totals.copy_count = totals.copy_count.saturating_add(1);
                    totals.additions = totals.additions.saturating_add(1);
                }
            }
            Action::SetFileDacl {
                path,
                plan: effect,
                source_hash,
                source_metadata,
            } => {
                effect.validate()?;
                source_metadata.validate()?;
                if !effect
                    .alias_scope
                    .selected_paths
                    .iter()
                    .any(|alias| alias == path)
                    || blake3::Hash::from_hex(source_hash)
                        .ok()
                        .map(|hash| *hash.as_bytes())
                        != Some(effect.content.digest)
                {
                    return Err(invalid_plan(
                        "Datei-DACL-Quellinhalt oder Aliasbindung ist ungültig",
                    ));
                }
                totals.metadata_count = totals.metadata_count.saturating_add(1);
                totals.updates = totals
                    .updates
                    .saturating_add(effect.alias_scope.selected_paths.len() as u64);
            }
            Action::SetMode { .. } | Action::SetDirectoryMetadata { .. } => {
                if let Action::SetDirectoryMetadata { metadata, .. } = action {
                    validate_directory_metadata_transfer(metadata)?;
                    if let Some(streams) = &metadata.directory_streams {
                        if !streams
                            .target_proof
                            .as_ref()
                            .is_some_and(|proof| proof.validates_directory_action(path))
                        {
                            return Err(invalid_plan(
                                "Ordnerstreamplan besitzt keinen passenden Zielordnerbeleg",
                            ));
                        }
                    }
                    totals.bytes_to_transfer = totals.bytes_to_transfer.saturating_add(
                        super::plan::planned_directory_stream_bytes(Some(metadata)),
                    );
                    if metadata.target.is_none() {
                        return Err(invalid_plan(
                            "SetDirectoryMetadata benötigt das bestätigte Zielmanifest",
                        ));
                    }
                }
                totals.metadata_count = totals.metadata_count.saturating_add(1);
                totals.updates = totals.updates.saturating_add(1);
            }
            Action::DeleteFile {
                path,
                size,
                target_mtime,
                target_mode,
                target_proof,
                ..
            } => {
                if !target_proof.validates_file_action(path, *size, *target_mtime, *target_mode) {
                    return Err(invalid_plan(
                        "DeleteFile-Zielbeleg widerspricht Pfad oder Zielmetadaten",
                    ));
                }
                totals.delete_count = totals.delete_count.saturating_add(1);
                totals.delete_bytes = totals.delete_bytes.saturating_add(*size);
                totals.quarantines = totals.quarantines.saturating_add(1);
            }
            Action::DeleteDir {
                path,
                entries,
                bytes,
                target_proof,
                ..
            } => {
                if !target_proof.validates_directory_action(path) {
                    return Err(invalid_plan(
                        "DeleteDir-Zielbeleg widerspricht Pfad oder Zieltyp",
                    ));
                }
                if *entries == 0 {
                    return Err(invalid_plan("DeleteDir enthält keine Einträge"));
                }
                totals.delete_count = totals.delete_count.saturating_add(*entries);
                totals.delete_bytes = totals.delete_bytes.saturating_add(*bytes);
                totals.quarantines = totals.quarantines.saturating_add(1);
                delete_directories.push((action_index, path.as_str()));
            }
            Action::SkipSymlink { .. }
            | Action::SkipConflict { .. }
            | Action::SkipUnsupported { .. } => {
                totals.skip_count = totals.skip_count.saturating_add(1);
                totals.skipped = totals.skipped.saturating_add(1);
            }
        }
    }

    for (delete_index, delete_path) in delete_directories {
        if all_paths.iter().enumerate().any(|(index, other)| {
            index != delete_index && Path::new(other).starts_with(Path::new(delete_path))
        }) {
            return Err(invalid_plan(format!(
                "DeleteDir überlappt eine weitere Planaktion: {delete_path}"
            )));
        }
    }

    if plan.copy_count != totals.copy_count
        || plan.update_count != totals.update_count
        || plan.metadata_count != totals.metadata_count
        || plan.delete_count != totals.delete_count
        || plan.skip_count != totals.skip_count
        || plan.delete_bytes != totals.delete_bytes
        || plan.bytes_to_transfer != totals.bytes_to_transfer
    {
        return Err(invalid_plan(
            "Aktions-, Byte- oder Metadatenzähler stimmen nicht mit den Planaktionen überein",
        ));
    }
    if plan.delete_count > plan.target_count {
        return Err(invalid_plan(
            "Löschzahl ist größer als der bestätigte Zielbestand",
        ));
    }

    // Alte Queue-Journale besitzen noch keine Vergleichszeilen. Sobald der DTO aber
    // vorhanden ist, müssen seine sichtbaren Kategorien denselben Aktionen folgen.
    if !plan.comparison.rows.is_empty()
        && (plan.comparison.rows.len()
            != plan.actions.len()
                + plan
                    .actions
                    .iter()
                    .map(|action| match action {
                        Action::SetFileDacl { plan, .. } => {
                            plan.alias_scope.selected_paths.len().saturating_sub(1)
                        }
                        _ => 0,
                    })
                    .sum::<usize>()
            || plan.comparison.categories.additions != totals.additions
            || plan.comparison.categories.updates != totals.updates
            || plan.comparison.categories.quarantines != totals.quarantines
            || plan.comparison.categories.skipped != totals.skipped)
    {
        return Err(invalid_plan(
            "Vergleichsansicht stimmt nicht mit den Planaktionen überein",
        ));
    }
    Ok(())
}

fn invalid_plan(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Führt einen bestätigten Plan aus. Meldet Fortschritt über `on_progress` und bricht
/// bei gesetztem `cancel` an der nächsten Aktionsgrenze sauber ab.
///
/// `safety` steuert den Datenschutz: `DryRun` verändert nichts, `TestSandbox`
/// verlangt die harte Sandbox-Schranke, `Real` ein kurzlebiges einmaliges Permit und
/// nur der ausdrücklich benannte Headless-Testmodus darf ohne diese Schranken mutieren.
pub fn execute(
    request: ExecuteRequest<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<SyncStats> {
    let mut stats = SyncStats {
        execution_class: request
            .safety
            .content_sync_permit()
            .map(|_| super::SyncExecutionClass::BestEffort),
        local_execution: request.safety.local_execution().cloned(),
        dry_run: request.safety.is_dry_run(),
        metadata_report: request.plan.metadata_report.clone(),
        ..Default::default()
    };
    // Auch Fehler späterer Aktionen müssen Reste früherer Übertragungen behalten.
    execute_with_stats(request, cancel, on_progress, &mut stats).map_err(|error| {
        crate::error::with_directory_dacl_report(
            crate::error::with_directory_stream_report(
                crate::error::with_file_dacl_report(
                    crate::error::with_created_directory_dacl_report(
                        crate::error::with_recovery_paths(error, &stats.temp_recovery_paths),
                        &stats.created_directory_dacl_report,
                    ),
                    &stats.file_dacl_report,
                ),
                &stats.directory_stream_report,
            ),
            &stats.directory_dacl_report,
        )
    })?;
    Ok(stats)
}

fn execute_with_stats(
    request: ExecuteRequest<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    stats: &mut SyncStats,
) -> io::Result<()> {
    let ExecuteRequest {
        src,
        src_root,
        dst,
        dst_root,
        plan,
        safety,
    } = request;
    validate_plan_consistency(plan)?;
    validate_safety(src, src_root, dst, dst_root, plan, safety)?;

    let dry = safety.is_dry_run();
    if !dry {
        super::verify_metadata_observations(src, src_root, dst, dst_root, plan)?;
        for expected in &plan.local_parent_inheritance {
            let parent = &expected.identity.display_path;
            if !parent.starts_with(dst_root)
                || dst.local_parent_inheritance(parent)?.as_ref() != Some(expected)
            {
                return Err(plan_changed(
                    &parent.to_string_lossy(),
                    "Lokale Ziel-Vererbung änderte sich seit der Vorschau",
                ));
            }
        }
        // Verzeichnisse werden absichtlich erst nach ihren Kindern verändert. Ihre
        // gesamte Planbindung wird deshalb einmal vor der ersten Zielmutation geprüft.
        preflight_directory_metadata(src, src_root, dst, dst_root, plan)?;
        // Auch alle laufbezogenen Inode-Beziehungen werden vor der ersten Kopie
        // geprüft. Ein späterer Gruppenfehler darf keinen halbfertigen Zielbaum lassen.
        preflight_hardlinks(src, src_root, dst, dst_root, plan)?;
        preflight_file_dacl(src, src_root, dst, dst_root, plan, safety)?;
    }
    let quarantine_run = (!dry
        && !safety.irreversible_replacements()
        && (plan.update_count > 0 || plan.delete_count > 0))
        .then_some(safety.quarantine.as_deref())
        .flatten();
    let quarantine_binding = if let Some(run_root) = quarantine_run {
        // Manifest, Laufordner und dessen Identität müssen vor dem ersten möglichen
        // Ziel-Rename gemeinsam als nicht serialisierbarer Backendzustand vorliegen.
        Some(start_sync_quarantine_run(dst, run_root, dst_root, safety)?)
    } else {
        None
    };
    let quarantine_run_proof = quarantine_binding.as_ref().map(|run| run.proof());
    let mut prog = Progress {
        files_total: plan.copy_count.saturating_add(plan.update_count),
        bytes_total: plan.bytes_to_transfer,
        phase: "copy".into(),
        ..Default::default()
    };

    let mut early_completed_directories = HashSet::new();
    let mut before_children_keys = std::collections::BTreeMap::new();
    let mut created_directory_proofs = std::collections::BTreeMap::new();
    // Phase 1: erst ALLE nicht löschenden Aktionen. Dadurch kann ein später Lesefehler
    // niemals dazu führen, dass bereits Zielobjekte entfernt wurden, obwohl Kopien fehlen.
    for action in &plan.actions {
        if matches!(action, Action::DeleteFile { .. } | Action::DeleteDir { .. }) {
            continue;
        }
        if cancel.is_cancelled() {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        if !dry {
            validate_anchor_identity(src, dst, safety)?;
        }

        match action {
            Action::CreateDir {
                path,
                metadata,
                source_mtime,
                ..
            } => {
                if !dry {
                    let target = checked_join(dst_root, path)?;
                    guard_path(safety.sandbox(), &target, "Schreibziel")?;
                    if optional_stat(dst, &target)?.is_some() {
                        return Err(plan_changed(path, "Zielordner existiert inzwischen"));
                    }
                    if let Some((metadata, selection)) = metadata.as_ref().and_then(|m| match &m.windows_dacl {
                        Some(crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(s)) => Some((m, s.as_ref())),
                        _ => None,
                    }) {
                        if safety.content_sync_permit().is_none() {
                            return Err(invalid_plan("V8 benötigt bestätigten gewöhnlichen Inhaltssync"));
                        }
                        let source_path = checked_join(src_root, path)?;
                        if let Some(omission) = validate_created_nonempty_source(src, &source_path, *source_mtime, metadata, &selection.source_children)? {
                            omission.record_before_children(&mut stats.metadata_report, path);
                        } else {
                            let scope = dst.staging_scope(&target, safety.sandbox())?;
                            let directory_vfs = scope.as_deref().unwrap_or(dst);
                            let result = directory_vfs.create_directory_before_children_receipt(&target);
                            let receipt = crate::atomic_file::finish_staging_scope(directory_vfs, result, |_, recovery| stats.temp_recovery_paths.push(recovery))?;
                            if let Some(mut receipt) = receipt {
                                let Some(key) = receipt.effect_key() else {
                                    let omission = receipt.omission();
                                    receipt.abandon()?;
                                    crate::content_sync::record_windows_dacl_omission(&mut stats.metadata_report, path, omission);
                                    // Die gewöhnliche Anlage ist bereits bestätigt. Kein
                                    // Originalbeleg wird aus einem späteren Pfadstat erfunden.
                                    continue;
                                };
                                {
                                    before_children_keys.insert(path.clone(), key.clone());
                                    stats.created_directory_dacl_report.observe_created(crate::created_directory_dacl::CreatedDirectoryObservation { effect_key: key, ordinary_metadata_applied: false });
                                    prog.created_directory_dacl_report = stats.created_directory_dacl_report.as_ref().clone();
                                    prog.current = path.clone();
                                    prog.phase = "directory_created".into();
                                    on_progress(&prog);
                                }
                                if cancel.is_cancelled() {
                                    receipt.abandon()?;
                                    return finish_cancelled(stats, &mut prog, on_progress);
                                }
                                sync_parent(directory_vfs, &target)?;
                                let mut transaction = match receipt.begin_before_children(selection) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        if let Some(issue) = crate::windows_dacl::known_write_omission(&error) {
                                            crate::content_sync::record_windows_dacl_omission(&mut stats.metadata_report, path, issue);
                                            continue;
                                        }
                                        return Err(error);
                                    }
                                };
                                match validate_created_nonempty_source(src, &source_path, *source_mtime, metadata, &selection.source_children) {
                                    Ok(None) => {},
                                    Ok(Some(omission)) => {
                                        transaction.abandon_prepared()?;
                                        omission.record_before_children(&mut stats.metadata_report, path);
                                        continue;
                                    },
                                    Err(error) => { transaction.abandon_prepared()?; return Err(error); }
                                }
                                if cancel.is_cancelled() {
                                    transaction.abandon_prepared()?;
                                    return finish_cancelled(stats, &mut prog, on_progress);
                                }
                                let result = transaction.apply();
                                stats.created_directory_dacl_report.merge(&transaction.report());
                                prog.created_directory_dacl_report = stats.created_directory_dacl_report.as_ref().clone();
                                prog.phase = "directory_rights_before_children".into();
                                // Der synchrone Queuefortschritt bestätigt Applied dauerhaft,
                                // bevor finish alle Handles und den Anlageintent abschließt.
                                on_progress(&prog);
                                let paths = transaction.recovery_paths();
                                if let Err(error) = result {
                                    if !transaction.report().has_rights_effects() {
                                        if let Some(issue) = crate::windows_dacl::known_write_omission(&error) {
                                            transaction.abandon_prepared()?;
                                            crate::content_sync::record_windows_dacl_omission(&mut stats.metadata_report, path, issue);
                                            continue;
                                        }
                                    }
                                    stats.temp_recovery_paths.extend(paths);
                                    return Err(error);
                                }
                                if cancel.is_cancelled() {
                                    stats.temp_recovery_paths.extend(paths);
                                    return finish_cancelled(stats, &mut prog, on_progress);
                                }
                                match transaction.finish() {
                                    Ok(report) => stats.created_directory_dacl_report.merge(&report),
                                    Err(error) => { stats.temp_recovery_paths.extend(paths); return Err(error); }
                                }
                                prog.created_directory_dacl_report = stats.created_directory_dacl_report.as_ref().clone();
                                on_progress(&prog);
                                // Kein vorgezogener Metadatenabschluss: Kinder und normale
                                // postorder-mtime bleiben weiterhin auszuführen.
                                prog.phase = "copy".into();
                                continue;
                            } else {
                                crate::content_sync::record_windows_dacl_omission(&mut stats.metadata_report, path, crate::windows_dacl::WindowsDaclTargetIssue::UnsupportedBackend);
                            }
                        }
                    }
                    if let Some((metadata, selection)) =
                        metadata.as_ref().and_then(|m| match &m.windows_dacl {
                            Some(
                                crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryExact(s),
                            ) => Some((m, s.as_ref())),
                            _ => None,
                        })
                    {
                        if safety.content_sync_permit().is_none() {
                            return Err(invalid_plan(
                                "V7 benötigt den gewöhnlichen bestätigten Inhaltssync",
                            ));
                        }
                        let source_path = checked_join(src_root, path)?;
                        if let Some(omission) = validate_created_empty_source(
                            src,
                            &source_path,
                            *source_mtime,
                            metadata,
                        )? {
                            omission.record(&mut stats.metadata_report, path);
                        } else {
                            let scope = dst.staging_scope(&target, safety.sandbox())?;
                            let directory_vfs = scope.as_deref().unwrap_or(dst);
                            let receipt_result =
                                directory_vfs.create_directory_dacl_receipt(&target);
                            let receipt_result = crate::atomic_file::finish_staging_scope(
                                directory_vfs,
                                receipt_result,
                                |_, path| {
                                    stats.temp_recovery_paths.push(path);
                                },
                            )?;
                            if let Some(mut receipt) = receipt_result {
                                if let Some(key) = receipt.effect_key() {
                                    stats.created_directory_dacl_report.observe_created(
                                    crate::created_directory_dacl::CreatedDirectoryObservation {
                                        effect_key: key,
                                        ordinary_metadata_applied: false,
                                    },
                                );
                                    prog.created_directory_dacl_report =
                                        stats.created_directory_dacl_report.as_ref().clone();
                                    prog.current = path.clone();
                                    prog.phase = "directory_created".into();
                                    on_progress(&prog);
                                }

                                if cancel.is_cancelled() {
                                    receipt.abandon()?;
                                    drop(receipt);
                                    return finish_cancelled(stats, &mut prog, on_progress);
                                }
                                sync_parent(directory_vfs, &target)?;
                                // Vor der ersten normalen Metadatenmutation darf eine
                                // zusätzliche Beobachtung das optionale Receipt aufgeben.
                                let read_before = validate_directory_source(
                                    src,
                                    &source_path,
                                    *source_mtime,
                                    metadata,
                                )
                                .and_then(|()| directory_vfs.stat(&target).map(|_| ()));
                                let mut mtime_state = DirectoryMtime::Pending;
                                let mut usable = match read_before {
                                    Ok(()) => match receipt.set_mtime(*source_mtime) {
                                        Ok(usable) => {
                                            mtime_state = DirectoryMtime::Applied;
                                            usable
                                        }
                                        Err(error) => {
                                            if let Some(omission) =
                                                safety.mtime_omission_reason(&error)
                                            {
                                                receipt.abandon()?;
                                                mtime_state =
                                                    DirectoryMtime::Omitted(omission.clone());
                                                false
                                            } else {
                                                return Err(error);
                                            }
                                        }
                                    },
                                    Err(error)
                                        if matches!(
                                            crate::error_code(&error),
                                            crate::ErrorCode::PlanDrift
                                                | crate::ErrorCode::Interrupted
                                        ) =>
                                    {
                                        return Err(error)
                                    }
                                    Err(_) => {
                                        receipt.abandon()?;
                                        false
                                    }
                                };
                                let normal_request = || DirectoryMetadataRequest {
                                    src,
                                    source_path: &source_path,
                                    dst: directory_vfs,
                                    target_path: &target,
                                    source_mtime: *source_mtime,
                                    target_mtime: None,
                                    metadata,
                                    is_new: true,
                                    permit: safety.content_sync_permit(),
                                    created_key: None,
                                    created_dacl_applied: false,
                                };
                                let normal =
                                    apply_directory_metadata(normal_request(), &mtime_state);
                                let (omitted_mtime, omitted_mode) = match normal {
                                    Ok(value) => value,
                                    Err(error)
                                        if !matches!(mtime_state, DirectoryMtime::Pending)
                                            && !matches!(
                                                crate::error_code(&error),
                                                crate::ErrorCode::PlanDrift
                                                    | crate::ErrorCode::Interrupted
                                            ) =>
                                    {
                                        receipt.abandon()?;
                                        usable = false;
                                        // In dieser engen SMB-Klasse sind nach mtime nur
                                        // Beobachtungen übrig. Kein zweiter normaler SET.
                                        apply_directory_metadata(normal_request(), &mtime_state)?
                                    }
                                    Err(error) => return Err(error),
                                };
                                let ordinary_metadata_applied =
                                    omitted_mtime.is_none() && omitted_mode.is_none();
                                if let Some(reason) = omitted_mtime {
                                    crate::content_sync::record_mtime_omission(
                                        &mut stats.metadata_report,
                                        path,
                                        reason,
                                    );
                                }
                                if let Some(reason) = omitted_mode {
                                    crate::content_sync::record_normal_mode_omission(
                                        &mut stats.metadata_report,
                                        path,
                                        reason,
                                    );
                                }
                                // Der normale CREATE-Intent wurde genau einmal direkt
                                // nach der bestätigten Anlage abgeschlossen. mtime ist
                                // eine getrennte normale Metadatenwirkung, kein zweites mkdir-Finish.
                                if let Some(key) = receipt.effect_key() {
                                    stats.created_directory_dacl_report.observe_created(
                                    crate::created_directory_dacl::CreatedDirectoryObservation {
                                        effect_key: key,
                                        ordinary_metadata_applied,
                                    },
                                );
                                    prog.created_directory_dacl_report =
                                        stats.created_directory_dacl_report.as_ref().clone();
                                    prog.phase = "directory_metadata_applied".into();
                                    on_progress(&prog);
                                }

                                // Auch eine legitim ausgelassene Zeit ist abschließend
                                // bilanziert. Der spätere Durchgang darf sie nicht erneut setzen.
                                early_completed_directories.insert(path.clone());
                                if cancel.is_cancelled() {
                                    receipt.abandon()?;
                                    return finish_cancelled(stats, &mut prog, on_progress);
                                }
                                if usable {
                                    if let Some(omission) = validate_created_empty_source(
                                        src,
                                        &source_path,
                                        *source_mtime,
                                        metadata,
                                    )? {
                                        receipt.abandon()?;
                                        omission.record(&mut stats.metadata_report, path);
                                        continue;
                                    }
                                    if cancel.is_cancelled() {
                                        receipt.abandon()?;
                                        return finish_cancelled(stats, &mut prog, on_progress);
                                    }
                                    let mut transaction = match receipt.begin(selection) {
                                        Ok(transaction) => transaction,
                                        Err(error) => {
                                            if let Some(issue) =
                                                crate::windows_dacl::known_write_omission(&error)
                                            {
                                                crate::content_sync::record_windows_dacl_omission(
                                                    &mut stats.metadata_report,
                                                    path,
                                                    issue,
                                                );
                                                continue;
                                            }
                                            return Err(error);
                                        }
                                    };
                                    match validate_created_empty_source(
                                        src,
                                        &source_path,
                                        *source_mtime,
                                        metadata,
                                    ) {
                                        Ok(None) => {}
                                        Ok(Some(omission)) => {
                                            transaction.abandon_prepared()?;
                                            omission.record(&mut stats.metadata_report, path);
                                            continue;
                                        }
                                        Err(error) => {
                                            transaction.abandon_prepared()?;
                                            return Err(error);
                                        }
                                    }
                                    let result = transaction.apply();
                                    stats
                                        .created_directory_dacl_report
                                        .merge(&transaction.report());
                                    prog.created_directory_dacl_report =
                                        stats.created_directory_dacl_report.as_ref().clone();
                                    prog.current = path.clone();
                                    on_progress(&prog);
                                    let paths = transaction.recovery_paths();
                                    if let Err(error) = result {
                                        if !transaction.report().has_rights_effects() {
                                            if let Some(issue) =
                                                crate::windows_dacl::known_write_omission(&error)
                                            {
                                                transaction.abandon_prepared()?;
                                                crate::content_sync::record_windows_dacl_omission(
                                                    &mut stats.metadata_report,
                                                    path,
                                                    issue,
                                                );
                                                continue;
                                            }
                                        }
                                        stats.temp_recovery_paths.extend(paths);
                                        return Err(error);
                                    }
                                    if cancel.is_cancelled() {
                                        stats.temp_recovery_paths.extend(paths);
                                        return finish_cancelled(stats, &mut prog, on_progress);
                                    }
                                    match transaction.finish() {
                                        Ok(report) => {
                                            stats.created_directory_dacl_report.merge(&report)
                                        }
                                        Err(error) => {
                                            stats.temp_recovery_paths.extend(paths);
                                            return Err(error);
                                        }
                                    }
                                    prog.created_directory_dacl_report =
                                        stats.created_directory_dacl_report.as_ref().clone();
                                    on_progress(&prog);
                                } else {
                                    let omission = receipt.omission();
                                    drop(receipt);
                                    crate::content_sync::record_windows_dacl_omission(
                                        &mut stats.metadata_report,
                                        path,
                                        omission,
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    // Atomare Einzelanlage: Entsteht der Name nach der Planprüfung,
                    // überschreiben/akzeptieren wir ihn nicht stillschweigend.
                    if safety.content_sync_permit().is_some() {
                        let scope = dst.staging_scope(&target, safety.sandbox())?;
                        let directory_vfs = scope.as_deref().unwrap_or(dst);
                        let result = directory_vfs
                            .create_dir(&target)
                            .and_then(|()| sync_parent(directory_vfs, &target));
                        crate::atomic_file::finish_staging_scope(
                            directory_vfs,
                            result,
                            |(), _| {},
                        )?;
                    } else {
                        dst.create_dir(&target)?;
                        sync_parent(dst, &target)?;
                    }
                    if metadata
                        .as_ref()
                        .is_some_and(|value| value.directory_streams.is_some())
                    {
                        let entry = dst.stat(&target)?;
                        let proof =
                            super::SyncTargetProof::from_listed(&crate::vfs::ProofListEntry {
                                raw_name: entry.name.as_bytes().to_vec(),
                                entry,
                                safely_decoded: true,
                                provider_token: None,
                            })?;
                        created_directory_proofs.insert(path.clone(), proof);
                    }
                }
            }
            Action::CopyFile {
                path,
                size,
                source_mtime,
                source_mode,
                source_hash,
                metadata,
            } => {
                prog.current = path.clone();
                on_progress(&prog);
                if dry {
                    // Nur simulieren: Fortschritt vorspulen, nichts schreiben.
                    prog.bytes_done = prog
                        .bytes_done
                        .saturating_add(size.saturating_add(planned_fork_bytes(metadata.as_ref())));
                } else {
                    match copy_one(
                        CopyFileRequest {
                            src,
                            src_path: &checked_join(src_root, path)?,
                            dst,
                            dst_path: &checked_join(dst_root, path)?,
                            relative_path: Path::new(path),
                            expected_size: *size,
                            expected_source_mtime: *source_mtime,
                            expected_source_mode: *source_mode,
                            expected_target: None,
                            expected_target_proof: None,
                            expected_source_hash: source_hash.as_deref(),
                            expected_target_hash: None,
                            expected_metadata: metadata.as_ref(),
                            quarantine_run_proof,
                            safety,
                        },
                        cancel,
                        &mut prog,
                        on_progress,
                    )? {
                        CopyResult::Complete {
                            quarantined,
                            recovery_paths,
                            omitted_mtime,
                            omitted_normal_mode,
                            omitted_windows_dacl,
                        } => {
                            if let Some(issue) = omitted_windows_dacl {
                                crate::content_sync::record_windows_dacl_omission(
                                    &mut stats.metadata_report,
                                    path,
                                    issue,
                                );
                            }
                            if let Some(reason) = omitted_normal_mode {
                                crate::content_sync::record_normal_mode_omission(
                                    &mut stats.metadata_report,
                                    path,
                                    reason,
                                );
                            }
                            if let Some(reason) = omitted_mtime {
                                crate::content_sync::record_mtime_omission(
                                    &mut stats.metadata_report,
                                    path,
                                    reason,
                                );
                            }
                            stats.temp_recovery_paths.extend(recovery_paths);
                            if quarantined {
                                stats.quarantine = quarantine_label(safety);
                            }
                        }
                        CopyResult::Cancelled { recovery_paths } => {
                            stats.temp_recovery_paths.extend(recovery_paths);
                            return finish_cancelled(stats, &mut prog, on_progress);
                        }
                    }
                }
                stats.files_copied = stats.files_copied.saturating_add(1);
                stats.bytes_copied = stats
                    .bytes_copied
                    .saturating_add(size.saturating_add(planned_fork_bytes(metadata.as_ref())));
                prog.files_done = prog.files_done.saturating_add(1);
                on_progress(&prog);
            }
            Action::UpdateFile {
                path,
                size,
                source_mtime,
                source_mode,
                target_size,
                target_mtime,
                target_mode,
                target_proof,
                source_hash,
                target_hash,
                metadata_only,
                metadata,
            } => {
                prog.current = path.clone();
                on_progress(&prog);
                if dry {
                    prog.bytes_done = prog
                        .bytes_done
                        .saturating_add(size.saturating_add(planned_fork_bytes(metadata.as_ref())));
                } else {
                    match copy_one(
                        CopyFileRequest {
                            src,
                            src_path: &checked_join(src_root, path)?,
                            dst,
                            dst_path: &checked_join(dst_root, path)?,
                            relative_path: Path::new(path),
                            expected_size: *size,
                            expected_source_mtime: *source_mtime,
                            expected_source_mode: *source_mode,
                            expected_target: Some((*target_size, *target_mtime, *target_mode)),
                            expected_target_proof: Some(target_proof),
                            expected_source_hash: source_hash.as_deref(),
                            expected_target_hash: target_hash.as_deref(),
                            expected_metadata: metadata.as_ref(),
                            quarantine_run_proof,
                            safety,
                        },
                        cancel,
                        &mut prog,
                        on_progress,
                    )? {
                        CopyResult::Complete {
                            quarantined,
                            recovery_paths,
                            omitted_mtime,
                            omitted_normal_mode,
                            omitted_windows_dacl,
                        } => {
                            if let Some(issue) = omitted_windows_dacl {
                                crate::content_sync::record_windows_dacl_omission(
                                    &mut stats.metadata_report,
                                    path,
                                    issue,
                                );
                            }
                            if let Some(reason) = omitted_normal_mode {
                                crate::content_sync::record_normal_mode_omission(
                                    &mut stats.metadata_report,
                                    path,
                                    reason,
                                );
                            }
                            if let Some(reason) = omitted_mtime {
                                crate::content_sync::record_mtime_omission(
                                    &mut stats.metadata_report,
                                    path,
                                    reason,
                                );
                            }
                            stats.temp_recovery_paths.extend(recovery_paths);
                            if quarantined {
                                stats.quarantine = quarantine_label(safety);
                            }
                        }
                        CopyResult::Cancelled { recovery_paths } => {
                            stats.temp_recovery_paths.extend(recovery_paths);
                            return finish_cancelled(stats, &mut prog, on_progress);
                        }
                    }
                }
                stats.files_copied = stats.files_copied.saturating_add(1);
                stats.bytes_copied = stats
                    .bytes_copied
                    .saturating_add(size.saturating_add(planned_fork_bytes(metadata.as_ref())));
                if *metadata_only {
                    stats.metadata_updated = stats.metadata_updated.saturating_add(1);
                }
                if !dry && safety.irreversible_replacements() {
                    stats.irreversible_replacements =
                        stats.irreversible_replacements.saturating_add(1);
                }
                prog.files_done = prog.files_done.saturating_add(1);
                on_progress(&prog);
            }
            Action::CreateHardlink {
                path,
                link_to,
                size,
                source_mtime,
                source_mode,
                source_hash,
                target_size,
                target_mtime,
                target_mode,
                target_proof,
                target_hash,
                metadata,
            } => {
                prog.current = path.clone();
                on_progress(&prog);
                if !dry {
                    let expected_target =
                        target_size.map(|size| (size, *target_mtime, *target_mode));
                    let (quarantined, recovery_paths) = create_hardlink_one(HardlinkRequest {
                        src,
                        src_root,
                        dst,
                        dst_root,
                        path,
                        link_to,
                        expected_source: (*size, *source_mtime, *source_mode),
                        expected_target,
                        expected_target_proof: target_proof.as_ref(),
                        source_hash,
                        target_hash: target_hash.as_deref(),
                        metadata,
                        quarantine_run_proof,
                        safety,
                    })?;
                    stats.temp_recovery_paths.extend(recovery_paths);
                    if quarantined {
                        stats.quarantine = quarantine_label(safety);
                    }
                }
                stats.files_copied = stats.files_copied.saturating_add(1);
                if !dry && target_size.is_some() && safety.irreversible_replacements() {
                    stats.irreversible_replacements =
                        stats.irreversible_replacements.saturating_add(1);
                }
                prog.files_done = prog.files_done.saturating_add(1);
                on_progress(&prog);
            }
            Action::SkipSymlink { .. }
            | Action::SkipConflict { .. }
            | Action::SkipUnsupported { .. }
            | Action::SetMode { .. }
            | Action::SetFileDacl { .. }
            | Action::SetDirectoryMetadata { .. } => { /* bewusst hier noch nichts tun */ }
            Action::DeleteFile { .. } | Action::DeleteDir { .. } => unreachable!(),
        }
    }

    // Dateirechte vor den Entfernungen abschließen; endgültige Ordnerrechte bleiben
    // bis nach den Kind-Renames offen. Tiefere Ordner kommen beim Abschluss zuerst.
    let mut pending_modes = Vec::new();
    for action in &plan.actions {
        match action {
            Action::CreateDir {
                path,
                source_mode: Some(source_mode),
                metadata: None,
                ..
            } => pending_modes.push(PendingMode {
                path,
                source_mode: *source_mode,
                target_mode: None,
                is_dir: true,
                is_new: true,
                content_hash: None,
            }),
            Action::SetMode {
                path,
                source_mode,
                target_mode,
                is_dir,
                content_hash,
            } => pending_modes.push(PendingMode {
                path,
                source_mode: *source_mode,
                target_mode: *target_mode,
                is_dir: *is_dir,
                is_new: false,
                content_hash: content_hash.as_deref(),
            }),
            _ => {}
        }
    }
    pending_modes.sort_by_key(|item| std::cmp::Reverse(item.path.matches('/').count()));

    for pending in pending_modes.iter().filter(|pending| !pending.is_dir) {
        if cancel.is_cancelled() {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        prog.phase = "metadata".into();
        prog.current = pending.path.to_string();
        on_progress(&prog);
        if let Some(reason) = apply_pending_mode(pending, &request)? {
            crate::content_sync::record_normal_mode_omission(
                &mut stats.metadata_report,
                pending.path,
                reason,
            );
        } else if !pending.is_new {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
        }
    }

    // Erst sämtliche Streamkopien bestätigen; danach dürfen Stream- und
    // Dateientfernungen beginnen. Ordnerrechte und -zeiten folgen weiterhin zuletzt.
    for removals in [false, true] {
        if !super::directory_streams::execute_directory_streams(
            super::directory_streams::DirectoryStreamExecution {
                request: ExecuteRequest {
                    src,
                    src_root,
                    dst,
                    dst_root,
                    plan,
                    safety,
                },
                created: &created_directory_proofs,
                removals,
            },
            cancel,
            &mut prog,
            stats,
            on_progress,
        )? {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
    }

    // Phase 2: Überzählige Zielobjekte nach allen erfolgreichen Dateiübertragungen
    // und Datei-Metadaten entfernen. Ordner-Metadaten folgen nach dieser Phase.
    // In der App bedeutet „entfernen" ein Verschieben in die laufbezogene Quarantäne.
    if !dry && plan.delete_count > 0 {
        if let Some(permit) = safety.real_permit() {
            permit.validate_mirror_rescan(src, src_root, dst, dst_root, plan)?;
        }
        if let Some(permit) = safety.content_sync_permit() {
            let stream_report = stats.directory_stream_report.clone();
            let cancelled = permit.execute_removals(
                src,
                dst,
                cancel,
                &mut stats.files_deleted,
                &mut |value| {
                    let mut cumulative = value.clone();
                    cumulative.directory_stream_report.merge(&stream_report);
                    on_progress(&cumulative);
                },
            )?;
            if cancelled {
                return finish_cancelled(stats, &mut prog, on_progress);
            }
        }
    }
    let mut removal_progress = Progress {
        directory_stream_report: stats.directory_stream_report.clone(),
        phase: "quarantine".into(),
        files_total: plan.delete_count,
        files_done: stats.files_deleted,
        ..Progress::default()
    };
    for action in &plan.actions {
        if safety.content_sync_permit().is_some() && safety.quarantine.is_none() {
            continue;
        }
        if cancel.is_cancelled() {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        match action {
            Action::DeleteFile {
                path,
                size,
                target_mtime,
                target_mode,
                target_proof,
                target_hash,
            } => {
                removal_progress.current = path.clone();
                on_progress(&removal_progress);
                let target = checked_join(dst_root, path)?;
                if !dry {
                    validate_anchor_identity(src, dst, safety)?;
                    let current = regular_copy_snapshot(dst, &target)?;
                    if current != (*size, *target_mtime, *target_mode) {
                        return Err(plan_changed(
                            path,
                            "Zieldatei wurde seit der Vorschau geändert",
                        ));
                    }
                    super::atomic::verify_target_proof(dst, &target, Some(target_proof))?;
                    if let Some(expected_hash) = target_hash {
                        if hash_file_hex(dst, &target)? != *expected_hash {
                            return Err(plan_changed(
                                path,
                                "Zieldatei-Inhalt änderte sich seit der Hash-Vorschau",
                            ));
                        }
                    }
                    validate_anchor_identity(src, dst, safety)?;
                    if remove_or_quarantine(RemoveOrQuarantineRequest {
                        dst,
                        target: &target,
                        relative_path: Path::new(path),
                        is_dir: false,
                        expected_target_proof: target_proof,
                        expected_hash: target_hash.as_deref(),
                        quarantine_run_proof,
                        safety,
                    })? {
                        stats.quarantine = quarantine_label(safety);
                    }
                }
                stats.files_deleted = stats.files_deleted.saturating_add(1);
                removal_progress.files_done = stats.files_deleted;
                on_progress(&removal_progress);
            }
            Action::DeleteDir {
                path,
                entries,
                bytes,
                target_proof,
                target_tree_hash,
            } => {
                removal_progress.current = path.clone();
                on_progress(&removal_progress);
                let target = checked_join(dst_root, path)?;
                if !dry {
                    validate_anchor_identity(src, dst, safety)?;
                    let current = dst.stat(&target)?;
                    if !current.is_dir || current.is_symlink {
                        return Err(plan_changed(
                            path,
                            "Löschziel ist nicht mehr derselbe Ordner",
                        ));
                    }
                    super::atomic::verify_target_proof(dst, &target, Some(target_proof))?;
                    let measured = measure_entry(dst, &target, &current)?;
                    if measured != (*entries, *bytes) {
                        return Err(plan_changed(
                            path,
                            "Zielordner wurde seit der Vorschau geändert",
                        ));
                    }
                    if let Some(expected_hash) = target_tree_hash {
                        if hash_tree(dst, &target)?.0 != *expected_hash {
                            return Err(plan_changed(
                                path,
                                "Zielordner-Inhalt änderte sich seit der Hash-Vorschau",
                            ));
                        }
                    }
                    validate_anchor_identity(src, dst, safety)?;
                    if remove_or_quarantine(RemoveOrQuarantineRequest {
                        dst,
                        target: &target,
                        relative_path: Path::new(path),
                        is_dir: true,
                        expected_target_proof: target_proof,
                        expected_hash: target_tree_hash.as_deref(),
                        quarantine_run_proof,
                        safety,
                    })? {
                        stats.quarantine = quarantine_label(safety);
                    }
                }
                stats.files_deleted = stats.files_deleted.saturating_add(*entries);
                removal_progress.files_done = stats.files_deleted;
                on_progress(&removal_progress);
            }
            _ => {}
        }
    }

    // Erst nach den letzten Kind-Renames die endgültigen Ordnerrechte, ACLs und
    // Zeitstempel setzen. Sonst sperrt etwa 0555 die eigene Mirror-Entfernung, und
    // ein späteres Quarantäne-Rename würde die bestätigte Ordner-mtime wieder ändern.
    for pending in pending_modes.iter().filter(|pending| pending.is_dir) {
        if cancel.is_cancelled() {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        prog.phase = "metadata".into();
        prog.current = pending.path.to_string();
        on_progress(&prog);
        if let Some(reason) = apply_pending_mode(pending, &request)? {
            crate::content_sync::record_normal_mode_omission(
                &mut stats.metadata_report,
                pending.path,
                reason,
            );
        } else if !pending.is_new {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
        }
    }

    if safety.content_sync_permit().is_some() && dst.supports_mtime_write() {
        // Portable Ordner haben kein natives Manifest. Ihre bereits bestätigte
        // Zeit wird trotzdem erst nach dem letzten Kindcommit gesetzt.
        for action in plan.actions.iter().rev() {
            let Action::CreateDir {
                path,
                source_mtime: Some(expected),
                metadata: None,
                ..
            } = action
            else {
                continue;
            };
            if cancel.is_cancelled() {
                return finish_cancelled(stats, &mut prog, on_progress);
            }
            prog.phase = "metadata".into();
            prog.current = path.clone();
            on_progress(&prog);
            if let Some(reason) = apply_directory_time(path, *expected, &request)? {
                crate::content_sync::record_mtime_omission(
                    &mut stats.metadata_report,
                    path,
                    reason,
                );
            }
        }
    }

    // Phase 3: Native Ordner-Metadaten ebenfalls post-order anwenden. Owner kommt
    // vor xattrs und Rechten; Sonderbits folgen erst nach dem ersten Datenträger-Sync.
    let mut pending_directories = Vec::new();
    for action in &plan.actions {
        match action {
            Action::CreateDir {
                path,
                source_mtime,
                metadata: Some(metadata),
                ..
            } if !early_completed_directories.contains(path) => {
                pending_directories.push(PendingDirectoryMetadata {
                    path,
                    source_mtime: *source_mtime,
                    target_mtime: None,
                    metadata,
                    is_new: true,
                })
            }
            Action::SetDirectoryMetadata {
                path,
                source_mtime,
                target_mtime,
                metadata,
            } => pending_directories.push(PendingDirectoryMetadata {
                path,
                source_mtime: *source_mtime,
                target_mtime: *target_mtime,
                metadata,
                is_new: false,
            }),
            _ => {}
        }
    }
    pending_directories.sort_by_key(|item| std::cmp::Reverse(item.path.matches('/').count()));

    let mut updated_directories = HashSet::new();
    for pending in pending_directories {
        if cancel.is_cancelled() {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        prog.phase = "metadata".into();
        prog.current = pending.path.to_string();
        on_progress(&prog);
        if !dry {
            validate_anchor_identity(src, dst, safety)?;
            let source_path = checked_join(src_root, pending.path)?;
            let target_path = checked_join(dst_root, pending.path)?;
            guard_path(safety.sandbox(), &target_path, "Metadaten-Ziel")?;
            let projected = super::directory_streams::project_completed_streams(pending.metadata)?;
            let (omitted_mtime, omitted_mode) = apply_directory_metadata(
                DirectoryMetadataRequest {
                    src,
                    source_path: &source_path,
                    dst,
                    target_path: &target_path,
                    source_mtime: pending.source_mtime,
                    target_mtime: pending.target_mtime,
                    metadata: &projected,
                    is_new: pending.is_new,
                    permit: safety.content_sync_permit(),
                    created_key: before_children_keys.get(pending.path),
                    created_dacl_applied: before_children_keys.get(pending.path).is_some_and(
                        |key| {
                            stats
                                .created_directory_dacl_report
                                .completed
                                .iter()
                                .any(|effect| {
                                    effect.origin == key.origin
                                        && effect.object_namespace == key.object_namespace
                                        && effect.file_id == key.file_id
                                        && effect.directory == key.directory
                                })
                        },
                    ),
                },
                &DirectoryMtime::Pending,
            )?;
            if pending.metadata.directory_streams.is_some()
                && dst.metadata_manifest(&target_path)?.named_streams
                    != pending.metadata.source.named_streams
            {
                return Err(crate::coded_error(
                    io::ErrorKind::InvalidData,
                    crate::ErrorCode::OutcomeUnclear,
                    "Ordnerstrommenge änderte sich nach dem Metadatenabschluss",
                ));
            }
            let changed = pending.metadata.directory_streams.is_some()
                || (pending.metadata.normal_mode && omitted_mode.is_none())
                || pending.metadata.owner
                || (pending.metadata.acl
                    && !matches!(
                        pending.metadata.windows_dacl,
                        Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(_))
                    ))
                || pending.metadata.xattrs
                || pending.metadata.special_mode_bits
                || (dst.supports_mtime_write()
                    && omitted_mtime.is_none()
                    && pending.source_mtime != pending.target_mtime);
            if omitted_mtime.is_none()
                && omitted_mode.is_none()
                && pending.source_mtime.is_some()
                && dst.supports_mtime_write()
            {
                if let Some(key) = before_children_keys.get(pending.path) {
                    stats.created_directory_dacl_report.observe_created(
                        crate::created_directory_dacl::CreatedDirectoryObservation {
                            effect_key: key.clone(),
                            ordinary_metadata_applied: true,
                        },
                    );
                    prog.created_directory_dacl_report =
                        stats.created_directory_dacl_report.as_ref().clone();
                    on_progress(&prog);
                }
            }
            if let Some(reason) = omitted_mtime {
                crate::content_sync::record_mtime_omission(
                    &mut stats.metadata_report,
                    pending.path,
                    reason,
                );
            }
            if let Some(reason) = omitted_mode {
                crate::content_sync::record_normal_mode_omission(
                    &mut stats.metadata_report,
                    pending.path,
                    reason,
                );
            }
            if !pending.is_new && changed {
                stats.metadata_updated = stats.metadata_updated.saturating_add(1);
                updated_directories.insert(pending.path);
            }
        } else if !pending.is_new {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
        }
    }

    // Datei-DACLs ändern bestehende Objekte; niemals copy_one oder Temp-Rename.
    for action in &plan.actions {
        let Action::SetFileDacl {
            path,
            source_hash,
            source_metadata,
            plan: effect,
        } = action
        else {
            continue;
        };
        if cancel.is_cancelled() {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        if dry {
            continue;
        }
        validate_anchor_identity(src, dst, safety)?;
        for alias in &effect.alias_scope.selected_paths {
            let source_path = checked_join(src_root, alias)?;
            if hash_file_hex(src, &source_path)? != *source_hash {
                return Err(plan_changed(alias, "Datei-DACL-Quellinhalt änderte sich"));
            }
            let expected = plan
                .file_dacl_selections
                .iter()
                .find(|selection| selection.path == *alias)
                .and_then(|selection| selection.source_metadata.as_ref())
                .unwrap_or(source_metadata);
            crate::metadata::verify_metadata_manifest(src, &source_path, expected)?;
        }
        let target_path = checked_join(dst_root, path)?;
        guard_path(safety.sandbox(), &target_path, "Datei-DACL-Ziel")?;
        prog.phase = "metadata".into();
        prog.current = path.clone();
        let mut transaction = match dst.begin_file_dacl_transaction(&target_path, effect) {
            Ok(transaction) => transaction,
            Err(error) => {
                if let Some(issue) = crate::windows_dacl::known_write_omission(&error) {
                    crate::content_sync::record_windows_dacl_omission(
                        &mut stats.metadata_report,
                        path,
                        issue,
                    );
                    on_progress(&prog);
                    continue;
                }
                return Err(error);
            }
        };
        let applied = transaction.apply();
        stats.file_dacl_report.merge(&transaction.report());
        if let Ok(report) = &applied {
            stats.file_dacl_report.merge(report);
        }
        if transaction
            .report()
            .completed
            .iter()
            .any(|outcome| outcome.file == target_path.to_string_lossy())
        {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
        }
        prog.file_dacl_report = stats.file_dacl_report.clone();
        on_progress(&prog);
        let paths = transaction.recovery_paths();
        if let Err(error) = applied {
            if transaction.report().is_empty() {
                if let Some(issue) = crate::windows_dacl::known_write_omission(&error) {
                    crate::content_sync::record_windows_dacl_omission(
                        &mut stats.metadata_report,
                        path,
                        issue,
                    );
                    continue;
                }
            }
            stats.temp_recovery_paths.extend(paths);
            return Err(error);
        }
        if cancel.is_cancelled() {
            stats.temp_recovery_paths.extend(paths);
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        match transaction.finish() {
            Ok(report) => stats.file_dacl_report.merge(&report),
            Err(error) => {
                stats.temp_recovery_paths.extend(paths);
                return Err(error);
            }
        }
        prog.file_dacl_report = stats.file_dacl_report.clone();
        on_progress(&prog);
    }

    // Die dauerhaft gebundene DACL ist die letzte Wirkung: auch alle mtime- und
    // Streamabschlüsse anderer Ordner müssen davor bereits abgeschlossen sein.
    for action in &plan.actions {
        let Action::SetDirectoryMetadata {
            path,
            source_mtime,
            metadata,
            ..
        } = action
        else {
            continue;
        };
        let Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(dacl)) =
            &metadata.windows_dacl
        else {
            continue;
        };
        if cancel.is_cancelled() {
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        if dry {
            continue;
        }
        validate_anchor_identity(src, dst, safety)?;
        let source_path = checked_join(src_root, path)?;
        let target_path = checked_join(dst_root, path)?;
        validate_directory_source(src, &source_path, *source_mtime, metadata)?;
        prog.phase = "metadata".into();
        prog.current = path.clone();
        let mut transaction = match dst.begin_directory_dacl_transaction(&target_path, dacl) {
            Ok(transaction) => transaction,
            Err(error) => {
                if let Some(issue) = crate::windows_dacl::known_write_omission(&error) {
                    crate::content_sync::record_windows_dacl_omission(
                        &mut stats.metadata_report,
                        path,
                        issue,
                    );
                    on_progress(&prog);
                    continue;
                }
                return Err(error);
            }
        };
        let applied = transaction.apply();
        stats.directory_dacl_report.merge(
            &transaction
                .report()
                .at_directory(&target_path.to_string_lossy()),
        );
        if let Ok(report) = &applied {
            stats
                .directory_dacl_report
                .merge(&report.clone().at_directory(&target_path.to_string_lossy()));
        }
        if stats
            .directory_dacl_report
            .completed
            .iter()
            .any(|outcome| outcome.directory == target_path.to_string_lossy())
            && updated_directories.insert(path.as_str())
        {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
        }
        prog.directory_dacl_report = stats.directory_dacl_report.clone();
        // Der synchrone Queuecallback sichert den Wirkungsbeleg vor dem Intentabbau.
        on_progress(&prog);
        let recovery_paths = transaction.recovery_paths();
        if let Err(error) = applied {
            stats.temp_recovery_paths.extend(recovery_paths);
            if transaction.report().is_empty() {
                if let Some(issue) = crate::windows_dacl::known_write_omission(&error) {
                    crate::content_sync::record_windows_dacl_omission(
                        &mut stats.metadata_report,
                        path,
                        issue,
                    );
                    continue;
                }
            }
            return Err(error);
        }
        if cancel.is_cancelled() {
            stats.temp_recovery_paths.extend(recovery_paths);
            return finish_cancelled(stats, &mut prog, on_progress);
        }
        match transaction.finish() {
            Ok(report) => {
                stats
                    .directory_dacl_report
                    .merge(&report.at_directory(&target_path.to_string_lossy()));
            }
            Err(error) => {
                stats.temp_recovery_paths.extend(recovery_paths);
                return Err(error);
            }
        }
        prog.directory_dacl_report = stats.directory_dacl_report.clone();
        on_progress(&prog);
    }

    prog.phase = "done".into();
    prog.current.clear();
    on_progress(&prog);
    if let Some(run_root) = quarantine_run {
        validate_anchor_identity(src, dst, safety)?;
        let binding = quarantine_binding.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quarantäneabschluss besitzt keine Laufbindung",
            )
        })?;
        verify_sync_quarantine_run(dst, run_root, binding)?;
        complete_quarantine_run(dst, run_root)?;
        verify_sync_quarantine_run(dst, run_root, binding)?;
    }
    Ok(())
}

fn apply_pending_mode(
    pending: &PendingMode<'_>,
    request: &ExecuteRequest<'_>,
) -> io::Result<Option<String>> {
    let PendingMode {
        path,
        source_mode,
        target_mode,
        is_dir,
        is_new,
        content_hash,
    } = *pending;
    let ExecuteRequest {
        src,
        src_root,
        dst,
        dst_root,
        safety,
        ..
    } = *request;
    let dry = safety.is_dry_run();
    // Der Planer stellt sicher, dass `SetMode` nur für ein Ziel entsteht, das
    // Rechte auch schreiben kann. Bleibt hier trotzdem eine solche Aktion übrig,
    // stammt sie aus einem persistierten Plan mit anderer Zielfähigkeit. Sie
    // still zu überspringen wäre die schlechteste Antwort: Die Vorschau
    // verspräche eine Anpassung, `stats.metadata_updated` bliebe null, der
    // Folgeplan würde nie leer, und ein Mirror-Zweitscan bräche nach den Kopien
    // ab, weil er die unerledigte Aktion als fremde Änderung liest.
    if !dst.supports_mode_write_at(dst_root)? {
        return Err(plan_changed(
            path,
            "Ziel kann Rechte nicht schreiben; der Plan stammt aus einer anderen Zielfähigkeit",
        ));
    }
    if !dry {
        validate_anchor_identity(src, dst, safety)?;
        let source = src.stat(&checked_join(src_root, path)?)?;
        if source.is_symlink || source.is_dir != is_dir || source.mode != Some(source_mode) {
            return Err(plan_changed(
                path,
                "Quellrechte änderten sich seit der Vorschau",
            ));
        }

        let target_path = checked_join(dst_root, path)?;
        let target = dst.stat(&target_path)?;
        if target.is_symlink || target.is_dir != is_dir {
            return Err(plan_changed(path, "Metadaten-Ziel änderte seinen Typ"));
        }
        if !is_new && target.mode != target_mode {
            return Err(plan_changed(
                path,
                "Zielrechte änderten sich seit der Vorschau",
            ));
        }
        if let Some(expected_hash) = content_hash {
            let source_hash = hash_file_hex(src, &checked_join(src_root, path)?)?;
            let target_hash = hash_file_hex(dst, &target_path)?;
            if source_hash != expected_hash || target_hash != expected_hash {
                return Err(plan_changed(
                    path,
                    "Dateiinhalt änderte sich seit der Hash-Vorschau",
                ));
            }
        }
        guard_path(safety.sandbox(), &target_path, "Metadaten-Ziel")?;
        if let Err(error) = dst.set_mode(&target_path, source_mode) {
            if let Some(reason) = safety.normal_mode_omission_reason(&error) {
                return Ok(Some(reason.to_owned()));
            }
            return Err(error);
        }
        // Die Rechte sind bereits sichtbar geändert. Ein Fehler ab hier darf weder
        // Erfolg noch eine wirkungslose Absage behaupten; das Ziel bleibt als
        // strukturierter Prüfpfad erhalten, auch wenn der Provider weitere Reste meldet.
        let unconfirmed = |error: io::Error| {
            crate::error::with_recovery_context(
                crate::coded_error(
                    error.kind(),
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Rechte von {} wurden gesetzt, aber nicht dauerhaft bestätigt: {error}",
                        target_path.display()
                    ),
                ),
                &[&error],
                &[&target_path],
            )
        };
        if is_dir {
            dst.sync_dir(&target_path).map_err(&unconfirmed)?;
        } else {
            dst.sync_file(&target_path).map_err(&unconfirmed)?;
        }
        validate_anchor_identity(src, dst, safety).map_err(&unconfirmed)?;
        let actual = dst.stat(&target_path).map_err(&unconfirmed)?;
        if actual.is_symlink || actual.is_dir != is_dir || actual.mode != Some(source_mode) {
            return Err(unconfirmed(io::Error::new(
                io::ErrorKind::InvalidData,
                "Typ oder Rechte weichen beim Rücklesen vom bestätigten Ziel ab",
            )));
        }
    }
    Ok(None)
}

fn apply_directory_time(
    path: &str,
    expected: i64,
    request: &ExecuteRequest<'_>,
) -> io::Result<Option<crate::content_sync::KnownMtimeOmission>> {
    if request.safety.is_dry_run() {
        return Ok(None);
    }
    validate_anchor_identity(request.src, request.dst, request.safety)?;
    let source_path = checked_join(request.src_root, path)?;
    let target_path = checked_join(request.dst_root, path)?;
    let source = request.src.stat(&source_path)?;
    let target = request.dst.stat(&target_path)?;
    if source.is_symlink
        || !source.is_dir
        || source.mtime != Some(expected)
        || target.is_symlink
        || !target.is_dir
    {
        return Err(plan_changed(
            path,
            "Ordner oder Quellzeit änderte sich vor der Zeitübernahme",
        ));
    }
    guard_path(request.safety.sandbox(), &target_path, "Ordnerzeit-Ziel")?;
    if let Err(error) = request.dst.set_mtime(&target_path, expected) {
        if let Some(reason) = request.safety.mtime_omission_reason(&error) {
            return Ok(Some(reason.to_owned()));
        }
        return Err(error);
    }
    let unconfirmed = |error: io::Error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Ordnerzeit von {} wurde gesetzt, aber ihr Abschluss ist unklar: {error}",
                    target_path.display()
                ),
            ),
            &[&error],
            &[&target_path],
        )
    };
    request.dst.sync_dir(&target_path).map_err(&unconfirmed)?;
    validate_anchor_identity(request.src, request.dst, request.safety).map_err(&unconfirmed)?;
    let actual = request.dst.stat(&target_path).map_err(&unconfirmed)?;
    if actual.is_symlink || !actual.is_dir || actual.mtime != Some(expected) {
        return Err(unconfirmed(io::Error::new(
            io::ErrorKind::InvalidData,
            "Ordnerzeit weicht beim Rücklesen ab",
        )));
    }
    Ok(None)
}

fn preflight_hardlinks(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    plan: &SyncPlan,
) -> io::Result<()> {
    for action in &plan.actions {
        let Action::CreateHardlink {
            path,
            link_to,
            size,
            source_mtime,
            source_mode,
            source_hash,
            target_size,
            target_mtime,
            target_mode,
            target_proof,
            target_hash,
            metadata,
        } = action
        else {
            continue;
        };
        let source_path = checked_join(src_root, path)?;
        let anchor_path = checked_join(src_root, link_to)?;
        let source = src.stat(&source_path)?;
        let anchor = src.stat(&anchor_path)?;
        if regular_copy_snapshot(src, &source_path)? != (*size, *source_mtime, *source_mode)
            || source.device.is_none()
            || (source.device, source.inode) != (anchor.device, anchor.inode)
            || hash_file_hex(src, &source_path)? != *source_hash
        {
            return Err(plan_changed(
                path,
                "Quell-Hardlink-Gruppe änderte sich seit der Vorschau",
            ));
        }
        let target_path = checked_join(dst_root, path)?;
        let expected_target = target_size.map(|size| (size, *target_mtime, *target_mode));
        if current_optional_copy_snapshot(dst, &target_path)? != expected_target {
            return Err(plan_changed(
                path,
                "Hardlink-Ziel änderte sich seit der Vorschau",
            ));
        }
        super::atomic::verify_target_proof(dst, &target_path, target_proof.as_ref())?;
        verify_planned_metadata(src, &source_path, dst, &target_path, Some(metadata))?;
        if let Some(expected_hash) = target_hash {
            if hash_file_hex(dst, &target_path)? != *expected_hash {
                return Err(plan_changed(
                    path,
                    "Hardlink-Zielinhalt änderte sich seit der Vorschau",
                ));
            }
        }
    }
    Ok(())
}

/// Prüft alle nativen Ordneraktionen, bevor der Lauf irgendein Ziel verändert. Spätere
/// Kindanlagen ändern die Ordner-mtime absichtlich; deshalb muss gerade dieser Wert hier
/// an den bestätigten Ausgangszustand gebunden werden.
fn preflight_directory_metadata(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    plan: &SyncPlan,
) -> io::Result<()> {
    for action in &plan.actions {
        match action {
            Action::CreateDir {
                path,
                source_mtime,
                metadata: Some(metadata),
                ..
            } => {
                validate_directory_source(
                    src,
                    &checked_join(src_root, path)?,
                    *source_mtime,
                    metadata,
                )?;
                if optional_stat(dst, &checked_join(dst_root, path)?)?.is_some() {
                    return Err(plan_changed(path, "Zielordner existiert inzwischen"));
                }
            }
            Action::SetDirectoryMetadata {
                path,
                source_mtime,
                target_mtime,
                metadata,
            } => {
                let source_path = checked_join(src_root, path)?;
                let target_path = checked_join(dst_root, path)?;
                validate_directory_source(src, &source_path, *source_mtime, metadata)?;
                let target_entry = dst.stat(&target_path)?;
                if target_entry.is_symlink || !target_entry.is_dir {
                    return Err(plan_changed(path, "Metadaten-Ziel ist kein echter Ordner"));
                }
                if target_entry.mtime != *target_mtime {
                    return Err(plan_changed(
                        path,
                        "Zielordner-Zeit änderte sich seit der Vorschau",
                    ));
                }
                verify_planned_metadata(src, &source_path, dst, &target_path, Some(metadata))?;
                if let Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(dacl)) =
                    &metadata.windows_dacl
                {
                    super::atomic::verify_target_proof(
                        dst,
                        &target_path,
                        dacl.target_proof.as_ref(),
                    )?;
                    match dst.directory_dacl_write_support()? {
                        crate::windows_dacl::WindowsDaclWriteSupport::Ready(current)
                            if current == dacl.context => {}
                        _ => {
                            return Err(plan_changed(
                                path,
                                "Verzeichnis-DACL-Probekontext änderte sich seit der Vorschau",
                            ))
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_directory_metadata_transfer(metadata: &MetadataTransfer) -> io::Result<()> {
    validate_metadata_transfer(metadata)?;
    if matches!(
        metadata.windows_dacl,
        Some(crate::windows_dacl::WindowsDaclTransfer::Exact(_))
    ) {
        return Err(invalid_plan(
            "Verzeichnis-DACL besitzt keinen bestätigten Einzelwirkungsvertrag",
        ));
    }
    Ok(())
}

pub(super) fn validate_directory_source(
    src: &dyn Vfs,
    source_path: &Path,
    source_mtime: Option<i64>,
    metadata: &MetadataTransfer,
) -> io::Result<()> {
    validate_directory_metadata_transfer(metadata)?;
    if metadata.resource_fork || metadata.named_streams {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Ordneraktion darf keine Metadaten-Inhaltsströme enthalten",
        ));
    }
    let source = src.stat(source_path)?;
    if source.is_symlink || !source.is_dir || source.mtime != source_mtime {
        return Err(plan_changed(
            &source_path.to_string_lossy(),
            "Quellordner änderte sich seit der Vorschau",
        ));
    }
    crate::metadata::verify_metadata_manifest(src, source_path, &metadata.source)
}

struct DirectoryMetadataRequest<'a> {
    src: &'a dyn Vfs,
    source_path: &'a Path,
    dst: &'a dyn Vfs,
    target_path: &'a Path,
    source_mtime: Option<i64>,
    target_mtime: Option<i64>,
    metadata: &'a MetadataTransfer,
    is_new: bool,
    permit: Option<&'a crate::content_sync::ContentSyncPermit>,
    created_key: Option<&'a crate::created_directory_dacl::CreatedDirectoryEffectKey>,
    created_dacl_applied: bool,
}

#[derive(Clone)]
enum DirectoryMtime {
    Pending,
    Applied,
    Omitted(crate::content_sync::KnownMtimeOmission),
}

fn apply_directory_metadata(
    request: DirectoryMetadataRequest<'_>,
    mtime_state: &DirectoryMtime,
) -> io::Result<(
    Option<crate::content_sync::KnownMtimeOmission>,
    Option<String>,
)> {
    let DirectoryMetadataRequest {
        src,
        source_path,
        dst,
        target_path,
        source_mtime,
        target_mtime: _target_mtime,
        metadata,
        is_new,
        permit,
        created_key,
        created_dacl_applied,
    } = request;
    let allow_metadata_omission =
        permit.is_some_and(crate::content_sync::ContentSyncPermit::allows_metadata_omission);
    let mut omitted_mtime = match mtime_state {
        DirectoryMtime::Omitted(omission) => Some(omission.clone()),
        _ => None,
    };
    let mut omitted_mode = None;
    validate_directory_source(src, source_path, source_mtime, metadata)?;
    let target = dst.stat(target_path)?;
    if target.is_symlink || !target.is_dir {
        return Err(plan_changed(
            &target_path.to_string_lossy(),
            "Metadaten-Ziel ist kein echter Ordner",
        ));
    }
    if created_key.is_some_and(|key| {
        target.inode != Some(key.file_id) || key.directory != target_path.to_string_lossy()
    }) {
        return Err(plan_changed(
            &target_path.to_string_lossy(),
            "Neuordneridentität änderte sich vor finalen Metadaten",
        ));
    }
    if created_dacl_applied {
        verify_before_children_rights(dst, target_path, metadata, false)?;
    }
    if is_new {
        if metadata.target.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Neuer Ordner enthält unerwartet ein Zielmanifest",
            ));
        }
    } else {
        verify_planned_metadata(src, source_path, dst, target_path, Some(metadata))?;
    }

    // Alle Quelldaten vor der ersten Zielmutation lesen und danach das Quellmanifest
    // erneut prüfen. So kann keine xattr-Drift mit einem alten Plan angewandt werden.
    let source_xattrs = if metadata.xattrs {
        let values = src.read_xattrs(source_path)?;
        if summarize_xattrs(&values) != metadata.source.xattrs {
            return Err(plan_changed(
                &source_path.to_string_lossy(),
                "Quell-xattrs änderten sich seit der Vorschau",
            ));
        }
        Some(values)
    } else {
        None
    };
    validate_directory_source(src, source_path, source_mtime, metadata)?;

    if metadata.owner {
        let owner = metadata.source.owner.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Owner-Transfer benötigt numerischen Nutzer und Gruppe",
            )
        })?;
        dst.set_metadata_owner(target_path, owner)?;
    }
    let directory_dacl = matches!(
        metadata.windows_dacl,
        Some(
            crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(_)
                | crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryExact(_)
                | crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(_)
        )
    );
    if metadata.acl && !directory_dacl {
        dst.write_acl_exact(target_path, metadata.source.acl.as_ref())?;
    }
    if let Some(values) = source_xattrs.as_deref() {
        dst.write_xattrs_exact(target_path, values)?;
    }
    if metadata.normal_mode {
        if let Err(error) = dst.set_metadata_normal_mode(target_path, metadata.source.mode & 0o777)
        {
            if let Some(reason) = allow_metadata_omission
                .then(|| crate::content_sync::normal_mode_omission_reason(&error))
                .flatten()
            {
                omitted_mode = Some(reason.to_owned());
            } else {
                return Err(error);
            }
        }
    }
    if matches!(mtime_state, DirectoryMtime::Pending) && dst.supports_mtime_write() {
        if let Some(mtime) = source_mtime {
            let result = match created_key {
                Some(key) => dst.set_created_directory_mtime(target_path, mtime, key),
                None => dst.set_mtime(target_path, mtime),
            };
            if let Err(error) = result {
                if let Some(reason) = permit.and_then(|permit| permit.mtime_omission(&error)) {
                    omitted_mtime = Some(reason.to_owned());
                } else {
                    return Err(error);
                }
            }
        }
    }
    dst.sync_dir(target_path)?;
    if metadata.special_mode_bits {
        dst.set_metadata_special_bits(target_path, metadata.source.mode & 0o7000)?;
        dst.sync_dir(target_path)?;
    }
    let mut verified_metadata = metadata.clone();
    if directory_dacl {
        verified_metadata.acl = false;
        verified_metadata.windows_dacl = None;
    }
    if omitted_mode.is_some() {
        verified_metadata.normal_mode = false;
    }
    verify_selected_metadata(dst, target_path, &verified_metadata)?;
    if created_dacl_applied {
        verify_before_children_rights(dst, target_path, metadata, true)?;
    }
    if let Some(key) = created_key {
        let final_entry = dst.stat(target_path)?;
        if final_entry.inode != Some(key.file_id) || !final_entry.is_dir || final_entry.is_symlink {
            return Err(crate::coded_error(
                io::ErrorKind::InvalidData,
                crate::ErrorCode::OutcomeUnclear,
                "Neuordneridentität nach Metadatenabschluss unklar",
            ));
        }
    }
    if omitted_mtime.is_none()
        && dst.supports_mtime_write()
        && source_mtime.is_some()
        && dst.stat(target_path)?.mtime != source_mtime
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Ordner-mtime stimmt nach dem Schreiben nicht exakt überein",
        ));
    }
    Ok((omitted_mtime, omitted_mode))
}

fn verify_before_children_rights(
    dst: &dyn Vfs,
    path: &Path,
    metadata: &MetadataTransfer,
    after_mutation: bool,
) -> io::Result<()> {
    let Some(crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(
        selection,
    )) = &metadata.windows_dacl
    else {
        return Err(invalid_plan("V8-Rechteabschluss ohne V8-Auswahl"));
    };
    let observed = dst.metadata_manifest(path)?;
    let context_matches = observed.windows_security.as_ref().is_some_and(|security| {
        security.issue.is_none()
            && security.namespace == selection.probe_context.namespace
            && security.owner.as_ref() == Some(&selection.probe_context.owner)
            && security.group == selection.probe_context.group
    });
    if observed.acl != Some(crate::metadata::Acl::WindowsDacl(selection.after.clone()))
        || !context_matches
    {
        return Err(crate::coded_error(
            io::ErrorKind::InvalidData,
            if after_mutation {
                crate::ErrorCode::OutcomeUnclear
            } else {
                crate::ErrorCode::PlanDrift
            },
            "Früh bestätigte Ordnerrechte oder SID-Kontext änderten sich vor dem Gesamtabschluss",
        ));
    }
    Ok(())
}

/// Einheitliches Abbruchende, damit Statistik und letztes Fortschritts-Event nicht
/// auseinanderlaufen.
fn finish_cancelled(
    stats: &mut SyncStats,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<()> {
    stats.cancelled = true;
    prog.phase = "cancelled".into();
    on_progress(prog);
    Ok(())
}

fn preflight_file_dacl(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    plan: &SyncPlan,
    safety: &Safety,
) -> io::Result<()> {
    let mut objects = std::collections::BTreeSet::new();
    for action in &plan.actions {
        let Action::SetFileDacl {
            path,
            source_hash,
            source_metadata,
            plan: effect,
        } = action
        else {
            continue;
        };
        if !safety.content_sync_permit().is_some_and(|permit| {
            permit.metadata_profile() == crate::MetadataProfile::NativeBestEffort
        }) {
            return Err(invalid_plan("Datei-DACL-Inplace ist ausschließlich für bestätigten NativeBestEffort-Inhaltssync zulässig"));
        }
        if !objects.insert((effect.object_namespace, effect.file_id)) {
            return Err(invalid_plan("Datei-DACL-Objekt wurde mehrfach geplant"));
        }
        for alias in &effect.alias_scope.selected_paths {
            let source_path = checked_join(src_root, alias)?;
            let target_path = checked_join(dst_root, alias)?;
            if hash_file_hex(src, &source_path)? != *source_hash
                || !effect
                    .target_proof
                    .matches_identity(&dst.stat(&target_path)?)
                || hash_file_hex(dst, &target_path)? != *source_hash
            {
                return Err(plan_changed(
                    alias,
                    "Datei-DACL-Inhalt oder Objekt änderte sich seit der Vorschau",
                ));
            }
            let expected = plan
                .file_dacl_selections
                .iter()
                .find(|selection| selection.path == *alias)
                .and_then(|selection| selection.source_metadata.as_ref())
                .unwrap_or(source_metadata);
            crate::metadata::verify_metadata_manifest(src, &source_path, expected)?;
            let observed = dst.metadata_manifest(&target_path)?;
            if observed.acl.as_ref()
                != Some(&crate::metadata::Acl::WindowsDacl(effect.before.clone()))
                || observed.named_streams.as_ref() != Some(&effect.streams)
            {
                return Err(plan_changed(
                    alias,
                    "Datei-DACL oder ADS änderten sich seit der Vorschau",
                ));
            }
        }
        if !effect.alias_scope.selected_paths.contains(path) {
            return Err(invalid_plan("Datei-DACL-Hauptpfad fehlt im Aliasumfang"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CreatedSourceOmission {
    ListingDenied,
    ListingUnknown,
    StreamsDenied,
    StreamsUnknown,
}
impl CreatedSourceOmission {
    fn record(self, report: &mut super::MetadataReport, path: &str) {
        self.record_for(report, path, false);
    }
    fn record_before_children(self, report: &mut super::MetadataReport, path: &str) {
        self.record_for(report, path, true);
    }
    fn record_for(self, report: &mut super::MetadataReport, path: &str, before_children: bool) {
        let denied = matches!(self, Self::ListingDenied | Self::StreamsDenied);
        crate::content_sync::record_windows_dacl_omission(
            report,
            path,
            if denied {
                crate::windows_dacl::WindowsDaclTargetIssue::RequiredAccessDenied
            } else {
                crate::windows_dacl::WindowsDaclTargetIssue::OptionalObservation
            },
        );
        if let Some(issue) = report.issues.last_mut() {
            issue.reason = match (self, before_children) {
            (Self::ListingDenied, true) => "Die zusätzliche ungefilterte Unterbestandsabfrage des Quellordners wurde verweigert; nur die Rechteänderung wird ausgelassen",
            (Self::ListingUnknown, true) => "Die zusätzliche vollständige Beobachtung des direkten Quellunterbestands ist nicht verfügbar; nur die Rechteänderung wird ausgelassen",
            (Self::ListingDenied, false) => "Die zusätzliche Leerheitsabfrage des Quellordners wurde verweigert; nur die Rechteänderung wird ausgelassen",
            (Self::ListingUnknown, false) => "Die zusätzliche vollständige Leerheitsbeobachtung des Quellordners ist nicht verfügbar; nur die Rechteänderung wird ausgelassen",
            (Self::StreamsDenied, _) => "Die zusätzliche Beobachtung der Quell-Ordnerdatenströme wurde verweigert; nur die Rechteänderung wird ausgelassen",
            (Self::StreamsUnknown, _) => "Die zusätzliche vollständige Quellmetadaten- und Ordnerstrombeobachtung ist nicht verfügbar; nur die Rechteänderung wird ausgelassen",
        }.into();
        }
    }
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreatedSourceFault {
    ListingDenied,
    ListingIncomplete,
    StreamsDenied,
    KnownStream,
    Interrupted,
}
#[cfg(test)]
thread_local! {pub(crate) static CREATED_SOURCE_FAULT:std::cell::Cell<Option<(u8,CreatedSourceFault)>>=const{std::cell::Cell::new(None)};pub(crate) static CREATED_SOURCE_CHECKS:std::cell::Cell<u8>=const{std::cell::Cell::new(0)};}
pub(super) fn validate_created_nonempty_source(
    src: &dyn Vfs,
    path: &Path,
    mtime: Option<i64>,
    metadata: &MetadataTransfer,
    proof: &crate::created_directory_dacl::CreatedDirectoryChildrenProof,
) -> io::Result<Option<CreatedSourceOmission>> {
    validate_created_source(src, path, mtime, metadata, Some(proof))
}
pub(super) fn validate_created_empty_source(
    src: &dyn Vfs,
    path: &Path,
    mtime: Option<i64>,
    metadata: &MetadataTransfer,
) -> io::Result<Option<CreatedSourceOmission>> {
    validate_created_source(src, path, mtime, metadata, None)
}
fn validate_created_source(
    src: &dyn Vfs,
    path: &Path,
    mtime: Option<i64>,
    metadata: &MetadataTransfer,
    children: Option<&crate::created_directory_dacl::CreatedDirectoryChildrenProof>,
) -> io::Result<Option<CreatedSourceOmission>> {
    // Die bestehenden Source-Identitäts-/Manifestprüfungen bleiben zwingend.
    validate_directory_source(src, path, mtime, metadata)?;
    #[cfg(test)]
    let fault = {
        let count = CREATED_SOURCE_CHECKS.with(|count| {
            count.set(count.get().saturating_add(1));
            count.get()
        });
        CREATED_SOURCE_FAULT.with(|fault| {
            if fault.get().is_some_and(|(stage, _)| stage == count) {
                fault.take().map(|(_, fault)| fault)
            } else {
                None
            }
        })
    };
    #[cfg(test)]
    if fault == Some(CreatedSourceFault::Interrupted) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Zusätzliche Beobachtung abgebrochen",
        ));
    }
    let listing = src.list_complete(path);
    #[cfg(test)]
    let listing = match fault {
        Some(CreatedSourceFault::ListingDenied) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Synthetische zusätzliche Listingabsage",
        )),
        Some(CreatedSourceFault::ListingIncomplete) => listing.map(|mut listing| {
            listing.complete = false;
            listing
        }),
        _ => listing,
    };
    let listing = match listing {
        Ok(listing) => listing,
        Err(error)
            if matches!(
                crate::error_code(&error),
                crate::ErrorCode::PlanDrift | crate::ErrorCode::Interrupted
            ) =>
        {
            return Err(error)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Err(crate::real_mode::plan_drift(
                "Quellordner verschwand während der zusätzlichen Leerheitsprüfung",
            ))
        }
        Err(error) => {
            return Ok(Some(if error.kind() == io::ErrorKind::PermissionDenied {
                CreatedSourceOmission::ListingDenied
            } else {
                CreatedSourceOmission::ListingUnknown
            }))
        }
    };
    if children.is_none() && !listing.entries.is_empty() {
        return Err(crate::real_mode::plan_drift(
            "Quellordner besitzt inzwischen bekannte Kinder",
        ));
    }
    if !listing.complete {
        return Ok(Some(CreatedSourceOmission::ListingUnknown));
    }
    if let Some(expected) = children {
        let actual =
            crate::created_directory_dacl::CreatedDirectoryChildrenProof::from_listing(&listing)
                .map_err(|_| {
                    crate::real_mode::plan_drift("Ungefilterter Quellunterbestand änderte sich")
                })?;
        if &actual != expected {
            return Err(crate::real_mode::plan_drift(
                "Rohe Quellnamen oder Kindtypen änderten sich",
            ));
        }
    }
    let observed = src.metadata_manifest(path);
    #[cfg(test)]
    let observed = match fault {
        Some(CreatedSourceFault::StreamsDenied) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Synthetische zusätzliche Streamabsage",
        )),
        Some(CreatedSourceFault::KnownStream) => observed.and_then(|m| {
            m.with_named_streams(Some(vec![crate::metadata::NamedBlobSummary {
                name: b"late".to_vec(),
                value: crate::metadata::BlobSummary::from_bytes(b"late"),
            }]))
        }),
        _ => observed,
    };
    let observed = match observed {
        Ok(value) => value,
        Err(error)
            if matches!(
                crate::error_code(&error),
                crate::ErrorCode::PlanDrift | crate::ErrorCode::Interrupted
            ) =>
        {
            return Err(error)
        }
        Err(error) => {
            return Ok(Some(if error.kind() == io::ErrorKind::PermissionDenied {
                CreatedSourceOmission::StreamsDenied
            } else {
                CreatedSourceOmission::StreamsUnknown
            }))
        }
    };
    if observed
        .acl
        .as_ref()
        .zip(metadata.source.acl.as_ref())
        .is_some_and(|(actual, planned)| actual != planned)
        || observed
            .windows_security
            .as_ref()
            .zip(metadata.source.windows_security.as_ref())
            .is_some_and(|(actual, planned)| {
                actual.namespace != planned.namespace
                    || actual
                        .owner
                        .as_ref()
                        .zip(planned.owner.as_ref())
                        .is_some_and(|(actual, planned)| actual != planned)
                    || actual
                        .group
                        .as_ref()
                        .zip(planned.group.as_ref())
                        .is_some_and(|(actual, planned)| actual != planned)
            })
    {
        return Err(crate::real_mode::plan_drift(
            "Bekannte Quellrechte oder SIDs änderten sich während der Zusatzbeobachtung",
        ));
    }
    if observed
        .named_streams
        .as_ref()
        .is_some_and(|streams| !streams.is_empty())
    {
        return Err(crate::real_mode::plan_drift(
            "Quellordner besitzt inzwischen bekannte Ordnerdatenströme",
        ));
    }
    if observed.named_streams.is_none()
        || (metadata.source.acl.is_some() && observed.acl.is_none())
        || observed
            .windows_security
            .as_ref()
            .is_some_and(|security| security.issue.is_some())
    {
        return Ok(Some(CreatedSourceOmission::StreamsUnknown));
    }
    if observed.digest != metadata.source.digest {
        return Err(crate::real_mode::plan_drift(
            "Quellmetadaten änderten sich während der zusätzlichen Leerheitsprüfung",
        ));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use crate::{Compare, LocalVfs, MetadataProfile, SyncMode, SyncOptions, Vfs};

    #[test]
    fn omitted_mtime_skips_only_time_fence_without_another_set() {
        use super::{apply_directory_metadata, DirectoryMetadataRequest, DirectoryMtime};
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let vfs = LocalVfs::new();
        let wanted = 1_700_000_000;
        let original = wanted + 100;
        vfs.set_mtime(&source, wanted).unwrap();
        vfs.set_mtime(&target, original).unwrap();
        let metadata = crate::sync::MetadataTransfer {
            source: vfs.metadata_manifest(&source).unwrap(),
            target: None,
            owner: false,
            acl: false,
            incompatible_acl_model: false,
            windows_dacl: None,
            normal_mode: false,
            special_mode_bits: false,
            xattrs: false,
            resource_fork: false,
            named_streams: false,
            directory_streams: None,
            sparse_extents: false,
            hardlinks: false,
        };
        let request = || DirectoryMetadataRequest {
            src: &vfs,
            source_path: &source,
            dst: &vfs,
            target_path: &target,
            source_mtime: Some(wanted),
            target_mtime: None,
            metadata: &metadata,
            is_new: true,
            permit: None,
            created_key: None,
            created_dacl_applied: false,
        };
        // Eine als gesetzt bestätigte Zeit muss weiterhin exakt stimmen.
        assert!(apply_directory_metadata(request(), &DirectoryMtime::Applied).is_err());
        let error = crate::content_sync::mtime_privilege_omission("Bewiesene OPEN-Absage");
        let omitted = DirectoryMtime::Omitted(
            crate::content_sync::mtime_omission_reason(&error)
                .unwrap()
                .clone(),
        );
        let (mtime, mode) = apply_directory_metadata(request(), &omitted).unwrap();
        assert_eq!(
            mtime.unwrap().kind,
            crate::sync::MetadataIssueKind::PrivilegeFailure
        );
        assert!(mode.is_none());
        assert_eq!(vfs.stat(&target).unwrap().mtime, Some(original));
        // Die ausgelassene Zeit entbindet weder die Quelle noch den Zieltyp.
        vfs.set_mtime(&source, wanted + 1).unwrap();
        assert_eq!(
            crate::error_code(&apply_directory_metadata(request(), &omitted).unwrap_err()),
            crate::ErrorCode::PlanDrift
        );
        vfs.set_mtime(&source, wanted).unwrap();
        std::fs::remove_dir(&target).unwrap();
        std::fs::write(&target, b"fremder Typ").unwrap();
        assert_eq!(
            crate::error_code(&apply_directory_metadata(request(), &omitted).unwrap_err()),
            crate::ErrorCode::PlanDrift
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"fremder Typ");
    }

    #[test]
    fn before_children_final_metadata_rejects_replaced_original_before_time_set() {
        use super::{apply_directory_metadata, DirectoryMetadataRequest, DirectoryMtime};
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let vfs = LocalVfs::new();
        let wanted = 1_700_000_000;
        vfs.set_mtime(&source, wanted).unwrap();
        let original_id = vfs.stat(&target).unwrap().inode.unwrap();
        let key = crate::created_directory_dacl::CreatedDirectoryEffectKey {
            origin: crate::created_directory_dacl::CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren,
            directory: target.to_string_lossy().into_owned(), file_id: original_id, object_namespace: [3; 32],
        };
        let metadata = crate::sync::MetadataTransfer {
            source: vfs.metadata_manifest(&source).unwrap(),
            target: None,
            owner: false,
            acl: false,
            incompatible_acl_model: false,
            windows_dacl: None,
            normal_mode: false,
            special_mode_bits: false,
            xattrs: false,
            resource_fork: false,
            named_streams: false,
            directory_streams: None,
            sparse_extents: false,
            hardlinks: false,
        };
        std::fs::rename(&target, root.join("original-held")).unwrap();
        std::fs::create_dir(&target).unwrap();
        vfs.set_mtime(&target, wanted + 100).unwrap();
        assert_ne!(vfs.stat(&target).unwrap().inode, Some(original_id));
        let error = apply_directory_metadata(
            DirectoryMetadataRequest {
                src: &vfs,
                source_path: &source,
                dst: &vfs,
                target_path: &target,
                source_mtime: Some(wanted),
                target_mtime: None,
                metadata: &metadata,
                is_new: true,
                permit: None,
                created_key: Some(&key),
                created_dacl_applied: false,
            },
            &DirectoryMtime::Pending,
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(vfs.stat(&target).unwrap().mtime, Some(wanted + 100));
        assert!(root.join("original-held").is_dir());
    }

    #[test]
    fn schwache_hardlink_ersetzung_zaehlt_den_tatsaechlichen_abschluss() {
        for replacing in [false, true] {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap();
            let source = root.join("quelle");
            let target = root.join("ziel");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&target).unwrap();
            for directory in [&root, &source, &target] {
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                crate::local_fs::write_acl_exact(directory, None).unwrap();
            }
            std::fs::write(source.join("a"), b"neu").unwrap();
            std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
            if replacing {
                std::fs::write(target.join("a"), b"neu").unwrap();
                std::fs::write(target.join("b"), b"alt").unwrap();
            }
            let vfs = LocalVfs::new();
            let mut paths = vec![source.join("a")];
            if replacing {
                paths.extend([target.join("a"), target.join("b")]);
            }
            for path in paths {
                vfs.set_mtime(&path, 1_700_000_000).unwrap();
            }
            crate::local_fs::with_weak_file_capabilities_for_test(&target, || {
                let registry_root = tempfile::Builder::new()
                    .permissions(
                        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(
                            0o700,
                        ),
                    )
                    .tempdir()
                    .unwrap();
                let registry_queue =
                    crate::OperationQueue::open(registry_root.path().join("operations-queue.json"))
                        .unwrap();
                let prepared = crate::prepare_local_sync_run_for_queue(
                    &registry_queue,
                    &source,
                    &target,
                    SyncOptions {
                        mode: SyncMode::Update,
                        compare: Compare::ContentHash,
                        metadata: MetadataProfile::NativeBestEffort,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
                assert!(prepared
                    .preview()
                    .plan
                    .actions
                    .iter()
                    .any(|action| matches!(
                        action,
                        crate::Action::CreateHardlink {
                            target_size,
                            ..
                        } if target_size.is_some() == replacing
                    )));
                let expected = prepared
                    .preview()
                    .local_execution
                    .as_ref()
                    .unwrap()
                    .irreversible_replacements;
                assert_eq!(expected > 0, replacing);
                let stats = prepared
                    .confirm(true)
                    .unwrap()
                    .execute(&crate::Cancel::new(), &mut |_| {})
                    .unwrap();
                assert_eq!(std::fs::read(target.join("b")).unwrap(), b"neu");
                let a = std::fs::metadata(target.join("a")).unwrap();
                let b = std::fs::metadata(target.join("b")).unwrap();
                assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
                assert!(stats.quarantine.is_none());
                assert_eq!(std::fs::read(source.join("a")).unwrap(), b"neu");
                assert_eq!(std::fs::read(source.join("b")).unwrap(), b"neu");
                assert_eq!(
                stats.irreversible_replacements, expected,
                "Die sichtbare Bilanz muss auch die tatsächlich ersetzte Hardlink-Datei enthalten"
            );
            });
        }
    }
}
