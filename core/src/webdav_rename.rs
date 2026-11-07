//! Private Rename-Sicht: gebundene Metadaten und genau eine bedingte Mutation,
//! ohne die strengeren Schreibfähigkeiten des öffentlichen VFS freizugeben.

use super::interactive_namespace::{
    check_deadline, directory_chain, drift, read_children, PREPARATION_LIMIT,
};
use super::*;
use crate::remote_rename::{RenameExecutionClass, RenameReport};
use crate::vfs::CompleteDirectoryListing;
use std::path::PathBuf;
use std::time::Instant;

pub(crate) fn validate_request(source: &Path, new_name: &str) -> io::Result<()> {
    interactive_namespace::validate_path(source)?;
    let parent = source
        .parent()
        .ok_or_else(|| drift("Umbenennen benötigt einen Elternordner"))?;
    interactive_namespace::validate_path(&parent.join(new_name))
}

impl WebDavVfs {
    pub(crate) fn execute_ordinary_rename(
        &self,
        source: &Path,
        new_name: &str,
    ) -> io::Result<RenameReport> {
        self.execute_bound_rename(source, new_name, None)
            .map(|(report, _)| report)
    }

    fn execute_bound_rename(
        &self,
        source: &Path,
        new_name: &str,
        expected: Option<(
            &crate::RootIdentity,
            &crate::remote_batch_rename::ParentProof,
            [u8; 32],
        )>,
    ) -> io::Result<(RenameReport, crate::remote_batch_rename::RemoteBatchRefresh)> {
        validate_request(source, new_name)?;
        let parent = source
            .parent()
            .ok_or_else(|| drift("Umbenennen benötigt einen Elternordner"))?
            .to_path_buf();
        let components = virtual_components(&parent)?;
        let (binding, remote_parent) = self.bind_mutation(
            &parent,
            |_| true,
            "WebDAV-Umbenennung benötigt eine gebundene Sitzung",
        )?;
        let deadline = Instant::now() + PREPARATION_LIMIT;
        let parents = directory_chain(&binding, &components, deadline)?;
        let context = InteractiveRename {
            vfs: self,
            binding,
            parents: Mutex::new(parents),
            components,
            parent,
            remote_parent,
            source: source.to_path_buf(),
            destination: source.parent().unwrap().join(new_name),
            state: Mutex::new(RenameState {
                deadline,
                source_proof: None,
                source_children: None,
                before_children: None,
                acknowledged: false,
                expected_source: expected.map(|value| value.2),
                attempted: false,
                definite_no_effect: false,
            }),
        };
        if let Some((identity, parents, _)) = expected {
            let current = context.batch_refresh()?;
            if &current.identity != identity || &current.parents != parents {
                return Err(drift("WebDAV-Stapelbindung änderte sich vor MOVE"));
            }
        }
        let crate::RenameInteractionContract::Confirmed { limitations, .. } =
            crate::ordinary_rename_contract(crate::ProviderKind::WebDav)
        else {
            unreachable!()
        };
        let result = crate::remote_rename::rename_with_class(
            &context,
            source,
            new_name,
            &None,
            RenameExecutionClass::BestEffort,
            limitations,
            None,
        );
        let possible_effect = {
            let state = lock(&context.state);
            state.attempted && !state.definite_no_effect
        };
        match result {
            Err(error) if possible_effect => {
                if crate::error_code(&error) == ErrorCode::OutcomeUnclear {
                    let paths = [&context.source, &context.destination, &context.parent]
                        .map(|path| path.to_string_lossy().into_owned());
                    let known = crate::error::recovery_paths(&error);
                    if paths.iter().all(|path| known.contains(path)) {
                        Err(error)
                    } else {
                        Err(crate::error::with_recovery_paths(error, &paths))
                    }
                } else {
                    Err(context.unclear(error))
                }
            }
            result => result.and_then(|report| {
                context
                    .batch_refresh()
                    .map(|refresh| (report, refresh))
                    .map_err(|error| {
                        if possible_effect {
                            context.unclear(error)
                        } else {
                            error
                        }
                    })
            }),
        }
    }
}

