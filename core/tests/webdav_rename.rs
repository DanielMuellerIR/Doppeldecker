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
    assert!(conditions.is_empty());
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

#[test]
fn webdav_stapel_ausschlussprobe_zwei_moves_mit_collection_etagwechsel() {
    for changing in [false, true] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        std::fs::write(pane.join("a"), b"erster Inhalt").unwrap();
        std::fs::write(pane.join("b"), b"zweiter Inhalt").unwrap();
        server.state.lock().unwrap().changing_collection_tokens = changing;
        let endpoint = server.endpoint();
        let first = rename(&endpoint, "/pane/a", "new-a");
        let second = if first.is_ok() {
            Some(rename(&endpoint, "/pane/b", "new-b"))
        } else {
            None
        };
        let state = server.state.lock().unwrap();
        let moves = state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count();
        assert!(!state
            .requests
            .iter()
            .any(|(method, _)| matches!(method.as_str(), "MKCOL" | "PUT" | "DELETE" | "GET")));
        eprintln!("changing_collection_tokens={changing}: first={first:?}; second={second:?}; MOVE={moves}; first_effect={}; second_untouched={}",pane.join("new-a").exists(),pane.join("b").exists());
        assert!(
            first.is_ok(),
            "Eigener Collection-ETagwechsel muss ersten Rename belegbar lassen: {first:?}"
        );
        assert!(second.unwrap().is_ok());
        assert_eq!(moves, 2);
    }
}

#[test]
fn webdav_eigener_parentrefresh_erfordert_exakten_bestand_und_sitzung() {
    for fault in [
        Fault::SiblingAddedAfterMove,
        Fault::SiblingRemovedAfterMove,
        Fault::SiblingChangedAfterMove,
        Fault::SiblingMetadataAfterMove,
        Fault::AncestorChangedAfterMove,
        Fault::LostAfterMove,
        Fault::LateMoveCollision,
    ] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        std::fs::write(pane.join("a"), b"A").unwrap();
        std::fs::write(pane.join("neighbor"), b"N").unwrap();
        {
            let mut state = server.state.lock().unwrap();
            state.changing_collection_tokens = true;
            state.fault = fault;
        }
        let result = rename(&server.endpoint(), "/pane/a", "new-a");
        let error = result.unwrap_err();
        assert_eq!(
            error_code(&error),
            if fault == Fault::LateMoveCollision {
                ErrorCode::PlanDrift
            } else {
                ErrorCode::OutcomeUnclear
            },
            "fault={}: {error}",
            fault as u32
        );
        let state = server.state.lock().unwrap();
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(method, _)| method == "MOVE")
                .count(),
            1
        );
        assert!(!state
            .requests
            .iter()
            .any(|(method, _)| matches!(method.as_str(), "GET" | "PUT" | "DELETE" | "MKCOL")));
    }
}

#[test]
fn webdav_stapel_bindet_tokens_abbruch_sitzung_und_einzelnen_move() {
    use doppeldecker_core::batch_rename::BatchRenameOptions;
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::{Cancel, WebDavVfs};
    use std::sync::Arc;
    for change in 0..9 {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        for name in ["a", "b", "c"] {
            std::fs::write(pane.join(name), name.as_bytes()).unwrap();
        }
        server.state.lock().unwrap().changing_collection_tokens = change != 1;
        let vfs = Arc::new(
            WebDavVfs::connect(server.config.clone(), Arc::new(server::TestSecrets)).unwrap(),
        );
        let endpoint = ProviderEndpoint::webdav(vfs.clone(), Path::new("/pane")).unwrap();
        let plan = PreparedWebDavBatchRename::prepare(
            &endpoint,
            &["a".into(), "b".into(), "c".into()],
            &BatchRenameOptions {
                find: String::new(),
                replacement: String::new(),
                prefix: "new-".into(),
                suffix: String::new(),
                preserve_extension: true,
                trim_outer_spaces: false,
                numbering: None,
            },
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(plan.execution_class(), RenameExecutionClass::BestEffort);
        assert!(!server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(method, _)| matches!(
                method.as_str(),
                "MOVE" | "GET" | "PUT" | "DELETE" | "MKCOL"
            )));
        let cancel = Cancel::new();
        if change == 7 {
            cancel.cancel();
        }
        let mut calls = 0;
        let report = plan.execute(
            &cancel,
            &mut || {
                calls += 1;
                if change == 8 && calls == 2 {
                    return Err(doppeldecker_core::coded_error(
                        std::io::ErrorKind::Interrupted,
                        ErrorCode::PlanDrift,
                        "Profilrevision gewechselt",
                    ));
                }
                Ok(())
            },
            &mut |done, _| {
                if done == 1 {
                    match change {
                        1 => std::fs::write(pane.join("new-b"), b"belegt").unwrap(),
                        2 => std::fs::write(pane.join("b"), b"B").unwrap(),
                        3 => std::fs::write(pane.join("foreign"), b"fremd").unwrap(),
                        4 => vfs.reconnect().unwrap(),
                        5 => server.state.lock().unwrap().fault = Fault::LostAfterMove,
                        6 => cancel.cancel(),
                        _ => {}
                    }
                }
            },
        );
        let state = server.state.lock().unwrap();
        let moves = state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count();
        match change {
            0 => {
                assert_eq!(report.renamed, 3, "{:?}", report.rows);
                assert_eq!(moves, 3);
            }
            1 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (2, 1),
                    "{:?}",
                    report.rows
                );
                assert_eq!(moves, 2);
                assert_eq!(std::fs::read(pane.join("new-b")).unwrap(), b"belegt");
            }
            2..=4 | 8 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (1, 2),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                assert_eq!(moves, 1);
            }
            5 => {
                assert_eq!(
                    (
                        report.renamed,
                        report.outcome_unclear,
                        report.failed_unchanged
                    ),
                    (1, 1, 1),
                    "{:?}",
                    report.rows
                );
                assert_eq!(moves, 2);
                assert!(pane.join("new-b").exists());
                assert!(pane.join("c").exists());
            }
            6 => {
                assert_eq!((report.renamed, report.cancelled), (1, 2));
                assert_eq!(moves, 1);
            }
            7 => {
                assert_eq!(report.cancelled, 3);
                assert_eq!(moves, 0);
            }
            _ => unreachable!(),
        }
        assert!(!state
            .requests
            .iter()
            .any(|(method, _)| matches!(method.as_str(), "GET" | "PUT" | "DELETE" | "MKCOL")));
    }
}

