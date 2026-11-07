//! Private additive SMB-Lesequelle mit vollständigen Namensgruppenbelegen
//! und Dateifakten am gehaltenen Inhalts-Handle.

use super::*;
use crate::vfs::{InteractiveSourceTree, SourceReadOmission};
use crate::{coded_error, Cancel, ErrorCode};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) const SMB_CONTENT_READ_LIMITATION: &str = "Additive SMB-Kopien zum lokalen Ziel binden vollständige Listings und ausgelassene Namensgruppen. Dateisystem-ID, ChangeTime und Dateifakten werden am geöffneten Handle vor dem Lesen und nach echtem EOF geprüft. Fehlt eine Dateisystem-ID (ID 0), bleibt die Kopie ausschließlich an die gemeldeten Attribute gebunden; ein Austausch mit identischen Attributen kann dann unbemerkt bleiben; vor dem lokalen Commit werden die benötigten Eltern frisch geprüft. Gleichzeitige Pfadänderungen zwischen diesen Prüfungen bleiben eine Best-Effort-Grenze. Baumbelege benötigen Speicher proportional zum beobachteten Bestand.";

#[derive(Clone)]
struct InteractiveFiles {
    provider: Arc<SmbVfs>,
    pane_root: PathBuf,
    identity: SmbServerIdentity,
    generation: u64,
    nodes: Arc<BTreeMap<PathBuf, Entry>>,
    listings: Arc<BTreeMap<PathBuf, BTreeMap<String, ProofListEntry>>>,
    children: Arc<BTreeMap<PathBuf, Vec<PathBuf>>>,
    groups: Arc<BTreeMap<PathBuf, BTreeSet<String>>>,
    omission_listings: Arc<BTreeSet<PathBuf>>,
    boundary_names: Arc<BTreeMap<PathBuf, BTreeSet<String>>>,
    tree: Arc<InteractiveSourceTree>,
    cancel: Cancel,
}

fn drift(message: &str) -> io::Error {
    coded_error(io::ErrorKind::InvalidData, ErrorCode::PlanDrift, message)
}

fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "SMB-Kopie abgebrochen",
        ));
    }
    Ok(())
}

