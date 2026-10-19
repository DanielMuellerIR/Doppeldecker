//! Atomare Datei-, Sparse- und Hardlink-Übertragung mit Quarantäne-Commit.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::atomic_file::{
    copy_stream, create_neighbor_temp, hash_vfs_file, rename_noreplace_with_phase, sync_parent,
    sync_rename_parents, SyncedRenameFailure, COPY_BUFFER_BYTES,
};
use crate::path::checked_join;
use crate::progress::{Cancel, Progress};
use crate::quarantine::{record_quarantine_intent, QuarantineReason};
use crate::vfs::{Entry, ProofListEntry, Vfs};

use super::{
    complete_listing, guard_path, optional_stat, transfer_planned_metadata,
    validate_anchor_identity, validate_metadata_transfer, verify_planned_metadata,
    verify_temp_metadata, MetadataTransfer, PlannedMetadataRequest, Safety,
};

/// Ergebnis eines einzelnen sicheren Kopiervorgangs.
pub(super) enum CopyResult {
    Complete {
        quarantined: bool,
        recovery_paths: Vec<String>,
    },
    Cancelled {
        recovery_paths: Vec<String>,
    },
}

/// Der für Planbindung und atomaren Commit relevante Zustand einer regulären Datei.
type FileSnapshot = (u64, Option<i64>, Option<u32>);

/// Nicht serialisierbare Laufbindung zwischen Manifestanlage und letztem Claim.
/// Der Eintragsbeleg verhindert, dass ein gleich benannter Ersatzordner die
/// Quarantäne eines laufenden Syncs übernimmt.
#[derive(Debug)]
pub(super) struct SyncQuarantineRun {
    proof: super::SyncTargetProof,
    _lease: crate::quarantine::QuarantineLease,
}

impl SyncQuarantineRun {
    pub(super) fn proof(&self) -> &super::SyncTargetProof {
        &self.proof
    }
}

pub(super) fn verify_sync_quarantine_run(
    vfs: &dyn Vfs,
    run_root: &Path,
    run: &SyncQuarantineRun,
) -> io::Result<()> {
    verify_directory_proof(vfs, run_root, run.proof()).map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Der Quarantänelauf wurde während der Ausführung ausgetauscht ({error}). Kein weiterer Ziel-Rename wurde versucht. Recoverypfad: {}",
                    run_root.display()
                ),
            ),
            &[&error],
            &[run_root],
        )
    })
}

pub(super) fn start_sync_quarantine_run(
    vfs: &dyn Vfs,
    run_root: &Path,
    target_root: &Path,
    safety: &Safety,
) -> io::Result<SyncQuarantineRun> {
    guard_path(safety.sandbox(), run_root, "Quarantänelauf")?;
    if optional_stat(vfs, run_root)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "Reservierter Quarantänelauf existiert bereits: {}",
                run_root.display()
            ),
        ));
    }
    // Das Manifest entsteht vor dem ersten sichtbaren Laufnamen. Scheitert dessen
    // Anlage anschließend unklar, bleibt damit ein auffindbarer Recoveryfall.
    let lease = crate::quarantine::start_quarantine_run(vfs, run_root, target_root)?;
    let creation_proof = match vfs.create_dir_with_removal_proof(run_root) {
        Ok(proof) => proof,
        Err(crate::vfs::MutationFailure::Unchanged(error)) => return Err(error),
        Err(crate::vfs::MutationFailure::OutcomeUnclear(error)) => {
            return Err(crate::error::with_recovery_context(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Das Anlegen des manifestierten Quarantänelaufs meldete einen Fehler nach möglicher Wirkung ({error}). Recoverypfad: {}",
                        run_root.display()
                    ),
                ),
                &[&error],
                &[run_root]),
            )
        }
    };
    sync_parent(vfs, run_root).map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Der manifestierte Quarantänelauf wurde sichtbar angelegt, aber sein Elternordner konnte nicht synchronisiert werden ({error}). Recoverypfad: {}",
                    run_root.display()
                ),
            ),
            &[&error],
            &[run_root],
        )
    })?;
    let listed = listed_entry(vfs, run_root)
        .map_err(|error| {
            crate::error::with_recovery_context(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Der sichtbar angelegte Quarantänelauf konnte nicht an seine Identität gebunden werden ({error}). Recoverypfad: {}",
                        run_root.display()
                    ),
                ),
                &[&error],
                &[run_root],
            )
        })?;
    let proof = if let Some(created) = creation_proof {
        let created = super::SyncTargetProof::from_listed(&created).map_err(|error| {
            crate::error::with_recovery_context(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Der Provider lieferte keinen gültigen atomaren Beleg für den sichtbar angelegten Quarantänelauf ({error}). Recoverypfad: {}",
                        run_root.display()
                    ),
                ),
                &[&error],
                &[run_root],
            )
        })?;
        if !created.matches_stable_directory_at(run_root, &listed) {
            return Err(crate::error::with_recovery_context(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Der Quarantänelauf wurde zwischen atomarer Anlage und Readback ausgetauscht. Recoverypfad: {}",
                        run_root.display()
                    ),
                ),
                &[],
                &[run_root]),
            );
        }
        created
    } else {
        // POSIX kann die Verzeichnisanlage nicht atomar an Gerät/Inode zurückgeben.
        // Vor der ersten Zielwirkung binden wir deshalb den frisch vollständig
        // gelisteten Namen als sichtbaren Best-Effort-Lauf. Jeder spätere Austausch
        // wird vor beziehungsweise nach dem Claim gegen genau diesen Beleg erkannt.
        super::SyncTargetProof::from_listed(&listed).map_err(|error| {
            crate::error::with_recovery_context(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!(
                        "Der sichtbar angelegte Quarantänelauf konnte nicht an seinen Readback gebunden werden ({error}). Recoverypfad: {}",
                        run_root.display()
                    ),
                ),
                &[&error],
                &[run_root],
            )
        })?
    };
    if !proof.validates_directory_action(&run_root.to_string_lossy()) {
        return Err(crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Der neu angelegte Quarantänelauf ließ sich nicht als echter Ordner binden. Recoverypfad: {}",
                    run_root.display()
                ),
            ),
            &[],
            &[run_root]),
        );
    }
    crate::quarantine::bind_quarantine_run_identity(vfs, run_root).map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Der angelegte Quarantänelauf konnte vor dem ersten Ziel-Claim nicht dauerhaft an sein Manifest gebunden werden ({error}). Recoverypfad: {}",
                    run_root.display()
                ),
            ),
            &[&error],
            &[run_root],
        )
    })?;
    Ok(SyncQuarantineRun {
        proof,
        _lease: lease,
    })
}

/// Kopiert in eine neue temporäre Nachbardatei, verifiziert Inhalt und Quellzustand und
/// verschiebt erst danach atomar an den endgültigen Pfad. Das bisherige Ziel bleibt bei
/// Abbruch, vollem Datenträger oder Lesefehler vollständig erhalten.
pub(super) struct CopyFileRequest<'a> {
    pub(super) src: &'a dyn Vfs,
    pub(super) src_path: &'a Path,
    pub(super) dst: &'a dyn Vfs,
    pub(super) dst_path: &'a Path,
    pub(super) relative_path: &'a Path,
    pub(super) expected_size: u64,
    pub(super) expected_source_mtime: Option<i64>,
    pub(super) expected_source_mode: Option<u32>,
    pub(super) expected_target: Option<FileSnapshot>,
    pub(super) expected_target_proof: Option<&'a super::SyncTargetProof>,
    pub(super) expected_source_hash: Option<&'a str>,
    pub(super) expected_target_hash: Option<&'a str>,
    pub(super) expected_metadata: Option<&'a MetadataTransfer>,
    pub(super) quarantine_run_proof: Option<&'a super::SyncTargetProof>,
    pub(super) safety: &'a Safety,
}

pub(super) fn copy_one(
    request: CopyFileRequest<'_>,
    cancel: &Cancel,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<CopyResult> {
    let CopyFileRequest {
        src,
        src_path,
        dst,
        dst_path,
        relative_path,
        expected_size,
        expected_source_mtime,
        expected_source_mode,
        expected_target,
        expected_target_proof,
        expected_source_hash,
        expected_target_hash,
        expected_metadata,
        quarantine_run_proof,
        safety,
    } = request;
    let source_before = regular_copy_snapshot(src, src_path)?;
    if source_before != (expected_size, expected_source_mtime, expected_source_mode) {
        return Err(plan_changed(
            &relative_path.to_string_lossy(),
            "Quelldatei wurde seit der Vorschau geändert",
        ));
    }

    let target_before = current_optional_copy_snapshot(dst, dst_path)?;
    if target_before != expected_target {
        return Err(plan_changed(
            &relative_path.to_string_lossy(),
            "Zieldatei wurde seit der Vorschau geändert",
        ));
    }
    verify_target_proof(dst, dst_path, expected_target_proof)?;
    verify_planned_metadata(src, src_path, dst, dst_path, expected_metadata)?;
    if let Some(expected_hash) = expected_target_hash {
        if target_before.is_none() || hash_file_hex(dst, dst_path)? != expected_hash {
            return Err(plan_changed(
                &relative_path.to_string_lossy(),
                "Zieldatei-Inhalt änderte sich seit der Hash-Vorschau",
            ));
        }
    }

    guard_path(safety.sandbox(), dst_path, "Schreibziel")?;
    if let Some(parent) = dst_path.parent() {
        guard_path(safety.sandbox(), parent, "Zielordner")?;
        dst.create_dir_all(parent)?;
    }

    let staging_scope = dst.staging_scope(dst_path, safety.sandbox())?;
    let dst = staging_scope.as_deref().unwrap_or(dst);
    let result = (|| -> io::Result<CopyResult> {
        let sparse_extents = expected_metadata
            .filter(|metadata| metadata.sparse_extents)
            .and_then(|metadata| metadata.source.sparse_extents.as_deref());
        let (temp_path, completed, source_hash, bytes_written) = transfer_file_data(
            TransferFileRequest {
                src,
                src_path,
                dst,
                dst_path,
                expected_size,
                sparse_extents,
                sandbox: safety.sandbox(),
            },
            cancel,
            prog,
            on_progress,
        )?;
        if !completed {
            let recovery_paths = cleanup_or_retain(dst, &temp_path)?;
            return Ok(CopyResult::Cancelled { recovery_paths });
        }
        // Der Commit meldet hierüber, ob er den Temp-Namen noch hält. Vor dem ersten
        // Rename gehört er ausschließlich uns, danach nicht mehr.
        let mut temp_released = false;
        let mut temp_proof = None;
        let verify_and_commit = (|| -> io::Result<Option<bool>> {
            if bytes_written != expected_size {
                return Err(plan_changed(
                    &relative_path.to_string_lossy(),
                    "Quellgröße änderte sich während des Kopierens",
                ));
            }

            let source_after = regular_copy_snapshot(src, src_path)?;
            if source_after != source_before {
                return Err(plan_changed(
                    &relative_path.to_string_lossy(),
                    "Quelle änderte sich während des Kopierens",
                ));
            }
            if current_optional_copy_snapshot(dst, dst_path)? != target_before {
                return Err(plan_changed(
                    &relative_path.to_string_lossy(),
                    "Ziel änderte sich während des Kopierens",
                ));
            }
            verify_target_proof(dst, dst_path, expected_target_proof)?;
            verify_planned_metadata(src, src_path, dst, dst_path, expected_metadata)?;
            if let Some(expected_hash) = expected_target_hash {
                if hash_file_hex(dst, dst_path)? != expected_hash {
                    return Err(plan_changed(
                        &relative_path.to_string_lossy(),
                        "Zieldatei-Inhalt änderte sich während des Kopierens",
                    ));
                }
            }

            let transferred_hash = source_hash;
            if let Some(expected_hash) = expected_source_hash {
                if transferred_hash.to_hex().as_str() != expected_hash {
                    return Err(plan_changed(
                        &relative_path.to_string_lossy(),
                        "Quelldatei-Inhalt änderte sich seit der Hash-Vorschau",
                    ));
                }
            }

            // Die geschriebenen Bytes werden nochmals vom Ziel gelesen. Damit kann weder
            // ein kurzer/fehlerhafter Write noch ein Providerfehler unbemerkt committed werden.
            let temp_hash = hash_vfs_file(dst, &temp_path)?;
            if temp_hash != transferred_hash {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Kopierprüfung fehlgeschlagen: {} stimmt nicht mit der Quelle überein",
                        relative_path.display()
                    ),
                ));
            }
            if !transfer_planned_metadata(
                PlannedMetadataRequest {
                    src,
                    src_path,
                    dst,
                    temp_path: &temp_path,
                    expected: expected_metadata,
                },
                cancel,
                prog,
                on_progress,
            )? {
                return Ok(None);
            }
            // Die Metadatenströme können länger laufen als der Daten-Fork. Quelle und
            // bisheriges Ziel müssen deshalb unmittelbar danach erneut plan-gebunden sein.
            verify_planned_metadata(src, src_path, dst, dst_path, expected_metadata)?;
            if let Some(metadata) = expected_metadata.filter(|metadata| metadata.normal_mode) {
                dst.set_metadata_normal_mode(&temp_path, metadata.source.mode & 0o777)?;
            } else if expected_metadata.is_none()
                && dst.supports_mode_write_at(
                    dst_path
                        .parent()
                        .ok_or_else(|| io::Error::other("Datei ohne Elternordner"))?,
                )?
            {
                if let Some(mode) = expected_source_mode {
                    dst.set_mode(&temp_path, mode)?;
                }
            }
            if dst.supports_mtime_write() {
                if let Some(mtime) = expected_source_mtime {
                    dst.set_mtime(&temp_path, mtime)?;
                }
            }
            dst.sync_file(&temp_path)?;
            // Sonderbits erst auf der vollständig geschriebenen und synchronisierten
            // Temp-Datei aktivieren. Ein abschließender Readback folgt vor dem Commit.
            if let Some(metadata) = expected_metadata.filter(|metadata| metadata.special_mode_bits)
            {
                dst.set_metadata_special_bits(&temp_path, metadata.source.mode & 0o7000)?;
                dst.sync_file(&temp_path)?;
            }
            verify_temp_metadata(dst, &temp_path, expected_metadata)?;
            validate_anchor_identity(src, dst, safety)?;
            // Identität der fertigen Temp-Datei unmittelbar vor dem Commit binden.
            // Nur damit lässt sich ein späteres Aufräumen auf genau dieses Objekt
            // beschränken, falls der Commit den Namen schon freigegeben hat.
            temp_proof = stat_bound_temp_proof(dst, &temp_path);
            commit_temp(CommitTempRequest {
                dst,
                temp_path: &temp_path,
                dst_path,
                relative_path,
                replacing: target_before.is_some(),
                expected_target_proof,
                expected_target_hash,
                quarantine_run_proof,
                safety,
                temp_released: &mut temp_released,
                transferred_size: expected_size,
                transferred_hash: *transferred_hash.as_bytes(),
            })
            .map(Some)
        })();

        match verify_and_commit {
            Ok(Some(quarantined)) => Ok(CopyResult::Complete {
                quarantined,
                recovery_paths: Vec::new(),
            }),
            Ok(None) => Ok(CopyResult::Cancelled {
                recovery_paths: cleanup_or_retain(dst, &temp_path)?,
            }),
            Err(e) => {
                match cleanup_failed_temp(dst, &temp_path, temp_proof.as_ref(), temp_released, &e) {
                    Ok(()) => Err(e),
                    Err(cleanup) => Err(crate::error::with_recovery_path(e, &temp_path, &cleanup)),
                }
            }
        }
    })();
    if staging_scope.is_none() {
        return result;
    }
    crate::atomic_file::finish_staging_scope(dst, result, |result, path| {
        let (CopyResult::Complete { recovery_paths, .. }
        | CopyResult::Cancelled { recovery_paths }) = result;
        if !recovery_paths.contains(&path) {
            recovery_paths.push(path);
        }
    })
}

