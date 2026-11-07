//! Native Freigabe für den eng begrenzten lokalen Metadatenhelfer.
use crate::AppError;
use doppeldecker_core::{
    file_properties::admin::{AdminMetadataRequest, AdminMetadataResult},
    Cancel,
};

#[cfg(target_os = "linux")]
#[path = "metadata_admin_linux.rs"]
mod linux;

// Mehrere Fenster dürfen gleichzeitig Metadaten ändern. Nur das Entfernen oder
// erneute Registrieren des gemeinsamen Dienstes ist währenddessen ausgeschlossen.
#[cfg(target_os = "macos")]
static SERVICE_ACCESS: std::sync::RwLock<()> = std::sync::RwLock::new(());

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminStatus {
    available: bool,
    state: &'static str,
    message: &'static str,
}

#[tauri::command]
pub(crate) async fn get_metadata_admin_status() -> Result<AdminStatus, AppError> {
    tauri::async_runtime::spawn_blocking(status)
        .await
        .map_err(|_| {
            AppError::new(
                "io",
                "Status des Administratorhelfers konnte nicht gelesen werden",
            )
        })
}

#[cfg(target_os = "macos")]
fn status() -> AdminStatus {
    let (state, message) = match unsafe { native::dd_admin_service_status() } {
        0 => (
            "notRegistered",
            "Administratorhelfer noch nicht eingerichtet.",
        ),
        1 => (
            "enabled",
            "Administratorhelfer verfügbar. macOS prüft die Administratorfreigabe für jede Änderung.",
        ),
        2 => (
            "requiresApproval",
            "Administratorhelfer in Systemeinstellungen → Allgemein → Anmeldeobjekte erlauben.",
        ),
        3 => ("notFound", "macOS kennt den vorhandenen Administratorhelfer noch nicht. Die Einrichtung prüft die Systemfreigabe."),
        -2 => ("invalidPackage", "Administratorhelfer fehlt oder seine Signatur passt nicht zum App-Paket."),
        _ => (
            "unavailable",
            "Administratorhelfer benötigt das signierte und notarisierte App-Paket.",
        ),
    };
    AdminStatus {
        available: true,
        state,
        message,
    }
}
#[cfg(target_os = "linux")]
fn status() -> AdminStatus {
    let available =
        linux::helper_path().is_ok() && std::path::Path::new("/usr/bin/pkexec").is_file();
    AdminStatus {
        available,
        state: if available { "polkit" } else { "unavailable" },
        message: if available {
            "Polkit fragt die Systemfreigabe für jede Änderung an. Ein dauerhafter Dienst wird nicht eingerichtet."
        } else {
            "Administratorzugriff benötigt Polkit (pkexec) und den mitgelieferten Metadatenhelfer."
        },
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn status() -> AdminStatus {
    AdminStatus {
        available: false,
        state: "unsupported",
        message: "Administratorhelfer derzeit nur auf macOS verfügbar.",
    }
}

#[tauri::command]
pub(crate) async fn set_metadata_admin_service(enabled: bool) -> Result<AdminStatus, AppError> {
    #[cfg(target_os = "macos")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            // Registrierung ist eine ausdrückliche UI-Aktion; weder App-Start
            // noch ein gewöhnlicher Datei- oder Metadatenauftrag installiert sie.
            let _service = SERVICE_ACCESS.try_write().map_err(|_| {
                AppError::new(
                    "busy",
                    "Eine Administratoränderung läuft noch; Dienst danach ändern",
                )
            })?;
            let value = native::take(unsafe { native::dd_admin_service_change(enabled) })?;
            decode(value)?;
            Ok(status())
        })
        .await
        .map_err(|_| AppError::new("outcome_unclear", "Status des Administratorhelfers unklar"))?
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = enabled;
        Err(AppError::new("unsupported", status().message))
    }
}
#[tauri::command]
pub(crate) fn open_metadata_admin_settings() -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    {
        unsafe { native::dd_admin_open_settings() };
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(AppError::new("unsupported", status().message))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn decode(value: serde_json::Value) -> Result<serde_json::Value, AppError> {
    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .and_then(|value| value.as_str())
            .unwrap_or("outcome_unclear");
        let message = error
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or("Administratorergebnis unklar; Teilwirkungen prüfen");
        return Err(AppError::new(code, message));
    }
    value.get("ok").cloned().ok_or_else(|| {
        AppError::new(
            "outcome_unclear",
            "Administratorergebnis unvollständig; Teilwirkungen prüfen",
        )
    })
}

