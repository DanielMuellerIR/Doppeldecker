//! Laufgebundene Sparse-Temps; Capabilityproben benutzen denselben geschlossenen Writer.
use super::{windows_dacl as wire, *};
use crate::smb::named_streams::{hash_stream_handle, sparse};
use crate::vfs::SparseWrite;
use smb2::msg::header::ErrorResponse;
use smb2::msg::ioctl::{IoctlRequest, IoctlResponse, SMB2_0_IOCTL_IS_FSCTL};
use std::io::{Seek, SeekFrom};

const SET_SPARSE: u32 = 0x0009_00c4;
const ZERO_DATA: u32 = 0x0009_80c8;
const PROBE_SIZE: u64 = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Active,
    Complete,
    Failed,
    Abandoned,
}
impl State {
    pub(super) fn allows_commit(self) -> bool {
        self == Self::Complete
    }
    pub(super) fn allows_cleanup(self) -> bool {
        matches!(self, Self::Complete | Self::Abandoned)
    }
}

#[derive(Debug)]
struct Unsupported(NtStatus);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "SMB-Sparse-Wirkung ist nicht verfügbar: NTSTATUS {:#010x}",
            self.0 .0
        )
    }
}
impl std::error::Error for Unsupported {}
fn unsupported(status: NtStatus) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, Unsupported(status))
}
fn is_unsupported(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|value| value.is::<Unsupported>())
}
fn status(frame: &smb2::client::connection::Frame, command: Command) -> io::Result<()> {
    if frame.header.command != command {
        return Err(invalid("Falscher SMB-Sparse-Antworttyp"));
    }
    if matches!(
        frame.header.status.0,
        0xc000_0002 | 0xc000_0003 | 0xc000_0010 | 0xc000_0022 | 0xc000_00bb
    ) {
        ErrorResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
        return Err(unsupported(frame.header.status));
    }
    require_status(frame, command)
}

