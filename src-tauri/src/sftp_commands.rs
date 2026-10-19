//! Dünne Tauri-Anbindung des GUI-freien SFTP-Kerns.
//!
//! Auf Platte landen ausschließlich Verbindungsprofile mit öffentlichen Pins und
//! Schlüsselbundreferenzen. Entschlüsselte Werte, Sitzungen und Realmodus-Permits
//! bleiben kurzlebig im Arbeitsspeicher.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use doppeldecker_core::{
    list_quarantine_entries_for_target, list_quarantine_runs,
    load_sftp_profiles as core_load_profiles, prepare_sftp_download, prepare_sftp_queue_entry,
    prepare_sftp_upload, purge_quarantine_run_for_target, restore_quarantine_entry_for_target,
    KeychainSecretReference, OperationQueue, Progress, ProviderEndpoint, QuarantineEntry,
    QuarantineRun, QueueEntry, QueueExecutionResult, QueueOperation, QueueSnapshot, RealRunPreview,
    RemoteCapabilities, RemoteProfileBinding, RemoteProfileProtocol, SecretResolver, SecretString,
    SftpAuthenticationReference, SftpProfile, SftpPublicKeyIdentity, SftpQueueDirection,
    SftpQueuePreparation, SftpServerIdentity, SftpVfs, SyncOptions, SyncPlan, VersionedProfile,
};
use tauri::{Manager, State, WebviewWindow};

use crate::quarantine_commands::{restore_response, QuarantineRestoreResponse};
use zeroize::{Zeroize, Zeroizing};

use crate::queue_runtime::{
    cached_remote_connection, enqueue_bound_pending, CachedRemoteConnection, PendingRemoteRealRun,
    QueueTerminalEvent,
};
use crate::{
    app_config_dir, begin_profile_transaction, emit_queue_snapshot,
    enqueue_operation_with_profile_bindings, finish_operation, operation_queue, reserve_operation,
    start_queue_worker, take_pending_for_operation, with_queue_extension, with_reserved_operation,
    AppError, AppState, MutexExt,
};

const PENDING_SECRET_LIFETIME: Duration = Duration::from_secs(10 * 60);

const PROFILE_FILE: &str = "sftp-profiles.json";
const AUDIT_FILE: &str = "sftp-real-runs.jsonl";

fn random_opaque_id(prefix: &str) -> Result<String, AppError> {
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|_| {
        AppError::new(
            "randomness",
            "Kryptografischer Systemzufall ist momentan nicht verfügbar",
        )
    })?;
    let mut id = String::with_capacity(prefix.len() + random.len() * 2);
    id.push_str(prefix);
    for byte in random {
        use std::fmt::Write as _;
        write!(id, "{byte:02x}").expect("Schreiben in String kann nicht fehlschlagen");
    }
    Ok(id)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CapturedConnectionSecret {
    secret_id: String,
}

fn store_captured_secret_using(
    state: &AppState,
    owner: &str,
    protocol: &str,
    profile_id: &str,
    capture: impl FnOnce() -> Result<Option<Zeroizing<String>>, AppError>,
) -> Result<CapturedConnectionSecret, AppError> {
    let Some(secret) = capture()? else {
        return Err(AppError::new(
            "cancelled",
            "Die sichere Passworteingabe wurde abgebrochen",
        ));
    };
    if secret.is_empty() {
        return Err(AppError::new(
            "invalid_input",
            "Das Secret darf nicht leer sein",
        ));
    }
    let window = state.window(owner);
    let mut pending = state.pending_connection_secrets.lock_recover();
    // CloseRequested setzt `closing` vor seinem Purge. Beide Seiten prüfen bzw.
    // verändern den Zustand unter derselben Pending-Secret-Sperre: Entweder wird
    // dieser Eintrag zuerst angelegt und anschließend vom Close-Purge entfernt,
    // oder das gesetzte Gate verhindert ihn danach vollständig.
    if state
        .exit_in_progress
        .load(std::sync::atomic::Ordering::Acquire)
        || window.closing.load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(AppError::new(
            "cancelled",
            "Das zugehörige Fenster oder die App wird bereits geschlossen",
        ));
    }
    pending.retain(|_, captured| captured.captured_at.elapsed() <= PENDING_SECRET_LIFETIME);
    for _ in 0..16 {
        let secret_id = random_opaque_id("connection-secret-")?;
        if let std::collections::hash_map::Entry::Vacant(slot) = pending.entry(secret_id.clone()) {
            slot.insert(crate::queue_runtime::PendingConnectionSecret {
                owner: owner.to_owned(),
                protocol: protocol.to_owned(),
                profile_id: profile_id.to_owned(),
                captured_at: Instant::now(),
                value: secret,
            });
            return Ok(CapturedConnectionSecret { secret_id });
        }
    }
    Err(AppError::new(
        "randomness",
        "Es konnte keine freie Secret-Einmal-ID erzeugt werden",
    ))
}

fn take_pending_connection_secret(
    state: &AppState,
    owner: &str,
    protocol: &str,
    profile_id: &str,
    secret_id: Option<String>,
) -> Result<Option<Zeroizing<String>>, AppError> {
    secret_id
        .map(|secret_id| {
            let mut pending = state.pending_connection_secrets.lock_recover();
            let captured = pending.get(&secret_id).ok_or_else(|| {
                AppError::new(
                    "secret_expired",
                    "Die sichere Passworteingabe fehlt oder wurde bereits verbraucht",
                )
            })?;
            if captured.owner != owner {
                return Err(AppError::new(
                    "secret_owner_mismatch",
                    "Die sichere Passworteingabe gehört zu einem anderen Fenster",
                ));
            }
            if captured.protocol != protocol || captured.profile_id != profile_id {
                return Err(AppError::new(
                    "secret_purpose_mismatch",
                    "Die sichere Passworteingabe gehört zu einem anderen Profil oder Protokoll",
                ));
            }
            if captured.captured_at.elapsed() > PENDING_SECRET_LIFETIME {
                pending.remove(&secret_id);
                return Err(AppError::new(
                    "secret_expired",
                    "Die sichere Passworteingabe ist abgelaufen",
                ));
            }
            Ok(pending
                .remove(&secret_id)
                .expect("unmittelbar zuvor geprüft")
                .value)
        })
        .transpose()
}

fn expire_pending_connection_secret(state: &AppState, secret_id: &str) -> bool {
    state
        .pending_connection_secrets
        .lock_recover()
        .remove(secret_id)
        .is_some()
}

fn schedule_pending_secret_expiry(lifetime: Duration, expire: impl FnOnce() + Send + 'static) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(lifetime).await;
        expire();
    });
}

pub(crate) fn discard_connection_secrets_for_owner(state: &AppState, owner: &str) {
    state
        .pending_connection_secrets
        .lock_recover()
        .retain(|_, captured| captured.owner != owner);
}

struct SecretCaptureGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for SecretCaptureGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

fn begin_secret_capture(state: &AppState) -> Result<SecretCaptureGuard<'_>, AppError> {
    state
        .secret_capture_active
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .map_err(|_| {
            AppError::new(
                "busy",
                "In einem anderen Fenster ist bereits die sichere Passworteingabe geöffnet",
            )
        })?;
    Ok(SecretCaptureGuard(&state.secret_capture_active))
}

#[cfg(target_os = "macos")]
fn native_connection_secret() -> Result<Option<Zeroizing<String>>, AppError> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSSecureTextField};
    use objc2_foundation::{NSSize, NSString};

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| AppError::new("app", "Die sichere Eingabe muss auf dem UI-Thread laufen"))?;
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str("Passwort sicher eingeben"));
    alert.setInformativeText(&NSString::from_str(
        "Die Eingabe bleibt außerhalb der Weboberfläche und wird nur einmal im Backend verwendet.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Sicher übernehmen"));
    alert.addButtonWithTitle(&NSString::from_str("Abbrechen"));
    let field = NSSecureTextField::new(mtm);
    field.setFrameSize(NSSize::new(340.0, 24.0));
    alert.setAccessoryView(Some(&field));
    let response = alert.runModal();
    let result = if response == NSAlertFirstButtonReturn {
        Some(Zeroizing::new(field.stringValue().to_string()))
    } else {
        None
    };
    // Auch den nativen Eingabepuffer leeren, bevor das Feld freigegeben wird.
    field.setStringValue(&NSString::from_str(""));
    Ok(result)
}

#[cfg(target_os = "linux")]
fn native_connection_secret() -> Result<Option<Zeroizing<String>>, AppError> {
    use gtk::prelude::*;
    use gtk::{DialogFlags, ResponseType};

    let dialog = gtk::Dialog::with_buttons(
        Some("Passwort sicher eingeben"),
        None::<&gtk::Window>,
        DialogFlags::MODAL,
        &[
            ("Abbrechen", ResponseType::Cancel),
            ("Sicher übernehmen", ResponseType::Accept),
        ],
    );
    let entry = gtk::Entry::new();
    entry.set_visibility(false);
    entry.set_activates_default(true);
    dialog.set_default_response(ResponseType::Accept);
    dialog.content_area().add(&entry);
    dialog.show_all();
    let response = dialog.run();
    let result = if response == ResponseType::Accept {
        Some(Zeroizing::new(entry.text().to_string()))
    } else {
        None
    };
    entry.set_text("");
    dialog.close();
    Ok(result)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_connection_secret() -> Result<Option<Zeroizing<String>>, AppError> {
    Err(AppError::new(
        "unavailable",
        "Sichere native Passworteingabe wird auf dieser Plattform nicht unterstützt",
    ))
}

#[tauri::command]
pub(crate) async fn capture_connection_secret(
    window: WebviewWindow,
    app: tauri::AppHandle,
    protocol: String,
    profile_id: String,
) -> Result<CapturedConnectionSecret, AppError> {
    if !matches!(protocol.as_str(), "sftp" | "ftps" | "webdav" | "smb") || profile_id.is_empty() {
        return Err(AppError::new(
            "invalid_input",
            "Sichere Eingabe braucht Protokoll und Profil-ID",
        ));
    }
    let app_state = app.state::<AppState>();
    let _capture = begin_secret_capture(&app_state)?;
    let owner = window.label().to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = sender.send(native_connection_secret());
    })
    .map_err(|error| AppError::new("app", error.to_string()))?;
    let captured = tauri::async_runtime::spawn_blocking(move || receiver.recv())
        .await
        .map_err(|_| AppError::new("app", "Sichere Eingabe wurde unerwartet beendet"))?
        .map_err(|_| AppError::new("app", "Sichere Eingabe lieferte keine Antwort"))?;
    if app.get_webview_window(&owner).is_none() {
        return Err(AppError::new(
            "cancelled",
            "Das zugehörige Fenster wurde vor Abschluss der sicheren Eingabe geschlossen",
        ));
    }
    let captured =
        store_captured_secret_using(&app_state, &owner, &protocol, &profile_id, || captured)?;
    let expiring_id = captured.secret_id.clone();
    let expiry_app = app.clone();
    schedule_pending_secret_expiry(PENDING_SECRET_LIFETIME, move || {
        expire_pending_connection_secret(&expiry_app.state::<AppState>(), &expiring_id);
    });
    Ok(captured)
}

