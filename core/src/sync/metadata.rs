//! Plan-gebundener Transfer und vollständiger Readback nativer Metadaten.

use std::io::{self, Read, Write};
use std::path::Path;

use crate::atomic_file::COPY_BUFFER_BYTES;
use crate::metadata::{BlobSummary, ForkKind, NamedBlobSummary, XattrValue};
use crate::path::checked_join;
use crate::progress::{Cancel, Progress};
use crate::vfs::Vfs;

use super::{optional_stat, plan_changed, MetadataTransfer};

pub(super) fn verify_planned_metadata(
    src: &dyn Vfs,
    src_path: &Path,
    dst: &dyn Vfs,
    dst_path: &Path,
    expected: Option<&MetadataTransfer>,
) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    validate_metadata_transfer(expected)?;
    expected.source.validate()?;
    // Die laufbezogene Gruppennummer bindet auch sichtbar ausgelassene
    // Quellbeziehungen; ein Einzeldateimanifest kennt diese Nummer noch nicht.
    if expected.source.hardlink_group.is_some() {
        let current = src
            .metadata_manifest(src_path)?
            .with_hardlink_group(expected.source.hardlink_group)?;
        if current.digest != expected.source.digest {
            return Err(plan_changed(
                &src_path.to_string_lossy(),
                "Metadaten-Planbindung der Hardlink-Quelle wurde verletzt",
            ));
        }
    } else {
        crate::metadata::verify_metadata_manifest(src, src_path, &expected.source)?;
    }
    match &expected.target {
        Some(target) => {
            target.validate()?;
            crate::metadata::verify_metadata_manifest(dst, dst_path, target)
        }
        None => match optional_stat(dst, dst_path)? {
            Some(_) => Err(plan_changed(
                &dst_path.to_string_lossy(),
                "Metadaten-Ziel entstand seit der Vorschau",
            )),
            None => Ok(()),
        },
    }
}

/// Überträgt die im Plan aktivierten nativen Metadaten ausschließlich auf die neue
/// Temp-Datei. Datenströme kommen zuerst, danach Owner und xattrs. Normale Rechte,
/// Zeitstempel und Sonderbits folgen im Aufrufer in ihrer festgelegten Reihenfolge.
/// `false` bedeutet einen sauberen Abbruch; das bestehende Ziel bleibt unangetastet.
pub(super) struct PlannedMetadataRequest<'a> {
    pub(super) src: &'a dyn Vfs,
    pub(super) src_path: &'a Path,
    pub(super) dst: &'a dyn Vfs,
    pub(super) temp_path: &'a Path,
    pub(super) expected: Option<&'a MetadataTransfer>,
    pub(super) allow_windows_dacl_omission: bool,
    pub(super) windows_dacl_omission: &'a mut Option<crate::windows_dacl::WindowsDaclTargetIssue>,
}