/// Materialisiert einen weiteren Hardlink-Namen über einen exklusiven temporären Link.
/// Erst nach vollständiger Plan- und Inode-Prüfung wird dieser Name wie ein atomarer
/// Datei-Commit an die endgültige Stelle verschoben.
pub(super) struct HardlinkRequest<'a> {
    pub(super) src: &'a dyn Vfs,
    pub(super) src_root: &'a Path,
    pub(super) dst: &'a dyn Vfs,
    pub(super) dst_root: &'a Path,
    pub(super) path: &'a str,
    pub(super) link_to: &'a str,
    pub(super) expected_source: FileSnapshot,
    pub(super) expected_target: Option<FileSnapshot>,
    pub(super) expected_target_proof: Option<&'a super::SyncTargetProof>,
    pub(super) source_hash: &'a str,
    pub(super) target_hash: Option<&'a str>,
    pub(super) metadata: &'a MetadataTransfer,
    pub(super) quarantine_run_proof: Option<&'a super::SyncTargetProof>,
    pub(super) safety: &'a Safety,
}

pub(super) fn create_hardlink_one(request: HardlinkRequest<'_>) -> io::Result<(bool, Vec<String>)> {
    let HardlinkRequest {
        src,
        src_root,
        dst,
        dst_root,
        path,
        link_to,
        expected_source,
        expected_target,
        expected_target_proof,
        source_hash,
        target_hash,
        metadata,
        quarantine_run_proof,
        safety,
    } = request;
    validate_metadata_transfer(metadata)?;
    if !metadata.hardlinks || metadata.source.hardlink_group.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CreateHardlink benötigt eine gebundene Hardlink-Gruppe",
        ));
    }

    let source_path = checked_join(src_root, path)?;
    let source_anchor = checked_join(src_root, link_to)?;
    let source = src.stat(&source_path)?;
    let anchor_source = src.stat(&source_anchor)?;
    if regular_copy_snapshot(src, &source_path)? != expected_source
        || source.device.is_none()
        || (source.device, source.inode) != (anchor_source.device, anchor_source.inode)
    {
        return Err(plan_changed(
            path,
            "Quell-Hardlink-Gruppe änderte sich seit der Vorschau",
        ));
    }
    if hash_file_hex(src, &source_path)? != source_hash {
        return Err(plan_changed(
            path,
            "Hardlink-Quellinhalt änderte sich seit der Vorschau",
        ));
    }

    let target_path = checked_join(dst_root, path)?;
    let staging_scope = dst.staging_scope(&target_path, safety.sandbox())?;
    let dst = staging_scope.as_deref().unwrap_or(dst);
    let result = (|| -> io::Result<(bool, Vec<String>)> {
        let target_anchor = checked_join(dst_root, link_to)?;
        if current_optional_copy_snapshot(dst, &target_path)? != expected_target {
            return Err(plan_changed(
                path,
                "Hardlink-Ziel änderte sich seit der Vorschau",
            ));
        }
        verify_target_proof(dst, &target_path, expected_target_proof)?;
        verify_planned_metadata(src, &source_path, dst, &target_path, Some(metadata))?;
        if let Some(expected_hash) = target_hash {
            if hash_file_hex(dst, &target_path)? != expected_hash {
                return Err(plan_changed(
                    path,
                    "Hardlink-Zielinhalt änderte sich seit der Vorschau",
                ));
            }
        }

        let anchor = dst.stat(&target_anchor)?;
        if anchor.is_symlink
            || !anchor.is_file
            || hash_file_hex(dst, &target_anchor)? != source_hash
        {
            return Err(plan_changed(
                link_to,
                "Materialisiertes Hardlink-Ziel ist nicht planidentisch",
            ));
        }
        if let Ok(current) = dst.stat(&target_path) {
            if (current.device, current.inode) == (anchor.device, anchor.inode) {
                return Ok((false, Vec::new()));
            }
        }

        guard_path(safety.sandbox(), &target_path, "Hardlink-Ziel")?;
        guard_path(safety.sandbox(), &target_anchor, "Hardlink-Quelle")?;
        let parent = target_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Hardlink-Ziel ohne Elternordner",
            )
        })?;
        let temp_path = loop {
            let candidate = dst.staging_candidate(&target_path, "link")?;
            guard_path(safety.sandbox(), &candidate, "Hardlink-Temporärname")?;
            match dst.create_hard_link_noreplace(&target_anchor, &candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };

        let mut temporary_proof = None;
        let mut temp_released = false;
        let result = (|| {
            let linked = dst.stat(&temp_path)?;
            temporary_proof = Some(bound_temp_proof(&temp_path, linked.clone()));
            let anchor_after = dst.stat(&target_anchor)?;
            if linked.is_symlink
                || !linked.is_file
                || (linked.device, linked.inode) != (anchor_after.device, anchor_after.inode)
                || (anchor.device, anchor.inode) != (anchor_after.device, anchor_after.inode)
            {
                return Err(plan_changed(
                    path,
                    "Hardlink-Anker änderte sich während der Anlage",
                ));
            }
            dst.sync_dir(parent)?;
            validate_anchor_identity(src, dst, safety)?;
            commit_temp(CommitTempRequest {
                dst,
                temp_path: &temp_path,
                dst_path: &target_path,
                relative_path: Path::new(path),
                replacing: expected_target.is_some(),
                expected_target_proof,
                expected_target_hash: target_hash,
                quarantine_run_proof,
                safety,
                temp_released: &mut temp_released,
                transferred_size: expected_source.0,
                transferred_hash: *blake3::Hash::from_hex(source_hash)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "Ungültiger Quellhash")
                    })?
                    .as_bytes(),
            })
        })();
        if let Err(error) = result {
            return match cleanup_failed_temp(
                dst,
                &temp_path,
                temporary_proof.as_ref(),
                temp_released,
                &error,
            ) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(crate::error::with_recovery_path(
                    error, &temp_path, &cleanup,
                )),
            };
        }
        result.map(|quarantined| (quarantined, Vec::new()))
    })();
    if staging_scope.is_none() {
        return result;
    }
    crate::atomic_file::finish_staging_scope(dst, result, |(_, recovery_paths), path| {
        if !recovery_paths.contains(&path) {
            recovery_paths.push(path);
        }
    })
}

/// Bindet einen Temp-Eintrag an seine bereits beobachtete Identität.
fn bound_temp_proof(temp_path: &Path, entry: Entry) -> ProofListEntry {
    let name = temp_path.file_name();
    ProofListEntry {
        raw_name: name
            .map(|name| name.to_string_lossy().as_bytes().to_vec())
            .unwrap_or_default(),
        safely_decoded: name.and_then(|name| name.to_str()).is_some(),
        provider_token: None,
        entry,
    }
}

/// Liest die Identität eines Temp-Eintrags frisch ein.
///
/// Ein Fehler ist hier kein Abbruchgrund: Ohne Identität wird später nur nicht
/// aufgeräumt, und ein stehengebliebener Temp-Eintrag ist harmloser als eine
/// pfadbasierte Löschung, die den falschen Nachfolger trifft.
fn stat_bound_temp_proof(dst: &dyn Vfs, temp_path: &Path) -> Option<ProofListEntry> {
    dst.stat(temp_path)
        .ok()
        .map(|entry| bound_temp_proof(temp_path, entry))
}

/// Räumt einen Temp-Eintrag auf, dessen Name bereits freigegeben sein kann.
///
/// Gelöscht wird nur mit atomarer Identitätsbedingung des Providers, also genau
/// der beobachtete Eintrag. Fehlt diese Bedingung — lokal ist das der Fall —,
/// bleibt der Eintrag sichtbar zur Recovery stehen, statt einen gleichnamigen
/// Nachfolger eines fremden Prozesses zu treffen. Ohne bestätigte Bereinigung bleibt
/// der Temp als sichtbarer Recoverypfad erhalten; sein Staging-Besitzer schließt
/// den Arbeitsbereich anschließend ab.
fn cleanup_or_retain(dst: &dyn Vfs, path: &Path) -> io::Result<Vec<String>> {
    if cleanup_unreleased_temp(dst, path).is_err() {
        return Ok(vec![path.to_string_lossy().into_owned()]);
    }
    Ok(Vec::new())
}

fn cleanup_unreleased_temp(dst: &dyn Vfs, path: &Path) -> io::Result<()> {
    if dst.is_local() {
        if dst.cleanup_owned_temp(path)? {
            return Ok(());
        }
        return Err(crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Temp blieb ohne privaten Namensraum erhalten. Recoverypfad: {}",
                    path.display()
                ),
            ),
            &[],
            &[path],
        ));
    }
    dst.remove_file(path)
}

fn cleanup_bound_temp(
    dst: &dyn Vfs,
    temp_path: &Path,
    proof: Option<&ProofListEntry>,
) -> io::Result<()> {
    if let Some(proof) = proof {
        if dst.supports_remove_if_matches() {
            return dst.remove_if_matches(temp_path, proof);
        }
    }
    Err(io::Error::other(
        "Temp-Identität für Bereinigung nicht bewiesen",
    ))
}

/// Räumt einen Temp-Eintrag nur nach einem geklärten Fehlerausgang auf.
///
/// `OutcomeUnclear` bindet beide Namen als Recoverypfade. Selbst eine bedingte
/// Entfernung des noch sichtbaren Temp-Eintrags wäre dann eine zweite Mutation,
/// bevor geklärt ist, ob der Rename wirkte. Bei allen anderen Fehlern gehört ein
/// nicht freigegebener Temp-Name weiterhin uns; ein freigegebener Name darf nur
/// über seinen atomaren Identitätsbeleg entfernt werden.
fn cleanup_failed_temp(
    dst: &dyn Vfs,
    temp_path: &Path,
    proof: Option<&ProofListEntry>,
    temp_released: bool,
    error: &io::Error,
) -> io::Result<()> {
    if crate::error_code(error) == crate::ErrorCode::OutcomeUnclear {
        return Ok(());
    }
    if temp_released {
        cleanup_bound_temp(dst, temp_path, proof)
    } else {
        cleanup_unreleased_temp(dst, temp_path)
    }
}