impl SmbVfs {
    pub(crate) fn interactive_files(
        self: &Arc<Self>,
        root: &Path,
        items: &[PathBuf],
        cancel: &Cancel,
    ) -> io::Result<Arc<dyn Vfs>> {
        let _ = virtual_components(root)?;
        if !root.is_absolute() || items.is_empty() {
            return Err(invalid(
                "SMB-Inhaltsauswahl benötigt einen absoluten Pane-Root und direkte Einträge",
            ));
        }
        let mut seen = BTreeSet::new();
        for path in items {
            let _ = virtual_components(path)?;
            if !path.is_absolute() || path.parent() != Some(root) || !seen.insert(path) {
                return Err(invalid(
                    "SMB-Inhaltsauswahl benötigt eindeutige direkte Einträge des Pane-Roots",
                ));
            }
        }
        let mut boundary_names = BTreeMap::<PathBuf, BTreeSet<String>>::new();
        for item in items {
            for node in item.ancestors() {
                if let (Some(parent), Some(name)) = (node.parent(), node.file_name()) {
                    boundary_names
                        .entry(parent.to_path_buf())
                        .or_default()
                        .insert(
                            name.to_str()
                                .ok_or_else(|| invalid("SMB-Pfad ist nicht UTF-8"))?
                                .into(),
                        );
                }
            }
        }
        check_cancel(cancel)?;
        let (identity, generation) = {
            let state = lock(&self.state);
            (state.session.identity.clone(), state.generation)
        };
        let mut result = InteractiveFiles {
            provider: self.clone(),
            pane_root: root.to_path_buf(),
            identity,
            generation,
            nodes: Arc::new(BTreeMap::new()),
            listings: Arc::new(BTreeMap::new()),
            children: Arc::new(BTreeMap::new()),
            groups: Arc::new(BTreeMap::new()),
            omission_listings: Arc::new(BTreeSet::new()),
            boundary_names: Arc::new(boundary_names),
            tree: Arc::new(InteractiveSourceTree {
                digest: String::new(),
                entries: 0,
                files: 0,
                directories: 0,
                bytes: 0,
                omitted: Vec::new(),
            }),
            cancel: cancel.clone(),
        };
        // Jede Elternkomponente muss aus einem vollständigen SMB-Listing als echter
        // Ordner hervorgehen. Ausgelassene Ordner selbst werden nie betreten.
        let mut current = PathBuf::from("/");
        for component in virtual_components(root)? {
            result.observe(&current)?;
            let entry = result.listings[&current]
                .get(component)
                .ok_or_else(|| drift("SMB-Pane-Elternkomponente fehlt"))?;
            if !entry.entry.is_dir || entry.entry.is_symlink {
                return Err(drift("SMB-Pane-Elternkomponente ist kein echter Ordner"));
            }
            current.push(component);
        }
        result.observe(root)?;
        let mut direct_counts = BTreeMap::<String, usize>::new();
        for path in items {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("SMB-Auswahlname ist nicht UTF-8"))?;
            *direct_counts
                .entry(crate::remote_rename::folded_name(name))
                .or_default() += 1;
        }
        let mut pending = items.iter().rev().cloned().collect::<Vec<_>>();
        while let Some(path) = pending.pop() {
            result.check_session()?;
            let _ = virtual_components(&path)?;
            let parent = path
                .parent()
                .ok_or_else(|| invalid("SMB-Elternpfad fehlt"))?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("SMB-Name fehlt"))?;
            let entry = result.listings[parent]
                .get(name)
                .ok_or_else(|| drift("SMB-Auswahl fehlt im vollständigen Elternlisting"))?
                .entry
                .clone();
            Arc::make_mut(&mut result.tree).entries += 1;
            let grouped = if parent == root {
                direct_counts[&crate::remote_rename::folded_name(name)] > 1
            } else {
                result.groups[parent].contains(&crate::remote_rename::folded_name(name))
            };
            if grouped || entry.is_symlink || !(entry.is_file || entry.is_dir) {
                Arc::make_mut(&mut result.tree).omitted.push(SourceReadOmission {
                    path: path.to_str().ok_or_else(|| invalid("SMB-Pfad ist nicht UTF-8"))?.into(),
                    reason: if grouped {
                        "Am lokalen Ziel möglicherweise gleichwertige Case-/Unicode-Namen; gesamte Namensgruppe nicht übertragen"
                    } else { "Symlink oder Spezialdatei nicht übertragen" }.into(),
                });
                continue;
            }
            if entry.is_dir {
                result.observe(&path)?;
                let children = result.listings[&path]
                    .keys()
                    .map(|name| path.join(name))
                    .collect::<Vec<_>>();
                pending.extend(children.iter().rev().cloned());
                Arc::make_mut(&mut result.children).insert(path.clone(), children);
                Arc::make_mut(&mut result.tree).directories += 1;
            } else {
                Arc::make_mut(&mut result.tree).files += 1;
                let bytes = result
                    .tree
                    .bytes
                    .checked_add(entry.size)
                    .ok_or_else(|| invalid("SMB-Gesamtgröße ist nicht darstellbar"))?;
                Arc::make_mut(&mut result.tree).bytes = bytes;
            }
            Arc::make_mut(&mut result.nodes).insert(path, entry);
        }
        result.omission_listings = Arc::new(
            result
                .tree
                .omitted
                .iter()
                .flat_map(|item| {
                    Path::new(&item.path)
                        .ancestors()
                        .skip(1)
                        .filter(|path| result.listings.contains_key(*path))
                        .map(Path::to_path_buf)
                })
                .collect(),
        );
        for children in Arc::make_mut(&mut result.children).values_mut() {
            children.retain(|path| result.nodes.contains_key(path));
        }
        Arc::make_mut(&mut result.tree)
            .omitted
            .sort_by(|a, b| a.path.cmp(&b.path));
        // Die Generation schützt nur den aktiven Versuch. Der Queue-Digest muss
        // nach Neustart dieselbe Auswahl mit denselben Fakten wiedererkennen.
        let observations = result
            .listings
            .iter()
            .map(|(path, listing)| (path, result.bound_facts(path, listing)))
            .collect::<BTreeMap<_, _>>();
        let digest = blake3::hash(
            &serde_json::to_vec(&(
                &result.identity,
                root,
                items,
                observations,
                &result.tree.omitted,
            ))
            .map_err(io::Error::other)?,
        )
        .to_hex()
        .to_string();
        Arc::make_mut(&mut result.tree).digest = digest;
        if result.tree.directories > 0 || !result.tree.omitted.is_empty() {
            result.tree.validate()?;
        }
        result.verify()?;
        Ok(Arc::new(result))
    }
}

