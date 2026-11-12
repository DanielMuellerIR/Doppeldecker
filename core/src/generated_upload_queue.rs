//! Erneute Konfliktentscheidung erhält die bereits erzeugte, versiegelte Quelle.
use super::*;
use crate::generated_source::{GeneratedSourceStore, ResolvedGeneratedSource};

pub struct GeneratedUploadPreparation {
    pub entry: QueueEntry,
    pub source: ResolvedGeneratedSource,
}
impl OperationQueue {
    /// Nur dauerhafter Erfolg gibt Ergebnisdaten frei. Abbruch, Fehler und
    /// Konfliktpausen behalten dieselben Bytes für ihren ausdrücklichen Retry.
    pub(super) fn cleanup_completed_generated_source(&self, operation_id: &str) {
        let Some(entry) = self.snapshot().entries.into_iter().find(|entry| {
            entry.operation_id == operation_id && entry.state == QueueState::Completed
        }) else {
            return;
        };
        let Some(reference) = &entry.generated_source else {
            return;
        };
        let store = GeneratedSourceStore::for_queue(self);
        let Ok(content) = reference.path_in(self._process_lock.root.path()) else {
            return;
        };
        let mut related = vec![content.clone(), content.parent().unwrap().to_path_buf()];
        if let Ok(store) = &store {
            if let Ok(claim) = store.cleanup_path(&reference.workspace_reference()) {
                related.push(claim);
            }
        }
        let result = store.and_then(|store| store.remove_source(reference));
        let mut recovery = result
            .as_ref()
            .err()
            .map(crate::error::recovery_paths)
            .unwrap_or_default();
        if result.is_err() && recovery.is_empty() {
            recovery.push(content.to_string_lossy().into_owned());
        }
        let related: Vec<_> = related
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let mut desired: Vec<_> = entry
            .temp_recovery_paths
            .iter()
            .filter(|path| !related.contains(path))
            .cloned()
            .collect();
        for path in recovery {
            if !desired.contains(&path) {
                desired.push(path);
            }
        }
        if desired == entry.temp_recovery_paths {
            return;
        }
        // Nur die Pfade dieser Quelle abgleichen; fremde Prüfpfade bleiben erhalten.
        let _ = self.update(operation_id, |entry| {
            entry
                .temp_recovery_paths
                .retain(|path| !related.contains(path));
            for path in &desired {
                if !entry.temp_recovery_paths.contains(path) {
                    entry.temp_recovery_paths.push(path.clone());
                }
            }
            Ok(())
        });
    }

    pub(super) fn cleanup_removed_generated_source(&self, entry: &QueueEntry) -> io::Result<()> {
        let reference = entry
            .generated_source
            .as_ref()
            .map(|source| source.workspace_reference())
            .or_else(|| match &entry.operation {
                QueueOperation::ArchiveUpload { workspace, .. } => Some(workspace.clone()),
                _ => None,
            });
        let Some(reference) = reference else {
            return Ok(());
        };
        let store = GeneratedSourceStore::for_queue(self)?;
        let result = if let Some(source) = &entry.generated_source {
            store.remove_source(source)
        } else {
            store.remove_empty_workspace(&reference)
        };
        result.map_err(|error| {
            let mut paths = crate::error::recovery_paths(&error);
            if paths.is_empty() {
                paths.push(reference.path_in(self._process_lock.root.path())
                    .unwrap_or_default().to_string_lossy().into_owned());
            }
            crate::error::with_recovery_paths(
                io::Error::new(
                    error.kind(),
                    format!(
                        "Auftrag nicht entfernt; lokaler Zwischenstand zur Prüfung erhalten: {error}"
                    ),
                ),
                &paths,
            )
        })
    }

