//! Begrenzte interaktive Inhaltslesewege; ausdrücklich kein Sync-VFS.
//!
//! SFTP v3 kann den ersten OPEN nicht atomar unter einen Root binden. Wir halten
//! deshalb Sitzung und Datei-Handle, prüfen alle beobachtbaren Pfadbelege und lesen
//! den Inhalt vor EOF erneut zurück. Ein unbemerkter Austausch mit identischen
//! Attributen zwischen diesen Requests bleibt das sichtbar erklärte Restrisiko.

use super::*;
use crate::error::{coded_error, ErrorCode};
use crate::Cancel;
use russh_sftp::protocol::OpenFlags;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[path = "sftp_tree.rs"]
mod tree;
use tree::DirectoryProof;

pub const SFTP_MAX_INTERACTIVE_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 100;
const READ_CHUNK: u32 = 64 * 1024;
pub const SFTP_CONTENT_READ_LIMITATION: &str = "SFTP-Inhalte · Best Effort: Hostkey, Sitzung, Pfadkomponenten, READDIR bis EOF, Datei-Handles, Größe und BLAKE3-Readback werden geprüft. SFTP v3 bindet OPENDIR und OPEN nicht atomar an den Pane-Root; ein unbemerkter Pfadtausch zwischen den Prüfungen bleibt möglich. Reguläre Dateien bis 8 GiB und begrenzte additive Verzeichnisbäume: höchstens 100 direkte Auswahleinträge, 1000 Baumeinträge, 16 Unterordnerebenen und 16 GiB Baumgesamtinhalt. Symlinks und nicht belegbare Inhalte werden sichtbar ausgelassen; keine Ersetzung oder Quellentfernung. Ausführungsrechte werden nicht übernommen.";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct Attributes {
    size: Option<u64>,
    mtime: Option<u32>,
    permissions: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
}
impl From<&FileAttributes> for Attributes {
    fn from(value: &FileAttributes) -> Self {
        Self {
            size: value.size,
            mtime: value.mtime,
            permissions: value.permissions,
            uid: value.uid,
            gid: value.gid,
        }
    }
}

impl Attributes {
    fn agrees_with(&self, fresh: &Self) -> bool {
        self.size.is_none_or(|value| Some(value) == fresh.size)
            && self.mtime.is_none_or(|value| Some(value) == fresh.mtime)
            && self
                .permissions
                .is_none_or(|value| Some(value) == fresh.permissions)
            && self.uid.is_none_or(|value| Some(value) == fresh.uid)
            && self.gid.is_none_or(|value| Some(value) == fresh.gid)
    }
}

#[derive(Clone)]
struct FileProof {
    path: PathBuf,
    remote: String,
    chain: Vec<(String, Attributes)>,
    attributes: Attributes,
    entry: Entry,
}

/// Der Typ ist privat und besitzt weder Root-Identitäten noch vollständige
/// Verzeichnislisten. Selbst eine versehentliche Weitergabe an den Planner kann
/// deshalb keine strikten Sync-/Mirrorbelege oder Schreibrechte erzeugen.
#[derive(Clone)]
struct InteractiveFiles {
    provider: Arc<SftpVfs>,
    identity: SftpServerIdentity,
    generation: u64,
    files: Arc<BTreeMap<PathBuf, FileProof>>,
    directories: Arc<BTreeMap<PathBuf, DirectoryProof>>,
    tree: Option<crate::vfs::InteractiveSourceTree>,
    cancel: Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadPurpose {
    Contents,
    Metadata,
}

pub(super) struct MetadataTreeSummary {
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub omitted: Vec<crate::vfs::SourceReadOmission>,
}

impl SftpVfs {
    pub(crate) fn interactive_files(
        self: &Arc<Self>,
        root: &Path,
        items: &[PathBuf],
        cancel: &Cancel,
    ) -> io::Result<Arc<dyn Vfs>> {
        let (mut context, summary) =
            self.prepare_interactive_context(root, items, cancel, ReadPurpose::Contents)?;
        context.tree = summary
            .map(tree::TreeSummary::into_interactive)
            .transpose()?;
        Ok(Arc::new(context))
    }