impl InteractiveFiles {
    fn bound_facts<'a>(
        &self,
        path: &Path,
        listing: &'a BTreeMap<String, ProofListEntry>,
    ) -> BTreeMap<&'a str, Vec<u8>> {
        // Innerhalb ausgewählter Bäume gehört jeder Name zum Umfang. Außerhalb
        // binden nur Auswahl und ihre Elternkomponenten die Wirkung; Änderungen
        // unabhängiger Geschwister erteilen oder entziehen keinen Kopierauftrag.
        listing
            .iter()
            .filter(|(name, _)| {
                self.children.contains_key(path)
                    || self
                        .boundary_names
                        .get(path)
                        .is_some_and(|names| names.contains(*name))
            })
            .map(|(name, item)| {
                (
                    name.as_str(),
                    bound_entry_facts(
                        item,
                        path != self.pane_root && !self.children.contains_key(path),
                    ),
                )
            })
            .collect()
    }
    fn check_session(&self) -> io::Result<()> {
        check_cancel(&self.cancel)?;
        let state = lock(&self.provider.state);
        if state.generation != self.generation || state.session.identity != self.identity {
            return Err(drift(
                "SMB-Sitzung oder Serveridentität änderte sich; Kopie frisch vorbereiten",
            ));
        }
        Ok(())
    }
    fn listing(&self, path: &Path) -> io::Result<BTreeMap<String, ProofListEntry>> {
        self.check_session()?;
        let (entries, generation) = self.provider.complete_list_unchecked(path)?;
        if generation != self.generation {
            return Err(drift("SMB-Sitzung wechselte vor Listing"));
        }
        self.check_session()?;
        Ok(entries
            .into_iter()
            .map(|item| {
                let entry = item.entry();
                let proof = ProofListEntry {
                    raw_name: entry.name.as_bytes().to_vec(),
                    entry,
                    safely_decoded: true,
                    provider_token: Some(item.provider_token()),
                };
                (item.name, proof)
            })
            .collect())
    }

    fn observe(&mut self, path: &Path) -> io::Result<()> {
        let listing = self.listing(path)?;
        let mut counts = BTreeMap::<String, usize>::new();
        for name in listing.keys() {
            *counts
                .entry(crate::remote_rename::folded_name(name))
                .or_default() += 1;
        }
        Arc::make_mut(&mut self.groups).insert(
            path.to_path_buf(),
            counts
                .into_iter()
                .filter_map(|(name, count)| (count > 1).then_some(name))
                .collect(),
        );
        Arc::make_mut(&mut self.listings).insert(path.to_path_buf(), listing);
        Ok(())
    }
    fn verify(&self) -> io::Result<()> {
        self.verify_for(None)
    }
    fn verify_listing(&self, path: &Path) -> io::Result<()> {
        let observed = self
            .listings
            .get(path)
            .ok_or_else(|| invalid("SMB-Ordnerbeleg fehlt"))?;
        if self.bound_facts(path, &self.listing(path)?) != self.bound_facts(path, observed) {
            return Err(drift(
                "SMB-Quellbaum oder ausgelassene Namensgruppe änderte sich",
            ));
        }
        self.check_session()
    }
    fn verify_omissions(&self) -> io::Result<()> {
        self.check_session()?;
        for path in self.omission_listings.iter() {
            self.verify_listing(path)?;
        }
        self.check_session()
    }
    fn verify_for(&self, selected: Option<&Path>) -> io::Result<()> {
        // Auch eine Änderung ausschließlich ausgelassener Mitglieder muss vor
        // jeder Folgewirkung und vor dem lokalen Commit sichtbar werden.
        for (path, observed) in self.listings.iter() {
            if selected.is_some_and(|selected| !selected.starts_with(path))
                && !self.omission_listings.contains(path)
            {
                continue;
            }
            if self.bound_facts(path, &self.listing(path)?) != self.bound_facts(path, observed) {
                return Err(drift(
                    "SMB-Quellbaum oder ausgelassene Namensgruppe änderte sich",
                ));
            }
        }
        self.check_session()
    }
}