struct Writer {
    inner: SmbWrite,
    parents: Vec<FileId>,
    inode: u64,
    size: u64,
    path: PathBuf,
    state: Arc<Mutex<State>>,
    scope: OrdinarySmb,
    generation: u64,
}
impl Writer {
    fn new(scope: &OrdinarySmb, path: &Path, delete_on_close: bool) -> io::Result<Self> {
        scope
            .provider
            .validate_stream_scope(path, Some(&scope.root))?;
        scope.check(path, true)?;
        let remote = scope.provider.remote_path(path)?;
        let selected = scope.provider.remote_path(&scope.root)?;
        let (tree, mut connection, max_write, generation) = {
            let session = lock(&scope.provider.state);
            (
                session.session.tree.clone(),
                session.session.connection.clone(),
                session
                    .session
                    .connection
                    .params()
                    .map(|p| p.max_write_size.min(MAX_IO_CHUNK))
                    .unwrap_or(65536)
                    .max(1),
                session.generation,
            )
        };
        let mut parents = Vec::new();
        let bound = scope
            .provider
            .runtime
            .block_on(wire::parents(
                &mut connection,
                &tree,
                &remote,
                &selected,
                &mut parents,
            ))
            .and_then(|()| scope.check(path, true));
        let opened = bound.and_then(|()| {
            scope.provider.runtime.block_on(async {
                let request = CreateRequest {
                    requested_oplock_level: OplockLevel::None,
                    impersonation_level: ImpersonationLevel::Impersonation,
                    desired_access: FileAccessMask::new(
                        FileAccessMask::FILE_READ_DATA
                            | FileAccessMask::FILE_WRITE_DATA
                            | FileAccessMask::FILE_READ_ATTRIBUTES
                            | FileAccessMask::FILE_WRITE_ATTRIBUTES
                            | FileAccessMask::SYNCHRONIZE
                            | if delete_on_close {
                                FileAccessMask::DELETE
                            } else {
                                0
                            },
                    ),
                    file_attributes: FILE_ATTRIBUTE_NORMAL,
                    share_access: ShareAccess(ShareAccess::FILE_SHARE_READ),
                    create_disposition: CreateDisposition::FileCreate,
                    create_options: FILE_NON_DIRECTORY_FILE
                        | FILE_OPEN_REPARSE_POINT
                        | if delete_on_close {
                            FILE_DELETE_ON_CLOSE
                        } else {
                            0
                        },
                    name: remote,
                    create_contexts: vec![],
                };
                let frame = connection
                    .execute(Command::Create, &request, Some(tree.tree_id))
                    .await
                    .map_err(smb_error)?;
                status(&frame, Command::Create)?;
                Ok::<_, io::Error>(
                    CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                        .map_err(smb_error)?
                        .file_id,
                )
            })
        });
        let id = match opened {
            Ok(id) => id,
            Err(error) => {
                let close = scope.provider.runtime.block_on(wire::close_all(
                    &mut connection,
                    &tree,
                    &mut parents,
                ));
                return combine(Err(error), close, path);
            }
        };
        let mut value = Self {
            inner: SmbWrite {
                runtime: scope.provider.runtime.clone(),
                tree,
                connection,
                file_id: id,
                offset: 0,
                max_write,
                finished: false,
            },
            parents,
            inode: 0,
            size: 0,
            path: path.into(),
            state: Arc::new(Mutex::new(State::Active)),
            scope: scope.clone(),
            generation,
        };
        let proof = value
            .inner
            .runtime
            .block_on(query_handle_entry(
                &mut value.inner.connection,
                &value.inner.tree,
                id,
                String::new(),
            ))
            .and_then(|entry| {
                if entry.file_id == 0 || entry.is_dir || entry.is_symlink || entry.size != 0 {
                    Err(crate::real_mode::plan_drift(
                        "SMB-Sparse-CREATE ist keine neue reguläre Tempdatei",
                    ))
                } else {
                    Ok(entry.file_id)
                }
            });
        value.inode = value.checked(proof)?;
        Ok(value)
    }
    fn state(&self) -> State {
        *lock(&self.state)
    }
    fn active(&mut self) -> io::Result<()> {
        if self.state() != State::Active {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SMB-Sparse-Writer ist abgeschlossen",
            ));
        }
        // Daten-/Lochrequests adressieren den gehaltenen Handle. Teure Pfad- und
        // Profilprüfungen gehören an Öffnung und Abschluss, nicht an jeden Chunk.
        let result = (if self.scope.cancel.is_cancelled() {
            Err(crate::coded_error(
                io::ErrorKind::Interrupted,
                crate::ErrorCode::Interrupted,
                "SMB-Sparse-Transfer abgebrochen",
            ))
        } else {
            Ok(())
        })
        .and_then(|()| {
            if self.scope.provider.session_generation() != self.generation {
                Err(crate::real_mode::plan_drift(
                    "SMB-Sitzung änderte sich während des Sparse-Transfers",
                ))
            } else {
                Ok(())
            }
        });
        self.checked(result)
    }
    fn close(&mut self) -> io::Result<()> {
        let file = if self.inner.finished {
            Ok(())
        } else {
            // Vor dem Netzwerkversuch markieren: Auch ein verlorenes CLOSE darf
            // weder durch einen weiteren flush noch durch SmbWrite::drop wiederholt werden.
            self.inner.finished = true;
            #[cfg(test)]
            CLOSES.with(|value| value.set(value.get() + 1));
            self.inner.runtime.block_on(async {
                #[cfg(test)]
                if self.scope.file.is_some() {
                    fault("close-before")?;
                }
                self.inner
                    .tree
                    .close_handle(&mut self.inner.connection, self.inner.file_id)
                    .await
                    .map_err(smb_error)?;
                #[cfg(test)]
                if self.scope.file.is_some() {
                    fault("close-after")?;
                }
                Ok(())
            })
        };
        let parents = self.inner.runtime.block_on(wire::close_all(
            &mut self.inner.connection,
            &self.inner.tree,
            &mut self.parents,
        ));
        combine(file, parents, &self.path)
    }
    fn checked<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                *lock(&self.state) = State::Failed;
                let close = self.close();
                combine(Err(error), close, &self.path)
            }
        }
    }
    fn control(&mut self, code: u32, input: Vec<u8>) -> io::Result<()> {
        self.active()?;
        let request = IoctlRequest {
            ctl_code: code,
            file_id: self.inner.file_id,
            max_input_response: 0,
            max_output_response: 0,
            flags: SMB2_0_IOCTL_IS_FSCTL,
            input_data: input,
        };
        let result = self.inner.runtime.block_on(async {
            let frame = self
                .inner
                .connection
                .execute(Command::Ioctl, &request, Some(self.inner.tree.tree_id))
                .await
                .map_err(smb_error)?;
            status(&frame, Command::Ioctl)?;
            let response =
                IoctlResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
            if response.ctl_code != code
                || response.file_id != self.inner.file_id
                || !response.output_data.is_empty()
            {
                return Err(invalid(
                    "SMB-Sparse-IOCTL bestätigte einen anderen Handle oder Nutzdaten",
                ));
            }
            Ok(())
        });
        self.checked(result)
    }
    fn initialize(&mut self) -> io::Result<()> {
        self.control(SET_SPARSE, vec![1])
    }
    fn prove_probe(&mut self) -> io::Result<()> {
        self.active()?;
        let result = self.inner.runtime.block_on(async {
            let ranges = sparse::observe(
                &mut self.inner.connection,
                &self.inner.tree,
                self.inner.file_id,
                PROBE_SIZE,
                Some(&self.scope.cancel),
            )
            .await?;
            match ranges.issue {
                Some(crate::metadata::SparseReadIssue::ServerStatus(code)) => {
                    return Err(unsupported(NtStatus(code)))
                }
                Some(_) => {
                    return Err(invalid(
                        "SMB-Sparse-Probe erhielt keine gültige vollständige Bereichsantwort",
                    ))
                }
                None => {}
            }
            if ranges.extents.as_deref() != Some(&[]) {
                return Err(invalid(
                    "SMB-Sparse-Probe wurde nicht vollständig deallokiert",
                ));
            }
            let read = hash_stream_handle(
                &mut self.inner.connection,
                &self.inner.tree,
                self.inner.file_id,
                PROBE_SIZE,
                65536,
                Some(&self.scope.cancel),
            )
            .await?;
            if read.digest != *blake3::hash(&vec![0; PROBE_SIZE as usize]).as_bytes() {
                return Err(invalid("SMB-Sparse-Probe besitzt falschen Nullinhalt"));
            }
            Ok(())
        });
        self.checked(result)
    }
}
pub(super) fn combine<T>(
    result: io::Result<T>,
    close: io::Result<()>,
    path: &Path,
) -> io::Result<T> {
    match close {
        Ok(()) => result,
        Err(close) => {
            let mut causes: Vec<&io::Error> = vec![&close];
            if let Err(error) = &result {
                causes.push(error);
            }
            Err(crate::error::with_recovery_context(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!("SMB-Sparse-Abschluss blieb unklar: {close}"),
                ),
                &causes,
                &[path],
            ))
        }
    }
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.active()?;
        if self
            .inner
            .offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > i64::MAX as u64)
        {
            return self.checked(Err(invalid("SMB-Sparse-WRITE läuft über signed64")));
        }
        let result = (|| {
            #[cfg(test)]
            fault("write-before")?;
            let count = self.inner.write(bytes)?;
            #[cfg(test)]
            fault("write-after")?;
            Ok(count)
        })();
        let count = self.checked(result)?;
        self.size = self.size.max(self.inner.offset);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.state() == State::Complete {
            return Ok(());
        }
        self.active()?;
        let context = self.scope.check(&self.path, true);
        self.checked(context)?;
        *lock(&self.state) = State::Failed;
        let proof = self.inner.runtime.block_on(async {
            wire::identity(
                &mut self.inner.connection,
                &self.inner.tree,
                self.inner.file_id,
                self.inode,
            )
            .await?;
            #[cfg(test)]
            FLUSHES.with(|value| value.set(value.get() + 1));
            #[cfg(test)]
            if self.scope.file.is_some() {
                fault("data-flush-before")?;
            }
            flush_handle(
                &mut self.inner.connection,
                &self.inner.tree,
                self.inner.file_id,
            )
            .await?;
            #[cfg(test)]
            if self.scope.file.is_some() {
                fault("data-flush-after")?;
            }
            Ok(())
        });
        let mut held = std::mem::take(&mut self.parents);
        let closed = self.close();
        let mut granted = None;
        let proof = combine(proof, closed, &self.path).and_then(|()| {
            if self.scope.file.is_some() {
                granted = Some(CommitGuard::grant(&self.scope, &self.path, self.inode)?);
            }
            Ok(())
        });
        if let Err(error) = proof {
            let close = self.inner.runtime.block_on(wire::close_all(
                &mut self.inner.connection,
                &self.inner.tree,
                &mut held,
            ));
            return combine(Err(error), close, &self.path);
        }
        if let Some(mut guard) = granted {
            guard.parents.append(&mut held);
            *lock(
                &self
                    .scope
                    .file
                    .as_ref()
                    .expect("Staging geprüft")
                    .sparse_commit,
            ) = Some(guard);
        } else {
            let close = self.inner.runtime.block_on(wire::close_all(
                &mut self.inner.connection,
                &self.inner.tree,
                &mut held,
            ));
            combine(Ok(()), close, &self.path)?;
        }
        *lock(&self.state) = State::Complete;
        Ok(())
    }
}
impl Seek for Writer {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.active()?;
        let result = sparse::seek_position(self.inner.offset, self.size, position);
        self.inner.offset = self.checked(result)?;
        Ok(self.inner.offset)
    }
}
impl SparseWrite for Writer {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        self.active()?;
        if size > i64::MAX as u64 {
            return self.checked(Err(invalid("SMB-Sparse-Größe liegt außerhalb signed64")));
        }
        let request = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: 20,
            additional_information: 0,
            file_id: self.inner.file_id,
            buffer: size.to_le_bytes().to_vec(),
        };
        let result = self.inner.runtime.block_on(async {
            let frame = self
                .inner
                .connection
                .execute(Command::SetInfo, &request, Some(self.inner.tree.tree_id))
                .await
                .map_err(smb_error)?;
            status(&frame, Command::SetInfo)?;
            smb2::msg::set_info::SetInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(smb_error)?;
            Ok(())
        });
        self.checked(result)?;
        self.size = size;
        Ok(())
    }
    fn punch_hole(&mut self, offset: u64, length: u64) -> io::Result<()> {
        self.active()?;
        let Some(end) = offset
            .checked_add(length)
            .filter(|end| *end <= i64::MAX as u64)
        else {
            return self.checked(Err(invalid("SMB-Sparse-Lochgrenze läuft über signed64")));
        };
        if length == 0 {
            return Ok(());
        }
        self.control(
            ZERO_DATA,
            [offset.to_le_bytes(), end.to_le_bytes()].concat(),
        )
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        let active = self.state() == State::Active;
        let closed = self.close();
        if active {
            *lock(&self.state) = if closed.is_ok() {
                State::Abandoned
            } else {
                State::Failed
            };
        }
    }
}

