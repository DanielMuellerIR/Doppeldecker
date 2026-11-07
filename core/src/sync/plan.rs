//! Rein lesende Planerstellung und plan-gebundene Metadatenbilanz.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use crate::metadata::{Capability, MetadataCapabilities, MetadataManifest, MetadataProfile};
use crate::tree_measure::measure_entry;
use crate::vfs::{Entry, ProofListEntry, Vfs};

use super::exclude::ExcludeMatcher;
use super::{
    complete_listing, hash_tree, optional_stat, Action, Compare, ComparisonAction, ComparisonEntry,
    ComparisonRow, ComparisonSide, MetadataClass, MetadataClassStats, MetadataIssue,
    MetadataIssueKind, MetadataReport, MetadataTransfer, SyncMode, SyncOptions, SyncPlan,
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
    plan_with_cancel(src, src_root, dst, dst_root, opts, &crate::Cancel::new())
}

pub(crate) fn plan_with_cancel(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    opts: &SyncOptions,
    cancel: &crate::Cancel,
) -> io::Result<SyncPlan> {
    plan_with_class(src, src_root, dst, dst_root, opts, cancel, false, true)
}
pub(crate) fn plan_content_with_cancel(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    opts: &SyncOptions,
    cancel: &crate::Cancel,
    shared_owner_domain: bool,
) -> io::Result<SyncPlan> {
    plan_with_class(
        src,
        src_root,
        dst,
        dst_root,
        opts,
        cancel,
        true,
        shared_owner_domain,
    )
}
#[allow(clippy::too_many_arguments)]
fn plan_with_class(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    opts: &SyncOptions,
    cancel: &crate::Cancel,
    ordinary_content: bool,
    shared_owner_domain: bool,
) -> io::Result<SyncPlan> {
    check_cancel(cancel)?;
    // Die Optionssyntax wird vor jedem Providerzugriff geprüft. CLI-Aufrufer tun das
    // zusätzlich noch früher, damit ungültige Muster nicht einmal Verbindungen öffnen.
    opts.validate()?;
    let exclusions = ExcludeMatcher::new(&opts.exclude)?;
    let metadata = MetadataPlanPolicy::for_roots(
        src,
        src_root,
        dst,
        dst_root,
        opts,
        ordinary_content,
        shared_owner_domain,
    )?;
    // Die Ziel-Gesamtzahl wird unabhängig vom eigentlichen Diff ermittelt. Nur so kann
    // eine spätere Löschgrenze auch einen einzigen geplanten `DeleteDir` korrekt als
    // möglicherweise tausende betroffene Dateien bewerten. Beide Zähler speisen
    // ausschließlich die Mirror-Löschgates (Kern, Realmodus, UI-Vorwarnung) — nur
    // Mirror kann überhaupt Entfernungen planen. Für AddOnly/Update entfallen die
    // zwei zusätzlichen Volltraversierungen deshalb ersatzlos; über ein Netzwerk
    // wären sie pro Lauf hunderte unnötige Listing-Round-Trips.
    let mut plan = match opts.mode {
        SyncMode::Mirror => SyncPlan {
            source_count: measure_contents(src, src_root, Path::new(""), &exclusions, cancel)?.0,
            target_count: measure_contents(dst, dst_root, Path::new(""), &exclusions, cancel)?.0,
            ..Default::default()
        },
        SyncMode::AddOnly | SyncMode::Update => SyncPlan::default(),
    };
    let mut hardlinks = SourceMetadataPlanState::default();
    let request = DiffRequest {
        src,
        src_root,
        dst,
        dst_root,
        opts,
        cancel,
        metadata,
        exclusions: &exclusions,
        conservative_names: dst.file_commit_class_at(dst_root)?
            == crate::vfs::FileCommitClass::BestEffort,
        ordinary_content,
        file_dacl_ready: ordinary_content
            && matches!(opts.mode, SyncMode::Update | SyncMode::Mirror)
            && opts.metadata == MetadataProfile::NativeBestEffort
            && matches!(
                dst.file_dacl_write_support()?,
                crate::windows_dacl::WindowsDaclWriteSupport::Ready(_)
            ),
    };
    diff_dir(
        &request,
        Path::new(""),
        TargetDirKnowledge::Unknown,
        &mut hardlinks,
        &mut plan,
    )?;
    hardlinks.finalize(&mut plan)?;
    deduplicate_file_dacl_actions(&mut plan);
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
    // Beobachtete oder ungeprüfte Metadaten können die einzige Abweichung sein.
    // Ihre Bilanz benötigt keine künstliche Metadaten-Schreibaktion.
    let non_action_metadata_issues = std::mem::take(&mut plan.metadata_report.issues);
    plan.metadata_report = build_metadata_report(opts.metadata, &plan.actions, shared_owner_domain);
    for issue in non_action_metadata_issues {
        if matches!(
            issue.class,
            MetadataClass::SparseExtents | MetadataClass::Hardlinks
        ) {
            if let Some(existing) = plan.metadata_report.issues.iter_mut().find(|existing| {
                existing.path == issue.path
                    && existing.class == issue.class
                    && existing.kind == issue.kind
            }) {
                // Eine konkrete Beobachtung präzisiert die generische Klassenmeldung;
                // derselbe Verlust zählt weiterhin nur einmal.
                *existing = issue;
                continue;
            }
        }
        match issue.kind {
            MetadataIssueKind::Unsupported => plan.metadata_report.unsupported_metadata += 1,
            MetadataIssueKind::IncompatibleAclModel => {
                plan.metadata_report.incompatible_acl_models += 1
            }
            MetadataIssueKind::PrivilegeFailure => plan.metadata_report.privilege_failures += 1,
        }
        if let Some(class) = plan
            .metadata_report
            .classes
            .iter_mut()
            .find(|value| value.class == issue.class)
        {
            class.omitted += 1;
        } else {
            plan.metadata_report.classes.push(MetadataClassStats {
                class: issue.class,
                transferred: 0,
                omitted: 1,
            });
        }
        plan.metadata_report.issues.push(issue);
    }
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
    source_size: Option<u64>,
    allow_sparse_layout_omission: bool,
    /// Kann das Ziel POSIX-Rechte überhaupt schreiben?
    ///
    /// Portable nutzt die allgemeine Providerzusage. Gewöhnliches NativeBestEffort
    /// darf den getrennten rwx-Vertrag mit konkret beobachtetem Quellmodus nutzen;
    /// andere native Wege behalten ihre zusätzlichen Modusbit-Fähigkeiten.
    target_mode_writable: bool,
    owner: bool,
    acl: bool,
    acl_incompatible: bool,
    windows_dacl: bool,
    normal_mode: bool,
    observe_normal_mode: bool,
    special_mode_bits: bool,
    xattrs: bool,
    resource_fork: bool,
    named_streams: bool,
    require_named_streams: bool,
    directory_stream_updates: bool,
    is_directory: bool,
    observe_source_named_streams: bool,
    observe_target_named_streams: bool,
    sparse_extents: bool,
    hardlinks: bool,
    /// Quellbeziehungen werden auch dann bilanziert, wenn das Ziel sie nicht erhält.
    observe_hardlinks: bool,
}

