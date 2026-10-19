//! Nicht serialisierbarer Realmodus für lokale↔Remote-Synchronisation.
//!
//! Der Aufbau entspricht dem lokalen Vertrag: vollständige Listings, exakte Root-
//! Identitäten, Restore-Probe, Platzbilanz, Zufallsphrase, zehn Minuten gültiges
//! Einmal-Permit und lokales append-only Audit. Ein SFTP-Reconnect ändert die gebundene
//! Sitzungsgeneration und verwirft damit Vorbereitung und Permit.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::atomic_file::{hash_vfs_file, sibling_candidate, sync_rename_parents};
use crate::error::{coded_error, with_recovery_context, ErrorCode};
use crate::progress::{Cancel, Progress};
use crate::real_mode::{
    bind_capabilities_digest, binding_digest, build_listing_proof, confirmation_mismatch,
    confirmation_phrase, create_real_audit_context, create_real_permit, ensure_quarantine_root,
    ensure_same_mount, execute_real_with_audit, execute_real_with_audit_detailed, impact_for_plan,
    plan_drift, reserve_quarantine_run_name, run_restore_probe as run_local_restore_probe,
    validate_real_plan, validate_real_target, RealAuditContext, RealAuditExecutionSuccess,
    RealAuditRun, RealPermitRequest, RealRunPreview, REAL_PERMIT_LIFETIME,
};
use crate::remote::combine_probe_outcome;
use crate::remote_rename::{ProviderRenameCapabilities, RenameReport};
use crate::sync::{self, Compare, Safety, SyncMode, SyncOptions, SyncPlan, SyncStats};
use crate::vfs::{
    BoundParentsRenameRequest, ConditionalContentReplaceRequest, Entry, MutationFailure,
    ProofListEntry,
};
use crate::{
    FtpsVfs, LocalVfs, RemoteCapabilities, SftpReadSecurity, SftpVfs, SmbVfs, Vfs, WebDavVfs,
};

enum RunVfs {
    Local(LocalVfs),
    Projected(Arc<dyn Vfs>),
    Sftp(Arc<SftpVfs>),
    Ftps(Arc<FtpsVfs>),
    WebDav(Arc<WebDavVfs>),
    Smb(Arc<SmbVfs>),
}

impl RunVfs {
    fn as_vfs(&self) -> &dyn Vfs {
        match self {
            Self::Local(vfs) => vfs,
            Self::Projected(vfs) => vfs.as_ref(),
            Self::Sftp(vfs) => vfs.as_ref(),
            Self::Ftps(vfs) => vfs.as_ref(),
            Self::WebDav(vfs) => vfs.as_ref(),
            Self::Smb(vfs) => vfs.as_ref(),
        }
    }

    fn sftp(&self) -> Option<&SftpVfs> {
        match self {
            Self::Sftp(vfs) => Some(vfs.as_ref()),
            Self::Local(_)
            | Self::Projected(_)
            | Self::Ftps(_)
            | Self::WebDav(_)
            | Self::Smb(_) => None,
        }
    }

    fn smb(&self) -> Option<&SmbVfs> {
        match self {
            Self::Smb(vfs) => Some(vfs.as_ref()),
            Self::Local(_)
            | Self::Projected(_)
            | Self::Sftp(_)
            | Self::Ftps(_)
            | Self::WebDav(_) => None,
        }
    }

    fn ftps(&self) -> Option<&FtpsVfs> {
        match self {
            Self::Ftps(vfs) => Some(vfs.as_ref()),
            Self::Local(_)
            | Self::Projected(_)
            | Self::Sftp(_)
            | Self::WebDav(_)
            | Self::Smb(_) => None,
        }
    }

    fn webdav(&self) -> Option<&WebDavVfs> {
        match self {
            Self::WebDav(vfs) => Some(vfs.as_ref()),
            Self::Local(_) | Self::Projected(_) | Self::Sftp(_) | Self::Ftps(_) | Self::Smb(_) => {
                None
            }
        }
    }

    fn probe_target_add_only(
        &self,
        target_root: &Path,
    ) -> io::Result<Option<crate::RemoteCapabilities>> {
        match self {
            Self::Sftp(vfs) => vfs.probe_add_only_capabilities_at(target_root).map(Some),
            Self::Ftps(vfs) => vfs.probe_add_only_capabilities_at(target_root).map(Some),
            Self::WebDav(vfs) => vfs.probe_add_only_capabilities_at(target_root).map(Some),
            Self::Smb(vfs) => vfs.probe_add_only_capabilities_at(target_root).map(Some),
            Self::Local(_) | Self::Projected(_) => Ok(None),
        }
    }

    fn probe_target_entry_delete(&self, target_root: &Path) -> io::Result<()> {
        match self {
            Self::Sftp(vfs) => require_quarantine_purge_contract(
                vfs.probe_delete_capability_at(target_root)?,
                "SFTP",
            ),
            Self::Smb(vfs) => require_quarantine_purge_contract(
                vfs.probe_delete_capability_at(target_root)?,
                "SMB",
            ),
            Self::Local(_) | Self::Projected(_) | Self::Ftps(_) | Self::WebDav(_) => Ok(()),
        }
    }

    fn remote_generation(&self) -> Option<u64> {
        match self {
            Self::Sftp(vfs) => Some(vfs.session_generation()),
            Self::Ftps(vfs) => Some(vfs.session_generation()),
            Self::WebDav(vfs) => Some(vfs.session_generation()),
            Self::Smb(vfs) => Some(vfs.session_generation()),
            Self::Local(_) | Self::Projected(_) => None,
        }
    }

    fn is_webdav(&self) -> bool {
        matches!(self, Self::WebDav(_))
    }

    fn is_projected(&self) -> bool {
        matches!(self, Self::Projected(_))
    }

    fn uses_local_restore_probe(&self) -> bool {
        matches!(self, Self::Local(_) | Self::Projected(_))
    }

    fn kind(&self) -> ProviderKind {
        match self {
            Self::Local(_) | Self::Projected(_) => ProviderKind::Local,
            Self::Sftp(_) => ProviderKind::Sftp,
            Self::Ftps(_) => ProviderKind::Ftps,
            Self::WebDav(_) => ProviderKind::WebDav,
            Self::Smb(_) => ProviderKind::Smb,
        }
    }

    fn current_transfer_capabilities(&self) -> ProviderTransferCapabilities {
        match self {
            Self::Local(_) | Self::Projected(_) => ProviderTransferCapabilities::local(),
            Self::Sftp(vfs) => {
                ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities())
            }
            Self::Ftps(vfs) => {
                ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities())
            }
            Self::WebDav(vfs) => {
                ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities())
            }
            Self::Smb(vfs) => ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities()),
        }
    }
}

fn cleanup_probe_quarantine_root(
    vfs: &dyn Vfs,
    parent: &Path,
    base: &Path,
    proof: Option<&ProofListEntry>,
) -> io::Result<()> {
    let Some(proof) = proof else {
        return Ok(());
    };
    // Der Name kann nach der Anlage ausgetauscht werden. Deshalb darf hier weder
    // ein getrenntes `stat` noch ein pfadbasiertes `remove_dir` entscheiden.
    vfs.remove_if_matches(base, proof).map_err(|error| {
        with_recovery_context(
            coded_error(
                error.kind(),
                ErrorCode::OutcomeUnclear,
                format!(
                    "Der nur für die Probe angelegte Quarantäneordner blieb zur Wiederherstellung unter {} stehen: {error}",
                    base.display()
                ),
            ),
            &[&error],
            &[base],
        )
    })?;
    vfs.sync_dir(parent).map_err(|error| {
        with_recovery_context(
            coded_error(
                error.kind(),
                ErrorCode::OutcomeUnclear,
                format!(
                    "Der Probeordner {} wurde bedingt entfernt, aber sein Elternordner konnte nicht bestätigt werden. Der Name kann nach einem Absturz wieder sichtbar sein: {error}",
                    base.display()
                ),
            ),
            &[&error],
            &[base],
        )
    })
}

#[derive(Debug)]
struct BoundProbeQuarantineRoot {
    path: PathBuf,
    identity: ProofListEntry,
    removal_proof: Option<ProofListEntry>,
}

fn same_probe_proof(left: &ProofListEntry, right: &ProofListEntry) -> bool {
    let stable_identity = match (&left.provider_token, &right.provider_token) {
        (Some(left), Some(right)) => left == right,
        (None, None)
            if left.entry.device.is_some()
                && left.entry.inode.is_some()
                && right.entry.device.is_some()
                && right.entry.inode.is_some() =>
        {
            left.entry.device == right.entry.device && left.entry.inode == right.entry.inode
        }
        (None, None) => {
            left.entry.size == right.entry.size
                && left.entry.mtime == right.entry.mtime
                && left.entry.mode == right.entry.mode
        }
        _ => false,
    };
    stable_identity
        && left.raw_name == right.raw_name
        && left.entry.name == right.entry.name
        && left.entry.is_dir == right.entry.is_dir
        && left.entry.is_file == right.entry.is_file
        && left.entry.is_symlink == right.entry.is_symlink
}

fn listed_bound_probe_directory(
    vfs: &dyn Vfs,
    parent: &Path,
    directory: &Path,
    role: &str,
) -> io::Result<Option<ProofListEntry>> {
    let name = directory
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!("Der {role} ist nicht eindeutig als UTF-8 darstellbar"),
            )
        })?;
    let listing = vfs.list_complete(parent).map_err(|error| {
        coded_error(
            error.kind(),
            ErrorCode::InvalidData,
            format!(
                "Der Elternbestand von {} konnte nicht vollständig und symlinkfrei gebunden werden: {error}",
                directory.display()
            ),
        )
    })?;
    crate::real_mode::require_complete_safe_listing(
        &listing,
        &format!("Gebundener Elternbestand für {}", directory.display()),
    )?;
    let mut matches = listing.entries.into_iter().filter(|entry| {
        entry.raw_name == name.as_bytes() || entry.entry.name.eq_ignore_ascii_case(name)
    });
    let Some(found) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || found.entry.is_symlink || !found.entry.is_dir {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!(
                "Der {role} ist kein eindeutiger, symlinkfreier Ordner: {}",
                directory.display()
            ),
        ));
    }
    Ok(Some(found))
}

fn listed_bound_probe_file(
    vfs: &dyn Vfs,
    parent: &Path,
    file: &Path,
) -> io::Result<ProofListEntry> {
    let name = file
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid("Restore-Probenname ist nicht eindeutig als UTF-8 darstellbar"))?;
    let listing = vfs.list_complete(parent)?;
    crate::real_mode::require_complete_safe_listing(
        &listing,
        &format!("Gebundener Elternbestand für {}", file.display()),
    )?;
    let mut matches = listing.entries.into_iter().filter(|entry| {
        entry.raw_name == name.as_bytes() || entry.entry.name.eq_ignore_ascii_case(name)
    });
    let found = matches.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Restore-Probe fehlt nach Manifest-CAS: {}", file.display()),
        )
    })?;
    if matches.next().is_some()
        || found.entry.is_symlink
        || !found.entry.is_file
        || found.entry.is_dir
    {
        return Err(invalid(format!(
            "Restore-Probe ist keine eindeutige, symlinkfreie Datei: {}",
            file.display()
        )));
    }
    Ok(found)
}

fn listed_probe_quarantine_root(
    vfs: &dyn Vfs,
    parent: &Path,
    base: &Path,
) -> io::Result<Option<ProofListEntry>> {
    listed_bound_probe_directory(vfs, parent, base, "Probe-Quarantäneroot")
}

fn bind_restore_target_root(vfs: &dyn Vfs, target: &Path) -> io::Result<ProofListEntry> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Der Restore-Zielroot {} besitzt keinen bindbaren Elternordner",
                target.display()
            ),
        )
    })?;
    listed_bound_probe_directory(vfs, parent, target, "Restore-Zielroot")?.ok_or_else(|| {
        coded_error(
            io::ErrorKind::NotFound,
            ErrorCode::OutcomeUnclear,
            format!(
                "Der Restore-Zielroot konnte vor der Probe nicht identitätsgebunden werden: {}",
                target.display()
            ),
        )
    })
}

fn ensure_probe_quarantine_root(
    vfs: &dyn Vfs,
    parent: &Path,
    base: &Path,
) -> io::Result<BoundProbeQuarantineRoot> {
    if let Some(identity) = listed_probe_quarantine_root(vfs, parent, base)? {
        return Ok(BoundProbeQuarantineRoot {
            path: base.to_path_buf(),
            identity,
            removal_proof: None,
        });
    }
    match vfs.create_dir_with_removal_proof(base) {
        Ok(Some(proof)) => {
            let proof = proof_at(proof, base)?;
            match vfs.sync_dir(parent) {
                Ok(()) => {
                    let rebound = listed_probe_quarantine_root(vfs, parent, base);
                    let identity = match rebound {
                        Ok(Some(current)) if same_probe_proof(&current, &proof) => current,
                        Ok(_) => {
                            let error = io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                "der sichtbare Name stimmt nicht mehr mit dem Erzeugungsbeleg überein",
                            );
                            let original = coded_error(
                                io::ErrorKind::Other,
                                ErrorCode::OutcomeUnclear,
                                format!(
                                    "Der neu angelegte Probeordner ließ sich nach dem Parent-Sync nicht mehr an seinen Erzeugungsbeleg binden ({error}). Recoverypfad: {}",
                                    base.display()
                                ),
                            );
                            let cleanup =
                                cleanup_probe_quarantine_root(vfs, parent, base, Some(&proof));
                            let original = with_recovery_context(original, &[&error], &[base]);
                            return combine_probe_outcome(
                                "Quarantäne", Err(original), cleanup, &base.to_string_lossy(),
                            );
                        }
                        Err(error) => {
                            let original = coded_error(
                                io::ErrorKind::Other,
                                ErrorCode::OutcomeUnclear,
                                format!(
                                    "Der neu angelegte Probeordner konnte nach dem Parent-Sync nicht mehr vollständig zurückgelesen werden ({error}). Recoverypfad: {}",
                                    base.display()
                                ),
                            );
                            let cleanup =
                                cleanup_probe_quarantine_root(vfs, parent, base, Some(&proof));
                            let original = with_recovery_context(original, &[&error], &[base]);
                            return combine_probe_outcome(
                                "Quarantäne", Err(original), cleanup, &base.to_string_lossy(),
                            );
                        }
                    };
                    Ok(BoundProbeQuarantineRoot {
                        path: base.to_path_buf(),
                        identity,
                        removal_proof: Some(proof),
                    })
                }
                Err(sync_error) => {
                    // Der Ordner ist bereits sichtbar und sein bei der Anlage
                    // gebundener Beleg darf bei einem Parent-Sync-Fehler nicht
                    // verloren gehen. Nur dieser Beleg darf den Cleanup auslösen.
                    let original = coded_error(
                        sync_error.kind(),
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Der neue Probe-Quarantäneordner wurde unter {} angelegt, aber sein Elternordner konnte nicht bestätigt werden ({sync_error}). Recoverypfad: {}",
                            base.display(),
                            base.display()
                        ),
                    );
                    let cleanup = cleanup_probe_quarantine_root(
                        vfs,
                        parent,
                        base,
                        Some(&proof),
                    );
                    let original = with_recovery_context(original, &[&sync_error], &[base]);
                    combine_probe_outcome(
                        "Quarantäne", Err(original), cleanup, &base.to_string_lossy(),
                    )
                }
            }
        }
        Ok(None) => Err(with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Der Provider konnte den neu angelegten Probeordner nicht atomar an einen Entfernungsbeleg binden. Er bleibt zur Wiederherstellung unter {} stehen",
                    base.display()
                ),
            ),
            &[],
            &[base],
        )),
        Err(MutationFailure::Unchanged(error)) => Err(error),
        Err(MutationFailure::OutcomeUnclear(error)) => Err(with_recovery_context(
            coded_error(
                error.kind(),
                ErrorCode::OutcomeUnclear,
                format!(
                    "Die Anlage des Probeordners hat einen unklaren Ausgang. Bitte beide Bestände prüfen; der mögliche Recoverypfad ist {}: {error}",
                    base.display()
                ),
            ),
            &[&error],
            &[base],
        )),
    }
}

fn proof_at(mut proof: ProofListEntry, path: &Path) -> io::Result<ProofListEntry> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Probenpfad besitzt keinen Dateinamen: {}", path.display()),
        )
    })?;
    let name = name.to_str().ok_or_else(|| {
        coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Probenname ist nicht eindeutig als UTF-8 darstellbar",
        )
    })?;
    proof.entry.name = name.to_owned();
    proof.raw_name = name.as_bytes().to_vec();
    proof.safely_decoded = true;
    Ok(proof)
}

fn probe_recovery_error(path: &Path, action: &str, error: io::Error) -> io::Error {
    with_recovery_context(
        coded_error(
            error.kind(),
            ErrorCode::OutcomeUnclear,
            format!(
                "{action}. Die identitätsgebundene Probe bleibt zur Wiederherstellung unter {} stehen: {error}",
                path.display()
            ),
        ),
        &[&error],
        &[path],
    )
}

fn cleanup_bound_probe(
    vfs: &dyn Vfs,
    path: &Path,
    proof: &ProofListEntry,
    cause: io::Error,
) -> io::Error {
    match vfs.remove_if_matches(path, proof) {
        Ok(()) => match vfs.sync_dir(path.parent().unwrap_or_else(|| Path::new("/"))) {
            Ok(()) => cause,
            Err(sync_error) => coded_error(
                sync_error.kind(),
                ErrorCode::OutcomeUnclear,
                format!(
                    "Die Probe wurde bedingt entfernt, aber die Verzeichnisbestätigung scheiterte. Der frühere Probenpfad kann nach einem Absturz wieder sichtbar sein: {}: {sync_error}",
                    path.display()
                ),
            ),
        },
        Err(cleanup_error) => probe_recovery_error(
            path,
            &format!("Die Probe scheiterte ({cause}); auch die bedingte Bereinigung scheiterte"),
            cleanup_error,
        ),
    }
}

