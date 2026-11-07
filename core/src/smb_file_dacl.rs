//! DACL-Einzelwirkung an bestehenden Dateien, getrennt vom Datei-Tempvertrag.
use super::windows_dacl as wire;
use super::*;
use crate::file_dacl::{FileDaclPlan, FileDaclReport, FileDaclTransaction};
use crate::file_dacl_registry::{Intent as RemoteFileDaclIntent, Phase};
use crate::windows_dacl::{WindowsDaclTargetIssue as Issue, WindowsDaclWriteSupport as Support};
use crate::windows_security::WindowsDacl;

// READ_DATA, READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC, SYNCHRONIZE.
const FILE_INPLACE_ACCESS: u32 = 0x0016_0081;

#[cfg(test)]
thread_local! {
    static TEST_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static TEST_BEFORE_FILE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static TEST_RECOVERY_HELD: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

struct FileTransaction {
    scope: OrdinarySmb,
    path: PathBuf,
    remote: String,
    tree: Arc<Tree>,
    connection: Connection,
    held: Vec<FileId>,
    base: FileId,
    inode: u64,
    plan: FileDaclPlan,
    intent: RemoteFileDaclIntent,
    report: FileDaclReport,
    attempted: bool,
    applied: bool,
}

impl SmbVfs {
    pub(in crate::smb) fn read_file_dacl_recovery(
        &self,
        path: &Path,
        expected_root: &crate::RootIdentity,
    ) -> io::Result<crate::file_dacl::FileDaclRecoveryObservation> {
        let crate::RootIdentity::Smb(expected) = expected_root else {
            return Err(invalid("SMB-DACL-Readback benötigt einen SMB-Root"));
        };
        expected.validate()?;
        let root = Path::new(&expected.virtual_root);
        let normalized = normalize_virtual_path(path)?;
        let path = Path::new(&normalized);
        if !path.starts_with(root) || path == root {
            return Err(invalid(
                "DACL-Recovery ist nur für einen bestehenden Unterdatei zulässig",
            ));
        }
        if self.root_identity(root)? != *expected_root {
            return Err(crate::real_mode::plan_drift(
                "SMB-DACL-Recoveryroot änderte sich",
            ));
        }
        let remote = self.remote_path(path)?;
        let selected = self.remote_path(root)?;
        let (tree, mut connection) = {
            let state = lock(&self.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let mut held = Vec::new();
        let result = (|| {
            let observation = self.runtime.block_on(async {
                wire::parents(&mut connection, &tree, &remote, &selected, &mut held).await?;
                // Recovery ist ausschließlich lesend; WRITE_DAC und DELETE fehlen.
                let id = wire::open_file_inplace(
                    &mut connection,
                    &tree,
                    &remote,
                    FileAccessMask::READ_CONTROL
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE,
                )
                .await?;
                held.push(id);
                let entry = query_handle_entry(&mut connection, &tree, id, String::new()).await?;
                let observed =
                    wire::query(&mut connection, &tree, id, expected.server.server_guid).await?;
                let context = wire::context(&observed)?;
                let Some(crate::metadata::Acl::WindowsDacl(dacl)) = observed.acl else {
                    return Err(invalid("SMB-DACL-Recovery besitzt keine vollständige DACL"));
                };
                let fresh = query_handle_entry(&mut connection, &tree, id, String::new()).await?;
                if entry.file_id == 0
                    || entry.file_id != fresh.file_id
                    || fresh.is_dir
                    || fresh.is_symlink
                {
                    return Err(crate::real_mode::plan_drift(
                        "SMB-DACL-Recoverybasis änderte sich",
                    ));
                }
                Ok(crate::file_dacl::FileDaclRecoveryObservation {
                    file_id: entry.file_id,
                    object_namespace: crate::file_dacl_registry::object_namespace(expected_root)?,
                    context,
                    dacl,
                })
            })?;
            #[cfg(test)]
            TEST_RECOVERY_HELD.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            // Diese Prüfung geschieht vor dem Schließen der Eltern und Basis.
            // Ein fremder Roottausch kann deshalb nicht zwischen Readback und
            // Identitätsvergleich unbemerkt zurückgetauscht werden.
            if self.root_identity(root)? != *expected_root {
                return Err(crate::real_mode::plan_drift(
                    "SMB-DACL-Recoveryroot änderte sich beim Readback",
                ));
            }
            Ok(observation)
        })();
        let closed = self
            .runtime
            .block_on(wire::close_all(&mut connection, &tree, &mut held));
        match (result, closed) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(observation), Ok(())) => Ok(observation),
        }
    }
}

impl OrdinarySmb {
    pub(super) fn file_dacl_support(&self) -> io::Result<Support> {
        self.check(&self.root, false)?;
        if !self.sync_metadata || !self.stream_target {
            return Ok(Support::Unavailable(Issue::UnsupportedBackend));
        }
        Ok(lock(&self.file_dacl_support)
            .clone()
            .unwrap_or(Support::Unavailable(Issue::NotPrepared)))
    }

    pub(super) fn begin_file_dacl(
        &self,
        path: &Path,
        plan: &FileDaclPlan,
    ) -> io::Result<Box<dyn FileDaclTransaction>> {
        self.check(path, true)?;
        plan.validate()?;
        if plan.object_namespace != crate::file_dacl_registry::object_namespace(&self.identity)? {
            return Err(crate::real_mode::plan_drift(
                "Datei-DACL-Objektnamensraum änderte sich",
            ));
        }
        if !matches!(self.file_dacl_support()?, Support::Ready(context) if context.namespace == plan.context.namespace)
        {
            return Err(crate::real_mode::plan_drift(
                "SMB-Datei-DACL-Kontext ist nicht vorbereitet",
            ));
        }
        let owner = self
            .owner
            .as_ref()
            .ok_or_else(|| crate::windows_dacl::write_omission(Issue::NoStagingOwner))?;
        let remote = self.provider.remote_path(path)?;
        let selected = self.provider.remote_path(&self.root)?;
        let (tree, mut connection, account) = {
            let state = lock(&self.provider.state);
            (
                state.session.tree.clone(),
                state.session.connection.clone(),
                state.session.identity.account_digest,
            )
        };
        let mut held = Vec::new();
        // Eine externe Fixtureaktion darf selbst SMB aufrufen. Außerhalb des
        // block_on bleibt dafür kein Tokio-Kontext verschachtelt.
        #[cfg(test)]
        TEST_BEFORE_FILE_OPEN.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let setup = self.provider.runtime.block_on(async {
            wire::parents(&mut connection, &tree, &remote, &selected, &mut held).await?;
            #[cfg(test)]
            if TEST_FAULT.with(|f| f.get() == Some("before_open")) {
                TEST_FAULT.with(|f| f.set(None));
                return Err(crate::windows_dacl::write_omission(
                    Issue::RequiredAccessDenied,
                ));
            }
            let id = wire::open_file_inplace(&mut connection, &tree, &remote, FILE_INPLACE_ACCESS)
                .await?;
            held.push(id);
            let entry = query_handle_entry(&mut connection, &tree, id, String::new()).await?;
            if entry.file_id == 0
                || entry.file_id != plan.file_id
                || !plan.target_proof.matches_identity(&entry.entry())
            {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Datei-DACL-Basis wurde ausgetauscht",
                ));
            }
            let observed = wire::query(&mut connection, &tree, id, plan.context.namespace).await?;
            if wire::context(&observed)? != plan.context
                || observed.acl.as_ref()
                    != Some(&crate::metadata::Acl::WindowsDacl(plan.before.clone()))
            {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Datei-DACL oder Owner-/Group-Kontext änderte sich",
                ));
            }
            Ok((id, entry.file_id))
        });
        let (base, inode) = match setup {
            Ok(value) => value,
            Err(error) => {
                let _ = self.provider.runtime.block_on(wire::close_all(
                    &mut connection,
                    &tree,
                    &mut held,
                ));
                return Err(error);
            }
        };
        let result = (|| {
            self.check(path, true)?;
            RemoteFileDaclIntent::create(&owner.staging, path, plan, &self.identity, account)
        })();
        let intent = match result {
            Ok(intent) => intent,
            Err(error) => {
                let _ = self.provider.runtime.block_on(wire::close_all(
                    &mut connection,
                    &tree,
                    &mut held,
                ));
                return Err(error);
            }
        };
        Ok(Box::new(FileTransaction {
            scope: self.clone(),
            path: path.to_path_buf(),
            remote,
            tree,
            connection,
            held,
            base,
            inode,
            plan: plan.clone(),
            intent,
            report: FileDaclReport::default(),
            attempted: false,
            applied: false,
        }))
    }
}

