//! V7 hält den normalen minimalen CREATE bis zum Abschluss der optionalen Rechtewirkung.
use super::windows_dacl as wire;
use super::*;
use crate::created_directory_dacl::*;
use crate::created_directory_dacl_registry::{Intent, Phase};
use crate::windows_dacl::{WindowsDaclTargetIssue as Issue, WindowsDaclWriteSupport as Support};
#[cfg(test)]
thread_local! { static TEST_FAULT:std::cell::Cell<Option<&'static str>>=const {std::cell::Cell::new(None)}; static TEST_BEFORE_SECURITY:std::cell::RefCell<Option<Box<dyn FnOnce()>>>=const {std::cell::RefCell::new(None)}; }
#[cfg(test)]
thread_local! {
    static TEST_BEFORE_CHILDREN_SETS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    static TEST_BEFORE_CHILDREN_FINISHED: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}
const DIRECTORY_ACCESS: u32 = 0x0016_00a1;
impl SmbVfs {
    pub(in crate::smb) fn observe_created_foreign_root_scope(
        &self,
        expected: &crate::RootIdentity,
    ) -> io::Result<Option<CreatedDirectoryRootScope>> {
        let crate::RootIdentity::Smb(root) = expected else {
            return Ok(None);
        };
        // Nur die unveränderte gespeicherte Server-/Kontobeziehung erteilt den
        // lesenden Fremdrootzugriff innerhalb desselben Profilroots.
        if root.server != self.identity() {
            return Ok(None);
        }
        let path = Path::new(&root.virtual_root);
        let current = match self.root_identity(path) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        if !crate::local_staging::same_recovery_root(expected, &current) {
            return Ok(None);
        }
        self.observe_created_root_scope(path, &current)
    }

    pub(in crate::smb) fn observe_created_root_scope(
        &self,
        path: &Path,
        expected: &crate::RootIdentity,
    ) -> io::Result<Option<CreatedDirectoryRootScope>> {
        if self.root_identity(path)? != *expected {
            return Err(crate::real_mode::plan_drift(
                "V7-Scope besitzt eine andere Root-ID",
            ));
        }
        let remote = self.remote_path(path)?;
        let probe = if remote.is_empty() {
            ".v7-scope-read-only".into()
        } else {
            format!("{remote}\\.v7-scope-read-only")
        };
        let (tree, mut connection) = {
            let state = lock(&self.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let mut held = Vec::new();
        let result = self.runtime.block_on(async {
            wire::parents(&mut connection, &tree, &probe, &remote, &mut held).await?;
            root_scope_from_handles(&mut connection, &tree, &held, expected).await
        });
        let closed = self
            .runtime
            .block_on(wire::close_all(&mut connection, &tree, &mut held));
        let observed = result.and_then(|value| closed.map(|()| value))?;
        if self.root_identity(path)? != *expected {
            return Err(crate::real_mode::plan_drift(
                "V7-Scope-Root änderte sich während der Beobachtung",
            ));
        }
        Ok(Some(observed))
    }
}
async fn root_scope_from_handles(
    connection: &mut Connection,
    tree: &Tree,
    held: &[FileId],
    root: &crate::RootIdentity,
) -> io::Result<CreatedDirectoryRootScope> {
    let crate::RootIdentity::Smb(expected) = root else {
        return Err(invalid("V7-Scope benötigt SMB"));
    };
    let mut ancestors = Vec::new();
    for handle in held {
        let observed = query_handle_entry(connection, tree, *handle, String::new()).await?;
        if observed.file_id == 0 || !observed.is_dir || observed.is_symlink {
            return Err(crate::real_mode::plan_drift(
                "V7-Vorfahre ist kein gebundener echter Ordner",
            ));
        }
        if observed.file_id == expected.root_file_id {
            let scope = CreatedDirectoryRootScope {
                object_namespace: crate::created_directory_dacl_registry::object_namespace(root)?,
                root_file_id: observed.file_id,
                ancestor_file_ids: ancestors,
            };
            scope.validate(root)?;
            return Ok(scope);
        }
        ancestors.push(observed.file_id);
    }
    Err(crate::real_mode::plan_drift(
        "Gehaltene Vorfahren enthalten nicht die bestätigte Auswahlroot-ID",
    ))
}
struct Receipt {
    scope: OrdinarySmb,
    path: PathBuf,
    remote: String,
    tree: Arc<Tree>,
    connection: Connection,
    original: Option<FileId>,
    inode: u64,
    origin: CreatedDirectoryOrigin,
    omission: Issue,
}
impl Receipt {
    fn release(&mut self) -> io::Result<()> {
        if let Some(id) = self.original.take() {
            self.scope
                .provider
                .runtime
                .block_on(self.tree.close_handle(&mut self.connection, id))
                .map_err(smb_error)?;
        }
        Ok(())
    }
    fn release_optional(&mut self, held: &mut Vec<FileId>) -> io::Result<()> {
        let closed = self.scope.provider.runtime.block_on(wire::close_all(
            &mut self.connection,
            &self.tree,
            held,
        ));
        let original = self.release();
        closed.and(original).map_err(|error|crate::error::with_recovery_context(crate::coded_error(error.kind(),crate::ErrorCode::OutcomeUnclear,format!("Zusätzliche Ordnerhandles konnten nicht bestätigt freigegeben werden: {error}")),&[&error],&[&self.path]))
    }
    fn original_id(&mut self) -> io::Result<()> {
        let Some(id) = self.original else {
            return Err(crate::windows_dacl::write_omission(Issue::MissingOwner));
        };
        let observed = self
            .scope
            .provider
            .runtime
            .block_on(query_handle_entry(
                &mut self.connection,
                &self.tree,
                id,
                String::new(),
            ))
            .map_err(|error| {
                if crate::error_code(&error) == crate::ErrorCode::PlanDrift {
                    error
                } else {
                    crate::windows_dacl::write_omission(Issue::OptionalObservation)
                }
            })?;
        if self.inode == 0 {
            return Err(crate::windows_dacl::write_omission(Issue::MissingOwner));
        }
        if observed.file_id != self.inode || !observed.is_dir || observed.is_symlink {
            return Err(crate::real_mode::plan_drift(
                "Original erzeugter Ordner wurde ausgetauscht",
            ));
        }
        Ok(())
    }
}
impl Drop for Receipt {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
impl OrdinarySmb {
    pub(super) fn create_dacl_receipt(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn CreatedDirectoryDaclReceipt>> {
        self.create_dacl_receipt_for_origin(path, CreatedDirectoryOrigin::ExclusiveMinimalCreate)
    }
    pub(super) fn create_dacl_receipt_for_origin(
        &self,
        path: &Path,
        origin: CreatedDirectoryOrigin,
    ) -> io::Result<Box<dyn CreatedDirectoryDaclReceipt>> {
        self.check(path, true)?;
        self.start_intent(path)?;
        let remote = self.provider.remote_path(path)?;
        let (tree, connection) = {
            let state = lock(&self.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let creation = self.provider.runtime.block_on(async {
            // Unveränderte Pflichtrechte und normale Flags des bisherigen mkdir.
            let request = CreateRequest {
                requested_oplock_level: OplockLevel::None,
                impersonation_level: ImpersonationLevel::Impersonation,
                desired_access: FileAccessMask::new(0x0010_0080),
                file_attributes: FILE_ATTRIBUTE_DIRECTORY,
                share_access: ShareAccess(
                    ShareAccess::FILE_SHARE_READ
                        | ShareAccess::FILE_SHARE_WRITE
                        | ShareAccess::FILE_SHARE_DELETE,
                ),
                create_disposition: CreateDisposition::FileCreate,
                create_options: FILE_DIRECTORY_FILE,
                name: remote.clone(),
                create_contexts: Vec::new(),
            };
            let frame = connection
                .execute(Command::Create, &request, Some(tree.tree_id))
                .await
                .map_err(|error|{let status=error.status();let error=smb_error(error);if status.is_none(){crate::error::with_recovery_context(crate::coded_error(error.kind(),crate::ErrorCode::OutcomeUnclear,format!("SMB-Ordneranlage besitzt einen unklaren Ausgang: {error}")),&[&error],&[path])}else{error}})?;
            require_status(&frame, Command::Create)?;
            #[cfg(test)]
            if TEST_FAULT.with(|fault| fault.get() == Some("after_create")) {
                TEST_FAULT.with(|fault| fault.set(None));
                return Err(crate::coded_error(io::ErrorKind::ConnectionAborted,crate::ErrorCode::OutcomeUnclear,"Synthetischer Antwortverlust nach tatsächlichem exklusiven CREATE"));
            }
            Ok::<_, io::Error>(
                CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(|error|crate::coded_error(io::ErrorKind::InvalidData,crate::ErrorCode::OutcomeUnclear,format!("Bestätigter CREATE besitzt keinen lesbaren Originalhandle: {error}")))?
                    .file_id,
            )
        });
        let original = match creation {
            Ok(id) => id,
            Err(error) => {
                if crate::error_code(&error) != crate::ErrorCode::OutcomeUnclear {
                    if let Some(file) = &self.file {
                        file.settled.store(true, Ordering::SeqCst);
                    }
                }
                return Err(error);
            }
        };
        let mut receipt = Receipt {
            scope: self.clone(),
            path: path.into(),
            remote,
            tree,
            connection,
            original: Some(original),
            inode: 0,
            origin,
            omission: Issue::OptionalObservation,
        };
        // Scheitert nur die zusätzliche Originalbeobachtung, bleibt die normale
        // Anlage erhalten. Der Receipt erhält dadurch keine spätere DACL-Freigabe.
        match self.provider.runtime.block_on(query_handle_entry(
            &mut receipt.connection,
            &receipt.tree,
            original,
            String::new(),
        )) {
            Ok(entry) if entry.is_dir && !entry.is_symlink && entry.file_id != 0 => {
                receipt.inode = entry.file_id
            }
            Ok(_) => {
                return Err(crate::real_mode::plan_drift(
                    "CREATE meldet keinen echten neuen Ordner",
                ))
            }
            Err(_) => {
                receipt.release()?;
                let entry = self.provider.stat(path)?;
                if !entry.is_dir || entry.is_symlink {
                    return Err(crate::real_mode::plan_drift(
                        "Neuer Ordner ist beim normalen Readback fremd",
                    ));
                }
            }
        }
        if let Some(file) = &self.file {
            file.settled.store(true, Ordering::SeqCst);
        }
        Ok(Box::new(receipt))
    }
}
impl CreatedDirectoryDaclReceipt for Receipt {
    fn effect_key(&self) -> Option<CreatedDirectoryEffectKey> {
        if self.inode == 0 {
            return None;
        }
        Some(CreatedDirectoryEffectKey {
            origin: self.origin,
            directory: self.path.to_string_lossy().into_owned(),
            file_id: self.inode,
            object_namespace: crate::created_directory_dacl_registry::object_namespace(
                &self.scope.identity,
            )
            .ok()?,
        })
    }
    fn omission(&self) -> Issue {
        self.omission
    }
    fn abandon(&mut self) -> io::Result<()> {
        self.release()
    }
    fn set_mtime(&mut self, mtime: Option<i64>) -> io::Result<bool> {
        if self.origin == CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren {
            return Err(invalid("V8 erlaubt keine frühe Parentzeit"));
        }
        let Some(mtime) = mtime else {
            return Ok(self.original.is_some());
        };
        if self.original.is_none() {
            self.scope.set_mtime(&self.path, mtime)?;
            return Ok(false);
        }
        if let Err(error) = self.scope.check(&self.path, true) {
            if matches!(
                crate::error_code(&error),
                crate::ErrorCode::PlanDrift | crate::ErrorCode::Interrupted
            ) {
                return Err(error);
            }
            self.release()?;
            self.scope.set_mtime(&self.path, mtime)?;
            return Ok(false);
        }
        match self
            .scope
            .provider
            .set_mtime_phased(&self.path, mtime, Some(self.inode))
        {
            Ok(()) => Ok(true),
            Err(failure) if failure.allows_receipt_release() => {
                self.omission = if failure.resource_open {
                    Issue::OptionalResources
                } else {
                    Issue::OptionalObservation
                };
                self.release()?;
                self.scope.set_mtime(&self.path, mtime)?;
                Ok(false)
            }
            Err(failure) if failure.allows_readback_retry() => {
                self.release()?;
                self.scope
                    .provider
                    .verify_mtime_readback(&self.path, mtime, Some(self.inode))?;
                Ok(false)
            }
            Err(failure) => {
                let before_set = failure.phase == super::super::mtime::MtimePhase::BeforeSet;
                let error = failure.into_io();
                if crate::error_code(&error) == crate::ErrorCode::PlanDrift
                    || (before_set && crate::error_code(&error) == crate::ErrorCode::Interrupted)
                {
                    return Err(error);
                }
                Err(crate::error::with_recovery_context(
                    crate::coded_error(
                        error.kind(),
                        crate::ErrorCode::OutcomeUnclear,
                        format!("SMB-Zeitübernahme besitzt einen unklaren Abschluss: {error}"),
                    ),
                    &[&error],
                    &[&self.path],
                ))
            }
        }
    }
    fn begin(
        self: Box<Self>,
        selection: &CreatedDirectoryDaclSelection,
    ) -> io::Result<Box<dyn CreatedDirectoryDaclTransaction>> {
        if self.origin != CreatedDirectoryOrigin::ExclusiveMinimalCreate {
            return Err(invalid("V7-Receipt besitzt eine fremde Herkunft"));
        }
        self.begin_selected(selection)
    }
    fn begin_before_children(
        self: Box<Self>,
        selection: &CreatedDirectoryBeforeChildrenSelection,
    ) -> io::Result<Box<dyn CreatedDirectoryDaclTransaction>> {
        selection.validate()?;
        if self.origin != CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren {
            return Err(invalid("V8-Receipt besitzt eine fremde Herkunft"));
        }
        self.begin_selected(&CreatedDirectoryDaclSelection {
            after: selection.after.clone(),
            probe_context: selection.probe_context.clone(),
        })
    }
}
impl Receipt {
    fn begin_selected(
        mut self: Box<Self>,
        selection: &CreatedDirectoryDaclSelection,
    ) -> io::Result<Box<dyn CreatedDirectoryDaclTransaction>> {
        selection.validate()?;
        self.original_id()?;
        if self.scope.directory_dacl_support()? != Support::Ready(selection.probe_context.clone()) {
            return Err(crate::windows_dacl::write_omission(Issue::NotPrepared));
        }
        self.scope.check(&self.path, true)?;
        #[cfg(test)]
        TEST_BEFORE_SECURITY.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let selected = self.scope.provider.remote_path(&self.scope.root)?;
        let mut held = Vec::new();
        let setup = self.scope.provider.runtime.block_on(async {
            wire::parents(
                &mut self.connection,
                &self.tree,
                &self.remote,
                &selected,
                &mut held,
            )
            .await?;
            let id = wire::open(
                &mut self.connection,
                &self.tree,
                &self.remote,
                DIRECTORY_ACCESS,
                true,
                false,
            )
            .await?;
            held.push(id);
            let entry =
                query_handle_entry(&mut self.connection, &self.tree, id, String::new()).await?;
            if entry.file_id != self.inode || !entry.is_dir || entry.is_symlink {
                return Err(crate::real_mode::plan_drift(
                    "Optionaler Rechtehandle bezeichnet nicht den original erzeugten Ordner",
                ));
            }
            let observed = wire::query(
                &mut self.connection,
                &self.tree,
                id,
                selection.probe_context.namespace,
            )
            .await?;
            let context = wire::context(&observed)?;
            if context != selection.probe_context {
                return Err(crate::windows_dacl::write_omission(
                    Issue::DirectoryContextMismatch,
                ));
            }
            let Some(crate::metadata::Acl::WindowsDacl(before)) = observed.acl else {
                return Err(crate::windows_dacl::write_omission(
                    Issue::SecurityUnsupported,
                ));
            };
            Ok((id, before, context))
        });
        let (base, before, context) = match setup {
            Ok(value) => value,
            Err(error) => {
                self.release_optional(&mut held)?;
                return Err(
                    if crate::error_code(&error) == crate::ErrorCode::PlanDrift
                        || crate::windows_dacl::known_write_omission(&error).is_some()
                    {
                        error
                    } else {
                        crate::windows_dacl::write_omission(Issue::SecurityUnsupported)
                    },
                );
            }
        };
        if before == selection.after {
            self.release_optional(&mut held)?;
            return Err(crate::windows_dacl::write_omission(Issue::AlreadyDesired));
        }
        let plan = CreatedDirectoryDaclPlan {
            origin: self.origin,
            file_id: self.inode,
            object_namespace: crate::created_directory_dacl_registry::object_namespace(
                &self.scope.identity,
            )?,
            before,
            after: selection.after.clone(),
            context,
        };
        let intent = (|| {
            self.original_id()?;
            self.scope.check(&self.path, true)?;
            let owner = self
                .scope
                .owner
                .as_ref()
                .ok_or_else(|| crate::windows_dacl::write_omission(Issue::NoStagingOwner))?;
            let scope = self
                .scope
                .provider
                .runtime
                .block_on(root_scope_from_handles(
                    &mut self.connection,
                    &self.tree,
                    &held,
                    &self.scope.identity,
                ))?;
            Intent::create(
                &owner.staging,
                &self.path,
                &plan,
                &self.scope.identity,
                self.scope.provider.identity().account_digest,
                &scope,
            )
        })();
        let intent = match intent {
            Ok(value) => value,
            Err(error) => {
                self.release_optional(&mut held)?;
                return Err(error);
            }
        };
        Ok(Box::new(Transaction {
            receipt: self,
            held,
            base,
            plan,
            intent,
            attempted: false,
            closed_prepared: false,
            applied: false,
            report: Default::default(),
        }))
    }
}
struct Transaction {
    receipt: Box<Receipt>,
    held: Vec<FileId>,
    base: FileId,
    plan: CreatedDirectoryDaclPlan,
    intent: Intent,
    attempted: bool,
    closed_prepared: bool,
    applied: bool,
    report: CreatedDirectoryDaclReport,
}
impl Transaction {
    fn verify(&mut self, after: bool) -> io::Result<()> {
        self.receipt.original_id()?;
        let r = &mut self.receipt;
        r.scope.provider.runtime.block_on(async {
            let entry =
                query_handle_entry(&mut r.connection, &r.tree, self.base, String::new()).await?;
            if entry.file_id != self.plan.file_id || !entry.is_dir || entry.is_symlink {
                return Err(crate::real_mode::plan_drift(
                    "V7-Rechteobjekt wurde ausgetauscht",
                ));
            }
            wire::exact(
                &wire::query(
                    &mut r.connection,
                    &r.tree,
                    self.base,
                    self.plan.context.namespace,
                )
                .await?,
                if after {
                    &self.plan.after
                } else {
                    &self.plan.before
                },
                &self.plan.context,
            )
        })
    }
    fn unclear(&self, error: io::Error) -> io::Error {
        crate::error::with_created_directory_dacl_report(crate::error::with_recovery_context(crate::coded_error(error.kind(),crate::ErrorCode::OutcomeUnclear,format!("Rechte des neu erzeugten Ordners besitzen einen unklaren Abschluss: {error}")),&[&error],&[&self.receipt.path,self.intent.path()]),&self.report())
    }
    fn close(&mut self) -> io::Result<()> {
        let r = &mut self.receipt;
        let closed = r.scope.provider.runtime.block_on(wire::close_all(
            &mut r.connection,
            &r.tree,
            &mut self.held,
        ));
        let original = r.release();
        closed.and(original)
    }
}
impl CreatedDirectoryDaclTransaction for Transaction {
    fn abandon_prepared(mut self: Box<Self>) -> io::Result<()> {
        if self.attempted {
            return Err(self.unclear(invalid(
                "Eine mögliche Rechtewirkung darf nicht als unbegonnen aufgegeben werden",
            )));
        }
        self.close().map_err(|error| self.unclear(error))?;
        self.closed_prepared = true;
        self.intent
            .finish(false)
            .map_err(|error| self.unclear(error))
    }
    fn apply(&mut self) -> io::Result<CreatedDirectoryDaclReport> {
        if self.attempted {
            return Err(self.unclear(invalid("V7-SET bereits versucht")));
        }
        self.receipt.scope.check(&self.receipt.path, true)?;
        self.verify(false).map_err(|error| {
            if crate::windows_dacl::known_write_omission(&error) == Some(Issue::Normalized) {
                crate::real_mode::plan_drift(
                    "Anfangsrechte des neuen Ordners änderten sich vor der Rechtewirkung",
                )
            } else {
                error
            }
        })?;
        let bytes = self
            .plan
            .after
            .encode_for_directory_write(self.plan.context.namespace, &self.plan.context.owner)?;
        self.intent.phase(Phase::Applying)?;
        self.attempted = true;
        #[cfg(test)]
        if self.plan.origin == CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren {
            TEST_BEFORE_CHILDREN_SETS
                .with(|paths| paths.borrow_mut().push(self.receipt.path.clone()));
        }
        let result = (|| {
            let r = &mut self.receipt;
            r.scope.provider.runtime.block_on(wire::set(
                &mut r.connection,
                &r.tree,
                self.base,
                bytes,
            ))?;
            #[cfg(test)]
            if TEST_FAULT.with(|fault| fault.get() == Some("after_set")) {
                TEST_FAULT.with(|fault| fault.set(None));
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Synthetischer Antwortverlust nach tatsächlichem V7-SET",
                ));
            }
            self.verify(true)?;
            let r = &mut self.receipt;
            r.scope.check(&r.path, true)?;
            let fresh = r.scope.provider.runtime.block_on(wire::open(
                &mut r.connection,
                &r.tree,
                &r.remote,
                DIRECTORY_ACCESS,
                true,
                false,
            ))?;
            self.held.push(fresh);
            let old = self.base;
            self.base = fresh;
            let readback = self.verify(true);
            self.base = old;
            readback?;
            self.report
                .completed
                .push(self.plan.outcome(&self.receipt.path.to_string_lossy())?);
            self.intent.phase(Phase::Applied)?;
            self.applied = true;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(self.report()),
            Err(error) => Err(self.unclear(error)),
        }
    }
    fn report(&self) -> CreatedDirectoryDaclReport {
        let mut report = self.intent.report();
        report.merge(&self.report);
        report
    }
    fn recovery_paths(&self) -> Vec<String> {
        vec![
            self.receipt.path.to_string_lossy().into_owned(),
            self.intent.path().to_string_lossy().into_owned(),
        ]
    }
    fn finish(mut self: Box<Self>) -> io::Result<CreatedDirectoryDaclReport> {
        if !self.applied {
            return Err(self.unclear(invalid("V7 nicht bestätigt")));
        }
        if let Err(error) = self.close().and_then(|()| self.intent.finish(true)) {
            return Err(self.unclear(error));
        }
        #[cfg(test)]
        if self.plan.origin == CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren {
            TEST_BEFORE_CHILDREN_FINISHED
                .with(|paths| paths.borrow_mut().push(self.receipt.path.clone()));
        }
        Ok(self.report.clone())
    }
}
impl Drop for Transaction {
    fn drop(&mut self) {
        let closed = self.close();
        if !self.attempted && !self.closed_prepared && closed.is_ok() {
            let _ = self.intent.finish(false);
        }
    }
}

#[cfg(test)]
#[path = "smb_created_directory_dacl_tests.rs"]
mod tests;
