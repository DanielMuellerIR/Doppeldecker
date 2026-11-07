//! SMB-Datenströme bleiben eine separate Namensklasse. Nur gebundene neue Temps
//! dürfen beschrieben werden; die allgemeine Pfadabbildung akzeptiert keinen Colon.
use super::*;
use crate::metadata::{BlobSummary, MetadataManifest, NamedBlobSummary};
use std::collections::BTreeMap;

#[path = "smb_directory_streams.rs"]
mod directory_streams;

#[path = "smb_sparse.rs"]
pub(super) mod sparse;

const FILE_STREAM_INFORMATION: u8 = 22;
const FILE_ACCESS_INFORMATION: u8 = 8;
const FILE_FS_ATTRIBUTE_INFORMATION: u8 = 5;
const FILE_NAMED_STREAMS: u32 = 0x0004_0000;
const FILE_SUPPORTS_SPARSE_FILES: u32 = 0x40;
const STREAM_INFO_LIMIT: u32 = 64 * 1024;

fn stream_name(name: &[u8]) -> io::Result<&str> {
    crate::validate_named_stream_name(name)?;
    let name = std::str::from_utf8(name).map_err(|_| invalid("SMB-Streamname ist kein UTF-8"))?;
    if name.is_empty() || name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '\0')) {
        return Err(invalid("Ungültiger separater SMB-Streamname"));
    }
    Ok(name)
}

pub(crate) fn lookup_key(name: &str) -> String {
    // Konservativ: Auch Unicode-Expansionen werden als mögliche serverseitige
    // Lookupkollision behandelt, statt zwei bestätigte Streams gleich zu adressieren.
    name.to_uppercase().to_lowercase()
}

fn stream_path(base: &str, name: &[u8]) -> io::Result<String> {
    let name = stream_name(name)?;
    if base.is_empty() || base.contains(':') {
        return Err(invalid(
            "SMB-Stream benötigt einen separat geprüften Basispfad",
        ));
    }
    let path = format!("{base}:{name}:$DATA");
    if path.encode_utf16().count() > MAX_PATH_UTF16_UNITS {
        return Err(invalid("SMB-Streampfad ist zu lang"));
    }
    Ok(path)
}

/// STATUS_SUCCESS ist bereits Voraussetzung. Insbesondere ist ein vollständiger
/// erster Eintrag bei STATUS_BUFFER_OVERFLOW kein vollständiger Streambestand.
fn parse_streams(bytes: &[u8]) -> io::Result<BTreeMap<Vec<u8>, u64>> {
    let mut entries = BTreeMap::new();
    let mut lookups = BTreeSet::new();
    let mut default_seen = false;
    let mut offset = 0usize;
    loop {
        let remaining = bytes
            .get(offset..)
            .ok_or_else(|| invalid("SMB-Streamoffset außerhalb Puffer"))?;
        if remaining.len() < 24 {
            return Err(invalid("Unvollständiger SMB-Streamheader"));
        }
        let next = read_u32(remaining, 0)? as usize;
        let length = read_u32(remaining, 4)? as usize;
        let size = read_u64(remaining, 8)?;
        let allocation = read_u64(remaining, 16)?;
        let end = 24usize
            .checked_add(length)
            .ok_or_else(|| invalid("SMB-Streamlänge läuft über"))?;
        if !length.is_multiple_of(2) || size > i64::MAX as u64 || allocation > i64::MAX as u64 {
            return Err(invalid("Ungültige SMB-Streamlänge oder negative Größe"));
        }
        let name_bytes = remaining
            .get(24..end)
            .ok_or_else(|| invalid("Abgeschnittener SMB-Streamname"))?;
        let units: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let name =
            String::from_utf16(&units).map_err(|_| invalid("Ungültiges SMB-Stream-UTF16"))?;
        let inner = name
            .strip_prefix(':')
            .and_then(|n| n.strip_suffix(":$DATA"))
            .ok_or_else(|| invalid("Ungültige SMB-Streamnamensyntax"))?;
        if inner.is_empty() {
            if default_seen {
                return Err(invalid("Doppelter SMB-Hauptstream"));
            }
            default_seen = true;
        } else {
            stream_name(inner.as_bytes())?;
            if !lookups.insert(lookup_key(inner))
                || entries.insert(inner.as_bytes().to_vec(), size).is_some()
            {
                return Err(invalid("Kollidierende SMB-Streamnamen"));
            }
        }
        if next == 0 {
            // MS-FSCC erlaubt beliebige Alignmentbytes. Am letzten Eintrag kann
            // höchstens dessen nächste 8-Byte-Grenze Padding enthalten.
            if remaining.len()
                > end
                    .checked_add((8 - end % 8) % 8)
                    .ok_or_else(|| invalid("SMB-Streampadding läuft über"))?
            {
                return Err(invalid("Ungebundene Bytes hinter letztem SMB-Stream"));
            }
            return Ok(entries);
        }
        if !next.is_multiple_of(8) || next < end || next >= remaining.len() {
            return Err(invalid("Ungültiger SMB-NextEntryOffset"));
        }
        offset = offset
            .checked_add(next)
            .ok_or_else(|| invalid("SMB-Streamoffset läuft über"))?;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamBaseKind {
    File,
    Directory,
}

impl StreamBaseKind {
    fn of(entry: &SmbListedEntry) -> Self {
        if entry.is_dir {
            Self::Directory
        } else {
            Self::File
        }
    }

    fn open_options(self) -> u32 {
        (match self {
            Self::File => FILE_NON_DIRECTORY_FILE,
            Self::Directory => FILE_DIRECTORY_FILE,
        }) | FILE_OPEN_REPARSE_POINT
    }
}

fn parse_streams_for_base(
    bytes: &[u8],
    kind: StreamBaseKind,
) -> io::Result<BTreeMap<Vec<u8>, u64>> {
    // Anders als reguläre Dateien besitzen Ordner keinen unbenannten $DATA-
    // Hauptstream. Nur ihre erfolgreiche vollständige Klassenantwort darf leer sein.
    if kind == StreamBaseKind::Directory && bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    parse_streams(bytes)
}

#[derive(Clone, Debug)]
enum StreamBindingScope {
    EntireShare,
    OrdinaryRoot(String),
}

impl StreamBindingScope {
    fn requires_directory_read(&self, remote: &str) -> bool {
        match self {
            Self::EntireShare => true,
            Self::OrdinaryRoot(root) => {
                root.is_empty()
                    || remote == root
                    || remote
                        .strip_prefix(root)
                        .is_some_and(|suffix| suffix.starts_with('\\'))
            }
        }
    }
}

fn stream_binding_scope(
    canonical_root: &str,
    path: &Path,
    ordinary_root: Option<&Path>,
) -> io::Result<(String, StreamBindingScope)> {
    // Die Scopeentscheidung muss vor dem ersten Serverzugriff fallen. Der Colon
    // bleibt ausschließlich Teil der separat geprüften internen Streamadresse.
    let checked = |path: &Path| -> io::Result<String> {
        if !path.is_absolute() {
            return Err(invalid(
                "SMB-Streambasis benötigt einen absoluten virtuellen Pfad",
            ));
        }
        let normalized = normalize_virtual_path(path)?;
        if normalized.contains(':') {
            return Err(invalid("SMB-Streambasis enthält einen Doppelpunkt"));
        }
        Ok(normalized)
    };
    let path = checked(path)?;
    let remote = map_virtual_path(canonical_root, Path::new(&path))?;
    if remote.contains(':') {
        return Err(invalid("SMB-Profilroot enthält einen Doppelpunkt"));
    }
    let scope = if let Some(root) = ordinary_root {
        let root = checked(root)?;
        if !Path::new(&path).starts_with(Path::new(&root)) {
            return Err(invalid(
                "SMB-Streamwirkung liegt außerhalb des gewählten Roots",
            ));
        }
        StreamBindingScope::OrdinaryRoot(map_virtual_path(canonical_root, Path::new(&root))?)
    } else {
        StreamBindingScope::EntireShare
    };
    Ok((remote, scope))
}

async fn open_base(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    kind: StreamBaseKind,
    write_stream: bool,
) -> io::Result<(FileId, SmbListedEntry)> {
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(
            FileAccessMask::FILE_READ_DATA
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
        ),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        // Der gehaltene Basishandle verhindert einen Rename/Unlink. Während der
        // Beobachtung schließt er zusätzlich konkurrierende Schreibhandles aus.
        share_access: ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | if write_stream {
                    ShareAccess::FILE_SHARE_WRITE
                } else {
                    0
                },
        ),
        create_disposition: CreateDisposition::FileOpen,
        create_options: kind.open_options(),
        name: path.to_owned(),
        create_contexts: Vec::new(),
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::Create)?;
    let opened = CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
    let result = query_handle_entry(connection, tree, opened.file_id, String::new())
        .await
        .and_then(|entry| {
            if StreamBaseKind::of(&entry) != kind || entry.is_symlink || entry.file_id == 0 {
                Err(invalid(
                    "SMB-Streambasis entspricht nicht dem gebundenen Eintragstyp",
                ))
            } else {
                Ok(entry)
            }
        });
    match result {
        Ok(entry) => Ok((opened.file_id, entry)),
        Err(error) => {
            let _ = tree.close_handle(connection, opened.file_id).await;
            Err(error)
        }
    }
}

async fn close_parents(
    connection: &mut Connection,
    tree: &Tree,
    parents: &mut Vec<FileId>,
) -> io::Result<()> {
    let mut failure = None;
    while let Some(id) = parents.pop() {
        if let Err(error) = tree.close_handle(connection, id).await.map_err(smb_error) {
            if failure.is_none() {
                failure = Some(error);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

async fn open_bound_base(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    kind: StreamBaseKind,
    write_stream: bool,
    scope: &StreamBindingScope,
) -> io::Result<(FileId, SmbListedEntry, Vec<FileId>)> {
    let components: Vec<_> = path.split('\\').collect();
    let mut parents = Vec::new();
    let result = async {
        for count in 0..components.len() {
            let remote_parent = components[..count].join("\\");
            let read_data = scope.requires_directory_read(&remote_parent);
            let request = CreateRequest {
                requested_oplock_level: OplockLevel::None,
                impersonation_level: ImpersonationLevel::Impersonation,
                desired_access: FileAccessMask::new(
                    FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE
                        | if read_data {
                            FileAccessMask::FILE_READ_DATA
                        } else {
                            0
                        },
                ),
                file_attributes: FILE_ATTRIBUTE_NORMAL,
                // Nur echte LIST_DIRECTORY/READ_DATA-Öffnungen erzwingen auf
                // Samba die fehlende Deletefreigabe. Oberhalb eines gewöhnlichen
                // Pane-Roots bleiben Attribute-only-Handles Identitätszeugen;
                // dort gelten weiterhin die frischen Best-Effort-Rootprüfungen.
                share_access: ShareAccess(
                    ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_WRITE,
                ),
                create_disposition: CreateDisposition::FileOpen,
                create_options: FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
                name: remote_parent,
                create_contexts: Vec::new(),
            };
            let frame = connection
                .execute(Command::Create, &request, Some(tree.tree_id))
                .await
                .map_err(smb_error)?;
            require_status(&frame, Command::Create)?;
            let opened =
                CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
            parents.push(opened.file_id);
            let entry = query_handle_entry(connection, tree, opened.file_id, String::new()).await?;
            if !entry.is_dir || entry.is_symlink || entry.file_id == 0 {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Streamelternpfad ist kein gebundener echter Ordner",
                ));
            }
        }
        open_base(connection, tree, path, kind, write_stream).await
    }
    .await;
    match result {
        Ok((base, entry)) => Ok((base, entry, parents)),
        Err(error) => {
            let _ = close_parents(connection, tree, &mut parents).await;
            Err(error)
        }
    }
}

fn same_base(before: &SmbListedEntry, after: &SmbListedEntry) -> bool {
    before.is_dir == after.is_dir
        && !after.is_symlink
        && before.file_id == after.file_id
        && before.size == after.size
        && before.change_time == after.change_time
}

fn same_manifest_base(before: &SmbListedEntry, after: &SmbListedEntry) -> bool {
    // Nur der vollständige Metadatenbeleg bindet die gemeldete Linkzahl. Reine
    // Inhalts-/DACL-Belege dürfen daraus keine neue Alias-Exklusivität ableiten.
    same_base(before, after) && (before.is_dir || before.hard_links == after.hard_links)
}

#[derive(Debug)]
struct DirectoryStreamQueryUnsupported(NtStatus);

impl std::fmt::Display for DirectoryStreamQueryUnsupported {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "SMB-Server unterstützt FileStreamInformation (Klasse 22) für diese typisierte Verzeichnisbasis nicht: NTSTATUS {:#010x}", self.0.0)
    }
}
impl std::error::Error for DirectoryStreamQueryUnsupported {}