    // Der Metadatenkontext besitzt keine Inhaltsgrößenlimits. Er bleibt deshalb
    // privat und darf niemals als VFS-Kopierquelle oder Queue-Baumbeleg herausgehen.
    pub(super) fn interactive_metadata_summary(
        self: &Arc<Self>,
        root: &Path,
        path: &Path,
        cancel: &Cancel,
    ) -> io::Result<MetadataTreeSummary> {
        let (context, summary) = self.prepare_interactive_context(
            root,
            &[path.to_path_buf()],
            cancel,
            ReadPurpose::Metadata,
        )?;
        if !context.directories.contains_key(path) {
            return Err(coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Ordnergröße benötigt einen echten Ordner",
            ));
        }
        let entry = context.stat(path)?;
        if !entry.is_dir || entry.is_symlink {
            return Err(coded_error(
                io::ErrorKind::InvalidInput,
                ErrorCode::InvalidInput,
                "Ordnergröße benötigt einen echten Ordner",
            ));
        }
        let summary = summary.ok_or_else(|| invalid("SFTP-Ordnerbeleg fehlt"))?;
        context.check_session()?;
        Ok(MetadataTreeSummary {
            bytes: summary.bytes,
            files: summary.files,
            directories: summary.directories,
            omitted: summary.omitted,
        })
    }

    fn prepare_interactive_context(
        self: &Arc<Self>,
        root: &Path,
        items: &[PathBuf],
        cancel: &Cancel,
        purpose: ReadPurpose,
    ) -> io::Result<(InteractiveFiles, Option<tree::TreeSummary>)> {
        validate_interactive_content_selection(root, items)?;
        let root_components = virtual_components(root)?;
        check_cancel(cancel)?;
        // Ausschließlich ein neuer Leseversuch darf eine verlorene Sitzung neu
        // aufbauen. Ein bereits teilweise lokal schreibender Versuch wird niemals
        // wiederholt. Gleichzeitige neue Leser lösen höchstens einen Reconnect aus.
        let reconnect = self.reconnect_lost_interactive_read();
        check_cancel(cancel)?;
        reconnect?;
        let state = lock(&self.state);
        let identity = state.connection.identity.clone();
        let generation = state.generation;
        let session = state.connection.sftp.clone();
        let raw = state.connection.read_session.clone();
        drop(state);
        let mut result = InteractiveFiles {
            provider: self.clone(),
            identity,
            generation,
            files: Arc::new(BTreeMap::new()),
            directories: Arc::new(BTreeMap::new()),
            tree: None,
            cancel: cancel.clone(),
        };
        let preparation_cancel = cancel.until(std::time::Instant::now() + tree::PREPARATION_LIMIT);
        let prepared = self.runtime.block_on(async {
            verify_sftp_directory_chain(
                &session,
                &result.identity.canonical_root,
                &root_components,
                Some(&preparation_cancel),
            )
            .await?;
            tree::prepare(
                &result,
                &session,
                &raw,
                root,
                items,
                &preparation_cancel,
                purpose,
            )
            .await
        });
        let prepared = limited_result(
            prepared,
            &preparation_cancel,
            cancel,
            "SFTP-Baumvorbereitung überschritt die Frist von 60 Sekunden; keine lokale Wirkung",
        );
        // Auch ein Ausfall vor dem ersten OPEN gehört zum gescheiterten Versuch.
        // Ein sekundärer CLOSE-Verlust wird am gehaltenen Kontext separat erfasst.
        let (files, directories, tree) = result.record_failure(prepared)?;
        result.files = Arc::new(files);
        result.directories = Arc::new(directories);
        result.check_session()?;
        Ok((result, tree))
    }
}

