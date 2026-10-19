//! Plan-gebundener Transfer und vollständiger Readback nativer Metadaten.

use std::io::{self, Read, Write};
use std::path::Path;

use crate::atomic_file::COPY_BUFFER_BYTES;
use crate::metadata::{BlobSummary, ForkKind, NamedBlobSummary, XattrValue};
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
    } = request;
    let Some(expected) = expected else {
        return Ok(true);
    };

    if expected.resource_fork {
        if let Some(summary) = &expected.source.resource_fork {
            let mut reader = src.open_fork_read(src_path, ForkKind::Resource)?;
            let mut writer = dst.open_fork_write_new(temp_path, ForkKind::Resource)?;
            let mut hash = blake3::Hasher::new();
            let mut bytes = 0u64;
            let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
            loop {
                if cancel.is_cancelled() {
                    return Ok(false);
                }
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                writer.write_all(&buffer[..read])?;
                hash.update(&buffer[..read]);
                bytes = bytes.saturating_add(read as u64);
                prog.bytes_done = prog.bytes_done.saturating_add(read as u64);
                on_progress(prog);
            }
            writer.flush()?;
            drop(writer);
            if bytes != summary.size || hash.finalize().as_bytes() != &summary.digest {
                return Err(plan_changed(
                    &src_path.to_string_lossy(),
                    "Ressourcen-Fork änderte sich seit der Vorschau",
                ));
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
        dst.write_acl_exact(temp_path, expected.source.acl.as_ref())?;
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
    if transfer.acl && !transfer.normal_mode {
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