fn directory_query_unsupported(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<DirectoryStreamQueryUnsupported>())
}

fn decode_stream_list_response(
    frame: &smb2::client::connection::Frame,
    kind: StreamBaseKind,
) -> io::Result<BTreeMap<Vec<u8>, u64>> {
    if frame.header.command != Command::QueryInfo {
        return Err(invalid(
            "SMB-Streamabfrage erhielt einen anderen Antworttyp",
        ));
    }
    // MS-FSA FileStreamInformation nennt INVALID_INFO_CLASS für fehlende ADS-
    // Unterstützung. NOT_SUPPORTED/INVALID_DEVICE_REQUEST lehnen die konkrete
    // Abfrage ebenfalls ausdrücklich ab. INVALID_PARAMETER ist mehrdeutig und
    // bleibt ebenso wie Rechte-, Sitzungs-, I/O- und Pufferfehler ein echter Fehler.
    if kind == StreamBaseKind::Directory
        && matches!(
            frame.header.status.0,
            0xc000_0003 | 0xc000_0010 | 0xc000_00bb
        )
    {
        smb2::msg::header::ErrorResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(smb_error)?;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            DirectoryStreamQueryUnsupported(frame.header.status),
        ));
    }
    require_status(frame, Command::QueryInfo)?;
    let response =
        QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
    parse_streams_for_base(&response.output_buffer, kind)
}

async fn stream_list(
    connection: &mut Connection,
    tree: &Tree,
    base: FileId,
    kind: StreamBaseKind,
) -> io::Result<BTreeMap<Vec<u8>, u64>> {
    let request = QueryInfoRequest {
        info_type: InfoType::File,
        file_info_class: FILE_STREAM_INFORMATION,
        output_buffer_length: STREAM_INFO_LIMIT,
        additional_information: 0,
        flags: 0,
        file_id: base,
        input_buffer: Vec::new(),
    };
    let frame = connection
        .execute(Command::QueryInfo, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    decode_stream_list_response(&frame, kind)
}

fn finish_stream_manifest(
    result: io::Result<MetadataManifest>,
    close: io::Result<()>,
    unobserved: MetadataManifest,
) -> io::Result<MetadataManifest> {
    match result {
        Ok(manifest) => close.map(|()| manifest),
        Err(error) if directory_query_unsupported(&error) => {
            // Fehlende Verzeichnisabfrage ist unbeobachtet, niemals beobachtet
            // leer. Auch diese Lesewirkung muss aber erfolgreich geschlossen sein.
            close?;
            Ok(unobserved)
        }
        Err(error) => Err(error),
    }
}

fn check_cancel(cancel: Option<&crate::Cancel>) -> io::Result<()> {
    if cancel.is_some_and(crate::Cancel::is_cancelled) {
        return Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            crate::ErrorCode::Interrupted,
            "SMB-Streambeobachtung abgebrochen",
        ));
    }
    Ok(())
}

async fn open_stream_handle(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    access: u32,
    disposition: CreateDisposition,
) -> io::Result<CreateResponse> {
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(access),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        // Stream-Sharing gilt unabhängig vom Hauptstream. Auch der konkrete
        // Stream bleibt daher während READ/WRITE gegen fremde Schreiber gebunden.
        share_access: ShareAccess(ShareAccess::FILE_SHARE_READ),
        create_disposition: disposition,
        create_options: FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        name: path.to_owned(),
        create_contexts: Vec::new(),
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::Create)?;
    CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)
}

/// Der exklusive Stream kann während des gesamten Schreibens nicht erneut am
/// Namen geöffnet werden. Auch nach Rename liest ausschließlich dieses Handle.
pub(in crate::smb) async fn hash_stream_handle(
    connection: &mut Connection,
    tree: &Tree,
    id: FileId,
    size: u64,
    max_read: u32,
    cancel: Option<&crate::Cancel>,
) -> io::Result<BlobSummary> {
    let standard = query_info(connection, tree, id, FILE_STANDARD_INFORMATION, 24).await?;
    if read_u64(&standard, 8)? != size {
        return Err(crate::real_mode::plan_drift(
            "SMB-Streamgröße stimmt nicht überein",
        ));
    }
    let mut hash = blake3::Hasher::new();
    let mut offset = 0;
    while offset < size {
        check_cancel(cancel)?;
        let length = (size - offset).min(u64::from(max_read)) as u32;
        let request = ReadRequest {
            padding: 0x50,
            flags: 0,
            length,
            offset,
            file_id: id,
            minimum_count: 0,
            channel: SMB2_CHANNEL_NONE,
            remaining_bytes: 0,
            read_channel_info: Vec::new(),
        };
        let frame = connection
            .execute_with_credits(
                Command::Read,
                &request,
                Some(tree.tree_id),
                CreditCharge(u64::from(length).div_ceil(65_536).max(1) as u16),
            )
            .await
            .map_err(smb_error)?;
        require_status(&frame, Command::Read)?;
        let response =
            ReadResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
        if response.data.is_empty() || response.data.len() > length as usize {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Unvollständiger SMB-Streamreadback",
            ));
        }
        offset += response.data.len() as u64;
        hash.update(&response.data);
    }
    let standard = query_info(connection, tree, id, FILE_STANDARD_INFORMATION, 24).await?;
    if read_u64(&standard, 8)? != size {
        return Err(crate::real_mode::plan_drift(
            "SMB-Streamgröße änderte sich beim Readback",
        ));
    }
    Ok(BlobSummary {
        size,
        digest: *hash.finalize().as_bytes(),
    })
}

/// Hauptinhalt und ADS am gehaltenen Basisobjekt vor und nach einer DACL-Wirkung.
/// Namen werden zweimal abgefragt; der offene Basis- und Elternhandle verhindert
/// einen unbemerkten Namenstausch, aber ausdrücklich keine neue Hardlinkanlage.
pub(super) async fn file_dacl_content_proof(
    connection: &mut Connection,
    tree: &Tree,
    base: FileId,
    remote: &str,
    size: u64,
    cancel: Option<&crate::Cancel>,
) -> io::Result<(BlobSummary, Vec<NamedBlobSummary>)> {
    let max_read = connection
        .params()
        .map(|p| p.max_read_size.min(MAX_IO_CHUNK))
        .unwrap_or(65536)
        .max(1);
    let before = query_handle_entry(connection, tree, base, String::new()).await?;
    let names = stream_list(connection, tree, base, StreamBaseKind::File).await?;
    let content = hash_stream_handle(connection, tree, base, size, max_read, cancel).await?;
    let mut streams = Vec::new();
    for (name, size) in &names {
        let value = hash_stream(
            connection,
            tree,
            &stream_path(remote, name)?,
            *size,
            max_read,
            cancel,
        )
        .await?;
        streams.push(NamedBlobSummary {
            name: name.clone(),
            value,
        });
    }
    let after = query_handle_entry(connection, tree, base, String::new()).await?;
    if !same_base(&before, &after)
        || names != stream_list(connection, tree, base, StreamBaseKind::File).await?
    {
        return Err(crate::real_mode::plan_drift(
            "SMB-Datei oder ADS änderten sich beim DACL-Inhaltsbeleg",
        ));
    }
    Ok((content, streams))
}

async fn hash_stream(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    size: u64,
    max_read: u32,
    cancel: Option<&crate::Cancel>,
) -> io::Result<BlobSummary> {
    check_cancel(cancel)?;
    let opened = open_stream_handle(
        connection,
        tree,
        path,
        FileAccessMask::FILE_READ_DATA
            | FileAccessMask::FILE_READ_ATTRIBUTES
            | FileAccessMask::SYNCHRONIZE,
        CreateDisposition::FileOpen,
    )
    .await?;
    let result = async {
        if opened.end_of_file != size
            || opened.file_attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                != 0
        {
            return Err(crate::real_mode::plan_drift(
                "SMB-Streamgröße oder Typ änderte sich",
            ));
        }
        hash_stream_handle(connection, tree, opened.file_id, size, max_read, cancel).await
    }
    .await;
    let close = tree
        .close_handle(connection, opened.file_id)
        .await
        .map_err(smb_error);
    result.and_then(|summary| close.map(|()| summary))
}

fn target_add_file_denied(frame: &smb2::client::connection::Frame) -> io::Result<bool> {
    if frame.header.command != Command::Create {
        return Err(invalid(
            "SMB-Zielrechteabfrage erhielt einen anderen Antworttyp",
        ));
    }
    if frame.header.status == NtStatus::ACCESS_DENIED {
        smb2::msg::header::ErrorResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(smb_error)?;
        return Ok(true);
    }
    require_status(frame, Command::Create)?;
    Ok(false)
}

async fn named_stream_target_access(
    connection: &mut Connection,
    tree: &Tree,
    remote: &str,
    expected_id: u64,
) -> io::Result<Capability> {
    // FILE_WRITE_DATA ist bei einem Directoryhandle FILE_ADD_FILE. DELETE und
    // MAXIMUM_ALLOWED gehören nicht zu dieser gezielten, nicht mutierenden Probe.
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(
            FileAccessMask::FILE_WRITE_DATA
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
        ),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access: ShareAccess(
            ShareAccess::FILE_SHARE_READ
                | ShareAccess::FILE_SHARE_WRITE
                | ShareAccess::FILE_SHARE_DELETE,
        ),
        create_disposition: CreateDisposition::FileOpen,
        create_options: FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        name: remote.into(),
        create_contexts: vec![],
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    if target_add_file_denied(&frame)? {
        return Ok(Capability::ReadOnly);
    }
    let opened = CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
    let result = async {
        let entry = query_handle_entry(connection, tree, opened.file_id, String::new()).await?;
        if !entry.is_dir || entry.is_symlink || entry.file_id != expected_id {
            return Err(crate::real_mode::plan_drift(
                "SMB-Zielroot änderte sich während der Rechteabfrage",
            ));
        }
        let access =
            query_info(connection, tree, opened.file_id, FILE_ACCESS_INFORMATION, 4).await?;
        Ok(
            if read_u32(&access, 0)? & FileAccessMask::FILE_WRITE_DATA != 0 {
                Capability::Exact
            } else {
                Capability::ReadOnly
            },
        )
    }
    .await;
    let close = tree
        .close_handle(connection, opened.file_id)
        .await
        .map_err(smb_error);
    result.and_then(|capability| close.map(|()| capability))
}

impl SmbVfs {
    pub(super) fn stream_metadata_capabilities(
        &self,
        root: &Path,
        target: bool,
    ) -> io::Result<MetadataCapabilities> {
        let (named_streams, sparse_extents) = self.root_metadata_capabilities(root, target)?;
        Ok(MetadataCapabilities {
            owner: Capability::Unsupported,
            special_mode_bits: Capability::Unsupported,
            acl_model: AclModel::WindowsSid,
            acl: Capability::ReadOnly,
            xattrs: Capability::Unsupported,
            resource_fork: Capability::Unsupported,
            named_streams,
            sparse_extents,
            hardlinks: Capability::Unsupported,
        })
    }