pub fn validate_interactive_content_selection(root: &Path, items: &[PathBuf]) -> io::Result<()> {
    validate_content_path(root)?;
    if !root.is_absolute() || items.is_empty() || items.len() > MAX_FILES {
        return Err(invalid(
            "SFTP-Inhaltsauswahl benötigt einen absoluten Pane-Root und 1 bis 100 direkte Einträge",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for path in items {
        validate_content_path(path)?;
        if !path.is_absolute() || path.parent() != Some(root) || path == root || !seen.insert(path)
        {
            return Err(invalid(
                "SFTP-Inhalte benötigen eindeutige direkte Einträge des gewählten Pane-Roots",
            ));
        }
    }
    Ok(())
}

fn validate_content_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().len() > 4096 || virtual_components(path)?.len() > 64 {
        return Err(invalid(
            "SFTP-Inhaltspfad überschreitet 4096 Bytes oder 64 Komponenten",
        ));
    }
    Ok(())
}

async fn capture_file(
    session: &SftpSession,
    canonical_root: &str,
    path: &Path,
    cancel: &Cancel,
) -> io::Result<FileProof> {
    capture_node(session, canonical_root, path, cancel, false).await
}

async fn capture_node(
    session: &SftpSession,
    canonical_root: &str,
    path: &Path,
    cancel: &Cancel,
    allow_directory: bool,
) -> io::Result<FileProof> {
    capture_node_for(
        session,
        canonical_root,
        path,
        cancel,
        allow_directory,
        ReadPurpose::Contents,
    )
    .await
}

async fn capture_node_for(
    session: &SftpSession,
    canonical_root: &str,
    path: &Path,
    cancel: &Cancel,
    allow_directory: bool,
    purpose: ReadPurpose,
) -> io::Result<FileProof> {
    let components = virtual_components(path)?;
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| invalid("SFTP-Dateiname fehlt"))?;
    let parent =
        verify_sftp_directory_chain(session, canonical_root, parents, Some(cancel)).await?;
    let mut chain = Vec::new();
    let mut current = canonical_root.to_owned();
    chain.push((
        current.clone(),
        Attributes::from(
            &session
                .symlink_metadata(current.clone())
                .await
                .map_err(sftp_error)?,
        ),
    ));
    check_cancel(cancel)?;
    for component in parents {
        check_cancel(cancel)?;
        current = append_sftp_component(&current, component);
        chain.push((
            current.clone(),
            Attributes::from(
                &session
                    .symlink_metadata(current.clone())
                    .await
                    .map_err(sftp_error)?,
            ),
        ));
    }
    check_cancel(cancel)?;
    let remote = append_sftp_component(&parent, name);
    let attrs = session
        .symlink_metadata(remote.clone())
        .await
        .map_err(sftp_error)?;
    check_cancel(cancel)?;
    if !(attrs.is_regular() || (allow_directory && attrs.is_dir()))
        || attrs.is_symlink()
        || attrs.permissions.is_none()
        || (attrs.is_regular()
            && (attrs.size.is_none()
                || (purpose == ReadPurpose::Contents && attrs.mtime.is_none())))
    {
        let message = if purpose == ReadPurpose::Contents {
            "SFTP-Inhalt benötigt eine reguläre Datei mit Typ, Größe und Änderungszeit oder einen belegten echten Ordner; Symlinks und Spezialdateien werden nicht übertragen"
        } else {
            "SFTP-Größenbeobachtung benötigt belegten Dateityp und Dateigröße; Symlinks, Spezialdateien und nicht belegbare Größen werden ausgelassen"
        };
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            message,
        ));
    }
    if purpose == ReadPurpose::Contents
        && attrs.is_regular()
        && attrs.size.unwrap_or(u64::MAX) > SFTP_MAX_INTERACTIVE_FILE_BYTES
    {
        return Err(invalid(
            "SFTP-Einzeldatei überschreitet die Inhaltsgrenze von 8 GiB",
        ));
    }
    if session
        .canonicalize(remote.clone())
        .await
        .map_err(sftp_error)?
        != remote
    {
        return Err(drift("SFTP-Dateipfad wechselte seine REALPATH-Bindung"));
    }
    check_cancel(cancel)?;
    let mut entry = entry_from_attributes((*name).to_owned(), &attrs)?;
    // Inhaltsdownload ist keine native Metadatenübertragung, insbesondere keine
    // Freigabe für ein vom Server geliefertes Programm oder Sonderbits.
    entry.mode = Some(if entry.is_dir { 0o700 } else { 0o600 });
    Ok(FileProof {
        path: path.to_path_buf(),
        remote,
        chain,
        attributes: Attributes::from(&attrs),
        entry,
    })
}

fn limited_result<T>(
    result: io::Result<T>,
    limited: &Cancel,
    original: &Cancel,
    message: &str,
) -> io::Result<T> {
    result.map_err(|error| {
        if crate::error_code(&error) == ErrorCode::Interrupted
            && limited.is_cancelled()
            && !original.is_cancelled()
        {
            invalid(message)
        } else {
            error
        }
    })
}

fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    check_sftp_cancel(Some(cancel))
}

