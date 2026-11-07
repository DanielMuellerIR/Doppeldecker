//! Private additive WebDAV-Lesequelle. Vorbereitete ETags binden GET und
//! vollständige Baumbeobachtungen binden auch ausgelassene Namensgruppen.

use super::*;
use crate::vfs::{InteractiveSourceTree, SourceReadOmission};
use crate::Cancel;
use std::path::PathBuf;

pub(crate) const WEBDAV_CONTENT_READ_LIMITATION: &str = "Additive WebDAV-Kopien zum lokalen Ziel laufen als Best Effort: Sie prüfen vollständige ausgewählte Baumlisten, ausgelassene Namensgruppen, starke Datei-ETags, bedingtes GET, Streamlänge und frischen Quellenreadback vor dem lokalen Commit. WebDAV bindet GET nicht atomar an die vollständige Elternkette; serverseitig als reguläre Ressourcen dargestellte Symlinks sind nicht erkennbar. Ein unbemerkter Vorfahrentausch bleibt möglich. Baumbelege benötigen Speicher proportional zum beobachteten Bestand.";

#[derive(Clone)]
struct InteractiveFiles {
    provider: Arc<WebDavVfs>,
    identity: WebDavServerIdentity,
    generation: u64,
    nodes: Arc<BTreeMap<PathBuf, Entry>>,
    listings: Arc<BTreeMap<PathBuf, BTreeMap<String, ProofListEntry>>>,
    children: Arc<BTreeMap<PathBuf, Vec<PathBuf>>>,
    groups: Arc<BTreeMap<PathBuf, BTreeSet<String>>>,
    omission_listings: Arc<BTreeSet<PathBuf>>,
    relevant: Arc<BTreeMap<PathBuf, BTreeSet<String>>>,
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
            "WebDAV-Kopie abgebrochen",
        ));
    }
    Ok(())
}

