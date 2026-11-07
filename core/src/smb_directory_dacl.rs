//! DACL-Einzelwirkung an bestehenden Ordnern, getrennt vom Datei-Tempvertrag.
use super::windows_dacl as wire;
use super::*;
use crate::directory_dacl::{DirectoryDaclPlan, DirectoryDaclReport, DirectoryDaclTransaction};
use crate::local_staging::{
    DirectoryDaclIntentBinding, DirectoryDaclIntentPhase as Phase, RemoteDirectoryDaclIntent,
};
use crate::windows_dacl::{WindowsDaclTargetIssue as Issue, WindowsDaclWriteSupport as Support};
use crate::windows_security::{WindowsAce, WindowsDacl, WindowsDaclState};

// LIST_DIRECTORY, TRAVERSE, READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC, SYNCHRONIZE.
const DIRECTORY_ACCESS: u32 = 0x0016_00a1;

#[cfg(test)]
thread_local! {
    static TEST_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static TEST_RECOVERY_HELD: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

struct DirectoryTransaction {
    scope: OrdinarySmb,
    path: PathBuf,
    remote: String,
    tree: Arc<Tree>,
    connection: Connection,
    held: Vec<FileId>,
    base: FileId,
    inode: u64,
    plan: DirectoryDaclPlan,
    intent: RemoteDirectoryDaclIntent,
    report: DirectoryDaclReport,
    attempted: bool,
    applied: bool,
}

impl SmbVfs {
    pub(in crate::smb) fn read_directory_dacl_recovery(
        &self,
        path: &Path,
        expected_root: &crate::RootIdentity,
    ) -> io::Result<crate::windows_dacl::DirectoryDaclRecoveryObservation> {
        let crate::RootIdentity::Smb(expected) = expected_root else {
            return Err(invalid("SMB-DACL-Readback benötigt einen SMB-Root"));
        };
        expected.validate()?;
        let root = Path::new(&expected.virtual_root);
        let normalized = normalize_virtual_path(path)?;
        let path = Path::new(&normalized);
        if !path.starts_with(root) || path == root {
            return Err(invalid(
                "DACL-Recovery ist nur für einen bestehenden Unterordner zulässig",
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
                let id = wire::open(
                    &mut connection,
                    &tree,
                    &remote,
                    DIRECTORY_ACCESS & !FileAccessMask::WRITE_DAC,
                    true,
                    false,
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
                    || !fresh.is_dir
                    || fresh.is_symlink
                {
                    return Err(crate::real_mode::plan_drift(
                        "SMB-DACL-Recoverybasis änderte sich",
                    ));
                }
                Ok(crate::windows_dacl::DirectoryDaclRecoveryObservation {
                    base_id: entry.file_id,
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
    pub(super) fn directory_dacl_support(&self) -> io::Result<Support> {
        self.check(&self.root, false)?;
        if !self.sync_metadata || !self.stream_target {
            return Ok(Support::Unavailable(Issue::UnsupportedBackend));
        }
        Ok(lock(&self.directory_dacl_support)
            .clone()
            .unwrap_or(Support::Unavailable(Issue::NotPrepared)))
    }

    pub(super) fn begin_directory_dacl(
        &self,
        path: &Path,
        plan: &DirectoryDaclPlan,
    ) -> io::Result<Box<dyn DirectoryDaclTransaction>> {
        self.check(path, true)?;
        plan.validate()?;
        if self.directory_dacl_support()? != Support::Ready(plan.context.clone()) {
            return Err(crate::real_mode::plan_drift(
                "SMB-Verzeichnis-DACL-Kontext ist nicht vorbereitet",
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
        let setup = self.provider.runtime.block_on(async {
            wire::parents(&mut connection, &tree, &remote, &selected, &mut held).await?;
            #[cfg(test)]
            if TEST_FAULT.with(|f| f.get() == Some("before_open")) {
                TEST_FAULT.with(|f| f.set(None));
                return Err(crate::windows_dacl::write_omission(
                    Issue::RequiredAccessDenied,
                ));
            }
            let id = wire::open(
                &mut connection,
                &tree,
                &remote,
                DIRECTORY_ACCESS,
                true,
                false,
            )
            .await?;
            held.push(id);
            let entry = query_handle_entry(&mut connection, &tree, id, String::new()).await?;
            if entry.file_id == 0
                || !plan
                    .target_proof
                    .as_ref()
                    .is_some_and(|proof| proof.matches_directory_identity(&entry.entry()))
            {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Verzeichnis-DACL-Basis wurde ausgetauscht",
                ));
            }
            let observed = wire::query(&mut connection, &tree, id, plan.context.namespace).await?;
            if wire::context(&observed)? != plan.context
                || observed.acl.as_ref()
                    != Some(&crate::metadata::Acl::WindowsDacl(plan.before.clone()))
            {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Verzeichnis-DACL oder Owner-/Group-Kontext änderte sich",
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
            RemoteDirectoryDaclIntent::create_with_root(
                &owner.staging,
                DirectoryDaclIntentBinding {
                    directory: path.to_path_buf(),
                    base_id: inode,
                    namespace: plan.context.namespace,
                    before_digest: crate::directory_dacl::dacl_digest(&plan.before)?,
                    after_digest: crate::directory_dacl::dacl_digest(&plan.after)?,
                    owner: plan.context.owner.clone(),
                    group: plan.context.group.clone(),
                    phase: Phase::Prepared,
                },
                &self.identity,
                account,
            )
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
        Ok(Box::new(DirectoryTransaction {
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
            report: DirectoryDaclReport::default(),
            attempted: false,
            applied: false,
        }))
    }
}

impl DirectoryTransaction {
    fn verify(&mut self, acl: &WindowsDacl) -> io::Result<()> {
        let expected = &self.plan.context;
        self.scope.provider.runtime.block_on(async {
            let entry =
                query_handle_entry(&mut self.connection, &self.tree, self.base, String::new())
                    .await?;
            if entry.file_id != self.inode || entry.is_symlink || !entry.is_dir {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Verzeichnis-DACL-ID änderte sich",
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
                format!("SMB-Verzeichnis-DACL-Wirkung oder Abschluss unklar: {error}"),
            ),
            &[&error],
            &[&self.path, self.intent.path()],
        );
        crate::error::with_directory_dacl_report(error, &self.report())
    }
    fn close(&mut self) -> io::Result<()> {
        self.scope.provider.runtime.block_on(wire::close_all(
            &mut self.connection,
            &self.tree,
            &mut self.held,
        ))
    }
}
impl DirectoryDaclTransaction for DirectoryTransaction {
    fn apply(&mut self) -> io::Result<DirectoryDaclReport> {
        if self.attempted {
            return Err(self.unclear(invalid("Verzeichnis-DACL-SET wurde bereits versucht")));
        }
        self.scope.check(&self.path, true)?;
        self.verify(&self.plan.before.clone()).map_err(|error| {
            if crate::windows_dacl::known_write_omission(&error) == Some(Issue::Normalized) {
                crate::real_mode::plan_drift("SMB-Verzeichnis-DACL änderte sich vor SET")
            } else {
                error
            }
        })?;
        let bytes = self
            .plan
            .after
            .encode_for_directory_write(self.plan.context.namespace, &self.plan.context.owner)?;
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
                    "Synthetischer Antwortverlust nach Directory-SET",
                ));
            }
            self.verify(&self.plan.after.clone())?;
            self.scope.check(&self.path, true)?;
            let fresh = self.scope.provider.runtime.block_on(wire::open(
                &mut self.connection,
                &self.tree,
                &self.remote,
                DIRECTORY_ACCESS,
                true,
                false,
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
    fn report(&self) -> DirectoryDaclReport {
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
    fn finish(mut self: Box<Self>) -> io::Result<DirectoryDaclReport> {
        if !self.applied {
            return Err(self.unclear(invalid("Verzeichnis-DACL-Wirkung ist nicht bestätigt")));
        }
        if let Err(error) = self.close().and_then(|()| self.intent.finish()) {
            return Err(self.unclear(error));
        }
        Ok(self.report.clone())
    }
}
impl Drop for DirectoryTransaction {
    fn drop(&mut self) {
        let _ = self.close();
        if !self.attempted {
            let _ = self.intent.finish_prepared();
        }
    }
}

fn probe_acl(context: &crate::windows_dacl::WindowsDaclWriteContext) -> WindowsDacl {
    WindowsDacl {
        namespace: context.namespace,
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            aces: vec![
                WindowsAce::Allow {
                    flags: 0,
                    mask: DIRECTORY_ACCESS | FileAccessMask::DELETE,
                    sid: context.owner.clone(),
                },
                WindowsAce::Deny {
                    flags: 0,
                    mask: FileAccessMask::WRITE_OWNER,
                    sid: context
                        .group
                        .clone()
                        .unwrap_or_else(|| context.owner.clone()),
                },
                WindowsAce::Allow {
                    flags: 0,
                    mask: 0x0012_00a9,
                    sid: crate::windows_security::WindowsSid {
                        revision: 1,
                        authority: 1,
                        sub_authorities: vec![0],
                    },
                },
            ],
        },
    }
}

impl OrdinarySmb {
    pub(super) fn prepare_directory_dacl(&self) -> io::Result<()> {
        self.check(&self.root, false)?;
        if !self.sync_metadata || !self.stream_target {
            return Ok(());
        }
        let mut cache = lock(&self.directory_dacl_support);
        if cache.is_some() {
            return Ok(());
        }
        let Some(owner) = &self.owner else {
            *cache = Some(Support::Unavailable(Issue::NoStagingOwner));
            return Ok(());
        };
        let suffix = random_hex()?;
        let path = self
            .root
            .join(format!(".doppeldecker-directory-dacl-probe-{suffix}"));
        let target = self
            .root
            .join(format!(".doppeldecker-directory-dacl-final-{suffix}"));
        let remote = self.provider.remote_path(&path)?;
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
        let mut creator = None;
        let mut intent = None;
        let result = (|| {
            self.provider.runtime.block_on(wire::parents(
                &mut connection,
                &tree,
                &remote,
                &selected,
                &mut held,
            ))?;
            self.check(&path, true)?;
            // Dies ist ausschließlich ein eigener neuer Tempordner. Der generische
            // Pfadintent entsteht vor CREATE und wird erst nach Abwesenheitsbeleg geschlossen.
            intent = Some(
                crate::local_staging::RemoteTempIntent::create_bound_with_root(
                    &owner.staging,
                    &target,
                    &path,
                    &self.identity,
                    account,
                )?,
            );
            let (id, inode, context) = self.provider.runtime.block_on(async {
                let request = CreateRequest {
                    requested_oplock_level: OplockLevel::None,
                    impersonation_level: ImpersonationLevel::Impersonation,
                    desired_access: FileAccessMask::new(DIRECTORY_ACCESS | FileAccessMask::DELETE),
                    file_attributes: FILE_ATTRIBUTE_NORMAL,
                    share_access: ShareAccess(
                        ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_WRITE,
                    ),
                    create_disposition: CreateDisposition::FileCreate,
                    create_options: FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
                    name: remote.clone(),
                    create_contexts: Vec::new(),
                };
                let frame = connection
                    .execute(Command::Create, &request, Some(tree.tree_id))
                    .await
                    .map_err(smb_error)?;
                wire::status(&frame, Command::Create)?;
                let id = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(smb_error)?
                    .file_id;
                held.push(id);
                let entry = query_handle_entry(&mut connection, &tree, id, String::new()).await?;
                // Ohne den originalen CREATE-Identitätsbeleg wird nie gelöscht.
                if entry.file_id == 0 || !entry.is_dir || entry.is_symlink {
                    return Err(invalid("Ordnerprobe besitzt keine echte CREATE-ID"));
                }
                creator = Some((id, entry.file_id));
                let observed =
                    wire::query(&mut connection, &tree, id, self.provider.config.server_guid)
                        .await?;
                Ok::<_, io::Error>((id, entry.file_id, wire::context(&observed)?))
            })?;
            let acl = probe_acl(&context);
            let bytes = acl.encode_for_directory_write(context.namespace, &context.owner)?;
            self.check(&path, true)?;
            self.provider.runtime.block_on(async {
                wire::set(&mut connection, &tree, id, bytes).await?;
                let entry = query_handle_entry(&mut connection, &tree, id, String::new()).await?;
                if entry.file_id != inode || !entry.is_dir || entry.is_symlink {
                    return Err(invalid("Ordnerprobe änderte die Basis-ID"));
                }
                wire::exact(
                    &wire::query(&mut connection, &tree, id, context.namespace).await?,
                    &acl,
                    &context,
                )?;
                // Der CREATE-Handle bleibt gehalten; Folgezugriff braucht kein DELETE.
                let fresh = wire::open(
                    &mut connection,
                    &tree,
                    &remote,
                    DIRECTORY_ACCESS,
                    true,
                    true,
                )
                .await?;
                held.push(fresh);
                let entry =
                    query_handle_entry(&mut connection, &tree, fresh, String::new()).await?;
                if entry.file_id != inode || !entry.is_dir || entry.is_symlink {
                    return Err(invalid("Frische Ordnerprobe besitzt andere ID"));
                }
                wire::exact(
                    &wire::query(&mut connection, &tree, fresh, context.namespace).await?,
                    &acl,
                    &context,
                )
            })?;
            self.check(&path, true)?;
            Ok(context)
        })();
        let cleanup = (|| {
            self.check_namespace(&path, true)?;
            if let Some((id, inode)) = creator {
                if !self.provider.list(&path)?.is_empty() {
                    return Err(invalid("Ordnerprobe enthält fremde Kinder"));
                }
                self.provider.runtime.block_on(async {
                    let entry =
                        query_handle_entry(&mut connection, &tree, id, String::new()).await?;
                    if entry.file_id != inode || !entry.is_dir || entry.is_symlink {
                        return Err(invalid("CREATE-ID der Ordnerprobe änderte sich"));
                    }
                    let request = SetInfoRequest {
                        info_type: InfoType::File,
                        file_info_class: 13,
                        additional_information: 0,
                        file_id: id,
                        buffer: vec![1],
                    };
                    let frame = connection
                        .execute(Command::SetInfo, &request, Some(tree.tree_id))
                        .await
                        .map_err(smb_error)?;
                    require_status(&frame, Command::SetInfo)?;
                    smb2::msg::set_info::SetInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
                        .map_err(smb_error)?;
                    Ok::<_, io::Error>(())
                })?;
            } else if intent.is_some()
                && result
                    .as_ref()
                    .err()
                    .is_none_or(|error| crate::windows_dacl::known_write_omission(error).is_none())
            {
                return Err(invalid(
                    "Ordnerprobe hat keinen bestätigten CREATE-Abschluss",
                ));
            }
            Ok(())
        })();
        let closed =
            self.provider
                .runtime
                .block_on(wire::close_all(&mut connection, &tree, &mut held));
        let cleanup =
            cleanup
                .and(closed)
                .and_then(|()| match self.provider.stat_unchecked(&path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error),
                    Ok(_) => Err(invalid("Ordnerprobe blieb nach Cleanup sichtbar")),
                });
        if let Err(error) = cleanup {
            return Err(self.dacl_unclear(&path, intent.as_ref().map(|value| value.path()), error));
        }
        if let Some(intent) = intent {
            intent.finish()?;
        }
        self.check(&self.root, false)?;
        *cache = Some(match result {
            Ok(context) => Support::Ready(context),
            Err(error) => match crate::windows_dacl::known_write_omission(&error) {
                Some(issue) => Support::Unavailable(issue),
                None => return Err(error),
            },
        });
        Ok(())
    }
}

#[cfg(test)]
#[path = "smb_directory_dacl_tests.rs"]
mod tests;