fn drift(message: &str) -> io::Error {
    coded_error(io::ErrorKind::InvalidData, ErrorCode::PlanDrift, message)
}
fn read_only<T>() -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Die begrenzte SFTP-Inhaltsklasse ist ausschließlich eine additive Lesequelle; strikte Listings und Remote-Mutationen bleiben gesperrt"))
}

impl InteractiveFiles {
    fn record_failure<T>(&self, result: io::Result<T>) -> io::Result<T> {
        self.provider
            .record_interactive_read_failure(self.generation, result)
    }

    fn proof(&self, path: &Path) -> io::Result<&FileProof> {
        check_cancel(&self.cancel)?;
        self.files
            .get(path)
            .ok_or_else(|| invalid("Pfad gehört nicht zur gehaltenen SFTP-Inhaltsauswahl"))
    }
    fn check_session(&self) -> io::Result<()> {
        check_cancel(&self.cancel)?;
        let state = lock(&self.provider.state);
        if state.generation != self.generation || state.connection.identity != self.identity {
            return Err(drift(
                "SFTP-Sitzung oder Serveridentität änderte sich; Inhalt frisch vorbereiten",
            ));
        }
        Ok(())
    }
    async fn verify(&self, proof: &FileProof, session: &SftpSession) -> io::Result<()> {
        self.check_session()?;
        let fresh = capture_file(
            session,
            &self.identity.canonical_root,
            &proof.path,
            &self.cancel,
        )
        .await?;
        if fresh.chain != proof.chain
            || fresh.attributes != proof.attributes
            || fresh.remote != proof.remote
        {
            return Err(drift("SFTP-Root, Elternordner oder Dateiattribute änderten sich während des Inhaltslesens"));
        }
        self.check_session()
    }
}

struct ContentReader {
    files: Arc<InteractiveFiles>,
    proof: FileProof,
    session: Arc<SftpSession>,
    raw: Arc<RawSftpSession>,
    handle: String,
    offset: u64,
    hash: blake3::Hasher,
    verified: bool,
    failed: bool,
}

