//! Rein lesende Planerstellung und plan-gebundene Metadatenbilanz.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use crate::metadata::{Capability, MetadataCapabilities, MetadataProfile};
use crate::tree_measure::measure_entry;
use crate::vfs::{Entry, ProofListEntry, Vfs};

use super::exclude::ExcludeMatcher;
use super::{
    complete_listing, hash_file_hex, hash_tree, optional_stat, Action, Compare, ComparisonAction,
    ComparisonEntry, ComparisonRow, ComparisonSide, MetadataClass, MetadataClassStats,
    MetadataIssue, MetadataIssueKind, MetadataReport, MetadataTransfer, SyncMode, SyncOptions,
    SyncPlan,
};

// ---------------------------------------------------------------------------
// Schritt 1: Plan berechnen (Dry-Run, verändert nichts)
// ---------------------------------------------------------------------------

/// Berechnet den Sync-Plan durch rekursiven Vergleich beider Bäume. Verändert nichts.
pub fn plan(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    opts: &SyncOptions,
) -> io::Result<SyncPlan> {
    // Die Optionssyntax wird vor jedem Providerzugriff geprüft. CLI-Aufrufer tun das
    // zusätzlich noch früher, damit ungültige Muster nicht einmal Verbindungen öffnen.
    opts.validate()?;
    let exclusions = ExcludeMatcher::new(&opts.exclude)?;
    let metadata = MetadataPlanPolicy::for_roots(src, src_root, dst, dst_root, opts)?;
    // Die Ziel-Gesamtzahl wird unabhängig vom eigentlichen Diff ermittelt. Nur so kann
    // eine spätere Löschgrenze auch einen einzigen geplanten `DeleteDir` korrekt als
    // möglicherweise tausende betroffene Dateien bewerten. Beide Zähler speisen
    // ausschließlich die Mirror-Löschgates (Kern, Realmodus, UI-Vorwarnung) — nur
    // Mirror kann überhaupt Entfernungen planen. Für AddOnly/Update entfallen die
    // zwei zusätzlichen Volltraversierungen deshalb ersatzlos; über ein Netzwerk
    // wären sie pro Lauf hunderte unnötige Listing-Round-Trips.
    let mut plan = match opts.mode {
        SyncMode::Mirror => SyncPlan {
            source_count: measure_contents(src, src_root, Path::new(""), &exclusions)?.0,
            target_count: measure_contents(dst, dst_root, Path::new(""), &exclusions)?.0,
            ..Default::default()
        },
        SyncMode::AddOnly | SyncMode::Update => SyncPlan::default(),
    };
    let mut hardlinks = HardlinkPlanState::default();
    let request = DiffRequest {
        src,
        src_root,
        dst,
        dst_root,
        opts,
        metadata,
        exclusions: &exclusions,
        conservative_names: dst.file_commit_class_at(dst_root)?
            == crate::vfs::FileCommitClass::BestEffort,
    };
    diff_dir(
        &request,
        Path::new(""),
        TargetDirKnowledge::Unknown,
        &mut hardlinks,
        &mut plan,
    )?;
    hardlinks.remove_unshared_groups(&mut plan)?;
    // Post-order geplante Ordner-Metadaten sollen in der Darstellung trotzdem vor
    // ihren Kindern stehen. Pfadsortierung stellt die Baumreihenfolge her, ohne einen
    // weiteren Dateisystemzugriff und damit ohne einen zweiten, abweichenden Planlauf.
    plan.comparison
        .rows
        .sort_by(|left, right| left.path.cmp(&right.path));
    if dst.is_local() {
        let mut parents = std::collections::BTreeMap::new();
        for action in &plan.actions {
            let path = match action {
                super::Action::CopyFile { path, .. }
                | super::Action::UpdateFile { path, .. }
                | super::Action::CreateHardlink { path, .. } => path,
                _ => continue,
            };
            let target = crate::path::checked_join(dst_root, path)?;
            let mut parent = target.parent();
            while let Some(path) = parent.filter(|path| path.starts_with(dst_root)) {
                match dst.local_parent_inheritance(path) {
                    Ok(Some(proof)) => {
                        parents.insert(path.to_path_buf(), proof);
                        break;
                    }
                    Ok(None) => break,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => parent = path.parent(),
                    Err(error) if error.kind() == io::ErrorKind::Unsupported => break,
                    Err(error) => return Err(error),
                }
            }
        }
        plan.local_parent_inheritance = parents.into_values().collect();
    }
    plan.metadata_report = build_metadata_report(opts.metadata, &plan.actions);
    if opts.metadata == MetadataProfile::Portable && !metadata.target_mode_writable {
        for row in &plan.comparison.rows {
            if matches!(row.action, ComparisonAction::Add | ComparisonAction::Update)
                && row
                    .source
                    .as_ref()
                    .is_some_and(|source| source.mode.is_some())
            {
                plan.metadata_report.issues.push(MetadataIssue {
                    path: row.path.clone(), class: MetadataClass::NormalMode,
                    kind: MetadataIssueKind::Unsupported,
                    reason: "Das konkrete Zielvolume stellt POSIX-Rechte nicht exakt dar; der Inhalt wird unabhängig davon übertragen".into(),
                });
            }
        }
        let omitted = plan.metadata_report.issues.len() as u64;
        plan.metadata_report.unsupported_metadata = omitted;
        if omitted > 0 {
            plan.metadata_report.classes.push(MetadataClassStats {
                class: MetadataClass::NormalMode,
                transferred: 0,
                omitted,
            });
        }
    }
    if opts.metadata == MetadataProfile::NativeStrict && !plan.metadata_report.issues.is_empty() {
        let first = &plan.metadata_report.issues[0];
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "NativeStrict ist für diesen Baum nicht vollständig darstellbar: {} ({:?}: {})",
                first.path, first.class, first.reason
            ),
        ));
    }
    plan.metadata_report.strict_verified = opts.metadata == MetadataProfile::NativeStrict;
    Ok(plan)
}

#[derive(Clone, Copy, Debug, Default)]
struct MetadataPlanPolicy {
    /// Native Vorschauen bilanzieren auch einen vollständig ausgelassenen Transfer.
    collect: bool,
    /// Kann das Ziel POSIX-Rechte überhaupt schreiben?
    ///
    /// Portable nutzt die allgemeine Providerzusage; native Profile verlangen
    /// zusätzlich exakt darstellbare Modusbits. Remote-Provider melden hier `false`.
    target_mode_writable: bool,
    owner: bool,
    acl: bool,
    acl_incompatible: bool,
    normal_mode: bool,
    special_mode_bits: bool,
    xattrs: bool,
    resource_fork: bool,
    sparse_extents: bool,
    hardlinks: bool,
    /// Quellbeziehungen werden auch dann bilanziert, wenn das Ziel sie nicht erhält.
    observe_hardlinks: bool,
}

