//! Gewöhnliche Ordneranlage gegen den gemeinsamen künstlichen HTTPS-Server.

#[path = "support/webdav_namespace.rs"]
mod server;
use doppeldecker_core::{
    error_code, DirectoryCreateExecutionClass, DirectoryCreateReport, ErrorCode, ProviderEndpoint,
};
use server::{Fault, Server};
use std::{io, path::Path};

#[test]
fn mkdir_vorbereitungsfrist_wird_nicht_zwischen_elternpruefungen_erneuert() {
    use std::time::{Duration, Instant};
    let server = Server::new();
    let endpoint = server.endpoint();
    let sentinel = server.root.path().join("pane/Sentinel.txt");
    std::fs::write(&sentinel, b"unveraendert").unwrap();
    server.state.lock().unwrap().depth_zero_delays = [Duration::from_secs(25); 4].into();
    let started = Instant::now();
    let result = create(&endpoint, "NachFrist");
    let elapsed = started.elapsed();
    let mutations = server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|(method, _)| matches!(method.as_str(), "MKCOL" | "PUT" | "MOVE" | "DELETE"))
        .count();
    assert_eq!(
        mutations, 0,
        "Nach {elapsed:?} wurde trotz abgelaufener Vorbereitung mutiert: {result:?}"
    );
    assert_eq!(error_code(&result.unwrap_err()), ErrorCode::ConnectionLost);
    assert!(
        elapsed < Duration::from_secs(95),
        "Vorbereitungsfrist plus laufender Request überschritten: {elapsed:?}"
    );
    assert!(!server.root.path().join("pane/NachFrist").exists());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"unveraendert");
}

#[test]
fn normales_webdav_mkdir_braucht_keine_kuenstliche_loeschprobe() {
    let server = Server::new();
    let endpoint = server.endpoint();
    let result = create(&endpoint, "Grüße %2F");
    assert!(result.is_ok(), "{result:?}");
    assert!(server.root.path().join("pane/Grüße %2F").is_dir());
    assert!(!endpoint.current_transfer_capabilities().add_only);
    let (strict, _) = endpoint.file_context().unwrap();
    let before = server.state.lock().unwrap().requests.len();
    assert!(strict
        .create_dir(Path::new("/pane/StrikterOrdner"))
        .is_err());
    assert!(strict
        .open_write_new(Path::new("/pane/StrikterUpload"))
        .is_err());
    assert_eq!(server.state.lock().unwrap().requests.len(), before);

    let state = server.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "MKCOL")
            .count(),
        1
    );
    assert!(!state
        .requests
        .iter()
        .any(|(method, _)| matches!(method.as_str(), "DELETE" | "PUT" | "MOVE")));
}

fn create(endpoint: &ProviderEndpoint, name: &str) -> io::Result<DirectoryCreateReport> {
    endpoint.create_interactive_directory(name, DirectoryCreateExecutionClass::BestEffort)
}

#[test]
fn normales_webdav_mkdir_akzeptiert_eigene_elternversion() {
    let server = Server::new();
    let parent = server.root.path().join("pane");
    std::fs::write(parent.join("neighbor"), b"Erhalten").unwrap();
    filetime::set_file_mtime(
        &parent,
        filetime::FileTime::from_unix_time(1_700_000_000, 0),
    )
    .unwrap();
    server.state.lock().unwrap().changing_collection_tokens = true;
    let result = create(&server.endpoint(), "Neu");
    assert!(result.is_ok(), "{result:?}");
    assert!(parent.join("Neu").is_dir());
    assert_eq!(std::fs::read(parent.join("neighbor")).unwrap(), b"Erhalten");
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "MKCOL").count(),
        1
    );
    assert!(!state
        .requests
        .iter()
        .any(|(m, _)| matches!(m.as_str(), "PUT" | "MOVE" | "DELETE")));
}

#[test]
fn normales_webdav_mkdir_meldet_fremde_bestandsaenderung_nach_anlage_als_unklar() {
    for fault in [
        Fault::SiblingAddedAfterMkdir,
        Fault::SiblingRemovedAfterMkdir,
        Fault::SiblingChangedAfterMkdir,
    ] {
        let server = Server::new();
        let parent = server.root.path().join("pane");
        std::fs::write(parent.join("neighbor"), b"Alt").unwrap();
        server.state.lock().unwrap().changing_collection_tokens = true;
        server.state.lock().unwrap().fault = fault;
        let error = create(&server.endpoint(), "Neu").unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(parent.join("Neu").is_dir());
        let state = server.state.lock().unwrap();
        assert_eq!(
            state.requests.iter().filter(|(m, _)| m == "MKCOL").count(),
            1
        );
        assert!(!state
            .requests
            .iter()
            .any(|(m, _)| matches!(m.as_str(), "PUT" | "MOVE" | "DELETE")));
    }
}