    pub(super) fn named_stream_capability(
        &self,
        root: &Path,
        target: bool,
    ) -> io::Result<Capability> {
        self.root_metadata_capabilities(root, target)
            .map(|value| value.0)
    }

    fn root_metadata_capabilities(
        &self,
        root: &Path,
        target: bool,
    ) -> io::Result<(Capability, Capability)> {
        let remote = self.remote_path(root)?;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(async {
            let id = open_handle(
                &mut state.session.connection,
                &tree,
                &remote,
                FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
                CreateDisposition::FileOpen,
                FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            )
            .await?;
            let result = async {
                let entry =
                    query_handle_entry(&mut state.session.connection, &tree, id, String::new())
                        .await?;
                if !entry.is_dir || entry.is_symlink || entry.file_id == 0 {
                    return Err(invalid(
                        "SMB-Streamfähigkeit benötigt einen echten gebundenen Rootordner",
                    ));
                }
                let request = QueryInfoRequest {
                    info_type: InfoType::Filesystem,
                    file_info_class: FILE_FS_ATTRIBUTE_INFORMATION,
                    output_buffer_length: 1024,
                    additional_information: 0,
                    flags: 0,
                    file_id: id,
                    input_buffer: Vec::new(),
                };
                let frame = state
                    .session
                    .connection
                    .execute(Command::QueryInfo, &request, Some(tree.tree_id))
                    .await
                    .map_err(smb_error)?;
                require_status(&frame, Command::QueryInfo)?;
                let bytes = QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(smb_error)?
                    .output_buffer;
                if bytes.len() < 12 {
                    return Err(invalid("SMB-Dateisystemattribute sind unvollständig"));
                }
                let name_len = read_u32(&bytes, 8)? as usize;
                if !name_len.is_multiple_of(2)
                    || bytes
                        .get(
                            12..12usize
                                .checked_add(name_len)
                                .ok_or_else(|| invalid("SMB-Dateisystemname läuft über"))?,
                        )
                        .is_none()
                {
                    return Err(invalid("SMB-Dateisystemname ist unvollständig"));
                }
                let flags = read_u32(&bytes, 0)?;
                let sparse = if !target && flags & FILE_SUPPORTS_SPARSE_FILES != 0 {
                    Capability::ReadOnly
                } else {
                    Capability::Unsupported
                };
                let streams = if flags & FILE_NAMED_STREAMS == 0 {
                    Capability::Unsupported
                } else if !target {
                    Capability::ReadOnly
                } else {
                    named_stream_target_access(
                        &mut state.session.connection,
                        &tree,
                        &remote,
                        entry.file_id,
                    )
                    .await?
                };
                Ok((streams, sparse))
            }
            .await;
            let close = tree
                .close_handle(&mut state.session.connection, id)
                .await
                .map_err(smb_error);
            result.and_then(|cap| close.map(|()| cap))
        })
    }

    pub(super) fn validate_stream_scope(
        &self,
        path: &Path,
        ordinary_root: Option<&Path>,
    ) -> io::Result<()> {
        stream_binding_scope(&self.canonical_root(), path, ordinary_root).map(|_| ())
    }

    pub(super) fn stream_manifest(&self, path: &Path) -> io::Result<MetadataManifest> {
        self.stream_manifest_checked(path, None, None)
    }

    pub(super) fn stream_manifest_checked(
        &self,
        path: &Path,
        cancel: Option<&crate::Cancel>,
        ordinary_root: Option<&Path>,
    ) -> io::Result<MetadataManifest> {
        self.stream_manifest_with_write_identity(path, cancel, ordinary_root, None)
    }

    // Ausschließlich der eigenen Temp-ID darf ein vorab gewährter FLUSH-Handle
    // gegenüberstehen. Dessen Sharemaske sperrt weiterhin fremde Datenwriter;
    // alle anderen Quellen behalten die unverändert exklusive Leseöffnung.
    pub(super) fn stream_manifest_with_write_identity(
        &self,
        path: &Path,
        cancel: Option<&crate::Cancel>,
        ordinary_root: Option<&Path>,
        write_identity: Option<u64>,
    ) -> io::Result<MetadataManifest> {
        check_cancel(cancel)?;
        let (remote, scope) = stream_binding_scope(&self.canonical_root(), path, ordinary_root)?;
        let capability_root = ordinary_root
            .filter(|root| path == *root)
            .unwrap_or_else(|| path.parent().unwrap_or(Path::new("/")));
        let capability = self.named_stream_capability(capability_root, false)?;
        let mut state = lock(&self.state);
        let tree = state.session.tree.clone();
        self.runtime.block_on(async {
            let entry =
                query_entry(&mut state.session.connection, &tree, &remote, String::new()).await?;
            let manifest = MetadataManifest::new(None, 0, None, vec![], None, None, None)?;
            let kind = StreamBaseKind::of(&entry);
            let (base, before, mut parents) = open_bound_base(
                &mut state.session.connection,
                &tree,
                &remote,
                kind,
                write_identity.is_some(),
                &scope,
            )
            .await?;
            let max_read = state
                .session
                .connection
                .params()
                .map(|p| p.max_read_size.min(MAX_IO_CHUNK))
                .unwrap_or(64 * 1024)
                .max(1);
            let unobserved = manifest.clone();
            let result = async {
                if entry.file_id != before.file_id
                    || write_identity.is_some_and(|expected| expected != before.file_id || before.is_dir)
                {
                    return Err(crate::real_mode::plan_drift(
                        "SMB-Streambasis änderte sich vor der typisierten Öffnung",
                    ));
                }
                let namespace = state.session.identity.server_guid;
                let mut previous = None;
                for _ in 0..2 {
                    check_cancel(cancel)?;
                    let names = if capability == Capability::Unsupported {
                        None
                    } else {
                        match stream_list(&mut state.session.connection, &tree, base, kind).await {
                            Ok(names) => Some(names),
                            Err(error) if directory_query_unsupported(&error) => None,
                            Err(error) => return Err(error),
                        }
                    };
                    let mut summaries = names.as_ref().map(|names| Vec::with_capacity(names.len()));
                    if let (Some(names), Some(summaries)) = (&names, &mut summaries) {
                        for (name, size) in names {
                            let value = hash_stream(
                                &mut state.session.connection, &tree,
                                &stream_path(&remote, name)?, *size, max_read, cancel,
                            ).await?;
                            summaries.push(NamedBlobSummary { name: name.clone(), value });
                        }
                    }
                    let security = super::security::observe_security(
                        &mut state.session.connection, &tree, &remote, &before, namespace,
                    ).await?;
                    if capability != Capability::Unsupported {
                        let after_names = match stream_list(&mut state.session.connection, &tree, base, kind).await {
                            Ok(names) => Some(names),
                            Err(error) if directory_query_unsupported(&error) => None,
                            Err(error) => return Err(error),
                        };
                        if after_names != names {
                            return Err(crate::real_mode::plan_drift(
                                "SMB-Streammenge änderte sich während der Metadatenbeobachtung",
                            ));
                        }
                    }
                    let sparse = if kind == StreamBaseKind::File {
                        Some(sparse::observe(&mut state.session.connection, &tree, base, before.size, cancel).await?)
                    } else { None };
                    let after = query_handle_entry(
                        &mut state.session.connection, &tree, base, String::new(),
                    ).await?;
                    if !same_manifest_base(&before, &after) {
                        return Err(crate::real_mode::plan_drift(
                            "SMB-Basisdatei änderte sich während der Metadatenbeobachtung",
                        ));
                    }
                    let observed = (summaries, security, sparse);
                    if previous.as_ref().is_some_and(|previous| previous != &observed) {
                        return Err(crate::real_mode::plan_drift(
                            "SMB-Streams, Windows-Sicherheit oder Sparse-Bereiche änderten sich während der Beobachtung",
                        ));
                    }
                    previous = Some(observed);
                }
                let (streams, security, sparse) = previous.expect("Zwei vollständige Metadatenbeobachtungen");
                MetadataManifest::new(None, 0, security.acl, vec![], None, None, None)?
                    .with_windows_security(Some(security.security))?
                    .with_named_streams(streams)?
                    .with_reported_hard_links(if kind == StreamBaseKind::File {
                        before.hard_links.map(u32::try_from).transpose().map_err(|_| invalid("SMB-Linkzahl überschreitet FileStandardInformation"))?
                    } else { None })?
                    .with_sparse_observation(sparse.as_ref().and_then(|value| value.extents.clone()), sparse.and_then(|value| value.issue))
            }
            .await;
            let close = tree
                .close_handle(&mut state.session.connection, base)
                .await
                .map_err(smb_error);
            let close_parents =
                close_parents(&mut state.session.connection, &tree, &mut parents).await;
            finish_stream_manifest(result, close.and(close_parents), unobserved)
        })
    }

    pub(super) fn sparse_read(
        &self,
        path: &Path,
        root: Option<&Path>,
        cancel: crate::Cancel,
    ) -> io::Result<Box<dyn crate::vfs::SparseRead>> {
        sparse::open(self, path, root, cancel)
    }

    pub(super) fn stream_read(
        &self,
        path: &Path,
        name: &[u8],
        ordinary_root: Option<&Path>,
    ) -> io::Result<Box<dyn Read + Send>> {
        self.stream_read_with_write_identity(path, name, ordinary_root, None)
    }
    pub(super) fn stream_read_with_write_identity(
        &self,
        path: &Path,
        name: &[u8],
        ordinary_root: Option<&Path>,
        write_identity: Option<u64>,
    ) -> io::Result<Box<dyn Read + Send>> {
        let (remote, scope) = stream_binding_scope(&self.canonical_root(), path, ordinary_root)?;
        let named = stream_path(&remote, name)?;
        let (tree, mut connection, max_read) = {
            let state = lock(&self.state);
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
        let entry =
            self.runtime
                .block_on(query_entry(&mut connection, &tree, &remote, String::new()))?;
        let kind = StreamBaseKind::of(&entry);
        let (base, before, mut parents) = self.runtime.block_on(open_bound_base(
            &mut connection,
            &tree,
            &remote,
            kind,
            write_identity.is_some(),
            &scope,
        ))?;
        let result = self.runtime.block_on(async {
            if entry.file_id != before.file_id
                || write_identity.is_some_and(|id| id != before.file_id)
            {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Streambasis änderte sich vor READ",
                ));
            }
            let names = stream_list(&mut connection, &tree, base, kind).await?;
            let size = names.get(name).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "SMB-Stream fehlt unter seinem exakten Namen",
                )
            })?;
            let opened = open_stream_handle(
                &mut connection,
                &tree,
                &named,
                FileAccessMask::FILE_READ_DATA
                    | FileAccessMask::FILE_READ_ATTRIBUTES
                    | FileAccessMask::SYNCHRONIZE,
                CreateDisposition::FileOpen,
            )
            .await?;
            if opened.end_of_file != *size
                || opened.file_attributes
                    & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                    != 0
            {
                let _ = tree.close_handle(&mut connection, opened.file_id).await;
                return Err(crate::real_mode::plan_drift(
                    "SMB-Streamgröße oder Typ änderte sich vor READ",
                ));
            }
            Ok(opened)
        });
        let opened = match result {
            Ok(opened) => opened,
            Err(error) => {
                let _ = self
                    .runtime
                    .block_on(tree.close_handle(&mut connection, base));
                let _ = self
                    .runtime
                    .block_on(close_parents(&mut connection, &tree, &mut parents));
                return Err(error);
            }
        };
        Ok(Box::new(NamedRead {
            inner: SmbRead {
                runtime: self.runtime.clone(),
                tree,
                connection,
                file_id: opened.file_id,
                size: opened.end_of_file,
                offset: 0,
                max_read,
                closed: false,
            },
            base: Some(base),
            parents,
        }))
    }

    pub(super) fn stream_write_new(
        &self,
        path: &Path,
        name: &[u8],
        expected_inode: u64,
        ordinary_root: Option<&Path>,
    ) -> io::Result<Box<dyn Write + Send>> {
        let (remote, scope) = stream_binding_scope(&self.canonical_root(), path, ordinary_root)?;
        let named = stream_path(&remote, name)?;
        let (tree, mut connection, max_write) = {
            let state = lock(&self.state);
            (
                state.session.tree.clone(),
                state.session.connection.clone(),
                state
                    .session
                    .connection
                    .params()
                    .map(|p| p.max_write_size.min(MAX_IO_CHUNK))
                    .unwrap_or(64 * 1024)
                    .max(1),
            )
        };
        if self
            .runtime
            .block_on(query_entry(&mut connection, &tree, &remote, String::new()))?
            .is_dir
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SMB-Verzeichnisstreams besitzen keinen Schreibvertrag",
            ));
        }
        let (base, entry, mut parents) = self.runtime.block_on(open_bound_base(
            &mut connection,
            &tree,
            &remote,
            StreamBaseKind::File,
            true,
            &scope,
        ))?;
        let result = self.runtime.block_on(async {
            if entry.file_id != expected_inode {
                return Err(crate::real_mode::plan_drift(
                    "SMB-Streambasis ist nicht die eigene Tempdatei",
                ));
            }
            let names = stream_list(&mut connection, &tree, base, StreamBaseKind::File).await?;
            let key = lookup_key(stream_name(name)?);
            if names
                .keys()
                .any(|n| stream_name(n).is_ok_and(|n| lookup_key(n) == key))
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "SMB-Streamname ist bereits vorhanden",
                ));
            }
            open_stream_handle(
                &mut connection,
                &tree,
                &named,
                FileAccessMask::FILE_WRITE_DATA
                    | FileAccessMask::FILE_READ_ATTRIBUTES
                    | FileAccessMask::SYNCHRONIZE,
                CreateDisposition::FileCreate,
            )
            .await
            .map(|opened| opened.file_id)
        });
        let id = match result {
            Ok(id) => id,
            Err(error) => {
                let _ = self
                    .runtime
                    .block_on(tree.close_handle(&mut connection, base));
                let _ = self
                    .runtime
                    .block_on(close_parents(&mut connection, &tree, &mut parents));
                return Err(error);
            }
        };
        Ok(Box::new(NamedWrite {
            inner: SmbWrite {
                runtime: self.runtime.clone(),
                tree,
                connection,
                file_id: id,
                offset: 0,
                max_write,
                finished: false,
            },
            base: Some(base),
            parents,
            failed: false,
        }))
    }
}