impl MetadataPlanPolicy {
    fn for_roots(
        src: &dyn Vfs,
        src_root: &Path,
        dst: &dyn Vfs,
        dst_root: &Path,
        opts: &SyncOptions,
    ) -> io::Result<Self> {
        let target_mode_writable = dst.supports_mode_write_at(dst_root)?;
        if opts.metadata == MetadataProfile::Portable {
            return Ok(Self {
                target_mode_writable,
                ..Self::default()
            });
        }
        if opts.compare != Compare::ContentHash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Native Metadatenprofile benötigen den vollständigen Inhaltsvergleich",
            ));
        }
        if opts.metadata == MetadataProfile::NativeBestEffort && opts.mode == SyncMode::Mirror {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "NativeBestEffort darf nicht für einen unvollständigen Mirror-Lauf verwendet werden",
            ));
        }

        let source = src.metadata_capabilities(src_root)?;
        let target = dst.metadata_capabilities(dst_root)?;
        if opts.metadata == MetadataProfile::NativeStrict {
            require_complete_native_capabilities(&source, &target)?;
        }

        Ok(Self {
            collect: true,
            target_mode_writable: target_mode_writable
                && readable(source.special_mode_bits)
                && target.special_mode_bits == Capability::Exact,
            owner: readable(source.owner) && target.owner == Capability::Exact,
            // ACL und normale Rechte bilden einen gekoppelten Schreibvertrag.
            // Ein Mount mit ignorierten Eigentümern kann das ACL-Modell lesend
            // kennen, ohne diese Kombination exakt wiederherstellen zu können.
            acl: readable(source.special_mode_bits)
                && target.special_mode_bits == Capability::Exact
                && source.acl_model == target.acl_model
                && matches!(
                    source.acl_model,
                    crate::metadata::AclModel::Posix | crate::metadata::AclModel::Nfs4
                ),
            acl_incompatible: source.acl_model != target.acl_model
                && !matches!(
                    source.acl_model,
                    crate::metadata::AclModel::Unsupported | crate::metadata::AclModel::Unknown
                ),
            normal_mode: readable(source.special_mode_bits)
                && target.special_mode_bits == Capability::Exact,
            special_mode_bits: readable(source.special_mode_bits)
                && target.special_mode_bits == Capability::Exact,
            xattrs: readable(source.xattrs) && target.xattrs == Capability::Exact,
            resource_fork: readable(source.resource_fork)
                && target.resource_fork == Capability::Exact,
            sparse_extents: readable(source.sparse_extents)
                && target.sparse_extents == Capability::Exact,
            hardlinks: readable(source.hardlinks) && target.hardlinks == Capability::Exact,
            observe_hardlinks: readable(source.hardlinks),
        })
    }

    /// Ressourcen-Forks sind in diesem Schnitt nur für reguläre Dateien ein
    /// eigener Inhaltsstrom. Alle anderen aktivierten Merkmale gelten auch für Ordner.
    fn for_directory(self) -> Self {
        Self {
            resource_fork: false,
            sparse_extents: false,
            hardlinks: false,
            observe_hardlinks: false,
            acl_incompatible: self.acl_incompatible,
            ..self
        }
    }
}

#[derive(Default)]
struct HardlinkPlanState {
    groups: std::collections::HashMap<(u64, u64), HardlinkGroup>,
    next_group: u64,
}

struct HardlinkGroup {
    id: u64,
    selected_members: usize,
    /// Erst gesetzt, sobald dieser Zielname nach dem Plan sicher den Quellinhalt trägt.
    /// Das Flag zeigt, ob der Anker in diesem Lauf einen neuen Inode erhält.
    target_anchor: Option<(String, bool)>,
}

impl HardlinkPlanState {
    fn group_for(&mut self, entry: &Entry, enabled: bool) -> Option<(u64, Option<(String, bool)>)> {
        if !enabled || entry.hard_links.unwrap_or(0) < 2 {
            return None;
        }
        let key = (entry.device?, entry.inode?);
        let group = self.groups.entry(key).or_insert_with(|| {
            self.next_group += 1;
            HardlinkGroup {
                id: self.next_group,
                selected_members: 0,
                target_anchor: None,
            }
        });
        group.selected_members += 1;
        Some((group.id, group.target_anchor.clone()))
    }

    fn remove_unshared_groups(&self, plan: &mut SyncPlan) -> io::Result<()> {
        // nlink zählt auch Namen außerhalb des gewählten Baums und ausgeschlossene
        // Pfade. Erst zwei tatsächlich gescannte Namen bilden eine zu erhaltende
        // Beziehung; nach einem externen Partner wird niemals gesucht.
        let unshared: std::collections::BTreeSet<_> = self
            .groups
            .values()
            .filter(|group| group.selected_members < 2)
            .map(|group| group.id)
            .collect();
        for action in &mut plan.actions {
            let metadata = match action {
                Action::CopyFile {
                    metadata: Some(metadata),
                    ..
                }
                | Action::UpdateFile {
                    metadata: Some(metadata),
                    ..
                } => metadata,
                _ => continue,
            };
            if metadata
                .source
                .hardlink_group
                .is_some_and(|id| unshared.contains(&id))
            {
                metadata.source = metadata.source.clone().with_hardlink_group(None)?;
                metadata.hardlinks = false;
            }
        }
        Ok(())
    }

    fn set_anchor(&mut self, entry: &Entry, path: String, will_replace: bool) {
        if let (Some(device), Some(inode)) = (entry.device, entry.inode) {
            if let Some(group) = self.groups.get_mut(&(device, inode)) {
                group.target_anchor = Some((path, will_replace));
            }
        }
    }
}

fn readable(capability: Capability) -> bool {
    matches!(capability, Capability::Exact | Capability::ReadOnly)
}

/// Das strikte Profil bleibt gesperrt, bis wirklich alle Stufe-2-Phasen einen exakten
/// Providervertrag besitzen. Ein Teilschnitt darf niemals still auf BestEffort fallen.
fn require_complete_native_capabilities(
    source: &MetadataCapabilities,
    target: &MetadataCapabilities,
) -> io::Result<()> {
    // `Unsupported` oder `ReadOnly` kann für einen konkreten Baum ausreichend sein,
    // wenn dort kein entsprechendes Merkmal vorkommt. Das entscheidet erst der
    // vollständige Scan anhand der pfadgenauen Bilanz. `Unknown` ist dagegen niemals
    // eine sichere Grundlage für NativeStrict.
    let unknown = |value| value == Capability::Unknown;
    if unknown(source.owner)
        || unknown(target.owner)
        || unknown(source.special_mode_bits)
        || unknown(target.special_mode_bits)
        || unknown(source.xattrs)
        || unknown(target.xattrs)
        || unknown(source.resource_fork)
        || unknown(target.resource_fork)
        || unknown(source.sparse_extents)
        || unknown(target.sparse_extents)
        || unknown(source.hardlinks)
        || unknown(target.hardlinks)
        || matches!(source.acl_model, crate::metadata::AclModel::Unknown)
        || matches!(target.acl_model, crate::metadata::AclModel::Unknown)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NativeStrict benötigt bekannte Fähigkeiten für alle Metadatenklassen",
        ));
    }
    Ok(())
}

