//! Klassifiziert persistente Laufjournale und räumt nachgewiesene Abschlüsse auf.

use std::io;
use std::path::Path;

use super::{
    bidirectional_effect_error, build_tree_snapshot, invalid_data, invalid_input, BaselineStore,
    BidirectionalBaseline, BidirectionalProjectionState, BidirectionalRunJournal,
    BidirectionalRunState, EndpointBinding, SnapshotLimits,
};
use crate::metadata::MetadataProfile;

pub(super) fn reject_completed_run_recovery(journal: &BidirectionalRunJournal) -> io::Result<()> {
    if matches!(
        journal.state,
        BidirectionalRunState::BaselineCommitted | BidirectionalRunState::RollbackCommitted
    ) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Datenwirkung ist bereits abgeschlossen; Journal braucht ausschließlich Abschluss-Recovery",
        ));
    }
    Ok(())
}

pub fn load_bidirectional_run_journal(
    baseline_path: &Path,
) -> io::Result<Option<BidirectionalRunJournal>> {
    BaselineStore::open(baseline_path)?.load_run_journal()
}

/// Maschinenlesbare Einordnung eines persistenten Laufjournals. Nach vollständig
/// bestätigter Daten- und Baselinewirkung darf die Queue ausschließlich das
/// mutationslose Journal-Cleanup anbieten, niemals Resume oder Rollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BidirectionalRunRecovery {
    Absent,
    ResumeOrRollback,
    CompletionCleanup,
    RollbackCompletionCleanup,
}

fn run_completion_shape_is_bound(journal: &BidirectionalRunJournal) -> bool {
    journal
        .projections
        .iter()
        .all(|projection| projection.state == BidirectionalProjectionState::Committed)
        && matches!(
            journal.state,
            BidirectionalRunState::Running | BidirectionalRunState::BaselineCommitted
        )
        && journal.rollback_step.is_none()
        && journal.type_swap_step.is_none()
}

fn run_completion_recovery_is_bound(
    journal: &BidirectionalRunJournal,
    baseline: Option<&BidirectionalBaseline>,
) -> bool {
    if !run_completion_shape_is_bound(journal) {
        return false;
    }
    match journal.committed_baseline_digest {
        Some(expected) => baseline.is_some_and(|baseline| baseline.body_digest == expected),
        None => {
            // Version 7 schrieb den Folgedigest noch nicht. Der erst nach dem
            // Baseline-Rename persistierte Zustand `BaselineCommitted` beweist
            // trotzdem, dass kein Datenplan mehr fortgesetzt werden darf. Die
            // aktuelle Baseline wird beim tatsächlichen Cleanup zusätzlich an
            // Paar und beide Endpunkte gebunden.
            journal.state == BidirectionalRunState::BaselineCommitted
        }
    }
}

fn rollback_completion_shape_is_bound(journal: &BidirectionalRunJournal) -> bool {
    journal.state == BidirectionalRunState::RollbackCommitted
        && journal.committed_baseline_digest.is_none()
        && journal
            .projections
            .iter()
            .all(|projection| projection.state == BidirectionalProjectionState::Prepared)
        && journal.rollback_step.is_none()
        && journal.type_swap_step.is_none()
}

fn rollback_completion_recovery_is_bound(
    journal: &BidirectionalRunJournal,
    baseline: Option<&BidirectionalBaseline>,
) -> bool {
    rollback_completion_shape_is_bound(journal)
        && match journal.baseline_digest {
            Some(expected) => baseline.is_some_and(|baseline| baseline.body_digest == expected),
            None => baseline.is_none(),
        }
}

fn legacy_run_completion_cleanup_is_safe(
    journal: &BidirectionalRunJournal,
    baseline: Option<&BidirectionalBaseline>,
) -> bool {
    let Some(baseline) = baseline else {
        return false;
    };
    journal.committed_baseline_digest.is_none()
        && journal.state == BidirectionalRunState::BaselineCommitted
        && baseline.pair_id == journal.pair_id
        && baseline.left_binding == journal.left_binding
        && baseline.right_binding == journal.right_binding
        && journal.baseline_digest != Some(baseline.body_digest)
}

pub fn classify_bidirectional_run_recovery(
    baseline_path: &Path,
) -> io::Result<BidirectionalRunRecovery> {
    let store = BaselineStore::open(baseline_path)?;
    let Some(journal) = store.load_run_journal()? else {
        return Ok(BidirectionalRunRecovery::Absent);
    };
    let baseline = store.load()?;
    let completion_is_bound = run_completion_recovery_is_bound(&journal, baseline.as_ref());
    let rollback_completion_is_bound =
        rollback_completion_recovery_is_bound(&journal, baseline.as_ref());
    Ok(if completion_is_bound {
        BidirectionalRunRecovery::CompletionCleanup
    } else if rollback_completion_is_bound {
        BidirectionalRunRecovery::RollbackCompletionCleanup
    } else {
        BidirectionalRunRecovery::ResumeOrRollback
    })
}

