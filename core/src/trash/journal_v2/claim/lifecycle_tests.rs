use super::tests::{hook, Fixture};
use super::*;
use std::os::unix::fs::MetadataExt;

fn journal_path(fixture: &Fixture, run: &ClaimRun) -> PathBuf {
    fixture.root.join("journal").join(&run.journal_name)
}
fn recover_fixture(fixture: &Fixture) -> super::super::super::RecoveryStats {
    super::super::super::recover_trash_journals(&fixture.root.join("journal")).unwrap()
}
fn error_at(point: &'static str) -> super::tests::HookGuard {
    hook(move |name, _| {
        if name == point {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    })
}
fn claimed(fixture: &Fixture) -> ClaimRun {
    let mut run = fixture.prepare();
    run.claim(0).unwrap();
    run
}
fn transferred(fixture: &Fixture) -> ClaimRun {
    let mut run = claimed(fixture);
    assert!(matches!(
        run.transfer(0, None).unwrap(),
        TransferOutcome::Durable
    ));
    run
}

#[test]
fn transfer_verwendet_privaten_claim_und_info_ist_vor_sichtbaren_daten_dauerhaft() {
    let fixture = Fixture::new();
    let inode = std::fs::metadata(&fixture.source).unwrap().ino();
    let mut run = claimed(&fixture);
    let copy = fixture.root.clone();
    let _hook = hook(move |name, _| {
        if name == "before_transfer" {
            assert!(std::fs::symlink_metadata(copy.join("files/file.txt")).is_err());
            if cfg!(target_os = "linux") {
                let info = std::fs::read_to_string(copy.join("info/file.txt.trashinfo"))?;
                assert!(info.starts_with("[Trash Info]\nPath=/"));
                assert!(info.contains("\nDeletionDate="));
            }
        }
        Ok(())
    });
    assert!(matches!(
        run.transfer(0, None).unwrap(),
        TransferOutcome::Durable
    ));
    let area = run.area.as_ref().unwrap().path().to_path_buf();
    let journal = journal_path(&fixture, &run);
    assert_eq!(
        std::fs::metadata(fixture.root.join("files/file.txt"))
            .unwrap()
            .ino(),
        inode
    );
    assert_eq!(fixture.stored(&run).entries[0].phase, EntryPhase::Trashed);
    run.finish().unwrap();
    assert!(!area.exists());
    assert!(!journal.exists());
    assert_eq!(
        std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
        b"original"
    );
    if cfg!(target_os = "linux") {
        assert!(fixture.root.join("info/file.txt.trashinfo").is_file());
    }
}

#[test]
fn vor_claim_beendeter_lauf_ist_unangetastet_und_erfindet_keine_claimidentitaet() {
    let fixture = Fixture::new();
    let run = fixture.prepare();
    let path = journal_path(&fixture, &run);
    drop(run);
    let _hook = hook(|point, _| {
        if point == "before_marker_cleanup" {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    });
    let report = recover_fixture(&fixture);
    assert!(!report.is_clean());
    let journal = decode(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(journal.entries[0].phase, EntryPhase::Untouched);
    assert!(journal.entries[0].claimed.is_none());
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
    drop(_hook);
    let report = recover_fixture(&fixture);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!((report.restored, report.completed), (0, 0));
    assert!(!path.exists());
}

#[test]
fn vor_claim_gedriftete_quelle_schliesst_v2_journal_ohne_wiederholung() {
    for replacement in [false, true] {
        let fixture = Fixture::new();
        let run = fixture.prepare();
        let path = journal_path(&fixture, &run);
        let area = run.area.as_ref().unwrap().path().to_path_buf();
        assert_eq!(fixture.stored(&run).entries[0].phase, EntryPhase::Planned);
        let original = fixture.source.with_extension("held-original");
        std::fs::rename(&fixture.source, &original).unwrap();
        let replacement_inode = if replacement {
            std::fs::write(&fixture.source, b"foreign").unwrap();
            Some(std::fs::metadata(&fixture.source).unwrap().ino())
        } else {
            None
        };
        drop(run);

        let report = recover_fixture(&fixture);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(
            (report.drifted, report.restored, report.completed),
            (1, 0, 0)
        );
        assert!(report.warnings.is_empty());
        assert!(!path.exists());
        assert!(!area.exists());
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        if let Some(inode) = replacement_inode {
            assert_eq!(std::fs::read(&fixture.source).unwrap(), b"foreign");
            assert_eq!(std::fs::metadata(&fixture.source).unwrap().ino(), inode);
        } else {
            assert_eq!(
                std::fs::symlink_metadata(&fixture.source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::NotFound
            );
        }
        assert_eq!(
            std::fs::read_dir(fixture.root.join("files"))
                .unwrap()
                .count(),
            0
        );
        if cfg!(target_os = "linux") {
            assert_eq!(
                std::fs::read_dir(fixture.root.join("info"))
                    .unwrap()
                    .count(),
                0
            );
        }
        let second = recover_fixture(&fixture);
        assert!(second.is_clean(), "{second:?}");
        assert_eq!(
            (second.drifted, second.restored, second.completed),
            (0, 0, 0)
        );
        assert!(second.warnings.is_empty());
    }
}

#[test]
fn recovery_setzt_claimintent_und_claimed_ohne_oeffentliche_zwischenquelle_zurueck() {
    for intent in [false, true] {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        if intent {
            let guard = error_at("after_claim");
            assert!(run.claim(0).is_err());
            drop(guard);
        } else {
            run.claim(0).unwrap();
        }
        let path = journal_path(&fixture, &run);
        drop(run);
        let report = recover_fixture(&fixture);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.restored, 1);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert!(!path.exists());
    }
}

#[test]
fn recovery_nach_restore_rename_bestaetigt_original_aber_ersetzt_keine_kollision() {
    for collision in [false, true] {
        let fixture = Fixture::new();
        let mut run = claimed(&fixture);
        if collision {
            std::fs::write(&fixture.source, b"foreign").unwrap();
            assert!(run.restore(0).is_err());
        } else {
            let guard = error_at("restore_sync");
            assert!(run.restore(0).is_err());
            drop(guard);
        }
        let path = journal_path(&fixture, &run);
        let area = run.area.as_ref().unwrap().path().to_path_buf();
        drop(run);
        let report = recover_fixture(&fixture);
        assert_eq!(report.is_clean(), !collision, "{report:?}");
        assert_eq!(
            std::fs::read(&fixture.source).unwrap(),
            if collision {
                b"foreign".as_slice()
            } else {
                b"original".as_slice()
            }
        );
        if collision {
            assert!(path.is_file());
            assert_eq!(std::fs::read(area.join("entry-0")).unwrap(), b"original");
        }
    }
}

#[test]
fn nicht_terminaler_transfer_bleibt_auch_bei_passendem_ziel_lesend() {
    for point in ["before_transfer", "after_transfer", "transfer_sync"] {
        let fixture = Fixture::new();
        let mut run = claimed(&fixture);
        let guard = error_at(point);
        match run.transfer(0, None) {
            Ok(TransferOutcome::Observed(error)) => {
                assert_eq!(point, "transfer_sync");
                assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            }
            Err(error) => assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear),
            _ => panic!("fehlender Fehler bei {point}"),
        }
        drop(guard);
        let path = journal_path(&fixture, &run);
        let before = std::fs::read(&path).unwrap();
        let area = run.area.as_ref().unwrap().path().to_path_buf();
        drop(run);
        let report = recover_fixture(&fixture);
        assert!(!report.is_clean());
        assert_eq!((report.restored, report.completed), (0, 0));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let data = if point == "before_transfer" {
            area.join("entry-0")
        } else {
            fixture.root.join("files/file.txt")
        };
        assert_eq!(std::fs::read(data).unwrap(), b"original");
        assert!(!fixture.source.exists());
    }
}

#[test]
fn spaete_trash_kollision_und_fremder_readback_werden_nicht_uebernommen() {
    for after in [false, true] {
        let fixture = Fixture::new();
        let mut run = claimed(&fixture);
        let target = fixture.root.join("files/file.txt");
        let foreign = target.clone();
        let guard = hook(move |point, _| {
            if point
                == if after {
                    "after_transfer"
                } else {
                    "before_transfer"
                }
            {
                if after {
                    std::fs::rename(&foreign, foreign.with_extension("held"))?;
                }
                std::fs::write(&foreign, b"foreign")?;
            }
            Ok(())
        });
        assert!(run.transfer(0, None).is_err());
        drop(guard);
        let path = journal_path(&fixture, &run);
        let before = std::fs::read(&path).unwrap();
        drop(run);
        assert!(!recover_fixture(&fixture).is_clean());
        assert_eq!(std::fs::read(&target).unwrap(), b"foreign");
        assert_eq!(std::fs::read(path).unwrap(), before);
        if after {
            assert_eq!(
                std::fs::read(target.with_extension("held")).unwrap(),
                b"original"
            );
        }
    }
}

#[test]
fn terminaler_transfer_bleibt_terminal_wenn_der_benutzer_den_papierkorb_leert() {
    let fixture = Fixture::new();
    let run = transferred(&fixture);
    let path = journal_path(&fixture, &run);
    std::fs::remove_file(fixture.root.join("files/file.txt")).unwrap();
    if cfg!(target_os = "linux") {
        std::fs::remove_file(fixture.root.join("info/file.txt.trashinfo")).unwrap();
    }
    drop(run);
    let report = recover_fixture(&fixture);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.completed, 1);
    assert!(!fixture.source.exists());
    assert!(!path.exists());
    assert_eq!(
        std::fs::read_dir(fixture.root.join("files"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn zusaetzlicher_inhalt_vor_und_waehrend_cleanup_erhaelt_marker_und_journal() {
    for late in [false, true] {
        let fixture = Fixture::new();
        let mut run = transferred(&fixture);
        let area = run.area.as_ref().unwrap().path().to_path_buf();
        let foreign = area.join("foreign");
        let copy = foreign.clone();
        let guard = if late {
            Some(hook(move |point, _| {
                if point == "before_marker_cleanup" {
                    std::fs::write(&copy, b"foreign")?;
                }
                Ok(())
            }))
        } else {
            std::fs::write(&foreign, b"foreign").unwrap();
            None
        };
        assert!(run.finish().is_err());
        drop(guard);
        let path = journal_path(&fixture, &run);
        drop(run);
        assert!(!recover_fixture(&fixture).is_clean());
        assert_eq!(std::fs::read(foreign).unwrap(), b"foreign");
        assert!(area.join(".doppeldecker-trash-marker").is_file());
        assert!(path.is_file());
    }
}

#[test]
fn markerloser_rest_und_unbestaetigtes_rmdir_bleiben_manuell() {
    for point in ["after_marker_cleanup", "area_cleanup_sync"] {
        let fixture = Fixture::new();
        let mut run = transferred(&fixture);
        let guard = error_at(point);
        assert!(run.finish().is_err());
        drop(guard);
        let path = journal_path(&fixture, &run);
        let before = std::fs::read(&path).unwrap();
        drop(run);
        let report = recover_fixture(&fixture);
        assert!(!report.is_clean());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(
            std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
            b"original"
        );
    }
}

#[test]
fn settled_journal_kann_ohne_neue_datenwirkung_abgeschlossen_werden() {
    let fixture = Fixture::new();
    let mut run = transferred(&fixture);
    let guard = error_at("before_journal_cleanup");
    assert!(run.finish().is_err());
    drop(guard);
    let path = journal_path(&fixture, &run);
    assert_eq!(fixture.stored(&run).phase, BatchPhase::Settled);
    drop(run);
    let report = recover_fixture(&fixture);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.completed, 1);
    assert!(!path.exists());
}

#[test]
fn recovery_verweigert_marker_bereich_rechte_und_sandboxnachfolger() {
    for change in ["marker", "area", "permissions", "sandbox"] {
        let fixture = Fixture::new();
        let run = claimed(&fixture);
        let area = run.area.as_ref().unwrap().path().to_path_buf();
        let path = journal_path(&fixture, &run);
        drop(run);
        match change {
            "marker" => {
                std::fs::write(area.join(".doppeldecker-trash-marker"), b"foreign").unwrap()
            }
            "area" => {
                std::fs::rename(&area, area.with_extension("held")).unwrap();
                std::fs::create_dir(&area).unwrap();
            }
            "permissions" => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    fixture.root.join("anchor"),
                    std::fs::Permissions::from_mode(0o777),
                )
                .unwrap();
            }
            "sandbox" => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&fixture.root, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = std::fs::read(&path).unwrap();
        assert!(!recover_fixture(&fixture).is_clean(), "{change}");
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(!fixture.source.exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn xdg_infofehler_vor_und_nach_publish_erhalten_daten_und_fremde_info() {
    for point in [
        "info_file_sync",
        "after_info_publish",
        "info_publish_sync",
        "collision",
    ] {
        let fixture = Fixture::new();
        let mut run = claimed(&fixture);
        let foreign = fixture.root.join("info/file.txt.trashinfo");
        let copy = foreign.clone();
        let guard = hook(move |name, _| {
            if point == "collision" && name == "before_info_publish" {
                std::fs::write(&copy, b"foreign")?;
            }
            if name == point {
                return Err(io::Error::other("injected"));
            }
            Ok(())
        });
        assert!(run.transfer(0, None).is_err());
        drop(guard);
        let path = journal_path(&fixture, &run);
        drop(run);
        let report = recover_fixture(&fixture);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert!(!fixture.root.join("files/file.txt").exists());
        if matches!(point, "after_info_publish" | "info_publish_sync") {
            assert!(report.is_clean(), "{point}: {report:?}");
            assert!(!path.exists());
            assert!(!foreign.exists());
        } else {
            assert!(!report.is_clean());
            assert!(path.is_file());
            if point == "collision" {
                assert_eq!(std::fs::read(foreign).unwrap(), b"foreign");
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn xdg_rohbytes_werden_prozentkodiert_und_mountinfo_bleibt_relativ() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let raw = fixture
        .root
        .join("source")
        .join(OsString::from_vec(vec![255, b'%', b'2', b'F']));
    std::fs::rename(&fixture.source, &raw).unwrap();
    let mut run = ClaimRun::prepare(fixture.layout.clone(), std::slice::from_ref(&raw)).unwrap();
    run.claim(0).unwrap();
    let mount = RootHandle::open_nofollow(Path::new("/")).unwrap();
    assert!(matches!(
        run.transfer(0, Some(&mount)).unwrap(),
        TransferOutcome::Durable
    ));
    let info = fixture.root.join("info").join(OsString::from_vec(
        [vec![255, b'%', b'2', b'F'], b".trashinfo".to_vec()].concat(),
    ));
    let content = std::fs::read_to_string(info).unwrap();
    assert!(!content.contains("Path=/"));
    assert!(content.contains("/%FF%252F\n"));
    run.finish().unwrap();
}

#[test]
fn positiver_transferreadback_bleibt_vom_terminalen_journalcommit_getrennt() {
    for point in ["journal_sync", "journal_parent_sync"] {
        let fixture = Fixture::new();
        let mut run = claimed(&fixture);
        let mut transferred = false;
        let guard = hook(move |name, _| {
            if name == "transfer_sync" {
                transferred = true;
            }
            if transferred && name == point {
                return Err(io::Error::other("injected"));
            }
            Ok(())
        });
        let TransferOutcome::Observed(error) = run.transfer(0, None).unwrap() else {
            panic!("Durabilityfehler fehlt");
        };
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        assert!(crate::error::recovery_paths(&error).contains(
            &fixture
                .root
                .join("files/file.txt")
                .to_string_lossy()
                .into_owned()
        ));
        drop(guard);
        assert!(run.finish().is_err());
        let terminal = fixture.stored(&run).entries[0].phase == EntryPhase::Trashed;
        assert_eq!(terminal, point == "journal_parent_sync");
        drop(run);
        let report = recover_fixture(&fixture);
        assert_eq!(report.is_clean(), terminal, "{point}: {report:?}");
        assert_eq!(report.completed, usize::from(terminal));
        assert_eq!(
            std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
            b"original"
        );
    }
}

#[test]
fn unvollstaendige_anlage_bleibt_bei_recovery_ohne_datenwirkung_erhalten() {
    for point in ["before_mkdir", "after_mkdir", "marker_sync"] {
        let fixture = Fixture::new();
        let guard = error_at(point);
        assert!(ClaimRun::prepare(
            fixture.layout.clone(),
            std::slice::from_ref(&fixture.source)
        )
        .is_err());
        drop(guard);
        let path = std::fs::read_dir(fixture.root.join("journal"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let before = std::fs::read(&path).unwrap();
        let report = recover_fixture(&fixture);
        assert!(!report.is_clean());
        assert_eq!((report.restored, report.completed), (0, 0));
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
    }
}

#[test]
#[ignore = "Benötigt einen ausdrücklich gewählten künstlichen Testmount"]
fn journal_auf_anderem_mount_bleibt_von_den_datenwurzeln_getrennt_erlaubt() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let mount = std::env::var_os("DOPPELDECKER_PRIVATE_CLAIM_TEST_MOUNT")
        .expect("Künstlicher Testmount fehlt");
    let external = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(mount)
        .unwrap();
    let path = external.path().canonicalize().unwrap();
    let mut layout = fixture.layout.clone();
    layout.sandbox = None;
    layout.journal = RootHandle::open_nofollow(&path).unwrap();
    assert_ne!(
        snapshot(&layout.journal).unwrap().mount,
        snapshot(&layout.files).unwrap().mount
    );
    let mut run = ClaimRun::prepare(layout, std::slice::from_ref(&fixture.source)).unwrap();
    run.claim(0).unwrap();
    assert!(matches!(
        run.transfer(0, None).unwrap(),
        TransferOutcome::Durable
    ));
    run.finish().unwrap();
    assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
    assert_eq!(
        std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
        b"original"
    );
}

#[test]
fn legacy_journal_und_quellbindung_bleiben_bei_pfadaustausch_gehalten() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fixture = Fixture::new();
    let source_parent = fixture.root.join("source");
    std::fs::set_permissions(&source_parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let expected = super::super::super::path_identity(&fixture.source).unwrap();
    std::fs::rename(&fixture.source, source_parent.join(".claim")).unwrap();
    let path = fixture.root.join("journal/trash-legacy.json");
    let journal = super::super::super::TrashJournal {
        version: 1,
        batch_id: "legacy".into(),
        entries: vec![super::super::super::JournalEntry {
            original: fixture.source.clone(),
            expected,
            claim_name: ".claim".into(),
            trash_path: fixture.root.join("files/file.txt"),
            info_path: None,
            status: super::super::super::JournalStatus::Claimed,
            failure: None,
        }],
    };
    super::super::super::persist_journal(&path, &journal, true).unwrap();
    let foreign = fixture.root.join("files/trash-legacy.json");
    std::fs::write(&foreign, b"foreign").unwrap();
    let root = fixture.root.clone();
    let guard = hook(move |point, _| {
        if point == "before_legacy_restore" {
            std::fs::rename(root.join("journal"), root.join("journal.held"))?;
            symlink(root.join("files"), root.join("journal"))?;
        }
        Ok(())
    });
    let report = recover_fixture(&fixture);
    drop(guard);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.restored, 1);
    assert_eq!(std::fs::read(foreign).unwrap(), b"foreign");
    assert_eq!(
        std::fs::read_dir(fixture.root.join("journal.held"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(std::fs::read(fixture.source).unwrap(), b"original");
}

#[test]
fn bereichsnachfolger_nach_markerentfernung_wird_nicht_entfernt() {
    let fixture = Fixture::new();
    let mut run = transferred(&fixture);
    let area = run.area.as_ref().unwrap().path().to_path_buf();
    let path = journal_path(&fixture, &run);
    let copy = area.clone();
    let guard = hook(move |point, _| {
        if point == "before_area_cleanup" {
            std::fs::rename(&copy, copy.with_extension("held"))?;
            std::fs::create_dir(&copy)?;
        }
        Ok(())
    });
    assert!(run.finish().is_err());
    drop(guard);
    assert!(area.is_dir());
    assert!(area.with_extension("held").is_dir());
    assert!(path.is_file());
    drop(run);
    assert!(!recover_fixture(&fixture).is_clean());
    assert!(area.is_dir());
}

#[test]
fn settled_namenskollision_erhaelt_journal_und_fremden_bereich() {
    let fixture = Fixture::new();
    let mut run = transferred(&fixture);
    let area = run.area.as_ref().unwrap().path().to_path_buf();
    let path = journal_path(&fixture, &run);
    let copy = area.clone();
    let guard = hook(move |point, _| {
        if point == "before_journal_cleanup" {
            std::fs::create_dir(&copy)?;
        }
        Ok(())
    });
    assert!(run.finish().is_err());
    drop(guard);
    assert_eq!(fixture.stored(&run).phase, BatchPhase::Settled);
    drop(run);
    assert!(!recover_fixture(&fixture).is_clean());
    assert!(path.is_file());
    assert!(area.is_dir());
}

#[test]
fn cleanup_eines_laufs_beruehrt_keinen_anderen_inaktiven_bereich() {
    let fixture = Fixture::new();
    let first = transferred(&fixture);
    let first_area = first.area.as_ref().unwrap().path().to_path_buf();
    let first_journal = journal_path(&fixture, &first);
    let before = std::fs::read(&first_journal).unwrap();
    drop(first);
    std::fs::write(&fixture.source, b"second").unwrap();
    let mut second = claimed(&fixture);
    assert!(matches!(
        second.transfer(0, None).unwrap(),
        TransferOutcome::Durable
    ));
    second.finish().unwrap();
    assert!(first_area.join(".doppeldecker-trash-marker").is_file());
    assert_eq!(std::fs::read(first_journal).unwrap(), before);
    assert_eq!(
        std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
        b"original"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn xdg_fremde_daten_oder_info_sperren_cleanup_auch_nach_restore() {
    for info_swap in [false, true] {
        let fixture = Fixture::new();
        let mut run = claimed(&fixture);
        let guard = error_at("info_publish_sync");
        assert!(run.transfer(0, None).is_err());
        drop(guard);
        let path = journal_path(&fixture, &run);
        let info = fixture.root.join("info/file.txt.trashinfo");
        let copy = info.clone();
        let guard = if info_swap {
            Some(hook(move |point, _| {
                if point == "before_info_cleanup" {
                    std::fs::rename(&copy, copy.with_extension("held"))?;
                    std::fs::write(&copy, b"foreign")?;
                }
                Ok(())
            }))
        } else {
            std::fs::write(fixture.root.join("files/file.txt"), b"foreign").unwrap();
            None
        };
        drop(run);
        let report = recover_fixture(&fixture);
        drop(guard);
        assert!(!report.is_clean());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert!(path.is_file());
        assert!(info.is_file());
        if info_swap {
            assert_eq!(std::fs::read(&info).unwrap(), b"foreign");
            assert!(info.with_extension("held").is_file());
            assert!(report
                .recovery_paths
                .contains(&info.to_string_lossy().into_owned()));
        } else {
            assert_eq!(
                std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
                b"foreign"
            );
        }
    }
}

#[test]
fn legacy_unsicheres_journal_oder_unbekannter_transferhinweis_autorisiert_nichts() {
    use std::os::unix::fs::PermissionsExt;
    for extra in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.root.join("source");
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
        let expected = super::super::super::path_identity(&fixture.source).unwrap();
        std::fs::rename(&fixture.source, source.join(".claim")).unwrap();
        let journal = super::super::super::TrashJournal {
            version: 1,
            batch_id: "legacy".into(),
            entries: vec![super::super::super::JournalEntry {
                original: fixture.source.clone(),
                expected,
                claim_name: ".claim".into(),
                trash_path: fixture.root.join("files/file.txt"),
                info_path: None,
                status: super::super::super::JournalStatus::Claimed,
                failure: None,
            }],
        };
        let path = fixture.root.join("journal/trash-legacy.json");
        super::super::super::persist_journal(&path, &journal, true).unwrap();
        if extra {
            let mut value = serde_json::to_value(&journal).unwrap();
            value["entries"][0]["unknown_transfer_intent"] = true.into();
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        } else {
            std::fs::set_permissions(
                fixture.root.join("journal"),
                std::fs::Permissions::from_mode(0o777),
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert!(!recover_fixture(&fixture).is_clean());
        assert!(!fixture.source.exists());
        assert_eq!(std::fs::read(source.join(".claim")).unwrap(), b"original");
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

#[test]
fn legacy_drift_commit_schreibt_nur_in_den_gehaltenen_journalordner() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let expected = super::super::super::path_identity(&fixture.source).unwrap();
    let journal = super::super::super::TrashJournal {
        version: 1,
        batch_id: "legacy".into(),
        entries: vec![super::super::super::JournalEntry {
            original: fixture.source.clone(),
            expected,
            claim_name: ".claim".into(),
            trash_path: fixture.root.join("files/file.txt"),
            info_path: None,
            status: super::super::super::JournalStatus::Planned,
            failure: None,
        }],
    };
    let path = fixture.root.join("journal/trash-legacy.json");
    super::super::super::persist_journal(&path, &journal, true).unwrap();
    std::fs::remove_file(&fixture.source).unwrap();
    let foreign = fixture.root.join("files/trash-legacy.json");
    std::fs::write(&foreign, b"foreign").unwrap();
    let root = fixture.root.clone();
    let guard = hook(move |point, _| {
        if point == "before_legacy_journal_commit" {
            std::fs::rename(root.join("journal"), root.join("journal.held"))?;
            symlink(root.join("files"), root.join("journal"))?;
        }
        Ok(())
    });
    let report = recover_fixture(&fixture);
    drop(guard);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.drifted, 1);
    assert_eq!(std::fs::read(foreign).unwrap(), b"foreign");
    let saved: super::super::super::TrashJournal = serde_json::from_slice(
        &std::fs::read(fixture.root.join("journal.held/trash-legacy.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved.entries[0].status,
        super::super::super::JournalStatus::Drifted
    );
}

#[test]
fn gemischte_recovery_braucht_keine_terminalen_quelleltern() {
    let mut failures = Vec::new();
    for terminal in [
        EntryPhase::Trashed,
        EntryPhase::Restored,
        EntryPhase::Untouched,
    ] {
        for terminal_first in [false, true] {
            for remove_terminal_parent in [false, true] {
                let fixture = Fixture::new();
                let open_parent = fixture.root.join("source-open");
                std::fs::create_dir(&open_parent).unwrap();
                let open_source = open_parent.join("offen.txt");
                std::fs::write(&open_source, b"offener-claim").unwrap();
                let open_inode = std::fs::metadata(&open_source).unwrap().ino();
                let items = if terminal_first {
                    vec![fixture.source.clone(), open_source.clone()]
                } else {
                    vec![open_source.clone(), fixture.source.clone()]
                };
                let terminal_index = usize::from(!terminal_first);
                let open_index = 1 - terminal_index;
                let mut run = ClaimRun::prepare(fixture.layout.clone(), &items).unwrap();
                match terminal {
                    EntryPhase::Trashed => {
                        run.claim(terminal_index).unwrap();
                        assert!(matches!(
                            run.transfer(terminal_index, None).unwrap(),
                            TransferOutcome::Durable
                        ));
                    }
                    EntryPhase::Restored => {
                        run.claim(terminal_index).unwrap();
                        run.restore(terminal_index).unwrap();
                    }
                    EntryPhase::Untouched => {
                        let mut journal = run.journal.clone();
                        journal.entries[terminal_index].phase = terminal;
                        run.persist(&journal).unwrap();
                        run.journal = journal;
                    }
                    _ => unreachable!(),
                }
                run.claim(open_index).unwrap();
                let journal = journal_path(&fixture, &run);
                let area = run.area.as_ref().unwrap().path().to_path_buf();
                drop(run);
                if remove_terminal_parent {
                    if terminal != EntryPhase::Trashed {
                        std::fs::remove_file(&fixture.source).unwrap();
                    }
                    std::fs::remove_dir(fixture.source.parent().unwrap()).unwrap();
                }
                let report = recover_fixture(&fixture);
                if !report.is_clean() {
                    failures.push((terminal, terminal_first, remove_terminal_parent, report));
                    continue;
                }
                assert_eq!(
                    report.completed,
                    usize::from(terminal == EntryPhase::Trashed)
                );
                assert_eq!(
                    report.restored,
                    1 + usize::from(terminal == EntryPhase::Restored)
                );
                assert_eq!(std::fs::read(&open_source).unwrap(), b"offener-claim");
                assert_eq!(std::fs::metadata(&open_source).unwrap().ino(), open_inode);
                assert!(!journal.exists());
                assert!(!area.exists());
                if terminal == EntryPhase::Trashed {
                    assert_eq!(
                        std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
                        b"original"
                    );
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "Unabhängige offene Claims blockiert: {failures:?}"
    );
}

#[test]
fn gemischte_recovery_verweigert_ungebundene_offene_quelleltern() {
    for replacement in ["missing", "directory", "symlink"] {
        let fixture = Fixture::new();
        let open_parent = fixture.root.join("source-open");
        std::fs::create_dir(&open_parent).unwrap();
        let open_source = open_parent.join("offen.txt");
        std::fs::write(&open_source, b"offener-claim").unwrap();
        let mut run = ClaimRun::prepare(
            fixture.layout.clone(),
            &[fixture.source.clone(), open_source.clone()],
        )
        .unwrap();
        run.claim(0).unwrap();
        assert!(matches!(
            run.transfer(0, None).unwrap(),
            TransferOutcome::Durable
        ));
        run.claim(1).unwrap();
        let journal = journal_path(&fixture, &run);
        let before = std::fs::read(&journal).unwrap();
        let claim = run.area.as_ref().unwrap().path().join("entry-1");
        drop(run);
        std::fs::remove_dir(fixture.source.parent().unwrap()).unwrap();
        std::fs::rename(&open_parent, fixture.root.join("source-open-held")).unwrap();
        let foreign_parent = fixture.root.join("foreign");
        std::fs::create_dir(&foreign_parent).unwrap();
        let foreign_source = foreign_parent.join("offen.txt");
        std::fs::write(&foreign_source, b"fremd").unwrap();
        match replacement {
            "missing" => {}
            "directory" => std::fs::create_dir(&open_parent).unwrap(),
            "symlink" => std::os::unix::fs::symlink(&foreign_parent, &open_parent).unwrap(),
            _ => unreachable!(),
        }
        let report = recover_fixture(&fixture);
        assert!(!report.is_clean(), "{replacement}: {report:?}");
        assert_eq!(report.restored, 0);
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        assert_eq!(std::fs::read(&claim).unwrap(), b"offener-claim");
        assert_eq!(std::fs::read(&foreign_source).unwrap(), b"fremd");
        assert_eq!(
            std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
            b"original"
        );
    }
}
