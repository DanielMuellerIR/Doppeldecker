//! Gemeinsame begrenzte Baumbeobachtung für Inhalte und Metadaten. READDIR-EOF
//! bleibt wirkungsbezogen und erzeugt keinen vollständigen Plannerbeleg.

use super::*;
use crate::vfs::{InteractiveSourceTree, SourceReadOmission};
use std::time::Duration;

pub(super) const PREPARATION_LIMIT: Duration = Duration::from_secs(60);
const MAX_ENTRIES: usize = 1000;
const MAX_DEPTH: usize = 16;
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct DirectoryProof {
    pub node: FileProof,
    pub observed: BTreeMap<String, Attributes>,
    pub children: Vec<PathBuf>,
}

type PreparedTree = (
    BTreeMap<PathBuf, FileProof>,
    BTreeMap<PathBuf, DirectoryProof>,
    Option<TreeSummary>,
);

pub(super) struct TreeSummary {
    pub digest: String,
    pub entries: u64,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub omitted: Vec<SourceReadOmission>,
}

impl TreeSummary {
    pub(super) fn into_interactive(self) -> io::Result<InteractiveSourceTree> {
        let result = InteractiveSourceTree {
            digest: self.digest,
            entries: self.entries,
            files: self.files,
            directories: self.directories,
            bytes: self.bytes,
            omitted: self.omitted,
        };
        result.validate()?;
        Ok(result)
    }
}

async fn read_directory(
    context: &InteractiveFiles,
    raw: &RawSftpSession,
    remote: &str,
    cancel: &Cancel,
    purpose: ReadPurpose,
) -> io::Result<BTreeMap<String, FileAttributes>> {
    let files = context.record_failure(
        super::super::directory_read::read_records(
            raw,
            remote,
            cancel,
            super::super::directory_read::DirectoryReadLimits {
                entries: MAX_ENTRIES,
                name_bytes: 1024 * 1024,
                pages: MAX_ENTRIES + 3,
                duplicate_error_kind: io::ErrorKind::InvalidInput,
            },
            |result| context.record_failure(result),
        )
        .await,
    )?;
    let mut entries = BTreeMap::new();
    for (name, attributes) in files {
        check_cancel(cancel)?;
        validate_remote_name(&name)?;
        if purpose == ReadPurpose::Contents
            && entries
                .keys()
                .any(|existing: &String| crate::remote_rename::names_may_collide(existing, &name))
        {
            return Err(invalid("SFTP-Ordner enthält doppelte oder am lokalen Ziel möglicherweise gleichgesetzte Namen"));
        }
        entries.insert(name, attributes);
    }
    Ok(entries)
}

