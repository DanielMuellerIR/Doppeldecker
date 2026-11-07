//! Vollständige lesende Bereiche und gehaltener Seek-Reader; keine SMB-Schreibfähigkeit.
use super::*;
use crate::metadata::{DataExtent, SparseReadIssue};
use crate::vfs::SparseRead;
use smb2::msg::header::ErrorResponse;
use smb2::msg::ioctl::{IoctlRequest, SMB2_0_IOCTL_IS_FSCTL};
use std::io::{Seek, SeekFrom};

const FSCTL_QUERY_ALLOCATED_RANGES: u32 = 0x0009_40cf;
const RESPONSE_LIMIT: u32 = 64 * 1024;
const EXTENT_LIMIT: usize = 65_536;
const PAGE_LIMIT: usize = 1024;
const BUFFER_TOO_SMALL: u32 = 0xc000_0023;
const BUFFER_OVERFLOW: u32 = 0x8000_0005;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::smb) struct Observation {
    pub extents: Option<Vec<DataExtent>>,
    pub issue: Option<SparseReadIssue>,
}
impl Observation {
    fn omitted(issue: SparseReadIssue) -> Self {
        Self {
            extents: None,
            issue: Some(issue),
        }
    }
    fn complete(extents: Vec<DataExtent>) -> Self {
        Self {
            extents: Some(extents),
            issue: None,
        }
    }
}

struct RangeScan {
    size: u64,
    start: u64,
    output_limit: u32,
    pages: usize,
    extents: Vec<DataExtent>,
}
impl RangeScan {
    fn new(size: u64) -> io::Result<Self> {
        if size > i64::MAX as u64 {
            return Err(invalid("SMB-Sparse-Dateigröße liegt außerhalb signed64"));
        }
        Ok(Self {
            size,
            start: 0,
            output_limit: 4096,
            pages: 0,
            extents: vec![],
        })
    }
    fn accept(
        &mut self,
        command: Command,
        status: NtStatus,
        body: &[u8],
        id: FileId,
    ) -> Result<Option<Observation>, SparseReadIssue> {
        use SparseReadIssue as I;
        self.pages += 1;
        if self.pages > PAGE_LIMIT {
            return Err(I::BufferLimit);
        }
        if command != Command::Ioctl {
            return Err(I::InvalidResponse);
        }
        if status.0 == BUFFER_TOO_SMALL {
            ErrorResponse::unpack(&mut ReadCursor::new(body)).map_err(|_| I::InvalidResponse)?;
            if self.output_limit >= RESPONSE_LIMIT {
                return Err(I::BufferLimit);
            }
            self.output_limit = (self.output_limit * 2).min(RESPONSE_LIMIT);
            return Ok(None);
        }
        if status != NtStatus::SUCCESS && status.0 != BUFFER_OVERFLOW {
            let issue = I::ServerStatus(status.0);
            issue.validate().map_err(|_| I::InvalidResponse)?;
            ErrorResponse::unpack(&mut ReadCursor::new(body)).map_err(|_| I::InvalidResponse)?;
            return Err(issue);
        }
        let ranges = parse_ranges(body, id, self.output_limit, self.start, self.size)
            .map_err(|_| I::InvalidResponse)?;
        let mut next = self.start;
        for extent in ranges {
            let end = extent.offset + extent.length;
            if extent.offset < next {
                return Err(I::InvalidResponse);
            }
            if let Some(last) = self
                .extents
                .last_mut()
                .filter(|last| last.offset + last.length == extent.offset)
            {
                last.length = end - last.offset;
            } else {
                self.extents.push(extent);
            }
            if self.extents.len() > EXTENT_LIMIT {
                return Err(I::BufferLimit);
            }
            next = end;
        }
        if status == NtStatus::SUCCESS {
            return Ok(Some(Observation::complete(std::mem::take(
                &mut self.extents,
            ))));
        }
        if next <= self.start || next >= self.size {
            return Err(I::InvalidResponse);
        }
        self.start = next;
        Ok(None)
    }
}