#[test]
fn webdav_stapel_nachbeleg_rahmt_parentversion_und_sitzung_ein() {
    use doppeldecker_core::batch_rename::BatchRenameOptions;
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::{Cancel, WebDavVfs};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    for reconnect in [false, true] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        for name in ["a", "b"] {
            std::fs::write(pane.join(name), name.as_bytes()).unwrap();
        }
        {
            let mut state = server.state.lock().unwrap();
            state.changing_collection_tokens = true;
            state.fault = Fault::PauseAfterMove;
        }
        let vfs = Arc::new(
            WebDavVfs::connect(server.config.clone(), Arc::new(server::TestSecrets)).unwrap(),
        );
        let endpoint = ProviderEndpoint::webdav(vfs.clone(), Path::new("/pane")).unwrap();
        let plan = PreparedWebDavBatchRename::prepare(
            &endpoint,
            &["a".into(), "b".into()],
            &BatchRenameOptions {
                find: String::new(),
                replacement: String::new(),
                prefix: "new-".into(),
                suffix: String::new(),
                preserve_extension: true,
                trim_outer_spaces: false,
                numbering: None,
            },
            &Cancel::new(),
        )
        .unwrap();
        let worker = std::thread::spawn(move || {
            plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {})
        });
        let deadline = Instant::now() + Duration::from_secs(4);
        while !server.state.lock().unwrap().parent_list_paused && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(server.state.lock().unwrap().parent_list_paused);
        if reconnect {
            vfs.reconnect().unwrap();
        } else {
            let version =
                filetime::FileTime::from_last_modification_time(&std::fs::metadata(&pane).unwrap());
            filetime::set_file_mtime(
                &pane,
                filetime::FileTime::from_unix_time(
                    version.unix_seconds() + 2,
                    version.nanoseconds(),
                ),
            )
            .unwrap();
        }
        server.state.lock().unwrap().resume_parent_list = true;
        let report = worker.join().unwrap();
        assert_eq!(
            (report.outcome_unclear, report.failed_unchanged),
            (1, 1),
            "{:?}",
            report.rows
        );
        assert!(pane.join("new-a").exists());
        assert!(pane.join("b").exists());
        assert_eq!(
            server
                .state
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|(method, _)| method == "MOVE")
                .count(),
            1
        );
    }
}

#[test]
fn webdav_sibling_ohne_starken_beleg_stoppt_vor_move() {
    for fault in [Fault::MissingSiblingEtag, Fault::WeakSiblingEtag] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        std::fs::write(pane.join("a"), b"Quelle mit starkem Token").unwrap();
        std::fs::create_dir(pane.join("neighbor")).unwrap();
        {
            let mut state = server.state.lock().unwrap();
            state.fault = fault;
            state.changing_collection_tokens = true;
        }
        let error = rename(&server.endpoint(), "/pane/a", "new-a").unwrap_err();
        let state = server.state.lock().unwrap();
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|(method, _)| method == "MOVE")
                .count(),
            0,
            "Vorhersehbar unvollstaendiger Beleg darf keine Wirkung beginnen: {error}"
        );
        assert!(pane.join("a").exists());
        assert!(!pane.join("new-a").exists());
        assert_ne!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(error_code(&error), ErrorCode::Unsupported);
        drop(state);
        use doppeldecker_core::batch_rename::BatchRenameOptions;
        let options = BatchRenameOptions {
            find: String::new(),
            replacement: String::new(),
            prefix: "new-".into(),
            suffix: String::new(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        };
        let plan = doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename::prepare(
            &server.endpoint(),
            &["a".into()],
            &options,
            &doppeldecker_core::Cancel::new(),
        );
        assert_eq!(
            error_code(
                &plan
                    .err()
                    .expect("Unbelegbarer Nachbar muss Vorbereitung sperren")
            ),
            ErrorCode::Unsupported
        );
        assert!(!server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(method, _)| method == "MOVE"));
    }
}

