//! Gemeinsame iterative Baumbeobachtung für Inhalte und Metadaten. READDIR-EOF
//! bleibt wirkungsbezogen und erzeugt keinen vollständigen Plannerbeleg.

use super::*;
use crate::vfs::{InteractiveSourceTree, SourceReadOmission};
#[derive(Clone)]
pub(super) struct DirectoryProof {
    pub node: FileProof,
    pub observed: BTreeMap<String, Attributes>,
    pub children: Vec<PathBuf>,
}

type PreparedTree = (
    BTreeMap<PathBuf, FileProof>,
    BTreeMap<PathBuf, DirectoryProof>,
    BTreeMap<PathBuf, FileProof>,
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
) -> io::Result<BTreeMap<String, FileAttributes>> {
    let files = context.record_failure(
        super::super::directory_read::read_records(
            raw,
            remote,
            cancel,
            super::super::directory_read::DirectoryReadLimits {
                entries: usize::MAX,
                name_bytes: usize::MAX,
                pages: usize::MAX,
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
    let mut omitted_selections = BTreeMap::new();
    let mut direct_counts = BTreeMap::<String, usize>::new();
    if purpose == ReadPurpose::Contents {
        for path in items {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("SFTP-Auswahlname ist nicht UTF-8"))?;
            *direct_counts
                .entry(crate::remote_rename::folded_name(name))
                .or_default() += 1;
        }
    }
    let grouped_paths = items
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    direct_counts
                        .get(&crate::remote_rename::folded_name(name))
                        .is_some_and(|count| *count > 1)
                })
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut pending = items
        .iter()
        .rev()
        .map(|path| (path.clone(), None::<Attributes>))
        .collect::<Vec<_>>();
    let mut count = 0usize;
    let mut total_bytes = 0u64;
    while let Some((path, listed)) = pending.pop() {
        context.check_session()?;
        check_cancel(cancel)?;
        validate_content_path(&path).map_err(|error| at_path(&path, error))?;
        count += 1;
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
        if grouped_paths.contains(&path) {
            omitted.push(SourceReadOmission {
                path: path_to_text(&path)?,
                reason: "Am lokalen Ziel möglicherweise gleichwertige Case-/Unicode-Namen; gesamte direkte Namensgruppe nicht übertragen".into(),
            });
            omitted_selections.insert(path, node);
            continue;
        }
        if node.entry.is_file {
            total_bytes = total_bytes
                .checked_add(node.entry.size)
                .ok_or_else(|| invalid("SFTP-Gesamtgröße ist nicht darstellbar"))?;
            files.insert(path, node);
        } else {
            let observed = read_directory(context, raw, &node.remote, cancel)
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
            let mut name_counts = BTreeMap::<String, usize>::new();
            if purpose == ReadPurpose::Contents {
                for name in observed.keys() {
                    *name_counts
                        .entry(crate::remote_rename::folded_name(name))
                        .or_default() += 1;
                }
            }
            for (name, attributes) in observed.iter().rev() {
                if purpose == ReadPurpose::Contents
                    && name_counts
                        .get(&crate::remote_rename::folded_name(name))
                        .is_some_and(|count| *count > 1)
                {
                    // Keinen willkürlichen Gewinner übertragen. Der vollständige
                    // READDIR-Bestand bleibt trotzdem im Elternbeleg gebunden.
                    count += 1;
                    omitted.push(SourceReadOmission {
                        path: path_to_text(&path.join(name))?,
                        reason: "Am lokalen Ziel möglicherweise gleichwertige Case-/Unicode-Namen; gesamte Namensgruppe nicht übertragen".into(),
                    });
                } else {
                    pending.push((path.join(name), Some(Attributes::from(attributes))));
                }
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
    if directories.is_empty() && omitted.is_empty() {
        return Ok((files, directories, omitted_selections, None));
    }
    {
        let omitted_paths = omitted
            .iter()
            .map(|item| Path::new(&item.path))
            .collect::<std::collections::HashSet<_>>();
        for (path, directory) in &mut directories {
            directory.children = directory
                .observed
                .keys()
                .map(|name| path.join(name))
                .filter(|path| !omitted_paths.contains(path.as_path()))
                .collect();
        }
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
        .chain(
            omitted_selections
                .iter()
                .map(|(path, node)| (path, (&node.attributes, &node.chain))),
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
    Ok((files, directories, omitted_selections, Some(summary)))
}

pub(super) async fn verify_directory(
    files: &InteractiveFiles,
    directory: &DirectoryProof,
    session: &SftpSession,
    raw: &RawSftpSession,
) -> io::Result<()> {
    files.check_session()?;
    let cancel = files.cancel.clone();
    let result = async {
        let before = capture_node(
            session,
            &files.identity.canonical_root,
            &directory.node.path,
            &cancel,
            true,
        )
        .await?;
        let listing = read_directory(files, raw, &directory.node.remote, &cancel).await?;
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
    result
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