/// Baut die sichtbare Bilanz ausschließlich aus den bereits plan-gebundenen
/// Manifesten. Dadurch kann die Oberfläche nichts versprechen, was `execute()` nicht
/// anhand genau desselben Plans verifiziert.
pub(super) fn build_metadata_report(
    profile: MetadataProfile,
    actions: &[Action],
) -> MetadataReport {
    let mut counts: BTreeMap<MetadataClass, (u64, u64)> = BTreeMap::new();
    let mut issues = Vec::new();

    for action in actions {
        let (path, transfer) = match action {
            Action::CreateDir {
                path,
                metadata: Some(value),
                ..
            }
            | Action::CopyFile {
                path,
                metadata: Some(value),
                ..
            }
            | Action::UpdateFile {
                path,
                metadata: Some(value),
                ..
            }
            | Action::SetDirectoryMetadata {
                path,
                metadata: value,
                ..
            }
            | Action::CreateHardlink {
                path,
                metadata: value,
                ..
            } => (path, value),
            _ => continue,
        };

        let present = [
            (
                MetadataClass::Owner,
                transfer.source.owner.is_some(),
                transfer.owner,
                MetadataIssueKind::Unsupported,
            ),
            (
                MetadataClass::Acl,
                transfer.source.acl.is_some(),
                transfer.acl,
                if transfer.incompatible_acl_model {
                    MetadataIssueKind::IncompatibleAclModel
                } else {
                    MetadataIssueKind::Unsupported
                },
            ),
            (
                MetadataClass::NormalMode,
                true,
                transfer.normal_mode,
                MetadataIssueKind::Unsupported,
            ),
            (
                MetadataClass::SpecialModeBits,
                transfer.source.mode & 0o7000 != 0,
                transfer.special_mode_bits,
                MetadataIssueKind::Unsupported,
            ),
            (
                MetadataClass::Xattrs,
                !transfer.source.xattrs.is_empty(),
                transfer.xattrs,
                MetadataIssueKind::Unsupported,
            ),
            (
                MetadataClass::ResourceFork,
                transfer.source.resource_fork.is_some(),
                transfer.resource_fork,
                MetadataIssueKind::Unsupported,
            ),
            (
                MetadataClass::SparseExtents,
                transfer.source.sparse_extents.is_some(),
                transfer.sparse_extents,
                MetadataIssueKind::Unsupported,
            ),
            (
                MetadataClass::Hardlinks,
                transfer.source.hardlink_group.is_some(),
                transfer.hardlinks,
                MetadataIssueKind::Unsupported,
            ),
        ];
        for (class, exists, enabled, kind) in present {
            if !exists {
                continue;
            }
            let entry = counts.entry(class).or_default();
            if enabled {
                entry.0 = entry.0.saturating_add(1);
            } else {
                entry.1 = entry.1.saturating_add(1);
                issues.push(MetadataIssue {
                    path: path.clone(),
                    class,
                    kind,
                    reason: if kind == MetadataIssueKind::IncompatibleAclModel {
                        "Quell- und Ziel-ACL verwenden inkompatible Modelle".into()
                    } else {
                        "Quelle ist lesbar, Ziel kann dieses Merkmal nicht exakt darstellen".into()
                    },
                });
            }
        }
    }

    let content_changes = actions
        .iter()
        .filter(|action| {
            matches!(
                action,
                Action::CopyFile { .. }
                    | Action::UpdateFile {
                        metadata_only: false,
                        ..
                    }
            )
        })
        .count() as u64;
    let metadata_changes = actions
        .iter()
        .filter(|action| match action {
            Action::CopyFile {
                metadata: Some(value),
                ..
            }
            | Action::UpdateFile {
                metadata: Some(value),
                ..
            }
            | Action::CreateDir {
                metadata: Some(value),
                ..
            }
            | Action::SetDirectoryMetadata {
                metadata: value, ..
            } => {
                value.owner
                    || value.acl
                    || value.normal_mode
                    || value.special_mode_bits
                    || value.xattrs
                    || value.resource_fork
                    || value.sparse_extents
                    || value.hardlinks
            }
            _ => false,
        })
        .count() as u64;
    let hardlink_actions = actions
        .iter()
        .filter(|action| matches!(action, Action::CreateHardlink { .. }))
        .count() as u64;
    let unsupported_metadata = issues
        .iter()
        .filter(|issue| issue.kind == MetadataIssueKind::Unsupported)
        .count() as u64;
    let privilege_failures = issues
        .iter()
        .filter(|issue| issue.kind == MetadataIssueKind::PrivilegeFailure)
        .count() as u64;
    let incompatible_acl_models = issues
        .iter()
        .filter(|issue| issue.kind == MetadataIssueKind::IncompatibleAclModel)
        .count() as u64;

    MetadataReport {
        profile,
        content_changes,
        metadata_changes,
        hardlink_actions,
        unsupported_metadata,
        privilege_failures,
        incompatible_acl_models,
        classes: counts
            .into_iter()
            .map(|(class, (transferred, omitted))| MetadataClassStats {
                class,
                transferred,
                omitted,
            })
            .collect(),
        issues,
        strict_verified: false,
    }
}

/// Was der Aufrufer über das Ziel-Verzeichnis dieser Rekursionsebene bereits weiß.
/// Das Eltern-Listing hat jeden Kind-Eintrag schon gesehen — ohne dieses Wissen
/// müsste jede Ebene das Ziel erneut `stat`en (über ein Netzwerk ein voller
/// Round-Trip pro Verzeichnis, lokal ein unnötiger Syscall).
#[derive(Clone, Copy)]
enum TargetDirKnowledge<'a> {
    /// Wurzelaufruf: noch nichts bekannt, per `stat` klären.
    Unknown,
    /// Das Eltern-Listing hat KEINEN gleichnamigen Ziel-Eintrag gesehen.
    Absent,
    /// Der gleichnamige Ziel-Eintrag aus dem Eltern-Listing.
    Present(&'a Entry),
}

/// Vergleicht ein einzelnes (relatives) Verzeichnis in Quelle und Ziel und hängt die
/// nötigen Aktionen an `plan` an; rekursiert in Unterverzeichnisse.
///
/// Nicht sicher dekodierbare (Nicht-UTF-8-)Namen werden konservativ ausgelassen:
/// Zwei VERSCHIEDENE rohe Namen können lossy auf denselben String zusammenfallen
/// und würden im Plan verschmelzen; außerdem träfe der String-Pfad im Dateisystem
/// nicht den echten Eintrag. `list_complete` liefert dafür pro Eintrag das
/// verlässliche `safely_decoded`-Flag samt rohem Namen — ein Name, der LEGITIM
/// ein U+FFFD trägt, bleibt damit ganz normal synchronisierbar.
#[derive(Clone, Copy)]
struct DiffRequest<'a> {
    src: &'a dyn Vfs,
    src_root: &'a Path,
    dst: &'a dyn Vfs,
    dst_root: &'a Path,
    opts: &'a SyncOptions,
    metadata: MetadataPlanPolicy,
    exclusions: &'a ExcludeMatcher,
    conservative_names: bool,
}