impl ContentReader {
    async fn verify_handle(&self, handle: &str) -> io::Result<()> {
        let attrs = self.raw.fstat(handle).await.map_err(sftp_error)?.attrs;
        if Attributes::from(&attrs) != self.proof.attributes {
            return Err(drift(
                "SFTP-Datei-Handle stimmt nicht mit den gebundenen Attributen überein",
            ));
        }
        Ok(())
    }
    async fn read_at(&self, handle: &str, offset: u64, length: u32) -> io::Result<Vec<u8>> {
        check_cancel(&self.files.cancel)?;
        match self.raw.read(handle, offset, length).await {
            Ok(data) if !data.data.is_empty() && data.data.len() <= length as usize => {
                Ok(data.data)
            }
            Ok(_) => Err(drift(
                "SFTP-READ lieferte eine leere oder zu große Datenantwort",
            )),
            Err(russh_sftp::client::error::Error::Status(status))
                if status.status_code == StatusCode::Eof =>
            {
                Ok(Vec::new())
            }
            Err(error) => Err(sftp_error(error)),
        }
    }
    async fn finish(&self) -> io::Result<()> {
        self.files.verify(&self.proof, &self.session).await?;
        self.verify_handle(&self.handle).await?;
        // Ein frischer Handle zum gleichen geprüften Pfad erkennt auch einen
        // Austausch mit gleicher Größe/Zeit, den FSTAT allein nicht unterscheiden kann.
        let second = self
            .raw
            .open(&self.proof.remote, OpenFlags::READ, FileAttributes::empty())
            .await
            .map_err(sftp_error)?
            .handle;
        let result = async {
            self.verify_handle(&second).await?;
            let mut offset = 0;
            let mut hash = blake3::Hasher::new();
            loop {
                let remaining = self.proof.entry.size.saturating_sub(offset);
                let data = self
                    .read_at(
                        &second,
                        offset,
                        u64::from(READ_CHUNK).min(remaining.saturating_add(1)) as u32,
                    )
                    .await?;
                if data.is_empty() {
                    break;
                }
                offset += data.len() as u64;
                if offset > self.proof.entry.size {
                    return Err(drift("SFTP-Inhalt wuchs beim BLAKE3-Readback"));
                }
                hash.update(&data);
            }
            if offset != self.proof.entry.size || hash.finalize() != self.hash.finalize() {
                return Err(drift(
                    "SFTP-Inhalt änderte sich beim vollständigen BLAKE3-Readback",
                ));
            }
            self.verify_handle(&second).await?;
            self.verify_handle(&self.handle).await?;
            self.files.verify(&self.proof, &self.session).await
        }
        .await;
        // Der primäre Inhaltsfehler bleibt führend. Ein zusätzlich gemeldeter
        // Verbindungsverlust muss die Sitzung trotzdem vor dem nächsten Leser entwerten.
        let closed = self
            .files
            .record_failure(self.raw.close(second).await.map_err(sftp_error));
        result?;
        closed?;
        Ok(())
    }
}
impl Read for ContentReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(drift(
                "SFTP-Inhaltsstrom ist nach einem Fehler nicht wiederverwendbar",
            ));
        }
        if self.verified {
            return Ok(0);
        }
        let result = (|| {
            self.files.check_session()?;
            let remaining = self.proof.entry.size.saturating_sub(self.offset);
            let length = (buffer.len() as u64)
                .min(u64::from(READ_CHUNK))
                .min(remaining.saturating_add(1)) as u32;
            let data = self.files.provider.runtime.block_on(self.read_at(
                &self.handle,
                self.offset,
                length,
            ))?;
            if data.is_empty() {
                if self.offset != self.proof.entry.size {
                    return Err(drift(
                        "SFTP-Inhalt wurde vor der erwarteten Größe abgeschnitten",
                    ));
                }
                self.files.provider.runtime.block_on(self.finish())?;
                let handle = std::mem::take(&mut self.handle);
                self.files
                    .provider
                    .runtime
                    .block_on(self.raw.close(handle))
                    .map_err(sftp_error)?;
                self.verified = true;
                return Ok(0);
            }
            if self.offset + data.len() as u64 > self.proof.entry.size {
                return Err(drift("SFTP-Inhalt überschreitet die gebundene Größe"));
            }
            buffer[..data.len()].copy_from_slice(&data);
            self.hash.update(&data);
            self.offset += data.len() as u64;
            Ok(data.len())
        })();
        let result = self.files.record_failure(result);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl Drop for ContentReader {
    fn drop(&mut self) {
        if !self.handle.is_empty() {
            let result = self
                .files
                .provider
                .runtime
                .block_on(self.raw.close(std::mem::take(&mut self.handle)))
                .map_err(sftp_error);
            let _ = self.files.record_failure(result);
        }
    }
}