fn parse_ranges(
    body: &[u8],
    id: FileId,
    limit: u32,
    query_start: u64,
    size: u64,
) -> io::Result<Vec<DataExtent>> {
    if body.len() < 48
        || u16::from_le_bytes([body[0], body[1]]) != 49
        || read_u32(body, 4)? != FSCTL_QUERY_ALLOCATED_RANGES
        || read_u64(body, 8)? != id.persistent
        || read_u64(body, 16)? != id.volatile
        || read_u32(body, 28)? != 0
        || read_u32(body, 40)? != 0
    {
        return Err(invalid("Ungültiger SMB-Sparse-IOCTL-Header"));
    }
    let count = read_u32(body, 36)?;
    if count > limit || !count.is_multiple_of(16) {
        return Err(invalid("Ungültige SMB-Sparse-Antwortlänge"));
    }
    if count == 0 {
        return Ok(vec![]);
    }
    // OutputOffset zählt ab SMB-Headerbeginn. Der Vendordecoder ignoriert ihn;
    // hier darf weder Padding noch ein fremder Header als Bereich gelesen werden.
    let offset = read_u32(body, 32)?
        .checked_sub(64)
        .filter(|offset| *offset >= 48)
        .ok_or_else(|| invalid("SMB-Sparse-Ausgabe liegt im Header"))? as usize;
    let bytes = body
        .get(
            offset
                ..offset
                    .checked_add(count as usize)
                    .ok_or_else(|| invalid("SMB-Sparse-Puffer läuft über"))?,
        )
        .ok_or_else(|| invalid("Abgeschnittene SMB-Sparse-Ausgabe"))?;
    let mut ranges = Vec::new();
    let mut previous = query_start;
    for chunk in bytes.chunks_exact(16) {
        let start = read_u64(chunk, 0)?;
        let length = read_u64(chunk, 8)?;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= i64::MAX as u64)
            .ok_or_else(|| invalid("SMB-Sparse-Bereich läuft über signed64"))?;
        if start > i64::MAX as u64 || length == 0 || end <= query_start || start >= size {
            return Err(invalid("SMB-Sparse-Bereich schneidet die Anfrage nicht"));
        }
        // MS-FSCC verlangt eine Schnittmenge, nicht auf allen Servern bereits
        // exakt beschnittene Clustergrenzen. Nur der logische Teil wird kopiert.
        let start = start.max(query_start);
        let end = end.min(size);
        if start < previous {
            return Err(invalid("Überlappende oder ungeordnete SMB-Sparse-Bereiche"));
        }
        ranges.push(DataExtent {
            offset: start,
            length: end - start,
        });
        previous = end;
    }
    Ok(ranges)
}

pub(in crate::smb) async fn observe(
    connection: &mut Connection,
    tree: &Tree,
    id: FileId,
    size: u64,
    cancel: Option<&crate::Cancel>,
) -> io::Result<Observation> {
    if size == 0 {
        return Ok(Observation::complete(vec![]));
    }
    #[cfg(test)]
    if let Some(extents) = TEST_EXTENTS.with(|value| value.borrow().clone()) {
        return Ok(Observation::complete(extents));
    }
    #[cfg(test)]
    if let Some(issue) = TEST_ISSUE.with(|value| value.get()) {
        return Ok(Observation::omitted(issue));
    }
    let mut scan = RangeScan::new(size)?;
    loop {
        check_cancel(cancel)?;
        if scan.pages >= PAGE_LIMIT {
            return Ok(Observation::omitted(SparseReadIssue::BufferLimit));
        }
        let mut input = Vec::with_capacity(16);
        input.extend_from_slice(&scan.start.to_le_bytes());
        input.extend_from_slice(&(size - scan.start).to_le_bytes());
        let request = IoctlRequest {
            ctl_code: FSCTL_QUERY_ALLOCATED_RANGES,
            file_id: id,
            max_input_response: 0,
            max_output_response: scan.output_limit,
            flags: SMB2_0_IOCTL_IS_FSCTL,
            input_data: input,
        };
        let frame = connection
            .execute(Command::Ioctl, &request, Some(tree.tree_id))
            .await
            .map_err(smb_error)?;
        if frame.header.command == Command::Ioctl
            && frame.header.status != NtStatus::SUCCESS
            && !matches!(
                frame.header.status.0,
                BUFFER_TOO_SMALL
                    | BUFFER_OVERFLOW
                    | 0xc000_0002
                    | 0xc000_0003
                    | 0xc000_0010
                    | 0xc000_000d
                    | 0xc000_0022
                    | 0xc000_00bb
            )
        {
            require_status(&frame, Command::Ioctl)?;
        }
        match scan.accept(frame.header.command, frame.header.status, &frame.body, id) {
            Ok(Some(observation)) => return Ok(observation),
            Ok(None) => {}
            Err(issue) => return Ok(Observation::omitted(issue)),
        }
    }
}

