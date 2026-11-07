//! Gemeinsame Handle-/Wire-Helfer für Windows-DACLs und Writer eigener Datei-Temps.
#[cfg(test)]
thread_local! {
    pub(super) static TEST_BEFORE_CHILDREN_FILE_DACL_PATHS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(super) static TEST_BEFORE_CHILDREN_CONTEXTS: std::cell::RefCell<Vec<(PathBuf, Context, Context)>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(super) static TEST_BEFORE_CHILDREN_FILE_IDS: std::cell::RefCell<Vec<(PathBuf, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}
use super::*;
use crate::metadata::Acl;
use crate::windows_dacl::{
    WindowsDaclTargetIssue as Issue, WindowsDaclWriteContext as Context,
    WindowsDaclWriteSupport as Support,
};
use crate::windows_security::{WindowsAce, WindowsDacl, WindowsDaclState, WindowsSid};

// Daten, Attribute, READ_CONTROL, WRITE_DAC, DELETE und SYNCHRONIZE; nie WRITE_OWNER.
pub(super) const FILE_ACCESS: u32 = 0x0017_01ff;
const DATA_WRITE_ACCESS: u32 = FileAccessMask::FILE_WRITE_DATA | FileAccessMask::FILE_APPEND_DATA;
const OPTIONAL_COMPLETION_ACCESS: u32 = FileAccessMask::FILE_READ_EA
    | FileAccessMask::FILE_WRITE_EA
    | FileAccessMask::FILE_EXECUTE
    | FileAccessMask::WRITE_DAC;
const DACL_INFORMATION: u32 = 0x8000_0004;

#[cfg(test)]
thread_local! {
    pub(super) static TEST_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    pub(super) static TEST_BEFORE_SET: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn test_fault(point: &str) -> io::Result<()> {
    TEST_FAULT.with(|fault| {
        if fault.get() == Some(point) {
            fault.set(None);
            if point == "before_open" || point == "reopen_denied" {
                return Err(omission(Issue::RequiredAccessDenied));
            }
            if point == "normalized" {
                return Err(omission(Issue::Normalized));
            }
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Synthetischer DACL-Transportfehler nach SET",
            ));
        }
        Ok(())
    })
}

fn omission(issue: Issue) -> io::Error {
    crate::windows_dacl::write_omission(issue)
}

pub(super) fn status(frame: &smb2::client::connection::Frame, command: Command) -> io::Result<()> {
    if frame.header.command != command {
        return Err(invalid("Falscher SMB-Antworttyp bei DACL-Wirkung"));
    }
    if frame.header.status == NtStatus::ACCESS_DENIED
        || matches!(
            frame.header.status.0,
            0xc000_0003 | 0xc000_0010 | 0xc000_00bb
        )
    {
        smb2::msg::header::ErrorResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(smb_error)?;
        return Err(omission(
            if frame.header.status == NtStatus::ACCESS_DENIED {
                Issue::RequiredAccessDenied
            } else {
                Issue::SecurityUnsupported
            },
        ));
    }
    require_status(frame, command)
}

pub(super) async fn open(
    connection: &mut Connection,
    tree: &Tree,
    remote: &str,
    access: u32,
    directory: bool,
    share_delete: bool,
) -> io::Result<FileId> {
    debug_assert_eq!(access & FileAccessMask::WRITE_OWNER, 0);
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(access),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access: ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | ShareAccess::FILE_SHARE_WRITE
                | if share_delete {
                    ShareAccess::FILE_SHARE_DELETE
                } else {
                    0
                },
        ),
        create_disposition: CreateDisposition::FileOpen,
        create_options: FILE_OPEN_REPARSE_POINT
            | if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            },
        name: remote.into(),
        create_contexts: Vec::new(),
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    status(&frame, Command::Create)?;
    let id = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
        .map_err(smb_error)?
        .file_id;
    let result = query_handle_entry(connection, tree, id, String::new())
        .await
        .and_then(|entry| {
            if entry.file_id == 0 || entry.is_symlink || entry.is_dir != directory {
                Err(crate::real_mode::plan_drift(
                    "SMB-DACL-Basis ist kein gebundenes echtes Objekt",
                ))
            } else {
                Ok(())
            }
        });
    if let Err(error) = result {
        let _ = tree.close_handle(connection, id).await;
        return Err(error);
    }
    Ok(id)
}