#[test]
fn webdav_stapel_nummeriert_reihenfolge_mit_gebundenem_parentrefresh() {
    use doppeldecker_core::batch_rename::{
        BatchRenameNumberPosition, BatchRenameNumbering, BatchRenameOptions,
    };
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    let server = Server::new();
    let pane = server.root.path().join("pane");
    std::fs::write(pane.join("a.txt"), b"A").unwrap();
    std::fs::write(pane.join("b.txt"), b"B").unwrap();
    server.state.lock().unwrap().changing_collection_tokens = true;
    let options = BatchRenameOptions {
        find: String::new(),
        replacement: String::new(),
        prefix: String::new(),
        suffix: String::new(),
        preserve_extension: true,
        trim_outer_spaces: false,
        numbering: Some(BatchRenameNumbering {
            start: 5,
            step: 3,
            min_digits: 2,
            position: BatchRenameNumberPosition::Before,
        }),
    };
    let plan = PreparedWebDavBatchRename::prepare(
        &server.endpoint(),
        &["b.txt".into(), "a.txt".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        plan.rows()
            .iter()
            .map(|row| row.new_name.as_str())
            .collect::<Vec<_>>(),
        ["05_b.txt", "08_a.txt"]
    );
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(report.renamed, 2, "{:?}", report.rows);
    assert_eq!(std::fs::read(pane.join("05_b.txt")).unwrap(), b"B");
    assert_eq!(std::fs::read(pane.join("08_a.txt")).unwrap(), b"A");
    let state = server.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count(),
        2
    );
    assert!(!state
        .requests
        .iter()
        .any(|(method, _)| matches!(method.as_str(), "GET" | "PUT" | "DELETE" | "MKCOL")));
}

#[test]
fn webdav_stapel_trim_erhaelt_kollision_und_benennt_unabhaengige_datei_um() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    let server = Server::new();
    let pane = server.root.path().join("pane");
    for (name, bytes) in [
        (" a.txt ", "Quelle"),
        ("a.txt", "Ziel"),
        (" good.txt ", "Gut"),
    ] {
        std::fs::write(pane.join(name), bytes).unwrap();
    }
    server.state.lock().unwrap().changing_collection_tokens = true;
    let options = BatchRenameOptions {
        find: String::new(),
        replacement: String::new(),
        prefix: String::new(),
        suffix: String::new(),
        preserve_extension: true,
        trim_outer_spaces: true,
        numbering: None,
    };
    let plan = PreparedWebDavBatchRename::prepare(
        &server.endpoint(),
        &[" a.txt ".into(), " good.txt ".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
    assert_eq!(plan.rows()[1].new_name, "good.txt");
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(report.renamed, 1, "{:?}", report.rows);
    assert_eq!(std::fs::read_to_string(pane.join("a.txt")).unwrap(), "Ziel");
    assert_eq!(
        std::fs::read_to_string(pane.join(" a.txt ")).unwrap(),
        "Quelle"
    );
    assert_eq!(
        std::fs::read_to_string(pane.join("good.txt")).unwrap(),
        "Gut"
    );
    let state = server.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count(),
        1
    );
    assert!(!state
        .requests
        .iter()
        .any(|(method, _)| matches!(method.as_str(), "GET" | "PUT" | "DELETE" | "MKCOL")));
}

#[test]
fn webdav_kette_verlorene_freigeberantwort_stoppt_ohne_move_retry() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    let server = Server::new();
    let pane = server.root.path().join("pane");
    for name in ["a", "aa", "aaa", "b"] {
        std::fs::write(pane.join(name), name).unwrap();
    }
    {
        let mut state = server.state.lock().unwrap();
        state.changing_collection_tokens = true;
        state.fault = Fault::LostAfterMove;
    }
    let plan = PreparedWebDavBatchRename::prepare(
        &server.endpoint(),
        &["a".into(), "aa".into(), "aaa".into(), "b".into()],
        &BatchRenameOptions {
            find: String::new(),
            replacement: String::new(),
            prefix: "a".into(),
            suffix: String::new(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        },
        &Cancel::new(),
    )
    .unwrap();
    assert!(
        plan.rows()
            .iter()
            .all(|row| row.status == BatchRenameStatus::Ready),
        "{:?}",
        plan.rows()
    );
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(
        (
            report.renamed,
            report.outcome_unclear,
            report.failed_unchanged
        ),
        (0, 1, 3),
        "{:?}",
        report.rows
    );
    assert_eq!(report.rows[2].status, BatchRenameStatus::OutcomeUnclear);
    assert_eq!(std::fs::read(pane.join("aaaa")).unwrap(), b"aaa");
    for name in ["a", "aa", "b"] {
        assert_eq!(std::fs::read(pane.join(name)).unwrap(), name.as_bytes());
    }
    assert!(!pane.join("ab").exists());
    let state = server.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count(),
        1
    );
    assert!(!state
        .requests
        .iter()
        .any(|(method, _)| matches!(method.as_str(), "GET" | "PUT" | "DELETE" | "MKCOL")));
}