/// Hängt ausführbare Aktion und sichtbare Vergleichszeile atomar an denselben Plan.
/// Dadurch kann kein späterer Darstellungsdurchlauf andere Metadaten sehen als der
/// Planer, dessen Ergebnis bestätigt und gehasht wird.
fn push_planned_action(
    plan: &mut SyncPlan,
    action: Action,
    source: Option<&Entry>,
    target: Option<&Entry>,
) {
    let path = match &action {
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
        | Action::SkipUnsupported { path } => path.clone(),
    };
    let comparison_action = match &action {
        Action::CreateDir { .. } | Action::CopyFile { .. } => ComparisonAction::Add,
        Action::CreateHardlink { .. } if target.is_none() => ComparisonAction::Add,
        Action::CreateHardlink { .. }
        | Action::UpdateFile { .. }
        | Action::SetMode { .. }
        | Action::SetDirectoryMetadata { .. } => ComparisonAction::Update,
        Action::DeleteFile { .. } | Action::DeleteDir { .. } => ComparisonAction::Quarantine,
        Action::SkipSymlink { .. }
        | Action::SkipConflict { .. }
        | Action::SkipUnsupported { .. } => ComparisonAction::Skip,
    };
    let affected_side = if comparison_action == ComparisonAction::Skip {
        ComparisonSide::Neither
    } else {
        ComparisonSide::Target
    };

    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let mut ancestors = Vec::with_capacity(parts.len().saturating_sub(1));
    let mut ancestor = String::new();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        if !ancestor.is_empty() {
            ancestor.push('/');
        }
        ancestor.push_str(part);
        ancestors.push(ancestor.clone());
    }
    let depth = u32::try_from(ancestors.len()).unwrap_or(u32::MAX);

    match comparison_action {
        ComparisonAction::Add => {
            plan.comparison.categories.additions =
                plan.comparison.categories.additions.saturating_add(1)
        }
        ComparisonAction::Update => {
            plan.comparison.categories.updates =
                plan.comparison.categories.updates.saturating_add(1)
        }
        ComparisonAction::Quarantine => {
            plan.comparison.categories.quarantines =
                plan.comparison.categories.quarantines.saturating_add(1)
        }
        ComparisonAction::Skip => {
            plan.comparison.categories.skipped =
                plan.comparison.categories.skipped.saturating_add(1)
        }
    }
    plan.comparison.rows.push(ComparisonRow {
        path,
        depth,
        ancestors,
        action: comparison_action,
        affected_side,
        source: source.map(ComparisonEntry::from),
        target: target.map(ComparisonEntry::from),
    });
    plan.actions.push(action);
}

