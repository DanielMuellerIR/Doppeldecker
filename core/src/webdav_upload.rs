//! Private additive Zielsicht; der gemeinsame Kopierer besitzt Transfer und Commitbilanz.
use super::interactive_namespace::{
    check_deadline, directory_chain, drift, read_children, PREPARATION_LIMIT,
};
use super::*;
use crate::local_staging::RemoteTempIntent;
use crate::remote_real_mode::InteractiveFileOwner;
use crate::vfs::FileCommitClass;
use crate::Cancel;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

pub(crate) fn ensure_profile_current(
    queue: &Path,
    bindings: &[crate::RemoteProfileBinding],
    config: &WebDavConnectionConfig,
) -> io::Result<()> {
    let profiles = bindings
        .iter()
        .filter(|binding| binding.protocol == crate::RemoteProfileProtocol::Webdav)
        .collect::<Vec<_>>();
    if profiles.is_empty() {
        return Ok(());
    }
    let config_dir = queue
        .parent()
        .ok_or_else(|| drift("WebDAV-Profilbindung besitzt keinen Konfigurationsordner"))?;
    let current = crate::load_webdav_profiles_versioned(&config_dir.join("webdav-profiles.json"))?;
    for binding in profiles {
        if !current.iter().any(|profile| {
            profile.profile.id == binding.profile_id
                && profile.revision == binding.revision
                && &profile.profile.connection == config
        }) {
            return Err(drift(
                "WebDAV-Uploadprofil änderte sich; vor weiterer Wirkung neu vorbereiten",
            ));
        }
    }
    Ok(())
}

pub(crate) fn capabilities() -> crate::ProviderFileCapabilities {
    crate::ProviderFileCapabilities {
        provider: crate::ProviderKind::WebDav, copy_new: false, copy_new_best_effort: true,
        replace: false, replace_best_effort: false, remove_source: false,
        remove_source_best_effort: false, rename_noreplace: false,
        limitations: vec![
            "WebDAV-Upload als Best Effort: neue Dateien und begrenzte Ordnerbäume unter dem gewählten Ordner; bedingtes PUT/MOVE, vollständige BLAKE3-Prüfung und keine künstliche Collection-/Löschprobe.".into(),
            "Die Serverbedingungen werden ohne Gegenprobe verwendet. Missachtet der Server If-None-Match oder Overwrite:F, kann eine späte Kollision genau ein Ziel ersetzen. Dauerhafte Speicherung und atomare lokale Elternbindung sind nicht bewiesen; unklare Wirkungen werden nicht wiederholt.".into(),
            super::NAMESPACE_BINDING_LIMITATION.into(),
            "Dateizeit, POSIX-Rechte, Owner, ACL, xattrs, Resource Forks, Sparse- und Hardlinkstruktur werden bei diesem Inhaltsupload nicht übertragen; die pfadgenaue Bilanz nennt Dateien und Ordner. Höchstens 100 Auswahleinträge, 1000 Baumeinträge, 16 Unterordnerebenen, 128 gehaltene Quellhandles, 8 GiB je Datei und 16 GiB je Auswahl; Vorbereitung höchstens 60 Sekunden.".into(),
        ], blockers: vec![super::FILE_REPLACE_BLOCKER.into(), "Quellentfernung gehört nicht zu dieser additiven Inhaltsklasse".into()],
    }
}

struct UploadBinding {
    vfs: Arc<WebDavVfs>,
    binding: BoundWebDavMutation,
    root: PathBuf,
    directories: Mutex<BTreeMap<PathBuf, Vec<WebDavResourceIdentity>>>,
    cancel: Cancel,
    owner: Option<InteractiveFileOwner>,
}
#[derive(Clone)]
struct Upload {
    binding: Arc<UploadBinding>,
    file: Option<Arc<UploadFile>>,
    parent: PathBuf,
}
struct UploadFile {
    target: PathBuf,
    intent: OnceLock<RemoteTempIntent>,
    owned: Arc<Mutex<BTreeMap<String, String>>>,
    state: Mutex<UploadState>,
}
#[derive(Default)]
struct UploadState {
    temp: Option<PathBuf>,
    hash: Option<blake3::Hash>,
    move_attempted: bool,
    delete_attempted: bool,
    settled: bool,
    directory_attempted: bool,
    directory_complete: bool,
}

