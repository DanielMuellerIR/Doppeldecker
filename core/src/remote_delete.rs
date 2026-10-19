//! Providerfähiges Löschen ausgewählter Pane-Einträge.
//!
//! Der Plan enthält ausschließlich öffentliche Root-Identität, exakte Pfade und
//! Listingbelege. Vor der ersten Mutation wird er frisch neu erzeugt und bytegenau
//! verglichen. SFTP/SMB parken Einträge in der manifestierten Doppeldecker-
//! Quarantäne; Provider ohne geprüften Restorepfad dürfen nur den ausdrücklich
//! bestätigten endgültigen Modus verwenden.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use crate::error::{coded_error, error_code, with_context, with_recovery_context, ErrorCode};
use crate::quarantine::{
    complete_quarantine_run_if_matches, record_quarantine_intent_with_proof,
    start_quarantine_run_with_proof, verify_quarantine_manifest_digest, QuarantineReason,
};
use crate::real_mode::reserve_quarantine_run_name;
use crate::vfs::{BoundContentDigest, BoundParentsRenameRequest, MutationFailure, ProofListEntry};
use crate::{Cancel, Progress, RootIdentity, Vfs};

const QUARANTINE_NAME: &str = ".doppeldecker-quarantine";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteDeleteMode {
    Quarantine,
    Permanent,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteDeleteRecord {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub provider_token: Option<Vec<u8>>,
    pub content_digest: Option<[u8; 32]>,
}

/// Serialisierbarer Beleg des ausgewählten Top-Level-Eintrags. Die einzelnen
/// Baumrecords binden Inhalt und Provider-Token; der Claim braucht zusätzlich den
/// rohen Namen und eine lokale Inode-Identität, damit auch ein gleichartiger
/// Nachfolger nicht als bestätigte Quelle gilt.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteDeleteSourceProof {
    pub raw_name: Vec<u8>,
    pub provider_token: Option<Vec<u8>>,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mtime: Option<i64>,
    pub mode: Option<u32>,
    pub device: Option<u64>,
    pub inode: Option<u64>,
}

impl RemoteDeleteSourceProof {
    fn from_listed(proof: &ProofListEntry) -> Self {
        Self {
            raw_name: proof.raw_name.clone(),
            provider_token: proof.provider_token.clone(),
            is_dir: proof.entry.is_dir,
            is_file: proof.entry.is_file,
            is_symlink: proof.entry.is_symlink,
            size: proof.entry.size,
            mtime: proof.entry.mtime,
            mode: proof.entry.mode,
            device: proof.entry.device,
            inode: proof.entry.inode,
        }
    }

    fn as_listed(&self, path: &Path) -> io::Result<ProofListEntry> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("Remote-Löschquelle besitzt keinen sicheren Namen"))?;
        if self.raw_name != name.as_bytes() || self.is_symlink || self.is_dir == self.is_file {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::PlanDrift,
                "Remote-Löschbeleg passt nicht mehr zum ausgewählten Quellpfad",
            ));
        }
        Ok(ProofListEntry {
            entry: crate::vfs::Entry {
                name: name.to_owned(),
                is_dir: self.is_dir,
                is_file: self.is_file,
                is_symlink: self.is_symlink,
                size: self.size,
                mtime: self.mtime,
                created: None,
                owner: None,
                group: None,
                extension: None,
                mode: self.mode,
                device: self.device,
                inode: self.inode,
                hard_links: None,
            },
            raw_name: self.raw_name.clone(),
            safely_decoded: true,
            provider_token: self.provider_token.clone(),
        })
    }

    fn matches_moved(&self, current: &ProofListEntry) -> bool {
        // SMB liefert zusätzlich zur veränderlichen Change-Time eine stabile
        // File-ID als `inode`. Nach einem Rename darf deshalb die stabile ID den
        // Vorrang haben; Provider ohne solche ID bleiben an ihren Token gebunden.
        let identity_matches = match (
            self.device,
            self.inode,
            current.entry.device,
            current.entry.inode,
        ) {
            (Some(expected_device), Some(expected_inode), Some(device), Some(inode)) => {
                expected_device == device && expected_inode == inode
            }
            _ => matches!(
                (&self.provider_token, &current.provider_token),
                (Some(expected), Some(actual)) if expected == actual
            ),
        };
        identity_matches
            && self.is_dir == current.entry.is_dir
            && self.is_file == current.entry.is_file
            && self.is_symlink == current.entry.is_symlink
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteDeleteItem {
    pub path: String,
    pub is_dir: bool,
    pub entries: u64,
    pub bytes: u64,
    pub digest: [u8; 32],
    #[serde(default)]
    pub source_proof: RemoteDeleteSourceProof,
    /// Präorder einschließlich des ausgewählten Eintrags. Der Executor verarbeitet
    /// die Liste rückwärts, sodass Verzeichnisse erst nach ihren Kindern verschwinden.
    pub records: Vec<RemoteDeleteRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteDeletePlan {
    pub root: String,
    pub root_identity: RootIdentity,
    pub items: Vec<RemoteDeleteItem>,
    pub entries: u64,
    pub bytes: u64,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RemoteDeleteStats {
    pub items: u64,
    pub entries: u64,
    pub bytes: u64,
    pub cancelled: bool,
    pub quarantine_run: Option<String>,
}

/// Erzeugt einen vollständigen, deterministischen Plan für direkte Kinder des
/// gebundenen Pane-Roots. Verschachtelte oder überlappende Auswahlen werden nicht
/// still normalisiert, weil die UI genau die sichtbare Auswahl bestätigen soll.
pub fn validate_remote_delete_request(root: &Path, selected: &[PathBuf]) -> io::Result<()> {
    validate_absolute(root)?;
    if selected.is_empty() {
        return Err(invalid("Remote-Löschen benötigt mindestens einen Eintrag"));
    }
    if selected.iter().any(|path| {
        validate_absolute(path).is_err()
            || path.parent() != Some(root)
            || path.file_name().and_then(|name| name.to_str()) == Some(QUARANTINE_NAME)
    }) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Remote-Löschauswahl verlässt den gebundenen Pane-Root oder trifft den reservierten Quarantänepfad",
        ));
    }
    Ok(())
}

pub fn plan_remote_delete(
    vfs: &dyn Vfs,
    root: &Path,
    selected: &[PathBuf],
) -> io::Result<RemoteDeletePlan> {
    // Diese reine Vorprüfung muss vollständig vor Rootbindung und Listing liegen.
    // Die Tauri-Schicht verwendet denselben Vertrag schon vor Verbindung und
    // mutierender Capability-Probe.
    validate_remote_delete_request(root, selected)?;
    let root_identity = vfs
        .root_identity(root)
        .map_err(|error| with_context(error, "Remote-Löschroot konnte nicht gebunden werden"))?;
    let root_listing = vfs.list_complete(root).map_err(|error| {
        with_context(
            error,
            "Remote-Löschroot konnte nicht vollständig gelistet werden",
        )
    })?;
    if !root_listing.complete {
        return Err(invalid(
            "Remote-Löschen benötigt ein vollständiges Listing des Pane-Roots",
        ));
    }
    let mut selected = selected.to_vec();
    selected.sort();
    selected.dedup();

    let mut items = Vec::with_capacity(selected.len());
    for path in selected {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("Remote-Löschpfad ist nicht eindeutig UTF-8"))?;
        let proof = root_listing
            .entries
            .iter()
            .find(|proof| proof.safely_decoded && proof.raw_name == name.as_bytes())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Ausgewählter Remote-Eintrag fehlt im vollständigen Listing: {name}"),
                )
            })?;
        let mut records = Vec::new();
        scan_entry(vfs, &path, proof, &mut records).map_err(|error| {
            with_context(
                error,
                format!(
                    "Remote-Löschauswahl {} konnte nicht gebunden werden",
                    path.display()
                ),
            )
        })?;
        if matches!(&root_identity, RootIdentity::WebDav(_)) {
            for record in &records {
                crate::webdav::confirmed_delete_etag(record.provider_token.as_deref()).map_err(
                    |error| {
                        with_context(
                            error,
                            format!("Remote-Löschbeleg für {} fehlt", record.path),
                        )
                    },
                )?;
            }
        }
        let entries = records.len() as u64;
        let bytes = sum_delete_values(
            records
                .iter()
                .filter(|record| !record.is_dir)
                .map(|record| record.size),
            "Bytes der Lösch-Auswahl",
        )?;
        let digest = digest_records(&records)?;
        items.push(RemoteDeleteItem {
            path: path.to_string_lossy().into_owned(),
            is_dir: proof.entry.is_dir,
            entries,
            bytes,
            digest,
            source_proof: RemoteDeleteSourceProof::from_listed(proof),
            records,
        });
    }
    let entries = sum_delete_values(
        items.iter().map(|item| item.entries),
        "Einträge des Löschplans",
    )?;
    let bytes = sum_delete_values(items.iter().map(|item| item.bytes), "Bytes des Löschplans")?;
    let digest = *blake3::hash(&serde_json::to_vec(&items).map_err(io::Error::other)?).as_bytes();
    Ok(RemoteDeletePlan {
        root: root.to_string_lossy().into_owned(),
        root_identity,
        items,
        entries,
        bytes,
        digest,
    })
}

fn sum_delete_values(mut values: impl Iterator<Item = u64>, label: &str) -> io::Result<u64> {
    values.try_fold(0_u64, |total, value| {
        total.checked_add(value).ok_or_else(|| {
            invalid(format!(
                "{label} überschreiten den darstellbaren Zahlenbereich"
            ))
        })
    })
}

