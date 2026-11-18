//! Kleiner Wiederanlaufbeleg für den laufenden Transfer einer Fensterqueue.

use super::*;
use crate::resumable_copy::{CopyCheckpoint, ResumeCopyPlan};

const MAX_RECORD_BYTES: u64 = 4096;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointRecord {
    operation: String,
    attempt: u32,
    plan_digest: [u8; 32],
    checkpoint: CopyCheckpoint,
}

fn plan_digest(plan: &ResumeCopyPlan) -> io::Result<[u8; 32]> {
    let payload = serde_json::to_vec(plan).map_err(io::Error::other)?;
    Ok(*blake3::hash(&payload).as_bytes())
}

fn record_path(journal: &Path) -> PathBuf {
    let name = journal.file_name().expect("validierter Journalname");
    let digest = blake3::hash(name.as_encoded_bytes());
    journal.with_file_name(format!(".operations-queue-resume-{digest}"))
}

fn same_stage_after(previous: &CopyCheckpoint, next: &CopyCheckpoint) -> bool {
    matches!(
        (previous, next),
        (
            CopyCheckpoint::Bytes { stage, file_id, written, sealed: false, .. },
            CopyCheckpoint::Bytes {
                stage: next_stage, file_id: next_file, written: next_written, sealed: false, ..
            }
        ) if stage == next_stage && file_id == next_file && next_written >= written
    )
}

impl CheckpointRecord {
    pub(super) fn apply(&self, snapshot: &mut QueueSnapshot) -> bool {
        let Some(entry) = snapshot.entries.iter_mut().find(|entry| {
            entry.operation_id == self.operation
                && entry.attempts == self.attempt
                && entry.state == QueueState::Running
        }) else {
            return false;
        };
        let QueueOperation::ResumableCopy {
            plan,
            checkpoint: Some(previous),
        } = &mut entry.operation
        else {
            return false;
        };
        if self.checkpoint.bytes() <= previous.bytes()
            || !same_stage_after(previous, &self.checkpoint)
            || plan_digest(plan).ok() != Some(self.plan_digest)
            || plan.validate(Some(&self.checkpoint)).is_err()
        {
            return false;
        }
        entry.progress.bytes_done = self.checkpoint.bytes();
        **previous = self.checkpoint.clone();
        true
    }
}

pub(super) fn recover(
    root: &crate::local_fs::RootHandle,
    journal: &Path,
    snapshot: &mut QueueSnapshot,
) -> io::Result<bool> {
    let path = record_path(journal);
    let file = match crate::local_fs::open_read_beneath(root, &path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Beleg der Teilkopie hat kein gültiges Dateiformat",
        ));
    }
    let record: CheckpointRecord = serde_json::from_reader(file.take(MAX_RECORD_BYTES))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !root.is_current()? {
        return Err(io::Error::other("Queueordner wurde ausgetauscht"));
    }
    Ok(record.apply(snapshot))
}