pub(super) async fn prepare(
    context: &InteractiveFiles,
    session: &SftpSession,
    raw: &RawSftpSession,
    root: &Path,
    items: &[PathBuf],
    cancel: &Cancel,
    purpose: ReadPurpose,
) -> io::Result<PreparedTree> {
    let identity = &context.identity;
    let account_digest = context.provider.account_digest();
    let mut files = BTreeMap::new();
    let mut directories = BTreeMap::new();
    let mut omitted = Vec::new();
    let mut pending = items
        .iter()
        .rev()
        .map(|path| (path.clone(), 0usize, None::<Attributes>))
        .collect::<Vec<_>>();
    let mut count = 0usize;
    let mut total_bytes = 0u64;
    while let Some((path, depth, listed)) = pending.pop() {
        context.check_session()?;
        check_cancel(cancel)?;
        validate_content_path(&path).map_err(|error| at_path(&path, error))?;
        count += 1;
        if count > MAX_ENTRIES || depth > MAX_DEPTH {
            return Err(invalid(
                "SFTP-Auswahl überschreitet 1000 Einträge oder 16 Unterordnerebenen",
            ));
        }
        let node = capture_node_for(
            session,
            &identity.canonical_root,
            &path,
            cancel,
            true,
            purpose,
        )
        .await
        .map_err(|error| at_path(&path, error));
        let node = match node {
            Ok(node) => node,
            Err(error)
                if listed.is_some() && crate::error_code(&error) == ErrorCode::Unsupported =>
            {
                omitted.push(SourceReadOmission {
                    path: path_to_text(&path)?,
                    reason: error.to_string(),
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        if listed
            .as_ref()
            .is_some_and(|attributes| !attributes.agrees_with(&node.attributes))
        {
            return Err(drift(
                "SFTP-Kindattribute änderten sich zwischen READDIR und LSTAT",
            ));
        }
        if node.entry.is_file {
            total_bytes = total_bytes
                .checked_add(node.entry.size)
                .ok_or_else(|| invalid("SFTP-Gesamtgröße ist nicht darstellbar"))?;
            files.insert(path, node);
        } else {
            let observed = read_directory(context, raw, &node.remote, cancel, purpose)
                .await
                .map_err(|error| at_path(&path, error))?;
            let fresh = capture_node_for(
                session,
                &identity.canonical_root,
                &path,
                cancel,
                true,
                purpose,
            )
            .await?;
            if fresh.attributes != node.attributes || fresh.chain != node.chain {
                return Err(drift("SFTP-Ordner änderte sich während READDIR"));
            }
            for (name, attributes) in observed.iter().rev() {
                if count.saturating_add(pending.len()) >= MAX_ENTRIES {
                    return Err(invalid("SFTP-Auswahl überschreitet 1000 Einträge"));
                }
                pending.push((
                    path.join(name),
                    depth + 1,
                    Some(Attributes::from(attributes)),
                ));
            }
            directories.insert(
                path,
                DirectoryProof {
                    node,
                    observed: observed
                        .iter()
                        .map(|(name, attributes)| (name.clone(), Attributes::from(attributes)))
                        .collect(),
                    children: Vec::new(),
                },
            );
        }
    }
    if directories.is_empty() {
        return Ok((files, directories, None));
    }
    if purpose == ReadPurpose::Contents && total_bytes > MAX_TOTAL_BYTES {
        return Err(invalid(
            "Rekursiver SFTP-Download überschreitet 16 GiB Gesamtinhalt",
        ));
    }
    for (path, directory) in &mut directories {
        directory.children = directory
            .observed
            .keys()
            .map(|name| path.join(name))
            .filter(|path| !omitted.iter().any(|item| Path::new(&item.path) == path))
            .collect();
    }
    omitted.sort_by(|a, b| a.path.cmp(&b.path));
    // Weder Generation noch flüchtige Handles gelangen in den Neustartbeleg.
    // Identität, Auswahl, beobachtete Metadaten und ausgelassene Namen dagegen schon.
    let nodes = files
        .iter()
        .map(|(path, node)| (path, (&node.attributes, &node.chain)))
        .chain(
            directories
                .iter()
                .map(|(path, node)| (path, (&node.node.attributes, &node.node.chain))),
        )
        .collect::<BTreeMap<_, _>>();
    let listings = directories
        .iter()
        .map(|(path, node)| (path, &node.observed))
        .collect::<BTreeMap<_, _>>();
    let digest = if purpose == ReadPurpose::Contents {
        blake3::hash(
            &serde_json::to_vec(&(
                identity,
                account_digest,
                root,
                items,
                nodes,
                listings,
                &omitted,
            ))
            .map_err(io::Error::other)?,
        )
        .to_hex()
        .to_string()
    } else {
        String::new()
    };
    let summary = TreeSummary {
        digest,
        entries: count as u64,
        files: files.len() as u64,
        directories: directories.len() as u64,
        bytes: total_bytes,
        omitted,
    };
    Ok((files, directories, Some(summary)))
}

pub(super) async fn verify_directory(
    files: &InteractiveFiles,
    directory: &DirectoryProof,
    session: &SftpSession,
    raw: &RawSftpSession,
) -> io::Result<()> {
    files.check_session()?;
    let cancel = files
        .cancel
        .until(std::time::Instant::now() + PREPARATION_LIMIT);
    let result = async {
        let before = capture_node(
            session,
            &files.identity.canonical_root,
            &directory.node.path,
            &cancel,
            true,
        )
        .await?;
        let listing = read_directory(
            files,
            raw,
            &directory.node.remote,
            &cancel,
            ReadPurpose::Contents,
        )
        .await?;
        let after = capture_node(
            session,
            &files.identity.canonical_root,
            &directory.node.path,
            &cancel,
            true,
        )
        .await?;
        let attributes = listing
            .iter()
            .map(|(name, attrs)| (name.clone(), Attributes::from(attrs)))
            .collect::<BTreeMap<_, _>>();
        if before.chain != directory.node.chain
            || after.chain != directory.node.chain
            || before.attributes != directory.node.attributes
            || after.attributes != directory.node.attributes
            || attributes != directory.observed
        {
            return Err(drift(
                "SFTP-Baum oder Ordnerbestand änderte sich seit der Vorbereitung",
            ));
        }
        files.check_session()
    }
    .await;
    limited_result(
        result,
        &cancel,
        &files.cancel,
        "SFTP-Ordnerprüfung überschritt die Frist von 60 Sekunden",
    )
}

fn at_path(path: &Path, error: io::Error) -> io::Error {
    coded_error(
        error.kind(),
        crate::error_code(&error),
        format!("SFTP-Baum {}: {error}", path.display()),
    )
}

fn path_to_text(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("SFTP-Baumpfad ist nicht UTF-8"))
}
