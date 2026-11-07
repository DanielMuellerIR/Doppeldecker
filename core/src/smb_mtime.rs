//! Interne mtime-Phasen. Ein möglicherweise gesendeter SET wird nie wiederholt.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MtimePhase {
    BeforeSet,
    SetUnclear,
    SetConfirmedReadbackUnclear,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeforeSetCause {
    Other,
    Open,
    OptionalIdentityObservation,
}
#[derive(Debug)]
pub(crate) struct MtimeFailure {
    pub(crate) phase: MtimePhase,
    pub(crate) before_cause: BeforeSetCause,
    pub(crate) resource_open: bool,
    open_access_denied: bool,
    pub(crate) close_failed: bool,
    error: io::Error,
}
impl MtimeFailure {
    fn before(error: io::Error) -> Self {
        Self {
            phase: MtimePhase::BeforeSet,
            before_cause: BeforeSetCause::Other,
            resource_open: false,
            open_access_denied: false,
            close_failed: false,
            error,
        }
    }
    fn sent(error: io::Error) -> Self {
        Self {
            phase: MtimePhase::SetUnclear,
            before_cause: BeforeSetCause::Other,
            resource_open: false,
            open_access_denied: false,
            close_failed: false,
            error,
        }
    }
    fn confirmed(error: io::Error) -> Self {
        Self {
            phase: MtimePhase::SetConfirmedReadbackUnclear,
            before_cause: BeforeSetCause::Other,
            resource_open: false,
            open_access_denied: false,
            close_failed: false,
            error,
        }
    }
    /// Nur gescheiterte Opens oder die zusätzliche ID-Beobachtung vor SET
    /// erlauben Aufgabe und normalen Schreibweg. Erkannte Drift tut dies nie.
    pub(crate) fn allows_receipt_release(&self) -> bool {
        self.phase == MtimePhase::BeforeSet
            && !matches!(
                crate::error_code(&self.error),
                crate::ErrorCode::Interrupted | crate::ErrorCode::PlanDrift
            )
            && self.error.kind() != io::ErrorKind::Interrupted
            && matches!(
                self.before_cause,
                BeforeSetCause::Open | BeforeSetCause::OptionalIdentityObservation
            )
    }
    /// Ein zweiter lesender Abschluss darf nur eine unklare Beobachtung klären.
    /// Bereits erkannte Objekt- oder Zeitabweichungen bleiben auch dann Fehler,
    /// wenn der Pfad beim nächsten Lesen wieder den erwarteten Zustand zeigt.
    pub(crate) fn allows_readback_retry(&self) -> bool {
        self.phase == MtimePhase::SetConfirmedReadbackUnclear
            && !self.close_failed
            && !matches!(
                crate::error_code(&self.error),
                crate::ErrorCode::PlanDrift | crate::ErrorCode::Interrupted
            )
            && !matches!(
                self.error.kind(),
                io::ErrorKind::InvalidInput
                    | io::ErrorKind::InvalidData
                    | io::ErrorKind::Interrupted
            )
    }
    // Nur die konkrete Protokollantwort des Attribut-OPEN beweist null SETs.
    // Die allgemeine PermissionDenied-Klasse enthält auch Auth-/Signingfehler.
    pub(crate) fn is_open_access_denied(&self) -> bool {
        self.phase == MtimePhase::BeforeSet
            && self.before_cause == BeforeSetCause::Open
            && self.open_access_denied
            && self.allows_receipt_release()
    }
    pub(crate) fn into_ordinary_io(self, path: &Path) -> io::Error {
        if self.is_open_access_denied() {
            return crate::content_sync::mtime_privilege_omission(
                "SMB hat WRITE_ATTRIBUTES beim Öffnen vor jedem Zeit-SET mit ACCESS_DENIED verweigert",
            );
        }
        let before_set = self.phase == MtimePhase::BeforeSet;
        let error = self.into_io();
        if before_set
            && (matches!(
                crate::error_code(&error),
                crate::ErrorCode::Interrupted | crate::ErrorCode::PlanDrift
            ) || matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::InvalidInput
            ))
        {
            return error;
        }
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::ErrorCode::OutcomeUnclear,
                format!("SMB-Zeitübernahme besitzt einen unklaren Abschluss: {error}"),
            ),
            &[&error],
            &[path],
        )
    }
    pub(crate) fn into_io(self) -> io::Error {
        // Der Ressourcenhinweis darf nie eine bereits mögliche SET-Wirkung
        // als gefahrlosen Openfehler erscheinen lassen. Alte Aufrufer erhalten
        // weiterhin unverändert den ursprünglichen I/O-Fehler.
        debug_assert!(!self.resource_open || self.allows_receipt_release());
        self.error
    }
}
struct OpenFailure {
    error: io::Error,
    resource: bool,
    access_denied: bool,
}
fn open_failure(error: smb2::Error) -> OpenFailure {
    let resource = error.status().is_some_and(resource_status);
    let access_denied = matches!(
        error,
        smb2::Error::Protocol {
            status: NtStatus::ACCESS_DENIED,
            command: Command::Create
        }
    );
    OpenFailure {
        error: smb_error(error),
        resource,
        access_denied,
    }
}
fn check_open_response(frame: &smb2::client::connection::Frame) -> Result<(), OpenFailure> {
    if frame.header.command != Command::Create {
        return Err(OpenFailure {
            error: invalid("SMB-mtime-OPEN erhielt eine Antwort auf einen anderen Befehl"),
            resource: false,
            access_denied: false,
        });
    }
    require_status(frame, Command::Create).map_err(|error| OpenFailure {
        error,
        resource: resource_status(frame.header.status),
        access_denied: frame.header.status == NtStatus::ACCESS_DENIED,
    })
}
trait MtimeWire {
    type Handle: Copy;
    async fn open(&mut self) -> Result<Self::Handle, OpenFailure>;
    async fn verify_identity(&mut self, handle: Self::Handle) -> io::Result<()>;
    async fn set(&mut self, handle: Self::Handle) -> io::Result<()>;
    async fn close(&mut self, handle: Self::Handle) -> io::Result<()>;
}
async fn set_once(wire: &mut impl MtimeWire) -> Result<(), MtimeFailure> {
    let handle = wire.open().await.map_err(|failure| MtimeFailure {
        phase: MtimePhase::BeforeSet,
        before_cause: BeforeSetCause::Open,
        resource_open: failure.resource,
        open_access_denied: failure.access_denied,
        close_failed: false,
        error: failure.error,
    })?;
    if let Err(error) = wire.verify_identity(handle).await {
        let closed = wire.close(handle).await;
        let optional = closed.is_ok()
            && !matches!(
                crate::error_code(&error),
                crate::ErrorCode::PlanDrift | crate::ErrorCode::Interrupted
            );
        let mut failure = MtimeFailure::before(error);
        if optional {
            failure.before_cause = BeforeSetCause::OptionalIdentityObservation;
        }
        return Err(failure);
    }
    let written = wire.set(handle).await;
    let closed = wire.close(handle).await;
    written.map_err(MtimeFailure::sent)?;
    closed.map_err(|error| {
        let mut failure = MtimeFailure::confirmed(error);
        failure.close_failed = true;
        failure
    })
}
struct Wire<'a> {
    connection: &'a mut Connection,
    tree: &'a Tree,
    remote: &'a str,
    raw_time: u64,
    expected_id: Option<u64>,
}
impl MtimeWire for Wire<'_> {
    type Handle = FileId;
    async fn open(&mut self) -> Result<FileId, OpenFailure> {
        #[cfg(test)]
        BEFORE_OPEN.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        #[cfg(test)]
        if take_fault(Fault::OpenAccessDenied) {
            OPEN_ACCESS_DENIED_PATHS.with(|paths| paths.borrow_mut().push(self.remote.into()));
            return Err(open_failure(smb2::Error::Protocol {
                status: NtStatus::ACCESS_DENIED,
                command: Command::Create,
            }));
        }
        #[cfg(test)]
        if take_fault(Fault::ResourceOpen) {
            return Err(open_failure(smb2::Error::Protocol {
                status: NtStatus::INSUFFICIENT_RESOURCES,
                command: Command::Create,
            }));
        }
        // Exakt dieselben Rechte und Shareflags wie der bisherige normale Helfer.
        let request = CreateRequest {
            requested_oplock_level: OplockLevel::None,
            impersonation_level: ImpersonationLevel::Impersonation,
            desired_access: FileAccessMask::new(
                FileAccessMask::FILE_WRITE_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
            ),
            file_attributes: FILE_ATTRIBUTE_NORMAL,
            share_access: ShareAccess(
                ShareAccess::FILE_SHARE_READ
                    | ShareAccess::FILE_SHARE_WRITE
                    | ShareAccess::FILE_SHARE_DELETE,
            ),
            create_disposition: CreateDisposition::FileOpen,
            create_options: FILE_OPEN_REPARSE_POINT,
            name: self.remote.into(),
            create_contexts: Vec::new(),
        };
        let frame = self
            .connection
            .execute(Command::Create, &request, Some(self.tree.tree_id))
            .await
            .map_err(|error| {
                let failure = open_failure(error);
                #[cfg(test)]
                if failure.access_denied {
                    OPEN_ACCESS_DENIED_PATHS
                        .with(|paths| paths.borrow_mut().push(self.remote.into()));
                }
                failure
            })?;
        let checked = check_open_response(&frame);
        #[cfg(test)]
        if checked.as_ref().is_err_and(|failure| failure.access_denied) {
            OPEN_ACCESS_DENIED_PATHS.with(|paths| paths.borrow_mut().push(self.remote.into()));
        }
        checked?;
        CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map(|response| response.file_id)
            .map_err(|error| OpenFailure {
                error: smb_error(error),
                resource: false,
                access_denied: false,
            })
    }
    async fn verify_identity(&mut self, handle: FileId) -> io::Result<()> {
        if let Some(expected) = self.expected_id {
            #[cfg(test)]
            if take_fault(Fault::IdentityRead) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Synthetische Absage der zusätzlichen Objekt-ID-Beobachtung vor SET",
                ));
            }
            let observed = query_info(
                self.connection,
                self.tree,
                handle,
                FILE_INTERNAL_INFORMATION,
                8,
            )
            .await?;
            if read_u64(&observed, 0)? != expected {
                return Err(crate::real_mode::plan_drift(
                    "mtime-Ziel ist nicht mehr das original erzeugte Verzeichnis",
                ));
            }
        }
        Ok(())
    }
    async fn set(&mut self, handle: FileId) -> io::Result<()> {
        let mut buffer = vec![0; 40];
        buffer[16..24].copy_from_slice(&self.raw_time.to_le_bytes());
        let request = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: FILE_BASIC_INFORMATION,
            additional_information: 0,
            file_id: handle,
            buffer,
        };
        #[cfg(test)]
        {
            SET_COUNT.with(|count| count.set(count.get() + 1));
            SET_PATHS.with(|paths| paths.borrow_mut().push(self.remote.into()));
        }
        let frame = self
            .connection
            .execute(Command::SetInfo, &request, Some(self.tree.tree_id))
            .await
            .map_err(smb_error)?;
        require_status(&frame, Command::SetInfo)?;
        #[cfg(test)]
        if take_fault(Fault::AfterSet) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Synthetischer Antwortverlust nach tatsächlichem mtime-SET",
            ));
        }
        Ok(())
    }
    async fn close(&mut self, handle: FileId) -> io::Result<()> {
        self.tree
            .close_handle(self.connection, handle)
            .await
            .map_err(smb_error)
    }
}
fn resource_status(status: NtStatus) -> bool {
    matches!(
        status,
        NtStatus::INSUFFICIENT_RESOURCES | NtStatus::INSUFF_SERVER_RESOURCES
    )
}
impl SmbVfs {
    pub(crate) fn set_mtime_phased(
        &self,
        path: &Path,
        mtime: i64,
        expected_id: Option<u64>,
    ) -> Result<(), MtimeFailure> {
        let remote = self.remote_path(path).map_err(MtimeFailure::before)?;
        let system_time = if mtime >= 0 {
            UNIX_EPOCH + Duration::from_secs(mtime as u64)
        } else {
            UNIX_EPOCH
                .checked_sub(Duration::from_secs(mtime.unsigned_abs()))
                .ok_or_else(|| {
                    MtimeFailure::before(invalid("SMB-mtime liegt außerhalb des Zeitbereichs"))
                })?
        };
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(set_once(&mut Wire {
            connection: &mut state.session.connection,
            tree: &tree,
            remote: &remote,
            raw_time: FileTime::from_system_time(system_time).0,
            expected_id,
        }))?;
        drop(state);
        #[cfg(test)]
        if take_fault(Fault::Readback) {
            return Err(MtimeFailure::confirmed(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "Synthetischer rein lesender mtime-Abschlussfehler",
            )));
        }
        #[cfg(test)]
        BEFORE_READBACK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook(path);
            }
        });
        let readback = self.verify_mtime_readback(path, mtime, expected_id);
        #[cfg(test)]
        AFTER_READBACK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook(&readback);
            }
        });
        readback.map_err(MtimeFailure::confirmed)
    }
    /// Ausschließlich lesend; auch nach Aufgabe eines CREATE-Receipts kein SET.
    pub(crate) fn verify_mtime_readback(
        &self,
        path: &Path,
        mtime: i64,
        expected_id: Option<u64>,
    ) -> io::Result<()> {
        let observed = self.stat_unchecked(path)?;
        if expected_id.is_some_and(|expected| expected != observed.file_id) {
            return Err(crate::real_mode::plan_drift(
                "mtime-Readback besitzt eine fremde Objekt-ID",
            ));
        }
        if observed.mtime != Some(mtime) {
            return Err(invalid("SMB-mtime-Readback stimmt nicht exakt überein"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Fault {
    OpenAccessDenied,
    ResourceOpen,
    IdentityRead,
    AfterSet,
    Readback,
}
#[cfg(test)]
type ReadbackHook = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
type ReadbackResultHook = Box<dyn FnOnce(&io::Result<()>)>;
#[cfg(test)]
thread_local! {
    pub(super) static BEFORE_READBACK:std::cell::RefCell<Option<ReadbackHook>>=const {std::cell::RefCell::new(None)};
    pub(super) static AFTER_READBACK:std::cell::RefCell<Option<ReadbackResultHook>>=const {std::cell::RefCell::new(None)};
    pub(super) static BEFORE_OPEN:std::cell::RefCell<Option<Box<dyn FnOnce()>>>=const {std::cell::RefCell::new(None)};
    pub(super) static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) };
    pub(super) static OPEN_ACCESS_DENIED_PATHS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(super) static SET_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static SET_PATHS:std::cell::RefCell<Vec<String>>=const{std::cell::RefCell::new(Vec::new())};
}
#[cfg(test)]
fn take_fault(expected: Fault) -> bool {
    FAULT.with(|fault| {
        if fault.get() == Some(expected) {
            fault.set(None);
            true
        } else {
            false
        }
    })
}
#[cfg(test)]
#[path = "smb_mtime_tests.rs"]
mod tests;