struct NamedRead {
    inner: SmbRead,
    base: Option<FileId>,
    parents: Vec<FileId>,
}
impl NamedRead {
    fn close_base(&mut self) -> io::Result<()> {
        let base = if let Some(base) = self.base.take() {
            self.inner
                .runtime
                .block_on(
                    self.inner
                        .tree
                        .close_handle(&mut self.inner.connection, base),
                )
                .map_err(smb_error)
        } else {
            Ok(())
        };
        let parents = self.inner.runtime.block_on(close_parents(
            &mut self.inner.connection,
            &self.inner.tree,
            &mut self.parents,
        ));
        base.and(parents)
    }
}
impl Read for NamedRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        if count == 0 && !buffer.is_empty() {
            self.close_base()?;
        }
        Ok(count)
    }
}
impl Drop for NamedRead {
    fn drop(&mut self) {
        let _ = self.inner.close();
        let _ = self.close_base();
    }
}
struct NamedWrite {
    inner: SmbWrite,
    base: Option<FileId>,
    parents: Vec<FileId>,
    failed: bool,
}
impl NamedWrite {
    fn close_base(&mut self) -> io::Result<()> {
        let base = if let Some(base) = self.base.take() {
            self.inner
                .runtime
                .block_on(
                    self.inner
                        .tree
                        .close_handle(&mut self.inner.connection, base),
                )
                .map_err(smb_error)
        } else {
            Ok(())
        };
        let parents = self.inner.runtime.block_on(close_parents(
            &mut self.inner.connection,
            &self.inner.tree,
            &mut self.parents,
        ));
        base.and(parents)
    }
}
impl Write for NamedWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        // Write::flush ist für SMB bereits der fallible Abschluss. Beide Handles
        // werden hier geprüft geschlossen, bevor der Transfer den Basiscommit erlaubt.
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SMB-Streamabschluss war bereits fehlgeschlagen",
            ));
        }
        let result = self.inner.flush();
        let close = self.close_base();
        let result = result.and(close);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl Drop for NamedWrite {
    fn drop(&mut self) {
        let _ = self.close_base();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_count_binds_only_regular_file_metadata_not_object_wide_content_proofs() {
        let before = SmbListedEntry {
            name: "file".into(),
            is_dir: false,
            is_symlink: false,
            size: 3,
            mtime: Some(1),
            created: Some(1),
            change_time: 1,
            file_id: 9,
            hard_links: Some(2),
        };
        for count in [None, Some(0), Some(1), Some(2), Some(3)] {
            let mut after = before.clone();
            after.hard_links = count;
            assert!(same_base(&before, &after));
            assert_eq!(same_manifest_base(&before, &after), count == Some(2));
            let mut directory_before = before.clone();
            directory_before.is_dir = true;
            after.is_dir = true;
            assert!(same_manifest_base(&directory_before, &after));
        }
    }

    fn record(name: &str, size: u64, next: bool) -> Vec<u8> {
        let encoded: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut bytes = vec![0; 24];
        bytes[4..8].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
        bytes[8..16].copy_from_slice(&size.to_le_bytes());
        bytes.extend(encoded);
        if next {
            bytes.resize(bytes.len().next_multiple_of(8), 0xa5);
            let length = bytes.len() as u32;
            bytes[..4].copy_from_slice(&length.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parser_bindet_nur_benannte_streams_und_ignoriert_alignmentbytes() {
        let mut bytes = record("::$DATA", 12, true);
        bytes.extend(record(":Ünicode:$DATA", 7, true));
        bytes.extend(record(":empty:$DATA", 0, false));
        assert_eq!(
            parse_streams(&bytes).unwrap(),
            BTreeMap::from([("Ünicode".as_bytes().to_vec(), 7), (b"empty".to_vec(), 0)])
        );
        let mut padded = record(":empty:$DATA", 0, false);
        padded.resize(padded.len().next_multiple_of(8), 0xfe);
        assert!(parse_streams(&padded).is_ok());
    }

    #[test]
    fn parser_verwirft_trunkierung_negative_groessen_utf16_und_offsets() {
        let bytes = record(":test:$DATA", 1, false);
        for length in 0..bytes.len() {
            assert!(parse_streams(&bytes[..length]).is_err(), "Länge {length}");
        }
        for position in [8, 16] {
            let mut bad = bytes.clone();
            bad[position..position + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert!(parse_streams(&bad).is_err());
        }
        let mut bad = bytes.clone();
        bad[24..26].copy_from_slice(&0xd800u16.to_le_bytes());
        assert!(parse_streams(&bad).is_err());
        for next in [8u32, 25, 0xffff_fff8] {
            let mut bad = bytes.clone();
            bad[..4].copy_from_slice(&next.to_le_bytes());
            assert!(parse_streams(&bad).is_err());
        }
        let mut bad = bytes.clone();
        bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_streams(&bad).is_err());
        let mut bad = bytes;
        bad.extend([0; 8]);
        assert!(parse_streams(&bad).is_err());
    }

    #[test]
    fn parser_verwirft_lookupkollisionen_und_streamnamensinjektion() {
        for name in ["", "a:b", "a/b", "a\\b", "a\0b"] {
            assert!(stream_path("base", name.as_bytes()).is_err());
        }
        assert!(stream_path("bad:base", b"okay").is_err());
        assert!(stream_path("base", &[0xff]).is_err());
        for (first, second) in [
            (":A:$DATA", ":a:$DATA"),
            (":Σ:$DATA", ":ς:$DATA"),
            ("::$DATA", "::$DATA"),
        ] {
            let mut bytes = record(first, 0, true);
            bytes.extend(record(second, 0, false));
            assert!(parse_streams(&bytes).is_err());
        }
        for name in [
            ":bad:extra:$DATA",
            ":x:$INDEX_ALLOCATION",
            "plain",
            ":bad\0:$DATA",
        ] {
            assert!(parse_streams(&record(name, 0, false)).is_err());
        }
    }

    fn test_provider() -> io::Result<(Arc<SmbVfs>, std::path::PathBuf)> {
        struct TestSecrets;
        impl SecretResolver for TestSecrets {
            fn resolve(
                &self,
                _: &crate::KeychainSecretReference,
            ) -> io::Result<crate::SecretString> {
                crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
            }
        }
        let required = |name| std::env::var(name).expect("Isolierter SMB-Proberunner erforderlich");
        let server_root = required("DOPPELDECKER_SMB_TEST_ROOT");
        let root = Path::new(&server_root);
        if !server_root.starts_with("/tmp/doppeldecker-smb-stream-probe-20261029/run.")
            || root.file_name() != Some(std::ffi::OsStr::new("share"))
        {
            return Err(invalid("Kein privater Stream-Proberoot"));
        }
        let host = required("DOPPELDECKER_SMB_TEST_HOST");
        let port = required("DOPPELDECKER_SMB_TEST_PORT")
            .parse()
            .map_err(|_| invalid("Port ungültig"))?;
        let discovery = discover_smb_server(&host, port)?;
        let vfs = Arc::new(SmbVfs::connect(
            SmbConnectionConfig {
                host,
                port,
                server_guid: discovery.server_guid,
                share: required("DOPPELDECKER_SMB_TEST_SHARE"),
                username: required("DOPPELDECKER_SMB_TEST_USER"),
                domain: String::new(),
                requested_root: "/".into(),
                password_reference: crate::KeychainSecretReference::new(
                    "Doppeldecker-Test",
                    "smb-stream",
                )?,
            },
            Arc::new(TestSecrets),
        )?);
        assert!(vfs.identity().signing_active && vfs.identity().encryption_active);
        assert!(vfs.list_complete(Path::new("/"))?.entries.is_empty());
        Ok((vfs, root.to_path_buf()))
    }

    #[test]
    #[ignore = "benötigt privaten Samba mit streams_xattr"]
    fn smb_named_stream_transport_roundtrip() -> io::Result<()> {
        let (vfs, root) = test_provider()?;
        let base = Path::new("/nested/stream-temp");
        let cancel = crate::Cancel::new();
        let ordinary = vfs.content_sync_mutations(Path::new("/"), &cancel, None)?;
        let mode = std::env::var("DOPPELDECKER_SMB_STREAM_TEST_MODE").unwrap_or_default();
        if mode == "readonly" || mode == "unsupported" {
            let fixture = root.join("fixture");
            std::fs::write(&fixture, b"main")?;
            let result = (|| {
                let expected = if mode == "unsupported" {
                    Capability::Unsupported
                } else {
                    Capability::ReadOnly
                };
                assert_eq!(
                    vfs.metadata_capabilities(Path::new("/"))?.named_streams,
                    expected
                );
                assert_eq!(
                    ordinary
                        .metadata_capabilities(Path::new("/"))?
                        .named_streams,
                    expected
                );
                let manifest = ordinary.metadata_manifest(Path::new("/fixture"))?;
                assert_eq!(
                    manifest.named_streams,
                    if mode == "unsupported" {
                        None
                    } else {
                        Some(vec![])
                    }
                );
                if mode == "readonly" {
                    assert!(ordinary
                        .open_write_new(Path::new("/never-created"))
                        .is_err());
                }
                assert_eq!(std::fs::read(&fixture)?, b"main");
                Ok(())
            })();
            let cleanup = std::fs::remove_file(&fixture);
            return result.and(cleanup);
        }
        assert_eq!(
            vfs.metadata_capabilities(Path::new("/"))?.named_streams,
            Capability::ReadOnly
        );
        assert_eq!(
            ordinary
                .metadata_capabilities(Path::new("/"))?
                .named_streams,
            Capability::Exact
        );
        assert!(vfs.open_named_stream_write_new(base, b"denied").is_err());
        assert!(ordinary
            .open_named_stream_write_new(base, b"denied")
            .is_err());
        super::super::interactive::verify_created_temp_binding(&vfs)?;
        ordinary.create_dir(Path::new("/nested"))?;
        let scoped = ordinary
            .staging_scope(Path::new("/nested/committed"), None)?
            .unwrap();
        let result = (|| {
            let mut writer = scoped.open_write_new(base)?;
            writer.write_all(b"main-content")?;
            writer.flush()?;
            drop(writer);
            assert_eq!(scoped.metadata_manifest(base)?.named_streams, Some(vec![]));
            assert!(scoped
                .open_named_stream_write_new(Path::new("/other"), b"unsafe")
                .is_err());
            for (name, bytes) in [
                ("Ünicode", &b"stream-content\0\xff"[..]),
                ("empty", &b""[..]),
            ] {
                let mut writer = scoped.open_named_stream_write_new(base, name.as_bytes())?;
                writer.write_all(bytes)?;
                writer.flush()?;
                drop(writer);
            }
            assert_eq!(
                scoped
                    .open_named_stream_write_new(base, "Ünicode".as_bytes())
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
            let manifest = scoped.metadata_manifest(base)?;
            assert_eq!(
                manifest.named_streams,
                Some(vec![
                    NamedBlobSummary {
                        name: b"empty".to_vec(),
                        value: BlobSummary::from_bytes(b"")
                    },
                    NamedBlobSummary {
                        name: "Ünicode".as_bytes().to_vec(),
                        value: BlobSummary::from_bytes(b"stream-content\0\xff")
                    },
                ])
            );
            for (name, expected) in [
                ("Ünicode", &b"stream-content\0\xff"[..]),
                ("empty", &b""[..]),
            ] {
                let mut actual = Vec::new();
                scoped
                    .open_named_stream_read(base, name.as_bytes())?
                    .read_to_end(&mut actual)?;
                assert_eq!(actual, expected);
            }
            let mut held = scoped.open_named_stream_write_new(base, b"held")?;
            assert!(
                vfs.rename_noreplace_unchecked(Path::new("/nested"), Path::new("/renamed"))
                    .is_err(),
                "Gehaltene Streameltern wurden trotzdem umbenannt"
            );
            assert!(
                vfs.rename_noreplace_unchecked(base, Path::new("/nested/moved"))
                    .is_err(),
                "Gehaltene Streambasis wurde trotzdem umbenannt"
            );
            held.write_all(b"held")?;
            held.flush()?;
            drop(held);
            let mut extra = scoped.open_named_stream_write_new(base, b"unexpected")?;
            extra.write_all(b"extra")?;
            extra.flush()?;
            drop(extra);
            assert_ne!(scoped.metadata_manifest(base)?.digest, manifest.digest);
            let mut huge = scoped.open_named_stream_write_new(base, b"too-large")?;
            let oversized = vec![0xa5; 128 * 1024];
            let oversized_result = huge.write_all(&oversized).and_then(|()| huge.flush());
            if std::env::var_os("DOPPELDECKER_SMB_STREAM_TEST_LIMITED").is_some() {
                assert!(
                    oversized_result.is_err(),
                    "Der künstlich begrenzte Server akzeptierte den großen Stream"
                );
            }
            drop(huge);
            let mut main = Vec::new();
            scoped.open_read(base)?.read_to_end(&mut main)?;
            assert_eq!(main, b"main-content");
            // Ein fremder Basispfad und ein ausgetauschtes Inode dürfen durch
            // Stream-CREATE weder neu entstehen noch nachträglich beschrieben werden.
            assert!(vfs
                .stream_write_new(Path::new("/missing"), b"never", 1, None)
                .is_err());
            assert!(vfs
                .stream_write_new(base, b"wrong-inode", u64::MAX, None)
                .is_err());
            assert_eq!(
                scoped.metadata_manifest(Path::new("/"))?.named_streams,
                Some(vec![])
            );
            cancel.cancel();
            assert!(scoped.open_named_stream_read(base, b"empty").is_err());
            assert!(scoped
                .open_named_stream_write_new(base, b"cancelled")
                .is_err());
            Ok(())
        })();
        let cleanup = vfs
            .remove_file_unchecked(base)
            .and_then(|()| vfs.remove_dir_unchecked(Path::new("/nested")))
            .and_then(|()| {
                if !vfs.list_complete(Path::new("/"))?.entries.is_empty() {
                    return Err(invalid("SMB-Proberoot nach Cleanup nicht leer"));
                }
                Ok(())
            });
        if let Err(error) = &cleanup {
            eprintln!("SMB-Stream-Cleanup fehlgeschlagen: {error}");
        }
        result.and(cleanup)
    }
    fn seed_directory_stream(
        vfs: &SmbVfs,
        directory: &Path,
        name: &[u8],
        bytes: &[u8],
    ) -> io::Result<()> {
        let remote = vfs.remote_path(directory)?;
        let path = stream_path(&remote, name)?;
        let mut state = lock(&vfs.state);
        let tree = state.session.tree.clone();
        vfs.runtime.block_on(async {
            let connection = &mut state.session.connection;
            let (base, _, mut parents) = open_bound_base(
                connection,
                &tree,
                &remote,
                StreamBaseKind::Directory,
                false,
                &StreamBindingScope::EntireShare,
            )
            .await?;
            let result = async {
                let opened = open_stream_handle(
                    connection,
                    &tree,
                    &path,
                    FileAccessMask::FILE_WRITE_DATA
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE,
                    CreateDisposition::FileCreate,
                )
                .await?;
                let result = async {
                    if !bytes.is_empty() {
                        let request = WriteRequest {
                            data_offset: 0x70,
                            offset: 0,
                            file_id: opened.file_id,
                            channel: 0,
                            remaining_bytes: 0,
                            write_channel_info_offset: 0,
                            write_channel_info_length: 0,
                            flags: 0,
                            data: bytes.to_vec(),
                        };
                        let frame = connection
                            .execute(Command::Write, &request, Some(tree.tree_id))
                            .await
                            .map_err(smb_error)?;
                        require_status(&frame, Command::Write)?;
                        let response = WriteResponse::unpack(&mut ReadCursor::new(&frame.body))
                            .map_err(smb_error)?;
                        if response.count as usize != bytes.len() {
                            return Err(invalid(
                                "Künstlicher Ordnerstream wurde unvollständig geschrieben",
                            ));
                        }
                    }
                    flush_handle(connection, &tree, opened.file_id).await
                }
                .await;
                let close = tree
                    .close_handle(connection, opened.file_id)
                    .await
                    .map_err(smb_error);
                result.and(close)
            }
            .await;
            let close = tree.close_handle(connection, base).await.map_err(smb_error);
            let parents = close_parents(connection, &tree, &mut parents).await;
            result.and(close).and(parents)
        })
    }

    #[test]
    fn leere_antwort_ist_nur_bei_typisiertem_ordner_eine_beobachtete_leermenge() {
        assert!(parse_streams_for_base(&[], StreamBaseKind::File).is_err());
        assert!(parse_streams_for_base(&[], StreamBaseKind::Directory)
            .unwrap()
            .is_empty());
        assert!(parse_streams_for_base(&[0; 24], StreamBaseKind::Directory).is_err());
        let mut entry = SmbListedEntry {
            name: "directory".into(),
            is_dir: true,
            is_symlink: false,
            size: 0,
            mtime: None,
            created: None,
            change_time: 7,
            file_id: 42,
            hard_links: None,
        };
        let before = entry.clone();
        assert!(same_base(&before, &entry));
        entry.change_time += 1;
        assert!(!same_base(&before, &entry));
        entry = before.clone();
        entry.file_id += 1;
        assert!(!same_base(&before, &entry));
        entry = before.clone();
        entry.is_dir = false;
        assert!(!same_base(&before, &entry));
        entry = before.clone();
        entry.is_symlink = true;
        assert!(!same_base(&before, &entry));
    }

    #[test]
    #[ignore = "benötigt privaten Samba mit streams_xattr"]
    fn smb_directory_stream_read_roundtrip() -> io::Result<()> {
        let (vfs, root) = test_provider()?;
        let directory = Path::new("/directory");
        let fixture = root.join("directory");
        let cancel = crate::Cancel::new();
        let ordinary = vfs.content_sync_mutations(Path::new("/"), &cancel, None)?;
        let mode = std::env::var("DOPPELDECKER_SMB_STREAM_TEST_MODE").unwrap_or_default();
        if mode == "unsupported" || mode == "readonly" {
            std::fs::create_dir(&fixture)?;
            let result = (|| {
                let observed = ordinary.metadata_manifest(directory)?.named_streams;
                assert_eq!(
                    observed,
                    if mode == "unsupported" {
                        None
                    } else {
                        Some(vec![])
                    }
                );
                assert!(ordinary
                    .open_named_stream_write_new(directory, b"must-not-write")
                    .is_err());
                Ok(())
            })();
            let cleanup = std::fs::remove_dir(&fixture);
            return result.and(cleanup);
        }
        vfs.create_dir_unchecked(directory)?;
        let result = (|| {
            assert_eq!(
                ordinary.metadata_manifest(directory)?.named_streams,
                Some(vec![])
            );
            seed_directory_stream(&vfs, directory, "Ünicode".as_bytes(), b"directory-stream")?;
            seed_directory_stream(&vfs, directory, b"empty", b"")?;
            let before = ordinary.metadata_manifest(directory)?;
            assert_eq!(
                before.named_streams,
                Some(vec![
                    NamedBlobSummary {
                        name: b"empty".to_vec(),
                        value: BlobSummary::from_bytes(b"")
                    },
                    NamedBlobSummary {
                        name: "Ünicode".as_bytes().to_vec(),
                        value: BlobSummary::from_bytes(b"directory-stream")
                    },
                ])
            );
            for (name, expected) in [("Ünicode", &b"directory-stream"[..]), ("empty", &b""[..])] {
                let mut bytes = Vec::new();
                ordinary
                    .open_named_stream_read(directory, name.as_bytes())?
                    .read_to_end(&mut bytes)?;
                assert_eq!(bytes, expected);
            }
            let reader = ordinary.open_named_stream_read(directory, b"empty")?;
            assert!(vfs
                .rename_noreplace_unchecked(directory, Path::new("/renamed-directory"))
                .is_err());
            drop(reader);
            let inode = vfs.stat(directory)?.inode.unwrap();
            assert_eq!(
                vfs.stream_write_new(directory, b"denied", inode, None)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::Unsupported
            );
            assert!(ordinary
                .open_named_stream_write_new(directory, b"denied")
                .is_err());
            assert_eq!(ordinary.metadata_manifest(directory)?.digest, before.digest);
            seed_directory_stream(&vfs, directory, b"drift", b"changed")?;
            let drift =
                crate::metadata::verify_metadata_manifest(ordinary.as_ref(), directory, &before)
                    .unwrap_err();
            assert_eq!(
                crate::error::error_code(&drift),
                crate::ErrorCode::PlanDrift
            );

            // Weder eine fehlgeschlagene Enumeration noch ein verlorener CLOSE
            // darf eine erfolgreich gelesene oder leere Menge vortäuschen.
            let remote = vfs.remote_path(directory)?;
            let (tree, mut connection) = {
                let state = lock(&vfs.state);
                (state.session.tree.clone(), state.session.connection.clone())
            };
            let (base, _, mut parents) = vfs.runtime.block_on(open_bound_base(
                &mut connection,
                &tree,
                &remote,
                StreamBaseKind::Directory,
                false,
                &StreamBindingScope::EntireShare,
            ))?;
            vfs.runtime
                .block_on(tree.close_handle(&mut connection, base))
                .map_err(smb_error)?;
            assert!(vfs
                .runtime
                .block_on(stream_list(
                    &mut connection,
                    &tree,
                    base,
                    StreamBaseKind::Directory
                ))
                .is_err());
            let opened = vfs.runtime.block_on(open_stream_handle(
                &mut connection,
                &tree,
                &stream_path(&remote, b"empty")?,
                FileAccessMask::FILE_READ_DATA
                    | FileAccessMask::FILE_READ_ATTRIBUTES
                    | FileAccessMask::SYNCHRONIZE,
                CreateDisposition::FileOpen,
            ))?;
            let mut reader = NamedRead {
                inner: SmbRead {
                    runtime: vfs.runtime.clone(),
                    tree,
                    connection,
                    file_id: opened.file_id,
                    size: 0,
                    offset: 0,
                    max_read: 1024,
                    closed: false,
                },
                base: Some(base),
                parents: std::mem::take(&mut parents),
            };
            assert!(reader.read(&mut [0; 1]).is_err());
            drop(reader);
            cancel.cancel();
            let cancelled = ordinary.metadata_manifest(directory).unwrap_err();
            assert_eq!(
                crate::error::error_code(&cancelled),
                crate::ErrorCode::Interrupted
            );
            assert!(ordinary
                .open_named_stream_read(directory, b"empty")
                .is_err());
            Ok(())
        })();
        let cleanup = vfs.remove_dir_unchecked(directory);
        result.and(cleanup)?;

        // Der beobachtete Ordnertyp wird vor der Handleöffnung ausgetauscht.
        vfs.create_dir_unchecked(directory)?;
        let observed = vfs.stat_unchecked(directory)?;
        assert_eq!(StreamBaseKind::of(&observed), StreamBaseKind::Directory);
        vfs.remove_dir_unchecked(directory)?;
        let mut writer = vfs.open_write_new_unchecked(directory)?;
        writer.flush()?;
        drop(writer);
        let remote = vfs.remote_path(directory)?;
        let rejected = {
            let mut state = lock(&vfs.state);
            let tree = state.session.tree.clone();
            vfs.runtime
                .block_on(open_bound_base(
                    &mut state.session.connection,
                    &tree,
                    &remote,
                    StreamBaseKind::Directory,
                    false,
                    &StreamBindingScope::EntireShare,
                ))
                .is_err()
        };
        assert!(
            rejected,
            "Typisierte Ordneröffnung akzeptierte eine reguläre Ersatzdatei"
        );
        vfs.remove_file_unchecked(directory)?;
        assert!(vfs.list_complete(Path::new("/"))?.entries.is_empty());
        Ok(())
    }
    #[test]
    #[ignore = "benötigt privaten Samba mit streams_xattr"]
    fn smb_directory_stream_content_queue_roundtrip() -> io::Result<()> {
        use crate::{
            execute_prepared_content_sync_queue_entry, prepare_content_sync_for_queue, Cancel,
            Compare, MetadataClass, MetadataProfile, OperationQueue, ProviderEndpoint,
            QueueExecutionResult, QueueState, SyncMode, SyncOptions,
        };
        use std::os::unix::fs::PermissionsExt;
        let (vfs, root) = test_provider()?;
        let base = Path::new("/directory-queue");
        let source_root = base.join("source");
        let target_root = base.join("target");
        let source_directory = source_root.join("subdirectory");
        let target_directory = target_root.join("subdirectory");
        for directory in [base, &source_root, &target_root, &source_directory] {
            vfs.create_dir_unchecked(directory)?;
        }
        let result = (|| {
            let child = source_directory.join("child.txt");
            let context = vfs.content_sync_mutations(&source_root, &Cancel::new(), None)?;
            let scoped = context.staging_scope(&child, None)?.unwrap();
            let temp = source_directory.join(".child-temp");
            let payload = b"child-main";
            let mut writer = scoped.open_write_new(&temp)?;
            writer.write_all(payload)?;
            writer.flush()?;
            drop(writer);
            scoped.set_mtime(&temp, 1_700_000_000)?;
            scoped.rename_noreplace(&temp, &child)?;
            scoped.finish_staging()?;
            seed_directory_stream(
                &vfs,
                &source_directory,
                "Ünicode".as_bytes(),
                b"directory-stream",
            )?;
            seed_directory_stream(&vfs, &source_directory, b"empty", b"")?;
            let source_manifest = context.metadata_manifest(&source_directory)?;
            let config = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()?;
            let config_root = config.path().canonicalize()?;
            let queue = OperationQueue::open(config_root.join("queue.json"))?;
            let audit = config_root.join("audit.jsonl");
            for mode in [SyncMode::AddOnly, SyncMode::Update] {
                let prepared = prepare_content_sync_for_queue(
                    &queue,
                    ProviderEndpoint::smb(vfs.clone(), &source_root)?,
                    ProviderEndpoint::smb(vfs.clone(), &target_root)?,
                    SyncOptions {
                        mode,
                        compare: Compare::ContentHash,
                        metadata: MetadataProfile::NativeBestEffort,
                        ..SyncOptions::default()
                    },
                    &Cancel::new(),
                )?;
                let preview = prepared.preview().clone();
                queue.enqueue(prepared.queue_operation())?;
                let entry = queue.claim_next()?.unwrap();
                let executed = execute_prepared_content_sync_queue_entry(
                    &queue,
                    &entry,
                    prepared,
                    true,
                    &audit,
                    &Cancel::new(),
                    &mut |_| {},
                )?;
                let QueueExecutionResult::Sync(stats) = executed else {
                    panic!("Syncbilanz fehlt");
                };
                assert_eq!(
                    queue.snapshot().entries.last().unwrap().state,
                    QueueState::Completed
                );
                assert!(
                    stats
                        .metadata_report
                        .issues
                        .iter()
                        .any(|issue| issue.path == "subdirectory"
                            && issue.class == MetadataClass::NamedStreams),
                    "Pfadgenaue Ordnerstream-Auslassung fehlt: {:?}",
                    stats.metadata_report
                );
                if mode == SyncMode::AddOnly {
                    assert_eq!(
                        (stats.files_copied, stats.bytes_copied),
                        (1, payload.len() as u64)
                    );
                    assert_eq!(preview.plan.copy_count, 1);
                } else {
                    assert_eq!(
                        (
                            stats.files_copied,
                            stats.bytes_copied,
                            stats.metadata_updated
                        ),
                        (0, 0, 0)
                    );
                    assert_eq!(
                        (
                            preview.plan.copy_count,
                            preview.plan.update_count,
                            preview.plan.metadata_count
                        ),
                        (0, 0, 0)
                    );
                    assert!(
                        preview.plan.actions.is_empty(),
                        "Noop erzeugte Dummywrites: {:?}",
                        preview.plan.actions
                    );
                }
                assert_eq!(
                    std::fs::read(root.join("directory-queue/target/subdirectory/child.txt"))?,
                    payload
                );
                assert_eq!(
                    context.metadata_manifest(&source_directory)?.digest,
                    source_manifest.digest
                );
                assert_eq!(
                    vfs.metadata_manifest(&target_directory)?.named_streams,
                    Some(vec![])
                );
            }
            assert_eq!(queue.snapshot().entries.len(), 2);
            assert!(queue
                .snapshot()
                .entries
                .iter()
                .all(|entry| entry.state == QueueState::Completed && entry.attempts == 1));
            println!("DIRECTORY_STREAM_QUEUE_OK add_files=1 add_bytes={} noop_files=0 noop_bytes=0 noop_metadata=0 named_stream_issue_retained=1 target_streams_unchanged=1", payload.len());
            Ok(())
        })();
        let cleanup = vfs.remove_dir_all_unchecked(base).and_then(|()| {
            if !vfs.list_complete(Path::new("/"))?.entries.is_empty() {
                return Err(invalid("Directory-Queue-Proberoot nach Cleanup nicht leer"));
            }
            Ok(())
        });
        result.and(cleanup)
    }
    #[test]
    fn nur_praezise_directory_query_status_werden_unbeobachtet_und_nie_leer() {
        use smb2::pack::{Pack, WriteCursor};
        fn frame(status: NtStatus) -> smb2::client::connection::Frame {
            let mut header = smb2::msg::header::Header::new_request(Command::QueryInfo);
            header.status = status;
            let mut body = WriteCursor::new();
            smb2::msg::header::ErrorResponse {
                error_context_count: 0,
                error_data: vec![],
            }
            .pack(&mut body);
            smb2::client::connection::Frame {
                header,
                body: body.into_inner(),
                raw: vec![],
            }
        }
        let empty = || MetadataManifest::new(None, 0, None, vec![], None, None, None).unwrap();
        for status in [
            NtStatus(0xc000_0003),
            NtStatus::INVALID_DEVICE_REQUEST,
            NtStatus::NOT_SUPPORTED,
        ] {
            let reply = frame(status);
            let error = decode_stream_list_response(&reply, StreamBaseKind::Directory).unwrap_err();
            assert!(directory_query_unsupported(&error));
            assert!(error.to_string().contains(&format!("{:#010x}", status.0)));
            let manifest = finish_stream_manifest(Err(error), Ok(()), empty()).unwrap();
            assert_eq!(manifest.named_streams, None);
            let regular = decode_stream_list_response(&reply, StreamBaseKind::File).unwrap_err();
            assert!(!directory_query_unsupported(&regular));
            assert!(finish_stream_manifest(Err(regular), Ok(()), empty()).is_err());
            let error = decode_stream_list_response(&reply, StreamBaseKind::Directory).unwrap_err();
            assert!(finish_stream_manifest(
                Err(error),
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "CLOSE verloren")),
                empty()
            )
            .is_err());
        }
        for status in [
            NtStatus::ACCESS_DENIED,
            NtStatus::INVALID_PARAMETER,
            NtStatus::NOT_IMPLEMENTED,
            NtStatus::BUFFER_OVERFLOW,
            NtStatus::UNSUCCESSFUL,
            NtStatus::NO_SUCH_FILE,
        ] {
            let error =
                decode_stream_list_response(&frame(status), StreamBaseKind::Directory).unwrap_err();
            assert!(
                !directory_query_unsupported(&error),
                "Status {status} wurde zu breit herabgestuft"
            );
            assert!(finish_stream_manifest(Err(error), Ok(()), empty()).is_err());
        }
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Unsupported,
        ] {
            assert!(finish_stream_manifest(
                Err(io::Error::new(kind, "Kein typisierter Query-Status")),
                Ok(()),
                empty()
            )
            .is_err());
        }
        let mut malformed = frame(NtStatus::NOT_SUPPORTED);
        malformed.body.clear();
        assert!(!directory_query_unsupported(
            &decode_stream_list_response(&malformed, StreamBaseKind::Directory).unwrap_err()
        ));
        let mut wrong_command = frame(NtStatus::NOT_SUPPORTED);
        wrong_command.header.command = Command::Read;
        assert!(!directory_query_unsupported(
            &decode_stream_list_response(&wrong_command, StreamBaseKind::Directory).unwrap_err()
        ));
        let mut success = frame(NtStatus::SUCCESS);
        let mut body = WriteCursor::new();
        QueryInfoResponse {
            output_buffer: vec![],
        }
        .pack(&mut body);
        success.body = body.into_inner();
        assert!(
            decode_stream_list_response(&success, StreamBaseKind::Directory)
                .unwrap()
                .is_empty()
        );
        assert!(decode_stream_list_response(&success, StreamBaseKind::File).is_err());
    }
    #[test]
    fn stream_scope_prueft_grenze_vor_zugriff_und_verlangt_obere_listingrechte_nur_stark() {
        let (remote, scope) = stream_binding_scope(
            "/configured",
            Path::new("/upper/selected/basis"),
            Some(Path::new("/upper/selected")),
        )
        .unwrap();
        assert_eq!(remote, "configured\\upper\\selected\\basis");
        for upper in ["", "configured", "configured\\upper"] {
            assert!(!scope.requires_directory_read(upper));
        }
        for held in [
            "configured\\upper\\selected",
            "configured\\upper\\selected\\child",
        ] {
            assert!(scope.requires_directory_read(held));
        }
        assert!(!scope.requires_directory_read("configured\\upper\\selected-sibling"));
        for outside in [
            "/upper/selected-sibling/file",
            "/other",
            "relative",
            "/upper/selected/../file",
            "/upper/selected/file:stream",
        ] {
            assert!(stream_binding_scope(
                "/configured",
                Path::new(outside),
                Some(Path::new("/upper/selected"))
            )
            .is_err());
        }
        assert!(stream_binding_scope("/bad:root", Path::new("/file"), None).is_err());
        let (_, strong) = stream_binding_scope("/configured", Path::new("/file"), None).unwrap();
        assert!(strong.requires_directory_read(""));
        assert!(strong.requires_directory_read("configured"));
    }

    async fn old_attribute_guard(
        c: &mut Connection,
        t: &Tree,
        path: &str,
        kind: StreamBaseKind,
    ) -> io::Result<FileId> {
        let request = CreateRequest {
            requested_oplock_level: OplockLevel::None,
            impersonation_level: ImpersonationLevel::Impersonation,
            desired_access: FileAccessMask::new(
                FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
            ),
            file_attributes: FILE_ATTRIBUTE_NORMAL,
            share_access: ShareAccess(ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_WRITE),
            create_disposition: CreateDisposition::FileOpen,
            create_options: kind.open_options(),
            name: path.into(),
            create_contexts: vec![],
        };
        let frame = c
            .execute(Command::Create, &request, Some(t.tree_id))
            .await
            .map_err(smb_error)?;
        require_status(&frame, Command::Create)?;
        Ok(CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(smb_error)?
            .file_id)
    }

    fn independent_provider(vfs: &SmbVfs) -> io::Result<SmbVfs> {
        struct Secrets;
        impl SecretResolver for Secrets {
            fn resolve(
                &self,
                _: &crate::KeychainSecretReference,
            ) -> io::Result<crate::SecretString> {
                crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
            }
        }
        let other = SmbVfs::connect(vfs.connection_config().clone(), Arc::new(Secrets))?;
        let first = lock(&vfs.state).session.connection.session_id();
        let second = lock(&other.state).session.connection.session_id();
        assert_ne!(first, second);
        eprintln!("INDEPENDENT_SESSIONS first={} second={}", first.0, second.0);
        Ok(other)
    }

    #[test]
    #[ignore = "benötigt privaten Samba mit zwei getrennten Sitzungen"]
    fn smb_stream_binding_independent_sessions() -> io::Result<()> {
        let (vfs, _) = test_provider()?;
        let other = independent_provider(&vfs)?;
        for (label, strong) in [("weak", false), ("strong", true)] {
            let parent = Path::new("/").join(label);
            let basis = parent.join("basis");
            vfs.create_dir_unchecked(&parent)?;
            vfs.create_dir_unchecked(&basis)?;
            let mut writer = vfs.open_write_new_unchecked(&basis.join("file.bin"))?;
            writer.write_all(b"main")?;
            writer.flush()?;
            drop(writer);
            let mut state = lock(&vfs.state);
            let tree = state.session.tree.clone();
            let (foreign_tree, mut foreign) = {
                let state = lock(&other.state);
                (state.session.tree.clone(), state.session.connection.clone())
            };
            let actual_parent = vfs.runtime.block_on(async {
                let c = &mut state.session.connection;
                let remote = format!("{label}\\basis");
                let (base, before, mut parents) = if strong {
                    open_bound_base(
                        c,
                        &tree,
                        &remote,
                        StreamBaseKind::Directory,
                        false,
                        &StreamBindingScope::EntireShare,
                    )
                    .await?
                } else {
                    let p = old_attribute_guard(c, &tree, label, StreamBaseKind::Directory).await?;
                    let b =
                        old_attribute_guard(c, &tree, &remote, StreamBaseKind::Directory).await?;
                    let entry = query_handle_entry(c, &tree, b, String::new()).await?;
                    (b, entry, vec![p])
                };
                let file_remote = format!("{remote}\\file.bin");
                let file_guard = if strong {
                    open_base(c, &tree, &file_remote, StreamBaseKind::File, false)
                        .await?
                        .0
                } else {
                    old_attribute_guard(c, &tree, &file_remote, StreamBaseKind::File).await?
                };
                let file_result = rename_path(
                    &mut foreign,
                    &foreign_tree,
                    &file_remote,
                    &format!("{remote}\\file-moved.bin"),
                    false,
                )
                .await;
                eprintln!("FILE_BASIS_RESULT {label}: {file_result:?}");
                if strong {
                    assert!(file_result.is_err());
                } else {
                    assert!(file_result.is_ok());
                }
                tree.close_handle(c, file_guard).await.map_err(smb_error)?;
                let basis_result = rename_path(
                    &mut foreign,
                    &foreign_tree,
                    &remote,
                    &format!("{label}\\basis-moved"),
                    false,
                )
                .await;
                let parent_result = rename_path(
                    &mut foreign,
                    &foreign_tree,
                    label,
                    &format!("{label}-moved"),
                    false,
                )
                .await;
                eprintln!("BINDING_RESULT {label}: base={basis_result:?} parent={parent_result:?}");
                if strong {
                    assert!(basis_result.is_err());
                    assert!(parent_result.is_err());
                } else {
                    assert!(basis_result.is_ok());
                    assert!(parent_result.is_ok());
                }
                let after = query_handle_entry(c, &tree, base, String::new()).await?;
                assert_eq!(before.file_id, after.file_id);
                tree.close_handle(c, base).await.map_err(smb_error)?;
                close_parents(c, &tree, &mut parents).await?;
                Ok::<_, io::Error>(if strong {
                    label.to_string()
                } else {
                    format!("{label}-moved")
                })
            })?;
            drop(state);
            vfs.remove_dir_all_unchecked(Path::new(&format!("/{actual_parent}")))?;
        }
        assert!(vfs.list_complete(Path::new("/"))?.entries.is_empty());
        Ok(())
    }

    #[test]
    #[ignore = "benötigt privaten Samba und künstliche POSIX-Rechte"]
    fn smb_ordinary_stream_scope_traverse_only_ancestor() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (vfs, root) = test_provider()?;
        let other = independent_provider(&vfs)?;
        let selected = Path::new("/upper/selected");
        let directory = selected.join("basis");
        let file = selected.join("file.bin");
        let target = Path::new("/upper/target");
        for path in [Path::new("/upper"), selected, &directory, target] {
            vfs.create_dir_unchecked(path)?;
        }
        let cancel = crate::Cancel::new();
        let ordinary = vfs.content_sync_mutations(selected, &cancel, None)?;
        let scoped = ordinary.staging_scope(&file, None)?.unwrap();
        let temp = selected.join(".file-temp");
        let mut writer = scoped.open_write_new(&temp)?;
        writer.write_all(b"main")?;
        writer.flush()?;
        drop(writer);
        let mut stream = scoped.open_named_stream_write_new(&temp, b"named")?;
        stream.write_all(b"value")?;
        stream.flush()?;
        drop(stream);
        scoped.rename_noreplace(&temp, &file)?;
        scoped.finish_staging()?;
        seed_directory_stream(&vfs, &directory, b"named", b"value")?;
        // Auch eine fehlgeschlagene Assertion muss den künstlichen Vorfahren
        // wieder für den begrenzten Runner-Cleanup zugänglich machen.
        struct RestoreAncestorPermissions(std::path::PathBuf);
        impl Drop for RestoreAncestorPermissions {
            fn drop(&mut self) {
                for name in ["upper", "upper-moved"] {
                    let _ = std::fs::set_permissions(
                        self.0.join(name),
                        std::fs::Permissions::from_mode(0o755),
                    );
                }
            }
        }
        let _restore_permissions = RestoreAncestorPermissions(root.clone());
        let upper = root.join("upper");
        std::fs::set_permissions(&upper, std::fs::Permissions::from_mode(0o111))?;
        let result = (|| {
            assert!(
                vfs.list_complete(Path::new("/upper")).is_err(),
                "Künstlicher oberer Vorfahr war weiterhin auflistbar"
            );
            assert!(
                vfs.metadata_manifest(&file).is_err(),
                "Starker Aufruf wurde still herabgestuft"
            );
            assert!(
                vfs.open_named_stream_read(&directory, b"named").is_err(),
                "Starker Reader wurde still herabgestuft"
            );
            assert!(ordinary.metadata_manifest(&file)?.named_streams.is_some());
            assert!(ordinary
                .metadata_manifest(&directory)?
                .named_streams
                .is_some());
            for path in [&file, &directory] {
                let mut bytes = Vec::new();
                ordinary
                    .open_named_stream_read(path, b"named")?
                    .read_to_end(&mut bytes)?;
                assert_eq!(bytes, b"value");
            }
            let second = selected.join("second.bin");
            let writer_scope = ordinary.staging_scope(&second, None)?.unwrap();
            let temp = selected.join(".second-temp");
            let mut writer = writer_scope.open_write_new(&temp)?;
            writer.write_all(b"second")?;
            writer.flush()?;
            drop(writer);
            let mut named = writer_scope.open_named_stream_write_new(&temp, b"named")?;
            named.write_all(b"second-value")?;
            named.flush()?;
            drop(named);
            assert!(writer_scope
                .metadata_manifest(&temp)?
                .named_streams
                .is_some());
            writer_scope.rename_noreplace(&temp, &second)?;
            writer_scope.finish_staging()?;
            {
                use crate::{
                    execute_prepared_content_sync_queue_entry, prepare_content_sync_for_queue,
                    Compare, MetadataProfile, OperationQueue, ProviderEndpoint,
                    QueueExecutionResult, QueueState, SyncMode, SyncOptions,
                };
                let config = tempfile::Builder::new()
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir()?;
                let config_root = config.path().canonicalize()?;
                let queue = OperationQueue::open(config_root.join("queue.json"))?;
                let prepared = prepare_content_sync_for_queue(
                    &queue,
                    ProviderEndpoint::smb(vfs.clone(), selected)?,
                    ProviderEndpoint::smb(vfs.clone(), target)?,
                    SyncOptions {
                        mode: SyncMode::AddOnly,
                        compare: Compare::ContentHash,
                        metadata: MetadataProfile::NativeBestEffort,
                        ..SyncOptions::default()
                    },
                    &crate::Cancel::new(),
                )?;
                assert!(prepared
                    .preview()
                    .limitations
                    .iter()
                    .any(|reason| reason.contains("SMB-Quelle")
                        && reason.contains("Darüberliegende Ordner")
                        && reason.contains("Best Effort")));
                queue.enqueue(prepared.queue_operation())?;
                let entry = queue.claim_next()?.unwrap();
                let result = execute_prepared_content_sync_queue_entry(
                    &queue,
                    &entry,
                    prepared,
                    true,
                    &config_root.join("audit.jsonl"),
                    &crate::Cancel::new(),
                    &mut |_| {},
                )?;
                let QueueExecutionResult::Sync(stats) = result else {
                    panic!("Syncbilanz fehlt");
                };
                assert_eq!((stats.files_copied, stats.bytes_copied), (2, 32));
                assert_eq!(stats.directory_stream_report.completed.len(), 1);
                assert!(stats.directory_stream_report.uncertain.is_empty());
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
                assert_eq!(std::fs::read(root.join("upper/target/file.bin"))?, b"main");
                assert_eq!(
                    std::fs::read(root.join("upper/target/second.bin"))?,
                    b"second"
                );
                let target_context =
                    vfs.content_sync_mutations(target, &crate::Cancel::new(), None)?;
                let mut named = Vec::new();
                target_context
                    .open_named_stream_read(&target.join("file.bin"), b"named")?
                    .read_to_end(&mut named)?;
                assert_eq!(named, b"value");
                let mut directory_named = Vec::new();
                target_context
                    .open_named_stream_read(&target.join("basis"), b"named")?
                    .read_to_end(&mut directory_named)?;
                assert_eq!(directory_named, b"value");
                assert!(
                    vfs.metadata_manifest(&file).is_err(),
                    "Queue hat starken Reader still herabgestuft"
                );
                eprintln!("SCOPED_NATIVE_QUEUE_OK files=2 bytes=32 preview_limit=true strong_reader_still_denied=true");
            }
            let reader = ordinary.open_named_stream_read(&directory, b"named")?;
            let (tree, mut foreign) = {
                let state = lock(&other.state);
                (state.session.tree.clone(), state.session.connection.clone())
            };
            let selected_rename = other.runtime.block_on(rename_path(
                &mut foreign,
                &tree,
                "upper\\selected",
                "upper\\selected-moved",
                false,
            ));
            let base_rename = other.runtime.block_on(rename_path(
                &mut foreign,
                &tree,
                "upper\\selected\\basis",
                "upper\\selected\\basis-moved",
                false,
            ));
            assert!(selected_rename.is_err());
            assert!(base_rename.is_err());
            let upper_rename = other.runtime.block_on(rename_path(
                &mut foreign,
                &tree,
                "upper",
                "upper-moved",
                false,
            ));
            eprintln!("ORDINARY_SCOPE root={selected_rename:?} base={base_rename:?} unbound_upper={upper_rename:?}");
            upper_rename?;
            assert!(
                ordinary.metadata_manifest(&directory).is_err(),
                "Oberer Rename blieb beim frischen Rootcheck unbemerkt"
            );
            vfs.create_dir_unchecked(Path::new("/upper"))?;
            vfs.create_dir_unchecked(selected)?;
            let drift = ordinary.metadata_manifest(&directory).unwrap_err();
            assert_eq!(
                crate::error::error_code(&drift),
                crate::ErrorCode::PlanDrift
            );
            drop(reader);
            Ok(())
        })();
        for name in ["upper", "upper-moved"] {
            let path = root.join(name);
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        let cleanup = (|| {
            for name in ["/upper", "/upper-moved"] {
                match vfs.stat(Path::new(name)) {
                    Ok(_) => vfs.remove_dir_all_unchecked(Path::new(name))?,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })();
        result.and(cleanup)?;
        assert!(vfs.list_complete(Path::new("/"))?.entries.is_empty());
        Ok(())
    }
    #[test]
    #[ignore = "benötigt privaten Samba mit unabhängigen parallelen Lesesitzungen"]
    fn smb_parallel_read_capabilities_and_source_preparation() -> io::Result<()> {
        use crate::{
            prepare_content_sync_for_queue, Compare, MetadataProfile, OperationQueue,
            ProviderEndpoint, SyncMode, SyncOptions,
        };
        use std::os::unix::fs::PermissionsExt;
        let (first, root) = test_provider()?;
        let second = Arc::new(independent_provider(&first)?);
        let source = Path::new("/source");
        let target = Path::new("/target");
        let denied = Path::new("/denied");
        for path in [source, target, denied] {
            first.create_dir_unchecked(path)?;
        }
        let cancel = crate::Cancel::new();
        let ordinary = first.content_sync_mutations(source, &cancel, None)?;
        let file = source.join("file.bin");
        let temp = source.join(".temp");
        let scope = ordinary.staging_scope(&file, None)?.unwrap();
        let mut main = scope.open_write_new(&temp)?;
        main.write_all(b"main")?;
        main.flush()?;
        drop(main);
        let mut named = scope.open_named_stream_write_new(&temp, b"named")?;
        named.write_all(b"ads")?;
        named.flush()?;
        drop(named);
        scope.rename_noreplace(&temp, &file)?;
        scope.finish_staging()?;
        let reader = ordinary.open_named_stream_read(&file, b"named")?;
        let endpoint = ProviderEndpoint::smb(second.clone(), source)?;
        let read_context = endpoint.content_sync_context(false, &cancel, None)?;
        let target_context = endpoint.content_sync_context(true, &cancel, None)?;
        assert_eq!(
            target_context.metadata_capabilities(source)?.named_streams,
            Capability::Exact
        );
        eprintln!("PARALLEL_TARGET_ADD_FILE under_no_share_delete_parent=Exact");
        for (label, access) in [
            (
                "source",
                FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
            ),
            (
                "target",
                FileAccessMask::FILE_WRITE_DATA
                    | FileAccessMask::FILE_READ_ATTRIBUTES
                    | FileAccessMask::SYNCHRONIZE,
            ),
        ] {
            let (tree, mut c) = {
                let state = lock(&second.state);
                (state.session.tree.clone(), state.session.connection.clone())
            };
            let id = second.runtime.block_on(open_handle(
                &mut c,
                &tree,
                "source",
                access,
                CreateDisposition::FileOpen,
                FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            ))?;
            let bytes = second.runtime.block_on(query_info(
                &mut c,
                &tree,
                id,
                FILE_ACCESS_INFORMATION,
                4,
            ))?;
            let granted = read_u32(&bytes, 0)?;
            assert_eq!(granted & FileAccessMask::DELETE, 0);
            if label == "source" {
                assert_eq!(granted & FileAccessMask::FILE_WRITE_DATA, 0);
            } else {
                assert_ne!(granted & FileAccessMask::FILE_WRITE_DATA, 0);
            }
            eprintln!("PARALLEL_CAP_ACCESS {label} requested={access:#010x} granted={granted:#010x} delete=false");
            second
                .runtime
                .block_on(tree.close_handle(&mut c, id))
                .map_err(smb_error)?;
        }

        let (tree, mut c) = {
            let state = lock(&first.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        let base_guard = first
            .runtime
            .block_on(open_base(
                &mut c,
                &tree,
                "source",
                StreamBaseKind::Directory,
                false,
            ))?
            .0;
        let result: io::Result<()> = (|| {
            assert_eq!(
                second.metadata_capabilities(source)?.named_streams,
                Capability::ReadOnly
            );
            assert_eq!(
                read_context.metadata_capabilities(source)?.named_streams,
                Capability::ReadOnly
            );
            assert_eq!(
                read_context.metadata_manifest(&file)?.named_streams,
                Some(vec![NamedBlobSummary {
                    name: b"named".to_vec(),
                    value: BlobSummary::from_bytes(b"ads")
                }])
            );
            assert_eq!(
                target_context.metadata_manifest(&file)?.digest,
                read_context.metadata_manifest(&file)?.digest
            );
            let busy = target_context.metadata_capabilities(source).unwrap_err();
            assert_eq!(busy.kind(), io::ErrorKind::Other);
            eprintln!("TARGET_ADD_FILE while_directory_read_denies_write={busy}");
            let config = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()?;
            let config_root = config.path().canonicalize()?;
            let queue = OperationQueue::open(config_root.join("queue.json"))?;
            let prepared = prepare_content_sync_for_queue(
                &queue,
                ProviderEndpoint::smb(second.clone(), source)?,
                ProviderEndpoint::smb(second.clone(), target)?,
                SyncOptions {
                    mode: SyncMode::AddOnly,
                    compare: Compare::ContentHash,
                    metadata: MetadataProfile::NativeBestEffort,
                    ..SyncOptions::default()
                },
                &cancel,
            )?;
            assert_eq!(prepared.preview().plan.copy_count, 1);
            let (foreign_tree, mut foreign) = {
                let state = lock(&second.state);
                (state.session.tree.clone(), state.session.connection.clone())
            };
            for (from, to) in [
                ("source", "source-moved"),
                ("source\\file.bin", "source\\file-moved.bin"),
            ] {
                let attempt = second.runtime.block_on(rename_path(
                    &mut foreign,
                    &foreign_tree,
                    from,
                    to,
                    false,
                ));
                assert!(attempt.is_err());
                eprintln!("PARALLEL_RENAME_STILL_BLOCKED {from}: {attempt:?}");
            }
            eprintln!("PARALLEL_SOURCE_READ_OK capabilities=ReadOnly manifest=observed native_prepared=true target_independent=true");
            Ok(())
        })();
        first
            .runtime
            .block_on(tree.close_handle(&mut c, base_guard))
            .map_err(smb_error)?;
        result?;
        assert_eq!(
            target_context.metadata_capabilities(source)?.named_streams,
            Capability::Exact
        );
        std::fs::set_permissions(root.join("denied"), std::fs::Permissions::from_mode(0o555))?;
        let denied_result: io::Result<()> = (|| {
            let denied_context = ProviderEndpoint::smb(second.clone(), denied)?
                .content_sync_context(true, &cancel, None)?;
            assert_eq!(
                denied_context.metadata_capabilities(denied)?.named_streams,
                Capability::ReadOnly
            );
            let (tree, c) = {
                let state = lock(&second.state);
                (state.session.tree.clone(), state.session.connection.clone())
            };
            let request = CreateRequest {
                requested_oplock_level: OplockLevel::None,
                impersonation_level: ImpersonationLevel::Impersonation,
                desired_access: FileAccessMask::new(
                    FileAccessMask::FILE_WRITE_DATA
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::SYNCHRONIZE,
                ),
                file_attributes: FILE_ATTRIBUTE_NORMAL,
                share_access: ShareAccess(
                    ShareAccess::FILE_SHARE_READ
                        | ShareAccess::FILE_SHARE_WRITE
                        | ShareAccess::FILE_SHARE_DELETE,
                ),
                create_disposition: CreateDisposition::FileOpen,
                create_options: FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
                name: "denied".into(),
                create_contexts: vec![],
            };
            let frame = second
                .runtime
                .block_on(c.execute(Command::Create, &request, Some(tree.tree_id)))
                .map_err(smb_error)?;
            assert_eq!(frame.header.status, NtStatus::ACCESS_DENIED);
            eprintln!("TARGET_REAL_ADD_FILE_DENIED desired=0x00100082 status={:#010x} capability=ReadOnly",frame.header.status.0);
            Ok(())
        })();
        std::fs::set_permissions(root.join("denied"), std::fs::Permissions::from_mode(0o755))?;
        denied_result?;
        drop(reader);
        for path in [source, target, denied] {
            first.remove_dir_all_unchecked(path)?;
        }
        assert!(first.list_complete(Path::new("/"))?.entries.is_empty());
        Ok(())
    }
    #[test]
    fn ziel_add_file_nur_bekanntes_access_denied_ist_readonly() {
        use smb2::pack::{Pack, WriteCursor};
        let mut header = smb2::msg::header::Header::new_request(Command::Create);
        header.status = NtStatus::ACCESS_DENIED;
        let mut body = WriteCursor::new();
        smb2::msg::header::ErrorResponse {
            error_context_count: 0,
            error_data: vec![],
        }
        .pack(&mut body);
        let mut frame = smb2::client::connection::Frame {
            header,
            body: body.into_inner(),
            raw: vec![],
        };
        assert!(target_add_file_denied(&frame).unwrap());
        for status in [
            NtStatus(0xc0000043),
            NtStatus::NOT_SUPPORTED,
            NtStatus::UNSUCCESSFUL,
            NtStatus::INVALID_PARAMETER,
        ] {
            frame.header.status = status;
            assert!(target_add_file_denied(&frame).is_err());
        }
        frame.header.status = NtStatus::ACCESS_DENIED;
        frame.header.command = Command::QueryInfo;
        assert!(target_add_file_denied(&frame).is_err());
        frame.header.command = Command::Create;
        frame.body.clear();
        assert!(target_add_file_denied(&frame).is_err());
    }
}
