//! Einreihen ohne Providerkontakt. Frische Proben und Konfliktprüfung gehören an
//! den Kopf der Queue, damit ein laufender Transfer weitere Absichten nicht sperrt.

use crate::bidirectional_commands::{validate_endpoint_request_root, BidirectionalEndpointRequest};
use crate::{AppError, AppState};
use doppeldecker_core::{
    BidirectionalQueueEndpoint, FileQueueKind, QueueOperation, RemoteProfileBinding,
    RemoteProfileProtocol,
};
use std::path::{Path, PathBuf};
use tauri::{Manager, WebviewWindow};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileIntent {
    kind: String,
    source: BidirectionalEndpointRequest,
    target: BidirectionalEndpointRequest,
    items: Vec<String>,
    destination: String,
}

fn endpoint(
    config: &Path,
    request: &BidirectionalEndpointRequest,
) -> Result<(Box<BidirectionalQueueEndpoint>, Vec<RemoteProfileBinding>), AppError> {
    macro_rules! remote {
        ($protocol:ident, $variant:ident, $file:literal, $load:ident, $id:expr, $revision:expr, $root:expr) => {{
            let stored = doppeldecker_core::$load(&config.join($file))
                .map_err(AppError::from)?
                .into_iter()
                .find(|stored| stored.profile.id == *$id)
                .ok_or_else(|| AppError::new("not_found", "Remote-Profil fehlt"))?;
            let binding =
                RemoteProfileBinding::new(RemoteProfileProtocol::$protocol, $id, stored.revision)
                    .map_err(AppError::from)?;
            crate::queue_runtime::ensure_requested_profile_revision(*$revision, &binding)?;
            (
                Box::new(BidirectionalQueueEndpoint::$variant {
                    connection: Box::new(stored.profile.connection),
                    root: $root.clone(),
                }),
                vec![binding],
            )
        }};
    }
    let result = match request {
        BidirectionalEndpointRequest::Local { root } => (
            Box::new(BidirectionalQueueEndpoint::Local { root: root.clone() }),
            vec![],
        ),
        BidirectionalEndpointRequest::Sftp {
            profile_id,
            profile_revision,
            root,
        } => remote!(
            Sftp,
            Sftp,
            "sftp-profiles.json",
            load_sftp_profiles_versioned,
            profile_id,
            profile_revision,
            root
        ),
        BidirectionalEndpointRequest::Ftps {
            profile_id,
            profile_revision,
            root,
        } => remote!(
            Ftps,
            Ftps,
            "ftps-profiles.json",
            load_ftps_profiles_versioned,
            profile_id,
            profile_revision,
            root
        ),
        BidirectionalEndpointRequest::Webdav {
            profile_id,
            profile_revision,
            root,
        } => remote!(
            Webdav,
            WebDav,
            "webdav-profiles.json",
            load_webdav_profiles_versioned,
            profile_id,
            profile_revision,
            root
        ),
        BidirectionalEndpointRequest::Smb {
            profile_id,
            profile_revision,
            root,
        } => remote!(
            Smb,
            Smb,
            "smb-profiles.json",
            load_smb_profiles_versioned,
            profile_id,
            profile_revision,
            root
        ),
    };
    result.0.validate().map_err(AppError::from)?;
    Ok(result)
}

fn prepare(
    config: &Path,
    request: FileIntent,
    sandbox: &Option<PathBuf>,
) -> Result<(QueueOperation, Vec<RemoteProfileBinding>), AppError> {
    let kind = match request.kind.as_str() {
        "copy" => FileQueueKind::Copy,
        "move" => FileQueueKind::Move,
        _ => return Err(AppError::new("invalid_input", "Unbekannte Dateiaktion")),
    };
    validate_endpoint_request_root(&request.source)?;
    validate_endpoint_request_root(&request.target)?;
    let items = request.items.iter().map(PathBuf::from).collect::<Vec<_>>();
    if request.source.provider_kind() == doppeldecker_core::ProviderKind::Sftp {
        if request.target.provider_kind() != doppeldecker_core::ProviderKind::Local
            || kind != FileQueueKind::Copy
        {
            return Err(AppError::new(
                "unsupported",
                "SFTP-Inhalte erlauben nur additive lokale Downloads",
            ));
        }
        doppeldecker_core::sftp::validate_interactive_content_selection(
            Path::new(request.source.root()),
            &items,
        )
        .map_err(AppError::from)?;
    }
    let destination = Path::new(&request.destination);
    if destination != Path::new(request.target.root()) {
        return Err(AppError::new(
            "invalid_input",
            "Zielordner und Ziel-Pane stimmen nicht überein",
        ));
    }
    doppeldecker_core::validate_file_operation_paths(&items, destination)
        .map_err(AppError::from)?;
    doppeldecker_core::validate_file_operation_scope(Path::new(request.source.root()), &items)
        .map_err(AppError::from)?;
    doppeldecker_core::fileops::guard_local_transfer_roots(
        (request.source.provider_kind() == doppeldecker_core::ProviderKind::Local)
            .then(|| Path::new(request.source.root())),
        (request.target.provider_kind() == doppeldecker_core::ProviderKind::Local)
            .then(|| Path::new(request.target.root())),
        kind == FileQueueKind::Move,
        sandbox,
    )
    .map_err(AppError::from)?;
    let (source, mut bindings) = endpoint(config, &request.source)?;
    let (target, target_bindings) = endpoint(config, &request.target)?;
    for binding in target_bindings {
        if !bindings.contains(&binding) {
            bindings.push(binding);
        }
    }
    // Die UI erklärt vor dem Einreihen die mögliche Best-Effort-Ausführung.
    // Sie bestätigt damit keinen erfundenen Capabilitybefund. Der Executor probt
    // frisch und nutzt stärkere Garantien, wo sie verfügbar sind. Bestehende Ziele
    // werden niemals ersetzt; neue Konflikte halten den Auftrag vor Wirkung an.
    let remove_source_best_effort_expected = (kind == FileQueueKind::Move
        && !request.source.shares_remote_profile(&request.target))
    .then_some(true);
    Ok((
        QueueOperation::ProviderFile {
            kind,
            source,
            target,
            items: request.items,
            destination: request.destination,
            overwrite: false,
            expected_conflicts: vec![],
            source_tree_expected: None,
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(true),
            replace_best_effort_expected: Some(false),
            remove_source_best_effort_expected,
        },
        bindings,
    ))
}

