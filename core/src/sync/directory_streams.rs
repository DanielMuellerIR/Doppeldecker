//! Ordnerströme werden vor jeder Entfernung kopiert. Der spätere Metadatenabschluss
//! darf die durch eigene bestätigte Streamwirkungen fortgeschriebene Menge erwarten.
use super::{Action, ExecuteRequest, SyncStats, SyncTargetProof};
use crate::directory_streams::DirectoryStreamEffect;
use crate::metadata::NamedBlobSummary;
use crate::{Cancel, Progress};
use std::collections::BTreeMap;
use std::io;

pub(super) struct DirectoryStreamExecution<'a> {
    pub request: ExecuteRequest<'a>,
    pub created: &'a BTreeMap<String, SyncTargetProof>,
    pub removals: bool,
}

fn after_copies(source: &[NamedBlobSummary], target: &[NamedBlobSummary]) -> Vec<NamedBlobSummary> {
    let mut values: BTreeMap<_, _> = target
        .iter()
        .map(|value| (value.name.clone(), value.clone()))
        .collect();
    for value in source {
        values.insert(value.name.clone(), value.clone());
    }
    values.into_values().collect()
}

pub(super) fn execute_directory_streams(
    context: DirectoryStreamExecution<'_>,
    cancel: &Cancel,
    progress: &mut Progress,
    stats: &mut SyncStats,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<bool> {
    let ExecuteRequest {
        src,
        src_root,
        dst,
        dst_root,
        plan,
        safety,
    } = context.request;
    let mut directories = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::CreateDir {
                path,
                source_mtime,
                metadata: Some(metadata),
                ..
            }
            | Action::SetDirectoryMetadata {
                path,
                source_mtime,
                metadata,
                ..
            } if metadata.directory_streams.is_some() => Some((path, *source_mtime, metadata)),
            _ => None,
        })
        .collect::<Vec<_>>();
    directories.sort_by_key(|(path, ..)| std::cmp::Reverse(path.matches('/').count()));
    for (path, source_mtime, metadata) in directories {
        let stream_plan = metadata
            .directory_streams
            .as_ref()
            .expect("gefilterter Streamplan");
        let effects = stream_plan
            .effects
            .iter()
            .filter(|effect| {
                matches!(effect, DirectoryStreamEffect::Remove { .. }) == context.removals
            })
            .collect::<Vec<_>>();
        if effects.is_empty() {
            continue;
        }
        let source = metadata.source.named_streams.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Ordnerstromquelle unbeobachtet")
        })?;
        let target = metadata
            .target
            .as_ref()
            .and_then(|value| value.named_streams.as_deref())
            .unwrap_or(&[]);
        let intermediate = after_copies(source, target);
        let expected = if context.removals {
            intermediate.as_slice()
        } else {
            target
        };
        let final_set = if context.removals {
            source
        } else {
            intermediate.as_slice()
        };
        let bytes = effects.iter().fold(0u64, |sum, effect| {
            sum.saturating_add(effect.transfer_bytes())
        });
        progress.phase = "metadata".into();
        progress.current = path.clone();
        if safety.is_dry_run() {
            progress.bytes_done = progress.bytes_done.saturating_add(bytes);
            stats.bytes_copied = stats.bytes_copied.saturating_add(bytes);
            on_progress(progress);
            continue;
        }
        if cancel.is_cancelled() {
            return Ok(false);
        }
        super::validate_anchor_identity(src, dst, safety)?;
        let source_path = crate::path::checked_join(src_root, path)?;
        let target_path = crate::path::checked_join(dst_root, path)?;
        super::guard_path(safety.sandbox(), &target_path, "Ordnerstreamziel")?;
        super::execute::validate_directory_source(src, &source_path, source_mtime, metadata)?;
        let proof = stream_plan
            .target_proof
            .as_ref()
            .or_else(|| context.created.get(path))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Ordnerstreamziel besitzt keine gebundene Basis-ID",
                )
            })?;
        let mut transaction = dst.begin_directory_stream_update(&target_path, proof, expected)?;
        let execution = (|| -> io::Result<bool> {
            for effect in effects {
                if cancel.is_cancelled() {
                    return Ok(false);
                }
                super::validate_anchor_identity(src, dst, safety)?;
                super::execute::validate_directory_source(
                    src,
                    &source_path,
                    source_mtime,
                    metadata,
                )?;
                let actual = if let Some(after) = effect.after() {
                    let mut reader = src.open_named_stream_read(&source_path, &after.name)?;
                    let mut temp = transaction.stage(after)?;
                    let copied = super::metadata::transfer_stream(
                        &mut reader,
                        &mut *temp,
                        &after.value,
                        &source_path,
                        cancel,
                        progress,
                        on_progress,
                    );
                    match copied {
                        Ok(true) => {
                            if let Err(error) = super::execute::validate_directory_source(
                                src,
                                &source_path,
                                source_mtime,
                                metadata,
                            ) {
                                return match temp.abort() {
                                    Ok(()) => Err(error),
                                    Err(cleanup) => Err(crate::error::with_recovery_context(
                                        error,
                                        &[&cleanup],
                                        &[&target_path],
                                    )),
                                };
                            }
                            temp.commit(effect.before())?
                        }
                        result => {
                            let cleanup = temp.abort();
                            match (result, cleanup) {
                                (Err(error), Err(cleanup)) => {
                                    return Err(crate::error::with_recovery_context(
                                        error,
                                        &[&cleanup],
                                        &[&target_path],
                                    ))
                                }
                                (_, Err(error)) | (Err(error), _) => return Err(error),
                                _ => return Ok(false),
                            }
                        }
                    }
                } else {
                    transaction.remove(effect.before().expect("Entfernungsplan"))?
                };
                if actual != *effect {
                    return Err(crate::coded_error(
                        io::ErrorKind::InvalidData,
                        crate::ErrorCode::OutcomeUnclear,
                        "Ordnerstreambeleg entspricht nicht der bestätigten Einzelwirkung",
                    ));
                }
                stats.bytes_copied = stats.bytes_copied.saturating_add(effect.transfer_bytes());
                stats.directory_stream_report.merge(
                    &transaction
                        .report()
                        .at_directory(&target_path.to_string_lossy()),
                );
                progress.directory_stream_report = stats.directory_stream_report.clone();
                // Der Queuecallback schreibt ein neues Wirkungsdelta synchron.
                // Sein Persistenzfehler setzt Cancel; dann bleiben die Intents offen.
                on_progress(progress);
            }
            Ok(!cancel.is_cancelled())
        })();
        stats.directory_stream_report.merge(
            &transaction
                .report()
                .at_directory(&target_path.to_string_lossy()),
        );
        progress.directory_stream_report = stats.directory_stream_report.clone();
        match execution {
            Err(error) => {
                let paths = transaction.recovery_paths();
                stats.temp_recovery_paths.extend(paths.clone());
                return Err(crate::error::with_directory_stream_report(
                    crate::error::with_recovery_paths(error, &paths),
                    &stats.directory_stream_report,
                ));
            }
            Ok(false) => {
                stats
                    .temp_recovery_paths
                    .extend(transaction.recovery_paths());
                return Ok(false);
            }
            Ok(true) => {}
        }
        let before_finish = transaction
            .report()
            .at_directory(&target_path.to_string_lossy());
        let finish_paths = transaction.recovery_paths();
        let finished = match transaction.finish(final_set) {
            Ok(report) => report,
            Err(error) => {
                stats.temp_recovery_paths.extend(finish_paths.clone());
                return Err(crate::error::with_directory_stream_report(
                    crate::error::with_recovery_paths(error, &finish_paths),
                    &before_finish,
                ));
            }
        };
        stats
            .directory_stream_report
            .merge(&finished.at_directory(&target_path.to_string_lossy()));
        progress.directory_stream_report = stats.directory_stream_report.clone();
    }
    Ok(true)
}

pub(super) fn project_completed_streams(
    metadata: &super::MetadataTransfer,
) -> io::Result<super::MetadataTransfer> {
    let mut projected = metadata.clone();
    if metadata.directory_streams.is_some() {
        projected.directory_streams = None;
        if let Some(target) = &metadata.target {
            projected.target = Some(
                target
                    .clone()
                    .with_named_streams(metadata.source.named_streams.clone())?,
            );
        }
    }
    Ok(projected)
}
