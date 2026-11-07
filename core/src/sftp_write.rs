//! Private, wirkungsbezogene SFTP-Dateiaktionen. Kein Planner-/Sync-VFS.
//!
//! Sämtliche Namespacezugriffe bleiben SFTP-v3-Pfadbefehle. Vor und nach jedem
//! Befehl werden Root, Eltern und Sitzung geprüft. Diese Beobachtungen schließen
//! einen zwischenzeitlichen Pfadtausch ausdrücklich nicht atomar aus.

use super::interactive_read::Attributes;
use super::*;
use crate::error::{coded_error, with_recovery_context, ErrorCode};
use crate::local_staging::RemoteTempIntent;
use crate::remote_real_mode::InteractiveFileOwner;
use crate::vfs::{FileCommitClass, FileReplaceClass, ProofListEntry};
use crate::Cancel;
use russh_sftp::protocol::{FileType, OpenFlags, Packet};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const SFTP_WRITE_LIMITATION: &str = "SFTP · Best Effort: Gleichzeitige serverseitige Pfadänderungen können die Aktion auf ein anderes Ziel im selben Konto umlenken, auch außerhalb dieses Ordners.";

pub const SFTP_DELETE_LIMITATION: &str =
    "Die bestätigte Auswahl wird dauerhaft ohne Papierkorb oder Wiederherstellungsweg entfernt.";

const CHUNK: usize = 32 * 1024;

#[derive(Clone)]
pub(super) struct InteractiveMutations {
    provider: Arc<SftpVfs>,
    root: PathBuf,
    move_target: Option<PathBuf>,
    sync_metadata: bool,
    identity: SftpServerIdentity,
    generation: u64,
    session: Arc<SftpSession>,
    raw: Arc<RawSftpSession>,
    fsync: bool,
    posix_rename: bool,
    cancel: Cancel,
    owner: Option<Arc<InteractiveFileOwner>>,
    profile_ids: Vec<String>,
    state: Arc<Mutex<WriteState>>,
}

#[derive(Default)]
struct WriteState {
    observed_types: BTreeMap<PathBuf, FileType>,
    completed: BTreeMap<PathBuf, (u64, blake3::Hash)>,
    intents: BTreeMap<PathBuf, Arc<RemoteTempIntent>>,
}

impl SftpVfs {
    pub(crate) fn batch_rename_source(
        self: &Arc<Self>,
        root: &Path,
        path: &Path,
        cancel: &Cancel,
    ) -> io::Result<(Entry, [u8; 32])> {
        // Keine Inhaltsöffnung: nur die vorhandene Pfad-/Attributprüfung nutzen.
        let context = self.mutation_context(root, cancel, None)?;
        let attrs = context.attributes(path)?;
        let entry = entry_from_attributes(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_owned(),
            &attrs,
        )?;
        let digest =
            blake3::hash(&serde_json::to_vec(&Attributes::from(&attrs)).map_err(io::Error::other)?);
        Ok((entry, *digest.as_bytes()))
    }
    pub(crate) fn batch_rename_parents(
        self: &Arc<Self>,
        root: &Path,
        cancel: &Cancel,
    ) -> io::Result<crate::sftp_batch_rename::ParentProof> {
        let context = self.mutation_context(root, cancel, None)?;
        let remote_root = context.directory(root)?;
        let mut current = context.identity.canonical_root.clone();
        let mut paths = vec![current.clone()];
        for part in virtual_components(root)? {
            current = append_sftp_component(&current, part);
            paths.push(current.clone());
        }
        let observed = context.provider.runtime.block_on(async {
            let mut result = Vec::new();
            for path in paths {
                context.check()?;
                let attrs = context.record(
                    context
                        .session
                        .symlink_metadata(path.clone())
                        .await
                        .map_err(sftp_error),
                )?;
                let full = *blake3::hash(
                    &serde_json::to_vec(&Attributes::from(&attrs)).map_err(io::Error::other)?,
                )
                .as_bytes();
                let stable = *blake3::hash(
                    &serde_json::to_vec(&(attrs.permissions, attrs.uid, attrs.gid))
                        .map_err(io::Error::other)?,
                )
                .as_bytes();
                result.push((path, full, stable));
            }
            Ok::<_, io::Error>(result)
        })?;
        context.directory(root)?;
        Ok(crate::sftp_batch_rename::ParentProof {
            entries: observed,
            root: remote_root,
        })
    }
    pub(crate) fn interactive_replace_limitations(&self) -> Vec<String> {
        let mut limitations = vec!["Die bestätigten regulären Altdateien werden dauerhaft ohne Wiederherstellungsweg ersetzt.".into()];
        if !lock(&self.state).connection.posix_rename {
            limitations.push("Dieser Server bietet kein POSIX-Rename: Das Altziel wird vor dem Commit entfernt; scheitert der anschließende Rename, kann das Altziel bereits fehlen.".into());
        }
        limitations
    }