pub(super) fn transfer_planned_metadata(
    request: PlannedMetadataRequest<'_>,
    cancel: &Cancel,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<bool> {
    let PlannedMetadataRequest {
        src,
        src_path,
        dst,
        temp_path,
        expected,
        allow_windows_dacl_omission,
        windows_dacl_omission,
    } = request;
    let Some(expected) = expected else {
        return Ok(true);
    };

    if expected.resource_fork {
        if let Some(summary) = &expected.source.resource_fork {
            let mut reader = src.open_fork_read(src_path, ForkKind::Resource)?;
            let mut writer = dst.open_fork_write_new(temp_path, ForkKind::Resource)?;
            if !transfer_stream(
                &mut reader,
                &mut writer,
                summary,
                src_path,
                cancel,
                prog,
                on_progress,
            )? {
                return Ok(false);
            }
        }
    }

    if expected.named_streams {
        let streams = expected.source.named_streams.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Stream-Transfer benötigt eine beobachtete vollständige Quellmenge",
            )
        })?;
        for stream in streams {
            if cancel.is_cancelled() {
                return Ok(false);
            }
            let mut reader = src.open_named_stream_read(src_path, &stream.name)?;
            let mut writer = dst.open_named_stream_write_new(temp_path, &stream.name)?;
            if !transfer_stream(
                &mut reader,
                &mut writer,
                &stream.value,
                src_path,
                cancel,
                prog,
                on_progress,
            )? {
                return Ok(false);
            }
        }
    }

    if expected.owner {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let owner = expected.source.owner.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Owner-Transfer benötigt numerischen Nutzer und Gruppe",
            )
        })?;
        dst.set_metadata_owner(temp_path, owner)?;
    }

    if expected.acl {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        if let Some(crate::windows_dacl::WindowsDaclTransfer::Exact(context)) =
            &expected.windows_dacl
        {
            let Some(crate::metadata::Acl::WindowsDacl(acl)) = &expected.source.acl else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Exakter Windows-DACL-Transfer ohne Quell-DACL",
                ));
            };
            if let Err(error) = dst.write_windows_dacl_exact(temp_path, acl, context) {
                if let Some(issue) = crate::windows_dacl::known_write_omission(&error)
                    .filter(|_| allow_windows_dacl_omission)
                {
                    *windows_dacl_omission = Some(issue);
                } else {
                    return Err(error);
                }
            }
        } else {
            dst.write_acl_exact(temp_path, expected.source.acl.as_ref())?;
        }
    }

    if expected.xattrs {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let values = src.read_xattrs(src_path)?;
        if summarize_xattrs(&values) != expected.source.xattrs {
            return Err(plan_changed(
                &src_path.to_string_lossy(),
                "Quell-xattrs änderten sich seit der Vorschau",
            ));
        }
        dst.write_xattrs_exact(temp_path, &values)?;
    }
    Ok(true)
}

// Die bestätigte Länge begrenzt auch das Lesen einer gleichzeitig wachsenden
// Quelle. Ein einziges zusätzliches Byte belegt Drift, ohne endlos nachzulesen.
pub(super) fn transfer_stream(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    summary: &BlobSummary,
    src_path: &Path,
    cancel: &Cancel,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<bool> {
    let mut hash = blake3::Hasher::new();
    let mut remaining = summary.size;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let limit = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..limit])?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        hash.update(&buffer[..read]);
        remaining -= read as u64;
        prog.bytes_done = prog.bytes_done.saturating_add(read as u64);
        on_progress(prog);
    }
    if cancel.is_cancelled() {
        return Ok(false);
    }
    if remaining != 0
        || reader.read(&mut buffer[..1])? != 0
        || hash.finalize().as_bytes() != &summary.digest
    {
        return Err(plan_changed(
            &src_path.to_string_lossy(),
            "Metadatenstrom änderte sich seit der Vorschau",
        ));
    }
    writer.flush()?;
    Ok(true)
}

pub(super) fn summarize_xattrs(values: &[XattrValue]) -> Vec<NamedBlobSummary> {
    let mut summaries: Vec<_> = values
        .iter()
        .map(|item| NamedBlobSummary {
            name: item.name.clone(),
            value: BlobSummary::from_bytes(&item.value),
        })
        .collect();
    summaries.sort_by(|left, right| left.name.cmp(&right.name));
    summaries
}

