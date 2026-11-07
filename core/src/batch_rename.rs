//! Kurzlebige lokale Stapelumbenennung mit backendseitiger Namensabbildung.
//! Einzelne Namen nutzen exklusives Rename, lokale Zweierzyklen atomaren Austausch.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Cancel, Entry, LocalVfs, Vfs};

pub(crate) const PLAN_LIFETIME: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRenameOptions {
    pub find: String,
    pub replacement: String,
    pub prefix: String,
    pub suffix: String,
    #[serde(default = "default_preserve_extension")]
    pub preserve_extension: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub trim_outer_spaces: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numbering: Option<BatchRenameNumbering>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRenameNumbering {
    pub start: u32,
    pub step: u32,
    pub min_digits: u8,
    pub position: BatchRenameNumberPosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchRenameNumberPosition {
    Before,
    After,
}

impl BatchRenameNumbering {
    fn counter(&self, index: usize) -> io::Result<u64> {
        if self.step == 0 || !(1..=20).contains(&self.min_digits) {
            return Err(invalid(
                "Nummerierung benötigt einen positiven Schritt und 1 bis 20 Mindestziffern",
            ));
        }
        u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(u64::from(self.step)))
            .and_then(|offset| u64::from(self.start).checked_add(offset))
            .ok_or_else(|| {
                invalid("Der Nummerierungszähler überschreitet den darstellbaren Wertebereich")
            })
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_preserve_extension() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchRenameStatus {
    Ready,
    Unchanged,
    Blocked,
    Renamed,
    PartiallyRenamed,
    FailedUnchanged,
    OutcomeUnclear,
    Cancelled,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRenameRow {
    pub old_name: String,
    pub new_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_name: Option<String>,
    pub status: BatchRenameStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<crate::ErrorCode>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRenameReport {
    pub rows: Vec<BatchRenameRow>,
    pub renamed: usize,
    pub partially_renamed: usize,
    pub failed_unchanged: usize,
    pub outcome_unclear: usize,
    pub cancelled: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_class: Option<crate::RenameExecutionClass>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
}

pub struct PreparedBatchRename {
    root: PathBuf,
    vfs: LocalVfs,
    root_identity: crate::vfs::LocalRootIdentity,
    sandbox: Option<PathBuf>,
    rows: Vec<BatchRenameRow>,
    proofs: Vec<Option<Entry>>,
    execution_order: Vec<usize>,
    dependencies: Vec<Vec<usize>>,
    exchange_partners: Vec<Option<usize>>,
    exchange_cycles: Vec<Vec<usize>>,
    cycle_by_row: Vec<Option<usize>>,
    created: Instant,
}

struct CycleResult {
    locations: Vec<String>,
    attempted: Vec<bool>,
    unknown: Vec<bool>,
    stop: Option<io::Error>,
}

enum SpellingAttempt {
    Succeeded,
    Unchanged(io::Error),
    Unclear(io::Error),
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn candidate(
    name: &str,
    is_file: bool,
    options: &BatchRenameOptions,
    index: usize,
) -> io::Result<String> {
    let name = if options.trim_outer_spaces {
        name.trim_matches(' ')
    } else {
        name
    };
    let (stem, extension) = if options.preserve_extension && is_file {
        name.rsplit_once('.')
            .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
            .map(|(stem, extension)| (stem, Some(extension)))
            .unwrap_or((name, None))
    } else {
        (name, None)
    };
    let number = options
        .numbering
        .as_ref()
        .map(|numbering| {
            let value = numbering.counter(index)?;
            Ok::<_, io::Error>(format!(
                "{value:0width$}",
                width = usize::from(numbering.min_digits)
            ))
        })
        .transpose()?;
    let matches = if options.find.is_empty() {
        0
    } else {
        stem.matches(&options.find).count()
    };
    let length = matches
        .checked_mul(options.find.len())
        .and_then(|removed| stem.len().checked_sub(removed))
        .and_then(|length| {
            matches
                .checked_mul(options.replacement.len())
                .and_then(|added| length.checked_add(added))
        })
        .and_then(|length| length.checked_add(options.prefix.len()))
        .and_then(|length| length.checked_add(options.suffix.len()))
        .and_then(|length| length.checked_add(number.as_ref().map_or(0, |number| number.len() + 1)))
        .and_then(|length| {
            length.checked_add(extension.map_or(0, |extension| extension.len() + 1))
        });
    if length.is_none_or(|length| length > 255) {
        return Err(invalid(
            "Der neue Name überschreitet 255 UTF-8-Bytes; er wurde nicht vollständig aufgebaut",
        ));
    }
    let replaced = if options.find.is_empty() {
        stem.to_owned()
    } else {
        stem.replace(&options.find, &options.replacement)
    };
    let mut result = match (&options.numbering, &number) {
        (Some(numbering), Some(number))
            if numbering.position == BatchRenameNumberPosition::Before =>
        {
            format!("{number}_{}{replaced}{}", options.prefix, options.suffix)
        }
        (Some(_), Some(number)) => {
            format!("{}{replaced}{}_{number}", options.prefix, options.suffix)
        }
        _ => format!("{}{replaced}{}", options.prefix, options.suffix),
    };
    if let Some(extension) = extension {
        result.push('.');
        result.push_str(extension);
    }
    Ok(result)
}

pub(crate) fn same_source(left: &Entry, right: &Entry) -> bool {
    left.device == right.device
        && left.inode == right.inode
        && left.is_dir == right.is_dir
        && left.is_file == right.is_file
        && left.is_symlink == right.is_symlink
        && left.size == right.size
        && left.mtime == right.mtime
        && left.mode == right.mode
}

pub(crate) fn set_status(
    row: &mut BatchRenameRow,
    status: BatchRenameStatus,
    message: impl Into<String>,
) {
    row.code = match status {
        BatchRenameStatus::Blocked => Some(crate::ErrorCode::InvalidInput),
        BatchRenameStatus::FailedUnchanged => Some(crate::ErrorCode::PlanDrift),
        BatchRenameStatus::OutcomeUnclear => Some(crate::ErrorCode::OutcomeUnclear),
        BatchRenameStatus::Cancelled => Some(crate::ErrorCode::Interrupted),
        _ => None,
    };
    row.current_name = None;
    row.status = status;
    row.message = message.into();
}

pub fn validate_batch_rename_request(
    names: &[String],
    options: &BatchRenameOptions,
) -> io::Result<()> {
    if names.is_empty() {
        return Err(invalid("Stapelumbenennen benötigt eine Auswahl"));
    }
    for value in [
        &options.find,
        &options.replacement,
        &options.prefix,
        &options.suffix,
    ] {
        if value.len() > 4096 {
            return Err(invalid(
                "Ein Umbenennungsfeld überschreitet 4096 UTF-8-Bytes",
            ));
        }
    }
    if let Some(numbering) = &options.numbering {
        numbering.counter(names.len() - 1)?;
    }
    Ok(())
}
pub(crate) fn plan_rows(
    listing: &crate::vfs::CompleteDirectoryListing,
    names: &[String],
    options: &BatchRenameOptions,
) -> (Vec<BatchRenameRow>, Vec<Option<Entry>>) {
    plan_rows_with_chains(listing, names, options, false)
}

pub(crate) fn plan_rows_with_chains(
    listing: &crate::vfs::CompleteDirectoryListing,
    names: &[String],
    options: &BatchRenameOptions,
    allow_chains: bool,
) -> (Vec<BatchRenameRow>, Vec<Option<Entry>>) {
    let occupied: HashSet<_> = listing
        .entries
        .iter()
        .map(|entry| crate::remote_rename::folded_name(&entry.entry.name))
        .collect();
    let mut rows = Vec::new();
    let mut proofs = Vec::new();
    let mut selected = HashSet::new();
    // Der eingefrorene Auswahlindex bleibt auch bei blockierten Zeilen erhalten.
    for (index, name) in names.iter().enumerate() {
        let mut row = BatchRenameRow {
            old_name: name.clone(),
            new_name: name.clone(),
            current_name: None,
            status: BatchRenameStatus::Ready,
            message: "Bereit".into(),
            code: None,
        };
        let safe = !name.is_empty()
            && !matches!(name.as_str(), "." | "..")
            && !name.contains('/')
            && !name.contains('\0')
            && !name.chars().any(char::is_control)
            && selected.insert(name.clone());
        let proof = if safe {
            listing
                .entries
                .iter()
                .find(|entry| entry.safely_decoded && entry.entry.name == *name)
                .map(|entry| entry.entry.clone())
        } else {
            None
        };
        if let Some(proof) = &proof {
            row.new_name = match candidate(name, proof.is_file, options, index) {
                Ok(name) => name,
                Err(error) => {
                    row.new_name = "—".into();
                    set_status(&mut row, BatchRenameStatus::Blocked, error.to_string());
                    rows.push(row);
                    proofs.push(Some(proof.clone()));
                    continue;
                }
            };
            if proof.is_symlink || (!proof.is_file && !proof.is_dir) {
                set_status(
                    &mut row,
                    BatchRenameStatus::Blocked,
                    "Symlinks und Spezialdateien werden in diesem Stapel nicht umbenannt",
                );
            } else if row.new_name == row.old_name {
                set_status(
                    &mut row,
                    BatchRenameStatus::Unchanged,
                    "Name bleibt unverändert",
                );
            } else if !crate::validate_file_name(&row.new_name)
                .is_ok_and(|validated| validated == row.new_name)
                || row.new_name.len() > 255
                || row.new_name.chars().any(char::is_control)
            {
                set_status(&mut row, BatchRenameStatus::Blocked, "Das Ergebnis ist kein gültiger Dateiname (höchstens 255 UTF-8-Bytes, ohne Steuerzeichen oder Pfadtrenner)");
            } else if !allow_chains
                && occupied.contains(&crate::remote_rename::folded_name(&row.new_name))
            {
                set_status(&mut row, BatchRenameStatus::Blocked, "Der Zielname ist bereits belegt oder unterscheidet sich nur in Groß-/Kleinschreibung beziehungsweise Unicode-Schreibweise. Namenstausch und Ketten werden nicht ausgeführt");
            }
        } else {
            set_status(&mut row, BatchRenameStatus::Blocked, "Auswahl fehlt im frischen Ordnerbestand, ist doppelt oder besitzt keinen eindeutig bedienbaren Namen");
        }
        rows.push(row);
        proofs.push(proof);
    }
    let mut target_counts = HashMap::new();
    for row in rows
        .iter()
        .filter(|row| row.status == BatchRenameStatus::Ready)
    {
        *target_counts
            .entry(crate::remote_rename::folded_name(&row.new_name))
            .or_insert(0_usize) += 1;
    }
    for row in &mut rows {
        if row.status == BatchRenameStatus::Ready
            && target_counts[&crate::remote_rename::folded_name(&row.new_name)] > 1
        {
            set_status(
                row,
                BatchRenameStatus::Blocked,
                "Mehrere ausgewählte Einträge erhalten denselben oder einen äquivalenten Zielnamen",
            );
        }
    }
    (rows, proofs)
}
// Ketten geben belegte Ziele durch bestätigte Vorgänger frei.
// Die jeweilige Ausführung muss Abhängigkeiten und eigene Identitätsbelege prüfen.
pub(crate) fn chain_execution_plan(
    listing: &crate::vfs::CompleteDirectoryListing,
    rows: &mut [BatchRenameRow],
) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut occupants: HashMap<String, Vec<&str>> = HashMap::new();
    for entry in &listing.entries {
        occupants
            .entry(crate::remote_rename::folded_name(&entry.entry.name))
            .or_default()
            .push(&entry.entry.name);
    }
    let sources: HashMap<_, _> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.status == BatchRenameStatus::Ready)
        .map(|(index, row)| (row.old_name.clone(), index))
        .collect();
    let mut dependencies = vec![Vec::new(); rows.len()];
    let mut dependents = vec![Vec::new(); rows.len()];
    let mut remaining = vec![0; rows.len()];
    let mut available = VecDeque::new();
    for (index, row) in rows.iter_mut().enumerate() {
        if row.status != BatchRenameStatus::Ready {
            continue;
        }
        let target = crate::remote_rename::folded_name(&row.new_name);
        let mut blocked = false;
        if let Some(names) = occupants.get(&target) {
            for name in names {
                match sources.get(*name) {
                    Some(&source) if source != index => dependencies[index].push(source),
                    _ => blocked = true,
                }
            }
        }
        if blocked {
            set_status(row, BatchRenameStatus::Blocked, "Der Zielname ist belegt und kann nicht durch eine gültige ausgewählte Umbenennung freigemacht werden");
            continue;
        }
        remaining[index] = dependencies[index].len();
        for &source in &dependencies[index] {
            dependents[source].push(index);
        }
        if remaining[index] == 0 {
            available.push_back(index);
        }
    }
    let mut order = Vec::new();
    while let Some(index) = available.pop_front() {
        order.push(index);
        for &dependent in &dependents[index] {
            remaining[dependent] -= 1;
            if remaining[dependent] == 0 {
                available.push_back(dependent);
            }
        }
    }
    let source_names: Vec<_> = rows.iter().map(|row| row.old_name.clone()).collect();
    // Ein Zyklus hat keinen freien Anfang. Auch von blockierten Zeilen abhängige
    // Ziele bleiben belegt; ohne Tempnamen oder Rücktausch gibt es dafür keinen Lauf.
    for (index, row) in rows.iter_mut().enumerate() {
        if row.status == BatchRenameStatus::Ready && remaining[index] > 0 {
            set_status(row, BatchRenameStatus::Blocked, "Der Zielname hängt von einer blockierten Umbenennung oder einem Namenstausch/Zyklus ab; diese Kette wird nicht ausgeführt");
        } else if row.status == BatchRenameStatus::Ready && !dependencies[index].is_empty() {
            row.message = format!(
                "Bereit; Ziel wird zuvor durch {} freigemacht",
                dependencies[index]
                    .iter()
                    .map(|&source| format!("„{}“", source_names[source]))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    (order, dependencies)
}

// Lokale Schreibweisenänderungen und exakte Zweierzyklen erhalten eigene atomare
// Wirkungen. Der gemeinsame Remote-Planer behält seine bisherigen Sperren.
fn local_execution_plan(
    listing: &crate::vfs::CompleteDirectoryListing,
    rows: &mut [BatchRenameRow],
) -> (Vec<usize>, Vec<Vec<usize>>, Vec<Option<usize>>) {
    let ready: HashMap<_, _> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.status == BatchRenameStatus::Ready)
        .map(|(index, row)| (row.old_name.clone(), index))
        .collect();
    let mut occupants: HashMap<String, Vec<&str>> = HashMap::new();
    for entry in &listing.entries {
        occupants
            .entry(crate::remote_rename::folded_name(&entry.entry.name))
            .or_default()
            .push(&entry.entry.name);
    }
    let unique_sources = unique_raw_sources(listing);
    let spelling_changes: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.status == BatchRenameStatus::Ready
                && is_spelling_change(row)
                && unique_sources.contains_key(row.old_name.as_str())
        })
        .map(|(index, _)| index)
        .collect();
    let mut pairs = Vec::new();
    for (&index, row) in ready.values().map(|index| (index, &rows[*index])) {
        let Some(&partner) = ready.get(&row.new_name) else {
            continue;
        };
        if index >= partner
            || rows[partner].new_name != row.old_name
            || crate::remote_rename::folded_name(&row.old_name)
                == crate::remote_rename::folded_name(&row.new_name)
        {
            continue;
        }
        if [&row.old_name, &row.new_name].iter().all(|name| {
            occupants
                .get(&crate::remote_rename::folded_name(name))
                .is_some_and(|names| names.as_slice() == [name.as_str()])
        }) {
            pairs.push((index, partner));
        }
    }
    pairs.sort_unstable();
    let (mut order, mut dependencies) = chain_execution_plan(listing, rows);
    let mut partners = vec![None; rows.len()];
    for index in spelling_changes {
        set_status(&mut rows[index], BatchRenameStatus::Ready,
            "Bereit; Schreibweise atomar ändern, sofern das Dateisystem sie zulässt. Ein No-Replace-Versuch, kein Ausweichname");
        dependencies[index].clear();
        order.push(index);
    }
    for (left, right) in pairs {
        for (index, partner) in [(left, right), (right, left)] {
            let message = format!(
                "Bereit; atomarer Namenstausch mit „{}“ in einem Schritt; kein Ausweichname",
                rows[partner].old_name
            );
            set_status(&mut rows[index], BatchRenameStatus::Ready, message);
            dependencies[index].clear();
            partners[index] = Some(partner);
            order.push(index);
        }
    }
    (order, dependencies, partners)
}

// Exakte geschlossene Zyklen besitzen keine freie Anfangsposition. Sie werden
// separat vom gemeinsamen Kettenplaner erkannt, damit Remote-Sperren gleich bleiben.
fn local_closed_cycles(
    listing: &crate::vfs::CompleteDirectoryListing,
    rows: &[BatchRenameRow],
) -> Vec<Vec<usize>> {
    let unique = unique_raw_sources(listing);
    let sources: HashMap<_, _> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.status == BatchRenameStatus::Ready
                && !is_spelling_change(row)
                && unique.contains_key(row.old_name.as_str())
        })
        .map(|(index, row)| (row.old_name.as_str(), index))
        .collect();
    let mut visited = HashSet::new();
    let mut cycles = Vec::new();
    for start in 0..rows.len() {
        let mut path = Vec::new();
        let mut positions = HashMap::new();
        let mut index = start;
        while !visited.contains(&index) {
            if let Some(&position) = positions.get(&index) {
                let cycle = &path[position..];
                let mut identities = HashSet::new();
                if cycle.len() >= 3
                    && cycle.iter().all(|&member: &usize| {
                        let entry = unique[rows[member].old_name.as_str()];
                        !entry.is_symlink
                            && (entry.is_file || entry.is_dir)
                            && entry
                                .device
                                .zip(entry.inode)
                                .is_some_and(|identity| identities.insert(identity))
                    })
                {
                    cycles.push(cycle.to_vec());
                }
                break;
            }
            if !sources.contains_key(rows[index].old_name.as_str()) {
                break;
            }
            positions.insert(index, path.len());
            path.push(index);
            let Some(&next) = sources.get(rows[index].new_name.as_str()) else {
                break;
            };
            index = next;
        }
        visited.extend(path);
    }
    cycles
}

fn is_spelling_change(row: &BatchRenameRow) -> bool {
    row.old_name != row.new_name
        && crate::remote_rename::folded_name(&row.old_name)
            == crate::remote_rename::folded_name(&row.new_name)
}

// Ein Stat kann unter einem Alias dieselbe Inode liefern. Der vollständige
// Roh-Namensbestand belegt den jeweiligen Zustand, nicht die Historie dazwischen.
// Auch ein zweiter Hardlink derselben Inode in dieser Gruppe bleibt eine Kollision.
fn unique_raw_sources(listing: &crate::vfs::CompleteDirectoryListing) -> HashMap<&str, &Entry> {
    if !listing.complete {
        return HashMap::new();
    }
    let mut groups = HashMap::new();
    for entry in &listing.entries {
        groups
            .entry(crate::remote_rename::folded_name(&entry.entry.name))
            .and_modify(|group| *group = None)
            .or_insert(Some(entry));
    }
    groups
        .values()
        .flatten()
        .filter(|entry| entry.safely_decoded && entry.raw_name == entry.entry.name.as_bytes())
        .map(|entry| (entry.entry.name.as_str(), &entry.entry))
        .collect()
}

#[cfg(test)]
fn unique_raw_source<'a>(
    listing: &'a crate::vfs::CompleteDirectoryListing,
    name: &str,
) -> Option<&'a Entry> {
    unique_raw_sources(listing).get(name).copied()
}

