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

        match action {
            Action::CreateDir { metadata, .. } => {
                if let Some(metadata) = metadata {
                    validate_metadata_transfer(metadata)?;
                    if metadata.target.is_some() {
                        return Err(invalid_plan(
                            "CreateDir darf kein vorhandenes Zielmanifest enthalten",
                        ));
                    }
                }
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
            Action::SetMode { .. } | Action::SetDirectoryMetadata { .. } => {
                if let Action::SetDirectoryMetadata { metadata, .. } = action {
                    validate_metadata_transfer(metadata)?;
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
        && (plan.comparison.rows.len() != plan.actions.len()
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
        local_execution: request.safety.local_execution().cloned(),
        dry_run: request.safety.is_dry_run(),
        metadata_report: request.plan.metadata_report.clone(),
        ..Default::default()
    };
    // Auch Fehler späterer Aktionen müssen Reste früherer Übertragungen behalten.
    execute_with_stats(request, cancel, on_progress, &mut stats)
        .map_err(|error| crate::error::with_recovery_paths(error, &stats.temp_recovery_paths))?;
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
            Action::CreateDir { path, .. } => {
                if !dry {
                    let target = checked_join(dst_root, path)?;
                    guard_path(safety.sandbox(), &target, "Schreibziel")?;
                    if optional_stat(dst, &target)?.is_some() {
                        return Err(plan_changed(path, "Zielordner existiert inzwischen"));
                    }
                    // Atomare Einzelanlage: Entsteht der Name nach der Planprüfung,
                    // überschreiben/akzeptieren wir ihn nicht stillschweigend.
                    dst.create_dir(&target)?;
                    sync_parent(dst, &target)?;
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
                        } => {
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
                        } => {
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
        apply_pending_mode(pending, &request)?;
        if !pending.is_new {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
        }
    }

    // Phase 2: Überzählige Zielobjekte nach allen erfolgreichen Dateiübertragungen
    // und Datei-Metadaten entfernen. Ordner-Metadaten folgen nach dieser Phase.
    // In der App bedeutet „entfernen" ein Verschieben in die laufbezogene Quarantäne.
    if !dry && plan.delete_count > 0 {
        if let Some(permit) = safety.real_permit() {
            permit.validate_mirror_rescan(src, src_root, dst, dst_root, plan)?;
        }
    }
    for action in &plan.actions {
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
                prog.phase = "quarantine".into();
                prog.current = path.clone();
                on_progress(&prog);
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
            }
            Action::DeleteDir {
                path,
                entries,
                bytes,
                target_proof,
                target_tree_hash,
            } => {
                prog.phase = "quarantine".into();
                prog.current = path.clone();
                on_progress(&prog);
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
        apply_pending_mode(pending, &request)?;
        if !pending.is_new {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
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
            } => pending_directories.push(PendingDirectoryMetadata {
                path,
                source_mtime: *source_mtime,
                target_mtime: None,
                metadata,
                is_new: true,
            }),
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
            apply_directory_metadata(DirectoryMetadataRequest {
                src,
                source_path: &source_path,
                dst,
                target_path: &target_path,
                source_mtime: pending.source_mtime,
                target_mtime: pending.target_mtime,
                metadata: pending.metadata,
                is_new: pending.is_new,
            })?;
        }
        if !pending.is_new {
            stats.metadata_updated = stats.metadata_updated.saturating_add(1);
        }
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

fn apply_pending_mode(pending: &PendingMode<'_>, request: &ExecuteRequest<'_>) -> io::Result<()> {
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
        dst.set_mode(&target_path, source_mode)?;
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
    Ok(())
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
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_directory_source(
    src: &dyn Vfs,
    source_path: &Path,
    source_mtime: Option<i64>,
    metadata: &MetadataTransfer,
) -> io::Result<()> {
    validate_metadata_transfer(metadata)?;
    if metadata.resource_fork {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Ordneraktion darf keinen Ressourcen-Fork-Inhaltsstrom enthalten",
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
}

fn apply_directory_metadata(request: DirectoryMetadataRequest<'_>) -> io::Result<()> {
    let DirectoryMetadataRequest {
        src,
        source_path,
        dst,
        target_path,
        source_mtime,
        target_mtime: _target_mtime,
        metadata,
        is_new,
    } = request;
    validate_directory_source(src, source_path, source_mtime, metadata)?;
    let target = dst.stat(target_path)?;
    if target.is_symlink || !target.is_dir {
        return Err(plan_changed(
            &target_path.to_string_lossy(),
            "Metadaten-Ziel ist kein echter Ordner",
        ));
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
    if metadata.acl {
        dst.write_acl_exact(target_path, metadata.source.acl.as_ref())?;
    }
    if let Some(values) = source_xattrs.as_deref() {
        dst.write_xattrs_exact(target_path, values)?;
    }
    if metadata.normal_mode {
        dst.set_metadata_normal_mode(target_path, metadata.source.mode & 0o777)?;
    }
    if dst.supports_mtime_write() {
        if let Some(mtime) = source_mtime {
            dst.set_mtime(target_path, mtime)?;
        }
    }
    dst.sync_dir(target_path)?;
    if metadata.special_mode_bits {
        dst.set_metadata_special_bits(target_path, metadata.source.mode & 0o7000)?;
        dst.sync_dir(target_path)?;
    }
    verify_selected_metadata(dst, target_path, metadata)?;
    if source_mtime.is_some() && dst.stat(target_path)?.mtime != source_mtime {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Ordner-mtime stimmt nach dem Schreiben nicht exakt überein",
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use crate::{Compare, LocalVfs, MetadataProfile, SyncMode, SyncOptions, Vfs};

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
                let prepared = crate::prepare_local_sync_run(
                    &source,
                    &target,
                    SyncOptions {
                        mode: SyncMode::Update,
                        compare: Compare::ContentHash,
                        metadata: MetadataProfile::NativeBestEffort,
                        ..Default::default()
                    },
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
