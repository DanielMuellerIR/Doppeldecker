//! Lokale Archiverzeugung als erste Phase eines gewöhnlichen Remoteuploads.
use crate::archive_extract::{ArchiveExtractBinding, ArchiveExtractRequest};
use crate::archive_pack::{ArchivePackBinding, ArchivePackRequest};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArchiveGeneration {
    Pack {
        request: Box<ArchivePackRequest>,
        expected: Box<ArchivePackBinding>,
    },
    Extract {
        request: Box<ArchiveExtractRequest>,
        expected: Box<ArchiveExtractBinding>,
    },
}
impl ArchiveGeneration {
    pub fn target_dir(&self) -> &Path {
        match self {
            Self::Pack { request, .. } => &request.target_dir,
            Self::Extract { request, .. } => &request.target_dir,
        }
    }
    pub fn uses_snapshot(&self) -> bool {
        matches!(self, Self::Extract { request, .. } if request.snapshot.is_some())
    }
    pub fn sandbox(&self) -> &Option<std::path::PathBuf> {
        match self {
            Self::Pack { request, .. } => &request.sandbox,
            Self::Extract { request, .. } => &request.sandbox,
        }
    }
}