#[tauri::command]
pub(crate) async fn enqueue_provider_file_intent(
    app: tauri::AppHandle,
    window: WebviewWindow,
    request: FileIntent,
) -> Result<String, AppError> {
    let worker = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker.state::<AppState>();
        let label = window.label();
        let window_state = state.window(label);
        let (operation, bindings) = prepare(
            &crate::app_config_dir(&app)?,
            request,
            &crate::sandbox_root()?,
        )?;
        let (queue, entry) = crate::with_queue_extension(&state, &window_state, || {
            crate::queue_runtime::enqueue_operation_with_profile_bindings(
                &app,
                &window_state,
                label,
                &operation,
                &bindings,
            )
        })?;
        crate::emit_queue_snapshot(&app, label, &queue);
        crate::start_queue_worker(app, label.to_owned());
        Ok(entry.operation_id)
    })
    .await
    .map_err(|error| AppError::new("app", error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frontend_payload_akzeptiert_verschachtelte_camelcase_profilfelder() {
        for provider in ["sftp", "ftps", "webdav", "smb"] {
            let request: FileIntent = serde_json::from_value(serde_json::json!({
                "kind": "copy",
                "source": {"provider": "local", "root": "/test/quelle"},
                "target": {"provider": provider, "profileId": "server",
                    "profileRevision": 7, "root": "/ziel"},
                "items": ["/test/quelle/A.txt"],
                "destination": "/ziel"
            }))
            .unwrap();
            assert_eq!(request.target.root(), "/ziel");
            let profile = match request.target {
                BidirectionalEndpointRequest::Sftp {
                    profile_id,
                    profile_revision,
                    ..
                }
                | BidirectionalEndpointRequest::Ftps {
                    profile_id,
                    profile_revision,
                    ..
                }
                | BidirectionalEndpointRequest::Webdav {
                    profile_id,
                    profile_revision,
                    ..
                }
                | BidirectionalEndpointRequest::Smb {
                    profile_id,
                    profile_revision,
                    ..
                } => (profile_id, profile_revision),
                _ => panic!("Remote-Endpunkt wurde lokal"),
            };
            assert_eq!(profile, ("server".into(), Some(7)));
        }
    }

    #[test]
    fn unerreichbares_profil_wird_ohne_secret_oder_providerprobe_persistiert() {
        use doppeldecker_core::{
            FtpTransportSecurity, FtpsConnectionConfig, FtpsProfile, KeychainSecretReference,
            OperationQueue,
        };
        let temp = tempfile::tempdir().unwrap();
        doppeldecker_core::save_ftps_profile(
            &temp.path().join("ftps-profiles.json"),
            FtpsProfile {
                id: "offline".into(),
                name: "Offline".into(),
                connection: FtpsConnectionConfig {
                    host: "unreachable.invalid".into(),
                    alternate_hosts: vec![],
                    port: 21,
                    username: "test".into(),
                    requested_root: "/".into(),
                    transport_security: FtpTransportSecurity::Plaintext,
                    certificate: None,
                    certificate_policy: None,
                    password_reference: KeychainSecretReference::new("test", "missing-secret")
                        .unwrap(),
                },
            },
        )
        .unwrap();
        let revision = doppeldecker_core::load_ftps_profiles_versioned(
            &temp.path().join("ftps-profiles.json"),
        )
        .unwrap()[0]
            .revision;
        let request = |revision| {
            serde_json::from_value::<FileIntent>(serde_json::json!({"kind":"copy", "source":{"provider":"ftps","profileId":"offline","profileRevision":revision,"root":"/source"},"target":{"provider":"local","root":temp.path().to_str().unwrap()},"items":["/source/file.txt"],"destination":temp.path().to_str().unwrap()})).unwrap()
        };
        let (operation, bindings) = prepare(temp.path(), request(revision), &None).unwrap();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let entry = queue
            .enqueue_reconciled_with_profile_bindings(operation, bindings)
            .unwrap();
        assert_eq!(entry.profile_bindings[0].revision, revision);
        assert!(matches!(
            entry.operation,
            QueueOperation::ProviderFile {
                overwrite: false,
                copy_new_best_effort_expected: Some(true),
                ..
            }
        ));
        assert_eq!(
            prepare(temp.path(), request(revision + 1), &None)
                .unwrap_err()
                .code,
            "plan_drift"
        );
    }
    #[test]
    fn ungueltige_absicht_liest_keine_profile() {
        let request: FileIntent = serde_json::from_value(serde_json::json!({"kind":"copy", "source":{"provider":"ftps","profileId":"missing","profileRevision":1,"root":"/source"},"target":{"provider":"local","root":"/target"},"items":["/else/file"],"destination":"/target"})).unwrap();
        let error = prepare(Path::new("/missing/config"), request, &None).unwrap_err();
        assert_eq!(error.code, "sandbox_violation");
    }
}