fn bound_entry_facts(item: &ProofListEntry, ancestor: bool) -> Vec<u8> {
    let entry = &item.entry;
    // Oberhalb des Pane-Roots bindet nur die Elternidentität den Zugriff.
    // Samba kann CreationTime aus veränderlichen Unix-Fakten ableiten; fremde
    // Geschwister dürfen dadurch keinen Drift unseres Kopierauftrags erzeugen.
    let facts = if ancestor && entry.is_dir && entry.inode.is_some_and(|id| id != 0) {
        serde_json::to_vec(&(
            &entry.name,
            entry.is_dir,
            entry.is_file,
            entry.is_symlink,
            entry.inode,
        ))
    } else {
        // Ohne verlässliche Eltern-ID bleibt nur die vollständige Attributbindung.
        // Auch Auswahl, ausgelassene Mitglieder und ausgewählte Baumlistings
        // behalten sämtliche Entry-Fakten und den vollständigen ChangeTime-Token.
        serde_json::to_vec(&(entry, &item.provider_token))
    };
    facts.expect("SMB-Fakten sind serialisierbar")
}

fn verify_content_facts(observed: &SmbListedEntry, proof: &ProofListEntry) -> io::Result<Entry> {
    // ID 0 ist ausdrücklich nur attributgebunden. Eine vorhandene ID darf weder
    // verschwinden noch wechseln; der Token bindet auch ChangeTime vollständig.
    let actual = observed.entry();
    let mut expected = proof.entry.clone();
    // DirectoryInfo enthält keine Linkzahl. Der erste Handle darf diesen
    // fehlenden Wert ergänzen; seine stärkere Beobachtung gilt danach bis EOF.
    if expected.hard_links.is_none() {
        expected.hard_links = actual.hard_links;
    }
    if serde_json::to_vec(&actual).map_err(io::Error::other)?
        != serde_json::to_vec(&expected).map_err(io::Error::other)?
        || Some(observed.provider_token()) != proof.provider_token
    {
        return Err(drift("SMB-Dateifakten am geöffneten Handle änderten sich"));
    }
    Ok(actual)
}