#[test]
fn webdav_ketten_binden_erfolg_kollision_drift_und_abbruch() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::{Cancel, WebDavVfs};
    use std::sync::Arc;
    for change in 0..10 {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        for name in ["a", "aa", "aaa", "b"] {
            std::fs::write(pane.join(name), name).unwrap();
        }
        {
            let mut state = server.state.lock().unwrap();
            state.changing_collection_tokens = change != 1;
            if change == 1 {
                state.fault = Fault::LateMoveCollision;
            }
        }
        let vfs = Arc::new(
            WebDavVfs::connect(server.config.clone(), Arc::new(server::TestSecrets)).unwrap(),
        );
        let endpoint = ProviderEndpoint::webdav(vfs.clone(), Path::new("/pane")).unwrap();
        let plan = PreparedWebDavBatchRename::prepare(
            &endpoint,
            &["a".into(), "aa".into(), "aaa".into(), "b".into()],
            &BatchRenameOptions {
                find: String::new(),
                replacement: String::new(),
                prefix: "a".into(),
                suffix: String::new(),
                preserve_extension: true,
                trim_outer_spaces: false,
                numbering: None,
            },
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(plan.execution_class(), RenameExecutionClass::BestEffort);
        assert_eq!(
            plan.rows()
                .iter()
                .map(|row| (row.old_name.as_str(), row.new_name.as_str()))
                .collect::<Vec<_>>(),
            [("a", "aa"), ("aa", "aaa"), ("aaa", "aaaa"), ("b", "ab")]
        );
        assert!(
            plan.rows()
                .iter()
                .all(|row| row.status == BatchRenameStatus::Ready),
            "change={change}: {:?}",
            plan.rows()
        );
        let cancel = Cancel::new();
        if change == 7 {
            cancel.cancel();
        }
        if change == 9 {
            std::fs::write(pane.join("aaa"), b"XXX").unwrap();
        }
        let mut calls = 0;
        let report = plan.execute(
            &cancel,
            &mut || {
                calls += 1;
                if change == 8 && calls == 2 {
                    return Err(doppeldecker_core::coded_error(
                        std::io::ErrorKind::Interrupted,
                        ErrorCode::PlanDrift,
                        "Profilrevision gewechselt",
                    ));
                }
                Ok(())
            },
            &mut |done, _| {
                if done == 1 {
                    match change {
                        2 => std::fs::write(pane.join("aa"), b"XX").unwrap(),
                        3 => std::fs::write(pane.join("foreign"), b"fremd").unwrap(),
                        4 => vfs.reconnect().unwrap(),
                        5 => server.state.lock().unwrap().fault = Fault::LostAfterMove,
                        6 => cancel.cancel(),
                        _ => {}
                    }
                }
            },
        );
        let state = server.state.lock().unwrap();
        let moves = state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .collect::<Vec<_>>();
        match change {
            0 => {
                assert_eq!(report.renamed, 4, "{:?}", report.rows);
                assert_eq!(
                    moves
                        .iter()
                        .map(|(_, path)| path.as_str())
                        .collect::<Vec<_>>(),
                    ["/pane/aaa", "/pane/b", "/pane/aa", "/pane/a"]
                );
                for (name, bytes) in [("aa", "a"), ("aaa", "aa"), ("aaaa", "aaa"), ("ab", "b")] {
                    assert_eq!(std::fs::read(pane.join(name)).unwrap(), bytes.as_bytes());
                }
            }
            1 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (1, 3),
                    "{:?}",
                    report.rows
                );
                assert_eq!(moves.len(), 2);
                assert_eq!(std::fs::read(pane.join("aaaa")).unwrap(), b"Fremdes Ziel");
                for name in ["a", "aa", "aaa"] {
                    assert_eq!(std::fs::read(pane.join(name)).unwrap(), name.as_bytes());
                }
                assert_eq!(std::fs::read(pane.join("ab")).unwrap(), b"b");
            }
            2 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (2, 2),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[1].code, Some(ErrorCode::PlanDrift));
                assert_eq!(moves.len(), 2);
                assert_eq!(std::fs::read(pane.join("aa")).unwrap(), b"XX");
                assert_eq!(std::fs::read(pane.join("a")).unwrap(), b"a");
            }
            3 | 4 | 8 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (1, 3),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[3].code, Some(ErrorCode::PlanDrift));
                assert_eq!(moves.len(), 1);
                assert!(
                    pane.join("a").exists() && pane.join("aa").exists() && pane.join("b").exists()
                );
            }
            5 => {
                assert_eq!(
                    (
                        report.renamed,
                        report.outcome_unclear,
                        report.failed_unchanged
                    ),
                    (1, 1, 2),
                    "{:?}",
                    report.rows
                );
                assert_eq!(moves.len(), 2);
                assert!(
                    pane.join("a").exists() && pane.join("aa").exists() && pane.join("ab").exists()
                );
            }
            6 => {
                assert_eq!((report.renamed, report.cancelled), (1, 3));
                assert_eq!(moves.len(), 1);
            }
            7 => {
                assert_eq!(report.cancelled, 4);
                assert!(moves.is_empty());
            }
            9 => {
                assert_eq!(
                    (report.renamed, report.failed_unchanged),
                    (0, 4),
                    "{:?}",
                    report.rows
                );
                assert_eq!(report.rows[2].code, Some(ErrorCode::PlanDrift));
                assert!(moves.is_empty());
                assert!(
                    pane.join("a").exists() && pane.join("aa").exists() && pane.join("b").exists()
                );
            }
            _ => unreachable!(),
        }
        assert!(!state
            .requests
            .iter()
            .any(|(method, _)| matches!(method.as_str(), "GET" | "PUT" | "DELETE" | "MKCOL")));
    }
}