impl FileTransaction {
    fn verify(&mut self, acl: &WindowsDacl) -> io::Result<()> {
        let expected = &self.plan.context;
        self.scope.provider.runtime.block_on(async {
            let entry =
                query_handle_entry(&mut self.connection, &self.tree, self.base, String::new())
                    .await?;
            if entry.file_id != self.inode || entry.is_symlink || entry.is_dir {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Datei-DACL-ID änderte sich",
                ));
            }
            let (content, streams) = super::super::named_streams::file_dacl_content_proof(
                &mut self.connection,
                &self.tree,
                self.base,
                &self.remote,
                self.plan.content.size,
                Some(&self.scope.cancel),
            )
            .await?;
            if content != self.plan.content || streams != self.plan.streams {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Dateiinhalt oder ADS änderten sich bei DACL-Wirkung",
                ));
            }
            wire::exact(
                &wire::query(
                    &mut self.connection,
                    &self.tree,
                    self.base,
                    expected.namespace,
                )
                .await?,
                acl,
                expected,
            )
        })
    }
    fn unclear(&mut self, error: io::Error) -> io::Error {
        if self.report.completed.is_empty() && self.report.uncertain.is_empty() {
            if let Ok(outcome) = self.plan.outcome(&self.path.to_string_lossy()) {
                self.report.uncertain.push(outcome);
            }
        }
        let error = crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::ErrorCode::OutcomeUnclear,
                format!("SMB-Datei-DACL-Wirkung oder Abschluss unklar: {error}"),
            ),
            &[&error],
            &[&self.path, self.intent.path()],
        );
        crate::error::with_file_dacl_report(error, &self.report())
    }
    fn close(&mut self) -> io::Result<()> {
        self.scope.provider.runtime.block_on(wire::close_all(
            &mut self.connection,
            &self.tree,
            &mut self.held,
        ))
    }
}
impl FileDaclTransaction for FileTransaction {
    fn apply(&mut self) -> io::Result<FileDaclReport> {
        if self.attempted {
            return Err(self.unclear(invalid("Datei-DACL-SET wurde bereits versucht")));
        }
        self.scope.check(&self.path, true)?;
        self.verify(&self.plan.before.clone()).map_err(|error| {
            if crate::windows_dacl::known_write_omission(&error) == Some(Issue::Normalized) {
                crate::real_mode::plan_drift("SMB-Datei-DACL änderte sich vor SET")
            } else {
                error
            }
        })?;
        let bytes = self
            .plan
            .after
            .encode_for_file_inplace_write(self.plan.context.namespace, &self.plan.context.owner)?;
        // Erst nach dauerhaft bestätigtem Applying darf ein SET das Netzwerk erreichen.
        self.intent.phase(Phase::Applying)?;
        self.attempted = true;
        let result = (|| {
            self.scope.provider.runtime.block_on(wire::set(
                &mut self.connection,
                &self.tree,
                self.base,
                bytes,
            ))?;
            #[cfg(test)]
            if TEST_FAULT.with(|f| f.get() == Some("after_set")) {
                TEST_FAULT.with(|f| f.set(None));
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Synthetischer Antwortverlust nach File-SET",
                ));
            }
            self.verify(&self.plan.after.clone())?;
            self.scope.check(&self.path, true)?;
            // Nach dem einzigen SET benötigt der frische Readback keine neue
            // Änderungsberechtigung. Der ursprüngliche SET-Handle bleibt gehalten.
            let fresh = self
                .scope
                .provider
                .runtime
                .block_on(wire::open_file_inplace(
                    &mut self.connection,
                    &self.tree,
                    &self.remote,
                    FILE_INPLACE_ACCESS & !FileAccessMask::WRITE_DAC,
                ))?;
            self.held.push(fresh);
            let old = self.base;
            self.base = fresh;
            let checked = self.verify(&self.plan.after.clone());
            self.base = old;
            checked?;
            self.intent.phase(Phase::Applied)?;
            self.applied = true;
            self.report
                .completed
                .push(self.plan.outcome(&self.path.to_string_lossy())?);
            Ok(())
        })();
        match result {
            Ok(()) => Ok(self.report.clone()),
            Err(error) => Err(self.unclear(error)),
        }
    }
    fn report(&self) -> FileDaclReport {
        let mut report = self.intent.report();
        report.merge(&self.report);
        report
    }
    fn recovery_paths(&self) -> Vec<String> {
        vec![
            self.path.to_string_lossy().into_owned(),
            self.intent.path().to_string_lossy().into_owned(),
        ]
    }
    fn finish(mut self: Box<Self>) -> io::Result<FileDaclReport> {
        if !self.applied {
            return Err(self.unclear(invalid("Datei-DACL-Wirkung ist nicht bestätigt")));
        }
        if let Err(error) = self.close().and_then(|()| self.intent.finish(true)) {
            return Err(self.unclear(error));
        }
        Ok(self.report.clone())
    }
}
impl Drop for FileTransaction {
    fn drop(&mut self) {
        let _ = self.close();
        if !self.attempted {
            let _ = self.intent.finish(false);
        }
    }
}