fn run_bound_restore_probe(
    vfs: &dyn Vfs,
    target: &Path,
    quarantine: &BoundProbeQuarantineRoot,
) -> io::Result<()> {
    const CONTENT: &[u8] = b"doppeldecker-identitaetsgebundene-restore-probe";
    const CAS_CONTENT: &[u8] = b"doppeldecker-dauerhafter-manifest-cas-probe";
    // Der Rückweg endet im Zielroot, nicht im Quarantäneroot. Deshalb binden wir
    // diesen Elternordner separat vor der ersten Probenmutation und tragen genau
    // seinen Beleg bis zum atomar bedingten Rückrename.
    let _target_listing_proof = bind_restore_target_root(vfs, target)?;
    let target_identity = vfs.root_identity(target)?;
    let current_quarantine = listed_probe_quarantine_root(
        vfs,
        quarantine
            .path
            .parent()
            .ok_or_else(|| invalid("Probe-Quarantäneroot besitzt keinen Elternpfad"))?,
        &quarantine.path,
    )?
    .ok_or_else(|| invalid("Probe-Quarantäneroot fehlt vor der Restore-Probe"))?;
    if !same_probe_proof(&quarantine.identity, &current_quarantine) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            "Probe-Quarantäneroot änderte sich vor der Restore-Probe",
        ));
    }
    let quarantine_identity = vfs.root_identity(&quarantine.path)?;
    let (first, mut writer, proof) = loop {
        let candidate = sibling_candidate(&target.join("restore-probe"), "remote-restore-probe")?;
        match vfs.open_write_new_with_removal_proof(&candidate) {
            Ok((writer, Some(proof))) => break (candidate, writer, proof),
            Ok((writer, None)) => {
                drop(writer);
                return Err(coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Der Provider konnte die exklusiv angelegte Restore-Probe nicht atomar an einen Entfernungsbeleg binden. Sie bleibt unter {} stehen",
                        candidate.display()
                    ),
                ));
            }
            Err(MutationFailure::Unchanged(error))
                if error.kind() == io::ErrorKind::AlreadyExists =>
            {
                continue
            }
            Err(MutationFailure::Unchanged(error)) => return Err(error),
            Err(MutationFailure::OutcomeUnclear(error)) => {
                return Err(probe_recovery_error(
                    &candidate,
                    "Die exklusive Anlage der Restore-Probe hat einen unklaren Ausgang",
                    error,
                ))
            }
        }
    };
    let proof = proof_at(proof.clone(), &first)
        .map_err(|error| cleanup_bound_probe(vfs, &first, &proof, error))?;
    if let Err(error) = std::io::Write::write_all(&mut writer, CONTENT)
        .and_then(|()| std::io::Write::flush(&mut writer))
    {
        drop(writer);
        return Err(cleanup_bound_probe(vfs, &first, &proof, error));
    }
    drop(writer);
    if let Err(error) = vfs.sync_file(&first) {
        return Err(cleanup_bound_probe(vfs, &first, &proof, error));
    }
    let initial_hash = hash_vfs_file(vfs, &first)
        .map_err(|error| cleanup_bound_probe(vfs, &first, &proof, error))?;
    vfs.replace_file_if_content_matches(ConditionalContentReplaceRequest {
        path: &first,
        expected_digest: *initial_hash.as_bytes(),
        replacement: CAS_CONTENT,
    })
    .map_err(|failure| match failure {
        MutationFailure::Unchanged(error) => cleanup_bound_probe(vfs, &first, &proof, error),
        MutationFailure::OutcomeUnclear(error) => probe_recovery_error(
            &first,
            "Der erfolgreiche Manifest-CAS der Restore-Probe hat einen unklaren Ausgang",
            error,
        ),
    })?;
    let proof = listed_bound_probe_file(vfs, target, &first).map_err(|error| {
        probe_recovery_error(
            &first,
            "Die Restore-Probe konnte nach dem erfolgreichen Manifest-CAS nicht neu gebunden werden",
            error,
        )
    })?;
    let expected_hash = hash_vfs_file(vfs, &first).map_err(|error| {
        probe_recovery_error(
            &first,
            "Der erfolgreiche Manifest-CAS konnte nicht zurückgelesen werden",
            error,
        )
    })?;
    if expected_hash != blake3::hash(CAS_CONTENT) {
        return Err(probe_recovery_error(
            &first,
            "Der erfolgreiche Manifest-CAS schrieb nicht den erwarteten Inhalt",
            io::Error::new(io::ErrorKind::InvalidData, "Inhaltsdigest weicht ab"),
        ));
    }
    let mut wrong_digest = *expected_hash.as_bytes();
    wrong_digest[0] ^= 0xff;
    match vfs.replace_file_if_content_matches(ConditionalContentReplaceRequest {
        path: &first,
        expected_digest: wrong_digest,
        replacement: b"ungebundener-inhalt",
    }) {
        Err(MutationFailure::Unchanged(_)) => {}
        Err(MutationFailure::OutcomeUnclear(error)) => {
            return Err(probe_recovery_error(
                &first,
                "Der abgelehnte Manifest-CAS garantierte keinen unveränderten Bestand",
                error,
            ))
        }
        Ok(()) => {
            return Err(probe_recovery_error(
                &first,
                "Der Manifest-CAS akzeptierte einen falschen Inhaltsdigest",
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Digestbedingung wurde übergangen",
                ),
            ))
        }
    }
    let rejected_hash = hash_vfs_file(vfs, &first).map_err(|error| {
        probe_recovery_error(
            &first,
            "Der abgelehnte Manifest-CAS konnte nicht zurückgelesen werden",
            error,
        )
    })?;
    if rejected_hash != expected_hash {
        return Err(probe_recovery_error(
            &first,
            "Der abgelehnte Manifest-CAS veränderte den Probeninhalt",
            io::Error::new(io::ErrorKind::InvalidData, "Inhaltsdigest weicht ab"),
        ));
    }
    let renamed = sibling_candidate(&target.join("restore-probe"), "remote-restore-renamed")
        .map_err(|error| cleanup_bound_probe(vfs, &first, &proof, error))?;
    vfs.rename_noreplace_if_matches_in_bound_parents(BoundParentsRenameRequest {
        from: &first,
        to: &renamed,
        expected: &proof,
        from_parent: target,
        expected_from_parent: &target_identity,
        to_parent: target,
        expected_to_parent: &target_identity,
        required_content: None,
    })
    .map_err(|error| cleanup_bound_probe(vfs, &first, &proof, error))?;
    let proof = proof_at(proof, &renamed).map_err(|error| {
        probe_recovery_error(
            &renamed,
            "Das erste Probe-Rename wurde sichtbar, der Beleg konnte aber nicht auf den neuen Namen übertragen werden",
            error,
        )
    })?;
    sync_rename_parents(vfs, &first, &renamed).map_err(|error| {
        probe_recovery_error(
            &renamed,
            "Das erste Probe-Rename wurde sichtbar, seine Bestätigung scheiterte",
            error,
        )
    })?;
    let renamed_hash = hash_vfs_file(vfs, &renamed).map_err(|error| {
        probe_recovery_error(
            &renamed,
            "Die umbenannte Restore-Probe konnte nicht zurückgelesen werden",
            error,
        )
    })?;
    if renamed_hash != expected_hash {
        return Err(probe_recovery_error(
            &renamed,
            "Der Inhalt der umbenannten Restore-Probe stimmt nicht mehr",
            io::Error::new(io::ErrorKind::InvalidData, "Inhaltsdigest weicht ab"),
        ));
    }
    let parked = sibling_candidate(
        &quarantine.path.join("restore-probe"),
        "remote-restore-parked",
    )
    .map_err(|error| {
        probe_recovery_error(
            &renamed,
            "Für die Restore-Probe konnte kein sicherer Parkname gebildet werden",
            error,
        )
    })?;
    // Der Quarantäneordner kann zwischen Listing und Wirkung ausgetauscht werden.
    // Deshalb müssen Quellbeleg, Elternbeleg und Rename eine einzige
    // Providerwirkung bilden; ein separater Recheck wäre nur ein neues Rennen.
    vfs.rename_noreplace_if_matches_in_bound_parents(BoundParentsRenameRequest {
        from: &renamed,
        to: &parked,
        expected: &proof,
        from_parent: target,
        expected_from_parent: &target_identity,
        to_parent: &quarantine.path,
        expected_to_parent: &quarantine_identity,
        required_content: None,
    })
    .map_err(|error| cleanup_bound_probe(vfs, &renamed, &proof, error))?;
    let proof = proof_at(proof, &parked).map_err(|error| {
        probe_recovery_error(
            &parked,
            "Das Parken wurde sichtbar, der Beleg konnte aber nicht auf den Parknamen übertragen werden",
            error,
        )
    })?;
    sync_rename_parents(vfs, &renamed, &parked).map_err(|error| {
        probe_recovery_error(
            &parked,
            "Das Parken der Restore-Probe wurde sichtbar, seine Bestätigung scheiterte",
            error,
        )
    })?;
    vfs.rename_noreplace_if_matches_in_bound_parents(BoundParentsRenameRequest {
        from: &parked,
        to: &first,
        expected: &proof,
        from_parent: &quarantine.path,
        expected_from_parent: &quarantine_identity,
        to_parent: target,
        expected_to_parent: &target_identity,
        required_content: None,
    })
    .map_err(|error| cleanup_bound_probe(vfs, &parked, &proof, error))?;
    let proof = proof_at(proof, &first).map_err(|error| {
        probe_recovery_error(
            &first,
            "Das Restore wurde sichtbar, der Beleg konnte aber nicht auf den Zielnamen übertragen werden",
            error,
        )
    })?;
    sync_rename_parents(vfs, &parked, &first).map_err(|error| {
        probe_recovery_error(
            &first,
            "Das Wiederherstellen der Restore-Probe wurde sichtbar, seine Bestätigung scheiterte",
            error,
        )
    })?;
    let restored_hash = hash_vfs_file(vfs, &first).map_err(|error| {
        probe_recovery_error(
            &first,
            "Die wiederhergestellte Restore-Probe konnte nicht zurückgelesen werden",
            error,
        )
    })?;
    if restored_hash != expected_hash {
        return Err(probe_recovery_error(
            &first,
            "Der Inhalt der wiederhergestellten Restore-Probe stimmt nicht mehr",
            io::Error::new(io::ErrorKind::InvalidData, "Inhaltsdigest weicht ab"),
        ));
    }
    vfs.remove_if_matches(&first, &proof).map_err(|error| {
        probe_recovery_error(&first, "Die bedingte Probe-Bereinigung scheiterte", error)
    })?;
    vfs.sync_dir(target).map_err(|error| {
        coded_error(
            error.kind(),
            ErrorCode::OutcomeUnclear,
            format!(
                "Die Restore-Probe wurde bedingt entfernt, aber der Zielordner konnte nicht bestätigt werden. Der frühere Probenpfad kann nach einem Absturz wieder sichtbar sein: {}: {error}",
                first.display()
            ),
        )
    })
}

/// Providerart einer konkreten Richtung. Die Zielseite entscheidet über die
/// freigegebenen Modi; die Quellart darf keine Zielgarantie vortäuschen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Local,
    Sftp,
    Ftps,
    WebDav,
    Smb,
}

/// Getrennte Fähigkeiten der vorhandenen Provider-Quarantäne. Listing liest nur
/// Manifeste; Restore und Purge mutieren dagegen jeweils mit eigenen Gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProviderQuarantineManagementCapabilities {
    pub listing: bool,
    pub restore: bool,
    pub purge: bool,
}

impl ProviderKind {
    pub fn quarantine_management_capabilities(self) -> ProviderQuarantineManagementCapabilities {
        match self {
            Self::Smb => ProviderQuarantineManagementCapabilities {
                listing: true,
                // SMB kann die manifestierten Läufe stabil über File-ID lesen. Die
                // heutigen Rename-/Delete-Befehle sind jedoch nur pfadbasiert und
                // schließen einen Austausch zwischen Run-Readback und Wirkung nicht.
                restore: false,
                purge: false,
            },
            // Schon das Manifestlisting benötigt pfadbasiertes READDIR und OPEN.
            // SFTP v3 kann beides nicht atomar unter dem Pane-Root halten; Restore
            // und Purge besitzen zusätzlich keine elterngebundene Mutation.
            Self::Sftp | Self::Local | Self::Ftps | Self::WebDav => {
                ProviderQuarantineManagementCapabilities {
                    listing: false,
                    restore: false,
                    purge: false,
                }
            }
        }
    }

    /// Ob bereits manifestierte Remote-Quarantäneläufe dieses Providers gelistet,
    /// wiederhergestellt und endgültig geleert werden können. Das ist bewusst eine
    /// statische Verwaltungsfähigkeit: Ob eine konkrete Sitzung NEUE Einträge
    /// quarantänisieren darf, entscheidet weiterhin ausschließlich die mutierende
    /// [`ProviderDeleteCapabilities`]-Probe.
    pub fn supports_quarantine_management(self) -> bool {
        self.quarantine_management_capabilities().listing
    }
}

/// Konkrete, für eine einzelne verbundene Sitzung geprüfte Fähigkeiten der
/// interaktiven Dateimanager-Operationen. Ersetzungen bleiben getrennt von neuen
/// Namen, damit ein AddOnly-Server F5 nicht vollständig verliert.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProviderFileCapabilities {
    pub provider: ProviderKind,
    pub copy_new: bool,
    /// Neue Dateien laufen als sichtbarer Best-Effort-Commit: frischer vollständiger
    /// Elternbestand, providergebundene Tempanlage, genau ein Commit und vollständiger
    /// Inhaltsreadback. Konkrete Serverbedingungen und fehlende Garantien stehen
    /// in limitations; daraus entsteht keine allgemeine Syncfreigabe.
    #[serde(default)]
    pub copy_new_best_effort: bool,
    pub replace: bool,
    /// Ersetzen ist nur ohne Quarantäne/Wiederherstellung möglich und verlangt eine
    /// sichtbare Bestätigung der konkreten vorhandenen Zieldateien.
    #[serde(default)]
    pub replace_best_effort: bool,
    /// Eine Quellentfernung ist durch einen provideratomaren Identitätsbeleg bis
    /// zur Wirkung gebunden. Eine bloß bestandene Pfad-Löschprobe reicht dafür nicht.
    pub remove_source: bool,
    /// Die Quelle kann nach vollständigem frischem Readback manifestgebunden Blatt
    /// für Blatt und bei Ordnern ausschließlich leer entfernt werden. Zwischen dem
    /// letzten Readback und der Einzelwirkung bleibt ein sichtbares Restrisiko.
    #[serde(default)]
    pub remove_source_best_effort: bool,
    pub rename_noreplace: bool,
    /// Konkrete Einschränkungen der Best-Effort-Wege, für Anzeige in GUI und CLI.
    #[serde(default)]
    pub limitations: Vec<String>,
    pub blockers: Vec<String>,
}

pub(crate) fn file_move_rename_candidate(
    kind: crate::FileQueueKind,
    same_remote_namespace: bool,
    all_top_level_targets_missing: bool,
) -> bool {
    kind == crate::FileQueueKind::Move && same_remote_namespace && all_top_level_targets_missing
}

/// Ausschließlich Backendzustand nach statischer Requestvalidierung und frischer
/// Konfliktprüfung. Die Oberfläche übergibt weder Typbelege noch Renamefreigaben.
pub struct ProviderFileTargetProbe<'a> {
    pub target: &'a ProviderEndpoint,
    pub items: &'a [PathBuf],
    pub kind: crate::FileQueueKind,
    pub overwrite: bool,
    /// Die übersprungenen Konflikte sind bereits die gewählte Wirkung.
    pub skip_conflicts: bool,
    pub inspect_conflicts: bool,
    /// Erwartete Konfliktlage vor jeder mutierenden Teilprobe erneut prüfen.
    pub expected_conflicts: Option<&'a [String]>,
    pub shared_session: bool,
    pub targets_missing: bool,
    pub capabilities_override: Option<ProviderFileCapabilities>,
}

pub struct ProviderFileTargetPreparation {
    /// Ausschließlich Überspringen, ohne irgendeine Provider-Mutationsfreigabe.
    pub skip_only: bool,
    pub choice_required: bool,
    pub rename_only: bool,
    /// Nur tatsächlich vollständig übertragbare Move-Auswahlen benötigen Abbau.
    pub source_removal_required: bool,
    /// Frisch gezählte Blätter; ausschließlich für den mutationsfreien Abschluss.
    pub skipped_stats: crate::fileops::OpStats,
    pub capabilities: Option<ProviderFileCapabilities>,
}

#[derive(Default)]
struct FileSelectionEffects {
    files: bool,
    create_directories: bool,
    remove_source: bool,
    stats: crate::fileops::OpStats,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProviderDeleteCapabilities {
    pub provider: ProviderKind,
    pub quarantine: bool,
    pub permanent: bool,
    pub blockers: Vec<String>,
}

/// Reine Anzeige vor einer abbrechbaren Inhaltsvorbereitung. Die konkrete
/// Zielklasse entsteht erst aus den anschließend gehaltenen Coreansichten.
pub fn interactive_file_preparation_notice(
    source: ProviderKind,
    target: ProviderKind,
    move_source: bool,
) -> Option<&'static str> {
    if source == ProviderKind::Sftp {
        Some(crate::sftp::SFTP_CONTENT_READ_LIMITATION)
    } else if source == ProviderKind::Ftps || target == ProviderKind::Ftps {
        Some("FTP-/FTPS-Dateioperation wird vorbereitet. Abbruch verhindert die spätere Einreihung; ein bereits laufender Serverzugriff wird noch geprüft beendet.")
    } else if source == ProviderKind::Local && target == ProviderKind::WebDav && !move_source {
        Some("Lokale Inhalte und WebDAV-Zielbedingungen werden frisch geprüft. Reguläre additive Dateien verwenden bedingtes PUT/MOVE als Best Effort ohne künstliche Collection-/Löschprobe. Die Quellprüfung ist auf 60 Sekunden begrenzt; Abbruch hält vor dem Zielcommit an.")
    } else {
        None
    }
}

pub(crate) struct InteractiveFileOwner {
    pub(crate) staging: crate::local_staging::StagingOwner,
    pub(crate) profiles: Vec<crate::RemoteProfileBinding>,
}

/// Gehaltene Inhaltsansichten einer gewöhnlichen Dateioperation. Eine optionale
/// Zielklasse gilt nur für diese Auswahl, niemals für globale Syncfähigkeiten.
pub struct InteractiveFileContexts {
    pub source_vfs: Arc<dyn Vfs>,
    pub target_vfs: Arc<dyn Vfs>,
    pub target_capabilities: Option<ProviderFileCapabilities>,
}

/// Reine Eingabegrenze vor Profil-, Secret- und Providerzugriff. Persistiertes
/// Overwrite und eine frühere Capability-Probe ersetzen keinen Wirkungsbeleg.
pub fn validate_interactive_file_transfer_request(
    source: ProviderKind,
    target: ProviderKind,
    move_source: bool,
    overwrite: bool,
) -> io::Result<()> {
    if source == ProviderKind::Local && target == ProviderKind::WebDav && !move_source && overwrite
    {
        return Err(crate::error::coded_error(
            io::ErrorKind::Unsupported,
            crate::ErrorCode::Unsupported,
            crate::webdav::FILE_REPLACE_BLOCKER,
        ));
    }
    if source == ProviderKind::Sftp && (target != ProviderKind::Local || move_source || overwrite) {
        return Err(io::Error::new(io::ErrorKind::Unsupported,
            "SFTP-Inhalte erlauben ausschließlich additive Downloads begrenzter Inhaltsauswahlen in ein lokales Ziel; Remote-Ziel, Verschieben und Ersetzen bleiben gesperrt"));
    }
    Ok(())
}

/// Nicht serialisierbarer, bereits verbundener Endpunkt. Remote-Sitzungen und
/// kurzlebige Secretwerte bleiben damit ausschließlich im RAM.
#[derive(Clone)]
pub struct ProviderEndpoint {
    inner: ProviderEndpointInner,
}

/// Ausschließlich für interaktive Verzeichnisanzeigen. Das SFTP-Ergebnis trägt
/// sichtbar seine pfadbasierte Best-Effort-Klasse und kann dadurch nicht als
/// vollständiger Planner- oder Mutationsbeleg wiederverwendet werden.
#[derive(Clone, Debug)]
pub struct ProviderInteractiveListing {
    pub entries: Vec<ProviderInteractiveEntry>,
    pub omitted: Vec<InteractiveListingOmission>,
    /// Vollständiger Providerbeleg. Beim SFTP-v3-Anzeigepfad bleibt dies trotz
    /// vollständig gelesener READDIR-Antwort falsch, weil der Pfadnamespace nicht
    /// bis zum Handle gebunden werden kann.
    pub complete: bool,
    pub security: Option<SftpReadSecurity>,
}

/// Reine Anzeigebilanz. Der maskierte Anzeigename ist ausdrücklich kein VFS-Pfad.
#[derive(Clone, Debug, serde::Serialize)]
pub struct InteractiveListingOmission {
    pub display_name: String,
    pub is_hidden: bool,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct ProviderInteractiveEntry {
    pub entry: Entry,
    pub safely_decoded: bool,
    /// Nur Darstellungswissen; ein fehlender Wert darf keine Nullgröße behaupten.
    pub size_known: bool,
}

impl ProviderInteractiveEntry {
    pub fn display_entry(&self) -> io::Result<serde_json::Value> {
        let mut value = serde_json::to_value(&self.entry).map_err(io::Error::other)?;
        if !self.size_known {
            value["size"] = serde_json::Value::Null;
        }
        Ok(value)
    }
}

#[derive(Clone)]
enum ProviderEndpointInner {
    Local(PathBuf),
    #[cfg_attr(not(test), allow(dead_code))]
    TestRemoteLocal(ProviderKind, PathBuf),
    Sftp(Arc<SftpVfs>, PathBuf),
    Ftps(Arc<FtpsVfs>, PathBuf),
    WebDav(Arc<WebDavVfs>, PathBuf),
    Smb(Arc<SmbVfs>, PathBuf),
}

fn require_quarantine_restore_contract(
    capabilities: RemoteCapabilities,
    provider: &str,
) -> io::Result<()> {
    if capabilities.permits_add_only() && capabilities.atomic_noreplace_rename {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{provider}-Restore benötigt bewiesenes AddOnly und bedingtes No-Replace-Rename"
            ),
        ))
    }
}

fn require_quarantine_purge_contract(
    capabilities: RemoteCapabilities,
    provider: &str,
) -> io::Result<()> {
    if capabilities.permits_entry_delete() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{provider}-Purge benötigt bewiesenes identitätsgebundenes Löschen"),
        ))
    }
}

impl ProviderEndpoint {
    // Transportabbruch, unklarer Ausgang, Drift und Abbruch bleiben Fehler mit
    // ihrem stabilen Code und Recoverypfad. Ein gewöhnlicher Blockertext würde
    // Wiederherstellung und Stoppen weiterer Proben verhindern.
    fn probe_blocker_or_error(error: io::Error, label: &str) -> io::Result<String> {
        if matches!(
            crate::error::error_code(&error),
            crate::ErrorCode::ConnectionLost
                | crate::ErrorCode::OutcomeUnclear
                | crate::ErrorCode::PlanDrift
                | crate::ErrorCode::Interrupted
        ) {
            return Err(error);
        }
        Ok(format!("{label}: {error}"))
    }

    /// Wählt für alle drei Einstiege dieselbe tatsächliche Wirkung. Eine fehlende
    /// FTPS-Renamegarantie führt in die bestehende bestätigte Copy-/Remove-Route;
    /// unklare Probenfehler beenden die Vorbereitung unverändert.
    pub fn probe_interactive_file_target(
        &self,
        request: ProviderFileTargetProbe<'_>,
    ) -> io::Result<ProviderFileTargetPreparation> {
        self.probe_interactive_file_target_with_effect_limit(request, 100_000)
    }