pub(crate) fn finish_report(
    rows: Vec<BatchRenameRow>,
    execution_class: Option<crate::RenameExecutionClass>,
    limitations: Vec<String>,
) -> BatchRenameReport {
    BatchRenameReport {
        renamed: rows
            .iter()
            .filter(|row| row.status == BatchRenameStatus::Renamed)
            .count(),
        partially_renamed: rows
            .iter()
            .filter(|row| row.status == BatchRenameStatus::PartiallyRenamed)
            .count(),
        failed_unchanged: rows
            .iter()
            .filter(|row| row.status == BatchRenameStatus::FailedUnchanged)
            .count(),
        outcome_unclear: rows
            .iter()
            .filter(|row| row.status == BatchRenameStatus::OutcomeUnclear)
            .count(),
        cancelled: rows
            .iter()
            .filter(|row| row.status == BatchRenameStatus::Cancelled)
            .count(),
        rows,
        execution_class,
        limitations,
    }
}
impl PreparedBatchRename {
    pub fn prepare(
        root: &Path,
        names: &[String],
        options: &BatchRenameOptions,
        sandbox: Option<PathBuf>,
    ) -> io::Result<Self> {
        validate_batch_rename_request(names, options)?;
        if crate::validate_file_operation_root(root)?.as_os_str() != root.as_os_str()
            || names.is_empty()
        {
            return Err(invalid("Die Stapelumbenennung benötigt einen normalisierten lokalen Pane-Root und eine Auswahl"));
        }
        if let Some(sandbox) = &sandbox {
            if !root.starts_with(sandbox) {
                return Err(invalid("Die Stapelumbenennung verlässt die Testsandbox"));
            }
        }
        let vfs = LocalVfs::rooted(root)?;
        let root_identity = vfs.local_root_identity(root)?;
        let listing = vfs.list_complete(root)?;
        if !listing.complete {
            return Err(invalid(
                "Der lokale Ordner konnte nicht vollständig gelesen werden",
            ));
        }
        let (mut rows, proofs) = plan_rows_with_chains(&listing, names, options, true);
        let exchange_cycles = local_closed_cycles(&listing, &rows);
        let (mut execution_order, mut dependencies, exchange_partners) =
            local_execution_plan(&listing, &mut rows);
        let mut cycle_by_row = vec![None; rows.len()];
        for (cycle_index, cycle) in exchange_cycles.iter().enumerate() {
            for &index in cycle {
                set_status(&mut rows[index], BatchRenameStatus::Ready, format!(
                    "Bereit; lokaler Zyklus mit {} atomaren Austauschen. Abbruch kann Teiländerungen hinterlassen; kein automatischer Rücktausch; keine automatische Wiederaufnahme nach Prozessende", cycle.len() - 1));
                dependencies[index].clear();
                cycle_by_row[index] = Some(cycle_index);
                execution_order.push(index);
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            vfs,
            root_identity,
            sandbox,
            rows,
            proofs,
            execution_order,
            dependencies,
            exchange_partners,
            exchange_cycles,
            cycle_by_row,
            created: Instant::now(),
        })
    }

    fn check_source(&self, index: usize) -> io::Result<()> {
        if LocalVfs::new().local_root_identity(&self.root)? != self.root_identity {
            return Err(crate::coded_error(
                io::ErrorKind::InvalidInput,
                crate::ErrorCode::PlanDrift,
                "Der angezeigte Pane-Root wurde seit der Vorschau ausgetauscht",
            ));
        }
        let path = self.root.join(&self.rows[index].old_name);
        crate::fileops::guard_vfs(&self.vfs, &self.sandbox, &path)?;
        let actual = self.vfs.stat(&path)?;
        if !self.proofs[index]
            .as_ref()
            .is_some_and(|proof| same_source(proof, &actual))
        {
            return Err(crate::coded_error(
                io::ErrorKind::InvalidInput,
                crate::ErrorCode::PlanDrift,
                "Die Quelle wurde seit der Vorschau verändert oder ausgetauscht",
            ));
        }
        Ok(())
    }

    fn check_cycle_location(&self, index: usize, name: &str) -> io::Result<()> {
        if LocalVfs::new().local_root_identity(&self.root)? != self.root_identity {
            return Err(crate::coded_error(
                io::ErrorKind::InvalidInput,
                crate::ErrorCode::PlanDrift,
                "Der Pane-Root des Namenszyklus wurde ausgetauscht",
            ));
        }
        let path = self.root.join(name);
        crate::fileops::guard_vfs(&self.vfs, &self.sandbox, &path)?;
        let actual = self.vfs.stat(&path)?;
        if !self.proofs[index]
            .as_ref()
            .is_some_and(|proof| same_source(proof, &actual))
        {
            return Err(crate::coded_error(io::ErrorKind::InvalidInput, crate::ErrorCode::PlanDrift,
                format!("Die erwartete Zyklusidentität unter „{name}“ wurde verändert oder ausgetauscht")));
        }
        Ok(())
    }

    fn execute_exchange_cycle<G, H>(
        &self,
        members: &[usize],
        initial_matches: &[bool],
        cancel: &Cancel,
        exchange: &mut G,
        sync: &mut H,
        on_step: &mut dyn FnMut(usize),
    ) -> CycleResult
    where
        G: FnMut(&dyn Vfs, &Path, &Path) -> io::Result<()>,
        H: FnMut(&dyn Vfs, &Path) -> io::Result<()>,
    {
        let mut result = CycleResult {
            locations: members
                .iter()
                .map(|&index| self.rows[index].old_name.clone())
                .collect(),
            attempted: vec![false; members.len()],
            unknown: vec![false; members.len()],
            stop: None,
        };
        if cancel.is_cancelled() {
            result.stop = Some(crate::coded_error(
                io::ErrorKind::Interrupted,
                crate::ErrorCode::Interrupted,
                "Vor dem Namenszyklus abgebrochen; keine Wirkung begonnen",
            ));
            return result;
        }
        if !members.iter().all(|&index| initial_matches[index]) {
            result.stop = Some(invalid(
                "Der Anfangsbestand des Zyklus ist nicht mehr exakt und eindeutig gebunden",
            ));
            return result;
        }
        let pivot = self.root.join(&self.rows[members[0]].old_name);
        for step in 1..members.len() {
            let left = step - 1;
            let right = step;
            let before = (|| -> io::Result<()> {
                if cancel.is_cancelled() {
                    return Err(crate::coded_error(
                        io::ErrorKind::Interrupted,
                        crate::ErrorCode::Interrupted,
                        "Vor dem nächsten Zyklusschritt abgebrochen; kein Rücktausch",
                    ));
                }
                if self.is_expired() {
                    return Err(invalid("Die Zyklusvorschau ist älter als zehn Minuten"));
                }
                // Am ursprünglichen Pivotnamen steht nach jedem Exchange eine
                // andere Originalidentität. Beide aktuellen Belegungen prüfen.
                self.check_cycle_location(members[left], &result.locations[left])?;
                self.check_cycle_location(members[right], &result.locations[right])?;
                if cancel.is_cancelled() {
                    return Err(crate::coded_error(
                        io::ErrorKind::Interrupted,
                        crate::ErrorCode::Interrupted,
                        "Vor dem Zyklusaustausch abgebrochen; kein Rücktausch",
                    ));
                }
                Ok(())
            })();
            if let Err(error) = before {
                result.stop = Some(error);
                break;
            }
            let other = self.root.join(&result.locations[right]);
            result.attempted[left] = true;
            result.attempted[right] = true;
            // Ein atomarer Exchange je Schritt; der Gesamtzyklus ist ausdrücklich
            // keine Transaktion. Fehler erhalten weder Ersatzrename noch Rückweg.
            // Vorprüfung und Exchange bilden keine atomare Identitätsbedingung.
            // Der Endscan belegt einen Zustand, keine exklusive eigene Historie
            // zwischenzeitlicher fremder Namespaceänderungen.
            match exchange(&self.vfs, &pivot, &other) {
                Ok(()) => {
                    let read_left =
                        self.check_cycle_location(members[left], &result.locations[right]);
                    let read_right =
                        self.check_cycle_location(members[right], &result.locations[left]);
                    let flushed = sync(&self.vfs, &self.root);
                    if let Err(error) = read_left.and(read_right).and(flushed) {
                        result.unknown[left] = true;
                        result.unknown[right] = true;
                        result.stop = Some(error);
                        on_step(step);
                        break;
                    }
                    result.locations.swap(left, right);
                }
                Err(error) => {
                    let unchanged = crate::error_code(&error) != crate::ErrorCode::OutcomeUnclear
                        && self
                            .check_cycle_location(members[left], &result.locations[left])
                            .is_ok()
                        && self
                            .check_cycle_location(members[right], &result.locations[right])
                            .is_ok();
                    if !unchanged {
                        result.unknown[left] = true;
                        result.unknown[right] = true;
                        let _ = sync(&self.vfs, &self.root);
                    }
                    result.stop = Some(error);
                    on_step(step);
                    break;
                }
            }
            on_step(step);
        }
        result
    }

    fn execute_spelling_change<F, G>(
        &self,
        index: usize,
        cancel: &Cancel,
        initial_source_matches: bool,
        rename: &mut F,
        sync: &mut G,
    ) -> io::Result<SpellingAttempt>
    where
        F: FnMut(
            &dyn Vfs,
            &Path,
            &Path,
        )
            -> Result<Option<crate::vfs::ProofListEntry>, crate::vfs::MutationFailure>,
        G: FnMut(&dyn Vfs, &Path) -> io::Result<()>,
    {
        let row = &self.rows[index];
        let old = self.root.join(&row.old_name);
        let new = self.root.join(&row.new_name);
        let cancelled = || {
            crate::coded_error(
                io::ErrorKind::Interrupted,
                crate::ErrorCode::Interrupted,
                "Vor der Schreibweisenänderung abgebrochen; unverändert",
            )
        };
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if self.is_expired() {
            return Err(invalid(
                "Die Vorschau ist älter als zehn Minuten; erneut vorbereiten",
            ));
        }
        if !initial_source_matches {
            return Err(crate::coded_error(io::ErrorKind::InvalidInput, crate::ErrorCode::PlanDrift,
                "Die eindeutige alte Rohschreibweise und ihre Identität fehlen im frischen Anfangsbestand"));
        }
        crate::fileops::guard_vfs(&self.vfs, &self.sandbox, &old)?;
        crate::fileops::guard_vfs(&self.vfs, &self.sandbox, &new)?;
        self.check_source(index)?;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        // Genau ein Kernelversuch, ohne Alias-Stat als Zielbelegung zu behandeln.
        // Die unmittelbare Identität ersetzt keinen exakten Namensbeleg: bis zum
        // gemeinsamen Abschlusslisting bleibt auch ein Kernel-Ok nur vorläufig.
        match rename(&self.vfs, &old, &new) {
            Ok(_) => {
                let actual = self.vfs.stat(&new);
                let flushed = sync(&self.vfs, &self.root);
                if !actual.as_ref().is_ok_and(|actual| {
                    self.proofs[index]
                        .as_ref()
                        .is_some_and(|proof| same_source(proof, actual))
                }) {
                    return Ok(SpellingAttempt::Unclear(io::Error::other("Die unmittelbare Zielidentität nach Rename ist verändert oder nicht lesbar")));
                }
                match flushed {
                    Ok(()) => Ok(SpellingAttempt::Succeeded),
                    Err(error) => Ok(SpellingAttempt::Unclear(io::Error::other(format!(
                        "Verzeichnis-Sync nach Rename scheiterte ({error})"
                    )))),
                }
            }
            Err(crate::vfs::MutationFailure::OutcomeUnclear(error)) => {
                let _ = sync(&self.vfs, &self.root);
                Ok(SpellingAttempt::Unclear(error))
            }
            Err(crate::vfs::MutationFailure::Unchanged(error)) => {
                if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear {
                    Ok(SpellingAttempt::Unclear(error))
                } else {
                    Ok(SpellingAttempt::Unchanged(error))
                }
            }
        }
    }

    pub fn rows(&self) -> &[BatchRenameRow] {
        &self.rows
    }
    pub fn is_expired(&self) -> bool {
        self.created.elapsed() > PLAN_LIFETIME
    }