impl WebDavVfs {
    pub(crate) fn interactive_upload(
        self: &Arc<Self>,
        root: &Path,
        cancel: &Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Arc<dyn Vfs>> {
        interactive_namespace::validate_path(root)?;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let components = virtual_components(root)?;
        let (binding, _) = self.bind_mutation(
            root,
            |_| true,
            "WebDAV-Upload benötigt eine gebundene Sitzung",
        )?;
        let parents = directory_chain(&binding, &components, Instant::now() + PREPARATION_LIMIT)?;
        self.ensure_mutation_binding_current(&binding)?;
        Ok(Arc::new(Upload {
            binding: Arc::new(UploadBinding {
                vfs: self.clone(),
                binding,
                root: root.to_path_buf(),
                directories: Mutex::new(BTreeMap::from([(root.to_path_buf(), parents.clone())])),
                cancel: cancel.clone(),
                owner,
            }),
            file: None,
            parent: root.to_path_buf(),
        }))
    }
}
fn cancelled() -> io::Error {
    coded_error(
        io::ErrorKind::Interrupted,
        ErrorCode::Interrupted,
        "WebDAV-Upload abgebrochen",
    )
}
impl Upload {
    fn check(&self) -> io::Result<()> {
        self.check_until(Instant::now() + PREPARATION_LIMIT)
    }
    fn check_until(&self, deadline: Instant) -> io::Result<()> {
        check_deadline(deadline)?;
        let b = &self.binding;
        if let Some(owner) = &b.owner {
            ensure_profile_current(&owner.staging.queue, &owner.profiles, &b.vfs.config)?;
        }
        b.vfs
            .ensure_mutation_binding_current(&b.binding)
            .map_err(|_| drift("WebDAV-Uploadsitzung änderte sich"))?;
        let expected = self.parents()?;
        if directory_chain(&b.binding, &virtual_components(&self.parent)?, deadline)? != expected {
            return Err(drift("WebDAV-Uploadeltern änderten sich"));
        }
        b.vfs
            .ensure_mutation_binding_current(&b.binding)
            .map_err(|_| drift("WebDAV-Uploadsitzung änderte sich"))?;
        if let Some(owner) = &b.owner {
            ensure_profile_current(&owner.staging.queue, &owner.profiles, &b.vfs.config)?;
        }
        check_deadline(deadline)
    }
    fn parents(&self) -> io::Result<Vec<WebDavResourceIdentity>> {
        lock(&self.binding.directories)
            .get(&self.parent)
            .cloned()
            .ok_or_else(|| drift("WebDAV-Unterordner besitzt keine gebundene Elternkette"))
    }
    fn directory_context(&self, path: &Path) -> io::Result<Self> {
        self.remote(path)?;
        if !lock(&self.binding.directories).contains_key(path) {
            let parent = path
                .parent()
                .ok_or_else(|| drift("WebDAV-Ordner ohne Eltern"))?;
            let context = self.directory_context(parent)?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| drift("Ungültiger Ordnername"))?;
            let children = context.children()?;
            let matching = children
                .iter()
                .filter(|entry| crate::remote_rename::names_may_collide(&entry.entry.name, name))
                .collect::<Vec<_>>();
            if matching.len() != 1
                || matching[0].entry.name != name
                || !matching[0].entry.is_dir
                || matching[0].entry.is_symlink
            {
                return Err(coded_error(
                    io::ErrorKind::AlreadyExists,
                    ErrorCode::AlreadyExists,
                    "WebDAV-Unterordner ist nicht eindeutig exakt zugeordnet",
                ));
            }
            let identity = WebDavVfs::resource_identity_bound(
                &self.binding.binding,
                &self.remote(path)?,
                true,
            )?;
            if matching[0].provider_token.as_deref() != Some(identity.etag.as_bytes()) {
                return Err(drift("WebDAV-Unterordner änderte sich beim Binden"));
            }
            context.check()?;
            let mut chain = context.parents()?;
            chain.push(identity);
            lock(&self.binding.directories).insert(path.to_path_buf(), chain);
        }
        let context = Self {
            binding: self.binding.clone(),
            file: None,
            parent: path.to_path_buf(),
        };
        context.check()?;
        Ok(context)
    }
    fn remote(&self, path: &Path) -> io::Result<String> {
        interactive_namespace::validate_path(path)?;
        if !path.starts_with(&self.binding.root) {
            return Err(drift("WebDAV-Upload verlässt den gewählten Ordner"));
        }
        map_virtual_path(&self.binding.binding.session.identity.canonical_root, path)
    }
    fn children(&self) -> io::Result<Vec<ProofListEntry>> {
        self.check()?;
        let entries = read_children(
            &self.binding.binding,
            &self.remote(&self.parent)?,
            Instant::now() + PREPARATION_LIMIT,
        )?;
        self.check()?;
        Ok(entries)
    }
    fn proof(&self, path: &Path) -> io::Result<ProofListEntry> {
        let remote = self.remote(path)?;
        let context = if path == self.binding.root {
            self.directory_context(path)?
        } else {
            self.directory_context(
                path.parent()
                    .ok_or_else(|| drift("WebDAV-Pfad ohne Eltern"))?,
            )?
        };
        context.check()?;
        let proof = stat_proof_in_session(&self.binding.binding.session, &remote)?;
        context.check()?;
        Ok(proof)
    }
    fn unclear(&self, phase: &str, error: impl std::fmt::Display) -> io::Error {
        let mut paths = vec![self.binding.root.as_path()];
        let temp;
        if let Some(file) = &self.file {
            temp = lock(&file.state).temp.clone();
            if let Some(temp) = &temp {
                paths.push(temp);
            }
            paths.push(&file.target);
        }
        crate::error::with_recovery_context(coded_error(io::ErrorKind::Other,ErrorCode::OutcomeUnclear,
            format!("WebDAV-Upload: {phase} hat einen unklaren Ausgang ({error}); genannte Pfade prüfen, keine automatische Wiederholung.")), &[], &paths)
    }
    fn file(&self) -> io::Result<&Arc<UploadFile>> {
        self.file
            .as_ref()
            .ok_or_else(|| drift("WebDAV-Upload benötigt seinen privaten Dateikontext"))
    }
    fn etag(proof: &ProofListEntry) -> io::Result<String> {
        if !proof.entry.is_file || proof.entry.is_symlink {
            return Err(invalid("WebDAV-Uploadbeleg ist keine reguläre Datei"));
        }
        let etag = proof
            .provider_token
            .as_ref()
            .and_then(|v| std::str::from_utf8(v).ok())
            .ok_or_else(|| invalid("WebDAV-Dateibeleg besitzt keine starke ETag"))?;
        validate_strong_etag(etag)?;
        Ok(etag.into())
    }
    fn owned_etag(&self, path: &Path) -> io::Result<String> {
        let file = self.file()?;
        let remote = self.remote(path)?;
        lock(&file.owned)
            .get(&remote)
            .cloned()
            .ok_or_else(|| drift("Der bei PUT gebundene eigene Dateitoken fehlt"))
    }
}