#[test]
fn webdav_kette_behaelt_nummerierung_in_auswahlreihenfolge() {
    use doppeldecker_core::batch_rename::{
        BatchRenameNumberPosition, BatchRenameNumbering, BatchRenameOptions,
    };
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    let server = Server::new();
    let pane = server.root.path().join("pane");
    for name in ["a", "1_a", "b"] {
        std::fs::write(pane.join(name), name).unwrap();
    }
    server.state.lock().unwrap().changing_collection_tokens = true;
    let plan = PreparedWebDavBatchRename::prepare(
        &server.endpoint(),
        &["a".into(), "1_a".into(), "b".into()],
        &BatchRenameOptions {
            find: String::new(),
            replacement: String::new(),
            prefix: String::new(),
            suffix: String::new(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: Some(BatchRenameNumbering {
                start: 1,
                step: 1,
                min_digits: 1,
                position: BatchRenameNumberPosition::Before,
            }),
        },
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        plan.rows()
            .iter()
            .map(|row| row.new_name.as_str())
            .collect::<Vec<_>>(),
        ["1_a", "2_1_a", "3_b"]
    );
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    assert_eq!(report.renamed, 3, "{:?}", report.rows);
    for (name, bytes) in [("1_a", "a"), ("2_1_a", "1_a"), ("3_b", "b")] {
        assert_eq!(std::fs::read(pane.join(name)).unwrap(), bytes.as_bytes());
    }
    let state = server.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        ["/pane/1_a", "/pane/b", "/pane/a"]
    );
}

