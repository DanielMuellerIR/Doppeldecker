//! Bestätigte Einzelwirkungen an bestehenden Verzeichnis-DACLs.
//! Der Ordner bleibt bestehen; ein unbekannter SET-Ausgang ist kein Temp-Cleanup.
use crate::sync::SyncTargetProof;
use crate::windows_dacl::WindowsDaclWriteContext;
use crate::windows_security::WindowsDacl;
use std::io;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryDaclPlan {
    pub before: WindowsDacl,
    pub after: WindowsDacl,
    pub context: WindowsDaclWriteContext,
    pub target_proof: Option<SyncTargetProof>,
}
impl DirectoryDaclPlan {
    pub fn validate(&self) -> io::Result<()> {
        self.before.validate()?;
        self.after.validate()?;
        self.context.validate()?;
        if self.before.namespace != self.context.namespace
            || self.after.namespace != self.context.namespace
            || self.before == self.after
            || self.target_proof.is_none()
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                "Verzeichnis-DACL benötigt zwei gebundene unterschiedliche Werte und einen Zielbeleg"));
        }
        if let Some(issue) = self
            .before
            .directory_write_issue(self.context.namespace, &self.context.owner)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, issue.reason()));
        }
        if let Some(issue) = self
            .after
            .directory_write_issue(self.context.namespace, &self.context.owner)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, issue.reason()));
        }
        Ok(())
    }
    pub fn outcome(&self, directory: &str) -> io::Result<DirectoryDaclOutcome> {
        self.validate()?;
        Ok(DirectoryDaclOutcome {
            directory: directory.into(),
            before_digest: dacl_digest(&self.before)?,
            after_digest: dacl_digest(&self.after)?,
        })
    }
}

pub fn dacl_digest(acl: &WindowsDacl) -> io::Result<[u8; 32]> {
    acl.validate()?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"doppeldecker-directory-dacl-v1\0");
    hash.update(&serde_json::to_vec(acl).map_err(io::Error::other)?);
    Ok(*hash.finalize().as_bytes())
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryDaclOutcome {
    pub directory: String,
    pub before_digest: [u8; 32],
    pub after_digest: [u8; 32],
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryDaclReport {
    pub completed: Vec<DirectoryDaclOutcome>,
    pub uncertain: Vec<DirectoryDaclOutcome>,
}
impl DirectoryDaclReport {
    pub fn is_empty(&self) -> bool {
        self.completed.is_empty() && self.uncertain.is_empty()
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
    pub fn at_directory(mut self, directory: &str) -> Self {
        for outcome in self.completed.iter_mut().chain(&mut self.uncertain) {
            outcome.directory = directory.into();
        }
        self
    }
}

/// Der gebundene Ordner bleibt bis zum bestätigten Bericht gehalten. Zwischen
/// apply und finish kann die Fensterqueue den bekannten Ausgang dauerhaft sichern.
pub trait DirectoryDaclTransaction: Send {
    fn apply(&mut self) -> io::Result<DirectoryDaclReport>;
    fn report(&self) -> DirectoryDaclReport;
    fn recovery_paths(&self) -> Vec<String> {
        Vec::new()
    }
    fn finish(self: Box<Self>) -> io::Result<DirectoryDaclReport>;
}