impl MetadataPlanPolicy {
    #[allow(clippy::too_many_arguments)]
    fn for_roots(
        src: &dyn Vfs,
        src_root: &Path,
        dst: &dyn Vfs,
        dst_root: &Path,
        opts: &SyncOptions,
        ordinary_content: bool,
        shared_owner_domain: bool,
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
        if !ordinary_content
            && opts.metadata == MetadataProfile::NativeBestEffort
            && opts.mode == SyncMode::Mirror
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "NativeBestEffort darf nicht für einen unvollständigen Mirror-Lauf verwendet werden",
            ));
        }

        if ordinary_content
            && opts.metadata == MetadataProfile::NativeStrict
            && !shared_owner_domain
        {
            return Err(io::Error::new(io::ErrorKind::Unsupported,
                "NativeStrict benötigt eine bestätigte gemeinsame UID/GID-Domäne; gewöhnlicher Sync kennt sie nur zwischen tatsächlichen lokalen Roots"));
        }

        let source = src.metadata_capabilities(src_root)?;
        let target = dst.metadata_capabilities(dst_root)?;
        if opts.metadata == MetadataProfile::NativeStrict {
            require_complete_native_capabilities(&source, &target)?;
        }

        let observe_normal_mode =
            ordinary_content && opts.metadata != MetadataProfile::NativeStrict;
        let normal_mode_readable = readable(source.special_mode_bits) || observe_normal_mode;
        Ok(Self {
            collect: true,
            source_size: None,
            allow_sparse_layout_omission: opts.metadata == MetadataProfile::NativeBestEffort,
            target_mode_writable: target_mode_writable
                && normal_mode_readable
                && (target.special_mode_bits == Capability::Exact
                    || (ordinary_content && opts.metadata != MetadataProfile::NativeStrict)),
            // Numerische IDs sind nur innerhalb derselben bestätigten Domäne
            // bedeutungsgleich. Der private Aufrufer bindet die echten Endpunkte;
            // eine lokale Projektionssicht darf keine Remote-IDs legitimieren.
            owner: (!ordinary_content || shared_owner_domain)
                && readable(source.owner)
                && target.owner == Capability::Exact,
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
            windows_dacl: ordinary_content
                && opts.metadata == MetadataProfile::NativeBestEffort
                && source.acl_model == crate::metadata::AclModel::WindowsSid,
            normal_mode: normal_mode_readable
                && (target.special_mode_bits == Capability::Exact
                    || (ordinary_content
                        && opts.metadata != MetadataProfile::NativeStrict
                        && target_mode_writable)),
            observe_normal_mode,
            special_mode_bits: readable(source.special_mode_bits)
                && target.special_mode_bits == Capability::Exact,
            xattrs: readable(source.xattrs) && target.xattrs == Capability::Exact,
            resource_fork: readable(source.resource_fork)
                && target.resource_fork == Capability::Exact,
            directory_stream_updates: opts.metadata == MetadataProfile::NativeBestEffort
                && readable(source.named_streams)
                && target.named_streams == Capability::Exact
                && dst.supports_directory_stream_updates(dst_root)?,
            is_directory: false,
            require_named_streams: opts.metadata == MetadataProfile::NativeStrict,
            observe_source_named_streams: readable(source.named_streams),
            observe_target_named_streams: readable(target.named_streams),
            named_streams: readable(source.named_streams)
                && target.named_streams == Capability::Exact,
            sparse_extents: readable(source.sparse_extents)
                && target.sparse_extents == Capability::Exact,
            hardlinks: readable(source.hardlinks)
                && target.hardlinks == Capability::Exact
                && source.acl_model != crate::metadata::AclModel::WindowsSid,
            observe_hardlinks: readable(source.hardlinks),
        })
    }

    /// Gewöhnliche native Übertragung darf beobachtete rwx-Rechte unabhängig
    /// von einer Lesefähigkeit für setuid/setgid/sticky erhalten. Ein fehlender
    /// konkreter Modus aktiviert dagegen niemals einen erfundenen Nullwert.
    fn for_entry(mut self, entry: &Entry) -> Self {
        self.source_size = (entry.is_file && !entry.is_dir).then_some(entry.size);
        if self.observe_normal_mode {
            self.normal_mode &= entry.mode.is_some();
        }
        self
    }

    /// Reguläre Dateiströme verwenden den atomaren Basistausch. Zusammengeführte
    /// Ordner benötigen den getrennten Vertrag für bestätigte Einzelwirkungen.
    fn for_directory(self) -> Self {
        Self {
            resource_fork: false,
            named_streams: false,
            is_directory: true,
            sparse_extents: false,
            hardlinks: false,
            observe_hardlinks: false,
            acl_incompatible: self.acl_incompatible,
            ..self
        }
    }
}

#[derive(Default)]
struct SourceMetadataPlanState {
    groups: std::collections::HashMap<(u64, u64), HardlinkGroup>,
    next_group: u64,
    reported_counts: std::collections::BTreeMap<String, u32>,
    omitted_groups: BTreeMap<String, u64>,
    selected_files: u8,
    sparse_issues: BTreeMap<String, crate::metadata::SparseReadIssue>,
    layout_issues: BTreeMap<String, SparseLayoutObservation>,
}

struct SparseLayoutObservation {
    unit: u32,
    source_digest: [u8; 32],
    target_digest: Option<[u8; 32]>,
}

struct HardlinkGroup {
    id: u64,
    selected_members: usize,
    /// Erst gesetzt, sobald dieser Zielname nach dem Plan sicher den Quellinhalt trägt.
    /// Das Flag zeigt, ob der Anker in diesem Lauf einen neuen Inode erhält.
    target_anchor: Option<(String, bool)>,
}

impl SourceMetadataPlanState {
    fn observe_manifest_facts(&mut self, path: &str, source: &MetadataManifest) {
        if let Some(issue) = source.sparse_read_issue {
            self.sparse_issues.insert(path.into(), issue);
        }
        if source.hardlink_group.is_none() {
            if let Some(count) = source.reported_hard_links.filter(|count| *count != 1) {
                self.reported_counts.insert(path.into(), count);
            }
        }
    }

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

