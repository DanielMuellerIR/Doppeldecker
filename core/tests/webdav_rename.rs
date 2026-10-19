//! Gewöhnliches Rename benötigt keine künstliche Collection oder Löschprobe.

#[path = "support/webdav_namespace.rs"]
mod server;

use doppeldecker_core::{
    error_code, ErrorCode, ProviderEndpoint, RenameExecutionClass, RenameOutcome,
};
use server::{Fault, Server};
use std::path::Path;

#[test]
fn normales_webdav_rename_braucht_keine_kuenstliche_schreib_oder_loeschprobe() {
    let server = Server::new();
    let old = server.root.path().join("pane/Alt.txt");
    let bytes = b"Unveraenderter Inhalt\n";
    std::fs::write(&old, bytes).unwrap();
    let endpoint = server.endpoint();
    let result = endpoint.execute_ordinary_rename(
        Path::new("/pane/Alt.txt"),
        "Grüße %2F.txt",
        &None,
        Some(RenameExecutionClass::BestEffort),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(result.unwrap().class, RenameExecutionClass::BestEffort);
    assert!(!old.exists());
    assert_eq!(
        std::fs::read(server.root.path().join("pane/Grüße %2F.txt")).unwrap(),
        bytes
    );
    assert!(!endpoint.current_transfer_capabilities().add_only);
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
        1
    );
    assert!(!state
        .requests
        .iter()
        .any(|(m, _)| matches!(m.as_str(), "MKCOL" | "PUT" | "DELETE")));
    let conditions = &state.move_if_headers[0];
    assert!(!conditions.contains(&format!("<{}/> ", server.config.origin)));
    assert_eq!(conditions.matches("> ([").count(), 1);
    assert!(conditions.contains(&format!("<{}/pane> ", server.config.origin)));
}

fn rename(
    endpoint: &ProviderEndpoint,
    old: &str,
    new: &str,
) -> std::io::Result<doppeldecker_core::RenameReport> {
    endpoint.execute_ordinary_rename(
        Path::new(old),
        new,
        &None,
        Some(RenameExecutionClass::BestEffort),
    )
}

#[test]
fn normales_webdav_rename_erhaelt_verzeichnisinhalt_und_vorhandene_ziele() {
    let server = Server::new();
    let root = server.root.path().join("pane");
    std::fs::create_dir(root.join("Ordner")).unwrap();
    std::fs::create_dir(root.join("Ordner/Leer")).unwrap();
    std::fs::write(root.join("Ordner/Kind.txt"), b"Kind bleibt erhalten").unwrap();
    let endpoint = server.endpoint();
    assert_eq!(
        rename(&endpoint, "/pane/Ordner", "Neuer Ordner")
            .unwrap()
            .outcome,
        RenameOutcome::Renamed
    );
    assert_eq!(
        std::fs::read(root.join("Neuer Ordner/Kind.txt")).unwrap(),
        b"Kind bleibt erhalten"
    );
    assert!(root.join("Neuer Ordner/Leer").is_dir());
    std::fs::write(root.join("Quelle"), b"Quelle").unwrap();
    std::fs::write(root.join("Belegt"), b"Altes Ziel").unwrap();
    let before = server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|(m, _)| m == "MOVE")
        .count();
    assert_eq!(
        rename(&endpoint, "/pane/Quelle", "Belegt").unwrap().outcome,
        RenameOutcome::Collision
    );
    assert_eq!(std::fs::read(root.join("Quelle")).unwrap(), b"Quelle");
    assert_eq!(std::fs::read(root.join("Belegt")).unwrap(), b"Altes Ziel");
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(m, _)| m == "MOVE")
            .count(),
        before
    );
}

#[test]
fn normales_webdav_rename_bilanziert_verlorene_antwort_und_falschen_readback_ohne_retry() {
    for fault in [
        Fault::LostAfterMove,
        Fault::ErrorAfterMove,
        Fault::MissingParentChild,
        Fault::WrongReadbackEtag,
        Fault::WrongKind,
    ] {
        let server = Server::new();
        let root = server.root.path().join("pane");
        std::fs::create_dir(root.join("Alt")).unwrap();
        std::fs::write(root.join("Alt/Kind"), b"Inhalt").unwrap();
        server.state.lock().unwrap().fault = fault;
        let error = rename(&server.endpoint(), "/pane/Alt", "Neu").unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear, "{error}");
        let message = error.to_string();
        assert!(message.contains("/pane/Alt"));
        assert!(message.contains("/pane/Neu"));
        assert!(
            message
                .matches("WebDAV-Umbenennung hat einen unklaren Ausgang")
                .count()
                <= 1
        );
        assert_eq!(message.matches("Recoverypfade:").count(), 1);
        assert!(!root.join("Alt").exists());
        assert_eq!(std::fs::read(root.join("Neu/Kind")).unwrap(), b"Inhalt");
        let state = server.state.lock().unwrap();
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
            1
        );
        assert!(!state
            .requests
            .iter()
            .any(|(m, _)| matches!(m.as_str(), "MKCOL" | "PUT" | "DELETE")));
    }
}

