//! Geschlossene Dateinamenzyklen: dauerhafte Teilbilanz, kein Rücktausch oder Retry.
use crate::batch_rename::{BatchRenameRow, BatchRenameStatus};
use crate::remote_rename::folded_name;
use crate::vfs::CompleteDirectoryListing;
use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameCycleMove {
    pub member: usize,
    pub from: String,
    pub to: String,
}

/// Ausschließlich Recoverydaten. Weder diese Abbildung noch ihr Queueeintrag
/// dürfen nach einem Neustart als ausführbarer Renameplan verwendet werden.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameCycleRecord {
    pub id: String,
    pub root: String,
    pub identity: crate::RootIdentity,
    pub old_names: Vec<String>,
    pub new_names: Vec<String>,
    pub current_names: Vec<String>,
    pub temp_name: String,
    pub pending: Option<RenameCycleMove>,
    pub finished: bool,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\0']) && !matches!(name, "." | "..")
}

impl RenameCycleRecord {
    pub fn validate(&self) -> io::Result<()> {
        let count = self.old_names.len();
        let old: HashSet<_> = self.old_names.iter().collect();
        let new: HashSet<_> = self.new_names.iter().collect();
        let current: HashSet<_> = self.current_names.iter().collect();
        if self.id.len() != 32
            || !self.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || crate::validate_file_operation_root(Path::new(&self.root))? != Path::new(&self.root)
            || !(2..=10_000).contains(&count)
            || self.new_names.len() != count
            || self.current_names.len() != count
            || old.len() != count
            || old != new
            || current.len() != count
            || self.temp_name != format!(".doppeldecker-rename-{}", self.id)
            || old.contains(&self.temp_name)
            || self
                .old_names
                .iter()
                .chain(&self.new_names)
                .any(|name| !valid_name(name))
            || self
                .current_names
                .iter()
                .any(|name| !old.contains(name) && *name != self.temp_name)
            || self
                .old_names
                .iter()
                .zip(&self.new_names)
                .any(|(old, new)| old == new)
            || (self.finished && (self.pending.is_some() || self.current_names != self.new_names))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ungültige Recoverybilanz eines entfernten Namenstauschs",
            ));
        }
        let actual_root = match &self.identity {
            crate::RootIdentity::Sftp(value) => {
                value.validate()?;
                &value.virtual_root
            }
            crate::RootIdentity::Ftps(value) => {
                value.validate()?;
                &value.virtual_root
            }
            crate::RootIdentity::WebDav(value) => {
                value.validate()?;
                &value.virtual_root
            }
            crate::RootIdentity::Smb(value) => {
                value.validate()?;
                &value.virtual_root
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Namenstausch-Recovery ohne Remote-Identität",
                ))
            }
        };
        if actual_root != &self.root
            || self
                .old_names
                .iter()
                .map(|name| folded_name(name))
                .collect::<HashSet<_>>()
                .len()
                != count
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Root oder exakte Namen der Recovery sind ungültig",
            ));
        }
        if let Some(step) = &self.pending {
            if step.member >= count
                || self.current_names[step.member] != step.from
                || !valid_name(&step.to)
                || (!old.contains(&step.to) && step.to != self.temp_name)
                || current.contains(&step.to)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Ungültiger offener Rename-Schritt",
                ));
            }
        }
        Ok(())
    }

    pub fn recovery_paths(&self) -> Vec<String> {
        let root = Path::new(&self.root);
        let mut paths: Vec<_> = self
            .old_names
            .iter()
            .chain(&self.current_names)
            .chain(std::iter::once(&self.temp_name))
            .map(|name| root.join(name).to_string_lossy().into_owned())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

pub(crate) struct RemoteRenameCycle {
    pub members: Vec<usize>,
    pub id: String,
    pub temp_name: String,
}

/// Plant Datei- und Ordnerzyklen. Die Ausführung prüft zusätzlich alle betroffenen
/// Unterwurzeln gegen offene Sync-/Staging-Intents.
pub(crate) fn plan_cycles(
    listing: &CompleteDirectoryListing,
    rows: &[BatchRenameRow],
) -> io::Result<Vec<RemoteRenameCycle>> {
    if !listing.complete {
        return Ok(Vec::new());
    }
    let mut occupants: HashMap<String, Vec<&crate::Entry>> = HashMap::new();
    for item in &listing.entries {
        occupants
            .entry(folded_name(&item.entry.name))
            .or_default()
            .push(&item.entry);
    }
    let sources: HashMap<_, _> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.status == BatchRenameStatus::Ready
                && row.old_name != row.new_name
                && folded_name(&row.old_name) != folded_name(&row.new_name)
                && occupants
                    .get(&folded_name(&row.old_name))
                    .is_some_and(|entries| {
                        entries.len() == 1
                            && entries[0].name == row.old_name
                            && (entries[0].is_file || entries[0].is_dir)
                            && !entries[0].is_symlink
                    })
        })
        .map(|(index, row)| (row.old_name.as_str(), index))
        .collect();
    let mut visited = HashSet::new();
    let mut cycles = Vec::new();
    for start in 0..rows.len() {
        let mut route = Vec::new();
        let mut positions = HashMap::new();
        let mut current = start;
        while !visited.contains(&current) {
            if let Some(&position) = positions.get(&current) {
                let members = &route[position..];
                let mut identities = HashSet::new();
                if members.len() >= 2
                    && members.iter().all(|&index: &usize| {
                        let entry = occupants[&folded_name(&rows[index].old_name)][0];
                        entry
                            .device
                            .zip(entry.inode)
                            .is_none_or(|identity| identities.insert(identity))
                    })
                {
                    let mut nonce = [0; 16];
                    getrandom::fill(&mut nonce)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    let id: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
                    let temp_name = format!(".doppeldecker-rename-{id}");
                    if occupants.contains_key(&folded_name(&temp_name))
                        || rows
                            .iter()
                            .any(|row| folded_name(&row.new_name) == folded_name(&temp_name))
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "Zwischenname ist bereits belegt; erneut vorbereiten",
                        ));
                    }
                    cycles.push(RemoteRenameCycle {
                        members: members.to_vec(),
                        id,
                        temp_name,
                    });
                }
                break;
            }
            if !sources.contains_key(rows[current].old_name.as_str()) {
                break;
            }
            positions.insert(current, route.len());
            route.push(current);
            let Some(&next) = sources.get(rows[current].new_name.as_str()) else {
                break;
            };
            current = next;
        }
        visited.extend(route);
    }
    Ok(cycles)
}