fn webdav_spelling_options(
    old: &str,
    new: &str,
) -> doppeldecker_core::batch_rename::BatchRenameOptions {
    doppeldecker_core::batch_rename::BatchRenameOptions {
        find: old.into(),
        replacement: new.into(),
        prefix: String::new(),
        suffix: String::new(),
        preserve_extension: false,
        trim_outer_spaces: false,
        numbering: None,
    }
}
#[test]
fn webdav_stapel_schreibweise_wird_ueber_oeffentliche_vorbereitung_zugelassen() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    let server = Server::new();
    std::fs::write(server.root.path().join("pane/Casefile"), b"Inhalt").unwrap();
    let plan = PreparedWebDavBatchRename::prepare(
        &server.endpoint(),
        &["Casefile".into()],
        &webdav_spelling_options("Casefile", "casefile"),
        &doppeldecker_core::Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        plan.rows()[0].status,
        BatchRenameStatus::Ready,
        "{:?}",
        plan.rows()
    );
    assert!(!server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(method, _)| method == "MOVE"));
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpellingHostEntry {
    name: Vec<u8>,
    device: u64,
    inode: u64,
    links: u64,
    directory: bool,
    bytes: Vec<u8>,
    children: Vec<SpellingHostEntry>,
}
fn spelling_host_tree(root: &Path) -> Vec<SpellingHostEntry> {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let mut entries = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            SpellingHostEntry {
                name: entry.file_name().as_bytes().to_vec(),
                device: metadata.dev(),
                inode: metadata.ino(),
                links: metadata.nlink(),
                directory: metadata.is_dir(),
                bytes: if metadata.is_file() {
                    std::fs::read(entry.path()).unwrap()
                } else {
                    Vec::new()
                },
                children: if metadata.is_dir() {
                    spelling_host_tree(&entry.path())
                } else {
                    Vec::new()
                },
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}
fn spelling_target_is_source_alias(source: &Path, target: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(target) {
        Ok(target) => {
            let source = std::fs::symlink_metadata(source).unwrap();
            assert_eq!((source.dev(), source.ino()), (target.dev(), target.ino()));
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("Zielalias kann nicht geprüft werden: {error}"),
    }
}
fn assert_webdav_spelling_requests(server: &Server, expected: usize) {
    let state = server.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count(),
        expected
    );
    assert!(!state
        .requests
        .iter()
        .any(|(method, _)| matches!(method.as_str(), "GET" | "PUT" | "MKCOL" | "DELETE")));
    assert_eq!(state.move_if_headers.len(), expected);
    for condition in &state.move_if_headers {
        assert!(condition.is_empty());
    }
}
#[test]
fn webdav_stapel_schreibweisen_behalten_rohnamen_inhalt_inode_und_aussenhardlink() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    for (old, new, directory, outside) in [
        ("Casefile", "casefile", false, false),
        ("CaseDir", "casedir", true, false),
        ("é", "e\u{301}", false, false),
        ("e\u{301}", "é", false, false),
        ("Outside", "outside", false, true),
    ] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        if directory {
            std::fs::create_dir(pane.join(old)).unwrap();
            std::fs::create_dir(pane.join(old).join("empty")).unwrap();
            std::fs::write(pane.join(old).join("child"), b"nested bytes").unwrap();
        } else {
            std::fs::write(pane.join(old), b"source bytes").unwrap();
        }
        if outside {
            std::fs::hard_link(pane.join(old), server.root.path().join("outside-link")).unwrap();
        }
        std::fs::write(pane.join("sentinel"), b"unchanged sentinel").unwrap();
        let before = spelling_host_tree(server.root.path());
        let target_is_alias = spelling_target_is_source_alias(&pane.join(old), &pane.join(new));
        server.state.lock().unwrap().changing_collection_tokens = true;
        let plan = PreparedWebDavBatchRename::prepare(
            &server.endpoint(),
            &[old.into()],
            &webdav_spelling_options(old, new),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Ready);
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        if target_is_alias {
            assert_eq!(
                report.rows[0].status,
                BatchRenameStatus::FailedUnchanged,
                "{old}: {:?}",
                report.rows
            );
            assert_eq!(report.rows[0].code, Some(ErrorCode::AlreadyExists));
            assert_eq!(spelling_host_tree(server.root.path()), before);
        } else {
            assert_eq!(report.renamed, 1, "{old}: {:?}", report.rows);
            let mut expected = before;
            let children = &mut expected
                .iter_mut()
                .find(|entry| entry.name == b"pane")
                .unwrap()
                .children;
            children
                .iter_mut()
                .find(|entry| entry.name == old.as_bytes())
                .unwrap()
                .name = new.as_bytes().to_vec();
            children.sort_by(|a, b| a.name.cmp(&b.name));
            assert_eq!(spelling_host_tree(server.root.path()), expected);
        }
        assert_webdav_spelling_requests(&server, 1);
    }
}
#[test]
fn webdav_stapel_schreibweisen_uebernehmen_nur_eigenen_starken_parentrefresh() {
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    let server = Server::new();
    let pane = server.root.path().join("pane");
    std::fs::write(pane.join("Ab"), b"first").unwrap();
    std::fs::write(pane.join("Ac"), b"second").unwrap();
    server.state.lock().unwrap().changing_collection_tokens = true;
    let options = doppeldecker_core::batch_rename::BatchRenameOptions {
        prefix: "a".into(),
        ..webdav_spelling_options("A", "")
    };
    let plan = PreparedWebDavBatchRename::prepare(
        &server.endpoint(),
        &["Ab".into(), "Ac".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    let before = spelling_host_tree(server.root.path());
    let aliases = [
        spelling_target_is_source_alias(&pane.join("Ab"), &pane.join("ab")),
        spelling_target_is_source_alias(&pane.join("Ac"), &pane.join("ac")),
    ];
    let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
    let mut expected = before;
    let children = &mut expected
        .iter_mut()
        .find(|entry| entry.name == b"pane")
        .unwrap()
        .children;
    for (index, (old, new)) in [("Ab", "ab"), ("Ac", "ac")].iter().enumerate() {
        if aliases[index] {
            assert_eq!(report.rows[index].code, Some(ErrorCode::AlreadyExists));
        } else {
            assert_eq!(
                report.rows[index].status,
                doppeldecker_core::batch_rename::BatchRenameStatus::Renamed
            );
            children
                .iter_mut()
                .find(|entry| entry.name == old.as_bytes())
                .unwrap()
                .name = new.as_bytes().to_vec();
        }
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        report.failed_unchanged,
        aliases.iter().filter(|alias| **alias).count()
    );
    assert_eq!(
        report.renamed,
        aliases.iter().filter(|alias| !**alias).count()
    );
    assert_eq!(spelling_host_tree(server.root.path()), expected);
    assert_webdav_spelling_requests(&server, 2);
}
#[test]
fn webdav_stapel_schreibweisen_lassen_fremde_und_gleiche_inode_aliasgruppen_gesperrt() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    for same_inode in [false, true] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        std::fs::write(pane.join("Foo"), b"source").unwrap();
        let target_is_alias = spelling_target_is_source_alias(&pane.join("Foo"), &pane.join("FOO"));
        if !target_is_alias {
            if same_inode {
                std::fs::hard_link(pane.join("Foo"), pane.join("FOO")).unwrap();
            } else {
                std::fs::write(pane.join("FOO"), b"foreign").unwrap();
            }
        } else {
            if same_inode {
                continue;
            }
            // Wo das Dateisystem nur einen Namen erlaubt, prüft dieser künstliche
            // zweite Listingname die Gruppensperre, keinen realen Hardlinkfall.
            server.state.lock().unwrap().download_aliases.push((
                pane.clone(),
                "FOO".into(),
                "\"synthetic-alias\"".into(),
            ));
        }
        let before = spelling_host_tree(server.root.path());
        let plan = PreparedWebDavBatchRename::prepare(
            &server.endpoint(),
            &["Foo".into()],
            &webdav_spelling_options("Foo", "foo"),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(plan.rows()[0].status, BatchRenameStatus::Blocked);
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |_, _| {});
        assert_eq!(report.rows[0].status, BatchRenameStatus::Blocked);
        assert_eq!(spelling_host_tree(server.root.path()), before);
        assert_webdav_spelling_requests(&server, 0);
    }
}
#[test]
fn webdav_stapel_schreibweisen_ungewissheit_stoppt_rest_ohne_heilung_oder_retry() {
    use doppeldecker_core::batch_rename::BatchRenameStatus;
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::Cancel;
    for fault in [
        Fault::NoopMoveAcknowledgement,
        Fault::LostAfterMove,
        Fault::NewEtagAfterMove,
        Fault::WrongReadbackEtag,
        Fault::SiblingAddedAfterMove,
    ] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        let (old, second) = ("Ab", "Ac");
        std::fs::write(pane.join(old), b"first bytes").unwrap();
        std::fs::write(pane.join(second), b"second bytes").unwrap();
        // Ein bereits aufgelöster eigener Alias wird vor jeder Wirkung abgelehnt.
        // Dort belegt ein gewöhnlicher Kontrollname den gemeinsamen Reststopp;
        // sonst läuft derselbe Gegenfall als tatsächliche Schreibweisenänderung.
        let (new, second_new, options) =
            if spelling_target_is_source_alias(&pane.join(old), &pane.join("ab")) {
                (
                    "new-Ab",
                    "new-Ac",
                    doppeldecker_core::batch_rename::BatchRenameOptions {
                        prefix: "new-".into(),
                        ..webdav_spelling_options("", "")
                    },
                )
            } else {
                (
                    "ab",
                    "ac",
                    doppeldecker_core::batch_rename::BatchRenameOptions {
                        prefix: "a".into(),
                        ..webdav_spelling_options("A", "")
                    },
                )
            };
        server.state.lock().unwrap().changing_collection_tokens = true;
        let plan = PreparedWebDavBatchRename::prepare(
            &server.endpoint(),
            &[old.into(), second.into()],
            &options,
            &Cancel::new(),
        )
        .unwrap();
        server.state.lock().unwrap().fault = fault;
        let report = plan.execute(&Cancel::new(), &mut || Ok(()), &mut |done, _| {
            if done == 1 {
                server.state.lock().unwrap().fault = Fault::None;
            }
        });
        assert_eq!(
            report.rows[0].status,
            BatchRenameStatus::OutcomeUnclear,
            "fault={}: {:?}",
            fault as u32,
            report.rows
        );
        assert_eq!(report.rows[1].status, BatchRenameStatus::FailedUnchanged);
        assert_eq!(std::fs::read(pane.join(second)).unwrap(), b"second bytes");
        assert!(!std::fs::read_dir(&pane)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .any(|name| name == second_new));
        if fault == Fault::NoopMoveAcknowledgement {
            assert_eq!(std::fs::read(pane.join(old)).unwrap(), b"first bytes");
        } else {
            assert_eq!(std::fs::read(pane.join(new)).unwrap(), b"first bytes");
        }
        assert_webdav_spelling_requests(&server, 1);
    }
}
#[test]
fn webdav_stapel_schreibweisen_pruefen_abbruch_quelle_und_eltern_vor_move() {
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::{Cancel, ErrorCode};
    for change in ["cancel", "source", "parent", "profile"] {
        let server = Server::new();
        let pane = server.root.path().join("pane");
        std::fs::write(pane.join("Casefile"), b"source").unwrap();
        server.state.lock().unwrap().changing_collection_tokens = true;
        let plan = PreparedWebDavBatchRename::prepare(
            &server.endpoint(),
            &["Casefile".into()],
            &webdav_spelling_options("Casefile", "casefile"),
            &Cancel::new(),
        )
        .unwrap();
        let cancel = Cancel::new();
        match change {
            "cancel" => cancel.cancel(),
            "source" => std::fs::write(pane.join("Casefile"), b"changed").unwrap(),
            "parent" => std::fs::write(pane.join("foreign"), b"foreign").unwrap(),
            _ => {}
        }
        let before = spelling_host_tree(server.root.path());
        let report = plan.execute(
            &cancel,
            &mut || {
                if change == "profile" {
                    Err(doppeldecker_core::coded_error(
                        std::io::ErrorKind::Interrupted,
                        ErrorCode::PlanDrift,
                        "Profil gewechselt",
                    ))
                } else {
                    Ok(())
                }
            },
            &mut |_, _| {},
        );
        assert_eq!(report.renamed, 0);
        assert_eq!(report.outcome_unclear, 0);
        if change == "cancel" {
            assert_eq!(report.cancelled, 1);
        } else {
            assert_eq!(report.rows[0].code, Some(ErrorCode::PlanDrift));
        }
        assert_eq!(spelling_host_tree(server.root.path()), before);
        assert_webdav_spelling_requests(&server, 0);
    }
}