fn diff_dir(
    request: &DiffRequest<'_>,
    rel: &Path,
    target_knowledge: TargetDirKnowledge<'_>,
    hardlinks: &mut HardlinkPlanState,
    plan: &mut SyncPlan,
) -> io::Result<()> {
    let DiffRequest {
        src,
        src_root,
        dst,
        dst_root,
        opts,
        metadata,
        exclusions,
        conservative_names,
    } = *request;
    let mut src_entries = complete_listing(src, &src_root.join(rel))?;
    src_entries.retain(|item| !is_excluded(exclusions, rel, item));
    src_entries.sort_by(|a, b| a.entry.name.cmp(&b.entry.name));

    // Ziel-Verzeichnis auflisten — aber NUR, wenn es dort tatsächlich ein Verzeichnis
    // ist. Fehlt es (neu) oder ist es (noch) eine Datei am Ordner-Pfad (Typkonflikt,
    // wird später ersetzt), behandeln wir es als leer, statt `list()` auf einer Datei
    // scheitern zu lassen. Der `stat` fällt nur an der Wurzel an; darunter reicht
    // das Wissen aus dem Eltern-Listing.
    let dst_dir = dst_root.join(rel);
    let list_target = |entry: &Entry| entry.is_dir && !entry.is_symlink;
    let mut dst_entries = match target_knowledge {
        TargetDirKnowledge::Absent => Vec::new(),
        TargetDirKnowledge::Present(entry) if list_target(entry) => {
            complete_listing(dst, &dst_dir)?
        }
        TargetDirKnowledge::Present(_) => Vec::new(),
        TargetDirKnowledge::Unknown => match optional_stat(dst, &dst_dir)? {
            Some(entry) if list_target(&entry) => complete_listing(dst, &dst_dir)?,
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Sync-Zielwurzel ist kein echter Ordner: {}",
                        dst_dir.display()
                    ),
                ))
            }
            None => Vec::new(),
        },
    };
    dst_entries.retain(|item| !is_excluded(exclusions, rel, item));
    dst_entries.sort_by(|a, b| a.entry.name.cmp(&b.entry.name));
    // Ziel-Einträge nach Namen greifbar machen — nur sicher dekodierte: ein lossy
    // Ziel-Name darf nie mit einem (gültigen) Quellnamen gleichen Strings matchen,
    // sonst würde der Plan den falschen Dateisystem-Eintrag adressieren.
    let dst_by_name: std::collections::HashMap<&str, &ProofListEntry> = dst_entries
        .iter()
        .filter(|e| e.safely_decoded)
        .map(|e| (e.entry.name.as_str(), e))
        .collect();
    let folded_targets: std::collections::HashSet<_> = dst_entries
        .iter()
        .filter(|item| item.safely_decoded)
        .map(|item| crate::remote_rename::folded_name(&item.entry.name))
        .collect();
    let mut folded_sources = std::collections::HashMap::<String, usize>::new();
    if conservative_names {
        for item in src_entries.iter().filter(|item| item.safely_decoded) {
            *folded_sources
                .entry(crate::remote_rename::folded_name(&item.entry.name))
                .or_default() += 1;
        }
    }

    // Merken, welche Ziel-Namen die Quelle „abdeckt" — der Rest ist im Mirror-Modus
    // überzählig und wird gelöscht.
    let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for src_item in &src_entries {
        let se = &src_item.entry;
        let child_rel = rel.join(&se.name);
        let child_str = rel_to_string(&child_rel);

        // Auch ausgelassene Einträge decken ihren Namen ab. „Überspringen" muss heißen,
        // dass das gleichnamige Zielobjekt unangetastet bleibt — nicht, dass Mirror es
        // anschließend als überzählig löscht.
        covered.insert(se.name.as_str());

        // Nur sicher dekodierte Namen dürfen einander zugeordnet werden. Für diese
        // Einträge steht das Ziel aber schon vor den Auslassungsprüfungen bereit,
        // damit auch eine Symlink-/Spezialdateizeile beide Seiten korrekt zeigt.
        let de_item = src_item
            .safely_decoded
            .then(|| dst_by_name.get(se.name.as_str()).copied())
            .flatten();
        let de = de_item.map(|item| &item.entry);

        // Ein normalisiert zurückgelesener Name ist weder ein freies Ziel noch
        // eine bestätigte Ersetzung. Den Konflikt sichtbar auslassen, damit
        // unabhängige Additionen nicht erst nach einem Teilcommit daran scheitern.
        let folded = crate::remote_rename::folded_name(&se.name);
        // Exakte lokale Listings beweisen keine Case-Sensitivität. Ein echter
        // Stat unterscheidet den Alias auf APFS/exFAT vom freien ext4-Namen.
        let folded_target_collision = src_item.safely_decoded
            && de_item.is_none()
            && folded_targets.contains(&folded)
            && (conservative_names
                || !dst.has_exact_names()
                || optional_stat(dst, &dst_root.join(&child_rel))?.is_some());
        if src_item.safely_decoded
            && (folded_target_collision
                || folded_sources.get(&folded).is_some_and(|count| *count > 1))
        {
            push_planned_action(
                plan,
                Action::SkipConflict { path: child_str },
                Some(se),
                None,
            );
            plan.skip_count = plan.skip_count.saturating_add(1);
            continue;
        }

        // Nicht sicher dekodierbare Quellnamen konservativ auslassen, BEVOR sie
        // gegen die Ziel-Map gematcht werden (siehe Funktionskommentar).
        if !src_item.safely_decoded {
            push_planned_action(
                plan,
                Action::SkipUnsupported { path: child_str },
                Some(se),
                None,
            );
            plan.skip_count = plan.skip_count.saturating_add(1);
            continue;
        }

        // Symlinks überspringen (v0.1).
        if se.is_symlink {
            push_planned_action(plan, Action::SkipSymlink { path: child_str }, Some(se), de);
            plan.skip_count = plan.skip_count.saturating_add(1);
            continue;
        }
        if !se.is_dir && !se.is_file {
            push_planned_action(
                plan,
                Action::SkipUnsupported { path: child_str },
                Some(se),
                de,
            );
            plan.skip_count = plan.skip_count.saturating_add(1);
            continue;
        }

        // Ziel-Symlinks und andere Spezialdateien werden nie automatisch geöffnet,
        // verfolgt oder ersetzt. Das verhindert insbesondere Schreibzugriffe außerhalb
        // einer Sandbox über einen Symlink im Zielbaum.
        if let Some(d) = de {
            if d.is_symlink {
                push_planned_action(
                    plan,
                    Action::SkipSymlink { path: child_str },
                    Some(se),
                    Some(d),
                );
                plan.skip_count = plan.skip_count.saturating_add(1);
                continue;
            }
            if !d.is_dir && !d.is_file {
                push_planned_action(
                    plan,
                    Action::SkipUnsupported { path: child_str },
                    Some(se),
                    Some(d),
                );
                plan.skip_count = plan.skip_count.saturating_add(1);
                continue;
            }
        }

        if se.is_dir {
            match de {
                // Ziel hat gleichnamiges Verzeichnis → hineinabsteigen.
                Some(d) if d.is_dir => {
                    let action_start = plan.actions.len();
                    diff_dir(
                        request,
                        &child_rel,
                        TargetDirKnowledge::Present(d),
                        hardlinks,
                        plan,
                    )?;
                    if opts.mode != SyncMode::AddOnly {
                        if metadata.collect {
                            let transfer = plan_metadata_transfer(
                                src,
                                &src_root.join(&child_rel),
                                dst,
                                Some(&dst_root.join(&child_rel)),
                                metadata.for_directory(),
                                None,
                            )?
                            .ok_or_else(|| {
                                io::Error::other(
                                    "Aktivierte Metadatenrichtlinie lieferte kein Manifest",
                                )
                            })?;
                            // Kind-Renames ändern die Ordner-mtime auch dann, wenn
                            // sie vor dem Lauf bereits gleich war. Den Abschluss
                            // deshalb samt altem Zielmanifest vorab bestätigen.
                            let changes_children =
                                plan.actions[action_start..]
                                    .iter()
                                    .any(|action| match action {
                                        Action::CreateDir { path, .. }
                                        | Action::CopyFile { path, .. }
                                        | Action::UpdateFile { path, .. }
                                        | Action::CreateHardlink { path, .. }
                                        | Action::DeleteFile { path, .. }
                                        | Action::DeleteDir { path, .. } => {
                                            Path::new(path).parent() == Some(child_rel.as_path())
                                        }
                                        _ => false,
                                    });
                            if metadata_transfer_differs(&transfer)
                                || directory_mtime_differs(se, d, opts)
                                || changes_children
                            {
                                push_planned_action(
                                    plan,
                                    Action::SetDirectoryMetadata {
                                        path: child_str,
                                        source_mtime: se.mtime,
                                        target_mtime: d.mtime,
                                        metadata: transfer,
                                    },
                                    Some(se),
                                    Some(d),
                                );
                                plan.metadata_count = plan.metadata_count.saturating_add(1);
                            }
                        } else if let Some(source_mode) =
                            changed_mode(se, d, metadata.target_mode_writable)
                        {
                            push_planned_action(
                                plan,
                                Action::SetMode {
                                    path: child_str,
                                    source_mode,
                                    target_mode: d.mode,
                                    is_dir: true,
                                    content_hash: None,
                                },
                                Some(se),
                                Some(d),
                            );
                            plan.metadata_count = plan.metadata_count.saturating_add(1);
                        }
                    }
                }
                // Ziel hat gleichnamige DATEI → nicht automatisch destruktiv auflösen.
                Some(file) => {
                    push_planned_action(
                        plan,
                        Action::SkipConflict { path: child_str },
                        Some(se),
                        Some(file),
                    );
                    plan.skip_count = plan.skip_count.saturating_add(1);
                }
                // Im Ziel nicht vorhanden → Verzeichnis anlegen und rekursiv füllen.
                None => {
                    let metadata_transfer = plan_metadata_transfer(
                        src,
                        &src_root.join(&child_rel),
                        dst,
                        None,
                        metadata.for_directory(),
                        None,
                    )?;
                    push_planned_action(
                        plan,
                        Action::CreateDir {
                            path: child_str,
                            source_mode: se.mode.filter(|_| metadata.target_mode_writable),
                            source_mtime: se.mtime,
                            metadata: metadata_transfer,
                        },
                        Some(se),
                        None,
                    );
                    diff_dir(
                        request,
                        &child_rel,
                        TargetDirKnowledge::Absent,
                        hardlinks,
                        plan,
                    )?;
                }
            }
        } else {
            // Quelle ist eine Datei.
            let hardlink_group = hardlinks.group_for(se, metadata.observe_hardlinks);
            if let Some((group_id, Some((link_to, anchor_will_replace)))) =
                hardlink_group.as_ref().filter(|_| metadata.hardlinks)
            {
                match de {
                    Some(d) if d.is_dir => {
                        push_planned_action(
                            plan,
                            Action::SkipConflict { path: child_str },
                            Some(se),
                            Some(d),
                        );
                        plan.skip_count = plan.skip_count.saturating_add(1);
                        continue;
                    }
                    Some(_) if opts.mode == SyncMode::AddOnly => continue,
                    _ => {}
                }
                if !anchor_will_replace {
                    if let Some(target) = de {
                        let anchor = dst.stat(&dst_root.join(link_to))?;
                        if target.device.is_some()
                            && (target.device, target.inode) == (anchor.device, anchor.inode)
                        {
                            continue;
                        }
                    }
                }
                let source_path = src_root.join(&child_rel);
                let target_path = dst_root.join(&child_rel);
                let source_hash = plan_file_hash(src, &source_path, se, opts, plan)?
                    .ok_or_else(|| io::Error::other("Hardlink-Plan benötigt ContentHash"))?;
                let target_hash = de
                    .map(|entry| plan_file_hash(dst, &target_path, entry, opts, plan))
                    .transpose()?
                    .flatten();
                if let Some(target) = de {
                    let equal = target_hash.as_deref() == Some(source_hash.as_str());
                    if !equal && !should_update(se, target, opts, false) {
                        continue;
                    }
                }
                let metadata_transfer = plan_metadata_transfer(
                    src,
                    &source_path,
                    dst,
                    de.map(|_| target_path.as_path()),
                    metadata,
                    Some(*group_id),
                )?
                .ok_or_else(|| io::Error::other("Hardlink-Plan benötigt Metadatenmanifest"))?;
                push_planned_action(
                    plan,
                    Action::CreateHardlink {
                        path: child_str,
                        link_to: link_to.clone(),
                        size: se.size,
                        source_mtime: se.mtime,
                        source_mode: se.mode,
                        source_hash,
                        target_size: de.map(|entry| entry.size),
                        target_mtime: de.and_then(|entry| entry.mtime),
                        target_mode: de.and_then(|entry| entry.mode),
                        target_proof: de_item
                            .map(super::SyncTargetProof::from_listed)
                            .transpose()?,
                        target_hash,
                        metadata: metadata_transfer,
                    },
                    Some(se),
                    de,
                );
                if de.is_some() {
                    plan.update_count = plan.update_count.saturating_add(1);
                } else {
                    plan.copy_count = plan.copy_count.saturating_add(1);
                }
                continue;
            }
            let group_id = hardlink_group.map(|(id, _)| id);
            let mut target_will_match = false;
            let mut target_will_replace = false;
            match de {
                // Ziel hat gleichnamige Datei → nur bei Unterschied aktualisieren.
                Some(d) if !d.is_dir => {
                    if opts.mode == SyncMode::AddOnly {
                        continue;
                    }
                    let source_path = src_root.join(&child_rel);
                    let target_path = dst_root.join(&child_rel);
                    let comparison = compare_existing_files(
                        FileComparisonRequest {
                            src,
                            src_path: &source_path,
                            src_entry: se,
                            dst,
                            dst_path: &target_path,
                            dst_entry: d,
                            opts,
                        },
                        plan,
                    )?;
                    let metadata_transfer = plan_metadata_transfer(
                        src,
                        &source_path,
                        dst,
                        Some(&target_path),
                        metadata,
                        group_id,
                    )?;
                    let metadata_changed = metadata_transfer
                        .as_ref()
                        .is_some_and(metadata_transfer_differs);
                    let metadata_only = comparison.equal && metadata_changed;
                    let fork_bytes = planned_fork_bytes(metadata_transfer.as_ref());
                    if should_update(se, d, opts, comparison.equal && !metadata_changed) {
                        push_planned_action(
                            plan,
                            Action::UpdateFile {
                                path: child_str.clone(),
                                size: se.size,
                                source_mtime: se.mtime,
                                source_mode: se.mode,
                                target_size: d.size,
                                target_mtime: d.mtime,
                                target_mode: d.mode,
                                target_proof: super::SyncTargetProof::from_listed(
                                    de_item.expect("Zieldatei stammt aus demselben Listing"),
                                )?,
                                source_hash: comparison.source_hash,
                                target_hash: comparison.target_hash,
                                metadata_only,
                                metadata: metadata_transfer,
                            },
                            Some(se),
                            Some(d),
                        );
                        plan.update_count = plan.update_count.saturating_add(1);
                        if metadata_only {
                            plan.metadata_count = plan.metadata_count.saturating_add(1);
                        }
                        plan.bytes_to_transfer = plan
                            .bytes_to_transfer
                            .saturating_add(se.size.saturating_add(fork_bytes));
                        target_will_match = true;
                        target_will_replace = true;
                    } else if comparison.equal && !metadata_changed {
                        target_will_match = true;
                        if let Some(source_mode) =
                            changed_mode(se, d, metadata.target_mode_writable)
                        {
                            push_planned_action(
                                plan,
                                Action::SetMode {
                                    path: child_str.clone(),
                                    source_mode,
                                    target_mode: d.mode,
                                    is_dir: false,
                                    content_hash: comparison.source_hash,
                                },
                                Some(se),
                                Some(d),
                            );
                            plan.metadata_count = plan.metadata_count.saturating_add(1);
                        }
                    }
                }
                // Ziel hat gleichnamiges VERZEICHNIS → nicht automatisch rekursiv löschen.
                Some(dir) => {
                    push_planned_action(
                        plan,
                        Action::SkipConflict {
                            path: child_str.clone(),
                        },
                        Some(se),
                        Some(dir),
                    );
                    plan.skip_count = plan.skip_count.saturating_add(1);
                }
                // Im Ziel nicht vorhanden → neu kopieren.
                None => {
                    let source_hash =
                        plan_file_hash(src, &src_root.join(&child_rel), se, opts, plan)?;
                    let metadata_transfer = plan_metadata_transfer(
                        src,
                        &src_root.join(&child_rel),
                        dst,
                        None,
                        metadata,
                        group_id,
                    )?;
                    let fork_bytes = planned_fork_bytes(metadata_transfer.as_ref());
                    push_planned_action(
                        plan,
                        Action::CopyFile {
                            path: child_str.clone(),
                            size: se.size,
                            source_mtime: se.mtime,
                            source_mode: se.mode,
                            source_hash,
                            metadata: metadata_transfer,
                        },
                        Some(se),
                        None,
                    );
                    plan.copy_count = plan.copy_count.saturating_add(1);
                    plan.bytes_to_transfer = plan
                        .bytes_to_transfer
                        .saturating_add(se.size.saturating_add(fork_bytes));
                    target_will_match = true;
                    target_will_replace = true;
                }
            }
            if target_will_match && group_id.is_some() {
                hardlinks.set_anchor(se, child_str, target_will_replace);
            }
        }
    }

    // Überzählige Einträge im Ziel — nur im Mirror-Modus löschen.
    if opts.mode == SyncMode::Mirror {
        for de_item in &dst_entries {
            let de = &de_item.entry;
            if covered.contains(de.name.as_str()) {
                continue;
            }
            let child_rel = rel.join(&de.name);
            let child_str = rel_to_string(&child_rel);

            // Ein ganzes Zielverzeichnis darf nur dann als eine DeleteDir-Aktion in
            // die Quarantäne wandern, wenn darin kein bewusst ausgeschlossener
            // Nachfahre liegt. Andernfalls bleibt der Ordner als Hülle stehen und nur
            // seine sichtbaren Kinder werden granular geplant.
            if !exclusions.is_empty() {
                let node =
                    scan_target_only_node(dst, dst_root, child_rel, de_item.clone(), exclusions)?;
                plan_target_only_node(dst, dst_root, opts, &node, plan)?;
                continue;
            }

            // Nicht sicher dekodierbare Ziel-Namen nie zum Löschen einplanen: der
            // String-Pfad träfe im Dateisystem nicht den echten Eintrag.
            if !de_item.safely_decoded {
                push_planned_action(
                    plan,
                    Action::SkipUnsupported { path: child_str },
                    None,
                    Some(de),
                );
                plan.skip_count = plan.skip_count.saturating_add(1);
            } else if de.is_symlink {
                push_planned_action(
                    plan,
                    Action::SkipSymlink { path: child_str },
                    None,
                    Some(de),
                );
                plan.skip_count = plan.skip_count.saturating_add(1);
            } else if !de.is_dir && !de.is_file {
                push_planned_action(
                    plan,
                    Action::SkipUnsupported { path: child_str },
                    None,
                    Some(de),
                );
                plan.skip_count = plan.skip_count.saturating_add(1);
            } else if de.is_dir {
                let (entries, bytes) = measure_entry(dst, &dst_root.join(rel).join(&de.name), de)?;
                let target_tree_hash = if opts.compare == Compare::ContentHash {
                    let (hash, files, hashed_bytes) =
                        hash_tree(dst, &dst_root.join(rel).join(&de.name))?;
                    plan.hashed_files = plan.hashed_files.saturating_add(files);
                    plan.bytes_hashed = plan.bytes_hashed.saturating_add(hashed_bytes);
                    Some(hash)
                } else {
                    None
                };
                push_planned_action(
                    plan,
                    Action::DeleteDir {
                        path: child_str,
                        entries,
                        bytes,
                        target_proof: super::SyncTargetProof::from_listed(de_item)?,
                        target_tree_hash,
                    },
                    None,
                    Some(de),
                );
                plan.delete_count = plan.delete_count.saturating_add(entries);
                plan.delete_bytes = plan.delete_bytes.saturating_add(bytes);
            } else {
                let target_hash =
                    plan_file_hash(dst, &dst_root.join(rel).join(&de.name), de, opts, plan)?;
                push_planned_action(
                    plan,
                    Action::DeleteFile {
                        path: child_str,
                        size: de.size,
                        target_mtime: de.mtime,
                        target_mode: de.mode,
                        target_proof: super::SyncTargetProof::from_listed(de_item)?,
                        target_hash,
                    },
                    None,
                    Some(de),
                );
                plan.delete_count = plan.delete_count.saturating_add(1);
                plan.delete_bytes = plan.delete_bytes.saturating_add(de.size);
            }
        }
    }

    Ok(())
}