    fn probe_interactive_file_target_with_effect_limit(
        &self,
        request: ProviderFileTargetProbe<'_>,
        maximum_entries: usize,
    ) -> io::Result<ProviderFileTargetPreparation> {
        let move_source = request.kind == crate::FileQueueKind::Move;
        self.validate_interactive_file_transfer(request.target, move_source, request.overwrite)?;
        if request.items.is_empty() {
            return Err(invalid("Dateiauswahl ist leer"));
        }
        crate::validate_file_operation_scope(self.root(), request.items)?;
        if move_source {
            if let ProviderEndpointInner::Ftps(vfs, _) = &self.inner {
                vfs.require_mutation_owner()?;
            }
        }
        if let ProviderEndpointInner::Ftps(vfs, _) = &request.target.inner {
            vfs.require_mutation_owner()?;
        }
        let same_runtime_namespace = self.shares_runtime_namespace(request.target)?;
        let ftps_selection =
            self.kind() == ProviderKind::Ftps || request.target.kind() == ProviderKind::Ftps;
        let current_conflicts = if ftps_selection {
            let source = self.file_context()?.0;
            let target = request.target.file_context()?.0;
            crate::find_recursive_conflicts(
                source.as_ref(),
                target.as_ref(),
                request.target.root(),
                request.items,
            )?
        } else {
            Vec::new()
        };
        if ftps_selection {
            if let Some(expected) = request.expected_conflicts {
                let mut expected = expected.to_vec();
                expected.sort();
                if expected != current_conflicts {
                    return Err(crate::real_mode::plan_drift("FTPS-Konflikte wechselten vor der Wirkungsprobe; nichts neu vorbereiten oder übertragen"));
                }
            }
        }
        if request.inspect_conflicts
            && !request.overwrite
            && ftps_selection
            && !current_conflicts.is_empty()
        {
            let mut prepared = self.skipped_file_preparation(request.target.kind());
            prepared.skip_only = false;
            prepared.choice_required = true;
            prepared.capabilities.as_mut().expect("Anzeigeobjekt").blockers = vec!["Zuerst Konfliktentscheidung wählen; erst danach werden die dafür nötigen Fähigkeiten geprüft.".into()];
            return Ok(prepared);
        }
        let mut rename_candidate = file_move_rename_candidate(
            request.kind,
            request.shared_session && same_runtime_namespace && current_conflicts.is_empty(),
            request.targets_missing,
        );
        if let (ProviderEndpointInner::Ftps(source, _), ProviderEndpointInner::Ftps(target, _)) =
            (&self.inner, &request.target.inner)
        {
            rename_candidate &= source.shares_connection_mapping(target);
        }
        let mut ftps_file_probe_used = false;
        if rename_candidate {
            let rename = if self.kind() == ProviderKind::Ftps {
                let vfs = self.file_context()?.0;
                let mut files = false;
                let mut directories = false;
                let mut supported = true;
                for path in request.items {
                    let entry = vfs.stat(path)?;
                    supported &= !entry.is_symlink && (entry.is_file || entry.is_dir);
                    files |= entry.is_file && !entry.is_symlink;
                    directories |= entry.is_dir && !entry.is_symlink;
                }
                if supported {
                    ftps_file_probe_used = files;
                    Some(
                        request
                            .target
                            .probe_rename_capabilities_for_types(files, directories)?,
                    )
                } else {
                    None
                }
            } else {
                Some(self.probe_rename_capabilities()?)
            };
            if let Some(rename) = rename {
                if rename.assured {
                    if let (
                        ProviderEndpointInner::Ftps(source, _),
                        ProviderEndpointInner::Ftps(target, _),
                    ) = (&self.inner, &request.target.inner)
                    {
                        source.adopt_rename_proofs(target)?;
                    }
                    return Ok(ProviderFileTargetPreparation {
                        skip_only: false,
                        choice_required: false,
                        rename_only: true,
                        source_removal_required: true,
                        skipped_stats: crate::fileops::OpStats::default(),
                        capabilities: None,
                    });
                }
                if self.kind() != ProviderKind::Ftps {
                    let mut reasons = rename.blockers;
                    reasons.extend(rename.limitations.into_iter().filter(|note| {
                        note != crate::remote_rename::SOURCE_PATH_BINDING_LIMITATION
                    }));
                    return Err(io::Error::new(io::ErrorKind::Unsupported, format!("Verschieben im selben Namespace benötigt bewiesenes No-Replace-Rename: {}", reasons.join("; "))));
                }
            }
        }
        let effects = if ftps_selection && request.skip_conflicts && !request.overwrite {
            self.skipped_file_selection_effects(&request, &current_conflicts, maximum_entries)?
        } else {
            None
        };
        let source_removal_required = effects
            .as_ref()
            .map_or(move_source, |effects| effects.remove_source);
        if let Some(effects) = &effects {
            if !effects.files && !effects.create_directories && !effects.remove_source {
                let mut prepared = self.skipped_file_preparation(request.target.kind());
                prepared.skipped_stats = effects.stats.clone();
                return Ok(prepared);
            }
        }
        let directory_selection = if request.target.kind() == ProviderKind::Ftps
            || (move_source && self.kind() == ProviderKind::Ftps)
        {
            match &effects {
                Some(effects) if !effects.files => Some(effects.create_directories),
                Some(_) => None,
                None => self.directory_only_file_selection(&request)?,
            }
        } else {
            None
        };
        if let Some(needs_directory_create) = directory_selection {
            if source_removal_required {
                if let ProviderEndpointInner::Ftps(vfs, _) = &self.inner {
                    vfs.restrict_to_directories()?;
                }
            }
            if let ProviderEndpointInner::Ftps(vfs, root) = &request.target.inner {
                vfs.restrict_to_directories()?;
                if needs_directory_create {
                    vfs.probe_directory_create_capability_at(root)?;
                }
                return Ok(ProviderFileTargetPreparation {
                    skip_only: false,
                    choice_required: false,
                    rename_only: false,
                    source_removal_required,
                    skipped_stats: crate::fileops::OpStats::default(),
                    capabilities: Some(ProviderFileCapabilities {
                        provider: ProviderKind::Ftps,
                        copy_new: false, copy_new_best_effort: true,
                        replace: false, replace_best_effort: false,
                        remove_source: false, remove_source_best_effort: false,
                        rename_noreplace: false,
                        limitations: vec!["Nur Ordner ohne Dateiinhalt werden mit MKD und Readback angelegt; zwischen Zielprüfung und Anlage bleibt ein Kollisionsfenster (Best Effort). Dateiinhalt benötigt eine neue Vorbereitung.".into()],
                        blockers: vec!["Ersetzen ist für die reine Ordnerauswahl nicht gebunden".into()],
                    }),
                });
            }
        }
        let capabilities = match request.capabilities_override {
            Some(capabilities) => capabilities,
            None if ftps_file_probe_used => request.target.file_capabilities(false, false)?,
            None => request.target.probe_file_target_capabilities()?,
        };
        Ok(ProviderFileTargetPreparation {
            skip_only: false,
            choice_required: false,
            rename_only: false,
            source_removal_required,
            skipped_stats: crate::fileops::OpStats::default(),
            capabilities: Some(capabilities),
        })
    }

    fn skipped_file_preparation(&self, provider: ProviderKind) -> ProviderFileTargetPreparation {
        ProviderFileTargetPreparation {
            skip_only: true,
            choice_required: false,
            rename_only: false,
            source_removal_required: false,
            skipped_stats: crate::fileops::OpStats::default(),
            capabilities: Some(ProviderFileCapabilities {
                provider, copy_new: false, copy_new_best_effort: false,
                replace: false, replace_best_effort: false,
                remove_source: false, remove_source_best_effort: false,
                rename_noreplace: false,
                limitations: Vec::new(),
                blockers: vec!["Diese Auswahl wird vollständig übersprungen; keine Schreib- oder Quellentfernungswirkung ist gebunden.".into()],
            }),
        }
    }

    fn skipped_file_selection_effects(
        &self,
        request: &ProviderFileTargetProbe<'_>,
        conflicts: &[String],
        maximum_entries: usize,
    ) -> io::Result<Option<FileSelectionEffects>> {
        let source = self.file_context()?.0;
        let target = request.target.file_context()?.0;
        // Ohne vollständige begrenzte Bilanz keine vereinfachte Klasse vergeben.
        // Der reguläre Datei-/Quellvertrag prüft seine Fähigkeiten unabhängig weiter.
        let mut effects = FileSelectionEffects::default();
        let mut visited = 0usize;
        for root in request.items {
            let before = (effects.stats.skipped, effects.stats.symlinks_skipped);
            let mut pending = vec![(
                root.clone(),
                request
                    .target
                    .root()
                    .join(root.file_name().expect("Quellscope zuvor validiert")),
                false,
            )];
            while let Some((path, destination, parent_missing)) = pending.pop() {
                for (endpoint, mutation) in [
                    (self, request.kind == crate::FileQueueKind::Move),
                    (request.target, true),
                ] {
                    if mutation {
                        if let ProviderEndpointInner::Ftps(vfs, _) = &endpoint.inner {
                            vfs.check_preparation_cancel()?;
                        }
                    }
                }
                visited += 1;
                if visited > maximum_entries {
                    return Ok(None);
                }
                let entry = source.stat(&path)?;
                if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
                    effects.stats.symlinks_skipped += 1;
                    continue;
                }
                // Dieselbe geprüfte Konfliktmenge entscheidet auch nach einem
                // späteren Verschwinden des Ziels weiter auf Überspringen.
                if conflicts
                    .iter()
                    .any(|conflict| Path::new(conflict) == destination)
                {
                    effects.stats.skipped += 1;
                    continue;
                }
                if entry.is_file {
                    effects.files = true;
                    continue;
                }
                let missing = if parent_missing {
                    true
                } else {
                    match target.stat(&destination) {
                        Ok(entry) if entry.is_dir && !entry.is_symlink => false,
                        Ok(_) => {
                            return Err(crate::real_mode::plan_drift(
                                "FTPS-Merge-Ziel wechselte seinen Typ",
                            ))
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                        Err(error) => return Err(error),
                    }
                };
                effects.create_directories |= missing;
                let listing = source.list_complete(&path)?;
                crate::fileops::require_complete_safe_listing(&listing, "FTPS-Skip-Wirkung")?;
                if visited + pending.len() + listing.entries.len() > maximum_entries {
                    return Ok(None);
                }
                pending.extend(listing.entries.into_iter().map(|child| {
                    (
                        path.join(&child.entry.name),
                        destination.join(&child.entry.name),
                        missing,
                    )
                }));
            }
            // Fileops behält bei jedem ausgelassenen Nachfahren den vollständigen
            // Top-Level-Quellbaum. Andere vollständig kopierte Auswahlen dürfen
            // weiterhin ihre eigene frisch manifestierte Entfernung benötigen.
            effects.remove_source |= request.kind == crate::FileQueueKind::Move
                && before == (effects.stats.skipped, effects.stats.symlinks_skipped);
        }
        Ok(Some(effects))
    }

