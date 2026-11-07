//! Private additive FTP-Lesequelle. Vollständige MLSD-Beobachtungen binden auch
//! ausgelassene Namen. FTP besitzt kein FSTAT: Pfadtausch mit identischen Fakten
//! und gleich konfigurierte Serverknoten bleiben Best-Effort-Grenzen.

use super::*;
use crate::vfs::{InteractiveSourceTree, SourceReadOmission};
use crate::Cancel;
use std::path::PathBuf;

pub(crate) const FTP_CONTENT_READ_LIMITATION: &str = "Additive FTP-/FTPS-Kopien zum lokalen Ziel laufen als Best Effort: vollständige MLSD-Beobachtungen binden Auswahl, Eltern und ausgelassene Namen; Länge, Transferabschluss und frischer Quellenreadback werden vor dem lokalen Commit geprüft. FTP besitzt kein FSTAT und keine atomare Pfadbindung. Ein unbemerkter Pfadtausch oder eine Änderung mit identischen gemeldeten Fakten bleibt möglich; ebenso Zugriffe auf gleich konfigurierte Serverknoten. Baumbelege benötigen Speicher proportional zum beobachteten Bestand.";

#[derive(Clone)]
struct InteractiveFiles {
    provider: Arc<FtpsVfs>,
    identity: FtpsServerIdentity,
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

pub(super) fn mlst_read_facts(features: &suppaftp::types::Features) -> Option<String> {
    let raw = features.get("MLST")?.as_deref()?;
    let names = raw
        .split(';')
        .filter(|name| !name.is_empty())
        .map(|name| name.trim_end_matches('*'))
        .collect::<Vec<_>>();
    if names.is_empty()
        || names.iter().any(|name| {
            name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        })
    {
        return None;
    }
    Some(format!("{};", names.join(";")))
}

fn drift(message: &str) -> io::Error {
    coded_error(io::ErrorKind::InvalidData, ErrorCode::PlanDrift, message)
}

fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "FTP-Kopie abgebrochen",
        ));
    }
    Ok(())
}