#[tauri::command]
pub(crate) fn discard_connection_secret(
    window: WebviewWindow,
    state: State<AppState>,
    secret_id: String,
) -> Result<(), AppError> {
    let mut pending = state.pending_connection_secrets.lock_recover();
    let captured = pending.get(&secret_id).ok_or_else(|| {
        AppError::new(
            "secret_expired",
            "Die sichere Passworteingabe fehlt oder wurde bereits verbraucht",
        )
    })?;
    if captured.owner != window.label() {
        return Err(AppError::new(
            "secret_owner_mismatch",
            "Die sichere Passworteingabe gehört zu einem anderen Fenster",
        ));
    }
    pending.remove(&secret_id);
    Ok(())
}

/// Der Resolver liest jeden Wert genau einmal für den Anmeldeversuch. Weder Fehlertext
/// noch Debug-Ausgabe enthalten den Wert.
pub(crate) struct KeyringResolver;

/// Kleine, testbare Grenze um den nativen Schlüsselbund. Sie nimmt niemals den
/// geheimen Wert in einen Fehlertext auf.
pub(crate) trait ProfileSecretStore {
    fn read(
        &self,
        reference: &KeychainSecretReference,
    ) -> Result<Option<Zeroizing<String>>, AppError>;
    fn write(&self, reference: &KeychainSecretReference, secret: &str) -> Result<(), AppError>;
    fn remove(&self, reference: &KeychainSecretReference) -> Result<(), AppError>;
}

struct SystemProfileSecretStore;

impl SystemProfileSecretStore {
    fn entry(reference: &KeychainSecretReference) -> Result<keyring::Entry, AppError> {
        keyring::Entry::new(&reference.service, &reference.account)
            .map_err(|_| AppError::new("keyring", "Schlüsselbundzugriff nicht verfügbar"))
    }
}

impl ProfileSecretStore for SystemProfileSecretStore {
    fn read(
        &self,
        reference: &KeychainSecretReference,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        match Self::entry(reference)?.get_password() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(AppError::new(
                "keyring",
                "Vorhandener Schlüsselbundwert konnte nicht gelesen werden",
            )),
        }
    }

    fn write(&self, reference: &KeychainSecretReference, secret: &str) -> Result<(), AppError> {
        Self::entry(reference)?.set_password(secret).map_err(|_| {
            AppError::new(
                "keyring",
                "Schlüsselbundwert konnte nicht gespeichert werden",
            )
        })
    }

    fn remove(&self, reference: &KeychainSecretReference) -> Result<(), AppError> {
        Self::entry(reference)?.delete_credential().map_err(|_| {
            AppError::new(
                "keyring",
                "Neu angelegter Schlüsselbundwert konnte nicht zurückgesetzt werden",
            )
        })
    }
}

fn fresh_secret_reference(
    store: &impl ProfileSecretStore,
    original: &KeychainSecretReference,
) -> Result<KeychainSecretReference, AppError> {
    original.validate().map_err(AppError::from)?;
    for _ in 0..16 {
        let candidate = KeychainSecretReference::new(
            original.service.clone(),
            random_opaque_id("doppeldecker-secret-")?,
        )
        .map_err(AppError::from)?;
        if store.read(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(AppError::new(
        "keyring",
        "Es konnte kein freier Schlüsselbundverweis reserviert werden",
    ))
}

/// Geheimnisfreie Zweckbindung einer nativen Eingabe. Der eigentliche Wert bleibt
/// separat im Backend-Speicher und kann nur für genau diese drei Angaben gelten.
pub(crate) struct ConnectionSecretPurpose<'a> {
    pub(crate) owner: &'a str,
    pub(crate) protocol: &'a str,
    pub(crate) profile_id: &'a str,
}

pub(crate) struct ProfileSecretUpdate<'a> {
    pub(crate) reference: Option<&'a KeychainSecretReference>,
    pub(crate) secret_id: Option<String>,
}

/// Speichert Secret und Profil als einen Backendvertrag. Jeder tatsächliche
/// Secretwechsel bekommt einen frischen Schlüsselbundverweis; ein vorhandener Wert
/// wird dadurch niemals überschrieben. Meldet die Profildatei nach ihrem Rename
/// einen Fehler, entscheidet ein exakter Readback, ob der neue Eintrag sicher
/// entfernt werden darf oder als Recoveryzustand erhalten bleiben muss.
pub(crate) fn save_profile_with_secret<P, T>(
    state: &AppState,
    purpose: ConnectionSecretPurpose<'_>,
    secret_update: ProfileSecretUpdate<'_>,
    build_profile: impl FnOnce(Option<KeychainSecretReference>) -> P,
    save_profile: impl FnOnce(P) -> Result<T, AppError>,
    load_profile: impl FnMut() -> Result<Option<P>, AppError>,
    invalidate: impl FnOnce(),
) -> Result<T, AppError>
where
    P: Clone + PartialEq,
{
    let owner_window = state.window(purpose.owner);
    let _transaction = begin_profile_transaction(state, purpose.owner, &owner_window)?;
    let secret = take_pending_connection_secret(
        state,
        purpose.owner,
        purpose.protocol,
        purpose.profile_id,
        secret_update.secret_id,
    )?;
    let attempt = save_profile_with_secret_under_transaction(
        &SystemProfileSecretStore,
        secret_update.reference,
        secret,
        build_profile,
        save_profile,
        load_profile,
    );
    finish_profile_save(attempt, invalidate)
}

/// Internes Ergebnis mit der einzigen Information, die der Aufrufer für seine
/// Cachegrenze braucht. Secretwerte und Einmal-IDs sind darin ausdrücklich nicht
/// enthalten.
pub(crate) struct ProfileSaveAttempt<T> {
    result: Result<T, AppError>,
    profile_effect_possible: bool,
}

impl<T> ProfileSaveAttempt<T> {
    fn failed_before_profile(error: AppError) -> Self {
        Self {
            result: Err(error),
            profile_effect_possible: false,
        }
    }
}

#[cfg(test)]
fn save_profile_with_secret_using<P, T>(
    state: &AppState,
    store: &impl ProfileSecretStore,
    reference: Option<&KeychainSecretReference>,
    secret: Option<String>,
    build_profile: impl FnOnce(Option<KeychainSecretReference>) -> P,
    save_profile: impl FnOnce(P) -> Result<T, AppError>,
    load_profile: impl FnMut() -> Result<Option<P>, AppError>,
) -> Result<T, AppError>
where
    P: Clone + PartialEq,
{
    let owner_window = state.window("profile-test");
    let _transaction = begin_profile_transaction(state, "profile-test", &owner_window)?;
    save_profile_with_secret_under_transaction(
        store,
        reference,
        secret.map(Zeroizing::new),
        build_profile,
        save_profile,
        load_profile,
    )
    .result
}