pub(super) fn validate_metadata_transfer(transfer: &MetadataTransfer) -> io::Result<()> {
    transfer.source.validate()?;
    if let Some(target) = &transfer.target {
        target.validate()?;
    }
    if transfer.sparse_extents && transfer.source.sparse_extents.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sparse-Transfer benötigt vollständige beobachtete Quellbereiche",
        ));
    }
    if transfer.named_streams && transfer.source.named_streams.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Stream-Transfer enthält keine beobachtete vollständige Quellmenge",
        ));
    }
    if let Some(plan) = &transfer.directory_streams {
        let source = transfer.source.named_streams.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Ordnerstreamplan benötigt eine vollständige Quellmenge",
            )
        })?;
        let target = match transfer.target.as_ref() {
            Some(target) => target.named_streams.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Ordnerstreamplan benötigt eine vollständige Zielmenge",
                )
            })?,
            None => &[],
        };
        if transfer.named_streams
            || transfer.resource_fork
            || plan.effects.is_empty()
            || plan.effects != crate::directory_streams::plan_effects(source, target)?
            || transfer.target.is_some() != plan.target_proof.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ordnerstreamwirkungen widersprechen dem bestätigten Manifest",
            ));
        }
        for effect in &plan.effects {
            effect.validate()?;
        }
    }
    if transfer.owner && transfer.source.owner.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Owner-Transfer enthält keinen numerischen Quell-Owner",
        ));
    }
    if transfer.special_mode_bits && !transfer.normal_mode {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sonderbit-Transfer benötigt die vorgelagerte normale Rechtephase",
        ));
    }
    let windows_exact = match &transfer.windows_dacl {
        Some(crate::windows_dacl::WindowsDaclTransfer::Exact(context)) => {
            context.validate()?;
            let Some(crate::metadata::Acl::WindowsDacl(acl)) = &transfer.source.acl else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows-DACL-Kontext benötigt eine beobachtete Quell-DACL",
                ));
            };
            if !transfer.acl
                || acl
                    .file_write_issue(context.namespace, &context.owner)
                    .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows-DACL-Transfer widerspricht seinem exakten Zielkontext",
                ));
            }
            true
        }
        Some(crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryExact(selection)) => {
            selection.validate()?;
            if !transfer.acl
                || transfer.target.is_some()
                || transfer.source.acl.as_ref()
                    != Some(&crate::metadata::Acl::WindowsDacl(selection.after.clone()))
                || !transfer
                    .source
                    .named_streams
                    .as_ref()
                    .is_some_and(Vec::is_empty)
            {
                return Err(io::Error::other(
                    "V7 widerspricht dem bestätigten Quellmanifest",
                ));
            }
            true
        }
        Some(crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(
            selection,
        )) => {
            selection.validate()?;
            if !transfer.acl
                || transfer.target.is_some()
                || transfer.source.acl.as_ref()
                    != Some(&crate::metadata::Acl::WindowsDacl(selection.after.clone()))
                || !transfer
                    .source
                    .named_streams
                    .as_ref()
                    .is_some_and(Vec::is_empty)
            {
                return Err(io::Error::other(
                    "V8 widerspricht dem bestätigten Quellmanifest",
                ));
            }
            true
        }
        Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(plan)) => {
            plan.validate()?;
            let target = transfer.target.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Verzeichnis-DACL benötigt ein Zielmanifest",
                )
            })?;
            let security = target.windows_security.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Verzeichnis-DACL benötigt Ziel-Owner und Gruppe",
                )
            })?;
            if !transfer.acl
                || transfer.source.acl.as_ref()
                    != Some(&crate::metadata::Acl::WindowsDacl(plan.after.clone()))
                || target.acl.as_ref()
                    != Some(&crate::metadata::Acl::WindowsDacl(plan.before.clone()))
                || security.issue.is_some()
                || security.namespace != plan.context.namespace
                || security.owner.as_ref() != Some(&plan.context.owner)
                || security.group != plan.context.group
                || transfer.owner
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Verzeichnis-DACL widerspricht ihrem Manifest oder Owner-Kontext",
                ));
            }
            true
        }
        Some(crate::windows_dacl::WindowsDaclTransfer::Omitted(_)) if transfer.acl => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ausgelassene Windows-DACL ist als Transfer aktiviert",
            ));
        }
        _ => false,
    };
    if transfer.acl
        && (transfer.source.windows_security.is_some()
            || matches!(
                transfer.source.acl,
                Some(crate::metadata::Acl::WindowsDacl(_))
            ))
        && !windows_exact
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows-DACL-Schreiben benötigt einen exakten Zielkontext",
        ));
    }
    if transfer.acl && !windows_exact && !transfer.normal_mode {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ACL-Transfer benötigt die gekoppelte normale Rechtephase",
        ));
    }
    Ok(())
}