    pub(crate) fn content_sync_context(
        self: &Arc<Self>,
        root: &Path,
        cancel: &Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Arc<dyn Vfs>> {
        let mut context = self.mutation_context(root, cancel, owner)?;
        context.sync_metadata = true;
        Ok(Arc::new(context))
    }

    pub(crate) fn interactive_mutations(
        self: &Arc<Self>,
        root: &Path,
        cancel: &Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Arc<dyn Vfs>> {
        Ok(Arc::new(self.mutation_context(root, cancel, owner)?))
    }

    pub(crate) fn interactive_mutation_source(
        self: &Arc<Self>,
        root: &Path,
        items: &[PathBuf],
        cancel: &Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Arc<dyn Vfs>> {
        validate_interactive_write_selection(root, items)?;
        Ok(Arc::new(self.mutation_context(root, cancel, owner)?))
    }

    pub(crate) fn interactive_shared_move(
        self: &Arc<Self>,
        root: &Path,
        target: &Path,
        items: &[PathBuf],
        cancel: &Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<Option<Arc<dyn Vfs>>> {
        validate_interactive_write_selection(root, items)?;
        let mut context = self.mutation_context(root, cancel, owner)?;
        context.move_target = Some(validate_write_root(target)?);
        context.directory(target)?;
        if selection_has_omissions(&context, root, items, cancel)? {
            return Ok(None);
        }
        for item in items {
            let name = item
                .file_name()
                .ok_or_else(|| invalid("SFTP-Movequelle ohne Namen"))?;
            if context.optional(&target.join(name))?.is_some() {
                return Ok(None);
            }
        }
        Ok(Some(Arc::new(context)))
    }

    fn mutation_context(
        self: &Arc<Self>,
        root: &Path,
        cancel: &Cancel,
        owner: Option<InteractiveFileOwner>,
    ) -> io::Result<InteractiveMutations> {
        let root = validate_write_root(root)?;
        check_sftp_cancel(Some(cancel))?;
        self.reconnect_lost_interactive_read()?;
        let profile_ids = if let Some(owner) = &owner {
            let profiles = owner_profiles(owner)?;
            let ids = profiles
                .iter()
                .filter(|profile| profile.profile.connection == self.config)
                .map(|profile| profile.profile.id.clone())
                .collect::<Vec<_>>();
            if !profiles.is_empty() && ids.is_empty() {
                return Err(drift(
                    "SFTP-Profilbindung gehört zu keiner verbundenen Konfiguration",
                ));
            }
            ids
        } else {
            Vec::new()
        };
        let state = lock(&self.state);
        let context = InteractiveMutations {
            provider: self.clone(),
            root,
            move_target: None,
            sync_metadata: false,
            identity: state.connection.identity.clone(),
            generation: state.generation,
            session: state.connection.sftp.clone(),
            raw: state.connection.read_session.clone(),
            fsync: state.connection.fsync,
            posix_rename: state.connection.posix_rename,
            cancel: cancel.clone(),
            owner: owner.map(Arc::new),
            profile_ids,
            state: Default::default(),
        };
        drop(state);
        context.directory(&context.root)?;
        Ok(context)
    }
}

fn selection_has_omissions(
    vfs: &dyn Vfs,
    root: &Path,
    items: &[PathBuf],
    cancel: &Cancel,
) -> io::Result<bool> {
    crate::fileops::validate_file_operation_scope(root, items)?;
    let mut pending = items.to_vec();
    while let Some(path) = pending.pop() {
        check_sftp_cancel(Some(cancel))?;
        let entry = vfs.stat(&path)?;
        if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
            return Ok(true);
        }
        if entry.is_dir {
            let listing = vfs.list_complete(&path)?;
            crate::fileops::require_complete_safe_listing(&listing, "SFTP-Movebestand")?;
            pending.extend(
                listing
                    .entries
                    .into_iter()
                    .map(|child| path.join(&child.entry.name)),
            );
        }
    }
    Ok(false)
}

fn next_offset(offset: u64, bytes: usize) -> io::Result<u64> {
    offset
        .checked_add(bytes as u64)
        .ok_or_else(|| invalid("SFTP-Dateioffset überschreitet den Protokollbereich"))
}

pub fn validate_interactive_write_selection(root: &Path, items: &[PathBuf]) -> io::Result<()> {
    validate_write_root(root)?;
    if items.is_empty() {
        return Err(invalid("SFTP-Dateiauswahl ist leer"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for path in items {
        validate_write_root(path)?;
        if path.parent() != Some(root) || !seen.insert(path) {
            return Err(invalid(
                "SFTP-Dateiaktionen benötigen eindeutige direkte Einträge des Pane-Roots",
            ));
        }
    }
    Ok(())
}

fn validate_write_root(root: &Path) -> io::Result<PathBuf> {
    let normalized = normalize_virtual_path(root)?;
    if !root.is_absolute() || root.to_str() != Some(normalized.as_str()) {
        return Err(invalid(
            "SFTP-Dateiaktion benötigt einen normalisierten absoluten Pane-Root",
        ));
    }
    Ok(PathBuf::from(normalized))
}

fn drift(message: &str) -> io::Error {
    coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift, message)
}

fn owner_profiles(
    owner: &InteractiveFileOwner,
) -> io::Result<Vec<crate::VersionedProfile<crate::SftpProfile>>> {
    let bindings = owner
        .profiles
        .iter()
        .filter(|binding| binding.protocol == crate::RemoteProfileProtocol::Sftp)
        .collect::<Vec<_>>();
    if bindings.is_empty() {
        return Ok(Vec::new());
    }
    let config = owner
        .staging
        .queue
        .parent()
        .ok_or_else(|| drift("SFTP-Queueordner fehlt"))?;
    let profiles = crate::load_sftp_profiles_versioned(&config.join("sftp-profiles.json"))?;
    bindings
        .into_iter()
        .map(|binding| {
            profiles
                .iter()
                .find(|profile| {
                    profile.profile.id == binding.profile_id && profile.revision == binding.revision
                })
                .cloned()
                .ok_or_else(|| drift("SFTP-Profilrevision änderte sich während der Dateiaktion"))
        })
        .collect()
}

impl InteractiveMutations {
    // Gehaltene READ-/WRITE-Handles benötigen pro Paket nur die RAM-Bindung.
    // Profildatei und Elternpfade werden an OPEN, Abschluss und Namespacewirkung
    // frisch geprüft; eine erneute Pfadauflösung würde Handles nicht stärker binden.
    fn check_session(&self) -> io::Result<()> {
        check_sftp_cancel(Some(&self.cancel))?;
        let state = lock(&self.provider.state);
        if state.generation != self.generation || state.connection.identity != self.identity {
            return Err(drift("SFTP-Sitzung änderte sich während der Dateiaktion"));
        }
        Ok(())
    }

    fn check(&self) -> io::Result<()> {
        self.check_session()?;
        if let Some(owner) = &self.owner {
            let profiles = owner_profiles(owner)?;
            for id in &self.profile_ids {
                if !profiles.iter().any(|profile| {
                    profile.profile.id == *id && profile.profile.connection == self.provider.config
                }) {
                    return Err(drift(
                        "SFTP-Konfiguration des gebundenen Profils änderte sich",
                    ));
                }
            }
        }
        Ok(())
    }

    fn path(&self, path: &Path, mutate: bool) -> io::Result<String> {
        let normalized = normalize_virtual_path(path)?;
        if !path.is_absolute() || path != Path::new(&normalized) {
            return Err(invalid("SFTP-Dateipfad ist nicht normalisiert"));
        }
        let within_target = self
            .move_target
            .as_ref()
            .is_some_and(|root| path.starts_with(root));
        if (!path.starts_with(&self.root) && !within_target)
            || (mutate && (path == self.root || self.move_target.as_deref() == Some(path)))
        {
            return Err(invalid("SFTP-Dateiaktion verlässt den gewählten Pane-Root"));
        }
        let mut remote = self.identity.canonical_root.clone();
        for component in virtual_components(path)? {
            remote = append_sftp_component(&remote, component);
        }
        Ok(remote)
    }

    fn record<T>(&self, result: io::Result<T>) -> io::Result<T> {
        self.provider
            .record_interactive_read_failure(self.generation, result)
    }

    fn directory(&self, path: &Path) -> io::Result<String> {
        self.check()?;
        self.path(path, false)?;
        let components = virtual_components(path)?;
        let remote = self.record(self.provider.runtime.block_on(verify_sftp_directory_chain(
            &self.session,
            &self.identity.canonical_root,
            &components,
            Some(&self.cancel),
        )))?;
        self.check()?;
        Ok(remote)
    }

    fn parent(&self, path: &Path) -> io::Result<String> {
        let remote = self.path(path, true)?;
        self.directory(
            path.parent()
                .ok_or_else(|| invalid("SFTP-Pfad ohne Elternordner"))?,
        )?;
        Ok(remote)
    }

    fn attributes(&self, path: &Path) -> io::Result<FileAttributes> {
        let remote = if path == self.root {
            self.directory(path)?
        } else {
            self.parent(path)?
        };
        let attrs = self.record(
            self.provider
                .runtime
                .block_on(self.session.symlink_metadata(remote))
                .map_err(sftp_error),
        )?;
        self.check()?;
        Ok(attrs)
    }

    fn metadata_attributes(&self, path: &Path) -> io::Result<FileAttributes> {
        let expected = {
            let state = lock(&self.state);
            // Ein abgeschlossener eigener Tempstrom ist unabhängig von späteren
            // Pfadbeobachtungen eine Datei. Andere Metadatenziele benötigen den
            // zuvor durch stat/proof gebundenen Typ des Aufrufpfads.
            if state.completed.contains_key(path) {
                FileType::File
            } else {
                state.observed_types.get(path).copied().ok_or_else(|| {
                    drift("SFTP-Metadatenziel besitzt keinen gebundenen erwarteten Typ")
                })?
            }
        };
        let attrs = self.attributes(path)?;
        // SETSTAT folgt Symlinks. Der frische LSTAT muss deshalb vor der
        // Mutation einen echten Datei-/Ordnertyp belegen; der verbleibende
        // serverseitige Pfadwettlauf bleibt der SFTP-Best-Effort-Vertrag.
        // Die is_regular()-Bitprüfung der Bibliothek trifft auch auf Symlinks
        // zu; file_type() wertet dagegen die vollständige Typmaske aus.
        let kind = attrs.file_type();
        if kind != expected || (!kind.is_file() && !kind.is_dir()) {
            return Err(drift(
                "SFTP-Metadatenziel besitzt nicht den gebundenen Datei-/Ordnertyp",
            ));
        }
        Ok(attrs)
    }

    fn proof(&self, path: &Path) -> io::Result<ProofListEntry> {
        let attrs = self.attributes(path)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_owned();
        let entry = entry_from_attributes(name.clone(), &attrs)?;
        if entry.is_file && attrs.size.is_none() {
            return Err(drift("SFTP-Dateigröße fehlt"));
        }
        if self.sync_metadata {
            // Spätere Stat-Aufrufe dürfen den erwarteten Typ bei einem
            // beobachteten Austausch nicht still neu binden.
            lock(&self.state)
                .observed_types
                .entry(path.to_path_buf())
                .or_insert(attrs.file_type());
        }
        Ok(ProofListEntry {
            entry,
            raw_name: name.into_bytes(),
            safely_decoded: true,
            provider_token: None,
        })
    }

    fn unclear(&self, action: &str, error: io::Error, paths: &[&Path]) -> io::Error {
        with_recovery_context(coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear,
            format!("SFTP-{action} hat einen unklaren Ausgang ({error}); Bestand prüfen, nicht automatisch wiederholen")), &[&error], paths)
    }

    fn settle(&self, action: &str, result: io::Result<()>, paths: &[&Path]) -> io::Result<()> {
        self.record(result)
            .map_err(|error| self.unclear(action, error, paths))?;
        for path in paths {
            self.parent(path)
                .map_err(|error| self.unclear(action, error, paths))?;
        }
        Ok(())
    }

    fn optional(&self, path: &Path) -> io::Result<Option<Entry>> {
        match self.stat(path) {
            Ok(entry) => Ok(Some(entry)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn hash(&self, path: &Path) -> io::Result<(u64, blake3::Hash)> {
        let mut reader = self.open_read(path)?;
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0_u8; CHUNK];
        let mut bytes = 0u64;
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            hash.update(&buffer[..count]);
        }
        Ok((bytes, hash.finalize()))
    }

    fn rename_once(&self, from: &Path, to: &Path, replace: bool) -> io::Result<()> {
        let from_remote = self.parent(from)?;
        let to_remote = self.parent(to)?;
        let source = self.proof(from)?;
        if source.entry.is_symlink || (!source.entry.is_dir && !source.entry.is_file) {
            return Err(invalid(
                "SFTP-Rename folgt keinen Symlinks oder Spezialdateien",
            ));
        }
        let target = self.optional(to)?;
        if target.is_some() && !replace {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::AlreadyExists,
                "SFTP-Zielname ist belegt",
            ));
        }
        if replace
            && (!source.entry.is_file
                || target
                    .as_ref()
                    .is_none_or(|entry| !entry.is_file || entry.is_symlink))
        {
            return Err(invalid("SFTP-Ersetzen benötigt zwei reguläre Dateien"));
        }
        // Erst unmittelbar nach allen Inhalts-/Bestandschecks die Eltern erneut prüfen.
        self.parent(from)?;
        self.parent(to)?;
        let result = if replace && self.posix_rename {
            let mut data = Vec::new();
            for path in [&from_remote, &to_remote] {
                data.extend_from_slice(&(path.len() as u32).to_be_bytes());
                data.extend_from_slice(path.as_bytes());
            }
            self.provider.runtime.block_on(async {
                match self
                    .raw
                    .extended("posix-rename@openssh.com", data)
                    .await
                    .map_err(sftp_error)?
                {
                    Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
                    Packet::Status(status) => {
                        Err(sftp_error(russh_sftp::client::error::Error::Status(status)))
                    }
                    _ => Err(io::Error::other(
                        "SFTP-POSIX-Rename lieferte unerwartete Antwort",
                    )),
                }
            })
        } else {
            if replace {
                // V3-Rename ersetzt nicht. Der bestätigte irreversible Weg entfernt
                // deshalb zunächst das Altziel; Fehler danach erhalten den Tempbestand.
                self.remove_move_leaf_best_effort(to)?;
                self.parent(from).map_err(|error| {
                    self.unclear("Ersetzen nach Altzielentfernung", error, &[from, to])
                })?;
                self.parent(to).map_err(|error| {
                    self.unclear("Ersetzen nach Altzielentfernung", error, &[from, to])
                })?;
            }
            self.provider
                .runtime
                .block_on(self.session.rename(from_remote, to_remote))
                .map_err(sftp_error)
        };
        self.settle("Rename", result, &[from, to])?;
        let readback = (|| {
            if self.optional(from)?.is_some() {
                return Err(drift("SFTP-Renamequelle besteht noch"));
            }
            let current = self.stat(to)?;
            if current.is_dir != source.entry.is_dir
                || current.is_file != source.entry.is_file
                || current.is_symlink
            {
                return Err(drift("SFTP-Renameziel besitzt einen anderen Typ"));
            }
            let expected = { lock(&self.state).completed.get(from).copied() };
            if let Some(expected) = expected {
                if self.hash(to)? != expected {
                    return Err(drift("SFTP-Commitinhalt änderte sich"));
                }
            }
            Ok(())
        })();
        readback.map_err(|error| self.unclear("Rename-Readback", error, &[from, to]))?;
        let intent = lock(&self.state).intents.remove(from);
        if let Some(intent) = intent {
            intent
                .finish()
                .map_err(|error| self.unclear("Intentabschluss", error, &[from, to]))?;
        }
        lock(&self.state).completed.remove(from);
        Ok(())
    }

    fn remove_leaf(&self, path: &Path, directory: bool) -> io::Result<()> {
        let remote = self.parent(path)?;
        let proof = self.proof(path)?;
        if proof.entry.is_symlink
            || (directory && !proof.entry.is_dir)
            || (!directory && !proof.entry.is_file)
        {
            return Err(drift("SFTP-Entfernung fand einen anderen Eintragstyp"));
        }
        if directory && !self.list_complete(path)?.entries.is_empty() {
            return Err(drift(
                "SFTP-Ordner ist nicht leer; keine rekursive Blindlöschung",
            ));
        }
        self.parent(path)?;
        let result = self
            .provider
            .runtime
            .block_on(async {
                if directory {
                    self.session.remove_dir(remote).await
                } else {
                    self.session.remove_file(remote).await
                }
            })
            .map_err(sftp_error);
        self.settle("Entfernen", result, &[path])?;
        match self.optional(path) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(self.unclear(
                "Entfernungs-Readback",
                drift("SFTP-Eintrag besteht noch"),
                &[path],
            )),
            Err(error) => Err(self.unclear("Entfernungs-Readback", error, &[path])),
        }
    }
}

impl Vfs for InteractiveMutations {
    fn supports_interactive_best_effort_move(&self) -> bool {
        self.move_target.is_some()
    }
    fn supports_best_effort_permanent_delete(&self) -> bool {
        true
    }
    fn verify_transferred_source(&self, path: &Path, hash: &blake3::Hash) -> io::Result<()> {
        if self.hash(path)?.1 != *hash {
            return Err(drift(
                "SFTP-Quellinhalt änderte sich während der Übertragung",
            ));
        }
        Ok(())
    }
    fn root_identity(&self, path: &Path) -> io::Result<crate::RootIdentity> {
        self.directory(path)?;
        Ok(crate::RootIdentity::Sftp(crate::remote::SftpRootIdentity {
            server: self.identity.clone(),
            virtual_root: path.to_string_lossy().into_owned(),
            session_generation: self.generation,
        }))
    }
    fn safety_capabilities_digest(&self) -> io::Result<Option<[u8; 32]>> {
        self.provider.safety_capabilities_digest()
    }
    fn file_commit_class(&self) -> FileCommitClass {
        FileCommitClass::BestEffort
    }
    fn file_flush_guarantees_at(&self, _: &Path) -> io::Result<(bool, bool)> {
        Ok((false, false))
    }
    fn file_replace_class(&self) -> FileReplaceClass {
        FileReplaceClass::BestEffort
    }
    fn supports_mode_write(&self) -> bool {
        self.sync_metadata
    }
    fn supports_mtime_write(&self) -> bool {
        self.sync_metadata
    }
    fn interactive_write_limitations(&self) -> Vec<String> {
        if self.sync_metadata {
            return vec![SFTP_WRITE_LIMITATION.into(), "SFTP v3 überträgt Dateizeit und POSIX-Modus per SETSTAT mit Readback. ACL, xattrs, Resource Forks und Hardlink-/Sparse-Struktur besitzen im verwendeten SFTP-v3-Protokoll keine portable Darstellung; Owner wird nur gelesen, nicht kontenübergreifend gesetzt.".into()];
        }
        vec![SFTP_WRITE_LIMITATION.into(), "SFTP überträgt Inhalte und Ordnerstruktur; Dateizeit, Rechte, Owner, ACL, xattrs, Resource Forks, Sparse- und Hardlinkstruktur bleiben ausgelassen.".into()]
    }
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(self
            .list_complete(path)?
            .entries
            .into_iter()
            .map(|proof| proof.entry)
            .collect())
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        let remote = self.directory(path)?;

        let records = self.record(self.provider.runtime.block_on(read_sftp_display_entries(
            &self.provider,
            self.generation,
            &self.raw,
            &remote,
            &self.cancel,
        )))?;
        let mut entries = Vec::with_capacity(records.len());
        for (name, attrs) in records {
            validate_remote_name(&name)?;
            let entry = entry_from_attributes(name.clone(), &attrs)?;
            if entry.is_file && attrs.size.is_none() {
                return Err(drift("SFTP-Listing enthält keine Dateigröße"));
            }
            entries.push(ProofListEntry {
                entry,
                raw_name: name.into_bytes(),
                safely_decoded: true,
                provider_token: None,
            });
        }
        self.directory(path)?;
        Ok(CompleteDirectoryListing {
            entries,
            provider_token: None,
            complete: true,
        })
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        Ok(self.proof(path)?.entry)
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.path(path, false)?;
        let mut current = self.root.clone();
        for component in path
            .strip_prefix(&self.root)
            .map_err(|_| invalid("SFTP-Ordner außerhalb Root"))?
            .components()
        {
            current.push(component);
            match self.optional(&current)? {
                Some(entry) if entry.is_dir && !entry.is_symlink => {}
                Some(_) => return Err(drift("SFTP-Ordnerpfad ist belegt")),
                None => self.create_dir(&current)?,
            }
        }
        Ok(())
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let remote = self.parent(path)?;
        let parent = path.parent().unwrap();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("SFTP-Ordnername fehlt"))?;
        if self
            .list_complete(parent)?
            .entries
            .iter()
            .any(|entry| crate::remote_rename::names_may_collide(&entry.entry.name, name))
        {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::AlreadyExists,
                "SFTP-Ordnername ist belegt",
            ));
        }
        let intent = self
            .owner
            .as_ref()
            .map(|owner| {
                RemoteTempIntent::create_bound_with_root(
                    &owner.staging,
                    path,
                    path,
                    &self.root_identity(&self.root)?,
                    self.provider.account_digest(),
                )
            })
            .transpose()?;
        self.parent(path)?;
        let result = self
            .provider
            .runtime
            .block_on(self.session.create_dir(remote))
            .map_err(sftp_error);
        self.settle("Ordneranlage", result, &[path])?;
        let entry = self
            .stat(path)
            .map_err(|error| self.unclear("Ordner-Readback", error, &[path]))?;
        if !entry.is_dir || entry.is_symlink {
            return Err(self.unclear(
                "Ordner-Readback",
                drift("SFTP-Ergebnis ist kein echter Ordner"),
                &[path],
            ));
        }
        if let Some(intent) = intent {
            intent
                .finish()
                .map_err(|error| self.unclear("Ordner-Intentabschluss", error, &[path]))?;
        }
        Ok(())
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let remote = self.parent(path)?;
        let attrs = self.attributes(path)?;
        if !attrs.is_regular() || attrs.size.is_none() {
            return Err(invalid(
                "SFTP-Inhalt benötigt eine reguläre Datei mit bekannter Größe",
            ));
        }
        self.parent(path)?;
        let handle = self
            .record(
                self.provider
                    .runtime
                    .block_on(
                        self.raw
                            .open(remote, OpenFlags::READ, FileAttributes::empty()),
                    )
                    .map_err(sftp_error),
            )?
            .handle;
        Ok(Box::new(Reader {
            context: self.clone(),
            path: path.to_path_buf(),
            handle: Some(handle),
            size: attrs.size.unwrap(),
            attributes: Attributes::from(&attrs),
            offset: 0,
        }))
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(invalid("SFTP schreibt ausschließlich neue Tempdateien"))
    }
    fn open_write_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let remote = self.parent(path)?;
        if self.optional(path)?.is_some() {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::AlreadyExists,
                "SFTP-Tempname ist belegt",
            ));
        }
        let intent = self
            .owner
            .as_ref()
            .map(|owner| {
                RemoteTempIntent::create_bound_with_root(
                    &owner.staging,
                    path,
                    path,
                    &self.root_identity(&self.root)?,
                    self.provider.account_digest(),
                )
            })
            .transpose()?;
        self.parent(path)?;
        let opened = self
            .provider
            .runtime
            .block_on(self.raw.open(
                remote,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                FileAttributes {
                    permissions: Some(0o600),
                    ..FileAttributes::empty()
                },
            ))
            .map_err(sftp_error);
        let handle = self
            .record(opened)
            .map_err(|error| self.unclear("Tempanlage", error, &[path]))?
            .handle;
        if let Some(intent) = intent {
            lock(&self.state)
                .intents
                .insert(path.to_path_buf(), Arc::new(intent));
        }
        Ok(Box::new(Writer {
            context: self.clone(),
            path: path.to_path_buf(),
            handle: Some(handle),
            offset: 0,
            hash: blake3::Hasher::new(),
            failed: false,
        }))
    }
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        let expected = lock(&self.state)
            .completed
            .get(path)
            .copied()
            .ok_or_else(|| drift("SFTP-Tempstrom wurde nicht erfolgreich geschlossen"))?;
        if self.hash(path)? != expected {
            return Err(drift("SFTP-Tempinhalt stimmt im Readback nicht überein"));
        }
        Ok(())
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.directory(path).map(|_| ())
    }
    fn set_mtime(&self, path: &Path, mtime: i64) -> io::Result<()> {
        if !self.sync_metadata {
            return Err(invalid("SFTP-Inhaltsklasse setzt keine Dateizeit"));
        }
        let before = self.metadata_attributes(path)?;
        let mtime = u32::try_from(mtime).map_err(|_| {
            crate::content_sync::mtime_omission("SFTP v3 kann diese Dateizeit nicht darstellen")
        })?;
        let remote = self.parent(path).and_then(|_| self.path(path, true))?;
        let attrs = FileAttributes {
            atime: Some(before.atime.unwrap_or(mtime)),
            mtime: Some(mtime),
            ..FileAttributes::empty()
        };
        self.settle(
            "SETSTAT",
            self.provider
                .runtime
                .block_on(self.session.set_metadata(remote, attrs))
                .map_err(sftp_error),
            &[path],
        )?;
        let after = self.metadata_attributes(path)?;
        if after.file_type() != before.file_type() || after.mtime != Some(mtime) {
            return Err(drift("SFTP-Dateizeit wurde nicht exakt zurückgelesen"));
        }
        Ok(())
    }
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        if !self.sync_metadata {
            return Err(invalid("SFTP-Inhaltsklasse setzt keine Rechte"));
        }
        let before = self.metadata_attributes(path)?;
        let remote = self.parent(path).and_then(|_| self.path(path, true))?;
        self.settle(
            "SETSTAT",
            self.provider
                .runtime
                .block_on(self.session.set_metadata(
                    remote,
                    FileAttributes {
                        permissions: Some(mode & 0o7777),
                        ..FileAttributes::empty()
                    },
                ))
                .map_err(sftp_error),
            &[path],
        )?;
        let after = self.metadata_attributes(path)?;
        if after.file_type() != before.file_type()
            || after.permissions.map(|value| value & 0o7777) != Some(mode & 0o7777)
        {
            return Err(drift("SFTP-Rechte wurden nicht exakt zurückgelesen"));
        }
        Ok(())
    }
    fn set_interactive_mode(&self, path: &Path, expected: &Entry, mode: u32) -> io::Result<Entry> {
        if !self.sync_metadata || mode & !0o777 != 0 {
            return Err(invalid("Ungültige gewöhnliche SFTP-Rechteänderung"));
        }
        let before = self.metadata_attributes(path)?;
        let observed = entry_from_attributes(expected.name.clone(), &before)?;
        if !crate::file_properties::same_observation(expected, &observed) {
            return Err(drift("SFTP-Objekt änderte sich vor der Rechteänderung"));
        }
        let full_mode = before
            .permissions
            .ok_or_else(|| invalid("SFTP meldet keine lesbaren Unix-Rechte"))?
            & 0o7777;
        let requested = full_mode & 0o7000 | mode;
        let remote = self.parent(path).and_then(|_| self.path(path, true))?;
        let attempted = requested != full_mode;
        if attempted {
            let result = self.provider.runtime.block_on(self.session.set_metadata(
                remote,
                FileAttributes {
                    permissions: Some(requested),
                    ..FileAttributes::empty()
                },
            ));
            if let Err(error) = result {
                let unchanged = matches!(&error, russh_sftp::client::error::Error::Status(status) if matches!(status.status_code, StatusCode::NoSuchFile | StatusCode::PermissionDenied | StatusCode::BadMessage | StatusCode::OpUnsupported));
                let error = sftp_error(error);
                return Err(if unchanged {
                    error
                } else {
                    self.unclear("SETSTAT", error, &[path])
                });
            }
        }
        let checked = (|| {
            self.parent(path)?;
            let after = self.metadata_attributes(path)?;
            let actual = entry_from_attributes(expected.name.clone(), &after)?;
            let mut changed = expected.clone();
            changed.mode = Some(mode);
            if !crate::file_properties::same_observation(&changed, &actual)
                || after.permissions.map(|value| value & 0o7777) != Some(requested)
            {
                return Err(drift(
                    "SFTP-Rechte oder Objektfakten nach SETSTAT abweichend",
                ));
            }
            Ok(actual)
        })();
        checked.map_err(|error| {
            if attempted {
                self.unclear("SETSTAT", error, &[path])
            } else {
                error
            }
        })
    }
    fn metadata_capabilities(&self, _: &Path) -> io::Result<MetadataCapabilities> {
        Ok(MetadataCapabilities {
            owner: Capability::ReadOnly,
            special_mode_bits: if self.sync_metadata {
                Capability::Exact
            } else {
                Capability::ReadOnly
            },
            acl_model: AclModel::Unsupported,
            acl: crate::metadata::Capability::Unknown,
            xattrs: Capability::Unsupported,
            resource_fork: Capability::Unsupported,
            named_streams: Capability::Unsupported,
            sparse_extents: Capability::Unsupported,
            hardlinks: Capability::Unsupported,
        })
    }
    fn metadata_manifest(&self, path: &Path) -> io::Result<crate::metadata::MetadataManifest> {
        let attrs = self.attributes(path)?;
        crate::metadata::MetadataManifest::new(
            attrs
                .uid
                .zip(attrs.gid)
                .map(|(uid, gid)| crate::metadata::Owner { uid, gid }),
            attrs.permissions.unwrap_or_default() & 0o7777,
            None,
            vec![],
            None,
            None,
            None,
        )
    }
    fn set_metadata_normal_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.set_mode(path, mode & 0o777)
    }
    fn set_metadata_special_bits(&self, path: &Path, bits: u32) -> io::Result<()> {
        let mode = self.attributes(path)?.permissions.unwrap_or_default();
        self.set_mode(path, mode & 0o777 | bits & 0o7000)
    }

    fn remove_file(&self, _: &Path) -> io::Result<()> {
        Err(invalid(
            "SFTP-Entfernung benötigt den bestätigten Best-Effort-Vertrag",
        ))
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(invalid(
            "SFTP benötigt einen vollständigen begrenzten Löschplan",
        ))
    }
    fn remove_dir(&self, _: &Path) -> io::Result<()> {
        Err(invalid(
            "SFTP-Entfernung benötigt den bestätigten Best-Effort-Vertrag",
        ))
    }
    fn remove_move_leaf_best_effort(&self, path: &Path) -> io::Result<()> {
        self.remove_leaf(path, false)
    }
    fn remove_move_empty_dir_best_effort(&self, path: &Path) -> io::Result<()> {
        self.remove_leaf(path, true)
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(invalid(
            "SFTP verwendet ausschließlich bestätigtes Best-Effort-Rename",
        ))
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(invalid("SFTP behauptet kein atomares No-Replace-Rename"))
    }
    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename_once(from, to, false)
    }
    fn rename_replace_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename_once(from, to, true)
    }
    fn available_bytes(&self, _: &Path) -> io::Result<u64> {
        Err(invalid("SFTP-Platz ist in dieser Klasse unbekannt"))
    }
}