fn save_profile_with_secret_under_transaction<P, T>(
    store: &impl ProfileSecretStore,
    reference: Option<&KeychainSecretReference>,
    secret: Option<Zeroizing<String>>,
    build_profile: impl FnOnce(Option<KeychainSecretReference>) -> P,
    save_profile: impl FnOnce(P) -> Result<T, AppError>,
    mut load_profile: impl FnMut() -> Result<Option<P>, AppError>,
) -> ProfileSaveAttempt<T>
where
    P: Clone + PartialEq,
{
    // Der Vorzustand wird immer vor einer möglichen Profildateiwirkung gelesen,
    // auch wenn kein neues Secret gespeichert wird. Nur so kann ein früher
    // Schreibfehler von einem späten Fehler nach dem Rename unterschieden werden.
    let previous_profile = match load_profile() {
        Ok(profile) => profile,
        Err(error) => return ProfileSaveAttempt::failed_before_profile(error),
    };
    let Some(mut secret) = secret else {
        let desired_profile = build_profile(None);
        return classify_profile_save(
            save_profile(desired_profile.clone()),
            previous_profile,
            desired_profile,
            load_profile,
        );
    };
    let Some(reference) = reference else {
        return ProfileSaveAttempt::failed_before_profile(AppError::new(
            "invalid_input",
            "Das Profil besitzt keinen Schlüsselbundverweis für das übergebene Secret",
        ));
    };
    if secret.is_empty() {
        return ProfileSaveAttempt::failed_before_profile(AppError::new(
            "invalid_input",
            "Das Secret darf nicht leer sein",
        ));
    }

    let fresh_reference = match fresh_secret_reference(store, reference) {
        Ok(reference) => reference,
        Err(error) => return ProfileSaveAttempt::failed_before_profile(error),
    };
    let desired_profile = build_profile(Some(fresh_reference.clone()));
    if let Err(write_error) = store.write(&fresh_reference, secret.as_str()) {
        secret.zeroize();
        return ProfileSaveAttempt::failed_before_profile(match store.read(&fresh_reference) {
            Ok(None) => write_error,
            // Der Schlüsselbund bietet hier kein atomares „nur löschen, wenn
            // dieser Wert noch uns gehört“. Zwischen Readback und Remove könnte
            // ein anderer Prozess den Eintrag ersetzt haben. Jeder sichtbare oder
            // unlesbare Wert bleibt deshalb unangetastet und wird als unklar gemeldet.
            Ok(Some(_)) | Err(_) => AppError::new(
                "outcome_unclear",
                "Der Schlüsselbund meldete beim Schreiben einen Fehler; der mögliche neue Eintrag konnte nicht sicher zurückgesetzt werden",
            ),
        });
    }
    let result = save_profile(desired_profile.clone());
    secret.zeroize();
    match result {
        Ok(value) => ProfileSaveAttempt {
            result: Ok(value),
            profile_effect_possible: true,
        },
        Err(profile_error) if profile_error.code == "plan_drift" => {
            // Der Core-CAS prüft unter demselben Profildatei-Lock und garantiert
            // bei Plan-Drift eine mutationsfreie Ablehnung. Der gerade unter einer
            // frischen Zufallsreferenz angelegte Secretwert gehört deshalb sicher
            // noch ausschließlich diesem fehlgeschlagenen Versuch.
            if store.remove(&fresh_reference).is_err() {
                return ProfileSaveAttempt::failed_before_profile(AppError::new(
                    "outcome_unclear",
                    "Das veraltete Profil wurde nicht gespeichert, aber der neue Schlüsselbundwert konnte nicht sicher entfernt werden",
                ));
            }
            ProfileSaveAttempt::failed_before_profile(profile_error)
        }
        Err(profile_error) => match load_profile() {
            Ok(current) if current == previous_profile => {
                if store.remove(&fresh_reference).is_err() {
                    return ProfileSaveAttempt::failed_before_profile(AppError::new(
                            "outcome_unclear",
                            "Profil konnte nicht gespeichert und der neue Schlüsselbundwert nicht sicher entfernt werden; Profil und Zugangsdaten müssen geprüft werden",
                        ));
                }
                ProfileSaveAttempt::failed_before_profile(profile_error)
            }
            Ok(current) if current.as_ref() == Some(&desired_profile) => ProfileSaveAttempt {
                result: Err(AppError::new(
                    "outcome_unclear",
                    "Das Profil ist trotz Speicherfehler mit dem neuen Schlüsselbundverweis sichtbar; die Dauerhaftigkeit muss geprüft werden",
                )),
                profile_effect_possible: true,
            },
            Ok(_) | Err(_) => ProfileSaveAttempt {
                result: Err(AppError::new(
                    "outcome_unclear",
                    "Nach dem Profilspeicherfehler war kein eindeutiger Readback möglich; der neue Schlüsselbundwert bleibt für die Recovery erhalten",
                )),
                profile_effect_possible: true,
            },
        },
    }
}

fn classify_profile_save<P, T>(
    result: Result<T, AppError>,
    previous_profile: Option<P>,
    desired_profile: P,
    mut load_profile: impl FnMut() -> Result<Option<P>, AppError>,
) -> ProfileSaveAttempt<T>
where
    P: PartialEq,
{
    match result {
        Ok(value) => ProfileSaveAttempt {
            result: Ok(value),
            profile_effect_possible: true,
        },
        // `plan_drift` stammt ausschließlich aus dem mutationsfreien Core-CAS.
        // Ein neuerer Profilstand darf die Readback-Klassifikation deshalb nicht
        // fälschlich in einen unklaren Ausgang umdeuten.
        Err(error) if error.code == "plan_drift" => {
            ProfileSaveAttempt::failed_before_profile(error)
        }
        Err(error) => match load_profile() {
            Ok(current) if current == previous_profile => {
                ProfileSaveAttempt::failed_before_profile(error)
            }
            Ok(current) if current.as_ref() == Some(&desired_profile) => ProfileSaveAttempt {
                result: Err(AppError::new(
                    "outcome_unclear",
                    "Das Profil ist trotz Speicherfehler sichtbar; die Dauerhaftigkeit muss geprüft werden",
                )),
                profile_effect_possible: true,
            },
            Ok(_) | Err(_) => ProfileSaveAttempt {
                result: Err(AppError::new(
                    "outcome_unclear",
                    "Nach dem Profilspeicherfehler war kein eindeutiger Readback möglich",
                )),
                profile_effect_possible: true,
            },
        },
    }
}

/// Cache und nicht gestartete Vorbereitungen werden auch auf dem Fehlerpfad vor der
/// Rückgabe verworfen. Ein später Profildateifehler kann nach dem Rename auftreten.
pub(crate) fn finish_profile_save<T>(
    attempt: ProfileSaveAttempt<T>,
    invalidate: impl FnOnce(),
) -> Result<T, AppError> {
    if attempt.profile_effect_possible {
        invalidate();
    }
    attempt.result
}

/// Entfernen besitzt dieselbe späte Fehlergrenze wie Speichern: Der atomare
/// Rename kann bereits sichtbar sein, bevor das Elternverzeichnis einen Fehler
/// meldet. Nur ein exakter Readback des Vorzustands belegt „keine Wirkung“.
pub(crate) fn remove_profile_with_readback<P, T>(
    state: &AppState,
    owner: &str,
    mut load_profile: impl FnMut() -> Result<Option<P>, AppError>,
    remove_profile: impl FnOnce() -> Result<T, AppError>,
    invalidate: impl FnOnce(),
) -> Result<T, AppError>
where
    P: PartialEq,
{
    let owner_window = state.window(owner);
    let _transaction = begin_profile_transaction(state, owner, &owner_window)?;
    let previous = load_profile()?;
    let attempt = match remove_profile() {
        Ok(value) => ProfileSaveAttempt {
            result: Ok(value),
            profile_effect_possible: true,
        },
        Err(error) if error.code == "plan_drift" => {
            ProfileSaveAttempt::failed_before_profile(error)
        }
        Err(error) => match load_profile() {
            Ok(current) if current == previous => {
                ProfileSaveAttempt::failed_before_profile(error)
            }
            Ok(_) | Err(_) => ProfileSaveAttempt {
                result: Err(AppError::new(
                    "outcome_unclear",
                    "Das Profil kann trotz Entfernen-Fehler bereits verschwunden sein; Verbindungen und Panes werden vorsorglich verworfen",
                )),
                profile_effect_possible: true,
            },
        },
    };
    finish_profile_save(attempt, invalidate)
}

impl SecretResolver for KeyringResolver {
    fn resolve(&self, reference: &KeychainSecretReference) -> io::Result<SecretString> {
        reference.validate()?;
        let entry = keyring::Entry::new(&reference.service, &reference.account).map_err(|_| {
            keyring_io(io::ErrorKind::Other, "Schlüsselbundzugriff nicht verfügbar")
        })?;
        let secret = entry.get_password().map_err(|error| match error {
            keyring::Error::NoEntry => {
                keyring_io(io::ErrorKind::NotFound, "Schlüsselbund-Eintrag fehlt")
            }
            keyring::Error::NoStorageAccess(_) => keyring_io(
                io::ErrorKind::PermissionDenied,
                "Zugriff auf den System-Schlüsselbund wurde abgelehnt",
            ),
            _ => keyring_io(
                io::ErrorKind::Other,
                "Schlüsselbundwert konnte nicht gelesen werden",
            ),
        })?;
        SecretString::new(secret)
    }
}

fn keyring_io(kind: io::ErrorKind, message: &str) -> io::Error {
    io::Error::new(kind, message)
}

fn profile_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?.join(PROFILE_FILE))
}

fn audit_path(app: &tauri::AppHandle, label: &str) -> Result<PathBuf, AppError> {
    Ok(app_config_dir(app)?.join(crate::window_data_file_name(AUDIT_FILE, label)?))
}

pub(crate) fn find_profile(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<SftpProfile, AppError> {
    core_load_profiles(&profile_path(app)?)
        .map_err(AppError::from)?
        .into_iter()
        .find(|profile| profile.id == profile_id)
        .ok_or_else(|| AppError::new("not_found", "SFTP-Profil wurde nicht gefunden"))
}

pub(crate) fn find_versioned_profile(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<VersionedProfile<SftpProfile>, AppError> {
    doppeldecker_core::load_sftp_profiles_versioned(&profile_path(app)?)
        .map_err(AppError::from)?
        .into_iter()
        .find(|profile| profile.profile.id == profile_id)
        .ok_or_else(|| AppError::new("not_found", "SFTP-Profil wurde nicht gefunden"))
}

fn connect_fresh_bound(
    app: &tauri::AppHandle,
    profile_id: &str,
    profile_revision: Option<u64>,
) -> Result<(Arc<SftpVfs>, VersionedProfile<SftpProfile>), AppError> {
    let profile = find_versioned_profile(app, profile_id)?;
    let binding =
        RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, profile_id, profile.revision)
            .map_err(AppError::from)?;
    crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &binding)?;
    let connection = SftpVfs::connect(
        profile.profile.connection.clone(),
        Arc::new(KeyringResolver),
    )
    .map_err(AppError::from)?;
    Ok((Arc::new(connection), profile))
}

pub(crate) fn current_profile_revision(
    app: &tauri::AppHandle,
    profile_id: &str,
) -> Result<Option<u64>, AppError> {
    doppeldecker_core::latest_sftp_profile_revision(&profile_path(app)?, profile_id)
        .map_err(AppError::from)
}