    fn directory_only_file_selection(
        &self,
        request: &ProviderFileTargetProbe<'_>,
    ) -> io::Result<Option<bool>> {
        let source = self.file_context()?.0;
        let target = (request.target.kind() == ProviderKind::Ftps)
            .then(|| request.target.file_context())
            .transpose()?
            .map(|context| context.0);
        let mut pending = request
            .items
            .iter()
            .map(|path| {
                let destination = target.as_ref().map(|_| {
                    (
                        request
                            .target
                            .root()
                            .join(path.file_name().expect("Quellscope zuvor validiert")),
                        false,
                    )
                });
                (path.clone(), destination)
            })
            .collect::<Vec<_>>();
        let mut visited = 0usize;
        let mut needs_create = false;
        // Nur vollständig gelesene, begrenzte Ordnerbäume erhalten diese Klasse.
        // Unter einem bekannten fehlenden Ziel wird nicht stat abgefragt: Ein
        // fehlender Elternordner ist bei FTP sonst kein belegtes NotFound.
        const MAX_DIRECTORY_ENTRIES: usize = 100_000;
        while let Some((path, destination)) = pending.pop() {
            for endpoint in [self, request.target] {
                if let ProviderEndpointInner::Ftps(vfs, _) = &endpoint.inner {
                    vfs.check_preparation_cancel()?;
                }
            }
            visited += 1;
            if visited > MAX_DIRECTORY_ENTRIES {
                return Ok(None);
            }
            let entry = source.stat(&path)?;
            if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
                continue;
            }
            if entry.is_file {
                return Ok(None);
            }
            let listing = source.list_complete(&path)?;
            crate::fileops::require_complete_safe_listing(&listing, "FTPS-Ordnerauswahl")?;
            if listing
                .entries
                .iter()
                .any(|item| item.entry.is_file && !item.entry.is_symlink)
            {
                return Ok(None);
            }
            if visited + pending.len() + listing.entries.len() > MAX_DIRECTORY_ENTRIES {
                return Ok(None);
            }
            let child_destination = if let Some((destination, parent_missing)) = destination {
                if parent_missing {
                    needs_create = true;
                    Some((destination, true))
                } else {
                    match target
                        .as_ref()
                        .expect("Ziel nur für FTPS gebunden")
                        .stat(&destination)
                    {
                        Ok(entry) if entry.is_dir && !entry.is_symlink => {
                            Some((destination, false))
                        }
                        Ok(_) => None,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            needs_create = true;
                            Some((destination, true))
                        }
                        Err(error) => return Err(error),
                    }
                }
            } else {
                None
            };
            pending.extend(
                listing
                    .entries
                    .into_iter()
                    .filter(|item| item.entry.is_dir && !item.entry.is_symlink)
                    .map(|item| {
                        let destination = child_destination
                            .as_ref()
                            .map(|(parent, missing)| (parent.join(&item.entry.name), *missing));
                        (path.join(item.entry.name), destination)
                    }),
            );
        }
        Ok(Some(needs_create))
    }

    pub(crate) fn with_queue_owner(
        &self,
        queue: &crate::OperationQueue,
        operation: Option<&str>,
        additional_roots: &[&Path],
    ) -> io::Result<Self> {
        self.with_queue_owner_with_cancel(queue, operation, additional_roots, None)
    }

    pub(crate) fn with_queue_owner_with_cancel(
        &self,
        queue: &crate::OperationQueue,
        operation: Option<&str>,
        additional_roots: &[&Path],
        cancel: Option<&Cancel>,
    ) -> io::Result<Self> {
        match &self.inner {
            ProviderEndpointInner::Ftps(vfs, root) => Self::ftps(
                Arc::new(vfs.for_queue_operation_with_cancel(
                    queue,
                    operation,
                    root,
                    additional_roots,
                    cancel,
                )?),
                root,
            ),
            _ => Ok(self.clone()),
        }
    }

    pub(crate) fn finish_ftps_intents(&self) -> io::Result<()> {
        match &self.inner {
            ProviderEndpointInner::Ftps(vfs, _) => vfs.finish_operation_intents(),
            _ => Ok(()),
        }
    }
    pub fn local(root: &Path) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let metadata = std::fs::symlink_metadata(&root)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid(
                "Lokaler Provider-Endpunkt ist kein echtes Verzeichnis",
            ));
        }
        Ok(Self {
            inner: ProviderEndpointInner::Local(root),
        })
    }

    pub fn sftp(vfs: Arc<SftpVfs>, root: &Path) -> io::Result<Self> {
        Self::remote(ProviderEndpointInner::Sftp(vfs, root.to_path_buf()), root)
    }

    /// Testet den providerneutralen Orchestrator gegen echte lokale, descriptor-
    /// relative Mutationen. Identität und Capability-Proben echter SFTP-Sitzungen
    /// bleiben zusätzlich dem isolierten SFTP-E2E vorbehalten.
    #[cfg(test)]
    pub(crate) fn test_sftp_local(root: &Path) -> io::Result<Self> {
        Self::test_remote_local(ProviderKind::Sftp, root)
    }

    /// Simuliert ausschließlich die providerneutrale Orchestrierung mit lokalen
    /// Wegwerfbäumen. Echte Protokollidentität und Capability-Proben bleiben den
    /// jeweiligen opt-in E2E-Tests vorbehalten.
    #[cfg(test)]
    pub(crate) fn test_remote_local(kind: ProviderKind, root: &Path) -> io::Result<Self> {
        if kind == ProviderKind::Local {
            return Err(invalid(
                "Test-Remote-Endpunkt benötigt eine Remote-Providerart",
            ));
        }
        let root = std::fs::canonicalize(root)?;
        validate_real_target(&root)?;
        Ok(Self {
            inner: ProviderEndpointInner::TestRemoteLocal(kind, root),
        })
    }

    pub fn ftps(vfs: Arc<FtpsVfs>, root: &Path) -> io::Result<Self> {
        Self::remote(ProviderEndpointInner::Ftps(vfs, root.to_path_buf()), root)
    }

    pub fn webdav(vfs: Arc<WebDavVfs>, root: &Path) -> io::Result<Self> {
        Self::remote(ProviderEndpointInner::WebDav(vfs, root.to_path_buf()), root)
    }

    pub fn smb(vfs: Arc<SmbVfs>, root: &Path) -> io::Result<Self> {
        Self::remote(ProviderEndpointInner::Smb(vfs, root.to_path_buf()), root)
    }

    fn remote(inner: ProviderEndpointInner, root: &Path) -> io::Result<Self> {
        validate_remote_root(root, "Remote-Endpunkt")?;
        Ok(Self { inner })
    }

    pub fn kind(&self) -> ProviderKind {
        match self.inner {
            ProviderEndpointInner::Local(_) => ProviderKind::Local,
            ProviderEndpointInner::TestRemoteLocal(kind, _) => kind,
            ProviderEndpointInner::Sftp(_, _) => ProviderKind::Sftp,
            ProviderEndpointInner::Ftps(_, _) => ProviderKind::Ftps,
            ProviderEndpointInner::WebDav(_, _) => ProviderKind::WebDav,
            ProviderEndpointInner::Smb(_, _) => ProviderKind::Smb,
        }
    }

    pub fn root(&self) -> &Path {
        match &self.inner {
            ProviderEndpointInner::Local(root)
            | ProviderEndpointInner::TestRemoteLocal(_, root)
            | ProviderEndpointInner::Sftp(_, root)
            | ProviderEndpointInner::Ftps(_, root)
            | ProviderEndpointInner::WebDav(_, root)
            | ProviderEndpointInner::Smb(_, root) => root,
        }
    }

    /// Bindet dieselbe bereits identitätsgeprüfte Remote-Sitzung an einen anderen
    /// virtuellen Pane-Root desselben Profil-Namespace. Für lokale Endpunkte wird
    /// bewusst keine implizite gemeinsame Rename-Grenze erfunden.
    pub fn with_root(&self, root: &Path) -> io::Result<Self> {
        match &self.inner {
            ProviderEndpointInner::Sftp(vfs, _) => Self::sftp(vfs.clone(), root),
            ProviderEndpointInner::Ftps(vfs, _) => Self::ftps(vfs.clone(), root),
            ProviderEndpointInner::WebDav(vfs, _) => Self::webdav(vfs.clone(), root),
            ProviderEndpointInner::Smb(vfs, _) => Self::smb(vfs.clone(), root),
            ProviderEndpointInner::Local(_) | ProviderEndpointInner::TestRemoteLocal(_, _) => {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Lokale Pane-Roots teilen ohne gemeinsame descriptor-relative Wurzel keinen Rename-Schnellweg",
                ))
            }
        }
    }

    /// Liefert eine gehaltene VFS-Sicht für genau den bereits validierten Pane-Root.
    /// Lokale Endpunkte bleiben descriptor-relativ an diesen Root gebunden; Remote-
    /// Endpunkte behalten ihre kanonische Profilwurzel und Sitzungsidentität.
    pub fn file_context(&self) -> io::Result<(Arc<dyn Vfs>, PathBuf)> {
        let root = self.root().to_path_buf();
        let vfs: Arc<dyn Vfs> = match &self.inner {
            ProviderEndpointInner::Local(root)
            | ProviderEndpointInner::TestRemoteLocal(_, root) => Arc::new(LocalVfs::rooted(root)?),
            ProviderEndpointInner::Sftp(vfs, _) => vfs.clone(),
            ProviderEndpointInner::Ftps(vfs, _) => vfs.clone(),
            ProviderEndpointInner::WebDav(vfs, _) => vfs.clone(),
            ProviderEndpointInner::Smb(vfs, _) => vfs.clone(),
        };
        Ok((vfs, root))
    }

    /// Nur für interaktives Öffnen und additive lokale Downloads.
    /// Der SFTP-Adapter liefert ausdrücklich keine Planner-Root-/Listingbelege.
    pub fn interactive_file_read_context(
        &self,
        items: &[PathBuf],
        cancel: &Cancel,
    ) -> io::Result<(Arc<dyn Vfs>, PathBuf)> {
        if let ProviderEndpointInner::Sftp(vfs, root) = &self.inner {
            return Ok((vfs.interactive_files(root, items, cancel)?, root.clone()));
        }
        self.file_context()
    }

    /// Gemeinsame Auswahl der interaktiven Inhaltsklasse für GUI, CLI und Queue.
    /// Lokale additive Inhaltsauswahlen verwenden die eng gebundene WebDAV-Sicht.
    pub fn interactive_file_contexts(
        &self,
        target: &Self,
        items: &[PathBuf],
        move_source: bool,
        overwrite: bool,
        cancel: &Cancel,
    ) -> io::Result<InteractiveFileContexts> {
        self.interactive_file_contexts_owned(target, items, move_source, overwrite, cancel, None)
    }

    pub(crate) fn interactive_file_contexts_owned(
        &self,
        target: &Self,
        items: &[PathBuf],
        move_source: bool,
        overwrite: bool,
        cancel: &Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<InteractiveFileContexts> {
        self.validate_interactive_file_transfer(target, move_source, overwrite)?;
        // Die reine Leseseite benötigt keinen Probenindex. Benötigte FTPS-
        // Schreibseiten dagegen vor den Proben anderer Provider prüfen.
        if move_source {
            if let ProviderEndpointInner::Ftps(vfs, _) = &self.inner {
                vfs.require_mutation_owner()?;
            }
        }
        if let ProviderEndpointInner::Ftps(vfs, _) = &target.inner {
            vfs.require_mutation_owner()?;
        }
        crate::fileops::validate_file_operation_paths(items, target.root())?;
        let (source_vfs, _) = self.interactive_file_read_context(items, cancel)?;
        if self.kind() == ProviderKind::Local && !move_source && !overwrite {
            if let ProviderEndpointInner::WebDav(vfs, root) = &target.inner {
                drop(source_vfs);
                let source_vfs =
                    Arc::new(LocalVfs::rooted(self.root())?.with_verified_reads(items, cancel)?);
                let target_vfs = vfs.interactive_upload(root, cancel, owner)?;
                return Ok(InteractiveFileContexts {
                    source_vfs,
                    target_vfs,
                    target_capabilities: Some(crate::webdav::ordinary_upload_capabilities()),
                });
            }
        }
        Ok(InteractiveFileContexts {
            source_vfs,
            target_vfs: target.file_context()?.0,
            target_capabilities: None,
        })
    }

    pub fn validate_interactive_file_transfer(
        &self,
        target: &Self,
        move_source: bool,
        overwrite: bool,
    ) -> io::Result<()> {
        validate_interactive_file_transfer_request(
            self.kind(),
            target.kind(),
            move_source,
            overwrite,
        )
    }

    pub fn interactive_file_read_limitations(&self) -> Vec<String> {
        if self.kind() == ProviderKind::Sftp {
            vec![crate::sftp::SFTP_CONTENT_READ_LIMITATION.into()]
        } else {
            Vec::new()
        }
    }

    pub fn download_interactive_snapshot(
        &self,
        source: &Path,
        cache: &Path,
        cancel: &Cancel,
    ) -> io::Result<crate::RemoteSnapshot> {
        let (vfs, _) = self.interactive_file_read_context(&[source.to_path_buf()], cancel)?;
        if self.kind() == ProviderKind::Sftp {
            crate::remote_snapshot::download_passive_remote_snapshot(
                vfs.as_ref(),
                source,
                cache,
                cancel,
            )
        } else {
            crate::download_remote_snapshot(vfs.as_ref(), source, cache, cancel)
        }
    }

    pub fn list_interactive(&self) -> io::Result<ProviderInteractiveListing> {
        if let ProviderEndpointInner::Sftp(vfs, root) = &self.inner {
            let listing = vfs.list_interactive_best_effort(root)?;
            return Ok(ProviderInteractiveListing {
                entries: listing.entries,
                omitted: listing.omitted,
                complete: false,
                security: Some(listing.security),
            });
        }
        let (vfs, root) = self.file_context()?;
        let listing = vfs.list_complete(&root)?;
        if !listing.complete {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Provider-Listing ist nicht vollständig",
            ));
        }
        Ok(ProviderInteractiveListing {
            entries: listing
                .entries
                .into_iter()
                .map(|listed| ProviderInteractiveEntry {
                    entry: listed.entry,
                    safely_decoded: listed.safely_decoded,
                    size_known: true,
                })
                .collect(),
            omitted: Vec::new(),
            complete: true,
            security: None,
        })
    }

    /// Verbindungsteilung benötigt dieselbe Profilroot-Abbildung; ein gemeinsamer
    /// physischer Servernamespace allein genügt bei FTPS dafür nicht.
    pub(crate) fn shares_connection_mapping(&self, other: &Self) -> bool {
        match (&self.inner, &other.inner) {
            (ProviderEndpointInner::Ftps(source, _), ProviderEndpointInner::Ftps(target, _)) => {
                source.shares_connection_mapping(target)
            }
            _ => true,
        }
    }

    /// Bindet die Namespace-Beziehung aus den Identitäten der beiden lebenden
    /// Sitzungen. Profil-IDs oder serialisierte Verbindungsdaten sind dafür kein
    /// Beweis: Zwei Profile können denselben Serverbaum zeigen und umgekehrt.
    ///
    /// Die Überlappungsprüfung ist Teil dieses lesenden Schritts und muss vor jeder
    /// Capability-Probe stattfinden, weil einzelne Providerproben mutieren können.
    pub fn shares_runtime_namespace(&self, other: &Self) -> io::Result<bool> {
        // Verschiedene Providerklassen teilen keinen adressierbaren Namespace.
        // Dies behauptet keine atomare SFTP-Rootidentität und gibt weder Sync noch
        // Remote-Mutationen frei. Additive lokale Downloads ersetzen keine Quelle.
        if (self.kind() == ProviderKind::Sftp && other.kind() == ProviderKind::Local)
            || (self.kind() == ProviderKind::Local && other.kind() == ProviderKind::Sftp)
        {
            return Ok(false);
        }
        let (source_vfs, source_root) = self.file_context()?;
        let (target_vfs, target_root) = other.file_context()?;
        let source_identity = source_vfs.root_identity(&source_root)?;
        let target_identity = target_vfs.root_identity(&target_root)?;
        let (same_namespace, source_path, target_path) = root_namespace_relation(
            &source_identity,
            &source_root,
            &target_identity,
            &target_root,
        );
        reject_related_paths(same_namespace, &source_path, &target_path)?;
        Ok(same_namespace)
    }

    /// Gewöhnliche Ordneraddition mit eigener Ausführungsklasse. Der WebDAV-Weg
    /// übernimmt keine Sync-/Löschfähigkeiten und setzt solche auch nicht frei.
    pub fn create_interactive_directory(
        &self,
        name: &str,
        expected_class: crate::DirectoryCreateExecutionClass,
    ) -> io::Result<crate::DirectoryCreateReport> {
        let name = if self.kind() == ProviderKind::Local {
            crate::validate_directory_create_name(name)?
        } else {
            crate::validate_directory_create_request(self.root(), name)?
        };
        crate::validate_directory_create_class(self.kind(), expected_class)?;
        if let ProviderEndpointInner::WebDav(vfs, root) = &self.inner {
            return vfs.create_interactive_directory(root, &name);
        }
        self.probe_directory_create_capability()?;
        let (vfs, parent) = self.file_context()?;
        let path = crate::create_dir(vfs.as_ref(), &parent, &name, &None)?;
        self.finish_ftps_intents()?;
        Ok(crate::DirectoryCreateReport {
            path,
            execution_class: expected_class,
            limitations: crate::directory_create_contract(self.kind()).limitations,
        })
    }

    /// Prüft ausschließlich exklusives Verzeichnisanlegen. Diese schmale Fähigkeit
    /// hängt bewusst weder an Upload noch Replace, Quarantäne oder Mirror.
    pub fn probe_directory_create_capability(&self) -> io::Result<RemoteCapabilities> {
        match &self.inner {
            ProviderEndpointInner::Local(_) | ProviderEndpointInner::TestRemoteLocal(_, _) => {
                Ok(RemoteCapabilities {
                    exclusive_directory_create: true,
                    ..RemoteCapabilities::default()
                })
            }
            ProviderEndpointInner::Sftp(vfs, root) => {
                vfs.probe_directory_create_capability_at(root)
            }
            ProviderEndpointInner::Ftps(vfs, root) => {
                vfs.probe_directory_create_capability_at(root)
            }
            ProviderEndpointInner::WebDav(vfs, root) => {
                vfs.probe_directory_create_capability_at(root)
            }
            ProviderEndpointInner::Smb(vfs, root) => vfs.probe_directory_create_capability_at(root),
        }
    }

    /// Momentaufnahme der aktuell tatsächlich gebundenen Sync-Zielwirkungen. Der
    /// Aufrufer muss die nötigen Verhaltensproben zuvor ausgeführt haben.
    pub fn current_transfer_capabilities(&self) -> ProviderTransferCapabilities {
        match &self.inner {
            ProviderEndpointInner::Local(_) => ProviderTransferCapabilities::local(),
            ProviderEndpointInner::TestRemoteLocal(kind, _) => ProviderTransferCapabilities {
                target: *kind,
                add_only: true,
                update: matches!(kind, ProviderKind::Sftp | ProviderKind::Smb),
                mirror: *kind == ProviderKind::Smb,
                available_space_known: true,
                quarantine_verified: matches!(kind, ProviderKind::Sftp | ProviderKind::Smb),
                durable_directory_flush: *kind == ProviderKind::Smb,
            },
            ProviderEndpointInner::Sftp(vfs, _) => {
                ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities())
            }
            ProviderEndpointInner::Ftps(vfs, _) => {
                ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities())
            }
            ProviderEndpointInner::WebDav(vfs, _) => {
                ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities())
            }
            ProviderEndpointInner::Smb(vfs, _) => {
                ProviderTransferCapabilities::remote(self.kind(), vfs.capabilities())
            }
        }
    }

    /// Bereitet ausschließlich die Wirkungen für den Restore eines vorhandenen
    /// Quarantäneeintrags vor: No-Replace-Rename plus atomare Manifestaktualisierung.
    /// Die Probe läuft unter der Single-Flight-Schranke des Aufrufers.
    pub fn probe_quarantine_restore_capability(&self) -> io::Result<()> {
        if !self.kind().quarantine_management_capabilities().restore {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Dieser Provider bindet Restore noch nicht atomar an den manifestierten Quarantänelauf",
            ));
        }
        match &self.inner {
            ProviderEndpointInner::TestRemoteLocal(kind, _)
                if kind.supports_quarantine_management() =>
            {
                Ok(())
            }
            ProviderEndpointInner::Sftp(vfs, root) => {
                vfs.probe_add_only_capabilities_at(root)?;
                vfs.probe_rename_capability_at(root)?;
                require_quarantine_restore_contract(vfs.capabilities(), "SFTP")
            }
            ProviderEndpointInner::Smb(vfs, root) => {
                vfs.probe_add_only_capabilities_at(root)?;
                vfs.probe_rename_capability_at(root)?;
                require_quarantine_restore_contract(vfs.capabilities(), "SMB")
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Dieser Provider besitzt keine verwaltbare Remote-Quarantäne",
            )),
        }
    }

    /// Bereitet das endgültige Leeren eines vorhandenen Quarantänelaufs vor. Seit
    /// Lösch- und Uploadvertrag getrennt sind, genügt hier gezielt die Löschprobe;
    /// ein Datei-Upload ist für das Entfernen des manifestierten Laufs irrelevant.
    pub fn probe_quarantine_purge_capability(&self) -> io::Result<()> {
        if !self.kind().quarantine_management_capabilities().purge {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Dieser Provider bindet Purge noch nicht atomar an den manifestierten Quarantänelauf",
            ));
        }
        match &self.inner {
            ProviderEndpointInner::TestRemoteLocal(kind, _)
                if kind.supports_quarantine_management() =>
            {
                Ok(())
            }
            ProviderEndpointInner::Sftp(vfs, root) => {
                require_quarantine_purge_contract(vfs.probe_delete_capability_at(root)?, "SFTP")
            }
            ProviderEndpointInner::Smb(vfs, root) => {
                require_quarantine_purge_contract(vfs.probe_delete_capability_at(root)?, "SMB")
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Dieser Provider besitzt keine verwaltbare Remote-Quarantäne",
            )),
        }
    }

    /// Führt nur die zerstörungsfreien Proben aus, die interaktives F5/F6/Rename
    /// tatsächlich benötigen. Die Rückgabe beschreibt Blocker pro Wirkung.
    pub fn probe_file_capabilities(&self) -> io::Result<ProviderFileCapabilities> {
        self.file_capabilities(true, true)
    }

    /// Ein Kopierziel benötigt keine künstliche Quellentfernung. Die vollständige
    /// STOU-/Rename-Probe belegt dabei auch ihren eigenen Verzeichnis-Container.
    pub fn probe_file_target_capabilities(&self) -> io::Result<ProviderFileCapabilities> {
        self.file_capabilities(false, true)
    }

    /// Eine Move-Quelle benötigt keinen Datei-Commit- oder Renameversuch.
    pub fn probe_file_source_removal_capabilities(&self) -> io::Result<ProviderFileCapabilities> {
        if let ProviderEndpointInner::Ftps(vfs, root) = &self.inner {
            if vfs.directories_only() {
                vfs.probe_directory_create_capability_at(root)?;
                return Ok(ProviderFileCapabilities {
                    provider: ProviderKind::Ftps, copy_new: false, copy_new_best_effort: false,
                    replace: false, replace_best_effort: false, remove_source: false,
                    remove_source_best_effort: true, rename_noreplace: false,
                    limitations: vec!["FTPS entfernt ausschließlich manifestgebundene, frisch leer gelistete Ordner mit RMD und Readback (Best Effort). Dateien und rekursives Löschen bleiben gesperrt.".into()],
                    blockers: Vec::new(),
                });
            }
            let capabilities = vfs.probe_permanent_delete_capability_at(root)?;
            return Ok(ProviderFileCapabilities {
                provider: ProviderKind::Ftps, copy_new: false, copy_new_best_effort: false,
                replace: false, replace_best_effort: false, remove_source: false,
                remove_source_best_effort: capabilities.permanent_delete,
                rename_noreplace: false,
                limitations: vec!["FTPS-Quellentfernung folgt ausschließlich dem bestätigten manifestgebundenen Best-Effort-Vertrag; zwischen Readback und DELE/RMD bleibt ein Austauschfenster.".into()],
                blockers: Vec::new(),
            });
        }
        self.probe_file_capabilities()
    }

    fn file_capabilities(
        &self,
        probe_source_removal: bool,
        probe_content: bool,
    ) -> io::Result<ProviderFileCapabilities> {
        let provider = self.kind();
        let mut blockers = Vec::new();
        let mut limitations = Vec::new();
        // Ein anderer paralleler Aufruf kann den gemeinsamen Cache inzwischen
        // neu belegen. Nur das Ergebnis dieser eigenen Teilprobe erteilt Ersetzen.
        let mut ftps_replace_best_effort = false;
        let (
            copy_new,
            copy_new_best_effort,
            remove_source,
            remove_source_best_effort,
            rename_noreplace,
        ) = match &self.inner {
            ProviderEndpointInner::Local(root)
            | ProviderEndpointInner::TestRemoteLocal(_, root) => {
                // Der lokale VFS begrenzt jede Wirkung descriptor-relativ auf genau
                // ein Blatt beziehungsweise einen frisch leeren Ordner. Er besitzt
                // hier aber keinen atomaren Token zwischen Readback und Remove.
                let assured = LocalVfs::rooted(root)?.file_commit_class_at(root)?
                    == crate::vfs::FileCommitClass::Assured;
                if !assured {
                    limitations.push(crate::vfs::LOCAL_FILE_COMMIT_LIMITATION.into());
                }
                (assured, !assured, false, true, assured)
            }
            ProviderEndpointInner::Sftp(vfs, _) => {
                // Die interaktiven SFTP-v3-Standardwirkungen werden vom Provider
                // bereits als schwächere lebende Fähigkeiten ausgewiesen. Hier darf
                // keine weitere Probe mutieren: Ohne bedingtes mkdir ließe sich ihr
                // eigener Cleanup nicht sicher an den angelegten Namen binden.
                let capabilities = vfs.capabilities();
                let assured =
                    capabilities.permits_add_only() && capabilities.permits_directory_create();
                let best_effort = !assured
                    && capabilities.complete_listing
                    && capabilities.best_effort_file_commit;
                if best_effort {
                    limitations.push(
                        "Nur neue reguläre Dateien laufen als Best Effort; SFTP-Ordneranlage bleibt ausdrücklich gesperrt."
                            .into(),
                    );
                }
                if !capabilities.permits_directory_create() {
                    limitations.push(
                        "SFTP-Ordneranlage ist gesperrt: SFTP v3 bindet MKDIR nicht atomar an den gewählten Pane-Root und Elternordner."
                            .into(),
                    );
                }
                blockers.push(
                    "SFTP-Dateidownload und Sync sind gesperrt: SFTP v3 bindet READDIR, LSTAT und OPEN nicht atomar an den gewählten Pane-Root. Nur das interaktive Pane-Listing läuft sichtbar als pfadbasiertes Best Effort."
                        .into(),
                );
                // Auch REMOVE/RMDIR besitzt keine Elternhandle-Bindung. Ein
                // vorheriger Readback begrenzt die spätere Außenwirkung nicht.
                let removable_best_effort = false;
                (
                    assured,
                    best_effort,
                    false,
                    removable_best_effort,
                    capabilities.atomic_noreplace_rename,
                )
            }
            ProviderEndpointInner::Ftps(vfs, root) => {
                let removable = if probe_source_removal {
                    vfs.probe_directory_create_capability_at(root)?;
                    match vfs.probe_permanent_delete_capability_at(root) {
                        Ok(value) => value.permanent_delete,
                        Err(error)
                            if matches!(
                                crate::error_code(&error),
                                crate::ErrorCode::OutcomeUnclear
                                    | crate::ErrorCode::ConnectionLost
                                    | crate::ErrorCode::PlanDrift
                                    | crate::ErrorCode::Interrupted
                            ) =>
                        {
                            return Err(error)
                        }
                        Err(error) => {
                            blockers.push(format!("FTPS-Quellentfernung: {error}"));
                            false
                        }
                    }
                } else {
                    false
                };
                let content_capabilities = if probe_content {
                    vfs.probe_add_only_capabilities_at(root)
                } else {
                    vfs.file_capabilities_at(root)
                };
                match content_capabilities {
                    Ok(capabilities) => {
                        ftps_replace_best_effort = capabilities.best_effort_file_replace;
                        let assured = capabilities.permits_add_only()
                            && capabilities.permits_directory_create();
                        // Ein Server mit überschreibendem RNTO verliert neue Dateien
                        // nicht mehr komplett: Der Best-Effort-Commit ist bewiesen
                        // und wird als eigene, sichtbar eingeschränkte Wirkung gemeldet.
                        let best_effort = !assured
                            && capabilities.complete_listing
                            && capabilities.best_effort_file_commit
                            && capabilities.permits_directory_create();
                        if best_effort {
                            limitations.push(
                                "Neue Dateien laufen als Best-Effort-Commit: Der Server ersetzt bei RNTO vorhandene Ziele; der Bestand wird unmittelbar vor dem Commit geprüft, ein kleines Kollisionsfenster bleibt."
                                    .into(),
                            );
                        }
                        (
                            assured,
                            best_effort,
                            false,
                            removable,
                            capabilities.atomic_noreplace_rename,
                        )
                    }
                    Err(error)
                        if matches!(
                            crate::error_code(&error),
                            crate::ErrorCode::OutcomeUnclear
                                | crate::ErrorCode::ConnectionLost
                                | crate::ErrorCode::PlanDrift
                                | crate::ErrorCode::Interrupted
                        ) =>
                    {
                        return Err(error)
                    }
                    Err(error) => {
                        blockers.push(format!("FTPS-Datei-Commit: {error}"));
                        (false, false, false, removable, false)
                    }
                }
            }
            ProviderEndpointInner::WebDav(_, _) => {
                blockers.push(crate::webdav::STRICT_MUTATION_BLOCKER.into());
                (false, false, false, false, false)
            }
            ProviderEndpointInner::Smb(vfs, root) => {
                let directory_create = match vfs.probe_directory_create_capability_at(root) {
                    Ok(capabilities) => capabilities.permits_directory_create(),
                    Err(error) => {
                        blockers.push(Self::probe_blocker_or_error(
                            error,
                            "SMB-Verzeichnis-Create",
                        )?);
                        false
                    }
                };
                let removable = if directory_create {
                    match vfs.probe_delete_capability_at(root) {
                        Ok(capabilities) => capabilities.permits_entry_delete(),
                        Err(error) => {
                            blockers
                                .push(Self::probe_blocker_or_error(error, "SMB-Quellentfernung")?);
                            false
                        }
                    }
                } else {
                    false
                };
                match vfs.probe_add_only_capabilities_at(root) {
                    Ok(capabilities) => (
                        capabilities.permits_add_only() && directory_create,
                        false,
                        false,
                        removable,
                        capabilities.atomic_noreplace_rename,
                    ),
                    Err(error) => {
                        blockers.push(Self::probe_blocker_or_error(error, "SMB-Datei-Commit")?);
                        (false, false, false, removable, false)
                    }
                }
            }
        };
        let replace = if provider == ProviderKind::Local {
            false
        } else if copy_new && rename_noreplace {
            let (vfs, _) = self.file_context()?;
            if !vfs.supports_rename_noreplace_if_matches() {
                blockers.push(
                    "Ersetzen benötigt ein identitätsgebundenes bedingtes Rename; ein normales No-Replace-Rename bindet die geparkte Altversion nicht bis zur Wirkung"
                        .into(),
                );
                false
            } else {
                match self.probe_delete_capabilities() {
                    Ok(capabilities) if capabilities.quarantine => true,
                    Ok(capabilities) => {
                        blockers.extend(capabilities.blockers);
                        false
                    }
                    Err(error) => {
                        blockers.push(Self::probe_blocker_or_error(error, "Ersetzungsquarantäne")?);
                        false
                    }
                }
            }
        } else {
            false
        };
        let replace_best_effort = !replace
            && match &self.inner {
                ProviderEndpointInner::Local(_) | ProviderEndpointInner::TestRemoteLocal(_, _) => {
                    true
                }
                ProviderEndpointInner::Ftps(_, _) => ftps_replace_best_effort,
                ProviderEndpointInner::WebDav(vfs, _) => {
                    vfs.capabilities().best_effort_file_replace
                }
                _ => false,
            };
        if !replace {
            if replace_best_effort {
                limitations.push(if provider == ProviderKind::Local {
                    "Lokales Ersetzen läuft sichtbar als Best Effort: Ohne atomaren bedingten Backup-Cleanup wird die bestätigte Altdatei nicht als wiederherstellbare Sicherung versprochen; Doppeldecker prüft das Ziel unmittelbar vorher, mutiert genau einmal und liest das Ergebnis zurück."
                        .into()
                } else {
                    "Bestätigte vorhandene reguläre Dateien können als Best Effort ersetzt werden: Die Altversion wird ohne Quarantäne dauerhaft verworfen; Doppeldecker prüft das Ziel unmittelbar vorher, mutiert genau einmal und liest das Ergebnis zurück."
                        .into()
                });
            } else {
                blockers.push(
                    "Ersetzen benötigt eine geprüfte Quarantäne oder eine getrennte Best-Effort-Ersetzungsprobe"
                        .into(),
                );
            }
        }
        if remove_source_best_effort {
            limitations.push(
                "Move-Quellen werden als Best Effort nach vollständigem frischem Readback manifestgebunden Blatt für Blatt und Ordner nur leer entfernt; zwischen Readback und Einzelwirkung bleibt ein begrenztes Austauschfenster."
                    .into(),
            );
        } else if !remove_source {
            blockers.push(
                "Quellentfernung ist für diese Verbindung weder provideratomar noch als begrenzter Best-Effort-Pfad verfügbar"
                    .into(),
            );
        }
        if !rename_noreplace {
            blockers.push("No-Replace-Rename ist für diese Sitzung nicht bewiesen".into());
        }
        Ok(ProviderFileCapabilities {
            provider,
            copy_new,
            copy_new_best_effort,
            replace,
            replace_best_effort,
            remove_source,
            remove_source_best_effort,
            rename_noreplace,
            limitations,
            blockers,
        })
    }

    /// Prüft den Löschpfad der konkreten Sitzung. SFTP und SMB werden nur nach einer
    /// echten Parken-/Restore-Probe als wiederherstellbar freigegeben. WebDAV
    /// darf andernfalls ausschließlich den getrennt bestätigten, ETag-bedingten
    /// endgültigen Pfad anbieten; FTPS besitzt keine gleichwertige Bedingung.
    pub fn probe_delete_capabilities(&self) -> io::Result<ProviderDeleteCapabilities> {
        let provider = self.kind();
        let mut blockers = Vec::new();
        // Die Restore-Probe benutzt später genau `rename_noreplace_if_matches`.
        // Solange der Provider diese Primitive nicht wirklich implementiert, darf
        // die Capability-Ermittlung keine Quarantäneobjekte anlegen und danach so
        // tun, als könne sie deren Identität bis zum Rückweg halten. WebDAV darf
        // unabhängig davon seinen bedingten endgültigen Löschweg prüfen; FTPS,
        // SFTP und SMB bleiben ohne starke Quarantäne mutationsfrei zu.
        match &self.inner {
            ProviderEndpointInner::Ftps(vfs, _) if !vfs.supports_bound_restore_probe() => {
                blockers.push(
                    "FTPS-Quarantäne benötigt ein identitätsgebundenes bedingtes Rename".into(),
                );
                blockers.push(
                    "FTPS-Dauerlöschen ist gesperrt: DELE/RMD kann den bestätigten Objekt-Token nicht atomar bis in die Löschwirkung tragen".into(),
                );
                return Ok(ProviderDeleteCapabilities {
                    provider,
                    quarantine: false,
                    permanent: false,
                    blockers,
                });
            }
            ProviderEndpointInner::WebDav(vfs, root) if !vfs.supports_bound_restore_probe() => {
                blockers.push(
                    "WebDAV-Quarantäne benötigt ein identitätsgebundenes bedingtes Rename".into(),
                );
                let permanent = if vfs.capabilities().permanent_delete {
                    true
                } else {
                    match vfs.probe_permanent_delete_capability_at(root) {
                        Ok(capabilities) => capabilities.permanent_delete,
                        Err(error) => {
                            blockers
                                .push(Self::probe_blocker_or_error(error, "WebDAV-Dauerlöschen")?);
                            false
                        }
                    }
                };
                return Ok(ProviderDeleteCapabilities {
                    provider,
                    quarantine: false,
                    permanent,
                    blockers,
                });
            }
            ProviderEndpointInner::Sftp(vfs, _) if !vfs.supports_bound_restore_probe() => {
                blockers.push(
                    "SFTP-Quarantäne benötigt ein identitätsgebundenes bedingtes Rename".into(),
                );
                return Ok(ProviderDeleteCapabilities {
                    provider,
                    quarantine: false,
                    permanent: false,
                    blockers,
                });
            }
            ProviderEndpointInner::Smb(vfs, _) if !vfs.supports_bound_restore_probe() => {
                blockers.push(
                    "SMB-Quarantäne benötigt ein identitätsgebundenes bedingtes Rename".into(),
                );
                return Ok(ProviderDeleteCapabilities {
                    provider,
                    quarantine: false,
                    permanent: false,
                    blockers,
                });
            }
            _ => {}
        }
        let (quarantine, permanent) = match &self.inner {
            ProviderEndpointInner::Local(_) => (false, false),
            ProviderEndpointInner::TestRemoteLocal(kind, _) => {
                let quarantine = matches!(kind, ProviderKind::Sftp | ProviderKind::Smb);
                if !quarantine {
                    blockers.push(
                        "Testendpunkt simuliert keinen wiederherstellbaren Löschvertrag".into(),
                    );
                }
                (quarantine, false)
            }
            ProviderEndpointInner::Sftp(vfs, root) => {
                let base = root.join(".doppeldecker-quarantine");
                let mut bound_base = None;
                let result = (|| {
                    let directory = vfs.probe_directory_create_capability_at(root)?;
                    let delete = vfs.probe_delete_capability_at(root)?;
                    let add_only = vfs.probe_add_only_capabilities_at(root)?;
                    let rename = vfs.probe_rename_capability_at(root)?;
                    // Diese vier SFTP-Abfragen sind absichtlich rein lesend. Erst
                    // wenn sie den vollständigen starken Vertrag bereits tragen,
                    // darf die Restore-Probe überhaupt einen Namen anlegen. Sonst
                    // könnte schon ihr eigener Cleanup fremd ausgetauschte Inhalte
                    // pfadbasiert entfernen.
                    if !directory.permits_directory_create()
                        || !delete.permits_entry_delete()
                        || !add_only.permits_add_only()
                        || !rename.atomic_noreplace_rename
                        || !rename.atomic_replace_rename
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "SFTP-Quarantäne benötigt vor jeder Probe bereits identitätsgebundene Create-, Delete- und Rename-Fähigkeiten",
                        ));
                    }
                    bound_base = Some(ensure_probe_quarantine_root(vfs.as_ref(), root, &base)?);
                    run_bound_restore_probe(
                        vfs.as_ref(),
                        root,
                        bound_base.as_ref().expect("unmittelbar zuvor gesetzt"),
                    )?;
                    vfs.mark_quarantine_verified(vfs.session_generation())?;
                    Ok::<_, io::Error>(())
                })();
                let cleanup = cleanup_probe_quarantine_root(
                    vfs.as_ref(),
                    root,
                    &base,
                    bound_base
                        .as_ref()
                        .and_then(|bound| bound.removal_proof.as_ref()),
                );
                let result =
                    combine_probe_outcome("Quarantäne", result, cleanup, &base.to_string_lossy());
                match result {
                    Ok(()) => (true, false),
                    Err(error) => {
                        blockers.push(Self::probe_blocker_or_error(error, "SFTP-Quarantäne")?);
                        (false, false)
                    }
                }
            }
            ProviderEndpointInner::Smb(vfs, root) => {
                let base = root.join(".doppeldecker-quarantine");
                let mut bound_base = None;
                let result = (|| {
                    vfs.probe_directory_create_capability_at(root)?;
                    vfs.probe_delete_capability_at(root)?;
                    vfs.probe_add_only_capabilities_at(root)?;
                    vfs.probe_rename_capability_at(root)?;
                    bound_base = Some(ensure_probe_quarantine_root(vfs.as_ref(), root, &base)?);
                    run_bound_restore_probe(
                        vfs.as_ref(),
                        root,
                        bound_base.as_ref().expect("unmittelbar zuvor gesetzt"),
                    )?;
                    vfs.mark_quarantine_verified(vfs.session_generation())?;
                    Ok::<_, io::Error>(())
                })();
                let cleanup = cleanup_probe_quarantine_root(
                    vfs.as_ref(),
                    root,
                    &base,
                    bound_base
                        .as_ref()
                        .and_then(|bound| bound.removal_proof.as_ref()),
                );
                let result =
                    combine_probe_outcome("Quarantäne", result, cleanup, &base.to_string_lossy());
                match result {
                    Ok(()) => (true, false),
                    Err(error) => {
                        blockers.push(Self::probe_blocker_or_error(error, "SMB-Quarantäne")?);
                        (false, false)
                    }
                }
            }
            ProviderEndpointInner::Ftps(vfs, root) => {
                let base = root.join(".doppeldecker-quarantine");
                let mut bound_base = None;
                let quarantine_result = (|| {
                    vfs.probe_directory_create_capability_at(root)?;
                    vfs.probe_add_only_capabilities_at(root)?;
                    let rename_capabilities = vfs.probe_rename_capability_at(root)?;
                    // Ohne No-Replace-Beweis kann die Quarantäne nie freigegeben
                    // werden (`mark_quarantine_verified` verlangt ihn). Der frühe
                    // Abbruch erspart dem Server die Anlage des Quarantäneordners
                    // samt Restore-Probe, die ohnehin am Rename-Gate scheitern würde.
                    if !rename_capabilities.atomic_noreplace_rename {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "FTPS-Quarantäne benötigt bewiesenes No-Replace-Rename",
                        ));
                    }
                    bound_base = Some(ensure_probe_quarantine_root(vfs.as_ref(), root, &base)?);
                    run_bound_restore_probe(
                        vfs.as_ref(),
                        root,
                        bound_base.as_ref().expect("unmittelbar zuvor gesetzt"),
                    )?;
                    vfs.mark_quarantine_verified(vfs.session_generation())?;
                    Ok::<_, io::Error>(())
                })();
                // FTPS schützt normale RMD-Aufrufe bis zur expliziten Löschprobe.
                // Die Probe wird deshalb auch für eine erfolgreiche Quarantäne
                // ausgeführt, damit ein ausschließlich für die Prüfung angelegter
                // Basisordner anschließend wieder verschwindet.
                let cleanup = cleanup_probe_quarantine_root(
                    vfs.as_ref(),
                    root,
                    &base,
                    bound_base
                        .as_ref()
                        .and_then(|bound| bound.removal_proof.as_ref()),
                );
                let quarantine_result = combine_probe_outcome(
                    "Quarantäne",
                    quarantine_result,
                    cleanup,
                    &base.to_string_lossy(),
                );
                match quarantine_result {
                    Ok(()) => (true, false),
                    Err(quarantine_error) => {
                        blockers.push(Self::probe_blocker_or_error(
                            quarantine_error,
                            "FTPS-Quarantäne",
                        )?);
                        blockers.push(
                            "FTPS-Dauerlöschen ist gesperrt: DELE/RMD besitzt keine provideratomare Objektbedingung".into(),
                        );
                        (false, false)
                    }
                }
            }
            ProviderEndpointInner::WebDav(_, _) => {
                blockers.push(crate::webdav::STRICT_MUTATION_BLOCKER.into());
                (false, false)
            }
        };
        if !quarantine && !permanent && blockers.is_empty() {
            blockers.push("Für diesen Endpoint ist kein Remote-Löschvertrag verfügbar".into());
        }
        Ok(ProviderDeleteCapabilities {
            provider,
            quarantine,
            permanent,
            blockers,
        })
    }

    /// Prüft die Rename-Semantik der konkreten Sitzung wirkungsspezifisch und
    /// liefert die stärkste verfügbare Ausführungsklasse samt Einschränkungen.
    /// Ein Server ohne beweisbares No-Replace verliert dadurch nicht mehr die
    /// gesamte Umbenennung, sondern fällt sichtbar auf Best Effort zurück.
    pub fn probe_rename_capabilities(&self) -> io::Result<ProviderRenameCapabilities> {
        self.probe_rename_capabilities_inner(None)
    }

    /// Ermittelt die Garantie für den wirklichen Eintragstyp. Der Aufrufer
    /// übergibt den vorhandenen Namen; Typ und Fähigkeiten bestimmt der Core.
    pub fn probe_rename_capabilities_for_item(
        &self,
        path: &Path,
    ) -> io::Result<ProviderRenameCapabilities> {
        if self.kind() != ProviderKind::Ftps {
            return self.probe_rename_capabilities();
        }
        if path.parent() != Some(self.root()) {
            return Err(invalid(
                "Rename-Quelle liegt nicht unmittelbar im gewählten Pane-Root",
            ));
        }
        let entry = self.file_context()?.0.stat(path)?;
        if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "FTPS-Rename benötigt eine normale Datei oder ein echtes Verzeichnis",
            ));
        }
        self.probe_rename_capabilities_for_types(entry.is_file, entry.is_dir)
    }

    pub(crate) fn probe_rename_capabilities_for_types(
        &self,
        files: bool,
        directories: bool,
    ) -> io::Result<ProviderRenameCapabilities> {
        if !files && !directories {
            return Err(invalid(
                "Rename-Auswahl enthält keinen unterstützten Eintragstyp",
            ));
        }
        self.probe_rename_capabilities_inner(Some((files, directories)))
    }

    fn probe_rename_capabilities_inner(
        &self,
        types: Option<(bool, bool)>,
    ) -> io::Result<ProviderRenameCapabilities> {
        let provider = self.kind();
        let mut assured = false;
        let mut best_effort = false;
        let mut limitations = Vec::new();
        let mut blockers = Vec::new();
        match &self.inner {
            ProviderEndpointInner::Local(root)
            | ProviderEndpointInner::TestRemoteLocal(_, root) => {
                assured = LocalVfs::rooted(root)?.file_commit_class_at(root)?
                    == crate::vfs::FileCommitClass::Assured;
                if !assured {
                    blockers.push("Dieses lokale Volume unterstützt kein exklusives Rename; Kopieren bleibt als bestätigtes Best Effort verfügbar".into());
                }
            }
            ProviderEndpointInner::Sftp(vfs, _) => {
                // SFTP-v3-Rename ist eine interaktive Standardprimitive. Eine
                // mutierende Probe könnte ihre eigenen Objekte ohne bedingtes
                // Remove nicht sicher aufräumen; daher lesen wir nur die lebende,
                // beim Sitzungsaufbau gesetzte Best-Effort-Fähigkeit.
                let capabilities = vfs.capabilities();
                if capabilities.atomic_noreplace_rename {
                    assured = true;
                } else if capabilities.complete_listing && capabilities.best_effort_rename {
                    best_effort = true;
                    limitations.push(
                        "SFTP v3 garantiert beim gewöhnlichen Rename kein atomar kollisionsfreies Ziel; Doppeldecker prüft den vollständigen Bestand unmittelbar vorher und liest das Ergebnis zurück."
                            .into(),
                    );
                } else {
                    blockers.push(
                        "Die SFTP-Sitzung bietet weder No-Replace noch ein sichtbares Best-Effort-Rename"
                            .into(),
                    );
                }
            }
            ProviderEndpointInner::Ftps(vfs, root) => {
                let result = (|| -> io::Result<(bool, bool)> {
                    if let Some((files, directories)) = types {
                        let mut assured = true;
                        let mut plain = true;
                        for directory in [false, true] {
                            if (directory && directories) || (!directory && files) {
                                let observed = vfs.probe_rename_kind_at(root, directory)?;
                                assured &= observed.0;
                                plain &= observed.1;
                            }
                        }
                        Ok((assured, plain))
                    } else {
                        let capabilities = vfs.probe_rename_capability_at(root)?;
                        // Ohne ausgewählten Typ darf die Bilanz keinen einzelnen
                        // Typbeleg als allgemeine No-Replace-Garantie ausgeben.
                        Ok((
                            vfs.rename_assured_at(root, false)?
                                && vfs.rename_assured_at(root, true)?,
                            capabilities.best_effort_rename,
                        ))
                    }
                })();
                match result {
                    Ok((true, _)) => assured = true,
                    Ok((false, true)) => {
                        best_effort = true;
                        limitations.push("Ein atomar kollisionsfreies Rename ist für diese Auswahl nicht belegt. Der Bestand wird unmittelbar vorher geprüft; ein kleines Kollisionsfenster bis zur Ausführung bleibt.".into());
                    }
                    Ok(_) => {
                        blockers.push("Die Rename-Probe belegte kein gewöhnliches Rename".into())
                    }
                    Err(error) => {
                        blockers.push(Self::probe_blocker_or_error(error, "FTPS-Rename-Probe")?)
                    }
                }
            }
            ProviderEndpointInner::WebDav(vfs, root) => {
                match vfs.probe_rename_capability_at(root) {
                    Ok(_) => assured = true,
                    Err(error) => {
                        blockers.push(Self::probe_blocker_or_error(error, "WebDAV-Rename-Probe")?)
                    }
                }
            }
            ProviderEndpointInner::Smb(vfs, root) => match vfs.probe_rename_capability_at(root) {
                Ok(_) => assured = true,
                Err(error) => {
                    blockers.push(Self::probe_blocker_or_error(error, "SMB-Rename-Probe")?)
                }
            },
        }
        // Beide Klassen sichern nur das ZIEL. Die Quelle ist in beiden über ihren
        // Pfad gebunden, deshalb hängt diese Einschränkung an jeder verfügbaren
        // Klasse — sonst hieße „abgesichert" fälschlich „ohne jede Einschränkung".
        // Sie steht bewusst hinten: Oberflächen, die nur den ersten Eintrag zeigen,
        // zeigen weiterhin die klassenspezifische Einschränkung.
        if assured || best_effort {
            limitations.push(crate::remote_rename::SOURCE_PATH_BINDING_LIMITATION.to_string());
        }
        Ok(ProviderRenameCapabilities {
            provider,
            assured,
            best_effort,
            limitations,
            blockers,
        })
    }

    /// Gewöhnliche Umbenennung mit zuvor angezeigter Klasse. WebDAV verwendet
    /// seine eigene gebundene Einzelwirkung ohne künstliche Schreib-/Löschprobe.
    pub fn execute_ordinary_rename(
        &self,
        old_abs: &Path,
        new_name: &str,
        sandbox: &Option<PathBuf>,
        expected_class: Option<crate::RenameExecutionClass>,
    ) -> io::Result<RenameReport> {
        let name = crate::validate_ordinary_rename_request(
            self.kind(),
            self.root(),
            old_abs,
            new_name,
            expected_class,
        )?;
        match &self.inner {
            ProviderEndpointInner::WebDav(vfs, _) => vfs.execute_ordinary_rename(old_abs, &name),
            _ => self.execute_interactive_rename(old_abs, &name, sandbox),
        }
    }

    /// Führt eine interaktive Umbenennung in der stärksten frisch geprüften
    /// Ausführungsklasse aus: frisches Elternlisting, sichtbare Kollision, genau
    /// eine Mutation, Listing-Readback und nach Verbindungsverlust ausschließlich
    /// eine identitätsgeprüfte Zustandsklärung ohne Wiederholung der Mutation.
    pub fn execute_interactive_rename(
        &self,
        old_abs: &Path,
        new_name: &str,
        sandbox: &Option<PathBuf>,
    ) -> io::Result<RenameReport> {
        let new_name =
            crate::fileops::validate_interactive_rename_request(self.root(), old_abs, new_name)?;
        let capabilities = self.probe_rename_capabilities_for_item(old_abs)?;
        let class = capabilities.execution_class().ok_or_else(|| {
            crate::error::coded_error(
                io::ErrorKind::Unsupported,
                crate::error::ErrorCode::Unsupported,
                capabilities.blockers.join("; "),
            )
        })?;
        let (vfs, _) = self.file_context()?;
        let limitations = capabilities.limitations.clone();
        let report = match &self.inner {
            ProviderEndpointInner::Ftps(ftps, _) => {
                let reclaim = || ftps.reconnect();
                crate::remote_rename::rename_with_class(
                    vfs.as_ref(),
                    old_abs,
                    &new_name,
                    sandbox,
                    class,
                    limitations,
                    Some(&reclaim),
                )
            }
            ProviderEndpointInner::Sftp(sftp, _) => {
                let reclaim = || sftp.reconnect();
                crate::remote_rename::rename_with_class(
                    vfs.as_ref(),
                    old_abs,
                    &new_name,
                    sandbox,
                    class,
                    limitations,
                    Some(&reclaim),
                )
            }
            ProviderEndpointInner::WebDav(webdav, _) => {
                let reclaim = || webdav.reconnect();
                crate::remote_rename::rename_with_class(
                    vfs.as_ref(),
                    old_abs,
                    &new_name,
                    sandbox,
                    class,
                    limitations,
                    Some(&reclaim),
                )
            }
            ProviderEndpointInner::Smb(smb, _) => {
                let reclaim = || smb.reconnect();
                crate::remote_rename::rename_with_class(
                    vfs.as_ref(),
                    old_abs,
                    &new_name,
                    sandbox,
                    class,
                    limitations,
                    Some(&reclaim),
                )
            }
            ProviderEndpointInner::Local(_) | ProviderEndpointInner::TestRemoteLocal(_, _) => {
                crate::remote_rename::rename_with_class(
                    vfs.as_ref(),
                    old_abs,
                    &new_name,
                    sandbox,
                    class,
                    limitations,
                    None,
                )
            }
        }?;
        self.finish_ftps_intents()?;
        Ok(report)
    }

    /// Liefert eine gehaltene Lesesicht samt dauerhafter Paarbindung. Beide Seiten
    /// eines bidirektionalen Paars sind spätere Ziele; gefährliche Root-Wurzeln werden
    /// deshalb bereits in der reinen Planstufe wie Realmodus-Ziele abgelehnt.
    pub(crate) fn bidirectional_snapshot_context(
        &self,
    ) -> io::Result<(Arc<dyn Vfs>, PathBuf, crate::EndpointBinding)> {
        let root = self.root().to_path_buf();
        let (vfs, account_digest): (Arc<dyn Vfs>, Option<[u8; 32]>) = match &self.inner {
            ProviderEndpointInner::Local(root)
            | ProviderEndpointInner::TestRemoteLocal(_, root) => {
                validate_real_target(root)?;
                let parent = root
                    .parent()
                    .ok_or_else(|| invalid("Lokaler Endpunkt besitzt keinen Elternordner"))?;
                (Arc::new(LocalVfs::rooted(parent)?), None)
            }
            ProviderEndpointInner::Sftp(vfs, root) => {
                validate_remote_target(root)?;
                (vfs.clone(), Some(vfs.account_digest()))
            }
            ProviderEndpointInner::Ftps(vfs, root) => {
                validate_remote_target(root)?;
                (vfs.clone(), Some(vfs.account_digest()))
            }
            ProviderEndpointInner::WebDav(vfs, root) => {
                validate_remote_target(root)?;
                (vfs.clone(), Some(vfs.identity().account_digest))
            }
            ProviderEndpointInner::Smb(vfs, root) => {
                validate_remote_target(root)?;
                (vfs.clone(), Some(vfs.identity().account_digest))
            }
        };
        let identity = vfs.root_identity(&root)?;
        let binding = match account_digest {
            Some(account) => crate::EndpointBinding::remote(&identity, account)?,
            None => crate::EndpointBinding::local(&identity)?,
        };
        Ok((vfs, root, binding))
    }

    fn materialize(self, target: bool) -> io::Result<MaterializedEndpoint> {
        match self.inner {
            ProviderEndpointInner::Local(root)
            | ProviderEndpointInner::TestRemoteLocal(_, root) => {
                let parent = root
                    .parent()
                    .ok_or_else(|| invalid("Lokaler Endpunkt besitzt keinen Elternordner"))?
                    .to_path_buf();
                if target {
                    validate_real_target(&root)?;
                }
                let anchor = if target { &parent } else { &root };
                Ok(MaterializedEndpoint {
                    vfs: RunVfs::Local(LocalVfs::rooted(anchor)?),
                    root,
                    target_parent: parent,
                })
            }
            ProviderEndpointInner::Sftp(vfs, root) => {
                materialize_remote(RunVfs::Sftp(vfs), root, target)
            }
            ProviderEndpointInner::Ftps(vfs, root) => {
                materialize_remote(RunVfs::Ftps(vfs), root, target)
            }
            ProviderEndpointInner::WebDav(vfs, root) => {
                materialize_remote(RunVfs::WebDav(vfs), root, target)
            }
            ProviderEndpointInner::Smb(vfs, root) => {
                materialize_remote(RunVfs::Smb(vfs), root, target)
            }
        }
    }
}

