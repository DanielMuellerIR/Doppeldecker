//! Interaktive Datei-Operationen des Dateimanagers: Kopieren, Verschieben, Umbenennen,
//! Ordner anlegen — plus Namenskonflikt-Erkennung.
//!
//! Abgrenzung zum [`crate::sync`]:
//! - **Sync** ist der planbestätigte Massen-Abgleich zweier Bäume. Der normale
//!   AddOnly-/Update-Start hält die Vorbereitung im Backend und bestätigt die
//!   konkrete Wirkung. Simulation bleibt lesend; Mirror verwendet seinen
//!   getrennten Assistenten mit zusätzlichen Belegen und einmaligen Permits.
//! - **Datei-Operationen** hier sind vom Nutzer einzeln ausgelöst. Ein Dateimanager muss
//!   real handeln, wenn man auf „Kopieren" klickt — sonst wäre er nutzlos. Die Sicherheit
//!   kommt darum aus anderen Schranken:
//!     1. **Löschen läuft über den descriptor-gebundenen Papierkorb** im Core.
//!        Diese Datei implementiert Kopieren, Move, Rename und Ordneranlage.
//!     2. **Nie blind überschreiben**: [`find_recursive_conflicts`] meldet die
//!        tatsächlich zu ersetzenden Blätter und Typkonflikte; die UI fragt dann nach
//!        (Überspringen/Ersetzen/Beide behalten). Konfliktkopien erhalten vorher
//!        backendseitig gebundene neue Namen mit atomarem No-Replace-Commit.
//!        Die Liste entsteht aus vollständigen, sicher
//!        dekodierten Listings, nicht aus einer booleschen Existenzabfrage: Ein
//!        Stat- oder Verbindungsfehler darf hier nie als „kein Konflikt“ durchgehen.
//!     3. **Sandbox-Schranke**: Ist eine Sandbox gesetzt, MUSS jedes Schreib-/Lösch-/
//!        Verschiebe-Ziel (real gegen Symlinks aufgelöst) darunter liegen — sonst
//!        Abbruch, bevor etwas passiert. Gleiche Auflösung wie beim Sync.
//!
//! Alles hier arbeitet gegen das [`Vfs`]-Trait und ist damit headless auf macOS UND
//! Linux testbar — genau wie die Sync-Engine.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::atomic_file::{
    copy_stream, hash_vfs_file, rename_noreplace_with_phase, sync_parent, sync_rename_parents,
    SyncedRenameFailure,
};
use crate::error::{coded_error, error_code, with_recovery_context, ErrorCode};
use crate::progress::{Cancel, Progress};
use crate::remote_rename::names_may_collide;
use crate::vfs::{
    Entry, FileCommitClass, FileReplaceClass, LocalVfs, MoveSourceRemovalProof, MutationFailure,
    ProofListEntry, Vfs,
};

/// Wie mit einem bereits im Ziel existierenden gleichnamigen Eintrag umgegangen wird.
///
/// Die Entscheidung trifft der Nutzer in der UI, nachdem [`find_recursive_conflicts`]
/// eine Kollision gemeldet hat. Sie gilt für den GESAMTEN Stapel (v0.1 keine
/// Pro-Datei-Abfrage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverwritePolicy {
    /// Vorhandene gleichnamige Einträge unangetastet lassen (den Quell-Eintrag NICHT kopieren).
    Skip,
    /// Zusammenführen/Überschreiben: Dateien werden überschrieben, Ordner rekursiv
    /// zusammengeführt (vorhandene Extra-Dateien im Ziel bleiben erhalten — es wird also
    /// NICHTS im Ziel gelöscht, nur ergänzt/ersetzt). Nur bei echtem Typkonflikt
    /// (Datei↔Ordner) muss das alte Ziel weichen (Sandbox-geschützt).
    Overwrite,
}

/// Bilanz einer abgeschlossenen Datei-Operation (für die UI-Rückmeldung).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct OpStats {
    /// Einschränkungen der tatsächlich verwendeten interaktiven Inhaltsquelle.
    pub source_read_limitations: Vec<String>,
    /// Ausgelassene Zielmetadaten der tatsächlich kopierten Dateien.
    pub target_write_limitations: Vec<String>,
    pub source_tree: Option<crate::vfs::InteractiveSourceTree>,
    /// Anzahl kopierter/verschobener Dateien (bei Ordnern: die enthaltenen Dateien).
    pub files: u64,
    /// Übertragene Bytes.
    pub bytes: u64,
    /// Wegen Konflikt oder nicht sicher dekodierbarem Quellnamen ausgelassene Einträge.
    pub skipped: u64,
    /// Übersprungene Symlinks (in v0.1 nicht kopiert/verschoben).
    pub symlinks_skipped: u64,
    /// Wurde die Operation abgebrochen?
    pub cancelled: bool,
    /// Mögliche Tempnamen, die mangels atomar mit der Anlage gebundenem
    /// Entfernungsbeleg absichtlich unangetastet blieben. Der Name kann inzwischen
    /// auch einen fremden Eintrag bezeichnen und ist deshalb nur ein sichtbarer
    /// Prüfpfad, niemals eine Löschfreigabe.
    pub temp_recovery_paths: Vec<String>,
    /// Nach bewiesenem Zielcommit bewusst erhaltene Altversionen.
    pub retained_backup_paths: Vec<String>,
    /// Wurde eine Move-Quelle ohne atomaren Providerbeleg, aber nach vollständigem
    /// frischem Readback manifestgebunden Blatt für Blatt entfernt?
    ///
    /// Das Kennzeichen wird erst nach erfolgreichem Abschluss dieses begrenzten
    /// Best-Effort-Pfads gesetzt. Fehler mit Teilwirkung bleiben `OutcomeUnclear`.
    pub source_removal_best_effort: bool,
    /// Mindestens eine tatsächlich neu angelegte Zieldatei wurde über den vorher
    /// bestätigten Best-Effort-Commit sichtbar gemacht.
    pub target_copy_new_best_effort: bool,
    /// Mindestens eine tatsächlich vorhandene Zieldatei wurde über den vorher
    /// bestätigten, nicht wiederherstellbaren Best-Effort-Replace-Pfad ersetzt.
    pub target_replace_best_effort: bool,
}

/// Phasengenaue tatsächliche Best-Effort-Wirkungen eines fehlgeschlagenen Laufs.
/// `None` bezeichnet ausschließlich die Wirkung, deren aktuelle Providerantwort
/// keinen sicheren Zustand mehr erkennen lässt. Früher abgeschlossene Wirkungen
/// bleiben als `Some(true)` erhalten.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FileOperationActuals {
    pub target_write_limitations: Vec<String>,
    pub source_removal_best_effort: Option<bool>,
    pub target_copy_new_best_effort: Option<bool>,
    pub target_replace_best_effort: Option<bool>,
    /// Tempnamen, die auf einem Fehlerpfad mangels weiterhin gültigem
    /// Anlagebeleg absichtlich nicht entfernt wurden. Sie bleiben reine
    /// Prüfpfade und erteilen kein Recht zur späteren pfadbasierten Löschung.
    pub temp_recovery_paths: Vec<String>,
    pub retained_backup_paths: Box<Vec<String>>,
}

impl Default for FileOperationActuals {
    fn default() -> Self {
        Self {
            target_write_limitations: Vec::new(),
            source_removal_best_effort: Some(false),
            target_copy_new_best_effort: Some(false),
            target_replace_best_effort: Some(false),
            temp_recovery_paths: Vec::new(),
            retained_backup_paths: Box::default(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum UnclearBestEffortEffect {
    TargetCopyNew,
    TargetReplace,
}

impl FileOperationActuals {
    /// Führt Teilwirkungen mehrerer Einträge zusammen. Eine schon sicher bekannte
    /// Best-Effort-Wirkung darf durch einen späteren unklaren oder nachweislich
    /// mutationsfreien Eintrag nicht aus der Fehlerbilanz verschwinden.
    fn merge_effect(current: &mut Option<bool>, next: Option<bool>) {
        *current = match (*current, next) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (None, _) | (_, None) => None,
            (Some(false), Some(false)) => Some(false),
        };
    }

    fn record_source_removal(&mut self, next: Option<bool>) {
        Self::merge_effect(&mut self.source_removal_best_effort, next);
    }

    fn record_target_copy_new(&mut self, next: Option<bool>) {
        Self::merge_effect(&mut self.target_copy_new_best_effort, next);
    }

    fn record_target_replace(&mut self, next: Option<bool>) {
        Self::merge_effect(&mut self.target_replace_best_effort, next);
    }

    fn mark_unclear(&mut self, effect: UnclearBestEffortEffect) {
        match effect {
            UnclearBestEffortEffect::TargetCopyNew => self.record_target_copy_new(None),
            UnclearBestEffortEffect::TargetReplace => self.record_target_replace(None),
        }
    }

    fn record_temp_recovery_path(&mut self, path: String) {
        if !self.temp_recovery_paths.contains(&path) {
            self.temp_recovery_paths.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Sandbox-Schranke (identische Semantik wie im Sync)
// ---------------------------------------------------------------------------

/// Prüft, dass `target` — falls eine Sandbox gesetzt ist — real unter deren Wurzel liegt.
/// Ist keine Sandbox gesetzt, ist alles erlaubt (normaler Dateimanager-Betrieb).
///
/// Öffentlich, damit auch die App-Schicht (z. B. der Papierkorb-Befehl, der die `trash`-
/// Crate nutzt) VOR dem Anfassen einer Datei dieselbe Schranke ziehen kann.
pub fn guard(sandbox: &Option<PathBuf>, target: &Path) -> io::Result<()> {
    if let Some(root) = sandbox {
        crate::path::reject_parent_traversal(target, || {
            format!(
                "Sicherheits-Sandbox aktiv: „{}“ enthält „..“ und ist damit nicht eindeutig — abgebrochen, es wurde nichts verändert.",
                target.display()
            )
        })?;
        let canon_root = crate::sync::canonical_existing(root)?;
        let canon_target = crate::sync::canonical_existing(target)?;
        if !canon_target.starts_with(&canon_root) {
            return Err(crate::error::coded_error(
                io::ErrorKind::PermissionDenied,
                crate::error::ErrorCode::SandboxViolation,
                format!(
                    "Sicherheits-Sandbox aktiv: „{}“ liegt nicht unter „{}“ — abgebrochen, es wurde nichts verändert.",
                    canon_target.display(),
                    canon_root.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Capabilityproben schreiben unter dem Pane-Root. Daher gelten lokale
/// Zielroots und bei Move auch lokale Quellroots schon vor der Providerprobe
/// als Schreibbereiche; eine reine lokale Copy-Quelle bleibt lesbar.
pub fn guard_local_transfer_roots(
    source: Option<&Path>,
    target: Option<&Path>,
    moving: bool,
    sandbox: &Option<PathBuf>,
) -> io::Result<()> {
    if let Some(target) = target {
        guard(sandbox, target)?;
    }
    if moving {
        if let Some(source) = source {
            guard(sandbox, source)?;
        }
    }
    Ok(())
}

pub(crate) fn guard_vfs(vfs: &dyn Vfs, sandbox: &Option<PathBuf>, target: &Path) -> io::Result<()> {
    if vfs.is_local() {
        guard(sandbox, target)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Konflikt-Erkennung (Dry-Run vor Kopieren/Verschieben)
// ---------------------------------------------------------------------------

/// Ermittelt providerneutral alle tatsächlich zu ersetzenden Blätter und
/// Typkonflikte. Zwei vorhandene Ordner sind kein Konflikt: sie werden rekursiv
/// zusammengeführt, während nur kollidierende Kinder in der Liste erscheinen.
pub fn find_recursive_conflicts(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    dst_dir: &Path,
    items: &[PathBuf],
) -> io::Result<Vec<String>> {
    let mut conflicts = Vec::new();
    for source in items {
        let target = target_path_in(dst_dir, source)?;
        collect_conflicts(src, source, dst, &target, &mut conflicts)?;
    }
    conflicts.sort();
    conflicts.dedup();
    Ok(conflicts)
}

/// Vom Backend vergebener neuer Top-Level-Name. Bei einem Ordnerkonflikt wird
/// der vollständige ausgewählte Quellbaum daneben erhalten, nicht nur ein Blatt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileKeepBothTarget {
    pub source: String,
    pub target: String,
    pub source_proof: FileConflictProof,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct FileConflictView {
    #[serde(skip)]
    pub source_proof: FileConflictProof,
    #[serde(skip)]
    pub target_proof: FileConflictProof,
    pub relative_name: String,
    pub source: crate::conflict_view::ConflictEntryView,
    pub target: crate::conflict_view::ConflictEntryView,
}

fn conflict_entry_view(
    endpoint: &Path,
    proof: &FileConflictProof,
) -> crate::conflict_view::ConflictEntryView {
    crate::conflict_view::ConflictEntryView {
        endpoint: endpoint.to_string_lossy().into_owned(),
        exists: true,
        kind: Some(
            if proof.is_symlink {
                "symlink"
            } else if proof.is_dir {
                "directory"
            } else if proof.is_file {
                "file"
            } else {
                "special"
            }
            .into(),
        ),
        size: (proof.is_file && !proof.is_symlink).then_some(proof.size),
        mtime: proof.mtime,
    }
}

/// Die Darstellung verwendet strikte Zielbelege und die wirkungsspezifische
/// Quellbindung. Eine schwache Inhaltsquelle wird dadurch nicht zum Planner-VFS.
pub fn file_conflict_views(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    destination: &Path,
    items: &[PathBuf],
    conflicts: &[String],
) -> io::Result<Vec<FileConflictView>> {
    file_conflict_views_inner(src, dst, destination, items, conflicts, false)
}

/// Private Metadatenanzeige für FTPS-Skip. Sie erteilt weder Ersetzung noch Alias-
/// oder Quellentfernung und benötigt deshalb keinen Inhalt ausgelassener Dateien.
pub fn file_conflict_metadata_views(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    destination: &Path,
    items: &[PathBuf],
    conflicts: &[String],
) -> io::Result<Vec<FileConflictView>> {
    file_conflict_views_inner(src, dst, destination, items, conflicts, true)
}

fn file_conflict_views_inner(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    destination: &Path,
    items: &[PathBuf],
    conflicts: &[String],
    metadata_only: bool,
) -> io::Result<Vec<FileConflictView>> {
    let targets = capture_file_proofs(dst, conflicts, !metadata_only)?;
    let mut views = Vec::new();
    for proof in targets {
        let relative = Path::new(&proof.path)
            .strip_prefix(destination)
            .map_err(io::Error::other)?;
        let source = items
            .iter()
            .find_map(|item| {
                let root = destination.join(item.file_name()?);
                Path::new(&proof.path)
                    .strip_prefix(&root)
                    .ok()
                    .map(|tail| item.join(tail))
            })
            .ok_or_else(|| invalid_transfer("Konflikt liegt außerhalb der Auswahl"))?;
        let source_proof = if metadata_only {
            capture_file_metadata_proofs(src, &[path_to_utf8(&source)?])?
        } else {
            capture_file_source_proofs(src, &[path_to_utf8(&source)?])?
        }
        .remove(0);
        let source_root = source
            .ancestors()
            .nth(relative.components().count())
            .unwrap_or(&source);
        let parts = relative
            .components()
            .map(|part| part.as_os_str().as_encoded_bytes().to_vec())
            .collect::<Vec<_>>();
        views.push(FileConflictView {
            source_proof: source_proof.clone(),
            target_proof: proof.clone(),
            relative_name: crate::conflict_view::display_relative_name(&parts),
            source: conflict_entry_view(source_root, &source_proof),
            target: conflict_entry_view(destination, &proof),
        });
    }
    Ok(views)
}

fn invalid_transfer(message: &str) -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}

/// Prüft auch persistierte Aliasdaten, bevor ein Executor VFS oder Secrets nutzt.
pub fn validate_keep_both_targets(
    items: &[PathBuf],
    destination: &Path,
    targets: &[FileKeepBothTarget],
) -> io::Result<()> {
    let mut names = Vec::new();
    for target in targets {
        let source = Path::new(&target.source);
        let path = Path::new(&target.target);
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| invalid_transfer("Konfliktkopie benötigt einen sicheren Namen"))?;
        validate_file_name(name)?;
        if !items.iter().any(|item| item == source)
            || path.parent() != Some(destination)
            || target.source_proof.path != target.source
            || names
                .iter()
                .any(|other: &&str| names_may_collide(other, name))
            || items.iter().any(|item| {
                item.file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|old| names_may_collide(old, name))
            })
            || targets
                .iter()
                .filter(|other| other.source == target.source)
                .count()
                != 1
        {
            return Err(invalid_transfer(
                "Konfliktkopie verlässt den Zielordner oder kollidiert mit der Auswahl",
            ));
        }
        names.push(name);
    }
    Ok(())
}

/// Vor jeder Wirkung alle vergebenen Namen und Quellbelege frisch prüfen. Eine
/// späte Kollision verlangt neue Planung; sie wird niemals zum Merge/Overwrite.
pub fn verify_keep_both_targets(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    items: &[PathBuf],
    destination: &Path,
    targets: &[FileKeepBothTarget],
) -> io::Result<()> {
    validate_keep_both_targets(items, destination, targets)?;
    if targets.is_empty() {
        return Ok(());
    }
    if dst.file_commit_class_at(destination)? != FileCommitClass::Assured {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Beide behalten benötigt hier einen atomaren kollisionsfreien Commit",
        ));
    }
    let listing = dst.list_complete(destination)?;
    require_complete_safe_listing(&listing, "Konfliktkopie-Ziel")?;
    let sources = capture_file_source_proofs(
        src,
        &targets
            .iter()
            .map(|target| target.source.clone())
            .collect::<Vec<_>>(),
    )?;
    for target in targets {
        let name = Path::new(&target.target)
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| invalid_transfer("Konfliktname fehlt"))?;
        if listing
            .entries
            .iter()
            .any(|entry| names_may_collide(&entry.entry.name, name))
            || !sources.contains(&target.source_proof)
        {
            return Err(coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift, format!("Der bestätigte Konfliktkopie-Name ist inzwischen belegt oder die Quelle geändert: {}. Bitte neu planen.", target.target)));
        }
    }
    Ok(())
}

pub fn prepare_keep_both_targets(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    destination: &Path,
    items: &[PathBuf],
    conflicts: &[String],
) -> io::Result<Vec<FileKeepBothTarget>> {
    if conflicts.is_empty() || dst.file_commit_class_at(destination)? != FileCommitClass::Assured {
        return Ok(Vec::new());
    }
    let listing = dst.list_complete(destination)?;
    require_complete_safe_listing(&listing, "Konfliktkopie-Namensvergabe")?;
    let mut names = listing
        .entries
        .iter()
        .map(|entry| entry.entry.name.clone())
        .collect::<Vec<_>>();
    names.extend(
        items
            .iter()
            .filter_map(|item| item.file_name()?.to_str().map(str::to_owned)),
    );
    let mut targets = Vec::new();
    for source in items {
        let original_target = target_path_in(destination, source)?;
        if !conflicts
            .iter()
            .any(|path| Path::new(path).starts_with(&original_target))
        {
            continue;
        }
        let source_text = path_to_utf8(source)?;
        let proof = capture_file_source_proofs(src, std::slice::from_ref(&source_text))?.remove(0);
        if proof.is_symlink || (!proof.is_dir && !proof.is_file) {
            return Ok(Vec::new());
        }
        if proof.is_dir {
            // Eine Konfliktkopie verspricht den vollständigen Baum. Die normale
            // additive Auslassungsbilanz darf diese Variante nicht freischalten.
            match scan_totals(src, std::slice::from_ref(source), false) {
                Ok(_) => {}
                Err(error) if error_code(&error) == ErrorCode::InvalidData => return Ok(Vec::new()),
                Err(error) => return Err(error),
            }
        }
        let original = crate::BinaryRelativePath(vec![source
            .file_name()
            .ok_or_else(|| invalid_transfer("Quellname fehlt"))?
            .as_encoded_bytes()
            .to_vec()]);
        let mut generated = None;
        for _ in 0..32 {
            let candidate = crate::bidirectional::generate_file_copy_path(&original)?;
            let name = String::from_utf8(candidate.0[0].clone()).map_err(io::Error::other)?;
            if names
                .iter()
                .all(|existing| !names_may_collide(existing, &name))
            {
                names.push(name.clone());
                generated = Some(name);
                break;
            }
        }
        let name =
            generated.ok_or_else(|| invalid_transfer("Kein freier Konfliktname gefunden"))?;
        targets.push(FileKeepBothTarget {
            source: source_text,
            target: path_to_utf8(&destination.join(name))?,
            source_proof: proof,
        });
    }
    verify_keep_both_targets(src, dst, items, destination, &targets)?;
    Ok(targets)
}

fn transfer_target(
    destination: &Path,
    source: &Path,
    targets: &[FileKeepBothTarget],
) -> io::Result<PathBuf> {
    Ok(
        match targets
            .iter()
            .find(|target| Path::new(&target.source) == source)
        {
            Some(target) => PathBuf::from(&target.target),
            None => target_path_in(destination, source)?,
        },
    )
}

/// Persistierbarer Beleg genau eines sichtbar bestätigten Konfliktziels. Pfad und
/// Metadaten allein reichen nicht: Provider-Token binden eine serverseitige Version,
/// der BLAKE3-Hash schützt auch Protokolle ohne solches Token gegen einen typ- und
/// größengleichen Austausch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileConflictProof {
    pub path: String,
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
    pub content_hash: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_inheritance: Option<crate::metadata::LocalParentInheritance>,
}

impl FileConflictProof {
    pub(crate) fn from_listed(
        path: &Path,
        listed: &ProofListEntry,
        content_hash: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        Self::from_entry(
            path,
            &listed.entry,
            &listed.raw_name,
            listed.provider_token.clone(),
            content_hash,
        )
    }

    fn from_entry(
        path: &Path,
        entry: &Entry,
        raw_name: &[u8],
        provider_token: Option<Vec<u8>>,
        content_hash: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        Ok(Self {
            path: path_to_utf8(path)?,
            raw_name: raw_name.to_vec(),
            provider_token,
            is_dir: entry.is_dir,
            is_file: entry.is_file,
            is_symlink: entry.is_symlink,
            size: entry.size,
            mtime: entry.mtime,
            mode: entry.mode,
            device: entry.device,
            inode: entry.inode,
            content_hash,
            parent_inheritance: None,
        })
    }

    fn matches_listed(&self, listed: &ProofListEntry) -> bool {
        self.raw_name == listed.raw_name
            && self.provider_token == listed.provider_token
            && self.is_dir == listed.entry.is_dir
            && self.is_file == listed.entry.is_file
            && self.is_symlink == listed.entry.is_symlink
            && self.size == listed.entry.size
            && self.mtime == listed.entry.mtime
            && self.mode == listed.entry.mode
            && self.device == listed.entry.device
            && self.inode == listed.entry.inode
    }
}

/// Bindet die aktuelle Identität aller bestätigten Konfliktpfade. Jedes Elternlisting
/// muss vollständig und sicher dekodierbar sein; reguläre Dateien werden zusätzlich
/// vollständig gehasht, bevor die Bestätigung in die Queue gelangt.
pub fn capture_file_conflict_proofs(
    dst: &dyn Vfs,
    conflicts: &[String],
) -> io::Result<Vec<FileConflictProof>> {
    capture_file_proofs(dst, conflicts, true)
}

/// Ausschließlich Metadaten; diese privaten Vorschauwerte sind keine Inhaltsbelege.
pub fn capture_file_metadata_proofs(
    vfs: &dyn Vfs,
    paths: &[String],
) -> io::Result<Vec<FileConflictProof>> {
    capture_file_proofs(vfs, paths, false)
}

fn capture_file_proofs(
    dst: &dyn Vfs,
    conflicts: &[String],
    read_content: bool,
) -> io::Result<Vec<FileConflictProof>> {
    let mut proofs = Vec::with_capacity(conflicts.len());
    for conflict in conflicts {
        let path = Path::new(conflict);
        let parent = path.parent().ok_or_else(|| {
            coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Konfliktpfad besitzt keinen Elternordner",
            )
        })?;
        let name = file_name(path).ok_or_else(|| {
            coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Konfliktname ist nicht sicher dekodierbar",
            )
        })?;
        let listing = dst.list_complete(parent)?;
        require_complete_safe_listing(&listing, "Konfliktbindung")?;
        let listed = unique_named_proof(&listing, &name, None)?;
        let content_hash = if read_content && listed.entry.is_file && !listed.entry.is_symlink {
            Some(*hash_vfs_file(dst, path)?.as_bytes())
        } else {
            None
        };
        let mut proof = FileConflictProof::from_listed(path, listed, content_hash)?;
        proof.parent_inheritance = match dst.local_parent_inheritance(parent) {
            Ok(proof) => proof,
            Err(error) if error.kind() == io::ErrorKind::Unsupported => None,
            Err(error) => return Err(error),
        };
        proofs.push(proof);
    }
    proofs.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(proofs)
}

/// Bindet nur Quellversionen. Eine begrenzte Inhaltsklasse liefert reguläre
/// Inhalte oder einen beobachteten Additionsbaum samt Kinderhashes, ausdrücklich
/// kein striktes Elternlisting und kein erfundenes Servertoken.
/// Native Ordner binden sichere Kinderinhalte und den vollständigen Rohbestand
/// zusätzlich zum strikten Elternbeleg. Unsichere Kinder werden niemals geöffnet.
/// Zielkonflikte und Ersetzungen verwenden weiterhin den strikten Listingvertrag.
pub fn capture_file_source_proofs(
    src: &dyn Vfs,
    paths: &[String],
) -> io::Result<Vec<FileConflictProof>> {
    let mut proofs = Vec::with_capacity(paths.len());
    for path in paths {
        let path = Path::new(path);
        if let Some(entry) = src.interactive_regular_file_source(path)? {
            if !entry.is_file
                || entry.is_dir
                || entry.is_symlink
                || Some(entry.name.as_str()) != path.file_name().and_then(|name| name.to_str())
            {
                return Err(invalid_transfer(
                    "Interaktiver Quellbeleg ist keine eindeutig benannte reguläre Datei",
                ));
            }
            let hash = *hash_vfs_file(src, path)?.as_bytes();
            // Der Inhaltsreader prüft EOF und Handle; der letzte Stat erhält auch
            // einen Generationstausch unmittelbar nach dem Readback als Fehler.
            let final_entry = src
                .interactive_regular_file_source(path)?
                .ok_or_else(|| invalid_transfer("Interaktiver Quellbeleg ging verloren"))?;
            let proof = FileConflictProof::from_entry(
                path,
                &entry,
                entry.name.as_bytes(),
                None,
                Some(hash),
            )?;
            if FileConflictProof::from_entry(
                path,
                &final_entry,
                final_entry.name.as_bytes(),
                None,
                Some(hash),
            )? != proof
            {
                return Err(coded_error(
                    io::ErrorKind::Interrupted,
                    ErrorCode::PlanDrift,
                    "Interaktive Quelle änderte sich bei der Konfliktbindung",
                ));
            }
            proofs.push(proof);
        } else if src.interactive_copy_children(path)?.is_some() {
            proofs.push(capture_interactive_directory_proof(src, path)?);
        } else {
            let mut current = capture_file_conflict_proofs(src, &[path_to_utf8(path)?])?;
            for proof in &mut current {
                if proof.is_dir && !proof.is_symlink {
                    let hash = capture_native_directory_content_hash(src, path)?;
                    let final_proof =
                        capture_file_metadata_proofs(src, std::slice::from_ref(&proof.path))?
                            .remove(0);
                    if final_proof != *proof {
                        return Err(coded_error(
                            io::ErrorKind::Interrupted,
                            ErrorCode::PlanDrift,
                            "Quellordner änderte sich während seiner Inhaltsbindung",
                        ));
                    }
                    proof.content_hash = Some(hash);
                }
            }
            proofs.extend(current);
        }
    }
    proofs.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(proofs)
}

// Der strikte Elternbeleg des ausgewählten Ordners bleibt separat erhalten.
// Kinder werden je Verzeichnis gemeinsam gebunden; ein eigenes Elternlisting
// je Datei würde große lokale Ordner quadratisch statt linear durchlaufen.
fn capture_native_directory_content_hash(src: &dyn Vfs, path: &Path) -> io::Result<[u8; 32]> {
    let mut listing = src.list_complete(path)?;
    require_complete_source_listing(&listing, "Quellordner-Inhaltsbindung", true)?;
    listing.entries.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));
    let mut children = Vec::with_capacity(listing.entries.len());
    for child in listing.entries.iter().filter(|child| child.safely_decoded) {
        let child_path = path.join(&child.entry.name);
        let hash = if child.entry.is_symlink {
            None
        } else if child.entry.is_dir {
            Some(capture_native_directory_content_hash(src, &child_path)?)
        } else if child.entry.is_file {
            Some(*hash_vfs_file(src, &child_path)?.as_bytes())
        } else {
            None
        };
        children.push(FileConflictProof::from_listed(&child_path, child, hash)?);
    }
    let mut final_listing = src.list_complete(path)?;
    require_complete_source_listing(&final_listing, "Quellordner nach Inhaltsbindung", true)?;
    final_listing
        .entries
        .sort_by(|a, b| a.raw_name.cmp(&b.raw_name));
    let before = listing
        .entries
        .iter()
        .map(|item| {
            (
                &item.raw_name,
                &item.entry,
                &item.provider_token,
                item.safely_decoded,
            )
        })
        .collect::<Vec<_>>();
    let after = final_listing
        .entries
        .iter()
        .map(|item| {
            (
                &item.raw_name,
                &item.entry,
                &item.provider_token,
                item.safely_decoded,
            )
        })
        .collect::<Vec<_>>();
    if serde_json::to_vec(&before).map_err(io::Error::other)?
        != serde_json::to_vec(&after).map_err(io::Error::other)?
    {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::PlanDrift,
            "Quellordnerbestand änderte sich während seiner Inhaltsbindung",
        ));
    }
    let encoded = if listing.entries.iter().any(|child| !child.safely_decoded) {
        // Die ausgelassenen Bytes und Metadaten bleiben Teil der Versionsbindung.
        // Eine Umbenennung darf nicht unbemerkt aus einer Auslassung eine neue
        // kopierbare Datei machen. Bestehende vollständige Belege bleiben stabil.
        serde_json::to_vec(&("native-source-tree-omissions-v1", children, before))
    } else {
        serde_json::to_vec(&("native-source-tree-v1", children))
    }
    .map_err(io::Error::other)?;
    Ok(*blake3::hash(&encoded).as_bytes())
}

fn capture_interactive_directory_proof(
    src: &dyn Vfs,
    path: &Path,
) -> io::Result<FileConflictProof> {
    let entry = src.stat(path)?;
    if !entry.is_dir || entry.is_symlink {
        return Err(invalid_transfer(
            "Interaktiver Baumbeleg benötigt einen echten Ordner",
        ));
    }
    let children = copy_source_children(src, path, "Interaktive Quellversion")?;
    let paths = children
        .iter()
        .map(|child| path_to_utf8(&path.join(&child.entry.name)))
        .collect::<io::Result<Vec<_>>>()?;
    let children = capture_file_source_proofs(src, &paths)?;
    let namespace = src.interactive_source_tree()?.map(|tree| tree.digest);
    // Der beobachtete Namespace-Digest ist kein vom Server ausgestelltes Token.
    // Er gehört zusammen mit allen Kinderinhalten in den Quellversionshash.
    let hash =
        *blake3::hash(&serde_json::to_vec(&(namespace, &children)).map_err(io::Error::other)?)
            .as_bytes();
    let proof =
        FileConflictProof::from_entry(path, &entry, entry.name.as_bytes(), None, Some(hash))?;
    let final_entry = src.stat(path)?;
    copy_source_children(src, path, "Interaktive Quellversion nach Inhaltsbindung")?;
    if FileConflictProof::from_entry(
        path,
        &final_entry,
        final_entry.name.as_bytes(),
        None,
        Some(hash),
    )? != proof
    {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::PlanDrift,
            "Interaktiver Ordner änderte sich während seiner Inhaltsbindung",
        ));
    }
    Ok(proof)
}

pub fn verify_file_source_proofs(src: &dyn Vfs, expected: &[FileConflictProof]) -> io::Result<()> {
    let paths = expected
        .iter()
        .map(|proof| proof.path.clone())
        .collect::<Vec<_>>();
    let current = capture_file_source_proofs(src, &paths)?;
    compare_file_proofs(&current, expected)
}

/// Prüft eine angezeigte Konfliktversion erneut. Ein weggefallener Eintrag ist
/// Drift; Verbindungs- und andere Lesefehler bleiben als solche erhalten.
pub fn verify_file_conflict_proofs(
    vfs: &dyn Vfs,
    expected: &[FileConflictProof],
) -> io::Result<()> {
    let paths = expected
        .iter()
        .map(|proof| proof.path.clone())
        .collect::<Vec<_>>();
    let current = capture_file_conflict_proofs(vfs, &paths).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift,
                format!("Ein angezeigter Konflikteintrag ist nicht mehr vorhanden ({error}); bitte neu planen"))
        } else { error }
    })?;
    compare_file_proofs(&current, expected)
}

/// Nur für eine ausdrücklich passive/Skip-Wirkung. Erwartete Inhaltsbelege
/// werden hier nicht still geschwächt; der Aufrufer muss die Metadatenwahl binden.
pub fn verify_file_metadata_proofs(
    vfs: &dyn Vfs,
    expected: &[FileConflictProof],
) -> io::Result<()> {
    if expected.iter().any(|proof| proof.content_hash.is_some()) {
        return Err(invalid_transfer(
            "Metadatenprüfung akzeptiert keinen Inhaltsbeleg",
        ));
    }
    let paths = expected
        .iter()
        .map(|proof| proof.path.clone())
        .collect::<Vec<_>>();
    let current =
        capture_file_metadata_proofs(vfs, &paths).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift,
                "Ein angezeigter Metadateneintrag ist nicht mehr vorhanden; bitte neu vorbereiten")
            } else {
                error
            }
        })?;
    compare_file_proofs(&current, expected)
}

fn compare_file_proofs(
    current: &[FileConflictProof],
    expected: &[FileConflictProof],
) -> io::Result<()> {
    let current = current
        .iter()
        .map(|proof| (proof.path.as_str(), proof))
        .collect::<std::collections::BTreeMap<_, _>>();
    if expected
        .iter()
        .any(|proof| current.get(proof.path.as_str()).copied() != Some(proof))
    {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::PlanDrift,
            "Quelle oder Ziel änderte sich seit dem Konfliktdialog; bitte neu planen",
        ));
    }
    Ok(())
}

/// Prüft die enge Wirkungsklasse für Provider ohne Quarantäne: Zusammenführen von
/// Ordnern bleibt erlaubt. Eine reguläre Quelldatei darf eine reguläre Zieldatei
/// oder den bestätigten Symlink als Verzeichniseintrag ersetzen; der Link wird dabei
/// nie geöffnet. Ordner bleiben von der unwiederherstellbaren Einzelwirkung ausgenommen.
pub fn validate_best_effort_file_replacements(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    dst_dir: &Path,
    items: &[PathBuf],
) -> io::Result<()> {
    for source in items {
        let target = target_path_in(dst_dir, source)?;
        validate_best_effort_replacement(src, source, dst, &target)?;
    }
    Ok(())
}

fn validate_best_effort_replacement(
    src: &dyn Vfs,
    source: &Path,
    dst: &dyn Vfs,
    target: &Path,
) -> io::Result<()> {
    let source_entry = src.stat(source)?;
    let target_entry = match dst.stat(target) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if source_entry.is_dir
        && !source_entry.is_symlink
        && target_entry.is_dir
        && !target_entry.is_symlink
    {
        let listing = src.list_complete(source)?;
        require_complete_safe_listing(&listing, "Best-Effort-Ersetzungsprüfung")?;
        for child in listing.entries {
            validate_best_effort_replacement(
                src,
                &source.join(&child.entry.name),
                dst,
                &target.join(&child.entry.name),
            )?;
        }
        return Ok(());
    }
    if source_entry.is_file
        && !source_entry.is_symlink
        && (target_entry.is_file || target_entry.is_symlink)
    {
        return Ok(());
    }
    Err(coded_error(
        io::ErrorKind::Unsupported,
        ErrorCode::Unsupported,
        format!(
            "Best-Effort-Ersetzen ohne Quarantäne ist am Konflikt „{}“ ausschließlich für eine reguläre Quelldatei und eine reguläre Zieldatei oder einen bestätigten Ziel-Symlink erlaubt",
            target.display()
        ),
    ))
}

fn collect_conflicts(
    src: &dyn Vfs,
    source: &Path,
    dst: &dyn Vfs,
    target: &Path,
    conflicts: &mut Vec<String>,
) -> io::Result<()> {
    let source_entry = src.stat(source)?;
    if source_entry.is_symlink || (!source_entry.is_file && !source_entry.is_dir) {
        return Ok(());
    }
    let target_entry = match dst.stat(target) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if source_entry.is_dir && target_entry.is_dir && !target_entry.is_symlink {
        let listing = copy_source_listing(src, source, "Rekursive Konfliktprüfung", true)?;
        for child in listing.entries {
            collect_conflicts(
                src,
                &source.join(&child.entry.name),
                dst,
                &target.join(&child.entry.name),
                conflicts,
            )?;
        }
    } else {
        conflicts.push(path_to_utf8(target)?);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Kopieren
// ---------------------------------------------------------------------------

/// Vollständiger, rein geliehener Kontext einer interaktiven Dateioperation.
///
/// Quelle, Ziel, Auswahl und Sandbox bleiben dadurch als eine geprüfte Einheit
/// sichtbar; Abbruch und Fortschritt sind weiterhin kurzlebige Laufzeitparameter.
pub struct FileOperationRequest<'a> {
    pub src: &'a dyn Vfs,
    pub dst: &'a dyn Vfs,
    pub items: &'a [PathBuf],
    pub dst_dir: &'a Path,
    pub policy: OverwritePolicy,
    pub sandbox: &'a Option<PathBuf>,
}

/// Private Bindungen des Queueauftrags; Skipnamen stammen aus dem frisch
/// verglichenen Konfliktstand vor jeder Capability-Probe.
#[derive(Clone, Copy, Default)]
pub(crate) struct FileTransferBindings<'a> {
    pub targets: &'a [FileKeepBothTarget],
    pub replacements: Option<&'a [FileConflictProof]>,
    pub skipped_targets: Option<&'a [String]>,
}

/// Kopiert die (absoluten) `items` in den Zielordner `dst_dir`.
///
/// `policy` steuert das Verhalten bei Namenskollisionen (siehe [`OverwritePolicy`]).
/// `sandbox` ist die optionale Schranke; `cancel` erlaubt sauberen Abbruch; Fortschritt
/// wird über `on_progress` gemeldet. Quelle und Ziel können verschiedene [`Vfs`] sein
/// (z. B. lokal → Remote) — kopiert wird generisch über den VFS-Vertrag.
pub fn copy_items(
    request: FileOperationRequest<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<OpStats> {
    copy_items_inner(
        request,
        FileTransferBindings::default(),
        cancel,
        on_progress,
        &mut FileOperationActuals::default(),
    )
}

#[cfg(test)]
fn copy_items_with_conflict_proofs(
    request: FileOperationRequest<'_>,
    expected_replacements: &[FileConflictProof],
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<OpStats> {
    copy_items_with_conflict_proofs_tracked(
        request,
        expected_replacements,
        cancel,
        on_progress,
        &mut FileOperationActuals::default(),
    )
}

#[cfg(test)]
pub(crate) fn copy_items_with_conflict_proofs_tracked(
    request: FileOperationRequest<'_>,
    expected_replacements: &[FileConflictProof],
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<OpStats> {
    copy_items_inner(
        request,
        FileTransferBindings {
            replacements: Some(expected_replacements),
            ..FileTransferBindings::default()
        },
        cancel,
        on_progress,
        actuals,
    )
}

pub(crate) fn copy_items_with_targets_tracked(
    request: FileOperationRequest<'_>,
    bindings: FileTransferBindings<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<OpStats> {
    copy_items_inner(
        request,
        bindings,
        cancel,
        on_progress,
        actuals,
    ).map_err(|error| {
        if !bindings.targets.is_empty() && error.kind() == io::ErrorKind::AlreadyExists {
            with_recovery_context(coded_error(io::ErrorKind::AlreadyExists, ErrorCode::PlanDrift,
                format!("Ein bestätigter Konfliktkopie-Name wurde inzwischen belegt ({error}); bitte neu planen")), &[&error], &[])
        } else { error }
    })
}

fn copy_items_inner(
    request: FileOperationRequest<'_>,
    bindings: FileTransferBindings<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<OpStats> {
    if request.dst.additive_content_target_only() && request.policy == OverwritePolicy::Overwrite {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Diese interaktive Zielsicht erlaubt nur additive Inhalte",
        ));
    }
    if request.src.interactive_additive_only()
        && (!request.dst.is_local() || request.policy == OverwritePolicy::Overwrite)
    {
        return Err(coded_error(io::ErrorKind::Unsupported, ErrorCode::Unsupported,
            "Die begrenzte SFTP-Inhaltsquelle erlaubt ausschließlich additive lokale Kopien; Remote-Ziele, Ersetzen und Verschieben bleiben gesperrt"));
    }
    let FileTransferBindings {
        targets,
        replacements: expected_replacements,
        skipped_targets: bound_skipped_targets,
    } = bindings;
    let FileOperationRequest {
        src,
        dst,
        items,
        dst_dir,
        policy,
        sandbox,
    } = request;
    validate_transfer_targets(src, dst, items, dst_dir)?;
    verify_keep_both_targets(src, dst, items, dst_dir, targets)?;
    guard_transfer_scope(src, dst, items, dst_dir, sandbox, false)?;
    let source_tree = src.interactive_source_tree()?;
    let mut stats = OpStats {
        skipped: source_tree
            .as_ref()
            .map_or(0, |tree| tree.omitted.len() as u64),
        source_tree,
        ..OpStats::default()
    };

    // Fortschritts-Summen vorab ermitteln (rekursiv über alle Items), damit die UI einen
    // sinnvollen Balken zeigen kann. Symlinks zählen nicht mit (werden übersprungen).
    let allow_name_omissions = policy == OverwritePolicy::Skip && targets.is_empty();
    let (files_total, bytes_total) = scan_totals(src, items, allow_name_omissions)?;
    let mut prog = Progress {
        files_total,
        bytes_total,
        phase: "copy".into(),
        ..Default::default()
    };

    for src_abs in items {
        if cancel.is_cancelled() {
            return Ok(finish_cancelled(stats, &mut prog, on_progress));
        }
        if stats.source_tree.as_ref().is_some_and(|tree| {
            tree.omitted
                .iter()
                .any(|omission| Path::new(&omission.path) == src_abs)
        }) {
            continue;
        }
        let Some(_name) = file_name(src_abs) else {
            continue;
        };
        let dst_abs = transfer_target(dst_dir, src_abs, targets)?;
        let exclusive = targets
            .iter()
            .any(|target| Path::new(&target.source) == src_abs);
        if exclusive && optional_stat(dst, &dst_abs)?.is_some() {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::PlanDrift,
                "Konfliktkopie inzwischen belegt; bitte neu planen",
            ));
        }

        let skipped_targets = if policy == OverwritePolicy::Skip && !exclusive {
            match bound_skipped_targets {
                Some(paths) => paths
                    .iter()
                    .filter(|path| Path::new(path).starts_with(&dst_abs))
                    .cloned()
                    .collect(),
                None => find_recursive_conflicts(src, dst, dst_dir, std::slice::from_ref(src_abs))?,
            }
        } else {
            Vec::new()
        };

        // Konflikt-Politik auf der obersten Ebene anwenden.
        if policy == OverwritePolicy::Skip && optional_stat(dst, &dst_abs)?.is_some() {
            let source = src.stat(src_abs)?;
            let target = dst.stat(&dst_abs)?;
            if !source.is_dir || !target.is_dir || source.is_symlink || target.is_symlink {
                stats.skipped += 1;
                continue;
            }
        }

        copy_tree(
            &CopyTreeRequest {
                src,
                dst,
                sandbox,
                cancel,
                policy,
                expected_replacements,
                exclusive_root: exclusive.then_some(dst_abs.as_path()),
                skipped_targets: &skipped_targets,
                allow_name_omissions,
            },
            src_abs,
            &dst_abs,
            &mut stats,
            &mut prog,
            on_progress,
            actuals,
        )?;
        if cancel.is_cancelled() {
            return Ok(finish_cancelled(stats, &mut prog, on_progress));
        }
    }

    prog.phase = "done".into();
    prog.current.clear();
    on_progress(&prog);
    Ok(stats)
}

/// Kopiert rekursiv einen einzelnen Quell-Eintrag (`src_abs`) an die Zielstelle (`dst_abs`).
/// Bei Ordnern werden vorhandene Ziel-Ordner ZUSAMMENGEFÜHRT (nichts wird gelöscht, nur
/// ergänzt/überschrieben). Symlinks werden übersprungen.
#[derive(Clone, Copy)]
struct CopyTreeRequest<'a> {
    src: &'a dyn Vfs,
    dst: &'a dyn Vfs,
    sandbox: &'a Option<PathBuf>,
    cancel: &'a Cancel,
    policy: OverwritePolicy,
    /// `None` ist der ältere direkte API-Weg. `Some`, auch leer, bedeutet: Nur exakt
    /// diese zuvor bestätigten Zielidentitäten dürfen ersetzt werden.
    expected_replacements: Option<&'a [FileConflictProof]>,
    exclusive_root: Option<&'a Path>,
    skipped_targets: &'a [String],
    /// Nur normale additive Copy-Aufträge dürfen unsichere Quellnamen auslassen.
    /// Move und vollständige Konfliktkopien behalten den strikten Baumvertrag.
    allow_name_omissions: bool,
}

fn copy_tree(
    request: &CopyTreeRequest<'_>,
    src_abs: &Path,
    dst_abs: &Path,
    stats: &mut OpStats,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<()> {
    let CopyTreeRequest {
        src,
        dst,
        sandbox,
        cancel,
        policy,
        expected_replacements,
        exclusive_root,
        skipped_targets,
        allow_name_omissions,
    } = *request;
    // Auch jedes Kind des neu vergebenen Ordners muss neu entstehen. Ein später
    // fremd angelegtes Kind darf weder übersprungen noch zusammengeführt werden.
    let exclusive_destination = exclusive_root.is_some_and(|root| dst_abs.starts_with(root));
    let meta = src.stat(src_abs)?;
    if dst.additive_content_target_only() && ((!meta.is_file && !meta.is_dir) || meta.is_symlink) {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Diese interaktive Zielsicht erlaubt nur reguläre Dateien und Ordner",
        ));
    }

    // Symlinks überspringen (v0.1, konsistent mit dem Sync — vermeidet Endlosschleifen).
    if meta.is_symlink || (!meta.is_file && !meta.is_dir) {
        stats.symlinks_skipped += 1;
        return Ok(());
    }

    if policy == OverwritePolicy::Skip
        && skipped_targets
            .iter()
            .any(|path| Path::new(path) == dst_abs)
    {
        if optional_stat(dst, dst_abs)?.is_none() {
            return Err(coded_error(
                io::ErrorKind::Interrupted,
                ErrorCode::PlanDrift,
                "Ein zum Überspringen gebundenes Ziel verschwand; kein neuer Kopierauftrag",
            ));
        }
        stats.skipped += 1;
        return Ok(());
    }

    if let Some(expected) = expected_replacements
        .and_then(|proofs| {
            proofs
                .iter()
                .find(|proof| Path::new(&proof.path) == dst_abs)
        })
        .and_then(|proof| proof.parent_inheritance.as_ref())
    {
        if dst_abs.parent() != Some(expected.identity.display_path.as_path())
            || dst
                .local_parent_inheritance(&expected.identity.display_path)?
                .as_ref()
                != Some(expected)
        {
            return Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::PlanDrift,
                "Lokale Ziel-Vererbung änderte sich seit der Konfliktbestätigung",
            ));
        }
    }
    if let Some(parent) = dst_abs.parent() {
        guard_vfs(dst, sandbox, parent)?;
        dst.create_dir_all(parent)?;
    }
    if exclusive_destination && optional_stat(dst, dst_abs)?.is_some() {
        return Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::PlanDrift,
            "Konfliktkopie inzwischen belegt; bitte neu planen",
        ));
    }
    if policy == OverwritePolicy::Skip {
        if let Some(target) = optional_stat(dst, dst_abs)? {
            if !meta.is_dir || !target.is_dir || target.is_symlink {
                stats.skipped += 1;
                return Ok(());
            }
        }
    }
    let staging_scope = dst
        .staging_scope(dst_abs, sandbox.as_deref())
        .inspect_err(|error| {
            for path in crate::error::recovery_paths(error) {
                actuals.record_temp_recovery_path(path);
            }
        })?;
    let dst = staging_scope.as_deref().unwrap_or(dst);
    let scoped_request = CopyTreeRequest { dst, ..*request };
    let request = &scoped_request;

    let result = (|| -> io::Result<()> {
        if meta.is_dir {
            // Ausgelassene Rohnamen bleiben reine Bilanzwerte. Ausschließlich
            // sichere Namen dürfen zu Quell- und Zielpfaden werden.
            let source_listing = copy_source_listing(
                src,
                src_abs,
                "Rekursives Quelllisting",
                allow_name_omissions,
            )?;
            for raw in &source_listing.omitted_names {
                stats.skipped = stats.skipped.saturating_add(1);
                stats.source_read_limitations.push(format!(
                    "Quellordner „{}“: nicht sicher dekodierbarer Kindname ausgelassen (Rohname hex: {})",
                    src_abs.display(), raw.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
                ));
            }
            // Schreib-Ziel absichern, dann Ordner (ggf.) anlegen und Inhalt rekursiv kopieren.
            guard_vfs(dst, sandbox, dst_abs)?;
            // Ein Typkonflikt wird nicht vorab gelöscht. Wir parken ihn atomar, bauen den
            // neuen Ordner und können bei Fehler/Abbruch den alten Eintrag zurückholen.
            let (parked, verified_parked, created_destination, created_proof) = match dst
                .stat(dst_abs)
            {
                Ok(_) if exclusive_destination => {
                    return Err(coded_error(
                        io::ErrorKind::AlreadyExists,
                        ErrorCode::PlanDrift,
                        "Konfliktkopie inzwischen belegt; bitte neu planen",
                    ))
                }
                Ok(target) if target.is_dir && !target.is_symlink => (None, None, false, None),
                Ok(_) => {
                    // Typkonflikt: Am Zielnamen liegt kein gewöhnlicher Ordner, sondern
                    // eine Datei, ein Symlink oder eine Spezialdatei. Das ist ein echter
                    // Blattkonflikt, und `Skip` heißt „vorhandenes Ziel unangetastet
                    // lassen". Ohne diese Abfrage würde `Skip` den Eintrag trotzdem
                    // parken und ersetzen — und `move_items` löschte anschließend die
                    // Quelle, obwohl der Nutzer „Überspringen" gewählt hat.
                    if policy == OverwritePolicy::Skip {
                        stats.skipped += 1;
                        return Ok(());
                    }
                    // Zuerst muss das Ziel überhaupt als Ersetzung bestätigt sein —
                    // diese Prüfung berührt das Dateisystem noch nicht.
                    let expected = expected_replacement_for(expected_replacements, dst_abs)?;
                    // Danach wird es atomar in einen exklusiv reservierten
                    // Sicherungsnamen verschoben und ERST DORT auf seine Identität
                    // geprüft. Andersherum bliebe zwischen Prüfung und Verschieben ein
                    // Rennen: Ein paralleler Lauf könnte das Ziel austauschen, und die
                    // Sicherung würde am Ende gelöscht, ohne je geprüft worden zu sein.
                    let parked = park_existing_target(dst, dst_abs, sandbox)?;
                    let verified_parked = match expected {
                        Some(expected) => {
                            match verify_parked_replacement(dst, &parked.path, dst_abs, expected) {
                                Ok(verified) => Some(verified),
                                Err(drift) => {
                                    return Err(restore_parked_target(
                                        dst,
                                        &parked.path,
                                        dst_abs,
                                        parked.removal_proof.as_ref(),
                                        drift,
                                    ));
                                }
                            }
                        }
                        None => parked.removal_proof.clone(),
                    };
                    let best_effort_create =
                        target_file_commit_class(dst, dst_abs)? == FileCommitClass::BestEffort;
                    let created_proof = match dst.create_dir_with_removal_proof(dst_abs) {
                        Ok(proof) => {
                            if best_effort_create {
                                // Die Anlage ist sichtbar, aber erst der folgende
                                // Eltern-Sync bestätigt ihren endgültigen Ausgang.
                                actuals.record_target_copy_new(None);
                            }
                            proof
                        }
                        Err(MutationFailure::Unchanged(create_error)) => {
                            if best_effort_create {
                                actuals.record_target_copy_new(Some(false));
                            }
                            return Err(restore_parked_target(
                                dst,
                                &parked.path,
                                dst_abs,
                                verified_parked.as_ref(),
                                create_error,
                            ));
                        }
                        Err(MutationFailure::OutcomeUnclear(create_error)) => {
                            if best_effort_create {
                                actuals.record_target_copy_new(None);
                            }
                            let failure = coded_error(
                                io::ErrorKind::Other,
                                ErrorCode::OutcomeUnclear,
                                format!(
                                    "Das Anlegen des Zielordners meldete erst nach möglicher Wirkung einen Fehler ({create_error}). Kein pfadbasierter Rückweg wurde versucht. Recoverypfade: möglicher neuer Zielordner: {}; geparktes Altziel: {}",
                                    dst_abs.display(),
                                    parked.path.display(),
                                ),
                            );
                            return Err(with_recovery_context(
                                failure,
                                &[&create_error],
                                &[dst_abs, &parked.path],
                            ));
                        }
                    };
                    (Some(parked.path), verified_parked, true, created_proof)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // `create_dir` ist absichtlich atomar: Entsteht der Name nach dem stat,
                    // brechen wir ab, statt den fremden Ordner stillschweigend zu befüllen.
                    let best_effort_create =
                        target_file_commit_class(dst, dst_abs)? == FileCommitClass::BestEffort;
                    let created_proof = match dst.create_dir_with_removal_proof(dst_abs) {
                        Ok(proof) => {
                            if best_effort_create {
                                actuals.record_target_copy_new(None);
                            }
                            proof
                        }
                        Err(MutationFailure::Unchanged(error)) => {
                            if best_effort_create {
                                actuals.record_target_copy_new(Some(false));
                            }
                            return Err(error);
                        }
                        Err(MutationFailure::OutcomeUnclear(error)) => {
                            if best_effort_create {
                                actuals.record_target_copy_new(None);
                            }
                            let failure = coded_error(
                                io::ErrorKind::Other,
                                ErrorCode::OutcomeUnclear,
                                format!(
                                    "Das Anlegen des Zielordners meldete erst nach möglicher Wirkung einen Fehler ({error}). Der sichtbare Name blieb unangetastet. Recoverypfad: möglicher neuer Zielordner: {}",
                                    dst_abs.display(),
                                ),
                            );
                            return Err(with_recovery_context(failure, &[&error], &[dst_abs]));
                        }
                    };
                    (None, None, true, created_proof)
                }
                Err(error) => return Err(error),
            };

            if created_destination {
                if let Err(sync_error) = sync_parent(dst, dst_abs) {
                    return rollback_created_target(
                    dst,
                    dst_abs,
                    parked.as_deref(),
                    verified_parked.as_ref(),
                    created_proof.as_ref(),
                    Some(coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Der neue Zielordner {} wurde sichtbar angelegt, aber sein Elternordner konnte nicht bestätigt werden ({sync_error}). Recoverypfade: unveränderte Quelle: {}; möglicher Zielordner: {}",
                            dst_abs.display(),
                            src_abs.display(),
                            dst_abs.display(),
                        ),
                    )),
                );
                }
                if target_file_commit_class(dst, dst_abs)? == FileCommitClass::BestEffort {
                    actuals.record_target_copy_new(Some(true));
                }
            }

            let copy_result = (|| -> io::Result<()> {
                for child in source_listing.entries {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    copy_tree(
                        request,
                        &src_abs.join(&child.entry.name),
                        &dst_abs.join(&child.entry.name),
                        stats,
                        prog,
                        on_progress,
                        actuals,
                    )?;
                }
                if !cancel.is_cancelled() {
                    // Der letzte Datei-EOF schützt seinen Inhalt. Ein danach
                    // geänderter Ordnerbestand braucht einen eigenen Schlusscheck.
                    src.interactive_copy_children(src_abs)?;
                }
                Ok(())
            })();

            if copy_result.is_err() || cancel.is_cancelled() {
                if created_destination {
                    // Der Zielname kann seit dem letzten Zugriff ausgetauscht worden sein.
                    // Der Rückweg darf deshalb nur einen schon bei der Anlage atomar
                    // gebundenen Eintrag entfernen. Fehlt diese Provider-Garantie, meldet
                    // er die bekannten Recoverypfade und lässt beide Einträge stehen.
                    return rollback_created_target(
                        dst,
                        dst_abs,
                        parked.as_deref(),
                        verified_parked.as_ref(),
                        created_proof.as_ref(),
                        copy_result.err(),
                    );
                }
                return copy_result.map_err(|error| {
                    if src.interactive_additive_only() || dst.additive_content_target_only() {
                        with_recovery_context(coded_error(error.kind(), error_code(&error),
                            format!("{error}. Bereits ergänzte Dateien können im bestehenden Zielteilbaum erhalten sein; prüfen: {}", dst_abs.display())),
                            &[&error], &[dst_abs])
                    } else { error }
                });
            }
            copy_result?;

            // Rechte erst NACH dem rekursiven Füllen anwenden. Ein Quellordner mit 0555
            // würde sonst seine eigenen noch ausstehenden Schreibzugriffe blockieren.
            if created_destination {
                let metadata_result = (|| -> io::Result<()> {
                    if dst.supports_mtime_write() {
                        if let Some(mtime) = meta.mtime {
                            dst.set_mtime(dst_abs, mtime)?;
                        }
                    }
                    if dst.supports_mode_write_at(dst_abs)? {
                        if let Some(mode) = meta.mode {
                            dst.set_mode(dst_abs, mode)?;
                        }
                    }
                    if dst.has_private_staging() {
                        let actual = dst.stat(dst_abs)?;
                        if (dst.supports_mode_write_at(dst_abs)?
                            && meta.mode.is_some_and(|mode| actual.mode != Some(mode)))
                            || (dst.supports_mtime_write()
                                && meta.mtime.is_some_and(|mtime| actual.mtime != Some(mtime)))
                        {
                            return Err(io::Error::other(
                                "Ordner-Metadaten stimmen im Readback nicht überein",
                            ));
                        }
                    }
                    // Die Kinder sind bereits einzeln bestätigt. Dieser letzte Sync
                    // bindet die abschließenden Ordner-Metadaten vor einem möglichen
                    // Quellabbau an den sichtbaren Zielordner.
                    dst.sync_dir(dst_abs)
                })();
                if let Err(metadata_error) = metadata_result {
                    return rollback_created_target(
                        dst,
                        dst_abs,
                        parked.as_deref(),
                        verified_parked.as_ref(),
                        created_proof.as_ref(),
                        Some(coded_error(
                            io::ErrorKind::Other,
                            ErrorCode::OutcomeUnclear,
                            format!(
                                "Die abschließenden Metadaten oder der Verzeichnis-Sync des Zielordners {} konnten nicht bestätigt werden ({metadata_error}). Recoverypfade: unveränderte Quelle: {}; möglicher Zielordner: {}",
                                dst_abs.display(),
                                src_abs.display(),
                                dst_abs.display(),
                            ),
                        )),
                    );
                }
                // Die Provider-Vorschau koppelt neue Dateien und neue Verzeichnisse an
                // dieselbe Commitklasse. Erst nachdem Anlage, Metadaten und Dir-Sync
                // vollständig bestätigt sind, erscheint diese tatsächliche schwächere
                // Zielwirkung in der Ergebnisbilanz.
                stats.target_copy_new_best_effort |=
                    target_file_commit_class(dst, dst_abs)? == FileCommitClass::BestEffort;
            }

            let limitations = dst.interactive_write_limitations();
            stats
                .target_write_limitations
                .extend(limitations.iter().cloned());
            actuals.target_write_limitations.extend(limitations);
            if let Some(backup) = parked {
                if let Err(error) = remove_bound_backup(dst, &backup, verified_parked.as_ref()) {
                    actuals.record_temp_recovery_path(backup.to_string_lossy().into_owned());
                    if verified_parked.as_ref().is_some_and(|proof| {
                        dst.staged_backup_is_retained(&backup, proof)
                            .unwrap_or(false)
                    }) {
                        actuals
                            .retained_backup_paths
                            .push(backup.to_string_lossy().into_owned());
                        stats
                            .temp_recovery_paths
                            .push(backup.to_string_lossy().into_owned());
                        stats
                            .retained_backup_paths
                            .push(backup.to_string_lossy().into_owned());
                    } else {
                        return Err(error);
                    }
                }
            }
        } else {
            // Datei. Ziel absichern. Ein möglicher Typkonflikt wird erst NACH der vollständig
            // geschriebenen Temp-Datei umgehängt; so bleibt das alte Ziel bei Abbruch erhalten.
            guard_vfs(dst, sandbox, dst_abs)?;
            let replace_existing = optional_stat(dst, dst_abs)?.is_some();
            if replace_existing && exclusive_destination {
                return Err(coded_error(
                    io::ErrorKind::AlreadyExists,
                    ErrorCode::PlanDrift,
                    "Konfliktkopie inzwischen belegt; bitte neu planen",
                ));
            }
            if replace_existing && policy == OverwritePolicy::Skip {
                stats.skipped += 1;
                return Ok(());
            }
            let expected_replacement = if replace_existing {
                expected_replacement_for(expected_replacements, dst_abs)?
            } else {
                None
            };
            prog.current = file_name(dst_abs).unwrap_or_default();
            on_progress(prog);
            let outcome = match copy_file_stream(
                FileTransferRequest {
                    src,
                    src_path: src_abs,
                    dst,
                    dst_path: dst_abs,
                    sandbox,
                    replace_existing,
                    expected_replacement,
                    source_mtime: meta.mtime,
                    source_mode: meta.mode,
                },
                cancel,
                prog,
                on_progress,
            ) {
                Ok(outcome) => outcome,
                Err(failure) => {
                    if let Some(effect) = failure.unclear_effect {
                        actuals.mark_unclear(effect);
                    }
                    if let Some(path) = failure.temp_recovery_path {
                        actuals.record_temp_recovery_path(path);
                    }
                    return Err(failure.error);
                }
            };
            if let Some(recovery_path) = &outcome.temp_recovery_path {
                if outcome.complete {
                    stats.retained_backup_paths.push(recovery_path.clone());
                    actuals.retained_backup_paths.push(recovery_path.clone());
                }
                stats.temp_recovery_paths.push(recovery_path.clone());
                actuals.record_temp_recovery_path(recovery_path.clone());
            }
            if !outcome.complete {
                return Ok(());
            }
            stats.target_copy_new_best_effort |= outcome.target_copy_new_best_effort;
            stats.target_replace_best_effort |= outcome.target_replace_best_effort;
            if outcome.target_copy_new_best_effort {
                actuals.record_target_copy_new(Some(true));
            }
            if outcome.target_replace_best_effort {
                actuals.record_target_replace(Some(true));
            }
            let metadata_limitations = dst.interactive_write_limitations();
            stats
                .target_write_limitations
                .extend(metadata_limitations.iter().cloned());
            actuals
                .target_write_limitations
                .extend(metadata_limitations);
            stats.files = stats.files.saturating_add(1);
            stats.bytes = stats.bytes.saturating_add(meta.size);
            prog.files_done = prog.files_done.saturating_add(1);
            on_progress(prog);
        }
        Ok(())
    })();
    let result = if staging_scope.is_some() {
        crate::atomic_file::finish_staging_scope(dst, result, |_, path| {
            if !stats.temp_recovery_paths.contains(&path) {
                stats.temp_recovery_paths.push(path.clone());
            }
            actuals.record_temp_recovery_path(path);
        })
    } else {
        result
    };
    if let Err(error) = &result {
        for path in crate::error::recovery_paths(error) {
            actuals.record_temp_recovery_path(path);
        }
    }
    // Der Rückweg eines übergeordneten Ordners kann den Kindfehler neu aufbauen.
    // Schon bekannte Kindreste bleiben deshalb auch im endgültigen Errorpayload.
    result.map_err(|error| crate::error::with_recovery_paths(error, &actuals.temp_recovery_paths))
}

// ---------------------------------------------------------------------------
// Verschieben
// ---------------------------------------------------------------------------

struct MoveSnapshotEntry {
    relative: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: Option<i64>,
    mode: Option<u32>,
    content_hash: Option<[u8; 32]>,
    /// Bei einem Merge gehört der Zielordner schon dem Zielbestand. Sein Modus darf
    /// nicht still an den Quellordner angeglichen werden. `Some` bindet diesen
    /// Vorzustand; `None` bedeutet, dass der Ordner von diesem Move neu entsteht.
    destination_preexisting_mode: Option<Option<u32>>,
}

struct MoveSourceBinding {
    entries: Vec<MoveSnapshotEntry>,
    removal_proof: Option<MoveSourceRemovalProof>,
}

/// Liest den gesamten Quellbestand vor dem Kopieren. Der Digest allein erlaubt noch
/// keine Entfernung: Erst `bind_move_source_for_removal` darf ihn provideratomar an
/// eine unveränderliche Version der Datei beziehungsweise des ganzen Baums binden.
fn bind_move_source(src: &dyn Vfs, source: &Path) -> io::Result<MoveSourceBinding> {
    fn collect(
        src: &dyn Vfs,
        path: &Path,
        relative: &Path,
        entries: &mut Vec<MoveSnapshotEntry>,
        digest: &mut blake3::Hasher,
        removal_possible: &mut bool,
    ) -> io::Result<()> {
        let metadata = src.stat(path)?;
        let relative_text = relative.to_str().ok_or_else(|| {
            coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                "Move-Quellbaum enthält einen nicht sicher dekodierbaren relativen Pfad",
            )
        })?;
        if metadata.is_symlink || (!metadata.is_file && !metadata.is_dir) {
            // Solche Einträge kopiert Doppeldecker bewusst nicht. Wir lesen sie vor
            // dem ersten Zielschreiben trotzdem und markieren den ganzen Quellbaum als
            // nicht entfernbar. Nach dem Kopieren bleibt er vollständig stehen; ein
            // rekursives Löschen anhand seines Pfadnamens findet niemals statt.
            digest.update(&(relative_text.len() as u64).to_le_bytes());
            digest.update(relative_text.as_bytes());
            digest.update(&[2]);
            *removal_possible = false;
            return Ok(());
        }
        digest.update(&(relative_text.len() as u64).to_le_bytes());
        digest.update(relative_text.as_bytes());
        digest.update(&[u8::from(metadata.is_dir)]);
        digest.update(&metadata.size.to_le_bytes());
        digest.update(&metadata.mtime.unwrap_or(i64::MIN).to_le_bytes());
        digest.update(&metadata.mode.unwrap_or(u32::MAX).to_le_bytes());

        let content_hash = if metadata.is_file {
            let hash = *hash_vfs_file(src, path)?.as_bytes();
            digest.update(&hash);
            Some(hash)
        } else {
            None
        };
        entries.push(MoveSnapshotEntry {
            relative: relative.to_path_buf(),
            is_dir: metadata.is_dir,
            size: metadata.size,
            mtime: metadata.mtime,
            mode: metadata.mode,
            content_hash,
            destination_preexisting_mode: None,
        });

        if metadata.is_dir {
            let listing = src.list_complete(path)?;
            require_complete_safe_listing(&listing, "Move-Quellbindung")?;
            let mut children = listing.entries.iter().collect::<Vec<_>>();
            children.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
            for child in children {
                collect(
                    src,
                    &path.join(&child.entry.name),
                    &relative.join(&child.entry.name),
                    entries,
                    digest,
                    removal_possible,
                )?;
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    let mut digest = blake3::Hasher::new();
    let mut removal_possible = true;
    collect(
        src,
        source,
        Path::new(""),
        &mut entries,
        &mut digest,
        &mut removal_possible,
    )?;
    let content_digest = *digest.finalize().as_bytes();
    let recursive = entries.first().is_some_and(|entry| entry.is_dir);
    let removal_proof = if removal_possible {
        src.bind_move_source_for_removal(source, content_digest, recursive)?
    } else {
        None
    };
    if removal_proof
        .as_ref()
        .is_some_and(|proof| proof.content_digest != content_digest || proof.recursive != recursive)
    {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Provider lieferte einen Move-Quellbeleg für einen anderen Inhaltsdigest",
        ));
    }
    Ok(MoveSourceBinding {
        entries,
        removal_proof,
    })
}

fn bind_existing_move_destination_directories(
    dst: &dyn Vfs,
    destination: &Path,
    binding: &mut MoveSourceBinding,
) -> io::Result<()> {
    for expected in &mut binding.entries {
        if !expected.is_dir {
            continue;
        }
        let path = if expected.relative.as_os_str().is_empty() {
            destination.to_path_buf()
        } else {
            destination.join(&expected.relative)
        };
        match dst.stat(&path) {
            Ok(entry) if entry.is_dir && !entry.is_symlink => {
                expected.destination_preexisting_mode = Some(entry.mode);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Liest jeden zuvor gebundenen Quellpfad am Ziel zurück. Zusätzliche Einträge
/// eines zusammengeführten Zielordners sind erlaubt; jeder erwartete Dateiinhalt
/// muss jedoch exakt dem vor dem Kopieren gebildeten Hash entsprechen.
fn verify_move_destination(
    dst: &dyn Vfs,
    destination: &Path,
    binding: &MoveSourceBinding,
) -> io::Result<()> {
    for expected in &binding.entries {
        let path = if expected.relative.as_os_str().is_empty() {
            destination.to_path_buf()
        } else {
            destination.join(&expected.relative)
        };
        let actual = dst.stat(&path).map_err(|error| {
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das kopierte Move-Ziel konnte nicht vollständig zurückgelesen werden ({}: {error})",
                    path.display()
                ),
            )
        })?;
        if actual.is_symlink
            || actual.is_dir != expected.is_dir
            || (!expected.is_dir && (!actual.is_file || actual.size != expected.size))
        {
            return Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das kopierte Move-Ziel stimmt in Typ oder Größe nicht mit der gebundenen Quelle überein: {}",
                    path.display()
                ),
            ));
        }
        if let Some(expected_hash) = expected.content_hash {
            if hash_vfs_file(dst, &path)?.as_bytes() != &expected_hash {
                return Err(coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Das kopierte Move-Ziel hat nicht den gebundenen Quellinhalt: {}",
                        path.display()
                    ),
                ));
            }
        }
        let mode_write = dst.supports_mode_write_at(if expected.is_dir {
            &path
        } else {
            path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Move-Ziel benötigt einen Elternordner",
                )
            })?
        })?;
        let metadata_mismatch = if expected.is_dir {
            match expected.destination_preexisting_mode {
                // Beim Zusammenführen verändert schon ein neu angelegtes Kind die
                // Ordnerzeit. Identitätsrelevant und von Fileops unverändert bleibt
                // hier der zuvor gebundene Zielmodus; Quellmetadaten gelten nur für
                // einen von dieser Operation neu angelegten Zielordner.
                Some(mode) => mode_write && mode.is_some_and(|mode| actual.mode != Some(mode)),
                None => {
                    (dst.supports_mtime_write()
                        && expected
                            .mtime
                            .is_some_and(|mtime| actual.mtime != Some(mtime)))
                        || (mode_write
                            && expected.mode.is_some_and(|mode| actual.mode != Some(mode)))
                }
            }
        } else {
            (dst.supports_mtime_write()
                && expected
                    .mtime
                    .is_some_and(|mtime| actual.mtime != Some(mtime)))
                || (mode_write && expected.mode.is_some_and(|mode| actual.mode != Some(mode)))
        };
        if metadata_mismatch {
            return Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Die übertragenen Metadaten des Move-Ziels stimmen nicht mit der gebundenen Quelle überein: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn verify_move_source_entry(
    src: &dyn Vfs,
    source: &Path,
    expected: &MoveSnapshotEntry,
) -> io::Result<()> {
    let actual = src.stat(source)?;
    if actual.is_symlink
        || actual.is_dir != expected.is_dir
        || (!expected.is_dir && (!actual.is_file || actual.size != expected.size))
        || actual.mtime != expected.mtime
        || actual.mode != expected.mode
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "Typ, Größe oder Metadaten der Move-Quelle haben sich geändert: {}",
                source.display()
            ),
        ));
    }
    if let Some(expected_hash) = expected.content_hash {
        if hash_vfs_file(src, source)?.as_bytes() != &expected_hash {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Der Inhalt der Move-Quelle hat sich geändert: {}",
                    source.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Entfernt einen vollständig rückgelesenen Quellbaum im begrenzten Best-Effort-
/// Modus. Vor der ersten Wirkung wird der gesamte Bestand erneut geprüft. Danach
/// werden nur einzelne gebundene Blätter und zuletzt nachweislich leere Ordner
/// entfernt; ein rekursives Löschen anhand des Wurzelnamens ist ausdrücklich verboten.
fn remove_move_source_best_effort(
    src: &dyn Vfs,
    source: &Path,
    binding: &MoveSourceBinding,
    on_first_remove: impl FnOnce(),
) -> io::Result<()> {
    for expected in &binding.entries {
        let path = if expected.relative.as_os_str().is_empty() {
            source.to_path_buf()
        } else {
            source.join(&expected.relative)
        };
        verify_move_source_entry(src, &path, expected)?;
        if expected.is_dir {
            let listing = src.list_complete(&path)?;
            require_complete_safe_listing(&listing, "Move-Quellprüfung")?;
            let mut actual_names = listing
                .entries
                .iter()
                .map(|entry| entry.entry.name.clone())
                .collect::<Vec<_>>();
            actual_names.sort();
            let mut expected_names = binding
                .entries
                .iter()
                .filter_map(|candidate| {
                    candidate
                        .relative
                        .parent()
                        .filter(|parent| *parent == expected.relative)
                        .and_then(|_| candidate.relative.file_name())
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>();
            expected_names.sort();
            if actual_names != expected_names {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Der vollständige Move-Quellbestand hat sich geändert: {}",
                        path.display()
                    ),
                ));
            }
        }
    }

    let mut on_first_remove = Some(on_first_remove);
    for expected in binding.entries.iter().rev() {
        let path = if expected.relative.as_os_str().is_empty() {
            source.to_path_buf()
        } else {
            source.join(&expected.relative)
        };
        if expected.is_dir {
            let listing = src.list_complete(&path)?;
            require_complete_safe_listing(&listing, "Move-Quellabbau")?;
            if !listing.entries.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::DirectoryNotEmpty,
                    format!("Move-Quellordner ist nicht leer: {}", path.display()),
                ));
            }
            if let Some(mark_started) = on_first_remove.take() {
                mark_started();
            }
            src.remove_move_empty_dir_best_effort(&path)?;
        } else {
            // Direkt vor jedem irreversiblen Blatt-Remove wird Inhalt und Metadaten
            // nochmals gelesen. Ein Austausch nach der Gesamtprüfung bleibt dadurch
            // auf genau diese eine Datei begrenzt und kann nie einen ganzen Baum treffen.
            verify_move_source_entry(src, &path, expected)?;
            if let Some(mark_started) = on_first_remove.take() {
                mark_started();
            }
            src.remove_move_leaf_best_effort(&path)?;
        }
        sync_parent(src, &path)?;
    }
    Ok(())
}

/// Verschiebt die (absoluten) `items` in den Zielordner `dst_dir`.
///
/// Schnellweg: [`Vfs::rename_noreplace`] (auf gleichem Volume atomar und sofort). Scheitert das
/// (z. B. über Volume-Grenzen, `EXDEV`, oder wenn ein Ziel-Ordner schon existiert), wird
/// auf Kopieren + Löschen der Quelle ausgewichen. Bei gesetzter Sandbox müssen QUELLE UND
/// ZIEL darunter liegen (Verschieben entfernt die Quelle an ihrem alten Ort).
pub fn move_items(
    request: FileOperationRequest<'_>,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
) -> io::Result<OpStats> {
    move_items_inner(
        request,
        FileTransferBindings::default(),
        false,
        cancel,
        on_progress,
        &mut FileOperationActuals::default(),
    )
}

#[cfg(test)]
pub(crate) fn move_items_with_conflict_proofs_tracked(
    request: FileOperationRequest<'_>,
    expected_replacements: &[FileConflictProof],
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<OpStats> {
    move_items_inner(
        request,
        FileTransferBindings {
            replacements: Some(expected_replacements),
            ..FileTransferBindings::default()
        },
        false,
        cancel,
        on_progress,
        actuals,
    )
}

pub(crate) fn move_items_with_targets_tracked(
    request: FileOperationRequest<'_>,
    bindings: FileTransferBindings<'_>,
    rename_only: bool,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<OpStats> {
    move_items_inner(
        request,
        bindings,
        rename_only,
        cancel,
        on_progress,
        actuals,
    ).map_err(|error| {
        if !bindings.targets.is_empty() && error.kind() == io::ErrorKind::AlreadyExists {
            with_recovery_context(coded_error(io::ErrorKind::AlreadyExists, ErrorCode::PlanDrift,
                format!("Ein bestätigter Konfliktkopie-Name wurde inzwischen belegt ({error}); bitte neu planen")), &[&error], &[])
        } else { error }
    })
}

fn move_items_inner(
    request: FileOperationRequest<'_>,
    bindings: FileTransferBindings<'_>,
    rename_only: bool,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(&Progress),
    actuals: &mut FileOperationActuals,
) -> io::Result<OpStats> {
    if request.dst.additive_content_target_only() {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Die additive WebDAV-Zielsicht erlaubt keine Quellentfernung",
        ));
    }
    if request.src.interactive_additive_only() {
        return Err(coded_error(io::ErrorKind::Unsupported, ErrorCode::Unsupported,
            "Die begrenzte SFTP-Inhaltsquelle erlaubt ausschließlich additive lokale Kopien; Remote-Ziele, Ersetzen und Verschieben bleiben gesperrt"));
    }
    let FileTransferBindings {
        targets,
        replacements: expected_replacements,
        skipped_targets: bound_skipped_targets,
    } = bindings;
    let FileOperationRequest {
        src,
        dst,
        items,
        dst_dir,
        policy,
        sandbox,
    } = request;
    validate_transfer_targets(src, dst, items, dst_dir)?;
    verify_keep_both_targets(src, dst, items, dst_dir, targets)?;
    guard_transfer_scope(src, dst, items, dst_dir, sandbox, true)?;
    let mut stats = OpStats::default();
    let (files_total, bytes_total) = scan_totals(src, items, false)?;
    let mut prog = Progress {
        files_total,
        bytes_total,
        phase: "move".into(),
        ..Default::default()
    };

    for src_abs in items {
        if cancel.is_cancelled() {
            return Ok(finish_cancelled(stats, &mut prog, on_progress));
        }
        let Some(name) = file_name(src_abs) else {
            continue;
        };
        let dst_abs = transfer_target(dst_dir, src_abs, targets)?;
        let exclusive = targets
            .iter()
            .any(|target| Path::new(&target.source) == src_abs);
        if exclusive && optional_stat(dst, &dst_abs)?.is_some() {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::PlanDrift,
                "Konfliktkopie inzwischen belegt; bitte neu planen",
            ));
        }

        // Symlink? In v0.1 überspringen (konsistent mit Kopieren/Sync).
        let source_entry = src.stat(src_abs)?;
        if source_entry.is_symlink {
            stats.symlinks_skipped = stats.symlinks_skipped.saturating_add(1);
            continue;
        }

        let target_entry = optional_stat(dst, &dst_abs)?;
        let exists = target_entry.is_some();
        if exists && policy == OverwritePolicy::Skip {
            // Wie beim Kopieren: `Skip` überspringt nur echte Blattkonflikte. Zwei
            // gewöhnliche Ordner werden über den Kopier-Fallback ZUSAMMENGEFÜHRT,
            // der `Skip` dann rekursiv auf kollidierende Blätter anwendet. Ohne
            // diese Unterscheidung würde ein bestätigtes Provider-Move, dessen
            // Blattkonflikte vorab in die Quarantäne geparkt wurden, den noch
            // vorhandenen Zielordner komplett überspringen und nichts verschieben.
            let target = target_entry
                .as_ref()
                .expect("exists stammt aus demselben optionalen Stat");
            if !source_entry.is_dir
                || !target.is_dir
                || source_entry.is_symlink
                || target.is_symlink
            {
                stats.skipped = stats.skipped.saturating_add(1);
                continue;
            }
        }

        // Sandbox: sowohl das Verschwinden an der Quelle als auch das Auftauchen am Ziel
        // müssen erlaubt sein.
        guard_vfs(src, sandbox, src_abs)?;
        guard_vfs(dst, sandbox, &dst_abs)?;

        // Automatisch darf Rename nur innerhalb derselben VFS-Instanz laufen. Zwei
        // Endpunkte können denselben virtuellen Pfadtext besitzen und trotzdem
        // verschiedene Server, Shares oder descriptor-gebundene Roots bezeichnen.
        // Der FTPS-Queueweg darf getrennte Operationssichten nur nach gemeinsamer
        // Sitzungs-/Rootbindung und Übernahme der typisierten Zielbelege verwenden.
        // Seine bestätigte reine Rename-Wirkung erlaubt keinen Copy-/Remove-Rückfall.
        let same_vfs = std::ptr::eq::<dyn Vfs>(src, dst);
        if !exists
            && (rename_only
                || (same_vfs && dst.file_commit_class_at(dst_dir)? == FileCommitClass::Assured))
        {
            prog.current = name.clone();
            on_progress(&prog);
            match rename_noreplace_with_phase(src, src_abs, &dst_abs) {
                Ok(()) => {
                    // Erfolg: Zähler grob nachführen (rename bewegt alles auf einmal).
                    let (f, b) = subtree_totals(dst, &dst_abs, false).map_err(|count_error| {
                        // Rename und beide Eltern-Syncs sind bereits abgeschlossen.
                        // Ein nachgelagerter Lesefehler darf diese vollzogene Wirkung
                        // nie wie einen gewöhnlichen mutationsfreien Fehler aussehen
                        // lassen oder die tatsächlich sichtbaren Pfade verschweigen.
                        coded_error(
                            io::ErrorKind::Other,
                            ErrorCode::OutcomeUnclear,
                            format!(
                                "Die Move-Quelle wurde vollständig und synchronisiert nach {} umbenannt, aber die nachgelagerte Fortschrittszählung scheiterte ({count_error}). Die Move-Wirkung ist bereits vollzogen. Recoverypfade: früherer Quellpfad: {}; sichtbares Ziel: {}",
                                dst_abs.display(),
                                src_abs.display(),
                                dst_abs.display(),
                            ),
                        )
                    })?;
                    stats.files = stats.files.saturating_add(f);
                    stats.bytes = stats.bytes.saturating_add(b);
                    prog.files_done = prog.files_done.saturating_add(f).min(prog.files_total);
                    prog.bytes_done = prog.bytes_done.saturating_add(b).min(prog.bytes_total);
                    on_progress(&prog);
                    continue;
                }
                Err(SyncedRenameFailure::RenameUnchanged(rename_error)) => {
                    // Erscheint das Ziel erst nach Konfliktprüfung, war dieses konkrete
                    // Überschreiben nie bestätigt. Auch bei Policy `Overwrite` brechen
                    // wir deshalb ab. Nur echte Rename-Grenzen wie EXDEV dürfen in den
                    // Kopieren-und-Löschen-Fallback gehen.
                    if optional_stat(dst, &dst_abs)?.is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "Ziel „{}“ entstand während des Verschiebens; nichts wurde überschrieben ({rename_error})",
                                dst_abs.display()
                            ),
                        ));
                    }
                    if rename_only || !rename_allows_copy_fallback(&rename_error) {
                        return Err(rename_error);
                    }
                }
                Err(SyncedRenameFailure::RenameOutcomeUnclear(rename_error)) => {
                    return Err(coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Das Move-Rename meldete einen Fehler nach möglicher Wirkung ({rename_error}). Es wurde kein Rückweg versucht. Recoverypfade: möglicher alter Quellname: {}; möglicher neuer Zielname: {}",
                            src_abs.display(),
                            dst_abs.display(),
                        ),
                    ));
                }
                Err(SyncedRenameFailure::Sync(sync_error)) => {
                    return Err(coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Die Move-Quelle wurde sichtbar nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({sync_error}). Recoverypfade: früherer Quellpfad: {}; sichtbares Ziel: {}",
                            dst_abs.display(),
                            src_abs.display(),
                            dst_abs.display(),
                        ),
                    ));
                }
            }
        }

        // Fallback / Überschreiben: Die Quelle wird VOR dem ersten Zielschreiben
        // vollständig gelesen. Ein Provider darf die spätere Entfernung nur mit
        // einem Beleg erlauben, der genau diesen Inhalt bis zur Wirkung bindet.
        if rename_only {
            return Err(coded_error(io::ErrorKind::AlreadyExists, ErrorCode::PlanDrift,
                "Das bestätigte Rename-Ziel ist inzwischen belegt; Zusammenführen benötigt eine neue Copy-/Remove-Bestätigung"));
        }
        let skipped_targets = if policy == OverwritePolicy::Skip && !exclusive {
            match bound_skipped_targets {
                Some(paths) => paths
                    .iter()
                    .filter(|path| Path::new(path).starts_with(&dst_abs))
                    .cloned()
                    .collect(),
                None => find_recursive_conflicts(src, dst, dst_dir, std::slice::from_ref(src_abs))?,
            }
        } else {
            Vec::new()
        };
        // Skip bindet ausdrücklich den Erhalt dieses gesamten Quellbaums. Dafür
        // ist weder ein Inhaltsbeleg ausgelassener Dateien noch Löschfähigkeit nötig.
        let mut source_binding = if skipped_targets.is_empty() {
            Some(bind_move_source(src, src_abs)?)
        } else {
            None
        };
        if let Some(binding) = &mut source_binding {
            bind_existing_move_destination_directories(dst, &dst_abs, binding)?;
        }
        let symlinks_skipped_before = stats.symlinks_skipped;
        let conflicts_skipped_before = stats.skipped;
        copy_tree(
            &CopyTreeRequest {
                src,
                dst,
                sandbox,
                cancel,
                policy,
                expected_replacements,
                exclusive_root: exclusive.then_some(dst_abs.as_path()),
                skipped_targets: &skipped_targets,
                allow_name_omissions: false,
            },
            src_abs,
            &dst_abs,
            &mut stats,
            &mut prog,
            on_progress,
            actuals,
        )?;
        if cancel.is_cancelled() {
            return Ok(finish_cancelled(stats, &mut prog, on_progress));
        }
        if stats.symlinks_skipped > symlinks_skipped_before
            || stats.skipped > conflicts_skipped_before
        {
            // Ein verschachtelter Symlink ODER ein bewusst übersprungener Konflikt
            // wurde NICHT kopiert. Würden wir nun den Quellordner rekursiv entfernen,
            // ginge genau dieser Eintrag verloren (das Move würde Daten löschen, die
            // nie am Ziel angekommen sind). Die sichere Semantik ist eine
            // vollständige Quellkopie zu behalten.
            continue;
        }
        let Some(source_binding) = source_binding else {
            // Auch bei späterem Zielwechsel bleibt die bestätigte Quelle erhalten.
            continue;
        };
        if let Err(readback_error) = verify_move_destination(dst, &dst_abs, &source_binding) {
            return Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das Move-Ziel wurde geschrieben, sein vollständiger Readback scheiterte aber ({readback_error}). Die Quelle blieb unangetastet. Recoverypfade: Quelle: {}; kopiertes Ziel: {}",
                    src_abs.display(),
                    dst_abs.display(),
                ),
            ));
        }
        let root_is_dir = source_binding
            .entries
            .first()
            .is_some_and(|entry| entry.is_dir);
        let source_removal_best_effort = !matches!(
            source_binding.removal_proof.as_ref(),
            Some(proof) if !root_is_dir && !proof.recursive
        );
        let remove_result = match source_binding.removal_proof.as_ref() {
            // Ein Providerbeleg darf genau eine reguläre Datei atomar schützen. Für
            // Bäume wäre ein Root-Token mit `remove_dir_all` zu grob: Er könnte nach
            // einem Namensaustausch beliebig viele fremde Einträge treffen.
            Some(proof) if !root_is_dir && !proof.recursive => {
                src.remove_move_source_if_matches(src_abs, proof)
            }
            _ => remove_move_source_best_effort(src, src_abs, &source_binding, || {
                // Erst unmittelbar vor der ersten irreversiblen Einzelwirkung ist
                // der tatsächliche Ausgang nicht mehr sicher `false`. Jeder rein
                // lesende Verifyfehler davor behält die klare Nullwirkung.
                actuals.record_source_removal(None);
            }),
        };
        if let Err(remove_error) = remove_result {
            return Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das Move-Ziel wurde vollständig kopiert, aber der frisch geprüfte Quellabbau konnte nicht vollständig bestätigt werden ({remove_error}). Es kann bereits eine begrenzte Teilwirkung gegeben haben. Recoverypfade: Quelle beziehungsweise Quellrest: {}; kopiertes Ziel: {}",
                    src_abs.display(),
                    dst_abs.display(),
                ),
            ));
        }
        if let Err(sync_error) = sync_parent(src, src_abs) {
            return Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Die gebundene Move-Quelle wurde entfernt, aber ihr Elternordner konnte nicht dauerhaft synchronisiert werden ({sync_error}). Recoverypfade: früherer Quellpfad: {}; kopiertes Ziel: {}",
                    src_abs.display(),
                    dst_abs.display(),
                ),
            ));
        }
        // Erst nach vollständiger Entfernung samt Eltern-Sync steht fest, dass der
        // begrenzte Pfad erfolgreich war. Bei jeder Teilwirkung kehren wir vorher mit
        // `OutcomeUnclear` zurück und dürfen keinen Erfolg vortäuschen.
        stats.source_removal_best_effort |= source_removal_best_effort;
        if source_removal_best_effort {
            actuals.record_source_removal(Some(true));
        }
    }

    prog.phase = "done".into();
    prog.current.clear();
    on_progress(&prog);
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Umbenennen & Ordner anlegen
// ---------------------------------------------------------------------------

/// Benennt den Eintrag `old_abs` in `new_name` (reiner Name, kein Pfad) um — im selben
/// Ordner. Bricht ab, wenn der Zielname schon existiert (kein stilles Überschreiben) oder
/// der Name ungültig ist (leer bzw. mit „/").
pub fn rename(
    vfs: &dyn Vfs,
    old_abs: &Path,
    new_name: &str,
    sandbox: &Option<PathBuf>,
) -> io::Result<()> {
    let new_name = validate_name(new_name)?;
    let parent = old_abs
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Kein übergeordneter Ordner"))?;
    let new_abs = parent.join(&new_name);
    guard_vfs(vfs, sandbox, old_abs)?;
    guard_vfs(vfs, sandbox, &new_abs)?;
    if optional_stat(vfs, &new_abs)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("„{new_name}“ existiert bereits"),
        ));
    }
    match vfs.rename_noreplace_with_removal_proof(old_abs, &new_abs) {
        Ok(_) => {}
        Err(MutationFailure::Unchanged(error)) => return Err(error),
        Err(MutationFailure::OutcomeUnclear(error)) => {
            return Err(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das Umbenennen meldete einen Fehler nach möglicher Wirkung ({error}). Es wurde kein blinder Rückweg versucht. Recoverypfade: möglicher alter Name: {}; möglicher neuer Name: {}",
                    old_abs.display(),
                    new_abs.display(),
                ),
            ));
        }
    }
    sync_rename_parents(vfs, old_abs, &new_abs).map_err(|error| {
        coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Der Eintrag wurde sichtbar nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({error}). Recoverypfade: früherer Name: {}; sichtbarer neuer Name: {}",
                new_abs.display(),
                old_abs.display(),
                new_abs.display(),
            ),
        )
    })
}

/// Legt im Ordner `parent` einen neuen Unterordner `name` an. Bricht ab, wenn er schon
/// existiert oder der Name ungültig ist. Gibt den vollständigen Pfad des neuen Ordners zurück.
pub fn create_dir(
    vfs: &dyn Vfs,
    parent: &Path,
    name: &str,
    sandbox: &Option<PathBuf>,
) -> io::Result<PathBuf> {
    let name = validate_name(name)?;
    let new_abs = parent.join(&name);
    guard_vfs(vfs, sandbox, &new_abs)?;
    if optional_stat(vfs, &new_abs)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("„{name}“ existiert bereits"),
        ));
    }
    match vfs.create_dir_with_removal_proof(&new_abs) {
        Ok(_) => {}
        Err(MutationFailure::Unchanged(error)) => return Err(error),
        Err(MutationFailure::OutcomeUnclear(error)) => {
            return Err(coded_error(
                error.kind(),
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das Anlegen des Ordners meldete einen Fehler nach möglicher Wirkung ({error}). Der sichtbare Name wurde nicht pfadbasiert entfernt. Recoverypfad: möglicher neuer Ordner: {}",
                    new_abs.display()
                ),
            ));
        }
    }
    sync_parent(vfs, &new_abs).map_err(|error| {
        coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Der Ordner wurde sichtbar angelegt, aber sein Elternordner konnte nicht dauerhaft synchronisiert werden ({error}). Recoverypfad: sichtbarer neuer Ordner: {}",
                new_abs.display(),
            ),
        )
    })?;
    Ok(new_abs)
}

// ---------------------------------------------------------------------------
// interne Helfer
// ---------------------------------------------------------------------------

/// Reinen Dateinamen (letzte Pfadkomponente) als String holen.
fn file_name(p: &Path) -> Option<String> {
    p.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

/// Fehler für eine Auswahl, deren letzter Namensbestandteil nicht sicher lesbar ist.
///
/// Ein nur ersetzungsweise dekodierter Name bezeichnet ein anderes Objekt als das
/// gemeinte. Er darf deshalb weder Ziel einer Operation noch Teil eines
/// persistierten Eintrags werden.
fn undecodable_name_error() -> io::Error {
    coded_error(
        io::ErrorKind::InvalidData,
        ErrorCode::InvalidData,
        "Auswahl enthält keinen sicher dekodierbaren Dateinamen",
    )
}

/// Weist eine Auswahl ohne sicher dekodierbaren Namen ab.
fn reject_undecodable_name(item: &Path) -> io::Result<()> {
    if file_name(item).is_none() {
        return Err(undecodable_name_error());
    }
    Ok(())
}

/// Bildet den Zielpfad, unter dem ein ausgewählter Eintrag im Zielordner landet.
fn target_path_in(dst_dir: &Path, source: &Path) -> io::Result<PathBuf> {
    let name = file_name(source).ok_or_else(undecodable_name_error)?;
    Ok(dst_dir.join(name))
}

/// Wandelt nur eindeutig als UTF-8 darstellbare Pfade in die persistierbare Form um.
/// Ein Ersatzzeichen wäre ein anderer Name und darf nie in Queue oder Audit landen.
pub fn path_to_utf8(path: &Path) -> io::Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!(
                "Pfad enthält einen nicht eindeutig als UTF-8 darstellbaren Namen: {}",
                path.display()
            ),
        )
    })
}

/// Rein lesende Eingabeprüfung für Queue-fähige Dateioperationen.
pub fn validate_file_operation_paths(items: &[PathBuf], destination: &Path) -> io::Result<()> {
    path_to_utf8(destination)?;
    if normalize_absolute(destination)? != destination {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Zielordner muss absolut und lexikalisch normalisiert sein",
        ));
    }
    let mut normalized = Vec::with_capacity(items.len());
    for item in items {
        path_to_utf8(item)?;
        reject_undecodable_name(item)?;
        let normalized_item = normalize_absolute(item)?;
        if normalized_item != *item {
            return Err(coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                format!(
                    "Auswahlpfad muss absolut und lexikalisch normalisiert sein: {}",
                    item.display()
                ),
            ));
        }
        normalized.push(normalized_item);
    }
    validate_nonoverlapping_file_items(&normalized)
}

fn validate_nonoverlapping_file_items(items: &[PathBuf]) -> io::Result<()> {
    let mut ordered = items.to_vec();
    ordered.sort();
    for pair in ordered.windows(2) {
        let parent = &pair[0];
        let candidate = &pair[1];
        if candidate == parent || candidate.starts_with(parent) {
            return Err(coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                format!(
                    "Die Auswahl enthält denselben Pfad mehrfach oder zugleich einen Ordner und dessen Kind: {} / {}",
                    parent.display(),
                    candidate.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Validiert und normalisiert eine Auswahl rein lexikalisch unter ihrem Pane-Root.
/// Diese Funktion öffnet weder Dateien noch Provider und eignet sich deshalb für
/// CLI-/Queue-Gates vor Secretzugriff und Netzwerk. Lokale Aufrufer kanonisieren die
/// so geprüften Pfade anschließend zusätzlich und wiederholen die Rootprüfung.
pub fn validate_file_operation_scope(
    root: &Path,
    items: &[PathBuf],
) -> io::Result<(PathBuf, Vec<PathBuf>)> {
    let root = validate_file_operation_root(root)?;
    if items.is_empty() {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Dateioperation braucht mindestens einen ausgewählten Pfad",
        ));
    }
    let mut normalized = Vec::with_capacity(items.len());
    for item in items {
        path_to_utf8(item)?;
        let item = normalize_absolute(item)?;
        if item == root || !item.starts_with(&root) {
            return Err(coded_error(
                io::ErrorKind::PermissionDenied,
                ErrorCode::SandboxViolation,
                format!(
                    "Auswahl „{}“ verlässt den gebundenen Root „{}“",
                    item.display(),
                    root.display()
                ),
            ));
        }
        reject_undecodable_name(&item)?;
        normalized.push(item);
    }
    validate_nonoverlapping_file_items(&normalized)?;
    Ok((root, normalized))
}

/// Bindet lokale Auswahlpfade unter einem gehaltenen Root-Deskriptor, ohne dem
/// letzten Eintrag zu folgen. Ein ausgewählter Symlink bleibt dadurch ein Symlink
/// und wird später von den Dateioperationen sichtbar übersprungen. Auch jede
/// Elternkomponente wird descriptor-relativ ohne Symlink-Folgen geöffnet.
pub fn bind_local_file_operation_scope(
    root: &Path,
    items: &[PathBuf],
) -> io::Result<(PathBuf, Vec<PathBuf>)> {
    let (lexical_root, lexical_items) = validate_file_operation_scope(root, items)?;
    let canonical_root = std::fs::canonicalize(&lexical_root)?;
    let canonical_root = validate_file_operation_root(&canonical_root)?;
    let rebound_items = lexical_items
        .into_iter()
        .map(|item| {
            item.strip_prefix(&lexical_root)
                .map(|relative| canonical_root.join(relative))
                .map_err(|_| {
                    coded_error(
                        io::ErrorKind::PermissionDenied,
                        ErrorCode::SandboxViolation,
                        "Lokale Auswahl ließ sich nicht an den kanonischen Pane-Root binden",
                    )
                })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let (canonical_root, rebound_items) =
        validate_file_operation_scope(&canonical_root, &rebound_items)?;
    let rooted = LocalVfs::rooted(&canonical_root)?;
    for item in &rebound_items {
        // `stat` des rooted VFS ist ein lstat für den letzten Namen. Anders als
        // `canonicalize` folgt es dem ausgewählten Symlink niemals zu dessen Ziel.
        rooted.stat(item)?;
    }
    Ok((canonical_root, rebound_items))
}

/// Prüft einen Pane-Root ohne Providerzugriff und liefert seine lexikalisch
/// normalisierte, eindeutig als UTF-8 darstellbare absolute Form.
pub fn validate_file_operation_root(root: &Path) -> io::Result<PathBuf> {
    path_to_utf8(root)?;
    normalize_absolute(root)
}

/// Verhindert rekursive Selbstkopien, bevor Fortschrittsscan oder Mutation beginnen.
/// Die Pane-Wurzeln werden beim Öffnen kanonisiert; die lexikalische Prüfung bleibt
/// zusätzlich für virtuelle Providerpfade gültig und benötigt keinen lokalen Zugriff.
fn validate_transfer_targets(
    src_vfs: &dyn Vfs,
    dst_vfs: &dyn Vfs,
    items: &[PathBuf],
    dst_dir: &Path,
) -> io::Result<()> {
    validate_file_operation_paths(items, dst_dir)?;
    let destination = normalize_absolute(dst_dir)?;
    // Gleicher Pfadtext bedeutet bei zwei Remote-Verbindungen nicht derselbe
    // Speicherort. Lexikalische Vorfahrprüfung ist nur zulässig, wenn beide Seiten
    // nachweislich denselben Namespace verwenden: lokal den gemeinsamen Host oder
    // remote exakt dieselbe bereits gebundene VFS-Sitzung.
    let same_namespace =
        (src_vfs.is_local() && dst_vfs.is_local()) || std::ptr::eq::<dyn Vfs>(src_vfs, dst_vfs);
    for source in items {
        let source = normalize_absolute(source)?;
        let name = source.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quelle besitzt keinen Dateinamen",
            )
        })?;
        let target = destination.join(name);
        if same_namespace && (target == source || target.starts_with(&source)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Quelle und Ziel sind gleich oder das Ziel liegt in der Quelle: {} → {}",
                    source.display(),
                    target.display()
                ),
            ));
        }
        // Lokale Einträge tragen stabile Geräte-/Inode-IDs. Der Identitätsvergleich
        // erkennt dieselbe Verschachtelung auch dann, wenn zwei kanonische Wurzeln den
        // Baum unter unterschiedlichen sichtbaren Pfaden referenzieren. Remote-VFS
        // ohne solche IDs bleiben bei der obigen providerneutralen Pfadprüfung.
        let source_identity = if same_namespace {
            optional_stat(src_vfs, &source)?.and_then(|entry| entry.device.zip(entry.inode))
        } else {
            None
        };
        if let Some(source_identity) = source_identity {
            let mut ancestor = Some(destination.as_path());
            while let Some(path) = ancestor {
                let matches_source = optional_stat(dst_vfs, path)?
                    .and_then(|entry| entry.device.zip(entry.inode))
                    == Some(source_identity);
                if matches_source {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Zielordner liegt anhand seiner Dateisystemidentität in der Quelle: {} → {}",
                            source.display(),
                            destination.display()
                        ),
                    ));
                }
                if dst_vfs.anchored_root() == Some(path) {
                    break;
                }
                ancestor = path.parent();
            }
        }
    }
    Ok(())
}

/// Prüft alle lokalen Sandboxwirkungen eines Stapels, bevor Fortschrittsscan oder
/// erste Mutation beginnen. So kann ein später ungültiger Move-Eintrag keinen früheren
/// Eintrag bereits aus seiner Quelle entfernen.
fn guard_transfer_scope(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    items: &[PathBuf],
    dst_dir: &Path,
    sandbox: &Option<PathBuf>,
    removes_source: bool,
) -> io::Result<()> {
    for source in items {
        let name = source.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Quelle besitzt keinen Dateinamen",
            )
        })?;
        if removes_source {
            guard_vfs(src, sandbox, source)?;
        }
        guard_vfs(dst, sandbox, &dst_dir.join(name))?;
    }
    Ok(())
}

/// Nur `NotFound` bedeutet Abwesenheit. Verbindungs-, Rechte- und Protokollfehler
/// dürfen niemals über den booleschen Komfortweg `Vfs::exists` eine Mutation öffnen.
pub(crate) fn optional_stat(vfs: &dyn Vfs, path: &Path) -> io::Result<Option<Entry>> {
    match vfs.stat(path) {
        Ok(entry) => Ok(Some(entry)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn normalize_absolute(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Dateioperation braucht absoluten Pfad: {}", path.display()),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Pfad verlässt seine Wurzel: {}", path.display()),
                    ));
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    Ok(normalized)
}

fn rename_allows_copy_fallback(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::CrossesDevices
}

/// Prüft/normalisiert einen vom Nutzer eingegebenen Datei-/Ordnernamen: nicht leer, kein
/// „/" (Pfad-Trenner), kein Nullbyte und nicht „." / „.." — verhindert versehentliches
/// Ausbrechen.
///
/// Das Nullbyte steht hier aus demselben Grund wie in der Listingprüfung: Kein
/// Dateisystem erlaubt es in einem Namen, und auf dem Weg zu einem Provider endet
/// eine Zeichenkette daran. Der Nutzer würde dann einen anderen Namen bestätigen
/// als der Server anlegt.
pub fn validate_file_name(name: &str) -> io::Result<String> {
    let n = name.trim();
    if n.is_empty() || n.contains('/') || n.contains('\0') || n == "." || n == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Ungültiger Name",
        ));
    }
    Ok(n.to_string())
}

pub(crate) fn validate_name(name: &str) -> io::Result<String> {
    validate_file_name(name)
}

/// Prüft Root, alten Pfad und neuen Namen vollständig, bevor eine möglicherweise
/// mutierende Rename-Capability-Probe gestartet wird.
pub fn validate_interactive_rename_request(
    root: &Path,
    old_abs: &Path,
    new_name: &str,
) -> io::Result<String> {
    let new_name = validate_file_name(new_name)?;
    path_to_utf8(root)?;
    path_to_utf8(old_abs)?;
    let normalized_root = normalize_absolute(root)?;
    let normalized_old = normalize_absolute(old_abs)?;
    if normalized_old == normalized_root
        || !normalized_old.starts_with(&normalized_root)
        || normalized_old.parent().is_none()
        || normalized_old
            .file_name()
            .and_then(|name| name.to_str())
            .is_none()
    {
        return Err(coded_error(
            io::ErrorKind::PermissionDenied,
            ErrorCode::SandboxViolation,
            "Rename-Pfad verlässt den gebundenen Root oder besitzt keinen sicheren Namen",
        ));
    }
    Ok(new_name)
}

/// Fortschritts-/Bilanz-Endzustand bei Abbruch setzen.
fn finish_cancelled(
    mut stats: OpStats,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> OpStats {
    stats.cancelled = true;
    prog.phase = "cancelled".into();
    on_progress(prog);
    stats
}

/// Summe (Dateien, Bytes) über alle `items` rekursiv — für die Fortschritts-Vorschau.
/// Unvollständige oder widersprüchliche Listings stoppen vor jeder Zielmutation.
/// Nur additive Copy-Aufträge zählen sicher dekodierbare Kinder separat.
fn scan_totals(
    vfs: &dyn Vfs,
    items: &[PathBuf],
    allow_name_omissions: bool,
) -> io::Result<(u64, u64)> {
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for it in items {
        let (f, b) = subtree_totals(vfs, it, allow_name_omissions)?;
        files = files.saturating_add(f);
        bytes = bytes.saturating_add(b);
    }
    Ok((files, bytes))
}

/// (Dateien, Bytes) eines einzelnen Teilbaums.
fn subtree_totals(
    vfs: &dyn Vfs,
    path: &Path,
    allow_name_omissions: bool,
) -> io::Result<(u64, u64)> {
    match vfs.stat(path) {
        Ok(m) if m.is_symlink => Ok((0, 0)),
        Ok(m) if m.is_dir => {
            let mut files = 0_u64;
            let mut bytes = 0_u64;
            let listing =
                copy_source_listing(vfs, path, "Fortschritts-Quelllisting", allow_name_omissions)?;
            for child in listing.entries {
                let (f, b) =
                    subtree_totals(vfs, &path.join(&child.entry.name), allow_name_omissions)?;
                files = files.saturating_add(f);
                bytes = bytes.saturating_add(b);
            }
            Ok((files, bytes))
        }
        Ok(m) => Ok((1, m.size)),
        Err(error) => Err(error),
    }
}

/// Einen vorhandenen Typkonflikt (Ordner an der Stelle der neuen Datei) zunächst
/// wiederherstellbar auf einen versteckten Nachbarpfad verschieben.
struct ParkedTarget {
    path: PathBuf,
    removal_proof: Option<ProofListEntry>,
}

fn park_existing_target(
    dst: &dyn Vfs,
    dst_path: &Path,
    sandbox: &Option<PathBuf>,
) -> io::Result<ParkedTarget> {
    for _ in 0..100 {
        let backup = dst.staging_candidate(dst_path, "replaced")?;
        guard_vfs(dst, sandbox, &backup)?;
        match dst.rename_noreplace_with_removal_proof(dst_path, &backup) {
            Ok(removal_proof) => match sync_rename_parents(dst, dst_path, &backup) {
                Ok(()) => {
                    return Ok(ParkedTarget {
                        path: backup,
                        removal_proof,
                    });
                }
                Err(sync_error) => {
                    let sync_error = coded_error(
                        sync_error.kind(),
                        error_code(&sync_error),
                        format!(
                            "Typkonflikt wurde geparkt, aber der Verzeichnis-Sync scheiterte: {sync_error}"
                        ),
                    );
                    return Err(restore_parked_target(
                        dst,
                        &backup,
                        dst_path,
                        removal_proof.as_ref(),
                        sync_error,
                    ));
                }
            },
            Err(MutationFailure::Unchanged(error))
                if error.kind() == io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(MutationFailure::Unchanged(error)) => return Err(error),
            Err(MutationFailure::OutcomeUnclear(error)) => {
                let failure = coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Das Park-Rename meldete erst nach möglicher Wirkung einen Fehler ({error}). Kein ungebundener Rückweg wurde versucht. Recoverypfade: ursprünglicher Zielname: {}; mögliche Sicherung: {}",
                        dst_path.display(),
                        backup.display(),
                    ),
                );
                return Err(with_recovery_context(
                    failure,
                    &[&error],
                    &[dst_path, &backup],
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Kein freier Sicherungspfad für den Typkonflikt gefunden",
    ))
}

/// Entfernt einen von dieser Operation angelegten Zielordner nur dann, wenn der
/// Provider dessen bei der Anlage gebundene Identität bis zur Entfernung garantiert.
fn rollback_created_target(
    dst: &dyn Vfs,
    dst_path: &Path,
    backup: Option<&Path>,
    backup_proof: Option<&ProofListEntry>,
    created_proof: Option<&ProofListEntry>,
    cause: Option<io::Error>,
) -> io::Result<()> {
    // Zwischen dem letzten Zugriff auf den neuen Ordner und diesem Fehler kann ein
    // anderer Prozess seinen Namen ausgetauscht haben. Ein rekursives Löschen über
    // den bloßen Pfad könnte dann fremde Daten treffen. Ohne gehaltenen Deskriptor
    // oder atomare bedingte Entfernung lassen wir deshalb beide bekannten Namen
    // stehen und geben sie für die manuelle Wiederherstellung vollständig aus.
    let cause_text = cause
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "Operation wurde abgebrochen".to_string());
    let recovery = |detail: &str| match backup {
        Some(backup) => format!(
            "{detail}. Recoverypfade: sichtbarer Zielbaum: {}; geparktes Altziel: {}",
            dst_path.display(),
            backup.display()
        ),
        None => format!(
            "{detail}. Recoverypfad: sichtbarer Zielbaum: {}; kein Altziel wurde geparkt",
            dst_path.display()
        ),
    };
    let retained_paths = std::iter::once(dst_path).chain(backup).collect::<Vec<_>>();
    let failure = |message: String, secondary: Option<&io::Error>, paths: &[&Path]| {
        let causes = cause.iter().chain(secondary).collect::<Vec<_>>();
        with_recovery_context(
            coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear, message),
            &causes,
            paths,
        )
    };
    let Some(created_proof) = created_proof else {
        return Err(failure(
            recovery(&format!(
                "Der neu angelegte Zielordner konnte nach einem Fehler nicht gebunden werden ({cause_text}) und blieb unangetastet"
            )),
            None,
            &retained_paths,
        ));
    };
    if let Err(remove_error) = dst.remove_if_matches(dst_path, created_proof) {
        return Err(failure(
            recovery(&format!(
                "Der neu angelegte Zielordner konnte nach einem Fehler nicht identitätsgebunden entfernt werden ({cause_text}; Entfernung: {remove_error}) und blieb unangetastet"
            )),
            Some(&remove_error),
            &retained_paths,
        ));
    }
    if let Err(sync_error) = sync_parent(dst, dst_path) {
        let removed_recovery = match backup {
            Some(backup) => format!(
                "Recoverypfade: soeben entfernter Zielpfad: {}; geparktes Altziel: {}",
                dst_path.display(),
                backup.display()
            ),
            None => format!(
                "Recoverypfad: soeben entfernter Zielpfad: {}; kein Altziel wurde geparkt",
                dst_path.display()
            ),
        };
        return Err(failure(
            format!(
                "Der gebundene Zielordner wurde nach einem Fehler entfernt ({cause_text}), aber sein Elternordner konnte nicht dauerhaft synchronisiert werden ({sync_error}). {removed_recovery}"
            ),
            Some(&sync_error),
            &retained_paths,
        ));
    }

    let Some(backup) = backup else {
        return match cause {
            Some(error) => Err(error),
            None => Ok(()),
        };
    };
    let Some(backup_proof) = backup_proof else {
        return Err(failure(
            format!(
                "Der gebundene neue Zielordner wurde entfernt ({cause_text}), aber das geparkte Altziel besitzt keinen Identitätsbeleg und wurde deshalb nicht pfadbasiert zurückbenannt. Recoverypfade: freier Zielname: {}; geparktes Altziel: {}",
                dst_path.display(),
                backup.display(),
            ),
            None,
            &retained_paths,
        ));
    };
    if let Err(restore_error) = dst.rename_noreplace_if_matches(backup, dst_path, backup_proof) {
        return Err(failure(
            format!(
                "Der gebundene neue Zielordner wurde entfernt ({cause_text}), aber das identitätsgebundene Zurückbenennen des Altziels wurde mutationsfrei abgelehnt ({restore_error}). Recoverypfade: freier Zielname: {}; geparktes Altziel: {}",
                dst_path.display(),
                backup.display(),
            ),
            Some(&restore_error),
            &retained_paths,
        ));
    }
    match sync_rename_parents(dst, backup, dst_path) {
        Ok(()) => match cause {
            Some(error) => Err(error),
            None => Ok(()),
        },
        Err(sync_error) => Err(failure(
            format!(
                "Der gebundene neue Zielordner wurde entfernt ({cause_text}) und das Altziel nach {} zurückbenannt, aber der Verzeichnis-Sync scheiterte ({sync_error}). Recoverypfade: sichtbares Altziel: {}; früherer Sicherungspfad: {}",
                dst_path.display(),
                dst_path.display(),
                backup.display(),
            ),
            Some(&sync_error),
            &[dst_path],
        )),
    }
}

fn expected_replacement_for<'a>(
    expected: Option<&'a [FileConflictProof]>,
    path: &Path,
) -> io::Result<Option<&'a FileConflictProof>> {
    let Some(expected) = expected else {
        return Ok(None);
    };
    let path = path_to_utf8(path)?;
    expected
        .iter()
        .find(|proof| proof.path == path)
        .map(Some)
        .ok_or_else(|| {
            coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::PlanDrift,
                format!(
                    "Ziel „{}“ war nicht als Ersetzung bestätigt und bleibt unangetastet",
                    path
                ),
            )
        })
}

fn verify_expected_replacement(
    dst: &dyn Vfs,
    dst_path: &Path,
    expected: &FileConflictProof,
    listed: Option<&ProofListEntry>,
) -> io::Result<()> {
    if expected.path != path_to_utf8(dst_path)? {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Konfliktbeleg gehört zu einem anderen Zielpfad",
        ));
    }
    let owned_listing;
    let listed = match listed {
        Some(listed) => listed,
        None => {
            let parent = dst_path.parent().ok_or_else(|| {
                coded_error(
                    io::ErrorKind::InvalidInput,
                    ErrorCode::InvalidInput,
                    "Ersetzungsziel besitzt keinen Elternordner",
                )
            })?;
            let name = file_name(dst_path).ok_or_else(|| {
                coded_error(
                    io::ErrorKind::InvalidInput,
                    ErrorCode::InvalidInput,
                    "Ersetzungsziel besitzt keinen sicheren Namen",
                )
            })?;
            owned_listing = dst.list_complete(parent)?;
            require_complete_safe_listing(&owned_listing, "Ersetzungsbindung")?;
            unique_named_proof(&owned_listing, &name, None)?
        }
    };
    if !expected.matches_listed(listed) {
        return Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::PlanDrift,
            format!(
                "Das bestätigte Altziel „{}“ wurde vor dem Commit ausgetauscht",
                dst_path.display()
            ),
        ));
    }
    if let Some(expected_hash) = expected.content_hash {
        let actual = hash_vfs_file(dst, dst_path)?;
        if actual.as_bytes() != &expected_hash {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::PlanDrift,
                format!(
                    "Der Inhalt des bestätigten Altziels „{}“ änderte sich vor dem Commit",
                    dst_path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Vergleicht einen bereits umbenannten Alteintrag mit seinem Nachweis.
///
/// Der Name bleibt bewusst außen vor: Nach dem atomaren Verschieben liegt der
/// Eintrag unter einem anderen Namen, seine Identität muss aber unverändert sein.
fn moved_proof_matches(expected: &FileConflictProof, listed: &ProofListEntry) -> bool {
    expected.provider_token == listed.provider_token
        && expected.is_dir == listed.entry.is_dir
        && expected.is_file == listed.entry.is_file
        && expected.is_symlink == listed.entry.is_symlink
        && expected.size == listed.entry.size
        && expected.mtime == listed.entry.mtime
        && expected.mode == listed.entry.mode
        && expected.device == listed.entry.device
        && expected.inode == listed.entry.inode
}

/// Prüft den bereits geparkten Alteintrag gegen den gebundenen Nachweis.
///
/// Der Nachweis gehört zum Zielnamen (`bound_path`), der Eintrag liegt aber schon
/// exklusiv unter `parked_path`. Erst diese Reihenfolge — zuerst atomar in einen
/// exklusiv reservierten Sicherungsnamen verschieben, dann prüfen — schließt das
/// Rennen: Prüft man zuerst am Zielnamen, kann ein paralleler Lauf den Eintrag bis
/// zum Verschieben austauschen. Die Sicherung würde dann nach erfolgreichem Commit
/// gelöscht, obwohl ihre Identität nie geprüft wurde.
fn verify_parked_replacement(
    dst: &dyn Vfs,
    parked_path: &Path,
    bound_path: &Path,
    expected: &FileConflictProof,
) -> io::Result<ProofListEntry> {
    if expected.path != path_to_utf8(bound_path)? {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Konfliktbeleg gehört zu einem anderen Zielpfad",
        ));
    }
    let parent = parked_path.parent().ok_or_else(|| {
        coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Geparktes Altziel besitzt keinen Elternordner",
        )
    })?;
    let name = file_name(parked_path).ok_or_else(|| {
        coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Geparktes Altziel besitzt keinen sicheren Namen",
        )
    })?;
    let listing = dst.list_complete(parent)?;
    require_complete_safe_listing(&listing, "Ersetzungsbindung")?;
    let listed = unique_named_proof(&listing, &name, None)?;
    if !moved_proof_matches(expected, listed) {
        return Err(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::PlanDrift,
            format!(
                "Das bestätigte Altziel „{}“ wurde unmittelbar vor dem Commit ausgetauscht",
                bound_path.display()
            ),
        ));
    }
    if let Some(expected_hash) = expected.content_hash {
        if hash_vfs_file(dst, parked_path)?.as_bytes() != &expected_hash {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::PlanDrift,
                format!(
                    "Der Inhalt des bestätigten Altziels „{}“ änderte sich unmittelbar vor dem Commit",
                    bound_path.display()
                ),
            ));
        }
    }
    // Der exakte, unter dem Sicherungsnamen gelesene Providerbeleg reist bis zur
    // bedingten Entfernung mit. Nur Pfad und Metadaten erneut zu lesen würde das
    // gerade geschlossene Rennen unmittelbar wieder öffnen.
    Ok(listed.clone())
}

/// Entfernt eine geprüfte Sicherung nur über die bedingte VFS-Primitive.
///
/// Kann der konkrete Provider Prüfung und Entfernung nicht atomar beziehungsweise
/// handle-gebunden zusammenhalten, bleibt der Eintrag am Recoverypfad liegen. Das
/// neue Ziel ist dann bereits fertig; deshalb ist weder Erfolg noch ein gefahrloser
/// automatischer Rollback behauptbar und der Ausgang wird sichtbar als unklar gemeldet.
fn remove_verified_backup(
    dst: &dyn Vfs,
    backup: &Path,
    verified: &ProofListEntry,
) -> io::Result<()> {
    if let Err(error) = dst.remove_if_matches(backup, verified) {
        return Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Das neue Ziel wurde geschrieben, aber der Eintrag am Sicherungsnamen konnte nicht identitätsgebunden entfernt werden ({error}). Er blieb unangetastet. Recoverypfad: {}",
                backup.display()
            ),
        ));
    }
    sync_parent(dst, backup).map_err(|error| {
        coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Die identitätsgebundene Entfernung der Sicherung {} wurde ausgeführt, aber der Elternordner konnte nicht bestätigt werden: {error}",
                backup.display()
            ),
        )
    })
}

/// Entfernt eine geparkte Sicherung nur, wenn ihr Besitz bereits während des
/// Park-Renames gebunden oder danach gegen einen bestätigten Konflikt geprüft wurde.
fn remove_bound_backup(
    dst: &dyn Vfs,
    backup: &Path,
    removal_proof: Option<&ProofListEntry>,
) -> io::Result<()> {
    let Some(removal_proof) = removal_proof else {
        return Err(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Das neue Ziel wurde geschrieben, aber die Sicherung war beim Parken nicht atomar an diese Operation gebunden. Sie blieb unangetastet. Recoverypfad: {}",
                backup.display()
            ),
        ));
    };
    if dst.cleanup_staged_backup(backup, removal_proof)? {
        return Ok(());
    }
    remove_verified_backup(dst, backup, removal_proof)
}

/// Hält fest, ob die fertig geschriebene Temp-Datei nach einem Commitfehler noch
/// sicher dieser Operation gehört. Nur im ersten Fall darf der Aufrufer ihren bei
/// der Anlage gebundenen Entfernungsbeleg verwenden.
pub(crate) enum TempCommitFailure {
    /// Der Commit hat den Tempnamen nachweislich nicht verändert oder ein bereits
    /// ausgeführter Austausch wurde sichtbar zurückgetauscht.
    TempStillOwned(io::Error),
    /// Der Tempname wurde verschoben, hält jetzt das Altziel oder sein Zustand ist
    /// nach einer unklaren Providerantwort nicht beweisbar.
    TempMovedOrUnclear(io::Error),
    /// Das neue Ziel steht bereits; die Sicherung konnte nicht sicher entfernt
    /// werden. Ihr Pfad muss unabhängig vom Fehlertext in die Bilanz gelangen.
    ParkedBackupCleanup(io::Error, String),
    /// Wie `TempMovedOrUnclear`, zusätzlich mit der unmittelbar vor der einzigen
    /// Mutation festgehaltenen Best-Effort-Klasse. Ein Reconnect darf die aktuell
    /// gemeldeten Providerfähigkeiten zurücksetzen; die frühere Wirkung bleibt
    /// trotzdem unklar und darf nicht nachträglich als `false` erscheinen.
    BestEffortMovedOrUnclear(io::Error, UnclearBestEffortEffect),
}

impl TempCommitFailure {
    pub(crate) fn into_sync_error(self, temp_released: &mut bool) -> io::Error {
        match self {
            Self::TempStillOwned(error) => error,
            Self::TempMovedOrUnclear(error)
            | Self::BestEffortMovedOrUnclear(error, _)
            | Self::ParkedBackupCleanup(error, _) => {
                *temp_released = true;
                error
            }
        }
    }
    fn with_readback_recovery(
        self,
        listing: &crate::vfs::CompleteDirectoryListing,
        temp_path: &Path,
        dst_path: &Path,
        causes: &[&io::Error],
    ) -> Self {
        match self {
            Self::TempMovedOrUnclear(error) => {
                let mut paths = vec![dst_path];
                // Nur ein vollständiges, sicher dekodiertes Listing belegt die
                // Abwesenheit des Tempnamens. Sonst bleibt auch dieser Prüfpfad nötig.
                if temp_path.parent() != dst_path.parent()
                    || require_complete_safe_listing(listing, "Commit-Recovery").is_err()
                    || listing.entries.iter().any(|item| {
                        Some(item.entry.name.as_str())
                            == temp_path.file_name().and_then(|n| n.to_str())
                    })
                {
                    paths.push(temp_path);
                }
                Self::TempMovedOrUnclear(with_recovery_context(error, causes, &paths))
            }
            Self::TempStillOwned(error) => {
                Self::TempStillOwned(with_recovery_context(error, causes, &[]))
            }
            other => other,
        }
    }

    fn with_best_effort_effect(self, effect: UnclearBestEffortEffect) -> Self {
        match self {
            Self::TempMovedOrUnclear(error) => Self::BestEffortMovedOrUnclear(error, effect),
            other => other,
        }
    }
}

#[cfg(test)]
impl TempCommitFailure {
    fn into_error(self) -> io::Error {
        match self {
            Self::TempStillOwned(error)
            | Self::TempMovedOrUnclear(error)
            | Self::BestEffortMovedOrUnclear(error, _)
            | Self::ParkedBackupCleanup(error, _) => error,
        }
    }
}

/// Meldet den Zustand nach einem bereits erfolgreich ausgeführten Datei-Exchange.
///
/// Der Tempname hält jetzt das Altziel. Ein zweiter Exchange nur anhand derselben
/// zwei Pfade könnte inzwischen ausgetauschte fremde Einträge bewegen. Ohne eine
/// atomare beidseitige Identitätsbedingung bleibt der sichtbare Zustand deshalb
/// unangetastet und beide tatsächlichen Recoverypfade werden genannt.
fn restore_exchanged_target(
    temp_path: &Path,
    dst_path: &Path,
    cause: io::Error,
) -> TempCommitFailure {
    let failure = coded_error(
        io::ErrorKind::Other,
        ErrorCode::OutcomeUnclear,
        format!(
            "Der Dateitausch wurde sichtbar ausgeführt, der anschließende Beleg scheiterte ({cause}). Ohne atomare beidseitige Identitätsbedingung wurde kein pfadbasierter Rücktausch versucht. Recoverypfade: neue Datei am Ziel: {}; festgehaltenes Altziel am Temp-Pfad: {}",
            dst_path.display(),
            temp_path.display(),
        ),
    );
    TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
        failure,
        &[&cause],
        &[dst_path, temp_path],
    ))
}

/// Holt einen geparkten Eintrag an seinen Zielnamen zurück, ohne ihn zu löschen.
///
/// Wird nach Drift sowie nach Fehlern beim Anlegen oder Commit aufgerufen. Gelingt
/// der Rückweg nicht vollständig, nennt die Meldung beide Pfade und die exakte
/// Fehlerphase, damit der tatsächlich sichtbare Zustand auffindbar bleibt.
fn restore_parked_target(
    dst: &dyn Vfs,
    backup: &Path,
    dst_path: &Path,
    expected_backup: Option<&ProofListEntry>,
    cause: io::Error,
) -> io::Error {
    let failure = |message: String, secondary: Option<&io::Error>, paths: &[&Path]| {
        let causes = std::iter::once(&cause).chain(secondary).collect::<Vec<_>>();
        with_recovery_context(
            coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear, message),
            &causes,
            paths,
        )
    };
    let Some(expected_backup) = expected_backup else {
        return failure(
            format!(
                "Der vorherige Vorgang scheiterte ({cause}). Die Sicherungsquelle war nicht identitätsgebunden und wurde deshalb nicht pfadbasiert zurückbenannt. Recoverypfade: Zielname: {}; geparktes Altziel: {}",
                dst_path.display(),
                backup.display(),
            ),
            None,
            &[dst_path, backup],
        );
    };
    match dst.restore_staged_backup(backup, dst_path, expected_backup) {
        Ok(true) => return match sync_rename_parents(dst, backup, dst_path) {
            Ok(()) => cause,
            Err(error) => failure(
                format!("Altziel zurückbenannt, Verzeichnis-Sync unklar ({error}); Ziel: {}; Sicherung: {}",
                    dst_path.display(), backup.display()), Some(&error), &[dst_path]),
        },
        Ok(false) => {},
        Err(error) => {
            let message = format!(
                "Vorgang scheiterte ({cause}); privater Rückweg nicht bestätigt ({error:?}); Ziel: {}; Sicherung: {}",
                dst_path.display(), backup.display()
            );
            let error = error.into_error();
            return failure(message, Some(&error), &[dst_path, backup]);
        },
    }
    if let Err(restore_error) = dst.rename_noreplace_if_matches(backup, dst_path, expected_backup) {
        return failure(
            format!(
                "Der vorherige Vorgang scheiterte ({cause}). Das identitätsgebundene Zurückbenennen wurde mutationsfrei abgelehnt ({restore_error}); kein fremder Sicherungseintrag wurde bewegt. Recoverypfade: Zielname: {}; geparktes Altziel: {}",
                dst_path.display(),
                backup.display(),
            ),
            Some(&restore_error),
            &[dst_path, backup],
        );
    }
    match sync_rename_parents(dst, backup, dst_path) {
        Ok(()) => cause,
        Err(sync_error) => failure(
            format!(
                "Der vorherige Vorgang scheiterte ({cause}). Das gebundene Altziel wurde nach {} zurückbenannt, aber der Verzeichnis-Sync scheiterte ({sync_error}). Recoverypfade: sichtbares Altziel: {}; früherer Sicherungspfad: {}",
                dst_path.display(),
                dst_path.display(),
                backup.display(),
            ),
            Some(&sync_error),
            &[dst_path],
        ),
    }
}

/// Tauscht neue und alte Datei atomar aus und prüft anschließend die nun unter dem
/// Tempnamen festgehaltene Altdatei. So bleibt zwischen Identitätsprüfung und Commit
/// kein Rennen für einen dritten Prozess. Bei Drift bleibt der sichtbare Zustand
/// stehen: Ein zweiter, nur pfadgebundener Austausch könnte inzwischen fremde
/// Einträge bewegen.
///
/// Heute erreicht kein ausgelieferter Weg diese Funktion; außerhalb der Tests läuft
/// sie nicht. Sie verlangt beides zugleich: `FileReplaceClass::Assured` und einen
/// bestätigten Konfliktbeleg. `LocalVfs` meldet aber ausdrücklich `BestEffort`
/// (siehe `vfs.rs`), und bei einem Remote-Ziel mit `Assured` wandern die Konflikte
/// schon vor dem Commit in die Quarantäne, sodass dort kein Beleg mehr ankommt.
/// Wer den Zweig scharf schaltet, braucht deshalb zuerst einen echten Provider, der
/// `rename_exchange` beweist — der Default lehnt ihn als `Unsupported` ab.
// codereview-ok: 2026-09-12 — Der Zweig ist der absichtlich vorgehaltene Vertrag für
// einen künftigen Assured-Provider; Produktion kompiliert ihn, aber kein heutiger
// Provider kann die Kombination aus Assured und bestätigtem Restkonflikt liefern.
fn exchange_assured_replacement(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    expected: &FileConflictProof,
) -> Result<(), TempCommitFailure> {
    if expected.path != path_to_utf8(dst_path).map_err(TempCommitFailure::TempStillOwned)? {
        return Err(TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Konfliktbeleg gehört zu einem anderen Zielpfad",
        )));
    }
    dst.rename_exchange(temp_path, dst_path)
        .map_err(TempCommitFailure::TempStillOwned)?;
    if let Err(sync_error) = sync_parent(dst, dst_path) {
        return Err(restore_exchanged_target(temp_path, dst_path, sync_error));
    }

    let verify_old = (|| -> io::Result<ProofListEntry> {
        let parent = temp_path.parent().ok_or_else(|| {
            coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Temporäre Ersetzungsdatei besitzt keinen Elternordner",
            )
        })?;
        let temp_name = file_name(temp_path).ok_or_else(|| {
            coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Temporäre Ersetzungsdatei besitzt keinen sicheren Namen",
            )
        })?;
        let listing = dst.list_complete(parent)?;
        require_complete_safe_listing(&listing, "Atomare Ersetzungsbindung")?;
        let old = unique_named_proof(&listing, &temp_name, None)?;
        if !moved_proof_matches(expected, old) {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::PlanDrift,
                format!(
                    "Das bestätigte Altziel „{}“ wurde unmittelbar vor dem Commit ausgetauscht",
                    dst_path.display()
                ),
            ));
        }
        if let Some(expected_hash) = expected.content_hash {
            if hash_vfs_file(dst, temp_path)?.as_bytes() != &expected_hash {
                return Err(coded_error(
                    io::ErrorKind::AlreadyExists,
                    ErrorCode::PlanDrift,
                    format!(
                        "Der Inhalt des bestätigten Altziels „{}“ änderte sich unmittelbar vor dem Commit",
                        dst_path.display()
                    ),
                ));
            }
        }
        // Der unter dem Tempnamen geprüfte Providerbeleg muss bis zur bedingten
        // Entfernung mitreisen. Ein erneutes `stat` vor einem gewöhnlichen Löschen
        // würde nur ein weiteres Rennen zwischen Prüfung und Wirkung öffnen.
        Ok(old.clone())
    })();

    let verified_old = match verify_old {
        Ok(verified) => verified,
        Err(drift) => {
            return Err(restore_exchanged_target(temp_path, dst_path, drift));
        }
    };
    remove_verified_backup(dst, temp_path, &verified_old)
        .map_err(TempCommitFailure::TempMovedOrUnclear)
}

/// Eine vollständig geschriebene Temp-Datei atomar sichtbar machen. Reguläre Dateien
/// und Symlinks werden als Verzeichniseintrag ersetzt, ohne ihr Ziel zu öffnen. Ein
/// vorhandener Ordner wird erst geparkt; bei Commit-Fehler bleibt er wiederherstellbar.
struct TempFileCommitRequest<'a> {
    dst: &'a dyn Vfs,
    temp_path: &'a Path,
    dst_path: &'a Path,
    sandbox: &'a Option<PathBuf>,
    replace_existing: bool,
    expected_replacement: Option<&'a FileConflictProof>,
    expected_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TargetCommitEffect {
    Assured,
    BackupRetained(String),
    CopyNewBestEffort,
    ReplaceBestEffort,
}

fn commit_temp_file_with_phase(
    request: TempFileCommitRequest<'_>,
    name_verification: Option<&Cancel>,
) -> Result<TargetCommitEffect, TempCommitFailure> {
    let TempFileCommitRequest {
        dst,
        temp_path,
        dst_path,
        sandbox,
        replace_existing,
        expected_replacement,
        expected_size,
        expected_hash,
    } = request;
    guard_vfs(dst, sandbox, temp_path).map_err(TempCommitFailure::TempStillOwned)?;
    guard_vfs(dst, sandbox, dst_path).map_err(TempCommitFailure::TempStillOwned)?;

    let private_metadata = if dst.has_private_staging() {
        Some(
            dst.metadata_manifest(temp_path)
                .map_err(TempCommitFailure::TempStillOwned)?,
        )
    } else {
        None
    };

    // Wenn der Name beim Start frei war, muss er es bis zum atomaren Commit bleiben.
    // Ein spät auftauchender Eintrag ist kein stillschweigend genehmigter Konflikt.
    if !replace_existing {
        return commit_new_name(dst, temp_path, dst_path, expected_size, expected_hash);
    }

    match dst.stat(dst_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            commit_new_name(dst, temp_path, dst_path, expected_size, expected_hash)
        }
        Err(error) => Err(TempCommitFailure::TempStillOwned(error)),
        Ok(target) if (target.is_dir && !target.is_symlink) || dst.has_private_staging() => {
            // Wie im Ordnerzweig von `copy_tree`: erst atomar parken, dann den
            // geparkten Eintrag gegen den Nachweis prüfen. Nur so kann kein
            // paralleler Lauf das Altziel zwischen Prüfung und Verschieben
            // austauschen und dadurch fremden Inhalt in die Sicherung bringen.
            let parked = park_existing_target(dst, dst_path, sandbox)
                .map_err(TempCommitFailure::TempStillOwned)?;
            let verified_parked = match expected_replacement {
                Some(expected) => {
                    match verify_parked_replacement(dst, &parked.path, dst_path, expected) {
                        Ok(verified) => Some(verified),
                        Err(drift) => {
                            return Err(TempCommitFailure::TempStillOwned(restore_parked_target(
                                dst,
                                &parked.path,
                                dst_path,
                                parked.removal_proof.as_ref(),
                                drift,
                            )));
                        }
                    }
                }
                None => parked.removal_proof.clone(),
            };
            match rename_noreplace_with_phase(dst, temp_path, dst_path) {
                Ok(()) => {}
                Err(SyncedRenameFailure::RenameUnchanged(commit_error)) => {
                    return Err(TempCommitFailure::TempStillOwned(restore_parked_target(
                        dst,
                        &parked.path,
                        dst_path,
                        verified_parked.as_ref(),
                        commit_error,
                    )));
                }
                Err(SyncedRenameFailure::RenameOutcomeUnclear(commit_error)) => {
                    return Err(TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Das Commit-Rename meldete einen Fehler nach möglicher Wirkung ({commit_error}). Es wurde kein pfadbasierter Rückweg versucht. Recoverypfade: möglicher neuer Zielname: {}; möglicher Tempbestand: {}; geparktes Altziel: {}",
                            dst_path.display(),
                            temp_path.display(),
                            parked.path.display(),
                        ),
                    ), &[&commit_error], &[temp_path, dst_path, &parked.path])));
                }
                Err(SyncedRenameFailure::Sync(sync_error)) => {
                    return Err(TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Die neue Datei wurde nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({sync_error}). Die neue Datei ist jetzt sichtbar unter {}; das geparkte Altziel blieb unter {}. Recoverypfade: neue Datei: {}; geparktes Altziel: {}",
                            dst_path.display(),
                            dst_path.display(),
                            parked.path.display(),
                            dst_path.display(),
                            parked.path.display(),
                        ),
                    ), &[&sync_error], &[dst_path, &parked.path])));
                }
            }
            if dst.has_private_staging() {
                let verified_commit = (|| -> io::Result<()> {
                    let committed = dst.stat(dst_path)?;
                    if !committed.is_file
                        || committed.is_symlink
                        || expected_size.is_some_and(|size| committed.size != size)
                        || expected_hash.is_some_and(|hash| {
                            hash_vfs_file(dst, dst_path)
                                .map_or(true, |actual| actual.as_bytes() != &hash)
                        })
                    {
                        return Err(io::Error::other(
                            "Neues Ziel stimmt nach dem Commit nicht mit dem Tempinhalt überein",
                        ));
                    }
                    if private_metadata.as_ref().is_some_and(|expected| {
                        dst.metadata_manifest(dst_path)
                            .map_or(true, |actual| &actual != expected)
                    }) {
                        return Err(io::Error::other("Zielmetadaten änderten sich beim Commit"));
                    }
                    Ok(())
                })();
                if let Err(error) = verified_commit {
                    return Err(TempCommitFailure::ParkedBackupCleanup(
                        coded_error(
                            io::ErrorKind::Other,
                            ErrorCode::OutcomeUnclear,
                            format!(
                                "Zielcommit nicht bewiesen ({error}); Altversion blieb erhalten"
                            ),
                        ),
                        parked.path.to_string_lossy().into_owned(),
                    ));
                }
            }
            if let Err(error) = remove_bound_backup(dst, &parked.path, verified_parked.as_ref()) {
                if verified_parked.as_ref().is_some_and(|proof| {
                    dst.staged_backup_is_retained(&parked.path, proof)
                        .unwrap_or(false)
                }) {
                    return Ok(TargetCommitEffect::BackupRetained(
                        parked.path.to_string_lossy().into_owned(),
                    ));
                }
                return Err(TempCommitFailure::ParkedBackupCleanup(
                    coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear,
                        format!("Ziel geschrieben, Sicherungs-Cleanup oder Erhaltung nicht bestätigt ({error})")),
                    parked.path.to_string_lossy().into_owned(),
                ));
            }
            Ok(TargetCommitEffect::Assured)
        }
        Ok(_) => match dst.file_replace_class() {
            FileReplaceClass::Assured => {
                if let Some(expected) = expected_replacement {
                    exchange_assured_replacement(dst, temp_path, dst_path, expected)?;
                } else {
                    // Der direkte API-Weg ohne persistierte Queue-Bindung behält
                    // seine bisherige atomare Replace-Semantik. Rename und Sync
                    // bleiben getrennt, damit ein bereits verschwundener Tempname
                    // nach einem Sync-Fehler niemals bereinigt wird.
                    match dst.rename(temp_path, dst_path) {
                        Err(error) => Err(TempCommitFailure::TempStillOwned(error)),
                        Ok(()) => sync_rename_parents(dst, temp_path, dst_path).map_err(|error| {
                            TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
                                io::ErrorKind::Other,
                                ErrorCode::OutcomeUnclear,
                                format!(
                                    "Die neue Datei wurde nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({error}). Recoverypfad: sichtbare neue Datei: {}; der frühere Temp-Pfad ist jetzt frei: {}",
                                    dst_path.display(),
                                    dst_path.display(),
                                    temp_path.display(),
                                ),
                            ), &[&error], &[dst_path]))
                        }),
                    }?
                }
                Ok(TargetCommitEffect::Assured)
            }
            FileReplaceClass::BestEffort => {
                best_effort_replace_file_with_phase(
                    dst,
                    temp_path,
                    dst_path,
                    expected_replacement,
                    expected_size,
                    expected_hash,
                    name_verification,
                )
                .map_err(|failure| {
                    failure.with_best_effort_effect(UnclearBestEffortEffect::TargetReplace)
                })?;
                Ok(TargetCommitEffect::ReplaceBestEffort)
            }
        },
    }
}

/// Testzugang ohne Phaseninformation. Der Produktionsaufrufer verwendet immer die
/// Variante darüber, damit er nur einen nachweislich noch eigenen Tempnamen bereinigt.
#[cfg(test)]
fn commit_temp_file(request: TempFileCommitRequest<'_>) -> io::Result<()> {
    commit_temp_file_with_phase(request, Some(&Cancel::new()))
        .map(|_| ())
        .map_err(TempCommitFailure::into_error)
}

/// Ersetzt genau eine zuvor sichtbar bestätigte reguläre Datei ohne Quarantäne.
/// Der frische vollständige Elternbestand bindet Altziel und Temp-Datei unmittelbar
/// vor exakt einer Provider-Mutation. Danach belegen Temp-Verschwinden, Endname, Typ
/// sowie Inhalts-Hash den Ausgang. Bei Verbindungsverlust wird nur lesend neu verbunden.
#[cfg(test)]
fn best_effort_replace_file(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    expected_replacement: Option<&FileConflictProof>,
    expected_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
) -> io::Result<()> {
    best_effort_replace_file_with_phase(
        dst,
        temp_path,
        dst_path,
        expected_replacement,
        expected_size,
        expected_hash,
        Some(&Cancel::new()),
    )
    .map_err(TempCommitFailure::into_error)
}

fn best_effort_replace_file_with_phase(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    expected_replacement: Option<&FileConflictProof>,
    expected_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
    name_verification: Option<&Cancel>,
) -> Result<(), TempCommitFailure> {
    let parent = dst_path.parent().ok_or_else(|| {
        TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Ersetzen benötigt einen Elternordner",
        ))
    })?;
    let dst_name = file_name(dst_path).ok_or_else(|| {
        TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Zielname ist nicht sicher dekodierbar",
        ))
    })?;
    let temp_name = file_name(temp_path).ok_or_else(|| {
        TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Tempname ist nicht sicher dekodierbar",
        ))
    })?;
    let listing = dst
        .list_complete(parent)
        .map_err(TempCommitFailure::TempStillOwned)?;
    require_complete_safe_listing(&listing, "Best-Effort-Ersetzen")
        .map_err(TempCommitFailure::TempStillOwned)?;
    let same_parent = temp_path.parent() == Some(parent);
    let excluded = same_parent.then_some(temp_name.as_str());
    let old_target = unique_named_proof(&listing, &dst_name, excluded)
        .map_err(TempCommitFailure::TempStillOwned)?;
    if !(old_target.entry.is_file || old_target.entry.is_symlink) {
        return Err(TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Best-Effort-Ersetzen ist ausschließlich für eine vorhandene reguläre Datei oder einen bestätigten Symlink erlaubt",
        )));
    }
    if let Some(expected) = expected_replacement {
        verify_expected_replacement(dst, dst_path, expected, Some(old_target))
            .map_err(TempCommitFailure::TempStillOwned)?;
    }
    let temp_listing;
    let temp_parent_listing = if same_parent {
        &listing
    } else {
        temp_listing =
            commit_temp_listing(dst, temp_path).map_err(TempCommitFailure::TempStillOwned)?;
        &temp_listing
    };
    let temp = unique_named_proof(temp_parent_listing, &temp_name, None)
        .map_err(TempCommitFailure::TempStillOwned)?;
    if !temp.entry.is_file || temp.entry.is_symlink {
        return Err(TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Best-Effort-Ersetzen fand keine reguläre Temp-Datei",
        )));
    }
    if expected_size.is_some_and(|expected| temp.entry.size != expected) {
        return Err(TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Die Temp-Dateigröße driftete vor dem Best-Effort-Ersetzen",
        )));
    }

    let name_neighbors = match (expected_hash, name_verification) {
        (Some(_), Some(cancel)) => {
            prepare_replace_name_neighbors(dst, parent, &listing, &dst_name, excluded, cancel)
                .map_err(TempCommitFailure::TempStillOwned)?
        }
        _ => None,
    };
    if name_neighbors.is_some() {
        let cancel = name_verification.expect("Namensbeleg besitzt Abbruchkanal");
        if let Some(hash) = expected_replacement.and_then(|proof| proof.content_hash) {
            if hash_replace_name_neighbor(dst, dst_path, old_target.entry.size, cancel)
                .map_err(TempCommitFailure::TempStillOwned)?
                != hash
            {
                return Err(TempCommitFailure::TempStillOwned(replace_name_changed(
                    dst_path,
                )));
            }
        }
        if cancel.is_cancelled() {
            return Err(TempCommitFailure::TempStillOwned(io::Error::new(
                io::ErrorKind::Interrupted,
                "Ersetzen wurde vor dem Commit abgebrochen",
            )));
        }
    }

    match dst.rename_replace_plain_once(temp_path, dst_path) {
        Ok(()) => {}
        Err(error) if error_code(&error) == ErrorCode::ConnectionLost => {
            return classify_replace_after_connection_loss(
                dst,
                ReplaceReadbackContext {
                    parent,
                    temp_path,
                    dst_path,
                    temp_name: &temp_name,
                    dst_name: &dst_name,
                    expected_size,
                    expected_hash,
                    old_target,
                    name_neighbors: name_neighbors.as_deref(),
                    name_verification,
                },
                error,
            );
        }
        Err(error) => {
            return Err(TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Das Best-Effort-Ersetzen meldete einen Fehler ({error}); seine Wirkung ist nicht sicher mutationsfrei. Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                    temp_path.display(),
                    dst_path.display(),
                ),
            ), &[&error], &[temp_path, dst_path])))
        }
    }

    sync_rename_parents(dst, temp_path, dst_path).map_err(|error| {
        TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!("Die Datei wurde ersetzt, aber der Eltern-Sync scheiterte ({error})"),
            ),
            &[&error],
            &[dst_path],
        ))
    })?;

    let mut readback = dst.list_complete(parent).map_err(|error| {
        TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Der Server bestätigte das Ersetzen, der Rücklese-Beleg scheiterte aber ({error}). Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                temp_path.display(),
                dst_path.display(),
            ),
        ), &[&error], &[temp_path, dst_path]))
    })?;
    let mut exact_names = dst.has_exact_names();
    if let (Some(neighbors), Some(cancel)) = (name_neighbors.as_deref(), name_verification) {
        readback = refresh_replace_name_readback(
            dst, parent, &readback, &dst_name, excluded, neighbors, cancel,
        )
        .map_err(|error| {
            TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
                coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!("Der zusätzliche Namensbeleg des Ersetzens scheiterte: {error}"),
                ),
                &[&error],
                &[temp_path, dst_path],
            ))
        })?;
        exact_names = true;
    }
    let temp_present =
        commit_temp_present(dst, temp_path, dst_path, &readback).map_err(|error| {
            TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
                error,
                &[],
                &[temp_path, dst_path],
            ))
        })?;
    classify_replace_readback(
        &readback,
        (if same_parent { &temp_name } else { "" }, temp_present),
        &dst_name,
        expected_size,
        old_target,
        None,
        exact_names,
    )
    .map_err(|failure| failure.with_readback_recovery(&readback, temp_path, dst_path, &[]))?;
    verify_replaced_content(dst, dst_path, expected_hash)
        .map_err(TempCommitFailure::TempMovedOrUnclear)
}

fn verify_replaced_content(
    dst: &dyn Vfs,
    dst_path: &Path,
    expected_hash: Option<[u8; 32]>,
) -> io::Result<()> {
    let Some(expected_hash) = expected_hash else {
        return Ok(());
    };
    let actual_hash = hash_vfs_file(dst, dst_path).map_err(|error| {
        with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Die Zieldatei „{}“ konnte nicht inhaltlich rückgelesen werden: {error}",
                    dst_path.display()
                ),
            ),
            &[&error],
            &[dst_path],
        )
    })?;
    if actual_hash.as_bytes() != &expected_hash {
        return Err(with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Die Zieldatei „{}“ hat nicht den Hash der vollständig übertragenen Temp-Datei",
                    dst_path.display()
                ),
            ),
            &[],
            &[dst_path],
        ));
    }
    Ok(())
}

pub(crate) fn require_complete_safe_listing(
    listing: &crate::vfs::CompleteDirectoryListing,
    action: &str,
) -> io::Result<()> {
    require_complete_source_listing(listing, action, false)
}

fn require_complete_source_listing(
    listing: &crate::vfs::CompleteDirectoryListing,
    action: &str,
    allow_name_omissions: bool,
) -> io::Result<()> {
    if !listing.complete {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!("{action} benötigt ein nachweislich vollständiges Elternlisting"),
        ));
    }
    validate_copy_source_names(&listing.entries, action, allow_name_omissions)
}

/// Nur additive Copy-Quellen dürfen den ausdrücklich schwächeren Baumvertrag
/// nutzen. Zielbelege, Ersetzungen, Move-Abbau und Planner bleiben strikt.
fn copy_source_children(
    vfs: &dyn Vfs,
    path: &Path,
    action: &str,
) -> io::Result<Vec<ProofListEntry>> {
    Ok(copy_source_listing(vfs, path, action, false)?.entries)
}

struct CopySourceListing {
    entries: Vec<ProofListEntry>,
    omitted_names: Vec<Vec<u8>>,
}

fn copy_source_listing(
    vfs: &dyn Vfs,
    path: &Path,
    action: &str,
    allow_name_omissions: bool,
) -> io::Result<CopySourceListing> {
    let entries = if let Some(entries) = vfs.interactive_copy_children(path)? {
        validate_copy_source_names(&entries, action, allow_name_omissions)?;
        entries
    } else {
        let listing = vfs.list_complete(path)?;
        require_complete_source_listing(&listing, action, allow_name_omissions)?;
        listing.entries
    };
    let mut result = CopySourceListing {
        entries: Vec::with_capacity(entries.len()),
        omitted_names: Vec::new(),
    };
    for item in entries {
        if item.safely_decoded {
            result.entries.push(item);
        } else {
            result.omitted_names.push(item.raw_name);
        }
    }
    Ok(result)
}

fn validate_copy_source_names(
    entries: &[ProofListEntry],
    action: &str,
    allow_name_omissions: bool,
) -> io::Result<()> {
    if !allow_name_omissions {
        return require_safe_source_names(entries, action);
    }
    let mut raw_names = std::collections::HashSet::with_capacity(entries.len());
    for item in entries {
        // Auch ausgelassene Einträge müssen eindeutige einzelne rohe Namen sein.
        // Ihre Anzeigezeichenfolge wird dagegen niemals als Pfad interpretiert.
        if item.raw_name.is_empty()
            || item.raw_name.contains(&0)
            || item.raw_name.contains(&b'/')
            || item.raw_name == b"."
            || item.raw_name == b".."
            || !raw_names.insert(item.raw_name.as_slice())
        {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!("{action} traf einen ungültigen oder doppelten rohen Kindnamen"),
            ));
        }
        if item.safely_decoded && !is_safe_source_name(item) {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!("{action} traf einen ungültigen oder mehrdeutigen Kindnamen"),
            ));
        }
    }
    Ok(())
}

fn require_safe_source_names(entries: &[ProofListEntry], action: &str) -> io::Result<()> {
    let mut raw_names = std::collections::HashSet::with_capacity(entries.len());
    for item in entries {
        if !is_safe_source_name(item) {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!(
                    "{action} traf einen ungültigen, mehrdeutigen oder nicht sicher dekodierbaren Kindnamen"
                ),
            ));
        }
        if !raw_names.insert(item.raw_name.as_slice()) {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!("{action} traf einen doppelten rohen Kindnamen"),
            ));
        }
    }
    Ok(())
}

fn is_safe_source_name(item: &ProofListEntry) -> bool {
    let name = item.entry.name.as_str();
    let mut components = Path::new(name).components();
    item.safely_decoded
        && !name.is_empty()
        && !name.contains('\0')
        && matches!(
            (components.next(), components.next()),
            (Some(std::path::Component::Normal(_)), None)
        )
        && !item.raw_name.is_empty()
        && !item.raw_name.contains(&0)
        && !item.raw_name.contains(&b'/')
        && item.raw_name == name.as_bytes()
}

/// Sucht den bestätigten Namen im frischen Providerbestand.
///
/// Ein byte-exakter Treffer ist eindeutig — auch dann, wenn daneben ein Name liegt,
/// den ein normalisierender Server gleichsetzen würde. Genau das ist auf einem
/// case-sensitiven Dateisystem der Normalfall: `datei.txt` und `Datei.txt` sind zwei
/// verschiedene Einträge, und `Ä.txt` darf in NFC und NFD nebeneinander liegen.
/// Ohne diesen Vorrang brach eine gewöhnliche lokale Kopie mit „Name nicht
/// eindeutig" ab, bevor irgendetwas passierte — für die gesamte Auswahl, nicht nur
/// für den betroffenen Namen. Die konservative Äquivalenz ist für Server gedacht,
/// die beide Schreibweisen tatsächlich zusammenfallen lassen, und greift deshalb
/// erst, wenn es keinen exakten Treffer gibt.
fn unique_named_proof<'a>(
    listing: &'a crate::vfs::CompleteDirectoryListing,
    name: &str,
    excluded: Option<&str>,
) -> io::Result<&'a ProofListEntry> {
    let mut exact = listing
        .entries
        .iter()
        .filter(|item| excluded != Some(item.entry.name.as_str()) && item.entry.name == name);
    if let Some(first) = exact.next() {
        if exact.next().is_some() {
            return Err(coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                format!("Der Name „{name}“ kommt im Providerbestand doppelt vor"),
            ));
        }
        return Ok(first);
    }
    let mut matches = listing.entries.iter().filter(|item| {
        excluded != Some(item.entry.name.as_str()) && names_may_collide(&item.entry.name, name)
    });
    let first = matches.next().ok_or_else(|| {
        coded_error(
            io::ErrorKind::NotFound,
            ErrorCode::PlanDrift,
            format!("Der bestätigte Name „{name}“ fehlt unmittelbar vor der Mutation"),
        )
    })?;
    if matches.next().is_some() {
        return Err(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            format!("Der Name „{name}“ ist im Providerbestand nicht eindeutig"),
        ));
    }
    Ok(first)
}

fn same_proof(left: &ProofListEntry, right: &ProofListEntry) -> bool {
    left.raw_name == right.raw_name
        && left.provider_token == right.provider_token
        && left.entry.name == right.entry.name
        && left.entry.is_dir == right.entry.is_dir
        && left.entry.is_file == right.entry.is_file
        && left.entry.is_symlink == right.entry.is_symlink
        && left.entry.size == right.entry.size
        && left.entry.mtime == right.entry.mtime
        && left.entry.mode == right.entry.mode
        && left.entry.device == right.entry.device
        && left.entry.inode == right.entry.inode
}

struct ReplaceNameNeighbor {
    proof: ProofListEntry,
    hash: [u8; 32],
}

fn replace_name_changed(path: &Path) -> io::Error {
    with_recovery_context(
        coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::PlanDrift,
            format!(
                "Der Namens- oder Inhaltsbeleg änderte sich: {}",
                path.display()
            ),
        ),
        &[],
        &[path],
    )
}

fn hash_replace_name_neighbor(
    vfs: &dyn Vfs,
    path: &Path,
    size: u64,
    cancel: &Cancel,
) -> io::Result<[u8; 32]> {
    let result = (|| {
        // Das zusätzliche Byte erkennt Wachstum, ohne einen unbeschränkten
        // Serverstrom für einen bloßen Nachbarbeleg einzulesen.
        let limit = size
            .checked_add(1)
            .ok_or_else(|| replace_name_changed(path))?;
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Namensprüfung abgebrochen",
            ));
        }
        let mut reader = vfs.open_read(path)?.take(limit);
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut bytes = 0_u64;
        loop {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Namensprüfung abgebrochen",
                ));
            }
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            hasher.update(&buffer[..count]);
        }
        if bytes != size {
            return Err(replace_name_changed(path));
        }
        Ok(*hasher.finalize().as_bytes())
    })();
    result.map_err(|error| {
        with_recovery_context(
            crate::error::with_context(
                error,
                format!("Inhaltsbeleg für kollidierenden Namen {}", path.display()),
            ),
            &[],
            &[path],
        )
    })
}

fn verify_replace_name_neighbors(
    parent: &Path,
    listing: &crate::vfs::CompleteDirectoryListing,
    dst_name: &str,
    excluded: Option<&str>,
    neighbors: &[ReplaceNameNeighbor],
) -> io::Result<()> {
    require_complete_safe_listing(listing, "Namensbeleg des Ersetzens")?;
    let current: std::collections::BTreeMap<_, _> = listing
        .entries
        .iter()
        .filter(|entry| {
            entry.entry.name != dst_name
                && excluded != Some(entry.entry.name.as_str())
                && names_may_collide(&entry.entry.name, dst_name)
        })
        .map(|entry| (entry.raw_name.as_slice(), entry))
        .collect();
    if current.len() != neighbors.len()
        || neighbors.iter().any(|known| {
            current
                .get(known.proof.raw_name.as_slice())
                .is_none_or(|entry| !same_proof(entry, &known.proof))
        })
    {
        let paths: Vec<_> = current
            .values()
            .map(|entry| &entry.entry.name)
            .chain(neighbors.iter().map(|known| &known.proof.entry.name))
            .map(|name| parent.join(name).to_string_lossy().into_owned())
            .collect();
        return Err(crate::error::with_recovery_paths(
            coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::PlanDrift,
                "Äquivalente Nachbarnamen änderten sich während des Ersetzens",
            ),
            &paths,
        ));
    }
    Ok(())
}

fn prepare_replace_name_neighbors(
    dst: &dyn Vfs,
    parent: &Path,
    listing: &crate::vfs::CompleteDirectoryListing,
    dst_name: &str,
    excluded: Option<&str>,
    cancel: &Cancel,
) -> io::Result<Option<Vec<ReplaceNameNeighbor>>> {
    if dst.has_exact_names() {
        return Ok(None);
    }
    let target = unique_named_proof(listing, dst_name, excluded)?;
    if target.entry.name != dst_name {
        return Ok(None);
    }
    let colliding: Vec<_> = listing
        .entries
        .iter()
        .filter(|entry| {
            entry.entry.name != dst_name
                && excluded != Some(entry.entry.name.as_str())
                && names_may_collide(&entry.entry.name, dst_name)
        })
        .collect();
    if colliding.is_empty()
        || colliding
            .iter()
            .any(|entry| !entry.entry.is_file || entry.entry.is_symlink)
    {
        return Ok(None);
    }
    // Schwache Größen-/Zeitbelege reichen bei gleich langen Dateien nicht. Nur
    // tatsächlich kollidierende reguläre Nachbarn werden zusätzlich gehasht.
    let mut neighbors = Vec::with_capacity(colliding.len());
    for entry in colliding {
        neighbors.push(ReplaceNameNeighbor {
            hash: hash_replace_name_neighbor(
                dst,
                &parent.join(&entry.entry.name),
                entry.entry.size,
                cancel,
            )?,
            proof: entry.clone(),
        });
    }
    let current = dst.list_complete(parent)?;
    verify_replace_name_neighbors(parent, &current, dst_name, excluded, &neighbors)?;
    if !same_proof(target, unique_named_proof(&current, dst_name, excluded)?) {
        return Err(replace_name_changed(&parent.join(dst_name)));
    }
    if let Some(temp_name) = excluded {
        if !same_proof(
            unique_named_proof(listing, temp_name, None)?,
            unique_named_proof(&current, temp_name, None)?,
        ) {
            return Err(replace_name_changed(&parent.join(temp_name)));
        }
    }
    Ok(Some(neighbors))
}

fn refresh_replace_name_readback(
    dst: &dyn Vfs,
    parent: &Path,
    listing: &crate::vfs::CompleteDirectoryListing,
    dst_name: &str,
    excluded: Option<&str>,
    neighbors: &[ReplaceNameNeighbor],
    cancel: &Cancel,
) -> io::Result<crate::vfs::CompleteDirectoryListing> {
    verify_replace_name_neighbors(parent, listing, dst_name, excluded, neighbors)?;
    let target = unique_named_proof(listing, dst_name, excluded)?;
    if target.entry.name != dst_name {
        return Err(replace_name_changed(&parent.join(dst_name)));
    }
    for known in neighbors {
        let path = parent.join(&known.proof.entry.name);
        if hash_replace_name_neighbor(dst, &path, known.proof.entry.size, cancel)? != known.hash {
            return Err(replace_name_changed(&path));
        }
    }
    let current = dst.list_complete(parent)?;
    verify_replace_name_neighbors(parent, &current, dst_name, excluded, neighbors)?;
    if !same_proof(target, unique_named_proof(&current, dst_name, excluded)?) {
        return Err(replace_name_changed(&parent.join(dst_name)));
    }
    Ok(current)
}

/// Findet den bestätigten Zielnamen in einem Rücklese-Listing.
///
/// Bei einem Provider mit exakten Namen (`Vfs::has_exact_names`) ist ein byte-exakter
/// Treffer eindeutig, auch wenn daneben ein Name liegt, den ein normalisierender
/// Server gleichsetzen würde: Auf ext4 sind `datei.txt` und `Datei.txt` zwei
/// verschiedene Einträge, und ohne diesen Vorrang meldete der Rückleseweg dort „nicht
/// eindeutig" und stufte eine vollzogene, korrekte Ersetzung als unklaren Ausgang ein.
///
/// Für jeden anderen Provider bleibt die konservative Äquivalenz die Schranke: Ein
/// Server kann beim Schreiben normalisieren und trotzdem beide Schreibweisen listen;
/// ein exakter Treffer wäre dann kein Beweis, welches Objekt die Mutation getroffen
/// hat. Ein Mehrfachtreffer bleibt ohne weitere Belege mehrdeutig. Der Aufrufer
/// darf für diesen einen Lauf einen exakten Readback auch dann freigeben, wenn
/// vorherige äquivalente reguläre Nachbarn mit vollständigen Listings und ihren
/// unveränderten Inhaltshashes unabhängig bestätigt wurden.
///
/// Der zweite Rückgabewert sagt, ob der Treffer eindeutig ist.
fn readback_target<'a>(
    listing: &'a crate::vfs::CompleteDirectoryListing,
    temp_name: &str,
    dst_name: &str,
    exact_names: bool,
) -> (Option<&'a ProofListEntry>, bool) {
    if exact_names {
        let mut exact = listing
            .entries
            .iter()
            .filter(|item| item.entry.name != temp_name && item.entry.name == dst_name);
        if let Some(first) = exact.next() {
            return (Some(first), exact.next().is_none());
        }
    }
    let mut folded = listing.entries.iter().filter(|item| {
        item.entry.name != temp_name && names_may_collide(&item.entry.name, dst_name)
    });
    let first = folded.next();
    let unique = folded.next().is_none();
    (first, unique)
}

fn classify_replace_readback(
    listing: &crate::vfs::CompleteDirectoryListing,
    (temp_name, temp_present): (&str, bool),
    dst_name: &str,
    expected_size: Option<u64>,
    old_target: &ProofListEntry,
    lost: Option<&io::Error>,
    exact_names: bool,
) -> Result<(), TempCommitFailure> {
    require_complete_safe_listing(listing, "Rücklese-Beleg des Ersetzens").map_err(|error| {
        TempCommitFailure::TempMovedOrUnclear(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            error.to_string(),
        ))
    })?;
    // Auf normalisierenden oder uneindeutigen Providerbeständen kann derselbe
    // bestätigte Zielname mehrfach erscheinen. Ein beliebiger erster Treffer ist
    // kein Commitbeleg: Ohne globale Exaktnamen oder den zusätzlichen, frischen
    // Nachbarbeleg bleibt bei mehreren Treffern unklar, welcher Eintrag ersetzt wurde.
    let (target, target_is_unique) = readback_target(listing, temp_name, dst_name, exact_names);
    if !target_is_unique {
        return Err(TempCommitFailure::TempMovedOrUnclear(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Der Zielname ist im Rücklese-Listing nicht eindeutig",
        )));
    }
    match (temp_present, target) {
        (false, Some(target)) if target.entry.is_file && !target.entry.is_symlink => {
            if expected_size.is_some_and(|expected| target.entry.size != expected) {
                return Err(TempCommitFailure::TempMovedOrUnclear(coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    "Die ersetzte Zieldatei meldet eine unerwartete Größe",
                )));
            }
            Ok(())
        }
        (true, Some(target)) if same_proof(target, old_target) => {
            Err(TempCommitFailure::TempStillOwned(match lost {
                Some(lost) => coded_error(
                io::ErrorKind::ConnectionAborted,
                ErrorCode::ConnectionLost,
                format!(
                    "Verbindung brach ab, bevor das Ersetzen wirkte; Temp-Datei und gebundenes Altziel sind unverändert ({lost})"
                ),
            ),
                None => coded_error(
                io::ErrorKind::InvalidData,
                ErrorCode::InvalidData,
                "Der Server meldete Erfolg, Temp-Datei und Altziel blieben aber unverändert",
            ),
            }))
        }
        _ => Err(TempCommitFailure::TempMovedOrUnclear(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Temp- und Zielzustand erlauben keinen eindeutigen Beleg des Ersetzens",
        ))),
    }
}

struct ReplaceReadbackContext<'a> {
    parent: &'a Path,
    temp_path: &'a Path,
    dst_path: &'a Path,
    temp_name: &'a str,
    dst_name: &'a str,
    expected_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
    old_target: &'a ProofListEntry,
    name_neighbors: Option<&'a [ReplaceNameNeighbor]>,
    name_verification: Option<&'a Cancel>,
}

fn classify_replace_after_connection_loss(
    dst: &dyn Vfs,
    context: ReplaceReadbackContext<'_>,
    lost: io::Error,
) -> Result<(), TempCommitFailure> {
    if let Err(reclaim_error) = dst.reclaim_verification_session() {
        return Err(TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Verbindung brach während des Ersetzens ab ({lost}); die Zustandsklärung scheiterte ({reclaim_error}). Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                context.temp_path.display(),
                context.dst_path.display(),
            ),
        ), &[&lost, &reclaim_error], &[context.temp_path, context.dst_path])));
    }
    let mut listing = dst.list_complete(context.parent).map_err(|listing_error| {
        TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!("Verbindung brach während des Commits ab ({lost}); das klärende Listing scheiterte ({listing_error})"),
            ),
            &[&lost, &listing_error],
            &[context.temp_path, context.dst_path],
        ))
    })?;
    let mut exact_names = dst.has_exact_names();
    if let (Some(neighbors), Some(cancel)) = (context.name_neighbors, context.name_verification) {
        listing = refresh_replace_name_readback(
            dst, context.parent, &listing, context.dst_name,
            (context.temp_path.parent() == Some(context.parent)).then_some(context.temp_name),
            neighbors, cancel,
        ).map_err(|error| TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
            coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear,
                format!("Der zusätzliche Namensbeleg nach dem Verbindungsabbruch scheiterte: {error}")),
            &[&lost, &error], &[context.temp_path, context.dst_path],
        )))?;
        exact_names = true;
    }
    let temp_present = commit_temp_present(dst, context.temp_path, context.dst_path, &listing)
        .map_err(|error| {
            TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
                error,
                &[&lost],
                &[context.temp_path, context.dst_path],
            ))
        })?;
    classify_replace_readback(
        &listing,
        (
            if context.temp_path.parent() == Some(context.parent) {
                context.temp_name
            } else {
                ""
            },
            temp_present,
        ),
        context.dst_name,
        context.expected_size,
        context.old_target,
        Some(&lost),
        exact_names,
    )
    .map_err(|failure| {
        failure.with_readback_recovery(&listing, context.temp_path, context.dst_path, &[&lost])
    })?;
    verify_replaced_content(dst, context.dst_path, context.expected_hash).map_err(|error| {
        TempCommitFailure::TempMovedOrUnclear(with_recovery_context(error, &[&lost], &[]))
    })
}

fn target_file_commit_class(dst: &dyn Vfs, target: &Path) -> io::Result<FileCommitClass> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Ziel benötigt einen Elternordner",
        )
    })?;
    dst.file_commit_class_at(parent)
}

/// Hängt eine fertige Temp-Datei an einen als frei bestätigten Zielnamen — in der
/// stärksten Commit-Klasse, die das konkrete Ziel beweist.
fn commit_new_name(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    expected_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
) -> Result<TargetCommitEffect, TempCommitFailure> {
    let parent = dst_path.parent().ok_or_else(|| {
        TempCommitFailure::TempStillOwned(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Commit benötigt einen Elternordner",
        ))
    })?;
    match dst
        .file_commit_class_at(parent)
        .map_err(TempCommitFailure::TempStillOwned)?
    {
        FileCommitClass::Assured => match rename_noreplace_with_phase(dst, temp_path, dst_path) {
            Ok(()) => Ok(TargetCommitEffect::Assured),
            Err(SyncedRenameFailure::RenameUnchanged(error)) => {
                Err(TempCommitFailure::TempStillOwned(error))
            }
            Err(SyncedRenameFailure::RenameOutcomeUnclear(error)) => {
                let failure = coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Das Commit-Rename meldete einen Fehler nach möglicher Wirkung ({error}). Es wurde kein Rückweg versucht. Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                        temp_path.display(),
                        dst_path.display(),
                    ),
                );
                Err(TempCommitFailure::TempMovedOrUnclear(
                    with_recovery_context(failure, &[&error], &[temp_path, dst_path]),
                ))
            }
            Err(SyncedRenameFailure::Sync(sync_error)) => {
                let failure = coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Die neue Datei wurde nach {} umbenannt, aber der Verzeichnis-Sync scheiterte ({sync_error}). Recoverypfad: sichtbare neue Datei: {}; der frühere Temp-Pfad ist jetzt frei: {}",
                        dst_path.display(),
                        dst_path.display(),
                        temp_path.display(),
                    ),
                );
                Err(TempCommitFailure::TempMovedOrUnclear(
                    with_recovery_context(failure, &[&sync_error], &[dst_path]),
                ))
            }
        },
        FileCommitClass::BestEffort => {
            best_effort_commit_new_with_phase(
                dst,
                temp_path,
                dst_path,
                expected_size,
                expected_hash,
            )
            .map_err(|failure| {
                failure.with_best_effort_effect(UnclearBestEffortEffect::TargetCopyNew)
            })?;
            Ok(TargetCommitEffect::CopyNewBestEffort)
        }
    }
}

/// Gemeinsamer Commit des bestätigten lokalen Syncplans. Der Sync behält Transfer,
/// Quellhash und Zielvorzustand; nur die schon bewährte Datei-Commitmechanik wird
/// wiederverwendet. `None` bezeichnet ausschließlich eine bestätigte Addition.
pub(crate) fn commit_sync_file(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    size: u64,
    hash: [u8; 32],
    replacement: Option<&FileConflictProof>,
) -> Result<(), TempCommitFailure> {
    if let Some(expected) = replacement {
        best_effort_replace_file_with_phase(
            dst,
            temp_path,
            dst_path,
            Some(expected),
            Some(size),
            Some(hash),
            // Dieser versiegelte lokale Syncpfad hat exakte Namen und benötigt
            // keine zusätzlichen, abbrechbaren Remote-Nachbarbelege.
            None,
        )
    } else {
        commit_new_name(dst, temp_path, dst_path, Some(size), Some(hash)).map(|_| ())
    }
}

fn commit_temp_listing(
    dst: &dyn Vfs,
    temp_path: &Path,
) -> io::Result<crate::vfs::CompleteDirectoryListing> {
    let parent = temp_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Tempdatei ohne Elternordner")
    })?;
    let listing = dst.list_complete(parent)?;
    require_complete_safe_listing(&listing, "Temp-Elternbestand des Commits")?;
    Ok(listing)
}

fn commit_temp_present(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    target_listing: &crate::vfs::CompleteDirectoryListing,
) -> io::Result<bool> {
    let result = (|| {
        let owned;
        let listing = if temp_path.parent() == dst_path.parent() {
            target_listing
        } else {
            owned = commit_temp_listing(dst, temp_path)?;
            &owned
        };
        require_complete_safe_listing(listing, "Temp-Readback des Commits")?;
        let name = temp_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Tempname ist nicht UTF-8")
            })?;
        if listing.entries.iter().any(|item| item.entry.name == name) {
            return Ok(true);
        }
        if !dst.has_exact_names() {
            let parent = temp_path
                .parent()
                .expect("Temp-Listing besitzt Elternordner");
            let possible: Vec<_> = listing
                .entries
                .iter()
                .filter(|item| names_may_collide(&item.entry.name, name))
                .map(|item| parent.join(&item.entry.name).to_string_lossy().into_owned())
                .collect();
            if !possible.is_empty() {
                // Ein nur normalisiert sichtbarer Name beweist weder Abwesenheit
                // noch Eigentum. Er darf deshalb auch keinen Temp-Cleanup freigeben.
                return Err(crate::error::with_recovery_paths(coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    "Der Tempname blieb möglicherweise unter normalisierter Schreibweise erhalten",
                ), &possible));
            }
        }
        Ok(false)
    })();
    result.map_err(|error: io::Error| {
        with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                error.to_string(),
            ),
            &[&error],
            &[temp_path, dst_path],
        )
    })
}

/// Best-Effort-Commit für Provider ohne jede starke lokale Garantie: frisches,
/// vollständiges Elternlisting mit konservativer Kollisionsprüfung, genau ein
/// gewöhnliches Rename, anschließend Listing-Readback. Das kann ein fehlender
/// No-Replace-Beweis oder — bei SFTP — fehlendes dauerhaftes Datei-Flush sein. Nach einem
/// Verbindungsverlust klärt höchstens ein weiteres Listing den Zustand — die
/// Mutation wird niemals wiederholt; ein offener Ausgang trägt `outcome_unclear`.
#[cfg(test)]
fn best_effort_commit_new(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    expected_size: Option<u64>,
) -> io::Result<()> {
    best_effort_commit_new_with_phase(dst, temp_path, dst_path, expected_size, None)
        .map_err(TempCommitFailure::into_error)
}

fn best_effort_commit_new_with_phase(
    dst: &dyn Vfs,
    temp_path: &Path,
    dst_path: &Path,
    expected_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
) -> Result<(), TempCommitFailure> {
    let parent = dst_path.parent().ok_or_else(|| {
        TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Commit benötigt einen Elternordner",
        ))
    })?;
    let dst_name = file_name(dst_path).ok_or_else(|| {
        TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Zielname ist nicht sicher dekodierbar",
        ))
    })?;
    // Die Anwesenheitsprüfung muss den wirklichen Tempnamen in dessen eigenem
    // Elternlisting suchen. Ein leerer Ersatzname wäre kein Abwesenheitsbeleg.
    let temp_name = file_name(temp_path).ok_or_else(|| {
        TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Tempname ist nicht sicher dekodierbar",
        ))
    })?;

    // Frische, nachweislich vollständige Bestandsprüfung: Sie allein trägt hier
    // das Verbot des stillen Ersetzens (siehe remote_rename für das Muster).
    let listing = dst
        .list_complete(parent)
        .map_err(TempCommitFailure::TempStillOwned)?;
    require_complete_safe_listing(&listing, "Best-Effort-Neunamencommit")
        .map_err(TempCommitFailure::TempStillOwned)?;
    let same_parent = temp_path.parent() == Some(parent);
    let temp_listing;
    let temp_listing = if same_parent {
        &listing
    } else {
        temp_listing =
            commit_temp_listing(dst, temp_path).map_err(TempCommitFailure::TempStillOwned)?;
        &temp_listing
    };
    let temp = unique_named_proof(temp_listing, &temp_name, None)
        .map_err(TempCommitFailure::TempStillOwned)?;
    if !temp.entry.is_file
        || temp.entry.is_symlink
        || expected_size.is_some_and(|size| size != temp.entry.size)
    {
        return Err(TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::InvalidData,
            ErrorCode::InvalidData,
            "Die reguläre Tempdatei fehlt oder ihre Größe driftete vor dem Commit",
        )));
    }
    if let Some(existing) = listing.entries.iter().find(|item| {
        (!same_parent || item.entry.name != temp_name)
            && names_may_collide(&item.entry.name, &dst_name)
    }) {
        // Späte Kollision: sichtbar stoppen statt still zu ersetzen. Der Aufrufer
        // entfernt die Temp-Datei; das vorhandene Ziel bleibt unangetastet.
        return Err(TempCommitFailure::TempStillOwned(coded_error(
            io::ErrorKind::AlreadyExists,
            ErrorCode::AlreadyExists,
            format!(
                "„{}“ ist inzwischen belegt ({}); die neue Datei wurde nicht committet",
                dst_name,
                if existing.entry.is_dir {
                    "ein vorhandener Ordner"
                } else {
                    "eine vorhandene Datei"
                }
            ),
        )));
    }

    // Genau eine Mutation — niemals ein blinder Wiederholversuch.
    match dst.rename_plain_once_with_phase(temp_path, dst_path) {
        Ok(()) => {}
        Err(MutationFailure::OutcomeUnclear(error)) if error_code(&error) == ErrorCode::ConnectionLost => {
            return classify_commit_after_connection_loss(
                dst,
                NewCommitReadbackContext {
                    parent,
                    temp_path,
                    temp_name: &temp_name,
                    dst_name: &dst_name,
                    dst_path,
                    expected_size,
                    expected_hash,
                },
                error,
            );
        }
        Err(MutationFailure::Unchanged(error)) => {
            return Err(TempCommitFailure::TempStillOwned(error))
        }
        Err(MutationFailure::OutcomeUnclear(error)) => {
            return Err(TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!(
                    "Der Best-Effort-Commit meldete einen Fehler ({error}); seine Wirkung ist nicht sicher mutationsfrei. Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                    temp_path.display(),
                    dst_path.display(),
                ),
            ), &[&error], &[temp_path, dst_path])))
        }
    }

    // Listing-Readback: Erst der frische Bestand macht die Serverantwort zum Beleg.
    let readback = dst.list_complete(parent).map_err(|error| {
        TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Der Server bestätigte den Commit, der Rücklese-Beleg scheiterte aber ({error}). Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                temp_path.display(),
                dst_path.display(),
            ),
        ), &[&error], &[temp_path, dst_path]))
    })?;
    let temp_present =
        commit_temp_present(dst, temp_path, dst_path, &readback).map_err(|error| {
            TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
                error,
                &[],
                &[temp_path, dst_path],
            ))
        })?;
    classify_commit_readback(
        &readback,
        if same_parent { &temp_name } else { "" },
        temp_present,
        &dst_name,
        expected_size,
        None,
        dst.has_exact_names(),
    )
    .map_err(|failure| failure.with_readback_recovery(&readback, temp_path, dst_path, &[]))?;
    verify_replaced_content(dst, dst_path, expected_hash)
        .map_err(TempCommitFailure::TempMovedOrUnclear)
}

/// Ordnet den frischen Bestand nach dem Commit einem eindeutigen Ergebnis zu.
fn classify_commit_readback(
    listing: &crate::vfs::CompleteDirectoryListing,
    temp_name: &str,
    temp_present: bool,
    dst_name: &str,
    expected_size: Option<u64>,
    lost: Option<&io::Error>,
    exact_names: bool,
) -> Result<(), TempCommitFailure> {
    require_complete_safe_listing(listing, "Rücklese-Beleg des Neunamencommits").map_err(
        |error| {
            TempCommitFailure::TempMovedOrUnclear(coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                error.to_string(),
            ))
        },
    )?;
    let (target, target_is_unique) = readback_target(listing, temp_name, dst_name, exact_names);
    match (temp_present, target, target_is_unique) {
        (false, Some(target), true) if target.entry.is_file && !target.entry.is_symlink => {
            // Zusätzlicher Beleg über die bekannte Bytezahl: Gerade nach einem
            // Verbindungsverlust gibt es keine Serverantwort — Name, Typ und
            // Größe zusammen machen den Erfolg erst belastbar.
            if let Some(expected) = expected_size {
                if target.entry.size != expected {
                    return Err(TempCommitFailure::TempMovedOrUnclear(coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!(
                            "Der Zielname existiert, meldet aber {} statt {} Byte; der Bestand muss geprüft werden",
                            target.entry.size, expected
                        ),
                    )));
                }
            }
            Ok(())
        }
        (true, None, true) => {
            // Der Bestand ist eindeutig unverändert: Die Mutation wirkte nicht und
            // der gebundene Tempname darf deshalb noch bedingt bereinigt werden.
            Err(TempCommitFailure::TempStillOwned(match lost {
                Some(lost) => coded_error(
                    io::ErrorKind::ConnectionAborted,
                    ErrorCode::ConnectionLost,
                    format!(
                        "Verbindung brach ab, bevor der Commit wirkte; das frische Listing bestätigt den unveränderten Bestand ({lost})"
                    ),
                ),
                None => coded_error(
                    io::ErrorKind::InvalidData,
                    ErrorCode::InvalidData,
                    "Das frische Listing zeigt den unveränderten Bestand — der Commit wurde nicht ausgeführt",
                ),
            }))
        }
        _ => Err(TempCommitFailure::TempMovedOrUnclear(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            "Temp- und Zielname erlauben keinen eindeutigen Commit-Beleg; der Bestand muss geprüft werden",
        ))),
    }
}

struct NewCommitReadbackContext<'a> {
    parent: &'a Path,
    temp_path: &'a Path,
    temp_name: &'a str,
    dst_name: &'a str,
    dst_path: &'a Path,
    expected_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
}

/// Klärt nach einem Verbindungsverlust ausschließlich den Zustand des Commits.
fn classify_commit_after_connection_loss(
    dst: &dyn Vfs,
    context: NewCommitReadbackContext<'_>,
    lost: io::Error,
) -> Result<(), TempCommitFailure> {
    // Höchstens ein identitätsgeprüfter Neuaufbau, ausschließlich für das
    // klärende Listing. Scheitert er, bleibt der Ausgang ehrlich unklar.
    if let Err(reclaim_error) = dst.reclaim_verification_session() {
        return Err(TempCommitFailure::TempMovedOrUnclear(with_recovery_context(coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "Verbindung brach während des Commits ab ({lost}); die Zustandsklärung scheiterte ({reclaim_error}). Recoverypfade: möglicher Tempbestand: {}; mögliches Ziel: {}",
                context.temp_path.display(),
                context.parent.join(context.dst_name).display(),
            ),
        ), &[&lost, &reclaim_error], &[context.temp_path, context.dst_path])));
    }
    let listing = dst.list_complete(context.parent).map_err(|listing_error| {
        TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                format!("Verbindung brach während des Commits ab ({lost}); das klärende Listing scheiterte ({listing_error})"),
            ),
            &[&lost, &listing_error],
            &[context.temp_path, context.dst_path],
        ))
    })?;
    let temp_present = commit_temp_present(dst, context.temp_path, context.dst_path, &listing)
        .map_err(|error| {
            TempCommitFailure::TempMovedOrUnclear(with_recovery_context(
                error,
                &[&lost],
                &[context.temp_path, context.dst_path],
            ))
        })?;
    classify_commit_readback(
        &listing,
        if context.temp_path.parent() == Some(context.parent) {
            context.temp_name
        } else {
            ""
        },
        temp_present,
        context.dst_name,
        context.expected_size,
        Some(&lost),
        dst.has_exact_names(),
    )
    .map_err(|failure| {
        failure.with_readback_recovery(&listing, context.temp_path, context.dst_path, &[&lost])
    })?;
    verify_replaced_content(dst, context.dst_path, context.expected_hash).map_err(|error| {
        TempCommitFailure::TempMovedOrUnclear(with_recovery_context(error, &[&lost], &[]))
    })
}

/// Pumpt eine einzelne Datei Byte für Byte in eine NEUE temporäre Nachbardatei, meldet
/// Fortschritt und reagiert auf Abbruch. Erst nach vollständigem Flush, mtime und
/// Datenträger-Sync wird atomar committed. Dadurch bleibt ein vorhandenes Ziel bei
/// Abbruch oder Fehler vollständig erhalten.
struct FileTransferRequest<'a> {
    src: &'a dyn Vfs,
    src_path: &'a Path,
    dst: &'a dyn Vfs,
    dst_path: &'a Path,
    sandbox: &'a Option<PathBuf>,
    replace_existing: bool,
    expected_replacement: Option<&'a FileConflictProof>,
    source_mtime: Option<i64>,
    source_mode: Option<u32>,
}

/// Legt einen kollisionsfreien Datei-Tempnachbarn an und übernimmt nur einen Beleg,
/// den der Provider untrennbar mit genau dieser Anlage verbunden hat.
struct CreatedFileopTemp {
    path: PathBuf,
    writer: Box<dyn io::Write + Send>,
    removal_proof: Option<ProofListEntry>,
}

fn create_fileop_temp(
    dst: &dyn Vfs,
    dst_path: &Path,
    sandbox: &Option<PathBuf>,
) -> Result<CreatedFileopTemp, FileCopyFailure> {
    for _ in 0..100 {
        let candidate = dst.staging_candidate(dst_path, "fileop-tmp")?;
        guard_vfs(dst, sandbox, &candidate)?;
        match dst.open_write_new_with_removal_proof(&candidate) {
            Ok((writer, removal_proof)) => {
                return Ok(CreatedFileopTemp {
                    path: candidate,
                    writer,
                    removal_proof,
                });
            }
            Err(MutationFailure::Unchanged(error))
                if error.kind() == io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(MutationFailure::Unchanged(error)) => return Err(error.into()),
            Err(MutationFailure::OutcomeUnclear(error)) => {
                let recovery_path = candidate.to_string_lossy().into_owned();
                let failure = coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!(
                        "Das exklusive Anlegen der Temp-Datei meldete erst nach möglicher Wirkung einen Fehler ({error}). Ohne gebundenen Beleg blieb der Name unangetastet. Temp-Recoverypfad: {}",
                        candidate.display(),
                    ),
                );
                return Err(FileCopyFailure {
                    error: with_recovery_context(failure, &[&error], &[&candidate]),
                    unclear_effect: None,
                    temp_recovery_path: Some(recovery_path),
                });
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Kein freier temporärer Dateiname gefunden",
    )
    .into())
}

// codereview-ok: Die erste Rustdoc-Zeile beschreibt den Sicherheitsmechanismus;
// die kurze Aufräumzusammenfassung ergänzt denselben Absatz bewusst (2026-09-12).
/// Entfernt eine Dateioperations-Tempdatei ausschließlich über den bei ihrer Anlage
/// atomar gebundenen Besitzbeleg. Ein späteres Listing oder Hashen beweist nur den
/// gerade sichtbaren Inhalt; zwischen Prüfung und Wirkung könnte ein fremder Prozess
/// denselben Namen austauschen. Ohne Anlagebeleg bleibt der genaue Recoverypfad stehen.
/// Räumt die eigene Temp-Datei eines gescheiterten Kopierschritts auf.
///
/// Bewusst ohne Größe oder Hash: Die Entfernung hängt allein am bei der Anlage
/// gebundenen Beleg. Übertragene Bytes und der bis dahin gebildete Hash wären hier
/// kein Besitznachweis, sondern nur ein zweiter, schwächerer Anhaltspunkt — die
/// Signatur trug sie deshalb bislang ungenutzt mit sich.
fn cleanup_fileop_temp(
    dst: &dyn Vfs,
    temp_path: &Path,
    removal_proof: Option<&ProofListEntry>,
    reason: &str,
) -> Result<Option<String>, FileCopyFailure> {
    if dst.cleanup_owned_temp(temp_path)? {
        return Ok(None);
    }
    let Some(removal_proof) = removal_proof else {
        // Sicherheitsgrenze: Ein zufälliger eigener Name ist noch kein Besitzbeleg.
        // Darum gibt es hier absichtlich weder stat noch Listing noch Hash als
        // Ersatzbeweis und vor allem keinen pfadbasierten Löschversuch.
        return Ok(Some(temp_path.to_string_lossy().into_owned()));
    };
    let remove_result = dst.remove_if_matches(temp_path, removal_proof);
    if let Err(remove_error) = remove_result {
        let failure = coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "{reason}. Die Tempdatei konnte nicht bis zur Entfernung identitätsgebunden werden ({remove_error}) und blieb unangetastet. Temp-Recoverypfad: {}",
                temp_path.display()
            ),
        );
        return Err(FileCopyFailure {
            error: with_recovery_context(failure, &[&remove_error], &[temp_path]),
            unclear_effect: None,
            temp_recovery_path: Some(temp_path.to_string_lossy().into_owned()),
        });
    }
    sync_parent(dst, temp_path).map_err(|sync_error| {
        let failure = coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "{reason}. Die eigene Tempdatei wurde entfernt, aber der Elternordner konnte nicht dauerhaft synchronisiert werden ({sync_error}). Früherer Temp-Pfad: {}",
                temp_path.display()
            ),
        );
        FileCopyFailure::from(with_recovery_context(failure, &[&sync_error], &[temp_path]))
    })?;
    Ok(None)
}

struct FileCopyOutcome {
    complete: bool,
    temp_recovery_path: Option<String>,
    target_copy_new_best_effort: bool,
    target_replace_best_effort: bool,
}

struct FileCopyFailure {
    error: io::Error,
    unclear_effect: Option<UnclearBestEffortEffect>,
    temp_recovery_path: Option<String>,
}

impl From<io::Error> for FileCopyFailure {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            unclear_effect: None,
            temp_recovery_path: None,
        }
    }
}

fn with_temp_recovery(error: io::Error, recovery_path: Option<&str>) -> io::Error {
    let Some(recovery_path) = recovery_path else {
        return error;
    };
    let failure = coded_error(
        error.kind(),
        error_code(&error),
        format!(
            "{error}. Das bestätigte Ziel blieb unverändert; der unbelegte Tempname wurde nicht pfadbasiert entfernt und kann inzwischen einen fremden Eintrag bezeichnen. Möglicher Temp-Recoverypfad: {recovery_path}"
        ),
    );
    with_recovery_context(failure, &[&error], &[Path::new(recovery_path)])
}

fn fail_file_copy_before_commit(
    dst: &dyn Vfs,
    temp_path: &Path,
    removal_proof: Option<&ProofListEntry>,
    cause: io::Error,
    action: &str,
) -> FileCopyFailure {
    let reason = format!("{action}: {cause}");
    match cleanup_fileop_temp(dst, temp_path, removal_proof, &reason) {
        Ok(temp_recovery_path) => FileCopyFailure {
            error: with_temp_recovery(cause, temp_recovery_path.as_deref()),
            unclear_effect: None,
            temp_recovery_path,
        },
        Err(mut failure) => {
            failure.error = with_recovery_context(failure.error, &[&cause], &[]);
            failure
        }
    }
}

fn copy_file_stream(
    request: FileTransferRequest<'_>,
    cancel: &Cancel,
    prog: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<FileCopyOutcome, FileCopyFailure> {
    let FileTransferRequest {
        src,
        src_path,
        dst,
        dst_path,
        sandbox,
        replace_existing,
        expected_replacement,
        source_mtime,
        source_mode,
    } = request;
    if let Some(parent) = dst_path.parent() {
        dst.create_dir_all(parent)?;
    }
    let mut reader = src.open_read(src_path)?;
    let CreatedFileopTemp {
        path: temp_path,
        mut writer,
        removal_proof: temp_removal_proof,
    } = create_fileop_temp(dst, dst_path, sandbox)?;
    let transfer = copy_stream(reader.as_mut(), writer.as_mut(), cancel, &mut |bytes| {
        prog.bytes_done = prog.bytes_done.saturating_add(bytes);
        on_progress(prog);
    });
    drop(writer);

    let transfer = match transfer {
        Ok(result) if !result.complete => {
            let temp_recovery_path = cleanup_fileop_temp(
                dst,
                &temp_path,
                temp_removal_proof.as_ref(),
                "Die Dateiübertragung wurde abgebrochen",
            )?;
            return Ok(FileCopyOutcome {
                complete: false,
                temp_recovery_path,
                target_copy_new_best_effort: false,
                target_replace_best_effort: false,
            });
        }
        Ok(result) => result,
        Err(error) => {
            return Err(fail_file_copy_before_commit(
                dst,
                &temp_path,
                temp_removal_proof.as_ref(),
                error,
                "Die Dateiübertragung scheiterte",
            ));
        }
    };

    let transferred_hash = transfer.hash;
    let transferred_bytes = transfer.bytes;
    let prepare = (|| -> io::Result<()> {
        let mode_write = dst.supports_mode_write_at(temp_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Tempdatei benötigt einen Elternordner",
            )
        })?)?;
        if dst.supports_mtime_write() {
            if let Some(mtime) = source_mtime {
                dst.set_mtime(&temp_path, mtime)?;
            }
        }
        dst.sync_file(&temp_path)?;
        if hash_vfs_file(dst, &temp_path)? != transferred_hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Kopierprüfung der temporären Zieldatei ist fehlgeschlagen",
            ));
        }
        // Erst nach dem Sync schreibschützende/ausführbare Rechte setzen; danach wird
        // die fertige Temp-Datei ohne erneutes Öffnen atomar an ihren Zielnamen gehängt.
        if mode_write {
            if let Some(mode) = source_mode {
                dst.set_mode(&temp_path, mode)?;
            }
        }
        if dst.has_private_staging() {
            let actual = dst.stat(&temp_path)?;
            if (mode_write && source_mode.is_some_and(|mode| actual.mode != Some(mode)))
                || (dst.supports_mtime_write()
                    && source_mtime.is_some_and(|mtime| actual.mtime != Some(mtime)))
            {
                return Err(io::Error::other(
                    "Temp-Metadaten stimmen im Readback nicht überein",
                ));
            }
        }
        src.verify_transferred_source(src_path, &transferred_hash)?;
        // Auch die zuletzt gesetzten Rechte müssen vor dem Rename bestätigt sein.
        // Sonst könnte ein Move die Quelle schon entfernen, obwohl am Ziel nur der
        // Dateiinhalt, nicht aber das gewählte Metadatenprofil dauerhaft ist.
        dst.sync_file(&temp_path)?;
        Ok(())
    })();
    if let Err(error) = prepare {
        return Err(fail_file_copy_before_commit(
            dst,
            &temp_path,
            temp_removal_proof.as_ref(),
            error,
            "Die Vorbereitung der Temp-Datei scheiterte",
        ));
    }

    if cancel.is_cancelled() {
        let temp_recovery_path = cleanup_fileop_temp(
            dst,
            &temp_path,
            temp_removal_proof.as_ref(),
            "Die Dateiübertragung wurde vor dem Commit abgebrochen",
        )?;
        return Ok(FileCopyOutcome {
            complete: false,
            temp_recovery_path,
            target_copy_new_best_effort: false,
            target_replace_best_effort: false,
        });
    }

    let commit = commit_temp_file_with_phase(
        TempFileCommitRequest {
            dst,
            temp_path: &temp_path,
            dst_path,
            sandbox,
            replace_existing,
            expected_replacement,
            expected_size: Some(transferred_bytes),
            expected_hash: Some(*transferred_hash.as_bytes()),
        },
        Some(cancel),
    );
    match commit {
        Ok(effect) => Ok(FileCopyOutcome {
            complete: true,
            temp_recovery_path: match &effect {
                TargetCommitEffect::BackupRetained(path) => Some(path.clone()),
                _ => None,
            },
            target_copy_new_best_effort: effect == TargetCommitEffect::CopyNewBestEffort,
            target_replace_best_effort: effect == TargetCommitEffect::ReplaceBestEffort,
        }),
        Err(TempCommitFailure::TempStillOwned(error)) => {
            // Nur diese Phase beweist, dass der Tempname noch den bei seiner Anlage
            // gebundenen Eintrag bezeichnet. Die bedingte Entfernung schützt ihn
            // trotzdem gegen einen Austausch unmittelbar vor dem Aufräumen.
            Err(fail_file_copy_before_commit(
                dst,
                &temp_path,
                temp_removal_proof.as_ref(),
                error,
                "Die Dateioperation scheiterte",
            ))
        }
        Err(TempCommitFailure::TempMovedOrUnclear(error)) => {
            // Nach einem sichtbaren Rename ist der Tempname frei oder hält beim
            // Exchange sogar die Altdatei. Jeder Cleanup-Versuch würde dann den
            // eigentlichen Fehler verfälschen oder fremde Daten gefährden.
            Err(FileCopyFailure {
                error: if crate::error::recovery_paths(&error).is_empty() {
                    with_recovery_context(error, &[], &[&temp_path, dst_path])
                } else {
                    error
                },
                unclear_effect: None,
                temp_recovery_path: None,
            })
        }
        Err(TempCommitFailure::ParkedBackupCleanup(error, recovery_path)) => Err(FileCopyFailure {
            error,
            unclear_effect: None,
            temp_recovery_path: Some(recovery_path),
        }),
        Err(TempCommitFailure::BestEffortMovedOrUnclear(error, effect)) => Err(FileCopyFailure {
            error,
            unclear_effect: Some(effect),
            temp_recovery_path: None,
        }),
    }
}

#[cfg(test)]
#[path = "fileops_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
mod tests {

    #[test]
    fn review_best_effort_recovery_erreicht_die_operationsbilanz() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::write(&source, b"Inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.best_effort_commit.store(true, Ordering::SeqCst);
        vfs.fail_plain_after_rename.store(true, Ordering::SeqCst);
        let mut actuals = FileOperationActuals::default();
        let error = copy_items_inner(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            FileTransferBindings::default(),
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(
            std::fs::read(destination.join("quelle")).unwrap(),
            b"Inhalt"
        );
        let paths = crate::error::recovery_paths(&error);
        assert!(paths.contains(&destination.join("quelle").to_string_lossy().into_owned()));
        assert!(paths.contains(&"provider-recovery".to_owned()));
        assert!(paths.iter().any(|p| p.contains("fileop-tmp")));
        for path in paths {
            assert!(actuals.temp_recovery_paths.contains(&path));
        }
    }

    /// Derselbe Vorrang gilt im Rücklese-Listing. Ohne ihn stufte ein
    /// case-sensitives Dateisystem eine vollzogene, korrekte Ersetzung als unklaren
    /// Ausgang ein — belegt am 2026-09-03 auf dem Linux-Testhost, wo
    /// `ersetzen_funktioniert_neben_einem_namen_mit_anderer_schreibweise` genau daran
    /// scheiterte, während macOS den Fall gar nicht herstellen konnte.
    #[test]
    fn rueckleseziel_bevorzugt_den_exakten_namen() {
        fn eintrag(name: &str) -> ProofListEntry {
            ProofListEntry {
                entry: Entry {
                    name: name.into(),
                    is_dir: false,
                    is_file: true,
                    is_symlink: false,
                    size: 3,
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
                raw_name: name.as_bytes().to_vec(),
                safely_decoded: true,
                provider_token: Some(vec![9]),
            }
        }

        let listing = CompleteDirectoryListing {
            entries: vec![eintrag("datei.txt"), eintrag("Datei.txt")],
            provider_token: None,
            complete: true,
        };
        let (treffer, eindeutig) = readback_target(&listing, ".temp", "datei.txt", true);
        assert!(eindeutig);
        assert_eq!(treffer.unwrap().entry.name, "datei.txt");

        // Ohne exakten Treffer bleibt die konservative Äquivalenz die Schranke.
        let (_, eindeutig) = readback_target(&listing, ".temp", "DATEI.TXT", true);
        assert!(!eindeutig, "zwei Aequivalenztreffer sind mehrdeutig");

        // Die Temp-Datei zaehlt nie als Ziel.
        let mit_temp = CompleteDirectoryListing {
            entries: vec![eintrag("datei.txt"), eintrag(".temp")],
            provider_token: None,
            complete: true,
        };
        let (treffer, eindeutig) = readback_target(&mit_temp, ".temp", "datei.txt", true);
        assert!(eindeutig);
        assert_eq!(treffer.unwrap().entry.name, "datei.txt");

        // Ohne die Fähigkeit bleibt es bei der konservativen Schranke: Ein Server, der
        // beim Schreiben normalisiert und trotzdem beide Schreibweisen listet, macht
        // den exakten Treffer wertlos — welches Objekt die Mutation traf, ist offen.
        let (_, eindeutig) = readback_target(&listing, ".temp", "datei.txt", false);
        assert!(
            !eindeutig,
            "ohne Exaktnamen-Zusage darf ein exakter Treffer nicht als Beleg gelten"
        );
    }

    /// Ein byte-exakter Treffer ist eindeutig, auch wenn daneben ein Name liegt, den
    /// ein normalisierender Server gleichsetzen würde. Ohne diesen Vorrang brach eine
    /// gewöhnliche lokale Kopie auf einem case-sensitiven Dateisystem mit „Name nicht
    /// eindeutig" ab — für die gesamte Auswahl, bevor irgendetwas passierte. Der Test
    /// prüft das plattformunabhängig am Providerbestand selbst; das Verhalten auf
    /// einem echten case-sensitiven Volume ist damit nicht mehr nötig, um den Fall zu
    /// belegen.
    #[test]
    fn exakter_name_schlaegt_die_konservative_aequivalenz() {
        fn eintrag(name: &str) -> ProofListEntry {
            ProofListEntry {
                entry: Entry {
                    name: name.into(),
                    is_dir: false,
                    is_file: true,
                    is_symlink: false,
                    size: 3,
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
                raw_name: name.as_bytes().to_vec(),
                safely_decoded: true,
                provider_token: Some(vec![1, 2, 3]),
            }
        }

        let listing = CompleteDirectoryListing {
            entries: vec![eintrag("datei.txt"), eintrag("Datei.txt")],
            provider_token: None,
            complete: true,
        };
        let treffer = unique_named_proof(&listing, "datei.txt", None)
            .expect("Ein exakter Treffer ist eindeutig");
        assert_eq!(treffer.entry.name, "datei.txt");
        let anderer = unique_named_proof(&listing, "Datei.txt", None)
            .expect("Auch die andere Schreibweise ist für sich eindeutig");
        assert_eq!(anderer.entry.name, "Datei.txt");

        // Ohne exakten Treffer bleibt die konservative Äquivalenz die Schranke: Ein
        // normalisierender Server, der zwei Schreibweisen zusammenfallen lässt, darf
        // nicht geraten werden.
        let mehrdeutig = unique_named_proof(&listing, "DATEI.TXT", None).unwrap_err();
        assert_eq!(crate::error_code(&mehrdeutig), ErrorCode::InvalidData);
    }

    use super::*;
    use crate::vfs::{CompleteDirectoryListing, Entry, ProofListEntry};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    // Der Fehler liegt ausdrücklich NACH dem erfolgreichen Unlink: Eine bloß
    // fehlgeschlagene Cleanupantwort beweist keine erhaltene Sicherung.
    struct ErrorAfterPrivateCleanup {
        inner: Box<dyn Vfs>,
    }
    impl Vfs for ErrorAfterPrivateCleanup {
        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
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
        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            self.inner.open_read(path)
        }
        fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(path)
        }
        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write_new(path)
        }
        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }
        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_dir(path)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }
        fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
            self.inner.list_complete(path)
        }
        fn metadata_manifest(&self, path: &Path) -> io::Result<crate::metadata::MetadataManifest> {
            self.inner.metadata_manifest(path)
        }
        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }
        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            self.inner.set_mode(path, mode)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_noreplace(from, to)
        }
        fn has_private_staging(&self) -> bool {
            true
        }
        fn staging_candidate(&self, target: &Path, role: &str) -> io::Result<PathBuf> {
            self.inner.staging_candidate(target, role)
        }
        fn rename_noreplace_with_removal_proof(
            &self,
            from: &Path,
            to: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            self.inner.rename_noreplace_with_removal_proof(from, to)
        }
        fn cleanup_staged_backup(&self, path: &Path, proof: &ProofListEntry) -> io::Result<bool> {
            assert!(self.inner.cleanup_staged_backup(path, proof)?);
            Err(io::Error::other(
                "erzwungener Fehler nach Sicherungs-Unlink",
            ))
        }
        fn staged_backup_is_retained(
            &self,
            path: &Path,
            proof: &ProofListEntry,
        ) -> io::Result<bool> {
            self.inner.staged_backup_is_retained(path, proof)
        }
    }

    #[test]
    fn c2_cleanupfehler_nach_unlink_behauptet_keine_erhaltene_sicherung() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::local_fs::write_acl_exact(&root, None).unwrap();
        std::fs::write(
            root.join(crate::TEST_SANDBOX_MARKER),
            crate::TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();
        let target = root.join("datei");
        std::fs::write(&target, b"alt").unwrap();
        let local = crate::LocalVfs::rooted(&root).unwrap();
        let proof =
            capture_file_conflict_proofs(&local, &[target.to_string_lossy().into_owned()]).unwrap();
        let scoped = local.staging_scope(&target, Some(&root)).unwrap().unwrap();
        let candidate = scoped.staging_candidate(&target, "tmp").unwrap();
        scoped
            .open_write_new(&candidate)
            .unwrap()
            .write_all(b"neu")
            .unwrap();
        scoped.sync_file(&candidate).unwrap();
        let fault = ErrorAfterPrivateCleanup { inner: scoped };
        let result = commit_temp_file_with_phase(
            TempFileCommitRequest {
                dst: &fault,
                temp_path: &candidate,
                dst_path: &target,
                sandbox: &Some(root.clone()),
                replace_existing: true,
                expected_replacement: Some(&proof[0]),
                expected_size: Some(3),
                expected_hash: Some(*blake3::hash(b"neu").as_bytes()),
            },
            None,
        );
        match result {
            Err(TempCommitFailure::ParkedBackupCleanup(error, path)) => {
                assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
                assert!(!Path::new(&path).exists());
            }
            _ => panic!("Gelöschte Sicherung darf nicht als erhalten bilanziert werden"),
        }
        assert_eq!(std::fs::read(target).unwrap(), b"neu");
    }

    #[test]
    fn c2_frueher_lesefehler_schliesst_staging_oder_bilanziert_reste() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages, FaultVfs};
        for finish_error in [false, true] {
            let (_temp, root, source, target, local) = fixture();
            let fault_source = FaultVfs {
                inner: Box::new(crate::LocalVfs::rooted(&root).unwrap()),
                read_error: true,
                cancel_on_read: None,
                finish_error: false,
            };
            let fault_target = FaultVfs {
                inner: Box::new(local),
                read_error: false,
                cancel_on_read: None,
                finish_error,
            };
            let mut actuals = FileOperationActuals::default();
            let error = copy_items_inner(
                FileOperationRequest {
                    src: &fault_source,
                    dst: &fault_target,
                    items: std::slice::from_ref(&source),
                    dst_dir: &target,
                    policy: OverwritePolicy::Overwrite,
                    sandbox: &Some(root.clone()),
                },
                FileTransferBindings::default(),
                &Cancel::new(),
                &mut |_| {},
                &mut actuals,
            )
            .unwrap_err();
            assert_eq!(crate::error_code(&error), ErrorCode::PermissionDenied);
            assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
            assert!(!target.join("datei").exists());
            let retained = stages(&root);
            if finish_error {
                assert_eq!(retained.len(), 1);
                let path = retained[0].to_string_lossy().into_owned();
                assert!(error.to_string().contains(&path));
                assert!(crate::error::recovery_paths(&error).contains(&path));
                assert!(actuals.temp_recovery_paths.contains(&path));
                assert_eq!(
                    std::fs::read(retained[0].join("erhaltener-datenrest")).unwrap(),
                    b"erhalten"
                );
            } else {
                assert!(retained.is_empty());
                assert!(actuals.temp_recovery_paths.is_empty());
            }
        }
    }

    #[test]
    fn c2_abbruch_schliesst_leeres_staging() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages, FaultVfs};
        let (_temp, root, source, target, local) = fixture();
        let cancel = Cancel::new();
        let source_vfs = FaultVfs {
            inner: Box::new(crate::LocalVfs::rooted(&root).unwrap()),
            read_error: false,
            cancel_on_read: Some(cancel.clone()),
            finish_error: false,
        };
        let stats = copy_items(
            FileOperationRequest {
                src: &source_vfs,
                dst: &local,
                items: std::slice::from_ref(&source),
                dst_dir: &target,
                policy: OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert!(stats.cancelled);
        assert!(stats.temp_recovery_paths.is_empty());
        assert!(stages(&root).is_empty());
        assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
        assert!(!target.join("datei").exists());
    }

    #[test]
    fn c2_erfolgreiche_kopie_bleibt_bei_staging_cleanupfehler_bilanziert() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages, FaultVfs};
        let (_temp, root, source, target, local) = fixture();
        let source_vfs = crate::LocalVfs::rooted(&root).unwrap();
        let target_vfs = FaultVfs {
            inner: Box::new(local),
            read_error: false,
            cancel_on_read: None,
            finish_error: true,
        };
        let mut actuals = FileOperationActuals::default();
        let stats = copy_items_inner(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &target,
                policy: OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            FileTransferBindings::default(),
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(
            std::fs::read(target.join("datei")).unwrap(),
            b"Quelle bleibt erhalten"
        );
        assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
        let retained = stages(&root);
        assert_eq!(retained.len(), 1);
        let path = retained[0].to_string_lossy().into_owned();
        assert!(stats.temp_recovery_paths.contains(&path));
        assert!(actuals.temp_recovery_paths.contains(&path));
        assert_eq!(
            std::fs::read(retained[0].join("erhaltener-datenrest")).unwrap(),
            b"erhalten"
        );
    }

    #[test]
    fn c2_rekursiver_kindfehler_erhaelt_alle_staging_recoverypfade() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages, FaultVfs};
        let (_temp, root, source, target, local) = fixture();
        let source_directory = source.parent().unwrap().to_path_buf();
        let source_vfs = FaultVfs {
            inner: Box::new(crate::LocalVfs::rooted(&root).unwrap()),
            read_error: true,
            cancel_on_read: None,
            finish_error: false,
        };
        let target_vfs = FaultVfs {
            inner: Box::new(local),
            read_error: false,
            cancel_on_read: None,
            finish_error: true,
        };
        let mut actuals = FileOperationActuals::default();
        let error = copy_items_inner(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: &[source_directory],
                dst_dir: &target,
                policy: OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            FileTransferBindings::default(),
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
        let retained = stages(&root);
        assert_eq!(retained.len(), 2);
        let structured = crate::error::recovery_paths(&error);
        for stage in retained {
            let path = stage.to_string_lossy().into_owned();
            assert!(structured.contains(&path), "Kindpfad fehlt: {path}");
            assert!(actuals.temp_recovery_paths.contains(&path));
            assert!(error.to_string().contains(&path));
            assert!(stage.join("journal.json").is_file());
        }
        assert!(stages(&root).iter().any(|stage| {
            std::fs::read(stage.join("erhaltener-datenrest"))
                .is_ok_and(|bytes| bytes == b"erhalten")
        }));
    }

    #[test]
    fn c2_staging_anlagefehler_erhaelt_recovery_in_actuals() {
        use crate::atomic_file::staging_fault_tests::{fixture, stages, FaultVfs};
        let (_temp, root, source, target, local) = fixture();
        let source_vfs = crate::LocalVfs::rooted(&root).unwrap();
        let target_vfs = FaultVfs {
            inner: Box::new(local),
            read_error: true,
            cancel_on_read: None,
            finish_error: true,
        };
        let mut actuals = FileOperationActuals::default();
        let error = copy_items_inner(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &target,
                policy: OverwritePolicy::Overwrite,
                sandbox: &Some(root.clone()),
            },
            FileTransferBindings::default(),
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&error), ErrorCode::StorageFull);
        let retained = stages(&root);
        assert_eq!(retained.len(), 1);
        let path = retained[0].to_string_lossy().into_owned();
        assert_eq!(actuals.temp_recovery_paths, vec![path.clone()]);
        assert_eq!(crate::error::recovery_paths(&error), vec![path.clone()]);
        assert!(error.to_string().contains(&path));
        assert!(retained[0].join("journal.json").is_file());
        assert_eq!(std::fs::read(source).unwrap(), b"Quelle bleibt erhalten");
        assert!(!target.join("datei").exists());
    }

    #[derive(Clone, Copy)]
    enum ForeignReplacementKind {
        File,
        Directory,
    }

    struct ErrorAfterFirstRead {
        content: std::io::Cursor<Vec<u8>>,
        first_read_done: bool,
        recovery_paths: Vec<String>,
    }

    impl Read for ErrorAfterFirstRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.first_read_done {
                return Err(crate::error::with_recovery_paths(
                    io::Error::other("erzwungener Lesefehler"),
                    &self.recovery_paths,
                ));
            }
            self.first_read_done = true;
            self.content.read(buffer)
        }
    }

    /// Erzeugt die kurzen Wettläufe und Sync-Fehler an einer exakt festgelegten
    /// Sicherheitsgrenze. So bleiben die Regressionstests deterministisch.
    struct FileopsRaceVfs {
        inner: crate::LocalVfs,
        replacement: ForeignReplacementKind,
        swapped: AtomicBool,
        attacked_path: Mutex<Option<PathBuf>>,
        displaced_path: Mutex<Option<PathBuf>>,
        rollback_target: Mutex<Option<PathBuf>>,
        rollback_swapped: AtomicBool,
        rollback_displaced_path: Mutex<Option<PathBuf>>,
        observed_backup_path: Mutex<Option<PathBuf>>,
        failing_create_dir: Mutex<Option<PathBuf>>,
        fail_next_sync: AtomicBool,
        sync_calls: AtomicUsize,
        fail_sync_on_call: AtomicUsize,
        failing_read_path: Mutex<Option<PathBuf>>,
        read_recovery_paths: Mutex<Vec<String>>,
        exchange_attack_target: Mutex<Option<PathBuf>>,
        exchange_attack_done: AtomicBool,
        exchange_displaced_path: Mutex<Option<PathBuf>>,
        exchange_calls: AtomicUsize,
        conditional_remove_calls: AtomicUsize,
        restore_attack_target: Mutex<Option<PathBuf>>,
        restore_attack_done: AtomicBool,
        restore_displaced_path: Mutex<Option<PathBuf>>,
        move_attack_path: Mutex<Option<PathBuf>>,
        move_attack_done: AtomicBool,
        move_displaced_path: Mutex<Option<PathBuf>>,
        fail_create_after_mutation: Mutex<Option<PathBuf>>,
        fail_open_after_mutation: AtomicBool,
        fail_park_after_mutation: AtomicBool,
        proof_at_calls: AtomicUsize,
        bound_tokens: Mutex<HashMap<PathBuf, Vec<u8>>>,
        next_bound_token: AtomicUsize,
        coded_sync_error: Mutex<Option<ErrorCode>>,
        namespace_lock: Mutex<()>,
        proof_window_attack_path: Mutex<Option<PathBuf>>,
        proof_window_attack_done: AtomicBool,
        proof_window_displaced_path: Mutex<Option<PathBuf>>,
        move_destination_mode_attack: Mutex<Option<PathBuf>>,
        move_destination_mode_attack_done: AtomicBool,
        temp_cleanup_without_proof: AtomicBool,
        unproved_temp_path: Mutex<Option<PathBuf>>,
        best_effort_commit: AtomicBool,
        plain_rename_calls: AtomicUsize,
        fail_plain_after_rename: AtomicBool,
        fail_plain_rename_on_call: AtomicUsize,
        replace_plain_rename_calls: AtomicUsize,
        fail_replace_plain_rename_on_call: AtomicUsize,
        fail_best_effort_remove_path: Mutex<Option<PathBuf>>,
        fail_stat_on_call: AtomicUsize,
        stat_calls: AtomicUsize,
        fail_stat_after_rename_at: Mutex<Option<PathBuf>>,
        completed_rename_target: Mutex<Option<PathBuf>>,
        unsafe_listing_path: Mutex<Option<PathBuf>>,
        traversal_listing_path: Mutex<Option<PathBuf>>,
        failing_stat_path: Mutex<Option<PathBuf>>,
        pretend_remote: bool,
    }

    impl FileopsRaceVfs {
        fn new(replacement: ForeignReplacementKind) -> Self {
            Self {
                inner: crate::LocalVfs::new(),
                replacement,
                swapped: AtomicBool::new(false),
                attacked_path: Mutex::new(None),
                displaced_path: Mutex::new(None),
                rollback_target: Mutex::new(None),
                rollback_swapped: AtomicBool::new(false),
                rollback_displaced_path: Mutex::new(None),
                observed_backup_path: Mutex::new(None),
                failing_create_dir: Mutex::new(None),
                fail_next_sync: AtomicBool::new(false),
                sync_calls: AtomicUsize::new(0),
                fail_sync_on_call: AtomicUsize::new(0),
                failing_read_path: Mutex::new(None),
                read_recovery_paths: Mutex::new(Vec::new()),
                exchange_attack_target: Mutex::new(None),
                exchange_attack_done: AtomicBool::new(false),
                exchange_displaced_path: Mutex::new(None),
                exchange_calls: AtomicUsize::new(0),
                conditional_remove_calls: AtomicUsize::new(0),
                restore_attack_target: Mutex::new(None),
                restore_attack_done: AtomicBool::new(false),
                restore_displaced_path: Mutex::new(None),
                move_attack_path: Mutex::new(None),
                move_attack_done: AtomicBool::new(false),
                move_displaced_path: Mutex::new(None),
                fail_create_after_mutation: Mutex::new(None),
                fail_open_after_mutation: AtomicBool::new(false),
                fail_park_after_mutation: AtomicBool::new(false),
                proof_at_calls: AtomicUsize::new(0),
                bound_tokens: Mutex::new(HashMap::new()),
                next_bound_token: AtomicUsize::new(1),
                coded_sync_error: Mutex::new(None),
                namespace_lock: Mutex::new(()),
                proof_window_attack_path: Mutex::new(None),
                proof_window_attack_done: AtomicBool::new(false),
                proof_window_displaced_path: Mutex::new(None),
                move_destination_mode_attack: Mutex::new(None),
                move_destination_mode_attack_done: AtomicBool::new(false),
                temp_cleanup_without_proof: AtomicBool::new(false),
                unproved_temp_path: Mutex::new(None),
                best_effort_commit: AtomicBool::new(false),
                plain_rename_calls: AtomicUsize::new(0),
                fail_plain_after_rename: AtomicBool::new(false),
                fail_plain_rename_on_call: AtomicUsize::new(0),
                replace_plain_rename_calls: AtomicUsize::new(0),
                fail_replace_plain_rename_on_call: AtomicUsize::new(0),
                fail_best_effort_remove_path: Mutex::new(None),
                fail_stat_on_call: AtomicUsize::new(0),
                stat_calls: AtomicUsize::new(0),
                fail_stat_after_rename_at: Mutex::new(None),
                completed_rename_target: Mutex::new(None),
                unsafe_listing_path: Mutex::new(None),
                traversal_listing_path: Mutex::new(None),
                failing_stat_path: Mutex::new(None),
                pretend_remote: false,
            }
        }

        fn swapping_created_target(target: &Path) -> Self {
            let vfs = Self::new(ForeignReplacementKind::Directory);
            *vfs.rollback_target.lock().unwrap() = Some(target.to_path_buf());
            vfs
        }

        fn different_remote_namespace() -> Self {
            let mut vfs = Self::new(ForeignReplacementKind::File);
            vfs.pretend_remote = true;
            vfs
        }

        fn failing_create_dir(target: &Path) -> Self {
            let vfs = Self::new(ForeignReplacementKind::File);
            *vfs.failing_create_dir.lock().unwrap() = Some(target.to_path_buf());
            vfs
        }

        fn arm_sync_failure(&self) {
            self.fail_next_sync.store(true, Ordering::SeqCst);
        }

        fn fail_sync_on_call(&self, call: usize) {
            self.fail_sync_on_call.store(call, Ordering::SeqCst);
        }

        fn fail_read_at(&self, path: &Path) {
            *self.failing_read_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn attack_target_before_exchange(&self, path: &Path) {
            *self.exchange_attack_target.lock().unwrap() = Some(path.to_path_buf());
        }

        fn attack_backup_before_restore_to(&self, path: &Path) {
            *self.restore_attack_target.lock().unwrap() = Some(path.to_path_buf());
        }

        fn attack_move_source_before_remove(&self, path: &Path) {
            *self.move_attack_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn fail_create_after_mutation_at(&self, path: &Path) {
            *self.fail_create_after_mutation.lock().unwrap() = Some(path.to_path_buf());
        }

        fn fail_next_open_after_mutation(&self) {
            self.fail_open_after_mutation.store(true, Ordering::SeqCst);
        }

        fn fail_next_park_after_mutation(&self) {
            self.fail_park_after_mutation.store(true, Ordering::SeqCst);
        }

        fn attack_between_mutation_and_proof_at(&self, path: &Path) {
            *self.proof_window_attack_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn attack_move_destination_mode_before_readback(&self, path: &Path) {
            *self.move_destination_mode_attack.lock().unwrap() = Some(path.to_path_buf());
        }

        fn attack_unproved_temp_before_remove(&self) {
            self.temp_cleanup_without_proof
                .store(true, Ordering::SeqCst);
        }

        fn use_best_effort_commit(&self) {
            self.best_effort_commit.store(true, Ordering::SeqCst);
        }

        fn fail_plain_rename_on_call(&self, call: usize) {
            self.fail_plain_rename_on_call.store(call, Ordering::SeqCst);
        }

        fn fail_replace_plain_rename_on_call(&self, call: usize) {
            self.fail_replace_plain_rename_on_call
                .store(call, Ordering::SeqCst);
        }

        fn fail_best_effort_remove_at(&self, path: &Path) {
            *self.fail_best_effort_remove_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn fail_stat_on_call(&self, call: usize) {
            self.fail_stat_on_call.store(call, Ordering::SeqCst);
        }

        fn fail_stat_after_rename_at(&self, path: &Path) {
            *self.fail_stat_after_rename_at.lock().unwrap() = Some(path.to_path_buf());
        }

        fn return_unsafe_listing_at(&self, path: &Path) {
            *self.unsafe_listing_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn return_traversal_listing_at(&self, path: &Path) {
            *self.traversal_listing_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn fail_stat_at(&self, path: &Path) {
            *self.failing_stat_path.lock().unwrap() = Some(path.to_path_buf());
        }

        fn arm_coded_sync_failure(&self, code: ErrorCode) {
            *self.coded_sync_error.lock().unwrap() = Some(code);
            self.arm_sync_failure();
        }

        fn attack_before_first_exchange(&self, path: &Path) {
            let target_matches = self
                .exchange_attack_target
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|target| target == path);
            if !target_matches || self.exchange_attack_done.swap(true, Ordering::SeqCst) {
                return;
            }
            let displaced = path.parent().unwrap().join(".altziel-vor-exchange-rennen");
            std::fs::rename(path, &displaced).unwrap();
            std::fs::write(path, b"fremdes-ziel").unwrap();
            *self.exchange_displaced_path.lock().unwrap() = Some(displaced);
        }

        fn remember_backup_next_to(&self, path: &Path) {
            let Some(parent) = path.parent() else {
                return;
            };
            let backup = std::fs::read_dir(parent)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|entry| {
                    entry.file_name().is_some_and(|name| {
                        name.to_string_lossy().contains("doppeldecker-replaced")
                    })
                });
            *self.observed_backup_path.lock().unwrap() = backup;
        }

        fn is_rollback_target(&self, path: &Path) -> bool {
            self.rollback_target
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|target| target == path)
        }

        fn proof_at(&self, path: &Path) -> io::Result<ProofListEntry> {
            self.proof_at_calls.fetch_add(1, Ordering::SeqCst);
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("Testpfad ohne Elternordner"))?;
            let name =
                file_name(path).ok_or_else(|| io::Error::other("Testpfad ohne sicheren Namen"))?;
            self.inner
                .list_complete(parent)?
                .entries
                .into_iter()
                .find(|entry| entry.entry.name == name)
                .ok_or_else(|| io::Error::other("Testbeleg fehlt"))
        }

        /// Das Testdouble erzeugt seinen unveränderlichen Provider-Token innerhalb
        /// derselben simulierten Mutationsaktion. `inner.stat` füllt nur die
        /// Anzeigefelder; die Sicherheitsbindung stammt ausschließlich aus dem Token
        /// und nicht aus einem nachträglichen Listing.
        fn bound_proof_under_namespace_lock(&self, path: &Path) -> io::Result<ProofListEntry> {
            let mut entry = self.inner.stat(path)?;
            entry.name =
                file_name(path).ok_or_else(|| io::Error::other("Testpfad ohne sicheren Namen"))?;
            let token = self
                .next_bound_token
                .fetch_add(1, Ordering::SeqCst)
                .to_le_bytes()
                .to_vec();
            self.bound_tokens
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), token.clone());
            Ok(ProofListEntry {
                raw_name: entry.name.as_bytes().to_vec(),
                entry,
                safely_decoded: true,
                provider_token: Some(token),
            })
        }

        fn atomic_bound_proof(&self, path: &Path) -> io::Result<ProofListEntry> {
            let _namespace = self.namespace_lock.lock().unwrap();
            self.bound_proof_under_namespace_lock(path)
        }

        fn swap_in_old_proof_window(&self, path: &Path) -> bool {
            let target_matches = self
                .proof_window_attack_path
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|target| target == path);
            if !target_matches
                || !path.exists()
                || self.proof_window_attack_done.swap(true, Ordering::SeqCst)
            {
                return false;
            }
            let _namespace = self.namespace_lock.lock().unwrap();
            let displaced = path
                .parent()
                .unwrap()
                .join(".vor-proof-ausgetauschter-eintrag");
            std::fs::rename(path, &displaced).unwrap();
            std::fs::write(path, b"fremd-im-proof-fenster").unwrap();
            self.invalidate_bound_token(path);
            *self.proof_window_displaced_path.lock().unwrap() = Some(displaced);
            true
        }

        fn invalidate_bound_token(&self, path: &Path) {
            self.bound_tokens.lock().unwrap().remove(path);
        }

        fn bound_token_matches(&self, path: &Path, expected: &[u8]) -> bool {
            self.bound_tokens
                .lock()
                .unwrap()
                .get(path)
                .is_some_and(|actual| actual.as_slice() == expected)
        }

        fn swap_backup_before_restore(&self, from: &Path, to: &Path) -> bool {
            let target_matches = self
                .restore_attack_target
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|target| target == to);
            if !target_matches
                || !from.exists()
                || self.restore_attack_done.swap(true, Ordering::SeqCst)
            {
                return false;
            }
            let displaced = from.parent().unwrap().join(".altes-backup-vor-restore");
            std::fs::rename(from, &displaced).unwrap();
            std::fs::write(from, b"fremde-sicherung").unwrap();
            self.invalidate_bound_token(from);
            *self.restore_displaced_path.lock().unwrap() = Some(displaced);
            true
        }

        fn swap_move_source_before_remove(&self, path: &Path) -> bool {
            let configured_source = self.move_attack_path.lock().unwrap().clone();
            let source_matches = configured_source
                .as_deref()
                .is_some_and(|source| path == source || path.starts_with(source));
            if !source_matches || self.move_attack_done.swap(true, Ordering::SeqCst) {
                return false;
            }
            let source = configured_source.unwrap();
            if !source.exists() {
                return false;
            }
            let displaced = source.parent().unwrap().join(".move-quelle-vor-remove");
            std::fs::rename(&source, &displaced).unwrap();
            self.invalidate_bound_token(&source);
            match self.replacement {
                ForeignReplacementKind::File => {
                    std::fs::write(&source, b"fremde-quelle").unwrap();
                }
                ForeignReplacementKind::Directory => {
                    std::fs::create_dir(&source).unwrap();
                    std::fs::write(source.join("fremd.txt"), b"fremder-baum").unwrap();
                }
            }
            *self.move_displaced_path.lock().unwrap() = Some(displaced);
            true
        }

        fn swap_created_target_before_rollback(&self, path: &Path) -> bool {
            if !self.is_rollback_target(path) || self.rollback_swapped.swap(true, Ordering::SeqCst)
            {
                return false;
            }
            let displaced = path
                .parent()
                .unwrap()
                .join(".angelegter-zielbaum-nach-rennen");
            std::fs::rename(path, &displaced).unwrap();
            self.invalidate_bound_token(path);
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("fremd.txt"), b"fremd").unwrap();
            *self.rollback_displaced_path.lock().unwrap() = Some(displaced);
            true
        }

        fn swap_if_backup(&self, path: &Path) -> bool {
            let is_verified_recovery_name = path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.contains("doppeldecker-replaced") || name.contains("doppeldecker-fileop-tmp")
            });
            if !is_verified_recovery_name
                || !path.exists()
                || self.swapped.swap(true, Ordering::SeqCst)
            {
                return false;
            }
            let parent = path.parent().unwrap();
            let displaced = parent.join(".gepruefte-sicherung-nach-rennen");
            std::fs::rename(path, &displaced).unwrap();
            self.invalidate_bound_token(path);
            match self.replacement {
                ForeignReplacementKind::File => std::fs::write(path, b"fremd").unwrap(),
                ForeignReplacementKind::Directory => {
                    std::fs::create_dir(path).unwrap();
                    std::fs::write(path.join("fremd.txt"), b"fremd").unwrap();
                }
            }
            *self.attacked_path.lock().unwrap() = Some(path.to_path_buf());
            *self.displaced_path.lock().unwrap() = Some(displaced);
            true
        }

        fn attacked_path(&self) -> PathBuf {
            self.attacked_path.lock().unwrap().clone().unwrap()
        }

        fn displaced_path(&self) -> PathBuf {
            self.displaced_path.lock().unwrap().clone().unwrap()
        }

        fn rollback_displaced_path(&self) -> PathBuf {
            self.rollback_displaced_path
                .lock()
                .unwrap()
                .clone()
                .unwrap()
        }

        fn observed_backup_path(&self) -> PathBuf {
            self.observed_backup_path.lock().unwrap().clone().unwrap()
        }

        fn exchange_displaced_path(&self) -> PathBuf {
            self.exchange_displaced_path
                .lock()
                .unwrap()
                .clone()
                .unwrap()
        }

        fn restore_displaced_path(&self) -> PathBuf {
            self.restore_displaced_path.lock().unwrap().clone().unwrap()
        }

        fn move_displaced_path(&self) -> PathBuf {
            self.move_displaced_path.lock().unwrap().clone().unwrap()
        }

        fn proof_window_displaced_path(&self) -> PathBuf {
            self.proof_window_displaced_path
                .lock()
                .unwrap()
                .clone()
                .unwrap()
        }

        fn exchange_count(&self) -> usize {
            self.exchange_calls.load(Ordering::SeqCst)
        }

        fn proof_at_count(&self) -> usize {
            self.proof_at_calls.load(Ordering::SeqCst)
        }

        fn conditional_remove_count(&self) -> usize {
            self.conditional_remove_calls.load(Ordering::SeqCst)
        }
    }

    impl Vfs for FileopsRaceVfs {
        fn is_local(&self) -> bool {
            !self.pretend_remote
        }

        fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
            self.inner.list(path)
        }

        fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
            let mut listing = self.inner.list_complete(path)?;
            if self.unsafe_listing_path.lock().unwrap().as_deref() == Some(path) {
                // Der Provider meldet denselben Anzeigenamen, aber ausdrücklich
                // unsicher dekodierte Rohbytes. Fileops darf daraus niemals einen
                // mutierbaren Pfad erfinden.
                listing.entries.push(ProofListEntry {
                    entry: Entry {
                        name: "nicht-verwendbarer-anzeigename".into(),
                        is_dir: false,
                        is_file: true,
                        is_symlink: false,
                        size: 3,
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
                    raw_name: vec![b'r', b'a', b'w', 0xff],
                    safely_decoded: false,
                    provider_token: None,
                });
            }
            if self.traversal_listing_path.lock().unwrap().as_deref() == Some(path) {
                // Der Provider behauptet fälschlich, ein Elternpfad sei ein normaler,
                // sicher dekodierter Kindname. Fileops darf den Namen nie an den
                // ausgewählten Quell- oder Zielpfad hängen.
                listing.entries.push(ProofListEntry {
                    entry: Entry {
                        name: "../ausbruch.txt".into(),
                        is_dir: false,
                        is_file: true,
                        is_symlink: false,
                        size: 8,
                        mtime: None,
                        created: None,
                        owner: None,
                        group: None,
                        extension: Some("txt".into()),
                        mode: None,
                        device: None,
                        inode: None,
                        hard_links: None,
                    },
                    raw_name: b"../ausbruch.txt".to_vec(),
                    safely_decoded: true,
                    provider_token: None,
                });
            }
            Ok(listing)
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            if self.failing_stat_path.lock().unwrap().as_deref() == Some(path) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "erzwungener pfadgebundener stat-Fehler",
                ));
            }
            let call = self.stat_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_stat_on_call.load(Ordering::SeqCst) == call {
                return Err(io::Error::other(format!(
                    "erzwungener rein lesender stat-Fehler bei Aufruf {call}"
                )));
            }
            if self.fail_stat_after_rename_at.lock().unwrap().as_deref() == Some(path)
                && self.completed_rename_target.lock().unwrap().as_deref() == Some(path)
            {
                return Err(io::Error::other(
                    "erzwungener Zielzählfehler nach vollzogenem Move-Rename",
                ));
            }
            let attack_mode = self
                .move_destination_mode_attack
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|target| target == path);
            if attack_mode
                && path.exists()
                && !self
                    .move_destination_mode_attack_done
                    .swap(true, Ordering::SeqCst)
            {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                }
            }
            self.inner.stat(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            let must_fail = self
                .failing_create_dir
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|target| target == path);
            if must_fail {
                self.remember_backup_next_to(path);
                self.arm_sync_failure();
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "erzwungener create_dir-Fehler",
                ));
            }
            self.inner.create_dir(path)
        }

        fn create_dir_with_removal_proof(
            &self,
            path: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            let namespace = self.namespace_lock.lock().unwrap();
            self.create_dir(path).map_err(MutationFailure::Unchanged)?;
            let fail_after_mutation = self
                .fail_create_after_mutation
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|target| target == path);
            if fail_after_mutation {
                return Err(MutationFailure::OutcomeUnclear(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "erzwungener Fehler nach create_dir",
                )));
            }
            let proof = self
                .bound_proof_under_namespace_lock(path)
                .map_err(MutationFailure::OutcomeUnclear)?;
            drop(namespace);
            // Das Rennen liegt nun NACH der atomaren Fake-Aktion. Der bereits
            // ausgegebene Token bleibt unverändert und muss den Austausch erkennen.
            self.swap_in_old_proof_window(path);
            Ok(Some(proof))
        }

        fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
            let must_fail = self
                .failing_read_path
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|source| source == path);
            if must_fail {
                if let Some(temp) = self.unproved_temp_path.lock().unwrap().take() {
                    self.swap_if_backup(&temp);
                }
                return Ok(Box::new(ErrorAfterFirstRead {
                    content: std::io::Cursor::new(std::fs::read(path)?),
                    first_read_done: false,
                    recovery_paths: self.read_recovery_paths.lock().unwrap().clone(),
                }));
            }
            self.inner.open_read(path)
        }

        fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write(path)
        }

        fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            self.inner.open_write_new(path)
        }

        fn open_write_new_with_removal_proof(
            &self,
            path: &Path,
        ) -> Result<(Box<dyn Write + Send>, Option<ProofListEntry>), MutationFailure> {
            let namespace = self.namespace_lock.lock().unwrap();
            let writer = self
                .open_write_new(path)
                .map_err(MutationFailure::Unchanged)?;
            if self.fail_open_after_mutation.swap(false, Ordering::SeqCst) {
                drop(writer);
                return Err(MutationFailure::OutcomeUnclear(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "erzwungener Fehler nach open_write_new",
                )));
            }
            let proof = self
                .bound_proof_under_namespace_lock(path)
                .map_err(MutationFailure::OutcomeUnclear)?;
            drop(namespace);
            self.swap_in_old_proof_window(path);
            if self.temp_cleanup_without_proof.load(Ordering::SeqCst) {
                self.invalidate_bound_token(path);
                *self.unproved_temp_path.lock().unwrap() = Some(path.to_path_buf());
                // Der fremde Name wird unmittelbar nach der exklusiven Anlage
                // eingesetzt, während der offene Writer weiterhin die beiseite
                // verschobene eigene Datei beschreibt. So prüft der Test genau den
                // Cleanup ohne Anlagebeleg und nicht ein nachträgliches Listing.
                self.swap_if_backup(path);
                Ok((writer, None))
            } else {
                Ok((writer, Some(proof)))
            }
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_file(path)
        }

        fn sync_dir(&self, path: &Path) -> io::Result<()> {
            let call = self.sync_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let scheduled = self.fail_sync_on_call.load(Ordering::SeqCst);
            if self.fail_next_sync.swap(false, Ordering::SeqCst)
                || (scheduled != 0 && scheduled == call)
            {
                let code = self.coded_sync_error.lock().unwrap().take();
                return Err(match code {
                    Some(code) => coded_error(
                        io::ErrorKind::ConnectionAborted,
                        code,
                        "erzwungener codierter Verzeichnis-Sync-Fehler",
                    ),
                    None => io::Error::other("erzwungener Verzeichnis-Sync-Fehler"),
                });
            }
            self.inner.sync_dir(path)
        }

        fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
            self.inner.set_mtime(path, mtime)
        }

        fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
            if self.is_rollback_target(path) {
                self.remember_backup_next_to(path);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "erzwungener set_mode-Fehler vor Zielaustausch",
                ));
            }
            self.inner.set_mode(path, mode)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.swap_move_source_before_remove(path);
            self.swap_if_backup(path);
            self.inner.remove_file(path)
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.swap_move_source_before_remove(path);
            self.swap_if_backup(path);
            self.inner.remove_dir_all(path)
        }

        fn remove_dir(&self, path: &Path) -> io::Result<()> {
            self.swap_move_source_before_remove(path);
            self.swap_if_backup(path);
            self.inner.remove_dir(path)
        }

        fn remove_move_leaf_best_effort(&self, path: &Path) -> io::Result<()> {
            if self.fail_best_effort_remove_path.lock().unwrap().as_deref() == Some(path) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "erzwungener Fehler beim begrenzten Quellabbau",
                ));
            }
            self.remove_file(path)
        }

        fn remove_move_empty_dir_best_effort(&self, path: &Path) -> io::Result<()> {
            self.remove_dir(path)
        }

        fn remove_if_matches(&self, path: &Path, expected: &ProofListEntry) -> io::Result<()> {
            self.conditional_remove_calls.fetch_add(1, Ordering::SeqCst);
            // Das Double tauscht den Namen exakt NACH dem erfolgreichen
            // `verify_parked_replacement` aus. Ein Provider ohne atomare Bedingung
            // muss jetzt ablehnen, statt auf `remove_file`/`remove_dir_all`
            // zurückzufallen und den fremden Eintrag zu löschen.
            let swapped =
                self.swap_created_target_before_rollback(path) || self.swap_if_backup(path);
            if swapped {
                let actual = self.proof_at(path)?;
                assert!(
                    !same_proof(&actual, expected),
                    "Das Testdouble muss vor der bedingten Entfernung eine andere Identität einsetzen"
                );
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "erzwungener Identitätswechsel vor bedingter Entfernung",
                ));
            }
            let _namespace = self.namespace_lock.lock().unwrap();
            if expected
                .provider_token
                .as_deref()
                .is_some_and(|token| self.bound_token_matches(path, token))
            {
                let metadata = self.inner.stat(path)?;
                return if metadata.is_dir && !metadata.is_symlink {
                    let listing = self.inner.list_complete(path)?;
                    require_complete_safe_listing(&listing, "bedingte Testentfernung")?;
                    if !listing.entries.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::DirectoryNotEmpty,
                            "Ein Root-Beleg darf keinen nichtleeren Baum rekursiv entfernen",
                        ));
                    }
                    self.invalidate_bound_token(path);
                    self.inner.remove_dir(path)
                } else {
                    self.invalidate_bound_token(path);
                    self.inner.remove_file(path)
                };
            }
            if expected.provider_token.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Testtoken stimmt nicht mehr mit dem sichtbaren Eintrag überein",
                ));
            }
            self.inner.remove_if_matches(path, expected)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            let force_fallback = self
                .move_attack_path
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|source| source == from);
            if force_fallback {
                return Err(io::Error::from(io::ErrorKind::CrossesDevices));
            }
            self.swap_backup_before_restore(from, to);
            let result = self.inner.rename_noreplace(from, to);
            if result.is_ok() {
                *self.completed_rename_target.lock().unwrap() = Some(to.to_path_buf());
            }
            result
        }

        fn rename_noreplace_with_removal_proof(
            &self,
            from: &Path,
            to: &Path,
        ) -> Result<Option<ProofListEntry>, MutationFailure> {
            let namespace = self.namespace_lock.lock().unwrap();
            self.rename_noreplace(from, to)
                .map_err(MutationFailure::Unchanged)?;
            if self.fail_park_after_mutation.swap(false, Ordering::SeqCst) {
                *self.observed_backup_path.lock().unwrap() = Some(to.to_path_buf());
                return Err(MutationFailure::OutcomeUnclear(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "erzwungener Fehler nach rename_noreplace",
                )));
            }
            let proof = self
                .bound_proof_under_namespace_lock(to)
                .map_err(MutationFailure::OutcomeUnclear)?;
            drop(namespace);
            self.swap_in_old_proof_window(to);
            Ok(Some(proof))
        }

        fn rename_noreplace_if_matches(
            &self,
            from: &Path,
            to: &Path,
            expected: &ProofListEntry,
        ) -> io::Result<()> {
            self.swap_backup_before_restore(from, to);
            let _namespace = self.namespace_lock.lock().unwrap();
            let matches = match expected.provider_token.as_deref() {
                Some(token) => self.bound_token_matches(from, token),
                None => same_proof(&self.proof_at(from)?, expected),
            };
            if !matches {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "erzwungener Identitätswechsel vor gebundenem Restore",
                ));
            }
            self.inner.rename_noreplace(from, to)?;
            let moved_token = self.bound_tokens.lock().unwrap().remove(from);
            if let Some(token) = moved_token {
                self.bound_tokens
                    .lock()
                    .unwrap()
                    .insert(to.to_path_buf(), token);
            }
            Ok(())
        }

        fn bind_move_source_for_removal(
            &self,
            path: &Path,
            content_digest: [u8; 32],
            recursive: bool,
        ) -> io::Result<Option<MoveSourceRemovalProof>> {
            if self.fail_best_effort_remove_path.lock().unwrap().is_some() {
                return Ok(None);
            }
            if recursive {
                return Ok(None);
            }
            let _namespace = self.namespace_lock.lock().unwrap();
            let metadata = self.inner.stat(path)?;
            if !metadata.is_file || metadata.is_symlink {
                return Ok(None);
            }
            let mut digest = blake3::Hasher::new();
            digest.update(&0_u64.to_le_bytes());
            digest.update(&[0]);
            digest.update(&metadata.size.to_le_bytes());
            digest.update(&metadata.mtime.unwrap_or(i64::MIN).to_le_bytes());
            digest.update(&metadata.mode.unwrap_or(u32::MAX).to_le_bytes());
            digest.update(hash_vfs_file(&self.inner, path)?.as_bytes());
            if *digest.finalize().as_bytes() != content_digest {
                return Ok(None);
            }
            let proof = self.bound_proof_under_namespace_lock(path)?;
            Ok(Some(MoveSourceRemovalProof {
                provider_token: proof.provider_token.unwrap(),
                content_digest,
                recursive,
            }))
        }

        fn remove_move_source_if_matches(
            &self,
            path: &Path,
            expected: &MoveSourceRemovalProof,
        ) -> io::Result<()> {
            self.swap_move_source_before_remove(path);
            let _namespace = self.namespace_lock.lock().unwrap();
            if !self.bound_token_matches(path, &expected.provider_token) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "erzwungener Move-Quellwechsel vor bedingter Entfernung",
                ));
            }
            if expected.recursive {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Ein Root-Token ist kein atomarer Beleg für rekursives Entfernen",
                ))
            } else {
                self.invalidate_bound_token(path);
                self.inner.remove_file(path)
            }
        }

        fn rename_exchange(&self, left: &Path, right: &Path) -> io::Result<()> {
            self.exchange_calls.fetch_add(1, Ordering::SeqCst);
            self.attack_before_first_exchange(right);
            self.inner.rename_exchange(left, right)
        }

        fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
            let call = self.plain_rename_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_plain_rename_on_call.load(Ordering::SeqCst) == call {
                return Err(io::Error::other(format!(
                    "erzwungene unklare Commit-Antwort bei Aufruf {call}"
                )));
            }
            self.inner.rename(from, to)?;
            if self.fail_plain_after_rename.load(Ordering::SeqCst) {
                return Err(with_recovery_context(
                    coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        "injizierter Fehler nach Rename",
                    ),
                    &[],
                    &[Path::new("provider-recovery")],
                ));
            }
            Ok(())
        }

        fn rename_replace_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
            let call = self
                .replace_plain_rename_calls
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            if self
                .fail_replace_plain_rename_on_call
                .load(Ordering::SeqCst)
                == call
            {
                return Err(io::Error::other(format!(
                    "erzwungene unklare Ersetzungsantwort bei Aufruf {call}"
                )));
            }
            self.inner.rename(from, to)
        }

        fn file_commit_class(&self) -> FileCommitClass {
            if self.best_effort_commit.load(Ordering::SeqCst) {
                FileCommitClass::BestEffort
            } else {
                FileCommitClass::Assured
            }
        }

        fn file_replace_class(&self) -> FileReplaceClass {
            if self.best_effort_commit.load(Ordering::SeqCst) {
                FileReplaceClass::BestEffort
            } else {
                FileReplaceClass::Assured
            }
        }
    }

    fn proof_entry(name: &str, is_dir: bool) -> ProofListEntry {
        ProofListEntry {
            entry: Entry {
                name: name.into(),
                is_dir,
                is_file: !is_dir,
                is_symlink: false,
                size: 0,
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
            raw_name: name.as_bytes().to_vec(),
            safely_decoded: true,
            provider_token: None,
        }
    }

    fn commit_listing(names: &[(&str, bool)], complete: bool) -> CompleteDirectoryListing {
        CompleteDirectoryListing {
            entries: names
                .iter()
                .map(|(name, dir)| proof_entry(name, *dir))
                .collect(),
            provider_token: None,
            complete,
        }
    }

    /// Skriptbarer Best-Effort-Provider: Listings in Aufrufreihenfolge, ein
    /// vorbestimmtes Rename-Ergebnis und ein Mutationszähler für den
    /// Exakt-einmal-Beleg des Commits.
    struct ScriptedCommitVfs {
        listings: Mutex<Vec<io::Result<CompleteDirectoryListing>>>,
        mutation: Mutex<Option<io::Result<()>>>,
        mutations_executed: AtomicUsize,
        reclaims: AtomicUsize,
        reclaim_result: Mutex<Option<io::Result<()>>>,
        read_content: Mutex<Option<Vec<u8>>>,
        reset_commit_class_after_reclaim: bool,
        reset_replace_class_after_reclaim: bool,
    }

    impl ScriptedCommitVfs {
        fn new(
            listings: Vec<io::Result<CompleteDirectoryListing>>,
            mutation: io::Result<()>,
        ) -> Self {
            Self {
                listings: Mutex::new(listings),
                mutation: Mutex::new(Some(mutation)),
                mutations_executed: AtomicUsize::new(0),
                reclaims: AtomicUsize::new(0),
                reclaim_result: Mutex::new(None),
                read_content: Mutex::new(None),
                reset_commit_class_after_reclaim: false,
                reset_replace_class_after_reclaim: false,
            }
        }

        fn with_read_content(self, content: &[u8]) -> Self {
            *self.read_content.lock().unwrap() = Some(content.to_vec());
            self
        }

        fn resetting_classes_after_reclaim(mut self) -> Self {
            self.reset_commit_class_after_reclaim = true;
            self.reset_replace_class_after_reclaim = true;
            self
        }

        fn mutation_count(&self) -> usize {
            self.mutations_executed.load(Ordering::SeqCst)
        }
    }

    impl Vfs for ScriptedCommitVfs {
        fn reclaim_verification_session(&self) -> io::Result<()> {
            self.reclaims.fetch_add(1, Ordering::SeqCst);
            self.reclaim_result.lock().unwrap().take().unwrap_or(Ok(()))
        }

        fn list(&self, _path: &Path) -> io::Result<Vec<Entry>> {
            unreachable!("Tests verwenden ausschließlich list_complete")
        }

        fn list_complete(&self, _path: &Path) -> io::Result<CompleteDirectoryListing> {
            self.listings.lock().unwrap().remove(0)
        }

        fn stat(&self, path: &Path) -> io::Result<Entry> {
            Ok(proof_entry(path.file_name().unwrap().to_str().unwrap(), false).entry)
        }

        fn exists(&self, _path: &Path) -> bool {
            false
        }

        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn create_dir(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn open_read(&self, _path: &Path) -> io::Result<Box<dyn Read + Send>> {
            let content = self
                .read_content
                .lock()
                .unwrap()
                .clone()
                .expect("Test muss Rückleseinhalt konfigurieren");
            Ok(Box::new(std::io::Cursor::new(content)))
        }

        fn open_write(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!()
        }

        fn open_write_new(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
            unreachable!()
        }

        fn sync_file(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn sync_dir(&self, _path: &Path) -> io::Result<()> {
            Ok(())
        }

        fn set_mtime(&self, _path: &Path, _mtime: i64) -> io::Result<()> {
            unreachable!()
        }

        fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
            unreachable!()
        }

        fn remove_file(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            unreachable!()
        }

        fn rename_noreplace(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            unreachable!("Best-Effort-Commit darf nie den No-Replace-Weg nehmen")
        }

        fn rename_plain_once(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            self.mutations_executed.fetch_add(1, Ordering::SeqCst);
            self.mutation
                .lock()
                .unwrap()
                .take()
                .expect("Commit-Mutation darf höchstens einmal laufen")
        }

        fn rename_replace_plain_once(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            self.mutations_executed.fetch_add(1, Ordering::SeqCst);
            self.mutation
                .lock()
                .unwrap()
                .take()
                .expect("Ersetzungs-Mutation darf höchstens einmal laufen")
        }

        fn file_commit_class(&self) -> FileCommitClass {
            if self.reset_commit_class_after_reclaim && self.reclaims.load(Ordering::SeqCst) > 0 {
                FileCommitClass::Assured
            } else {
                FileCommitClass::BestEffort
            }
        }

        fn file_replace_class(&self) -> FileReplaceClass {
            if self.reset_replace_class_after_reclaim && self.reclaims.load(Ordering::SeqCst) > 0 {
                FileReplaceClass::Assured
            } else {
                FileReplaceClass::BestEffort
            }
        }
    }

    fn assert_recovery_paths(error: &io::Error, expected: &[&Path]) {
        let actual = crate::error::recovery_paths(error);
        for path in expected {
            assert!(
                actual.contains(&path.to_string_lossy().into_owned()),
                "Strukturierter Prüfpfad {} fehlt in {actual:?}",
                path.display()
            );
        }
    }

    fn commit_lost() -> io::Error {
        io::Error::new(io::ErrorKind::UnexpectedEof, "Antwort ging verloren")
    }

    const TEMP: &str = "/pane/.ziel.txt.doppeldecker-fileop-tmp-1-1";

    #[test]
    fn best_effort_klasse_bleibt_nach_reconnect_in_actuals_unklar() {
        let new_name = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
                Ok(commit_listing(&[], true)),
            ],
            Err(commit_lost()),
        )
        .resetting_classes_after_reclaim();
        let new_failure = commit_temp_file_with_phase(
            TempFileCommitRequest {
                dst: &new_name,
                temp_path: Path::new(TEMP),
                dst_path: Path::new("/pane/ziel.txt"),
                sandbox: &None,
                replace_existing: false,
                expected_replacement: None,
                expected_size: Some(0),
                expected_hash: None,
            },
            None,
        )
        .unwrap_err();
        let mut new_actuals = FileOperationActuals::default();
        match new_failure {
            TempCommitFailure::BestEffortMovedOrUnclear(_, effect) => {
                new_actuals.mark_unclear(effect)
            }
            _ => panic!("Neuanlage verlor die vor der Mutation gebundene Klasse"),
        }
        assert_eq!(new_name.file_commit_class(), FileCommitClass::Assured);
        assert_eq!(new_actuals.target_copy_new_best_effort, None);

        let replacement = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
                Ok(commit_listing(&[], true)),
            ],
            Err(commit_lost()),
        )
        .resetting_classes_after_reclaim();
        let replace_failure = commit_temp_file_with_phase(
            TempFileCommitRequest {
                dst: &replacement,
                temp_path: Path::new(TEMP),
                dst_path: Path::new("/pane/ziel.txt"),
                sandbox: &None,
                replace_existing: true,
                expected_replacement: None,
                expected_size: Some(0),
                expected_hash: None,
            },
            None,
        )
        .unwrap_err();
        let mut replace_actuals = FileOperationActuals::default();
        match replace_failure {
            TempCommitFailure::BestEffortMovedOrUnclear(_, effect) => {
                replace_actuals.mark_unclear(effect)
            }
            _ => panic!("Ersetzung verlor die vor der Mutation gebundene Klasse"),
        }
        assert_eq!(replacement.file_replace_class(), FileReplaceClass::Assured);
        assert_eq!(replace_actuals.target_replace_best_effort, None);
    }

    #[test]
    fn best_effort_replace_belegt_erfolg_mit_genau_einer_mutation() {
        let vfs = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
                Ok(commit_listing(&[("ziel.txt", false)], true)),
            ],
            Ok(()),
        );
        best_effort_replace_file(
            &vfs,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
            Some(0),
            None,
        )
        .unwrap();
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn best_effort_replace_klaert_verbindungsverlust_ohne_retry() {
        let committed = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
                Ok(commit_listing(&[("ziel.txt", false)], true)),
            ],
            Err(commit_lost()),
        );
        best_effort_replace_file(
            &committed,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
            Some(0),
            None,
        )
        .unwrap();
        assert_eq!(committed.mutation_count(), 1, "keine blinde Wiederholung");
        assert_eq!(committed.reclaims.load(Ordering::SeqCst), 1);

        let unchanged = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
            ],
            Err(commit_lost()),
        );
        let error = best_effort_replace_file(
            &unchanged,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
            Some(0),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        assert_eq!(unchanged.mutation_count(), 1);
        assert_eq!(unchanged.reclaims.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn best_effort_replace_verweigert_ordner_und_drift_vor_der_mutation() {
        let directory = ScriptedCommitVfs::new(
            vec![Ok(commit_listing(
                &[
                    (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                    ("ziel.txt", true),
                ],
                true,
            ))],
            Ok(()),
        );
        let error = best_effort_replace_file(
            &directory,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
            Some(0),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::Unsupported);
        assert_eq!(directory.mutation_count(), 0);

        let wrong_size = ScriptedCommitVfs::new(
            vec![Ok(commit_listing(
                &[
                    (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                    ("ziel.txt", false),
                ],
                true,
            ))],
            Ok(()),
        );
        let error = best_effort_replace_file(
            &wrong_size,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
            Some(11),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(wrong_size.mutation_count(), 0);
    }

    #[test]
    fn best_effort_replace_bindet_das_bestaetigte_providerziel_bis_zur_mutation() {
        let vfs = ScriptedCommitVfs::new(
            vec![Ok(commit_listing(
                &[
                    (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                    ("ziel.txt", false),
                ],
                true,
            ))],
            Ok(()),
        );
        let expected = FileConflictProof {
            path: "/pane/ziel.txt".into(),
            raw_name: b"ziel.txt".to_vec(),
            // Das frische Listing liefert bewusst kein Token mehr: Der Eintrag
            // wurde nach der Bestätigung ausgetauscht, obwohl Name und Typ gleich
            // aussehen. Vor der einzigen Provider-Mutation muss das stoppen.
            provider_token: Some(b"bestaetigte-version".to_vec()),
            is_dir: false,
            is_file: true,
            is_symlink: false,
            size: 0,
            mtime: None,
            mode: None,
            device: None,
            inode: None,
            content_hash: None,
            parent_inheritance: None,
        };
        let error = best_effort_replace_file(
            &vfs,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            Some(&expected),
            Some(0),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(vfs.mutation_count(), 0);
    }

    #[test]
    fn lokales_best_effort_ersetzen_stoppt_bei_drift_vor_der_mutation() {
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let target = root_path.join("ziel.txt");
        let prepared = root_path.join(".ziel.txt.doppeldecker-fileop-tmp-test");
        std::fs::write(&target, b"alt").unwrap();
        let vfs = crate::LocalVfs::new();
        let proof = capture_file_conflict_proofs(&vfs, &[target.to_string_lossy().into_owned()])
            .unwrap()
            .remove(0);

        // Gleicher Pfad und gleicher Typ, aber ein neuer Verzeichniseintrag nach
        // Bestätigung. Der lokale Best-Effort-Weg prüft diesen gebundenen Vorzustand
        // unmittelbar vor seinem einzigen Rename und stoppt hier mutationsfrei.
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, b"fremd").unwrap();
        std::fs::write(&prepared, b"neu").unwrap();
        let error = commit_temp_file(TempFileCommitRequest {
            dst: &vfs,
            temp_path: &prepared,
            dst_path: &target,
            sandbox: &None,
            replace_existing: true,
            expected_replacement: Some(&proof),
            expected_size: Some(3),
            expected_hash: Some(*blake3::hash(b"neu").as_bytes()),
        })
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(std::fs::read(&target).unwrap(), b"fremd");
        assert_eq!(std::fs::read(&prepared).unwrap(), b"neu");
    }

    #[test]
    fn best_effort_replace_meldet_mehrdeutigen_readback_als_unklar() {
        let vfs = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ZIEL.TXT", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
            ],
            Ok(()),
        );
        let error = best_effort_replace_file(
            &vfs,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
            Some(0),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn best_effort_replace_akzeptiert_ohne_temp_nur_einen_zieltreffer() {
        let vfs = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[
                        (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                        ("ziel.txt", false),
                    ],
                    true,
                )),
                Ok(commit_listing(
                    &[("ZIEL.TXT", false), ("ziel.txt", false)],
                    true,
                )),
            ],
            Ok(()),
        );

        let error = best_effort_replace_file(
            &vfs,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
            Some(0),
            None,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn best_effort_replace_verlangt_den_erwarteten_inhalts_hash() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ziel.txt");
        std::fs::write(&path, b"tatsaechlicher Inhalt").unwrap();
        let vfs = crate::LocalVfs::new();
        let error = verify_replaced_content(
            &vfs,
            &path,
            Some(*blake3::hash(b"erwarteter Inhalt").as_bytes()),
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    }

    #[test]
    fn best_effort_replace_verweigert_typkonflikte_vor_der_ausfuehrung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_root = root.join("quelle");
        let target_root = root.join("ziel");
        std::fs::create_dir(&source_root).unwrap();
        std::fs::create_dir(&target_root).unwrap();
        std::fs::create_dir(source_root.join("konflikt")).unwrap();
        std::fs::write(source_root.join("konflikt/kind.txt"), b"neu").unwrap();
        std::fs::write(target_root.join("konflikt"), b"alt").unwrap();
        let vfs = crate::LocalVfs::new();
        let error = validate_best_effort_file_replacements(
            &vfs,
            &vfs,
            &target_root,
            &[source_root.join("konflikt")],
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::Unsupported);
        assert_eq!(std::fs::read(target_root.join("konflikt")).unwrap(), b"alt");
    }

    #[test]
    fn best_effort_commit_stoppt_bei_spaeter_kollision_ohne_mutation() {
        let vfs = ScriptedCommitVfs::new(
            vec![Ok(commit_listing(
                &[
                    (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                    ("ziel.txt", false),
                ],
                true,
            ))],
            Ok(()),
        );
        let error =
            best_effort_commit_new(&vfs, Path::new(TEMP), Path::new("/pane/ziel.txt"), None)
                .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::AlreadyExists);
        assert_eq!(vfs.mutation_count(), 0);
    }

    #[test]
    fn best_effort_commit_stoppt_bei_casefold_kollision() {
        let vfs = ScriptedCommitVfs::new(
            vec![Ok(commit_listing(
                &[
                    (".ziel.txt.doppeldecker-fileop-tmp-1-1", false),
                    ("ZIEL.TXT", false),
                ],
                true,
            ))],
            Ok(()),
        );
        let error =
            best_effort_commit_new(&vfs, Path::new(TEMP), Path::new("/pane/ziel.txt"), None)
                .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::AlreadyExists);
        assert_eq!(vfs.mutation_count(), 0);
    }

    #[test]
    fn best_effort_commit_belegt_erfolg_per_listing_readback() {
        let vfs = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
                Ok(commit_listing(&[("ziel.txt", false)], true)),
            ],
            Ok(()),
        );
        best_effort_commit_new(&vfs, Path::new(TEMP), Path::new("/pane/ziel.txt"), None).unwrap();
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn best_effort_commit_prueft_den_tatsaechlichen_tempelternordner() {
        for temp_remains in [false, true] {
            let temp = Path::new("/private-stage/temp");
            let vfs = ScriptedCommitVfs::new(
                vec![
                    Ok(commit_listing(&[], true)),
                    Ok(commit_listing(&[("temp", false)], true)),
                    Ok(commit_listing(&[("ziel.txt", false)], true)),
                    Ok(commit_listing(
                        if temp_remains {
                            &[("temp", false)]
                        } else {
                            &[]
                        },
                        true,
                    )),
                ],
                Ok(()),
            );
            let result = best_effort_commit_new(&vfs, temp, Path::new("/pane/ziel.txt"), None);
            assert_eq!(vfs.mutation_count(), 1);
            if temp_remains {
                assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    fn best_effort_ersetzen_prueft_temp_und_ziel_in_getrennten_eltern() {
        let vfs = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(&[("ziel.txt", false)], true)),
                Ok(commit_listing(&[("temp", false)], true)),
                Ok(commit_listing(&[("ziel.txt", false)], true)),
                Ok(commit_listing(&[], true)),
            ],
            Ok(()),
        );
        best_effort_replace_file(
            &vfs,
            Path::new("/private-stage/temp"),
            Path::new("/pane/ziel.txt"),
            None,
            Some(0),
            None,
        )
        .unwrap();
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn best_effort_neunamencommit_akzeptiert_keinen_gleich_grossen_fremdinhalt() {
        let mut before = commit_listing(&[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)], true);
        before.entries[0].entry.size = 4;
        let mut after = commit_listing(&[("ziel.txt", false)], true);
        after.entries[0].entry.size = 4;
        let vfs =
            ScriptedCommitVfs::new(vec![Ok(before), Ok(after)], Ok(())).with_read_content(b"FREMD");

        let error = commit_temp_file(TempFileCommitRequest {
            dst: &vfs,
            temp_path: Path::new(TEMP),
            dst_path: Path::new("/pane/ziel.txt"),
            sandbox: &None,
            replace_existing: false,
            expected_replacement: None,
            expected_size: Some(4),
            expected_hash: Some(*blake3::hash(b"ECHT").as_bytes()),
        })
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn best_effort_commit_meldet_unveraenderten_bestand_trotz_servermeldung() {
        let vfs = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
            ],
            Ok(()),
        );
        let error =
            best_effort_commit_new(&vfs, Path::new(TEMP), Path::new("/pane/ziel.txt"), None)
                .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(vfs.mutation_count(), 1);
    }

    #[test]
    fn best_effort_commit_klaert_verbindungsverlust_per_listing() {
        // Fall A: Der Commit hat gewirkt — das klärende Listing belegt den Erfolg.
        let committed = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
                Ok(commit_listing(&[("ziel.txt", false)], true)),
            ],
            Err(commit_lost()),
        );
        best_effort_commit_new(
            &committed,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
        )
        .unwrap();
        assert_eq!(committed.mutation_count(), 1, "keine blinde Wiederholung");
        // Fall B: unverändert — der stabile Code bleibt connection_lost.
        let unchanged = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
            ],
            Err(commit_lost()),
        );
        let error = best_effort_commit_new(
            &unchanged,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        // Fall C: Klärung unmöglich — Ausgang ehrlich unklar.
        let unclear = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "weiter weg")),
            ],
            Err(commit_lost()),
        );
        let error =
            best_effort_commit_new(&unclear, Path::new(TEMP), Path::new("/pane/ziel.txt"), None)
                .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    }

    #[test]
    fn best_effort_commit_reclaimt_genau_einmal_und_meldet_scheiternde_klaerung() {
        // Erfolgsklärung nach Verbindungsverlust nutzt genau einen Reclaim.
        let committed = ScriptedCommitVfs::new(
            vec![
                Ok(commit_listing(
                    &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                    true,
                )),
                Ok(commit_listing(&[("ziel.txt", false)], true)),
            ],
            Err(commit_lost()),
        );
        best_effort_commit_new(
            &committed,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
        )
        .unwrap();
        assert_eq!(committed.reclaims.load(Ordering::SeqCst), 1);
        // Scheitert schon der identitätsgeprüfte Neuaufbau, bleibt der Ausgang unklar.
        let broken = ScriptedCommitVfs::new(
            vec![Ok(commit_listing(
                &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                true,
            ))],
            Err(commit_lost()),
        );
        *broken.reclaim_result.lock().unwrap() = Some(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "andere Serveridentität",
        )));
        let error =
            best_effort_commit_new(&broken, Path::new(TEMP), Path::new("/pane/ziel.txt"), None)
                .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(broken.reclaims.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn best_effort_commit_prueft_die_bekannte_groesse_im_readback() {
        let mut before = commit_listing(&[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)], true);
        before.entries[0].entry.size = 11;
        let vfs = ScriptedCommitVfs::new(
            vec![Ok(before), Ok(commit_listing(&[("ziel.txt", false)], true))],
            Ok(()),
        );
        // Erst der Readback meldet 0 statt der vollständig geschriebenen 11 Byte.
        let error =
            best_effort_commit_new(&vfs, Path::new(TEMP), Path::new("/pane/ziel.txt"), Some(11))
                .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    }

    #[test]
    fn best_effort_commit_verlangt_vollstaendiges_und_sicher_dekodiertes_listing() {
        let incomplete = ScriptedCommitVfs::new(
            vec![Ok(commit_listing(
                &[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)],
                false,
            ))],
            Ok(()),
        );
        let error = best_effort_commit_new(
            &incomplete,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(incomplete.mutation_count(), 0);

        let mut unsafe_listing =
            commit_listing(&[(".ziel.txt.doppeldecker-fileop-tmp-1-1", false)], true);
        let mut raw = proof_entry("kaputt", false);
        raw.safely_decoded = false;
        raw.raw_name = vec![0xff, 0xfe];
        unsafe_listing.entries.push(raw);
        let undecodable = ScriptedCommitVfs::new(vec![Ok(unsafe_listing)], Ok(()));
        let error = best_effort_commit_new(
            &undecodable,
            Path::new(TEMP),
            Path::new("/pane/ziel.txt"),
            None,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(undecodable.mutation_count(), 0);
    }

    /// Ein Nullbyte im Namen endet auf dem Weg zu einem Provider die Zeichenkette.
    /// Der Nutzer würde einen anderen Namen bestätigen als angelegt wird.
    #[test]
    fn nullbyte_im_namen_wird_abgewiesen() {
        assert!(validate_file_name("normal.txt").is_ok());
        assert!(validate_file_name("neu\0boese").is_err());
        assert!(validate_file_name("\0").is_err());
        assert!(validate_file_name("..").is_err());
        assert!(validate_file_name("a/b").is_err());
    }

    /// Regression: Ein `..` nach einer noch nicht vorhandenen Komponente führte
    /// an der Sandbox-Schranke vorbei.
    ///
    /// `canonical_existing` löst nur den nächsten vorhandenen Vorfahren auf. Bei
    /// `<sandbox>/fehlt/../../draussen/datei.txt` war das die Sandbox selbst, der
    /// Rest fiel weg — und der Pfad galt als erlaubt. Sobald `fehlt` später
    /// entsteht, zeigt derselbe Pfad aus der Sandbox heraus.
    #[test]
    fn sandbox_weist_ein_punktpunkt_nach_fehlender_komponente_ab() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let sandbox = root.join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::create_dir_all(root.join("draussen")).unwrap();

        let escape = sandbox.join("fehlt/../../draussen/datei.txt");
        let error = guard(&Some(sandbox.clone()), &escape).unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::SandboxViolation);

        // Der schon vorher erkannte Fall bleibt erkannt.
        let direct = sandbox.join("../draussen/datei.txt");
        assert_eq!(
            error_code(&guard(&Some(sandbox.clone()), &direct).unwrap_err()),
            ErrorCode::SandboxViolation
        );

        // Ein gewöhnliches, noch nicht vorhandenes Ziel bleibt erlaubt.
        guard(&Some(sandbox.clone()), &sandbox.join("neu/tief/datei.txt")).unwrap();
        guard(&Some(sandbox.clone()), &sandbox.join("datei.txt")).unwrap();
    }

    /// Testinhalt fester Länge, damit sich zwei Stände nur im Inhalt unterscheiden
    /// und nicht schon an der Dateigröße auffallen.
    fn conflict_content(tag: &str, index: usize) -> Vec<u8> {
        format!("{tag}{index:04}").into_bytes()
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,
            .. proptest::test_runner::Config::default()
        })]

        /// Ein nicht bestätigtes Ersetzungsziel darf sich niemals ändern.
        ///
        /// `expected_replacement_for` weist ein unbestätigtes Ziel mit `plan_drift`
        /// ab. Die Kopie ist aber nicht transaktional: Vorher bestätigte Ziele
        /// dürfen bereits ersetzt sein. Der Test prüft deshalb genau die enge
        /// Zusage — **kein** unbestätigtes Ziel trägt hinterher neuen Inhalt —
        /// und zusätzlich, dass ein vollständig bestätigter Satz wirklich
        /// durchläuft.
        #[test]
        fn unbestaetigte_ziele_bleiben_bei_jeder_bestaetigungsteilmenge_unberuehrt(
            confirmed_mask in proptest::collection::vec(proptest::bool::ANY, 3..6),
        ) {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let source = root.join("quelle");
            let destination = root.join("ziel");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::create_dir_all(&destination).unwrap();

            let mut items = Vec::new();
            let mut conflicts = Vec::new();
            for (index, _) in confirmed_mask.iter().enumerate() {
                let name = format!("kollision-{index}");
                std::fs::write(source.join(&name), conflict_content("N", index)).unwrap();
                std::fs::write(destination.join(&name), conflict_content("A", index)).unwrap();
                items.push(source.join(&name));
                conflicts.push(destination.join(&name));
            }

            let vfs = crate::vfs::LocalVfs::new();
            let all_proofs = capture_file_conflict_proofs(
                &vfs,
                &conflicts
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let confirmed: Vec<FileConflictProof> = all_proofs
                .iter()
                .filter(|proof| {
                    let index: usize = proof
                        .path
                        .rsplit('-')
                        .next()
                        .and_then(|value| value.parse().ok())
                        .expect("Konfliktname trägt seinen Index");
                    confirmed_mask[index]
                })
                .cloned()
                .collect();

            let result = copy_items_with_conflict_proofs(
                FileOperationRequest {
                    src: &vfs,
                    dst: &vfs,
                    items: &items,
                    dst_dir: &destination,
                    policy: OverwritePolicy::Overwrite,
                    sandbox: &None,
                },
                &confirmed,
                &crate::Cancel::new(),
                &mut |_| {},
            );

            for (index, is_confirmed) in confirmed_mask.iter().enumerate() {
                if *is_confirmed {
                    continue;
                }
                let target = destination.join(format!("kollision-{index}"));
                proptest::prop_assert_eq!(
                    std::fs::read(&target).unwrap(),
                    conflict_content("A", index),
                    "unbestätigtes Ziel {} wurde verändert",
                    target.display()
                );
            }

            if confirmed_mask.iter().all(|confirmed| *confirmed) {
                proptest::prop_assert!(
                    result.is_ok(),
                    "ein vollständig bestätigter Satz muss durchlaufen: {:?}",
                    result.err()
                );
                for index in 0..confirmed_mask.len() {
                    proptest::prop_assert_eq!(
                        std::fs::read(destination.join(format!("kollision-{index}"))).unwrap(),
                        conflict_content("N", index)
                    );
                }
            } else {
                proptest::prop_assert!(
                    result.is_err(),
                    "ein unvollständig bestätigter Satz darf nicht als Erfolg enden"
                );
                proptest::prop_assert_eq!(
                    error_code(&result.unwrap_err()),
                    ErrorCode::PlanDrift
                );
            }
        }
    }

    /// Ein Typkonflikt wird zuerst atomar geparkt und erst dort geprüft. Stimmt
    /// die Identität nicht mehr, bleibt der geparkte Eintrag ohne gebundenen
    /// Providerbeleg unter seinem Recoverynamen stehen. Ein pfadbasierter Rückweg
    /// dürfte inzwischen fremden Inhalt bewegen.
    #[test]
    fn geparkter_typkonflikt_bleibt_bei_drift_unversehrt_im_recoverypfad() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir_all(source.join("ordner")).unwrap();
        std::fs::write(source.join("ordner/inhalt.txt"), b"neu").unwrap();
        std::fs::create_dir(&destination).unwrap();
        // Typkonflikt: Am Zielnamen liegt eine Datei, kopiert wird ein Ordner.
        let conflict = destination.join("ordner");
        std::fs::write(&conflict, b"altbestand").unwrap();

        let vfs = crate::vfs::LocalVfs::new();
        let mut proofs =
            capture_file_conflict_proofs(&vfs, &[conflict.to_string_lossy().into_owned()]).unwrap();
        // So sieht ein zwischen Bestätigung und Commit ausgetauschtes Altziel aus.
        proofs[0].size += 1;

        let error = copy_items_with_conflict_proofs(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source.join("ordner")],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &proofs,
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(!conflict.exists());
        let backups: Vec<PathBuf> = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("doppeldecker-replaced"))
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"altbestand");
        assert!(error.to_string().contains(&conflict.display().to_string()));
        assert!(error
            .to_string()
            .contains(&backups[0].display().to_string()));
    }

    #[test]
    fn ordnerkopie_entfernt_keinen_nach_pruefung_ausgetauschten_sicherungsnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir_all(source.join("ordner")).unwrap();
        std::fs::write(source.join("ordner/neu.txt"), b"neu").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let conflict = destination.join("ordner");
        std::fs::write(&conflict, b"bestaetigte-altdatei").unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let proofs =
            capture_file_conflict_proofs(&vfs, &[conflict.to_string_lossy().into_owned()]).unwrap();
        let error = copy_items_with_conflict_proofs(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source.join("ordner")],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &proofs,
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        let attacked = vfs.attacked_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(
            error.to_string().contains(&attacked.display().to_string()),
            "Die Meldung muss den stehen gelassenen Recoverypfad nennen"
        );
        assert_eq!(
            std::fs::read(&attacked).unwrap(),
            b"fremd",
            "Der nach der Prüfung eingesetzte fremde Inhalt darf nie entfernt werden"
        );
        assert_eq!(
            std::fs::read(vfs.displaced_path()).unwrap(),
            b"bestaetigte-altdatei"
        );
        assert_eq!(std::fs::read(conflict.join("neu.txt")).unwrap(), b"neu");
    }

    #[test]
    fn datei_gegen_ordner_entfernt_keinen_nach_pruefung_ausgetauschten_sicherungsnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(source.join("konflikt"), b"neue-datei").unwrap();
        let conflict = destination.join("konflikt");
        std::fs::create_dir(&conflict).unwrap();
        std::fs::write(conflict.join("alt.txt"), b"bestaetigter-altordner").unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        let proofs =
            capture_file_conflict_proofs(&vfs, &[conflict.to_string_lossy().into_owned()]).unwrap();
        let error = copy_items_with_conflict_proofs(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source.join("konflikt")],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &proofs,
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        let attacked = vfs.attacked_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(
            error.to_string().contains(&attacked.display().to_string()),
            "Die Meldung muss den stehen gelassenen Recoverypfad nennen"
        );
        assert_eq!(
            std::fs::read(attacked.join("fremd.txt")).unwrap(),
            b"fremd",
            "Der nach der Prüfung eingesetzte fremde Ordner darf nie rekursiv entfernt werden"
        );
        assert_eq!(
            std::fs::read(vfs.displaced_path().join("alt.txt")).unwrap(),
            b"bestaetigter-altordner"
        );
        assert_eq!(std::fs::read(&conflict).unwrap(), b"neue-datei");
    }

    #[test]
    fn direkte_ordnerkopie_entfernt_keinen_ausgetauschten_sicherungsnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir_all(source.join("ordner")).unwrap();
        std::fs::write(source.join("ordner/neu.txt"), b"neu").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let conflict = destination.join("ordner");
        std::fs::write(&conflict, b"altbestand").unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let error = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source.join("ordner")],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        let attacked = vfs.attacked_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(&attacked.display().to_string()));
        assert_eq!(std::fs::read(&attacked).unwrap(), b"fremd");
        assert_eq!(std::fs::read(vfs.displaced_path()).unwrap(), b"altbestand");
        assert_eq!(std::fs::read(conflict.join("neu.txt")).unwrap(), b"neu");
    }

    #[test]
    fn direkte_datei_gegen_ordner_entfernt_keinen_ausgetauschten_sicherungsnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(source.join("konflikt"), b"neue-datei").unwrap();
        let conflict = destination.join("konflikt");
        std::fs::create_dir(&conflict).unwrap();
        std::fs::write(conflict.join("alt.txt"), b"altbestand").unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        let error = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source.join("konflikt")],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        let attacked = vfs.attacked_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(&attacked.display().to_string()));
        assert_eq!(std::fs::read(attacked.join("fremd.txt")).unwrap(), b"fremd");
        assert_eq!(
            std::fs::read(vfs.displaced_path().join("alt.txt")).unwrap(),
            b"altbestand"
        );
        assert_eq!(std::fs::read(&conflict).unwrap(), b"neue-datei");
    }

    #[test]
    fn dateitausch_entfernt_keinen_nach_pruefung_ausgetauschten_tempnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel.txt");
        let prepared = root.join(".ziel.txt.doppeldecker-fileop-tmp-test");
        std::fs::write(&target, b"altbestand").unwrap();
        std::fs::write(&prepared, b"neuer-inhalt").unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let proof = capture_file_conflict_proofs(&vfs, &[target.to_string_lossy().into_owned()])
            .unwrap()
            .remove(0);
        let error = commit_temp_file(TempFileCommitRequest {
            dst: &vfs,
            temp_path: &prepared,
            dst_path: &target,
            sandbox: &None,
            replace_existing: true,
            expected_replacement: Some(&proof),
            expected_size: Some(b"neuer-inhalt".len() as u64),
            expected_hash: Some(*blake3::hash(b"neuer-inhalt").as_bytes()),
        })
        .unwrap_err();

        let attacked = vfs.attacked_path();
        assert_eq!(attacked, prepared);
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(&attacked.display().to_string()));
        assert_eq!(std::fs::read(&target).unwrap(), b"neuer-inhalt");
        assert_eq!(
            std::fs::read(&attacked).unwrap(),
            b"fremd",
            "Der nach der Prüfung eingesetzte fremde Tempname darf nie entfernt werden"
        );
        assert_eq!(std::fs::read(vfs.displaced_path()).unwrap(), b"altbestand");
    }

    #[test]
    fn abbruch_entfernt_keinen_ausgetauschten_datei_tempnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.bin");
        let destination = root.join("ziel");
        std::fs::write(&source, vec![b'N'; 256 * 1024]).unwrap();
        std::fs::create_dir(&destination).unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let cancel = crate::Cancel::new();
        let cancel_from_progress = cancel.clone();
        let error = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &cancel,
            &mut |progress| {
                if progress.bytes_done > 0 {
                    cancel_from_progress.cancel();
                }
            },
        )
        .unwrap_err();

        let attacked = vfs.attacked_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(&attacked.display().to_string()));
        assert_eq!(std::fs::read(&attacked).unwrap(), b"fremd");
        assert!(!std::fs::read(vfs.displaced_path()).unwrap().is_empty());
    }

    #[test]
    fn lesefehler_entfernt_keinen_ausgetauschten_datei_tempnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.bin");
        let destination = root.join("ziel");
        std::fs::write(&source, b"lesbarer-anfang").unwrap();
        std::fs::create_dir(&destination).unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.fail_read_at(&source);
        let error = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        let attacked = vfs.attacked_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(&attacked.display().to_string()));
        assert_eq!(std::fs::read(&attacked).unwrap(), b"fremd");
        assert_eq!(
            std::fs::read(vfs.displaced_path()).unwrap(),
            b"lesbarer-anfang"
        );
    }

    #[test]
    fn plan_drift_nach_exchange_laesst_beide_recoverypfade_unveraendert() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"neuer-inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&target, b"altbestand").unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let proofs =
            capture_file_conflict_proofs(&vfs, &[target.to_string_lossy().into_owned()]).unwrap();
        vfs.attack_target_before_exchange(&target);
        let error = copy_items_with_conflict_proofs(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &proofs,
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(vfs.exchange_count(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"neuer-inhalt");
        let recovery_temp = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("doppeldecker-fileop-tmp"))
            })
            .unwrap();
        assert_eq!(std::fs::read(&recovery_temp).unwrap(), b"fremdes-ziel");
        assert_eq!(
            std::fs::read(vfs.exchange_displaced_path()).unwrap(),
            b"altbestand"
        );
        assert!(error.to_string().contains(&target.display().to_string()));
        assert!(error
            .to_string()
            .contains(&recovery_temp.display().to_string()));
        assert_recovery_paths(&error, &[&target, &recovery_temp]);
    }

    #[test]
    fn commit_syncfehler_meldet_sichtbares_ziel_statt_falscher_temp_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"neuer-inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.arm_sync_failure();
        let error = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&target).unwrap(), b"neuer-inhalt");
        assert!(error.to_string().contains(&target.display().to_string()));
        assert!(error.to_string().contains("Verzeichnis-Sync scheiterte"));
        assert!(
            !error.to_string().contains("Temp-Recoverypfad"),
            "Nach dem sichtbaren Rename darf der verschwundene Tempname nicht als Recoverypfad gemeldet werden: {error}"
        );
        assert!(
            std::fs::read_dir(&destination).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("fileop-tmp")
            }),
            "Nach dem sichtbaren Rename darf keine Temp-Datei behauptet werden"
        );
        assert_recovery_paths(&error, &[&target]);
        assert!(crate::error::recovery_paths(&error)
            .iter()
            .all(|path| !path.contains("fileop-tmp")));
    }

    #[test]
    fn exchange_fehler_nach_sichtbarer_mutation_tauscht_nicht_pfadbasiert_zurueck() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel.txt");
        let prepared = root.join(".ziel.txt.doppeldecker-fileop-tmp-test");
        std::fs::write(&target, b"altbestand").unwrap();
        std::fs::write(&prepared, b"neuer-inhalt").unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let proof = capture_file_conflict_proofs(&vfs, &[target.to_string_lossy().into_owned()])
            .unwrap()
            .remove(0);
        vfs.arm_sync_failure();

        let error = commit_temp_file(TempFileCommitRequest {
            dst: &vfs,
            temp_path: &prepared,
            dst_path: &target,
            sandbox: &None,
            replace_existing: true,
            expected_replacement: Some(&proof),
            expected_size: Some(b"neuer-inhalt".len() as u64),
            expected_hash: Some(*blake3::hash(b"neuer-inhalt").as_bytes()),
        })
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(
            vfs.exchange_count(),
            1,
            "Nach dem ersten Exchange ist jede zweite pfadgebundene Mutation verboten"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"neuer-inhalt");
        assert_eq!(std::fs::read(&prepared).unwrap(), b"altbestand");
        assert!(error.to_string().contains(&target.display().to_string()));
        assert!(error.to_string().contains(&prepared.display().to_string()));
        assert_recovery_paths(&error, &[&target, &prepared]);
    }

    #[test]
    fn restore_parkt_keine_fremd_ausgetauschte_sicherungsquelle_zurueck() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel.txt");
        let backup = root.join(".ziel.txt.doppeldecker-replaced-test");
        std::fs::write(&backup, b"altbestand").unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let backup_proof = vfs.atomic_bound_proof(&backup).unwrap();
        vfs.attack_backup_before_restore_to(&target);

        let error = restore_parked_target(
            &vfs,
            &backup,
            &target,
            Some(&backup_proof),
            io::Error::new(io::ErrorKind::PermissionDenied, "erzwungener Folgefehler"),
        );

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(
            !target.exists(),
            "Der fremde Sicherungseintrag darf nicht an den Zielnamen verschoben werden"
        );
        assert_eq!(std::fs::read(&backup).unwrap(), b"fremde-sicherung");
        assert_eq!(
            std::fs::read(vfs.restore_displaced_path()).unwrap(),
            b"altbestand"
        );
        assert!(error.to_string().contains(&target.display().to_string()));
        assert!(error.to_string().contains(&backup.display().to_string()));
        assert_recovery_paths(&error, &[&target, &backup]);
    }

    #[test]
    fn rollback_benennt_keine_ausgetauschte_sicherungsquelle_zurueck() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel");
        let backup = root.join(".ziel.doppeldecker-replaced-test");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(&backup, b"altbestand").unwrap();

        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let target_proof = vfs.atomic_bound_proof(&target).unwrap();
        let backup_proof = vfs.atomic_bound_proof(&backup).unwrap();
        vfs.attack_backup_before_restore_to(&target);

        let error = rollback_created_target(
            &vfs,
            &target,
            Some(&backup),
            Some(&backup_proof),
            Some(&target_proof),
            Some(coded_error(
                io::ErrorKind::Other,
                ErrorCode::PlanDrift,
                "erzwungener Kopierfehler",
            )),
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(!target.exists());
        assert_eq!(std::fs::read(&backup).unwrap(), b"fremde-sicherung");
        assert_eq!(
            std::fs::read(vfs.restore_displaced_path()).unwrap(),
            b"altbestand"
        );
        assert!(error.to_string().contains(&target.display().to_string()));
        assert!(error.to_string().contains(&backup.display().to_string()));
        assert_recovery_paths(&error, &[&target, &backup]);
    }

    #[test]
    fn move_fallback_entfernt_keine_ausgetauschte_quell_datei() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"original").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.attack_move_source_before_remove(&source);

        let error = move_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&source).unwrap(), b"fremde-quelle");
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert_eq!(
            std::fs::read(vfs.move_displaced_path()).unwrap(),
            b"original"
        );
        assert!(error.to_string().contains(&source.display().to_string()));
    }

    #[test]
    fn move_zaehlfehler_nach_rename_meldet_vollzogene_wirkung_mit_beiden_pfaden() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.fail_stat_after_rename_at(&target);

        let error = move_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(
            !source.exists(),
            "der gesyncte Rename ist bereits vollzogen"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"inhalt");
        let message = error.to_string();
        assert!(message.contains(&source.display().to_string()));
        assert!(message.contains(&target.display().to_string()));
    }

    #[test]
    fn move_fallback_entfernt_keinen_ausgetauschten_quellbaum() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        let target = destination.join("quelle");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("original.txt"), b"original").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        vfs.attack_move_source_before_remove(&source);

        let error = move_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(
            std::fs::read(source.join("fremd.txt")).unwrap(),
            b"fremder-baum"
        );
        assert_eq!(
            std::fs::read(target.join("original.txt")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(vfs.move_displaced_path().join("original.txt")).unwrap(),
            b"original"
        );
        assert!(error.to_string().contains(&source.display().to_string()));
    }

    #[test]
    fn mutate_then_error_gilt_bei_anlage_und_parken_nie_als_unveraendert() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();

        let source_dir = root.join("quellordner");
        let destination_dir = root.join("zielordner");
        let created_target = destination_dir.join("quellordner");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::write(source_dir.join("kind.txt"), b"inhalt").unwrap();
        std::fs::create_dir(&destination_dir).unwrap();
        let create_vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        create_vfs.fail_create_after_mutation_at(&created_target);
        let create_error = copy_items(
            FileOperationRequest {
                src: &create_vfs,
                dst: &create_vfs,
                items: std::slice::from_ref(&source_dir),
                dst_dir: &destination_dir,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(error_code(&create_error), ErrorCode::OutcomeUnclear);
        assert!(created_target.is_dir());
        assert!(create_error
            .to_string()
            .contains(&created_target.display().to_string()));

        let source_file = root.join("quelldatei.txt");
        let file_destination = root.join("dateiziel");
        std::fs::write(&source_file, b"inhalt").unwrap();
        std::fs::create_dir(&file_destination).unwrap();
        let open_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        open_vfs.fail_next_open_after_mutation();
        let open_error = copy_items(
            FileOperationRequest {
                src: &open_vfs,
                dst: &open_vfs,
                items: std::slice::from_ref(&source_file),
                dst_dir: &file_destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(error_code(&open_error), ErrorCode::OutcomeUnclear);
        let open_recovery = std::fs::read_dir(&file_destination)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("fileop-tmp"))
            })
            .unwrap();
        assert!(open_error
            .to_string()
            .contains(&open_recovery.display().to_string()));

        let parked_source = root.join("parkquelle");
        let parked_destination = root.join("parkziel");
        let parked_target = parked_destination.join("parkquelle");
        std::fs::create_dir(&parked_source).unwrap();
        std::fs::write(parked_source.join("kind.txt"), b"neu").unwrap();
        std::fs::create_dir(&parked_destination).unwrap();
        std::fs::write(&parked_target, b"alt").unwrap();
        let park_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        park_vfs.fail_next_park_after_mutation();
        let park_error = copy_items(
            FileOperationRequest {
                src: &park_vfs,
                dst: &park_vfs,
                items: std::slice::from_ref(&parked_source),
                dst_dir: &parked_destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        let parked_recovery = park_vfs.observed_backup_path();
        assert_eq!(error_code(&park_error), ErrorCode::OutcomeUnclear);
        assert!(!parked_target.exists());
        assert_eq!(std::fs::read(&parked_recovery).unwrap(), b"alt");
        assert!(park_error
            .to_string()
            .contains(&parked_target.display().to_string()));
        assert!(park_error
            .to_string()
            .contains(&parked_recovery.display().to_string()));
        let missing = [
            ("create", &create_error, vec![created_target]),
            ("open", &open_error, vec![open_recovery]),
            ("park", &park_error, vec![parked_target, parked_recovery]),
        ]
        .into_iter()
        .filter_map(|(phase, error, expected)| {
            let paths = crate::error::recovery_paths(error);
            expected
                .iter()
                .any(|path| !paths.contains(&path.to_string_lossy().into_owned()))
                .then_some((phase, paths))
        })
        .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "Fehlende strukturierte Wirkungsnamen: {missing:?}"
        );
    }

    #[test]
    fn testdouble_erzeugt_entfernungsproof_ohne_nachtraegliches_listing() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let directory = root.join("ordner");
        let file = root.join("datei.txt");
        let moved = root.join("verschoben.txt");

        assert!(vfs
            .create_dir_with_removal_proof(&directory)
            .unwrap()
            .is_some());
        let (writer, proof) = vfs.open_write_new_with_removal_proof(&file).unwrap();
        drop(writer);
        assert!(proof.is_some());
        assert!(vfs
            .rename_noreplace_with_removal_proof(&file, &moved)
            .unwrap()
            .is_some());
        assert_eq!(
            vfs.proof_at_count(),
            0,
            "Der Fake muss den unveränderlichen Provider-Token innerhalb der Mutationsaktion liefern"
        );
    }

    #[test]
    fn park_syncfehler_behaelt_errorcode_nach_sicherem_restore() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("ziel.txt");
        std::fs::write(&target, b"altbestand").unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.arm_coded_sync_failure(ErrorCode::ConnectionLost);

        let error = match park_existing_target(&vfs, &target, &None) {
            Ok(_) => panic!("Der erzwungene Sync-Fehler muss sichtbar bleiben"),
            Err(error) => error,
        };

        assert_eq!(error_code(&error), ErrorCode::ConnectionLost);
        assert_eq!(std::fs::read(&target).unwrap(), b"altbestand");
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("doppeldecker-replaced")
        }));
    }

    #[test]
    fn rollback_entfernt_keinen_ausgetauschten_zielbaum() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir_all(source.join("projekt")).unwrap();
        std::fs::write(source.join("projekt/neu.txt"), b"neu").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let conflict = destination.join("projekt");
        std::fs::write(&conflict, b"altbestand").unwrap();

        let vfs = FileopsRaceVfs::swapping_created_target(&conflict);
        let proofs =
            capture_file_conflict_proofs(&vfs, &[conflict.to_string_lossy().into_owned()]).unwrap();
        let error = copy_items_with_conflict_proofs(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source.join("projekt")],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &proofs,
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        let backup = vfs.observed_backup_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(&conflict.display().to_string()));
        assert!(error.to_string().contains(&backup.display().to_string()));
        assert_eq!(
            std::fs::read(conflict.join("fremd.txt")).unwrap(),
            b"fremd",
            "Der nach dem Anlegen ausgetauschte Zielbaum darf nie rekursiv entfernt werden"
        );
        assert_eq!(std::fs::read(&backup).unwrap(), b"altbestand");
        assert_eq!(
            std::fs::read(vfs.rollback_displaced_path().join("neu.txt")).unwrap(),
            b"neu"
        );
        assert_eq!(
            vfs.conditional_remove_count(),
            1,
            "Der Rollback muss die bei der Anlage gebundene bedingte Entfernung aufrufen"
        );
    }

    #[test]
    fn best_effort_move_entfernt_frisch_verifizierte_dateiquelle() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let source_vfs = crate::LocalVfs::new();
        let destination_vfs = crate::LocalVfs::new();

        let stats = move_items(
            FileOperationRequest {
                src: &source_vfs,
                dst: &destination_vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap();

        assert!(stats.source_removal_best_effort);
        assert!(!source.exists());
        assert_eq!(std::fs::read(target).unwrap(), b"inhalt");
    }

    #[test]
    fn fehler_behaelt_fruehere_best_effort_zielwirkung_in_teilbilanz() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_dir = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let first = source_dir.join("eins.txt");
        let second = source_dir.join("zwei.txt");
        std::fs::write(&first, b"eins").unwrap();
        std::fs::write(&second, b"zwei").unwrap();

        let source_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        source_vfs.fail_best_effort_remove_at(&second);
        let target_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        target_vfs.use_best_effort_commit();
        let mut actuals = FileOperationActuals::default();

        let error = move_items_with_conflict_proofs_tracked(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: &[first.clone(), second.clone()],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &[],
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(actuals.target_copy_new_best_effort, Some(true));
        assert_eq!(actuals.source_removal_best_effort, Some(true));
        assert!(!first.exists());
        assert!(second.exists());
        assert_eq!(
            std::fs::read(destination.join("eins.txt")).unwrap(),
            b"eins"
        );
        assert_eq!(
            std::fs::read(destination.join("zwei.txt")).unwrap(),
            b"zwei"
        );
    }

    #[test]
    fn actuals_behalten_fruehere_best_effort_neuanlage_bei_spaeter_unklarheit() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_dir = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let first = source_dir.join("eins.txt");
        let second = source_dir.join("zwei.txt");
        std::fs::write(&first, b"eins").unwrap();
        std::fs::write(&second, b"zwei").unwrap();

        let source_vfs = crate::LocalVfs::new();
        let target_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        target_vfs.use_best_effort_commit();
        target_vfs.fail_plain_rename_on_call(2);
        let mut actuals = FileOperationActuals::default();

        let error = copy_items_with_conflict_proofs_tracked(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: &[first, second],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &[],
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(actuals.target_copy_new_best_effort, Some(true));
        assert_eq!(
            std::fs::read(destination.join("eins.txt")).unwrap(),
            b"eins"
        );
    }

    #[test]
    fn actuals_behalten_fruehere_best_effort_neuanlage_bei_spaeter_unveraenderter_absage() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_dir = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let first = source_dir.join("eins.txt");
        let second = source_dir.join("ordner");
        std::fs::write(&first, b"eins").unwrap();
        std::fs::create_dir(&second).unwrap();

        let source_vfs = crate::LocalVfs::new();
        let target_vfs = FileopsRaceVfs::failing_create_dir(&destination.join("ordner"));
        target_vfs.use_best_effort_commit();
        let mut actuals = FileOperationActuals::default();

        let error = copy_items_with_conflict_proofs_tracked(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: &[first, second],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &[],
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(actuals.target_copy_new_best_effort, Some(true));
        assert_eq!(
            std::fs::read(destination.join("eins.txt")).unwrap(),
            b"eins"
        );
        assert!(!destination.join("ordner").exists());
    }

    #[test]
    fn actuals_behalten_fruehere_best_effort_ersetzung_bei_spaeter_unklarheit() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source_dir = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let first = source_dir.join("eins.txt");
        let second = source_dir.join("zwei.txt");
        let first_target = destination.join("eins.txt");
        let second_target = destination.join("zwei.txt");
        std::fs::write(&first, b"eins-neu").unwrap();
        std::fs::write(&second, b"zwei-neu").unwrap();
        std::fs::write(&first_target, b"eins-alt").unwrap();
        std::fs::write(&second_target, b"zwei-alt").unwrap();

        let source_vfs = crate::LocalVfs::new();
        let target_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        target_vfs.use_best_effort_commit();
        let conflicts = capture_file_conflict_proofs(
            &target_vfs,
            &[
                first_target.to_str().unwrap().to_owned(),
                second_target.to_str().unwrap().to_owned(),
            ],
        )
        .unwrap();
        target_vfs.fail_replace_plain_rename_on_call(2);
        let mut actuals = FileOperationActuals::default();

        let error = copy_items_with_conflict_proofs_tracked(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: &[first, second],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &conflicts,
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(actuals.target_replace_best_effort, Some(true));
        assert_eq!(std::fs::read(first_target).unwrap(), b"eins-neu");
    }

    #[test]
    fn create_dir_fehler_nach_moeglicher_wirkung_markiert_ziel_actual_unklar() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        let source_dir = source.join("projekt");
        let target_dir = destination.join("projekt");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let source_vfs = crate::LocalVfs::new();
        let target_vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        target_vfs.use_best_effort_commit();
        target_vfs.fail_create_after_mutation_at(&target_dir);
        let mut actuals = FileOperationActuals::default();

        let error = copy_items_with_conflict_proofs_tracked(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: std::slice::from_ref(&source_dir),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &[],
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(actuals.target_copy_new_best_effort, None);
        assert!(target_dir.is_dir());
    }

    #[test]
    fn rein_lesender_quell_verifyfehler_startet_keinen_best_effort_remove() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let source_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        source_vfs.fail_best_effort_remove_at(&root.join("nur-proof-deaktivieren"));
        // Auswahlprüfung, Fortschrittsscan, Symlinkprüfung, Quellbindung und
        // Kopierlesen liegen davor; Aufruf sechs ist die rein lesende Prüfung im
        // Quellabbau unmittelbar vor seiner ersten möglichen Wirkung.
        source_vfs.fail_stat_on_call(6);
        let target_vfs = crate::LocalVfs::new();
        let mut actuals = FileOperationActuals::default();

        let error = move_items_with_conflict_proofs_tracked(
            FileOperationRequest {
                src: &source_vfs,
                dst: &target_vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &[],
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(actuals.source_removal_best_effort, Some(false));
        assert_eq!(std::fs::read(&source).unwrap(), b"inhalt");
        assert_eq!(
            std::fs::read(destination.join("quelle.txt")).unwrap(),
            b"inhalt"
        );
    }

    #[test]
    fn nicht_utf8_quellname_stoppt_rekursive_kopie_vor_jeder_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let selected = source.join("ordner");
        let destination = root.join("ziel");
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("fremd.txt"), b"fremd").unwrap();
        let source_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        source_vfs.return_unsafe_listing_at(&selected);

        let error = copy_items(
            FileOperationRequest {
                src: &source_vfs,
                dst: &crate::LocalVfs::new(),
                items: std::slice::from_ref(&selected),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert!(!destination.join("ordner").exists());
        assert_eq!(
            std::fs::read(destination.join("fremd.txt")).unwrap(),
            b"fremd"
        );
        assert_eq!(std::fs::read_dir(&selected).unwrap().count(), 0);
    }

    #[test]
    fn additive_kopie_bilanziert_undekodierbaren_provider_kindnamen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let selected = root.join("quelle/ordner");
        let destination = root.join("ziel");
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(selected.join("sicher.txt"), b"sicher").unwrap();
        let source_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        source_vfs.return_unsafe_listing_at(&selected);
        let stats = copy_items(
            FileOperationRequest {
                src: &source_vfs,
                dst: &crate::LocalVfs::new(),
                items: std::slice::from_ref(&selected),
                dst_dir: &destination,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .expect("Der sichere unabhängige Kindinhalt muss kopierbar sein");
        assert_eq!(stats.files, 1);
        assert_eq!(stats.skipped, 1);
        assert!(stats
            .source_read_limitations
            .join("\n")
            .contains("726177ff"));
        assert_eq!(
            std::fs::read(destination.join("ordner/sicher.txt")).unwrap(),
            b"sicher"
        );
        assert_eq!(
            std::fs::read(selected.join("sicher.txt")).unwrap(),
            b"sicher"
        );
        assert_eq!(
            std::fs::read_dir(destination.join("ordner"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn traversalname_im_providerlisting_stoppt_vor_jeder_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let selected = source.join("ordner");
        let destination = root.join("ziel");
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::write(source.join("ausbruch.txt"), b"ausbruch").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let source_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        source_vfs.return_traversal_listing_at(&selected);

        let error = copy_items(
            FileOperationRequest {
                src: &source_vfs,
                dst: &crate::LocalVfs::new(),
                items: std::slice::from_ref(&selected),
                dst_dir: &destination,
                policy: OverwritePolicy::Skip,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn ziel_stat_fehler_stoppt_dateikopie_vor_jeder_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let target_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        target_vfs.fail_stat_at(&target);

        let error = copy_items(
            FileOperationRequest {
                src: &crate::LocalVfs::new(),
                dst: &target_vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn move_prueft_alle_sandbox_quellen_vor_der_ersten_mutation() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sandbox_root = std::fs::canonicalize(sandbox.path()).unwrap();
        let outside_root = std::fs::canonicalize(outside.path()).unwrap();
        let inside_source = sandbox_root.join("innen.txt");
        let outside_source = outside_root.join("aussen.txt");
        let destination = sandbox_root.join("ziel");
        std::fs::write(&inside_source, b"innen").unwrap();
        std::fs::write(&outside_source, b"aussen").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = crate::LocalVfs::new();

        let error = move_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[inside_source.clone(), outside_source.clone()],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &Some(sandbox_root),
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::SandboxViolation);
        assert_eq!(std::fs::read(&inside_source).unwrap(), b"innen");
        assert_eq!(std::fs::read(&outside_source).unwrap(), b"aussen");
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn mkdir_und_rename_melden_fehler_nach_sichtbarer_wirkung_als_unklar() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();

        let mkdir_vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        mkdir_vfs.arm_sync_failure();
        let mkdir_error = create_dir(&mkdir_vfs, &root, "neu", &None).unwrap_err();
        assert_eq!(error_code(&mkdir_error), ErrorCode::OutcomeUnclear);
        assert!(root.join("neu").is_dir());

        let old = root.join("alt.txt");
        let new = root.join("umbenannt.txt");
        std::fs::write(&old, b"inhalt").unwrap();
        let rename_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        rename_vfs.arm_sync_failure();
        let rename_error = rename(&rename_vfs, &old, "umbenannt.txt", &None).unwrap_err();
        assert_eq!(error_code(&rename_error), ErrorCode::OutcomeUnclear);
        assert!(!old.exists());
        assert_eq!(std::fs::read(new).unwrap(), b"inhalt");
    }

    #[cfg(unix)]
    #[test]
    fn nicht_utf8_auswahlpfad_wird_vor_jeder_mutation_abgelehnt() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("ziel");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("fremd.txt"), b"fremd").unwrap();
        let selected = temp
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'd', b'a', b't', 0xff]));

        let error = copy_items(
            FileOperationRequest {
                src: &crate::LocalVfs::new(),
                dst: &crate::LocalVfs::new(),
                items: &[selected],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::InvalidData);
        assert_eq!(
            std::fs::read(destination.join("fremd.txt")).unwrap(),
            b"fremd"
        );
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn move_merge_prueft_vorhandenen_zielordnermodus_statt_quellmodus() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle/ordner");
        let destination = root.join("ziel");
        let target = destination.join("ordner");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("neu.txt"), b"neu").unwrap();
        std::fs::write(target.join("alt.txt"), b"alt").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let vfs = crate::LocalVfs::new();

        let stats = move_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(stats.files, 1);
        assert!(!source.exists());
        assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"neu");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn typkonflikt_cleanup_bilanziert_erhaltene_sicherung_in_beiden_richtungen() {
        for source_is_directory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            std::fs::write(root.join(".doppeldecker-test-sandbox"), b"Paket 01").unwrap();
            let source = root.join("quelle/eintrag");
            let destination = root.join("ziel");
            let target = destination.join("eintrag");
            std::fs::create_dir(root.join("quelle")).unwrap();
            std::fs::create_dir(&destination).unwrap();
            if source_is_directory {
                std::fs::create_dir(&source).unwrap();
                std::fs::write(source.join("neu.txt"), b"neu").unwrap();
                std::fs::write(&target, b"alt").unwrap();
            } else {
                std::fs::write(&source, b"neu").unwrap();
                std::fs::create_dir(&target).unwrap();
                std::fs::write(target.join("alt.txt"), b"alt").unwrap();
            }
            let vfs = crate::LocalVfs::rooted(&root).unwrap();
            let proofs =
                capture_file_conflict_proofs(&vfs, &[target.to_string_lossy().into_owned()])
                    .unwrap();
            let mut actuals = FileOperationActuals::default();
            let error = copy_items_with_conflict_proofs_tracked(
                FileOperationRequest {
                    src: &vfs,
                    dst: &vfs,
                    items: std::slice::from_ref(&source),
                    dst_dir: &destination,
                    policy: OverwritePolicy::Overwrite,
                    sandbox: &Some(root.clone()),
                },
                &proofs,
                &Cancel::new(),
                &mut |_| {},
                &mut actuals,
            )
            .unwrap_err();

            assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
            assert_eq!(actuals.temp_recovery_paths.len(), 1);
            let backup = PathBuf::from(&actuals.temp_recovery_paths[0]);
            assert_eq!(backup.parent(), Some(destination.as_path()));
            assert!(error.to_string().contains(&backup.display().to_string()));
            let (new_content, old_content) = if source_is_directory {
                (target.join("neu.txt"), backup)
            } else {
                (target, backup.join("alt.txt"))
            };
            assert_eq!(std::fs::read(new_content).unwrap(), b"neu");
            assert_eq!(std::fs::read(old_content).unwrap(), b"alt");
        }
    }

    #[test]
    fn gleiche_remote_pfadtexte_in_getrennten_namespaces_sind_keine_selbstkopie() {
        let source = FileopsRaceVfs::different_remote_namespace();
        let target = FileopsRaceVfs::different_remote_namespace();

        validate_transfer_targets(&source, &target, &[PathBuf::from("/foo")], Path::new("/"))
            .unwrap();
    }

    #[test]
    fn localer_temp_cleanup_entfernt_keinen_nach_readback_ausgetauschten_namen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.bin");
        let destination = root.join("ziel");
        std::fs::write(&source, vec![b'N'; 32 * 1024]).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.fail_read_at(&source);
        vfs.attack_unproved_temp_before_remove();
        let mut actuals = FileOperationActuals::default();

        let error = copy_items_with_conflict_proofs_tracked(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &[],
            &Cancel::new(),
            &mut |_| {},
            &mut actuals,
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::Io);
        let foreign = vfs.attacked_path();
        let displaced = vfs.displaced_path();
        assert_eq!(std::fs::read(&foreign).unwrap(), b"fremd");
        assert!(displaced.exists());
        assert!(error.to_string().contains(&foreign.display().to_string()));
        assert!(error
            .to_string()
            .contains("kann inzwischen einen fremden Eintrag bezeichnen"));
        assert_eq!(
            actuals.temp_recovery_paths,
            vec![foreign.to_string_lossy().into_owned()],
            "Der Fehlerpfad muss den Recoverynamen zusätzlich zum Text typisiert behalten"
        );
        assert_eq!(
            vfs.conditional_remove_count(),
            0,
            "Ein erst nach der Anlage gelisteter Tempname ist kein Besitzbeleg und darf die bedingte Entfernung gar nicht erreichen"
        );
    }

    #[test]
    fn lokaler_abbruch_laesst_ungebundene_tempdatei_als_recoverypfad_stehen() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.bin");
        let destination = root.join("ziel");
        std::fs::write(&source, vec![b'N'; 512 * 1024]).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = crate::LocalVfs::new();
        let cancel = crate::Cancel::new();
        let callback_cancel = cancel.clone();

        let stats = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &cancel,
            &mut |progress| {
                if progress.bytes_done > 0 {
                    callback_cancel.cancel();
                }
            },
        )
        .unwrap();

        assert!(stats.cancelled);
        assert_eq!(stats.temp_recovery_paths.len(), 1);
        let recovery = PathBuf::from(&stats.temp_recovery_paths[0]);
        assert!(recovery.exists());
        assert!(recovery
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains("doppeldecker-fileop-tmp")));
    }

    #[test]
    fn move_schnellweg_meldet_fehler_nach_sichtbarem_rename_als_unklar() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.fail_next_park_after_mutation();

        let error = move_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(!source.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"inhalt");
        assert!(error.to_string().contains(&source.display().to_string()));
        assert!(error.to_string().contains(&target.display().to_string()));
    }

    #[test]
    fn rekursiver_rootbeleg_erlaubt_keine_remove_dir_all_sicherung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let backup = root.join("sicherung");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("kind.txt"), b"wichtig").unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        let proof = vfs.atomic_bound_proof(&backup).unwrap();

        let error = remove_verified_backup(&vfs, &backup, &proof).unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(backup.join("kind.txt")).unwrap(), b"wichtig");
    }

    #[test]
    fn testproof_schliesst_das_fenster_zwischen_mutation_und_bindung() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let target = root.join("neu.txt");
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        vfs.attack_between_mutation_and_proof_at(&target);

        let (writer, proof) = vfs.open_write_new_with_removal_proof(&target).unwrap();
        drop(writer);
        let error = vfs
            .remove_if_matches(&target, &proof.expect("atomarer Testbeleg"))
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target).unwrap(), b"fremd-im-proof-fenster");
        assert!(vfs.proof_window_displaced_path().exists());
    }

    #[test]
    #[cfg(unix)]
    fn move_readback_bindet_unterstuetzten_modus_vor_quellentfernung() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle.txt");
        let destination = root.join("ziel");
        let target = destination.join("quelle.txt");
        std::fs::write(&source, b"inhalt").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let source_vfs = crate::LocalVfs::new();
        let destination_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        destination_vfs.attack_move_destination_mode_before_readback(&target);

        let error = move_items(
            FileOperationRequest {
                src: &source_vfs,
                dst: &destination_vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(source.exists());
        assert!(error.to_string().contains("Metadaten"));
    }

    #[test]
    fn ordnerziel_sync_nach_abschliessenden_metadaten_ist_pflicht() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        let target = destination.join("quelle");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::Directory);
        vfs.fail_sync_on_call(2);

        let error = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: std::slice::from_ref(&source),
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(error.to_string().contains(&source.display().to_string()));
        assert!(error.to_string().contains(&target.display().to_string()));
    }

    #[test]
    fn restore_syncfehler_nennt_das_tatsaechlich_sichtbare_altziel() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let destination = root.join("ziel");
        let backup = root.join(".ziel.doppeldecker-replaced-test");
        std::fs::write(&backup, b"altbestand").unwrap();
        let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        let backup_proof = vfs.atomic_bound_proof(&backup).unwrap();
        vfs.arm_sync_failure();

        let error = restore_parked_target(
            &vfs,
            &backup,
            &destination,
            Some(&backup_proof),
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::PlanDrift,
                "erzwungene Drift",
            ),
        );

        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&destination).unwrap(), b"altbestand");
        assert!(!backup.exists());
        assert!(error
            .to_string()
            .contains(&destination.display().to_string()));
        assert!(error.to_string().contains(&backup.display().to_string()));
        assert!(
            error.to_string().contains("sichtbares Altziel"),
            "Nach erfolgreichem Rename darf die Meldung nicht behaupten, die Datei liege noch in der Sicherung: {error}"
        );
        assert_recovery_paths(&error, &[&destination]);
        assert!(
            !crate::error::recovery_paths(&error).contains(&backup.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn create_dir_fehler_mit_restore_syncfehler_meldet_beide_pfade() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("quelle");
        let destination = root.join("ziel");
        std::fs::create_dir_all(source.join("projekt")).unwrap();
        std::fs::write(source.join("projekt/neu.txt"), b"neu").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let conflict = destination.join("projekt");
        std::fs::write(&conflict, b"altbestand").unwrap();

        let vfs = FileopsRaceVfs::failing_create_dir(&conflict);
        let proofs =
            capture_file_conflict_proofs(&vfs, &[conflict.to_string_lossy().into_owned()]).unwrap();
        let error = copy_items_with_conflict_proofs(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &[source.join("projekt")],
                dst_dir: &destination,
                policy: OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &proofs,
            &crate::Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();

        let backup = vfs.observed_backup_path();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(std::fs::read(&conflict).unwrap(), b"altbestand");
        assert!(!backup.exists());
        assert!(error.to_string().contains(&conflict.display().to_string()));
        assert!(error.to_string().contains(&backup.display().to_string()));
        assert!(error.to_string().contains("sichtbares Altziel"));
    }

    #[test]
    fn interaktives_mkdir_unterscheidet_unveraenderte_und_unklare_anlagefehler() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();

        let unchanged_path = root.join("unveraendert");
        let unchanged_vfs = FileopsRaceVfs::failing_create_dir(&unchanged_path);
        let unchanged = create_dir(&unchanged_vfs, &root, "unveraendert", &None).unwrap_err();
        assert_eq!(unchanged.kind(), io::ErrorKind::PermissionDenied);
        assert_ne!(error_code(&unchanged), ErrorCode::OutcomeUnclear);
        assert!(!unchanged_path.exists());

        let unclear_path = root.join("unklar");
        let unclear_vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
        unclear_vfs.fail_create_after_mutation_at(&unclear_path);
        let unclear = create_dir(&unclear_vfs, &root, "unklar", &None).unwrap_err();
        assert_eq!(error_code(&unclear), ErrorCode::OutcomeUnclear);
        assert!(
            unclear_path.is_dir(),
            "die mögliche Wirkung bleibt sichtbar"
        );
        assert!(unclear
            .to_string()
            .contains(&unclear_path.display().to_string()));
    }

    #[test]
    fn rename_fallback_gilt_nur_fuer_dateisystemgrenze() {
        assert!(rename_allows_copy_fallback(&io::Error::from(
            io::ErrorKind::CrossesDevices
        )));
        assert!(!rename_allows_copy_fallback(&io::Error::from(
            io::ErrorKind::InvalidInput
        )));
        assert!(!rename_allows_copy_fallback(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn lesefehler_und_temp_cleanup_erhalten_fruehere_pruefpfade() {
        let mut failures = Vec::new();
        for unproved in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let source = root.join("quelle.bin");
            let destination = root.join("ziel");
            std::fs::write(&source, b"lesbarer-anfang").unwrap();
            std::fs::create_dir(&destination).unwrap();
            let prior = root.join("frueherer-datenrest");
            std::fs::write(&prior, b"erhalten").unwrap();
            let vfs = FileopsRaceVfs::new(ForeignReplacementKind::File);
            vfs.fail_read_at(&source);
            vfs.read_recovery_paths
                .lock()
                .unwrap()
                .push(prior.to_string_lossy().into_owned());
            if unproved {
                vfs.attack_unproved_temp_before_remove();
            }
            let mut actuals = FileOperationActuals::default();
            let error = copy_items_with_conflict_proofs_tracked(
                FileOperationRequest {
                    src: &vfs,
                    dst: &vfs,
                    items: std::slice::from_ref(&source),
                    dst_dir: &destination,
                    policy: OverwritePolicy::Overwrite,
                    sandbox: &None,
                },
                &[],
                &Cancel::new(),
                &mut |_| {},
                &mut actuals,
            )
            .unwrap_err();
            assert_eq!(
                error_code(&error),
                if unproved {
                    ErrorCode::Io
                } else {
                    ErrorCode::OutcomeUnclear
                }
            );
            assert_eq!(std::fs::read(&source).unwrap(), b"lesbarer-anfang");
            assert_eq!(std::fs::read(&prior).unwrap(), b"erhalten");
            assert!(!destination.join("quelle.bin").exists());
            let foreign = vfs.attacked_path();
            assert_eq!(std::fs::read(&foreign).unwrap(), b"fremd");
            let paths = crate::error::recovery_paths(&error);
            if !paths.contains(&prior.to_string_lossy().into_owned())
                || !paths.contains(&foreign.to_string_lossy().into_owned())
                || !actuals
                    .temp_recovery_paths
                    .contains(&prior.to_string_lossy().into_owned())
            {
                failures.push((unproved, paths, actuals.temp_recovery_paths));
            }
        }
        assert!(
            failures.is_empty(),
            "Verlorene frühere Prüfpfade: {failures:?}"
        );
    }
}
