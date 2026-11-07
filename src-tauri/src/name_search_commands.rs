//! Dünne Fensterbindung der GUI-freien Namenssuche. Kein Queue-/Mutationssignal.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use doppeldecker_core::name_search::{
    search_names, NameSearchLimits, NameSearchOptions, NameSearchStatus, NameSearchUpdate,
    SftpNameSearchReader, VfsNameSearchReader,
};
use doppeldecker_core::name_search::{NameSearchHit, NameSearchIssue};
use doppeldecker_core::Cancel;
use std::time::{Duration, Instant};
use tauri::{Manager, State};

use crate::bidirectional_commands::BidirectionalEndpointRequest;
use crate::read_endpoint::{endpoint_is_current, resolve_read_endpoint};
use crate::{AppError, AppState, MutexExt};

struct SearchRun {
    run_id: u64,
    client_token: u64,
    cancel: Cancel,
}
#[derive(Default)]
pub(crate) struct NameSearchRuntime {
    runs: Mutex<HashMap<String, SearchRun>>,
    sequence: AtomicU64,
}
impl NameSearchRuntime {
    fn start(&self, window: &str, client_token: u64) -> Result<(u64, Cancel), AppError> {
        let mut runs = self.runs.lock_recover();
        if runs.contains_key(window) {
            return Err(AppError::new(
                "busy",
                "Die vorige Suche endet noch. Bitte deren Abschluss abwarten.",
            ));
        }
        let run_id = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let cancel = Cancel::new();
        runs.insert(
            window.into(),
            SearchRun {
                run_id,
                client_token,
                cancel: cancel.clone(),
            },
        );
        Ok((run_id, cancel))
    }
    fn finish(&self, window: &str, run_id: u64) {
        let mut runs = self.runs.lock_recover();
        if runs.get(window).is_some_and(|run| run.run_id == run_id) {
            runs.remove(window);
        }
    }
    fn cancel(&self, window: &str, client_token: u64) {
        if let Some(run) = self.runs.lock_recover().get(window) {
            if run.client_token == client_token {
                run.cancel.cancel();
            }
        }
    }
    pub(crate) fn cancel_window(&self, window: &str) {
        if let Some(run) = self.runs.lock_recover().get(window) {
            run.cancel.cancel();
        }
    }
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NameSearchRequest {
    side: String,
    endpoint: BidirectionalEndpointRequest,
    session_generation: Option<u64>,
    options: NameSearchOptions,
    client_token: u64,
}
#[derive(serde::Serialize)]
pub(crate) struct NameSearchStarted {
    run_id: u64,
}
#[derive(Clone, serde::Serialize)]
struct SearchEvent<T: Clone + serde::Serialize> {
    client_token: u64,
    run_id: u64,
    side: String,
    result: T,
}

struct SearchUpdateBuffer {
    hits: Vec<NameSearchHit>,
    issues: Vec<NameSearchIssue>,
    last_sent: Instant,
}
impl SearchUpdateBuffer {
    fn new(now: Instant) -> Self {
        Self {
            hits: Vec::new(),
            issues: Vec::new(),
            last_sent: now,
        }
    }
    fn push(&mut self, update: &NameSearchUpdate, now: Instant) -> Option<NameSearchUpdate> {
        self.hits.extend(update.hits.iter().cloned());
        self.issues.extend(update.issues.iter().cloned());
        if update.status == NameSearchStatus::Running
            && self.hits.len() < 100
            && now.duration_since(self.last_sent) < Duration::from_millis(100)
        {
            return None;
        }
        let mut batch = update.clone();
        batch.hits = std::mem::take(&mut self.hits);
        batch.issues = std::mem::take(&mut self.issues);
        self.last_sent = now;
        Some(batch)
    }
}

fn run_search(
    window: &tauri::WebviewWindow,
    request: &NameSearchRequest,
    run_id: u64,
    cancel: &Cancel,
) -> Result<(), AppError> {
    let app = window.app_handle();
    let state = app.state::<AppState>();
    if cancel.is_cancelled() {
        return Err(AppError::new("interrupted", "Suche abgebrochen"));
    }
    let context =
        resolve_read_endpoint(app, &state, &request.endpoint, request.session_generation)?;
    let mut drift = false;
    let mut buffer = SearchUpdateBuffer::new(Instant::now());
    let mut send = |update: &NameSearchUpdate| {
        if !endpoint_is_current(&state, &context.identity, &context.vfs) {
            drift = true;
            cancel.cancel();
            return;
        }
        let Some(batch) = buffer.push(update, Instant::now()) else {
            return;
        };
        // Der Slot wird vor dem Terminalevent freigegeben. Ein sofortiger neuer
        // Start darf nicht am gerade abgeschlossenen Worker hängen bleiben.
        if update.status != NameSearchStatus::Running {
            state.name_searches.finish(window.label(), run_id);
        }
        let _ = crate::emit_to_window(
            window,
            "name-search-update",
            SearchEvent {
                client_token: request.client_token,
                run_id,
                side: request.side.clone(),
                result: batch,
            },
        );
    };
    let limits = NameSearchLimits::default();
    if let Some(sftp) = &context.sftp {
        search_names(
            &SftpNameSearchReader::new(sftp),
            &context.parent,
            &request.options,
            limits,
            cancel,
            &mut send,
        )
    } else {
        search_names(
            &VfsNameSearchReader(context.vfs.as_ref()),
            &context.parent,
            &request.options,
            limits,
            cancel,
            &mut send,
        )
    }
    .map_err(AppError::from)?;
    if drift {
        Err(AppError::new("plan_drift", "Die Verbindung wechselte während der Suche. Bisherige Treffer bleiben eine unvollständige Beobachtung."))
    } else {
        Ok(())
    }
}

#[tauri::command]
pub(crate) fn start_name_search(
    window: tauri::WebviewWindow,
    state: State<AppState>,
    request: NameSearchRequest,
) -> Result<NameSearchStarted, AppError> {
    request.options.validate().map_err(AppError::from)?;
    if !matches!(request.side.as_str(), "left" | "right") {
        return Err(AppError::new("invalid_input", "Ungültige Suchpane"));
    }
    let (run_id, cancel) = state
        .name_searches
        .start(window.label(), request.client_token)?;
    let label = window.label().to_string();
    let spawn = std::thread::Builder::new()
        .name(format!("name-search-{run_id}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_search(&window, &request, run_id, &cancel)
            }))
            .unwrap_or_else(|_| {
                Err(AppError::new(
                    "io",
                    "Der Suchworker wurde unerwartet beendet",
                ))
            });
            window
                .state::<AppState>()
                .name_searches
                .finish(window.label(), run_id);
            if let Err(error) = result {
                let _ = crate::emit_to_window(
                    &window,
                    "name-search-error",
                    SearchEvent {
                        client_token: request.client_token,
                        run_id,
                        side: request.side,
                        result: error,
                    },
                );
            }
        });
    if let Err(error) = spawn {
        state.name_searches.finish(&label, run_id);
        return Err(AppError::new(
            "io",
            format!("Suchworker konnte nicht gestartet werden: {error}"),
        ));
    }
    Ok(NameSearchStarted { run_id })
}