fn scan_entry(
    vfs: &dyn Vfs,
    path: &Path,
    proof: &ProofListEntry,
    records: &mut Vec<RemoteDeleteRecord>,
) -> io::Result<()> {
    let entry = &proof.entry;
    if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
        return Err(invalid(format!(
            "Remote-Löschen überspringt Symlinks und Spezialeinträge: {}",
            path.display()
        )));
    }
    records.push(RemoteDeleteRecord {
        path: path.to_string_lossy().into_owned(),
        is_dir: entry.is_dir,
        size: entry.size,
        provider_token: proof.provider_token.clone(),
        content_digest: if entry.is_file {
            let mut reader = vfs.open_read(path)?;
            let mut hasher = blake3::Hasher::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
            Some(*hasher.finalize().as_bytes())
        } else {
            None
        },
    });
    if !entry.is_dir {
        return Ok(());
    }
    let listing = vfs.list_complete(path)?;
    if !listing.complete {
        return Err(invalid(format!(
            "Remote-Unterordner wurde nicht vollständig gelistet: {}",
            path.display()
        )));
    }
    let mut children = listing.entries;
    children.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
    for child in children {
        if !child.safely_decoded {
            return Err(invalid(format!(
                "Remote-Unterordner enthält einen nicht eindeutig dekodierbaren Namen: {}",
                path.display()
            )));
        }
        let name = std::str::from_utf8(&child.raw_name)
            .map_err(|_| invalid("Remote-Listing enthält keinen eindeutigen UTF-8-Namen"))?;
        scan_entry(vfs, &path.join(name), &child, records)?;
    }
    Ok(())
}

fn digest_records(records: &[RemoteDeleteRecord]) -> io::Result<[u8; 32]> {
    Ok(*blake3::hash(&serde_json::to_vec(records).map_err(io::Error::other)?).as_bytes())
}

/// Führt nur den exakt frisch bestätigten Plan aus. `Quarantine` erzeugt vor jedem
/// Rename einen dauerhaften Manifest-Intent. `Permanent` entfernt Blattdateien und
/// danach ausschließlich leere Ordner, damit ein später hinzugekommener Kindname
/// nicht durch eine serverseitige Rekursion verschwindet; zusätzlich wird jeder
/// Record unmittelbar vor seiner Mutation erneut gegen Typ und Größe geprüft.
pub fn execute_remote_delete(
    vfs: &dyn Vfs,
    expected: &RemoteDeletePlan,
    mode: RemoteDeleteMode,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<RemoteDeleteStats> {
    if mode == RemoteDeleteMode::Permanent && !vfs.supports_remove_if_matches() {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Dauerhaftes Remote-Löschen benötigt eine provideratomare Bedingung, die den bestätigten Objekt-Token bis in DELETE/RMD trägt",
        ));
    }
    let selected: Vec<PathBuf> = expected
        .items
        .iter()
        .map(|item| PathBuf::from(&item.path))
        .collect();
    let current = plan_remote_delete(vfs, Path::new(&expected.root), &selected)?;
    if !same_plan_across_fresh_session(expected, &current) {
        return Err(crate::error::coded_error(
            io::ErrorKind::InvalidData,
            crate::ErrorCode::PlanDrift,
            "Remote-Löschplan änderte sich unmittelbar vor der Mutation",
        ));
    }
    let mut progress = Progress {
        files_total: expected.entries,
        bytes_total: expected.bytes,
        phase: match mode {
            RemoteDeleteMode::Quarantine => "quarantine",
            RemoteDeleteMode::Permanent => "delete",
        }
        .into(),
        ..Progress::default()
    };
    on_progress(&progress);
    match mode {
        RemoteDeleteMode::Quarantine => execute_quarantine(
            vfs,
            expected,
            &current.root_identity,
            cancel,
            &mut progress,
            on_progress,
        ),
        RemoteDeleteMode::Permanent => {
            execute_permanent(vfs, expected, cancel, &mut progress, on_progress)
        }
    }
}

/// Ein Queue-Claim verbindet absichtlich frisch und beginnt dadurch wieder bei einer
/// neuen lokalen Sitzungsgeneration. Serveridentität und virtueller Root müssen exakt
/// gleich bleiben; nur diese pro Prozess vergebene Generation darf abweichen.
fn same_plan_across_fresh_session(expected: &RemoteDeletePlan, current: &RemoteDeletePlan) -> bool {
    expected.root == current.root
        && expected.items == current.items
        && expected.entries == current.entries
        && expected.bytes == current.bytes
        && expected.digest == current.digest
        && same_root_namespace(&expected.root_identity, &current.root_identity)
}

fn same_root_namespace(expected: &RootIdentity, current: &RootIdentity) -> bool {
    match (expected, current) {
        (RootIdentity::Local(left), RootIdentity::Local(right)) => left == right,
        (RootIdentity::Sftp(left), RootIdentity::Sftp(right)) => {
            left.server == right.server && left.virtual_root == right.virtual_root
        }
        (RootIdentity::Ftps(left), RootIdentity::Ftps(right)) => {
            left.server == right.server && left.virtual_root == right.virtual_root
        }
        (RootIdentity::WebDav(left), RootIdentity::WebDav(right)) => {
            left.server == right.server
                && left.virtual_root == right.virtual_root
                && left.root_etag == right.root_etag
        }
        (RootIdentity::Smb(left), RootIdentity::Smb(right)) => {
            left.server == right.server
                && left.virtual_root == right.virtual_root
                && left.root_file_id == right.root_file_id
        }
        _ => false,
    }
}

fn execute_quarantine(
    vfs: &dyn Vfs,
    plan: &RemoteDeletePlan,
    claim_root_identity: &RootIdentity,
    cancel: &Cancel,
    progress: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<RemoteDeleteStats> {
    if !vfs.supports_create_dir_with_removal_proof()
        || !vfs.supports_rename_noreplace_if_matches_in_bound_parents()
        || !vfs.supports_replace_file_if_content_matches()
    {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Provider-Quarantäne benötigt atomar gebundene Ordneranlagen, einen Claim mit gebundenem Quell- und Ziel-Elternordner und inhaltsgebundene Manifestwechsel",
        ));
    }
    let root = Path::new(&plan.root);
    let base = root.join(QUARANTINE_NAME);
    let base_binding = bind_or_create_quarantine_directory(vfs, root, &base, "Quarantänebasis")?;
    let run = reserve_quarantine_run_name(vfs, &base)?;
    let run_binding = create_bound_quarantine_directory(vfs, &base, &run, "Quarantänelauf")?;
    verify_bound_quarantine_directory(vfs, &base_binding)?;
    let started = start_quarantine_run_with_proof(vfs, &run, root, &run_binding.identity).map_err(
        |error| {
            delete_unclear(
                &run,
                "Der sichtbare Quarantänelauf konnte nicht manifestiert werden",
                error,
            )
        },
    )?;
    let _quarantine_lease = started.lease;
    let manifest_path = started.manifest_path;
    let mut manifest_digest = started.payload_digest;
    verify_bound_quarantine_directory(vfs, &base_binding)
        .and_then(|()| verify_bound_quarantine_directory(vfs, &run_binding))
        .map_err(|error| {
            delete_unclear(
                &run,
                "Der manifestierte Lauf wurde vor dem ersten Claim ausgetauscht",
                error,
            )
        })?;
    let mut stats = RemoteDeleteStats {
        quarantine_run: run
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        ..RemoteDeleteStats::default()
    };
    for item in &plan.items {
        if cancel.is_cancelled() {
            stats.cancelled = true;
            progress.phase = "cancelled".into();
            on_progress(progress);
            return Ok(stats);
        }
        let source = Path::new(&item.path);
        let relative = source
            .strip_prefix(root)
            .map_err(|_| invalid("Quarantänepfad verlässt den gebundenen Root"))?;
        let target = run.join(relative);
        let source_proof = bind_quarantine_source(vfs, root, item)
            .map_err(|error| after_partial_claim(stats.items, &run, error))?;
        let intent = record_quarantine_intent_with_proof(
            vfs,
            &run,
            relative,
            item.is_dir,
            QuarantineReason::UserDelete,
            manifest_digest,
        )
        .map_err(|failure| match failure {
            MutationFailure::Unchanged(error) => after_partial_claim(
                stats.items,
                &run,
                bound_delete_error(error, "Quarantäne-Manifestwechsel vor dem Claim scheiterte"),
            ),
            MutationFailure::OutcomeUnclear(error) => delete_unclear(
                &run,
                "Der inhaltsgebundene Manifestwechsel hat einen unklaren Ausgang",
                error,
            ),
        })?;
        manifest_digest = intent.payload_digest;
        progress.current = relative.to_string_lossy().into_owned();
        on_progress(progress);
        verify_bound_quarantine_directory(vfs, &base_binding)
            .and_then(|()| verify_bound_quarantine_directory(vfs, &run_binding))
            .map_err(|error| after_partial_claim(stats.items, &run, error))?;
        vfs.rename_noreplace_if_matches_in_bound_parents(BoundParentsRenameRequest {
            from: source,
            to: &target,
            expected: &source_proof,
            from_parent: root,
            expected_from_parent: claim_root_identity,
            to_parent: &run,
            expected_to_parent: &run_binding.identity,
            required_content: Some(BoundContentDigest {
                path: &intent.manifest_path,
                digest: intent.payload_digest,
            }),
        })
        .map_err(|error| {
            after_partial_claim(
                stats.items,
                &run,
                bound_delete_error(error, "Bedingter Quarantäne-Claim scheiterte"),
            )
        })?;
        verify_bound_quarantine_directory(vfs, &base_binding)
            .and_then(|()| verify_bound_quarantine_directory(vfs, &run_binding))
            .and_then(|()| verify_parked_item(vfs, &target, item))
            .and_then(|()| vfs.sync_dir(root))
            .and_then(|()| vfs.sync_dir(&run))
            .map_err(|error| {
                delete_unclear(
                    &run,
                    &format!(
                        "Der Claim von {} wurde sichtbar, aber sein Identitäts- oder Durability-Readback scheiterte",
                        source.display()
                    ),
                    error,
                )
            })?;
        stats.items += 1;
        stats.entries += item.entries;
        stats.bytes += item.bytes;
        progress.files_done += item.entries;
        progress.bytes_done += item.bytes;
        on_progress(progress);
    }
    verify_bound_quarantine_directory(vfs, &base_binding)
        .and_then(|()| verify_bound_quarantine_directory(vfs, &run_binding))
        .map_err(|error| {
            delete_unclear(&run, "Der Lauf wurde vor dem Abschluss ausgetauscht", error)
        })?;
    let complete_digest =
        complete_quarantine_run_if_matches(vfs, &run, manifest_digest).map_err(|failure| {
            match failure {
                MutationFailure::Unchanged(error) => after_partial_claim(stats.items, &run, error),
                MutationFailure::OutcomeUnclear(error) => delete_unclear(
                    &run,
                    "Der inhaltsgebundene Manifestabschluss hat einen unklaren Ausgang",
                    error,
                ),
            }
        })?;
    verify_quarantine_manifest_digest(vfs, &manifest_path, complete_digest).map_err(|error| {
        delete_unclear(
            &run,
            "Das Abschlussmanifest änderte sich vor seinem Readback",
            error,
        )
    })?;
    verify_bound_quarantine_directory(vfs, &base_binding)
        .and_then(|()| verify_bound_quarantine_directory(vfs, &run_binding))
        .map_err(|error| {
            delete_unclear(
                &run,
                "Der Lauf wurde während des Abschlusses ausgetauscht",
                error,
            )
        })?;
    progress.phase = "done".into();
    progress.current.clear();
    on_progress(progress);
    Ok(stats)
}