struct ContentReader {
    files: InteractiveFiles,
    reader: SmbRead,
    path: PathBuf,
    proof: ProofListEntry,
    finished: bool,
    failed: bool,
}
impl ContentReader {
    fn verify_handle(&mut self) -> io::Result<()> {
        let name = self.proof.entry.name.clone();
        let observed = self.reader.runtime.block_on(query_handle_entry(
            &mut self.reader.connection,
            &self.reader.tree,
            self.reader.file_id,
            name,
        ))?;
        self.proof.entry = verify_content_facts(&observed, &self.proof)?;
        self.files.check_session()
    }
}
impl Read for ContentReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(drift(
                "SMB-Inhaltsstrom ist nach einem Fehler nicht wiederverwendbar",
            ));
        }
        if self.finished {
            return Ok(0);
        }
        let result = (|| {
            self.files.check_session()?;
            // Die allgemeine SmbRead-Klasse stoppt an ihrer Startgröße. Diese
            // Klasse fragt darüber hinaus genau ein Byte ab: Wachstum darf
            // nicht als erfolgreicher Abschluss am lokalen Temp vorbeigehen.
            let remaining = self.reader.size.saturating_sub(self.reader.offset);
            let length = (buffer.len() as u64)
                .min(remaining.max(1))
                .min(u64::from(self.reader.max_read))
                .min(64 * 1024) as u32;
            let request = ReadRequest {
                padding: 0x50,
                flags: 0,
                length,
                offset: self.reader.offset,
                file_id: self.reader.file_id,
                minimum_count: 0,
                channel: SMB2_CHANNEL_NONE,
                remaining_bytes: 0,
                read_channel_info: Vec::new(),
            };
            let frame = self
                .reader
                .runtime
                .block_on(self.reader.connection.execute_with_credits(
                    Command::Read,
                    &request,
                    Some(self.reader.tree.tree_id),
                    CreditCharge(1),
                ))
                .map_err(smb_error)?;
            let data = if frame.header.status == NtStatus::END_OF_FILE {
                Vec::new()
            } else {
                require_status(&frame, Command::Read)?;
                ReadResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(smb_error)?
                    .data
            };
            if data.len() > length as usize {
                return Err(drift("SMB-READ überschreitet die angeforderte Größe"));
            }
            if data.is_empty() {
                if remaining != 0 {
                    return Err(drift(
                        "SMB-Inhalt wurde vor der gebundenen Größe abgeschnitten",
                    ));
                }
                self.verify_handle()?;
                self.files.verify_for(Some(&self.path))?;
                self.reader.close()?;
                self.finished = true;
                return Ok(0);
            }
            if remaining == 0 {
                return Err(drift("SMB-Inhalt überschreitet die gebundene Größe"));
            }
            // MinimumCount 0 erlaubt positive Kurzblöcke. Erst ein separater
            // EOF entscheidet, ob der vollständig gebundene Inhalt gelesen wurde.
            buffer[..data.len()].copy_from_slice(&data);
            self.reader.offset += data.len() as u64;
            Ok(data.len())
        })();
        if result.is_err() {
            self.failed = true;
            let _ = self.reader.close();
        }
        result
    }
}