#[tauri::command]
pub(crate) fn cancel_name_search(
    window: tauri::WebviewWindow,
    state: State<AppState>,
    client_token: u64,
) {
    state.name_searches.cancel(window.label(), client_token);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminalevent_liefert_gesammelte_treffer_ohne_drosselwartezeit() {
        use doppeldecker_core::name_search::NameSearchStats;
        let now = Instant::now();
        let mut buffer = SearchUpdateBuffer::new(now);
        let hit = NameSearchHit {
            is_hidden: false,
            name: "x".into(),
            parent: "/root".into(),
            relative_path: "x".into(),
            is_dir: false,
            is_symlink: false,
        };
        let mut update = NameSearchUpdate {
            status: NameSearchStatus::Running,
            stats: NameSearchStats::default(),
            hits: vec![hit.clone()],
            issues: Vec::new(),
            limit: None,
        };
        assert!(buffer
            .push(&update, now + Duration::from_millis(10))
            .is_none());
        update.stats.hits = 2;
        assert!(buffer
            .push(&update, now + Duration::from_millis(20))
            .is_none());
        update.hits.clear();
        update.status = NameSearchStatus::Cancelled;
        let terminal = buffer
            .push(&update, now + Duration::from_millis(21))
            .unwrap();
        assert_eq!(terminal.hits.len(), 2);
        assert_eq!(terminal.stats.hits, 2);
        assert_eq!(terminal.status, NameSearchStatus::Cancelled);
        let empty = buffer
            .push(&update, now + Duration::from_millis(22))
            .unwrap();
        assert!(empty.hits.is_empty());
    }
    #[test]
    fn zeitgrenze_und_treffermenge_liefern_alle_deltas() {
        use doppeldecker_core::name_search::NameSearchStats;
        let now = Instant::now();
        let mut buffer = SearchUpdateBuffer::new(now);
        let hit = NameSearchHit {
            is_hidden: false,
            name: "x".into(),
            parent: "/root".into(),
            relative_path: "x".into(),
            is_dir: false,
            is_symlink: false,
        };
        let mut update = NameSearchUpdate {
            status: NameSearchStatus::Running,
            stats: NameSearchStats::default(),
            hits: vec![hit; 50],
            issues: Vec::new(),
            limit: None,
        };
        assert!(buffer.push(&update, now).is_none());
        assert_eq!(buffer.push(&update, now).unwrap().hits.len(), 100);
        update.hits.truncate(1);
        assert_eq!(
            buffer
                .push(&update, now + Duration::from_millis(100))
                .unwrap()
                .hits
                .len(),
            1
        );
    }
    #[test]
    fn abbruch_ist_fenster_und_laufgebunden() {
        let runtime = NameSearchRuntime::default();
        let (a, cancel_a) = runtime.start("main", 11).unwrap();
        let (_, cancel_b) = runtime.start("window-2", 22).unwrap();
        runtime.cancel("main", 22);
        assert!(!cancel_a.is_cancelled());
        runtime.cancel("main", 11);
        assert!(cancel_a.is_cancelled());
        assert!(!cancel_b.is_cancelled());
        assert!(runtime.start("main", 33).is_err());
        runtime.finish("main", a);
        let (_, cancel_c) = runtime.start("main", 33).unwrap();
        runtime.finish("main", a);
        runtime.cancel("main", 11);
        assert!(!cancel_c.is_cancelled());
        assert!(runtime.start("main", 44).is_err());
    }
    #[test]
    fn frontend_request_akzeptiert_verschachtelte_camelcase_felder() {
        let request: NameSearchRequest = serde_json::from_value(serde_json::json!({
            "side": "left", "endpoint": {"provider": "sftp", "root": "/from",
                "profileId": "test", "profileRevision": 7}, "sessionGeneration": 3,
            "clientToken": 12, "options": {"query": "txt", "caseSensitive": true, "includeHidden": true}
        })).unwrap();
        assert_eq!(request.session_generation, Some(3));
        assert!(request.options.case_sensitive);
        assert!(request.options.include_hidden);
    }
}
