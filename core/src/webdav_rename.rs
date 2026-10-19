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
            parents,
            components,
            parent,
            remote_parent,
            source: source.to_path_buf(),
            destination: source.parent().unwrap().join(new_name),
            state: Mutex::new(RenameState {
                deadline,
                source_proof: None,
                attempted: false,
                definite_no_effect: false,
            }),
        };
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
            result => result,
        }
    }
}

struct RenameState {
    deadline: Instant,
    source_proof: Option<ProofListEntry>,
    attempted: bool,
    definite_no_effect: bool,
}

struct InteractiveRename<'a> {
    vfs: &'a WebDavVfs,
    binding: BoundWebDavMutation,
    parents: Vec<WebDavResourceIdentity>,
    components: Vec<String>,
    parent: PathBuf,
    remote_parent: String,
    source: PathBuf,
    destination: PathBuf,
    state: Mutex<RenameState>,
}

impl InteractiveRename<'_> {
    fn check_binding(&self) -> io::Result<()> {
        self.vfs
            .ensure_mutation_binding_current(&self.binding)
            .map_err(|_| drift("WebDAV-Sitzung änderte sich während der Umbenennung"))?;
        let deadline = lock(&self.state).deadline;
        if directory_chain(&self.binding, &self.components, deadline)? != self.parents {
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
            &self.parents,
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
            state.source_proof = entries.iter().find(|e| e.entry.name == old_name).cloned();
        }
        Ok(CompleteDirectoryListing {
            entries,
            provider_token: self.parents.last().map(|p| p.etag.as_bytes().to_vec()),
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