/// Überträgt den regulären Datenstrom in eine neue Temp-Datei. Bei aktivem Sparse-
/// Vertrag werden ausschließlich manifestierte Datenbereiche geschrieben; Lücken
/// bleiben echte Löcher, zählen aber als logische Nullbytes für Digest und Fortschritt.
struct TransferFileRequest<'a> {
    src: &'a dyn Vfs,
    src_path: &'a Path,
    dst: &'a dyn Vfs,
    dst_path: &'a Path,
    expected_size: u64,
    sparse_extents: Option<&'a [crate::metadata::DataExtent]>,
    sandbox: Option<&'a Path>,
}

fn transfer_file_data(
    request: TransferFileRequest<'_>,
    cancel: &Cancel,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<(PathBuf, bool, blake3::Hash, u64)> {
    let TransferFileRequest {
        src,
        src_path,
        dst,
        dst_path,
        expected_size,
        sparse_extents,
        sandbox,
    } = request;
    if let Some(extents) = sparse_extents {
        let mut reader = src.open_sparse_read(src_path)?;
        let (temp_path, mut writer) = create_sparse_temp(dst, dst_path, sandbox)?;
        let result = (|| -> io::Result<(bool, blake3::Hasher, u64)> {
            let mut hash = blake3::Hasher::new();
            let mut logical_offset = 0u64;
            let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
            for extent in extents {
                let end = extent.offset.checked_add(extent.length).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Sparse-Extent läuft über")
                })?;
                if extent.offset < logical_offset || end > expected_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Sparse-Extent liegt außerhalb der bestätigten Dateigröße",
                    ));
                }
                if !hash_sparse_hole(
                    extent.offset - logical_offset,
                    &mut hash,
                    cancel,
                    prog,
                    on_progress,
                )? {
                    return Ok((false, hash, logical_offset));
                }
                reader.seek(SeekFrom::Start(extent.offset))?;
                writer.seek(SeekFrom::Start(extent.offset))?;
                let mut remaining = extent.length;
                while remaining > 0 {
                    if cancel.is_cancelled() {
                        return Ok((false, hash, end - remaining));
                    }
                    let wanted = remaining.min(buffer.len() as u64) as usize;
                    let read = reader.read(&mut buffer[..wanted])?;
                    if read == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "Sparse-Datenbereich endete vor der manifestierten Länge",
                        ));
                    }
                    writer.write_all(&buffer[..read])?;
                    hash.update(&buffer[..read]);
                    remaining -= read as u64;
                    prog.bytes_done = prog.bytes_done.saturating_add(read as u64);
                    on_progress(prog);
                }
                logical_offset = end;
            }
            if !hash_sparse_hole(
                expected_size - logical_offset,
                &mut hash,
                cancel,
                prog,
                on_progress,
            )? {
                return Ok((false, hash, logical_offset));
            }
            writer.set_len(expected_size)?;
            let mut hole_start = 0u64;
            for extent in extents {
                if extent.offset > hole_start {
                    writer.punch_hole(hole_start, extent.offset - hole_start)?;
                }
                hole_start = extent.offset + extent.length;
            }
            if hole_start < expected_size {
                writer.punch_hole(hole_start, expected_size - hole_start)?;
            }
            writer.flush()?;
            Ok((true, hash, expected_size))
        })();
        drop(writer);
        return match result {
            Ok((complete, hash, bytes)) => Ok((temp_path, complete, hash.finalize(), bytes)),
            Err(error) => match cleanup_unreleased_temp(dst, &temp_path) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(crate::error::with_recovery_path(
                    error, &temp_path, &cleanup,
                )),
            },
        };
    }

    let mut reader = src.open_read(src_path)?;
    let (temp_path, mut writer) = create_temp(dst, dst_path, sandbox)?;
    // Der bestätigte Platzbedarf begrenzt bereits das Schreiben, nicht erst den Commit.
    let result = copy_stream(
        &mut reader.as_mut().take(expected_size),
        writer.as_mut(),
        cancel,
        &mut |bytes| {
            prog.bytes_done = prog.bytes_done.saturating_add(bytes);
            on_progress(prog);
        },
    )
    .and_then(|result| {
        if result.complete && (result.bytes != expected_size || reader.read(&mut [0u8; 1])? != 0) {
            return Err(plan_changed(
                &src_path.to_string_lossy(),
                "Quellstream hat eine andere Länge",
            ));
        }
        Ok(result)
    });
    drop(writer);
    match result {
        Ok(result) => Ok((temp_path, result.complete, result.hash, result.bytes)),
        Err(error) => match cleanup_unreleased_temp(dst, &temp_path) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(crate::error::with_recovery_path(
                error, &temp_path, &cleanup,
            )),
        },
    }
}

fn hash_sparse_hole(
    mut length: u64,
    hash: &mut blake3::Hasher,
    cancel: &Cancel,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<bool> {
    let zeros = vec![0u8; COPY_BUFFER_BYTES];
    while length > 0 {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let chunk = length.min(zeros.len() as u64) as usize;
        hash.update(&zeros[..chunk]);
        length -= chunk as u64;
        prog.bytes_done = prog.bytes_done.saturating_add(chunk as u64);
        on_progress(prog);
    }
    Ok(true)
}

/// Neue, kollisionsfreie temporäre Datei im selben Ordner wie das endgültige Ziel.
fn create_temp(
    dst: &dyn Vfs,
    dst_path: &Path,
    sandbox: Option<&Path>,
) -> io::Result<(PathBuf, Box<dyn Write + Send>)> {
    create_neighbor_temp(dst, dst_path, "tmp", &mut |candidate| {
        guard_path(sandbox, candidate, "Temporärdatei")
    })
}

/// Sparse-Variante der exklusiven Temp-Anlage; die Namens- und Sandbox-Schranken sind
/// identisch zum normalen Datenstrom.
fn create_sparse_temp(
    dst: &dyn Vfs,
    dst_path: &Path,
    sandbox: Option<&Path>,
) -> io::Result<(PathBuf, Box<dyn crate::vfs::SparseWrite>)> {
    for _ in 0..100 {
        let candidate = dst.staging_candidate(dst_path, "tmp")?;
        guard_path(sandbox, &candidate, "Sparse-Temporärdatei")?;
        match dst.open_sparse_write_new(&candidate) {
            Ok(writer) => return Ok((candidate, writer)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Kein freier temporärer Sparse-Dateiname gefunden",
    ))
}

/// Temporäre Datei sichtbar committen. Bei einer Aktualisierung wird das bisherige
/// Ziel zuerst in die Quarantäne verschoben und bei einem Commit-Fehler zurückgerollt.
/// Verschiebt die fertige Temp-Datei an ihren Zielplatz.
///
/// Für das spätere Aufräumen sind das drei verschiedene Ausgänge, die
/// `SyncedRenameFailure` bereits benennt: Nur `RenameUnchanged` lässt den
/// Temp-Namen bei uns. Ein erfolgreicher Rename, ein Fehler nach möglicher
/// Wirkung und ein Fehler erst im Verzeichnis-Sync geben ihn dagegen frei —
/// danach darf ihn ein fremder Prozess neu belegen, und ein pfadbasiertes
/// Löschen träfe womöglich diesen Nachfolger. `temp_released` reicht genau diese
/// Unterscheidung an den Aufrufer weiter.
enum TempRenameFailure {
    Unchanged(io::Error),
    OutcomeUnclear(io::Error),
    Sync(io::Error),
}

impl TempRenameFailure {
    /// Formuliert den Fehler für einen Ablauf ohne vorhandenes Ziel und damit ohne
    /// möglichen Rollback. Ersetzende Läufe werten dieselbe Phase erst nach ihrem
    /// konkreten Rückweg aus und verwenden diesen Text bewusst nicht.
    fn without_rollback(self, temp_path: &Path, dst_path: &Path) -> io::Error {
        match self {
            Self::Unchanged(error) => error,
            Self::OutcomeUnclear(error) => crate::error::with_recovery_context(
                crate::error::coded_error(
                    io::ErrorKind::Other,
                    crate::error::ErrorCode::OutcomeUnclear,
                    format!(
                        "Das Commit-Rename meldete einen Fehler nach möglicher Wirkung ({error}). Ein altes Ziel für einen Rückweg gab es nicht. Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                        temp_path.display(),
                        dst_path.display(),
                    ),
                ),
                &[&error],
                &[temp_path, dst_path],
            ),
            Self::Sync(error) => crate::error::with_recovery_context(
                crate::error::coded_error(
                    io::ErrorKind::Other,
                    crate::error::ErrorCode::OutcomeUnclear,
                    format!(
                        "Die neue Datei wurde nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({error}). Recoverypfade: sichtbare neue Datei: {}; der frühere Temp-Pfad ist jetzt frei: {}",
                        dst_path.display(),
                        dst_path.display(),
                        temp_path.display(),
                    ),
                ),
                &[&error],
                &[temp_path, dst_path],
            ),
        }
    }
}

fn rename_temp_into_place_phase(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    temp_released: &mut bool,
) -> Result<(), TempRenameFailure> {
    match rename_noreplace_with_phase(dst, temp_path, dst_path) {
        Ok(()) => {
            *temp_released = true;
            Ok(())
        }
        Err(SyncedRenameFailure::RenameUnchanged(error)) => {
            Err(TempRenameFailure::Unchanged(error))
        }
        Err(SyncedRenameFailure::RenameOutcomeUnclear(error)) => {
            *temp_released = true;
            Err(TempRenameFailure::OutcomeUnclear(error))
        }
        Err(SyncedRenameFailure::Sync(error)) => {
            *temp_released = true;
            Err(TempRenameFailure::Sync(error))
        }
    }
}

/// No-Replace-Rename für Quarantäne- und Rückwege. Anders als ein nacktes
/// `io::Result` hält dieser Helfer fest, ob der Rename sicher unverändert blieb,
/// möglicherweise wirkte oder sichtbar wirkte und erst der Verzeichnis-Sync
/// scheiterte. Nur so darf der Queue-Aufrufer einen Lauf später wiederholen.
fn rename_noreplace_recovery_aware(vfs: &dyn Vfs, from: &Path, to: &Path) -> io::Result<()> {
    match rename_noreplace_with_phase(vfs, from, to) {
        Ok(()) => Ok(()),
        Err(SyncedRenameFailure::RenameUnchanged(error)) => Err(error),
        Err(SyncedRenameFailure::RenameOutcomeUnclear(error)) => Err(crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Rename-Ausgang ist unklar ({error}); mögliche Recoverypfade: {} und {}",
                    from.display(),
                    to.display()
                ),
            ),
            &[&error],
            &[from, to]),
        ),
        Err(SyncedRenameFailure::Sync(error)) => Err(crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Eintrag wurde von {} nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({error})",
                    from.display(),
                    to.display()
                ),
            ),
            &[&error],
            &[from, to]),
        ),
    }
}

fn rename_temp_into_place(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    replace: bool,
    temp_released: &mut bool,
) -> io::Result<()> {
    if replace {
        // Für den ersetzenden Rename bietet der Vfs-Vertrag keine phasengetrennte
        // Antwort. Ein Fehler kann also schon gewirkt haben, weshalb der Name hier
        // vorsorglich als freigegeben gilt. Dieser Zweig läuft nur ohne Quarantäne,
        // also in headless Kerntests.
        *temp_released = true;
        dst.rename(temp_path, dst_path)?;
        return sync_rename_parents(dst, temp_path, dst_path);
    }
    rename_temp_into_place_phase(dst, temp_path, dst_path, temp_released)
        .map_err(|failure| failure.without_rollback(temp_path, dst_path))
}

/// Führt den vorbereiteten Temp-Stand als endgültiges Ziel ein.
///
/// `temp_released` wird gesetzt, sobald der Temp-Name freigegeben sein kann. Der
/// Aufrufer darf danach nur noch identitätsgebunden aufräumen, weil ein fremder
/// Prozess denselben Namen sofort neu belegen darf.
struct CommitTempRequest<'a> {
    dst: &'a dyn Vfs,
    temp_path: &'a Path,
    dst_path: &'a Path,
    relative_path: &'a Path,
    replacing: bool,
    expected_target_proof: Option<&'a super::SyncTargetProof>,
    expected_target_hash: Option<&'a str>,
    quarantine_run_proof: Option<&'a super::SyncTargetProof>,
    safety: &'a Safety,
    temp_released: &'a mut bool,
    transferred_size: u64,
    transferred_hash: [u8; 32],
}