/// Ein einmal vollständig gelesener Ziel-Teilbaum. Die Struktur verhindert zwei
/// Fehler zugleich: ausgeschlossene Nachfahren werden nie durch ein DeleteDir ihres
/// Elternordners mitentfernt, und alle übrigen unsicheren Einträge bleiben sichtbare
/// Skips, die Mirror weiterhin sperren.
struct TargetOnlyNode {
    relative: std::path::PathBuf,
    item: ProofListEntry,
    children: Vec<TargetOnlyNode>,
    contains_excluded: bool,
    entries: u64,
    bytes: u64,
}

fn scan_target_only_node(
    dst: &dyn Vfs,
    dst_root: &Path,
    relative: std::path::PathBuf,
    item: ProofListEntry,
    exclusions: &ExcludeMatcher,
) -> io::Result<TargetOnlyNode> {
    let traverse = item.safely_decoded && item.entry.is_dir && !item.entry.is_symlink;
    let bytes = if item.entry.is_file {
        item.entry.size
    } else {
        0
    };
    let mut node = TargetOnlyNode {
        relative,
        children: Vec::new(),
        contains_excluded: false,
        entries: 1,
        bytes,
        item,
    };

    if traverse {
        let listing = complete_listing(dst, &dst_root.join(&node.relative))?;
        for child in listing {
            if is_excluded(exclusions, &node.relative, &child) {
                node.contains_excluded = true;
                continue;
            }
            let child_relative = node.relative.join(&child.entry.name);
            let child = scan_target_only_node(dst, dst_root, child_relative, child, exclusions)?;
            node.contains_excluded |= child.contains_excluded;
            node.entries = node.entries.saturating_add(child.entries);
            node.bytes = node.bytes.saturating_add(child.bytes);
            node.children.push(child);
        }
    }

    Ok(node)
}