pub(super) fn open(
    provider: &SmbVfs,
    path: &Path,
    root: Option<&Path>,
    cancel: crate::Cancel,
) -> io::Result<Box<dyn SparseRead>> {
    check_cancel(Some(&cancel))?;
    let (remote, scope) = stream_binding_scope(&provider.canonical_root(), path, root)?;
    let (tree, mut connection, max_read) = {
        let state = lock(&provider.state);
        (
            state.session.tree.clone(),
            state.session.connection.clone(),
            state
                .session
                .connection
                .params()
                .map(|p| p.max_read_size.min(MAX_IO_CHUNK))
                .unwrap_or(64 * 1024)
                .max(1),
        )
    };
    let (base, before, mut parents) = provider.runtime.block_on(open_bound_base(
        &mut connection,
        &tree,
        &remote,
        StreamBaseKind::File,
        false,
        &scope,
    ))?;
    if let Err(error) = RangeScan::new(before.size) {
        let _ = provider
            .runtime
            .block_on(tree.close_handle(&mut connection, base));
        let _ = provider
            .runtime
            .block_on(close_parents(&mut connection, &tree, &mut parents));
        return Err(error);
    }
    Ok(Box::new(SparseReader {
        inner: SmbRead {
            runtime: provider.runtime.clone(),
            tree,
            connection,
            file_id: base,
            size: before.size,
            offset: 0,
            max_read,
            closed: false,
        },
        before,
        parents,
        cancel,
        state: ReaderState::Active,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReaderState {
    Active,
    Complete,
    Failed,
}
struct SparseReader {
    inner: SmbRead,
    before: SmbListedEntry,
    parents: Vec<FileId>,
    cancel: crate::Cancel,
    state: ReaderState,
}
impl SparseReader {
    fn active(&self) -> io::Result<()> {
        if self.state != ReaderState::Active {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SMB-Sparse-Reader ist abgeschlossen",
            ));
        }
        check_cancel(Some(&self.cancel))
    }
    fn close(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if !self.inner.closed {
            TEST_CLOSE_CALLS.with(|value| value.set(value.get() + 1));
        }
        let file = self.inner.close();
        let parents = self.inner.runtime.block_on(close_parents(
            &mut self.inner.connection,
            &self.inner.tree,
            &mut self.parents,
        ));
        file.and(parents)
    }
}
impl Read for SparseReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.active()?;
        // Gewöhnlicher SmbRead schließt bei EOF. Seek-fähige Quellen behalten
        // das Handle bis finish, auch bei leeren oder vollständig gelochten Dateien.
        if buffer.is_empty() || self.inner.offset >= self.inner.size {
            return Ok(0);
        }
        #[cfg(test)]
        if TEST_FINISH_FAULT.with(|value| value.get()) == Some("read") {
            return Err(io::Error::other("Künstlicher Sparse-READ-Fehler"));
        }
        let count = self.inner.read(buffer)?;
        #[cfg(test)]
        TEST_READ_BYTES.with(|value| value.set(value.get() + count as u64));
        Ok(count)
    }
}
pub(in crate::smb) fn seek_position(
    current: u64,
    size: u64,
    position: SeekFrom,
) -> io::Result<u64> {
    let value = match position {
        SeekFrom::Start(value) => i128::from(value),
        SeekFrom::Current(delta) => i128::from(current) + i128::from(delta),
        SeekFrom::End(delta) => i128::from(size) + i128::from(delta),
    };
    if !(0..=i128::from(i64::MAX)).contains(&value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SMB-Sparse-Seek liegt außerhalb signed64",
        ));
    }
    Ok(value as u64)
}
impl Seek for SparseReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.active()?;
        self.inner.offset = seek_position(self.inner.offset, self.inner.size, position)?;
        Ok(self.inner.offset)
    }
}
impl SparseRead for SparseReader {
    fn finish(&mut self) -> io::Result<()> {
        if self.state == ReaderState::Complete {
            return Ok(());
        }
        if self.state != ReaderState::Active {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SMB-Sparse-Reader ist fehlgeschlagen",
            ));
        }
        self.state = ReaderState::Failed;
        let proof = check_cancel(Some(&self.cancel)).and_then(|()| {
            let after = self.inner.runtime.block_on(query_handle_entry(
                &mut self.inner.connection,
                &self.inner.tree,
                self.inner.file_id,
                String::new(),
            ))?;
            if same_manifest_base(&self.before, &after) {
                Ok(())
            } else {
                Err(crate::real_mode::plan_drift(
                    "SMB-Sparse-Quelle änderte sich am gehaltenen Handle",
                ))
            }
        });
        let close = self.close();
        #[cfg(test)]
        let close = close.and_then(|()| {
            if TEST_FINISH_FAULT.with(|value| value.get()) == Some("close") {
                Err(io::Error::other(
                    "Künstlicher verlorener Sparse-CLOSE-Abschluss",
                ))
            } else {
                Ok(())
            }
        });
        proof.and(close)?;
        self.state = ReaderState::Complete;
        Ok(())
    }
}
impl Drop for SparseReader {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_EXTENTS:std::cell::RefCell<Option<Vec<DataExtent>>>=const {std::cell::RefCell::new(None)};
    pub(crate) static TEST_ISSUE:std::cell::Cell<Option<SparseReadIssue>>=const {std::cell::Cell::new(None)};
    pub(crate) static TEST_FINISH_FAULT:std::cell::Cell<Option<&'static str>>=const {std::cell::Cell::new(None)};
    pub(crate) static TEST_READ_BYTES:std::cell::Cell<u64>=const {std::cell::Cell::new(0)};
    pub(crate) static TEST_CLOSE_CALLS:std::cell::Cell<u64>=const {std::cell::Cell::new(0)};
}

#[cfg(test)]
#[path = "smb_sparse_tests.rs"]
mod tests;