/// Entfernt ausschließlich das Journal eines exakt zurückgelesenen vollständigen
/// Baselineabschlusses. Der Aufruf ist idempotent, damit ein später Queuecommit
/// nach Prozessabbruch gefahrlos erneut versucht werden kann.
pub fn finish_bidirectional_run_completion(baseline_path: &Path) -> io::Result<()> {
    finish_bidirectional_run_completion_using(baseline_path, BaselineStore::sync_baseline)
}

pub(super) fn finish_bidirectional_run_completion_using(
    baseline_path: &Path,
    sync: impl FnOnce(&BaselineStore, &BidirectionalBaseline) -> io::Result<()>,
) -> io::Result<()> {
    let store = BaselineStore::open(baseline_path)?;
    let Some(journal) = store.load_run_journal()? else {
        return Ok(());
    };
    let baseline = store.load()?;
    let completion_is_bound = run_completion_recovery_is_bound(&journal, baseline.as_ref())
        && (journal.committed_baseline_digest.is_some()
            || legacy_run_completion_cleanup_is_safe(&journal, baseline.as_ref()));
    if !completion_is_bound {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Laufjournal besitzt keinen vollständig gebundenen Baselineabschluss",
        ));
    }
    // Der Digest beweist nur Sichtbarkeit. Das noch vorhandene Journal bleibt
    // erhalten, bis auch die Baseline nach einem früheren Savefehler dauerhaft ist.
    sync(
        &store,
        baseline
            .as_ref()
            .ok_or_else(|| invalid_data("Abschluss-Baseline fehlt"))?,
    )
    .map_err(|error| {
        bidirectional_effect_error(error, &[&store.path, &store.run_journal_path()])
    })?;
    store.remove_run_journal_reconciled(&journal)
}

/// Entfernt ausschließlich den bereits vollständig zurückgerollten Laufmarker.
/// Dieser Schritt öffnet keine Endpunkte und führt keine Datenprojektion aus. Er
/// bindet den Abschluss erneut an die alte Baseline und darf danach bei einem
/// unklaren Unlink-/fsync-Ausgang beliebig wiederholt werden, sobald die Queue
/// ihren terminalen Rollbackzustand bewiesen hat.
pub fn finish_bidirectional_rollback_completion(baseline_path: &Path) -> io::Result<()> {
    let store = BaselineStore::open(baseline_path)?;
    let Some(journal) = store.load_run_journal()? else {
        return Ok(());
    };
    let baseline = store.load()?;
    let rollback_completion_is_bound =
        rollback_completion_recovery_is_bound(&journal, baseline.as_ref());
    if !rollback_completion_is_bound {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Laufjournal besitzt keinen vollständig gebundenen Rollbackabschluss",
        ));
    }
    store.remove_run_journal_reconciled(&journal)
}

/// Entfernt ein offenes Journal nur dann, wenn keine Projektion als committed gilt
/// und beide Bäume bytegenau wieder den gebundenen Ausgangssnapshots entsprechen.
/// Dieser Beweis ist für Fehler vor dem ersten Teilcommit der sichere Recovery-Weg.
pub fn discard_unchanged_bidirectional_run(
    baseline_path: &Path,
    left: &Path,
    right: &Path,
) -> io::Result<()> {
    let store = BaselineStore::open(baseline_path)?;
    let journal = store
        .load_run_journal()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Kein Laufjournal vorhanden"))?;
    if matches!(
        journal.state,
        BidirectionalRunState::BaselineCommitted | BidirectionalRunState::RollbackCommitted
    ) || journal
        .projections
        .iter()
        .any(|projection| projection.state == BidirectionalProjectionState::Committed)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Teilcommits dürfen nicht als unveränderter Lauf verworfen werden",
        ));
    }
    let baseline_digest = store.load()?.map(|baseline| baseline.body_digest);
    if baseline_digest != journal.baseline_digest {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Baseline änderte sich seit dem unterbrochenen Lauf",
        ));
    }
    let (left_root, right_root) = crate::sync::validate_local_sync_roots(left, right)?;
    let left_parent = std::fs::canonicalize(
        left_root
            .parent()
            .ok_or_else(|| invalid_input("Linker Root besitzt keinen Elternordner"))?,
    )?;
    let right_parent = std::fs::canonicalize(
        right_root
            .parent()
            .ok_or_else(|| invalid_input("Rechter Root besitzt keinen Elternordner"))?,
    )?;
    let left_vfs = crate::LocalVfs::rooted(&left_parent)?;
    let right_vfs = crate::LocalVfs::rooted(&right_parent)?;
    let left_snapshot = build_tree_snapshot(
        &left_vfs,
        &left_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    let right_snapshot = build_tree_snapshot(
        &right_vfs,
        &right_root,
        MetadataProfile::Portable,
        SnapshotLimits::default(),
    )?;
    if EndpointBinding::local(&left_snapshot.root_identity)? != journal.left_binding
        || EndpointBinding::local(&right_snapshot.root_identity)? != journal.right_binding
        || left_snapshot.snapshot_digest != journal.left_snapshot_digest
        || right_snapshot.snapshot_digest != journal.right_snapshot_digest
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Aktuelle Bäume entsprechen nicht dem unveränderten Journal-Ausgang",
        ));
    }
    store.remove_run_journal()
}