impl OrdinarySmb {
    pub(super) fn prepare_file_dacl(&self) -> io::Result<()> {
        self.check(&self.root, false)?;
        if !self.sync_metadata || !self.stream_target {
            return Ok(());
        }
        let mut cache = lock(&self.file_dacl_support);
        if cache.is_some() {
            return Ok(());
        }
        if self.owner.is_none() {
            *cache = Some(Support::Unavailable(Issue::NoStagingOwner));
            return Ok(());
        }
        // Keine Temp- oder Directory-Schreibprobe: jedes bestehende Dateiobjekt
        // wird vor seinem eigenen Intent mit minimalem Zugriff frisch geöffnet.
        let remote = self.provider.remote_path(&self.root)?;
        let (tree, mut connection) = {
            let state = lock(&self.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let result = self.provider.runtime.block_on(async {
            let id = wire::open(
                &mut connection,
                &tree,
                &remote,
                FileAccessMask::READ_CONTROL
                    | FileAccessMask::FILE_READ_ATTRIBUTES
                    | FileAccessMask::SYNCHRONIZE,
                true,
                false,
            )
            .await?;
            let result = wire::query(
                &mut connection,
                &tree,
                id,
                match &self.identity {
                    crate::RootIdentity::Smb(root) => root.server.server_guid,
                    _ => return Err(invalid("Kein SMB-Root")),
                },
            )
            .await
            .and_then(|value| wire::context(&value));
            let close = tree
                .close_handle(&mut connection, id)
                .await
                .map_err(smb_error);
            match (result, close) {
                (Ok(context), Ok(())) => Ok(context),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        });
        match result {
            Ok(context) => *cache = Some(Support::Ready(context)),
            Err(error) => {
                if let Some(issue) = crate::windows_dacl::known_write_omission(&error) {
                    *cache = Some(Support::Unavailable(issue));
                } else {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "smb_file_dacl_tests.rs"]
mod tests;