#[derive(Clone, Debug)]
struct BoundQuarantineDirectory {
    path: PathBuf,
    listing_proof: ProofListEntry,
    identity: RootIdentity,
}

fn bind_or_create_quarantine_directory(
    vfs: &dyn Vfs,
    parent: &Path,
    path: &Path,
    role: &str,
) -> io::Result<BoundQuarantineDirectory> {
    if let Some(bound) = bind_existing_quarantine_directory(vfs, parent, path, role)? {
        return Ok(bound);
    }
    create_bound_quarantine_directory(vfs, parent, path, role)
}

fn create_bound_quarantine_directory(
    vfs: &dyn Vfs,
    parent: &Path,
    path: &Path,
    role: &str,
) -> io::Result<BoundQuarantineDirectory> {
    let creation_proof = match vfs.create_dir_with_removal_proof(path) {
        Ok(Some(proof)) => proof_at_path(proof, path).map_err(|error| {
            delete_unclear(
                path,
                &format!("{role} wurde angelegt, aber sein Erzeugungsbeleg ist ungültig"),
                error,
            )
        })?,
        Ok(None) => {
            return Err(delete_unclear(
                path,
                &format!("{role} wurde sichtbar angelegt, aber nicht atomar gebunden"),
                io::Error::new(io::ErrorKind::Unsupported, "Erzeugungsbeleg fehlt"),
            ))
        }
        Err(MutationFailure::Unchanged(error)) => return Err(error),
        Err(MutationFailure::OutcomeUnclear(error)) => {
            return Err(delete_unclear(
                path,
                &format!("Die Anlage von {role} hat einen unklaren Ausgang"),
                error,
            ))
        }
    };
    vfs.sync_dir(parent).map_err(|error| {
        delete_unclear(
            path,
            &format!("{role} wurde nicht dauerhaft bestätigt"),
            error,
        )
    })?;
    let bound = bind_existing_quarantine_directory(vfs, parent, path, role)
        .map_err(|error| {
            delete_unclear(
                path,
                &format!("{role} konnte nach seiner sichtbaren Anlage nicht gebunden werden"),
                error,
            )
        })?
        .ok_or_else(|| {
            delete_unclear(
                path,
                &format!("{role} fehlt nach seiner sichtbaren Anlage"),
                io::Error::new(io::ErrorKind::NotFound, "Readback fand den Ordner nicht"),
            )
        })?;
    if !same_directory_proof(&creation_proof, &bound.listing_proof) {
        return Err(delete_unclear(
            path,
            &format!("{role} wurde zwischen Erzeugung und erstem Readback ausgetauscht"),
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Erzeugungsbeleg stimmt nicht überein",
            ),
        ));
    }
    Ok(bound)
}

fn bind_existing_quarantine_directory(
    vfs: &dyn Vfs,
    parent: &Path,
    path: &Path,
    role: &str,
) -> io::Result<Option<BoundQuarantineDirectory>> {
    let Some(first) = listed_directory_proof(vfs, parent, path, role)? else {
        return Ok(None);
    };
    let identity = vfs.root_identity(path)?;
    let second = listed_directory_proof(vfs, parent, path, role)?.ok_or_else(|| {
        coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            format!("{role} verschwand während seiner Identitätsbindung"),
        )
    })?;
    if !same_directory_proof(&first, &second) {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            format!("{role} wurde während seiner Identitätsbindung ausgetauscht"),
        ));
    }
    Ok(Some(BoundQuarantineDirectory {
        path: path.to_path_buf(),
        listing_proof: second,
        identity,
    }))
}

fn listed_directory_proof(
    vfs: &dyn Vfs,
    parent: &Path,
    path: &Path,
    role: &str,
) -> io::Result<Option<ProofListEntry>> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid(format!("{role} besitzt keinen sicheren Namen")))?;
    let listing = vfs.list_complete(parent)?;
    if !listing.complete || listing.entries.iter().any(|entry| !entry.safely_decoded) {
        return Err(invalid(format!(
            "{role} benötigt ein vollständiges, sicher dekodiertes Elternlisting"
        )));
    }
    let mut matching = listing
        .entries
        .into_iter()
        .filter(|entry| entry.raw_name == name.as_bytes());
    let Some(found) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() || !found.entry.is_dir || found.entry.is_symlink {
        return Err(invalid(format!(
            "{role} ist kein eindeutiger, symlinkfreier Ordner"
        )));
    }
    Ok(Some(found))
}

fn proof_at_path(mut proof: ProofListEntry, path: &Path) -> io::Result<ProofListEntry> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("Quarantänebeleg besitzt keinen sicheren Namen"))?;
    proof.entry.name = name.to_owned();
    proof.raw_name = name.as_bytes().to_vec();
    proof.safely_decoded = true;
    Ok(proof)
}

fn same_directory_proof(expected: &ProofListEntry, current: &ProofListEntry) -> bool {
    let identity_matches = match (
        expected.entry.device,
        expected.entry.inode,
        current.entry.device,
        current.entry.inode,
    ) {
        (Some(expected_device), Some(expected_inode), Some(device), Some(inode)) => {
            expected_device == device && expected_inode == inode
        }
        _ => matches!(
            (&expected.provider_token, &current.provider_token),
            (Some(expected), Some(current)) if expected == current
        ),
    };
    identity_matches
        && expected.raw_name == current.raw_name
        && expected.entry.is_dir
        && current.entry.is_dir
        && !expected.entry.is_symlink
        && !current.entry.is_symlink
}

fn verify_bound_quarantine_directory(
    vfs: &dyn Vfs,
    bound: &BoundQuarantineDirectory,
) -> io::Result<()> {
    if vfs.root_identity(&bound.path)? != bound.identity {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "Gebundener Quarantäneordner wurde ausgetauscht: {}",
                bound.path.display()
            ),
        ));
    }
    let parent = bound
        .path
        .parent()
        .ok_or_else(|| invalid("Quarantäneordner besitzt keinen Elternpfad"))?;
    let current = listed_directory_proof(vfs, parent, &bound.path, "gebundener Quarantäneordner")?
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Gebundener Quarantäneordner fehlt")
        })?;
    if !same_directory_proof(&bound.listing_proof, &current) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "Gebundener Quarantäneordner änderte seine Identität: {}",
                bound.path.display()
            ),
        ));
    }
    Ok(())
}

fn bind_quarantine_source(
    vfs: &dyn Vfs,
    root: &Path,
    item: &RemoteDeleteItem,
) -> io::Result<ProofListEntry> {
    let source = Path::new(&item.path);
    let expected = item.source_proof.as_listed(source)?;
    let current = listed_source_proof(vfs, root, source)?;
    if RemoteDeleteSourceProof::from_listed(&current) != item.source_proof {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            format!(
                "Remote-Löschquelle wurde unmittelbar vor dem Claim ausgetauscht: {}",
                source.display()
            ),
        ));
    }
    let mut records = Vec::new();
    scan_entry(vfs, source, &current, &mut records)?;
    if records != item.records || digest_records(&records)? != item.digest {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            format!(
                "Remote-Löschbaum änderte sich unmittelbar vor dem Claim: {}",
                source.display()
            ),
        ));
    }
    Ok(expected)
}

fn listed_source_proof(vfs: &dyn Vfs, root: &Path, source: &Path) -> io::Result<ProofListEntry> {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("Remote-Löschquelle besitzt keinen sicheren Namen"))?;
    let listing = vfs.list_complete(root)?;
    if !listing.complete {
        return Err(invalid(
            "Remote-Löschroot ist unmittelbar vor dem Claim unvollständig",
        ));
    }
    listing
        .entries
        .into_iter()
        .find(|entry| entry.safely_decoded && entry.raw_name == name.as_bytes())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Remote-Löschquelle fehlt unmittelbar vor dem Claim",
            )
        })
}

fn verify_parked_item(vfs: &dyn Vfs, target: &Path, item: &RemoteDeleteItem) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| invalid("Geparkter Eintrag besitzt keinen Elternpfad"))?;
    let proof = listed_source_proof(vfs, parent, target)?;
    if !item.source_proof.matches_moved(&proof) {
        return Err(invalid(
            "Geparkter Eintrag stimmt nicht mit der bestätigten Quellidentität überein",
        ));
    }
    let mut records = Vec::new();
    scan_entry(vfs, target, &proof, &mut records)?;
    let source = Path::new(&item.path);
    for record in &mut records {
        let moved = Path::new(&record.path);
        let relative = moved
            .strip_prefix(target)
            .map_err(|_| invalid("Geparkter Readback verlässt seinen Zielpfad"))?;
        record.path = if relative.as_os_str().is_empty() {
            source.to_string_lossy().into_owned()
        } else {
            source.join(relative).to_string_lossy().into_owned()
        };
    }
    if records != item.records || digest_records(&records)? != item.digest {
        return Err(invalid(
            "Geparkter Baum stimmt nicht mit dem bestätigten Löschplan überein",
        ));
    }
    Ok(())
}