    pub(super) fn recover_generated_cleanup(&self) -> io::Result<()> {
        let entries = self.snapshot().entries;
        for entry in entries {
            let reference = entry
                .generated_source
                .as_ref()
                .map(|source| source.workspace_reference())
                .or_else(|| match &entry.operation {
                    QueueOperation::ArchiveUpload { workspace, .. } => Some(workspace.clone()),
                    _ => None,
                });
            let Some(reference) = reference else {
                continue;
            };
            let store = GeneratedSourceStore::for_queue(self)?;
            let Some(path) = store.cleanup_recovery_path(&reference)? else {
                continue;
            };
            let path = path.to_string_lossy().into_owned();
            self.update(&entry.operation_id, |entry| {
                let content = reference.path_in(self._process_lock.root.path())?;
                let workspace = content.parent().unwrap();
                entry.temp_recovery_paths.retain(|item| Path::new(item) != content && Path::new(item) != workspace);
                if !entry.temp_recovery_paths.contains(&path) {
                    entry.temp_recovery_paths.push(path.clone());
                }
                if !matches!(entry.state, QueueState::Completed | QueueState::Cancelled) {
                    entry.state = QueueState::Failed;
                    entry.attention = None;
                    entry.error_code = Some(crate::ErrorCode::OutcomeUnclear);
                    entry.error = Some("Unterbrochene Bereinigung; privaten Rest manuell prüfen, kein erneuter Upload".into());
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn prepare_generated_upload(
        &self,
        operation_id: &str,
        cancel: &Cancel,
    ) -> io::Result<GeneratedUploadPreparation> {
        let entry = self
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == operation_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Archivupload gehört nicht zu dieser Fensterqueue",
                )
            })?;
        require_state(&entry, QueueState::AwaitingConfirmation)?;
        if !matches!(
            entry.attention,
            Some(QueueAttention::ConflictsChanged { .. })
        ) {
            return Err(crate::real_mode::plan_drift(
                "Keine offene Konfliktentscheidung für diesen Archivupload",
            ));
        }
        let reference = entry
            .generated_source
            .as_ref()
            .ok_or_else(|| crate::real_mode::plan_drift("Keine erzeugte Archivquelle"))?;
        let (source, _) =
            resolve_generated_operation_source(self, reference, &entry.operation, cancel)?;
        Ok(GeneratedUploadPreparation { entry, source })
    }

    pub fn resume_generated_upload(
        &self,
        previous: &QueueEntry,
        mut operation: QueueOperation,
        profile_bindings: Vec<crate::RemoteProfileBinding>,
        cancel: &Cancel,
    ) -> io::Result<QueueEntry> {
        require_state(previous, QueueState::AwaitingConfirmation)?;
        if !matches!(
            previous.attention,
            Some(QueueAttention::ConflictsChanged { .. })
        ) {
            return Err(crate::real_mode::plan_drift(
                "Archivupload wartet nicht auf Zielkonflikte",
            ));
        }
        let reference = previous
            .generated_source
            .as_ref()
            .ok_or_else(|| crate::real_mode::plan_drift("Ergebnisbeleg fehlt"))?;
        validate_enqueue_operation(&operation)?;
        validate_profile_bindings(&profile_bindings)?;
        if profile_bindings != previous.profile_bindings {
            return Err(crate::real_mode::plan_drift(
                "Archivupload darf nicht auf ein anderes Profil wechseln",
            ));
        }
        let (
            QueueOperation::ProviderFile {
                target: old_target,
                destination: old_destination,
                ..
            },
            QueueOperation::ProviderFile {
                target,
                destination,
                ..
            },
        ) = (&previous.operation, &operation)
        else {
            return Err(crate::real_mode::plan_drift(
                "Keine gewöhnliche Uploadabsicht",
            ));
        };
        if target != old_target || destination != old_destination {
            return Err(crate::real_mode::plan_drift(
                "Archivupload darf nicht auf ein anderes Ziel wechseln",
            ));
        }
        let (_held, source_vfs) =
            resolve_generated_operation_source(self, reference, &operation, cancel)?;
        if let QueueOperation::ProviderFile {
            source_tree_expected,
            ..
        } = &mut operation
        {
            *source_tree_expected = source_vfs.interactive_source_tree()?.map(Box::new);
        }
        self.transact(|journal| {
            let entry = find_mut(journal, &previous.operation_id)?;
            require_state(entry, QueueState::AwaitingConfirmation)?;
            if entry.operation != previous.operation
                || entry.attention != previous.attention
                || entry.generated_source != previous.generated_source
                || entry.profile_bindings != previous.profile_bindings
            {
                return Err(crate::real_mode::plan_drift(
                    "Archivupload änderte sich während der Konfliktentscheidung",
                ));
            }
            entry.operation = operation;
            entry.profile_bindings = profile_bindings;
            entry.state = QueueState::Pending;
            entry.attention = None;
            entry.error = None;
            entry.error_code = None;
            entry.progress = Progress::default();
            validate_generated_entry(entry, self._process_lock.root.path())?;
            Ok(entry.clone())
        })
    }

    /// Zeigt nur einen bekannten privaten Arbeitsordner. Ein beschädigter Inhalt
    /// muss für diese Navigation nicht als ausführbare Uploadquelle gelten.
    pub fn generated_source_path(&self, operation_id: &str) -> io::Result<PathBuf> {
        let entry = self
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.operation_id == operation_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Archivauftrag gehört nicht zu dieser Fensterqueue",
                )
            })?;
        validate_generated_entry(&entry, self._process_lock.root.path())?;
        let reference = if let Some(reference) = &entry.generated_source {
            reference.workspace_reference()
        } else if let QueueOperation::ArchiveUpload { workspace, .. } = &entry.operation {
            workspace.clone()
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Kein erzeugter Zwischenstand",
            ));
        };
        let store = GeneratedSourceStore::for_queue(self)?;
        if let Some(path) = store.cleanup_recovery_path(&reference)? {
            return Ok(path);
        }
        let workspace = store.open_workspace(&reference)?;
        Ok(workspace.path().to_path_buf())
    }
}