impl OrdinarySmb {
    pub(super) fn open_sparse(&self, path: &Path) -> io::Result<Box<dyn SparseWrite>> {
        self.check(path, true)?;
        if !self.sync_metadata
            || !self.stream_target
            || *lock(&self.sparse_support) != Some(Capability::Exact)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SMB-Sparse-Zielkontext ist nicht vorbereitet",
            ));
        }
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| invalid("SMB-Sparse-Write benötigt eine eigene Tempbindung"))?;
        self.start_intent(path)?;
        let mut writer = Writer::new(self, path, false)?;
        self.bind_created_temp_identity(path, Ok(writer.inode))?;
        *lock(&file.sparse) = Some(writer.state.clone());
        writer.initialize()?;
        Ok(Box::new(writer))
    }
    pub(super) fn prepare_sparse(&self, source_acl: crate::metadata::AclModel) -> io::Result<()> {
        use crate::windows_dacl::{
            WindowsDaclTargetIssue as Issue, WindowsDaclWriteSupport as Support,
        };
        let windows = source_acl == crate::metadata::AclModel::WindowsSid;
        // Nur abgeschlossene, konkrete DACL-Absagen lassen den unabhängigen
        // Sparse-Weg zu. Unvorbereitete oder unklare Kontexte sind kein Beleg.
        let dacl = if windows {
            match self.dacl_support()? {
                Support::Ready(_) => true,
                Support::Unavailable(
                    Issue::RequiredAccessDenied
                    | Issue::SecurityUnsupported
                    | Issue::MissingOwner
                    | Issue::DifferentTempContext
                    | Issue::Normalized
                    | Issue::ReopenDenied,
                ) => false,
                Support::Unavailable(_) => {
                    *lock(&self.sparse_support) = Some(Capability::Unsupported);
                    return Ok(());
                }
            }
        } else {
            false
        };
        self.sparse_dacl.store(dacl, Ordering::SeqCst);
        // ADS können das Sparse-Attribut auch ohne DACL-SET entfernen. Dieser
        // Abschlussbedarf erteilt deshalb noch keinen breiteren DACL-Zugriff.
        self.sparse_metadata_restore
            .store(windows, Ordering::SeqCst);
        if !self.sync_metadata || !self.stream_target || self.owner.is_none() {
            return Ok(());
        }
        self.check(&self.root, false)?;
        let mut support = lock(&self.sparse_support);
        if let Some(current) = *support {
            return if current == Capability::Unknown {
                Err(crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    "Die Sparse-Probe dieses Kontexts ist fehlgeschlagen und wird nicht wiederholt",
                ))
            } else {
                Ok(())
            };
        }
        if self
            .provider
            .stream_metadata_capabilities(&self.root, false)?
            .sparse_extents
            != Capability::ReadOnly
        {
            *support = Some(Capability::Unsupported);
            return Ok(());
        }
        let path =
            self.staging_candidate(&self.root.join(".doppeldecker-sparse-probe"), "probe")?;
        let owner = self.owner.as_ref().expect("Laufowner geprüft");
        *support = Some(Capability::Unknown);
        let intent = crate::local_staging::RemoteTempIntent::create_bound_with_root(
            &owner.staging,
            &path,
            &path,
            &self.identity,
            self.provider.identity().account_digest,
        )?;
        // DELETE_ON_CLOSE gilt nur für die eigene Probe. Auch ein Abbruch kann
        // damit keine fremde Datei adressieren; ein unklarer CLOSE behält den Intent.
        let result = (|| {
            let mut writer = Writer::new(self, &path, true)?;
            writer.initialize()?;
            writer.set_len(PROBE_SIZE)?;
            writer.punch_hole(0, PROBE_SIZE)?;
            writer.prove_probe()?;
            writer.flush()
        })();
        let profile_path = Path::new("/").join(path.file_name().expect("Probename erzeugt"));
        let cleanup = self.check_namespace(&path, true).and_then(|()| {
            let mut errors = Vec::new();
            for candidate in [&path, &profile_path] {
                #[cfg(test)]
                let checked = {
                    PROBE_STATS.with(|value| value.set(value.get() + 1));
                    fault(if candidate == &path {
                        "probe-selected-stat"
                    } else {
                        "probe-profile-stat"
                    })
                };
                #[cfg(not(test))]
                let checked = Ok::<(), io::Error>(());
                let observed = checked.and_then(|()| self.provider.stat(candidate));
                match observed {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => errors.push(error),
                    Ok(_) => errors.push(io::Error::other(format!(
                        "SMB-Sparse-Probenname ist noch sichtbar: {}",
                        candidate.display()
                    ))),
                }
            }
            if result
                .as_ref()
                .is_err_and(|error| crate::error_code(error) == crate::ErrorCode::OutcomeUnclear)
            {
                errors.push(io::Error::other(
                    "Unklarer Sparse-Probenabschluss behält den Intent",
                ));
            }
            crate::remote::fold_cleanup_errors(errors)?;
            intent.finish()
        });
        if let Err(error) = cleanup {
            return Err(crate::error::with_recovery_context(
                crate::coded_error(
                    io::ErrorKind::Other,
                    crate::ErrorCode::OutcomeUnclear,
                    format!("SMB-Sparse-Probe blieb zur Prüfung erhalten: {error}"),
                ),
                &[&error],
                &[&path, intent.path()],
            ));
        }
        match result {
            Ok(()) => *support = Some(Capability::Exact),
            Err(error) if is_unsupported(&error) => *support = Some(Capability::Unsupported),
            Err(error) => return Err(error),
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static CLOSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FLUSHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[path = "smb_sparse_write_tests.rs"]
pub(super) mod tests;

/// Samba kann beim Rename das Sparse-Attribut trotz erhaltener Löcher entfernen.
/// Der vorab gewährte Schreib-/Attributzugriff bleibt ausschließlich am eigenen Objekt.
pub(super) struct CommitGuard {
    scope: OrdinarySmb,
    tree: Arc<Tree>,
    connection: Connection,
    id: Option<FileId>,
    inode: u64,
    path: PathBuf,
    parents: Vec<FileId>,
    metadata_flushed: bool,
}
async fn restore_sparse_attribute(
    connection: &mut Connection,
    tree: &Tree,
    id: FileId,
) -> io::Result<()> {
    let request = IoctlRequest {
        ctl_code: SET_SPARSE,
        file_id: id,
        max_input_response: 0,
        max_output_response: 0,
        flags: SMB2_0_IOCTL_IS_FSCTL,
        input_data: vec![1],
    };
    let frame = connection
        .execute(Command::Ioctl, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    status(&frame, Command::Ioctl)?;
    let response = IoctlResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
    if response.ctl_code != SET_SPARSE || response.file_id != id || !response.output_data.is_empty()
    {
        return Err(invalid(
            "Sparse-Abschluss erhielt einen fremden IOCTL-Abschluss",
        ));
    }
    Ok(())
}

impl CommitGuard {
    fn grant(scope: &OrdinarySmb, path: &Path, inode: u64) -> io::Result<Self> {
        scope.check(path, true)?;
        #[cfg(test)]
        fault("grant-before")?;
        let remote = scope.provider.remote_path(path)?;
        let (tree, connection) = {
            let state = lock(&scope.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let id = scope.provider.runtime.block_on(async {
            let request = CreateRequest {
                requested_oplock_level: OplockLevel::None,
                impersonation_level: ImpersonationLevel::Impersonation,
                desired_access: FileAccessMask::new(if scope.sparse_dacl.load(Ordering::SeqCst) {
                    // DELETE wird über einen frischen, namensgebundenen Handle
                    // geprüft. Ein hier gehaltener DELETE-Zugriff würde die
                    // unveränderten lesenden Basisbeobachtungen ausschließen.
                    wire::FILE_ACCESS & !FileAccessMask::DELETE
                } else {
                    FileAccessMask::FILE_WRITE_DATA
                        | FileAccessMask::FILE_WRITE_ATTRIBUTES
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE
                }),
                file_attributes: FILE_ATTRIBUTE_NORMAL,
                // Fremde Datenwriter bleiben ausgeschlossen. Nur die identitäts-
                // gebundene Tempbeobachtung teilt unseren eigenen FLUSH-Zugriff.
                share_access: ShareAccess(
                    ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_DELETE,
                ),
                create_disposition: CreateDisposition::FileOpen,
                create_options: FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
                name: remote,
                create_contexts: vec![],
            };
            let frame = connection
                .execute(Command::Create, &request, Some(tree.tree_id))
                .await
                .map_err(smb_error)?;
            require_status(&frame, Command::Create)?;
            Ok::<_, io::Error>(
                CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(smb_error)?
                    .file_id,
            )
        })?;
        let mut context = scope.clone();
        context.file = None;
        let mut value = Self {
            scope: context,
            tree,
            connection,
            id: Some(id),
            inode,
            path: path.into(),
            parents: vec![],
            metadata_flushed: false,
        };
        let proof = value.scope.provider.runtime.block_on(wire::identity(
            &mut value.connection,
            &value.tree,
            id,
            inode,
        ));
        if let Err(error) = proof {
            let close = value.close();
            return combine(Err(error), close, path);
        }
        Ok(value)
    }
    fn close(&mut self) -> io::Result<()> {
        let file = if let Some(id) = self.id.take() {
            #[cfg(test)]
            ATTRIBUTE_CLOSES.with(|value| value.set(value.get() + 1));
            self.scope.provider.runtime.block_on(async {
                #[cfg(test)]
                fault("attribute-close-before")?;
                self.tree
                    .close_handle(&mut self.connection, id)
                    .await
                    .map_err(smb_error)?;
                #[cfg(test)]
                fault("attribute-close-after")?;
                Ok(())
            })
        } else {
            Ok(())
        };
        let parents = self.scope.provider.runtime.block_on(wire::close_all(
            &mut self.connection,
            &self.tree,
            &mut self.parents,
        ));
        combine(file, parents, &self.path)
    }
    fn finish(&mut self, target: &Path) -> io::Result<()> {
        let result = (|| {
            self.scope.check(target, true)?;
            let entry = self.scope.provider.stat(target)?;
            if entry.inode != Some(self.inode) || !entry.is_file || entry.is_symlink {
                return Err(crate::real_mode::plan_drift(
                    "Sparse-Commit adressiert nicht die erzeugte Tempidentität",
                ));
            }
            let id = self
                .id
                .ok_or_else(|| invalid("Sparse-Commit-Handle wurde bereits verbraucht"))?;
            self.scope.provider.runtime.block_on(async {
                let before =
                    query_handle_entry(&mut self.connection, &self.tree, id, String::new()).await?;
                if before.file_id != self.inode || before.is_dir || before.is_symlink {
                    return Err(crate::real_mode::plan_drift(
                        "Sparse-Commit-Handle änderte seine Identität",
                    ));
                }
                #[cfg(test)]
                {
                    RESTORES.with(|value| value.set(value.get() + 1));
                    fault("restore-before")?;
                }
                restore_sparse_attribute(&mut self.connection, &self.tree, id).await?;
                #[cfg(test)]
                fault("restore-after")?;
                // Der vorab gewährte Daten-Schreibzugriff dient hier ausschließlich
                // dem FLUSH nach der Attributwiederherstellung.
                #[cfg(test)]
                fault("attribute-flush-before")?;
                flush_handle(&mut self.connection, &self.tree, id).await?;
                #[cfg(test)]
                fault("attribute-flush-after")?;
                let after =
                    query_handle_entry(&mut self.connection, &self.tree, id, String::new()).await?;
                if before.file_id != after.file_id
                    || before.size != after.size
                    || before.mtime != after.mtime
                {
                    return Err(crate::real_mode::plan_drift(
                        "Sparse-Commit änderte Identität, Größe oder Dateizeit",
                    ));
                }
                Ok(())
            })
        })();
        let close = self.close();
        combine(result, close, target)
    }
}
impl Drop for CommitGuard {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
impl OrdinarySmb {
    pub(super) fn sparse_dacl_handle(
        &self,
        path: &Path,
    ) -> io::Result<Option<(Arc<Tree>, Connection, FileId)>> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let Some(state) = lock(&file.sparse).clone() else {
            return Ok(None);
        };
        if !lock(&state).allows_commit()
            || file.attempted.load(Ordering::SeqCst)
            || file.settled.load(Ordering::SeqCst)
        {
            return Err(self.dacl_unclear(
                path,
                self.staging_recovery_path(),
                invalid("Gemeinsamer Sparse-/DACL-Temp ist nicht mehr für Metadaten freigegeben"),
            ));
        }
        let guard = lock(&file.sparse_commit);
        let guard = guard.as_ref().ok_or_else(|| {
            self.dacl_unclear(
                path,
                self.staging_recovery_path(),
                invalid("Gemeinsamer Sparse-/DACL-Abschlusshandle fehlt"),
            )
        })?;
        if !self.sparse_dacl.load(Ordering::SeqCst)
            || guard.metadata_flushed
            || guard.path != path
            || lock(&file.temp)
                .as_ref()
                .is_none_or(|(_, inode)| *inode != guard.inode)
        {
            return Err(invalid(
                "Gemeinsamer Sparse-/DACL-Handle ist nicht an diesen Temp gebunden",
            ));
        }
        let id = guard
            .id
            .ok_or_else(|| invalid("Sparse-/DACL-Handle wurde bereits verbraucht"))?;
        Ok(Some((guard.tree.clone(), guard.connection.clone(), id)))
    }
    pub(super) fn sparse_temp_mtime_identity(&self, path: &Path) -> io::Result<Option<u64>> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let Some(state) = lock(&file.sparse).clone() else {
            return Ok(None);
        };
        let guard = lock(&file.sparse_commit);
        if !lock(&state).allows_commit()
            || file.attempted.load(Ordering::SeqCst)
            || file.settled.load(Ordering::SeqCst)
            || guard
                .as_ref()
                .is_none_or(|guard| guard.path != path || guard.metadata_flushed)
        {
            return Err(self.dacl_unclear(
                path,
                self.staging_recovery_path(),
                invalid("Sparse-Dateizeit ist nach dem Metadatenabschluss nicht erneut schreibbar"),
            ));
        }
        Ok(Some(guard.as_ref().expect("Offener Guard geprüft").inode))
    }

    pub(super) fn sparse_manifest_identity(&self, path: &Path) -> Option<u64> {
        let file = self.file.as_ref()?;
        let owned = lock(&file.temp).clone()?;
        if owned.0 != path {
            return None;
        }
        let guard = lock(&file.sparse_commit);
        guard
            .as_ref()
            .filter(|guard| guard.id.is_some() && guard.inode == owned.1)
            .map(|guard| guard.inode)
    }
    pub(super) fn sync_sparse_metadata(&self, path: &Path) -> io::Result<bool> {
        let Some(file) = &self.file else {
            return Ok(false);
        };
        let Some(state) = lock(&file.sparse).clone() else {
            return Ok(false);
        };
        if !lock(&state).allows_commit() {
            return Err(self.dacl_unclear(
                path,
                self.staging_recovery_path(),
                invalid("Sparse-Metadatenabschluss ist fehlgeschlagen"),
            ));
        }
        let mut guard = lock(&file.sparse_commit)
            .take()
            .ok_or_else(|| invalid("Sparse-Metadatenabschluss ist bereits verbraucht"))?;
        let result = (|| {
            if guard.path != path {
                return Err(invalid(
                    "Sparse-Metadatenabschluss liegt auf einem anderen Temp",
                ));
            }
            self.check(path, true)?;
            if !guard.metadata_flushed {
                guard.metadata_flushed = true;
                let id = guard
                    .id
                    .ok_or_else(|| invalid("Sparse-Metadatenhandle ist verbraucht"))?;
                self.provider.runtime.block_on(async {
                    wire::identity(&mut guard.connection, &guard.tree, id, guard.inode).await?;
                    let before = if self.sparse_metadata_restore.load(Ordering::SeqCst) {
                        let before = query_handle_entry(
                            &mut guard.connection,
                            &guard.tree,
                            id,
                            String::new(),
                        )
                        .await?;
                        #[cfg(test)]
                        METADATA_RESTORES.with(|value| value.set(value.get() + 1));
                        #[cfg(test)]
                        fault("metadata-restore-before")?;
                        restore_sparse_attribute(&mut guard.connection, &guard.tree, id).await?;
                        #[cfg(test)]
                        fault("metadata-restore-after")?;
                        Some(before)
                    } else {
                        None
                    };
                    #[cfg(test)]
                    fault("metadata-flush-before")?;
                    flush_handle(&mut guard.connection, &guard.tree, id).await?;
                    #[cfg(test)]
                    fault("metadata-flush-after")?;
                    if let Some(before) = before {
                        let after = query_handle_entry(
                            &mut guard.connection,
                            &guard.tree,
                            id,
                            String::new(),
                        )
                        .await?;
                        if before.file_id != after.file_id
                            || before.size != after.size
                            || before.mtime != after.mtime
                        {
                            return Err(crate::real_mode::plan_drift(
                                "Sparse-Metadatenabschluss änderte Identität, Größe oder Dateizeit",
                            ));
                        }
                    }
                    Ok::<(), io::Error>(())
                })?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            *lock(&state) = State::Failed;
            let close = guard.close();
            return combine(Err(error), close, path)
                .map_err(|error| self.dacl_unclear(path, self.staging_recovery_path(), error));
        }
        *lock(&file.sparse_commit) = Some(guard);
        Ok(true)
    }
    pub(super) fn check_sparse_metadata_flushed(&self, path: &Path) -> io::Result<()> {
        if let Some(file) = &self.file {
            if lock(&file.sparse).is_some()
                && lock(&file.sparse_commit)
                    .as_ref()
                    .is_none_or(|guard| !guard.metadata_flushed)
            {
                return Err(self.dacl_unclear(
                    path,
                    self.staging_recovery_path(),
                    invalid("Sparse-Metadaten sind nicht abgeschlossen"),
                ));
            }
        }
        Ok(())
    }
    pub(super) fn close_sparse_commit(&self) -> io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let guard = lock(&file.sparse_commit).take();
        if let Some(mut guard) = guard {
            if let Err(error) = guard.close() {
                if let Some(state) = lock(&file.sparse).as_ref() {
                    *lock(state) = State::Failed;
                }
                return Err(self.dacl_unclear(&guard.path, self.staging_recovery_path(), error));
            }
        }
        Ok(())
    }
    pub(super) fn prepare_sparse_commit(&self) -> io::Result<()> {
        if let Some(file) = &self.file {
            if lock(&file.sparse).is_some() {
                file.intent
                    .get()
                    .ok_or_else(|| invalid("Sparse-Commit besitzt keinen Intent"))?
                    .smb_commit_pending()?;
            }
        }
        Ok(())
    }
    pub(super) fn reject_sparse_commit(&self, from: &Path) -> io::Result<()> {
        if let Some(file) = &self.file {
            if lock(&file.sparse).is_some() {
                if lock(&file.temp)
                    .as_ref()
                    .is_none_or(|(path, _)| path != from)
                {
                    return Err(invalid(
                        "Abgelehnter Sparse-Commit gehört nicht zum eigenen Temp",
                    ));
                }
                file.intent
                    .get()
                    .ok_or_else(|| invalid("Sparse-Commit besitzt keinen Intent"))?
                    .smb_commit_rejected(from)?;
            }
        }
        Ok(())
    }
    pub(super) fn finish_sparse_commit(&self, target: &Path) -> io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let guard = lock(&file.sparse_commit).take();
        if let Some(mut guard) = guard {
            if let Some(state) = lock(&file.sparse).as_ref() {
                *lock(state) = State::Failed;
            }
            if target != file.target {
                return Err(self.dacl_unclear(
                    target,
                    self.staging_recovery_path(),
                    invalid("Sparse-Commitziel weicht vom Stagingziel ab"),
                ));
            }
            guard
                .finish(target)
                .map_err(|error| self.dacl_unclear(target, self.staging_recovery_path(), error))?;
            if let Some(state) = lock(&file.sparse).as_ref() {
                *lock(state) = State::Complete;
            }
        }
        Ok(())
    }
}
#[cfg(test)]
thread_local! {
    static ATTRIBUTE_CLOSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static RESTORES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static METADATA_RESTORES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROBE_STATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
fn fault(name: &'static str) -> io::Result<()> {
    FAULT.with(|value| {
        if value.get() == Some(name) {
            value.set(None);
            Err(crate::coded_error(
                io::ErrorKind::ConnectionReset,
                crate::ErrorCode::ConnectionLost,
                format!("Künstlicher Sparse-Antwortverlust: {name}"),
            ))
        } else {
            Ok(())
        }
    })
}