impl OperationQueue {
    pub(super) fn write_resume_progress(
        &self,
        operation: &str,
        plan: &ResumeCopyPlan,
        checkpoint: CopyCheckpoint,
    ) -> io::Result<()> {
        let journal = self.journal.lock().unwrap();
        self.require_current_config()?;
        if self.journal_outcome_unclear.load(Ordering::Acquire) {
            return Err(crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Queuejournal ist nicht sicher gebunden",
            ));
        }
        let entry = journal
            .entries
            .iter()
            .find(|entry| entry.operation_id == operation)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Queue-Eintrag nicht gefunden")
            })?;
        require_state(entry, QueueState::Running)?;
        let QueueOperation::ResumableCopy {
            plan: current,
            checkpoint: Some(base),
        } = &entry.operation
        else {
            return Err(crate::real_mode::plan_drift(
                "Teilkopieauftrag hat sich geändert",
            ));
        };
        plan.validate(Some(&checkpoint))?;
        let mut latest = self.resume_progress.lock().unwrap();
        let previous = latest
            .as_ref()
            .filter(|record| record.operation == operation && record.attempt == entry.attempts)
            .map_or(base.as_ref(), |record| &record.checkpoint);
        if current.as_ref() != plan || !same_stage_after(previous, &checkpoint) {
            return Err(crate::real_mode::plan_drift(
                "Beleg der Teilkopie hat sich geändert",
            ));
        }
        let record = CheckpointRecord {
            operation: operation.into(),
            attempt: entry.attempts,
            plan_digest: plan_digest(plan)?,
            checkpoint,
        };
        let payload = serde_json::to_vec(&record).map_err(io::Error::other)?;
        if payload.len() as u64 > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Beleg der Teilkopie ist zu groß",
            ));
        }
        // Der Executor hat die Nutzdaten bereits synchronisiert. Erst der
        // dauerhafte Beleg darf in der Queue als gesichert erscheinen.
        write_queue_payload(
            &self._process_lock.root,
            &record_path(&self.path),
            &payload,
            ".operations-queue-resume-tmp-",
            AtomicWriteMode::Replace,
        )?;
        self.require_current_config()?;
        *latest = Some(record);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resumable_copy::prepare_resume_copies;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn resumable_checkpoint_survives_a_crash_without_rewriting_queue_history() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("probe");
        let destination = root.join("ziel");
        std::fs::create_dir(&destination).unwrap();
        const MIB: u64 = 1024 * 1024;
        let payload = vec![23; (10 * MIB) as usize];
        std::fs::write(&source, &payload).unwrap();
        let plan = prepare_resume_copies(&[source], &destination, None)
            .unwrap()
            .remove(0);
        let target = destination.join("probe");
        let journal = root.join("queue.json");
        let execution = LocalQueueExecution {
            sandbox: None,
            sync_quarantine: None,
            max_delete_entries: 0,
            max_delete_fraction: 0.0,
        };
        let id;
        {
            let queue = OperationQueue::open(&journal).unwrap();
            let entry = queue
                .enqueue(QueueOperation::ResumableCopy {
                    plan: Box::new(plan),
                    checkpoint: None,
                })
                .unwrap();
            id = entry.operation_id.clone();
            queue
                .transact(|snapshot| {
                    for number in 0..128 {
                        let mut previous = entry.clone();
                        previous.operation_id = format!("vorher-{number}");
                        previous.state = QueueState::Completed;
                        snapshot.entries.push(previous);
                    }
                    Ok(())
                })
                .unwrap();
            let claimed = queue.claim_next().unwrap().unwrap();
            let mut before = None;
            let aborted = catch_unwind(AssertUnwindSafe(|| {
                execute_local_queue_entry(
                    &queue,
                    claimed,
                    &execution,
                    &Cancel::new(),
                    &mut |progress| {
                        if progress.bytes_done == 0 {
                            before = Some(std::fs::read(&journal).unwrap());
                        }
                        if progress.bytes_done == 9 * MIB {
                            panic!("simulierter Prozessabbruch vor dem nächsten Checkpoint");
                        }
                    },
                )
                .unwrap();
            }));
            assert!(aborted.is_err());
            let before = before.unwrap();
            assert_eq!(std::fs::read(&journal).unwrap(), before);
            let record_bytes = std::fs::read(record_path(&journal)).unwrap();
            assert!(record_bytes.len() * 100 < before.len());
            let record: CheckpointRecord = serde_json::from_slice(&record_bytes).unwrap();
            let mut later: QueueSnapshot = serde_json::from_slice(&before).unwrap();
            later.entries[0].attempts += 1;
            assert!(!record.apply(&mut later));
            assert_eq!(later.entries[0].progress.bytes_done, 0);
            assert_eq!(queue.snapshot().entries[0].progress.bytes_done, 8 * MIB);
            assert!(!target.exists());
        }
        let queue = OperationQueue::open(&journal).unwrap();
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.entries.len(), 129);
        assert_eq!(snapshot.entries[0].state, QueueState::Interrupted);
        assert_eq!(snapshot.entries[0].progress.bytes_done, 8 * MIB);
        queue.retry(&id).unwrap();
        let mut first = None;
        execute_local_queue_entry(
            &queue,
            queue.claim_next().unwrap().unwrap(),
            &execution,
            &Cancel::new(),
            &mut |progress| {
                first.get_or_insert(progress.bytes_done);
            },
        )
        .unwrap();
        assert_eq!(first, Some(8 * MIB));
        assert_eq!(std::fs::read(target).unwrap(), payload);
        assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    }
}
