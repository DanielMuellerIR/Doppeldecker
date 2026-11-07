//! Gewöhnliche irreversible Wirkungen ohne künstliche Proben.
#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::*;
use std::path::Path;

#[test]
fn webdav_collectiondelete_ist_vor_mutation_sichtbar_dateiblatt_bleibt_entfernbar() {
    let server = server::Server::new();
    let tree = server.root.path().join("pane/Baum");
    std::fs::create_dir_all(tree.join("Tief/Leer")).unwrap();
    std::fs::write(tree.join("Tief/Datei"), b"Inhalt").unwrap();
    std::fs::write(server.root.path().join("pane/Sentinel"), b"Erhalten").unwrap();
    let endpoint = server.endpoint();
    let capabilities = endpoint.probe_delete_capabilities().unwrap();
    assert!(capabilities.permanent && !capabilities.quarantine);
    let (vfs, root) = endpoint.ordinary_file_context().unwrap();
    let error =
        plan_remote_delete(vfs.as_ref(), &root, &[Path::new("/pane/Baum").into()]).unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::Unsupported);
    assert!(error.to_string().contains("rekursiv"));
    assert!(!server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(m, _)| m == "DELETE"));
    let endpoint = ProviderEndpoint::webdav(
        std::sync::Arc::new(
            WebDavVfs::connect(
                server.config.clone(),
                std::sync::Arc::new(server::TestSecrets),
            )
            .unwrap(),
        ),
        Path::new("/pane/Baum/Tief"),
    )
    .unwrap();
    let (vfs, root) = endpoint.ordinary_file_context().unwrap();
    let plan = plan_remote_delete(
        vfs.as_ref(),
        &root,
        &[Path::new("/pane/Baum/Tief/Datei").into()],
    )
    .unwrap();
    assert_eq!(plan.entries, 1);
    let result = execute_remote_delete(
        vfs.as_ref(),
        &plan,
        RemoteDeleteMode::Permanent,
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(result.entries, 1);
    assert!(tree.join("Tief/Leer").is_dir());
    assert!(!tree.join("Tief/Datei").exists());
    assert_eq!(
        std::fs::read(server.root.path().join("pane/Sentinel")).unwrap(),
        b"Erhalten"
    );
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "DELETE").count(),
        1
    );
    assert!(!state
        .requests
        .iter()
        .any(|(m, p)| matches!(m.as_str(), "PUT" | "MOVE" | "MKCOL") || p.contains("probe")));
}

#[test]
fn webdav_loeschdrift_und_abbruch_mutieren_nicht_antwortverlust_wird_nie_wiederholt() {
    for mode in [0, 1, 2] {
        let server = server::Server::new();
        let file = server.root.path().join("pane/Datei");
        std::fs::write(&file, b"Original").unwrap();
        let endpoint = server.endpoint();
        let (vfs, root) = endpoint.ordinary_file_context().unwrap();
        let plan =
            plan_remote_delete(vfs.as_ref(), &root, &[Path::new("/pane/Datei").into()]).unwrap();
        let cancel = Cancel::new();
        if mode == 0 {
            std::fs::write(&file, b"Geaendert").unwrap();
        }
        if mode == 1 {
            cancel.cancel();
        }
        if mode == 2 {
            server.state.lock().unwrap().fault = server::Fault::LostAfterDelete;
        }
        let result = execute_remote_delete(
            vfs.as_ref(),
            &plan,
            RemoteDeleteMode::Permanent,
            &cancel,
            &mut |_| {},
        );
        if mode == 0 {
            assert_eq!(
                error_code(result.as_ref().unwrap_err()),
                ErrorCode::PlanDrift
            );
        }
        if mode == 1 {
            assert!(result.as_ref().unwrap().cancelled);
        }
        if mode == 2 {
            assert!(
                result.is_ok()
                    || error_code(result.as_ref().unwrap_err()) == ErrorCode::OutcomeUnclear
            );
        }
        let count = server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(m, _)| m == "DELETE")
            .count();
        assert_eq!(count, usize::from(mode == 2));
        assert_eq!(file.exists(), mode != 2);
    }
}

#[test]
fn webdav_ersetzen_benutzt_altzielbedingung_und_genau_einen_move() {
    for fault in [
        server::Fault::None,
        server::Fault::NewEtagAfterMove,
        server::Fault::LateMoveCollision,
        server::Fault::LostAfterMove,
    ] {
        let server = server::Server::new();
        {
            let mut state = server.state.lock().unwrap();
            state.allow_file_overwrite = true;
            state.changing_collection_tokens = fault == server::Fault::NewEtagAfterMove;
        }
        let local = tempfile::tempdir().unwrap();
        let root = local.path().canonicalize().unwrap();
        std::fs::write(root.join("Datei.txt"), b"Neuer Inhalt").unwrap();
        std::fs::write(server.root.path().join("pane/Datei.txt"), b"Alt").unwrap();
        std::fs::write(server.root.path().join("pane/Nachbar"), b"Erhalten").unwrap();
        let source = ProviderEndpoint::local(&root).unwrap();
        let target = server.endpoint();
        let items = vec![root.join("Datei.txt")];
        let contexts = source
            .interactive_file_contexts(&target, &items, false, true, &Cancel::new())
            .unwrap();
        assert!(
            contexts
                .target_capabilities
                .as_ref()
                .unwrap()
                .replace_best_effort
        );
        server.state.lock().unwrap().fault = fault;
        let result = fileops::copy_items(
            fileops::FileOperationRequest {
                src: contexts.source_vfs.as_ref(),
                dst: contexts.target_vfs.as_ref(),
                items: &items,
                dst_dir: Path::new("/pane"),
                policy: fileops::OverwritePolicy::Overwrite,
                sandbox: &None,
            },
            &Cancel::new(),
            &mut |_| {},
        );
        if matches!(fault, server::Fault::None | server::Fault::NewEtagAfterMove) {
            assert!(result.is_ok(), "{result:?}");
        }
        if fault == server::Fault::LateMoveCollision {
            assert!(result.is_err());
        }
        if fault == server::Fault::LostAfterMove {
            assert!(result.is_err());
        }
        let content = std::fs::read(server.root.path().join("pane/Datei.txt")).unwrap();
        if fault == server::Fault::LateMoveCollision {
            assert_eq!(content, b"Fremdes Ziel");
        } else {
            assert_eq!(content, b"Neuer Inhalt");
        }
        assert_eq!(
            std::fs::read(server.root.path().join("pane/Nachbar")).unwrap(),
            b"Erhalten"
        );
        let state = server.state.lock().unwrap();
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
            1
        );
        assert_eq!(state.requests.iter().filter(|(m, _)| m == "PUT").count(), 1);
        assert!(!state
            .requests
            .iter()
            .any(|(m, p)| m == "MKCOL" || p.contains("probe")));
        assert_eq!(state.move_if_headers.len(), 1);
        assert_eq!(state.move_if_headers[0].matches('<').count(), 1);
        assert!(state.move_if_headers[0].contains("/pane/Datei.txt>"));
    }
}