pub(crate) fn cached_connection_bound(
    app: &tauri::AppHandle,
    state: &AppState,
    profile_id: &str,
    profile_revision: Option<u64>,
) -> Result<CachedRemoteConnection<SftpVfs>, AppError> {
    cached_remote_connection(
        app,
        state,
        &state.sftp_connections,
        crate::queue_runtime::RemoteConnectionRequest {
            protocol: RemoteProfileProtocol::Sftp,
            profile_id,
            requested_revision: profile_revision,
        },
        || find_versioned_profile(app, profile_id),
        |profile| {
            SftpVfs::connect(profile.connection, Arc::new(KeyringResolver))
                .map(Arc::new)
                .map_err(AppError::from)
        },
    )
    .map(|(cached, _)| cached)
}

#[tauri::command]
pub(crate) async fn discover_sftp_host_key(
    host: String,
    port: u16,
) -> Result<doppeldecker_core::SftpHostKeyPin, AppError> {
    // „Hostkey ermitteln" ist der erste Klick im Profileditor und trifft
    // naturgemäss oft einen vertippten oder gefilterten Host. Synchron liefe der
    // Verbindungsaufbau auf dem Tauri-/AppKit-Thread; der Core begrenzt ihn jetzt
    // zwar auf 15 Sekunden, aber auch die darf das Fenster nicht stehen.
    tauri::async_runtime::spawn_blocking(move || {
        doppeldecker_core::discover_sftp_host_key(&host, port).map_err(AppError::from)
    })
    .await
    .map_err(|_| AppError::new("io", "Hostkey-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) fn list_sftp_agent_keys() -> Result<Vec<SftpPublicKeyIdentity>, AppError> {
    doppeldecker_core::list_sftp_agent_keys().map_err(AppError::from)
}

#[tauri::command]
pub(crate) fn list_sftp_profiles(
    app: tauri::AppHandle,
) -> Result<Vec<VersionedProfile<SftpProfile>>, AppError> {
    doppeldecker_core::load_sftp_profiles_versioned(&profile_path(&app)?).map_err(AppError::from)
}

#[tauri::command]
pub(crate) fn save_sftp_profile(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    mut profile: SftpProfile,
    secret_id: Option<String>,
    profile_revision: Option<u64>,
) -> Result<Vec<VersionedProfile<SftpProfile>>, AppError> {
    profile.validate().map_err(AppError::from)?;
    let id = profile.id.clone();
    let reference = match &profile.connection.authentication {
        SftpAuthenticationReference::Password { secret } => Some(secret.clone()),
        SftpAuthenticationReference::PrivateKey { passphrase, .. } => passphrase.clone(),
        SftpAuthenticationReference::SshAgent { .. } => None,
    };
    let path = profile_path(&app)?;
    let save_result = save_profile_with_secret(
        &state,
        ConnectionSecretPurpose {
            owner: window.label(),
            protocol: "sftp",
            profile_id: &id,
        },
        ProfileSecretUpdate {
            reference: reference.as_ref(),
            secret_id,
        },
        |fresh| {
            if let Some(fresh) = fresh {
                match &mut profile.connection.authentication {
                    SftpAuthenticationReference::Password { secret } => *secret = fresh,
                    SftpAuthenticationReference::PrivateKey { passphrase, .. } => {
                        *passphrase = Some(fresh);
                    }
                    SftpAuthenticationReference::SshAgent { .. } => {}
                }
            }
            profile
        },
        |profile| {
            doppeldecker_core::save_sftp_profile_versioned_if_current(
                &path,
                profile,
                profile_revision,
            )
            .map_err(AppError::from)
        },
        || {
            core_load_profiles(&path)
                .map_err(AppError::from)
                .map(|profiles| profiles.into_iter().find(|profile| profile.id == id))
        },
        || {
            // Auch ein Fehler nach dem atomaren Profil-Rename kann bereits den neuen
            // Endpunkt sichtbar gemacht haben. Deshalb wird der alte Cache vor der
            // Fehler-Rückgabe immer fail-closed verworfen.
            state.sftp_connections.lock_recover().remove(&id);
            crate::queue_runtime::invalidate_remote_profile(
                &app,
                &state,
                RemoteProfileProtocol::Sftp,
                &id,
            );
            if let Ok(Some(revision)) = current_profile_revision(&app, &id) {
                let binding = RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, &id, revision)
                    .expect("bereits validierte Profil-ID");
                crate::connection_state_commands::emit_connection_update(
                    &app,
                    &binding,
                    crate::connection_state_commands::ConnectionChangeKind::Saved,
                );
            }
        },
    );
    save_result.map(|mutation| mutation.profiles)
}

#[tauri::command]
pub(crate) fn remove_sftp_profile(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    profile_id: String,
    profile_revision: u64,
) -> Result<Vec<VersionedProfile<SftpProfile>>, AppError> {
    let path = profile_path(&app)?;
    let attempt = remove_profile_with_readback(
        &state,
        window.label(),
        || {
            core_load_profiles(&path)
                .map_err(AppError::from)
                .map(|profiles| {
                    profiles
                        .into_iter()
                        .find(|profile| profile.id == profile_id)
                })
        },
        || {
            doppeldecker_core::remove_sftp_profile_versioned_if_current(
                &path,
                &profile_id,
                profile_revision,
            )
            .map_err(AppError::from)
        },
        || {
            state.sftp_connections.lock_recover().remove(&profile_id);
            crate::queue_runtime::invalidate_remote_profile(
                &app,
                &state,
                RemoteProfileProtocol::Sftp,
                &profile_id,
            );
            let binding = RemoteProfileBinding::new(
                RemoteProfileProtocol::Sftp,
                &profile_id,
                profile_revision,
            )
            .expect("bereits validierte Profil-ID");
            crate::connection_state_commands::emit_connection_update(
                &app,
                &binding,
                crate::connection_state_commands::ConnectionChangeKind::Removed,
            );
        },
    );
    attempt.map(|mutation| mutation.profiles)
}

#[derive(serde::Serialize)]
pub(crate) struct SftpConnectionStatus {
    profile_revision: u64,
    identity: SftpServerIdentity,
    capabilities: RemoteCapabilities,
    quarantine_capabilities:
        doppeldecker_core::remote_real_mode::ProviderQuarantineManagementCapabilities,
}

#[tauri::command]
pub(crate) async fn connect_sftp_profile(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
) -> Result<SftpConnectionStatus, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let connection = cached.connection;
        let status = SftpConnectionStatus {
            profile_revision: cached.binding.revision,
            identity: connection.identity(),
            capabilities: connection.capabilities(),
            quarantine_capabilities: doppeldecker_core::ProviderKind::Sftp
                .quarantine_management_capabilities(),
        };
        crate::queue_runtime::ensure_profile_bindings_current(
            &app,
            std::slice::from_ref(&cached.binding),
        )?;
        Ok(status)
    })
    .await
    .map_err(|_| AppError::new("app", "SFTP-Worker wurde unterbrochen"))?
}

#[derive(serde::Serialize)]
pub(crate) struct RemoteDirListing {
    profile_revision: u64,
    path: String,
    parent: Option<String>,
    entries: Vec<serde_json::Value>,
    omitted: Vec<doppeldecker_core::InteractiveListingOmission>,
    session_generation: u64,
    security: doppeldecker_core::sftp::SftpReadSecurity,
}

#[tauri::command]
pub(crate) async fn scan_sftp_dir(
    app: tauri::AppHandle,
    profile_id: String,
    profile_revision: Option<u64>,
    path: String,
    show_hidden: bool,
) -> Result<RemoteDirListing, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let binding = cached.binding.clone();
        let connection = cached.connection;
        let remote_path = Path::new(&path);
        let listing = crate::queue_runtime::retry_read_once(
            || connection.list_interactive_best_effort(remote_path),
            || connection.reconnect(),
        )
        .map_err(AppError::from)?;
        let mut entries = listing.entries;
        let mut omitted = listing.omitted;
        if !show_hidden {
            entries.retain(|listed| !listed.entry.name.starts_with('.'));
            omitted.retain(|item| !item.is_hidden);
        }
        entries.sort_by(|left, right| {
            right.entry.is_dir.cmp(&left.entry.is_dir).then_with(|| {
                left.entry
                    .name
                    .to_lowercase()
                    .cmp(&right.entry.name.to_lowercase())
            })
        });
        let parent = remote_path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .filter(|parent| !parent.is_empty());
        crate::queue_runtime::ensure_profile_bindings_current(
            &app,
            std::slice::from_ref(&binding),
        )?;
        Ok(RemoteDirListing {
            profile_revision: binding.revision,
            path,
            parent,
            entries: entries
                .iter()
                .map(|listed| listed.display_entry())
                .collect::<io::Result<Vec<_>>>()
                .map_err(AppError::from)?,
            omitted,
            session_generation: listing.session_generation,
            security: listing.security,
        })
    })
    .await
    .map_err(|_| AppError::new("app", "SFTP-Worker wurde unterbrochen"))?
}

#[tauri::command]
pub(crate) fn list_sftp_quarantine_runs(
    app: tauri::AppHandle,
    state: State<AppState>,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
) -> Result<Vec<QuarantineRun>, AppError> {
    let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
    crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
    let result = list_quarantine_runs(
        cached.connection.as_ref(),
        &crate::queue_runtime::remote_quarantine_base("SFTP", Path::new(&target))?,
        &None,
    )
    .map_err(AppError::from)?;
    crate::queue_runtime::ensure_profile_bindings_current(&app, &[cached.binding])?;
    Ok(result)
}

#[tauri::command]
pub(crate) fn list_sftp_quarantine_entries(
    app: tauri::AppHandle,
    state: State<AppState>,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
    run_id: String,
) -> Result<Vec<QuarantineEntry>, AppError> {
    let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
    crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
    let result = list_quarantine_entries_for_target(
        cached.connection.as_ref(),
        &crate::queue_runtime::remote_quarantine_base("SFTP", Path::new(&target))?,
        &run_id,
        &None,
        Some(Path::new(&target)),
    )
    .map_err(AppError::from)?;
    crate::queue_runtime::ensure_profile_bindings_current(&app, &[cached.binding])?;
    Ok(result)
}

