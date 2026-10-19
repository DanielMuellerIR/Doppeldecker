//! Reine Anzeigezuordnung eines Queue-Laufs. Sie erteilt keine Ausführungsrechte;
//! Plan, Permits und Providerprüfung bleiben vollständig bei den Executoren.

use std::collections::HashSet;
use std::path::Path;

use doppeldecker_core::{
    BidirectionalQueueEndpoint, QueueEntry, QueueOperation, RemoteProfileBinding,
    RemoteProfileProtocol,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ViewEndpoint {
    provider: &'static str,
    profile_id: Option<String>,
    profile_revision: Option<u64>,
    path: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OperationViewContext {
    pub(crate) operation_id: String,
    attempt: u32,
    affected_endpoints: Vec<ViewEndpoint>,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct OperationEvent<'a, T> {
    #[serde(flatten)]
    context: &'a OperationViewContext,
    #[serde(flatten)]
    payload: T,
}

impl OperationViewContext {
    pub(crate) fn wrap<T>(&self, payload: T) -> OperationEvent<'_, T> {
        OperationEvent {
            context: self,
            payload,
        }
    }

    pub(crate) fn for_entry(config: &Path, entry: &QueueEntry) -> Self {
        let mut endpoints = Vec::new();
        let mut local_roots = HashSet::new();
        let mut local = |path: &str| {
            if local_roots.insert(path.to_owned()) {
                endpoints.push(ViewEndpoint {
                    provider: "local",
                    profile_id: None,
                    profile_revision: None,
                    path: local_path(path),
                });
            }
        };
        let bindings = &entry.profile_bindings;
        match &entry.operation {
            QueueOperation::FtpsRecovery { .. } => {}
            QueueOperation::File {
                items, destination, ..
            } => {
                for item in items {
                    if let Some(parent) = Path::new(item).parent().and_then(Path::to_str) {
                        local(parent);
                    }
                }
                local(destination);
            }
            QueueOperation::Sync { source, target, .. }
            | QueueOperation::LocalSync { source, target, .. } => {
                local(source);
                local(target);
            }
            QueueOperation::ProviderFile { source, target, .. }
            | QueueOperation::ProviderSync { source, target, .. } => {
                append_endpoint(&mut endpoints, config, bindings, source);
                append_endpoint(&mut endpoints, config, bindings, target);
            }
            QueueOperation::ProviderDelete { endpoint, .. } => {
                append_endpoint(&mut endpoints, config, bindings, endpoint);
            }
            QueueOperation::Bidirectional { left, right, .. } => {
                append_endpoint(&mut endpoints, config, bindings, left);
                append_endpoint(&mut endpoints, config, bindings, right);
            }
            QueueOperation::SftpSync {
                connection,
                local_root,
                remote_root,
                ..
            } => {
                local(local_root);
                append_endpoint(
                    &mut endpoints,
                    config,
                    bindings,
                    &BidirectionalQueueEndpoint::Sftp {
                        connection: connection.clone(),
                        root: remote_root.clone(),
                    },
                );
            }
            QueueOperation::FtpsSync {
                connection,
                local_root,
                remote_root,
                ..
            } => {
                local(local_root);
                append_endpoint(
                    &mut endpoints,
                    config,
                    bindings,
                    &BidirectionalQueueEndpoint::Ftps {
                        connection: connection.clone(),
                        root: remote_root.clone(),
                    },
                );
            }
            QueueOperation::WebDavSync {
                connection,
                local_root,
                remote_root,
                ..
            } => {
                local(local_root);
                append_endpoint(
                    &mut endpoints,
                    config,
                    bindings,
                    &BidirectionalQueueEndpoint::WebDav {
                        connection: connection.clone(),
                        root: remote_root.clone(),
                    },
                );
            }
            QueueOperation::SmbSync {
                connection,
                local_root,
                remote_root,
                ..
            } => {
                local(local_root);
                append_endpoint(
                    &mut endpoints,
                    config,
                    bindings,
                    &BidirectionalQueueEndpoint::Smb {
                        connection: connection.clone(),
                        root: remote_root.clone(),
                    },
                );
            }
        }
        let mut seen = HashSet::new();
        endpoints.retain(|endpoint| seen.insert(endpoint.clone()));
        Self {
            operation_id: entry.operation_id.clone(),
            attempt: entry.attempts,
            affected_endpoints: endpoints,
        }
    }
}

fn append_endpoint(
    endpoints: &mut Vec<ViewEndpoint>,
    config: &Path,
    bindings: &[RemoteProfileBinding],
    endpoint: &BidirectionalQueueEndpoint,
) {
    // Profil-ID und Revision kommen aus dem Queuebeleg. Gleiche Roots auf zwei
    // Servern dürfen keine Zuordnung allein nach Providername/Pfad erzeugen.
    macro_rules! remote {
        ($provider:literal, $protocol:ident, $file:literal, $load:ident, $connection:expr, $root:expr) => {{
            if let Ok(profiles) = doppeldecker_core::$load(&config.join($file)) {
                for binding in bindings
                    .iter()
                    .filter(|binding| binding.protocol == RemoteProfileProtocol::$protocol)
                {
                    if profiles.iter().any(|profile| {
                        profile.profile.id == binding.profile_id
                            && profile.revision == binding.revision
                            && profile.profile.connection == **$connection
                    }) {
                        endpoints.push(ViewEndpoint {
                            provider: $provider,
                            profile_id: Some(binding.profile_id.clone()),
                            profile_revision: Some(binding.revision),
                            path: $root.clone(),
                        });
                    }
                }
            }
        }};
    }
    match endpoint {
        BidirectionalQueueEndpoint::Local { root } => endpoints.push(ViewEndpoint {
            provider: "local",
            profile_id: None,
            profile_revision: None,
            path: local_path(root),
        }),
        BidirectionalQueueEndpoint::Sftp { connection, root } => remote!(
            "sftp",
            Sftp,
            "sftp-profiles.json",
            load_sftp_profiles_versioned,
            connection,
            root
        ),
        BidirectionalQueueEndpoint::Ftps { connection, root } => remote!(
            "ftps",
            Ftps,
            "ftps-profiles.json",
            load_ftps_profiles_versioned,
            connection,
            root
        ),
        BidirectionalQueueEndpoint::WebDav { connection, root } => remote!(
            "webdav",
            Webdav,
            "webdav-profiles.json",
            load_webdav_profiles_versioned,
            connection,
            root
        ),
        BidirectionalQueueEndpoint::Smb { connection, root } => remote!(
            "smb",
            Smb,
            "smb-profiles.json",
            load_smb_profiles_versioned,
            connection,
            root
        ),
    }
}

fn local_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kontext_ergaenzt_progress_ohne_dessen_payload_zu_veraendern() {
        let context = OperationViewContext {
            operation_id: "lauf-1".into(),
            attempt: 2,
            affected_endpoints: vec![ViewEndpoint {
                provider: "local",
                profile_id: None,
                profile_revision: None,
                path: "/test/ziel".into(),
            }],
        };
        let progress = doppeldecker_core::Progress {
            bytes_done: 37,
            phase: "copy".into(),
            ..Default::default()
        };
        let value = serde_json::to_value(context.wrap(&progress)).unwrap();
        assert_eq!(value["operationId"], "lauf-1");
        assert_eq!(value["attempt"], 2);
        assert_eq!(value["bytes_done"], 37);
        assert_eq!(value["phase"], "copy");
        assert_eq!(value["affectedEndpoints"][0]["path"], "/test/ziel");
        assert!(value["affectedEndpoints"][0]["profileId"].is_null());
    }

    #[test]
    fn gleicher_remotepfad_bindet_den_jeweiligen_server_und_die_profilrevision() {
        use doppeldecker_core::{
            FileQueueKind, FtpTransportSecurity, FtpsConnectionConfig, FtpsProfile,
            KeychainSecretReference, OperationQueue,
        };
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("ftps-profiles.json");
        for id in ["alpha", "beta"] {
            doppeldecker_core::save_ftps_profile(
                &store,
                FtpsProfile {
                    id: id.into(),
                    name: id.into(),
                    connection: FtpsConnectionConfig {
                        host: format!("{id}.test"),
                        alternate_hosts: Vec::new(),
                        port: 21,
                        username: "test".into(),
                        requested_root: "/".into(),
                        transport_security: FtpTransportSecurity::Plaintext,
                        certificate: None,
                        certificate_policy: None,
                        password_reference: KeychainSecretReference::new("test", id).unwrap(),
                    },
                },
            )
            .unwrap();
        }
        let profiles = doppeldecker_core::load_ftps_profiles_versioned(&store).unwrap();
        let endpoint = |id: &str| {
            let profile = profiles
                .iter()
                .find(|profile| profile.profile.id == id)
                .unwrap();
            Box::new(BidirectionalQueueEndpoint::Ftps {
                connection: Box::new(profile.profile.connection.clone()),
                root: "/same".into(),
            })
        };
        let bindings = profiles
            .iter()
            .map(|profile| {
                RemoteProfileBinding::new(
                    RemoteProfileProtocol::Ftps,
                    &profile.profile.id,
                    profile.revision,
                )
                .unwrap()
            })
            .collect();
        let queue = OperationQueue::open(temp.path().join("queue.json")).unwrap();
        let mut entry = queue
            .enqueue_reconciled_with_profile_bindings(
                QueueOperation::ProviderFile {
                    kind: FileQueueKind::Copy,
                    source: endpoint("alpha"),
                    target: endpoint("beta"),
                    items: vec!["/same/a.txt".into()],
                    destination: "/same".into(),
                    overwrite: false,
                    expected_conflicts: Vec::new(),
                    source_tree_expected: None,
                    keep_both_targets: Vec::new(),
                    expected_conflict_proofs: Vec::new(),
                    best_effort_commit_expected: None,
                    local_copy_new_best_effort_expected: None,
                    copy_new_best_effort_expected: Some(false),
                    replace_best_effort_expected: Some(false),
                    remove_source_best_effort_expected: None,
                },
                bindings,
            )
            .unwrap();
        let context = OperationViewContext::for_entry(temp.path(), &entry);
        assert_eq!(context.affected_endpoints.len(), 2);
        assert_eq!(
            context.affected_endpoints[0].profile_id.as_deref(),
            Some("alpha")
        );
        assert_eq!(
            context.affected_endpoints[1].profile_id.as_deref(),
            Some("beta")
        );
        assert!(context
            .affected_endpoints
            .iter()
            .all(|endpoint| endpoint.path == "/same"));
        for binding in &mut entry.profile_bindings {
            binding.revision += 100;
        }
        assert!(OperationViewContext::for_entry(temp.path(), &entry)
            .affected_endpoints
            .is_empty());
    }
}