fn plan_target_only_node(
    dst: &dyn Vfs,
    dst_root: &Path,
    opts: &SyncOptions,
    node: &TargetOnlyNode,
    plan: &mut SyncPlan,
) -> io::Result<()> {
    let entry = &node.item.entry;
    let path = rel_to_string(&node.relative);

    if !node.item.safely_decoded {
        push_planned_action(plan, Action::SkipUnsupported { path }, None, Some(entry));
        plan.skip_count = plan.skip_count.saturating_add(1);
    } else if entry.is_symlink {
        push_planned_action(plan, Action::SkipSymlink { path }, None, Some(entry));
        plan.skip_count = plan.skip_count.saturating_add(1);
    } else if !entry.is_dir && !entry.is_file {
        push_planned_action(plan, Action::SkipUnsupported { path }, None, Some(entry));
        plan.skip_count = plan.skip_count.saturating_add(1);
    } else if entry.is_dir {
        if node.contains_excluded {
            for child in &node.children {
                plan_target_only_node(dst, dst_root, opts, child, plan)?;
            }
        } else {
            let target_tree_hash = if opts.compare == Compare::ContentHash {
                let (hash, files, hashed_bytes) = hash_tree(dst, &dst_root.join(&node.relative))?;
                plan.hashed_files = plan.hashed_files.saturating_add(files);
                plan.bytes_hashed = plan.bytes_hashed.saturating_add(hashed_bytes);
                Some(hash)
            } else {
                None
            };
            push_planned_action(
                plan,
                Action::DeleteDir {
                    path,
                    entries: node.entries,
                    bytes: node.bytes,
                    target_proof: super::SyncTargetProof::from_listed(&node.item)?,
                    target_tree_hash,
                },
                None,
                Some(entry),
            );
            plan.delete_count = plan.delete_count.saturating_add(node.entries);
            plan.delete_bytes = plan.delete_bytes.saturating_add(node.bytes);
        }
    } else {
        let target_hash = plan_file_hash(dst, &dst_root.join(&node.relative), entry, opts, plan)?;
        push_planned_action(
            plan,
            Action::DeleteFile {
                path,
                size: entry.size,
                target_mtime: entry.mtime,
                target_mode: entry.mode,
                target_proof: super::SyncTargetProof::from_listed(&node.item)?,
                target_hash,
            },
            None,
            Some(entry),
        );
        plan.delete_count = plan.delete_count.saturating_add(1);
        plan.delete_bytes = plan.delete_bytes.saturating_add(entry.size);
    }

    Ok(())
}

/// Entscheidet, ob eine im Ziel vorhandene Datei aktualisiert werden muss.
///
/// - `Mirror`: aktualisieren, sobald Größe oder Zeit abweichen (Ziel soll exakt der
///   Quelle entsprechen — auch „zurück" auf einen älteren Stand).
/// - `Update`: aktualisieren nur, wenn sich die Datei unterscheidet UND die Quelle
///   nicht älter ist (schützt eine ggf. neuere Ziel-Datei vor Überschreiben).
fn should_update(src: &Entry, dst: &Entry, opts: &SyncOptions, equal: bool) -> bool {
    if equal {
        return false;
    }
    match opts.mode {
        SyncMode::AddOnly => false,
        SyncMode::Mirror => true,
        // Update: nur wenn die Quelle nicht älter als das Ziel ist.
        SyncMode::Update => match (src.mtime, dst.mtime) {
            (Some(s), Some(d)) => s >= d.saturating_sub(opts.mtime_tolerance),
            // Ohne verlässliche Zeit kann Update nicht beweisen, dass die Quelle
            // mindestens so neu ist. Sicherheitsprinzip: auslassen statt überschreiben.
            _ => false,
        },
    }
}

struct FileComparison {
    equal: bool,
    source_hash: Option<String>,
    target_hash: Option<String>,
}

/// Bestehende Dateien nach dem gewählten Vertrag vergleichen. Im Hashmodus werden
/// beide Digests in die spätere Aktion übernommen; dadurch ist nicht nur die
/// Entscheidung, sondern auch der exakt bestätigte Inhalt plan-gebunden.
struct FileComparisonRequest<'a> {
    src: &'a dyn Vfs,
    src_path: &'a Path,
    src_entry: &'a Entry,
    dst: &'a dyn Vfs,
    dst_path: &'a Path,
    dst_entry: &'a Entry,
    opts: &'a SyncOptions,
}