#[tauri::command]
// Bestehende Top-Level-Argumente bleiben für ältere IPC-Aufrufer kompatibel; die
// zusätzliche Revision darf nicht zugunsten einer künstlichen Kürzung entfallen.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_sftp_quarantine_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
    run_id: String,
    relative_path: String,
) -> Result<QuarantineRestoreResponse, AppError> {
    let label = window.label();
    let window_state = state.window(label);
    operation_queue(&app, &window_state, label)?;
    reserve_operation(&window_state)?;
    let result = (|| {
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let connection = cached.connection;
        connection
            .probe_add_only_capabilities_at(Path::new(&target))
            .map_err(AppError::from)?;
        restore_quarantine_entry_for_target(
            connection.as_ref(),
            &crate::queue_runtime::remote_quarantine_base("SFTP", Path::new(&target))?,
            &run_id,
            &relative_path,
            &None,
            Some(Path::new(&target)),
        )
        .map(restore_response)
        .map_err(AppError::from)
    })();
    let succeeded = result.is_ok();
    finish_operation(&app, label, || {
        let _ = crate::emit_to_window(&window, "quarantine-terminal", succeeded);
    });
    result
}

#[tauri::command]
// Siehe `restore_sftp_quarantine_entry`: Der stabile Tauri-Vertrag ist absichtlich
// flach und bindet nun zusätzlich die Profilrevision.
#[allow(clippy::too_many_arguments)]
pub(crate) fn purge_sftp_quarantine_run(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    profile_id: String,
    profile_revision: Option<u64>,
    target: String,
    run_id: String,
    confirmation: String,
) -> Result<(), AppError> {
    let expected = format!("LEEREN {run_id}");
    if confirmation != expected {
        return Err(AppError::new(
            "confirmation_mismatch",
            format!("Zum endgültigen Leeren exakt „{expected}“ eingeben"),
        ));
    }
    let label = window.label();
    let window_state = state.window(label);
    operation_queue(&app, &window_state, label)?;
    reserve_operation(&window_state)?;
    let result = (|| {
        let cached = cached_connection_bound(&app, &state, &profile_id, profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &cached.binding)?;
        let connection = cached.connection;
        connection
            .probe_delete_capability_at(Path::new(&target))
            .map_err(AppError::from)?;
        purge_quarantine_run_for_target(
            connection.as_ref(),
            &crate::queue_runtime::remote_quarantine_base("SFTP", Path::new(&target))?,
            &run_id,
            &None,
            Some(Path::new(&target)),
        )
        .map_err(AppError::from)
    })();
    let succeeded = result.is_ok();
    finish_operation(&app, label, || {
        let _ = crate::emit_to_window(&window, "quarantine-terminal", succeeded);
    });
    result
}

/// Gemeinsame verschachtelte Request-Struktur aller Einweg-Remoteprotokolle.
/// `camelCase` ist ausdrücklich gebunden, weil Tauri verschachtelte Felder nicht
/// zuverlässig wie Top-Level-Argumente umbenennt.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteSyncRequest {
    pub(crate) profile_id: String,
    #[serde(default)]
    pub(crate) profile_revision: Option<u64>,
    pub(crate) direction: SftpQueueDirection,
    pub(crate) local_root: String,
    pub(crate) remote_root: String,
    pub(crate) options: SyncOptions,
}

/// Bindet den lokalen Ordner und prüft den Remote-Root vollständig, bevor ein
/// Profilsecret gelesen oder ein Handshake gestartet wird. Der kanonische lokale
/// Pfad wird anschließend auch im Queueeintrag verwendet; damit planen und führen
/// spätere Schritte nicht gegen einen anderen bloß lexikalischen Alias aus.
pub(crate) fn prevalidate_remote_sync_roots(
    local_root: &str,
    remote_root: &str,
) -> Result<(String, String), AppError> {
    let local = ProviderEndpoint::local(Path::new(local_root)).map_err(AppError::from)?;
    let local_root =
        doppeldecker_core::fileops::path_to_utf8(local.root()).map_err(AppError::from)?;
    let remote = Path::new(remote_root);
    let normalized =
        doppeldecker_core::validate_file_operation_root(remote).map_err(AppError::from)?;
    if normalized != remote {
        return Err(AppError::new(
            "invalid_input",
            "Remote-Sync-Root muss absolut und lexikalisch normalisiert sein",
        ));
    }
    Ok((local_root, remote_root.to_owned()))
}

#[tauri::command]
pub(crate) fn prepare_sftp_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    request: RemoteSyncRequest,
) -> Result<RealRunPreview, AppError> {
    // Schon die Vorbereitung mutiert den gewählten Providerroot durch Capability-
    // und Restore-Proben. Queue-Prozessbesitz und Single-Flight müssen deshalb vor
    // dem Verbindungsaufbau gelten, nicht erst beim später bestätigten Lauf.
    let label = window.label();
    let window_state = state.window(label);
    let RemoteSyncRequest {
        profile_id,
        profile_revision,
        direction,
        local_root,
        remote_root,
        options,
    } = request;
    options.validate().map_err(AppError::from)?;
    let (local_root, remote_root) = prevalidate_remote_sync_roots(&local_root, &remote_root)?;
    with_reserved_operation(&app, label, &window_state, || {
        let (connection, profile) = connect_fresh_bound(&app, &profile_id, profile_revision)?;
        let binding =
            RemoteProfileBinding::new(RemoteProfileProtocol::Sftp, &profile_id, profile.revision)
                .map_err(AppError::from)?;
        crate::queue_runtime::ensure_requested_profile_revision(profile_revision, &binding)?;
        crate::queue_runtime::ensure_profile_bindings_current(
            &app,
            std::slice::from_ref(&binding),
        )?;
        let prepared = match direction {
            SftpQueueDirection::Upload => prepare_sftp_upload(
                Path::new(&local_root),
                connection,
                Path::new(&remote_root),
                options.clone(),
            ),
            SftpQueueDirection::Download => prepare_sftp_download(
                connection,
                Path::new(&remote_root),
                Path::new(&local_root),
                options.clone(),
            ),
        }
        .map_err(AppError::from)?;
        let preview = prepared.preview().clone();
        let operation = QueueOperation::SftpSync {
            direction,
            connection: Box::new(profile.profile.connection),
            local_root,
            remote_root,
            options,
            expected_plan: Box::new(preview.plan.clone()),
        };
        let profile_bindings = vec![binding];
        crate::queue_runtime::store_profile_bound_pending(
            &app,
            &state,
            &window_state.pending_sftp_real,
            &profile_bindings,
            PendingRemoteRealRun {
                prepared,
                operation,
                profile_bindings: profile_bindings.clone(),
            },
        )?;
        Ok(preview)
    })
}

#[tauri::command]
pub(crate) fn discard_sftp_sync(window: WebviewWindow, state: State<AppState>) {
    state
        .window(window.label())
        .pending_sftp_real
        .lock_recover()
        .take();
}

#[tauri::command]
pub(crate) fn run_sftp_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<(), AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    operation_queue(&app, &window_state, &label)?;
    let audit = audit_path(&app, &label)?;
    let (cancel, prepared, ()) = take_pending_for_operation(
        &state,
        &window_state,
        &window_state.pending_sftp_real,
        "Keine SFTP-Vorbereitung vorhanden",
        |pending| {
            crate::queue_runtime::ensure_profile_bindings_current(&app, &pending.profile_bindings)?;
            validate_run_confirmation(pending.prepared.preview(), &confirmation, backup_confirmed)
        },
    )?;
    // Wie FTPS, WebDAV, SMB und Providersync läuft auch SFTP über den gemeinsamen
    // Ablauf: erneute Bindungsprüfung, Hintergrundthread, gedrosselter Fortschritt,
    // nachgereichtes Terminalevent und Freigabe der Single-Flight-Schranke. Eine
    // eigene Kopie hier würde bei jeder Änderung an dieser Reihenfolge auseinanderlaufen.
    crate::queue_runtime::spawn_confirmed_remote_run(
        app,
        window,
        label,
        prepared.profile_bindings.clone(),
        move |on_progress| {
            prepared
                .prepared
                .confirm(&confirmation, backup_confirmed)?
                .execute_with_audit(&audit, &cancel, on_progress)
        },
    );
    Ok(())
}