struct RenameState {
    deadline: Instant,
    source_proof: Option<ProofListEntry>,
    source_children: Option<Vec<ProofListEntry>>,
    before_children: Option<Vec<ProofListEntry>>,
    acknowledged: bool,
    expected_source: Option<[u8; 32]>,
    attempted: bool,
    definite_no_effect: bool,
}

struct InteractiveRename<'a> {
    vfs: &'a WebDavVfs,
    binding: BoundWebDavMutation,
    parents: Mutex<Vec<WebDavResourceIdentity>>,
    components: Vec<String>,
    parent: PathBuf,
    remote_parent: String,
    source: PathBuf,
    destination: PathBuf,
    state: Mutex<RenameState>,
}

impl InteractiveRename<'_> {
    fn batch_refresh(&self) -> io::Result<crate::remote_batch_rename::RemoteBatchRefresh> {
        self.vfs.ensure_mutation_binding_current(&self.binding)?;
        let parents = lock(&self.parents).clone();
        let last = parents
            .last()
            .ok_or_else(|| drift("WebDAV-Elternbeleg fehlt"))?;
        Ok(crate::remote_batch_rename::RemoteBatchRefresh {
            identity: crate::RootIdentity::WebDav(WebDavRootIdentity {
                server: self.binding.session.identity.clone(),
                virtual_root: normalize_virtual_path(&self.parent)?,
                root_etag: last.etag.clone().unwrap_or_default(),
                namespace_only: last.etag.is_none(),
                session_generation: self.binding.generation,
            }),
            parents: parent_proof(&parents)?,
        })
    }

    fn read_after_own_move(&self) -> io::Result<CompleteDirectoryListing> {
        self.vfs.ensure_mutation_binding_current(&self.binding)?;
        let deadline = lock(&self.state).deadline;
        let old_parents = lock(&self.parents).clone();
        let fresh = directory_chain(&self.binding, &self.components, deadline)?;
        if fresh.len() != old_parents.len()
            || fresh.is_empty()
            || fresh[..fresh.len() - 1] != old_parents[..old_parents.len() - 1]
            || fresh.last().unwrap().remote != old_parents.last().unwrap().remote
        {
            return Err(self.unclear("Vorfahren änderten sich nach MOVE"));
        }
        let entries = read_children(&self.binding, &self.remote_parent, deadline)?;
        let mut expected = lock(&self.state)
            .before_children
            .clone()
            .ok_or_else(|| self.unclear("Bestand vor MOVE fehlt"))?;
        let old_name = self
            .source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap();
        let new_name = self
            .destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap();
        let source = expected
            .iter_mut()
            .find(|entry| entry.entry.name == old_name)
            .ok_or_else(|| self.unclear("Quellbeleg fehlt"))?;
        source.entry.name = new_name.to_owned();
        source.entry.extension = crate::vfs::entry_extension(new_name, source.entry.is_file);
        source.raw_name = new_name.as_bytes().to_vec();
        if namespace_digest(&expected)? != namespace_digest(&entries)? {
            return Err(
                self.unclear("Readback enthält mehr als die bestätigte eigene Namensänderung")
            );
        }
        if directory_chain(&self.binding, &self.components, deadline)? != fresh {
            return Err(self.unclear("Elternversion änderte sich während des Readbacks"));
        }
        self.vfs.ensure_mutation_binding_current(&self.binding)?;
        if let Some(before) = lock(&self.state).source_children.clone() {
            let remote = map_virtual_path(
                &self.binding.session.identity.canonical_root,
                &self.destination,
            )?;
            let after = read_children(&self.binding, &remote, deadline)?;
            if namespace_digest(&before)? != namespace_digest(&after)? {
                return Err(self.unclear("Collectioninhalt änderte sich während MOVE"));
            }
        }
        // Nur der exakt belegte eigene Namenswechsel erlaubt diese eine neue
        // Collectionversion. Antwortverlust und Ablehnungen erreichen dies nie.
        *lock(&self.parents) = fresh.clone();
        Ok(CompleteDirectoryListing {
            entries,
            provider_token: fresh
                .last()
                .and_then(|parent| parent.etag.as_ref().map(|etag| etag.as_bytes().to_vec())),
            complete: true,
        })
    }

    fn check_binding(&self) -> io::Result<()> {
        self.vfs
            .ensure_mutation_binding_current(&self.binding)
            .map_err(|_| drift("WebDAV-Sitzung änderte sich während der Umbenennung"))?;
        let deadline = lock(&self.state).deadline;
        if directory_chain(&self.binding, &self.components, deadline)? != *lock(&self.parents) {
            return Err(drift(
                "WebDAV-Elternkette änderte sich während der Umbenennung",
            ));
        }
        self.vfs
            .ensure_mutation_binding_current(&self.binding)
            .map_err(|_| drift("WebDAV-Sitzung änderte sich während der Umbenennung"))
    }

    fn proof(&self, path: &Path) -> io::Result<ProofListEntry> {
        if path != self.source && path != self.destination && path != self.parent {
            return Err(drift("WebDAV-Rename-Sicht verlässt ihre gebundenen Namen"));
        }
        check_deadline(lock(&self.state).deadline)?;
        let remote = map_virtual_path(&self.binding.session.identity.canonical_root, path)?;
        stat_proof_in_session(&self.binding.session, &remote)
    }

    fn same_version(a: &ProofListEntry, b: &ProofListEntry) -> bool {
        a.provider_token.is_some()
            && a.provider_token == b.provider_token
            && a.entry.is_file == b.entry.is_file
            && a.entry.is_dir == b.entry.is_dir
            && a.entry.is_symlink == b.entry.is_symlink
            && a.entry.size == b.entry.size
    }

    fn unclear(&self, error: impl std::fmt::Display) -> io::Error {
        crate::error::with_recovery_context(
            coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear, format!("WebDAV-Umbenennung hat einen unklaren Ausgang: {error}. Beide Namen und den Elternordner prüfen; nicht automatisch wiederholen oder entfernen.")),
            &[], &[&self.source, &self.destination, &self.parent],
        )
    }

    fn mutate(&self, from: &Path, to: &Path) -> io::Result<()> {
        if from != self.source || to != self.destination || lock(&self.state).attempted {
            return Err(drift(
                "WebDAV-Rename-Sicht erlaubt nur ihre eine bestätigte Umbenennung",
            ));
        }
        self.check_binding()?;
        let expected = lock(&self.state)
            .source_proof
            .clone()
            .ok_or_else(|| drift("Der frische WebDAV-Quellbeleg fehlt"))?;
        if let Some(digest) = lock(&self.state).expected_source {
            if source_digest(&expected)? != digest {
                return Err(drift("WebDAV-Stapelquelle änderte sich vor MOVE"));
            }
        }
        let fresh = self.proof(from)?;
        if !Self::same_version(&expected, &fresh) {
            return Err(drift("WebDAV-Quelle änderte sich vor der Umbenennung"));
        }
        let etag = expected
            .provider_token
            .as_ref()
            .and_then(|v| std::str::from_utf8(v).ok())
            .ok_or_else(|| invalid("WebDAV-Quelle besitzt keine starke ETag"))?;
        validate_strong_etag(etag)?;
        self.check_binding()?;
        check_deadline(lock(&self.state).deadline)?;
        let remote_from = map_virtual_path(&self.binding.session.identity.canonical_root, from)?;
        let remote_to = map_virtual_path(&self.binding.session.identity.canonical_root, to)?;
        if expected.entry.is_dir {
            let deadline = lock(&self.state).deadline;
            let children = read_children(&self.binding, &remote_from, deadline)?;
            lock(&self.state).source_children = Some(children);
            self.check_binding()?;
            if !Self::same_version(&expected, &self.proof(from)?) {
                return Err(drift(
                    "Collection änderte sich während der vollständigen Quellenprüfung",
                ));
            }
        }
        {
            let mut state = lock(&self.state);
            if state.attempted {
                return Err(drift("WebDAV-Rename wurde bereits versucht"));
            }
            state.attempted = true;
        }
        // Antwortverlust erlaubt keinen zweiten MOVE. Der gemeinsame Koordinator
        // erhält deshalb sofort OutcomeUnclear statt einer mutierenden Fortsetzung.
        let response = move_request(
            &self.binding.session.http,
            &self.binding.session.identity.origin,
            &remote_from,
            &remote_to,
            etag,
            &lock(&self.parents).clone(),
            if expected.entry.is_dir {
                WebDavRequestClass::OrdinaryCollection
            } else {
                WebDavRequestClass::OrdinaryFile
            },
        )
        .map_err(|error| self.unclear(error))?;
        lock(&self.state).deadline = Instant::now() + PREPARATION_LIMIT;
        if matches!(
            response.status(),
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT | StatusCode::METHOD_NOT_ALLOWED
        ) {
            self.clarify_rejection(&expected)?;
            return Err(coded_error(io::ErrorKind::Unsupported, ErrorCode::Unsupported, format!("WebDAV-Server lehnte MOVE mit HTTP {} ab; Quelle unverändert, neuer Name nicht vorhanden", response.status().as_u16())));
        }
        if !matches!(
            response.status(),
            StatusCode::CREATED | StatusCode::NO_CONTENT
        ) {
            return Err(self.unclear(format!("MOVE-Antwort HTTP {}", response.status().as_u16())));
        }
        lock(&self.state).acknowledged = true;
        Ok(())
    }

    fn clarify_rejection(&self, expected: &ProofListEntry) -> io::Result<()> {
        // Ein 412 kann sowohl Zielkollision als auch Quellen-/Elterndrift bedeuten.
        // Die Ablehnung wird rein lesend eingeordnet, ohne Cleanup oder Wiederholung.
        if let Err(error) = self.check_binding() {
            if crate::error_code(&error) == ErrorCode::PlanDrift {
                lock(&self.state).definite_no_effect = true;
            }
            return Err(error);
        }
        let source = self.proof(&self.source)?;
        if !Self::same_version(expected, &source) {
            lock(&self.state).definite_no_effect = true;
            return Err(drift(
                "WebDAV-Quellbedingung wurde abgelehnt; Quelle neu laden",
            ));
        }
        let children = read_children(
            &self.binding,
            &self.remote_parent,
            lock(&self.state).deadline,
        )?;
        self.check_binding()?;
        let new_name = self
            .destination
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap();
        lock(&self.state).definite_no_effect = true;
        if children
            .iter()
            .any(|entry| crate::remote_rename::names_may_collide(&entry.entry.name, new_name))
        {
            return Err(coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::AlreadyExists,
                "Späte WebDAV-Namenskollision; vorhandenen Eintrag erhalten",
            ));
        }
        Ok(())
    }
}