// Nur fehlende oder widersprüchliche Belege werden zu Drift. Ein bereits
// klassifizierter Transportfehler oder unklarer Ausgang behält seine Bedeutung.
fn bound_delete_error(error: io::Error, context: &str) -> io::Error {
    if matches!(
        error_code(&error),
        ErrorCode::AlreadyExists
            | ErrorCode::InvalidData
            | ErrorCode::NotFound
            | ErrorCode::PlanDrift
    ) {
        with_recovery_context(
            coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::PlanDrift,
                format!("{context}: {error}"),
            ),
            &[&error],
            &[],
        )
    } else {
        with_context(error, context)
    }
}

fn delete_unclear(path: &Path, action: &str, error: io::Error) -> io::Error {
    with_recovery_context(
        coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!("{action} ({error}). Recoverypfad: {}", path.display()),
        ),
        &[&error],
        &[path],
    )
}

fn after_partial_claim(claimed_items: u64, run: &Path, error: io::Error) -> io::Error {
    if error_code(&error) == ErrorCode::OutcomeUnclear {
        return with_recovery_context(error, &[], &[run]);
    }
    if claimed_items == 0 {
        return error;
    }
    delete_unclear(
        run,
        &format!(
            "Der aktuelle Schritt scheiterte, nachdem bereits {claimed_items} bestätigte Auswahl(en) im Quarantänelauf liegen"
        ),
        error,
    )
}

fn execute_permanent(
    vfs: &dyn Vfs,
    plan: &RemoteDeletePlan,
    cancel: &Cancel,
    progress: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<RemoteDeleteStats> {
    let mut stats = RemoteDeleteStats::default();
    for item in &plan.items {
        for record in item.records.iter().rev() {
            if cancel.is_cancelled() {
                stats.cancelled = true;
                progress.phase = "cancelled".into();
                on_progress(progress);
                return Ok(stats);
            }
            let path = Path::new(&record.path);
            progress.current = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            on_progress(progress);
            let verified_after_reconnect = matches!(
                delete_permanent_record(vfs, record)?,
                PermanentDeleteOutcome::RemovedAfterReconnect
            );
            if !record.is_dir {
                stats.bytes += record.size;
                progress.bytes_done += record.size;
            }
            stats.entries += 1;
            progress.files_done += 1;
            on_progress(progress);
            if verified_after_reconnect && stats.entries < plan.entries {
                // Der Reconnect setzt verhaltensgeprüfte Löschfähigkeiten zurück.
                // Deshalb ist ausschließlich die eben geklärte Mutation belegt;
                // weitere Einträge dieses Laufs bleiben unangetastet und brauchen
                // nach einer neuen Vorschau wieder frische Capability-Proben.
                return Err(coded_error(
                    io::ErrorKind::ConnectionAborted,
                    ErrorCode::ConnectionLost,
                    format!(
                        "Die Löschung von {} wurde nach dem Verbindungsabbruch bestätigt; weitere geplante Einträge wurden nicht mehr verändert",
                        path.display()
                    ),
                ));
            }
        }
        stats.items += 1;
    }
    progress.phase = "done".into();
    progress.current.clear();
    on_progress(progress);
    Ok(stats)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermanentDeleteOutcome {
    Removed,
    RemovedAfterReconnect,
}

/// Prüft und entfernt genau einen bestätigten Record. Der Exakt-einmal-Vertrag
/// sitzt bewusst in dieser kleinen Einheit, damit ein Verbindungsfehler niemals
/// durch eine äußere Rekursions- oder Queue-Schleife zur Wiederholung wird.
fn delete_permanent_record(
    vfs: &dyn Vfs,
    record: &RemoteDeleteRecord,
) -> io::Result<PermanentDeleteOutcome> {
    let path = Path::new(&record.path);
    // Unmittelbar vor der irreversiblen Mutation den konkreten Eintrag erneut
    // gegen den bestätigten Record prüfen: Der frische Replan liegt bei großen
    // Auswahlen bereits spürbar zurück, und ein inzwischen ausgetauschter Eintrag
    // darf nicht anstelle des bestätigten gelöscht werden.
    let current = match vfs.stat(path) {
        Ok(current) => current,
        // Vor der Mutation ist der Ausgang eindeutig: Es wurde noch kein DELETE
        // gesendet. Den Transportcode unverändert durchreichen, damit Oberfläche
        // und CLI nicht fälschlich neu planen.
        Err(error) if error_code(&error) == ErrorCode::ConnectionLost => return Err(error),
        Err(error) => {
            return Err(bound_delete_error(
                error,
                &format!(
                    "Bestätigter Eintrag ist unmittelbar vor dem Löschen nicht mehr prüfbar: {}",
                    path.display()
                ),
            ))
        }
    };
    let matches = match delete_record_matches(vfs, record, &current) {
        Ok(matches) => matches,
        Err(error) if error_code(&error) == ErrorCode::ConnectionLost => return Err(error),
        Err(error) => {
            return Err(bound_delete_error(
                error,
                &format!(
                "Bestätigter Eintrag ist unmittelbar vor dem Löschen nicht vollständig prüfbar: {}",
                path.display()
            ),
            ))
        }
    };
    if !matches {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            format!(
                "Eintrag wurde zwischen Bestätigung und Löschen ausgetauscht; nichts weiter entfernt: {}",
                path.display()
            ),
        ));
    }
    // Der Plan-Token muss bis in dieselbe Providerwirkung reichen, die den Namen
    // entfernt. Ein frisches `stat` gefolgt von pfadbasiertem DELETE schließt einen
    // Austausch in der Zwischenzeit nicht und ist für diesen irreversiblen Pfad
    // deshalb kein Best-Effort-Fallback.
    let mutation = vfs.remove_if_matches(
        path,
        &ProofListEntry {
            entry: current,
            raw_name: path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::as_bytes)
                .map(Vec::from)
                .unwrap_or_default(),
            safely_decoded: path.file_name().and_then(|name| name.to_str()).is_some(),
            provider_token: record.provider_token.clone(),
        },
    );
    match mutation {
        Ok(()) => {
            // Auch eine erfolgreiche Serverantwort beweist die Wirkung nicht. Ein
            // neuer gleichnamiger Eintrag kann hier ebenfalls nicht vom stehen
            // gebliebenen Vorgänger unterschieden werden; beides ist sichtbar
            // unklar und darf nie als erfolgreiche Löschung bilanziert werden.
            match vfs.stat(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    Ok(PermanentDeleteOutcome::Removed)
                }
                Ok(_) => Err(coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Provider meldete erfolgreiches Löschen, aber der Pfad blieb im Readback sichtbar: {}",
                        path.display()
                    ),
                )),
                Err(error) => Err(delete_unclear(
                    path,
                    "Provider meldete erfolgreiches Löschen, aber der Abwesenheits-Readback scheiterte",
                    error,
                )),
            }
            .map_err(|error| with_recovery_context(error, &[], &[path]))
        }
        Err(error) if error_code(&error) == ErrorCode::ConnectionLost => {
            // Genau ein identitätsgeprüfter Neuaufbau und genau ein lesender
            // Zustandsbeleg. Der Löschbefehl wird niemals wiederholt.
            verify_delete_after_connection_loss(vfs, record, error)?;
            Ok(PermanentDeleteOutcome::RemovedAfterReconnect)
        }
        Err(error) => Err(error),
    }
}

fn delete_record_matches(
    vfs: &dyn Vfs,
    record: &RemoteDeleteRecord,
    current: &crate::vfs::Entry,
) -> io::Result<bool> {
    if current.is_symlink {
        return Ok(false);
    }
    let type_matches = if record.is_dir {
        current.is_dir && !current.is_file
    } else {
        current.is_file && !current.is_dir
    };
    if !type_matches || (!record.is_dir && current.size != record.size) {
        return Ok(false);
    }
    if record.is_dir {
        return Ok(record.content_digest.is_none());
    }
    let Some(expected_digest) = record.content_digest else {
        // Alte direkte Unit-Test-Records besitzen keinen Hash. Produktive Pläne
        // tragen ihn für jede Datei; `execute_remote_delete` replante sie ohnehin
        // unmittelbar vorher und würde einen fehlenden Wert als Drift erkennen.
        return Ok(true);
    };
    let actual = crate::atomic_file::hash_vfs_file(vfs, Path::new(&record.path))?;
    Ok(*actual.as_bytes() == expected_digest)
}

/// Klärt nach verloren gegangener Serverantwort ausschließlich den Zustand des
/// zuletzt genau einmal gesendeten DELETE/RMD. Die Mutation wird nie wiederholt.
fn verify_delete_after_connection_loss(
    vfs: &dyn Vfs,
    record: &RemoteDeleteRecord,
    lost: io::Error,
) -> io::Result<()> {
    let path = Path::new(&record.path);
    if let Err(reclaim_error) = vfs.reclaim_verification_session() {
        return Err(with_recovery_context(
            delete_unclear(path,
                &format!("Verbindung brach während des Löschens ab ({lost}); die Zustandsklärung scheiterte"),
                reclaim_error,
            ),
            &[&lost],
            &[],
        ));
    }
    let result = match vfs.stat(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(current) => match delete_record_matches(vfs, record, &current).and_then(|matches| {
            if !matches { return Ok(false); }
            let Some(expected_token) = &record.provider_token else { return Ok(false); };
            let parent = path.parent().ok_or_else(|| invalid("Löschpfad besitzt keinen Elternordner"))?;
            let name = path.file_name().and_then(|name| name.to_str())
                .ok_or_else(|| invalid("Löschpfad besitzt keinen sicher dekodierten Namen"))?;
            // Gleicher Typ/Inhalt beweist keine Nichtwirkung: DELETE kann gewirkt
            // haben, während am Pfad bereits ein gleichartiger Nachfolger steht.
            let listing = vfs.list_complete(parent)?;
            if !listing.complete {
                return Err(invalid("Identitätsreadback benötigt ein vollständiges Elternlisting"));
            }
            let mut matches = listing.entries.iter().filter(|entry| entry.raw_name == name.as_bytes());
            let Some(found) = matches.next() else { return Ok(false); };
            if matches.next().is_some() { return Err(invalid("Identitätsreadback ist nicht eindeutig")); }
            Ok(found.safely_decoded && found.provider_token.as_ref() == Some(expected_token)
                && !found.entry.is_symlink
                && found.entry.is_dir == record.is_dir
                && found.entry.is_file != record.is_dir
                && (record.is_dir || found.entry.size == record.size))
        }) {
            Ok(true) => Err(coded_error(
                io::ErrorKind::ConnectionAborted,
                ErrorCode::ConnectionLost,
                format!(
                    "Verbindung brach ab, bevor die Löschung wirkte; der frische Readback bestätigt den unveränderten Eintrag ({lost}): {}",
                    path.display()
                ),
            )),
            Ok(false) => Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Verbindung brach während des Löschens ab ({lost}); die Identität des Eintrags am Pfad ist danach nicht bestätigt: {}",
                    path.display()
                ),
            )),
            Err(readback_error) => Err(delete_unclear(
                path,
                &format!("Verbindung brach während des Löschens ab ({lost}); der Inhaltsbeleg scheiterte"),
                readback_error,
            )),
        },
        Err(readback_error) => Err(delete_unclear(
            path,
            &format!("Verbindung brach während des Löschens ab ({lost}); der klärende Readback scheiterte"),
            readback_error,
        )),
    };
    result.map_err(|error| with_recovery_context(error, &[&lost], &[path]))
}