pub(crate) fn execute(
    request: AdminMetadataRequest,
    cancel: &Cancel,
) -> Result<AdminMetadataResult, AppError> {
    #[cfg(target_os = "macos")]
    {
        let _service = SERVICE_ACCESS.try_read().map_err(|_| {
            AppError::new(
                "busy",
                "Administratorhelfer wird gerade eingerichtet oder entfernt",
            )
        })?;
        let bytes = serde_json::to_vec(&request).map_err(|_| {
            AppError::new("invalid_input", "Administratorauftrag nicht darstellbar")
        })?;
        let result = native::perform(&bytes, cancel)?;
        serde_json::from_value(decode(result)?).map_err(|_| {
            AppError::new(
                "outcome_unclear",
                "Administratorergebnis nicht lesbar; Teilwirkungen prüfen",
            )
        })
    }
    #[cfg(target_os = "linux")]
    {
        let result = linux::perform(&request, cancel)?;
        serde_json::from_value(decode(result)?).map_err(|_| {
            AppError::new(
                "outcome_unclear",
                "Administratorergebnis nicht lesbar; Teilwirkungen prüfen",
            )
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (request, cancel);
        Err(AppError::new("unsupported", status().message))
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::ffi::{c_char, c_void, CStr};
    unsafe extern "C" {
        pub(super) fn dd_admin_service_status() -> i32;
        pub(super) fn dd_admin_service_change(enabled: bool) -> *mut c_char;
        pub(super) fn dd_admin_open_settings();
        fn dd_admin_client_perform(
            bytes: *const c_char,
            length: usize,
            cancelled: unsafe extern "C" fn(*mut c_void) -> bool,
            context: *mut c_void,
        ) -> *mut c_char;
    }
    pub(super) fn take(pointer: *mut c_char) -> Result<serde_json::Value, AppError> {
        if pointer.is_null() {
            return Err(AppError::new(
                "outcome_unclear",
                "Administratorergebnis fehlt; Teilwirkungen prüfen",
            ));
        }
        let parsed = serde_json::from_slice(unsafe { CStr::from_ptr(pointer) }.to_bytes());
        unsafe { libc::free(pointer.cast()) };
        parsed.map_err(|_| {
            AppError::new(
                "outcome_unclear",
                "Administratorergebnis nicht lesbar; Teilwirkungen prüfen",
            )
        })
    }
    unsafe extern "C" fn cancelled(context: *mut c_void) -> bool {
        unsafe { &*context.cast::<Cancel>() }.is_cancelled()
    }
    pub(super) fn perform(bytes: &[u8], cancel: &Cancel) -> Result<serde_json::Value, AppError> {
        take(unsafe {
            dd_admin_client_perform(
                bytes.as_ptr().cast(),
                bytes.len(),
                cancelled,
                (cancel as *const Cancel).cast_mut().cast(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn unsigned_test_app_refuses_elevation_before_any_system_prompt() {
        use doppeldecker_core::file_properties::admin::AdminMetadataAction;
        let area = tempfile::tempdir().unwrap();
        let root = area.path().canonicalize().unwrap();
        std::fs::write(root.join("file"), b"unchanged content").unwrap();
        assert_eq!(unsafe { native::dd_admin_service_status() }, -1);
        let request = AdminMetadataRequest::capture(
            "unsigned-test".into(),
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
        assert_eq!(
            execute(request, &Cancel::new()).unwrap_err().code,
            "unsupported"
        );
        assert_eq!(
            std::fs::read(root.join("file")).unwrap(),
            b"unchanged content"
        );
    }

    #[test]
    fn admin_transport_preserves_codes_and_rejects_missing_or_invalid_results() {
        let error = decode(
            serde_json::json!({"error":{"code":"interrupted","message":"Freigabe abgebrochen"}}),
        )
        .unwrap_err();
        assert_eq!(error.code, "interrupted");
        assert_eq!(error.message, "Freigabe abgebrochen");
        assert_eq!(
            decode(serde_json::json!({})).unwrap_err().code,
            "outcome_unclear"
        );
        assert!(decode(serde_json::json!({"ok":true}))
            .unwrap()
            .as_bool()
            .unwrap());
    }
}