impl Vfs for InteractiveRename<'_> {
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        Ok(self
            .list_complete(path)?
            .entries
            .into_iter()
            .map(|e| e.entry)
            .collect())
    }
    fn list_complete(&self, path: &Path) -> io::Result<CompleteDirectoryListing> {
        if path != self.parent {
            return Err(drift("WebDAV-Rename-Listing verlässt seinen Elternordner"));
        }
        if lock(&self.state).acknowledged {
            return self.read_after_own_move();
        }
        self.check_binding()?;
        let entries = read_children(
            &self.binding,
            &self.remote_parent,
            lock(&self.state).deadline,
        )?;
        self.check_binding()?;
        let mut state = lock(&self.state);
        let old_name = self.source.file_name().and_then(|v| v.to_str()).unwrap();
        let new_name = self
            .destination
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap();
        if state.attempted {
            let expected = state
                .source_proof
                .as_ref()
                .ok_or_else(|| self.unclear("Quellbeleg fehlt nach MOVE"))?;
            let target = entries.iter().find(|e| e.entry.name == new_name);
            if entries.iter().any(|e| e.entry.name == old_name)
                || target.is_none_or(|e| !Self::same_version(expected, e))
            {
                return Err(self
                    .unclear("Readback bestätigt Quellabwesenheit und exakte Zielversion nicht"));
            }
        } else {
            WebDavVfs::validate_batch_rename_listing(&entries)?;
            state.source_proof = entries.iter().find(|e| e.entry.name == old_name).cloned();
            state.before_children = Some(entries.clone());
        }
        Ok(CompleteDirectoryListing {
            entries,
            provider_token: lock(&self.parents)
                .last()
                .and_then(|p| p.etag.as_ref().map(|etag| etag.as_bytes().to_vec())),
            complete: true,
        })
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        self.check_binding()?;
        Ok(self.proof(path)?.entry)
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.mutate(from, to)
    }
    fn rename_plain_once(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.mutate(from, to)
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn open_read(&self, _: &Path) -> io::Result<Box<dyn Read + Send>> {
        Err(read_only())
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
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
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(read_only())
    }
}