struct MaterializedEndpoint {
    vfs: RunVfs,
    root: PathBuf,
    target_parent: PathBuf,
}

fn materialize_remote(
    vfs: RunVfs,
    root: PathBuf,
    target: bool,
) -> io::Result<MaterializedEndpoint> {
    if target {
        validate_remote_target(&root)?;
    }
    let parent = match root.parent() {
        Some(parent) => parent.to_path_buf(),
        None if !target => root.clone(),
        None => return Err(invalid("Remote-Ziel besitzt keinen Eltern-Namespace")),
    };
    Ok(MaterializedEndpoint {
        vfs,
        root,
        target_parent: parent,
    })
}

/// Zur Laufzeit geprüfte Fähigkeiten genau einer Zielrichtung.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProviderTransferCapabilities {
    pub target: ProviderKind,
    pub add_only: bool,
    pub update: bool,
    pub mirror: bool,
    pub available_space_known: bool,
    pub quarantine_verified: bool,
    pub durable_directory_flush: bool,
}

impl ProviderTransferCapabilities {
    fn local() -> Self {
        Self {
            target: ProviderKind::Local,
            add_only: true,
            update: true,
            mirror: true,
            available_space_known: true,
            quarantine_verified: true,
            durable_directory_flush: true,
        }
    }