    fn finalize(&self, plan: &mut SyncPlan) -> io::Result<()> {
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
        for (path, group) in &self.omitted_groups {
            if !unshared.contains(group) {
                plan.metadata_report.issues.push(MetadataIssue {
                    path: path.clone(),
                    class: MetadataClass::Hardlinks,
                    kind: MetadataIssueKind::Unsupported,
                    reason: "Die ausgewählte Hardlink-Gruppe ist bekannt; der Zieladapter kann ihre gemeinsame Dateiidentität nicht erhalten".into(),
                });
            }
        }
        // Linkzahlen enthalten auch äußere Namen. Mit nur einer ausgewählten
        // regulären Datei gibt es keine Beziehung innerhalb dieser Auswahl zu
        // rekonstruieren. Die rohe Beobachtung bleibt trotzdem digestgebunden.
        for (path, count) in &self.reported_counts {
            if *count == 0 || self.selected_files > 1 {
                plan.metadata_report
                    .issues
                    .push(reported_link_issue(path, *count));
            }
        }
        for (path, issue) in &self.sparse_issues {
            plan.metadata_report
                .issues
                .push(sparse_read_issue(path, *issue));
        }
        for (path, observation) in &self.layout_issues {
            let unit = observation.unit;
            if observation.target_digest.is_some() {
                if let Some(existing) = plan
                    .metadata_observations
                    .iter()
                    .find(|value| value.path == *path)
                {
                    if existing.source_digest != observation.source_digest
                        || existing.target_digest != observation.target_digest
                    {
                        return Err(crate::real_mode::plan_drift(
                            "Sparse-Geometriebeobachtung änderte sich während der Planung",
                        ));
                    }
                } else {
                    plan.metadata_observations.push(super::MetadataObservation {
                        path: path.clone(),
                        source_digest: observation.source_digest,
                        target_digest: observation.target_digest,
                    });
                }
            }
            plan.metadata_report.issues.push(MetadataIssue {
                path: path.clone(),
                class: MetadataClass::SparseExtents,
                kind: MetadataIssueKind::Unsupported,
                reason: format!("Die gemeldeten Sparse-Grenzen sind feiner als der lokale Allokationsblock von {unit} Byte; der logische Inhalt bleibt übertragbar, die genaue Bereichsstruktur wird ausgelassen"),
            });
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
    if source.acl_model == crate::metadata::AclModel::WindowsSid
        || target.acl_model == crate::metadata::AclModel::WindowsSid
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NativeStrict besitzt noch keinen vollständigen Windows-SID-/DACL-Schreibvertrag",
        ));
    }
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
        || unknown(source.named_streams)
        || unknown(target.named_streams)
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
    shared_owner_domain: bool,
) -> MetadataReport {
    let mut counts: BTreeMap<MetadataClass, (u64, u64)> = BTreeMap::new();
    let mut issues = Vec::new();

    for action in actions {
        if let Action::SetFileDacl { .. } = action {
            counts.entry(MetadataClass::Acl).or_default().0 += 1;
            continue;
        }
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

        // Eine ganze neue Basisdatei beseitigt auch alte Zielströme. Diese Wirkung
        // bleibt bei beobachteter leerer Quelle ein Transfer, bei unbeobachteter
        // Quelle dagegen ein konkret bekannter Metadatenverlust.
        let source_streams_present = transfer
            .source
            .named_streams
            .as_ref()
            .is_some_and(|streams| !streams.is_empty());
        let target_streams_present = transfer
            .target
            .as_ref()
            .and_then(|target| target.named_streams.as_ref())
            .is_some_and(|streams| !streams.is_empty());
        if matches!(
            action,
            Action::CreateDir { .. } | Action::SetDirectoryMetadata { .. }
        ) {
            if transfer.directory_streams.is_some() {
                counts.entry(MetadataClass::NamedStreams).or_default().0 += 1;
            } else if let Some(issue) = directory_named_stream_issue(path, transfer) {
                counts.entry(MetadataClass::NamedStreams).or_default().1 += 1;
                issues.push(issue);
            }
        } else if source_streams_present || target_streams_present {
            let entry = counts.entry(MetadataClass::NamedStreams).or_default();
            if transfer.named_streams {
                entry.0 = entry.0.saturating_add(1);
            } else {
                entry.1 = entry.1.saturating_add(1);
                issues.push(MetadataIssue {
                    path: path.clone(),
                    class: MetadataClass::NamedStreams,
                    kind: MetadataIssueKind::Unsupported,
                    reason: if transfer.source.named_streams.is_none() && target_streams_present {
                        "Benannte Datenströme der Quelle sind nicht beobachtet; beim Ersetzen gehen die beobachteten Zielströme verloren".into()
                    } else if target_streams_present && !source_streams_present {
                        "Die neue Basisdatei erhält die vorhandenen benannten Zielströme nicht".into()
                    } else {
                        "Der Zieladapter bietet keinen geprüften Schreibzugriff auf benannte Datenströme".into()
                    },
                });
            }
        }

        let replaces_base = matches!(
            action,
            Action::CopyFile { .. } | Action::UpdateFile { .. } | Action::CreateHardlink { .. }
        );
        for issue in observed_metadata_issues(path, transfer, replaces_base) {
            counts.entry(issue.class).or_default().1 += 1;
            issues.push(issue);
        }

        if matches!(
            transfer.windows_dacl,
            Some(
                crate::windows_dacl::WindowsDaclTransfer::Exact(_)
                    | crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(_)
                    | crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryExact(_)
                    | crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(
                        _
                    )
            )
        ) && transfer.acl
        {
            counts.entry(MetadataClass::Acl).or_default().0 += 1;
        }
        let present = [
            (
                MetadataClass::Owner,
                transfer.source.owner.is_some(),
                transfer.owner,
                MetadataIssueKind::Unsupported,
            ),
            (
                MetadataClass::Acl,
                transfer.source.acl.is_some() && !has_windows_security_context(transfer),
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
                    reason: if class == MetadataClass::Owner && !shared_owner_domain {
                        "Die numerischen UID/GID der Quelle sind bekannt, aber eine bestätigte gemeinsame UID/GID-Domäne fehlt. Owner und Gruppe werden nicht anhand gleicher Nummern übernommen; Inhalt, normale Rechte und Dateizeit bleiben unabhängig davon übertragbar.".into()
                    } else if kind == MetadataIssueKind::IncompatibleAclModel {
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
                    || value.named_streams
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

fn has_windows_security_context(transfer: &MetadataTransfer) -> bool {
    use crate::metadata::Acl;
    transfer.source.windows_security.is_some()
        || matches!(transfer.source.acl, Some(Acl::WindowsDacl(_)))
        || transfer.target.as_ref().is_some_and(|target| {
            target.windows_security.is_some() || matches!(target.acl, Some(Acl::WindowsDacl(_)))
        })
}

fn reported_link_issue(path: &str, count: u32) -> MetadataIssue {
    MetadataIssue {
        path: path.into(),
        class: MetadataClass::Hardlinks,
        kind: MetadataIssueKind::Unsupported,
        reason: if count == 0 {
            "Die gemeldete Linkzahl 0 belegt keine vollständige Hardlink-Beobachtung; eine Gruppe wird nicht angenommen".into()
        } else {
            format!("Die Quelle meldet insgesamt {count} Links für diese reguläre Datei; eine vollständige volumengebundene Hardlink-Gruppe ist im Adapter nicht modelliert und wird nicht rekonstruiert")
        },
    }
}

fn sparse_read_issue(path: &str, issue: crate::metadata::SparseReadIssue) -> MetadataIssue {
    use crate::metadata::SparseReadIssue as I;
    MetadataIssue {
        path: path.into(), class: MetadataClass::SparseExtents,
        kind: if issue == I::ServerStatus(0xc000_0022) { MetadataIssueKind::PrivilegeFailure } else { MetadataIssueKind::Unsupported },
        reason: match issue {
            I::ServerStatus(status) => format!("Die SMB-Sparse-Abfrage wurde mit Status {status:#010x} abgewiesen; der logische Inhalt bleibt unabhängig übertragbar"),
            I::BufferLimit => "Die SMB-Sparse-Abfrage lieferte innerhalb der begrenzten Puffer-/Seitenmenge keinen vollständigen Bestand; der Inhalt wird ohne Sparse-Zusage übertragen".into(),
            I::InvalidResponse => "Die SMB-Sparse-Antwort ist ungültig; aus ihr werden keine Löcher abgeleitet, der Inhalt bleibt unabhängig übertragbar".into(),
        },
    }
}

fn sparse_metadata_issues(
    path: &str,
    transfer: &MetadataTransfer,
    replaces_base: bool,
) -> Vec<MetadataIssue> {
    if !replaces_base
        && !transfer.sparse_extents
        && transfer.source.sparse_extents.is_some()
        && transfer
            .target
            .as_ref()
            .is_some_and(|target| target.sparse_extents != transfer.source.sparse_extents)
    {
        return vec![MetadataIssue { path:path.into(), class:MetadataClass::SparseExtents, kind:MetadataIssueKind::Unsupported,
            reason:"Die gemeldete Sparse-Quellstruktur ist am Ziel abweichend oder nicht beobachtet; der Zieladapter bietet keinen geprüften Sparse-Schreibvertrag. Gleicher logischer Inhalt bleibt unverändert".into() }];
    }
    vec![]
}

fn observed_metadata_issues(
    path: &str,
    transfer: &MetadataTransfer,
    replaces_base: bool,
) -> Vec<MetadataIssue> {
    use crate::webdav_security::WebDavPropertyObservation;
    let mut issues = windows_metadata_issues(path, transfer, replaces_base);
    issues.extend(sparse_metadata_issues(path, transfer, replaces_base));
    for (side, manifest) in [
        ("Quelle", Some(&transfer.source)),
        ("Ziel", transfer.target.as_ref()),
    ] {
        let Some(observation) = manifest.and_then(|value| value.webdav_security.as_ref()) else {
            continue;
        };
        for (class, property, reason) in [
            (
                MetadataClass::Acl,
                &observation.acl,
                observation.acl_reason(),
            ),
            (
                MetadataClass::Owner,
                &observation.owner,
                observation.owner_reason(),
            ),
        ] {
            let denied = observation.query_status == 403
                || observation.resource_status == Some(403)
                || matches!(
                    property,
                    WebDavPropertyObservation::Unavailable { status: 403 }
                );
            let property_detail = match property {
                WebDavPropertyObservation::Unavailable { status } => {
                    format!("; Property-Status {status}")
                }
                WebDavPropertyObservation::Observed { empty: true, .. } => {
                    "; Property-Status 200, ausdrücklich leer".into()
                }
                WebDavPropertyObservation::Observed { empty: false, .. } => {
                    "; Property-Status 200, opaker Wert".into()
                }
                WebDavPropertyObservation::Missing => String::new(),
            };
            let resource_detail = observation
                .resource_status
                .map_or_else(String::new, |status| format!("; Ressourcenstatus {status}"));
            issues.push(MetadataIssue {
                path: path.into(),
                class,
                kind: if denied {
                    MetadataIssueKind::PrivilegeFailure
                } else {
                    MetadataIssueKind::Unsupported
                },
                reason: format!(
                    "{side}: {reason} (HTTP {}{resource_detail}{property_detail})",
                    observation.query_status
                ),
            });
        }
    }
    issues
}

/// Windows-SIDs sind keine numerischen POSIX-Owner. Der lesende Vertrag bindet
/// sowohl bekannte Werte als auch konkrete Beobachtungslücken, ohne daraus eine
/// Schreibzusage oder bei gleichem Hauptinhalt einen neuen Basistausch abzuleiten.
fn windows_metadata_issues(
    path: &str,
    transfer: &MetadataTransfer,
    replaces_base: bool,
) -> Vec<MetadataIssue> {
    use crate::metadata::{Acl, WindowsSecurityReadIssue};
    if !has_windows_security_context(transfer) {
        return Vec::new();
    }
    let source = &transfer.source;
    let target = transfer.target.as_ref();
    let source_security = source.windows_security.as_ref();
    let target_security = target.and_then(|value| value.windows_security.as_ref());
    let target_acl = target.and_then(|value| value.acl.as_ref());
    let mut issues: Vec<MetadataIssue> = Vec::new();
    let issue = |class, kind, reason: String| MetadataIssue {
        path: path.into(),
        class,
        kind,
        reason,
    };
    if let Some(unreadable) = source_security.and_then(|value| value.issue) {
        let kind = if unreadable == WindowsSecurityReadIssue::ReadControlDenied {
            MetadataIssueKind::PrivilegeFailure
        } else {
            MetadataIssueKind::Unsupported
        };
        issues.push(issue(MetadataClass::Acl, kind, unreadable.reason().into()));
        issues.push(issue(
            MetadataClass::Owner,
            kind,
            format!(
                "Windows-Owner-/Group-SIDs wurden nicht beobachtet: {}",
                unreadable.reason()
            ),
        ));
    } else {
        if source.acl.is_some()
            && !transfer.acl
            && (replaces_base
                || source.acl.as_ref() != target_acl
                || matches!(
                    transfer.windows_dacl,
                    Some(crate::windows_dacl::WindowsDaclTransfer::Omitted(
                        crate::windows_dacl::WindowsDaclOmission::Source(_)
                    ))
                ))
        {
            let kind = match &transfer.windows_dacl {
                Some(crate::windows_dacl::WindowsDaclTransfer::Omitted(
                    crate::windows_dacl::WindowsDaclOmission::Target(issue),
                )) if issue.is_access_denied() => MetadataIssueKind::PrivilegeFailure,
                _ if transfer.incompatible_acl_model => MetadataIssueKind::IncompatibleAclModel,
                _ => MetadataIssueKind::Unsupported,
            };
            let reason = match &transfer.windows_dacl {
                Some(crate::windows_dacl::WindowsDaclTransfer::Omitted(reason)) => reason.reason(),
                _ if matches!(source.acl, Some(Acl::WindowsDacl(_))) =>
                    "Der SMB-Adapter beobachtet Windows-DACLs; für diese Ausführung ist kein passender DACL-Schreibvertrag vorbereitet",
                _ => "Quell- und Ziel-ACL verwenden inkompatible Modelle; die Quell-ACL wird nicht übertragen",
            };
            issues.push(issue(MetadataClass::Acl, kind, reason.into()));
        }
        if let Some(observation) = source_security {
            let same_owner = target_security.is_some_and(|target| {
                target.issue.is_none()
                    && target.namespace == observation.namespace
                    && target.owner == observation.owner
                    && target.group == observation.group
            });
            if replaces_base || !same_owner {
                issues.push(issue(MetadataClass::Owner, MetadataIssueKind::Unsupported,
                    "Windows-Owner-/Group-SIDs werden nicht gesetzt oder in numerische UID/GID übersetzt; eine gemeinsame Identitätsdomäne ist nicht belegt".into()));
            }
        }
    }
    // Quellenübertragung und fehlende Zielbeobachtung sind unterschiedliche
    // Auslassungen. Beide Seiten bleiben auch im Noop sichtbar; ein unbekannter
    // Zielkontext wird nicht durch die inkompatible Quellen-ACL erklärt.
    if let Some(unreadable) = target_security.and_then(|value| value.issue) {
        issues.push(issue(
            MetadataClass::Acl,
            if unreadable == WindowsSecurityReadIssue::ReadControlDenied {
                MetadataIssueKind::PrivilegeFailure
            } else {
                MetadataIssueKind::Unsupported
            },
            format!(
                "Windows-Sicherheit des Ziels nicht beobachtet: {}",
                unreadable.reason()
            ),
        ));
    } else if replaces_base
        && source.acl.is_none()
        && source_security.is_none()
        && matches!(target_acl, Some(Acl::WindowsDacl(_)))
    {
        issues.push(issue(
            MetadataClass::Acl,
            MetadataIssueKind::Unsupported,
            "Die neue Basisdatei erhält die vorhandene Windows-DACL des Ziels nicht".into(),
        ));
    }
    issues
}

fn observed_named_streams_differ(transfer: &MetadataTransfer) -> bool {
    let Some(source) = transfer.source.named_streams.as_ref() else {
        return false;
    };
    let target = transfer
        .target
        .as_ref()
        .and_then(|value| value.named_streams.as_ref());
    target != Some(source) && (!source.is_empty() || target.is_some_and(|value| !value.is_empty()))
}

fn directory_named_stream_issue(path: &str, transfer: &MetadataTransfer) -> Option<MetadataIssue> {
    if !observed_named_streams_differ(transfer) {
        return None;
    }
    Some(MetadataIssue {
        path: path.into(),
        class: MetadataClass::NamedStreams,
        kind: MetadataIssueKind::Unsupported,
        reason: "Der Zieladapter kann benannte Verzeichnisströme noch nicht einzeln übernehmen. Zusammengeführte Ordner behalten ihre vorhandenen Zielströme unverändert".into(),
    })
}

fn directory_stream_observation_issue(
    path: &str,
    transfer: &MetadataTransfer,
    source_readable: bool,
) -> Option<MetadataIssue> {
    (source_readable && transfer.source.named_streams.is_none()).then(|| MetadataIssue {
        path: path.into(),
        class: MetadataClass::NamedStreams,
        kind: MetadataIssueKind::Unsupported,
        reason: "Die benannten Verzeichnisströme der Quelle wurden nicht vollständig beobachtet. Vorhandene Zielströme bleiben beim Zusammenführen unverändert".into(),
    })
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
    cancel: &'a crate::Cancel,
    metadata: MetadataPlanPolicy,
    exclusions: &'a ExcludeMatcher,
    conservative_names: bool,
    ordinary_content: bool,
    file_dacl_ready: bool,
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
        | Action::SetFileDacl { path, .. }
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
        | Action::SetFileDacl { .. }
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
    hardlinks: &mut SourceMetadataPlanState,
    plan: &mut SyncPlan,
) -> io::Result<()> {
    let DiffRequest {
        src,
        src_root,
        dst,
        dst_root,
        opts,
        cancel,
        metadata,
        exclusions,
        conservative_names,
        ordinary_content,
        file_dacl_ready,
    } = *request;
    check_cancel(cancel)?;
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
        check_cancel(cancel)?;
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
        if file_dacl_ready && metadata.windows_dacl {
            if let Some(target) = de_item.filter(|target| {
                target.entry.is_file
                    && !target.entry.is_symlink
                    && target.entry.inode.is_some_and(|id| id != 0)
            }) {
                let source_metadata = if se.is_file && !se.is_symlink {
                    Some(src.metadata_manifest(&src_root.join(&child_rel))?)
                } else {
                    None
                };
                plan.file_dacl_selections
                    .push(crate::file_dacl::FileDaclSelection {
                        path: child_str.clone(),
                        file_id: target.entry.inode.unwrap(),
                        object_namespace: crate::file_dacl_registry::object_namespace(
                            &dst.root_identity(dst_root)?,
                        )?,
                        target_proof: super::SyncTargetProof::from_listed(target)?,
                        source_metadata,
                    });
            }
        }

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
                            let mut transfer = plan_metadata_transfer(
                                src,
                                &src_root.join(&child_rel),
                                MetadataTarget {
                                    vfs: dst,
                                    path: &dst_root.join(&child_rel),
                                    existing: true,
                                    relative: &child_str,
                                },
                                metadata.for_directory().for_entry(se),
                                None,
                                hardlinks,
                            )?
                            .ok_or_else(|| {
                                io::Error::other(
                                    "Aktivierte Metadatenrichtlinie lieferte kein Manifest",
                                )
                            })?;
                            bind_metadata_observation(plan, hardlinks, &child_str, &transfer)?;
                            if let Some(streams) = &mut transfer.directory_streams {
                                streams.target_proof = Some(super::SyncTargetProof::from_listed(
                                    de_item.expect("Zielordner stammt aus demselben Listing"),
                                )?);
                            }
                            if let Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(
                                dacl,
                            )) = &mut transfer.windows_dacl
                            {
                                dacl.target_proof = Some(super::SyncTargetProof::from_listed(
                                    de_item.expect("Zielordner stammt aus demselben Listing"),
                                )?);
                                dacl.validate()?;
                            }
                            if let Some(issue) = directory_stream_observation_issue(
                                &child_str,
                                &transfer,
                                metadata.observe_source_named_streams,
                            ) {
                                plan.metadata_report.issues.push(issue);
                            }
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
                            let has_directory_effect = transfer.owner
                                || transfer.acl
                                || transfer.normal_mode
                                || transfer.special_mode_bits
                                || transfer.xattrs
                                || transfer.directory_streams.is_some()
                                || dst.supports_mtime_write();
                            if metadata_transfer_differs(&transfer)
                                || (has_directory_effect
                                    && ((dst.supports_mtime_write()
                                        && directory_mtime_differs(se, d, opts))
                                        || changes_children))
                            {
                                plan.bytes_to_transfer = plan.bytes_to_transfer.saturating_add(
                                    planned_directory_stream_bytes(Some(&transfer)),
                                );
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
                            } else {
                                if let Some(issue) =
                                    directory_named_stream_issue(&child_str, &transfer)
                                {
                                    plan.metadata_report.issues.push(issue);
                                }
                                plan.metadata_report
                                    .issues
                                    .extend(observed_metadata_issues(&child_str, &transfer, false));
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
                    let mut metadata_transfer = plan_metadata_transfer(
                        src,
                        &src_root.join(&child_rel),
                        MetadataTarget {
                            vfs: dst,
                            path: &dst_root.join(&child_rel),
                            existing: false,
                            relative: &child_str,
                        },
                        metadata.for_directory().for_entry(se),
                        None,
                        hardlinks,
                    )?;
                    if ordinary_content && opts.metadata == MetadataProfile::NativeBestEffort {
                        if let Some(transfer) = &mut metadata_transfer {
                            use crate::windows_dacl::{
                                WindowsDaclOmission as O, WindowsDaclTransfer as T,
                                WindowsDaclWriteSupport as S,
                            };
                            if let Some(crate::metadata::Acl::WindowsDacl(after)) =
                                &transfer.source.acl
                            {
                                // Nur diese zusätzliche Eignungsbeobachtung ist optional.
                                // Das folgende diff_dir verlangt weiter sein vollständiges Listing.
                                let eligible = match src.list_complete(&src_root.join(&child_rel)) {
                                    Ok(listing) if !listing.complete => Err(O::CreatedDirectoryListingUnobserved),
                                    Ok(listing) => match &transfer.source.named_streams {
                                        None => Err(O::CreatedDirectoryStreamsUnobserved),
                                        Some(v) if !v.is_empty() => Err(O::CreatedDirectoryStreamsNonempty),
                                        Some(_) if listing.entries.is_empty() => Ok(None),
                                        Some(_) => match crate::created_directory_dacl::CreatedDirectoryChildrenProof::from_listing(&listing) {
                                            Ok(proof) => Ok(Some(proof)),
                                            Err(_) => Err(O::CreatedDirectoryListingUnobserved),
                                        },
                                    },
                                    Err(error) if matches!(crate::error_code(&error), crate::ErrorCode::Interrupted | crate::ErrorCode::PlanDrift) => return Err(error),
                                    Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
                                        return Err(crate::real_mode::plan_drift("Quellordner verschwand während der zusätzlichen Rechtebeobachtung"))
                                    }
                                    Err(_) => Err(O::CreatedDirectoryListingUnobserved),
                                };
                                transfer.windows_dacl = Some(match eligible {
                                    Err(issue) => T::Omitted(issue),
                                    Ok(children) => {
                                        match dst.created_directory_dacl_write_support()? {
                                            S::Unavailable(issue) => T::Omitted(O::Target(issue)),
                                            S::Ready(context) => {
                                                let issue = if children.is_some() {
                                                    after.directory_before_children_write_issue(
                                                        context.namespace,
                                                        &context.owner,
                                                    )
                                                } else {
                                                    after.directory_write_issue(
                                                        context.namespace,
                                                        &context.owner,
                                                    )
                                                };
                                                match issue {
                                                Some(issue) => T::Omitted(O::Source(issue)),
                                                None => match children {
                                                    None => T::CreatedDirectoryExact(Box::new(crate::created_directory_dacl::CreatedDirectoryDaclSelection {
                                                        after: after.clone(), probe_context: context,
                                                    })),
                                                    Some(source_children) => T::CreatedDirectoryBeforeChildrenExact(Box::new(crate::created_directory_dacl::CreatedDirectoryBeforeChildrenSelection {
                                                        after: after.clone(), probe_context: context, source_children,
                                                    })),
                                                },
                                            }
                                            }
                                        }
                                    }
                                });
                                transfer.acl = matches!(
                                    transfer.windows_dacl,
                                    Some(
                                        T::CreatedDirectoryExact(_)
                                            | T::CreatedDirectoryBeforeChildrenExact(_)
                                    )
                                );
                            }
                        }
                    }
                    if let Some(issue) = metadata_transfer.as_ref().and_then(|transfer| {
                        directory_stream_observation_issue(
                            &child_str,
                            transfer,
                            metadata.observe_source_named_streams,
                        )
                    }) {
                        plan.metadata_report.issues.push(issue);
                    }
                    plan.bytes_to_transfer = plan
                        .bytes_to_transfer
                        .saturating_add(planned_directory_stream_bytes(metadata_transfer.as_ref()));
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
            hardlinks.selected_files = hardlinks.selected_files.saturating_add(1).min(2);
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
                            // Der bereits korrekte Alias benötigt keine Aktion,
                            // behält aber seine konkrete Metadatenauslassung.
                            plan_metadata_transfer(
                                src,
                                &src_root.join(&child_rel),
                                MetadataTarget {
                                    vfs: dst,
                                    path: &dst_root.join(&child_rel),
                                    existing: true,
                                    relative: &child_str,
                                },
                                metadata.for_entry(se),
                                Some(*group_id),
                                hardlinks,
                            )?;
                            continue;
                        }
                    }
                }
                let source_path = src_root.join(&child_rel);
                let target_path = dst_root.join(&child_rel);
                let source_hash = plan_file_hash(src, &source_path, se, opts, plan, cancel)?
                    .ok_or_else(|| io::Error::other("Hardlink-Plan benötigt ContentHash"))?;
                let target_hash = de
                    .map(|entry| plan_file_hash(dst, &target_path, entry, opts, plan, cancel))
                    .transpose()?
                    .flatten();
                if let Some(target) = de {
                    let equal = target_hash.as_deref() == Some(source_hash.as_str());
                    if !equal && !should_update(se, target, opts, false) {
                        if opts.mode == SyncMode::Update
                            && (ordinary_content || se.mtime.is_none() || target.mtime.is_none())
                        {
                            push_planned_action(
                                plan,
                                Action::SkipConflict { path: child_str },
                                Some(se),
                                Some(target),
                            );
                            plan.skip_count = plan.skip_count.saturating_add(1);
                        }
                        continue;
                    }
                }
                let metadata_transfer = plan_metadata_transfer(
                    src,
                    &source_path,
                    MetadataTarget {
                        vfs: dst,
                        path: &target_path,
                        existing: de.is_some(),
                        relative: &child_str,
                    },
                    metadata.for_entry(se),
                    Some(*group_id),
                    hardlinks,
                )?
                .ok_or_else(|| io::Error::other("Hardlink-Plan benötigt Metadatenmanifest"))?;
                hardlinks.observe_manifest_facts(&child_str, &metadata_transfer.source);
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
                            cancel,
                        },
                        plan,
                    )?;
                    let mut metadata_transfer = plan_metadata_transfer(
                        src,
                        &source_path,
                        MetadataTarget {
                            vfs: dst,
                            path: &target_path,
                            existing: true,
                            relative: &child_str,
                        },
                        metadata.for_entry(se),
                        group_id,
                        hardlinks,
                    )?;
                    if let Some(transfer) = &metadata_transfer {
                        bind_metadata_observation(plan, hardlinks, &child_str, transfer)?;
                    }
                    let metadata_changed = metadata_transfer
                        .as_ref()
                        .is_some_and(metadata_transfer_differs);
                    let metadata_only = comparison.equal && metadata_changed;
                    if file_dacl_ready && comparison.equal && !metadata_changed {
                        if let Some(file_plan) = plan_existing_file_dacl(
                            dst,
                            dst_root,
                            &child_str,
                            de_item.expect("Gelistete Zieldatei"),
                            metadata_transfer.as_mut(),
                            comparison.target_hash.as_deref(),
                        )? {
                            // Die DACL ist selbst die angeforderte Abweichung. Auch
                            // bei gleichem Inhalt braucht Update eine bekannte Zeitordnung.
                            if !should_update(se, d, opts, false) {
                                push_planned_action(
                                    plan,
                                    Action::SkipConflict {
                                        path: child_str.clone(),
                                    },
                                    Some(se),
                                    Some(d),
                                );
                                plan.skip_count = plan.skip_count.saturating_add(1);
                                continue;
                            }
                            if let Some(transfer) = &metadata_transfer {
                                plan.metadata_report
                                    .issues
                                    .extend(sparse_metadata_issues(&child_str, transfer, false));
                            }
                            push_planned_action(
                                plan,
                                Action::SetFileDacl {
                                    path: child_str.clone(),
                                    source_hash: comparison.source_hash.clone().ok_or_else(
                                        || io::Error::other("Datei-DACL benötigt ContentHash"),
                                    )?,
                                    source_metadata: metadata_transfer
                                        .as_ref()
                                        .unwrap()
                                        .source
                                        .clone(),
                                    plan: file_plan,
                                },
                                Some(se),
                                Some(d),
                            );
                            plan.metadata_count = plan.metadata_count.saturating_add(1);
                            continue;
                        }
                    }
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
                        if let Some(transfer) = &mut metadata_transfer {
                            if matches!(
                                transfer.windows_dacl,
                                Some(crate::windows_dacl::WindowsDaclTransfer::Exact(_))
                            ) {
                                transfer.acl = false;
                                transfer.windows_dacl =
                                    Some(crate::windows_dacl::WindowsDaclTransfer::Omitted(
                                        crate::windows_dacl::WindowsDaclOmission::AclOnlyChange,
                                    ));
                            }
                        }
                        if let Some(transfer) = &metadata_transfer {
                            plan.metadata_report
                                .issues
                                .extend(observed_metadata_issues(&child_str, transfer, false));
                        }
                        if metadata_transfer.as_ref().is_some_and(|transfer| {
                            !transfer.named_streams && observed_named_streams_differ(transfer)
                        }) {
                            plan.metadata_report.issues.push(MetadataIssue {
                                path: child_str.clone(),
                                class: MetadataClass::NamedStreams,
                                kind: MetadataIssueKind::Unsupported,
                                reason: "Der Zieladapter bietet keinen geprüften Schreibzugriff auf benannte Datenströme. Der gleiche Hauptinhalt wird nicht erneut kopiert; vorhandene Zielströme bleiben unverändert".into(),
                            });
                        }
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
                    } else if opts.mode == SyncMode::Update
                        && (ordinary_content || se.mtime.is_none() || d.mtime.is_none())
                    {
                        // Ein abweichender Inhalt ohne zeitliche Versionsordnung
                        // ist eine offene Entscheidung, kein unverändertes Paar.
                        push_planned_action(
                            plan,
                            Action::SkipConflict {
                                path: child_str.clone(),
                            },
                            Some(se),
                            Some(d),
                        );
                        plan.skip_count = plan.skip_count.saturating_add(1);
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
                        plan_file_hash(src, &src_root.join(&child_rel), se, opts, plan, cancel)?;
                    let metadata_transfer = plan_metadata_transfer(
                        src,
                        &src_root.join(&child_rel),
                        MetadataTarget {
                            vfs: dst,
                            path: &dst_root.join(&child_rel),
                            existing: false,
                            relative: &child_str,
                        },
                        metadata.for_entry(se),
                        group_id,
                        hardlinks,
                    )?;
                    if let Some(transfer) = &metadata_transfer {
                        hardlinks.observe_manifest_facts(&child_str, &transfer.source);
                    }
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
            check_cancel(cancel)?;
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
            if !exclusions.is_empty()
                || (request.ordinary_content
                    && de.is_dir
                    && dst.best_effort_permanent_delete_blocker(true).is_some())
            {
                let node = scan_target_only_node(
                    dst,
                    dst_root,
                    child_rel,
                    de_item.clone(),
                    exclusions,
                    cancel,
                )?;
                plan_target_only_node(
                    dst,
                    dst_root,
                    opts,
                    &node,
                    plan,
                    cancel,
                    request.ordinary_content,
                )?;
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
                let target_hash = plan_file_hash(
                    dst,
                    &dst_root.join(rel).join(&de.name),
                    de,
                    opts,
                    plan,
                    cancel,
                )?;
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
    cancel: &crate::Cancel,
) -> io::Result<TargetOnlyNode> {
    check_cancel(cancel)?;
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
            let child =
                scan_target_only_node(dst, dst_root, child_relative, child, exclusions, cancel)?;
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
    cancel: &crate::Cancel,
    ordinary_content: bool,
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
        let unavailable =
            ordinary_content && dst.best_effort_permanent_delete_blocker(true).is_some();
        if node.contains_excluded || unavailable {
            for child in &node.children {
                plan_target_only_node(dst, dst_root, opts, child, plan, cancel, ordinary_content)?;
            }
            if unavailable {
                push_planned_action(plan, Action::SkipUnsupported { path }, None, Some(entry));
                plan.skip_count = plan.skip_count.saturating_add(1);
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
        let target_hash = plan_file_hash(
            dst,
            &dst_root.join(&node.relative),
            entry,
            opts,
            plan,
            cancel,
        )?;
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
    cancel: &'a crate::Cancel,
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
        cancel,
    } = request;
    match opts.compare {
        Compare::SizeAndTime => Ok(FileComparison {
            equal: src_entry.size == dst_entry.size && times_equal(src_entry, dst_entry, opts),
            source_hash: None,
            target_hash: None,
        }),
        Compare::ContentHash => {
            let source_hash = hash_file_hex_cancellable(src, src_path, cancel)?;
            let target_hash = hash_file_hex_cancellable(dst, dst_path, cancel)?;
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
    cancel: &crate::Cancel,
) -> io::Result<Option<String>> {
    if opts.compare != Compare::ContentHash {
        return Ok(None);
    }
    plan.hashed_files = plan.hashed_files.saturating_add(1);
    plan.bytes_hashed = plan.bytes_hashed.saturating_add(entry.size);
    hash_file_hex_cancellable(vfs, path, cancel).map(Some)
}

struct MetadataTarget<'a> {
    vfs: &'a dyn Vfs,
    path: &'a Path,
    existing: bool,
    relative: &'a str,
}

fn plan_metadata_transfer(
    src: &dyn Vfs,
    src_path: &Path,
    location: MetadataTarget<'_>,
    policy: MetadataPlanPolicy,
    hardlink_group: Option<u64>,
    facts: &mut SourceMetadataPlanState,
) -> io::Result<Option<MetadataTransfer>> {
    if !policy.collect {
        return Ok(None);
    }
    let dst = location.vfs;
    let dst_path = location.existing.then_some(location.path);
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
    if policy.require_named_streams
        && ((policy.observe_source_named_streams && source.named_streams.is_none())
            || (policy.observe_target_named_streams
                && target
                    .as_ref()
                    .is_some_and(|value| value.named_streams.is_none())))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "NativeStrict benötigt eine vollständige Beobachtung der als lesbar angekündigten benannten Datenströme: {}",
                src_path.display()
            ),
        ));
    }
    let incompatible_acl_model = policy.acl_incompatible && source.acl.is_some();
    let named_streams = policy.named_streams && source.named_streams.is_some();
    if policy.require_named_streams
        && !named_streams
        && (source
            .named_streams
            .as_ref()
            .is_some_and(|streams| !streams.is_empty())
            || target
                .as_ref()
                .and_then(|manifest| manifest.named_streams.as_ref())
                .is_some_and(|streams| !streams.is_empty()))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "NativeStrict kann die beobachteten benannten Datenströme nicht exakt erhalten: {}",
                src_path.display()
            ),
        ));
    }

    let directory_streams = if policy.is_directory && policy.directory_stream_updates {
        match (&source.named_streams, target.as_ref()) {
            (Some(source), target) if target.is_none_or(|value| value.named_streams.is_some()) => {
                let target = target
                    .and_then(|value| value.named_streams.as_deref())
                    .unwrap_or(&[]);
                let effects = crate::directory_streams::plan_effects(source, target)?;
                (!effects.is_empty()).then_some(crate::directory_streams::DirectoryStreamPlan {
                    effects,
                    target_proof: None,
                })
            }
            _ => None,
        }
    } else {
        None
    };

    let windows_dacl = if policy.windows_dacl {
        use crate::windows_dacl::{
            WindowsDaclOmission as Omission, WindowsDaclTransfer as Transfer,
            WindowsDaclWriteSupport as Support,
        };
        Some(if policy.is_directory {
            plan_directory_dacl(&source, target.as_ref(), dst)?
        } else if let Some(crate::metadata::Acl::WindowsDacl(acl)) = &source.acl {
            match dst.windows_dacl_write_support()? {
                Support::Ready(context) => {
                    context.validate()?;
                    match acl.file_write_issue(context.namespace, &context.owner) {
                        Some(issue) => Transfer::Omitted(Omission::Source(issue)),
                        None => Transfer::Exact(context),
                    }
                }
                Support::Unavailable(issue) => Transfer::Omitted(Omission::Target(issue)),
            }
        } else {
            Transfer::Omitted(Omission::MissingObservation)
        })
    } else {
        None
    };
    let acl = policy.acl
        || matches!(
            windows_dacl,
            Some(
                crate::windows_dacl::WindowsDaclTransfer::Exact(_)
                    | crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(_)
                    | crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryExact(_)
                    | crate::windows_dacl::WindowsDaclTransfer::CreatedDirectoryBeforeChildrenExact(
                        _
                    )
            )
        );
    let mut sparse_extents = policy.sparse_extents && source.sparse_extents.is_some();
    if sparse_extents {
        let extents = source
            .sparse_extents
            .as_deref()
            .expect("Vollständige Sparse-Menge");
        let size = policy.source_size.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Sparse-Datei besitzt keine gebundene logische Größe",
            )
        })?;
        super::sparse::aligned(extents, size, 1)?;
        if super::sparse::has_holes(extents, size) {
            let parent = location.path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Sparse-Ziel besitzt keinen Elternordner",
                )
            })?;
            if let Some(unit) = dst.sparse_allocation_unit(parent)? {
                if !super::sparse::aligned(extents, size, unit)? {
                    if !policy.allow_sparse_layout_omission {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "Die Sparse-Quellgrenzen sind am lokalen Ziel nicht exakt darstellbar",
                        ));
                    }
                    sparse_extents = false;
                    facts.layout_issues.insert(
                        location.relative.into(),
                        SparseLayoutObservation {
                            unit,
                            source_digest: source.clone().with_hardlink_group(None)?.digest,
                            target_digest: target.as_ref().map(|value| value.digest),
                        },
                    );
                }
            }
        }
    }
    Ok(Some(MetadataTransfer {
        source,
        target,
        owner: policy.owner,
        acl,
        incompatible_acl_model,
        windows_dacl,
        normal_mode: policy.normal_mode,
        special_mode_bits: policy.special_mode_bits,
        xattrs: policy.xattrs,
        resource_fork: policy.resource_fork,
        named_streams,
        directory_streams,
        sparse_extents,
        hardlinks: policy.hardlinks && hardlink_group.is_some(),
    }))
}