#[test]
fn normales_webdav_mkdir_toleriert_fehlende_anlege_etag_mit_sichtbarem_readback_vertrag() {
    let server = Server::new();
    server.state.lock().unwrap().fault = Fault::NoCreatedEtag;
    let report = create(&server.endpoint(), "Ohne Antwortetag").unwrap();
    assert_eq!(
        report.execution_class,
        DirectoryCreateExecutionClass::BestEffort
    );
    assert!(!report.limitations.is_empty());
    assert!(server.root.path().join("pane/Ohne Antwortetag").is_dir());
}

#[test]
fn normales_webdav_mkdir_bilanziert_antwort_und_readbackfehler_ohne_retry_oder_cleanup() {
    for fault in [
        Fault::LostAfterCreate,
        Fault::MissingReadback,
        Fault::WrongKind,
        Fault::WrongCreatedEtag,
        Fault::ErrorAfterCreate,
        Fault::NonemptyCreated,
        Fault::MissingParentChild,
    ] {
        let server = Server::new();
        let endpoint = server.endpoint();
        server.state.lock().unwrap().fault = fault;
        let error = create(&endpoint, "Neu").unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert!(server.root.path().join("pane/Neu").is_dir());
        assert!(error.to_string().contains("/pane/Neu"));
        let state = server.state.lock().unwrap();
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(method, _)| method == "MKCOL")
                .count(),
            1
        );
        assert!(!state
            .requests
            .iter()
            .any(|(method, _)| matches!(method.as_str(), "DELETE" | "PUT" | "MOVE")));
    }
}

#[test]
fn normales_webdav_mkdir_erhaelt_kollisionen_und_bilanziert_spaeten_elterntausch() {
    for fault in [
        Fault::ParentSwapAfterList,
        Fault::ParentSwapAtMkdir,
        Fault::LateCollision,
    ] {
        let server = Server::new();
        let endpoint = server.endpoint();
        server.state.lock().unwrap().fault = fault;
        let error = create(&endpoint, "Neu").unwrap_err();
        let target = server.root.path().join("pane/Neu");
        if fault == Fault::LateCollision {
            assert_eq!(std::fs::read(&target).unwrap(), b"Fremd");
        } else if fault == Fault::ParentSwapAtMkdir {
            assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
            assert!(target.is_dir());
            assert_eq!(
                std::fs::read(server.root.path().join("pane/Fremd.txt")).unwrap(),
                b"Fremder Elternordner"
            );
            assert_eq!(
                std::fs::read_dir(server.root.path().join("pane-before-swap"))
                    .unwrap()
                    .count(),
                0
            );
        } else {
            assert!(!target.exists());
        }
        let count = server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "MKCOL")
            .count();
        assert_eq!(
            count,
            usize::from(matches!(
                fault,
                Fault::LateCollision | Fault::ParentSwapAtMkdir
            ))
        );
    }
}

#[test]
fn normales_webdav_mkdir_verweigert_namenskollisionen_und_falsche_klasse_vor_zugriff() {
    let server = Server::new();
    let endpoint = server.endpoint();
    std::fs::write(server.root.path().join("pane/Foo"), b"Alt").unwrap();
    let error = create(&endpoint, "foo").unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::AlreadyExists);
    let before = server.state.lock().unwrap().requests.len();
    for name in ["", "..", "a/b", "a\nB"] {
        assert!(create(&endpoint, name).is_err());
    }
    assert!(endpoint
        .create_interactive_directory("Neu", DirectoryCreateExecutionClass::Assured)
        .is_err());
    assert_eq!(server.state.lock().unwrap().requests.len(), before);
    assert_eq!(
        std::fs::read(server.root.path().join("pane/Foo")).unwrap(),
        b"Alt"
    );
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "MKCOL")
            .count(),
        0
    );
}

#[test]
fn normales_webdav_mkdir_meldet_profilrootwechsel_nach_moeglicher_wirkung_als_unklar() {
    let mut server = Server::new();
    std::fs::create_dir(server.root.path().join("profil")).unwrap();
    std::fs::rename(
        server.root.path().join("pane"),
        server.root.path().join("profil/pane"),
    )
    .unwrap();
    server.config.requested_root = "/profil".into();
    let endpoint = server.endpoint();
    server.state.lock().unwrap().fault = Fault::ProfileRootSwapAtMkdir;
    let error = create(&endpoint, "Neu").unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert!(server.root.path().join("profil/pane").is_dir());
    assert!(server.root.path().join("profil/pane/Neu").is_dir());
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "MKCOL")
            .count(),
        1
    );
}

#[test]
fn normales_mkdir_ohne_collection_etag_prueft_namen_und_leeren_readback() {
    let server = Server::new();
    server.state.lock().unwrap().omit_collection_etags = true;
    create(&server.endpoint(), "OhneETag").unwrap();
    assert!(server.root.path().join("pane/OhneETag").is_dir());
    assert_eq!(
        std::fs::read_dir(server.root.path().join("pane/OhneETag"))
            .unwrap()
            .count(),
        0
    );
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.requests.iter().filter(|(m, _)| m == "MKCOL").count(),
        1
    );
    assert!(state.mkdir_if_headers.iter().all(String::is_empty));
}
