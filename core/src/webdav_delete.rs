//! Gewöhnliches dauerhaftes Löschen: vollständiger Plan, frische ETags und genau
//! ein bedingtes DELETE je bestätigter Datei; Collection-DELETE bleibt ausgespart.
use super::*;
use std::path::PathBuf;
use std::time::Instant;

struct OrdinaryDelete {
    provider: Arc<WebDavVfs>,
    binding: BoundWebDavMutation,
    root: PathBuf,
    parents: Mutex<Vec<WebDavResourceIdentity>>,
}
impl WebDavVfs {
    pub(crate) fn ordinary_delete(self: &Arc<Self>, root: &Path) -> io::Result<Arc<dyn Vfs>> {
        interactive_namespace::validate_path(root)?;
        let (binding, _) = self.bind_mutation(
            root,
            |_| true,
            "WebDAV-Löschen benötigt eine gebundene Sitzung",
        )?;
        let parents = interactive_namespace::directory_chain(
            &binding,
            &virtual_components(root)?,
            Instant::now() + interactive_namespace::PREPARATION_LIMIT,
        )?;
        Ok(Arc::new(OrdinaryDelete {
            provider: self.clone(),
            binding,
            root: root.to_path_buf(),
            parents: Mutex::new(parents),
        }))
    }
}
impl OrdinaryDelete {
    fn check(&self, path: &Path, mutation: bool) -> io::Result<Vec<WebDavResourceIdentity>> {
        interactive_namespace::validate_path(path)?;
        if !path.starts_with(&self.root) || (mutation && path == self.root) {
            return Err(invalid(
                "WebDAV-Löschpfad liegt außerhalb des gewählten Roots",
            ));
        }
        self.provider
            .ensure_mutation_binding_current(&self.binding)?;
        let parent = if mutation {
            path.parent().unwrap_or(&self.root)
        } else {
            path
        };
        let parents = interactive_namespace::directory_chain(
            &self.binding,
            &virtual_components(parent)?,
            Instant::now() + interactive_namespace::PREPARATION_LIMIT,
        )?;
        if !parents.starts_with(&lock(&self.parents)) {
            return Err(interactive_namespace::drift(
                "WebDAV-Löschroot änderte sich",
            ));
        }
        Ok(parents)
    }
    fn remove(
        &self,
        path: &Path,
        directory: bool,
        expected: Option<&ProofListEntry>,
    ) -> io::Result<()> {
        if directory {
            return Err(coded_error(
                io::ErrorKind::Unsupported,
                ErrorCode::Unsupported,
                COLLECTION_DELETE_LIMITATION,
            ));
        }
        let parents = self.check(path, true)?;
        let remote = self.provider.remote_path(path)?;
        let proof = stat_proof_in_session(&self.binding.session, &remote)?;
        if proof.entry.is_symlink
            || proof.entry.is_dir != directory
            || (!directory && !proof.entry.is_file)
        {
            return Err(interactive_namespace::drift(
                "WebDAV-Löschobjekt änderte seinen Typ",
            ));
        }
        if expected.is_some_and(|expected| proof.provider_token != expected.provider_token) {
            return Err(interactive_namespace::drift(
                "WebDAV-Löschobjekt änderte seine bestätigte Version",
            ));
        }
        if directory && !self.list_complete(path)?.entries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                "WebDAV-Collection ist nicht leer",
            ));
        }
        let token = expected.map_or(&proof.provider_token, |expected| &expected.provider_token);
        let etag = token
            .as_deref()
            .and_then(|v| std::str::from_utf8(v).ok())
            .ok_or_else(|| invalid("WebDAV-Löschobjekt besitzt keine starke ETag"))?;
        self.check(path, true)?;
        // If-Match bindet die konkrete Datei. Frische Elternprüfungen vor und
        // nach dem Request beobachten die Pfadbindung ohne atomare Elterngarantie.
        let response = delete_request(
            &self.binding.session.http,
            &self.binding.session.identity.origin,
            &remote,
            Some(etag),
            None,
            &parents,
            WebDavRequestClass::OrdinaryFile,
        )?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Err(interactive_namespace::drift(
                "WebDAV-DELETE wegen geänderter Version abgelehnt",
            ));
        }
        require_status(
            response.status(),
            &[StatusCode::NO_CONTENT, StatusCode::OK],
            "WebDAV-Dauerlöschen",
        )?;
        require_absent_after_delete_readback(
            stat_proof_in_session(&self.binding.session, &remote),
            &remote,
            "WebDAV-Dauerlöschen",
        )?;
        // Eine eigene Kindentfernung darf die Collection-ETag verändern. Nach
        // belegter Abwesenheit die frische Kette als nächsten Versionsstand
        // übernehmen; das ersetzt keine atomare Bindung gegen Vorfahrenwechsel.
        let refreshed = interactive_namespace::directory_chain(
            &self.binding,
            &virtual_components(&self.root)?,
            Instant::now() + interactive_namespace::PREPARATION_LIMIT,
        )?;
        self.provider
            .ensure_mutation_binding_current(&self.binding)?;
        *lock(&self.parents) = refreshed;
        Ok(())
    }
}
impl Vfs for OrdinaryDelete {
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
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        Err(read_only())
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        Err(read_only())
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn best_effort_permanent_delete_blocker(&self, directory: bool) -> Option<String> {
        ordinary_delete_blocker(directory)
    }
    fn supports_best_effort_permanent_delete(&self) -> bool {
        true
    }
    fn remove_permanent_best_effort_if_matches(
        &self,
        path: &Path,
        expected: &ProofListEntry,
    ) -> io::Result<()> {
        // Blattversionen müssen exakt der Bestätigung entsprechen. Collections
        // bleiben unabhängig von einem frischen Leercheck technisch ausgespart.
        self.remove(
            path,
            expected.entry.is_dir,
            (!expected.entry.is_dir).then_some(expected),
        )
    }
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(self
            .list_complete(path)?
            .entries
            .into_iter()
            .map(|p| p.entry)
            .collect())
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        self.check(path, false)?;
        self.provider.list_complete(path)
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.provider.stat(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn root_identity(&self, path: &Path) -> io::Result<crate::real_mode::RootIdentity> {
        self.check(path, false)?;
        self.provider.root_identity(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.check(path, true)?;
        self.provider.open_read(path)
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(read_only())
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.remove(path, false, None)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.remove(path, true, None)
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(invalid(
            "WebDAV entfernt Bäume ausschließlich über bestätigte Einzelrecords",
        ))
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn reclaim_verification_session(&self) -> io::Result<()> {
        self.provider.reconnect()
    }
}