fn commit_temp(request: CommitTempRequest<'_>) -> io::Result<bool> {
    let CommitTempRequest {
        dst,
        temp_path,
        dst_path,
        relative_path,
        replacing,
        expected_target_proof,
        expected_target_hash,
        quarantine_run_proof,
        safety,
        temp_released,
        transferred_size,
        transferred_hash,
    } = request;
    guard_path(safety.sandbox(), temp_path, "Temporärdatei")?;
    guard_path(safety.sandbox(), dst_path, "Schreibziel")?;

    if let Some(execution) = safety.local_execution() {
        // Der Sync behält seinen bestätigten Quellhash und Zielvorzustand. Nur
        // der versiegelte normale Einstieg darf den gemeinsamen schwächeren
        // Commit nutzen; stärkere Ersetzungen behalten die Quarantäne.
        if !replacing || execution.irreversible_replacements > 0 {
            let replacement = if replacing {
                let proof = expected_target_proof.ok_or_else(|| {
                    plan_changed(
                        &relative_path.to_string_lossy(),
                        "Ersetzung ohne bestätigten Zielbeleg",
                    )
                })?;
                let hash = expected_target_hash.ok_or_else(|| {
                    plan_changed(
                        &relative_path.to_string_lossy(),
                        "Ersetzung ohne bestätigten Zielhash",
                    )
                })?;
                let hash = blake3::Hash::from_hex(hash).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Ungültiger Zielhash")
                })?;
                Some(crate::fileops::FileConflictProof::from_listed(
                    dst_path,
                    &proof.as_listed_for_path(dst_path)?,
                    Some(*hash.as_bytes()),
                )?)
            } else {
                None
            };
            crate::fileops::commit_sync_file(
                dst,
                temp_path,
                dst_path,
                transferred_size,
                transferred_hash,
                replacement.as_ref(),
            )
            .map_err(|failure| failure.into_sync_error(temp_released))?;
            *temp_released = true;
            return Ok(false);
        }
    }

    if !replacing {
        if optional_stat(dst, dst_path)?.is_some() {
            return Err(plan_changed(
                &relative_path.to_string_lossy(),
                "Ziel entstand während des Kopierens",
            ));
        }
        rename_temp_into_place(dst, temp_path, dst_path, false, temp_released)?;
        return Ok(false);
    }

    if let Some(root) = &safety.quarantine {
        let backup = checked_join(root, relative_path)?;
        guard_path(safety.sandbox(), &backup, "Quarantänepfad")?;
        if optional_stat(dst, &backup)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Quarantänepfad existiert bereits: {}", backup.display()),
            ));
        }
        if let Some(parent) = backup.parent() {
            dst.create_dir_all(parent)?;
        }
        let parked_parent = backup.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quarantänepfad besitzt keinen Elternordner",
            )
        })?;
        let parked_parent_proof =
            super::SyncTargetProof::from_listed(&listed_entry(dst, parked_parent)?)?;
        let quarantine_run_proof = quarantine_run_proof.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quarantäne-Commit besitzt keinen gebundenen Laufordner",
            )
        })?;
        record_quarantine_intent(dst, root, relative_path, false, QuarantineReason::Replaced)?;
        // Hier wandert das alte Ziel in die Quarantäne; der Temp-Name bleibt davon
        // unberührt und gehört weiterhin uns.
        claim_target_into_quarantine(
            dst,
            dst_path,
            &backup,
            root,
            quarantine_run_proof,
            &parked_parent_proof,
            expected_target_proof,
        )?;
        verify_parked_target(
            dst,
            &backup,
            dst_path,
            expected_target_proof,
            expected_target_hash,
            false,
        )?;
        if let Err(commit_failure) =
            rename_temp_into_place_phase(dst, temp_path, dst_path, temp_released)
        {
            // Scheitert erst der Verzeichnis-Sync nach erfolgreichem Rename, ist die
            // neue Datei bereits sichtbar. Dann darf der alte Stand nicht darüber
            // zurückverschoben werden; beide Zustände bleiben klar benannt erhalten.
            // Die folgenden Umformulierungen dürfen den maschinenlesbaren Code des
            // Commit-Fehlers nicht verlieren: Nur er sagt dem Aufrufer, ob die
            // Wirkung schon eingetreten sein kann. `io::Error::other` würde ihn auf
            // `io` zurücksetzen und die Mutation wieder blind wiederholbar aussehen
            // lassen.
            if let TempRenameFailure::Sync(sync_error) = &commit_failure {
                return Err(crate::error::with_recovery_context(
                    crate::error::coded_error(
                        io::ErrorKind::Other,
                        crate::error::ErrorCode::OutcomeUnclear,
                        format!(
                            "Die neue Datei ist nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({sync_error}); das alte Ziel bleibt in {}",
                            dst_path.display(),
                            backup.display()
                        ),
                    ),
                    &[sync_error],
                    &[temp_path, dst_path, &backup]),
                );
            }
            let commit_error = match commit_failure {
                TempRenameFailure::Unchanged(error) => error,
                TempRenameFailure::OutcomeUnclear(error) => {
                    // Der gemeinsame Phasenvertrag bindet in diesem Fall beide
                    // Namen als Recoverypfade. Auch ein anschließendes `stat` eines
                    // einzelnen Namens beweist keinen mutationsfreien Ausgang und
                    // autorisiert deshalb keinen zweiten, blinden Rename.
                    return Err(crate::error::with_recovery_context(
                        crate::error::coded_error(
                            io::ErrorKind::Other,
                            crate::error::ErrorCode::OutcomeUnclear,
                            format!(
                                "Das Commit-Rename meldete einen Fehler nach möglicher Wirkung ({error}); mögliche Recoverypfade: {} und {}; das alte Ziel bleibt in {}",
                                temp_path.display(),
                                dst_path.display(),
                                backup.display()
                            ),
                        ),
                        &[&error],
                        &[temp_path, dst_path, &backup]),
                    );
                }
                TempRenameFailure::Sync(_) => unreachable!("oben behandelt"),
            };
            match optional_stat(dst, temp_path) {
                Ok(None) => {
                    return Err(crate::error::with_recovery_context(
                        crate::error::coded_error(
                            io::ErrorKind::Other,
                            crate::error::ErrorCode::OutcomeUnclear,
                            format!(
                                "Der Commit-Ausgang ist unklar ({commit_error}); der Temp-Name ist frei und das mögliche neue Ziel liegt bei {}; das alte Ziel bleibt in {}",
                                dst_path.display(),
                                backup.display()
                            ),
                        ),
                        &[&commit_error],
                        &[temp_path, dst_path, &backup]),
                    )
                }
                Ok(Some(_)) => {}
                Err(state_error) => {
                    return Err(crate::error::with_recovery_context(
                        crate::error::coded_error(
                            io::ErrorKind::Other,
                            crate::error::ErrorCode::OutcomeUnclear,
                            format!(
                            "Commit scheiterte ({commit_error}); der Temp-Zustand konnte nicht geklärt werden ({state_error}); altes Ziel bleibt in {}",
                            backup.display()
                        ),
                        ),
                        &[&commit_error, &state_error],
                        &[temp_path, dst_path, &backup]),
                    )
                }
            }
            // Bestmöglicher Rollback: das alte Ziel sofort zurück an seinen Platz.
            // Auch der Rollback darf eine inzwischen entstandene Datei nicht ersetzen.
            let restore = rename_noreplace_recovery_aware(dst, &backup, dst_path);
            return match restore {
                Ok(()) => {
                    let code = crate::error::error_code(&commit_error);
                    Err(crate::error::with_recovery_context(
                        crate::error::coded_error(
                            commit_error.kind(),
                            code,
                            format!(
                                "Commit fehlgeschlagen ({commit_error}); das alte Ziel wurde erfolgreich aus {} zurück nach {} verschoben. Der eigene Tempbestand bei {} bleibt zur Prüfung erhalten",
                                backup.display(),
                                dst_path.display(),
                                temp_path.display()
                            ),
                        ),
                        &[&commit_error],
                        &[temp_path, dst_path]),
                    )
                }
                Err(restore_error) => {
                    // Ein unklarer Rückweg ist selbst dann nicht wiederholbar, wenn
                    // der vorausgehende Commit nachweislich unverändert blieb. Der
                    // stärkere Code darf deshalb nicht vom früheren Fehler verdeckt
                    // werden.
                    let code =
                        if crate::error_code(&restore_error) == crate::ErrorCode::OutcomeUnclear {
                            crate::ErrorCode::OutcomeUnclear
                        } else {
                            crate::error_code(&commit_error)
                        };
                    Err(crate::error::with_recovery_context(
                        crate::error::coded_error(
                            io::ErrorKind::Other,
                            code,
                            format!(
                            "Commit fehlgeschlagen ({commit_error}); altes Ziel liegt in {} und konnte nicht zurückverschoben werden ({restore_error})",
                            backup.display()
                        ),
                        ),
                        &[&commit_error, &restore_error],
                        &[temp_path, dst_path, &backup]),
                    )
                }
            };
        }
        return Ok(true);
    }

    // Headless-Kerntests dürfen ohne Quarantäne arbeiten; der Tauri-Ausführungspfad
    // stellt für reale Läufe immer eine bereit. Rename ersetzt auf macOS/Linux atomar.
    rename_temp_into_place(dst, temp_path, dst_path, true, temp_released)?;
    Ok(false)
}

/// Überzähliges Zielobjekt entweder wiederherstellbar verschieben oder — nur in
/// ausdrücklich so konfigurierten Kerntests — direkt entfernen.
pub(super) struct RemoveOrQuarantineRequest<'a> {
    pub(super) dst: &'a dyn Vfs,
    pub(super) target: &'a Path,
    pub(super) relative_path: &'a Path,
    pub(super) is_dir: bool,
    pub(super) expected_target_proof: &'a super::SyncTargetProof,
    pub(super) expected_hash: Option<&'a str>,
    pub(super) quarantine_run_proof: Option<&'a super::SyncTargetProof>,
    pub(super) safety: &'a Safety,
}

pub(super) fn remove_or_quarantine(request: RemoveOrQuarantineRequest<'_>) -> io::Result<bool> {
    let RemoveOrQuarantineRequest {
        dst,
        target,
        relative_path,
        is_dir,
        expected_target_proof,
        expected_hash,
        quarantine_run_proof,
        safety,
    } = request;
    guard_path(safety.sandbox(), target, "Löschziel")?;
    if let Some(root) = &safety.quarantine {
        let backup = checked_join(root, relative_path)?;
        guard_path(safety.sandbox(), &backup, "Quarantänepfad")?;
        if optional_stat(dst, &backup)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Quarantänepfad existiert bereits: {}", backup.display()),
            ));
        }
        if let Some(parent) = backup.parent() {
            dst.create_dir_all(parent)?;
        }
        let parked_parent = backup.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quarantänepfad besitzt keinen Elternordner",
            )
        })?;
        let parked_parent_proof =
            super::SyncTargetProof::from_listed(&listed_entry(dst, parked_parent)?)?;
        let quarantine_run_proof = quarantine_run_proof.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mirror-Quarantäne besitzt keinen gebundenen Laufordner",
            )
        })?;
        record_quarantine_intent(
            dst,
            root,
            relative_path,
            is_dir,
            QuarantineReason::MirrorRemoval,
        )?;
        claim_target_into_quarantine(
            dst,
            target,
            &backup,
            root,
            quarantine_run_proof,
            &parked_parent_proof,
            Some(expected_target_proof),
        )?;
        verify_parked_target(
            dst,
            &backup,
            target,
            Some(expected_target_proof),
            expected_hash,
            is_dir,
        )?;
        return Ok(true);
    }
    if is_dir {
        dst.remove_dir_all(target)?;
    } else {
        dst.remove_file(target)?;
    }
    sync_parent(dst, target)?;
    Ok(false)
}