    fn remote(target: ProviderKind, value: crate::RemoteCapabilities) -> Self {
        Self {
            target,
            add_only: value.permits_add_only(),
            update: value.permits_update(),
            mirror: value.permits_mirror(),
            available_space_known: value.available_space,
            quarantine_verified: value.quarantine,
            durable_directory_flush: value.durable_directory_flush,
        }
    }

    fn permits(self, mode: SyncMode) -> bool {
        match mode {
            SyncMode::AddOnly => self.add_only,
            SyncMode::Update => self.update,
            SyncMode::Mirror => self.mirror,
        }
    }
}

pub struct PreparedSftpRealRun {
    source_vfs: RunVfs,
    target_vfs: RunVfs,
    source_root: PathBuf,
    target_root: PathBuf,
    quarantine_root: PathBuf,
    quarantine_run: PathBuf,
    preview: RealRunPreview,
    prepared_at: SystemTime,
    expires_at: SystemTime,
    capabilities: ProviderTransferCapabilities,
}

pub struct SftpRealRun {
    source_vfs: RunVfs,
    target_vfs: RunVfs,
    source_root: PathBuf,
    target_root: PathBuf,
    plan: SyncPlan,
    safety: Safety,
    audit: RealAuditContext,
}

pub type PreparedProviderRealRun = PreparedSftpRealRun;
pub type ProviderRealRun = SftpRealRun;

/// Bereitet eine beliebige Provider→Provider-Richtung mit demselben Permit- und
/// Quarantänevertrag vor. Kein Endpunkt und keine Sitzung wird serialisiert.
pub fn prepare_provider_transfer(
    source: ProviderEndpoint,
    target: ProviderEndpoint,
    options: SyncOptions,
) -> io::Result<PreparedProviderRealRun> {
    let source = source.materialize(false)?;
    let target = target.materialize(true)?;
    prepare_remote_run(
        source.vfs,
        source.root,
        target.vfs,
        target.root,
        target.target_parent,
        options,
    )
}

/// Providerneutrale Variante für bidirektionale Gewinnerprojektionen. Der echte
/// Zielendpunkt wird erst hier materialisiert, damit dessen konkrete Remote-Proben,
/// Quarantäne und Sitzungsgeneration im normalen Realmodusvertrag bleiben.
pub(crate) fn prepare_projected_provider_transfer(
    source: Arc<dyn Vfs>,
    projection_root: PathBuf,
    target: ProviderEndpoint,
    options: SyncOptions,
) -> io::Result<PreparedProviderRealRun> {
    let target = target.materialize(true)?;
    prepare_remote_run(
        RunVfs::Projected(source),
        projection_root,
        target.vfs,
        target.root,
        target.target_parent,
        options,
    )
}

/// Upload von einem gehaltenen lokalen Quellroot in einen virtuellen SFTP-Zielroot.
/// Die SFTP-Verbindung muss an dessen sicheren Eltern-Namespace gebunden sein.
pub fn prepare_sftp_upload(
    local_source: &Path,
    sftp: Arc<SftpVfs>,
    remote_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::local(local_source)?,
        ProviderEndpoint::sftp(sftp, remote_target)?,
        options,
    )
}

/// Download von einem virtuellen SFTP-Quellroot in einen gehaltenen lokalen Zielroot.
pub fn prepare_sftp_download(
    sftp: Arc<SftpVfs>,
    remote_source: &Path,
    local_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::sftp(sftp, remote_source)?,
        ProviderEndpoint::local(local_target)?,
        options,
    )
}

/// Upload in einen expliziten FTPS-Root. Server ohne erfolgreich geprüfte
/// STOU-/No-Replace-Semantik bleiben bereits in der Vorbereitung schreibgeschützt.
pub fn prepare_ftps_upload(
    local_source: &Path,
    ftps: Arc<FtpsVfs>,
    remote_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::local(local_source)?,
        ProviderEndpoint::ftps(ftps, remote_target)?,
        options,
    )
}

/// Download aus einem gebundenen FTPS-Root. Alle Zielmutationen laufen lokal über
/// denselben Quarantäne-, Permit- und Auditvertrag wie SFTP-Downloads.
pub fn prepare_ftps_download(
    ftps: Arc<FtpsVfs>,
    remote_source: &Path,
    local_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::ftps(ftps, remote_source)?,
        ProviderEndpoint::local(local_target)?,
        options,
    )
}

/// Upload in eine WebDAV-Collection. Das Ziel bleibt bis zum erfolgreichen
/// If-None-Match-/ETag-/MOVE-Gegenlauf auf AddOnly begrenzt.
pub fn prepare_webdav_upload(
    local_source: &Path,
    webdav: Arc<WebDavVfs>,
    remote_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::local(local_source)?,
        ProviderEndpoint::webdav(webdav, remote_target)?,
        options,
    )
}

/// Download aus einer gebundenen WebDAV-Collection in einen lokalen Root.
pub fn prepare_webdav_download(
    webdav: Arc<WebDavVfs>,
    remote_source: &Path,
    local_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::webdav(webdav, remote_source)?,
        ProviderEndpoint::local(local_target)?,
        options,
    )
}

/// Upload in eine verschlüsselte, identitätsgebundene SMB3-Freigabe.
pub fn prepare_smb_upload(
    local_source: &Path,
    smb: Arc<SmbVfs>,
    remote_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::local(local_source)?,
        ProviderEndpoint::smb(smb, remote_target)?,
        options,
    )
}

/// Download aus einer verschlüsselten SMB3-Freigabe in einen lokalen Root.
pub fn prepare_smb_download(
    smb: Arc<SmbVfs>,
    remote_source: &Path,
    local_target: &Path,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    prepare_provider_transfer(
        ProviderEndpoint::smb(smb, remote_source)?,
        ProviderEndpoint::local(local_target)?,
        options,
    )
}

fn bind_nonoverlapping_roots_before_probe<T>(
    source_vfs: &RunVfs,
    source_root: &Path,
    target_vfs: &RunVfs,
    target_root: &Path,
    probe: impl FnOnce() -> io::Result<T>,
) -> io::Result<(crate::RootIdentity, crate::RootIdentity, T)> {
    // Der rote Gegenbeleg bindet diese Reihenfolge: Eine Capability-Probe kann
    // mutieren und darf daher erst nach der vollständig geprüften Root-Beziehung
    // laufen.
    let source_identity = source_vfs.as_vfs().root_identity(source_root)?;
    let target_identity = target_vfs.as_vfs().root_identity(target_root)?;
    if !source_vfs.is_projected() {
        reject_overlapping_roots(&source_identity, source_root, &target_identity, target_root)?;
    }
    let probe_result = probe()?;
    Ok((source_identity, target_identity, probe_result))
}

fn probe_target_add_only_for_mode<T>(
    mode: SyncMode,
    uses_local_restore_probe: bool,
    target_vfs: &dyn Vfs,
    probe: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    if mode != SyncMode::AddOnly
        && !uses_local_restore_probe
        && !target_vfs.supports_bound_restore_probe()
    {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Remote-Update und -Mirror benötigen proofgebundene Datei- und Ordneranlage, bedingtes Rename sowie bedingte Entfernung für die Restore-Probe; dieser Provider implementiert den vollständigen Vertrag nicht",
        ));
    }
    // Erst nach dem vollständigen Supportgate darf eine Capability-Probe eigene
    // Namen anlegen. Ein Provider ohne bedingten Cleanup erhält so gar keine
    // Gelegenheit, bei einem Fehler Probeobjekte zurückzulassen.
    probe()
}

fn prepare_remote_run(
    source_vfs: RunVfs,
    source_root: PathBuf,
    target_vfs: RunVfs,
    target_root: PathBuf,
    target_parent: PathBuf,
    options: SyncOptions,
) -> io::Result<PreparedSftpRealRun> {
    // Optionssyntax ist reine Eingabevalidierung und muss vor jedem Listing sowie
    // erst recht vor mutierenden Capability-Proben scheitern können.
    options.validate()?;
    if source_vfs.is_webdav() && options.compare != Compare::ContentHash {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WebDAV-Quellen verlangen ContentHash zusätzlich zur ETag-Bindung",
        ));
    }
    ensure_directory(source_vfs.as_vfs(), &source_root, "Quelle")?;
    ensure_directory(target_vfs.as_vfs(), &target_root, "Ziel")?;

    let (source_identity, target_identity, mut remote_capabilities) =
        bind_nonoverlapping_roots_before_probe(
            &source_vfs,
            &source_root,
            &target_vfs,
            &target_root,
            || {
                probe_target_add_only_for_mode(
                    options.mode,
                    target_vfs.uses_local_restore_probe(),
                    target_vfs.as_vfs(),
                    || target_vfs.probe_target_add_only(&target_root),
                )
            },
        )?;
    let target_generation = if let Some(capabilities) = remote_capabilities {
        if !capabilities.permits_add_only() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Remote-Ziel erfüllt den geprüften AddOnly-Vertrag nicht",
            ));
        }
        target_vfs.remote_generation()
    } else {
        None
    };

    let source_listing =
        build_listing_proof(source_vfs.as_vfs(), &source_root, source_identity.clone());
    let target_listing =
        build_listing_proof(target_vfs.as_vfs(), &target_root, target_identity.clone());
    require_complete("Quelle", &source_listing)?;
    require_complete("Ziel", &target_listing)?;

    let plan = sync::plan(
        source_vfs.as_vfs(),
        &source_root,
        target_vfs.as_vfs(),
        &target_root,
        &options,
    )?;
    // Eine bidirektionale Dateiersetzung nutzt absichtlich die Mirror-Plansemantik,
    // damit eine explizite Gewinnerentscheidung nicht wieder von mtime abhängt. Wenn
    // dieser konkrete Plan nichts entfernt, benötigt das Ziel dennoch nur den
    // geprüften Update-Vertrag. Der normale Mirror mit Entfernungen bleibt unverändert
    // an starke Verzeichnis-Dauerhaftigkeit gebunden.
    let required_target_mode = if options.mode == SyncMode::Mirror && plan.delete_count == 0 {
        SyncMode::Update
    } else {
        options.mode
    };
    let mut validation_options = options.clone();
    validation_options.mode = required_target_mode;
    validate_real_plan(&plan, &validation_options)?;
    if required_target_mode == SyncMode::Mirror
        && remote_capabilities.is_some_and(|value| !value.durable_directory_flush)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Remote-Mirror bleibt ohne starke Verzeichnis-Dauerhaftigkeit gesperrt",
        ));
    }

    if options.mode != SyncMode::AddOnly {
        // SFTP/SMB entfernen die künstliche Restore-Datei über ihr eigenes,
        // sitzungsgebundenes Lösch-Gate. Der vorausgehende Upload-Beweis darf diese
        // unabhängige Wirkung nicht mehr nebenbei freischalten.
        target_vfs.probe_target_entry_delete(&target_root)?;
    }
    let (quarantine_root, quarantine_run, quarantine_identity, restore_probe_ok) =
        if options.mode == SyncMode::AddOnly && remote_capabilities.is_some() {
            // AddOnly verändert nie einen bestehenden Zieleintrag. Ein Provider ohne
            // Löschoperation braucht dafür weder Quarantäneordner noch Restore-Probe.
            // Der Zielroot selbst bindet als unveränderter Permit-Anker den Dummy-Lauf.
            (
                target_root.clone(),
                reserve_quarantine_run_name(target_vfs.as_vfs(), &target_root)?,
                target_identity.clone(),
                false,
            )
        } else {
            let quarantine_root = target_parent.join(".doppeldecker-quarantine");
            if target_vfs.uses_local_restore_probe() {
                // Der lokale Realmodus besitzt seinen eigenen descriptor-relativen
                // Restorevertrag. Die hier eingeführte Providergrenze gilt für
                // Remote-Artefakte, deren Protokollnamen parallel austauschbar sind.
                ensure_quarantine_root(target_vfs.as_vfs(), &target_parent, &quarantine_root)?;
                run_local_restore_probe(target_vfs.as_vfs(), &target_root, &quarantine_root)?;
            } else {
                let bound_quarantine = ensure_probe_quarantine_root(
                    target_vfs.as_vfs(),
                    &target_parent,
                    &quarantine_root,
                )?;
                run_bound_restore_probe(target_vfs.as_vfs(), &target_root, &bound_quarantine)?;
            }
            let quarantine_run =
                reserve_quarantine_run_name(target_vfs.as_vfs(), &quarantine_root)?;
            let quarantine_identity = target_vfs.as_vfs().root_identity(&quarantine_root)?;
            ensure_same_mount(&target_identity, &quarantine_identity)?;
            (quarantine_root, quarantine_run, quarantine_identity, true)
        };
    if options.mode != SyncMode::AddOnly {
        if let (Some(sftp), Some(generation)) = (target_vfs.sftp(), target_generation) {
            let capabilities = sftp.mark_quarantine_verified(generation)?;
            remote_capabilities = Some(capabilities);
        }
        if let (Some(smb), Some(generation)) = (target_vfs.smb(), target_generation) {
            let capabilities = smb.mark_quarantine_verified(generation)?;
            remote_capabilities = Some(capabilities);
        }
        if let (Some(ftps), Some(generation)) = (target_vfs.ftps(), target_generation) {
            let capabilities = ftps.mark_quarantine_verified(generation)?;
            remote_capabilities = Some(capabilities);
        }
        if let (Some(webdav), Some(generation)) = (target_vfs.webdav(), target_generation) {
            let capabilities = webdav.mark_quarantine_verified(generation)?;
            remote_capabilities = Some(capabilities);
        }
    }
    let capabilities = remote_capabilities
        .map_or_else(ProviderTransferCapabilities::local, |value| {
            ProviderTransferCapabilities::remote(target_vfs.kind(), value)
        });
    if !capabilities.permits(required_target_mode) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Zielrichtung {:?} erfüllt den geprüften {:?}-Vertrag nicht",
                capabilities.target, required_target_mode
            ),
        ));
    }
    let mut impact = impact_for_plan(&plan);
    match target_vfs.as_vfs().available_bytes(&target_root) {
        Ok(available) => {
            impact.available_bytes = available;
            impact.available_bytes_known = true;
        }
        Err(error)
            if remote_capabilities.is_some()
                && options.mode == SyncMode::AddOnly
                && !capabilities.available_space_known
                && error.kind() == io::ErrorKind::Unsupported => {}
        Err(error) => return Err(error),
    }
    if impact.available_bytes_known && impact.available_bytes < impact.required_available_bytes {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!(
                "Zu wenig freier Platz: {} Bytes benötigt, {} Bytes verfügbar",
                impact.required_available_bytes, impact.available_bytes
            ),
        ));
    }

    let prepared_at = SystemTime::now();
    let expires_at = prepared_at + REAL_PERMIT_LIFETIME;
    let base_plan_digest = binding_digest(
        &plan,
        &options,
        &source_identity,
        &target_identity,
        &quarantine_identity,
        &source_listing,
        &target_listing,
    )?;
    let target_capabilities_digest = target_vfs.as_vfs().safety_capabilities_digest()?;
    let plan_digest = bind_capabilities_digest(base_plan_digest, target_capabilities_digest);
    let confirmation_phrase = confirmation_phrase(&target_root)?;
    let preview = RealRunPreview {
        source_identity,
        target_identity,
        quarantine_identity,
        source_listing,
        target_listing,
        plan,
        options: options.clone(),
        impact,
        plan_digest,
        confirmation_phrase,
        prepared_at_unix_ms: unix_millis(prepared_at)?,
        expires_at_unix_ms: unix_millis(expires_at)?,
        restore_probe_ok,
        backup_confirmation_required: options.mode != SyncMode::AddOnly,
        provider_capabilities: Some(capabilities),
        local_execution: None,
    };
    Ok(PreparedSftpRealRun {
        source_vfs,
        target_vfs,
        source_root,
        target_root,
        quarantine_root,
        quarantine_run,
        preview,
        prepared_at,
        expires_at,
        capabilities,
    })
}

impl PreparedSftpRealRun {
    pub fn preview(&self) -> &RealRunPreview {
        &self.preview
    }

    pub fn capabilities(&self) -> ProviderTransferCapabilities {
        self.capabilities
    }

    pub fn confirm(self, phrase: &str, backup_confirmed: bool) -> io::Result<SftpRealRun> {
        if phrase != self.preview.confirmation_phrase {
            return Err(confirmation_mismatch(
                "Remote-Realmodus-Bestätigung stimmt nicht exakt überein",
            ));
        }
        if self.preview.backup_confirmation_required && !backup_confirmed {
            return Err(confirmation_mismatch(
                "Remote-Update/Mirror benötigt ein bestätigtes unabhängiges Backup",
            ));
        }
        let now = SystemTime::now();
        if now > self.expires_at {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Remote-Realmodus-Vorprüfung ist älter als zehn Minuten",
            ));
        }
        if !self.source_vfs.as_vfs().anchor_is_current()?
            || !self.target_vfs.as_vfs().anchor_is_current()?
        {
            return Err(plan_drift(
                "Lokale Wurzel änderte sich seit der Remote-Vorschau",
            ));
        }
        if self.target_vfs.current_transfer_capabilities() != self.capabilities {
            return Err(plan_drift(
                "Geprüfte Ziel-Fähigkeiten änderten sich seit der Vorschau",
            ));
        }
        let source_identity = self.source_vfs.as_vfs().root_identity(&self.source_root)?;
        let target_identity = self.target_vfs.as_vfs().root_identity(&self.target_root)?;
        let quarantine_identity = self
            .target_vfs
            .as_vfs()
            .root_identity(&self.quarantine_root)?;
        if source_identity != self.preview.source_identity
            || target_identity != self.preview.target_identity
            || quarantine_identity != self.preview.quarantine_identity
        {
            return Err(plan_drift(
                "Provider-, Root- oder Sitzungsidentität änderte sich seit der Vorschau",
            ));
        }
        let source_listing = build_listing_proof(
            self.source_vfs.as_vfs(),
            &self.source_root,
            source_identity.clone(),
        );
        let target_listing = build_listing_proof(
            self.target_vfs.as_vfs(),
            &self.target_root,
            target_identity.clone(),
        );
        require_complete("Quelle", &source_listing)?;
        require_complete("Ziel", &target_listing)?;
        if source_listing != self.preview.source_listing
            || target_listing != self.preview.target_listing
        {
            return Err(plan_drift(
                "Quelle oder Ziel änderte sich seit dem vollständigen Remote-Listing",
            ));
        }
        let fresh_plan = sync::plan(
            self.source_vfs.as_vfs(),
            &self.source_root,
            self.target_vfs.as_vfs(),
            &self.target_root,
            &self.preview.options,
        )?;
        if fresh_plan != self.preview.plan {
            return Err(plan_drift("Remote-Plan änderte sich seit der Vorschau"));
        }
        let insufficient_space = if self.preview.impact.available_bytes_known {
            self.target_vfs
                .as_vfs()
                .available_bytes(&self.target_root)?
                < self.preview.impact.required_available_bytes
        } else {
            false
        };
        if insufficient_space {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "Freier Platz reicht vor der Remote-Permit-Ausgabe nicht mehr aus",
            ));
        }

        let permit = create_real_permit(RealPermitRequest {
            plan_digest: self.preview.plan_digest,
            target_capabilities_digest: self.target_vfs.as_vfs().safety_capabilities_digest()?,
            source_identity,
            target_identity,
            quarantine_identity,
            source_listing,
            target_listing,
            options: self.preview.options.clone(),
            source_root: self.source_root.clone(),
            target_root: self.target_root.clone(),
            quarantine_root: self.quarantine_root.clone(),
            issued_at: now.max(self.prepared_at),
            expires_at: self.expires_at,
        })?;
        let audit = create_real_audit_context(
            self.preview.plan_digest,
            &self.preview.source_identity,
            &self.preview.target_identity,
            self.preview.impact.clone(),
            &self.quarantine_run,
            self.preview.restore_probe_ok,
        )?;
        let plan = self.preview.plan;
        let safety = Safety::real(permit, self.quarantine_run);
        Ok(SftpRealRun {
            source_vfs: self.source_vfs,
            target_vfs: self.target_vfs,
            source_root: self.source_root,
            target_root: self.target_root,
            plan,
            safety,
            audit,
        })
    }
}