impl FtpsVfs {
    pub(crate) fn interactive_files(
        self: &Arc<Self>,
        root: &Path,
        items: &[PathBuf],
        cancel: &Cancel,
    ) -> io::Result<Arc<dyn Vfs>> {
        virtual_components(root)?;
        if !root.is_absolute() || items.is_empty() {
            return Err(invalid(
                "FTP-Inhaltsauswahl benötigt einen absoluten Pane-Root und direkte Einträge",
            ));
        }
        let mut seen = BTreeSet::new();
        for path in items {
            virtual_components(path)?;
            if !path.is_absolute() || path.parent() != Some(root) || !seen.insert(path) {
                return Err(invalid(
                    "FTP-Inhaltsauswahl benötigt eindeutige direkte Einträge des Pane-Roots",
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
                                .ok_or_else(|| invalid("FTP-Pfad ist nicht UTF-8"))?
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
        // Jede Elternkomponente muss aus einem vollständigen MLSD als echter
        // Ordner hervorgehen. Ausgelassene Ordner selbst werden nie betreten.
        let mut current = PathBuf::from("/");
        for component in virtual_components(root)? {
            result.observe(&current)?;
            let entry = result.listings[&current]
                .get(component)
                .ok_or_else(|| drift("FTP-Pane-Elternkomponente fehlt"))?;
            if !entry.entry.is_dir || entry.entry.is_symlink {
                return Err(drift("FTP-Pane-Elternkomponente ist kein echter Ordner"));
            }
            current.push(component);
        }
        result.observe(root)?;
        let mut direct_counts = BTreeMap::<String, usize>::new();
        for path in items {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("FTP-Auswahlname ist nicht UTF-8"))?;
            *direct_counts
                .entry(crate::remote_rename::folded_name(name))
                .or_default() += 1;
        }
        let mut pending = items.iter().rev().cloned().collect::<Vec<_>>();
        while let Some(path) = pending.pop() {
            result.check_session()?;
            virtual_components(&path)?;
            let parent = path
                .parent()
                .ok_or_else(|| invalid("FTP-Elternpfad fehlt"))?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("FTP-Name fehlt"))?;
            let entry = result.listings[parent]
                .get(name)
                .ok_or_else(|| drift("FTP-Auswahl fehlt im vollständigen Elternlisting"))?
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
                    path: path.to_str().ok_or_else(|| invalid("FTP-Pfad ist nicht UTF-8"))?.into(),
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
                    .ok_or_else(|| invalid("FTP-Gesamtgröße ist nicht darstellbar"))?;
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
                self.account_digest(),
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
    ) -> BTreeMap<&'a str, &'a Option<Vec<u8>>> {
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
            .map(|(name, item)| (name.as_str(), &item.provider_token))
            .collect()
    }
    fn check_session(&self) -> io::Result<()> {
        check_cancel(&self.cancel)?;
        let state = lock(&self.provider.state);
        if state.generation != self.generation || state.session.identity != self.identity {
            return Err(drift(
                "FTP-Sitzung oder Serveridentität änderte sich; Kopie frisch vorbereiten",
            ));
        }
        Ok(())
    }
    fn listing(&self, path: &Path) -> io::Result<BTreeMap<String, ProofListEntry>> {
        self.check_session()?;
        let remote = map_virtual_path(&self.identity.canonical_root, path)?;
        let mut state = lock(&self.provider.state);
        if state.generation != self.generation || state.session.identity != self.identity {
            return Err(drift("FTP-Sitzung wechselte vor MLSD"));
        }
        // Alle angebotenen Fakten erhalten insbesondere Unique, modify-Bruchteile
        // und Metadaten. Eine feste Kurzliste würde z. B. UNIX.mode abwählen.
        if let Some(selection) = state.session.read_mlst_facts.clone() {
            if let Err(error) = state.session.stream.opts("MLST", Some(&selection)) {
                if !opts_mlst_rejection_is_tolerable(&error) {
                    return Err(ftp_error(error));
                }
            }
        }
        state.session.stream.cwd(remote).map_err(ftp_error)?;
        let entries = raw_mlsd_current_for(&mut state.session.stream, true)?;
        check_cancel(&self.cancel)?;
        Ok(entries
            .into_iter()
            .map(|item| (item.entry.name.clone(), item))
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
            .ok_or_else(|| invalid("FTP-Ordnerbeleg fehlt"))?;
        if self.bound_facts(path, &self.listing(path)?) != self.bound_facts(path, observed) {
            return Err(drift(
                "FTP-Quellbaum oder ausgelassene Namensgruppe änderte sich",
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
                    "FTP-Quellbaum oder ausgelassene Namensgruppe änderte sich",
                ));
            }
        }
        self.check_session()
    }
}

struct ContentReader {
    files: InteractiveFiles,
    reader: FtpsRead,
    path: PathBuf,
    expected: u64,
    offset: u64,
    finished: bool,
    failed: bool,
}
impl Read for ContentReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(drift(
                "FTP-Inhaltsstrom ist nach einem Fehler nicht wiederverwendbar",
            ));
        }
        if self.finished {
            return Ok(0);
        }
        let result = (|| {
            self.files.check_session()?;
            let length = buffer
                .len()
                .min(64 * 1024)
                .min(self.expected.saturating_sub(self.offset).saturating_add(1) as usize);
            let count = self.reader.read(&mut buffer[..length])?;
            if count == 0 {
                if self.offset != self.expected {
                    return Err(drift(
                        "FTP-Inhalt wurde vor der erwarteten Größe abgeschnitten",
                    ));
                }
                // FtpsRead liefert EOF erst nach erfolgreichem finalen 226/250.
                self.files.verify_for(Some(&self.path))?;
                self.finished = true;
            } else {
                self.offset += count as u64;
                if self.offset > self.expected {
                    return Err(drift("FTP-Inhalt überschreitet die gebundene Größe"));
                }
            }
            Ok(count)
        })();
        if result.is_err() {
            self.failed = true;
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
            // Readback gehört vor RETR, nach EOF und vor den Datei-Commit.
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
            .ok_or_else(|| invalid("Pfad gehört nicht zur FTP-Inhaltsauswahl"))
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let entry = self.stat(path)?;
        if !entry.is_file {
            return Err(invalid("FTP-Inhaltsstrom benötigt eine reguläre Datei"));
        }
        let mut session = self.provider.fresh_session()?;
        if session.identity != self.identity {
            return Err(drift("FTP-Datenstrom wechselte die Identität"));
        }
        self.verify_for(Some(path))?;
        let remote = map_virtual_path(&self.identity.canonical_root, path)?;
        let data = session.stream.retr_as_stream(remote).map_err(ftp_error)?;
        Ok(Box::new(ContentReader {
            files: self.clone(),
            reader: FtpsRead {
                session: Some(session.stream),
                data: Some(Box::new(data)),
                finished: false,
            },
            path: path.to_path_buf(),
            expected: entry.size,
            offset: 0,
            finished: false,
            failed: false,
        }))
    }
    fn verify_transferred_source(&self, path: &Path, _: &blake3::Hash) -> io::Result<()> {
        self.verify_for(Some(path))
    }
    fn list(&self, _: &Path) -> io::Result<Vec<Entry>> {
        Err(read_only())
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(read_only())
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(read_only())
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        Err(read_only())
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        Err(read_only())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_mlst_auswahl_erhaelt_alle_angebotenen_fakten() {
        let features = [(
            "MLST".into(),
            Some("Type*;Size*;Modify*;Unique;UNIX.mode*;Create;".into()),
        )]
        .into();
        assert_eq!(
            mlst_read_facts(&features).as_deref(),
            Some("Type;Size;Modify;Unique;UNIX.mode;Create;")
        );
        assert_eq!(mlst_read_facts(&[("MLST".into(), None)].into()), None);
        assert_eq!(
            mlst_read_facts(&[("MLST".into(), Some("Type;\r\nDELE /bad;".into()))].into()),
            None
        );
    }
}