#[test]
fn webdav_dateinamenzyklus_bilanziert_einzelne_bedingte_moves() {
    use doppeldecker_core::batch_rename::{BatchRenameOptions, BatchRenameStatus};
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::{Cancel, OperationQueue, QueueState};
    for directories in [false, true] {
        for interrupted in [false, true] {
            let server = Server::new();
            let pane = server.root.path().join("pane");
            let names = ["ab", "aab", "aaab"];
            let content = |name: &str| {
                let path = pane.join(name);
                if directories {
                    path.join("inside/leaf")
                } else {
                    path
                }
            };
            for (index, name) in names.iter().enumerate() {
                if directories {
                    std::fs::create_dir_all(pane.join(name).join("inside")).unwrap();
                }
                std::fs::write(content(name), format!("Original{index}")).unwrap();
            }
            server.state.lock().unwrap().changing_collection_tokens = true;
            let config = tempfile::Builder::new()
                .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .tempdir()
                .unwrap();
            let queue = OperationQueue::open(config.path().join("operations-queue.json")).unwrap();
            let options = BatchRenameOptions {
                find: "aaa".into(),
                replacement: String::new(),
                prefix: "a".into(),
                suffix: String::new(),
                preserve_extension: false,
                trim_outer_spaces: false,
                numbering: None,
            };
            let plan = PreparedWebDavBatchRename::prepare(
                &server.endpoint(),
                &names.map(str::to_string),
                &options,
                &Cancel::new(),
            )
            .unwrap();
            assert!(plan
                .rows()
                .iter()
                .all(|row| row.status == BatchRenameStatus::Ready));
            let cancel = Cancel::new();
            let report = plan.execute_with_queue(
                &cancel,
                &mut || {
                    if interrupted
                        && std::fs::read_dir(&pane).unwrap().any(|entry| {
                            entry
                                .unwrap()
                                .file_name()
                                .to_string_lossy()
                                .starts_with(".doppeldecker-rename-")
                        })
                    {
                        cancel.cancel();
                    }
                    Ok(())
                },
                &mut |_, _| {},
                &queue,
            );
            if interrupted {
                assert_eq!(
                    report.partially_renamed, 1,
                    "{:?} {:?}",
                    report.rows, report.limitations
                );
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Interrupted);
                assert_eq!(std::fs::read(content("aab")).unwrap(), b"Original1");
                assert_eq!(std::fs::read(content("aaab")).unwrap(), b"Original2");
            } else {
                assert_eq!(
                    report.renamed, 3,
                    "{:?} {:?}",
                    report.rows, report.limitations
                );
                assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
                for (index, name) in ["aab", "aaab", "ab"].iter().enumerate() {
                    assert_eq!(
                        std::fs::read(content(name)).unwrap(),
                        format!("Original{index}").as_bytes()
                    );
                }
            }
            let state = server.state.lock().unwrap();
            assert_eq!(
                state
                    .requests
                    .iter()
                    .filter(|(method, _)| method == "MOVE")
                    .count(),
                if interrupted { 1 } else { 4 }
            );
            assert!(!state
                .requests
                .iter()
                .any(|(method, _)| matches!(method.as_str(), "PUT" | "DELETE" | "MKCOL")));
        }
    }
}