/// Verwendet die stärkste vom Provider bewiesene Claim-Primitive. Ein Provider mit
/// bedingtem Rename bindet sowohl den bestätigten Zieleintrag als auch den bereits
/// angelegten Quarantäne-Elternordner atomar an die Wirkung. Lokale POSIX-Dateisysteme
/// besitzen dafür keine portable Primitive; sie fallen auf den exklusiven Rename mit
/// anschließendem Identitäts- und Hash-Readback zurück.
fn claim_target_into_quarantine(
    vfs: &dyn Vfs,
    target: &Path,
    parked: &Path,
    quarantine_root: &Path,
    quarantine_root_proof: &super::SyncTargetProof,
    parked_parent_proof: &super::SyncTargetProof,
    expected: Option<&super::SyncTargetProof>,
) -> io::Result<()> {
    let parked_parent = parked.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Quarantänepfad besitzt keinen Elternordner",
        )
    })?;
    verify_directory_proof(vfs, quarantine_root, quarantine_root_proof)?;
    if !vfs.supports_rename_noreplace_if_matches_in_bound_parent() {
        rename_noreplace_recovery_aware(vfs, target, parked)?;
        return verify_park_parent_bindings_after_claim(
            vfs,
            quarantine_root,
            quarantine_root_proof,
            parked_parent,
            parked_parent_proof,
            target,
            parked,
        );
    }
    let expected = expected.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Bedingter Quarantäne-Claim besitzt keinen Zielbeleg",
        )
    })?;
    let target_proof = expected.as_listed_for_path(target)?;
    let parent_proof = parked_parent_proof.as_listed_for_path(parked_parent)?;
    vfs.rename_noreplace_if_matches_in_bound_parent(
        target,
        parked,
        &target_proof,
        parked_parent,
        &parent_proof,
    )
    .map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::ErrorCode::PlanDrift,
                format!(
                    "Der bedingte Quarantäne-Claim lehnte den bestätigten Ziel- oder Elternbeleg ab ({error}); kein Eintrag wurde durch diesen Claim verändert. Bitte neue Vorschau erstellen"
                ),
            ),
            &[&error],
            &[],
        )
    })?;
    sync_rename_parents(vfs, target, parked).map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Der bestätigte Zieleintrag wurde von {} nach {} geclaimt, aber der Verzeichnis-Sync scheiterte ({error})",
                    target.display(),
                    parked.display()
                ),
            ),
            &[&error],
            &[target, parked],
        )
    })?;
    verify_park_parent_bindings_after_claim(
        vfs,
        quarantine_root,
        quarantine_root_proof,
        parked_parent,
        parked_parent_proof,
        target,
        parked,
    )
}

fn verify_park_parent_bindings_after_claim(
    vfs: &dyn Vfs,
    quarantine_root: &Path,
    quarantine_root_proof: &super::SyncTargetProof,
    parked_parent: &Path,
    parked_parent_proof: &super::SyncTargetProof,
    original: &Path,
    parked: &Path,
) -> io::Result<()> {
    let verified = (|| {
        verify_directory_proof(vfs, quarantine_root, quarantine_root_proof)?;
        verify_directory_proof(vfs, parked_parent, parked_parent_proof)
    })();
    verified.map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Der Zielbestand wurde geclaimt, aber Quarantänelauf oder Park-Elternordner wurde ausgetauscht ({error}). Kein weiterer Rename wurde versucht. Recoverypfade: früherer Zielname: {}; geparkter Bestand: {}; erwarteter Quarantänelauf: {}",
                    original.display(),
                    parked.display(),
                    quarantine_root.display()
                ),
            ),
            &[&error],
            &[original, parked, quarantine_root],
        )
    })
}

/// Vergleicht den aktuellen Zielnamen mit dem im bestätigten Plan gespeicherten
/// Eintragsbeleg. Lokale Ziele nutzen Gerät/Inode aus `stat`; besitzt der Plan einen
/// Provider-Token, kommt er aus demselben vollständigen Elternlisting wie der Name.
pub(super) fn verify_target_proof(
    vfs: &dyn Vfs,
    path: &Path,
    expected: Option<&super::SyncTargetProof>,
) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let current = vfs.stat(path)?;
    if !expected.matches_entry(path, &current) {
        return Err(plan_changed(
            &path.to_string_lossy(),
            "Zielidentität wurde seit der Vorschau ausgetauscht",
        ));
    }
    if expected.provider_token.is_some() {
        let listed = listed_entry(vfs, path)?;
        if !expected.matches_listed_at(path, &listed) {
            return Err(plan_changed(
                &path.to_string_lossy(),
                "Provider-Version des Ziels wurde seit der Vorschau ausgetauscht",
            ));
        }
    }
    Ok(())
}

fn verify_directory_proof(
    vfs: &dyn Vfs,
    path: &Path,
    expected: &super::SyncTargetProof,
) -> io::Result<()> {
    let current = listed_entry(vfs, path)?;
    if expected.matches_stable_directory_at(path, &current) {
        Ok(())
    } else {
        Err(plan_changed(
            &path.to_string_lossy(),
            "Verzeichnisidentität wurde seit ihrer Bindung ausgetauscht",
        ))
    }
}

/// Prüft nach dem exklusiven Claim, dass tatsächlich der bestätigte Zielbestand
/// unter dem Quarantänenamen liegt. POSIX kann Identitätsvergleich und Rename nicht
/// atomar koppeln; der Readback verhindert deshalb zumindest, dass ein ausgetauschter
/// Nachfolger als erfolgreicher Quarantäneeintrag oder abgeschlossener Lauf gilt.
/// Nach der sichtbaren Mutation ist jeder Belegfehler `OutcomeUnclear`: Ein blinder
/// Rückrename würde nur ein zweites Rennen öffnen.
fn verify_parked_target(
    vfs: &dyn Vfs,
    parked: &Path,
    original: &Path,
    expected: Option<&super::SyncTargetProof>,
    expected_hash: Option<&str>,
    is_dir: bool,
) -> io::Result<()> {
    let verified = (|| -> io::Result<()> {
        let expected = expected.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quarantäne-Claim besitzt keinen bestätigten Zielbeleg",
            )
        })?;
        let current = vfs.stat(parked)?;
        if !expected.matches_identity(&current) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Identität am Quarantänenamen stimmt nicht mit dem bestätigten Ziel überein",
            ));
        }
        if expected.provider_token.is_some() {
            let listed = listed_entry(vfs, parked)?;
            if !expected.matches_moved_listed(&listed) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Provider-Version am Quarantänenamen stimmt nicht mit dem bestätigten Ziel überein",
                ));
            }
        }
        if let Some(expected_hash) = expected_hash {
            let actual = if is_dir {
                hash_tree(vfs, parked)?.0
            } else {
                hash_file_hex(vfs, parked)?
            };
            if actual != expected_hash {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Inhalt am Quarantänenamen stimmt nicht mit dem bestätigten Ziel überein",
                ));
            }
        }
        Ok(())
    })();
    verified.map_err(|error| {
        crate::error::with_recovery_context(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!(
                    "Der Zielname wurde nach {} geclaimt, der Identitäts-Readback scheiterte jedoch ({error}). Kein weiterer Rename wurde versucht. Recoverypfade: früherer Zielname: {}; geclaimter Bestand: {}",
                    parked.display(),
                    original.display(),
                    parked.display()
                ),
            ),
            &[&error],
            &[original, parked],
        )
    })
}

fn listed_entry(vfs: &dyn Vfs, path: &Path) -> io::Result<ProofListEntry> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Eintragsbeleg besitzt keinen Elternordner: {}",
                path.display()
            ),
        )
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Eintragsbeleg besitzt keinen sicheren Namen: {}",
                    path.display()
                ),
            )
        })?;
    let mut matches = complete_listing(vfs, parent)?
        .into_iter()
        .filter(|listed| listed.raw_name == name.as_bytes());
    let found = matches.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Eintrag fehlt im vollständigen Elternlisting: {}",
                path.display()
            ),
        )
    })?;
    if matches.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Eintrag ist im Elternlisting nicht eindeutig: {}",
                path.display()
            ),
        ));
    }
    Ok(found)
}

/// Regulären, nicht verlinkten Dateizustand für Plan-Drift-Prüfungen lesen.
/// Snapshot für einen Kopier-Commit. Zusätzlich zu Größe/mtime gehört der Modus zur
/// Planbindung, damit eine parallele Rechteänderung nicht unbemerkt überschrieben wird.
pub(super) fn regular_copy_snapshot(vfs: &dyn Vfs, path: &Path) -> io::Result<FileSnapshot> {
    let entry = vfs.stat(path)?;
    snapshot_from_entry(path, &entry)
}

pub(super) fn current_optional_copy_snapshot(
    vfs: &dyn Vfs,
    path: &Path,
) -> io::Result<Option<FileSnapshot>> {
    match optional_stat(vfs, path)? {
        Some(entry) => snapshot_from_entry(path, &entry).map(Some),
        None => Ok(None),
    }
}

fn snapshot_from_entry(path: &Path, entry: &crate::vfs::Entry) -> io::Result<FileSnapshot> {
    if entry.is_symlink || !entry.is_file {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Keine reguläre, unverlinkte Datei: {}", path.display()),
        ));
    }
    Ok((entry.size, entry.mtime, entry.mode))
}

pub(super) fn hash_file_hex(vfs: &dyn Vfs, path: &Path) -> io::Result<String> {
    hash_vfs_file(vfs, path).map(|hash| hash.to_hex().to_string())
}

/// Kanonischer Digest eines zu quarantänierenden Teilbaums. Neben Dateiinhalten werden
/// Namen, Typen und die bereits plan-gebundenen Stufe-1-Metadaten aufgenommen. Damit
/// erkennt der Hashmodus auch Änderungen tief in einem als eine Aktion dargestellten
/// `DeleteDir`.
pub(super) fn hash_tree(vfs: &dyn Vfs, root: &Path) -> io::Result<(String, u64, u64)> {
    fn visit(
        vfs: &dyn Vfs,
        path: &Path,
        relative: &Path,
        hasher: &mut blake3::Hasher,
        files: &mut u64,
        bytes: &mut u64,
    ) -> io::Result<()> {
        let entry = vfs.stat(path)?;
        let relative_bytes = relative.to_string_lossy();
        hasher.update(&(relative_bytes.len() as u64).to_le_bytes());
        hasher.update(relative_bytes.as_bytes());
        hasher.update(&[
            u8::from(entry.is_dir),
            u8::from(entry.is_file),
            u8::from(entry.is_symlink),
        ]);
        hasher.update(&entry.size.to_le_bytes());
        hasher.update(&entry.mtime.unwrap_or(i64::MIN).to_le_bytes());
        hasher.update(&entry.mode.unwrap_or(u32::MAX).to_le_bytes());

        if entry.is_file && !entry.is_symlink {
            *files = files.saturating_add(1);
            let mut reader = vfs.open_read(path)?;
            let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
            let mut remaining = entry.size;
            while remaining > 0 {
                let wanted = remaining.min(buffer.len() as u64) as usize;
                let read = reader.read(&mut buffer[..wanted])?;
                if read == 0 {
                    return Err(plan_changed(
                        &path.to_string_lossy(),
                        "Quellstream endete vor der gebundenen Größe",
                    ));
                }
                hasher.update(&buffer[..read]);
                remaining -= read as u64;
            }
            if reader.read(&mut [0u8; 1])? != 0 {
                return Err(plan_changed(
                    &path.to_string_lossy(),
                    "Quellstream überschreitet die gebundene Größe",
                ));
            }
            *bytes = bytes.saturating_add(entry.size);
        } else if entry.is_dir && !entry.is_symlink {
            let mut children = complete_listing(vfs, path)?;
            children.sort_by(|left, right| left.entry.name.cmp(&right.entry.name));
            for child in children {
                visit(
                    vfs,
                    &path.join(&child.entry.name),
                    &relative.join(&child.entry.name),
                    hasher,
                    files,
                    bytes,
                )?;
            }
        }
        Ok(())
    }

    let mut hasher = blake3::Hasher::new();
    let mut files = 0;
    let mut bytes = 0;
    visit(
        vfs,
        root,
        Path::new(""),
        &mut hasher,
        &mut files,
        &mut bytes,
    )?;
    Ok((hasher.finalize().to_hex().to_string(), files, bytes))
}

pub(super) fn plan_changed(path: &str, reason: &str) -> io::Error {
    // Trägt zusätzlich den maschinenlesbaren Code `PlanDrift`, damit GUI/CLI
    // gezielt „neue Vorschau erstellen“ anbieten können (siehe crate::error).
    crate::error::coded_error(
        io::ErrorKind::Interrupted,
        crate::error::ErrorCode::PlanDrift,
        format!(
            "Plan ist nicht mehr aktuell bei „{path}“: {reason}. Bitte neue Vorschau erstellen."
        ),
    )
}