impl SftpRealRun {
    pub fn plan(&self) -> &SyncPlan {
        &self.plan
    }

    pub fn execute(
        self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<SyncStats> {
        let result = sync::execute(
            sync::ExecuteRequest {
                src: self.source_vfs.as_vfs(),
                src_root: &self.source_root,
                dst: self.target_vfs.as_vfs(),
                dst_root: &self.target_root,
                plan: &self.plan,
                safety: &self.safety,
            },
            cancel,
            on_progress,
        );
        match self.target_vfs.ftps() {
            Some(ftps) => ftps.finish_sync_result(result, |stats| stats),
            None => result,
        }
    }

    pub fn execute_with_audit(
        self,
        audit_path: &Path,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<SyncStats> {
        let result = execute_real_with_audit(
            RealAuditRun {
                source_vfs: self.source_vfs.as_vfs(),
                source_root: &self.source_root,
                target_vfs: self.target_vfs.as_vfs(),
                target_root: &self.target_root,
                plan: &self.plan,
                safety: &self.safety,
                audit: &self.audit,
                audit_path,
            },
            cancel,
            on_progress,
        );
        match self.target_vfs.ftps() {
            Some(ftps) => ftps.finish_sync_result(result, |stats| stats),
            None => result,
        }
    }

    pub(crate) fn execute_with_audit_detailed(
        self,
        audit_path: &Path,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<RealAuditExecutionSuccess> {
        let result = execute_real_with_audit_detailed(
            RealAuditRun {
                source_vfs: self.source_vfs.as_vfs(),
                source_root: &self.source_root,
                target_vfs: self.target_vfs.as_vfs(),
                target_root: &self.target_root,
                plan: &self.plan,
                safety: &self.safety,
                audit: &self.audit,
                audit_path,
            },
            cancel,
            on_progress,
        );
        match self.target_vfs.ftps() {
            Some(ftps) => ftps.finish_sync_result(result, |success| success.stats()),
            None => result,
        }
    }
}

fn ensure_directory(vfs: &dyn Vfs, root: &Path, role: &str) -> io::Result<()> {
    let entry = vfs.stat(root)?;
    if entry.is_dir && !entry.is_symlink {
        Ok(())
    } else {
        Err(invalid(format!("{role} ist kein echtes Verzeichnis")))
    }
}

pub(crate) fn reject_overlapping_roots(
    source_identity: &crate::RootIdentity,
    source_root: &Path,
    target_identity: &crate::RootIdentity,
    target_root: &Path,
) -> io::Result<()> {
    let (same_namespace, source_path, target_path) =
        root_namespace_relation(source_identity, source_root, target_identity, target_root);
    reject_related_paths(same_namespace, &source_path, &target_path)
}

fn root_namespace_relation(
    source_identity: &crate::RootIdentity,
    source_root: &Path,
    target_identity: &crate::RootIdentity,
    target_root: &Path,
) -> (bool, PathBuf, PathBuf) {
    fn canonical_remote_path(canonical_root: &str, virtual_root: &str) -> PathBuf {
        let mut full = PathBuf::from(canonical_root);
        for component in Path::new(virtual_root).components() {
            if let std::path::Component::Normal(name) = component {
                full.push(name);
            }
        }
        full
    }

    match (source_identity, target_identity) {
        (crate::RootIdentity::Local(_), crate::RootIdentity::Local(_)) => (
            // Alle lokalen Pfade teilen denselben Host-Namespace. Eine Mountgrenze
            // ändert die Rename-Fähigkeit, aber nicht die lexikalische Beziehung:
            // `/daten` enthält weiterhin `/daten/eingehangen`.
            true,
            source_root.to_path_buf(),
            target_root.to_path_buf(),
        ),
        (crate::RootIdentity::Sftp(source), crate::RootIdentity::Sftp(target)) => {
            let same_server = source.server.host == target.server.host
                && source.server.port == target.server.port
                && source.server.host_key == target.server.host_key
                && source.server.sftp_version == target.server.sftp_version
                && source.server.extensions_digest == target.server.extensions_digest;
            (
                same_server,
                canonical_remote_path(&source.server.canonical_root, &source.virtual_root),
                canonical_remote_path(&target.server.canonical_root, &target.virtual_root),
            )
        }
        (crate::RootIdentity::Ftps(source), crate::RootIdentity::Ftps(target)) => {
            let same_server = source.server.host == target.server.host
                && source.server.port == target.server.port
                && source.server.transport_security == target.server.transport_security
                && source.server.certificate == target.server.certificate
                && source.server.features_digest == target.server.features_digest
                && source.server.welcome_digest == target.server.welcome_digest;
            (
                same_server,
                canonical_remote_path(&source.server.canonical_root, &source.virtual_root),
                canonical_remote_path(&target.server.canonical_root, &target.virtual_root),
            )
        }
        (crate::RootIdentity::WebDav(source), crate::RootIdentity::WebDav(target)) => {
            let same_server = source.server.origin == target.server.origin
                && source.server.certificate == target.server.certificate
                && source.server.account_digest == target.server.account_digest
                && source.server.dav_header_digest == target.server.dav_header_digest
                && source.server.server_header_digest == target.server.server_header_digest;
            (
                same_server,
                canonical_remote_path(&source.server.canonical_root, &source.virtual_root),
                canonical_remote_path(&target.server.canonical_root, &target.virtual_root),
            )
        }
        (crate::RootIdentity::Smb(source), crate::RootIdentity::Smb(target)) => {
            let same_server = source.server.host == target.server.host
                && source.server.port == target.server.port
                && source.server.server_guid == target.server.server_guid
                && source.server.share == target.server.share
                && source.server.account_digest == target.server.account_digest
                && source.server.dialect == target.server.dialect
                && source.server.signing_active == target.server.signing_active
                && source.server.encryption_active == target.server.encryption_active
                && source.server.signing_algorithm == target.server.signing_algorithm
                && source.server.encryption_cipher == target.server.encryption_cipher;
            (
                same_server,
                canonical_remote_path(&source.server.canonical_root, &source.virtual_root),
                canonical_remote_path(&target.server.canonical_root, &target.virtual_root),
            )
        }
        _ => (false, source_root.to_path_buf(), target_root.to_path_buf()),
    }
}

fn reject_related_paths(
    same_namespace: bool,
    source_path: &Path,
    target_path: &Path,
) -> io::Result<()> {
    if same_namespace
        && (source_path.starts_with(target_path) || target_path.starts_with(source_path))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Provider-Quelle und -Ziel sind gleich oder ineinander verschachtelt",
        ));
    }
    Ok(())
}

fn validate_remote_target(path: &Path) -> io::Result<()> {
    validate_remote_root(path, "Remote-Ziel")?;
    if path == Path::new("/") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Gebundener Remote-Serverroot darf nicht selbst Ziel sein",
        ));
    }
    Ok(())
}

fn validate_remote_root(path: &Path, role: &str) -> io::Result<()> {
    if !path.is_absolute() || path.components().any(|part| part.as_os_str() == "..") {
        return Err(invalid(format!("{role} ist kein sicherer absoluter Pfad")));
    }
    Ok(())
}

fn require_complete(role: &str, proof: &crate::real_mode::ListingProof) -> io::Result<()> {
    if proof.complete {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{role}-Listing ist unvollständig"),
        ))
    }
}