fn compare_existing_files(
    request: FileComparisonRequest<'_>,
    plan: &mut SyncPlan,
) -> io::Result<FileComparison> {
    let FileComparisonRequest {
        src,
        src_path,
        src_entry,
        dst,
        dst_path,
        dst_entry,
        opts,
    } = request;
    match opts.compare {
        Compare::SizeAndTime => Ok(FileComparison {
            equal: src_entry.size == dst_entry.size && times_equal(src_entry, dst_entry, opts),
            source_hash: None,
            target_hash: None,
        }),
        Compare::ContentHash => {
            let source_hash = hash_file_hex(src, src_path)?;
            let target_hash = hash_file_hex(dst, dst_path)?;
            plan.hashed_files = plan.hashed_files.saturating_add(2);
            plan.bytes_hashed = plan
                .bytes_hashed
                .saturating_add(src_entry.size.saturating_add(dst_entry.size));
            Ok(FileComparison {
                equal: source_hash == target_hash,
                source_hash: Some(source_hash),
                target_hash: Some(target_hash),
            })
        }
    }
}

fn plan_file_hash(
    vfs: &dyn Vfs,
    path: &Path,
    entry: &Entry,
    opts: &SyncOptions,
    plan: &mut SyncPlan,
) -> io::Result<Option<String>> {
    if opts.compare != Compare::ContentHash {
        return Ok(None);
    }
    plan.hashed_files = plan.hashed_files.saturating_add(1);
    plan.bytes_hashed = plan.bytes_hashed.saturating_add(entry.size);
    hash_file_hex(vfs, path).map(Some)
}

fn plan_metadata_transfer(
    src: &dyn Vfs,
    src_path: &Path,
    dst: &dyn Vfs,
    dst_path: Option<&Path>,
    policy: MetadataPlanPolicy,
    hardlink_group: Option<u64>,
) -> io::Result<Option<MetadataTransfer>> {
    if !policy.collect {
        return Ok(None);
    }
    let source = src
        .metadata_manifest(src_path)?
        .with_hardlink_group(hardlink_group)?;
    source.validate()?;
    let target = dst_path
        .map(|path| dst.metadata_manifest(path))
        .transpose()?;
    if let Some(manifest) = &target {
        manifest.validate()?;
    }
    let incompatible_acl_model = policy.acl_incompatible && source.acl.is_some();
    Ok(Some(MetadataTransfer {
        source,
        target,
        owner: policy.owner,
        acl: policy.acl,
        incompatible_acl_model,
        normal_mode: policy.normal_mode,
        special_mode_bits: policy.special_mode_bits,
        xattrs: policy.xattrs,
        resource_fork: policy.resource_fork,
        sparse_extents: policy.sparse_extents,
        hardlinks: policy.hardlinks && hardlink_group.is_some(),
    }))
}

fn metadata_transfer_differs(transfer: &MetadataTransfer) -> bool {
    let Some(target) = &transfer.target else {
        return true;
    };
    (transfer.owner && transfer.source.owner != target.owner)
        || (transfer.acl && transfer.source.acl != target.acl)
        || (transfer.normal_mode && transfer.source.mode & 0o777 != target.mode & 0o777)
        || (transfer.special_mode_bits && transfer.source.mode & 0o7000 != target.mode & 0o7000)
        || (transfer.xattrs && transfer.source.xattrs != target.xattrs)
        || (transfer.resource_fork && transfer.source.resource_fork != target.resource_fork)
        || (transfer.sparse_extents && transfer.source.sparse_extents != target.sparse_extents)
}

fn directory_mtime_differs(source: &Entry, target: &Entry, opts: &SyncOptions) -> bool {
    match (source.mtime, target.mtime) {
        (Some(source), Some(target)) => source.abs_diff(target) > opts.mtime_tolerance as u64,
        // Ohne Quellzeit gibt es keinen verlässlichen Wert, den der Sync setzen könnte.
        (Some(_), None) => true,
        _ => false,
    }
}

pub(super) fn planned_fork_bytes(transfer: Option<&MetadataTransfer>) -> u64 {
    transfer
        .filter(|transfer| transfer.resource_fork)
        .and_then(|transfer| transfer.source.resource_fork.as_ref())
        .map_or(0, |fork| fork.size)
}

/// Liefert den anzuwendenden Quellmodus nur dann, wenn beide Provider genügend
/// Information liefern, das Ziel Rechte überhaupt schreiben kann und sich die
/// Rechte tatsächlich unterscheiden.
///
/// Ohne die Fähigkeitsprüfung entstand eine Aktion, die nie ausgeführt werden
/// konnte: Alle vier Remote-Provider melden `supports_mode_write() == false`, und
/// SMB, WebDAV sowie FTPS listen `mode: None` — gegen einen lokalen Quellmodus ist
/// das immer ein Unterschied. Die Vorschau versprach dann eine Rechteanpassung, die
/// nirgends bilanziert wurde, der Folgeplan wurde nie leer, und ein Mirror-Zweitscan
/// brach nach den Kopien ab, weil er die unerledigte Aktion als fremde Änderung las.
fn changed_mode(src: &Entry, dst: &Entry, target_mode_writable: bool) -> Option<u32> {
    if !target_mode_writable {
        return None;
    }
    src.mode.filter(|source| Some(*source) != dst.mode)
}

/// Gelten die Änderungszeiten (innerhalb der Toleranz) als gleich?
fn times_equal(a: &Entry, b: &Entry, opts: &SyncOptions) -> bool {
    match (a.mtime, b.mtime) {
        (Some(x), Some(y)) => x.abs_diff(y) <= opts.mtime_tolerance as u64,
        // Wenn eine Zeit fehlt, behandeln wir sie als „nicht gleich" → sicherheitshalber kopieren.
        _ => false,
    }
}

/// Relativen Pfad in einen String mit Vorwärts-Slashes umwandeln (macOS/Linux).
fn rel_to_string(rel: &Path) -> String {
    rel.to_string_lossy().into_owned()
}

/// Ausschlüsse gelten nur für Namen, deren Stringpfad den echten Providernamen
/// eindeutig bezeichnet. Ein nicht sicher dekodierbarer Name bleibt deshalb ein
/// normaler Skip und kann Mirror weiterhin sperren.
fn is_excluded(exclusions: &ExcludeMatcher, parent: &Path, item: &ProofListEntry) -> bool {
    item.safely_decoded && exclusions.matches(&rel_to_string(&parent.join(&item.entry.name)))
}

/// Rekursive Zahl und Dateigröße aller Einträge UNTERHALB eines Wurzelordners.
/// Ein fehlendes Ziel gilt als leer; alle anderen Lesefehler werden weitergereicht,
/// damit eine unvollständige Auflistung niemals versehentlich wie ein leerer Baum wirkt.
fn measure_contents(
    vfs: &dyn Vfs,
    root: &Path,
    rel: &Path,
    exclusions: &ExcludeMatcher,
) -> io::Result<(u64, u64)> {
    let entries = match complete_listing(vfs, &root.join(rel)) {
        Ok(listing) => listing,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let mut count = 0u64;
    let mut bytes = 0u64;
    for item in &entries {
        if is_excluded(exclusions, rel, item) {
            continue;
        }
        let entry = &item.entry;
        let child_rel = rel.join(&entry.name);
        count = count.saturating_add(1);
        if item.safely_decoded && entry.is_dir && !entry.is_symlink {
            let (child_count, child_bytes) = measure_contents(vfs, root, &child_rel, exclusions)?;
            count = count.saturating_add(child_count);
            bytes = bytes.saturating_add(child_bytes);
        } else if entry.is_file {
            bytes = bytes.saturating_add(entry.size);
        }
    }
    Ok((count, bytes))
}