impl Vfs for Upload {
    fn interactive_write_limitations(&self) -> Vec<String> {
        self.file.as_ref().map(|file| vec![format!("{}: nur Inhalte beziehungsweise Ordnerstruktur übertragen; Dateizeit, Rechte, Owner, ACL, xattrs, Resource Forks, Sparse- und Hardlinkstruktur ausgelassen", file.target.display())]).unwrap_or_default()
    }

    fn additive_content_target_only(&self) -> bool {
        true
    }
    fn file_commit_class(&self) -> FileCommitClass {
        FileCommitClass::BestEffort
    }
    fn supports_mode_write(&self) -> bool {
        false
    }
    fn supports_mtime_write(&self) -> bool {
        false
    }
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(self
            .list_complete(path)?
            .entries
            .into_iter()
            .map(|e| e.entry)
            .collect())
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let context = self.directory_context(path)?;
        Ok(CompleteDirectoryListing {
            entries: context.children()?,
            provider_token: context
                .parents()?
                .last()
                .map(|p| p.etag.as_bytes().to_vec()),
            complete: true,
        })
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        Ok(self.proof(path)?.entry)
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn staging_scope(&self, target: &Path, _: Option<&Path>) -> io::Result<Option<Box<dyn Vfs>>> {
        self.remote(target)?;
        if target == self.binding.root
            || self
                .file
                .as_ref()
                .is_some_and(|file| target.parent() != Some(file.target.as_path()))
        {
            return Err(drift("Ungültiger WebDAV-Inhaltskontext"));
        }
        let parent = target
            .parent()
            .ok_or_else(|| drift("Uploadziel ohne Eltern"))?;
        self.directory_context(parent)?;
        Ok(Some(Box::new(Self {
            binding: self.binding.clone(),
            parent: parent.to_path_buf(),
            file: Some(Arc::new(UploadFile {
                target: target.to_path_buf(),
                intent: OnceLock::new(),
                owned: Default::default(),
                state: Default::default(),
            })),
        })))
    }
    fn staging_recovery_path(&self) -> Option<&Path> {
        self.file
            .as_ref()
            .and_then(|f| f.intent.get())
            .map(|i| i.path())
    }
    fn finish_staging(&self) -> io::Result<()> {
        let file = self.file()?;
        if let Some(intent) = file.intent.get() {
            let complete = {
                let state = lock(&file.state);
                state.settled || state.directory_complete
            };
            if !complete {
                return Err(self.unclear(
                    "Tempabschluss",
                    "möglicher Temp-/Zielbestand bleibt erhalten",
                ));
            }
            intent.finish()?;
        }
        Ok(())
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.directory_context(path)?.check()
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let file = self.file()?;
        if file.target != path
            || path.parent() != Some(self.parent.as_path())
            || lock(&file.state).directory_attempted
        {
            return Err(read_only());
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| drift("Ordnername fehlt"))?;
        // Geplante Quellnamen sind keine Benutzereingabe: Ein Trimmen würde einen
        // anderen Zielpfad erzeugen als den im dauerhaften Intent bestätigten.
        if crate::directory_create::validate_directory_create_request(&self.parent, name)? != name {
            return Err(coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "WebDAV-Uploadordner benötigt einen unverändert bedienbaren Namen",
            ));
        }
        // Ein bereits gebundener Ordner darf nach seinem Verschwinden nicht mit
        // einem neuen MKCOL samt neuem Token unbemerkt wiederbelebt werden.
        if lock(&self.binding.directories).contains_key(path) {
            return Err(drift(
                "Gebundener WebDAV-Zielordner verschwand; nicht neu angelegt",
            ));
        }
        if self.binding.cancel.is_cancelled() {
            return Err(cancelled());
        }
        let deadline = Instant::now() + PREPARATION_LIMIT;
        self.check_until(deadline)?;
        let result = self.binding.vfs.create_bound_directory(
            &self.parent,
            path.file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| drift("Ordnername fehlt"))?,
            &self.binding.binding,
            &self.parents()?,
            deadline,
            &mut || {
                self.check_until(deadline)?;
                if self.binding.cancel.is_cancelled() {
                    return Err(cancelled());
                }
                if let Some(owner) = &self.binding.owner {
                    file.intent
                        .set(RemoteTempIntent::create(&owner.staging, path, path)?)
                        .map_err(|_| drift("Ordnerintent bereits gebunden"))?;
                }
                lock(&file.state).directory_attempted = true;
                Ok(())
            },
        );
        match result {
            Ok((_, identity)) => {
                let identity = identity.ok_or_else(|| {
                    self.unclear(
                        "MKCOL-Readback",
                        "neuer Ordner besitzt keinen starken Elternbeleg; keine Kinderwirkung",
                    )
                })?;
                let mut parents = self.parents()?;
                parents.push(identity);
                lock(&self.binding.directories).insert(path.to_path_buf(), parents);
                Ok(())
            }
            Err(error) => {
                // Eine nachweislich abgelehnte Anlage hinterlässt keinen eigenen
                // Ordner. Unklare Antworten behalten ihren dauerhaften Prüfhinweis.
                if crate::error_code(&error) != ErrorCode::OutcomeUnclear {
                    lock(&file.state).settled = true;
                }
                Err(error)
            }
        }
    }
    fn create_dir_with_removal_proof(
        &self,
        path: &Path,
    ) -> Result<Option<ProofListEntry>, crate::vfs::MutationFailure> {
        self.create_dir(path).map(|()| None).map_err(|error| {
            if crate::error_code(&error) == ErrorCode::OutcomeUnclear {
                crate::vfs::MutationFailure::OutcomeUnclear(error)
            } else {
                crate::vfs::MutationFailure::Unchanged(error)
            }
        })
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let proof = self.proof(path)?;
        let etag = Self::etag(&proof)?;
        if let Some(file) = &self.file {
            let state = lock(&file.state);
            if state.temp.as_deref() != Some(path) && file.target != path {
                return Err(read_only());
            }
            let remote = self.remote(
                state
                    .temp
                    .as_deref()
                    .ok_or_else(|| drift("Uploadtemp fehlt"))?,
            )?;
            if lock(&file.owned).get(&remote) != Some(&etag) {
                return Err(drift("WebDAV-Uploaddatei wurde ausgetauscht"));
            }
        }
        let b = &self.binding.binding.session;
        let response = authenticated(
            b.http
                .client
                .get(resource_url(&b.identity.origin, &self.remote(path)?)?),
            &b.http,
        )
        .header(IF_MATCH, &etag)
        .timeout(Duration::from_secs(300))
        .send()
        .map_err(http_error)?;
        require_status(response.status(), &[StatusCode::OK], "WebDAV-Upload-GET")?;
        require_matching_etag(response.headers(), &etag, "WebDAV-Upload-GET")?;
        Ok(Box::new(UploadRead {
            response,
            context: self.clone(),
            path: path.to_path_buf(),
            etag,
        }))
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(read_only())
    }
    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = self.file()?;
        if lock(&file.state).temp.is_some()
            || path == file.target
            || path.parent() != Some(self.parent.as_path())
        {
            return Err(read_only());
        }
        let remote = self.remote(path)?;
        if self.binding.cancel.is_cancelled() {
            return Err(cancelled());
        }
        let entries = self.children()?;
        for entry in entries {
            if crate::remote_rename::names_may_collide(
                &entry.entry.name,
                file.target.file_name().unwrap().to_str().unwrap(),
            ) {
                return Err(drift(
                    "WebDAV-Uploadziel inzwischen belegt; neu vorbereiten",
                ));
            }
            if crate::remote_rename::names_may_collide(
                &entry.entry.name,
                path.file_name().unwrap().to_str().unwrap(),
            ) {
                return Err(coded_error(
                    io::ErrorKind::AlreadyExists,
                    ErrorCode::AlreadyExists,
                    "WebDAV-Tempname inzwischen belegt",
                ));
            }
        }
        self.check()?;
        if let Some(owner) = &self.binding.owner {
            file.intent
                .set(RemoteTempIntent::create(
                    &owner.staging,
                    &file.target,
                    path,
                )?)
                .map_err(|_| drift("Uploadintent bereits gebunden"))?;
        }
        let (reader, writer) = UnixStream::pair()?;
        if self.binding.cancel.is_cancelled() {
            lock(&file.state).settled = true;
            return Err(cancelled());
        }
        lock(&file.state).temp = Some(path.to_path_buf());
        let session = self.binding.binding.session.clone();
        let task_session = session.clone();
        let task_remote = remote.clone();
        let parents = self.parents()?;
        let owned = file.owned.clone();
        let task = std::thread::spawn(move || {
            put_stream_and_readback(
                task_session,
                task_remote,
                parents,
                reader,
                owned,
                Duration::from_secs(300),
            )
        });
        Ok(Box::new(UploadWrite {
            inner: WebDavWrite {
                writer: Some(writer),
                task: Some(task),
                source_hash: blake3::Hasher::new(),
                finished: false,
            },
            context: self.clone(),
        }))
    }
    fn open_write_new_with_removal_proof(
        &self,
        path: &Path,
    ) -> Result<(Box<dyn Write + Send>, Option<ProofListEntry>), crate::vfs::MutationFailure> {
        // Alle Fehler liegen vor dem Workerstart. Der PUT-Worker meldet seinen
        // Ausgang ausschließlich über den späteren Schreibstrom.
        self.open_write_new(path)
            .map(|writer| (writer, None))
            .map_err(crate::vfs::MutationFailure::Unchanged)
    }
    fn rename_plain_once_with_phase(
        &self,
        from: &Path,
        to: &Path,
    ) -> Result<(), crate::vfs::MutationFailure> {
        self.rename_plain_once(from, to).map_err(|error| {
            if self
                .file
                .as_ref()
                .is_some_and(|file| lock(&file.state).move_attempted)
            {
                crate::vfs::MutationFailure::OutcomeUnclear(error)
            } else {
                crate::vfs::MutationFailure::Unchanged(error)
            }
        })
    }
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        let file = self.file()?;
        if lock(&file.state).temp.as_deref() != Some(path) {
            return Err(read_only());
        }
        let proof = self.proof(path)?;
        if Self::etag(&proof)? != self.owned_etag(path)? {
            return Err(drift("Uploadtemp wurde vor dem Commit ausgetauscht"));
        }
        Ok(())
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.directory_context(path)?.check()?;
        if let Some(file) = &self.file {
            if file.target == path && lock(&file.state).directory_attempted {
                lock(&file.state).directory_complete = true;
            }
        }
        Ok(())
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        Err(read_only())
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        Err(read_only())
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        let file = self.file()?;
        {
            let state = lock(&file.state);
            if state.temp.as_deref() != Some(from)
                || file.target != to
                || state.move_attempted
                || state.hash.is_none()
            {
                return Err(read_only());
            }
        }
        if self.binding.cancel.is_cancelled() {
            return Err(cancelled());
        }
        self.check()?;
        let etag = self.owned_etag(from)?;
        let proof = self.proof(from)?;
        if Self::etag(&proof)? != etag {
            return Err(drift("Uploadtemp änderte sich vor MOVE"));
        }
        let name = to.file_name().unwrap().to_str().unwrap();
        if self
            .children()?
            .iter()
            .any(|e| crate::remote_rename::names_may_collide(&e.entry.name, name))
        {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::AlreadyExists,
                "Späte WebDAV-Zielkollision; vorhandenes Ziel bleibt erhalten",
            ));
        }
        self.check()?;
        lock(&file.state).move_attempted = true;
        let b = &self.binding.binding.session;
        let response = move_request(
            &b.http,
            &b.identity.origin,
            &self.remote(from)?,
            &self.remote(to)?,
            &etag,
            &self.parents()?,
        )
        .map_err(|e| self.unclear("MOVE", e))?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            // Eine Ablehnung ist keine Freigabe zum Löschen. Erst der unveränderte
            // eigene Tempbeleg und die frische Elternkette erlauben dessen Cleanup.
            self.check()
                .map_err(|e| self.unclear("MOVE-Ablehnung", e))?;
            let fresh = self
                .proof(from)
                .map_err(|e| self.unclear("MOVE-Ablehnung", e))?;
            if Self::etag(&fresh)? == etag {
                lock(&file.state).move_attempted = false;
                return Err(coded_error(
                    io::ErrorKind::AlreadyExists,
                    ErrorCode::AlreadyExists,
                    "Bedingtes WebDAV-MOVE abgelehnt; Zielbestand neu prüfen",
                ));
            }
            return Err(self.unclear("MOVE-Ablehnung", "Tempversion nicht unverändert"));
        }
        if !matches!(
            response.status(),
            StatusCode::CREATED | StatusCode::NO_CONTENT
        ) {
            return Err(self.unclear("MOVE", response.status()));
        }
        let verify = (|| -> io::Result<()> {
            let entries = self.children()?;
            if entries
                .iter()
                .any(|e| e.entry.name == from.file_name().unwrap().to_str().unwrap())
            {
                return Err(invalid("Tempname ist nach MOVE weiter vorhanden"));
            }
            let matching = entries
                .iter()
                .filter(|e| crate::remote_rename::names_may_collide(&e.entry.name, name))
                .collect::<Vec<_>>();
            if matching.len() != 1
                || matching[0].entry.name != name
                || matching[0].entry.size != proof.entry.size
                || Self::etag(matching[0])? != etag
            {
                return Err(invalid(
                    "MOVE-Readback bestätigt Namen, Größe und eigene Version nicht",
                ));
            }
            let hash = get_hash_with_timeout(
                &b.http,
                &b.identity.origin,
                &self.remote(to)?,
                &etag,
                Duration::from_secs(300),
            )?;
            if Some(hash) != lock(&file.state).hash {
                return Err(invalid("MOVE-Inhaltsreadback weicht ab"));
            }
            self.check()?;
            Ok(())
        })();
        verify.map_err(|e| self.unclear("MOVE-Readback", e))?;
        lock(&file.state).settled = true;
        Ok(())
    }
    fn cleanup_owned_temp(&self, path: &Path) -> io::Result<bool> {
        let file = self.file()?;
        if lock(&file.state).temp.as_deref() != Some(path) {
            return Ok(false);
        }
        let already_attempted = {
            let state = lock(&file.state);
            state.move_attempted || state.delete_attempted
        };
        if already_attempted {
            return Err(self.unclear("Temp-Cleanup", "Wirkung bereits versucht"));
        }
        let remote = self.remote(path)?;
        let Some(etag) = lock(&file.owned).get(&remote).cloned() else {
            return Ok(false);
        };
        let cleanup = (|| -> io::Result<()> {
            self.check()?;
            let proof = self.proof(path)?;
            if Self::etag(&proof)? != etag {
                return Err(drift("Eigener Uploadtemp wurde ausgetauscht; kein DELETE"));
            }
            lock(&file.state).delete_attempted = true;
            let b = &self.binding.binding.session;
            delete_owned(
                &b.http,
                &b.identity.origin,
                &remote,
                &etag,
                &self.parents()?,
                || stat_proof_in_session(b, &remote),
            )?;
            self.check()?;
            Ok(())
        })();
        cleanup.map_err(|e| self.unclear("bedingtes Temp-DELETE", e))?;
        lock(&file.state).settled = true;
        lock(&file.owned).remove(&remote);
        Ok(true)
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
}
struct UploadWrite {
    inner: WebDavWrite,
    context: Upload,
}
impl Write for UploadWrite {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner
            .write(buffer)
            .map_err(|e| self.context.unclear("PUT-Strom", e))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .flush()
            .map_err(|e| self.context.unclear("PUT/Readback", e))?;
        self.context
            .check()
            .map_err(|e| self.context.unclear("PUT-Elternreadback", e))?;
        lock(&self.context.file()?.state).hash = Some(self.inner.source_hash.finalize());
        Ok(())
    }
}
struct UploadRead {
    response: reqwest::blocking::Response,
    context: Upload,
    path: PathBuf,
    etag: String,
}
impl Read for UploadRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let count = self.response.read(buffer)?;
        if count == 0 && Upload::etag(&self.context.proof(&self.path)?)? != self.etag {
            return Err(drift("WebDAV-Inhalt änderte sich während GET"));
        }
        Ok(count)
    }
}