impl Vfs for InteractiveFiles {
    fn interactive_additive_only(&self) -> bool {
        true
    }
    fn supports_mode_write(&self) -> bool {
        false
    }
    fn supports_mtime_write(&self) -> bool {
        false
    }
    fn interactive_regular_file_source(&self, path: &Path) -> io::Result<Option<Entry>> {
        self.stat(path).map(|entry| entry.is_file.then_some(entry))
    }
    fn interactive_source_tree(&self) -> io::Result<Option<InteractiveSourceTree>> {
        self.verify()?;
        Ok((self.tree.directories > 0 || !self.tree.omitted.is_empty())
            .then(|| self.tree.as_ref().clone()))
    }
    fn interactive_copy_children(&self, path: &Path) -> io::Result<Option<Vec<ProofListEntry>>> {
        self.verify_omissions()?;
        if self.children.contains_key(path) && !self.omission_listings.contains(path) {
            // Ordnergrenzen prüfen ihren eigenen Bestand. Reine Navigation liest
            // nicht bei jedem Kind erneut alle Vorfahren; deren vollständiger
            // Readback gehört vor READ, nach EOF und vor den Datei-Commit.
            self.verify_listing(path)?;
        }
        Ok(self.children.get(path).map(|children| {
            children
                .iter()
                .map(|path| {
                    let entry = self.nodes[path].clone();
                    ProofListEntry {
                        raw_name: entry.name.as_bytes().to_vec(),
                        entry,
                        safely_decoded: true,
                        provider_token: None,
                    }
                })
                .collect()
        }))
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.verify_omissions()?;
        self.nodes
            .get(path)
            .cloned()
            .ok_or_else(|| invalid("Pfad gehört nicht zur SMB-Inhaltsauswahl"))
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let entry = self.stat(path)?;
        if !entry.is_file {
            return Err(invalid("SMB-Inhaltsstrom benötigt eine reguläre Datei"));
        }
        self.verify_for(Some(path))?;
        let remote = self.provider.remote_path(path)?;
        let (tree, mut connection, max_read) = {
            let state = lock(&self.provider.state);
            if state.generation != self.generation || state.session.identity != self.identity {
                return Err(drift("SMB-Sitzung wechselte vor OPEN"));
            }
            (
                state.session.tree.clone(),
                state.session.connection.clone(),
                state
                    .session
                    .connection
                    .params()
                    .map(|params| params.max_read_size.min(MAX_IO_CHUNK))
                    .unwrap_or(64 * 1024),
            )
        };
        let file_id = self.provider.runtime.block_on(open_handle(
            &mut connection,
            &tree,
            &remote,
            FileAccessMask::FILE_READ_DATA
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
            CreateDisposition::FileOpen,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ))?;
        let proof = self.listings[path
            .parent()
            .ok_or_else(|| invalid("SMB-Elternpfad fehlt"))?][&entry.name]
            .clone();
        let mut reader = ContentReader {
            files: self.clone(),
            reader: SmbRead {
                runtime: self.provider.runtime.clone(),
                tree,
                connection,
                file_id,
                size: entry.size,
                offset: 0,
                max_read,
                closed: false,
            },
            path: path.to_path_buf(),
            proof,
            finished: false,
            failed: false,
        };
        // FILE_INTERNAL_INFORMATION ist die Dateisystem-ID, nicht die kurzlebige
        // SMB-Handle-ID aus CREATE. Beide Prüfungen lesen denselben offenen Handle.
        reader.verify_handle()?;
        self.verify_for(Some(path))?;
        Ok(Box::new(reader))
    }