fn validate_run_confirmation(
    preview: &RealRunPreview,
    confirmation: &str,
    backup_confirmed: bool,
) -> Result<(), AppError> {
    if confirmation != preview.confirmation_phrase {
        return Err(AppError::new(
            "confirmation_mismatch",
            "SFTP-Laufphrase stimmt nicht exakt überein",
        ));
    }
    if preview.backup_confirmation_required && !backup_confirmed {
        return Err(AppError::new(
            "confirmation_mismatch",
            "SFTP-Update/Mirror benötigt ein bestätigtes unabhängiges Backup",
        ));
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn enqueue_sftp_sync(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    request: EnqueueSftpRequest,
) -> Result<QueueSnapshot, AppError> {
    let label = window.label();
    let window_state = state.window(label);
    let queue = with_queue_extension(&state, &window_state, || {
        let connection = find_profile(&app, &request.profile_id)?.connection;
        let requested_operation = QueueOperation::SftpSync {
            direction: request.direction,
            connection: Box::new(connection),
            local_root: request.local_root,
            remote_root: request.remote_root,
            options: request.options,
            expected_plan: Box::new(request.expected_plan),
        };
        let mut pending_slot = window_state.pending_sftp_real.lock_recover();
        let bindings = pending_slot
            .as_ref()
            .ok_or_else(|| AppError::new("app", "Keine SFTP-Vorbereitung zum Einreihen vorhanden"))?
            .profile_bindings
            .clone();
        crate::queue_runtime::ensure_pending_profile_binding(
            &bindings,
            RemoteProfileProtocol::Sftp,
            &request.profile_id,
            request.profile_revision,
        )?;
        enqueue_bound_pending(
            &mut pending_slot,
            &requested_operation,
            |pending| &pending.operation,
            "Keine SFTP-Vorbereitung zum Einreihen vorhanden",
            "SFTP-Queueanfrage stimmt nicht exakt mit der vorbereiteten Operation überein",
            |operation| {
                enqueue_operation_with_profile_bindings(
                    &app,
                    &window_state,
                    label,
                    operation,
                    &bindings,
                )
                .map(|(queue, _)| queue)
            },
        )
    })?;
    emit_queue_snapshot(&app, label, &queue);
    start_queue_worker(app, label.to_owned());
    Ok(queue.snapshot())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueSftpRequest {
    profile_id: String,
    #[serde(default)]
    profile_revision: Option<u64>,
    direction: SftpQueueDirection,
    local_root: String,
    remote_root: String,
    options: SyncOptions,
    expected_plan: SyncPlan,
}

/// Der normale Queue-Worker führt bei Remote-Einträgen nur die frische, lesende
/// Vorbereitung aus. Die zufällige Phrase muss danach separat bestätigt werden.
pub(crate) fn prepare_remote_queue_entry(
    app: &tauri::AppHandle,
    label: &str,
    queue: &OperationQueue,
    entry: QueueEntry,
) -> QueueTerminalEvent {
    let operation_id = entry.operation_id.clone();
    let profile_bindings = entry.profile_bindings.clone();
    if let Err(error) =
        crate::queue_runtime::ensure_profile_bindings_current(app, &profile_bindings)
    {
        let _ = queue.fail(&operation_id, Progress::default(), error.message.clone());
        return QueueTerminalEvent::error("sync-error", error);
    }
    match prepare_sftp_queue_entry(queue, entry, Arc::new(KeyringResolver)) {
        Ok(SftpQueuePreparation::Ready(prepared)) => {
            let state = app.state::<AppState>();
            let window_state = state.window(label);
            let pending = crate::queue_runtime::PendingRemoteQueueRun {
                prepared: *prepared,
                profile_bindings: profile_bindings.clone(),
            };
            if let Err(error) = crate::queue_runtime::store_profile_bound_pending(
                app,
                &state,
                &window_state.pending_sftp_queue,
                &profile_bindings,
                pending,
            ) {
                let error = crate::queue_runtime::reset_consumed_remote_preparation(
                    queue,
                    &operation_id,
                    error,
                );
                return QueueTerminalEvent::error("sync-error", error);
            }
            QueueTerminalEvent::error(
                "sync-error",
                AppError::new(
                    "app",
                    "SFTP-Lauf ist geprüft; Queue öffnen und die einmalige Laufphrase bestätigen",
                ),
            )
        }
        Ok(SftpQueuePreparation::AwaitingConfirmation { reason }) => {
            QueueTerminalEvent::error("sync-error", AppError::new("app", reason))
        }
        Err(error) => {
            let _ = queue.fail_coded(&operation_id, Progress::default(), &error);
            QueueTerminalEvent::error("sync-error", AppError::from(error))
        }
    }
}

#[tauri::command]
pub(crate) fn approve_sftp_queue_entry(
    app: tauri::AppHandle,
    window: WebviewWindow,
    state: State<AppState>,
    operation_id: String,
    confirmation: String,
    backup_confirmed: bool,
) -> Result<QueueSnapshot, AppError> {
    let label = window.label().to_owned();
    let window_state = state.window(&label);
    let queue = operation_queue(&app, &window_state, &label)?;
    let audit = audit_path(&app, &label)?;
    crate::queue_runtime::ensure_queue_pending_current_or_reset(
        &app,
        &label,
        &queue,
        &window_state.pending_sftp_queue,
        &operation_id,
        |pending| &pending.profile_bindings,
        |pending| pending.prepared.operation_id(),
    )?;
    let (cancel, prepared, ()) = take_pending_for_operation(
        &state,
        &window_state,
        &window_state.pending_sftp_queue,
        "SFTP-Queue-Vorbereitung fehlt im RAM",
        |prepared| {
            crate::queue_runtime::ensure_profile_bindings_current(
                &app,
                &prepared.profile_bindings,
            )?;
            if prepared.prepared.operation_id() != operation_id {
                return Err(AppError::new(
                    "app",
                    "SFTP-Queue-Vorbereitung gehört zu einem anderen Eintrag",
                ));
            }
            validate_run_confirmation(prepared.prepared.preview(), &confirmation, backup_confirmed)
        },
    )?;
    let queue_for_worker = queue.clone();
    let operation_for_worker = operation_id.clone();
    std::thread::spawn(move || {
        if let Err(error) =
            crate::queue_runtime::ensure_profile_bindings_current(&app, &prepared.profile_bindings)
        {
            let error = crate::queue_runtime::reset_consumed_remote_preparation(
                &queue_for_worker,
                &operation_for_worker,
                error,
            );
            finish_operation(&app, &label, || {
                let _ = crate::emit_to_window(&window, "sync-error", error);
            });
            return;
        }
        let events = window.clone();
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut terminal_progress = None;
        let result = doppeldecker_core::execute_prepared_sftp_queue_entry(
            &queue_for_worker,
            prepared.prepared,
            &confirmation,
            backup_confirmed,
            &audit,
            &cancel,
            &mut |progress| {
                let terminal = matches!(progress.phase.as_str(), "done" | "cancelled");
                if terminal {
                    terminal_progress = Some(progress.clone());
                } else if last.elapsed() >= Duration::from_millis(50) {
                    last = Instant::now();
                    let _ = crate::emit_to_window(&events, "sync-progress", progress);
                }
            },
        );
        finish_operation(&app, &label, || {
            if let Some(progress) = terminal_progress {
                let _ = crate::emit_to_window(&window, "sync-progress", progress);
            }
            match result {
                Ok(QueueExecutionResult::Sync(stats)) => {
                    emit_queue_snapshot(&app, &label, &queue_for_worker);
                    let _ = crate::emit_to_window(&window, "sync-done", stats);
                }
                Ok(_) => {
                    emit_queue_snapshot(&app, &label, &queue_for_worker);
                    let _ = crate::emit_to_window(
                        &window,
                        "sync-error",
                        AppError::new("app", "SFTP-Queue lieferte einen unerwarteten Ergebnistyp"),
                    );
                }
                Err(error) => {
                    let mut error = AppError::from(error);
                    // Fehler vor `Running` (Ablauf, Reconnect oder Drift) lassen den
                    // Eintrag noch auf RemoteRun stehen. Ohne das RAM-Objekt muss er
                    // wieder Pending werden, damit der Worker vollständig neu prüft.
                    let still_waiting = queue_for_worker.snapshot().entries.iter().any(|entry| {
                        entry.operation_id == operation_for_worker
                            && entry.state == doppeldecker_core::QueueState::AwaitingConfirmation
                    });
                    if still_waiting {
                        error = crate::queue_runtime::reset_consumed_remote_preparation(
                            &queue_for_worker,
                            &operation_for_worker,
                            error,
                        );
                    }
                    emit_queue_snapshot(&app, &label, &queue_for_worker);
                    let _ = crate::emit_to_window(&window, "sync-error", error);
                }
            }
        });
    });
    Ok(queue.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    #[test]
    fn sftp_listing_payload_bindet_generation_und_sicherheitsklasse_atomar() {
        let source = include_str!("sftp_commands.rs");
        let start = source.find("pub(crate) async fn scan_sftp_dir(").unwrap();
        let body = &source[start
            ..source[start..]
                .find("pub(crate) fn list_sftp_quarantine_runs(")
                .expect("Folgecommand von scan_sftp_dir fehlt")
                + start];

        assert!(body.contains("list_interactive_best_effort"));
        assert!(body.contains("session_generation: listing.session_generation"));
        assert!(body.contains("security: listing.security"));
        assert!(
            !body.contains("connection.session_generation()"),
            "Listing und Generation dürfen nicht aus zwei getrennten State-Snapshots stammen"
        );

        let payload = RemoteDirListing {
            profile_revision: 11,
            path: "/".into(),
            parent: None,
            entries: Vec::new(),
            omitted: vec![doppeldecker_core::InteractiveListingOmission {
                display_name: "Zeile\\nneu.txt".into(),
                is_hidden: false,
                reason: "Dateityp nicht belegt".into(),
            }],
            session_generation: 7,
            security: doppeldecker_core::sftp::SftpReadSecurity::SftpPathBestEffort,
        };
        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["profile_revision"], 11);
        assert_eq!(json["session_generation"], 7);
        assert_eq!(json["security"], "sftp_path_best_effort");
        assert_eq!(json["omitted"][0]["display_name"], "Zeile\\nneu.txt");
        let unknown = RemoteDirListing {
            profile_revision: 11,
            path: "/".into(),
            parent: None,
            entries: {
                use doppeldecker_core::{LocalVfs, ProviderInteractiveEntry, Vfs};
                let area = tempfile::tempdir().unwrap();
                std::fs::write(area.path().join("datei.txt"), b"Inhalt").unwrap();
                let entry = LocalVfs::new()
                    .list_complete(&area.path().canonicalize().unwrap())
                    .unwrap()
                    .entries
                    .remove(0)
                    .entry;
                [false, true]
                    .into_iter()
                    .map(|size_known| {
                        ProviderInteractiveEntry {
                            entry: entry.clone(),
                            safely_decoded: true,
                            size_known,
                        }
                        .display_entry()
                        .unwrap()
                    })
                    .collect()
            },
            omitted: Vec::new(),
            session_generation: 7,
            security: doppeldecker_core::sftp::SftpReadSecurity::SftpPathBestEffort,
        };
        let json = serde_json::to_value(unknown).unwrap();
        assert!(json["entries"][0]["size"].is_null());
        assert_eq!(json["entries"][1]["size"], 6);
    }

    #[derive(Default)]
    struct FakeSecretStore {
        values: std::sync::Mutex<HashMap<(String, String), String>>,
    }

    #[derive(Default)]
    struct WriteActsThenFailsStore {
        inner: FakeSecretStore,
    }

    impl ProfileSecretStore for WriteActsThenFailsStore {
        fn read(
            &self,
            reference: &KeychainSecretReference,
        ) -> Result<Option<Zeroizing<String>>, AppError> {
            self.inner.read(reference)
        }

        fn write(
            &self,
            reference: &KeychainSecretReference,
            _secret: &str,
        ) -> Result<(), AppError> {
            // Simuliert einen fremden Austausch zwischen unserem Write und dem
            // Readback. Ohne atomaren Eigentumsbeleg darf der Fehlerpfad diesen
            // inzwischen fremden Wert keinesfalls entfernen.
            self.inner.write(reference, "fremder-austausch")?;
            Err(AppError::new(
                "keyring",
                "Schreiben wirkte, meldete aber einen Fehler",
            ))
        }

        fn remove(&self, reference: &KeychainSecretReference) -> Result<(), AppError> {
            self.inner.remove(reference)
        }
    }

    impl FakeSecretStore {
        fn value(&self, reference: &KeychainSecretReference) -> Option<String> {
            self.values
                .lock_recover()
                .get(&(reference.service.clone(), reference.account.clone()))
                .cloned()
        }

        fn len(&self) -> usize {
            self.values.lock_recover().len()
        }
    }

    impl ProfileSecretStore for FakeSecretStore {
        fn read(
            &self,
            reference: &KeychainSecretReference,
        ) -> Result<Option<Zeroizing<String>>, AppError> {
            Ok(self.value(reference).map(Zeroizing::new))
        }

        fn write(&self, reference: &KeychainSecretReference, secret: &str) -> Result<(), AppError> {
            self.values.lock_recover().insert(
                (reference.service.clone(), reference.account.clone()),
                secret.to_owned(),
            );
            Ok(())
        }

        fn remove(&self, reference: &KeychainSecretReference) -> Result<(), AppError> {
            self.values
                .lock_recover()
                .remove(&(reference.service.clone(), reference.account.clone()));
            Ok(())
        }
    }

    #[test]
    fn remote_sync_request_akzeptiert_verschachtelte_frontend_feldnamen() {
        let request: RemoteSyncRequest = serde_json::from_value(serde_json::json!({
            "profileId": "server",
            "profileRevision": 21,
            "direction": "Upload",
            "localRoot": "/lokal",
            "remoteRoot": "/remote",
            "options": serde_json::to_value(SyncOptions::default()).unwrap()
        }))
        .unwrap();

        assert_eq!(request.profile_id, "server");
        assert_eq!(request.profile_revision, Some(21));
        assert_eq!(request.local_root, "/lokal");
        assert_eq!(request.remote_root, "/remote");
        assert_eq!(request.direction, SftpQueueDirection::Upload);
    }

    #[test]
    fn remote_sync_roots_werden_vor_jedem_providerzugriff_vollstaendig_geprueft() {
        let local = tempfile::tempdir().unwrap();
        let (bound_local, bound_remote) =
            prevalidate_remote_sync_roots(local.path().to_str().unwrap(), "/remote").unwrap();
        assert_eq!(
            Path::new(&bound_local),
            local.path().canonicalize().unwrap()
        );
        assert_eq!(bound_remote, "/remote");

        let error = prevalidate_remote_sync_roots(
            local.path().to_str().unwrap(),
            "/remote/../anderes-ziel",
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_input");

        let missing = local.path().join("fehlt");
        assert!(prevalidate_remote_sync_roots(missing.to_str().unwrap(), "/remote").is_err());
    }

    #[test]
    fn remote_queue_request_akzeptiert_verschachtelte_frontend_feldnamen() {
        let request: EnqueueSftpRequest = serde_json::from_value(serde_json::json!({
            "profileId": "server",
            "profileRevision": 21,
            "direction": "Download",
            "localRoot": "/lokal",
            "remoteRoot": "/remote",
            "options": serde_json::to_value(SyncOptions::default()).unwrap(),
            "expectedPlan": serde_json::to_value(SyncPlan::default()).unwrap()
        }))
        .unwrap();

        assert_eq!(request.profile_id, "server");
        assert_eq!(request.profile_revision, Some(21));
        assert_eq!(request.expected_plan, SyncPlan::default());
    }

    #[test]
    fn profilfehler_laesst_altes_secret_fuer_alle_protokolle_unveraendert() {
        for protocol in ["sftp", "ftps", "webdav", "smb"] {
            let reference = KeychainSecretReference::new("Doppeldecker-Test", protocol).unwrap();
            let store = FakeSecretStore::default();
            store.write(&reference, "vorher").unwrap();
            let state = AppState::default();
            let mut reads = 0;

            let error = save_profile_with_secret_using(
                &state,
                &store,
                Some(&reference),
                Some("neu".into()),
                |fresh| fresh.unwrap().account,
                |_| Err::<(), _>(AppError::new("io", "Profildatei absichtlich gesperrt")),
                || {
                    reads += 1;
                    Ok(Some("altes-profil".to_owned()))
                },
            )
            .unwrap_err();

            assert_eq!(error.code, "io", "Protokoll {protocol}");
            assert_eq!(store.value(&reference).as_deref(), Some("vorher"));
            assert_eq!(
                store.len(),
                1,
                "Der neue Eintrag wurde entfernt ({protocol})"
            );
        }
    }

    #[test]
    fn profilfehler_entfernt_neues_secret_fuer_alle_protokolle() {
        for protocol in ["sftp", "ftps", "webdav", "smb"] {
            let reference = KeychainSecretReference::new("Doppeldecker-Test", protocol).unwrap();
            let store = FakeSecretStore::default();
            let state = AppState::default();
            let mut reads = 0;

            let error = save_profile_with_secret_using(
                &state,
                &store,
                Some(&reference),
                Some("neu".into()),
                |fresh| fresh.unwrap().account,
                |_| Err::<(), _>(AppError::new("io", "Profildatei absichtlich gesperrt")),
                || {
                    reads += 1;
                    Ok(None::<String>)
                },
            )
            .unwrap_err();

            assert_eq!(error.code, "io", "Protokoll {protocol}");
            assert_eq!(store.value(&reference), None);
            assert_eq!(store.len(), 0);
        }
    }

    #[test]
    fn plan_drift_rollt_frisches_secret_ohne_profilinvalidierung_zurueck() {
        let reference = KeychainSecretReference::new("Doppeldecker-Test", "geteilt").unwrap();
        let store = FakeSecretStore::default();
        store.write(&reference, "vorher").unwrap();
        let state = AppState::default();
        let mut readbacks = 0;

        let error = save_profile_with_secret_using(
            &state,
            &store,
            Some(&reference),
            Some("neu".into()),
            |fresh| fresh.unwrap().account,
            |_| Err::<(), _>(AppError::new("plan_drift", "neuerer Profilstand")),
            || {
                readbacks += 1;
                Ok(Some("neuerer-stand".to_owned()))
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "plan_drift");
        assert_eq!(
            readbacks, 1,
            "nach mutationsfreiem CAS-Fehler ist kein Readback nötig"
        );
        assert_eq!(store.value(&reference).as_deref(), Some("vorher"));
        assert_eq!(store.len(), 1, "frischer Zufallseintrag muss entfernt sein");
    }

    #[test]
    fn sichtbarer_profilcommit_mit_spaetem_fehler_behaelt_neues_secret() {
        let reference = KeychainSecretReference::new("Doppeldecker-Test", "alt").unwrap();
        let store = FakeSecretStore::default();
        store.write(&reference, "vorher").unwrap();
        let state = AppState::default();
        let desired = std::sync::Mutex::new(None::<String>);
        let mut reads = 0;

        let error = save_profile_with_secret_using(
            &state,
            &store,
            Some(&reference),
            Some("neu".into()),
            |fresh| {
                let profile = fresh.unwrap().account;
                *desired.lock_recover() = Some(profile.clone());
                profile
            },
            |_| Err::<(), _>(AppError::new("io", "Eltern-fsync fehlgeschlagen")),
            || {
                reads += 1;
                if reads == 1 {
                    Ok(Some("altes-profil".to_owned()))
                } else {
                    Ok(desired.lock_recover().clone())
                }
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "outcome_unclear");
        assert_eq!(store.value(&reference).as_deref(), Some("vorher"));
        assert_eq!(
            store.len(),
            2,
            "Der sichtbar referenzierte neue Wert muss bleiben"
        );
    }

    #[test]
    fn direkter_ipc_secretwechsel_ueberschreibt_keine_alte_referenz() {
        let reference = KeychainSecretReference::new("Doppeldecker-Test", "alt").unwrap();
        let store = FakeSecretStore::default();
        store.write(&reference, "vorher").unwrap();
        let state = AppState::default();

        let saved_reference = save_profile_with_secret_using(
            &state,
            &store,
            Some(&reference),
            Some("neu".into()),
            |fresh| fresh.unwrap(),
            Ok,
            || Ok(Some(reference.clone())),
        )
        .unwrap();

        assert_ne!(saved_reference, reference);
        assert_eq!(store.value(&reference).as_deref(), Some("vorher"));
        assert_eq!(store.value(&saved_reference).as_deref(), Some("neu"));
    }

    #[test]
    fn unklar_wirkender_schluesselbund_schreibfehler_loescht_nie_blind() {
        let reference = KeychainSecretReference::new("Doppeldecker-Test", "alt").unwrap();
        let store = WriteActsThenFailsStore::default();
        let state = AppState::default();

        let error = save_profile_with_secret_using(
            &state,
            &store,
            Some(&reference),
            Some("neu".into()),
            |fresh| fresh.unwrap().account,
            |_| Ok::<(), AppError>(()),
            || Ok(None::<String>),
        )
        .unwrap_err();

        assert_eq!(error.code, "outcome_unclear");
        assert_eq!(
            store.inner.len(),
            1,
            "Ohne atomaren Eigentumsbeleg darf der sichtbare Wert nicht gelöscht werden"
        );
        assert!(store
            .inner
            .values
            .lock_recover()
            .values()
            .any(|value| value == "fremder-austausch"));
    }

    #[test]
    fn frische_schluesselbundreferenz_hat_256_bit_systemzufall() {
        let original = KeychainSecretReference::new("Doppeldecker-Test", "alt").unwrap();
        let store = FakeSecretStore::default();

        let first = fresh_secret_reference(&store, &original).unwrap();
        let second = fresh_secret_reference(&store, &original).unwrap();
        let first_random = first.account.strip_prefix("doppeldecker-secret-").unwrap();

        assert_eq!(first_random.len(), 64);
        assert!(first_random.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn exit_bleibt_zwischen_secretwirkung_und_profilspeicherung_gesperrt() {
        let reference = KeychainSecretReference::new("Doppeldecker-Test", "alt").unwrap();
        let store = FakeSecretStore::default();
        let state = AppState::default();
        let exit_was_blocked = std::sync::atomic::AtomicBool::new(false);

        save_profile_with_secret_using(
            &state,
            &store,
            Some(&reference),
            Some("neu".into()),
            |fresh| fresh.unwrap().account,
            |saved_profile| {
                // Zu diesem Zeitpunkt ist der neue Schlüsselbundwert bereits
                // wirksam, die Profildatei aber noch nicht geschrieben. Genau hier
                // darf ein nativer Exit den Prozess nicht beenden.
                assert!(store.len() > 0);
                let assessment = crate::app_commands::assess_exit(
                    &["main".into()],
                    state.profile_transaction_active.load(Ordering::Acquire),
                    |_| (false, Ok(Vec::new())),
                );
                exit_was_blocked.store(assessment.must_prevent_exit(), Ordering::Release);
                Ok::<_, AppError>(saved_profile)
            },
            || Ok(None::<String>),
        )
        .unwrap();

        assert!(exit_was_blocked.load(Ordering::Acquire));
        assert!(!state.profile_transaction_active.load(Ordering::Acquire));
        let after =
            crate::app_commands::assess_exit(&["main".into()], false, |_| (false, Ok(Vec::new())));
        assert!(!after.must_prevent_exit());
    }

    #[test]
    fn profilfehler_invalidiert_cache_vor_der_rueckgabe() {
        let invalidated = std::sync::atomic::AtomicBool::new(false);
        let result = finish_profile_save(
            ProfileSaveAttempt::<()> {
                result: Err(AppError::new(
                    "outcome_unclear",
                    "Commit möglicherweise sichtbar",
                )),
                profile_effect_possible: true,
            },
            || invalidated.store(true, Ordering::Release),
        );

        assert!(invalidated.load(Ordering::Acquire));
        assert_eq!(result.unwrap_err().code, "outcome_unclear");
    }

    #[test]
    fn fehler_vor_jeder_profilwirkung_invalidiert_keinen_cache() {
        let invalidated = std::sync::atomic::AtomicBool::new(false);
        let result = finish_profile_save(
            ProfileSaveAttempt::<()>::failed_before_profile(AppError::new(
                "io",
                "Schreiben begann nicht",
            )),
            || invalidated.store(true, Ordering::Release),
        );

        assert!(!invalidated.load(Ordering::Acquire));
        assert_eq!(result.unwrap_err().code, "io");
    }

    #[test]
    fn wirkendes_remove_mit_spaetem_fehler_invalidiert_alle_protokolle() {
        for protocol in ["sftp", "ftps", "webdav", "smb"] {
            let state = AppState::default();
            let current = std::sync::Mutex::new(Some("profil".to_owned()));
            let mut invalidated = false;
            let mut invalidated_while_transaction_active = false;
            let error = remove_profile_with_readback(
                &state,
                "main",
                || Ok(current.lock_recover().clone()),
                || {
                    current.lock_recover().take();
                    Err::<(), _>(AppError::new("io", "Eltern-fsync fehlgeschlagen"))
                },
                || {
                    invalidated = true;
                    invalidated_while_transaction_active =
                        state.profile_transaction_active.load(Ordering::Acquire);
                },
            )
            .unwrap_err();

            assert_eq!(error.code, "outcome_unclear", "Protokoll {protocol}");
            assert!(invalidated, "Protokoll {protocol}");
            assert!(
                invalidated_while_transaction_active,
                "Protokoll {protocol}: Invalidierung lag außerhalb der Profiltransaktion"
            );
        }
    }

    #[test]
    fn native_secret_ids_sind_single_flight_einmalig_und_fenstergebunden() {
        let state = AppState::default();
        let capture = begin_secret_capture(&state).unwrap();
        let second = match begin_secret_capture(&state) {
            Ok(_) => panic!("Zweiter nativer Prompt wurde parallel zugelassen"),
            Err(error) => error,
        };
        assert_eq!(second.code, "busy");
        drop(capture);

        let captured = store_captured_secret_using(&state, "main", "sftp", "profil-1", || {
            Ok(Some(Zeroizing::new("nur-im-test".to_owned())))
        })
        .unwrap();
        assert_eq!(
            take_pending_connection_secret(
                &state,
                "window-1",
                "sftp",
                "profil-1",
                Some(captured.secret_id.clone())
            )
            .unwrap_err()
            .code,
            "secret_owner_mismatch"
        );
        let window = state.window("main");
        let _transaction = begin_profile_transaction(&state, "main", &window).unwrap();
        let secret = take_pending_connection_secret(
            &state,
            "main",
            "sftp",
            "profil-1",
            Some(captured.secret_id.clone()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(secret.as_str(), "nur-im-test");
        assert_eq!(
            take_pending_connection_secret(
                &state,
                "main",
                "sftp",
                "profil-1",
                Some(captured.secret_id)
            )
            .unwrap_err()
            .code,
            "secret_expired"
        );
    }

    #[test]
    fn native_secret_id_kann_nicht_fuer_fremden_zweck_verbraucht_werden() {
        let state = AppState::default();
        let captured = store_captured_secret_using(&state, "main", "sftp", "profil-1", || {
            Ok(Some(Zeroizing::new("nur-im-test".to_owned())))
        })
        .unwrap();

        for (protocol, profile_id) in [("smb", "profil-1"), ("sftp", "profil-2")] {
            assert_eq!(
                take_pending_connection_secret(
                    &state,
                    "main",
                    protocol,
                    profile_id,
                    Some(captured.secret_id.clone()),
                )
                .unwrap_err()
                .code,
                "secret_purpose_mismatch"
            );
        }
        assert!(expire_pending_connection_secret(
            &state,
            &captured.secret_id
        ));
        assert!(!expire_pending_connection_secret(
            &state,
            &captured.secret_id
        ));
    }

    #[test]
    fn fensterschluss_verwirft_nur_eigene_pending_secret_ids() {
        let state = AppState::default();
        let main = store_captured_secret_using(&state, "main", "sftp", "profil-1", || {
            Ok(Some(Zeroizing::new("main-test".to_owned())))
        })
        .unwrap();
        let other = store_captured_secret_using(&state, "window-1", "smb", "profil-2", || {
            Ok(Some(Zeroizing::new("window-test".to_owned())))
        })
        .unwrap();

        discard_connection_secrets_for_owner(&state, "main");

        assert!(!state
            .pending_connection_secrets
            .lock_recover()
            .contains_key(&main.secret_id));
        assert!(state
            .pending_connection_secrets
            .lock_recover()
            .contains_key(&other.secret_id));
    }

    #[test]
    fn close_purge_kann_keine_verspaetete_secret_erfassung_hinterlassen() {
        let state = AppState::default();
        let window = state.window("main");

        let error = match store_captured_secret_using(&state, "main", "sftp", "profil-1", || {
            // Deterministische Nachbildung des kritischen Interleavings: Der
            // native Dialog hat seinen Wert schon geliefert. Danach setzt
            // CloseRequested das Gate und leert die bisherige Map, bevor die
            // Capture-Seite ihren neuen Eintrag anlegen kann.
            window.closing.store(true, Ordering::Release);
            discard_connection_secrets_for_owner(&state, "main");
            Ok(Some(Zeroizing::new("zu-spaet".to_owned())))
        }) {
            Ok(_) => panic!("Verspätetes Secret wurde nach dem Close-Purge eingetragen"),
            Err(error) => error,
        };

        assert_eq!(error.code, "cancelled");
        assert!(state.pending_connection_secrets.lock_recover().is_empty());

        window.closing.store(false, Ordering::Release);
        state.exit_in_progress.store(true, Ordering::Release);
        let exit_error =
            match store_captured_secret_using(&state, "main", "sftp", "profil-1", || {
                Ok(Some(Zeroizing::new("im-exit".to_owned())))
            }) {
                Ok(_) => panic!("Secret wurde während der Exit-Prüfung eingetragen"),
                Err(error) => error,
            };
        assert_eq!(exit_error.code, "cancelled");
        assert!(state.pending_connection_secrets.lock_recover().is_empty());
    }

    #[test]
    fn pending_secret_wird_durch_den_hintergrundablauf_real_entfernt() {
        let state = Arc::new(AppState::default());
        let captured = store_captured_secret_using(&state, "main", "sftp", "profil-1", || {
            Ok(Some(Zeroizing::new("ablauf-test".to_owned())))
        })
        .unwrap();
        let expiring_state = state.clone();
        let expiring_id = captured.secret_id.clone();

        tauri::async_runtime::block_on(async move {
            schedule_pending_secret_expiry(Duration::from_millis(1), move || {
                expire_pending_connection_secret(&expiring_state, &expiring_id);
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
        });

        assert!(state.pending_connection_secrets.lock_recover().is_empty());
    }
}