fn plan_directory_dacl(
    source: &crate::metadata::MetadataManifest,
    target: Option<&crate::metadata::MetadataManifest>,
    dst: &dyn Vfs,
) -> io::Result<crate::windows_dacl::WindowsDaclTransfer> {
    use crate::windows_dacl::{
        WindowsDaclOmission as Omission, WindowsDaclTransfer as Transfer,
        WindowsDaclWriteSupport as Support,
    };
    let Some(crate::metadata::Acl::WindowsDacl(after)) = &source.acl else {
        return Ok(Transfer::Omitted(Omission::MissingObservation));
    };
    if let Some(owner) = source
        .windows_security
        .as_ref()
        .and_then(|value| value.owner.as_ref())
    {
        if let Some(issue) = after
            .directory_write_issue(after.namespace, owner)
            .filter(|issue| {
                !matches!(
                    issue,
                    crate::windows_security::WindowsDaclWriteIssue::MissingOwnerAccess
                        | crate::windows_security::WindowsDaclWriteIssue::RelativeTargetOwner
                        | crate::windows_security::WindowsDaclWriteIssue::InvalidTargetOwner
                )
            })
        {
            return Ok(Transfer::Omitted(Omission::Source(issue)));
        }
    }
    let Some(target) = target else {
        return Ok(Transfer::Omitted(Omission::Directory));
    };
    let Some(crate::metadata::Acl::WindowsDacl(before)) = &target.acl else {
        return Ok(Transfer::Omitted(Omission::MissingTargetObservation));
    };
    // Ein lesbarer Ziel-Owner ist Voraussetzung, keine Erlaubnis ihn zu setzen.
    let Some(security) = &target.windows_security else {
        return Ok(Transfer::Omitted(Omission::MissingTargetObservation));
    };
    let Some(owner) = &security.owner else {
        return Ok(Transfer::Omitted(Omission::MissingTargetObservation));
    };
    if let Some(issue) = after.directory_write_issue(before.namespace, owner) {
        return Ok(Transfer::Omitted(Omission::Source(issue)));
    }
    match dst.directory_dacl_write_support()? {
        Support::Unavailable(issue) => Ok(Transfer::Omitted(Omission::Target(issue))),
        Support::Ready(context) => {
            context.validate()?;
            if security.issue.is_some()
                || security.namespace != context.namespace
                || security.owner.as_ref() != Some(&context.owner)
                || security.group != context.group
            {
                return Ok(Transfer::Omitted(Omission::Target(
                    crate::windows_dacl::WindowsDaclTargetIssue::DirectoryContextMismatch,
                )));
            }
            if let Some(issue) = before.directory_write_issue(context.namespace, &context.owner) {
                return Ok(Transfer::Omitted(Omission::TargetDacl(issue)));
            }
            if before == after {
                return Ok(Transfer::Omitted(Omission::Directory));
            }
            Ok(Transfer::DirectoryExact(Box::new(
                crate::directory_dacl::DirectoryDaclPlan {
                    before: before.clone(),
                    after: after.clone(),
                    context,
                    target_proof: None,
                },
            )))
        }
    }
}

