//! Bestätigte objektweite DACL-Einzelwirkung an einer bestehenden SMB-Datei.
//! Hardlinks können auch bei gehaltenen Handles neu entstehen. Die Wirkung gilt
//! deshalb ausdrücklich für alle Namen des Objekts, auch außerhalb der Auswahl.
use crate::metadata::{BlobSummary, NamedBlobSummary};
use crate::windows_dacl::WindowsDaclWriteContext;
use crate::windows_security::WindowsDacl;
use std::io;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDaclAliasScope {
    /// Beobachtete ausgewählte Namen; dies ist ausdrücklich keine vollständige Liste.
    pub selected_paths: Vec<String>,
    pub observed_link_count: Option<u64>,
    pub all_existing_and_future_names_including_outside_root: bool,
}
impl FileDaclAliasScope {
    pub fn validate(&self) -> io::Result<()> {
        if !self.all_existing_and_future_names_including_outside_root
            || self.selected_paths.is_empty()
            || self
                .selected_paths
                .windows(2)
                .any(|paths| paths[0] >= paths[1])
            || self
                .selected_paths
                .iter()
                .any(|path| crate::path::checked_join(std::path::Path::new("/"), path).is_err())
            || self.observed_link_count == Some(0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Datei-DACL benötigt den bestätigten objektweiten Namensumfang",
            ));
        }
        Ok(())
    }
}

/// Gewünschter Rechtezustand jedes im Scan bekannten vorhandenen Zielnamens.
/// Auch ein Noop oder eine ausgelassene Quelle besitzt damit eine überprüfbare Stimme.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDaclSelection {
    pub path: String,
    pub file_id: u64,
    pub object_namespace: [u8; 32],
    pub target_proof: crate::sync::SyncTargetProof,
    pub source_metadata: Option<crate::MetadataManifest>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDaclPlan {
    pub before: WindowsDacl,
    pub after: WindowsDacl,
    pub context: WindowsDaclWriteContext,
    pub target_proof: crate::sync::SyncTargetProof,
    pub file_id: u64,
    /// Server- und Share-Namensraum; eine Dateinummer allein ist nicht global eindeutig.
    pub object_namespace: [u8; 32],
    pub content: BlobSummary,
    pub streams: Vec<NamedBlobSummary>,
    pub alias_scope: FileDaclAliasScope,
}
impl FileDaclPlan {
    pub fn validate(&self) -> io::Result<()> {
        self.context.validate()?;
        self.alias_scope.validate()?;
        if self.file_id == 0 || self.object_namespace == [0; 32] || self.before == self.after {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Datei-DACL benötigt gebundene Identität und unterschiedliche DACLs",
            ));
        }
        for acl in [&self.before, &self.after] {
            if let Some(issue) =
                acl.file_inplace_write_issue(self.context.namespace, &self.context.owner)
            {
                return Err(io::Error::new(io::ErrorKind::InvalidData, issue.reason()));
            }
        }
        Ok(())
    }
    pub fn outcome(&self, file: &str) -> io::Result<FileDaclOutcome> {
        self.validate()?;
        Ok(FileDaclOutcome {
            file: file.into(),
            file_id: self.file_id,
            object_namespace: self.object_namespace,
            before_digest: dacl_digest(&self.before)?,
            after_digest: dacl_digest(&self.after)?,
            alias_scope: self.alias_scope.clone(),
        })
    }
}
pub fn dacl_digest(acl: &WindowsDacl) -> io::Result<[u8; 32]> {
    acl.validate()?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"doppeldecker-file-dacl-v6\0");
    hash.update(&serde_json::to_vec(acl).map_err(io::Error::other)?);
    Ok(*hash.finalize().as_bytes())
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileDaclOutcome {
    pub file: String,
    pub file_id: u64,
    pub object_namespace: [u8; 32],
    pub before_digest: [u8; 32],
    pub after_digest: [u8; 32],
    pub alias_scope: FileDaclAliasScope,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileDaclReport {
    pub completed: Vec<FileDaclOutcome>,
    pub uncertain: Vec<FileDaclOutcome>,
}
impl FileDaclReport {
    pub fn is_empty(&self) -> bool {
        self.completed.is_empty() && self.uncertain.is_empty()
    }
    pub(crate) fn reconcile(&mut self, resolved: &[FileDaclOutcome], report: &Self) {
        self.uncertain.retain(|outcome| !resolved.contains(outcome));
        self.merge(report);
    }
    pub fn merge(&mut self, other: &Self) {
        for outcome in &other.completed {
            if !self.completed.contains(outcome) {
                self.completed.push(outcome.clone());
            }
        }
        for outcome in &other.uncertain {
            if !self.uncertain.contains(outcome) {
                self.uncertain.push(outcome.clone());
            }
        }
        self.uncertain
            .retain(|outcome| !self.completed.contains(outcome));
    }
}
/// apply hält das Objekt bis der Queuecallback seinen Ausgang dauerhaft gesichert
/// hat. Erst finish darf den Intent schließen; ein Abbruch dazwischen bleibt offen.
pub trait FileDaclTransaction: Send {
    fn apply(&mut self) -> io::Result<FileDaclReport>;
    fn report(&self) -> FileDaclReport;
    fn recovery_paths(&self) -> Vec<String> {
        Vec::new()
    }
    fn finish(self: Box<Self>) -> io::Result<FileDaclReport>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDaclRecoveryObservation {
    pub file_id: u64,
    pub object_namespace: [u8; 32],
    pub context: WindowsDaclWriteContext,
    pub dacl: WindowsDacl,
}

#[cfg(test)]
#[path = "file_dacl_tests.rs"]
pub(crate) mod tests;