pub(crate) fn steps(members: usize) -> Vec<usize> {
    std::iter::once(0)
        .chain((1..members).rev())
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vfs;

    fn row(old: &str, new: &str) -> BatchRenameRow {
        BatchRenameRow {
            old_name: old.into(),
            new_name: new.into(),
            current_name: None,
            status: BatchRenameStatus::Ready,
            message: String::new(),
            code: None,
        }
    }

    #[test]
    fn exact_rings_use_one_free_slot_without_reordering_the_visible_rows() {
        for count in [2, 3, 9] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let names: Vec<_> = (0..count).map(|index| format!("file{index}")).collect();
            for (index, name) in names.iter().enumerate() {
                if index % 2 == 0 {
                    std::fs::write(root.join(name), name).unwrap();
                } else {
                    std::fs::create_dir(root.join(name)).unwrap();
                    std::fs::write(root.join(name).join("child"), name).unwrap();
                }
            }
            let rows: Vec<_> = names
                .iter()
                .enumerate()
                .map(|(index, name)| row(name, &names[(index + 1) % count]))
                .collect();
            let listing = crate::LocalVfs::rooted(&root)
                .unwrap()
                .list_complete(&root)
                .unwrap();
            let cycles = plan_cycles(&listing, &rows).unwrap();
            assert_eq!(cycles.len(), 1);
            let cycle = &cycles[0];
            assert_eq!(cycle.members, (0..count).collect::<Vec<_>>());
            let mut locations = names.clone();
            for (position, member) in steps(count).into_iter().enumerate() {
                let to = if position == 0 {
                    cycle.temp_name.clone()
                } else {
                    rows[member].new_name.clone()
                };
                assert!(!locations.contains(&to));
                locations[member] = to;
            }
            assert_eq!(
                locations,
                rows.iter()
                    .map(|row| row.new_name.clone())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                rows.iter()
                    .map(|row| row.old_name.clone())
                    .collect::<Vec<_>>(),
                names
            );
        }
    }

    #[test]
    fn symlinks_hardlink_aliases_partial_listings_and_unselected_occupants_are_not_cycles() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("a"), b"a").unwrap();
        std::fs::write(root.join("b"), b"b").unwrap();
        let vfs = crate::LocalVfs::rooted(&root).unwrap();
        let mut listing = vfs.list_complete(&root).unwrap();
        assert!(plan_cycles(&listing, &[row("a", "b")]).unwrap().is_empty());
        listing.complete = false;
        assert!(plan_cycles(&listing, &[row("a", "b"), row("b", "a")])
            .unwrap()
            .is_empty());
        listing.complete = true;
        listing.entries[1].entry.is_file = false;
        listing.entries[1].entry.is_symlink = true;
        assert!(plan_cycles(&listing, &[row("a", "b"), row("b", "a")])
            .unwrap()
            .is_empty());
        std::fs::remove_file(root.join("b")).unwrap();
        std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
        let listing = vfs.list_complete(&root).unwrap();
        assert!(plan_cycles(&listing, &[row("a", "b"), row("b", "a")])
            .unwrap()
            .is_empty());
    }
}