fn metadata_transfer_differs(transfer: &MetadataTransfer) -> bool {
    let Some(target) = &transfer.target else {
        return true;
    };
    transfer.directory_streams.is_some()
        || matches!(
            transfer.windows_dacl,
            Some(crate::windows_dacl::WindowsDaclTransfer::DirectoryExact(_))
        )
        || (transfer.owner && transfer.source.owner != target.owner)
        || (transfer.acl && transfer.windows_dacl.is_none() && transfer.source.acl != target.acl)
        || (transfer.normal_mode && transfer.source.mode & 0o777 != target.mode & 0o777)
        || (transfer.special_mode_bits && transfer.source.mode & 0o7000 != target.mode & 0o7000)
        || (transfer.xattrs && transfer.source.xattrs != target.xattrs)
        || (transfer.resource_fork && transfer.source.resource_fork != target.resource_fork)
        || (transfer.named_streams && transfer.source.named_streams != target.named_streams)
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
    let Some(transfer) = transfer else {
        return 0;
    };
    let fork_bytes = if transfer.resource_fork {
        transfer
            .source
            .resource_fork
            .as_ref()
            .map_or(0, |fork| fork.size)
    } else {
        0
    };
    transfer
        .source
        .named_streams
        .as_ref()
        .filter(|_| transfer.named_streams)
        .into_iter()
        .flatten()
        .fold(fork_bytes, |bytes, stream| {
            bytes.saturating_add(stream.value.size)
        })
}