/// Nur der Inplace-Datei-Writer sperrt Daten-Schreibhandles zusätzlich aus.
/// Das verhindert ausdrücklich keine neuen Hardlinks auf dem Server.
pub(super) async fn open_file_inplace(
    connection: &mut Connection,
    tree: &Tree,
    remote: &str,
    access: u32,
) -> io::Result<FileId> {
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(access),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access: ShareAccess(ShareAccess::FILE_SHARE_READ),
        create_disposition: CreateDisposition::FileOpen,
        create_options: FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE,
        name: remote.into(),
        create_contexts: Vec::new(),
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    status(&frame, Command::Create)?;
    let id = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
        .map_err(smb_error)?
        .file_id;
    let checked = query_handle_entry(connection, tree, id, String::new()).await;
    match checked {
        Ok(entry) if entry.file_id != 0 && !entry.is_dir && !entry.is_symlink => Ok(id),
        result => {
            let _ = tree.close_handle(connection, id).await;
            result?;
            Err(crate::real_mode::plan_drift(
                "Datei-DACL benötigt eine reguläre bestehende Datei",
            ))
        }
    }
}

pub(super) async fn parents(
    connection: &mut Connection,
    tree: &Tree,
    remote: &str,
    selected: &str,
    held: &mut Vec<FileId>,
) -> io::Result<()> {
    let components: Vec<_> = remote.split('\\').collect();
    for count in 0..components.len() {
        let parent = components[..count].join("\\");
        let in_root = selected.is_empty()
            || parent == selected
            || parent
                .strip_prefix(selected)
                .is_some_and(|rest| rest.starts_with('\\'));
        let access = FileAccessMask::FILE_READ_ATTRIBUTES
            | FileAccessMask::SYNCHRONIZE
            | if in_root {
                FileAccessMask::FILE_READ_DATA
            } else {
                0
            };
        held.push(open(connection, tree, &parent, access, true, false).await?);
    }
    Ok(())
}