    pub fn execute(
        self,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> BatchRenameReport {
        let listing = self.vfs.list_complete(&self.root);
        self.execute_with_listing(listing, cancel, on_progress)
    }

    fn execute_with_listing(
        self,
        listing: io::Result<crate::vfs::CompleteDirectoryListing>,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> BatchRenameReport {
        self.execute_with_rename(listing, cancel, on_progress, &mut crate::fileops::rename)
    }

    fn execute_with_rename<F>(
        self,
        listing: io::Result<crate::vfs::CompleteDirectoryListing>,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(usize, usize),
        rename: &mut F,
    ) -> BatchRenameReport
    where
        F: FnMut(&dyn Vfs, &Path, &str, &Option<PathBuf>) -> io::Result<()>,
    {
        self.execute_with_actions(
            listing,
            cancel,
            on_progress,
            rename,
            &mut |vfs, left, right| vfs.rename_exchange(left, right),
            &mut |vfs, root| vfs.sync_dir(root),
        )
    }

    fn execute_with_actions<F, G, H>(
        self,
        listing: io::Result<crate::vfs::CompleteDirectoryListing>,
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(usize, usize),
        rename: &mut F,
        exchange: &mut G,
        sync: &mut H,
    ) -> BatchRenameReport
    where
        F: FnMut(&dyn Vfs, &Path, &str, &Option<PathBuf>) -> io::Result<()>,
        G: FnMut(&dyn Vfs, &Path, &Path) -> io::Result<()>,
        H: FnMut(&dyn Vfs, &Path) -> io::Result<()>,
    {
        self.execute_with_case_actions(
            (
                listing,
                &mut |vfs, old, new| vfs.rename_noreplace_with_removal_proof(old, new),
                &mut |vfs, root| vfs.list_complete(root),
            ),
            cancel,
            on_progress,
            rename,
            exchange,
            sync,
        )
    }

    fn execute_with_case_actions<F, G, H, I, J>(
        mut self,
        case_actions: (
            io::Result<crate::vfs::CompleteDirectoryListing>,
            &mut I,
            &mut J,
        ),
        cancel: &Cancel,
        on_progress: &mut dyn FnMut(usize, usize),
        rename: &mut F,
        exchange: &mut G,
        sync: &mut H,
    ) -> BatchRenameReport
    where
        F: FnMut(&dyn Vfs, &Path, &str, &Option<PathBuf>) -> io::Result<()>,
        G: FnMut(&dyn Vfs, &Path, &Path) -> io::Result<()>,
        H: FnMut(&dyn Vfs, &Path) -> io::Result<()>,
        I: FnMut(
            &dyn Vfs,
            &Path,
            &Path,
        )
            -> Result<Option<crate::vfs::ProofListEntry>, crate::vfs::MutationFailure>,
        J: FnMut(&dyn Vfs, &Path) -> io::Result<crate::vfs::CompleteDirectoryListing>,
    {
        let (listing, case_rename, finish_listing) = case_actions;
        let mut spelling_attempts: Vec<Option<SpellingAttempt>> =
            (0..self.rows.len()).map(|_| None).collect();
        let mut initial_spelling_sources = vec![false; self.rows.len()];
        let mut cycle_results: Vec<Option<CycleResult>> =
            (0..self.exchange_cycles.len()).map(|_| None).collect();
        let expired = self.is_expired();
        let total = self.execution_order.len();
        // Eine frische vollständige Namensbilanz pro Stapel genügt. Späte echte
        // Zielkollisionen prüft anschließend jedes gewöhnliche Rename erneut
        // und atomar ohne Ersetzen. Schreibweisenänderungen teilen zusätzlich ein
        // gemeinsames vollständiges Abschlusslisting; pro Zeile bleibt nur Stat.
        let mut current_names = listing.and_then(|listing| {
            if !listing.complete {
                return Err(invalid("Der aktuelle Ordnerbestand ist unvollständig"));
            }
            let unique_sources = unique_raw_sources(&listing);
            for &index in &self.execution_order {
                if is_spelling_change(&self.rows[index]) || self.cycle_by_row[index].is_some() {
                    initial_spelling_sources[index] = unique_sources
                        .get(self.rows[index].old_name.as_str())
                        .is_some_and(|actual| {
                            self.proofs[index]
                                .as_ref()
                                .is_some_and(|proof| same_source(proof, actual))
                        });
                }
            }
            let mut counts = HashMap::new();
            for entry in &listing.entries {
                *counts
                    .entry(crate::remote_rename::folded_name(&entry.entry.name))
                    .or_insert(0_usize) += 1;
            }
            Ok(counts)
        });
        for (completed, &index) in self.execution_order.iter().enumerate() {
            if let Some(cycle_index) = self.cycle_by_row[index] {
                if cycle_results[cycle_index].is_some() {
                    continue;
                }
                let members = &self.exchange_cycles[cycle_index];
                let mut result = self.execute_exchange_cycle(
                    members,
                    &initial_spelling_sources,
                    cancel,
                    exchange,
                    sync,
                    &mut |step| on_progress(completed + step + 1, total),
                );
                if let Err(error) = &current_names {
                    if !result.stop.as_ref().is_some_and(|stop| {
                        crate::error_code(stop) == crate::ErrorCode::Interrupted
                    }) {
                        result.stop = Some(crate::coded_error(
                            error.kind(),
                            crate::error_code(error),
                            error.to_string(),
                        ));
                    }
                }
                cycle_results[cycle_index] = Some(result);
                continue;
            }
            if let Some(partner) = self.exchange_partners[index] {
                // Beide Zeilen bleiben in der Vorschau an ihrem Platz, werden aber
                // gemeinsam genau einmal bilanziert. Abbruch trennt das Paar nicht.
                if self.rows[index].status != BatchRenameStatus::Ready {
                    continue;
                }
                let left = self.root.join(&self.rows[index].old_name);
                let right = self.root.join(&self.rows[partner].old_name);
                let result = (|| -> io::Result<()> {
                    if cancel.is_cancelled() {
                        return Err(crate::coded_error(
                            io::ErrorKind::Interrupted,
                            crate::ErrorCode::Interrupted,
                            "Vor dem Namenstausch abgebrochen; beide Namen unverändert",
                        ));
                    }
                    if expired {
                        return Err(invalid(
                            "Die Vorschau ist älter als zehn Minuten; erneut vorbereiten",
                        ));
                    }
                    let occupied = current_names.as_ref().map_err(|error| {
                        crate::coded_error(
                            error.kind(),
                            crate::error_code(error),
                            error.to_string(),
                        )
                    })?;
                    for source in [index, partner] {
                        let row = &self.rows[source];
                        if occupied.get(&crate::remote_rename::folded_name(&row.old_name))
                            != Some(&1)
                        {
                            return Err(invalid(
                                "Die Namen des Austauschpaars sind nicht mehr eindeutig belegt",
                            ));
                        }
                        self.check_source(source)?;
                    }
                    if cancel.is_cancelled() {
                        return Err(crate::coded_error(
                            io::ErrorKind::Interrupted,
                            crate::ErrorCode::Interrupted,
                            "Vor dem Namenstausch abgebrochen; beide Namen unverändert",
                        ));
                    }
                    // Ausschließlich der atomare lokale Kernel-Austausch: weder
                    // Ersatz durch mehrere Renames noch Retry oder Rücktausch.
                    if let Err(error) = exchange(&self.vfs, &left, &right) {
                        // Vfs liefert io::Error statt MutationFailure. Ein Fehler
                        // allein beweist deshalb keine ausgebliebene Wirkung.
                        let original_left = self.check_source(index);
                        let original_right = self.check_source(partner);
                        if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear
                            || original_left.is_err()
                            || original_right.is_err()
                        {
                            let _ = sync(&self.vfs, &self.root);
                            return Err(crate::coded_error(io::ErrorKind::Other,
                                crate::ErrorCode::OutcomeUnclear,
                                format!("Atomarer Namenstausch meldete einen Fehler ({error}); unveränderte ursprüngliche Namen sind nicht sicher bestätigt")));
                        }
                        return Err(crate::coded_error(error.kind(), crate::error_code(&error),
                            format!("Atomarer Namenstausch meldete einen Fehler ({error}); beide ursprünglichen Identitäten unter ihren bisherigen Namen zurückgelesen, unverändert")));
                    }
                    let readback = (|| -> io::Result<()> {
                        let actual_left = self.vfs.stat(&left);
                        let actual_right = self.vfs.stat(&right);
                        if LocalVfs::new().local_root_identity(&self.root)? != self.root_identity {
                            return Err(invalid("Pane-Root nach dem Namenstausch ausgetauscht"));
                        }
                        for (source, actual) in [(index, actual_right), (partner, actual_left)] {
                            let actual = actual?;
                            if !self.proofs[source]
                                .as_ref()
                                .is_some_and(|proof| same_source(proof, &actual))
                            {
                                return Err(invalid(
                                    "Identitätsbeleg nach dem Namenstausch verändert",
                                ));
                            }
                        }
                        Ok(())
                    })();
                    let flushed = sync(&self.vfs, &self.root);
                    readback.and(flushed).map_err(|error| crate::coded_error(
                        io::ErrorKind::Other, crate::ErrorCode::OutcomeUnclear,
                        format!("Namenstausch wurde ausgeführt, aber Abschlussprüfung oder Verzeichnis-Sync scheiterte ({error})")))
                })();
                let (status, code, message) = match result {
                    Ok(()) => (BatchRenameStatus::Renamed, None, "Atomar ausgetauscht; beide Identitäten zurückgelesen und Elternordner synchronisiert".to_string()),
                    Err(error) => {
                        let code = crate::error_code(&error);
                        let status = match code {
                            crate::ErrorCode::OutcomeUnclear => BatchRenameStatus::OutcomeUnclear,
                            crate::ErrorCode::Interrupted => BatchRenameStatus::Cancelled,
                            _ => BatchRenameStatus::FailedUnchanged,
                        };
                        let message = if status == BatchRenameStatus::OutcomeUnclear {
                            format!("{error}. Beide Pfade vor erneutem Versuch prüfen: {}; {}. Kein automatischer Rücktausch", left.display(), right.display())
                        } else {
                            format!("{error}; Paar nicht erfolgreich abgeschlossen, kein weiterer Versuch")
                        };
                        (status, Some(code), message)
                    }
                };
                for source in [index, partner] {
                    set_status(&mut self.rows[source], status.clone(), message.clone());
                    self.rows[source].code = code;
                }
                on_progress(completed + 2, total);
                continue;
            }
            if is_spelling_change(&self.rows[index]) {
                let result = current_names
                    .as_ref()
                    .map_err(|error| {
                        crate::coded_error(
                            error.kind(),
                            crate::error_code(error),
                            error.to_string(),
                        )
                    })
                    .and_then(|_| {
                        self.execute_spelling_change(
                            index,
                            cancel,
                            initial_spelling_sources[index],
                            case_rename,
                            sync,
                        )
                    });
                let row = &mut self.rows[index];
                match result {
                    Ok(attempt) => {
                        spelling_attempts[index] = Some(attempt);
                        // Kein Renamed vor dem Endscan: selbst ein vorläufiger
                        // Erfolg gibt abhängige Wirkungen ausdrücklich nicht frei.
                        row.message =
                            "Versuch bearbeitet; endgültiger Namensbeleg steht aus".into();
                    }
                    Err(error) => {
                        let code = crate::error_code(&error);
                        let status = if code == crate::ErrorCode::Interrupted {
                            BatchRenameStatus::Cancelled
                        } else {
                            BatchRenameStatus::FailedUnchanged
                        };
                        set_status(row, status, error.to_string());
                        row.code = Some(code);
                    }
                }
                on_progress(completed + 1, total);
                continue;
            }
            let dependency_failed = self.dependencies[index]
                .iter()
                .any(|&source| self.rows[source].status != BatchRenameStatus::Renamed);
            let row = &mut self.rows[index];
            let proof = &self.proofs[index];
            if cancel.is_cancelled() {
                set_status(
                    row,
                    BatchRenameStatus::Cancelled,
                    "Vor dieser Namensänderung abgebrochen; unverändert",
                );
                continue;
            }
            if expired {
                set_status(
                    row,
                    BatchRenameStatus::FailedUnchanged,
                    "Die Vorschau ist älter als zehn Minuten; erneut vorbereiten",
                );
                continue;
            }
            if dependency_failed {
                set_status(row, BatchRenameStatus::FailedUnchanged, "Nicht ausgeführt: Eine vorausgehende Umbenennung hat den Zielnamen nicht nachweislich freigemacht");
                on_progress(completed + 1, total);
                continue;
            }
            let old = self.root.join(&row.old_name);
            let new = self.root.join(&row.new_name);
            let preflight = (|| -> io::Result<()> {
                let current_root = LocalVfs::new().local_root_identity(&self.root)?;
                if current_root != self.root_identity {
                    return Err(crate::coded_error(
                        io::ErrorKind::InvalidInput,
                        crate::ErrorCode::PlanDrift,
                        "Der angezeigte Pane-Root wurde seit der Vorschau ausgetauscht",
                    ));
                }
                let current = self.vfs.stat(&old)?;
                if !proof
                    .as_ref()
                    .is_some_and(|proof| same_source(proof, &current))
                {
                    return Err(crate::coded_error(
                        io::ErrorKind::InvalidInput,
                        crate::ErrorCode::PlanDrift,
                        "Die Quelle wurde seit der Vorschau verändert oder ausgetauscht",
                    ));
                }
                let occupied = current_names.as_ref().map_err(|error| {
                    crate::coded_error(error.kind(), crate::error_code(error), error.to_string())
                })?;
                if occupied.contains_key(&crate::remote_rename::folded_name(&row.new_name)) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Das Ziel ist inzwischen belegt oder besitzt einen äquivalenten Namen",
                    ));
                }
                Ok(())
            })();
            if let Err(error) = preflight {
                set_status(row, BatchRenameStatus::FailedUnchanged, error.to_string());
                row.code = Some(crate::error_code(&error));
                on_progress(completed + 1, total);
                continue;
            }
            if cancel.is_cancelled() {
                set_status(
                    row,
                    BatchRenameStatus::Cancelled,
                    "Vor der Namensänderung abgebrochen; unverändert",
                );
                continue;
            }
            match rename(&self.vfs, &old, &row.new_name, &self.sandbox) {
                Ok(()) => match self.vfs.stat(&new) {
                    Ok(actual) if proof.as_ref().is_some_and(|proof| same_source(proof, &actual)) => set_status(row, BatchRenameStatus::Renamed, "Umbenannt; Identität zurückgelesen"),
                    _ => set_status(row, BatchRenameStatus::OutcomeUnclear, "Rename wurde ausgeführt, aber der Zielbeleg ist verändert oder nicht lesbar. Beide Namen vor erneutem Versuch prüfen"),
                },
                Err(error) if crate::error_code(&error) == crate::ErrorCode::OutcomeUnclear => set_status(row, BatchRenameStatus::OutcomeUnclear, error.to_string()),
                Err(error) => { set_status(row, BatchRenameStatus::FailedUnchanged, error.to_string()); row.code = Some(crate::error_code(&error)); },
            }
            if row.status == BatchRenameStatus::Renamed {
                if let Ok(occupied) = &mut current_names {
                    let old_key = crate::remote_rename::folded_name(&row.old_name);
                    if let Some(count) = occupied.get_mut(&old_key) {
                        *count -= 1;
                        if *count == 0 {
                            occupied.remove(&old_key);
                        }
                    }
                    *occupied
                        .entry(crate::remote_rename::folded_name(&row.new_name))
                        .or_insert(0) += 1;
                }
            }
            on_progress(completed + 1, total);
        }
        let needs_final_listing = spelling_attempts.iter().any(Option::is_some)
            || cycle_results
                .iter()
                .flatten()
                .any(|result| result.attempted.iter().any(|attempted| *attempted));
        if needs_final_listing || !cycle_results.is_empty() {
            // Bewusst ohne Cancel: auch nach Abbruch brauchen bereits versuchte
            // Zeilen genau einen gemeinsamen, vollständigen Abschlussbeleg.
            let final_listing = if needs_final_listing {
                finish_listing(&self.vfs, &self.root)
            } else {
                Err(invalid("Kein Zyklusschritt wurde begonnen"))
            };
            let root_matches = LocalVfs::new()
                .local_root_identity(&self.root)
                .is_ok_and(|identity| identity == self.root_identity);
            let final_sources = final_listing
                .as_ref()
                .map(unique_raw_sources)
                .unwrap_or_default();
            for (index, attempt) in spelling_attempts.into_iter().enumerate() {
                let Some(attempt) = attempt else {
                    continue;
                };
                let row = &mut self.rows[index];
                let matches = |name: &str| {
                    root_matches
                        && final_sources.get(name).is_some_and(|actual| {
                            self.proofs[index]
                                .as_ref()
                                .is_some_and(|proof| same_source(proof, actual))
                        })
                };
                // Anfangs-/Endbestand und Kernel-NoReplace belegen keine lückenlose
                // eigene Namenshistorie: fremde Casewechsel derselben Inode oder
                // vorübergehende Aliasnamen können dazwischen unbeobachtet bleiben.
                // Der vollständige Endscan belegt nur die eindeutige Zielschreibweise
                // samt Identität. Wegen gleicher Faltung ist dabei auch old weg.
                let result = match attempt {
                    SpellingAttempt::Succeeded if matches(&row.new_name) => Ok(()),
                    SpellingAttempt::Unchanged(error) if matches(&row.old_name) => {
                        set_status(row, BatchRenameStatus::FailedUnchanged,
                            format!("Dateisystem verweigerte die Änderung ({error}); alter Rohname und Identität im Abschlussbestand bestätigt, eigene Wirkung unverändert"));
                        row.code = Some(crate::error_code(&error));
                        continue;
                    }
                    // Frühere Unsicherheit darf ein später passender Name nicht
                    // heilen; Kernel-Ok mit altem Namen ist ebenfalls unklar.
                    SpellingAttempt::Unclear(error) => Err(error.to_string()),
                    SpellingAttempt::Unchanged(error) => Err(format!("Rename meldete ausgebliebene Wirkung ({error}), aber der alte Rohname und seine Identität sind nicht bestätigt")),
                    SpellingAttempt::Succeeded => Err("Der exakte neue Rohname und seine Identität sind im Abschlussbestand nicht eindeutig bestätigt".into()),
                };
                match result {
                    Ok(()) => set_status(row, BatchRenameStatus::Renamed,
                        "Zielschreibweise und Identität bestätigt; Elternordner synchronisiert"),
                    Err(reason) => set_status(row, BatchRenameStatus::OutcomeUnclear,
                        format!("{reason}. Beide Namen vor erneutem Versuch prüfen: {}; {}. Kein automatischer Wiederholversuch oder Rücktausch", self.root.join(&row.old_name).display(), self.root.join(&row.new_name).display())),
                }
            }
            for (cycle_index, result) in cycle_results.into_iter().enumerate() {
                let Some(result) = result else {
                    continue;
                };
                let possible_names = self.exchange_cycles[cycle_index]
                    .iter()
                    .map(|&index| format!("„{}“", self.rows[index].old_name))
                    .collect::<Vec<_>>()
                    .join(", ");
                for (position, &index) in self.exchange_cycles[cycle_index].iter().enumerate() {
                    let row = &mut self.rows[index];
                    let current = &result.locations[position];
                    let confirmed = root_matches
                        && final_sources.get(current.as_str()).is_some_and(|actual| {
                            self.proofs[index]
                                .as_ref()
                                .is_some_and(|proof| same_source(proof, actual))
                        });
                    let reason = result
                        .stop
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "Zyklus ausgeführt".into());
                    let cancelled = result.stop.as_ref().is_some_and(|error| {
                        crate::error_code(error) == crate::ErrorCode::Interrupted
                    });
                    if result.unknown[position] || (!confirmed && result.attempted[position]) {
                        set_status(row, BatchRenameStatus::OutcomeUnclear,
                            format!("Zyklusausgang für dieses Objekt unklar ({reason}). Mögliche betroffene Namen: {possible_names}. Diese Namen vor erneutem Versuch prüfen; kein automatischer Rücktausch"));
                    } else if confirmed && current == &row.new_name {
                        set_status(
                            row,
                            BatchRenameStatus::Renamed,
                            "Zielname und Identität bestätigt; Zyklusschritt synchronisiert",
                        );
                        row.current_name = Some(current.clone());
                    } else if confirmed && current != &row.old_name {
                        set_status(row, BatchRenameStatus::PartiallyRenamed,
                            format!("Bereits bewegt, endgültiger Zielname noch nicht erreicht ({reason}). Kein automatischer Rücktausch"));
                        row.current_name = Some(current.clone());
                        row.code = result.stop.as_ref().map(crate::error_code);
                    } else {
                        let status = if cancelled {
                            BatchRenameStatus::Cancelled
                        } else {
                            BatchRenameStatus::FailedUnchanged
                        };
                        set_status(
                            row,
                            status,
                            format!("Für dieses Objekt keine bestätigte Namensänderung ({reason})"),
                        );
                        row.code = result.stop.as_ref().map(crate::error_code);
                        if confirmed {
                            row.current_name = Some(current.clone());
                        }
                    }
                }
            }
        }
        finish_report(self.rows, None, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> BatchRenameOptions {
        BatchRenameOptions {
            find: "alt".into(),
            replacement: "neu".into(),
            prefix: "".into(),
            suffix: "".into(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        }
    }
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        (temp, root)
    }
    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn spelling_options(old: &str, new: &str) -> BatchRenameOptions {
        BatchRenameOptions {
            find: old.into(),
            replacement: new.into(),
            preserve_extension: false,
            ..options()
        }
    }

    #[test]
    fn batch_schreibweise_datei_ordner_unicode_und_unabhaengige_zeile() {
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        #[derive(Debug, PartialEq)]
        struct Evidence {
            device: u64,
            inode: u64,
            size: u64,
            mode: u32,
            mtime: (i64, i64),
            directory: bool,
            content: Vec<u8>,
        }
        fn raw_evidence(root: &Path) -> HashMap<Vec<u8>, Evidence> {
            std::fs::read_dir(root)
                .unwrap()
                .map(|item| {
                    let item = item.unwrap();
                    let path = item.path();
                    let metadata = std::fs::symlink_metadata(&path).unwrap();
                    assert!(!metadata.is_symlink());
                    let content = if metadata.is_dir() {
                        let children: Vec<_> = std::fs::read_dir(&path)
                            .unwrap()
                            .map(|child| child.unwrap().file_name().as_bytes().to_vec())
                            .collect();
                        assert_eq!(children, [b"kind".to_vec()]);
                        std::fs::read(path.join("kind")).unwrap()
                    } else {
                        std::fs::read(path).unwrap()
                    };
                    (
                        item.file_name().as_bytes().to_vec(),
                        Evidence {
                            device: metadata.dev(),
                            inode: metadata.ino(),
                            size: metadata.size(),
                            mode: metadata.mode(),
                            mtime: (metadata.mtime(), metadata.mtime_nsec()),
                            directory: metadata.is_dir(),
                            content,
                        },
                    )
                })
                .collect()
        }
        for (old, new, directory) in [
            ("Foo", "foo", false),
            ("Foo", "foo", true),
            ("café", "cafe\u{301}", false),
            ("cafe\u{301}", "café", false),
        ] {
            let (_temp, root) = fixture();
            if directory {
                std::fs::create_dir(root.join(old)).unwrap();
                std::fs::write(root.join(old).join("kind"), b"inhalt").unwrap();
            } else {
                std::fs::write(root.join(old), b"inhalt").unwrap();
            }
            let extra_old = format!("{old}-extra");
            let extra_new = format!("{new}-extra");
            std::fs::write(root.join(&extra_old), b"unabhaengig").unwrap();
            let before = raw_evidence(&root);
            assert_eq!(before.len(), 2);
            assert_eq!(before.get(old.as_bytes()).unwrap().content, b"inhalt");
            assert_eq!(
                before.get(extra_old.as_bytes()).unwrap().content,
                b"unabhaengig"
            );
            let plan = PreparedBatchRename::prepare(
                &root,
                &names(&[old, &extra_old]),
                &spelling_options(old, new),
                None,
            )
            .unwrap();
            assert!(plan
                .rows()
                .iter()
                .all(|row| row.status == BatchRenameStatus::Ready));
            assert!(plan.rows()[0]
                .message
                .contains("Schreibweise atomar ändern, sofern das Dateisystem sie zulässt"));
            let initial = plan.vfs.list_complete(&root).unwrap();
            for source in [old, extra_old.as_str()] {
                let listed = unique_raw_source(&initial, source).unwrap();
                let stored = before.get(source.as_bytes()).unwrap();
                assert_eq!(listed.device, Some(stored.device));
                assert_eq!(listed.inode, Some(stored.inode));
            }
            let mut attempts = Vec::new();
            let report = plan.execute_with_case_actions(
                (
                    Ok(initial),
                    &mut |vfs: &dyn Vfs, from: &Path, to: &Path| {
                        let result = vfs.rename_noreplace_with_removal_proof(from, to);
                        let failure = result.as_ref().err().map(|failure| match failure {
                            crate::vfs::MutationFailure::Unchanged(error) => {
                                (true, error.kind(), error.raw_os_error())
                            }
                            crate::vfs::MutationFailure::OutcomeUnclear(error) => {
                                (false, error.kind(), error.raw_os_error())
                            }
                        });
                        attempts.push((from.to_path_buf(), to.to_path_buf(), failure));
                        result
                    },
                    &mut |vfs: &dyn Vfs, path: &Path| vfs.list_complete(path),
                ),
                &Cancel::new(),
                &mut |_, _| {},
                &mut crate::fileops::rename,
                &mut |_, _, _| panic!("Kein Austausch bei reiner Schreibweisenänderung"),
                &mut |vfs, path| vfs.sync_dir(path),
            );
            let after = raw_evidence(&root);
            assert_eq!(after.len(), 2);
            assert_eq!(attempts.len(), 2);
            for (index, (source, target)) in [(old, new), (extra_old.as_str(), extra_new.as_str())]
                .into_iter()
                .enumerate()
            {
                assert_eq!(report.rows[index].old_name, source);
                assert_eq!(report.rows[index].new_name, target);
                let calls: Vec<_> = attempts
                    .iter()
                    .filter(|(from, _, _)| *from == root.join(source))
                    .collect();
                assert_eq!(
                    calls.len(),
                    1,
                    "Genau ein echter NoReplace-Versuch je Zeile"
                );
                assert_eq!(calls[0].1, root.join(target));
                let original = before.get(source.as_bytes()).unwrap();
                match report.rows[index].status {
                    BatchRenameStatus::Renamed => {
                        assert_eq!(calls[0].2, None);
                        assert_eq!(after.get(target.as_bytes()), Some(original));
                        assert!(!after.contains_key(source.as_bytes()));
                    }
                    BatchRenameStatus::FailedUnchanged => {
                        assert_ne!(old, "Foo", "Case-Datei und -Ordner müssen umbenannt werden");
                        let (unchanged, kind, errno) = calls[0]
                            .2
                            .expect("Verweigerung braucht einen echten Primitivefehler");
                        assert!(unchanged);
                        assert!(
                            errno == Some(libc::EEXIST) || kind == io::ErrorKind::Unsupported,
                            "Nur EEXIST/Unsupported darf Unicode verweigern: {kind:?}/{errno:?}"
                        );
                        // Der echte Rohbestand ist unabhängig vom Produkt-Readback:
                        // alter Name, Device/Inode, Metadaten und Inhalt bleiben exakt.
                        assert_eq!(after.get(source.as_bytes()), Some(original));
                        assert!(!after.contains_key(target.as_bytes()));
                    }
                    ref status => panic!("Unerwartete Bilanz für {source}: {status:?}"),
                }
            }
        }
    }

    #[test]
    fn batch_schreibweise_fremde_und_hardlinkgruppe_bleiben_gesperrt() {
        for hardlink in [false, true] {
            let (_temp, root) = fixture();
            std::fs::write(root.join("Foo"), b"quelle").unwrap();
            let mut listing = LocalVfs::new().list_complete(&root).unwrap();
            let mut alias = listing.entries[0].clone();
            alias.entry.name = "foo".into();
            alias.raw_name = b"foo".to_vec();
            if !hardlink {
                alias.entry.inode = alias.entry.inode.map(|inode| inode + 1);
            }
            listing.entries.push(alias);
            let (mut rows, _) = plan_rows_with_chains(
                &listing,
                &names(&["Foo"]),
                &spelling_options("Foo", "foo"),
                true,
            );
            assert!(local_execution_plan(&listing, &mut rows).0.is_empty());
            assert_eq!(rows[0].status, BatchRenameStatus::Blocked);
            let (mut remote, _) = plan_rows_with_chains(
                &LocalVfs::new().list_complete(&root).unwrap(),
                &names(&["Foo"]),
                &spelling_options("Foo", "foo"),
                true,
            );
            assert!(chain_execution_plan(&listing, &mut remote).0.is_empty());
        }
    }

    #[test]
    fn batch_schreibweise_startlistingfehler_und_zielduplikate_bleiben_gesperrt() {
        let (_temp, root) = fixture();
        std::fs::write(root.join("Foo"), b"quelle").unwrap();
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["Foo"]),
            &spelling_options("Foo", "foo"),
            None,
        )
        .unwrap();
        let report = plan.execute_with_listing(
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Listing verweigert",
            )),
            &Cancel::new(),
            &mut |_, _| {},
        );
        assert_eq!(report.failed_unchanged, 1);
        assert_eq!(
            report.rows[0].code,
            Some(crate::ErrorCode::PermissionDenied)
        );
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name(),
            "Foo"
        );
        std::fs::write(root.join("foo-x"), b"zweite").unwrap();
        let rules = BatchRenameOptions {
            find: "-x".into(),
            replacement: String::new(),
            ..options()
        };
        let listing = LocalVfs::new().list_complete(&root).unwrap();
        let (mut rows, _) =
            plan_rows_with_chains(&listing, &names(&["Foo", "foo-x"]), &rules, true);
        // Beide Zeilen beanspruchen dieselbe Gruppe; eine unveränderte Zeile darf
        // keinen Platz für die andere freigeben.
        assert!(local_execution_plan(&listing, &mut rows).0.is_empty());
        assert_eq!(rows[0].status, BatchRenameStatus::Unchanged);
        assert_eq!(rows[1].status, BatchRenameStatus::Blocked);
    }

    #[test]
    fn batch_schreibweise_reale_fremd_und_hardlinknamen_soweit_darstellbar() {
        for hardlink in [false, true] {
            for (old, alias, new) in [("Foo", "FOO", "foo"), ("café", "cafe\u{301}", "CAFÉ")] {
                let (_temp, root) = fixture();
                std::fs::write(root.join(old), b"quelle").unwrap();
                let added = if hardlink {
                    std::fs::hard_link(root.join(old), root.join(alias))
                } else {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(root.join(alias))
                        .map(drop)
                };
                match added {
                    // Ein aliasauflösendes Dateisystem kann dieses Paar bereits
                    // beim exklusiven Anlegen nicht als zwei Namen darstellen.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    result => result.unwrap(),
                }
                let plan = PreparedBatchRename::prepare(
                    &root,
                    &names(&[old]),
                    &spelling_options(old, new),
                    None,
                )
                .unwrap();
                assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
                assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 0);
                assert_eq!(std::fs::read(root.join(old)).unwrap(), b"quelle");
                assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
            }
        }
    }

    #[test]
    fn batch_schreibweise_hardlink_ausserhalb_der_gruppe_ist_erlaubt() {
        let (_temp, root) = fixture();
        std::fs::write(root.join("Foo"), b"quelle").unwrap();
        std::fs::hard_link(root.join("Foo"), root.join("anderer_name")).unwrap();
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["Foo"]),
            &spelling_options("Foo", "foo"),
            None,
        )
        .unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Ready);
        assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 1);
        assert_eq!(std::fs::read(root.join("anderer_name")).unwrap(), b"quelle");
    }

    #[test]
    fn batch_schreibweise_drift_und_abbruch_starten_keinen_versuch() {
        for fault in ["rawname", "source", "root", "cancel", "alias"] {
            let (_temp, root) = fixture();
            let pane = root.join("pane");
            std::fs::create_dir(&pane).unwrap();
            std::fs::write(pane.join("Foo"), b"quelle").unwrap();
            let plan = PreparedBatchRename::prepare(
                &pane,
                &names(&["Foo"]),
                &spelling_options("Foo", "foo"),
                None,
            )
            .unwrap();
            let cancel = Cancel::new();
            match fault {
                "rawname" => std::fs::rename(pane.join("Foo"), pane.join("foo")).unwrap(),
                "source" => {
                    std::fs::rename(pane.join("Foo"), pane.join("gesichert")).unwrap();
                    std::fs::write(pane.join("Foo"), b"fremd").unwrap();
                }
                "root" => {
                    std::fs::rename(&pane, root.join("original")).unwrap();
                    std::fs::create_dir(&pane).unwrap();
                    std::fs::write(pane.join("Foo"), b"fremd").unwrap();
                }
                "cancel" => cancel.cancel(),
                _ => {}
            }
            let mut listing = plan.vfs.list_complete(&pane).unwrap();
            if fault == "alias" {
                let mut alias = listing.entries[0].clone();
                alias.entry.name = "foo".into();
                alias.raw_name = b"foo".to_vec();
                listing.entries.push(alias);
            }
            let report = plan.execute_with_case_actions(
                (
                    Ok(listing),
                    &mut |_, _, _| panic!("Keine Mutation bei {fault}"),
                    &mut |_, _| panic!("Kein Endscan ohne Versuch bei {fault}"),
                ),
                &cancel,
                &mut |_, _| {},
                &mut crate::fileops::rename,
                &mut |_, _, _| panic!("Kein Exchange bei {fault}"),
                &mut |_, _| panic!("Kein Sync bei {fault}"),
            );
            assert_eq!(report.renamed, 0);
            assert_eq!(report.outcome_unclear, 0);
            if fault == "cancel" {
                assert_eq!(report.cancelled, 1);
            } else {
                assert_eq!(report.failed_unchanged, 1);
            }
        }
    }

    #[test]
    fn batch_schreibweise_ein_versuch_noop_fehler_und_abschlussbelege() {
        for fault in [
            "success",
            "noop",
            "unsupported",
            "effect_error",
            "effect_plain_error",
            "sync",
            "readback",
            "wrong_identity",
            "wrong_rawname",
            "incomplete",
            "unclear_unchanged",
            "foreign_revert",
            "late_alias",
            "root_drift",
            "immediate_readback",
            "immediate_identity",
        ] {
            let (_temp, root) = fixture();
            let pane = root.join("pane");
            std::fs::create_dir(&pane).unwrap();
            std::fs::write(pane.join("Foo"), b"quelle").unwrap();
            let plan = PreparedBatchRename::prepare(
                &pane,
                &names(&["Foo"]),
                &spelling_options("Foo", "foo"),
                None,
            )
            .unwrap();
            let listing = plan.vfs.list_complete(&pane);
            let mut attempts = 0;
            let mut final_listings = 0;
            let report = plan.execute_with_case_actions(
                (
                    listing,
                    &mut |vfs: &dyn Vfs, old: &Path, new: &Path| {
                        attempts += 1;
                        if fault == "noop" {
                            return Ok(None);
                        }
                        if fault == "unsupported" {
                            return Err(crate::vfs::MutationFailure::Unchanged(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "kein No-Replace",
                            )));
                        }
                        if fault == "unclear_unchanged" {
                            return Err(crate::vfs::MutationFailure::OutcomeUnclear(
                                io::Error::other("Antwort unklar"),
                            ));
                        }
                        vfs.rename_noreplace_with_removal_proof(old, new)?;
                        if fault == "effect_error" {
                            return Err(crate::vfs::MutationFailure::OutcomeUnclear(
                                io::Error::other("Antwort verloren"),
                            ));
                        }
                        if fault == "effect_plain_error" {
                            return Err(crate::vfs::MutationFailure::Unchanged(io::Error::other(
                                "unkodierter Fehler nach Wirkung",
                            )));
                        }
                        if matches!(fault, "immediate_readback" | "immediate_identity") {
                            std::fs::rename(new, pane.join("zwischenzeitlich")).unwrap();
                            if fault == "immediate_identity" {
                                std::fs::write(new, b"fremd").unwrap();
                            }
                        }
                        Ok(None)
                    },
                    &mut |vfs: &dyn Vfs, path: &Path| {
                        final_listings += 1;
                        if fault == "readback" {
                            return Err(io::Error::other("Listing fehlgeschlagen"));
                        }
                        if fault == "foreign_revert" {
                            std::fs::rename(pane.join("foo"), pane.join("Foo")).unwrap();
                        }
                        if matches!(fault, "immediate_readback" | "immediate_identity") {
                            if fault == "immediate_identity" {
                                std::fs::remove_file(pane.join("foo")).unwrap();
                            }
                            std::fs::rename(pane.join("zwischenzeitlich"), pane.join("foo"))
                                .unwrap();
                        }
                        if fault == "root_drift" {
                            std::fs::rename(&pane, root.join("original")).unwrap();
                            std::fs::create_dir(&pane).unwrap();
                        }
                        let mut listing = vfs.list_complete(path)?;
                        match fault {
                            "wrong_identity" => listing.entries[0].entry.inode = None,
                            "wrong_rawname" => listing.entries[0].raw_name = b"FOO".to_vec(),
                            "incomplete" => listing.complete = false,
                            "late_alias" => {
                                let mut alias = listing.entries[0].clone();
                                alias.entry.name = "FOO".into();
                                alias.raw_name = b"FOO".to_vec();
                                listing.entries.push(alias);
                            }
                            _ => {}
                        }
                        Ok(listing)
                    },
                ),
                &Cancel::new(),
                &mut |_, _| {},
                &mut crate::fileops::rename,
                &mut |_, _, _| panic!("Kein Austausch bei {fault}"),
                &mut |vfs, path| {
                    if fault == "sync" {
                        Err(io::Error::other("Sync fehlgeschlagen"))
                    } else {
                        vfs.sync_dir(path)
                    }
                },
            );
            assert_eq!(attempts, 1, "{fault}");
            assert_eq!(final_listings, 1, "{fault}");
            match fault {
                "success" => assert_eq!(report.renamed, 1),
                "unsupported" => assert_eq!(report.failed_unchanged, 1),
                _ => assert_eq!(report.outcome_unclear, 1, "{fault}: {:?}", report.rows),
            }
            let actual_pane = if fault == "root_drift" {
                root.join("original")
            } else {
                pane
            };
            assert_eq!(std::fs::read_dir(actual_pane).unwrap().count(), 1);
        }
    }

    #[test]
    fn batch_schreibweise_abbruch_nach_mehreren_versuchen_liest_trotzdem_einmal_zurueck() {
        let (_temp, root) = fixture();
        for name in ["Foo0", "Foo1", "Foo2"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["Foo0", "Foo1", "Foo2"]),
            &spelling_options("Foo", "foo"),
            None,
        )
        .unwrap();
        let listing = plan.vfs.list_complete(&root);
        let cancel = Cancel::new();
        let trigger = cancel.clone();
        let mut attempts = 0;
        let mut finals = 0;
        let report = plan.execute_with_case_actions(
            (
                listing,
                &mut |vfs: &dyn Vfs, old: &Path, new: &Path| {
                    attempts += 1;
                    vfs.rename_noreplace_with_removal_proof(old, new)
                },
                &mut |vfs: &dyn Vfs, path: &Path| {
                    finals += 1;
                    assert!(cancel.is_cancelled());
                    vfs.list_complete(path)
                },
            ),
            &cancel,
            &mut |done, _| {
                if done == 2 {
                    trigger.cancel();
                }
            },
            &mut crate::fileops::rename,
            &mut |_, _, _| panic!("Kein Austausch"),
            &mut |vfs, path| vfs.sync_dir(path),
        );
        assert_eq!(attempts, 2);
        assert_eq!(finals, 1);
        assert_eq!(report.renamed, 2);
        assert_eq!(report.cancelled, 1);
        assert_eq!(std::fs::read(root.join("Foo2")).unwrap(), b"Foo2");
    }

    #[test]
    fn batch_schreibweise_vorlaeufiger_erfolg_gibt_keine_abhaengige_wirkung_frei() {
        let (_temp, root) = fixture();
        std::fs::write(root.join("Foo"), b"quelle").unwrap();
        std::fs::write(root.join("alt"), b"unabhaengig").unwrap();
        let mut plan = PreparedBatchRename::prepare(
            &root,
            &names(&["Foo", "alt"]),
            &spelling_options("Foo", "foo"),
            None,
        )
        .unwrap();
        // Der Planer lässt für dieselbe Zielgruppe ohnehin keine solche Kette zu.
        // Ein gezielt eingesetzter Abhängigkeitseintrag prüft zusätzlich die
        // Ausführungsschranke gegen vorläufige statt bestätigte Case-Erfolge.
        plan.rows[1].new_name = "neu".into();
        plan.rows[1].status = BatchRenameStatus::Ready;
        plan.dependencies[1] = vec![0];
        plan.execution_order = vec![0, 1];
        let listing = plan.vfs.list_complete(&root);
        let report = plan.execute_with_actions(
            listing,
            &Cancel::new(),
            &mut |_, _| {},
            &mut |_, _, _, _| panic!("Vorläufiger Case-Erfolg darf keine Abhängigkeit freigeben"),
            &mut |_, _, _| panic!("Kein Austausch"),
            &mut |vfs, path| vfs.sync_dir(path),
        );
        assert_eq!(report.renamed, 1);
        assert_eq!(report.failed_unchanged, 1);
        assert_eq!(std::fs::read(root.join("alt")).unwrap(), b"unabhaengig");
    }

    #[test]
    fn batch_schreibweise_spaeter_echter_hardlinkalias_soweit_darstellbar() {
        let (_temp, root) = fixture();
        std::fs::write(root.join("Foo"), b"quelle").unwrap();
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["Foo"]),
            &spelling_options("Foo", "foo"),
            None,
        )
        .unwrap();
        let listing = plan.vfs.list_complete(&root);
        let mut alias_created = false;
        let report = plan.execute_with_case_actions(
            (
                listing,
                &mut |vfs: &dyn Vfs, old: &Path, new: &Path| {
                    vfs.rename_noreplace_with_removal_proof(old, new)
                },
                &mut |vfs: &dyn Vfs, path: &Path| {
                    match std::fs::hard_link(root.join("foo"), root.join("FOO")) {
                        Ok(()) => alias_created = true,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => panic!("Hardlinkprobe fehlgeschlagen: {error}"),
                    }
                    vfs.list_complete(path)
                },
            ),
            &Cancel::new(),
            &mut |_, _| {},
            &mut crate::fileops::rename,
            &mut |_, _, _| panic!("Kein Austausch"),
            &mut |vfs, path| vfs.sync_dir(path),
        );
        if alias_created {
            assert_eq!(report.outcome_unclear, 1);
            assert_eq!(report.renamed, 0);
        } else {
            assert_eq!(report.renamed, 1);
        }
        assert_eq!(std::fs::read(root.join("foo")).unwrap(), b"quelle");
    }

    #[test]
    fn batch_schreibweise_hundert_versuche_brauchen_nur_zwei_vollstaendige_listings() {
        let (_temp, root) = fixture();
        let selected: Vec<_> = (0..100).map(|index| format!("Foo{index:04}")).collect();
        for name in &selected {
            std::fs::write(root.join(name), b"quelle").unwrap();
        }
        for index in 0..900 {
            std::fs::write(root.join(format!("nachbar{index:04}")), b"nachbar").unwrap();
        }
        let plan =
            PreparedBatchRename::prepare(&root, &selected, &spelling_options("Foo", "foo"), None)
                .unwrap();
        let mut listings = 0;
        let initial = {
            listings += 1;
            plan.vfs.list_complete(&root)
        };
        let mut attempts = 0;
        let report = plan.execute_with_case_actions(
            (
                initial,
                &mut |vfs: &dyn Vfs, old: &Path, new: &Path| {
                    attempts += 1;
                    vfs.rename_noreplace_with_removal_proof(old, new)
                },
                &mut |vfs: &dyn Vfs, path: &Path| {
                    listings += 1;
                    vfs.list_complete(path)
                },
            ),
            &Cancel::new(),
            &mut |_, _| {},
            &mut crate::fileops::rename,
            &mut |_, _, _| panic!("Kein Austausch"),
            &mut |vfs, path| vfs.sync_dir(path),
        );
        assert_eq!(attempts, 100);
        assert_eq!(listings, 2);
        assert_eq!(report.renamed, 100);
        assert_eq!(report.outcome_unclear, 0);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1000);
    }

    fn long_cycle_options(length: usize) -> BatchRenameOptions {
        BatchRenameOptions {
            find: "a".repeat(length),
            replacement: String::new(),
            prefix: "a".into(),
            preserve_extension: false,
            ..options()
        }
    }
    fn add_long_cycle(root: &Path, length: usize, suffix: &str) -> Vec<String> {
        (1..=length)
            .map(|index| {
                let name = format!("{}b{suffix}", "a".repeat(index));
                if index == 2 {
                    std::fs::create_dir(root.join(&name)).unwrap();
                    std::fs::write(
                        root.join(&name).join("kind"),
                        format!("object{index}{suffix}"),
                    )
                    .unwrap();
                } else {
                    std::fs::write(root.join(&name), format!("object{index}{suffix}")).unwrap();
                }
                name
            })
            .collect()
    }
    fn assert_cycle_content(root: &Path, name: &str, original: usize, suffix: &str) {
        let path = if original == 2 {
            root.join(name).join("kind")
        } else {
            root.join(name)
        };
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("object{original}{suffix}")
        );
    }

    #[test]
    fn batch_langzyklus_drei_und_vier_objekte_brauchen_n_minus_eins_echte_exchanges() {
        for length in [3, 4] {
            let (_temp, root) = fixture();
            let selected = add_long_cycle(&root, length, "");
            let plan =
                PreparedBatchRename::prepare(&root, &selected, &long_cycle_options(length), None)
                    .unwrap();
            assert!(plan
                .rows()
                .iter()
                .all(|row| row.status == BatchRenameStatus::Ready
                    && row.message.contains("Teiländerungen")));
            assert_eq!(plan.exchange_cycles, [((0..length).collect::<Vec<_>>())]);
            let original: Vec<_> = selected
                .iter()
                .map(|name| plan.vfs.stat(&root.join(name)).unwrap())
                .collect();
            let initial = plan.vfs.list_complete(&root);
            let mut attempts = 0;
            let mut end_scans = 0;
            let report = plan.execute_with_case_actions(
                (
                    initial,
                    &mut |_, _, _| panic!("Kein Schreibweisenrename"),
                    &mut |vfs: &dyn Vfs, path: &Path| {
                        end_scans += 1;
                        vfs.list_complete(path)
                    },
                ),
                &Cancel::new(),
                &mut |_, _| {},
                &mut |_, _, _, _| panic!("Kein Ersatzrename"),
                &mut |vfs, left, right| {
                    attempts += 1;
                    assert_eq!(left, root.join(&selected[0]));
                    assert_eq!(right, root.join(&selected[attempts]));
                    vfs.rename_exchange(left, right)
                },
                &mut |vfs, path| vfs.sync_dir(path),
            );
            assert_eq!(attempts, length - 1);
            assert_eq!(end_scans, 1);
            assert_eq!(report.renamed, length);
            assert_eq!(report.partially_renamed, 0);
            for (index, row) in report.rows.iter().enumerate() {
                let expected = &selected[(index + 1) % length];
                assert_eq!(row.current_name.as_ref(), Some(expected));
                assert!(same_source(
                    &original[index],
                    &LocalVfs::new().stat(&root.join(expected)).unwrap()
                ));
                assert_cycle_content(&root, expected, index + 1, "");
            }
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), length);
        }
    }

    #[test]
    fn batch_langzyklus_abbruch_vorher_und_nach_jedem_schritt_hat_ehrliche_namen() {
        for length in [3, 4] {
            for stop_after in 0..length {
                let (_temp, root) = fixture();
                let selected = add_long_cycle(&root, length, "");
                let plan = PreparedBatchRename::prepare(
                    &root,
                    &selected,
                    &long_cycle_options(length),
                    None,
                )
                .unwrap();
                let initial = plan.vfs.list_complete(&root);
                let cancel = Cancel::new();
                if stop_after == 0 {
                    cancel.cancel();
                }
                let mut attempts = 0;
                let report = plan.execute_with_actions(
                    initial,
                    &cancel,
                    &mut |_, _| {},
                    &mut |_, _, _, _| panic!("Kein Ersatzrename"),
                    &mut |vfs, left, right| {
                        attempts += 1;
                        vfs.rename_exchange(left, right)?;
                        if attempts == stop_after {
                            cancel.cancel();
                        }
                        Ok(())
                    },
                    &mut |vfs, path| vfs.sync_dir(path),
                );
                assert_eq!(attempts, stop_after);
                if stop_after == 0 {
                    assert_eq!(report.cancelled, length);
                    assert_eq!(report.partially_renamed, 0);
                } else if stop_after == length - 1 {
                    assert_eq!(report.renamed, length);
                } else {
                    assert_eq!(report.renamed, stop_after);
                    assert_eq!(report.partially_renamed, 1);
                    let json = serde_json::to_value(&report).unwrap();
                    assert_eq!(json["partiallyRenamed"], 1);
                    assert_eq!(json["rows"][stop_after]["status"], "partially_renamed");
                    assert_eq!(json["rows"][stop_after]["currentName"], selected[0]);
                    assert_eq!(
                        report.rows[stop_after].status,
                        BatchRenameStatus::PartiallyRenamed
                    );
                    assert_eq!(
                        report.rows[stop_after].current_name.as_ref(),
                        Some(&selected[0])
                    );
                    assert_cycle_content(&root, &selected[0], stop_after + 1, "");
                    for (index, row) in report.rows.iter().enumerate().skip(stop_after + 1) {
                        assert_eq!(row.status, BatchRenameStatus::Cancelled);
                        assert_eq!(row.current_name.as_ref(), Some(&selected[index]));
                    }
                }
                assert_eq!(report.outcome_unclear, 0);
            }
        }
    }

    #[test]
    fn batch_langzyklus_fehler_nach_teilerfolg_heilen_keine_beteiligten_objekte() {
        for fault in [
            "unsupported_first",
            "unsupported_second",
            "effect_error",
            "plain_effect_error",
            "readback",
            "sync_first",
            "sync_second",
            "pivot_drift",
            "root_drift",
            "final_listing",
            "pivot_drift_between",
            "other_drift_between",
            "root_drift_between",
            "alias_between",
        ] {
            let (_temp, root) = fixture();
            let pane = root.join("pane");
            std::fs::create_dir(&pane).unwrap();
            let selected = add_long_cycle(&pane, 3, "");
            let plan = PreparedBatchRename::prepare(&pane, &selected, &long_cycle_options(3), None)
                .unwrap();
            let initial = plan.vfs.list_complete(&pane);
            let mut attempts = 0;
            let mut syncs = 0;
            let cancel = Cancel::new();
            let report = plan.execute_with_case_actions(
                (
                    initial,
                    &mut |_, _, _| panic!("Kein Schreibweisenrename"),
                    &mut |vfs: &dyn Vfs, path: &Path| {
                        if fault == "readback" {
                            std::fs::remove_file(pane.join("aaab")).unwrap();
                            std::fs::rename(pane.join("gesichert"), pane.join("aaab")).unwrap();
                        }
                        if fault == "final_listing" {
                            Err(io::Error::other("Abschlusslisting fehlt"))
                        } else {
                            let mut listing = vfs.list_complete(path)?;
                            if fault == "alias_between" {
                                let mut alias = listing
                                    .entries
                                    .iter()
                                    .find(|entry| entry.entry.name == "ab")
                                    .unwrap()
                                    .clone();
                                alias.entry.name = "AB".into();
                                alias.raw_name = b"AB".to_vec();
                                listing.entries.push(alias);
                            }
                            Ok(listing)
                        }
                    },
                ),
                &cancel,
                &mut |done, _| {
                    if done == 2 {
                        if matches!(fault, "pivot_drift_between" | "other_drift_between") {
                            let name = if fault == "pivot_drift_between" {
                                "ab"
                            } else {
                                "aaab"
                            };
                            std::fs::rename(pane.join(name), pane.join("gesichert")).unwrap();
                            std::fs::write(pane.join(name), b"fremd").unwrap();
                        }
                        if fault == "root_drift_between" {
                            std::fs::rename(&pane, root.join("original")).unwrap();
                            std::fs::create_dir(&pane).unwrap();
                        }
                        if fault == "alias_between" {
                            cancel.cancel();
                        }
                    }
                },
                &mut |_, _, _, _| panic!("Kein Ersatzrename"),
                &mut |vfs, left, right| {
                    attempts += 1;
                    if fault == "unsupported_first"
                        || (fault == "unsupported_second" && attempts == 2)
                    {
                        return Err(io::Error::new(io::ErrorKind::Unsupported, "Kein Exchange"));
                    }
                    vfs.rename_exchange(left, right)?;
                    if attempts == 2 {
                        if fault == "effect_error" {
                            return Err(crate::coded_error(
                                io::ErrorKind::Other,
                                crate::ErrorCode::OutcomeUnclear,
                                "Antwort nach Wirkung verloren",
                            ));
                        }
                        if fault == "plain_effect_error" {
                            return Err(io::Error::other("Fehler nach Wirkung"));
                        }
                        if fault == "readback" {
                            std::fs::rename(right, pane.join("gesichert")).unwrap();
                            std::fs::write(right, b"fremd").unwrap();
                        }
                    }
                    if attempts == 1 && fault == "pivot_drift" {
                        std::fs::rename(left, pane.join("gesichert")).unwrap();
                        std::fs::write(left, b"fremd").unwrap();
                    }
                    if attempts == 1 && fault == "root_drift" {
                        std::fs::rename(&pane, root.join("original")).unwrap();
                        std::fs::create_dir(&pane).unwrap();
                    }
                    Ok(())
                },
                &mut |vfs, path| {
                    syncs += 1;
                    if (fault == "sync_first" && syncs == 1)
                        || (fault == "sync_second" && syncs == 2)
                    {
                        Err(io::Error::other("Syncfehler"))
                    } else {
                        vfs.sync_dir(path)
                    }
                },
            );
            match fault {
                "unsupported_first" => {
                    assert_eq!(attempts, 1);
                    assert_eq!(report.failed_unchanged, 3);
                    assert_eq!(report.outcome_unclear, 0);
                }
                "unsupported_second" => {
                    assert_eq!(attempts, 2);
                    assert_eq!(report.renamed, 1);
                    assert_eq!(report.partially_renamed, 1);
                    assert_eq!(report.failed_unchanged, 1);
                    assert_eq!(report.rows[1].current_name.as_deref(), Some("ab"));
                }
                "pivot_drift_between" | "alias_between" => {
                    assert_eq!(attempts, 1);
                    assert_eq!(report.renamed, 1);
                    assert_eq!(report.outcome_unclear, 1);
                    assert_eq!(report.rows[0].current_name.as_deref(), Some("aab"));
                }
                "other_drift_between" => {
                    assert_eq!(attempts, 1);
                    assert_eq!(report.renamed, 1);
                    assert_eq!(report.partially_renamed, 1);
                    assert_eq!(report.rows[1].current_name.as_deref(), Some("ab"));
                    assert_eq!(report.rows[2].status, BatchRenameStatus::FailedUnchanged);
                    assert!(report.rows[2].current_name.is_none());
                }
                "sync_first" | "pivot_drift" | "root_drift" | "root_drift_between" => {
                    assert_eq!(attempts, 1);
                    assert_eq!(report.outcome_unclear, 2);
                }
                "final_listing" => {
                    assert_eq!(attempts, 2);
                    assert_eq!(report.outcome_unclear, 3);
                }
                _ => {
                    assert_eq!(attempts, 2);
                    assert_eq!(report.rows[0].status, BatchRenameStatus::Renamed);
                    assert_eq!(report.outcome_unclear, 2);
                }
            }
            assert!(report
                .rows
                .iter()
                .filter(|row| row.status == BatchRenameStatus::OutcomeUnclear)
                .all(|row| row.current_name.is_none()));
        }
    }

    #[test]
    fn batch_langzyklus_mehrere_unabhaengige_zyklen_und_einzelzeile_bleiben_geordnet() {
        let (_temp, root) = fixture();
        let mut selected = add_long_cycle(&root, 3, "x");
        selected.extend(add_long_cycle(&root, 3, "y"));
        std::fs::write(root.join("solo"), b"einzeln").unwrap();
        selected.push("solo".into());
        let plan =
            PreparedBatchRename::prepare(&root, &selected, &long_cycle_options(3), None).unwrap();
        assert_eq!(plan.exchange_cycles.len(), 2);
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 7);
        assert_eq!(
            report
                .rows
                .iter()
                .map(|row| row.old_name.clone())
                .collect::<Vec<_>>(),
            selected
        );
        assert_eq!(std::fs::read(root.join("asolo")).unwrap(), b"einzeln");
    }

    #[test]
    fn batch_langzyklus_alias_doppelte_identitaet_und_remote_bleiben_gesperrt() {
        let (_temp, root) = fixture();
        let selected = add_long_cycle(&root, 3, "");
        let listing = LocalVfs::new().list_complete(&root).unwrap();
        let (rows, _) = plan_rows_with_chains(&listing, &selected, &long_cycle_options(3), true);
        assert_eq!(local_closed_cycles(&listing, &rows).len(), 1);
        let mut remote = rows.clone();
        assert!(chain_execution_plan(&listing, &mut remote).0.is_empty());
        for fault in ["alias", "identity", "raw", "incomplete", "blocked"] {
            let mut current = listing.clone();
            let mut rows = rows.clone();
            match fault {
                "alias" => {
                    let mut alias = current
                        .entries
                        .iter()
                        .find(|entry| entry.entry.name == "ab")
                        .unwrap()
                        .clone();
                    alias.entry.name = "AB".into();
                    alias.raw_name = b"AB".to_vec();
                    current.entries.push(alias);
                }
                "identity" => {
                    let first = current.entries[0].entry.clone();
                    current.entries[1].entry.device = first.device;
                    current.entries[1].entry.inode = first.inode;
                }
                "raw" => current.entries[0].raw_name = b"fremd".to_vec(),
                "incomplete" => current.complete = false,
                "blocked" => rows[1].status = BatchRenameStatus::Blocked,
                _ => unreachable!(),
            }
            assert!(local_closed_cycles(&current, &rows).is_empty(), "{fault}");
        }
    }

    #[test]
    fn batch_langzyklus_echte_gleiche_inodes_und_startlistingfehler_bleiben_ohne_wirkung() {
        let (_temp, root) = fixture();
        std::fs::write(root.join("ab"), b"gleich").unwrap();
        std::fs::write(root.join("aab"), b"andere").unwrap();
        std::fs::hard_link(root.join("ab"), root.join("aaab")).unwrap();
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["ab", "aab", "aaab"]),
            &long_cycle_options(3),
            None,
        )
        .unwrap();
        assert!(plan.exchange_cycles.is_empty());
        assert!(plan
            .rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        let (_temp2, other) = fixture();
        let selected = add_long_cycle(&other, 3, "");
        let plan =
            PreparedBatchRename::prepare(&other, &selected, &long_cycle_options(3), None).unwrap();
        let report = plan.execute_with_actions(
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Listing verweigert",
            )),
            &Cancel::new(),
            &mut |_, _| {},
            &mut |_, _, _, _| panic!("Kein Rename"),
            &mut |_, _, _| panic!("Kein Exchange ohne Anfangsbestand"),
            &mut |_, _| panic!("Kein Sync"),
        );
        assert_eq!(report.failed_unchanged, 3);
        assert!(report
            .rows
            .iter()
            .all(|row| row.code == Some(crate::ErrorCode::PermissionDenied)
                && row.current_name.is_none()));
    }

    #[test]
    fn batch_langzyklus_unklarer_pivot_nennt_alle_moeglichen_namen() {
        let (_temp, root) = fixture();
        let selected = add_long_cycle(&root, 3, "");
        let plan =
            PreparedBatchRename::prepare(&root, &selected, &long_cycle_options(3), None).unwrap();
        let initial = plan.vfs.list_complete(&root);
        let mut attempts = 0;
        let report = plan.execute_with_actions(
            initial,
            &Cancel::new(),
            &mut |_, _| {},
            &mut |_, _, _, _| panic!("Kein Ersatzrename"),
            &mut |vfs, left, right| {
                attempts += 1;
                vfs.rename_exchange(left, right)?;
                if attempts == 2 {
                    return Err(crate::coded_error(
                        io::ErrorKind::Other,
                        crate::ErrorCode::OutcomeUnclear,
                        "Zweite Exchangeantwort verloren",
                    ));
                }
                Ok(())
            },
            &mut |vfs, path| vfs.sync_dir(path),
        );
        assert_eq!(attempts, 2);
        let row = &report.rows[1];
        assert_eq!(row.old_name, "aab");
        assert_eq!(row.new_name, "aaab");
        assert_eq!(row.status, BatchRenameStatus::OutcomeUnclear);
        assert!(row.current_name.is_none());
        let named: HashSet<_> = row
            .message
            .split('„')
            .skip(1)
            .filter_map(|part| part.split_once('“').map(|(name, _)| name))
            .collect();
        // Exakte zitierte Namen vergleichen: „aaab“ ist kein Beleg für „ab“.
        assert_eq!(named, HashSet::from(["ab", "aab", "aaab"]));
    }

    fn chain_options() -> BatchRenameOptions {
        BatchRenameOptions {
            find: String::new(),
            prefix: "a".into(),
            ..options()
        }
    }

    #[test]
    fn batch_kette_gibt_ziele_zuerst_frei_und_erhaelt_vorschau_und_inhalte() {
        let (_temp, root) = fixture();
        for name in ["a", "aa", "aaa"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let selected = names(&["a", "aa", "aaa"]);
        let plan = PreparedBatchRename::prepare(&root, &selected, &chain_options(), None).unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Ready));
        assert_eq!(plan.execution_order, [2, 1, 0]);
        assert_eq!(
            plan.rows()
                .iter()
                .map(|row| &row.old_name)
                .collect::<Vec<_>>(),
            selected.iter().collect::<Vec<_>>()
        );
        assert!(plan.rows()[0].message.contains("„aa“"));
        let mut progress = Vec::new();
        let report = plan.execute(&Cancel::new(), &mut |done, total| {
            progress.push((done, total))
        });
        assert_eq!(progress, [(1, 3), (2, 3), (3, 3)]);
        assert_eq!(report.renamed, 3);
        assert_eq!(
            report
                .rows
                .iter()
                .map(|row| &row.old_name)
                .collect::<Vec<_>>(),
            selected.iter().collect::<Vec<_>>()
        );
        assert!(!root.join("a").exists());
        for (target, content) in [("aa", "a"), ("aaa", "aa"), ("aaaa", "aaa")] {
            assert_eq!(std::fs::read_to_string(root.join(target)).unwrap(), content);
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
    }

    #[test]
    fn batch_kette_ordner_und_nummerierung_behalten_auswahlindex_und_unterbaum() {
        let (_temp, root) = fixture();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::create_dir(root.join("1_a")).unwrap();
        std::fs::write(root.join("a/kind"), "erstes").unwrap();
        std::fs::write(root.join("1_a/kind"), "zweites").unwrap();
        let rules = BatchRenameOptions {
            find: String::new(),
            numbering: Some(BatchRenameNumbering {
                start: 1,
                step: 1,
                min_digits: 1,
                position: BatchRenameNumberPosition::Before,
            }),
            ..options()
        };
        let plan =
            PreparedBatchRename::prepare(&root, &names(&["a", "1_a"]), &rules, None).unwrap();
        assert_eq!(plan.execution_order, [1, 0]);
        assert_eq!(plan.rows()[0].new_name, "1_a");
        assert_eq!(plan.rows()[1].new_name, "2_1_a");
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 2);
        assert_eq!(
            std::fs::read_to_string(root.join("1_a/kind")).unwrap(),
            "erstes"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("2_1_a/kind")).unwrap(),
            "zweites"
        );
    }

    #[test]
    fn batch_kette_bleibt_remote_gesperrt_und_ungewaehltes_ziel_blockiert_vorgaenger() {
        let (_temp, root) = fixture();
        for name in ["a", "aa", "aaa", "b"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let listing = LocalVfs::rooted(&root)
            .unwrap()
            .list_complete(&root)
            .unwrap();
        let selected = names(&["a", "aa", "b"]);
        let (remote_rows, _) = plan_rows(&listing, &selected, &chain_options());
        assert_eq!(remote_rows[0].status, BatchRenameStatus::Blocked);
        assert_eq!(remote_rows[1].status, BatchRenameStatus::Blocked);
        let plan = PreparedBatchRename::prepare(&root, &selected, &chain_options(), None).unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[1].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[2].status, BatchRenameStatus::Ready);
        let mut progress = Vec::new();
        let report = plan.execute(&Cancel::new(), &mut |done, total| {
            progress.push((done, total))
        });
        assert_eq!(progress, [(1, 1)]);
        assert_eq!(report.renamed, 1);
        for name in ["a", "aa", "aaa"] {
            assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), name);
        }
    }

    #[test]
    fn batch_kette_blockiert_bei_ungueltigem_oder_unveraendertem_freigebendem_eintrag() {
        let (_temp, root) = fixture();
        let shorter = "a".repeat(254);
        let longer = "a".repeat(255);
        for name in [&shorter, &longer] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let plan = PreparedBatchRename::prepare(&root, &[shorter, longer], &chain_options(), None)
            .unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        for name in [" a ", "a", " b "] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let rules = BatchRenameOptions {
            find: String::new(),
            trim_outer_spaces: true,
            ..options()
        };
        let plan = PreparedBatchRename::prepare(&root, &names(&[" a ", "a", " b "]), &rules, None)
            .unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[1].status, BatchRenameStatus::Unchanged);
        assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 1);
    }

    #[test]
    fn batch_kette_abbruch_nach_freigabe_erhaelt_teilbilanz() {
        let (_temp, root) = fixture();
        for name in ["a", "aa", "aaa"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["a", "aa", "aaa"]),
            &chain_options(),
            None,
        )
        .unwrap();
        let cancel = Cancel::new();
        let report = plan.execute(&cancel, &mut |_, _| cancel.cancel());
        assert_eq!(report.renamed, 1);
        assert_eq!(report.cancelled, 2);
        assert_eq!(report.rows[2].status, BatchRenameStatus::Renamed);
        for (name, content) in [("a", "a"), ("aa", "aa"), ("aaaa", "aaa")] {
            assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), content);
        }
    }

    #[test]
    fn batch_kette_drift_spaete_kollision_und_unklarer_ausgang_stoppen_nur_abhaengige() {
        for fault in ["drift", "collision", "unclear"] {
            let (_temp, root) = fixture();
            for name in ["a", "aa", "aaa", "b"] {
                std::fs::write(root.join(name), name).unwrap();
            }
            let plan = PreparedBatchRename::prepare(
                &root,
                &names(&["a", "aa", "aaa", "b"]),
                &chain_options(),
                None,
            )
            .unwrap();
            let listing = plan.vfs.list_complete(&root);
            if fault == "drift" {
                std::fs::write(root.join("aaa"), "veraendert").unwrap();
            } else if fault == "collision" {
                // Nach dem Stapellisting: Die Einzelwirkung muss die Kollision schützen.
                std::fs::write(root.join("aaaa"), "fremd").unwrap();
            }
            let mut attempted = Vec::new();
            let report = plan.execute_with_rename(
                listing,
                &Cancel::new(),
                &mut |_, _| {},
                &mut |vfs, old, new, sandbox| {
                    attempted.push(new.to_string());
                    crate::fileops::rename(vfs, old, new, sandbox)?;
                    if fault == "unclear" && new == "aaaa" {
                        // Das Rename wirkte, aber die abschließende Bestätigung ging verloren.
                        return Err(crate::coded_error(
                            io::ErrorKind::Other,
                            crate::ErrorCode::OutcomeUnclear,
                            "Abschlussbeleg fehlt",
                        ));
                    }
                    Ok(())
                },
            );
            assert_eq!(report.renamed, 1, "{fault}");
            assert_eq!(report.rows[0].status, BatchRenameStatus::FailedUnchanged);
            assert_eq!(report.rows[1].status, BatchRenameStatus::FailedUnchanged);
            assert!(report.rows[0].message.contains("vorausgehende"));
            assert_eq!(
                report.rows[2].status,
                if fault == "unclear" {
                    BatchRenameStatus::OutcomeUnclear
                } else {
                    BatchRenameStatus::FailedUnchanged
                }
            );
            assert_eq!(report.rows[3].status, BatchRenameStatus::Renamed);
            assert!(!attempted.contains(&"aa".to_string()));
            assert!(!attempted.contains(&"aaa".to_string()));
            assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "a");
            assert_eq!(std::fs::read_to_string(root.join("aa")).unwrap(), "aa");
            if fault == "collision" {
                assert_eq!(std::fs::read_to_string(root.join("aaaa")).unwrap(), "fremd");
            }
        }
    }

    fn swap_options() -> BatchRenameOptions {
        BatchRenameOptions {
            find: ".x".into(),
            replacement: "".into(),
            prefix: "".into(),
            suffix: ".x".into(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        }
    }

    fn add_swap_pair(root: &Path, stem: &str) {
        std::fs::write(root.join(format!("{stem}.x")), b"datei").unwrap();
        std::fs::create_dir(root.join(format!("{stem}.x.x"))).unwrap();
        std::fs::write(root.join(format!("{stem}.x.x/kind")), b"ordner").unwrap();
    }

    fn assert_swapped(root: &Path, stem: &str) {
        assert_eq!(
            std::fs::read(root.join(format!("{stem}.x/kind"))).unwrap(),
            b"ordner"
        );
        assert_eq!(
            std::fs::read(root.join(format!("{stem}.x.x"))).unwrap(),
            b"datei"
        );
    }

    #[test]
    fn batch_swap_atomare_probe_und_syncfehler_erhalten_beide_inhalte() {
        for fail_sync in [false, true] {
            let (_temp, root) = fixture();
            add_swap_pair(&root, "a");
            let plan = PreparedBatchRename::prepare(
                &root,
                &names(&["a.x", "a.x.x"]),
                &swap_options(),
                Some(root.clone()),
            )
            .unwrap();
            assert!(plan
                .rows()
                .iter()
                .all(|row| row.status == BatchRenameStatus::Ready
                    && row.message.contains("atomarer Namenstausch")));
            let listing = plan.vfs.list_complete(&root);
            let mut attempts = 0;
            let mut progress = Vec::new();
            let report = plan.execute_with_actions(
                listing,
                &Cancel::new(),
                &mut |done, total| progress.push((done, total)),
                &mut crate::fileops::rename,
                &mut |vfs, left, right| {
                    attempts += 1;
                    vfs.rename_exchange(left, right)
                },
                &mut |vfs, path| {
                    if fail_sync {
                        Err(io::Error::other("injizierter Syncfehler"))
                    } else {
                        vfs.sync_dir(path)
                    }
                },
            );
            assert_eq!(attempts, 1);
            assert_eq!(progress, [(2, 2)]);
            assert_swapped(&root, "a");
            assert_eq!(
                report
                    .rows
                    .iter()
                    .map(|row| row.old_name.as_str())
                    .collect::<Vec<_>>(),
                ["a.x", "a.x.x"]
            );
            if fail_sync {
                assert_eq!(report.outcome_unclear, 2);
                for row in report.rows {
                    assert_eq!(row.code, Some(crate::ErrorCode::OutcomeUnclear));
                    assert!(row.message.contains(root.join("a.x").to_str().unwrap()));
                    assert!(row.message.contains(root.join("a.x.x").to_str().unwrap()));
                }
            } else {
                assert_eq!(report.renamed, 2);
            }
        }
    }

    #[test]
    fn batch_swap_mehrere_paare_und_unabhaengige_zeile_behalten_vorschaureihenfolge() {
        let (_temp, root) = fixture();
        add_swap_pair(&root, "a");
        add_swap_pair(&root, "b");
        std::fs::write(root.join("c.txt"), b"einzeln").unwrap();
        let selected = names(&["b.x.x", "c.txt", "a.x", "b.x", "a.x.x"]);
        let plan = PreparedBatchRename::prepare(&root, &selected, &swap_options(), None).unwrap();
        let mut progress = Vec::new();
        let report = plan.execute(&Cancel::new(), &mut |done, total| {
            progress.push((done, total))
        });
        assert_eq!(report.renamed, 5);
        assert_eq!(
            report
                .rows
                .iter()
                .map(|row| &row.old_name)
                .collect::<Vec<_>>(),
            selected.iter().collect::<Vec<_>>()
        );
        assert_eq!(progress, [(1, 5), (3, 5), (5, 5)]);
        assert_swapped(&root, "a");
        assert_swapped(&root, "b");
        assert_eq!(std::fs::read(root.join("c.x.txt")).unwrap(), b"einzeln");
    }

    #[test]
    fn batch_swap_quell_und_rootdrift_sowie_abbruch_starten_keinen_austausch() {
        for fault in ["source", "root", "cancel", "listing"] {
            let (_temp, root) = fixture();
            let pane = root.join("pane");
            std::fs::create_dir(&pane).unwrap();
            add_swap_pair(&pane, "a");
            let plan = PreparedBatchRename::prepare(
                &pane,
                &names(&["a.x", "a.x.x"]),
                &swap_options(),
                None,
            )
            .unwrap();
            let mut listing = plan.vfs.list_complete(&pane);
            let cancel = Cancel::new();
            match fault {
                "source" => {
                    std::fs::rename(pane.join("a.x.x"), pane.join("original")).unwrap();
                    std::fs::create_dir(pane.join("a.x.x")).unwrap();
                }
                "root" => {
                    std::fs::rename(&pane, root.join("original")).unwrap();
                    std::fs::create_dir(&pane).unwrap();
                }
                "cancel" => cancel.cancel(),
                "listing" => listing = Err(io::Error::other("Listing unlesbar")),
                _ => unreachable!(),
            }
            let report = plan.execute_with_actions(
                listing,
                &cancel,
                &mut |_, _| {},
                &mut crate::fileops::rename,
                &mut |_, _, _| panic!("Kein Exchange bei {fault}"),
                &mut |_, _| panic!("Kein Sync bei {fault}"),
            );
            assert_eq!(report.renamed, 0, "{fault}");
            assert_eq!(report.outcome_unclear, 0, "{fault}");
            if fault == "cancel" {
                assert_eq!(report.cancelled, 2);
            } else {
                assert_eq!(report.failed_unchanged, 2);
            }
            let original = if fault == "root" {
                root.join("original")
            } else {
                pane.clone()
            };
            assert_eq!(std::fs::read(original.join("a.x")).unwrap(), b"datei");
            let directory = if fault == "source" {
                original.join("original")
            } else {
                original.join("a.x.x")
            };
            assert_eq!(std::fs::read(directory.join("kind")).unwrap(), b"ordner");
        }
    }

    #[test]
    fn batch_swap_mutationsfehler_readback_und_spaeter_tausch_werden_paarweise_bilanziert() {
        for fault in [
            "unsupported",
            "effect_error",
            "effect_io_error",
            "readback",
            "late_replacement",
        ] {
            let (_temp, root) = fixture();
            add_swap_pair(&root, "a");
            let plan = PreparedBatchRename::prepare(
                &root,
                &names(&["a.x", "a.x.x"]),
                &swap_options(),
                None,
            )
            .unwrap();
            let listing = plan.vfs.list_complete(&root);
            let mut attempts = 0;
            let report = plan.execute_with_actions(
                listing,
                &Cancel::new(),
                &mut |_, _| {},
                &mut crate::fileops::rename,
                &mut |vfs, left, right| {
                    attempts += 1;
                    if fault == "unsupported" {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "Kein atomarer Austausch",
                        ));
                    }
                    if fault == "late_replacement" {
                        std::fs::rename(right, root.join("original_ordner")).unwrap();
                        std::fs::create_dir(right).unwrap();
                    }
                    vfs.rename_exchange(left, right)?;
                    if fault == "effect_io_error" {
                        return Err(io::Error::other("Unkodierter Fehler nach Wirkung"));
                    }
                    if fault == "effect_error" {
                        return Err(crate::coded_error(
                            io::ErrorKind::Other,
                            crate::ErrorCode::OutcomeUnclear,
                            "Antwort nach Wirkung verloren",
                        ));
                    }
                    if fault == "readback" {
                        std::fs::rename(right, root.join("original_datei")).unwrap();
                    }
                    Ok(())
                },
                &mut |vfs, path| vfs.sync_dir(path),
            );
            assert_eq!(attempts, 1, "{fault}");
            if fault == "unsupported" {
                assert_eq!(report.failed_unchanged, 2);
                assert_eq!(std::fs::read(root.join("a.x")).unwrap(), b"datei");
                assert_eq!(std::fs::read(root.join("a.x.x/kind")).unwrap(), b"ordner");
            } else {
                assert_eq!(report.outcome_unclear, 2, "{fault}");
                assert!(report
                    .rows
                    .iter()
                    .all(|row| row.code == Some(crate::ErrorCode::OutcomeUnclear)));
                if fault == "readback" {
                    assert_eq!(
                        std::fs::read(root.join("original_datei")).unwrap(),
                        b"datei"
                    );
                    assert_eq!(std::fs::read(root.join("a.x/kind")).unwrap(), b"ordner");
                } else if fault == "late_replacement" {
                    assert_eq!(std::fs::read(root.join("a.x.x")).unwrap(), b"datei");
                    assert_eq!(
                        std::fs::read(root.join("original_ordner/kind")).unwrap(),
                        b"ordner"
                    );
                } else {
                    assert_swapped(&root, "a");
                }
            }
        }
    }

    #[test]
    fn batch_swap_abbruch_nach_einzelzeile_laesst_das_ganze_paar_unveraendert() {
        let (_temp, root) = fixture();
        add_swap_pair(&root, "a");
        std::fs::write(root.join("b.txt"), b"einzeln").unwrap();
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["a.x", "b.txt", "a.x.x"]),
            &swap_options(),
            None,
        )
        .unwrap();
        let cancel = Cancel::new();
        let report = plan.execute(&cancel, &mut |_, _| cancel.cancel());
        assert_eq!(report.renamed, 1);
        assert_eq!(report.cancelled, 2);
        assert_eq!(std::fs::read(root.join("a.x")).unwrap(), b"datei");
        assert_eq!(std::fs::read(root.join("a.x.x/kind")).unwrap(), b"ordner");
        assert_eq!(std::fs::read(root.join("b.x.txt")).unwrap(), b"einzeln");
    }

    #[test]
    fn batch_swap_blockierter_partner_dreierzyklus_und_remote_bleiben_gesperrt() {
        let (_temp, root) = fixture();
        add_swap_pair(&root, "a");
        let vfs = LocalVfs::rooted(&root).unwrap();
        let listing = vfs.list_complete(&root).unwrap();
        let (rows, _) =
            plan_rows_with_chains(&listing, &names(&["a.x", "a.x.x"]), &swap_options(), true);
        let mut remote = rows.clone();
        assert!(chain_execution_plan(&listing, &mut remote).0.is_empty());
        assert!(remote
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        let mut blocked = rows;
        set_status(
            &mut blocked[1],
            BatchRenameStatus::Blocked,
            "Partner gesperrt",
        );
        let (order, _, partners) = local_execution_plan(&listing, &mut blocked);
        assert!(order.is_empty());
        assert!(partners.iter().all(Option::is_none));
        assert!(blocked
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        for name in ["b", "c", "d"] {
            std::fs::write(root.join(name), b"zyklus").unwrap();
        }
        let listing = vfs.list_complete(&root).unwrap();
        let mut cycle: Vec<_> = [("b", "c"), ("c", "d"), ("d", "b")]
            .into_iter()
            .map(|(old, new)| BatchRenameRow {
                old_name: old.into(),
                new_name: new.into(),
                current_name: None,
                status: BatchRenameStatus::Ready,
                message: "Bereit".into(),
                code: None,
            })
            .collect();
        assert!(local_execution_plan(&listing, &mut cycle).0.is_empty());
        assert!(cycle
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
    }

    #[test]
    fn batch_kette_zyklen_und_davon_abhaengige_zeilen_bleiben_blockiert() {
        let (_temp, root) = fixture();
        for name in ["a", "b", "c", "d"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let listing = LocalVfs::rooted(&root)
            .unwrap()
            .list_complete(&root)
            .unwrap();
        let mut rows: Vec<_> = [("a", "b"), ("b", "a"), ("c", "d"), ("d", "e")]
            .into_iter()
            .map(|(old, new)| BatchRenameRow {
                old_name: old.into(),
                new_name: new.into(),
                current_name: None,
                status: BatchRenameStatus::Ready,
                message: "Bereit".into(),
                code: None,
            })
            .collect();
        let (order, _) = chain_execution_plan(&listing, &mut rows);
        assert_eq!(order, [3, 2]);
        assert_eq!(rows[0].status, BatchRenameStatus::Blocked);
        assert_eq!(rows[1].status, BatchRenameStatus::Blocked);
        assert_eq!(rows[2].status, BatchRenameStatus::Ready);
        // Ein Selbstbezug nach konservativer Faltung bleibt ebenfalls gesperrt.
        rows[0].new_name = "A".into();
        rows[0].status = BatchRenameStatus::Ready;
        let (order, _) = chain_execution_plan(&listing, &mut rows);
        assert_eq!(order, [3, 2]);
        assert_eq!(rows[0].status, BatchRenameStatus::Blocked);
    }

    #[test]
    fn batch_namen_werden_backendseitig_mit_endungsregel_gebildet() {
        let options = BatchRenameOptions {
            prefix: "vor-".into(),
            suffix: "-nach".into(),
            ..options()
        };
        assert_eq!(
            candidate("alt.tar.gz", true, &options, 0).unwrap(),
            "vor-neu.tar-nach.gz"
        );
        assert_eq!(
            candidate(".alt", true, &options, 0).unwrap(),
            "vor-.neu-nach"
        );
        assert_eq!(
            candidate("alt.ordner", false, &options, 0).unwrap(),
            "vor-neu.ordner-nach"
        );
        assert_eq!(
            candidate(
                "alt.txt",
                true,
                &BatchRenameOptions {
                    preserve_extension: false,
                    trim_outer_spaces: false,
                    numbering: None,
                    ..options
                },
                0
            )
            .unwrap(),
            "vor-neu.txt-nach"
        );
    }

    #[test]
    fn batch_zu_lange_ergebnisse_werden_vor_allokation_begrenzt_und_blockierte_ziele_nicht_reserviert(
    ) {
        let (_temp, root) = fixture();
        std::fs::write(root.join("aa"), b"x").unwrap();
        std::os::unix::fs::symlink("aa", root.join("a")).unwrap();
        let oversized = BatchRenameOptions {
            find: "a".into(),
            replacement: "x".repeat(4096),
            ..options()
        };
        let plan = PreparedBatchRename::prepare(&root, &names(&["aa"]), &oversized, None).unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[0].new_name, "—");
        let rules = BatchRenameOptions {
            find: "a".into(),
            replacement: "".into(),
            prefix: "n".into(),
            ..options()
        };
        let plan = PreparedBatchRename::prepare(&root, &names(&["a", "aa"]), &rules, None).unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[1].status, BatchRenameStatus::Ready);
        assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 1);
        assert!(root.join("n").is_file());
        assert!(std::fs::symlink_metadata(root.join("a"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn batch_kollisionen_sperren_nur_betroffene_zeilen_und_ersetzen_nichts() {
        let (_temp, root) = fixture();
        for name in ["alt-a.txt", "alt-b.txt", "NEU-A.txt"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["alt-a.txt", "alt-b.txt"]),
            &options(),
            None,
        )
        .unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[1].status, BatchRenameStatus::Ready);
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 1);
        assert!(root.join("alt-a.txt").is_file());
        assert_eq!(
            std::fs::read_to_string(root.join("NEU-A.txt")).unwrap(),
            "NEU-A.txt"
        );
        assert!(root.join("neu-b.txt").is_file());
    }

    #[test]
    fn batch_drift_und_spaetes_ziel_bleiben_unveraendert_bei_unabhaengigem_erfolg() {
        let (_temp, root) = fixture();
        for name in ["alt-a", "alt-b", "alt-c"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["alt-a", "alt-b", "alt-c"]),
            &options(),
            None,
        )
        .unwrap();
        std::fs::rename(root.join("alt-a"), root.join("weg")).unwrap();
        std::fs::write(root.join("alt-a"), "fremd").unwrap();
        std::fs::write(root.join("neu-b"), "spät").unwrap();
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 1);
        assert_eq!(report.failed_unchanged, 2);
        assert_eq!(
            std::fs::read_to_string(root.join("alt-a")).unwrap(),
            "fremd"
        );
        assert_eq!(std::fs::read_to_string(root.join("neu-b")).unwrap(), "spät");
    }

    #[test]
    fn batch_abbruch_bilanziert_bereits_erfolgte_zeilen_und_startet_keine_weiteren() {
        let (_temp, root) = fixture();
        for name in ["alt-a", "alt-b", "alt-c"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["alt-a", "alt-b", "alt-c"]),
            &options(),
            None,
        )
        .unwrap();
        let cancel = Cancel::new();
        let trigger = cancel.clone();
        let report = plan.execute(&cancel, &mut |_, _| trigger.cancel());
        assert_eq!(report.renamed, 1);
        assert_eq!(report.cancelled, 2);
        assert!(root.join("neu-a").is_file());
        assert!(root.join("alt-b").is_file());
    }

    #[test]
    fn batch_startlistingfehler_behalten_originalcode_trotz_erfolgreicher_spaeterer_stats() {
        for (error, expected_code) in [
            (
                io::Error::from_raw_os_error(libc::EIO),
                crate::ErrorCode::Io,
            ),
            (
                io::Error::new(io::ErrorKind::PermissionDenied, "Listing verweigert"),
                crate::ErrorCode::PermissionDenied,
            ),
            (
                crate::coded_error(
                    io::ErrorKind::PermissionDenied,
                    crate::ErrorCode::PlanDrift,
                    "Gebundenes Listing verändert",
                ),
                crate::ErrorCode::PlanDrift,
            ),
        ] {
            let (_temp, root) = fixture();
            std::fs::write(root.join("alt"), b"x").unwrap();
            let plan =
                PreparedBatchRename::prepare(&root, &names(&["alt"]), &options(), None).unwrap();
            assert!(plan.vfs.local_root_identity(&root).is_ok());
            assert!(plan.vfs.stat(&root.join("alt")).is_ok());
            let report = plan.execute_with_listing(Err(error), &Cancel::new(), &mut |_, _| {});
            assert_eq!(report.failed_unchanged, 1);
            assert_eq!(report.renamed, 0);
            assert_eq!(report.rows[0].code, Some(expected_code));
            assert!(root.join("alt").is_file());
            assert!(!root.join("neu").exists());
        }
    }

    #[test]
    fn batch_bestehende_leerzeichen_im_quellnamen_werden_nicht_still_getrimmt() {
        let (_temp, root) = fixture();
        std::fs::write(root.join(" alt.txt"), b"x").unwrap();
        let rules = BatchRenameOptions {
            prefix: "vor-".into(),
            ..options()
        };
        let plan =
            PreparedBatchRename::prepare(&root, &names(&[" alt.txt"]), &rules, None).unwrap();
        assert_eq!(plan.rows()[0].new_name, "vor- neu.txt");
        assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 1);
        assert!(root.join("vor- neu.txt").is_file());
    }

    #[test]
    fn batch_roottausch_und_nichtregulaere_quellen_erhalten_den_bestand() {
        let (_temp, container) = fixture();
        let root = container.join("pane");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("alt"), b"original").unwrap();
        std::os::unix::fs::symlink("alt", root.join("link")).unwrap();
        let plan = PreparedBatchRename::prepare(&root, &names(&["alt", "link"]), &options(), None)
            .unwrap();
        assert_eq!(plan.rows()[1].status, BatchRenameStatus::Blocked);
        let displaced = container.join("vorher");
        std::fs::rename(&root, &displaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("alt"), b"fremd").unwrap();
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 0);
        assert_eq!(report.failed_unchanged, 1);
        assert_eq!(report.rows[0].code, Some(crate::ErrorCode::PlanDrift));
        assert_eq!(std::fs::read(root.join("alt")).unwrap(), b"fremd");
        assert_eq!(std::fs::read(displaced.join("alt")).unwrap(), b"original");
    }

    #[test]
    fn batch_grosse_datei_braucht_keinen_inhaltsdurchlauf_und_doppelte_ziele_bleiben_stehen() {
        let (_temp, root) = fixture();
        let file = std::fs::File::create(root.join("alt-gross")).unwrap();
        file.set_len(2 * 1024 * 1024 * 1024).unwrap();
        let plan =
            PreparedBatchRename::prepare(&root, &names(&["alt-gross"]), &options(), None).unwrap();
        assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 1);
        assert_eq!(
            std::fs::metadata(root.join("neu-gross")).unwrap().len(),
            2 * 1024 * 1024 * 1024
        );
        for name in ["a.txt", "aa.txt"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let rules = BatchRenameOptions {
            find: "a".into(),
            replacement: "".into(),
            ..options()
        };
        let plan = PreparedBatchRename::prepare(&root, &names(&["a.txt", "aa.txt"]), &rules, None)
            .unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 0);
    }

    #[test]
    fn batch_unicode_doppelte_ziele_und_ungueltige_zeilen_werden_erklaert() {
        let (_temp, root) = fixture();
        for name in ["alt.txt", "neu-é.txt", "alt-e\u{301}.txt"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["alt-e\u{301}.txt", "../fremd", "fehlt"]),
            &options(),
            None,
        )
        .unwrap();
        assert!(plan
            .rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        let options = BatchRenameOptions {
            find: "alt".into(),
            replacement: "".into(),
            ..options()
        };
        let plan =
            PreparedBatchRename::prepare(&root, &names(&["alt.txt"]), &options, None).unwrap();
        assert_eq!(plan.rows()[0].new_name, ".txt");
        let mut expired = plan;
        expired.created = Instant::now() - PLAN_LIFETIME - Duration::from_secs(1);
        assert_eq!(
            expired
                .execute(&Cancel::new(), &mut |_, _| {})
                .failed_unchanged,
            1
        );
    }
    fn numbered(
        start: u32,
        step: u32,
        min_digits: u8,
        position: BatchRenameNumberPosition,
    ) -> BatchRenameOptions {
        BatchRenameOptions {
            find: String::new(),
            replacement: String::new(),
            prefix: String::new(),
            suffix: String::new(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: Some(BatchRenameNumbering {
                start,
                step,
                min_digits,
                position,
            }),
        }
    }

    #[test]
    fn nummerierung_bindet_reihenfolge_endungen_und_blockierten_index() {
        use std::os::unix::fs::symlink;
        let (_area, root) = fixture();
        std::fs::write(root.join("z.tar.gz"), b"Z").unwrap();
        std::fs::write(root.join("a.txt"), b"A").unwrap();
        std::fs::write(root.join(".profil"), b"P").unwrap();
        std::fs::create_dir(root.join("empty.dir")).unwrap();
        std::fs::write(root.join("empty.dir/Kind"), b"Kind bleibt").unwrap();
        symlink(root.join("a.txt"), root.join("link")).unwrap();
        let selection = names(&["z.tar.gz", "link", "a.txt", "empty.dir", ".profil"]);
        let plan = PreparedBatchRename::prepare(
            &root,
            &selection,
            &numbered(10, 5, 3, BatchRenameNumberPosition::Before),
            None,
        )
        .unwrap();
        assert_eq!(
            plan.rows()
                .iter()
                .map(|row| row.new_name.as_str())
                .collect::<Vec<_>>(),
            [
                "010_z.tar.gz",
                "015_link",
                "020_a.txt",
                "025_empty.dir",
                "030_.profil"
            ]
        );
        assert_eq!(plan.rows()[1].status, BatchRenameStatus::Blocked);
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 4);
        assert_eq!(std::fs::read(root.join("020_a.txt")).unwrap(), b"A");
        assert_eq!(
            std::fs::read(root.join("025_empty.dir/Kind")).unwrap(),
            b"Kind bleibt"
        );
        assert!(std::fs::symlink_metadata(root.join("link"))
            .unwrap()
            .is_symlink());
    }

    #[test]
    fn nummerierung_steht_nach_textregeln_aber_vor_geschuetzter_endung() {
        let mut rules = numbered(0, 2, 3, BatchRenameNumberPosition::After);
        rules.find = "alt".into();
        rules.replacement = "neu".into();
        rules.prefix = "vor-".into();
        rules.suffix = "-nach".into();
        assert_eq!(
            candidate("alt.tar.gz", true, &rules, 0).unwrap(),
            "vor-neu.tar-nach_000.gz"
        );
        assert_eq!(
            candidate("alt.tar.gz", true, &rules, 5).unwrap(),
            "vor-neu.tar-nach_010.gz"
        );
        assert_eq!(
            candidate("alt.dir", false, &rules, 1).unwrap(),
            "vor-neu.dir-nach_002"
        );
        rules.preserve_extension = false;
        assert_eq!(
            candidate("alt.tar.gz", true, &rules, 1).unwrap(),
            "vor-neu.tar.gz-nach_002"
        );
        rules.numbering.as_mut().unwrap().start = 999;
        assert_eq!(
            candidate("alt", true, &rules, 1).unwrap(),
            "vor-neu-nach_1001"
        );
    }

    #[test]
    fn nummerierung_bereinigt_keine_kollision_und_nummeriert_nach_abbruch_nicht_neu() {
        let (_area, root) = fixture();
        for name in ["a", "b", "c"] {
            std::fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        std::fs::write(root.join("001_a"), b"bestehend").unwrap();
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&["a", "b", "c"]),
            &numbered(1, 1, 3, BatchRenameNumberPosition::Before),
            None,
        )
        .unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        let cancel = Cancel::new();
        let report = plan.execute(&cancel, &mut |_, _| cancel.cancel());
        assert_eq!((report.renamed, report.cancelled), (1, 1));
        assert_eq!(std::fs::read(root.join("001_a")).unwrap(), b"bestehend");
        assert_eq!(std::fs::read(root.join("002_b")).unwrap(), b"b");
        assert!(root.join("c").exists());
        assert_eq!(report.rows[2].new_name, "003_c");
        assert!(!root.join("002_c").exists());
    }

    #[test]
    fn nummerierung_zaehlt_utf8_bytes_vor_namensaufbau() {
        let (_area, root) = fixture();
        let fits = "😀".repeat(63);
        let exceeds = format!("{fits}a");
        for name in [&fits, &exceeds, "good.txt"] {
            std::fs::write(root.join(name), b"Inhalt").unwrap();
        }
        let selection = vec![fits, exceeds, "good.txt".into()];
        let plan = PreparedBatchRename::prepare(
            &root,
            &selection,
            &numbered(1, 1, 2, BatchRenameNumberPosition::Before),
            None,
        )
        .unwrap();
        assert_eq!(plan.rows()[0].new_name.len(), 255);
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Ready);
        assert_eq!(plan.rows()[1].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[1].new_name, "—");
        assert_eq!(plan.rows()[2].new_name, "03_good.txt");
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 2);
        assert_eq!(std::fs::read(root.join("03_good.txt")).unwrap(), b"Inhalt");
        assert!(root.join(&selection[1]).exists());
    }

    #[test]
    fn nummerierungsoptionen_werden_vor_rootzugriff_validiert_und_zaehler_ueberlaufen_nicht() {
        let selection = names(&["a"]);
        for (step, digits) in [(0, 3), (1, 0), (1, 21)] {
            let rules = numbered(0, step, digits, BatchRenameNumberPosition::Before);
            let error = PreparedBatchRename::prepare(
                Path::new("/definitely-missing-numbering-root"),
                &selection,
                &rules,
                None,
            )
            .err()
            .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("Nummerierung"));
        }
        let rules = numbered(u32::MAX, u32::MAX, 1, BatchRenameNumberPosition::Before);
        assert_eq!(
            rules.numbering.as_ref().unwrap().counter(2).unwrap(),
            12_884_901_885
        );
        #[cfg(target_pointer_width = "64")]
        assert!(rules
            .numbering
            .as_ref()
            .unwrap()
            .counter(usize::MAX)
            .is_err());
        for json in [
            r#"{"start":-1,"step":1,"minDigits":3,"position":"before"}"#,
            r#"{"start":4294967296,"step":1,"minDigits":3,"position":"before"}"#,
            r#"{"start":1,"step":1,"minDigits":3,"position":"middle"}"#,
        ] {
            assert!(serde_json::from_str::<BatchRenameNumbering>(json).is_err());
        }
    }

    #[test]
    fn alte_optionen_ohne_nummerierung_bleiben_identisch_und_camelcase_bleibt_gebunden() {
        let legacy = serde_json::json!({"find":"alt","replacement":"neu","prefix":"","suffix":"","preserveExtension":true});
        let mut options: BatchRenameOptions = serde_json::from_value(legacy.clone()).unwrap();
        assert!(options.numbering.is_none());
        assert_eq!(serde_json::to_value(&options).unwrap(), legacy);
        assert_eq!(candidate("alt.txt", true, &options, 99).unwrap(), "neu.txt");
        options.numbering = Some(BatchRenameNumbering {
            start: 7,
            step: 3,
            min_digits: 2,
            position: BatchRenameNumberPosition::After,
        });
        let json = serde_json::to_value(&options).unwrap();
        assert_eq!(json["numbering"]["minDigits"], 2);
        assert_eq!(json["numbering"]["position"], "after");
        let restored: BatchRenameOptions = serde_json::from_value(json).unwrap();
        assert_eq!(restored.numbering, options.numbering);
    }
    #[test]
    fn batch_trim_kollision_erhaelt_ziel_und_unabhaengige_datei_laeuft() {
        let (_temp, root) = fixture();
        for (name, bytes) in [
            (" a.txt ", "Quelle"),
            ("a.txt", "Ziel"),
            (" good.txt ", "Gut"),
        ] {
            std::fs::write(root.join(name), bytes).unwrap();
        }
        let rules: BatchRenameOptions = serde_json::from_value(serde_json::json!({
            "find":"", "replacement":"", "prefix":"", "suffix":"",
            "preserveExtension":true, "trimOuterSpaces":true
        }))
        .unwrap();
        let plan =
            PreparedBatchRename::prepare(&root, &names(&[" a.txt ", " good.txt "]), &rules, None)
                .unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.rows()[1].new_name, "good.txt");
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 1);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "Ziel");
        assert_eq!(
            std::fs::read_to_string(root.join(" a.txt ")).unwrap(),
            "Quelle"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("good.txt")).unwrap(),
            "Gut"
        );
    }

    #[test]
    fn batch_trim_zielgruppen_und_leerresultat_blockieren_nur_betroffene_zeilen() {
        let (_temp, root) = fixture();
        for name in [" a ", "a ", "   ", " good "] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let rules = BatchRenameOptions {
            find: String::new(),
            trim_outer_spaces: true,
            ..options()
        };
        let plan = PreparedBatchRename::prepare(
            &root,
            &names(&[" a ", "a ", "   ", " good "]),
            &rules,
            None,
        )
        .unwrap();
        assert!(plan.rows()[..3]
            .iter()
            .all(|row| row.status == BatchRenameStatus::Blocked));
        assert_eq!(plan.rows()[3].new_name, "good");
        let report = plan.execute(&Cancel::new(), &mut |_, _| {});
        assert_eq!(report.renamed, 1);
        for name in [" a ", "a ", "   "] {
            assert!(root.join(name).is_file());
        }
        assert!(!root.join("a").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("good")).unwrap(),
            " good "
        );
    }

    #[test]
    fn batch_trim_nur_u0020_vor_endung_regeln_und_nummerierung() {
        let mut rules = numbered(2, 1, 2, BatchRenameNumberPosition::After);
        rules.trim_outer_spaces = true;
        rules.find = "alt".into();
        rules.replacement = "neu".into();
        rules.prefix = " vor ".into();
        rules.suffix = " nach ".into();
        assert_eq!(
            candidate(" alt mitte.tar.gz ", true, &rules, 0).unwrap(),
            " vor neu mitte.tar nach _02.gz"
        );
        assert_eq!(
            candidate(" alt.ordner ", false, &rules, 1).unwrap(),
            " vor neu.ordner nach _03"
        );
        assert_eq!(
            candidate(" .profil ", true, &rules, 2).unwrap(),
            " vor .profil nach _04"
        );
        rules.preserve_extension = false;
        assert_eq!(
            candidate(" alt.txt ", true, &rules, 3).unwrap(),
            " vor neu.txt nach _05"
        );
        let plain = BatchRenameOptions {
            find: String::new(),
            trim_outer_spaces: true,
            ..options()
        };
        for name in [
            "\u{a0} a \u{a0}",
            "\u{3000}a\u{3000}",
            "a  b",
            "e\u{301}",
            "é",
        ] {
            assert_eq!(
                candidate(&format!("  {name}  "), true, &plain, 0).unwrap(),
                name
            );
        }
        assert_eq!(candidate("\ta\t", true, &plain, 0).unwrap(), "\ta\t");
    }

    #[test]
    fn batch_trim_realer_ordner_und_abbruch_nach_erster_wirkung() {
        let (_temp, root) = fixture();
        std::fs::create_dir(root.join(" Ordner ")).unwrap();
        std::fs::write(root.join(" Ordner /Kind"), b"Kind").unwrap();
        std::fs::write(root.join(" Datei.txt "), b"Datei").unwrap();
        let rules = BatchRenameOptions {
            find: String::new(),
            trim_outer_spaces: true,
            ..options()
        };
        let plan =
            PreparedBatchRename::prepare(&root, &names(&[" Ordner ", " Datei.txt "]), &rules, None)
                .unwrap();
        let cancel = Cancel::new();
        let trigger = cancel.clone();
        let report = plan.execute(&cancel, &mut |_, _| trigger.cancel());
        assert_eq!(report.renamed, 1);
        assert_eq!(report.cancelled, 1);
        assert_eq!(std::fs::read(root.join("Ordner/Kind")).unwrap(), b"Kind");
        assert!(root.join(" Datei.txt ").is_file());
        assert!(!root.join("Datei.txt").exists());
    }

    #[test]
    fn batch_trim_utf8_laenge_wird_nach_trim_vor_aufbau_geprueft() {
        let (_temp, root) = fixture();
        let source = format!(" {}a ", "😀".repeat(62));
        std::fs::write(root.join(&source), b"Unicode").unwrap();
        let mut rules = BatchRenameOptions {
            find: String::new(),
            prefix: "123456".into(),
            trim_outer_spaces: true,
            ..options()
        };
        let plan = PreparedBatchRename::prepare(&root, &names(&[&source]), &rules, None).unwrap();
        assert_eq!(plan.rows()[0].new_name.len(), 255);
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Ready);
        rules.prefix.push('7');
        let blocked =
            PreparedBatchRename::prepare(&root, &names(&[&source]), &rules, None).unwrap();
        assert_eq!(blocked.rows()[0].status, BatchRenameStatus::Blocked);
        assert_eq!(plan.execute(&Cancel::new(), &mut |_, _| {}).renamed, 1);
        let expected = format!("123456{}a", "😀".repeat(62));
        assert_eq!(std::fs::read(root.join(expected)).unwrap(), b"Unicode");
    }

    #[test]
    fn batch_trim_alte_json_requests_bleiben_ohne_neues_feld_identisch() {
        let legacy = serde_json::json!({"find":"", "replacement":"", "prefix":"", "suffix":"", "preserveExtension":true});
        let mut rules: BatchRenameOptions = serde_json::from_value(legacy.clone()).unwrap();
        assert!(!rules.trim_outer_spaces);
        assert_eq!(candidate(" a.txt ", true, &rules, 0).unwrap(), " a.txt ");
        assert_eq!(serde_json::to_value(&rules).unwrap(), legacy);
        rules.trim_outer_spaces = true;
        let json = serde_json::to_value(&rules).unwrap();
        assert_eq!(json["trimOuterSpaces"], true);
        let restored: BatchRenameOptions = serde_json::from_value(json).unwrap();
        assert!(restored.trim_outer_spaces);
        let mut invalid = legacy;
        invalid["trimOuterSpaces"] = serde_json::json!("true");
        assert!(serde_json::from_value::<BatchRenameOptions>(invalid).is_err());
    }
}