/// Liefert den anzuwendenden Quellmodus nur dann, wenn beide Provider genügend
/// Information liefern, das Ziel Rechte überhaupt schreiben kann und sich die
/// Rechte tatsächlich unterscheiden.
///
/// Ohne die Fähigkeitsprüfung entstand eine Aktion, die nie ausgeführt werden
/// konnte: Eine Zielansicht ohne Rechte-Schreibvertrag kann trotzdem `mode: None`
/// listen — gegen einen lokalen Quellmodus wäre das stets ein Unterschied.
/// Private Inhaltssync-Ansichten dürfen normale Rechte dagegen wirkungsbezogen
/// versuchen und konkrete technische Auslassungen im Ergebnis bilanzieren. Die Vorschau versprach dann eine Rechteanpassung, die
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
    cancel: &crate::Cancel,
) -> io::Result<(u64, u64)> {
    check_cancel(cancel)?;
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
            let (child_count, child_bytes) =
                measure_contents(vfs, root, &child_rel, exclusions, cancel)?;
            count = count.saturating_add(child_count);
            bytes = bytes.saturating_add(child_bytes);
        } else if entry.is_file {
            bytes = bytes.saturating_add(entry.size);
        }
    }
    Ok((count, bytes))
}

fn check_cancel(cancel: &crate::Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "Sync-Planung abgebrochen",
        ))
    } else {
        Ok(())
    }
}
fn hash_file_hex_cancellable(
    vfs: &dyn Vfs,
    path: &Path,
    cancel: &crate::Cancel,
) -> io::Result<String> {
    use std::io::Read;
    check_cancel(cancel)?;
    let mut reader = vfs.open_read(path)?;
    let mut buffer = vec![0u8; crate::atomic_file::COPY_BUFFER_BYTES];
    let mut hasher = blake3::Hasher::new();
    loop {
        check_cancel(cancel)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub(super) fn planned_directory_stream_bytes(transfer: Option<&MetadataTransfer>) -> u64 {
    transfer
        .and_then(|value| value.directory_streams.as_ref())
        .map_or(0, |plan| {
            plan.effects.iter().fold(0u64, |sum, effect| {
                sum.saturating_add(effect.transfer_bytes())
            })
        })
}

// Eine lesende Bilanz ist Teil des bestätigten Plans, auch wenn keine Aktion folgt.
fn bind_metadata_observation(
    plan: &mut SyncPlan,
    hardlinks: &mut SourceMetadataPlanState,
    path: &str,
    transfer: &MetadataTransfer,
) -> io::Result<()> {
    hardlinks.observe_manifest_facts(path, &transfer.source);
    if !transfer.hardlinks {
        if let Some(group) = transfer.source.hardlink_group {
            hardlinks.omitted_groups.insert(path.into(), group);
        }
    }
    if transfer.source.hardlink_group.is_some()
        || transfer.source.sparse_read_issue.is_some()
        || transfer.source.reported_hard_links.is_some()
        || transfer
            .target
            .as_ref()
            .is_some_and(|value| value.reported_hard_links.is_some())
        || has_windows_security_context(transfer)
        || transfer.source.webdav_security.is_some()
        || transfer
            .target
            .as_ref()
            .is_some_and(|value| value.webdav_security.is_some())
    {
        plan.metadata_observations.push(super::MetadataObservation {
            path: path.into(),
            source_digest: transfer.source.clone().with_hardlink_group(None)?.digest,
            target_digest: transfer.target.as_ref().map(|value| value.digest),
        });
    }
    Ok(())
}

fn plan_existing_file_dacl(
    dst: &dyn Vfs,
    dst_root: &Path,
    path: &str,
    target: &ProofListEntry,
    transfer: Option<&mut MetadataTransfer>,
    content_hash: Option<&str>,
) -> io::Result<Option<crate::file_dacl::FileDaclPlan>> {
    use crate::metadata::Acl;
    use crate::windows_dacl::{
        WindowsDaclOmission as Omission, WindowsDaclTransfer as Transfer,
        WindowsDaclWriteSupport as Support,
    };
    let Some(transfer) = transfer else {
        return Ok(None);
    };
    let Some(Acl::WindowsDacl(after)) = &transfer.source.acl else {
        return Ok(None);
    };
    let candidate =
        (|| -> io::Result<Result<Option<crate::file_dacl::FileDaclPlan>, Omission>> {
            let Some(before_manifest) = &transfer.target else {
                return Ok(Err(Omission::MissingTargetObservation));
            };
            let Some(Acl::WindowsDacl(before)) = &before_manifest.acl else {
                return Ok(Err(Omission::MissingTargetObservation));
            };
            if before == after {
                let inplace_supported = match (
                    &transfer.source.windows_security,
                    &before_manifest.windows_security,
                ) {
                    (Some(source), Some(target)) => {
                        source.issue.is_none()
                            && target.issue.is_none()
                            && source.namespace == target.namespace
                            && source.owner == target.owner
                            && source.group == target.group
                            && target.owner.as_ref().is_some_and(|owner| {
                                after
                                    .file_inplace_write_issue(target.namespace, owner)
                                    .is_none()
                            })
                    }
                    _ => false,
                };
                // Ein bereits gleicher, gebundener Inplace-Zustand braucht keinen
                // Temp-Writer. Dessen Formatabsage wäre hier eine falsche Auslassung.
                if inplace_supported
                    && matches!(
                        transfer.windows_dacl,
                        Some(Transfer::Omitted(Omission::Source(_)))
                    )
                {
                    transfer.windows_dacl = None;
                }
                return Ok(Ok(None));
            }
            let Some(source_security) = &transfer.source.windows_security else {
                return Ok(Err(Omission::MissingObservation));
            };
            let Some(security) = &before_manifest.windows_security else {
                return Ok(Err(Omission::MissingTargetObservation));
            };
            if source_security.issue.is_some() {
                return Ok(Err(Omission::MissingObservation));
            }
            if security.issue.is_some() {
                return Ok(Err(Omission::MissingTargetObservation));
            }
            let Some(owner) = &security.owner else {
                return Ok(Err(Omission::MissingTargetObservation));
            };
            if source_security.namespace != security.namespace {
                return Ok(Err(Omission::Source(
                    crate::windows_security::WindowsDaclWriteIssue::NamespaceMismatch,
                )));
            }
            if source_security.owner != security.owner || source_security.group != security.group {
                return Ok(Err(Omission::FileSidContextMismatch));
            }
            let (Some(source_streams), Some(streams)) = (
                &transfer.source.named_streams,
                &before_manifest.named_streams,
            ) else {
                return Ok(Err(Omission::FileStreamsUnobserved));
            };
            if source_streams != streams {
                return Ok(Err(Omission::FileStreamMismatch));
            }
            if let Some(issue) = before.file_inplace_write_issue(security.namespace, owner) {
                return Ok(Err(Omission::TargetDacl(issue)));
            }
            if let Some(issue) = after.file_inplace_write_issue(security.namespace, owner) {
                return Ok(Err(Omission::Source(issue)));
            }
            match dst.file_dacl_write_support()? {
                Support::Unavailable(issue) => return Ok(Err(Omission::Target(issue))),
                Support::Ready(context) if context.namespace != security.namespace => {
                    return Ok(Err(Omission::Source(
                        crate::windows_security::WindowsDaclWriteIssue::NamespaceMismatch,
                    )))
                }
                _ => {}
            }
            let Some(file_id) = target.entry.inode.filter(|id| *id != 0) else {
                return Ok(Err(Omission::FileTargetIdentityMissing));
            };
            let Some(digest) = content_hash.and_then(|hash| blake3::Hash::from_hex(hash).ok())
            else {
                return Ok(Err(Omission::FileTargetIdentityMissing));
            };
            let identity = dst.root_identity(dst_root)?;
            let plan = crate::file_dacl::FileDaclPlan {
                before: before.clone(),
                after: after.clone(),
                context: crate::windows_dacl::WindowsDaclWriteContext {
                    namespace: security.namespace,
                    owner: owner.clone(),
                    group: security.group.clone(),
                },
                target_proof: super::SyncTargetProof::from_listed(target)?,
                file_id,
                object_namespace: crate::file_dacl_registry::object_namespace(&identity)?,
                content: crate::metadata::BlobSummary {
                    size: target.entry.size,
                    digest: *digest.as_bytes(),
                },
                streams: streams.clone(),
                alias_scope: crate::file_dacl::FileDaclAliasScope {
                    selected_paths: vec![path.into()],
                    observed_link_count: dst.stat(&dst_root.join(path))?.hard_links,
                    all_existing_and_future_names_including_outside_root: true,
                },
            };
            plan.validate()?;
            Ok(Ok(Some(plan)))
        })()?;
    match candidate {
        Ok(plan) => Ok(plan),
        Err(reason) => {
            transfer.acl = false;
            transfer.windows_dacl = Some(Transfer::Omitted(reason));
            Ok(None)
        }
    }
}

/// Gleiche File-IDs gelten nur im gebundenen Server-/Share-Namensraum. Ein Objekt
/// bekommt genau einen SET; widersprüchliche Sollwerte werden vor Vorschau gesperrt.
fn deduplicate_file_dacl_actions(plan: &mut SyncPlan) {
    let mut groups = std::collections::BTreeMap::<([u8; 32], u64), Vec<usize>>::new();
    for (index, action) in plan.actions.iter().enumerate() {
        if let Action::SetFileDacl { plan: effect, .. } = action {
            groups
                .entry((effect.object_namespace, effect.file_id))
                .or_default()
                .push(index);
        }
    }
    let mut remove = std::collections::BTreeSet::new();
    let mut missing_conflicts = Vec::new();
    for (key, indexes) in &groups {
        let Action::SetFileDacl { plan: first, .. } = &plan.actions[indexes[0]] else {
            unreachable!()
        };
        let selections = plan
            .file_dacl_selections
            .iter()
            .filter(|selection| (selection.object_namespace, selection.file_id) == *key)
            .collect::<Vec<_>>();
        let mut paths = selections
            .iter()
            .map(|selection| selection.path.clone())
            .collect::<Vec<_>>();
        paths.extend(
            indexes
                .iter()
                .filter_map(|index| match &plan.actions[*index] {
                    Action::SetFileDacl { path, .. } => Some(path.clone()),
                    _ => None,
                }),
        );
        paths.sort();
        paths.dedup();
        let conflict = indexes.iter().any(|index| match &plan.actions[*index] {
            Action::SetFileDacl { plan: other, .. } => {
                other.after != first.after
                    || other.before != first.before
                    || other.context != first.context
                    || other.content != first.content
                    || other.streams != first.streams
            }
            _ => false,
        }) || selections.iter().any(|selection| {
            selection
                .source_metadata
                .as_ref()
                .and_then(|manifest| manifest.acl.as_ref())
                != Some(&crate::metadata::Acl::WindowsDacl(first.after.clone()))
        }) || plan.actions.iter().any(|action| match action {
            Action::UpdateFile { path, .. }
            | Action::CreateHardlink { path, .. }
            | Action::SetMode { path, .. }
            | Action::SkipConflict { path }
            | Action::SkipSymlink { path }
            | Action::SkipUnsupported { path } => paths.contains(path),
            _ => false,
        });
        if conflict {
            for path in &paths {
                plan.metadata_report.issues.push(MetadataIssue{path:path.clone(),class:MetadataClass::Acl,kind:MetadataIssueKind::Unsupported,reason:"Ausgewählte Namen desselben SMB-Dateiobjekts verlangen widersprüchliche Rechte oder besitzen eine andere geplante Wirkung; die objektweite Rechteänderung bleibt aus".into()});
                let mut effect_found = false;
                for index in indexes {
                    if matches!(&plan.actions[*index],Action::SetFileDacl{path:effect_path,..} if effect_path==path)
                    {
                        plan.actions[*index] = Action::SkipConflict { path: path.clone() };
                        plan.skip_count = plan.skip_count.saturating_add(1);
                        plan.metadata_count = plan.metadata_count.saturating_sub(1);
                        effect_found = true;
                        if let Some(row) = plan
                            .comparison
                            .rows
                            .iter_mut()
                            .find(|row| row.path == *path)
                        {
                            row.action = ComparisonAction::Skip;
                            row.affected_side = ComparisonSide::Neither;
                        }
                        plan.comparison.categories.updates =
                            plan.comparison.categories.updates.saturating_sub(1);
                        plan.comparison.categories.skipped =
                            plan.comparison.categories.skipped.saturating_add(1);
                    }
                }
                if !effect_found && !plan.comparison.rows.iter().any(|row| row.path == *path) {
                    missing_conflicts.push(path.clone());
                }
            }
        } else {
            let Action::SetFileDacl { plan: effect, .. } = &mut plan.actions[indexes[0]] else {
                unreachable!()
            };
            effect.alias_scope.selected_paths = paths;
            for index in &indexes[1..] {
                remove.insert(*index);
                plan.metadata_count = plan.metadata_count.saturating_sub(1);
            }
        }
    }
    let mut index = 0;
    plan.actions.retain(|_| {
        let keep = !remove.contains(&index);
        index += 1;
        keep
    });
    for path in missing_conflicts {
        push_planned_action(plan, Action::SkipConflict { path }, None, None);
        plan.skip_count = plan.skip_count.saturating_add(1);
    }
    if !plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::SetFileDacl { .. }))
    {
        plan.file_dacl_selections.clear();
    }
}