fn validate_absolute(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(format!(
            "Remote-Löschpfad ist nicht absolut normalisiert: {}",
            path.display()
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{CompleteDirectoryListing, ConditionalContentReplaceRequest, Entry};
    use crate::{error_code, list_quarantine_runs, LocalVfs};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Lokales, vollständig serialisiertes Testdouble für den provideratomaren
    /// Quarantäne-Claim. Die Namespace-Sperre steht hier für eine echte
    /// Servertransaktion; die einzelnen Angriffe laufen innerhalb genau dieser
    /// Wirkung und treffen damit deterministisch die sonst sehr kleinen Rennen.
    struct BoundQuarantineRaceVfs {
        inner: LocalVfs,
        namespace: Mutex<()>,
        claims: AtomicUsize,
        cas_calls: AtomicUsize,
        fail_cas_sync_on_call: AtomicUsize,
        attack_source_claim: AtomicUsize,
        attack_from_parent_claim: AtomicUsize,
        attack_to_parent_claim: AtomicUsize,
        attack_after_claim: AtomicUsize,
        attack_manifest_after_claim: AtomicUsize,
        attack_manifest_claim: AtomicUsize,
        attack_created_run: AtomicUsize,
        fail_plan_context_on: AtomicUsize,
        fail_mutation_context_on: AtomicUsize,
        displaced: Mutex<Vec<PathBuf>>,
    }

    impl BoundQuarantineRaceVfs {
        fn new() -> Self {
            Self {
                inner: LocalVfs::new(),
                namespace: Mutex::new(()),
                claims: AtomicUsize::new(0),
                cas_calls: AtomicUsize::new(0),
                fail_cas_sync_on_call: AtomicUsize::new(0),
                attack_source_claim: AtomicUsize::new(0),
                attack_from_parent_claim: AtomicUsize::new(0),
                attack_to_parent_claim: AtomicUsize::new(0),
                attack_after_claim: AtomicUsize::new(0),
                attack_manifest_after_claim: AtomicUsize::new(0),
                attack_manifest_claim: AtomicUsize::new(0),
                attack_created_run: AtomicUsize::new(0),
                fail_plan_context_on: AtomicUsize::new(0),
                fail_mutation_context_on: AtomicUsize::new(0),
                displaced: Mutex::new(Vec::new()),
            }
        }

        fn attack_source_on(&self, claim: usize) {
            self.attack_source_claim.store(claim, Ordering::SeqCst);
        }

        fn fail_cas_sync_on(&self, call: usize) {
            self.fail_cas_sync_on_call.store(call, Ordering::SeqCst);
        }

        fn attack_from_parent_on(&self, claim: usize) {
            self.attack_from_parent_claim.store(claim, Ordering::SeqCst);
        }

        fn attack_to_parent_on(&self, claim: usize) {
            self.attack_to_parent_claim.store(claim, Ordering::SeqCst);
        }

        fn attack_after_claim_on(&self, claim: usize) {
            self.attack_after_claim.store(claim, Ordering::SeqCst);
        }

        fn attack_manifest_after_claim_on(&self, claim: usize) {
            self.attack_manifest_after_claim
                .store(claim, Ordering::SeqCst);
        }

        fn attack_manifest_on(&self, claim: usize) {
            self.attack_manifest_claim.store(claim, Ordering::SeqCst);
        }

        fn attack_run_creation(&self) {
            self.attack_created_run.store(1, Ordering::SeqCst);
        }

        fn displaced_paths(&self) -> Vec<PathBuf> {
            self.displaced.lock().unwrap().clone()
        }

        fn displace(&self, path: &Path, replacement_file: Option<&[u8]>) {
            let ordinal = self.displaced.lock().unwrap().len();
            let displaced = path.with_extension(format!("doppeldecker-bestaetigt-{ordinal}"));
            std::fs::rename(path, &displaced).unwrap();
            if displaced.is_dir() {
                std::fs::create_dir(path).unwrap();
            } else {
                std::fs::write(path, replacement_file.unwrap_or(b"gleicher-inhalt")).unwrap();
            }
            self.displaced.lock().unwrap().push(displaced);
        }

        fn listed_proof(&self, path: &Path) -> io::Result<ProofListEntry> {
            let parent = path
                .parent()
                .ok_or_else(|| invalid("Testpfad ohne Elternordner"))?;
            let name = path.file_name().and_then(|name| name.to_str()).unwrap();
            let listing = self.inner.list_complete(parent)?;
            let mut proof = listing
                .entries
                .into_iter()
                .find(|entry| entry.raw_name == name.as_bytes())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Testbeleg fehlt"))?;
            proof.provider_token = proof.entry.inode.map(|inode| inode.to_le_bytes().to_vec());
            Ok(proof)
        }

        fn proof_matches(&self, path: &Path, expected: &ProofListEntry) -> bool {
            self.listed_proof(path).is_ok_and(|current| {
                current.provider_token == expected.provider_token
                    && current.entry.device == expected.entry.device
                    && current.entry.inode == expected.entry.inode
                    && current.entry.is_dir == expected.entry.is_dir
                    && current.entry.is_file == expected.entry.is_file
            })
        }
    }

    impl Vfs for BoundQuarantineRaceVfs {
        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
            let fault = self.fail_plan_context_on.load(Ordering::SeqCst);
            if fault == 2 || (fault == 3 && path.ends_with("unterordner")) {
                return Err(crate::error::with_recovery_paths(
                    coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::PlanDrift,
                        "gezielter Listingdrift",
                    ),
                    &[path.to_string_lossy().into_owned()],
                ));
            }
            let _namespace = self.namespace.lock().unwrap();
            let mut listing = self.inner.list_complete(path)?;
            for entry in &mut listing.entries {
                entry.provider_token = entry.entry.inode.map(|inode| inode.to_le_bytes().to_vec());
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
            self.inner
                .create_dir(path)
                .map_err(MutationFailure::Unchanged)?;
            let proof = self
                .listed_proof(path)
                .map_err(MutationFailure::OutcomeUnclear)?;
            if path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some(QUARANTINE_NAME)
                && self.attack_created_run.swap(0, Ordering::SeqCst) == 1
            {
                self.displace(path, None);
            }
            Ok(Some(proof))
        }

        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            self.inner.open_read(path)
        }

        fn open_write(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.inner.open_write(path)
        }

        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.inner.open_write_new(path)
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }

        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_dir(path)
        }

        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }

        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
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
            if !self.proof_matches(path, expected) {
                return Err(coded_error(
                    io::ErrorKind::AlreadyExists,
                    ErrorCode::PlanDrift,
                    "Bedingter Test-DELETE traf nicht mehr den bestätigten Eintrag",
                ));
            }
            if expected.entry.is_dir {
                self.inner.remove_dir(path)
            } else {
                self.inner.remove_file(path)
            }
        }

        fn supports_remove_if_matches(&self) -> bool {
            true
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
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
            let claim = self.claims.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_mutation_context_on.load(Ordering::SeqCst) == 2 {
                return Err(crate::error::with_recovery_paths(
                    coded_error(
                        io::ErrorKind::AlreadyExists,
                        ErrorCode::PlanDrift,
                        "gezielter Claimdrift",
                    ),
                    &[from.to_string_lossy().into_owned()],
                ));
            }
            if self.attack_source_claim.load(Ordering::SeqCst) == claim {
                let bytes = std::fs::read(from).unwrap();
                self.displace(from, Some(&bytes));
            }
            if self.attack_from_parent_claim.load(Ordering::SeqCst) == claim {
                self.displace(from_parent, None);
            }
            if self.attack_to_parent_claim.load(Ordering::SeqCst) == claim {
                self.displace(to_parent, None);
            }
            if self.attack_manifest_claim.load(Ordering::SeqCst) == claim {
                let required = required_content.expect("Manifestangriff benötigt Inhaltsbeleg");
                let mut manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(required.path).unwrap()).unwrap();
                manifest["records"] = serde_json::json!([]);
                std::fs::write(required.path, serde_json::to_vec_pretty(&manifest).unwrap())
                    .unwrap();
            }
            if from.parent() != Some(from_parent)
                || to.parent() != Some(to_parent)
                || !self.proof_matches(from, expected)
                || self.inner.root_identity(from_parent)? != *expected_from_parent
                || self.inner.root_identity(to_parent)? != *expected_to_parent
                || match required_content {
                    Some(required) => std::fs::read(required.path)
                        .map(|bytes| *blake3::hash(&bytes).as_bytes() != required.digest)
                        .unwrap_or(true),
                    None => false,
                }
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Quellobjekt oder einer der Elternordner wurde ausgetauscht",
                ));
            }
            self.inner.rename_noreplace(from, to)?;
            if self.attack_manifest_after_claim.load(Ordering::SeqCst) == claim {
                let required = required_content.expect("Manifestangriff benötigt Inhaltsbeleg");
                let mut manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(required.path).unwrap()).unwrap();
                manifest["records"] = serde_json::json!([]);
                std::fs::write(required.path, serde_json::to_vec_pretty(&manifest).unwrap())
                    .unwrap();
            }
            if self.attack_after_claim.load(Ordering::SeqCst) == claim {
                self.displace(to_parent, None);
            }
            Ok(())
        }

        fn supports_rename_noreplace_if_matches_in_bound_parents(&self) -> bool {
            true
        }

        fn replace_file_if_content_matches(
            &self,
            request: ConditionalContentReplaceRequest<'_>,
        ) -> Result<(), MutationFailure> {
            let _namespace = self.namespace.lock().unwrap();
            let call = self.cas_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_mutation_context_on.load(Ordering::SeqCst) == 1 {
                return Err(MutationFailure::Unchanged(
                    crate::error::with_recovery_paths(
                        coded_error(
                            io::ErrorKind::Other,
                            ErrorCode::ConnectionLost,
                            "Verbindung vor Manifestwechsel verloren",
                        ),
                        &[request.path.to_string_lossy().into_owned()],
                    ),
                ));
            }
            let current = std::fs::read(request.path).map_err(MutationFailure::Unchanged)?;
            if *blake3::hash(&current).as_bytes() != request.expected_digest {
                return Err(MutationFailure::Unchanged(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Manifestdigest änderte sich vor dem Testaustausch",
                )));
            }
            std::fs::write(request.path, request.replacement)
                .map_err(MutationFailure::OutcomeUnclear)?;
            if self.fail_cas_sync_on_call.load(Ordering::SeqCst) == call {
                return Err(MutationFailure::OutcomeUnclear(io::Error::other(
                    "erzwungener Manifest-CAS-Syncfehler",
                )));
            }
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

        fn supports_create_dir_with_removal_proof(&self) -> bool {
            true
        }

        fn root_identity(&self, path: &Path) -> io::Result<RootIdentity> {
            if self.fail_plan_context_on.load(Ordering::SeqCst) == 1 {
                return Err(crate::error::with_recovery_paths(
                    coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::PlanDrift,
                        "gezielter Rootdrift",
                    ),
                    &[path.to_string_lossy().into_owned()],
                ));
            }
            self.inner.root_identity(path)
        }
    }

    #[test]
    fn loeschplan_kontexte_erhalten_driftcode_und_pruefpfade() {
        let mut lost = Vec::new();
        for fault in 1..=3 {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap().join("remote");
            let child = root.join("unterordner");
            std::fs::create_dir_all(&child).unwrap();
            let sentinel = child.join("behalten.txt");
            std::fs::write(&sentinel, b"unveraendert").unwrap();
            let vfs = BoundQuarantineRaceVfs::new();
            vfs.fail_plan_context_on.store(fault, Ordering::SeqCst);

            let error = plan_remote_delete(&vfs, &root, std::slice::from_ref(&child)).unwrap_err();
            let expected_path = if fault == 3 { &child } else { &root };
            if error_code(&error) != ErrorCode::PlanDrift
                || !crate::error::recovery_paths(&error)
                    .contains(&expected_path.to_string_lossy().into_owned())
            {
                lost.push((
                    fault,
                    error_code(&error),
                    crate::error::recovery_paths(&error),
                ));
            }
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"unveraendert");
            assert!(!root.join(QUARANTINE_NAME).try_exists().unwrap());
            assert_eq!(vfs.claims.load(Ordering::SeqCst), 0);
        }
        assert!(
            lost.is_empty(),
            "Kontext verlor Code oder Prüfpfad: {lost:?}"
        );
    }

    #[test]
    fn claim_und_manifestfehler_erhalten_code_und_pruefpfade() {
        let mut lost = Vec::new();
        for (fault, code) in [(1, ErrorCode::ConnectionLost), (2, ErrorCode::PlanDrift)] {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap().join("remote");
            std::fs::create_dir(&root).unwrap();
            let file = root.join("behalten.txt");
            std::fs::write(&file, b"unveraendert").unwrap();
            let vfs = BoundQuarantineRaceVfs::new();
            let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
            vfs.fail_mutation_context_on.store(fault, Ordering::SeqCst);

            let error = execute_remote_delete(
                &vfs,
                &plan,
                RemoteDeleteMode::Quarantine,
                &Cancel::new(),
                &mut |_| {},
            )
            .unwrap_err();

            let paths = crate::error::recovery_paths(&error);
            if error_code(&error) != code
                || paths.is_empty()
                || (fault == 2 && !paths.contains(&file.to_string_lossy().into_owned()))
            {
                lost.push((fault, error_code(&error), paths.clone()));
            }
            assert!(paths
                .iter()
                .all(|path| Path::new(path).try_exists().unwrap()));
            assert_eq!(std::fs::read(&file).unwrap(), b"unveraendert");
        }
        assert!(
            lost.is_empty(),
            "Claimkontext verlor Code oder Prüfpfad: {lost:?}"
        );
    }

    /// Payload-Vertrag Richtung Frontend: `RemoteDeleteMode` serialisiert wegen
    /// `rename_all = "snake_case"` als `"quarantine"`/`"permanent"`. Die Queue-UI
    /// vergleicht exakt diese Strings — ein PascalCase-Vergleich ließ
    /// wiederherstellbare Quarantäneläufe als „Dauerhaft löschen" erscheinen.
    #[test]
    fn remote_delete_mode_serialisiert_snake_case_fuer_das_frontend() {
        assert_eq!(
            serde_json::to_value(RemoteDeleteMode::Quarantine).unwrap(),
            serde_json::json!("quarantine")
        );
        assert_eq!(
            serde_json::to_value(RemoteDeleteMode::Permanent).unwrap(),
            serde_json::json!("permanent")
        );
    }

    /// Skriptbarer Provider für die Grenze direkt vor und nach genau einem
    /// permanenten DELETE. Die Mutationszahl belegt, dass ein verlorener
    /// Serverantwortpfad niemals blind wiederholt wird.
    struct ScriptedDeleteVfs {
        stats: Mutex<Vec<io::Result<Entry>>>,
        mutations: Mutex<Vec<io::Result<()>>>,
        conditional_tokens: Mutex<Option<Vec<Option<Vec<u8>>>>>,
        mutations_executed: AtomicUsize,
        reclaims: AtomicUsize,
        reclaim_result: Mutex<Option<io::Result<()>>>,
        readback_listing: Mutex<Option<io::Result<crate::vfs::CompleteDirectoryListing>>>,
    }

    impl ScriptedDeleteVfs {
        fn new(stats: Vec<io::Result<Entry>>, mutations: Vec<io::Result<()>>) -> Self {
            Self {
                stats: Mutex::new(stats),
                mutations: Mutex::new(mutations),
                conditional_tokens: Mutex::new(None),
                mutations_executed: AtomicUsize::new(0),
                reclaims: AtomicUsize::new(0),
                reclaim_result: Mutex::new(Some(Ok(()))),
                readback_listing: Mutex::new(None),
            }
        }

        fn with_reclaim_error(self, error: io::Error) -> Self {
            *self.reclaim_result.lock().unwrap() = Some(Err(error));
            self
        }

        fn with_readback_token(self, entry: Entry, token: Option<&[u8]>) -> Self {
            *self.readback_listing.lock().unwrap() =
                Some(Ok(crate::vfs::CompleteDirectoryListing {
                    entries: vec![ProofListEntry {
                        raw_name: entry.name.as_bytes().to_vec(),
                        entry,
                        safely_decoded: true,
                        provider_token: token.map(Vec::from),
                    }],
                    complete: true,
                    provider_token: None,
                }));
            self
        }

        fn with_conditional_delete(self) -> Self {
            *self.conditional_tokens.lock().unwrap() = Some(Vec::new());
            self
        }

        fn conditional_tokens(&self) -> Vec<Option<Vec<u8>>> {
            self.conditional_tokens
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default()
        }

        fn mutation_count(&self) -> usize {
            self.mutations_executed.load(Ordering::SeqCst)
        }

        fn reclaim_count(&self) -> usize {
            self.reclaims.load(Ordering::SeqCst)
        }

        fn mutate(&self) -> io::Result<()> {
            self.mutations_executed.fetch_add(1, Ordering::SeqCst);
            self.mutations.lock().unwrap().remove(0)
        }
    }

    impl Vfs for ScriptedDeleteVfs {
        fn list_complete(&self, _path: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
            self.readback_listing
                .lock()
                .unwrap()
                .take()
                .expect("Nur ein lesender Identitätsbeleg")
        }
        fn reclaim_verification_session(&self) -> io::Result<()> {
            self.reclaims.fetch_add(1, Ordering::SeqCst);
            self.reclaim_result
                .lock()
                .unwrap()
                .take()
                .expect("Reconnect darf höchstens einmal laufen")
        }

        fn list(&self, _path: &Path) -> io::Result<Vec<Entry>> {
            unreachable!()
        }

        fn stat(&self, _path: &Path) -> io::Result<Entry> {
            self.stats.lock().unwrap().remove(0)
        }

        fn exists(&self, _path: &Path) -> bool {
            unreachable!()
        }

        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn create_dir(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn open_read(&self, _path: &Path) -> io::Result<Box<dyn std::io::Read + Send>> {
            unreachable!()
        }

        fn open_write(&self, _path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            unreachable!()
        }

        fn open_write_new(&self, _path: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            unreachable!()
        }

        fn sync_file(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn sync_dir(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn set_mtime(&self, _path: &Path, _mtime: i64) -> io::Result<()> {
            unreachable!()
        }

        fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
            unreachable!()
        }

        fn remove_file(&self, _path: &Path) -> io::Result<()> {
            self.mutate()
        }

        fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn remove_dir(&self, _path: &Path) -> io::Result<()> {
            self.mutate()
        }

        fn remove_if_matches(&self, _path: &Path, expected: &ProofListEntry) -> io::Result<()> {
            self.conditional_tokens
                .lock()
                .unwrap()
                .as_mut()
                .expect("bedingtes DELETE wurde nicht aktiviert")
                .push(expected.provider_token.clone());
            self.mutate()
        }

        fn supports_remove_if_matches(&self) -> bool {
            self.conditional_tokens.lock().unwrap().is_some()
        }

        fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn rename_noreplace(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            unreachable!()
        }
    }

    fn regular_entry(size: u64) -> Entry {
        Entry {
            name: "datei".into(),
            is_dir: false,
            is_file: true,
            is_symlink: false,
            size,
            mtime: None,
            created: None,
            owner: None,
            group: None,
            extension: None,
            mode: None,
            device: None,
            inode: None,
            hard_links: None,
        }
    }

    fn delete_record(size: u64) -> RemoteDeleteRecord {
        delete_record_at("/pane/datei", size)
    }

    fn delete_record_at(path: &str, size: u64) -> RemoteDeleteRecord {
        RemoteDeleteRecord {
            path: path.into(),
            is_dir: false,
            size,
            provider_token: None,
            content_digest: None,
        }
    }

    fn connection_lost() -> io::Error {
        io::Error::new(io::ErrorKind::UnexpectedEof, "Serverantwort ging verloren")
    }

    #[test]
    fn verbindungsverlust_vor_delete_bleibt_connection_lost() {
        let vfs = ScriptedDeleteVfs::new(vec![Err(connection_lost())], Vec::new())
            .with_conditional_delete();
        let error = delete_permanent_record(&vfs, &delete_record(3)).unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        assert_eq!(vfs.mutation_count(), 0);
        assert_eq!(vfs.reclaim_count(), 0);
    }

    #[test]
    fn provider_token_reicht_bis_in_das_bedingte_delete() {
        let vfs = ScriptedDeleteVfs::new(
            vec![
                Ok(regular_entry(3)),
                Err(io::Error::new(io::ErrorKind::NotFound, "Pfad fehlt")),
            ],
            vec![Ok(())],
        )
        .with_conditional_delete();
        let mut record = delete_record(3);
        record.provider_token = Some(b"\"plan-etag\"".to_vec());

        let outcome = delete_permanent_record(&vfs, &record).unwrap();

        assert_eq!(outcome, PermanentDeleteOutcome::Removed);
        assert_eq!(vfs.conditional_tokens(), vec![record.provider_token]);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn smb_reconnect_akzeptiert_keine_ausgetauschte_root_file_id() {
        let server = crate::remote::SmbServerIdentity {
            host: "server.example".into(),
            port: 445,
            server_guid: [1; 16],
            share: "daten".into(),
            account_digest: [2; 32],
            canonical_root: "/".into(),
            dialect: 0x0311,
            signing_active: true,
            encryption_active: true,
            signing_algorithm: "AES-CMAC".into(),
            encryption_cipher: "AES-128-GCM".into(),
        };
        let expected = RootIdentity::Smb(crate::remote::SmbRootIdentity {
            server: server.clone(),
            virtual_root: "/pane".into(),
            root_file_id: 41,
            tree_id: 7,
            session_generation: 1,
        });
        let current = RootIdentity::Smb(crate::remote::SmbRootIdentity {
            server,
            virtual_root: "/pane".into(),
            root_file_id: 42,
            tree_id: 8,
            session_generation: 2,
        });

        assert!(!same_root_namespace(&expected, &current));
    }

    #[test]
    fn verlorene_delete_antwort_mit_fehlendem_pfad_belegt_die_wirkung() {
        let vfs = ScriptedDeleteVfs::new(
            vec![
                Ok(regular_entry(3)),
                Err(io::Error::new(io::ErrorKind::NotFound, "Pfad fehlt")),
            ],
            vec![Err(connection_lost())],
        )
        .with_conditional_delete();

        let outcome = delete_permanent_record(&vfs, &delete_record(3)).unwrap();

        assert_eq!(outcome, PermanentDeleteOutcome::RemovedAfterReconnect);
        assert_eq!(vfs.mutation_count(), 1, "DELETE darf nie wiederholt werden");
        assert_eq!(vfs.reclaim_count(), 1);
    }

    #[test]
    fn bestaetigte_wirkung_nach_reconnect_stoppt_vor_weiterem_delete() {
        let identity_root = tempfile::tempdir().unwrap();
        let root_identity = LocalVfs::new().root_identity(identity_root.path()).unwrap();
        let records = vec![
            delete_record_at("/pane/erste", 3),
            delete_record_at("/pane/zweite", 4),
        ];
        let plan = RemoteDeletePlan {
            root: "/pane".into(),
            root_identity,
            items: vec![RemoteDeleteItem {
                path: "/pane/auswahl".into(),
                is_dir: true,
                entries: 2,
                bytes: 7,
                digest: [0; 32],
                source_proof: RemoteDeleteSourceProof::default(),
                records,
            }],
            entries: 2,
            bytes: 7,
            digest: [0; 32],
        };
        // Records laufen rückwärts: Nur „zweite“ wird einmal gesendet und nach
        // dem Reconnect als entfernt belegt. „erste“ darf nie erreicht werden.
        let vfs = ScriptedDeleteVfs::new(
            vec![
                Ok(regular_entry(4)),
                Err(io::Error::new(io::ErrorKind::NotFound, "Pfad fehlt")),
            ],
            vec![Err(connection_lost())],
        )
        .with_conditional_delete();
        let mut progress = Progress {
            files_total: 2,
            bytes_total: 7,
            phase: "delete".into(),
            ..Progress::default()
        };

        let error =
            execute_permanent(&vfs, &plan, &Cancel::new(), &mut progress, &mut |_| {}).unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        assert_eq!(
            vfs.mutation_count(),
            1,
            "kein zweites DELETE nach Reconnect"
        );
        assert_eq!(vfs.reclaim_count(), 1);
        assert_eq!(progress.files_done, 1);
        assert_eq!(progress.bytes_done, 4);
    }

    #[test]
    fn verlorene_delete_antwort_mit_unveraendertem_pfad_bleibt_connection_lost() {
        let vfs = ScriptedDeleteVfs::new(
            vec![Ok(regular_entry(3)), Ok(regular_entry(3))],
            vec![Err(connection_lost())],
        )
        .with_conditional_delete()
        .with_readback_token(regular_entry(3), Some(b"plan-token"));

        let mut record = delete_record(3);
        record.provider_token = Some(b"plan-token".to_vec());
        let error = delete_permanent_record(&vfs, &record).unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        assert_eq!(vfs.mutation_count(), 1, "DELETE darf nie wiederholt werden");
        assert_eq!(vfs.reclaim_count(), 1);
    }

    #[test]
    fn unveraenderter_delete_token_bleibt_trotz_unsicherem_nachbarn_belegt() {
        for directory in [false, true] {
            let mut entry = regular_entry(if directory { 0 } else { 3 });
            entry.is_dir = directory;
            entry.is_file = !directory;
            let vfs = ScriptedDeleteVfs::new(
                vec![Ok(entry.clone()), Ok(entry.clone())],
                vec![Err(connection_lost())],
            )
            .with_conditional_delete()
            .with_readback_token(entry, Some(b"plan-token"));
            {
                let mut listing = vfs.readback_listing.lock().unwrap();
                listing
                    .as_mut()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .entries
                    .push(ProofListEntry {
                        entry: regular_entry(3),
                        raw_name: vec![0xff],
                        safely_decoded: false,
                        provider_token: None,
                    });
            }
            let mut record = delete_record(if directory { 0 } else { 3 });
            record.is_dir = directory;
            record.provider_token = Some(b"plan-token".to_vec());
            let error = delete_permanent_record(&vfs, &record).unwrap_err();
            assert_eq!(error_code(&error), ErrorCode::ConnectionLost, "{error}");
            assert_eq!(vfs.mutation_count(), 1);
            assert_eq!(vfs.reclaim_count(), 1);
        }
    }

    #[test]
    fn unvollstaendiger_mehrdeutiger_oder_gescheiterter_delete_readback_bleibt_unklar() {
        for case in ["unvollständig", "mehrdeutig", "fehlerhaft"] {
            let vfs = ScriptedDeleteVfs::new(
                vec![Ok(regular_entry(3)), Ok(regular_entry(3))],
                vec![Err(crate::error::with_recovery_paths(
                    connection_lost(),
                    &["/synthetic/lost".into()],
                ))],
            )
            .with_conditional_delete()
            .with_readback_token(regular_entry(3), Some(b"plan-token"));
            {
                let mut stored = vfs.readback_listing.lock().unwrap();
                if case == "fehlerhaft" {
                    *stored = Some(Err(crate::error::with_recovery_paths(
                        connection_lost(),
                        &["/synthetic/readback".into()],
                    )));
                } else {
                    let listing = stored.as_mut().unwrap().as_mut().unwrap();
                    if case == "unvollständig" {
                        listing.complete = false;
                    } else {
                        listing.entries.push(listing.entries[0].clone());
                    }
                }
            }
            let mut record = delete_record(3);
            record.provider_token = Some(b"plan-token".to_vec());
            let error = delete_permanent_record(&vfs, &record).unwrap_err();
            assert_eq!(
                error_code(&error),
                ErrorCode::OutcomeUnclear,
                "{case}: {error}"
            );
            assert_eq!(vfs.mutation_count(), 1);
            assert_eq!(vfs.reclaim_count(), 1);
            let paths = crate::error::recovery_paths(&error);
            assert!(
                paths.contains(&"/synthetic/lost".into()),
                "{case}: {paths:?}"
            );
            assert!(paths.contains(&record.path));
            if case == "fehlerhaft" {
                assert!(paths.contains(&"/synthetic/readback".into()));
            }
        }
    }

    #[test]
    fn gleicher_inhalt_mit_anderem_oder_fehlendem_token_bleibt_nach_delete_unklar() {
        for directory in [false, true] {
            for token in [Some(b"nachfolger".as_slice()), None] {
                let mut entry = regular_entry(if directory { 0 } else { 3 });
                entry.is_dir = directory;
                entry.is_file = !directory;
                let vfs = ScriptedDeleteVfs::new(
                    vec![Ok(entry.clone()), Ok(entry.clone())],
                    vec![Err(connection_lost())],
                )
                .with_conditional_delete()
                .with_readback_token(entry, token);
                let mut record = delete_record(if directory { 0 } else { 3 });
                record.is_dir = directory;
                record.provider_token = Some(b"plan-token".to_vec());
                let error = delete_permanent_record(&vfs, &record).unwrap_err();
                assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
                assert_eq!(vfs.mutation_count(), 1);
                assert_eq!(vfs.reclaim_count(), 1);
            }
        }
    }

    #[test]
    fn anderer_eintrag_nach_verlorener_delete_antwort_ist_outcome_unclear() {
        let vfs = ScriptedDeleteVfs::new(
            vec![Ok(regular_entry(3)), Ok(regular_entry(9))],
            vec![Err(connection_lost())],
        )
        .with_conditional_delete();

        let error = delete_permanent_record(&vfs, &delete_record(3)).unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1, "DELETE darf nie wiederholt werden");
        assert_eq!(vfs.reclaim_count(), 1);
    }

    #[test]
    fn gescheiterter_reconnect_nach_delete_ist_outcome_unclear() {
        let vfs = ScriptedDeleteVfs::new(
            vec![Ok(regular_entry(3))],
            vec![Err(crate::error::with_recovery_paths(
                connection_lost(),
                &["/synthetic/lost".into()],
            ))],
        )
        .with_conditional_delete()
        .with_reclaim_error(crate::error::with_recovery_paths(
            io::Error::new(io::ErrorKind::PermissionDenied, "Serveridentität wechselte"),
            &["/synthetic/reclaim".into()],
        ));

        let error = delete_permanent_record(&vfs, &delete_record(3)).unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1, "DELETE darf nie wiederholt werden");
        assert_eq!(vfs.reclaim_count(), 1);
        let paths = crate::error::recovery_paths(&error);
        let selected_path = delete_record(3).path;
        for expected in [
            "/synthetic/lost",
            "/synthetic/reclaim",
            selected_path.as_str(),
        ] {
            assert!(
                paths.contains(&expected.into()),
                "Prüfpfad fehlt: {expected}: {paths:?}"
            );
        }
    }

    #[test]
    fn erfolgreiche_delete_antwort_ohne_abwesenheit_ist_outcome_unclear() {
        let vfs = ScriptedDeleteVfs::new(
            vec![Ok(regular_entry(3)), Ok(regular_entry(3))],
            vec![Ok(())],
        )
        .with_conditional_delete();

        let error = delete_permanent_record(&vfs, &delete_record(3)).unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1);
        assert_eq!(vfs.reclaim_count(), 0);
        assert!(crate::error::recovery_paths(&error).contains(&delete_record(3).path));
    }

    #[test]
    fn quarantine_manifestiert_auswahl_und_bleibt_wiederherstellbar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(root.join("ordner")).unwrap();
        std::fs::write(root.join("ordner/datei"), b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, &[root.join("ordner")]).unwrap();

        let stats = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(stats.items, 1);
        assert_eq!(stats.entries, 2);
        assert!(!root.join("ordner").exists());
        let base = root.join(QUARANTINE_NAME);
        let runs = list_quarantine_runs(&vfs, &base, &None).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].items_available, 1);
        assert_eq!(runs[0].target_root, root.to_string_lossy());
    }

    #[test]
    fn quarantine_claim_parkt_keinen_gleichartigen_nachfolger() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"gleicher-inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.attack_source_on(1);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(&file).unwrap(), b"gleicher-inhalt");
        assert_eq!(
            std::fs::read(&vfs.displaced_paths()[0]).unwrap(),
            b"gleicher-inhalt"
        );
    }

    #[test]
    fn quarantine_claim_bindet_auch_den_quellroot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.attack_from_parent_on(1);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        let displaced_root = &vfs.displaced_paths()[0];
        assert_eq!(
            std::fs::read(displaced_root.join("datei")).unwrap(),
            b"inhalt"
        );
    }

    #[test]
    fn quarantine_claim_bindet_den_laufordner_bis_zur_wirkung() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.attack_to_parent_on(1);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(&file).unwrap(), b"inhalt");
        assert!(vfs.displaced_paths()[0].is_dir());
    }

    #[test]
    fn quarantine_claim_bindet_den_manifest_intent_atomar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.attack_manifest_on(1);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(&file).unwrap(), b"inhalt");
    }

    #[test]
    fn manifest_cas_syncfehler_stoppt_vor_dem_quellclaim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.fail_cas_sync_on(1);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&file).unwrap(), b"inhalt");
        assert_eq!(vfs.cas_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manifest_abschluss_syncfehler_bleibt_recovery_statt_erfolg() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.fail_cas_sync_on(2);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains("Recoverypfad"));
        assert!(!file.exists());
        assert_eq!(vfs.cas_calls.load(Ordering::SeqCst), 2);
        let runs = list_quarantine_runs(&vfs, &root.join(QUARANTINE_NAME), &None).unwrap();
        assert_eq!(runs.len(), 1);
        let run = root.join(QUARANTINE_NAME).join(&runs[0].id);
        assert!(crate::error::recovery_paths(&error).contains(&run.to_string_lossy().into_owned()));
    }

    #[test]
    fn ausgetauschter_lauf_zwischen_create_und_readback_ist_unclear() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.attack_run_creation();

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&file).unwrap(), b"inhalt");
    }

    #[test]
    fn laufswap_nach_claim_bleibt_recovery_statt_erfolg() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"inhalt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        vfs.attack_after_claim_on(1);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(!file.exists());
        assert_eq!(
            std::fs::read(vfs.displaced_paths()[0].join("datei")).unwrap(),
            b"inhalt"
        );
    }

    #[test]
    fn zweiter_claim_drift_bewahrt_den_ersten_recoverybestand() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("a");
        let second = root.join("b");
        std::fs::write(&first, b"eins").unwrap();
        std::fs::write(&second, b"zwei").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, &[first.clone(), second.clone()]).unwrap();
        vfs.attack_source_on(2);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains("Recoverypfad"));
        assert!(!first.exists());
        assert_eq!(std::fs::read(&second).unwrap(), b"zwei");
        let runs = list_quarantine_runs(&vfs, &root.join(QUARANTINE_NAME), &None).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].items_available, 1);
        let run = root.join(QUARANTINE_NAME).join(&runs[0].id);
        assert!(crate::error::recovery_paths(&error).contains(&run.to_string_lossy().into_owned()));
    }

    #[test]
    fn geloeschter_erster_manifest_record_stoppt_den_zweiten_claim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("a");
        let second = root.join("b");
        std::fs::write(&first, b"eins").unwrap();
        std::fs::write(&second, b"zwei").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, &[first.clone(), second.clone()]).unwrap();
        vfs.attack_manifest_after_claim_on(1);

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Quarantine,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains("Recoverypfad"));
        assert!(!first.exists());
        assert_eq!(std::fs::read(&second).unwrap(), b"zwei");
    }

    #[test]
    fn drift_stoppt_vor_der_ersten_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"alt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();
        std::fs::write(&file, b"neuer und anderer Inhalt").unwrap();

        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Permanent,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), crate::ErrorCode::PlanDrift);
        assert!(file.exists());
    }

    #[test]
    fn permanent_entfernt_nur_geplante_blaetter_und_leere_ordner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(root.join("ordner/tief")).unwrap();
        std::fs::write(root.join("ordner/tief/datei"), b"inhalt").unwrap();
        std::fs::write(root.join("daneben"), b"bleibt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, &[root.join("ordner")]).unwrap();

        execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Permanent,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();

        assert!(!root.join("ordner").exists());
        assert_eq!(std::fs::read(root.join("daneben")).unwrap(), b"bleibt");
    }

    #[test]
    fn permanent_stoppt_wenn_eintrag_unmittelbar_vor_der_mutation_ausgetauscht_wurde() {
        // Regression: Der frische Replan allein genügt nicht — zwischen Replan und
        // `remove_file` kann ein bestätigter Name ausgetauscht werden. Der Test
        // tauscht die Datei im Progress-Callback (der unmittelbar vor der
        // Mutation feuert) gegen eine größere aus und erwartet PlanDrift statt
        // Löschung der fremden Datei.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"alt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();

        let mut swapped = false;
        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Permanent,
            &Cancel::new(),
            &mut |progress| {
                if progress.phase == "delete" && progress.current == "datei" && !swapped {
                    swapped = true;
                    std::fs::write(&file, b"fremder neuer Inhalt").unwrap();
                }
            },
        )
        .unwrap_err();

        assert!(
            swapped,
            "Der Austausch muss vor der Mutation stattgefunden haben"
        );
        assert_eq!(error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"fremder neuer Inhalt",
            "Die ausgetauschte Datei darf nicht gelöscht worden sein"
        );
    }

    #[test]
    fn permanent_stoppt_auch_bei_gleich_grossem_inhaltsaustausch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("datei");
        std::fs::write(&file, b"alt").unwrap();
        let vfs = BoundQuarantineRaceVfs::new();
        let plan = plan_remote_delete(&vfs, &root, std::slice::from_ref(&file)).unwrap();

        let mut swapped = false;
        let error = execute_remote_delete(
            &vfs,
            &plan,
            RemoteDeleteMode::Permanent,
            &Cancel::new(),
            &mut |progress| {
                if progress.phase == "delete" && progress.current == "datei" && !swapped {
                    swapped = true;
                    std::fs::write(&file, b"neu").unwrap();
                }
            },
        )
        .expect_err("Ein gleich großer fremder Inhalt darf nicht gelöscht werden");

        assert_eq!(error_code(&error), crate::ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(&file).unwrap(), b"neu");
    }

    #[test]
    fn loeschbilanz_lehnt_u64_ueberlauf_ab() {
        let error = sum_delete_values([u64::MAX, 1].into_iter(), "Bytes des Löschplans")
            .expect_err("Eine Löschbilanz darf nicht umbrechen");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_im_teilbaum_sperrt_den_gesamten_plan() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("remote");
        std::fs::create_dir_all(root.join("ordner")).unwrap();
        symlink(dir.path().canonicalize().unwrap(), root.join("ordner/link")).unwrap();
        let error =
            plan_remote_delete(&LocalVfs::new(), &root, &[root.join("ordner")]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(root.join("ordner/link").exists());
    }
}