pub(super) async fn query(
    connection: &mut Connection,
    tree: &Tree,
    id: FileId,
    namespace: [u8; 16],
) -> io::Result<security::SecurityObservation> {
    let request = QueryInfoRequest {
        info_type: InfoType::Security,
        file_info_class: 0,
        output_buffer_length: 64 * 1024,
        additional_information: 0x7,
        flags: 0,
        file_id: id,
        input_buffer: Vec::new(),
    };
    let frame = connection
        .execute(Command::QueryInfo, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    let observed = security::decode_security(&frame, namespace)?;
    if let Some(issue) = &observed.security.issue {
        return Err(match issue {
            crate::metadata::WindowsSecurityReadIssue::ReadControlDenied => {
                omission(Issue::RequiredAccessDenied)
            }
            crate::metadata::WindowsSecurityReadIssue::QueryUnsupported => {
                omission(Issue::SecurityUnsupported)
            }
            crate::metadata::WindowsSecurityReadIssue::InvalidDescriptor => {
                invalid("Ungültiger DACL-Descriptor im SMB-Readback")
            }
        });
    }
    Ok(observed)
}

pub(super) fn context(observed: &security::SecurityObservation) -> io::Result<Context> {
    let value = Context {
        namespace: observed.security.namespace,
        owner: observed
            .security
            .owner
            .clone()
            .ok_or_else(|| omission(Issue::MissingOwner))?,
        group: observed.security.group.clone(),
    };
    value.validate()?;
    Ok(value)
}

pub(super) fn exact(
    observed: &security::SecurityObservation,
    acl: &WindowsDacl,
    expected: &Context,
) -> io::Result<()> {
    if context(observed)? != *expected {
        return Err(crate::real_mode::plan_drift(
            "SMB-DACL-Wirkung änderte Owner oder Group",
        ));
    }
    if observed.acl.as_ref() != Some(&Acl::WindowsDacl(acl.clone())) {
        return Err(omission(Issue::Normalized));
    }
    Ok(())
}

pub(super) async fn set(
    connection: &mut Connection,
    tree: &Tree,
    id: FileId,
    bytes: Vec<u8>,
) -> io::Result<()> {
    let request = SetInfoRequest {
        info_type: InfoType::Security,
        file_info_class: 0,
        additional_information: DACL_INFORMATION,
        file_id: id,
        buffer: bytes,
    };
    let frame = connection
        .execute(Command::SetInfo, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    #[cfg(test)]
    let frame = TEST_FAULT.with(|fault| {
        let mut frame = frame;
        if fault.get() == Some("invalid_frame") {
            fault.set(None);
            frame.body.clear();
        }
        frame
    });
    status(&frame, Command::SetInfo)?;
    smb2::msg::set_info::SetInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
        .map_err(smb_error)?;
    Ok(())
}

fn probe_acl(context: &Context) -> WindowsDacl {
    WindowsDacl {
        namespace: context.namespace,
        control: 0x1004,
        state: WindowsDaclState::Acl {
            revision: 2,
            // Absichtlich nicht kanonisch: Ein Server darf weder Deny noch Reihenfolge verlieren.
            aces: vec![
                WindowsAce::Allow {
                    flags: 0,
                    mask: FILE_ACCESS,
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
                    mask: 0x0012_0089,
                    sid: WindowsSid {
                        revision: 1,
                        authority: 1,
                        sub_authorities: vec![0],
                    },
                },
            ],
        },
    }
}

pub(super) async fn identity(
    connection: &mut Connection,
    tree: &Tree,
    id: FileId,
    expected: u64,
) -> io::Result<()> {
    let entry = query_handle_entry(connection, tree, id, String::new()).await?;
    if entry.file_id != expected || entry.is_dir || entry.is_symlink {
        return Err(crate::real_mode::plan_drift(
            "SMB-DACL-Dateiidentität änderte sich",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn fresh(
    connection: &mut Connection,
    tree: &Tree,
    remote: &str,
    inode: u64,
    acl: &WindowsDacl,
    expected: &Context,
    access: u32,
    share_delete: bool,
) -> io::Result<FileId> {
    #[cfg(test)]
    test_fault("reopen_denied")?;
    let id = open(connection, tree, remote, access, false, share_delete)
        .await
        .map_err(|error| {
            if crate::windows_dacl::known_write_omission(&error)
                == Some(Issue::RequiredAccessDenied)
            {
                omission(Issue::ReopenDenied)
            } else {
                error
            }
        })?;
    let result = async {
        identity(connection, tree, id, inode).await?;
        exact(
            &query(connection, tree, id, expected.namespace).await?,
            acl,
            expected,
        )
    }
    .await;
    match result {
        Ok(()) => Ok(id),
        Err(error) => {
            let _ = tree.close_handle(connection, id).await;
            Err(error)
        }
    }
}

async fn partial_reopen(
    connection: &mut Connection,
    tree: &Tree,
    remote: &str,
    inode: u64,
    acl: &WindowsDacl,
    expected: &Context,
    access: u32,
) -> io::Result<()> {
    // Der SET-Handle hält DELETE ohne Deletefreigabe und verhindert damit fremden
    // Rename/Unlink während SET. Ein zweiter DELETE-Zugriff würde mit dieser
    // eigenen Sperre kollidieren; zunächst alle übrigen Abschlussrechte prüfen.
    let id = fresh(
        connection,
        tree,
        remote,
        inode,
        acl,
        expected,
        access & !FileAccessMask::DELETE,
        true,
    )
    .await?;
    tree.close_handle(connection, id).await.map_err(smb_error)
}

pub(super) async fn close_all(
    connection: &mut Connection,
    tree: &Tree,
    held: &mut Vec<FileId>,
) -> io::Result<()> {
    let mut result = Ok(());
    while let Some(id) = held.pop() {
        let close = tree.close_handle(connection, id).await.map_err(smb_error);
        if result.is_ok() {
            result = close;
        }
    }
    result
}

impl OrdinarySmb {
    pub(super) fn dacl_support(&self) -> io::Result<Support> {
        self.check(&self.root, false)?;
        if !self.sync_metadata || !self.stream_target {
            return Ok(Support::Unavailable(Issue::UnsupportedBackend));
        }
        Ok(lock(&self.dacl_support)
            .clone()
            .unwrap_or(Support::Unavailable(Issue::NotPrepared)))
    }

    pub(super) fn prepare_dacl(&self) -> io::Result<()> {
        self.check(&self.root, false)?;
        if !self.sync_metadata || !self.stream_target {
            return Ok(());
        }
        let mut cache = lock(&self.dacl_support);
        if cache.is_some() {
            return Ok(());
        }
        if self.owner.is_none() {
            *cache = Some(Support::Unavailable(Issue::NoStagingOwner));
            return Ok(());
        }
        let suffix = random_hex()?;
        let target = self
            .root
            .join(format!(".doppeldecker-dacl-probe-final-{suffix}"));
        let temp = self.root.join(format!(".doppeldecker-dacl-probe-{suffix}"));
        let mut scope = self.clone();
        scope.file = Some(Arc::new(OrdinarySmbFile {
            target: target.clone(),
            temp: Mutex::new(None),
            intent: OnceLock::new(),
            attempted: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            dacl_attempted: AtomicBool::new(false),
            dacl_flush: Mutex::new(None),
            sparse: Mutex::new(None),
            sparse_commit: Mutex::new(None),
        }));
        let remote = self.provider.remote_path(&temp)?;
        let selected = self.provider.remote_path(&self.root)?;
        let (tree, mut connection) = {
            let state = lock(&self.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let mut held = Vec::new();
        let mut base = None;
        let mut created_inode = None;
        let result = (|| {
            self.provider.runtime.block_on(parents(
                &mut connection,
                &tree,
                &remote,
                &selected,
                &mut held,
            ))?;
            // Der Intent entsteht vor CREATE; die ausgewählten Eltern bleiben
            // während Anlage, SET, Readback und Cleanup durch Handles gebunden.
            let mut writer = scope.open_write_new(&temp)?;
            let inode = lock(&scope.file.as_ref().unwrap().temp)
                .as_ref()
                .map(|(_, inode)| *inode)
                .ok_or_else(|| invalid("DACL-Probe hat keine gebundene Datei-ID"))?;
            created_inode = Some(inode);
            writer.flush()?;
            drop(writer);
            let (id, expected) = self.provider.runtime.block_on(async {
                let id = open(&mut connection, &tree, &remote, FILE_ACCESS, false, false).await?;
                base = Some(id);
                identity(&mut connection, &tree, id, inode).await?;
                let observed =
                    query(&mut connection, &tree, id, self.provider.config.server_guid).await?;
                Ok::<_, io::Error>((id, context(&observed)?))
            })?;
            let acl = probe_acl(&expected);
            let bytes = acl.encode_for_file_write(expected.namespace, &expected.owner)?;
            self.check(&temp, true)?;
            self.provider.runtime.block_on(async {
                set(&mut connection, &tree, id, bytes).await?;
                #[cfg(test)]
                test_fault("after_set")?;
                #[cfg(test)]
                test_fault("normalized")?;
                identity(&mut connection, &tree, id, inode).await?;
                exact(
                    &query(&mut connection, &tree, id, expected.namespace).await?,
                    &acl,
                    &expected,
                )?;
                partial_reopen(
                    &mut connection,
                    &tree,
                    &remote,
                    inode,
                    &acl,
                    &expected,
                    FILE_ACCESS,
                )
                .await
            })?;
            self.check(&temp, true)?;
            self.provider
                .runtime
                .block_on(tree.close_handle(&mut connection, id))
                .map_err(smb_error)?;
            base = None;
            let fresh = self.provider.runtime.block_on(fresh(
                &mut connection,
                &tree,
                &remote,
                inode,
                &acl,
                &expected,
                FILE_ACCESS,
                false,
            ))?;
            base = Some(fresh);
            self.check(&temp, true)?;
            Ok(expected)
        })();
        // Selbst bei normalisierter/zugriffssperrender DACL bleibt der vorher
        // gehaltene DELETE-Handle für den bekannten eigenen Probe-Temp zuständig.
        let cleanup = if let Err(error) = self.check_namespace(&temp, true) {
            Err(error)
        } else if let Some(id) = base {
            self.provider.runtime.block_on(async {
                identity(&mut connection, &tree, id, created_inode.unwrap()).await?;
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
                require_status(&frame, Command::SetInfo)
            })
        } else if created_inode.is_none() {
            // Ohne CREATE-Identitätsbeleg ist keine Löschung legitimiert.
            // Auch bei einem unklaren CREATE bleibt der vorher gespeicherte Intent.
            Err(invalid(
                "DACL-Probe hat keinen gesicherten CREATE-Abschluss",
            ))
        } else {
            scope.cleanup_owned_temp(&temp).and_then(|removed| {
                if removed {
                    Ok(())
                } else {
                    Err(invalid("DACL-Probetemp blieb ungebunden"))
                }
            })
        };
        if let Some(id) = base {
            held.push(id);
        }
        let closed = self
            .provider
            .runtime
            .block_on(close_all(&mut connection, &tree, &mut held));
        let cleanup = cleanup
            .and(closed)
            .and_then(|()| match self.provider.stat(&temp) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
                Ok(_) => Err(invalid("DACL-Probe ist nach Cleanup noch sichtbar")),
            });
        if let Err(error) = cleanup {
            return Err(self.dacl_unclear(&temp, scope.staging_recovery_path(), error));
        }
        // Den Scope nur nach belegter Abwesenheit abschließen. Sein normaler
        // Cleanup besitzt hier keinen weiteren Dateieffekt mehr.
        scope
            .file
            .as_ref()
            .unwrap()
            .settled
            .store(true, Ordering::SeqCst);
        scope.finish_staging()?;
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

    pub(super) fn dacl_unclear(
        &self,
        path: &Path,
        intent: Option<&Path>,
        error: io::Error,
    ) -> io::Error {
        let mut paths = vec![path];
        paths.extend(intent);
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::ErrorCode::OutcomeUnclear,
                format!("SMB-Tempwirkung besitzt einen unklaren Abschluss: {error}"),
            ),
            &[&error],
            &paths,
        )
    }

    pub(super) fn remove_owned_temp(&self, path: &Path) -> io::Result<bool> {
        let Some(file) = &self.file else {
            return Ok(false);
        };
        if file.attempted.load(Ordering::SeqCst) || file.settled.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if lock(&file.sparse)
            .as_ref()
            .is_some_and(|state| !lock(state).allows_cleanup())
        {
            return Err(self.dacl_unclear(
                path,
                self.staging_recovery_path(),
                invalid("SMB-Sparse-Temp besitzt keinen sicheren Cleanupabschluss"),
            ));
        }
        let Some((temp, inode)) = lock(&file.temp).clone() else {
            return Ok(false);
        };
        if path != temp {
            return Ok(false);
        }
        if lock(&file.dacl_flush).is_some() {
            return Err(self.dacl_unclear(
                path,
                self.staging_recovery_path(),
                invalid("DACL-Temp mit gebundenem FLUSH-Abschluss bleibt zur Prüfung erhalten"),
            ));
        }
        // Abbruch verhindert neue Arbeit, aber nicht den sicheren Abschluss eines
        // eigenen Temps. Profil, Root und Eltern bleiben dabei frisch geprüft.
        self.check_namespace(path, true)?;
        self.close_sparse_commit()?;
        if file
            .attempted
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(false);
        }
        let remote = self.provider.remote_path(path)?;
        let selected = self.provider.remote_path(&self.root)?;
        let (tree, mut connection) = {
            let state = lock(&self.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let mut held = Vec::new();
        let result = (|| {
            let id = self.provider.runtime.block_on(async {
                parents(&mut connection, &tree, &remote, &selected, &mut held).await?;
                let id = open(
                    &mut connection,
                    &tree,
                    &remote,
                    FileAccessMask::DELETE
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE,
                    false,
                    false,
                )
                .await?;
                held.push(id);
                identity(&mut connection, &tree, id, inode).await?;
                Ok::<_, io::Error>(id)
            })?;
            self.check_namespace(path, true)?;
            self.provider.runtime.block_on(async {
                identity(&mut connection, &tree, id, inode).await?;
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
            })
        })();
        let closed = self
            .provider
            .runtime
            .block_on(close_all(&mut connection, &tree, &mut held));
        result
            .and(closed)
            .and_then(|()| match self.provider.stat(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
                Ok(_) => Err(invalid(
                    "SMB-Temp ist nach identitätsgebundener Löschung noch sichtbar",
                )),
            })
            .map_err(|error| {
                self.dacl_unclear(path, file.intent.get().map(|intent| intent.path()), error)
            })?;
        file.settled.store(true, Ordering::SeqCst);
        Ok(true)
    }

    pub(super) fn rename_dacl_temp(&self, from: &Path, to: &Path, replace: bool) -> io::Result<()> {
        self.check_temp_completion(from)?;
        self.check(from, true)?;
        self.check(to, true)?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| invalid("DACL-Commit hat keinen eigenen Temp"))?;
        let (temp, inode) = lock(&file.temp)
            .clone()
            .ok_or_else(|| invalid("DACL-Commit besitzt keine Temp-ID"))?;
        if from != temp {
            return Err(invalid("DACL-Commit liegt nicht auf dem eigenen Temp"));
        }
        if file
            .attempted
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(self.dacl_unclear(
                from,
                self.staging_recovery_path(),
                invalid("SMB-Temp ist bereits beansprucht"),
            ));
        }
        self.prepare_sparse_commit()?;
        let remote_from = self.provider.remote_path(from)?;
        let remote_to = self.provider.remote_path(to)?;
        let selected = self.provider.remote_path(&self.root)?;
        let (tree, mut connection) = {
            let state = lock(&self.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let mut held = Vec::new();
        let mut collision = false;
        let result = (|| {
            let id = self.provider.runtime.block_on(async {
                parents(&mut connection, &tree, &remote_from, &selected, &mut held).await?;
                parents(&mut connection, &tree, &remote_to, &selected, &mut held).await?;
                let id = open(
                    &mut connection,
                    &tree,
                    &remote_from,
                    FileAccessMask::DELETE
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE,
                    false,
                    false,
                )
                .await?;
                held.push(id);
                identity(&mut connection, &tree, id, inode).await?;
                Ok::<_, io::Error>(id)
            })?;
            self.check(from, true)?;
            self.check(to, true)?;
            self.provider.runtime.block_on(async {
                identity(&mut connection, &tree, id, inode).await?;
                // Die DACL gilt dem eigenen Objekt. Auch der spätere Commit darf
                // daher niemals einen nur gleichnamigen Nachfolger umbenennen.
                let request = SetInfoRequest {
                    info_type: InfoType::File,
                    file_info_class: FILE_RENAME_INFORMATION,
                    additional_information: 0,
                    file_id: id,
                    buffer: rename_buffer(&remote_to, replace),
                };
                let frame = connection
                    .execute(Command::SetInfo, &request, Some(tree.tree_id))
                    .await
                    .map_err(smb_error)?;
                if let Err(error) = require_status(&frame, Command::SetInfo) {
                    smb2::msg::header::ErrorResponse::unpack(&mut ReadCursor::new(&frame.body))
                        .map_err(smb_error)?;
                    collision = !replace && error.kind() == io::ErrorKind::AlreadyExists;
                    return Err(error);
                }
                smb2::msg::set_info::SetInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(smb_error)?;
                identity(&mut connection, &tree, id, inode).await
            })?;
            self.check(to, true)?;
            match self.provider.stat(from) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => return Err(invalid("DACL-Commit ließ den Quellnamen sichtbar")),
            }
            if self.provider.stat(to)?.inode != Some(inode) {
                return Err(invalid("DACL-Commit lieferte eine andere Zielidentität"));
            }
            Ok(())
        })();
        let closed = self
            .provider
            .runtime
            .block_on(close_all(&mut connection, &tree, &mut held));
        if let Err(error) = closed {
            return Err(self.dacl_unclear(from, self.staging_recovery_path(), error));
        }
        if let Err(error) = result {
            if collision {
                self.reject_sparse_commit(from).map_err(|error| {
                    self.dacl_unclear(from, self.staging_recovery_path(), error)
                })?;
                file.attempted.store(false, Ordering::SeqCst);
                return Err(error);
            }
            return Err(self.dacl_unclear(from, self.staging_recovery_path(), error));
        }
        self.finish_sparse_commit(to)?;
        file.settled.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub(super) fn check_temp_completion(&self, path: &Path) -> io::Result<()> {
        self.check_sparse_metadata_flushed(path)?;
        if let Some(file) = &self.file {
            if lock(&file.sparse)
                .as_ref()
                .is_some_and(|state| !lock(state).allows_commit())
            {
                return Err(self.dacl_unclear(
                    path,
                    self.staging_recovery_path(),
                    invalid("SMB-Sparse-Temp wurde nicht erfolgreich abgeschlossen"),
                ));
            }
            if file.attempted.load(Ordering::SeqCst) {
                return Err(self.dacl_unclear(
                    path,
                    file.intent.get().map(|intent| intent.path()),
                    invalid(
                        "SMB-DACL-Temp besitzt bereits einen unklaren oder abgeschlossenen Effekt",
                    ),
                ));
            }
        }
        self.check_dacl_temp_flushed(path)
    }

    pub(super) fn write_dacl(
        &self,
        path: &Path,
        acl: &WindowsDacl,
        expected: &Context,
    ) -> io::Result<()> {
        self.check(path, true)?;
        expected.validate()?;
        if self.dacl_support()? != Support::Ready(expected.clone()) {
            return Err(crate::real_mode::plan_drift(
                "SMB-DACL-Zielkontext ist nicht vorbereitet",
            ));
        }
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| invalid("DACL-Write braucht einen eigenen Temp"))?;
        if file.attempted.load(Ordering::SeqCst)
            || file.settled.load(Ordering::SeqCst)
            || file.dacl_attempted.load(Ordering::SeqCst)
        {
            return Err(invalid("SMB-DACL-Temp wurde bereits verwendet"));
        }
        let (temp, inode) = lock(&file.temp)
            .clone()
            .ok_or_else(|| invalid("SMB-DACL-Temp ist nicht gebunden"))?;
        if path != temp || file.intent.get().is_none() {
            return Err(invalid(
                "SMB-DACL-Write liegt nicht auf dem eigenen Queue-Temp",
            ));
        }
        let bytes = acl.encode_for_file_write(expected.namespace, &expected.owner)?;
        let requires_granted_flush = matches!(&acl.state, WindowsDaclState::Acl { aces, .. }
            if aces.iter().any(|ace| matches!(ace, WindowsAce::Deny { mask, .. } if mask & DATA_WRITE_ACCESS != 0)));
        let shared_sparse = self.sparse_dacl_handle(path)?;
        let completion_access = if requires_granted_flush || shared_sparse.is_some() {
            FILE_ACCESS & !DATA_WRITE_ACCESS
        } else {
            FILE_ACCESS
        } & !OPTIONAL_COMPLETION_ACCESS;
        let remote = self.provider.remote_path(path)?;
        let selected = self.provider.remote_path(&self.root)?;
        let (tree, mut connection) = if let Some((tree, connection, _)) = &shared_sparse {
            (tree.clone(), connection.clone())
        } else {
            let state = lock(&self.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let mut held = Vec::new();
        let mut submitted = false;
        let result = (|| {
            let (id, observed_context) = self.provider.runtime.block_on(async {
                #[cfg(test)]
                test_fault("before_open")?;
                parents(&mut connection, &tree, &remote, &selected, &mut held).await?;
                // Nur der Sparse-Guard besitzt diesen FID. DACL-SET und Query
                // leihen ihn; die lokale Elternliste darf ihn nicht schließen.
                let id = if let Some((_, _, id)) = &shared_sparse {
                    *id
                } else {
                    let id =
                        open(&mut connection, &tree, &remote, FILE_ACCESS, false, false).await?;
                    held.push(id);
                    id
                };
                identity(&mut connection, &tree, id, inode)
                    .await
                    .map_err(|error| {
                        file.attempted.store(true, Ordering::SeqCst);
                        self.dacl_unclear(
                            path,
                            file.intent.get().map(|intent| intent.path()),
                            error,
                        )
                    })?;
                let observed_context =
                    context(&query(&mut connection, &tree, id, expected.namespace).await?)?;
                #[cfg(test)]
                let observed_context = TEST_FAULT.with(|fault| {
                    let mut observed = observed_context;
                    if fault.get() == Some("different_context") {
                        fault.set(None);
                        observed.group = None;
                    }
                    observed
                });
                Ok::<_, io::Error>((id, observed_context))
            })?;
            self.check(path, true)?;
            #[cfg(test)]
            TEST_BEFORE_CHILDREN_CONTEXTS.with(|contexts| {
                contexts.borrow_mut().push((
                    path.to_path_buf(),
                    expected.clone(),
                    observed_context.clone(),
                ))
            });
            if observed_context != *expected {
                return Err(omission(Issue::DifferentTempContext));
            }
            #[cfg(test)]
            TEST_BEFORE_SET.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            self.check(path, true)?;
            let completion = if requires_granted_flush {
                Some(super::temp_dacl_flush::TempDaclFlush::grant(
                    self, path, acl, expected,
                )?)
            } else {
                None
            };
            let mut flush = lock(&file.dacl_flush);
            if flush.is_some() {
                return Err(invalid(
                    "SMB-DACL-Temp besitzt bereits eine Abschlussressource",
                ));
            }
            if file.dacl_attempted.swap(true, Ordering::SeqCst) {
                return Err(invalid("SMB-DACL-SET wurde bereits versucht"));
            }
            *flush = completion;
            drop(flush);
            submitted = true;
            #[cfg(test)]
            TEST_BEFORE_CHILDREN_FILE_DACL_PATHS
                .with(|paths| paths.borrow_mut().push(path.to_path_buf()));
            #[cfg(test)]
            TEST_BEFORE_CHILDREN_FILE_IDS
                .with(|ids| ids.borrow_mut().push((file.target.clone(), inode)));
            self.provider.runtime.block_on(async {
                set(&mut connection, &tree, id, bytes).await?;
                #[cfg(test)]
                test_fault("after_set")?;
                #[cfg(test)]
                test_fault("normalized")?;
                identity(&mut connection, &tree, id, inode).await?;
                exact(
                    &query(&mut connection, &tree, id, expected.namespace).await?,
                    acl,
                    expected,
                )?;
                partial_reopen(
                    &mut connection,
                    &tree,
                    &remote,
                    inode,
                    acl,
                    expected,
                    completion_access,
                )
                .await
            })?;
            self.check(path, true)?;
            if shared_sparse.is_none() {
                let old = held.pop().ok_or_else(|| invalid("DACL-SET-Handle fehlt"))?;
                self.provider
                    .runtime
                    .block_on(tree.close_handle(&mut connection, old))
                    .map_err(smb_error)?;
            }
            // Eltern bleiben über diesen Wechsel gehalten. Erst jetzt ist der
            // benötigte frische Abschlusszugriff einschließlich DELETE möglich.
            let fresh = self.provider.runtime.block_on(fresh(
                &mut connection,
                &tree,
                &remote,
                inode,
                acl,
                expected,
                completion_access,
                false,
            ))?;
            held.push(fresh);
            self.check(path, true)
        })();
        let closed = self
            .provider
            .runtime
            .block_on(close_all(&mut connection, &tree, &mut held));
        let result = result.and(closed);
        match result {
            Err(error) if submitted => {
                // Ein möglicherweise wirksamer SET wird niemals automatisch gelöscht,
                // wiederholt oder als bloße Metadatenauslassung fortgesetzt.
                file.attempted.store(true, Ordering::SeqCst);
                Err(self.dacl_unclear(path, file.intent.get().map(|i| i.path()), error))
            }
            other => other,
        }
    }
}

#[cfg(test)]
#[path = "smb_windows_dacl_tests.rs"]
mod tests;