/// Vergleicht nur die im Profil aktivierten Merkmale. Das vollständige Zielmanifest
/// darf bei `NativeBestEffort` weiterhin nicht darstellbare Klassen enthalten.
pub(super) fn verify_selected_metadata(
    dst: &dyn Vfs,
    path: &Path,
    expected: &MetadataTransfer,
) -> io::Result<()> {
    validate_metadata_transfer(expected)?;
    let actual = dst.metadata_manifest(path)?;
    actual.validate()?;
    let mismatch = (expected.owner && actual.owner != expected.source.owner)
        || (expected.acl && actual.acl != expected.source.acl)
        || (expected.normal_mode && actual.mode & 0o777 != expected.source.mode & 0o777)
        || (expected.special_mode_bits && actual.mode & 0o7000 != expected.source.mode & 0o7000)
        || (expected.xattrs && actual.xattrs != expected.source.xattrs)
        || (expected.resource_fork && actual.resource_fork != expected.source.resource_fork)
        || ((expected.named_streams || expected.directory_streams.is_some())
            && actual.named_streams != expected.source.named_streams)
        || (expected.sparse_extents && actual.sparse_extents != expected.source.sparse_extents);
    if mismatch {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Native Metadaten stimmen nach dem Schreiben nicht exakt überein: {}{}",
                path.display(),
                if expected.sparse_extents {
                    format!(
                        " (Sparse erwartet {:?}, gelesen {:?})",
                        expected.source.sparse_extents, actual.sparse_extents
                    )
                } else {
                    String::new()
                }
            ),
        ));
    }
    Ok(())
}

pub(super) fn verify_temp_metadata(
    dst: &dyn Vfs,
    temp_path: &Path,
    expected: Option<&MetadataTransfer>,
) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    verify_selected_metadata(dst, temp_path, expected)
}

/// Die Manifestbindung einer lesenden Sicherheitsbilanz gilt auch ohne Schreibaktion.
pub(crate) fn verify_metadata_observations(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    plan: &super::SyncPlan,
) -> io::Result<()> {
    // Auch eine reine Beobachtung ohne Aktion kann nach Bestätigung driften.
    // Alle solchen Bindungen stehen vor Quarantäneanlage und erster Mutation.
    for expected in &plan.metadata_observations {
        let source_path = checked_join(src_root, &expected.path)?;
        let target_path = checked_join(dst_root, &expected.path)?;
        let source = src.metadata_manifest(&source_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                plan_changed(&expected.path, "Beobachtete Quelle fehlt seit der Vorschau")
            } else {
                error
            }
        })?;
        source.validate()?;
        let target = dst.metadata_manifest(&target_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                plan_changed(&expected.path, "Beobachtetes Ziel fehlt seit der Vorschau")
            } else {
                error
            }
        })?;
        target.validate()?;
        if source.digest != expected.source_digest || Some(target.digest) != expected.target_digest
        {
            return Err(plan_changed(
                &expected.path,
                "Beobachtete Sicherheitsmetadaten änderten sich seit der Vorschau",
            ));
        }
    }
    // Neue Pfade besitzen noch keine Zielbeobachtung. Auch deren DAV-Bilanz
    // wird vor der ersten Geschwisterwirkung geprüft, nicht erst beim Kopieren.
    let bound_paths: std::collections::HashSet<&str> = plan
        .metadata_observations
        .iter()
        .map(|value| value.path.as_str())
        .collect();
    for action in &plan.actions {
        let (path, metadata) = match action {
            super::Action::CopyFile {
                path,
                metadata: Some(metadata),
                ..
            }
            | super::Action::UpdateFile {
                path,
                metadata: Some(metadata),
                ..
            }
            | super::Action::CreateDir {
                path,
                metadata: Some(metadata),
                ..
            }
            | super::Action::CreateHardlink { path, metadata, .. }
            | super::Action::SetDirectoryMetadata { path, metadata, .. } => (path, metadata),
            _ => continue,
        };
        let dav = metadata.source.webdav_security.is_some()
            || metadata
                .target
                .as_ref()
                .is_some_and(|value| value.webdav_security.is_some());
        if dav && !bound_paths.contains(path.as_str()) {
            verify_planned_metadata(
                src,
                &checked_join(src_root, path)?,
                dst,
                &checked_join(dst_root, path)?,
                Some(metadata),
            )?;
        }
    }
    Ok(())
}