#[test]
fn normales_webdav_rename_prueft_quellversion_und_bilanziert_spaeten_profilrootwechsel() {
    for fault in [
        Fault::ParentSwapAfterList,
        Fault::ParentSwapAtMove,
        Fault::ProfileRootSwapAtMove,
        Fault::SourceSwapAtMove,
        Fault::LateMoveCollision,
    ] {
        let mut server = Server::new();
        let root = server.root.path().to_path_buf();
        if fault == Fault::ProfileRootSwapAtMove {
            std::fs::create_dir(root.join("profil")).unwrap();
            std::fs::rename(root.join("pane"), root.join("profil/pane")).unwrap();
            server.config.requested_root = "/profil".into();
        }
        let pane = root.join(if fault == Fault::ProfileRootSwapAtMove {
            "profil/pane"
        } else {
            "pane"
        });
        let bytes = b"Originalinhalt";
        std::fs::write(pane.join("Alt"), bytes).unwrap();
        server.state.lock().unwrap().fault = fault;
        let result = rename(&server.endpoint(), "/pane/Alt", "Neu");
        if fault == Fault::LateMoveCollision {
            assert_eq!(result.unwrap().outcome, RenameOutcome::Collision);
            assert_eq!(std::fs::read(pane.join("Alt")).unwrap(), bytes);
            assert_eq!(std::fs::read(pane.join("Neu")).unwrap(), b"Fremdes Ziel");
        } else if fault == Fault::ProfileRootSwapAtMove {
            assert_eq!(error_code(&result.unwrap_err()), ErrorCode::OutcomeUnclear);
            assert_eq!(std::fs::read(pane.join("Neu")).unwrap(), bytes);
            assert!(!pane.join("Alt").exists());
        } else {
            let error = result.unwrap_err();
            assert_eq!(error_code(&error), ErrorCode::PlanDrift, "{error}");
            assert!(!pane.join("Neu").exists());
            if matches!(fault, Fault::ParentSwapAtMove | Fault::ParentSwapAfterList) {
                assert_eq!(
                    std::fs::read(root.join("pane-before-swap/Alt")).unwrap(),
                    bytes
                );
            } else if fault == Fault::SourceSwapAtMove {
                assert_eq!(
                    std::fs::read(pane.join("Alt")).unwrap(),
                    vec![b'X'; bytes.len()]
                );
            } else {
                assert_eq!(std::fs::read(pane.join("Alt")).unwrap(), bytes);
            }
        }
        let state = server.state.lock().unwrap();
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "MOVE").count(),
            if fault == Fault::ParentSwapAfterList {
                0
            } else {
                1
            }
        );
        assert!(!state
            .requests
            .iter()
            .any(|(m, _)| matches!(m.as_str(), "MKCOL" | "PUT" | "DELETE")));
    }
}

#[test]
fn normales_webdav_rename_validiert_klasse_pfade_und_namen_vor_providerzugriff() {
    let server = Server::new();
    std::fs::write(server.root.path().join("pane/Alt"), b"Original").unwrap();
    let endpoint = server.endpoint();
    let too_deep = format!("/pane/{}/Alt", vec!["unter"; 64].join("/"));
    let before = server.state.lock().unwrap().requests.len();
    for (old, new, class) in [
        ("/pane/Alt", "Neu", None),
        ("/pane/Alt", "Neu", Some(RenameExecutionClass::Assured)),
        ("/pane/Alt", "Alt", Some(RenameExecutionClass::BestEffort)),
        ("/pane/Alt", "a/b", Some(RenameExecutionClass::BestEffort)),
        ("/pane/Alt", "a\nb", Some(RenameExecutionClass::BestEffort)),
        ("/fremd/Alt", "Neu", Some(RenameExecutionClass::BestEffort)),
        (
            too_deep.as_str(),
            "Neu",
            Some(RenameExecutionClass::BestEffort),
        ),
    ] {
        assert!(endpoint
            .execute_ordinary_rename(Path::new(old), new, &None, class)
            .is_err());
        assert_eq!(server.state.lock().unwrap().requests.len(), before);
    }
    assert_eq!(
        std::fs::read(server.root.path().join("pane/Alt")).unwrap(),
        b"Original"
    );
}

#[test]
fn normales_webdav_rename_stoppt_nach_sitzungswechsel_waehrend_des_listings() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    let server = Server::new();
    std::fs::write(server.root.path().join("pane/Alt"), b"Original").unwrap();
    let vfs = Arc::new(
        doppeldecker_core::WebDavVfs::connect(server.config.clone(), Arc::new(server::TestSecrets))
            .unwrap(),
    );
    let endpoint = ProviderEndpoint::webdav(vfs.clone(), Path::new("/pane")).unwrap();
    server.state.lock().unwrap().hold_parent_list = true;
    let worker = std::thread::spawn(move || rename(&endpoint, "/pane/Alt", "Neu"));
    let deadline = Instant::now() + Duration::from_secs(4);
    while !server.state.lock().unwrap().parent_list_paused && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(server.state.lock().unwrap().parent_list_paused);
    vfs.reconnect().unwrap();
    server.state.lock().unwrap().resume_parent_list = true;
    let error = worker.join().unwrap().unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::PlanDrift, "{error}");
    assert_eq!(
        std::fs::read(server.root.path().join("pane/Alt")).unwrap(),
        b"Original"
    );
    assert!(!server.root.path().join("pane/Neu").exists());
    assert!(!server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(m, _)| m == "MOVE"));
    let before = server.state.lock().unwrap().requests.len();
    assert!(
        doppeldecker_core::Vfs::rename(&*vfs, Path::new("/pane/Alt"), Path::new("/pane/Neu"))
            .is_err()
    );
    assert_eq!(server.state.lock().unwrap().requests.len(), before);
}
