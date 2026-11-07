//! Enger Metadatenhelfer. Kein CLI-Parser und keine Programmausführung.
#[cfg(target_os = "linux")]
#[path = "metadata_helper_linux.rs"]
mod linux;
#[cfg(target_os = "macos")]
mod native {
    use doppeldecker_core::{file_properties::admin::AdminMetadataRequest, Cancel};
    use std::{
        collections::{HashMap, VecDeque},
        ffi::{c_char, CStr, CString},
        sync::{Mutex, OnceLock},
    };

    #[derive(Default)]
    struct Requests {
        active: HashMap<String, Cancel>,
        early_cancels: VecDeque<String>,
    }
    fn requests() -> &'static Mutex<Requests> {
        static STATE: OnceLock<Mutex<Requests>> = OnceLock::new();
        STATE.get_or_init(Mutex::default)
    }
    struct Active(String);
    impl Drop for Active {
        fn drop(&mut self) {
            requests()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                .remove(&self.0);
        }
    }
    fn error(code: &str, message: &str) -> serde_json::Value {
        serde_json::json!({"error":{"code":code,"message":message}})
    }
    fn execute(bytes: &[u8]) -> serde_json::Value {
        let request: AdminMetadataRequest = match serde_json::from_slice(bytes) {
            Ok(request) => request,
            Err(_) => {
                return error(
                    "invalid_input",
                    "Ungültiger typisierter Administratorauftrag",
                )
            }
        };
        let cancel = {
            let mut state = requests().lock().unwrap_or_else(|e| e.into_inner());
            if state.active.len() >= 8 || state.active.contains_key(&request.request_id) {
                return error(
                    "busy",
                    "Administratorauftrag bereits aktiv oder Helfer ausgelastet",
                );
            }
            let cancel = Cancel::new();
            if let Some(index) = state
                .early_cancels
                .iter()
                .position(|id| id == &request.request_id)
            {
                state.early_cancels.remove(index);
                cancel.cancel();
            }
            state
                .active
                .insert(request.request_id.clone(), cancel.clone());
            cancel
        };
        let _active = Active(request.request_id.clone());
        match request
            .bind(&cancel)
            .and_then(|bound| bound.execute(&cancel))
        {
            Ok(rows) => serde_json::json!({"ok":rows}),
            Err(error) => {
                serde_json::json!({"error":{"code":doppeldecker_core::error_code(&error),"message":error.to_string()}})
            }
        }
    }
    unsafe extern "C" fn perform(bytes: *const c_char, length: usize) -> *mut c_char {
        let result = std::panic::catch_unwind(|| {
            if bytes.is_null() || length == 0 || length > 32 * 1024 * 1024 {
                return error("invalid_input", "Administratorauftrag zu groß oder leer");
            }
            execute(unsafe { std::slice::from_raw_parts(bytes.cast(), length) })
        })
        .unwrap_or_else(|_| {
            error(
                "outcome_unclear",
                "Administratoränderung unterbrochen; Teilwirkungen prüfen",
            )
        });
        CString::new(result.to_string())
            .expect("JSON enthält keine NUL-Bytes")
            .into_raw()
    }
    unsafe extern "C" fn cancel(id: *const c_char) {
        if id.is_null() {
            return;
        }
        let Ok(id) = unsafe { CStr::from_ptr(id) }.to_str() else {
            return;
        };
        if id.len() > 128 {
            return;
        }
        let mut state = requests().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cancel) = state.active.get(id) {
            cancel.cancel();
        } else if !state.early_cancels.iter().any(|pending| pending == id) {
            if state.early_cancels.len() == 64 {
                state.early_cancels.pop_front();
            }
            state.early_cancels.push_back(id.into());
        }
    }
    unsafe extern "C" fn release(pointer: *mut c_char) {
        if !pointer.is_null() {
            drop(unsafe { CString::from_raw(pointer) });
        }
    }
    unsafe extern "C" {
        fn dd_admin_helper_run(
            execute: unsafe extern "C" fn(*const c_char, usize) -> *mut c_char,
            cancel: unsafe extern "C" fn(*const c_char),
            free: unsafe extern "C" fn(*mut c_char),
        ) -> i32;
    }
    pub fn run() -> i32 {
        unsafe { dd_admin_helper_run(perform, cancel, release) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use doppeldecker_core::file_properties::admin::AdminMetadataAction;

        #[test]
        fn helper_early_cancel_and_invalid_protocol_do_not_change_files() {
            let area = tempfile::tempdir().unwrap();
            let root = area.path().canonicalize().unwrap();
            std::fs::write(root.join("file"), b"unchanged").unwrap();
            let request = AdminMetadataRequest::capture(
                "helper-early-cancel".into(),
                &root,
                vec!["file".into()],
                AdminMetadataAction::Permissions {
                    mode: 0o640,
                    recursive: false,
                },
                Some(&root),
                &Cancel::new(),
            )
            .unwrap();
            let id = CString::new(request.request_id.clone()).unwrap();
            unsafe {
                cancel(id.as_ptr());
            }
            let result = execute(&serde_json::to_vec(&request).unwrap());
            assert_eq!(result["error"]["code"], "interrupted");
            assert_eq!(
                execute(br#"{"command":"not-a-metadata-action"}"#)["error"]["code"],
                "invalid_input"
            );
            assert_eq!(std::fs::read(root.join("file")).unwrap(), b"unchanged");
            assert!(!requests()
                .lock()
                .unwrap()
                .active
                .contains_key(&request.request_id));
        }
    }
}
fn main() {
    #[cfg(target_os = "macos")]
    std::process::exit(native::run());
    #[cfg(target_os = "linux")]
    std::process::exit(linux::run());
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    std::process::exit(1);
}