fn unix_millis(time: SystemTime) -> io::Result<u128> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(io::Error::other)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{CompleteDirectoryListing, MutationFailure, ProofListEntry};
    use crate::{Entry, RemoteCapabilities};
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn ftps_wirkungsgrenze_laesst_regulaere_dateivorbereitung_verfuegbar() {
        for kind in [crate::FileQueueKind::Copy, crate::FileQueueKind::Move] {
            for merge in [false, true] {
                let area = tempfile::tempdir().unwrap();
                let root = std::fs::canonicalize(area.path()).unwrap();
                let source_root = root.join("source");
                let target_root = root.join("target");
                std::fs::create_dir_all(source_root.join("tree")).unwrap();
                std::fs::create_dir(&target_root).unwrap();
                std::fs::write(source_root.join("tree/a.txt"), b"source a").unwrap();
                std::fs::write(source_root.join("tree/b.txt"), b"source b").unwrap();
                if merge {
                    std::fs::create_dir(target_root.join("tree")).unwrap();
                    std::fs::write(target_root.join("tree/a.txt"), b"old target").unwrap();
                }
                let source = ProviderEndpoint::local(&source_root).unwrap();
                let target =
                    ProviderEndpoint::test_remote_local(ProviderKind::Ftps, &target_root).unwrap();
                let items = [source_root.join("tree")];
                // Dieselbe Produktionsgrenze wird hier mit kleinem tatsächlichem
                // Baum erreicht. Transportgarantien prüft der echte FTP-Gegenserver;
                // dieser Fall prüft ausschließlich den unveränderten Core-Routenweg.
                let result = source.probe_interactive_file_target_with_effect_limit(
                    ProviderFileTargetProbe {
                        target: &target,
                        items: &items,
                        kind,
                        overwrite: false,
                        skip_conflicts: true,
                        inspect_conflicts: false,
                        expected_conflicts: None,
                        shared_session: false,
                        targets_missing: !merge,
                        capabilities_override: None,
                    },
                    2,
                );
                assert!(result.is_ok(), "Die begrenzte Optimierung darf den regulären Dateiweg nicht sperren: {kind:?}, merge={merge}: {:?}", result.err());
                let prepared = result.unwrap();
                assert!(!prepared.skip_only && !prepared.rename_only);
                assert_eq!(
                    prepared.source_removal_required,
                    kind == crate::FileQueueKind::Move
                );
                assert!(prepared.capabilities.unwrap().copy_new);
                assert_eq!(
                    std::fs::read(source_root.join("tree/a.txt")).unwrap(),
                    b"source a"
                );
                assert_eq!(
                    std::fs::read(source_root.join("tree/b.txt")).unwrap(),
                    b"source b"
                );
                if merge {
                    assert_eq!(
                        std::fs::read(target_root.join("tree/a.txt")).unwrap(),
                        b"old target"
                    );
                }
                assert!(!target_root.join("tree/b.txt").exists());
            }
        }
    }

    struct RestoreProbeRaceVfs {
        inner: LocalVfs,
        collision_on_call: AtomicUsize,
        unclear_on_call: AtomicUsize,
        collision_path: Mutex<Option<PathBuf>>,
        namespace: Mutex<()>,
        conditional_renames: AtomicUsize,
        conditional_removes: AtomicUsize,
        conditional_replaces: AtomicUsize,
        sync_calls: AtomicUsize,
        fail_sync_on_call: AtomicUsize,
        attacked_path: Mutex<Option<PathBuf>>,
        displaced_path: Mutex<Option<PathBuf>>,
        restore_target_root_attack: Mutex<Option<PathBuf>>,
        restore_target_root_displaced: Mutex<Option<PathBuf>>,
    }

    impl RestoreProbeRaceVfs {
        fn new() -> Self {
            Self {
                inner: LocalVfs::new(),
                collision_on_call: AtomicUsize::new(usize::MAX),
                unclear_on_call: AtomicUsize::new(usize::MAX),
                collision_path: Mutex::new(None),
                namespace: Mutex::new(()),
                conditional_renames: AtomicUsize::new(0),
                conditional_removes: AtomicUsize::new(0),
                conditional_replaces: AtomicUsize::new(0),
                sync_calls: AtomicUsize::new(0),
                fail_sync_on_call: AtomicUsize::new(0),
                attacked_path: Mutex::new(None),
                displaced_path: Mutex::new(None),
                restore_target_root_attack: Mutex::new(None),
                restore_target_root_displaced: Mutex::new(None),
            }
        }

        fn fail_sync_on_call(&self, call: usize) {
            self.fail_sync_on_call.store(call, Ordering::SeqCst);
        }

        fn attack_target_root_before_restore(&self, target: &Path) {
            *self.restore_target_root_attack.lock().unwrap() = Some(target.to_path_buf());
        }

        fn swap_target_root_before_restore(&self, to: &Path) {
            if self.conditional_renames.load(Ordering::SeqCst) != 2 {
                return;
            }
            let Some(target) = self.restore_target_root_attack.lock().unwrap().take() else {
                return;
            };
            if to.parent() != Some(target.as_path()) {
                *self.restore_target_root_attack.lock().unwrap() = Some(target);
                return;
            }
            let displaced = target.with_extension("eigener-zielroot");
            std::fs::rename(&target, &displaced).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("fremd.txt"), b"fremder-zielroot").unwrap();
            *self.restore_target_root_displaced.lock().unwrap() = Some(displaced);
        }

        fn proof(path: &Path, metadata: &std::fs::Metadata) -> ProofListEntry {
            ProofListEntry {
                entry: Entry {
                    name: path.file_name().unwrap().to_string_lossy().into_owned(),
                    is_dir: metadata.is_dir(),
                    is_file: metadata.is_file(),
                    is_symlink: metadata.file_type().is_symlink(),
                    size: metadata.len(),
                    mtime: Some(metadata.mtime()),
                    created: None,
                    owner: None,
                    group: None,
                    extension: None,
                    mode: Some(metadata.mode() & 0o7777),
                    device: Some(metadata.dev()),
                    inode: Some(metadata.ino()),
                    hard_links: None,
                },
                raw_name: path.file_name().unwrap().as_bytes().to_vec(),
                safely_decoded: true,
                provider_token: Some(metadata.ino().to_le_bytes().to_vec()),
            }
        }

        fn matches(path: &Path, expected: &ProofListEntry) -> bool {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                expected.entry.device == Some(metadata.dev())
                    && expected.entry.inode == Some(metadata.ino())
                    && expected.provider_token == Some(metadata.ino().to_le_bytes().to_vec())
            })
        }

        fn swap_before_remove(&self, path: &Path) {
            let displaced = path.with_extension("eigene-probe");
            let is_dir = std::fs::symlink_metadata(path).unwrap().is_dir();
            std::fs::rename(path, &displaced).unwrap();
            if is_dir {
                std::fs::create_dir(path).unwrap();
                std::fs::write(path.join("fremd.txt"), b"fremder-inhalt").unwrap();
            } else {
                std::fs::write(path, b"fremder-inhalt").unwrap();
            }
            *self.attacked_path.lock().unwrap() = Some(path.to_path_buf());
            *self.displaced_path.lock().unwrap() = Some(displaced);
        }
    }

    impl Vfs for RestoreProbeRaceVfs {
        fn is_local(&self) -> bool {
            true
        }
        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }
        fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
            let _namespace = self.namespace.lock().unwrap();
            let mut listing = self.inner.list_complete(path)?;
            for entry in &mut listing.entries {
                let metadata = std::fs::symlink_metadata(path.join(&entry.entry.name))?;
                entry.provider_token = Some(metadata.ino().to_le_bytes().to_vec());
            }
            Ok(listing)
        }
        fn stat(&self, path: &Path) -> io::Result<Entry> {
            self.inner.stat(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
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
            let _namespace = self.namespace.lock().unwrap();
            std::fs::create_dir(path).map_err(MutationFailure::Unchanged)?;
            let metadata = std::fs::symlink_metadata(path).map_err(MutationFailure::Unchanged)?;
            Ok(Some(Self::proof(path, &metadata)))
        }
        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            self.inner.open_read(path)
        }
        fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(path)
        }
        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            if self.collision_on_call.load(Ordering::SeqCst) == 0 {
                std::fs::write(path, b"FREMD").unwrap();
                *self.collision_path.lock().unwrap() = Some(path.to_path_buf());
            }
            self.inner.open_write_new(path)
        }
        fn open_write_new_with_removal_proof(
            &self,
            path: &Path,
        ) -> Result<(Box<dyn Write + Send>, Option<ProofListEntry>), MutationFailure> {
            let _namespace = self.namespace.lock().unwrap();
            if self.collision_on_call.load(Ordering::SeqCst) == 0 {
                std::fs::write(path, b"FREMD").unwrap();
                *self.collision_path.lock().unwrap() = Some(path.to_path_buf());
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            let file = options.open(path).map_err(MutationFailure::Unchanged)?;
            let proof = Self::proof(path, &file.metadata().map_err(MutationFailure::Unchanged)?);
            if self.unclear_on_call.load(Ordering::SeqCst) == 0 {
                return Err(MutationFailure::OutcomeUnclear(io::Error::other(
                    "erzwungener Fehler nach tatsächlicher Probenanlage",
                )));
            }
            Ok((Box::new(file), Some(proof)))
        }
        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }
        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            let call = self.sync_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_sync_on_call.load(Ordering::SeqCst) == call {
                return Err(io::Error::other(format!(
                    "erzwungener Probe-Parent-Syncfehler bei Aufruf {call}"
                )));
            }
            std::fs::File::open(path)?.sync_all()
        }
        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }
        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            let _namespace = self.namespace.lock().unwrap();
            if self.collision_on_call.load(Ordering::SeqCst) == usize::MAX
                && self.conditional_renames.load(Ordering::SeqCst) >= 3
            {
                self.swap_before_remove(path);
            }
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }
        fn remove_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir(path)
        }
        fn remove_if_matches(&self, path: &Path, expected: &ProofListEntry) -> io::Result<()> {
            let _namespace = self.namespace.lock().unwrap();
            self.conditional_removes.fetch_add(1, Ordering::SeqCst);
            self.swap_before_remove(path);
            if !Self::matches(path, expected) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Probename wurde unmittelbar vor der bedingten Entfernung ausgetauscht",
                ));
            }
            self.inner.remove_file(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            let call = self.conditional_renames.fetch_add(1, Ordering::SeqCst) + 1;
            if self.collision_on_call.load(Ordering::SeqCst) == call {
                std::fs::write(to, b"FREMD").unwrap();
                *self.collision_path.lock().unwrap() = Some(to.to_path_buf());
            }
            self.inner.rename_noreplace(from, to)
        }
        fn rename_noreplace_with_removal_proof(
            &self,
            from: &Path,
            to: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            self.rename_noreplace(from, to)
                .map_err(MutationFailure::Unchanged)?;
            if self.unclear_on_call.load(Ordering::SeqCst)
                == self.conditional_renames.load(Ordering::SeqCst)
            {
                return Err(MutationFailure::OutcomeUnclear(io::Error::other(
                    "erzwungener Antwortverlust nach tatsächlichem Probenrename",
                )));
            }
            Ok(None)
        }
        fn rename_noreplace_if_matches(
            &self,
            from: &Path,
            to: &Path,
            expected: &ProofListEntry,
        ) -> io::Result<()> {
            let _namespace = self.namespace.lock().unwrap();
            if !Self::matches(from, expected) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Probeidentität änderte sich vor dem bedingten Rename",
                ));
            }
            self.conditional_renames.fetch_add(1, Ordering::SeqCst);
            self.inner.rename_noreplace(from, to)
        }
        fn supports_rename_noreplace_if_matches(&self) -> bool {
            true
        }
        fn rename_noreplace_if_matches_in_bound_parent(
            &self,
            from: &Path,
            to: &Path,
            expected: &ProofListEntry,
            bound_parent: &Path,
            expected_parent: &ProofListEntry,
        ) -> io::Result<()> {
            let _namespace = self.namespace.lock().unwrap();
            self.swap_target_root_before_restore(to);
            if (from.parent() != Some(bound_parent) && to.parent() != Some(bound_parent))
                || !Self::matches(from, expected)
                || !Self::matches(bound_parent, expected_parent)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Probe oder gebundener Elternordner änderten sich vor dem atomaren Rename",
                ));
            }
            self.conditional_renames.fetch_add(1, Ordering::SeqCst);
            self.inner.rename_noreplace(from, to)
        }
        fn supports_rename_noreplace_if_matches_in_bound_parent(&self) -> bool {
            true
        }
        fn rename_noreplace_if_matches_in_bound_parents(
            &self,
            request: BoundParentsRenameRequest<'_>,
        ) -> io::Result<()> {
            let BoundParentsRenameRequest {
                from,
                to,
                expected,
                from_parent,
                expected_from_parent,
                to_parent,
                expected_to_parent,
                required_content,
            } = request;
            let _namespace = self.namespace.lock().unwrap();
            self.swap_target_root_before_restore(to);
            if from.parent() != Some(from_parent)
                || to.parent() != Some(to_parent)
                || !Self::matches(from, expected)
                || self.inner.root_identity(from_parent)? != *expected_from_parent
                || self.inner.root_identity(to_parent)? != *expected_to_parent
                || match required_content {
                    Some(required) => match hash_vfs_file(&self.inner, required.path) {
                        Ok(actual) => *actual.as_bytes() != required.digest,
                        Err(_) => true,
                    },
                    None => false,
                }
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Probe oder einer ihrer gebundenen Elternordner änderten sich vor dem atomaren Rename",
                ));
            }
            self.conditional_renames.fetch_add(1, Ordering::SeqCst);
            self.inner.rename_noreplace(from, to)
        }
        fn supports_rename_noreplace_if_matches_in_bound_parents(&self) -> bool {
            true
        }
        fn replace_file_if_content_matches(
            &self,
            request: ConditionalContentReplaceRequest<'_>,
        ) -> Result<(), MutationFailure> {
            let _namespace = self.namespace.lock().unwrap();
            self.conditional_replaces.fetch_add(1, Ordering::SeqCst);
            let current = std::fs::read(request.path).map_err(MutationFailure::Unchanged)?;
            if *blake3::hash(&current).as_bytes() != request.expected_digest {
                return Err(MutationFailure::Unchanged(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Manifestdigest änderte sich vor dem Testaustausch",
                )));
            }
            std::fs::write(request.path, request.replacement)
                .map_err(MutationFailure::OutcomeUnclear)?;
            std::fs::File::open(request.path)
                .and_then(|file| file.sync_all())
                .and_then(|()| {
                    std::fs::File::open(
                        request
                            .path
                            .parent()
                            .ok_or_else(|| invalid("Manifestpfad ohne Elternordner"))?,
                    )?
                    .sync_all()
                })
                .map_err(MutationFailure::OutcomeUnclear)
        }
        fn supports_replace_file_if_content_matches(&self) -> bool {
            true
        }
        fn supports_open_write_new_with_removal_proof(&self) -> bool {
            true
        }
        fn supports_create_dir_with_removal_proof(&self) -> bool {
            true
        }
        fn supports_remove_if_matches(&self) -> bool {
            true
        }
        fn root_identity(&self, path: &Path) -> io::Result<crate::RootIdentity> {
            self.inner.root_identity(path)
        }
    }

    fn ignore_progress(_: &Progress) {}

    #[test]
    fn lokale_restore_probe_erhaelt_kollisionen_an_allen_vier_namenuebergaengen() {
        let mut lost = Vec::new();
        for step in 0..=3 {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let target = root.join("ziel");
            let quarantine = root.join("quarantaene");
            std::fs::create_dir(&target).unwrap();
            std::fs::create_dir(&quarantine).unwrap();
            let vfs = RestoreProbeRaceVfs::new();
            vfs.collision_on_call.store(step, Ordering::SeqCst);

            let error = run_local_restore_probe(&vfs, &target, &quarantine).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            let foreign = vfs.collision_path.lock().unwrap().clone().unwrap();
            if std::fs::read(&foreign).ok().as_deref() != Some(b"FREMD".as_slice()) {
                lost.push(step);
                continue;
            }
            assert_eq!(
                std::fs::read_dir(&target).unwrap().count()
                    + std::fs::read_dir(&quarantine).unwrap().count(),
                1,
                "Neben der fremden Kollision darf keine eigene Probe bleiben"
            );
        }
        assert!(
            lost.is_empty(),
            "Fremde Kollisionsdateien gingen an diesen Übergängen verloren: {lost:?}"
        );
    }

    #[test]
    fn lokale_restore_probe_erhaelt_unklare_wirkung_mit_beiden_pruefpfaden() {
        for step in 0..=3 {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let target = root.join("ziel");
            let quarantine = root.join("quarantaene");
            std::fs::create_dir(&target).unwrap();
            std::fs::create_dir(&quarantine).unwrap();
            let vfs = RestoreProbeRaceVfs::new();
            vfs.unclear_on_call.store(step, Ordering::SeqCst);

            let error = run_local_restore_probe(&vfs, &target, &quarantine).unwrap_err();

            assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
            let paths = crate::error::recovery_paths(&error);
            assert_eq!(paths.len(), if step == 0 { 1 } else { 2 });
            assert_eq!(
                paths
                    .iter()
                    .filter(|path| Path::new(path).is_file())
                    .count(),
                1
            );
            assert_eq!(
                std::fs::read_dir(&target).unwrap().count()
                    + std::fs::read_dir(&quarantine).unwrap().count(),
                1
            );
        }
    }

    #[test]
    fn restore_probe_entfernt_keinen_ausgetauschten_probenamen() {
        let temp = tempfile::tempdir().unwrap();
        let test_root = std::fs::canonicalize(temp.path()).unwrap();
        let target = test_root.join("ziel");
        let quarantine = test_root.join("quarantaene");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&quarantine).unwrap();
        let vfs = RestoreProbeRaceVfs::new();

        let bound = ensure_probe_quarantine_root(&vfs, &test_root, &quarantine).unwrap();
        let result = run_bound_restore_probe(&vfs, &target, &bound);

        let error = result.expect_err("Der ausgetauschte Probename darf kein Erfolg sein");
        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::OutcomeUnclear,
            "unerwarteter Restore-Probenfehler: {error}"
        );
        let attacked = vfs.attacked_path.lock().unwrap().clone().unwrap();
        let displaced = vfs.displaced_path.lock().unwrap().clone().unwrap();
        assert_eq!(std::fs::read(&attacked).unwrap(), b"fremder-inhalt");
        assert!(std::fs::read(displaced).unwrap().len() >= 32);
        assert!(error.to_string().contains(&attacked.display().to_string()));
        assert_eq!(vfs.conditional_replaces.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn restore_probe_bindet_den_zielroot_bis_zum_rueckrename() {
        let temp = tempfile::tempdir().unwrap();
        let test_root = std::fs::canonicalize(temp.path()).unwrap();
        let target = test_root.join("ziel");
        let quarantine = test_root.join("quarantaene");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&quarantine).unwrap();
        let vfs = RestoreProbeRaceVfs::new();
        vfs.attack_target_root_before_restore(&target);

        let bound = ensure_probe_quarantine_root(&vfs, &test_root, &quarantine).unwrap();
        let error = run_bound_restore_probe(&vfs, &target, &bound)
            .expect_err("Der ausgetauschte Zielroot muss das Rückrename stoppen");

        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            std::fs::read(target.join("fremd.txt")).unwrap(),
            b"fremder-zielroot"
        );
        assert_eq!(
            std::fs::read_dir(&target).unwrap().count(),
            1,
            "Das Rückrename darf im fremd eingesetzten Zielroot keinen weiteren Namen anlegen"
        );
        let displaced = vfs
            .restore_target_root_displaced
            .lock()
            .unwrap()
            .clone()
            .expect("eigener Zielroot bleibt als Recoverybestand erhalten");
        assert!(displaced.is_dir());
    }

    #[test]
    fn probeordner_cleanup_entfernt_keinen_ausgetauschten_ordner() {
        let temp = tempfile::tempdir().unwrap();
        let test_root = std::fs::canonicalize(temp.path()).unwrap();
        let parent = test_root.join("ziel");
        let base = parent.join(".doppeldecker-quarantine");
        std::fs::create_dir(&parent).unwrap();
        let vfs = RestoreProbeRaceVfs::new();
        let bound = ensure_probe_quarantine_root(&vfs, &parent, &base).unwrap();
        let proof = bound
            .removal_proof
            .as_ref()
            .expect("der Test-VFS bindet die Ordneranlage atomar");

        let error = cleanup_probe_quarantine_root(&vfs, &parent, &base, Some(proof))
            .expect_err("Der ausgetauschte Probeordner darf kein Erfolg sein");

        let error = ProviderEndpoint::probe_blocker_or_error(error, "Quarantäne")
            .expect_err("Unklarer Cleanup darf kein gewöhnlicher Fähigkeitsblocker sein");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(
            std::fs::read(base.join("fremd.txt")).unwrap(),
            b"fremder-inhalt"
        );
        let displaced = vfs.displaced_path.lock().unwrap().clone().unwrap();
        assert!(displaced.is_dir(), "der eigene Probeordner bleibt erhalten");
        assert!(error.to_string().contains(&base.display().to_string()));
        assert!(
            crate::error::recovery_paths(&error).contains(&base.to_string_lossy().into_owned()),
            "Der echte Cleanupfehler verliert den strukturierten Prüfpfad"
        );
    }

    #[test]
    fn bestehende_probe_quarantaene_verwirft_symlink_vor_kindmutation() {
        let temp = tempfile::tempdir().unwrap();
        let test_root = std::fs::canonicalize(temp.path()).unwrap();
        let parent = test_root.join("ziel");
        let outside = test_root.join("fremder-ordner");
        let base = parent.join(".doppeldecker-quarantine");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("fremd.txt"), b"unveraendert").unwrap();
        std::os::unix::fs::symlink(&outside, &base).unwrap();

        let error = ensure_probe_quarantine_root(&LocalVfs::new(), &parent, &base)
            .expect_err("ein bestehender Symlink ist keine gebundene Quarantänebasis");

        assert_eq!(
            crate::error_code(&error),
            crate::ErrorCode::InvalidData,
            "unerwarteter Basisfehler: {error}"
        );
        assert_eq!(
            std::fs::read(outside.join("fremd.txt")).unwrap(),
            b"unveraendert"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn probe_quarantaene_syncfehler_behaelt_den_anlageproof_fuer_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let test_root = std::fs::canonicalize(temp.path()).unwrap();
        let parent = test_root.join("ziel");
        let base = parent.join(".doppeldecker-quarantine");
        std::fs::create_dir(&parent).unwrap();
        let vfs = RestoreProbeRaceVfs::new();
        vfs.fail_sync_on_call(1);

        let error = ensure_probe_quarantine_root(&vfs, &parent, &base).unwrap_err();

        let error = ProviderEndpoint::probe_blocker_or_error(error, "Quarantäne")
            .expect_err("Unklarer Cleanup darf kein gewöhnlicher Fähigkeitsblocker sein");
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.conditional_removes.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(base.join("fremd.txt")).unwrap(),
            b"fremder-inhalt"
        );
        let displaced = vfs.displaced_path.lock().unwrap().clone().unwrap();
        assert!(
            displaced.is_dir(),
            "der gebundene eigene Probeordner bleibt erhalten"
        );
        let message = error.to_string();
        assert!(message.contains("Probe-Parent-Syncfehler"));
        assert!(message
            .contains("Probename wurde unmittelbar vor der bedingten Entfernung ausgetauscht"));
        assert!(message.contains(&base.display().to_string()));
        assert!(
            crate::error::recovery_paths(&error).contains(&base.to_string_lossy().into_owned()),
            "Probe- und Cleanupfehler nennen den Prüfpfad nur im Text"
        );
    }

    #[test]
    fn restore_probe_freigabe_verlangt_alle_identitaets_und_manifestprimitiven() {
        assert!(RestoreProbeRaceVfs::new().supports_bound_restore_probe());
        assert!(
            !LocalVfs::new().supports_bound_restore_probe(),
            "ein normales descriptor-relatives Rename oder Unlink ist noch kein bedingter Restorevertrag"
        );
    }

    #[test]
    fn update_gate_stoppt_vor_der_ersten_mutierenden_capability_probe() {
        let temp = tempfile::tempdir().unwrap();
        let residue = temp.path().join("darf-nie-entstehen");
        let probe_called = std::cell::Cell::new(false);

        let error =
            probe_target_add_only_for_mode(SyncMode::Update, false, &LocalVfs::new(), || {
                probe_called.set(true);
                // Simuliert gerade den gefährlichen Providerfall: Die Probe hat
                // schon angelegt und verliert anschließend ihren Cleanup.
                std::fs::write(&residue, b"Probe-Rest")?;
                Err::<(), _>(io::Error::other("erzwungener Cleanupfehler"))
            })
            .unwrap_err();

        assert_eq!(crate::error_code(&error), crate::ErrorCode::Unsupported);
        assert!(
            !probe_called.get(),
            "das Restore-Gate muss vor der Probe liegen"
        );
        assert!(
            !residue.exists(),
            "eine abgelehnte Probe darf keinen Rest erzeugen"
        );
    }

    #[test]
    fn interaktives_rename_validiert_root_und_namen_vor_der_capability() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let old = root.join("alt.txt");
        std::fs::write(&old, b"inhalt").unwrap();
        let endpoint = ProviderEndpoint::test_remote_local(ProviderKind::Smb, &root).unwrap();

        let error = endpoint
            .execute_interactive_rename(&old, "../ausbruch", &None)
            .unwrap_err();

        assert_eq!(crate::error_code(&error), crate::ErrorCode::InvalidInput);
        assert_eq!(std::fs::read(&old).unwrap(), b"inhalt");
        assert!(!root.parent().unwrap().join("ausbruch").exists());
    }

    #[test]
    fn generische_lokalrichtung_nutzt_denselben_realmodusvertrag() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("datei"), b"inhalt").unwrap();

        let prepared = prepare_provider_transfer(
            ProviderEndpoint::local(&source).unwrap(),
            ProviderEndpoint::local(&target).unwrap(),
            SyncOptions {
                compare: Compare::ContentHash,
                ..SyncOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            prepared.capabilities(),
            ProviderTransferCapabilities::local()
        );
        let phrase = prepared.preview().confirmation_phrase.clone();
        let run = prepared.confirm(&phrase, false).unwrap();
        run.execute(&Cancel::new(), &mut ignore_progress).unwrap();
        assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"inhalt");
    }

    #[test]
    fn ungueltiger_ausschluss_stoppt_vor_jedem_providerzugriff() {
        let missing = PathBuf::from("/doppeldecker-test-darf-nicht-geoeffnet-werden");
        let error = match prepare_remote_run(
            RunVfs::Local(LocalVfs::new()),
            missing.join("quelle"),
            RunVfs::Local(LocalVfs::new()),
            missing.join("ziel"),
            missing,
            SyncOptions {
                exclude: vec!["/absolut-ist-ungueltig".into()],
                ..SyncOptions::default()
            },
        ) {
            Ok(_) => panic!("Ungültige Optionen dürfen keine Vorbereitung liefern"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("Ausschlussmuster"));
    }

    #[test]
    fn generische_matrix_blockt_gleiche_und_verschachtelte_roots() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let nested = source.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let result = prepare_provider_transfer(
            ProviderEndpoint::local(&source).unwrap(),
            ProviderEndpoint::local(&nested).unwrap(),
            SyncOptions::default(),
        );
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn zielmatrix_leitet_modi_nur_aus_geprueften_faehigkeiten_ab() {
        let add_only = RemoteCapabilities {
            complete_listing: true,
            exclusive_create: true,
            verified_file_commit: true,
            ..RemoteCapabilities::default()
        };
        let matrix = ProviderTransferCapabilities::remote(ProviderKind::WebDav, add_only);
        assert!(matrix.add_only);
        assert!(!matrix.update);
        assert!(!matrix.mirror);

        let mirror = RemoteCapabilities {
            complete_listing: true,
            exclusive_create: true,
            atomic_noreplace_rename: true,
            atomic_replace_rename: true,
            durable_file_flush: true,
            durable_directory_flush: true,
            entry_delete: true,
            quarantine: true,
            available_space: true,
            ..RemoteCapabilities::default()
        };
        let matrix = ProviderTransferCapabilities::remote(ProviderKind::Smb, mirror);
        assert!(matrix.add_only && matrix.update && matrix.mirror);
        let add_only_digest = crate::remote::capabilities_digest(add_only).unwrap();
        let mirror_digest = crate::remote::capabilities_digest(mirror).unwrap();
        assert_ne!(
            bind_capabilities_digest([7; 32], Some(add_only_digest)),
            bind_capabilities_digest([7; 32], Some(mirror_digest))
        );
    }

    #[test]
    fn datei_capability_trennt_starke_von_begrenzter_quellentfernung() {
        let temp = tempfile::tempdir().unwrap();
        let endpoint = ProviderEndpoint::local(temp.path()).unwrap();
        let capabilities = endpoint.probe_file_capabilities().unwrap();

        // Ein lokaler Einzel-Remove ist descriptor-relativ begrenzt, bindet die
        // Identität zwischen Readback und Wirkung aber nicht atomar. Die Vorschau
        // darf daraus deshalb niemals die starke Providerzusage erfinden.
        assert!(!capabilities.remove_source);
        assert!(capabilities.remove_source_best_effort);
        assert!(!capabilities.replace);
        assert!(capabilities.replace_best_effort);
        assert!(capabilities
            .limitations
            .iter()
            .any(|note| note.contains("Move-Quellen werden als Best Effort")));
    }

    #[test]
    fn datei_capability_meldet_kein_starkes_replace_ohne_bedingtes_rename() {
        let temp = tempfile::tempdir().unwrap();
        let endpoint = ProviderEndpoint::test_remote_local(ProviderKind::Smb, temp.path()).unwrap();

        let capabilities = endpoint.probe_file_capabilities().unwrap();

        assert!(capabilities.copy_new);
        assert!(capabilities.rename_noreplace);
        assert!(
            !capabilities.replace,
            "Quarantäne und No-Replace-Rename allein belegen noch kein identitätsgebundenes Park-Rename"
        );
        assert!(capabilities
            .blockers
            .iter()
            .any(|blocker| blocker.contains("identitätsgebundenes bedingtes Rename")));
    }

    #[test]
    fn quarantineverwaltung_folgt_dem_providervertrag() {
        let sftp_management = ProviderKind::Sftp.quarantine_management_capabilities();
        assert!(!sftp_management.listing);
        assert!(!sftp_management.restore);
        assert!(!sftp_management.purge);
        assert!(!ProviderKind::Sftp.supports_quarantine_management());
        let smb_management = ProviderKind::Smb.quarantine_management_capabilities();
        assert!(smb_management.listing);
        assert!(!smb_management.restore);
        assert!(!smb_management.purge);
        assert!(ProviderKind::Smb.supports_quarantine_management());
        assert!(!ProviderKind::Local.supports_quarantine_management());
        assert!(!ProviderKind::Ftps.supports_quarantine_management());
        assert!(!ProviderKind::WebDav.supports_quarantine_management());

        let temp = tempfile::tempdir().unwrap();
        let sftp = ProviderEndpoint::test_remote_local(ProviderKind::Sftp, temp.path()).unwrap();
        assert_eq!(
            sftp.probe_quarantine_restore_capability()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            sftp.probe_quarantine_purge_capability().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );

        let ftps = ProviderEndpoint::test_remote_local(ProviderKind::Ftps, temp.path()).unwrap();
        assert_eq!(
            ftps.probe_quarantine_restore_capability()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            ftps.probe_quarantine_purge_capability().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );

        let smb = ProviderEndpoint::test_remote_local(ProviderKind::Smb, temp.path()).unwrap();
        assert_eq!(
            smb.probe_quarantine_restore_capability()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            smb.probe_quarantine_purge_capability().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    /// „Abgesichert" darf nicht als „ohne jede Einschränkung" gelesen werden: Die
    /// Klasse schützt das Ziel, die Quelle bleibt in beiden Klassen pfadgebunden.
    /// Fähigkeitsbericht und Ergebnisbericht müssen das ausweisen — hier am
    /// lokalen Endpunkt, der die Klasse `Assured` ohne Probe erhält.
    #[test]
    fn root_ueberlappung_stoppt_vor_jeder_mutierenden_capability_probe() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("quelle");
        let target = source.join("unterziel");
        std::fs::create_dir_all(&target).unwrap();
        let source_vfs = RunVfs::Local(crate::LocalVfs::new());
        let target_vfs = RunVfs::Local(crate::LocalVfs::new());
        let probed = std::cell::Cell::new(false);

        let error = bind_nonoverlapping_roots_before_probe(
            &source_vfs,
            &source,
            &target_vfs,
            &target,
            || {
                probed.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            !probed.get(),
            "Die mutierende Probe lief vor der Root-Schranke"
        );
    }

    #[test]
    fn remote_namespace_vergleicht_kanonische_gesamtpfade_statt_profilroots() {
        let server = |canonical_root: &str| crate::remote::SftpServerIdentity {
            host: "sftp.example.test".into(),
            port: 22,
            host_key: crate::remote::SftpHostKeyPin {
                algorithm: "ssh-ed25519".into(),
                sha256: [7; 32],
            },
            canonical_root: canonical_root.into(),
            sftp_version: 3,
            extensions_digest: [8; 32],
        };
        let identity = |canonical_root: &str| {
            crate::RootIdentity::Sftp(crate::remote::SftpRootIdentity {
                server: server(canonical_root),
                virtual_root: "/".into(),
                session_generation: 1,
            })
        };

        let nested = reject_overlapping_roots(
            &identity("/data"),
            Path::new("/"),
            &identity("/data/sub"),
            Path::new("/"),
        )
        .unwrap_err();
        assert_eq!(nested.kind(), io::ErrorKind::PermissionDenied);

        reject_overlapping_roots(
            &identity("/data/links"),
            Path::new("/"),
            &identity("/data/rechts"),
            Path::new("/"),
        )
        .unwrap();
    }

    #[test]
    fn lokale_pfade_bleiben_ueber_mountgrenzen_ein_host_namespace() {
        let local = |device, mount_id, fsid: u8| {
            crate::RootIdentity::Local(crate::vfs::LocalRootIdentity {
                device,
                inode: device + 100,
                is_directory: true,
                mount: crate::vfs::LocalMountIdentity {
                    mount_id,
                    unique_mount_id: true,
                    fsid: vec![fsid],
                    volume_id: None,
                    fs_type: "test".into(),
                    mount_flags: 0,
                },
                display_path: PathBuf::from("/nur-anzeige"),
            })
        };

        let error = reject_overlapping_roots(
            &local(1, 11, 1),
            Path::new("/host/daten"),
            &local(2, 22, 2),
            Path::new("/host/daten/eingehangen"),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn getrennte_laufzeit_endpunkte_binden_namespace_und_ueberlappung() {
        let temp = tempfile::tempdir().unwrap();
        let links = temp.path().join("links");
        let rechts = temp.path().join("rechts");
        let nested = links.join("unterziel");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(&rechts).unwrap();

        let source = ProviderEndpoint::test_remote_local(ProviderKind::Sftp, &links).unwrap();
        let sibling = ProviderEndpoint::test_remote_local(ProviderKind::Sftp, &rechts).unwrap();
        assert!(source.shares_runtime_namespace(&sibling).unwrap());

        let nested = ProviderEndpoint::test_remote_local(ProviderKind::Sftp, &nested).unwrap();
        let error = source.shares_runtime_namespace(&nested).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn abgesichertes_umbenennen_weist_die_pfadbindung_der_quelle_aus() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("alt.txt"), b"inhalt").unwrap();
        let endpoint = ProviderEndpoint::local(&root).unwrap();

        let capabilities = endpoint.probe_rename_capabilities().unwrap();
        assert!(capabilities.assured);
        assert_eq!(
            capabilities.limitations,
            vec![crate::remote_rename::SOURCE_PATH_BINDING_LIMITATION.to_string()]
        );
        assert!(capabilities.blockers.is_empty());

        let report = endpoint
            .execute_interactive_rename(&root.join("alt.txt"), "neu.txt", &None)
            .unwrap();
        assert_eq!(report.outcome, crate::RenameOutcome::Renamed);
        assert_eq!(report.class, crate::RenameExecutionClass::Assured);
        assert_eq!(
            report.limitations,
            vec![crate::remote_rename::SOURCE_PATH_BINDING_LIMITATION.to_string()]
        );
        assert_eq!(std::fs::read(root.join("neu.txt")).unwrap(), b"inhalt");
    }
}