pub(super) fn quarantine_label(safety: &Safety) -> Option<String> {
    safety
        .quarantine
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{Entry, MutationFailure};
    use crate::LocalVfs;

    fn assert_recovery_paths(error: &io::Error, expected: &[&Path]) {
        let actual = crate::error::recovery_paths(error);
        for path in expected {
            assert!(
                actual.contains(&path.to_string_lossy().into_owned()),
                "Recoverypfad {} fehlt strukturiert in {actual:?}: {error}",
                path.display()
            );
        }
    }

    #[test]
    fn c2_sync_frueher_lesefehler_schliesst_staging_oder_bilanziert_reste() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages, FaultVfs};
        for finish_error in [false, true] {
            let (_temp, root, source, target, local) = fixture();
            let snapshot = regular_copy_snapshot(&local, &source).unwrap();
            let source_vfs = FaultVfs {
                inner: Box::new(LocalVfs::rooted(&root).unwrap()),
                read_error: true,
                cancel_on_read: None,
                finish_error: false,
            };
            let target_vfs = FaultVfs {
                inner: Box::new(local),
                read_error: false,
                cancel_on_read: None,
                finish_error,
            };
            let result = copy_one(
                CopyFileRequest {
                    src: &source_vfs,
                    src_path: &source,
                    dst: &target_vfs,
                    dst_path: &target.join("datei"),
                    relative_path: Path::new("datei"),
                    expected_size: snapshot.0,
                    expected_source_mtime: snapshot.1,
                    expected_source_mode: snapshot.2,
                    expected_target: None,
                    expected_target_proof: None,
                    expected_source_hash: None,
                    expected_target_hash: None,
                    expected_metadata: None,
                    quarantine_run_proof: None,
                    safety: &Safety::headless_test(),
                },
                &Cancel::new(),
                &mut Progress::default(),
                &mut |_| {},
            );
            let error = result.err().expect("Quell-Lesefehler");
            assert_eq!(
                crate::error_code(&error),
                crate::ErrorCode::PermissionDenied
            );
            assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
            assert!(!target.join("datei").exists());
            let retained = stages(&root);
            if finish_error {
                assert_eq!(retained.len(), 1);
                let path = retained[0].to_string_lossy().into_owned();
                assert!(error.to_string().contains(&path));
                assert!(crate::error::recovery_paths(&error).contains(&path));
                assert_eq!(
                    std::fs::read(retained[0].join("erhaltener-datenrest")).unwrap(),
                    b"erhalten"
                );
            } else {
                assert!(retained.is_empty());
            }
        }
    }

    #[test]
    fn c2_sync_abbruch_schliesst_leeres_staging() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages, FaultVfs};
        let (_temp, root, source, target, local) = fixture();
        let snapshot = regular_copy_snapshot(&local, &source).unwrap();
        let cancel = Cancel::new();
        let source_vfs = FaultVfs {
            inner: Box::new(LocalVfs::rooted(&root).unwrap()),
            read_error: false,
            cancel_on_read: Some(cancel.clone()),
            finish_error: false,
        };
        let result = copy_one(
            CopyFileRequest {
                src: &source_vfs,
                src_path: &source,
                dst: &local,
                dst_path: &target.join("datei"),
                relative_path: Path::new("datei"),
                expected_size: snapshot.0,
                expected_source_mtime: snapshot.1,
                expected_source_mode: snapshot.2,
                expected_target: None,
                expected_target_proof: None,
                expected_source_hash: None,
                expected_target_hash: None,
                expected_metadata: None,
                quarantine_run_proof: None,
                safety: &Safety::headless_test(),
            },
            &cancel,
            &mut Progress::default(),
            &mut |_| {},
        )
        .unwrap();
        match result {
            CopyResult::Cancelled { recovery_paths } => assert!(recovery_paths.is_empty()),
            _ => panic!("Abbruch erwartet"),
        }
        assert!(stages(&root).is_empty());
        assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
        assert!(!target.join("datei").exists());
    }

    #[test]
    fn c2_hardlink_frueher_ankerfehler_schliesst_staging() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages};
        let (_temp, root, source, target, local) = fixture();
        std::fs::hard_link(&source, source.with_file_name("anker")).unwrap();
        let manifest = local
            .metadata_manifest(&source)
            .unwrap()
            .with_hardlink_group(Some(1))
            .unwrap();
        let metadata = MetadataTransfer {
            source: manifest,
            target: None,
            owner: false,
            acl: false,
            incompatible_acl_model: false,
            normal_mode: false,
            special_mode_bits: false,
            xattrs: false,
            resource_fork: false,
            sparse_extents: false,
            hardlinks: true,
        };
        let snapshot = regular_copy_snapshot(&local, &source).unwrap();
        let hash = hash_file_hex(&local, &source).unwrap();
        let result = create_hardlink_one(HardlinkRequest {
            src: &local,
            src_root: source.parent().unwrap(),
            dst: &local,
            dst_root: &target,
            path: "datei",
            link_to: "anker",
            expected_source: snapshot,
            expected_target: None,
            expected_target_proof: None,
            source_hash: &hash,
            target_hash: None,
            metadata: &metadata,
            quarantine_run_proof: None,
            safety: &Safety::headless_test(),
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(stages(&root).is_empty());
        assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
        assert!(!target.join("datei").exists());
    }

    #[test]
    fn c2_sync_fehler_nach_temp_anlage_schliesst_staging() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages};
        let (_temp, root, source, target, local) = fixture();
        let snapshot = regular_copy_snapshot(&local, &source).unwrap();
        let result = copy_one(
            CopyFileRequest {
                src: &local,
                src_path: &source,
                dst: &local,
                dst_path: &target.join("datei"),
                relative_path: Path::new("datei"),
                expected_size: snapshot.0,
                expected_source_mtime: snapshot.1,
                expected_source_mode: snapshot.2,
                expected_target: None,
                expected_target_proof: None,
                expected_source_hash: Some("absichtlich-falscher-hash"),
                expected_target_hash: None,
                expected_metadata: None,
                quarantine_run_proof: None,
                safety: &Safety::headless_test(),
            },
            &Cancel::new(),
            &mut Progress::default(),
            &mut |_| {},
        );
        assert_eq!(
            crate::error_code(&result.err().unwrap()),
            crate::ErrorCode::PlanDrift
        );
        assert!(stages(&root).is_empty());
        assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
        assert!(!target.join("datei").exists());
    }

    fn planned_target_proof(vfs: &dyn Vfs, path: &Path) -> super::super::SyncTargetProof {
        super::super::SyncTargetProof::from_listed(&listed_entry(vfs, path).unwrap()).unwrap()
    }

    fn start_test_quarantine(
        vfs: &dyn Vfs,
        run_root: &Path,
        target_root: &Path,
    ) -> (Safety, SyncQuarantineRun) {
        let mut safety = Safety::headless_test();
        safety.quarantine = Some(run_root.to_path_buf());
        let run = start_sync_quarantine_run(vfs, run_root, target_root, &safety).unwrap();
        (safety, run)
    }

    #[test]
    fn hardlink_fehler_loescht_keinen_neu_belegten_temp_namen() {
        let directory = tempfile::tempdir().unwrap();
        let temp = directory.path().join(".ziel.link-temp");
        let committed = directory.path().join("ziel");
        std::fs::write(&temp, b"eigener Link").unwrap();
        let vfs = LocalVfs::new();
        let proof = ProofListEntry {
            entry: Entry {
                name: ".ziel.link-temp".into(),
                is_dir: false,
                is_file: true,
                is_symlink: false,
                size: 12,
                mtime: None,
                created: None,
                owner: None,
                group: None,
                extension: None,
                mode: None,
                device: None,
                inode: None,
                hard_links: None,
            },
            raw_name: b".ziel.link-temp".to_vec(),
            safely_decoded: true,
            provider_token: None,
        };

        // Der Commit hat den belegten Eintrag bereits verschoben. Danach belegt
        // ein fremder Prozess denselben Temp-Namen neu, bevor der Syncfehler beim
        // Aufrufer ankommt.
        std::fs::rename(&temp, &committed).unwrap();
        std::fs::write(&temp, b"fremder Inhalt").unwrap();
        assert!(cleanup_bound_temp(&vfs, &temp, Some(&proof)).is_err());

        assert_eq!(std::fs::read(&temp).unwrap(), b"fremder Inhalt");
        assert_eq!(std::fs::read(&committed).unwrap(), b"eigener Link");
    }

    #[test]
    fn erfolgreicher_rename_gibt_den_temp_namen_frei() {
        let directory = tempfile::tempdir().unwrap();
        // Auf macOS ist der Temp-Pfad symlinkbehaftet; das descriptor-relative
        // LocalVfs folgt Symlinks bewusst nicht.
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        std::fs::write(&temp, b"fertiger Inhalt").unwrap();
        let vfs = LocalVfs::new();
        let mut temp_released = false;

        rename_temp_into_place(&vfs, &temp, &target, false, &mut temp_released).unwrap();

        // Ab hier darf ein fremder Prozess denselben Temp-Namen neu belegen; der
        // Aufrufer darf ihn deshalb nicht mehr pfadbasiert löschen.
        assert!(temp_released);
        assert_eq!(std::fs::read(&target).unwrap(), b"fertiger Inhalt");
    }

    #[test]
    fn gescheiterter_rename_haelt_den_temp_namen() {
        let directory = tempfile::tempdir().unwrap();
        // Auf macOS ist der Temp-Pfad symlinkbehaftet; das descriptor-relative
        // LocalVfs folgt Symlinks bewusst nicht.
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        std::fs::write(&temp, b"fertiger Inhalt").unwrap();
        std::fs::write(&target, b"belegtes Ziel").unwrap();
        let vfs = LocalVfs::new();
        let mut temp_released = false;

        // `rename_noreplace` scheitert am belegten Ziel, bevor irgendetwas bewegt
        // wurde. Der Temp-Name gehört danach unverändert uns.
        let error =
            rename_temp_into_place(&vfs, &temp, &target, false, &mut temp_released).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!temp_released);
        assert_eq!(std::fs::read(&temp).unwrap(), b"fertiger Inhalt");
        assert_eq!(std::fs::read(&target).unwrap(), b"belegtes Ziel");
    }

    #[test]
    fn review_streamgroesse_begrenzt_schreiben_und_baumhash() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::local_fs::write_acl_exact(&root, None).unwrap();
        let source = root.join("quelle");
        let destination = root.join("zielordner");
        std::fs::create_dir(&destination).unwrap();
        let target = destination.join("ziel");
        std::fs::write(&source, [7u8]).unwrap();
        let dst = LocalVfs::rooted(&root).unwrap();
        for content in [vec![], vec![7u8; 2 * 1024 * 1024]] {
            let mut src = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX));
            src.read_override = Some(content);
            let staged = dst.staging_scope(&target, None).unwrap().unwrap();
            let mut progress = Progress::default();
            let error = transfer_file_data(
                TransferFileRequest {
                    src: &src,
                    src_path: &source,
                    dst: staged.as_ref(),
                    dst_path: &target,
                    expected_size: 1,
                    sparse_extents: None,
                    sandbox: None,
                },
                &Cancel::new(),
                &mut progress,
                &mut |_| {},
            )
            .unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
            assert!(
                progress.bytes_done <= 1,
                "Schreibbudget muss schon vor dem Fehler gelten"
            );
            assert!(!target.exists());
            staged.finish_staging().unwrap();
            assert_eq!(
                std::fs::read_dir(&destination).unwrap().count(),
                0,
                "Temp vollständig entfernt"
            );
            let error = hash_tree(&src, &source).unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        }
        let actual = hash_tree(&dst, &source).unwrap();
        assert_eq!((actual.1, actual.2), (1, 1));
    }

    #[derive(Clone, Copy)]
    enum AtomicTestFault {
        UnclearRenameOnCall(usize),
        UnchangedRenameOnCall(usize),
        UnchangedThenUnclear {
            unchanged_call: usize,
            unclear_call: usize,
        },
        SyncDir,
    }

    /// Gemeinsames lokales Testdouble für die beiden atomaren Fehlerphasen. Alle
    /// nicht ausdrücklich gestörten Methoden — einschließlich `is_local` und der
    /// phasengetrennten Rename-Antwort — behalten exakt die `LocalVfs`-Semantik.
    struct AtomicTestVfs {
        inner: LocalVfs,
        read_override: Option<Vec<u8>>,
        fault: AtomicTestFault,
        rename_calls: std::sync::atomic::AtomicUsize,
        swap_before_rename: Option<(PathBuf, PathBuf, PathBuf, PathBuf)>,
        swap_done: std::sync::atomic::AtomicBool,
        quarantine_swap_before_rename: Option<(PathBuf, PathBuf, PathBuf, PathBuf, PathBuf)>,
        quarantine_swap_done: std::sync::atomic::AtomicBool,
        swap_after_bound_create: Option<(PathBuf, PathBuf, PathBuf)>,
        bound_create_swap_done: std::sync::atomic::AtomicBool,
        strict_conditional_rename: bool,
    }

    impl AtomicTestVfs {
        fn new(fault: AtomicTestFault) -> Self {
            Self {
                inner: LocalVfs::new(),
                read_override: None,
                fault,
                rename_calls: std::sync::atomic::AtomicUsize::new(0),
                swap_before_rename: None,
                swap_done: std::sync::atomic::AtomicBool::new(false),
                quarantine_swap_before_rename: None,
                quarantine_swap_done: std::sync::atomic::AtomicBool::new(false),
                swap_after_bound_create: None,
                bound_create_swap_done: std::sync::atomic::AtomicBool::new(false),
                strict_conditional_rename: false,
            }
        }

        fn swapping_before_rename(
            mut self,
            from: PathBuf,
            to: PathBuf,
            staged_replacement: PathBuf,
            displaced_original: PathBuf,
        ) -> Self {
            self.swap_before_rename = Some((from, to, staged_replacement, displaced_original));
            self
        }

        fn with_strict_conditional_rename(mut self) -> Self {
            self.strict_conditional_rename = true;
            self
        }

        fn swapping_after_bound_create(
            mut self,
            created: PathBuf,
            staged_replacement: PathBuf,
            displaced: PathBuf,
        ) -> Self {
            self.swap_after_bound_create = Some((created, staged_replacement, displaced));
            self
        }

        fn swapping_quarantine_before_rename(
            mut self,
            target: PathBuf,
            parked: PathBuf,
            quarantine_root: PathBuf,
            staged_replacement: PathBuf,
        ) -> Self {
            let displaced = quarantine_root.with_extension("ausgetauscht");
            self.quarantine_swap_before_rename = Some((
                target,
                parked,
                quarantine_root,
                staged_replacement,
                displaced,
            ));
            self
        }

        fn apply_external_swap(&self, from: &Path, to: &Path) {
            if let Some((expected_from, expected_to, staged, displaced)) = &self.swap_before_rename
            {
                if from == expected_from
                    && to == expected_to
                    && !self
                        .swap_done
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    std::fs::rename(from, displaced).unwrap();
                    std::fs::rename(staged, from).unwrap();
                }
            }
            if let Some((expected_from, expected_to, quarantine_root, staged, displaced)) =
                &self.quarantine_swap_before_rename
            {
                if from == expected_from
                    && to == expected_to
                    && !self
                        .quarantine_swap_done
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    std::fs::rename(quarantine_root, displaced).unwrap();
                    std::fs::rename(staged, quarantine_root).unwrap();
                }
            }
        }
    }

    impl Vfs for AtomicTestVfs {
        fn is_local(&self) -> bool {
            self.inner.is_local()
        }

        fn rename_noreplace_with_removal_proof(
            &self,
            from: &Path,
            to: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            self.apply_external_swap(from, to);
            let call = self
                .rename_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if matches!(self.fault, AtomicTestFault::UnclearRenameOnCall(expected) if expected == call)
            {
                return Err(MutationFailure::OutcomeUnclear(io::Error::other(
                    "Antwort ging nach dem Rename verloren",
                )));
            }
            if matches!(self.fault, AtomicTestFault::UnchangedRenameOnCall(expected) if expected == call)
            {
                return Err(MutationFailure::Unchanged(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Commit-Rename blieb nachweislich unverändert",
                )));
            }
            if matches!(
                self.fault,
                AtomicTestFault::UnchangedThenUnclear { unchanged_call, .. }
                    if unchanged_call == call
            ) {
                return Err(MutationFailure::Unchanged(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Rename blieb nachweislich unverändert",
                )));
            }
            if matches!(
                self.fault,
                AtomicTestFault::UnchangedThenUnclear { unclear_call, .. }
                    if unclear_call == call
            ) {
                return Err(MutationFailure::OutcomeUnclear(io::Error::other(
                    "Antwort des Rückwegs ging verloren",
                )));
            }
            self.inner.rename_noreplace_with_removal_proof(from, to)
        }

        fn rename_noreplace_if_matches_in_bound_parent(
            &self,
            from: &Path,
            to: &Path,
            expected: &ProofListEntry,
            bound_parent: &Path,
            expected_parent: &ProofListEntry,
        ) -> io::Result<()> {
            if !self.strict_conditional_rename {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Testdouble unterstützt keinen bedingten Rename",
                ));
            }
            self.apply_external_swap(from, to);
            let current = listed_entry(self, from)?;
            let current_parent = listed_entry(self, bound_parent)?;
            let expected_target = super::super::SyncTargetProof::from_listed(expected)?;
            let expected_parent = super::super::SyncTargetProof::from_listed(expected_parent)?;
            if !expected_target.matches_listed_at(from, &current)
                || !expected_parent.matches_listed_at(bound_parent, &current_parent)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "bedingter Rename erkannte einen Identitätswechsel",
                ));
            }
            self.inner.rename_noreplace(from, to)
        }

        fn supports_rename_noreplace_if_matches_in_bound_parent(&self) -> bool {
            self.strict_conditional_rename
        }

        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn list_complete(&self, path: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
            self.inner.list_complete(path)
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.inner.stat(path)
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir(path)
        }

        fn create_dir_with_removal_proof(
            &self,
            path: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            self.inner
                .create_dir(path)
                .map_err(MutationFailure::Unchanged)?;
            let proof = listed_entry(&self.inner, path).map_err(MutationFailure::OutcomeUnclear)?;
            if let Some((expected, staged, displaced)) = &self.swap_after_bound_create {
                if path == expected
                    && !self
                        .bound_create_swap_done
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    std::fs::rename(path, displaced).unwrap();
                    std::fs::rename(staged, path).unwrap();
                }
            }
            Ok(self.strict_conditional_rename.then_some(proof))
        }

        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            if let Some(content) = &self.read_override {
                return Ok(Box::new(std::io::Cursor::new(content.clone())));
            }
            self.inner.open_read(path)
        }

        fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(path)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_file(path)
        }

        fn remove_if_matches(&self, path: &Path, _expected: &ProofListEntry) -> io::Result<()> {
            self.inner.remove_file(path)
        }

        fn supports_remove_if_matches(&self) -> bool {
            true
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }

        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write_new(path)
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }

        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            if matches!(self.fault, AtomicTestFault::SyncDir) {
                return Err(io::Error::other("Verzeichnis-Sync scheiterte"));
            }
            self.inner.sync_dir(path)
        }

        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }

        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }

        fn anchored_root(&self) -> Option<&Path> {
            self.inner.anchored_root()
        }

        fn anchored_identity(&self) -> Option<(u64, u64)> {
            self.inner.anchored_identity()
        }

        fn anchor_is_current(&self) -> io::Result<bool> {
            self.inner.anchor_is_current()
        }

        fn local_root_identity(&self, path: &Path) -> io::Result<crate::LocalRootIdentity> {
            self.inner.local_root_identity(path)
        }

        fn available_bytes(&self, path: &Path) -> io::Result<u64> {
            self.inner.available_bytes(path)
        }
    }

    #[test]
    fn unklarer_rename_gibt_den_temp_namen_ebenfalls_frei() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        std::fs::write(&temp, b"fertiger Inhalt").unwrap();
        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(1));
        let mut temp_released = false;

        let error =
            rename_temp_into_place(&vfs, &temp, &target, false, &mut temp_released).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Antwort ging nach dem Rename verloren"),
            "der ursprüngliche Providertext muss erhalten bleiben: {error}"
        );
        // Der Aufrufer darf diesen Fehler nicht wie einen gewöhnlichen Verbindungs-
        // oder I/O-Fehler behandeln: Der Rename kann bereits gewirkt haben.
        assert_eq!(
            crate::error::error_code(&error),
            crate::error::ErrorCode::OutcomeUnclear
        );
        assert!(
            error.to_string().contains(&temp.display().to_string()),
            "der Recoverypfad der Temp-Datei muss im Text stehen: {error}"
        );
        assert!(
            error.to_string().contains(&target.display().to_string()),
            "der Recoverypfad des Ziels muss im Text stehen: {error}"
        );
        assert!(
            temp_released,
            "ein Fehler nach möglicher Rename-Wirkung darf den Temp-Namen nicht mehr als gehalten melden"
        );
        assert_recovery_paths(&error, &[&temp, &target]);
    }

    #[test]
    fn lokaler_temp_rest_bleibt_im_fehler_und_in_der_strukturierten_bilanz() {
        let directory = tempfile::tempdir().unwrap();
        let temp = directory.path().join(".rest.temp");
        std::fs::write(&temp, b"Restbestand").unwrap();
        let vfs = LocalVfs::new();
        let original = crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::PlanDrift,
            "Verifikation scheiterte",
        );
        let cleanup = cleanup_failed_temp(&vfs, &temp, None, false, &original).unwrap_err();
        let error = crate::error::with_recovery_path(original, &temp, &cleanup);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(
            crate::error::recovery_paths(&error),
            vec![temp.to_string_lossy().into_owned()]
        );
        assert!(error.to_string().contains(&temp.display().to_string()));
        assert_eq!(std::fs::read(temp).unwrap(), b"Restbestand");
    }

    #[test]
    fn unklarer_rename_laesst_auch_bedingt_loeschbaren_tempbestand_stehen() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let unclear_temp = root.join(".unklar.temp");
        let ordinary_temp = root.join(".geklaert.temp");
        std::fs::write(&unclear_temp, b"Recoverybestand").unwrap();
        std::fs::write(&ordinary_temp, b"aufraeumbar").unwrap();
        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX));
        let unclear_proof = stat_bound_temp_proof(&vfs, &unclear_temp);
        let ordinary_proof = stat_bound_temp_proof(&vfs, &ordinary_temp);
        let unclear = crate::coded_error(
            io::ErrorKind::Other,
            crate::ErrorCode::OutcomeUnclear,
            "Rename-Ausgang ist unklar",
        );

        cleanup_failed_temp(&vfs, &unclear_temp, unclear_proof.as_ref(), true, &unclear).unwrap();
        cleanup_failed_temp(
            &vfs,
            &ordinary_temp,
            ordinary_proof.as_ref(),
            true,
            &io::Error::other("geklärter Folgefehler"),
        )
        .unwrap();

        assert_eq!(std::fs::read(&unclear_temp).unwrap(), b"Recoverybestand");
        assert!(
            !ordinary_temp.exists(),
            "das Testdouble muss die bedingte Bereinigung tatsächlich ausführen"
        );
    }

    #[test]
    fn erfolgreicher_rollback_meldet_das_wiederhergestellte_alte_ziel() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        let quarantine = root.join("quarantaene/lauf");
        std::fs::write(&temp, b"neuer Inhalt").unwrap();
        std::fs::write(&target, b"alter Inhalt").unwrap();
        // Das Anlegen des Quarantänelaufs verbraucht bereits den ersten
        // kontrollierten Rename; Ziel→Quarantäne und Temp→Ziel folgen danach.
        let vfs = AtomicTestVfs::new(AtomicTestFault::UnchangedRenameOnCall(3));
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let mut temp_released = false;

        let error = commit_temp(CommitTempRequest {
            dst: &vfs,
            temp_path: &temp,
            dst_path: &target,
            relative_path: Path::new("ziel"),
            replacing: true,
            expected_target_proof: Some(&target_proof),
            expected_target_hash: None,
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
            temp_released: &mut temp_released,
            transferred_size: 0,
            transferred_hash: [0; 32],
        })
        .unwrap_err();

        assert_eq!(std::fs::read(&target).unwrap(), b"alter Inhalt");
        assert_eq!(std::fs::read(&temp).unwrap(), b"neuer Inhalt");
        assert!(!quarantine.join("ziel").exists());
        assert!(
            error.to_string().contains("erfolgreich"),
            "unerwarteter Fehlertext nach {} Renames: {error}",
            vfs.rename_calls.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(error.to_string().contains("zurück"), "{error}");
        assert!(
            !error.to_string().contains("kein Rückweg"),
            "der Text darf einen gerade vollzogenen Rollback nicht abstreiten: {error}"
        );
        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::AlreadyExists,
            "der nachweislich unveränderte Commitfehler bleibt maschinenlesbar"
        );
        assert_recovery_paths(&error, &[&temp, &target]);
    }

    #[test]
    fn unklarer_commit_startet_keinen_blinden_rueckweg() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        let quarantine = root.join("quarantaene/lauf");
        std::fs::write(&temp, b"neuer Inhalt").unwrap();
        std::fs::write(&target, b"alter Inhalt").unwrap();
        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(3));
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let mut temp_released = false;

        let error = commit_temp(CommitTempRequest {
            dst: &vfs,
            temp_path: &temp,
            dst_path: &target,
            relative_path: Path::new("ziel"),
            replacing: true,
            expected_target_proof: Some(&target_proof),
            expected_target_hash: None,
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
            temp_released: &mut temp_released,
            transferred_size: 0,
            transferred_hash: [0; 32],
        })
        .unwrap_err();

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            vfs.rename_calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "nach einem unklaren Commit darf kein Restore-Rename folgen"
        );
        assert_eq!(std::fs::read(&temp).unwrap(), b"neuer Inhalt");
        assert!(!target.exists());
        assert_eq!(
            std::fs::read(quarantine.join("ziel")).unwrap(),
            b"alter Inhalt"
        );
        assert_recovery_paths(&error, &[&temp, &target, &quarantine.join("ziel")]);
    }

    #[test]
    fn gescheiterter_verzeichnis_sync_meldet_einen_unklaren_ausgang() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        std::fs::write(&temp, b"fertiger Inhalt").unwrap();
        let vfs = AtomicTestVfs::new(AtomicTestFault::SyncDir);
        let mut temp_released = false;

        let error =
            rename_temp_into_place(&vfs, &temp, &target, false, &mut temp_released).unwrap_err();

        assert_eq!(
            crate::error::error_code(&error),
            crate::error::ErrorCode::OutcomeUnclear
        );
        assert!(temp_released);
        // Der Rename selbst war erfolgreich; die neue Datei liegt sichtbar am Ziel.
        assert_eq!(std::fs::read(&target).unwrap(), b"fertiger Inhalt");
        assert_recovery_paths(&error, &[&temp, &target]);
    }

    #[test]
    fn quarantaene_rename_syncfehler_bleibt_nicht_wiederholbar() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let source = root.join("alter-name");
        let target = root.join("quarantaene/neuer-name");
        std::fs::create_dir(target.parent().unwrap()).unwrap();
        std::fs::write(&source, b"alter Inhalt").unwrap();
        let vfs = AtomicTestVfs::new(AtomicTestFault::SyncDir);

        let error = rename_noreplace_recovery_aware(&vfs, &source, &target).unwrap_err();

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(!source.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"alter Inhalt");
        assert_recovery_paths(&error, &[&source, &target]);
    }

    #[test]
    fn unklarer_rueckweg_ueberstimmt_einen_unveraenderten_commitfehler() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        let quarantine = root.join("quarantaene/lauf");
        std::fs::write(&temp, b"neuer Inhalt").unwrap();
        std::fs::write(&target, b"alter Inhalt").unwrap();
        let vfs = AtomicTestVfs::new(AtomicTestFault::UnchangedThenUnclear {
            unchanged_call: 3,
            unclear_call: 4,
        });
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let mut temp_released = false;

        let error = commit_temp(CommitTempRequest {
            dst: &vfs,
            temp_path: &temp,
            dst_path: &target,
            relative_path: Path::new("ziel"),
            replacing: true,
            expected_target_proof: Some(&target_proof),
            expected_target_hash: None,
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
            temp_released: &mut temp_released,
            transferred_size: 0,
            transferred_hash: [0; 32],
        })
        .unwrap_err();

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&temp).unwrap(), b"neuer Inhalt");
        assert!(!target.exists());
        assert_eq!(
            std::fs::read(quarantine.join("ziel")).unwrap(),
            b"alter Inhalt"
        );
        assert_recovery_paths(&error, &[&temp, &target, &quarantine.join("ziel")]);
    }

    #[test]
    fn ausgetauschtes_updateziel_wird_nach_claim_nicht_als_erfolg_gemeldet() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        let staged_successor = root.join("fenster-b-ziel");
        let displaced_original = root.join("fenster-b-quarantaene/ziel");
        let quarantine = root.join("quarantaene/lauf");
        let parked = quarantine.join("ziel");
        std::fs::create_dir_all(displaced_original.parent().unwrap()).unwrap();
        std::fs::write(&temp, b"neuer Inhalt aus Fenster A").unwrap();
        std::fs::write(&target, b"bestaetigter alter Inhalt").unwrap();
        std::fs::write(&staged_successor, b"Nachfolger aus Fenster B").unwrap();

        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX))
            .swapping_before_rename(
                target.clone(),
                parked.clone(),
                staged_successor.clone(),
                displaced_original.clone(),
            );
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let target_hash = hash_file_hex(&vfs, &target).unwrap();
        let mut temp_released = false;

        let error = commit_temp(CommitTempRequest {
            dst: &vfs,
            temp_path: &temp,
            dst_path: &target,
            relative_path: Path::new("ziel"),
            replacing: true,
            expected_target_proof: Some(&target_proof),
            expected_target_hash: Some(&target_hash),
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
            temp_released: &mut temp_released,
            transferred_size: 0,
            transferred_hash: [0; 32],
        })
        .expect_err("ein ausgetauschter Nachfolger darf nicht als altes Ziel gelten");

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(
            error.to_string().contains(&target.display().to_string())
                && error.to_string().contains(&parked.display().to_string()),
            "beide Recoverypfade müssen sichtbar bleiben: {error}"
        );
        assert_eq!(std::fs::read(&temp).unwrap(), b"neuer Inhalt aus Fenster A");
        assert_eq!(
            std::fs::read(&parked).unwrap(),
            b"Nachfolger aus Fenster B",
            "der konkurrierende Nachfolger darf nicht als bestätigtes altes Ziel gelten"
        );
        assert_eq!(
            std::fs::read(&displaced_original).unwrap(),
            b"bestaetigter alter Inhalt"
        );
        assert!(!target.exists());
        assert!(
            !temp_released,
            "der eigene Temp-Name wurde noch nicht committed"
        );
        assert_recovery_paths(&error, &[&target, &parked]);
    }

    #[test]
    fn bedingter_provider_claim_laesst_ausgetauschten_nachfolger_am_ziel() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        let staged_successor = root.join("fenster-b-ziel");
        let displaced_original = root.join("fenster-b-quarantaene/ziel");
        let quarantine = root.join("quarantaene/lauf");
        let parked = quarantine.join("ziel");
        std::fs::create_dir_all(displaced_original.parent().unwrap()).unwrap();
        std::fs::write(&temp, b"neuer Inhalt aus Fenster A").unwrap();
        std::fs::write(&target, b"bestaetigter alter Inhalt").unwrap();
        std::fs::write(&staged_successor, b"Nachfolger aus Fenster B").unwrap();

        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX))
            .swapping_before_rename(
                target.clone(),
                parked.clone(),
                staged_successor.clone(),
                displaced_original.clone(),
            )
            .with_strict_conditional_rename();
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let target_hash = hash_file_hex(&vfs, &target).unwrap();
        let mut temp_released = false;

        let error = commit_temp(CommitTempRequest {
            dst: &vfs,
            temp_path: &temp,
            dst_path: &target,
            relative_path: Path::new("ziel"),
            replacing: true,
            expected_target_proof: Some(&target_proof),
            expected_target_hash: Some(&target_hash),
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
            temp_released: &mut temp_released,
            transferred_size: 0,
            transferred_hash: [0; 32],
        })
        .expect_err("der bedingte Claim muss den ausgetauschten Namen ablehnen");

        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(&target).unwrap(), b"Nachfolger aus Fenster B");
        assert_eq!(
            std::fs::read(&displaced_original).unwrap(),
            b"bestaetigter alter Inhalt"
        );
        assert!(!parked.exists());
        assert_eq!(std::fs::read(&temp).unwrap(), b"neuer Inhalt aus Fenster A");
        assert!(!temp_released);
    }

    #[test]
    fn lokaler_claim_meldet_ausgetauschten_quarantaenelauf_als_unklar() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        let quarantine = root.join("quarantaene/lauf");
        let parked = quarantine.join("ziel");
        let replacement_run = root.join("quarantaene/lauf-ersatz");
        let displaced_run = quarantine.with_extension("ausgetauscht");
        std::fs::create_dir_all(&replacement_run).unwrap();
        std::fs::write(&temp, b"neuer Inhalt").unwrap();
        std::fs::write(&target, b"bestaetigter alter Inhalt").unwrap();

        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX))
            .swapping_quarantine_before_rename(
                target.clone(),
                parked.clone(),
                quarantine.clone(),
                replacement_run,
            );
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let target_hash = hash_file_hex(&vfs, &target).unwrap();
        let mut temp_released = false;

        let error = commit_temp(CommitTempRequest {
            dst: &vfs,
            temp_path: &temp,
            dst_path: &target,
            relative_path: Path::new("ziel"),
            replacing: true,
            expected_target_proof: Some(&target_proof),
            expected_target_hash: Some(&target_hash),
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
            temp_released: &mut temp_released,
            transferred_size: 0,
            transferred_hash: [0; 32],
        })
        .expect_err("ein ausgetauschter Laufordner darf keinen erfolgreichen Claim ergeben");

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            std::fs::read(&parked).unwrap(),
            b"bestaetigter alter Inhalt"
        );
        assert!(displaced_run.is_dir());
        assert!(!target.exists());
        assert_eq!(std::fs::read(&temp).unwrap(), b"neuer Inhalt");
        assert!(!temp_released);
        assert_recovery_paths(&error, &[&target, &parked, &quarantine]);
    }

    #[test]
    fn atomarer_laufbeleg_erkennt_austausch_vor_dem_ersten_listing() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let quarantine = root.join("quarantaene/lauf");
        let replacement_run = root.join("quarantaene/lauf-ersatz");
        let displaced_run = quarantine.with_extension("ausgetauscht");
        std::fs::create_dir_all(&replacement_run).unwrap();
        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX))
            .with_strict_conditional_rename()
            .swapping_after_bound_create(
                quarantine.clone(),
                replacement_run,
                displaced_run.clone(),
            );
        let mut safety = Safety::headless_test();
        safety.quarantine = Some(quarantine.clone());

        let error = start_sync_quarantine_run(&vfs, &quarantine, &root, &safety)
            .expect_err("der atomare Erzeugungsbeleg darf keinen Ersatzordner binden");

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(quarantine.is_dir());
        assert!(displaced_run.is_dir());
        assert_recovery_paths(&error, &[&quarantine]);
    }

    #[test]
    fn bedingter_provider_claim_stoppt_vor_ausgetauschtem_quarantaenelauf() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temp = root.join(".ziel.temp");
        let target = root.join("ziel");
        let quarantine = root.join("quarantaene/lauf");
        let parked = quarantine.join("ziel");
        let replacement_run = root.join("quarantaene/lauf-ersatz");
        let displaced_run = quarantine.with_extension("ausgetauscht");
        std::fs::create_dir_all(&replacement_run).unwrap();
        std::fs::write(&temp, b"neuer Inhalt").unwrap();
        std::fs::write(&target, b"bestaetigter alter Inhalt").unwrap();

        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX))
            .swapping_quarantine_before_rename(
                target.clone(),
                parked.clone(),
                quarantine.clone(),
                replacement_run,
            )
            .with_strict_conditional_rename();
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let target_hash = hash_file_hex(&vfs, &target).unwrap();
        let mut temp_released = false;

        let error = commit_temp(CommitTempRequest {
            dst: &vfs,
            temp_path: &temp,
            dst_path: &target,
            relative_path: Path::new("ziel"),
            replacing: true,
            expected_target_proof: Some(&target_proof),
            expected_target_hash: Some(&target_hash),
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
            temp_released: &mut temp_released,
            transferred_size: 0,
            transferred_hash: [0; 32],
        })
        .expect_err("der bedingte Claim muss den ausgetauschten Laufordner ablehnen");

        assert_eq!(crate::error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"bestaetigter alter Inhalt"
        );
        assert!(!parked.exists());
        assert!(displaced_run.is_dir());
        assert_eq!(std::fs::read(&temp).unwrap(), b"neuer Inhalt");
        assert!(!temp_released);
    }

    #[test]
    fn ausgetauschtes_mirror_verzeichnis_bleibt_ein_sichtbarer_recoveryfall() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let target = root.join("ueberzaehlig");
        let staged_successor = root.join("fenster-b-verzeichnis");
        let displaced_original = root.join("fenster-b-quarantaene/ueberzaehlig");
        let quarantine = root.join("quarantaene/lauf");
        let parked = quarantine.join("ueberzaehlig");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("inhalt.txt"), b"bestaetigter Baum").unwrap();
        std::fs::create_dir_all(&staged_successor).unwrap();
        std::fs::write(
            staged_successor.join("inhalt.txt"),
            b"Nachfolger aus Fenster B",
        )
        .unwrap();
        std::fs::create_dir_all(displaced_original.parent().unwrap()).unwrap();

        let vfs = AtomicTestVfs::new(AtomicTestFault::UnclearRenameOnCall(usize::MAX))
            .swapping_before_rename(
                target.clone(),
                parked.clone(),
                staged_successor.clone(),
                displaced_original.clone(),
            );
        let (safety, run) = start_test_quarantine(&vfs, &quarantine, &root);
        let target_proof = planned_target_proof(&vfs, &target);
        let tree_hash = hash_tree(&vfs, &target).unwrap().0;

        let error = remove_or_quarantine(RemoveOrQuarantineRequest {
            dst: &vfs,
            target: &target,
            relative_path: Path::new("ueberzaehlig"),
            is_dir: true,
            expected_target_proof: &target_proof,
            expected_hash: Some(&tree_hash),
            quarantine_run_proof: Some(run.proof()),
            safety: &safety,
        })
        .expect_err("ein ausgetauschter Verzeichnisnachfolger darf nicht verschwinden");

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            std::fs::read(parked.join("inhalt.txt")).unwrap(),
            b"Nachfolger aus Fenster B"
        );
        assert_eq!(
            std::fs::read(displaced_original.join("inhalt.txt")).unwrap(),
            b"bestaetigter Baum"
        );
        assert!(!target.exists());
        assert_recovery_paths(&error, &[&target, &parked]);
    }
}