struct Reader {
    context: InteractiveMutations,
    path: PathBuf,
    handle: Option<String>,
    size: u64,
    attributes: Attributes,
    offset: u64,
}
impl Reader {
    fn verify(&self, handle: &str) -> io::Result<()> {
        let attrs = self
            .context
            .record(
                self.context
                    .provider
                    .runtime
                    .block_on(self.context.raw.fstat(handle))
                    .map_err(sftp_error),
            )?
            .attrs;
        let path_attrs = self.context.attributes(&self.path)?;
        // atime darf sich durch das eigene Lesen ändern. Größe, mtime, Rechte
        // und Owner müssen dagegen am gehaltenen Handle und am Pfad gleich bleiben.
        if !attrs.is_regular()
            || !path_attrs.is_regular()
            || Attributes::from(&attrs) != self.attributes
            || Attributes::from(&path_attrs) != self.attributes
        {
            return Err(drift("SFTP-Datei änderte ihre gebundenen Leseattribute"));
        }
        Ok(())
    }
}
impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // Read::read_to_end wiederholt Interrupted automatisch. Abbruch und
        // beobachtete Drift müssen den Stream mit erhaltenem Fachcode beenden.
        self.read_chunk(buffer).map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                with_recovery_context(
                    coded_error(
                        io::ErrorKind::Other,
                        crate::error_code(&error),
                        error.to_string(),
                    ),
                    &[&error],
                    &[],
                )
            } else {
                error
            }
        })
    }
}
impl Reader {
    fn read_chunk(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.context.check_session()?;
        let Some(handle) = self.handle.clone() else {
            return Ok(0);
        };
        if self.offset == 0 || self.offset == self.size {
            self.verify(&handle)?;
        }
        if self.offset == self.size {
            self.handle.take();
            self.context.record(
                self.context
                    .provider
                    .runtime
                    .block_on(self.context.raw.close(handle))
                    .map(|_| ())
                    .map_err(sftp_error),
            )?;
            self.context.check()?;
            return Ok(0);
        }
        let count = buffer
            .len()
            .min(CHUNK)
            .min((self.size - self.offset).min(CHUNK as u64) as usize) as u32;
        let result = self.context.record(
            self.context
                .provider
                .runtime
                .block_on(self.context.raw.read(handle, self.offset, count))
                .map_err(sftp_error),
        )?;
        if result.data.is_empty() || result.data.len() > count as usize {
            return Err(drift("SFTP-Datei lieferte unerwartete Inhaltslänge"));
        }
        buffer[..result.data.len()].copy_from_slice(&result.data);
        self.offset += result.data.len() as u64;
        Ok(result.data.len())
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.context.record(
                self.context
                    .provider
                    .runtime
                    .block_on(self.context.raw.close(handle))
                    .map(|_| ())
                    .map_err(sftp_error),
            );
        }
    }
}
struct Writer {
    context: InteractiveMutations,
    path: PathBuf,
    handle: Option<String>,
    offset: u64,
    hash: blake3::Hasher,
    failed: bool,
}
impl Write for Writer {
    // std::Write::write_all wiederholt Interrupted automatisch. Bei einem
    // mutierenden Providerstrom darf weder Abbruch noch Antwortverlust einen
    // weiteren WRITE auslösen; jede fehlgeschlagene Teilwirkung beendet den Strom.
    fn write_all(&mut self, mut buffer: &[u8]) -> io::Result<()> {
        while !buffer.is_empty() {
            let count = self.write(buffer)?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "SFTP-Schreibstrom nahm keine Bytes an",
                ));
            }
            buffer = &buffer[count..];
        }
        Ok(())
    }

    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.context.check_session()?;
        let handle = self
            .handle
            .clone()
            .ok_or_else(|| drift("SFTP-Schreibhandle ist geschlossen"))?;
        let count = buffer.len().min(CHUNK);
        let end_offset = next_offset(self.offset, count)?;
        let result = self
            .context
            .provider
            .runtime
            .block_on(
                self.context
                    .raw
                    .write(handle, self.offset, buffer[..count].to_vec()),
            )
            .map(|_| ())
            .map_err(sftp_error);
        if let Err(error) = self.context.record(result) {
            self.failed = true;
            return Err(self.context.unclear("Schreiben", error, &[&self.path]));
        }
        self.hash.update(&buffer[..count]);
        self.offset = end_offset;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.context.check()?;
        let Some(handle) = self.handle.take() else {
            return if self.failed {
                Err(drift("SFTP-Schreibstrom ist fehlgeschlagen"))
            } else {
                Ok(())
            };
        };
        let result = self.context.provider.runtime.block_on(async {
            let flushed = if self.context.fsync {
                self.context
                    .raw
                    .fsync(handle.clone())
                    .await
                    .map(|_| ())
                    .map_err(sftp_error)
            } else {
                Ok(())
            };
            let closed = self.context.record(
                self.context
                    .raw
                    .close(handle)
                    .await
                    .map(|_| ())
                    .map_err(sftp_error),
            );
            let flushed = match flushed {
                Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(()),
                result => result,
            };
            flushed.and(closed)
        });
        if let Err(error) = self.context.record(result) {
            self.failed = true;
            return Err(self.context.unclear("Dateiabschluss", error, &[&self.path]));
        }
        self.context.check().map_err(|error| {
            self.context
                .unclear("Profilprüfung nach Dateiabschluss", error, &[&self.path])
        })?;
        if !self.failed {
            lock(&self.context.state)
                .completed
                .insert(self.path.clone(), (self.offset, self.hash.finalize()));
        }
        Ok(())
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.context.record(
                self.context
                    .provider
                    .runtime
                    .block_on(self.context.raw.close(handle))
                    .map(|_| ())
                    .map_err(sftp_error),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sftp_offsets_bleiben_ueber_acht_gib_streamfaehig_und_stoppen_nur_am_u64_ende() {
        assert_eq!(
            next_offset(8 * 1024 * 1024 * 1024, CHUNK).unwrap(),
            8 * 1024 * 1024 * 1024 + CHUNK as u64
        );
        assert_eq!(
            next_offset(u64::MAX - CHUNK as u64, CHUNK).unwrap(),
            u64::MAX
        );
        assert!(next_offset(u64::MAX, 1).is_err());
    }
    #[test]
    fn schreibroot_akzeptiert_keine_lexikalischen_auswege_oder_aliaspfade() {
        for path in [
            "relativ",
            "/pane/..",
            "/pane/../fremd",
            "/pane//kind",
            "/pane/./kind",
            "/pane/",
            "/pa\nne",
        ] {
            assert!(validate_write_root(Path::new(path)).is_err(), "{path}");
        }
        assert!(validate_write_root(Path::new("/")).is_ok());
        assert!(validate_write_root(Path::new("/pane/Grüße %2F")).is_ok());
    }
}