fn source_digest(proof: &ProofListEntry) -> io::Result<[u8; 32]> {
    let token = proof
        .provider_token
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .ok_or_else(|| invalid("WebDAV-Namespacebeleg braucht eine starke ETag"))?;
    validate_strong_etag(token)?;
    if !proof.safely_decoded || proof.raw_name != proof.entry.name.as_bytes() {
        return Err(invalid("WebDAV-Namensbeleg ist nicht eindeutig"));
    }
    Ok(*blake3::hash(
        &serde_json::to_vec(&(&proof.raw_name, &proof.entry, &proof.provider_token))
            .map_err(io::Error::other)?,
    )
    .as_bytes())
}
fn namespace_digest(entries: &[ProofListEntry]) -> io::Result<Vec<(String, [u8; 32])>> {
    let mut result = BTreeMap::new();
    for entry in entries {
        if result
            .insert(entry.entry.name.clone(), source_digest(entry)?)
            .is_some()
        {
            return Err(invalid("WebDAV-Readback enthält doppelte Namen"));
        }
    }
    Ok(result.into_iter().collect())
}
fn parent_proof(
    parents: &[WebDavResourceIdentity],
) -> io::Result<crate::remote_batch_rename::ParentProof> {
    Ok(crate::remote_batch_rename::ParentProof {
        root: parents
            .last()
            .ok_or_else(|| invalid("WebDAV-Eltern fehlen"))?
            .remote
            .clone(),
        entries: parents
            .iter()
            .map(|parent| {
                (
                    parent.remote.clone(),
                    *blake3::hash(parent.etag.as_deref().unwrap_or("").as_bytes()).as_bytes(),
                    *blake3::hash(parent.remote.as_bytes()).as_bytes(),
                )
            })
            .collect(),
    })
}
impl WebDavVfs {
    pub(crate) fn validate_batch_rename_listing(entries: &[ProofListEntry]) -> io::Result<()> {
        namespace_digest(entries).map(|_| ()).map_err(|error| coded_error(io::ErrorKind::Unsupported, ErrorCode::Unsupported,
            format!("WebDAV-Umbenennung benötigt für den Nachbeleg einen vollständig und stark belegten unmittelbaren Ordnerbestand; diese Auswahl kann nicht beginnen: {error}")))
    }
    pub(crate) fn batch_rename_source(
        &self,
        path: &Path,
        cancel: &crate::Cancel,
    ) -> io::Result<(Entry, [u8; 32])> {
        if cancel.is_cancelled() {
            return Err(coded_error(
                io::ErrorKind::Interrupted,
                ErrorCode::Interrupted,
                "WebDAV-Stapel abgebrochen",
            ));
        }
        interactive_namespace::validate_path(path)?;
        let proof = self.stat_proof(path)?;
        let digest = source_digest(&proof)?;
        Ok((proof.entry, digest))
    }
    pub(crate) fn batch_rename_parents(
        &self,
        root: &Path,
        cancel: &crate::Cancel,
    ) -> io::Result<crate::remote_batch_rename::ParentProof> {
        if cancel.is_cancelled() {
            return Err(coded_error(
                io::ErrorKind::Interrupted,
                ErrorCode::Interrupted,
                "WebDAV-Stapel abgebrochen",
            ));
        }
        interactive_namespace::validate_path(root)?;
        let (binding, _) = self.bind_mutation(root, |_| true, "WebDAV-Stapelbindung fehlt")?;
        let parents = directory_chain(
            &binding,
            &virtual_components(root)?,
            Instant::now() + PREPARATION_LIMIT,
        )?;
        self.ensure_mutation_binding_current(&binding)?;
        parent_proof(&parents)
    }
    pub(crate) fn execute_batch_rename(
        &self,
        source: &Path,
        new_name: &str,
        identity: &crate::RootIdentity,
        parents: &crate::remote_batch_rename::ParentProof,
        source_digest: [u8; 32],
    ) -> io::Result<(RenameReport, crate::remote_batch_rename::RemoteBatchRefresh)> {
        self.execute_bound_rename(source, new_name, Some((identity, parents, source_digest)))
    }
}