#[test]
fn webdav_dateinamenzyklus_stoppt_nach_antwortverlust_ohne_zweiten_move() {
    use doppeldecker_core::batch_rename::BatchRenameOptions;
    use doppeldecker_core::webdav_batch_rename::PreparedWebDavBatchRename;
    use doppeldecker_core::{Cancel, OperationQueue, QueueOperation, QueueState};
    let server = Server::new();
    let pane = server.root.path().join("pane");
    for (index, name) in ["ab", "aab", "aaab"].iter().enumerate() {
        std::fs::write(pane.join(name), format!("Original{index}")).unwrap();
    }
    let config = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .unwrap();
    let queue = OperationQueue::open(config.path().join("operations-queue.json")).unwrap();
    let options = BatchRenameOptions {
        find: "aaa".into(),
        replacement: String::new(),
        prefix: "a".into(),
        suffix: String::new(),
        preserve_extension: false,
        trim_outer_spaces: false,
        numbering: None,
    };
    let plan = PreparedWebDavBatchRename::prepare(
        &server.endpoint(),
        &["ab".into(), "aab".into(), "aaab".into()],
        &options,
        &Cancel::new(),
    )
    .unwrap();
    server.state.lock().unwrap().fault = Fault::LostAfterMove;
    let report = plan.execute_with_queue(&Cancel::new(), &mut || Ok(()), &mut |_, _| {}, &queue);
    assert_eq!(report.outcome_unclear, 1, "{:?}", report.rows);
    assert_eq!(report.renamed, 0);
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count(),
        1
    );
    let entry = queue.snapshot().entries.into_iter().next().unwrap();
    assert_eq!(entry.state, QueueState::Interrupted);
    let QueueOperation::RenameCycleRecovery { record } = entry.operation else {
        unreachable!()
    };
    assert!(record.pending.is_some());
    assert!(!record.finished);
    assert_eq!(
        std::fs::read(pane.join(&record.temp_name)).unwrap(),
        b"Original0"
    );
    assert_eq!(std::fs::read(pane.join("aab")).unwrap(), b"Original1");
    assert_eq!(std::fs::read(pane.join("aaab")).unwrap(), b"Original2");
    let path = config.path().join("operations-queue.json");
    drop(queue);
    let queue = OperationQueue::open(path).unwrap();
    assert!(queue.retry(&entry.operation_id).is_err());
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _)| method == "MOVE")
            .count(),
        1
    );
}
