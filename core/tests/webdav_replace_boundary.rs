//! HTTPS-Gegenbeleg zur gemeinsamen Bindung von Altziel und Eltern bei MOVE.
#[path = "support/webdav_namespace.rs"]
mod server;

use doppeldecker_core::*;
use reqwest::blocking::Client;
use reqwest::{Method, StatusCode};
use std::sync::Arc;
use std::time::Duration;

struct NoSecrets;
impl SecretResolver for NoSecrets {
    fn resolve(&self, _: &KeychainSecretReference) -> std::io::Result<SecretString> {
        panic!("Die abgelehnte Ersetzung darf keinen Schlüsselbund lesen");
    }
}

#[test]
fn persistiertes_webdav_overwrite_erhaelt_auch_nach_reopen_keine_freigabe() {
    use std::os::unix::fs::PermissionsExt;
    let server = server::Server::new();
    let source_directory = tempfile::tempdir().unwrap();
    let source = source_directory.path().canonicalize().unwrap();
    std::fs::write(source.join("Datei.txt"), b"neu").unwrap();
    std::fs::write(server.root.path().join("pane/Datei.txt"), b"alt").unwrap();
    let config = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let journal = config.path().join("queue.json");
    let mut queue = OperationQueue::open(&journal).unwrap();
    let id = queue
        .enqueue(QueueOperation::ProviderFile {
            kind: FileQueueKind::Copy,
            source: Box::new(BidirectionalQueueEndpoint::Local {
                root: source.to_string_lossy().into_owned(),
            }),
            target: Box::new(BidirectionalQueueEndpoint::WebDav {
                connection: Box::new(server.config.clone()),
                root: "/pane".into(),
            }),
            items: vec![source.join("Datei.txt").to_string_lossy().into_owned()],
            destination: "/pane".into(),
            overwrite: true,
            expected_conflicts: vec![],
            expected_conflict_proofs: vec![],
            source_tree_expected: None,
            keep_both_targets: vec![],
            best_effort_commit_expected: None,
            local_copy_new_best_effort_expected: None,
            copy_new_best_effort_expected: Some(true),
            replace_best_effort_expected: Some(true),
            remove_source_best_effort_expected: None,
        })
        .unwrap()
        .operation_id;
    for reopen in [false, true] {
        if reopen {
            drop(queue);
            queue = OperationQueue::open(&journal).unwrap();
            queue.retry(&id).unwrap();
        }
        let entry = queue.claim_next().unwrap().unwrap();
        let result = execute_provider_file_queue_entry(
            &queue,
            entry,
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            Arc::new(NoSecrets),
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!(error_code(&result.unwrap_err()), ErrorCode::Unsupported);
        // Das gespeicherte Ergebnis selbst muss lesbar bleiben; Open migriert
        // hier keine Vorbereitung und erzeugt keine neue Ausführungsbefugnis.
        if !reopen {
            let snapshot = queue.snapshot();
            assert_eq!(snapshot.entries[0].error_code, Some(ErrorCode::Unsupported));
        }
    }
    assert!(server.state.lock().unwrap().requests.is_empty());
    assert_eq!(std::fs::read(source.join("Datei.txt")).unwrap(), b"neu");
    assert_eq!(
        std::fs::read(server.root.path().join("pane/Datei.txt")).unwrap(),
        b"alt"
    );
    assert_eq!(
        std::fs::read_dir(server.root.path().join("pane"))
            .unwrap()
            .count(),
        1
    );
}

fn token(client: &Client, origin: &str, path: &str) -> String {
    let response = client
        .request(
            Method::from_bytes(b"PROPFIND").unwrap(),
            format!("{origin}{path}"),
        )
        .basic_auth("tester", Some("wertlos"))
        .header("Depth", "0")
        .send()
        .unwrap();
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let text = response.text().unwrap();
    text.split("<D:getetag>")
        .nth(1)
        .unwrap()
        .split("</D:getetag>")
        .next()
        .unwrap()
        .into()
}

#[test]
fn https_einzelne_elternbedingung_und_request_bedingung_gelten_gemeinsam() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    for method in ["PUT", "MKCOL", "MOVE", "DELETE"] {
        for (parent_matches, request_matches) in [(true, true), (false, true), (true, false)] {
            let server = server::Server::new();
            let origin = &server.config.origin;
            let client = Client::builder()
                .add_root_certificate(server.trust_certificate.clone())
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let pane = server.root.path().join("pane");
            std::fs::write(pane.join("temp"), b"eigene Bytes").unwrap();
            std::fs::write(pane.join("belegt"), b"fremder Bestand").unwrap();
            std::fs::write(server.root.path().join("sentinel"), b"ausserhalb").unwrap();
            let parent = token(&client, origin, "/pane");
            let source = token(&client, origin, "/pane/temp");
            let parent = if parent_matches {
                parent
            } else {
                "\"falsch\"".into()
            };
            let mut request = client
                .request(
                    Method::from_bytes(method.as_bytes()).unwrap(),
                    format!(
                        "{origin}/pane/{}",
                        if matches!(method, "PUT" | "MKCOL") {
                            "neu"
                        } else {
                            "temp"
                        }
                    ),
                )
                .basic_auth("tester", Some("wertlos"))
                .header("If", format!("<{origin}/pane> ([{parent}])"));
            if matches!(method, "PUT" | "MKCOL") {
                // Falsche Existenzbedingung bei ansonsten wahrer Elternliste.
                request = request.header("If-None-Match", "*");
                if !request_matches {
                    std::fs::write(pane.join("neu"), b"fremder Bestand").unwrap();
                }
            } else {
                request = request.header(
                    "If-Match",
                    if request_matches {
                        source.as_str()
                    } else {
                        "\"falsch\""
                    },
                );
            }
            if method == "PUT" {
                request = request.body(b"neue Bytes".to_vec());
            }
            if method == "MOVE" {
                request = request
                    .header("Destination", format!("{origin}/pane/neu"))
                    .header("Overwrite", "F");
            }
            let response = request.send().unwrap();
            let succeeds = parent_matches && request_matches;
            assert_eq!(
                response.status().is_success(),
                succeeds,
                "{method}: {}",
                response.status()
            );
            if succeeds {
                match method {
                    "PUT" => assert_eq!(std::fs::read(pane.join("neu")).unwrap(), b"neue Bytes"),
                    "MKCOL" => assert!(pane.join("neu").is_dir()),
                    "MOVE" => {
                        assert_eq!(std::fs::read(pane.join("neu")).unwrap(), b"eigene Bytes");
                        assert!(!pane.join("temp").exists());
                    }
                    "DELETE" => assert!(!pane.join("temp").exists()),
                    _ => unreachable!(),
                }
            } else {
                assert_eq!(std::fs::read(pane.join("temp")).unwrap(), b"eigene Bytes");
                if !request_matches && matches!(method, "PUT" | "MKCOL") {
                    assert_eq!(std::fs::read(pane.join("neu")).unwrap(), b"fremder Bestand");
                } else {
                    assert!(!pane.join("neu").exists());
                }
            }
            assert_eq!(
                std::fs::read(pane.join("belegt")).unwrap(),
                b"fremder Bestand"
            );
            assert_eq!(
                std::fs::read(server.root.path().join("sentinel")).unwrap(),
                b"ausserhalb"
            );
            assert_eq!(
                server
                    .state
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .filter(|(m, _)| matches!(m.as_str(), "PUT" | "MKCOL" | "MOVE" | "DELETE"))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn strikte_webdav_abfragen_bleiben_auch_im_unterordner_ohne_providerzugriff() {
    let server = server::Server::new();
    let vfs = WebDavVfs::connect(server.config.clone(), Arc::new(server::TestSecrets)).unwrap();
    std::fs::write(server.root.path().join("pane/sentinel"), b"Pane erhalten").unwrap();
    std::fs::write(server.root.path().join("sentinel"), b"Root erhalten").unwrap();
    let before = server.state.lock().unwrap().requests.len();
    for result in [
        vfs.probe_directory_create_capability_at(std::path::Path::new("/pane")),
        vfs.probe_rename_capability_at(std::path::Path::new("/pane")),
        vfs.probe_permanent_delete_capability_at(std::path::Path::new("/pane")),
        vfs.probe_add_only_capabilities_at(std::path::Path::new("/pane")),
    ] {
        assert_eq!(error_code(&result.unwrap_err()), ErrorCode::Unsupported);
    }
    let path = std::path::Path::new("/pane/sentinel");
    for result in [
        vfs.create_dir(path),
        vfs.create_dir_all(path),
        vfs.remove_file(path),
        vfs.remove_dir(path),
        vfs.remove_dir_all(path),
        vfs.rename_noreplace(path, std::path::Path::new("/pane/neu")),
        vfs.rename_replace_plain_once(path, std::path::Path::new("/pane/neu")),
    ] {
        assert_eq!(error_code(&result.unwrap_err()), ErrorCode::Unsupported);
    }
    assert_eq!(
        error_code(&vfs.open_write_new(path).err().unwrap()),
        ErrorCode::Unsupported
    );
    assert!(!vfs.supports_remove_if_matches());
    let endpoint = ProviderEndpoint::webdav(Arc::new(vfs), std::path::Path::new("/pane")).unwrap();
    let files = endpoint.probe_file_capabilities().unwrap();
    assert!(!files.copy_new && !files.replace && !files.remove_source);
    let deletion = endpoint.probe_delete_capabilities().unwrap();
    assert!(!deletion.quarantine && !deletion.permanent);
    assert_eq!(server.state.lock().unwrap().requests.len(), before);
    assert_eq!(
        std::fs::read(server.root.path().join("pane/sentinel")).unwrap(),
        b"Pane erhalten"
    );
    assert_eq!(
        std::fs::read(server.root.path().join("sentinel")).unwrap(),
        b"Root erhalten"
    );
    assert_eq!(
        std::fs::read_dir(server.root.path().join("pane"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn https_move_kann_altziel_und_eltern_nicht_mit_getaggtem_if_gemeinsam_binden() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Nur eine Ressource pro Liste: genau die vom Produkt erzeugte Syntax.
    // Auch ein RFC-konformer Server darf bei EINER wahren Liste ausführen.
    for (target_matches, pane_matches, root_matches, temp_matches, expected) in [
        (true, true, true, true, StatusCode::NO_CONTENT),
        (false, true, true, true, StatusCode::NO_CONTENT),
        (true, false, true, true, StatusCode::NO_CONTENT),
        (true, true, false, true, StatusCode::NO_CONTENT),
        (false, false, false, true, StatusCode::PRECONDITION_FAILED),
        (true, true, true, false, StatusCode::PRECONDITION_FAILED),
    ] {
        let server = server::Server::new();
        {
            let mut state = server.state.lock().unwrap();
            state.allow_file_overwrite = true;
            state.deny_collection_probes = true;
        }
        let origin = &server.config.origin;
        let client = Client::builder()
            .add_root_certificate(server.trust_certificate.clone())
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        std::fs::write(
            server.root.path().join("pane/Datei.txt"),
            b"bestaetigte Altversion",
        )
        .unwrap();
        std::fs::write(server.root.path().join("sentinel"), b"ausserhalb").unwrap();
        let old = token(&client, origin, "/pane/Datei.txt");
        let pane = token(&client, origin, "/pane");
        let root = token(&client, origin, "/");
        let parents = format!("<{origin}/pane> ([{pane}]) <{origin}/> ([{root}])");
        let new_bytes = b"vollstaendig gepruefter Quellinhalt";
        let put = client
            .put(format!("{origin}/pane/eigener-fileop-tmp"))
            .basic_auth("tester", Some("wertlos"))
            .header("If-None-Match", "*")
            .header("If", &parents)
            .body(new_bytes.to_vec())
            .send()
            .unwrap();
        assert_eq!(put.status(), StatusCode::CREATED);
        let temp = put.headers()["etag"].to_str().unwrap().to_owned();
        let readback = client
            .get(format!("{origin}/pane/eigener-fileop-tmp"))
            .basic_auth("tester", Some("wertlos"))
            .header("If-Match", &temp)
            .send()
            .unwrap()
            .bytes()
            .unwrap();
        assert_eq!(blake3::hash(&readback), blake3::hash(new_bytes));
        assert_eq!(
            std::fs::read(server.root.path().join("pane/Datei.txt")).unwrap(),
            b"bestaetigte Altversion"
        );

        // Der Altzieltausch passiert nach Bestätigung und vor dem einzigen MOVE.
        if !target_matches {
            std::fs::write(
                server.root.path().join("pane/Datei.txt"),
                b"fremde neue Altversion",
            )
            .unwrap();
        }
        let pane = if pane_matches {
            pane
        } else {
            "\"veralteter-pane\"".into()
        };
        let root = if root_matches {
            root
        } else {
            "\"veralteter-root\"".into()
        };
        let source = if temp_matches {
            temp
        } else {
            "\"veralteter-temp\"".into()
        };
        let conditions = format!(
            "<{origin}/pane/Datei.txt> ([{old}]) <{origin}/pane> ([{pane}]) <{origin}/> ([{root}])"
        );
        let result = client
            .request(
                Method::from_bytes(b"MOVE").unwrap(),
                format!("{origin}/pane/eigener-fileop-tmp"),
            )
            .basic_auth("tester", Some("wertlos"))
            .header("Destination", format!("{origin}/pane/Datei.txt"))
            .header("Overwrite", "T")
            .header("If-Match", source)
            .header("If", conditions)
            .send()
            .unwrap();
        assert_eq!(result.status(), expected);
        let target = std::fs::read(server.root.path().join("pane/Datei.txt")).unwrap();
        if expected == StatusCode::NO_CONTENT {
            assert_eq!(target, new_bytes);
            assert!(!server.root.path().join("pane/eigener-fileop-tmp").exists());
        } else {
            assert_eq!(
                target,
                if target_matches {
                    &b"bestaetigte Altversion"[..]
                } else {
                    &b"fremde neue Altversion"[..]
                }
            );
            assert_eq!(
                std::fs::read(server.root.path().join("pane/eigener-fileop-tmp")).unwrap(),
                new_bytes
            );
        }
        assert_eq!(
            std::fs::read(server.root.path().join("sentinel")).unwrap(),
            b"ausserhalb"
        );
        let state = server.state.lock().unwrap();
        for (method, count) in [("PUT", 1), ("MOVE", 1), ("MKCOL", 0), ("DELETE", 0)] {
            assert_eq!(
                state.requests.iter().filter(|(m, _)| m == method).count(),
                count
            );
        }
    }
}