    fn verify_transferred_source(&self, path: &Path, _: &blake3::Hash) -> io::Result<()> {
        self.verify_for(Some(path))
    }
    fn list(&self, _: &Path) -> io::Result<Vec<Entry>> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SMB-Inhaltsquelle ist schreibgeschützt",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vorfahren_ohne_datei_id_behalten_attribut_und_changetimebindung() {
        let directory = SmbListedEntry {
            name: "parent".into(),
            is_dir: true,
            is_symlink: false,
            size: 0,
            mtime: Some(12),
            created: Some(10),
            change_time: 100,
            file_id: 0,
            hard_links: None,
        };
        let proof = |entry: &SmbListedEntry| ProofListEntry {
            entry: entry.entry(),
            provider_token: Some(entry.provider_token()),
            raw_name: entry.name.as_bytes().to_vec(),
            safely_decoded: true,
        };
        let original = proof(&directory);
        assert_eq!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&proof(&directory), true)
        );
        let mut changed = directory.clone();
        changed.change_time += 1;
        assert_ne!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&proof(&changed), true),
            "ID0-Vorfahrentausch mit anderem ChangeTime muss auffallen"
        );
        changed = directory.clone();
        changed.mtime = Some(20);
        assert_ne!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&proof(&changed), true)
        );
        changed = directory;
        changed.created = Some(20);
        assert_ne!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&proof(&changed), true)
        );
    }
    #[test]
    fn vorfahren_binden_identitaet_ohne_veraenderliche_ordnerzeiten() {
        let directory = SmbListedEntry {
            name: "parent".into(),
            is_dir: true,
            is_symlink: false,
            size: 0,
            mtime: Some(12),
            created: Some(10),
            change_time: 100,
            file_id: 4,
            hard_links: None,
        };
        let proof = |entry: &SmbListedEntry| ProofListEntry {
            entry: entry.entry(),
            provider_token: Some(entry.provider_token()),
            raw_name: entry.name.as_bytes().to_vec(),
            safely_decoded: true,
        };
        let original = proof(&directory);
        let mut changed = directory.clone();
        changed.mtime = Some(20);
        changed.created = Some(20);
        changed.change_time = 200;
        let mut changed_attributes = proof(&changed);
        changed_attributes.entry.size = 4096;
        changed_attributes.entry.owner = Some("other".into());
        changed_attributes.entry.group = Some("other".into());
        changed_attributes.entry.mode = Some(0o755);
        changed_attributes.entry.hard_links = Some(5);
        assert_eq!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&changed_attributes, true)
        );
        assert_ne!(
            bound_entry_facts(&original, false),
            bound_entry_facts(&changed_attributes, false)
        );
        assert_eq!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&proof(&changed), true)
        );
        assert_ne!(
            bound_entry_facts(&original, false),
            bound_entry_facts(&proof(&changed), false)
        );
        for id in [0, 5] {
            changed.file_id = id;
            assert_ne!(
                bound_entry_facts(&original, true),
                bound_entry_facts(&proof(&changed), true)
            );
        }
        changed = directory.clone();
        changed.is_symlink = true;
        assert_ne!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&proof(&changed), true)
        );
        changed = directory;
        changed.is_dir = false;
        assert_ne!(
            bound_entry_facts(&original, true),
            bound_entry_facts(&proof(&changed), true)
        );
    }
    #[test]
    fn handlefakten_ohne_datei_id_bleiben_attributgebunden() {
        let mut observed = SmbListedEntry {
            name: "file".into(),
            is_dir: false,
            is_symlink: false,
            size: 17,
            mtime: Some(12),
            created: Some(10),
            change_time: 100,
            file_id: 0,
            hard_links: None,
        };
        let proof = ProofListEntry {
            entry: observed.entry(),
            provider_token: Some(observed.provider_token()),
            raw_name: b"file".to_vec(),
            safely_decoded: true,
        };
        verify_content_facts(&observed, &proof).unwrap();
        observed.size += 1;
        assert_eq!(
            crate::error_code(&verify_content_facts(&observed, &proof).unwrap_err()),
            ErrorCode::PlanDrift
        );
        observed.size -= 1;
        observed.change_time += 1;
        assert_eq!(
            crate::error_code(&verify_content_facts(&observed, &proof).unwrap_err()),
            ErrorCode::PlanDrift
        );
        observed.change_time -= 1;
        observed.file_id = 4;
        let identified = ProofListEntry {
            entry: observed.entry(),
            provider_token: Some(observed.provider_token()),
            ..proof.clone()
        };
        verify_content_facts(&observed, &identified).unwrap();
        for changed in [0, 5] {
            observed.file_id = changed;
            assert_eq!(
                crate::error_code(&verify_content_facts(&observed, &identified).unwrap_err()),
                ErrorCode::PlanDrift
            );
        }
    }

    #[test]
    fn erster_handle_ergaenzt_im_listing_fehlende_linkzahl() {
        let mut observed = SmbListedEntry {
            name: "selected-copy.txt".into(),
            is_dir: false,
            is_symlink: false,
            size: 5,
            mtime: Some(12),
            created: Some(10),
            change_time: 100,
            file_id: 7,
            hard_links: Some(1),
        };
        let mut listing = observed.entry();
        listing.hard_links = None;
        let mut proof = ProofListEntry {
            entry: listing,
            provider_token: Some(observed.provider_token()),
            raw_name: b"selected-copy.txt".to_vec(),
            safely_decoded: true,
        };
        proof.entry = verify_content_facts(&observed, &proof).unwrap();
        assert_eq!(proof.entry.hard_links, Some(1));
        for links in [None, Some(2)] {
            observed.hard_links = links;
            assert_eq!(
                crate::error_code(&verify_content_facts(&observed, &proof).unwrap_err()),
                ErrorCode::PlanDrift
            );
        }
        observed.hard_links = Some(1);
        observed.change_time += 1;
        assert_eq!(
            crate::error_code(&verify_content_facts(&observed, &proof).unwrap_err()),
            ErrorCode::PlanDrift
        );
    }
}
