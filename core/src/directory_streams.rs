//! Einzelwirkungen benannter Verzeichnisströme. Der Ordner selbst wird nie als
//! Tempdatei behandelt; abgeschlossene Teilwirkungen bleiben nach Fehlern sichtbar.
use crate::metadata::{validate_named_stream_name, NamedBlobSummary};
use std::io::{self, Write};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DirectoryStreamEffect {
    Add {
        after: NamedBlobSummary,
    },
    Replace {
        before: NamedBlobSummary,
        after: NamedBlobSummary,
    },
    Remove {
        before: NamedBlobSummary,
    },
}
impl DirectoryStreamEffect {
    pub fn name(&self) -> &[u8] {
        match self {
            Self::Add { after } | Self::Replace { after, .. } => &after.name,
            Self::Remove { before } => &before.name,
        }
    }
    pub fn after(&self) -> Option<&NamedBlobSummary> {
        match self {
            Self::Add { after } | Self::Replace { after, .. } => Some(after),
            Self::Remove { .. } => None,
        }
    }
    pub fn before(&self) -> Option<&NamedBlobSummary> {
        match self {
            Self::Replace { before, .. } | Self::Remove { before } => Some(before),
            Self::Add { .. } => None,
        }
    }
    pub fn transfer_bytes(&self) -> u64 {
        self.after().map_or(0, |value| value.value.size)
    }
    pub fn validate(&self) -> io::Result<()> {
        validate_named_stream_name(self.name())?;
        if let Self::Replace { before, after } = self {
            if before.name != after.name || before == after {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Ungültige Verzeichnisstream-Ersetzung",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryStreamOutcome {
    pub directory: String,
    pub effect: DirectoryStreamEffect,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryStreamReport {
    pub completed: Vec<DirectoryStreamOutcome>,
    pub uncertain: Vec<DirectoryStreamOutcome>,
}
impl DirectoryStreamReport {
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryStreamPlan {
    pub effects: Vec<DirectoryStreamEffect>,
    pub target_proof: Option<crate::sync::SyncTargetProof>,
}

/// Nur der ununterbrochen gehaltene Creator darf committen oder vor dem Rename
/// abbrechen. `flush` bestätigt Daten und Readback, schließt den Handle aber nicht.
pub trait DirectoryStreamTemp: Write + Send {
    fn commit(
        self: Box<Self>,
        before: Option<&NamedBlobSummary>,
    ) -> io::Result<DirectoryStreamEffect>;
    fn abort(self: Box<Self>) -> io::Result<()>;
}
pub trait DirectoryStreamTransaction: Send {
    fn stage(
        &mut self,
        desired: &NamedBlobSummary,
    ) -> io::Result<Box<dyn DirectoryStreamTemp + '_>>;
    fn remove(&mut self, before: &NamedBlobSummary) -> io::Result<DirectoryStreamEffect>;
    fn report(&self) -> DirectoryStreamReport;
    fn recovery_paths(&self) -> Vec<String> {
        Vec::new()
    }
    fn finish(
        self: Box<Self>,
        expected_final: &[NamedBlobSummary],
    ) -> io::Result<DirectoryStreamReport>;
}

pub fn plan_effects(
    source: &[NamedBlobSummary],
    target: &[NamedBlobSummary],
) -> io::Result<Vec<DirectoryStreamEffect>> {
    fn map(
        values: &[NamedBlobSummary],
    ) -> io::Result<std::collections::BTreeMap<Vec<u8>, &NamedBlobSummary>> {
        let mut map = std::collections::BTreeMap::new();
        for value in values {
            validate_named_stream_name(&value.name)?;
            if map.insert(value.name.clone(), value).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Doppelter Verzeichnisstreamname",
                ));
            }
        }
        Ok(map)
    }
    let source = map(source)?;
    let target = map(target)?;
    let mut effects = Vec::new();
    for (name, after) in &source {
        match target.get(name) {
            None => effects.push(DirectoryStreamEffect::Add {
                after: (*after).clone(),
            }),
            Some(before) if before != after => effects.push(DirectoryStreamEffect::Replace {
                before: (*before).clone(),
                after: (*after).clone(),
            }),
            _ => {}
        }
    }
    for (name, before) in &target {
        if !source.contains_key(name) {
            effects.push(DirectoryStreamEffect::Remove {
                before: (*before).clone(),
            });
        }
    }
    Ok(effects)
}