impl Vfs for InteractiveFiles {
    fn interactive_additive_only(&self) -> bool {
        true
    }
    fn supports_mode_write(&self) -> bool {
        false
    }
    fn supports_mtime_write(&self) -> bool {
        false
    }
    fn file_flush_guarantees_at(&self, _: &Path) -> io::Result<(bool, bool)> {
        Ok((false, false))
    }
    fn interactive_regular_file_source(&self, path: &Path) -> io::Result<Option<Entry>> {
        self.stat(path).map(|entry| entry.is_file.then_some(entry))
    }
    fn interactive_copy_children(
        &self,
        path: &Path,
    ) -> io::Result<Option<Vec<crate::vfs::ProofListEntry>>> {
        self.check_session()?;
        let Some(proof) = self.directories.get(path) else {
            return Ok(None);
        };
        let (session, raw) = {
            let state = lock(&self.provider.state);
            (
                state.connection.sftp.clone(),
                state.connection.read_session.clone(),
            )
        };
        self.record_failure(
            self.provider
                .runtime
                .block_on(tree::verify_directory(self, proof, &session, &raw)),
        )?;
        Ok(Some(
            proof
                .children
                .iter()
                .map(|path| {
                    let entry = self
                        .files
                        .get(path)
                        .map(|file| &file.entry)
                        .or_else(|| self.directories.get(path).map(|dir| &dir.node.entry))
                        .expect("Baumvorbereitung bindet jedes übertragbare Kind")
                        .clone();
                    crate::vfs::ProofListEntry {
                        raw_name: entry.name.as_bytes().to_vec(),
                        entry,
                        safely_decoded: true,
                        provider_token: None,
                    }
                })
                .collect(),
        ))
    }
    fn interactive_source_tree(&self) -> io::Result<Option<crate::vfs::InteractiveSourceTree>> {
        self.check_session()?;
        Ok(self.tree.clone())
    }
    fn list(&self, _: &Path) -> io::Result<Vec<Entry>> {
        read_only()
    }
    fn stat(&self, path: &Path) -> io::Result<Entry> {
        if let Some(directory) = self.directories.get(path) {
            self.check_session()?;
            let session = lock(&self.provider.state).connection.sftp.clone();
            let fresh = self.record_failure(self.provider.runtime.block_on(capture_node(
                &session,
                &self.identity.canonical_root,
                path,
                &self.cancel,
                true,
            )))?;
            if fresh.attributes != directory.node.attributes || fresh.chain != directory.node.chain
            {
                return Err(drift(
                    "SFTP-Ordner oder Elternattribute änderten sich seit der Baumvorbereitung",
                ));
            }
            self.check_session()?;
            return Ok(directory.node.entry.clone());
        }
        let proof = self.proof(path)?;
        let session = lock(&self.provider.state).connection.sftp.clone();
        self.record_failure(self.provider.runtime.block_on(self.verify(proof, &session)))?;
        Ok(proof.entry.clone())
    }
    fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let proof = self.proof(path)?.clone();
        self.check_session()?;
        let (session, raw) = {
            let state = lock(&self.provider.state);
            (
                state.connection.sftp.clone(),
                state.connection.read_session.clone(),
            )
        };
        self.record_failure(
            self.provider
                .runtime
                .block_on(self.verify(&proof, &session)),
        )?;
        let opened = self
            .provider
            .runtime
            .block_on(raw.open(&proof.remote, OpenFlags::READ, FileAttributes::empty()))
            .map_err(sftp_error);
        let handle = self.record_failure(opened)?.handle;
        let reader = ContentReader {
            files: Arc::new(self.clone()),
            proof,
            session,
            raw,
            handle,
            offset: 0,
            hash: blake3::Hasher::new(),
            verified: false,
            failed: false,
        };
        self.record_failure(
            self.provider
                .runtime
                .block_on(reader.verify_handle(&reader.handle)),
        )?;
        self.record_failure(
            self.provider
                .runtime
                .block_on(self.verify(&reader.proof, &reader.session)),
        )?;
        Ok(Box::new(reader))
    }
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        read_only()
    }
    fn create_dir(&self, _: &Path) -> io::Result<()> {
        read_only()
    }
    fn open_write(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        read_only()
    }
    fn open_write_new(&self, _: &Path) -> io::Result<Box<dyn Write + Send>> {
        read_only()
    }
    fn sync_file(&self, _: &Path) -> io::Result<()> {
        read_only()
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        read_only()
    }
    fn set_mtime(&self, _: &Path, _: i64) -> io::Result<()> {
        read_only()
    }
    fn set_mode(&self, _: &Path, _: u32) -> io::Result<()> {
        read_only()
    }
    fn remove_file(&self, _: &Path) -> io::Result<()> {
        read_only()
    }
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        read_only()
    }
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        read_only()
    }
    fn rename_noreplace(&self, _: &Path, _: &Path) -> io::Result<()> {
        read_only()
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn vorbereitungsfrist_erhaelt_verbindungsfehler_und_setzt_nicht_den_nutzerabbruch() {
        let original = Cancel::new();
        let expired = original.until(std::time::Instant::now());
        let lost = limited_result::<()>(
            Err(coded_error(
                io::ErrorKind::ConnectionAborted,
                ErrorCode::ConnectionLost,
                "künstlicher Antwortverlust",
            )),
            &expired,
            &original,
            "Frist",
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&lost), ErrorCode::ConnectionLost);
        let stopped = limited_result::<()>(
            Err(io::Error::new(io::ErrorKind::Interrupted, "Frist")),
            &expired,
            &original,
            "Vorbereitungsfrist",
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&stopped), ErrorCode::InvalidInput);
        assert!(!original.is_cancelled());
        original.cancel();
        let stopped = limited_result::<()>(
            Err(io::Error::new(io::ErrorKind::Interrupted, "Nutzerabbruch")),
            &expired,
            &original,
            "Vorbereitungsfrist",
        )
        .unwrap_err();
        assert_eq!(crate::error_code(&stopped), ErrorCode::Interrupted);
    }
}