impl WebDavVfs {
    pub(crate) fn interactive_files(
        self: &Arc<Self>,
        root: &Path,
        items: &[PathBuf],
        cancel: &Cancel,
    ) -> io::Result<Arc<dyn Vfs>> {
        virtual_components(root)?;
        if !root.is_absolute() || items.is_empty() {
            return Err(invalid(
                "WebDAV-Inhaltsauswahl benötigt einen absoluten Pane-Root und direkte Einträge",
            ));
        }
        let mut seen = BTreeSet::new();
        for path in items {
            virtual_components(path)?;
            if !path.is_absolute() || path.parent() != Some(root) || !seen.insert(path) {
                return Err(invalid(
                    "WebDAV-Inhaltsauswahl benötigt eindeutige direkte Einträge des Pane-Roots",
                ));
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
            relevant: Arc::new(BTreeMap::new()),
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
        // Jede Elternkomponente muss aus einem vollständigen Listing als echter
        // Ordner hervorgehen. Ausgelassene Ordner selbst werden nie betreten.
        let mut current = PathBuf::from("/");
        for component in virtual_components(root)? {
            result.observe(&current)?;
            let entry = result.listings[&current]
                .get(&component)
                .ok_or_else(|| drift("WebDAV-Pane-Elternkomponente fehlt"))?;
            if !entry.entry.is_dir || entry.entry.is_symlink {
                return Err(drift("WebDAV-Pane-Elternkomponente ist kein echter Ordner"));
            }
            Arc::make_mut(&mut result.relevant)
                .entry(current.clone())
                .or_default()
                .insert(component.to_owned());
            current.push(component);
        }
        result.observe(root)?;
        Arc::make_mut(&mut result.relevant).insert(
            root.to_path_buf(),
            items
                .iter()
                .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
                .collect(),
        );
        let mut direct_counts = BTreeMap::<String, usize>::new();
        for path in items {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("WebDAV-Auswahlname ist nicht UTF-8"))?;
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
                .ok_or_else(|| invalid("WebDAV-Elternpfad fehlt"))?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("WebDAV-Name fehlt"))?;
            let entry = result.listings[parent]
                .get(name)
                .ok_or_else(|| drift("WebDAV-Auswahl fehlt im vollständigen Elternlisting"))?
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
                    path: path.to_str().ok_or_else(|| invalid("WebDAV-Pfad ist nicht UTF-8"))?.into(),
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
                    .ok_or_else(|| invalid("WebDAV-Gesamtgröße ist nicht darstellbar"))?;
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
            .map(|(path, listing)| (path, result.observation(path, listing)))
            .collect::<BTreeMap<_, _>>();
        let digest = blake3::hash(
            &serde_json::to_vec(&(
                &result.identity,
                &self.config.username,
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
    fn check_session(&self) -> io::Result<()> {
        check_cancel(&self.cancel)?;
        let state = lock(&self.provider.state);
        if state.generation != self.generation || state.session.identity != self.identity {
            return Err(drift(
                "WebDAV-Sitzung oder Serveridentität änderte sich; Kopie frisch vorbereiten",
            ));
        }
        Ok(())
    }
    fn listing(&self, path: &Path) -> io::Result<BTreeMap<String, ProofListEntry>> {
        self.check_session()?;
        let listing = self.provider.complete_list_bound(path);
        self.check_session()?;
        let (listing, _, generation) = listing?;
        if generation != self.generation || !listing.complete {
            return Err(drift(
                "WebDAV-Sitzung wechselte vor dem vollständigen Listing",
            ));
        }
        self.check_session()?;
        Ok(listing
            .entries
            .into_iter()
            .map(|item| (item.entry.name.clone(), item))
            .collect())
    }
    fn observation(
        &self,
        path: &Path,
        listing: &BTreeMap<String, ProofListEntry>,
    ) -> BTreeMap<String, Vec<u8>> {
        listing
            .iter()
            .filter(|(name, _)| {
                self.relevant
                    .get(path)
                    .is_none_or(|names| names.contains(*name))
            })
            .map(|(name, item)| {
                (
                    name.clone(),
                    serde_json::to_vec(&(&item.entry, &item.provider_token))
                        .expect("WebDAV-Beleg ist serialisierbar"),
                )
            })
            .collect()
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
            .ok_or_else(|| invalid("WebDAV-Ordnerbeleg fehlt"))?;
        if self.observation(path, &self.listing(path)?) != self.observation(path, observed) {
            return Err(drift(
                "WebDAV-Quellbaum oder ausgelassene Namensgruppe änderte sich",
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
            if self.observation(path, &self.listing(path)?) != self.observation(path, observed) {
                return Err(drift(
                    "WebDAV-Quellbaum oder ausgelassene Namensgruppe änderte sich",
                ));
            }
        }
        self.check_session()
    }
}

struct ContentReader {
    files: InteractiveFiles,
    reader: reqwest::blocking::Response,
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
                "WebDAV-Inhaltsstrom ist nach einem Fehler nicht wiederverwendbar",
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
                        "WebDAV-Inhalt wurde vor der erwarteten Größe abgeschnitten",
                    ));
                }
                // EOF und exakte Länge werden vor dem lokalen Commit geprüft.
                self.files.verify_for(Some(&self.path))?;
                self.finished = true;
            } else {
                self.offset += count as u64;
                if self.offset > self.expected {
                    return Err(drift("WebDAV-Inhalt überschreitet die gebundene Größe"));
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
            // Readback gehört vor GET, nach EOF und vor den Datei-Commit.
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
            .ok_or_else(|| invalid("Pfad gehört nicht zur WebDAV-Inhaltsauswahl"))
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let entry = self.stat(path)?;
        if !entry.is_file {
            return Err(invalid("WebDAV-Inhaltsstrom benötigt eine reguläre Datei"));
        }
        self.verify_for(Some(path))?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("WebDAV-Dateieltern fehlen"))?;
        let etag = self.listings[parent][&entry.name]
            .provider_token
            .as_ref()
            .and_then(|value| std::str::from_utf8(value).ok())
            .ok_or_else(|| invalid("WebDAV-Datei benötigt eine starke ETag"))?;
        validate_strong_etag(etag)?;
        let session = {
            let state = lock(&self.provider.state);
            if state.generation != self.generation || state.session.identity != self.identity {
                return Err(drift("WebDAV-Sitzung wechselte vor GET"));
            }
            state.session.clone()
        };
        let remote = map_virtual_path(&self.identity.canonical_root, path)?;
        let url = resource_url(&self.identity.origin, &remote)?;
        // Ausschließlich die vorbereitete Version autorisiert den Inhaltsstrom.
        let response = authenticated(session.http.client.get(url), &session.http)
            .header(IF_MATCH, etag)
            .send()
            .map_err(http_error)?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Err(drift("WebDAV-Dateiversion änderte sich vor GET"));
        }
        require_status(response.status(), &[StatusCode::OK], "WebDAV-GET")?;
        require_matching_etag(response.headers(), etag, "WebDAV-GET")
            .map_err(|_| drift("WebDAV-GET lieferte nicht die vorbereitete starke ETag"))?;
        self.check_session()?;
        Ok(Box::new(ContentReader {
            files: self.clone(),
            reader: response,
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
