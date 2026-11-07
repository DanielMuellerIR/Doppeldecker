use super::*;
fn fixture(items: &[(&[u8], &[u8], u32)]) -> Vec<u8> {
    fn s16(v: &mut Vec<u8>, n: u16) {
        v.extend(n.to_le_bytes());
    }
    fn s32(v: &mut Vec<u8>, n: u32) {
        v.extend(n.to_le_bytes());
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, mode) in items {
        let offset = out.len() as u32;
        let mut crc = rawzip::Crc32::new();
        crc.update(data);
        let crc = crc.checksum();
        s32(&mut out, 0x04034b50);
        for n in [20, 0, 0, 0, 0] {
            s16(&mut out, n);
        }
        for n in [crc, data.len() as u32, data.len() as u32] {
            s32(&mut out, n);
        }
        s16(&mut out, name.len() as u16);
        s16(&mut out, 0);
        out.extend(*name);
        out.extend(*data);
        s32(&mut central, 0x02014b50);
        for n in [(3 << 8) | 20, 20, 0, 0, 0, 0] {
            s16(&mut central, n);
        }
        for n in [crc, data.len() as u32, data.len() as u32] {
            s32(&mut central, n);
        }
        for n in [name.len() as u16, 0, 0, 0, 0] {
            s16(&mut central, n);
        }
        s32(&mut central, mode << 16);
        s32(&mut central, offset);
        central.extend(*name);
    }
    let offset = out.len() as u32;
    let length = central.len() as u32;
    out.extend(central);
    s32(&mut out, 0x06054b50);
    for n in [0, 0, items.len() as u16, items.len() as u16] {
        s16(&mut out, n);
    }
    s32(&mut out, length);
    s32(&mut out, offset);
    s16(&mut out, 0);
    out
}
fn request(bytes: &[u8]) -> (tempfile::TempDir, ArchiveExtractRequest) {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::create_dir(root.join("dst")).unwrap();
    std::fs::write(root.join("src/a.zip"), bytes).unwrap();
    let request = ArchiveExtractRequest {
        snapshot: None,
        source_root: root.join("src"),
        source_path: root.join("src/a.zip"),
        target_dir: root.join("dst"),
        options: ArchiveExtractOptions::default(),
        selection: crate::archive_extract::ArchiveExtractSelection::All,
        sandbox: Some(root),
    };
    (temp, request)
}
#[test]
fn crc_fehler_laesst_bestaetigtes_altziel_unveraendert() {
    let mut bytes = fixture(&[(b"file", b"new", 0o100644)]);
    bytes[34] ^= 1;
    let (_temp, mut request) = request(&bytes);
    request.options.overwrite = true;
    std::fs::write(request.target_dir.join("file"), b"old").unwrap();
    let prepared = request.prepare(&Cancel::new()).unwrap();
    let failure = prepared.execute(None, &mut |_| {}).unwrap_err();
    assert_eq!(error_code(&failure.error), ErrorCode::InvalidData);
    assert_eq!(
        std::fs::read(request.target_dir.join("file")).unwrap(),
        b"old"
    );
    assert!(!failure.report.has_effect);
}
#[test]
fn abbruch_im_stream_entfernt_temp_und_erhaelt_altziel() {
    let data = vec![7; 50000];
    let (_temp, mut request) = request(&fixture(&[(b"file", &data, 0o100644)]));
    request.options.overwrite = true;
    std::fs::write(request.target_dir.join("file"), b"old").unwrap();
    let cancel = Cancel::new();
    let prepared = request.prepare(&cancel).unwrap();
    let report = prepared
        .execute(None, &mut |p| {
            if matches!(p.phase, ArchiveExtractPhase::Extracting) {
                cancel.cancel();
            }
        })
        .unwrap();
    assert_eq!(report.status, ArchiveExtractStatus::Cancelled);
    assert_eq!(
        std::fs::read(request.target_dir.join("file")).unwrap(),
        b"old"
    );
    assert!(!report.has_effect);
}
#[test]
fn spaete_kollision_wird_nicht_still_ersetzt() {
    let (_temp, request) = request(&fixture(&[(b"file", b"new", 0o100644)]));
    let prepared = request.prepare(&Cancel::new()).unwrap();
    let mut changed = false;
    let failure = prepared
        .execute(None, &mut |p| {
            if !changed && matches!(p.phase, ArchiveExtractPhase::Committing) {
                std::fs::write(request.target_dir.join("file"), b"late").unwrap();
                changed = true;
            }
        })
        .unwrap_err();
    assert_eq!(
        std::fs::read(request.target_dir.join("file")).unwrap(),
        b"late"
    );
    assert!(!failure.report.has_effect);
}
#[test]
fn quellentausch_nach_readback_verhindert_commit_und_altzielverlust() {
    let (_temp, mut request) = request(&fixture(&[(b"file", b"new", 0o100644)]));
    request.options.overwrite = true;
    std::fs::write(request.target_dir.join("file"), b"old").unwrap();
    let prepared = request.prepare(&Cancel::new()).unwrap();
    let mut changed = false;
    let failure = prepared
        .execute(None, &mut |p| {
            if !changed && matches!(p.phase, ArchiveExtractPhase::Committing) {
                std::fs::write(&request.source_path, b"changed").unwrap();
                changed = true;
            }
        })
        .unwrap_err();
    assert_eq!(error_code(&failure.error), ErrorCode::PlanDrift);
    assert_eq!(
        std::fs::read(request.target_dir.join("file")).unwrap(),
        b"old"
    );
    assert!(!failure.report.has_effect);
}
#[test]
fn grosse_datei_leerordner_kuenstliche_eltern_und_optionen_sind_nutzbar() {
    let large = vec![9; 9 * 1024 * 1024];
    let (_temp, request) = request(&fixture(&[
        (b"large", &large, 0o100644),
        (b"empty/", b"", 0o040755),
        (b"a/b/file", b"nested", 0o100644),
    ]));
    let prepared = request.prepare(&Cancel::new()).unwrap();
    let prepared = prepared
        .with_options(
            ArchiveExtractOptions {
                overwrite: true,
                allow_partial: false,
            },
            &Cancel::new(),
        )
        .unwrap();
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 2);
    assert_eq!(report.bytes, large.len() as u64 + 6);
    assert_eq!(report.directories, 3);
    assert!(request.target_dir.join("empty").is_dir());
    assert_eq!(
        std::fs::read(request.target_dir.join("large")).unwrap(),
        large
    );
    assert_eq!(
        std::fs::read(request.target_dir.join("a/b/file")).unwrap(),
        b"nested"
    );
}
#[test]
fn namensgruppen_und_typkonflikte_lassen_unabhaengigen_inhalt_zu() {
    let (_temp, request) = request(&fixture(&[
        (b"dup", b"one", 0o100644),
        (b"dup", b"two", 0o100644),
        (b"A/file", b"a", 0o100644),
        (b"a/other", b"a", 0o100644),
        ("e\u{301}".as_bytes(), b"nfd", 0o100644),
        ("é".as_bytes(), b"nfc", 0o100644),
        (b"same", b"file", 0o100644),
        (b"same/child", b"child", 0o100644),
        (b"type/file", b"child", 0o100644),
        (b"good.txt", b"good", 0o100644),
    ]));
    std::fs::write(request.target_dir.join("type"), b"existing file").unwrap();
    let prepared = request.prepare(&Cancel::new()).unwrap();
    assert_eq!(prepared.preview().omitted_entries, 9);
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(report.skipped, 9);
    assert_eq!(
        std::fs::read(request.target_dir.join("good.txt")).unwrap(),
        b"good"
    );
    assert_eq!(
        std::fs::read(request.target_dir.join("type")).unwrap(),
        b"existing file"
    );
    assert!(!request.target_dir.join("same").exists());
}
#[test]
fn quelle_im_ziel_selbst_hardlink_und_eltern_bleiben_erhalten() {
    let (_temp, mut request) = request(&fixture(&[
        (b"a.zip", b"overwrite", 0o100644),
        (b"alias.zip", b"overwrite", 0o100644),
        (b"good", b"good", 0o100644),
    ]));
    request.target_dir = request.source_root.clone();
    std::fs::hard_link(&request.source_path, request.target_dir.join("alias.zip")).unwrap();
    let before = std::fs::read(&request.source_path).unwrap();
    let prepared = request.prepare(&Cancel::new()).unwrap();
    assert_eq!(prepared.preview().omitted_entries, 2);
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(std::fs::read(&request.source_path).unwrap(), before);
    assert_eq!(
        std::fs::read(request.target_dir.join("alias.zip")).unwrap(),
        before
    );
}
#[test]
fn normale_dateikonflikte_skip_replace_und_drift_nach_optionenwechsel() {
    let (_temp, request) = request(&fixture(&[(b"file", b"new", 0o100644)]));
    std::fs::write(request.target_dir.join("file"), b"old").unwrap();
    let prepared = request.prepare(&Cancel::new()).unwrap();
    assert_eq!(prepared.preview().conflicts.len(), 1);
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.skipped, 1);
    assert!(!report.has_effect);
    assert_eq!(
        std::fs::read(request.target_dir.join("file")).unwrap(),
        b"old"
    );
    let prepared = request
        .prepare(&Cancel::new())
        .unwrap()
        .with_options(
            ArchiveExtractOptions {
                overwrite: true,
                allow_partial: false,
            },
            &Cancel::new(),
        )
        .unwrap();
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(
        std::fs::read(request.target_dir.join("file")).unwrap(),
        b"new"
    );
    let prepared = request.prepare(&Cancel::new()).unwrap();
    std::fs::write(request.target_dir.join("file"), b"changed").unwrap();
    assert!(prepared
        .with_options(
            ArchiveExtractOptions {
                overwrite: true,
                allow_partial: false
            },
            &Cancel::new()
        )
        .is_err());
}
#[test]
fn teilkatalog_verlangt_bekannten_umfang_und_kein_stilles_vollstaendig() {
    let mut bytes = fixture(&[(b"file", b"known", 0o100644)]);
    let end = bytes.len() - 22;
    bytes[end + 8..end + 10].copy_from_slice(&2u16.to_le_bytes());
    bytes[end + 10..end + 12].copy_from_slice(&2u16.to_le_bytes());
    let (_temp, request) = request(&bytes);
    let prepared = request.prepare(&Cancel::new()).unwrap();
    assert!(!prepared.preview().catalog_complete);
    assert_eq!(prepared.preview().known_entries, 1);
    assert!(prepared.execute(None, &mut |_| {}).is_err());
    assert!(!request.target_dir.join("file").exists());
    let prepared = request
        .prepare(&Cancel::new())
        .unwrap()
        .with_options(
            ArchiveExtractOptions {
                overwrite: false,
                allow_partial: true,
            },
            &Cancel::new(),
        )
        .unwrap();
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert!(!report.preview.catalog_complete);
    assert_eq!(report.files, 1);
}
#[test]
fn problematische_extrem_tiefe_namen_erzeugen_keinen_praefix_index() {
    let bad = "a/".repeat(32000);
    let (_temp, request) = request(&fixture(&[
        (bad.as_bytes(), b"bad", 0o100644),
        (b"good", b"good", 0o100644),
    ]));
    let prepared = request.prepare(&Cancel::new()).unwrap();
    assert_eq!(prepared.preview().omitted_entries, 1);
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(
        std::fs::read(request.target_dir.join("good")).unwrap(),
        b"good"
    );
}
#[test]
fn ziel_eltern_tausch_vor_start_und_im_stream_schreibt_nicht_in_fremden_ordner() {
    for during_stream in [false, true] {
        let (_temp, request) = request(&fixture(&[(b"sub/file", &vec![7; 50000], 0o100644)]));
        std::fs::create_dir(request.target_dir.join("sub")).unwrap();
        let prepared = request.prepare(&Cancel::new()).unwrap();
        let mut swapped = false;
        let swap = || {
            std::fs::rename(
                request.target_dir.join("sub"),
                request.target_dir.join("old-sub"),
            )
            .unwrap();
            std::fs::create_dir(request.target_dir.join("sub")).unwrap();
        };
        if !during_stream {
            swap();
        }
        let failure = prepared
            .execute(None, &mut |p| {
                if during_stream && !swapped && matches!(p.phase, ArchiveExtractPhase::Extracting) {
                    swap();
                    swapped = true;
                }
            })
            .unwrap_err();
        assert!(
            matches!(
                error_code(&failure.error),
                ErrorCode::PlanDrift | ErrorCode::OutcomeUnclear
            ),
            "during_stream={during_stream}, {failure:?}"
        );
        assert!(!request.target_dir.join("sub/file").exists());
        assert!(!request.target_dir.join("old-sub/file").exists());
        assert_eq!(failure.report.files, 0);
    }
}
#[test]
fn zip64_groessensumme_laeuft_nicht_ueber_und_aendert_keine_ziele() {
    fn n16(v: &mut Vec<u8>, n: u16) {
        v.extend(n.to_le_bytes());
    }
    fn n32(v: &mut Vec<u8>, n: u32) {
        v.extend(n.to_le_bytes());
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for name in [b'a', b'b'] {
        let offset = out.len() as u32;
        let mut extra = Vec::new();
        n16(&mut extra, 1);
        n16(&mut extra, 16);
        extra.extend(u64::MAX.to_le_bytes());
        extra.extend(0u64.to_le_bytes());
        n32(&mut out, 0x04034b50);
        for n in [45, 0, 0, 0, 0] {
            n16(&mut out, n);
        }
        for n in [0, u32::MAX, u32::MAX] {
            n32(&mut out, n);
        }
        n16(&mut out, 1);
        n16(&mut out, 20);
        out.push(name);
        out.extend(&extra);
        n32(&mut central, 0x02014b50);
        for n in [(3 << 8) | 45, 45, 0, 0, 0, 0] {
            n16(&mut central, n);
        }
        for n in [0, u32::MAX, u32::MAX] {
            n32(&mut central, n);
        }
        for n in [1, 20, 0, 0, 0] {
            n16(&mut central, n);
        }
        n32(&mut central, 0o100644 << 16);
        n32(&mut central, offset);
        central.push(name);
        central.extend(extra);
    }
    let offset = out.len() as u32;
    let len = central.len() as u32;
    out.extend(central);
    n32(&mut out, 0x06054b50);
    for n in [0, 0, 2, 2] {
        n16(&mut out, n);
    }
    n32(&mut out, len);
    n32(&mut out, offset);
    n16(&mut out, 0);
    let (_temp, request) = request(&out);
    let error = request.prepare(&Cancel::new()).err().unwrap();
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    assert!(std::fs::read_dir(&request.target_dir)
        .unwrap()
        .next()
        .is_none());
}
#[test]
fn fremder_tempname_ohne_privates_staging_bleibt_beim_abbruch_erhalten() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, request) = request(&fixture(&[(b"file", &vec![7; 50000], 0o100644)]));
    std::fs::set_permissions(&request.target_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cancel = Cancel::new();
    let prepared = request.prepare(&cancel).unwrap();
    let mut foreign = None;
    let report = prepared
        .execute(None, &mut |p| {
            if foreign.is_none() && matches!(p.phase, ArchiveExtractPhase::Extracting) {
                let temp = std::fs::read_dir(&request.target_dir)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .contains("fileop-tmp")
                    })
                    .unwrap();
                std::fs::rename(&temp, temp.with_extension("moved-own")).unwrap();
                std::fs::write(&temp, b"foreign").unwrap();
                foreign = Some(temp);
                cancel.cancel();
            }
        })
        .unwrap();
    let foreign = foreign.unwrap();
    assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
    assert_eq!(report.status, ArchiveExtractStatus::Cancelled);
    assert!(report
        .recovery_paths
        .iter()
        .any(|p| Path::new(p) == foreign));
    assert!(!request.target_dir.join("file").exists());
    assert_eq!(report.bytes, 0);
}

fn crc_queue_target_namespace(
    mode: u32,
) -> (
    tempfile::TempDir,
    ArchiveExtractRequest,
    crate::queue::OperationQueue,
) {
    use std::os::unix::fs::PermissionsExt;
    let mut bytes = fixture(&[(b"bad", b"content", 0o100644)]);
    bytes[33] ^= 1;
    let (temp, request) = request(&bytes);
    std::fs::set_permissions(&request.target_dir, std::fs::Permissions::from_mode(mode)).unwrap();
    std::fs::write(request.target_dir.join("schutz"), b"unveraendert").unwrap();
    let state = request.target_dir.parent().unwrap().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let queue = crate::queue::OperationQueue::open(state.join("queue.json")).unwrap();
    let expected = request.prepare(&Cancel::new()).unwrap().binding().unwrap();
    queue
        .enqueue(crate::queue::QueueOperation::ArchiveExtract {
            request: Box::new(request.clone()),
            expected: Box::new(expected),
        })
        .unwrap();
    let entry = queue.claim_next().unwrap().unwrap();
    let execution = crate::queue::LocalQueueExecution {
        sandbox: request.sandbox.clone(),
        sync_quarantine: None,
        max_delete_entries: 0,
        max_delete_fraction: 0.0,
    };
    let error = crate::queue::execute_local_queue_entry_detailed(
        &queue,
        entry,
        &execution,
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(error_code(&error.error), ErrorCode::InvalidData);
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.entries[0].state, crate::queue::QueueState::Failed);
    assert_eq!(
        snapshot.entries[0]
            .archive_extract_report
            .as_ref()
            .unwrap()
            .status,
        ArchiveExtractStatus::Failed
    );
    assert_eq!(
        snapshot.entries[0]
            .archive_extract_report
            .as_ref()
            .unwrap()
            .files,
        0
    );
    assert!(!request.target_dir.join("bad").exists());
    assert_eq!(
        std::fs::read(request.target_dir.join("schutz")).unwrap(),
        b"unveraendert"
    );
    (temp, request, queue)
}

#[test]
fn crc_im_privaten_0700_ziel_bereinigt_eigenen_temp_und_recoveryindex() {
    let (temp, request, queue) = crc_queue_target_namespace(0o700);
    let paths: Vec<_> = std::fs::read_dir(&request.target_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(paths, vec![std::ffi::OsString::from("schutz")]);
    assert!(queue.snapshot().entries[0].temp_recovery_paths.is_empty());
    assert!(queue.snapshot().entries[0]
        .archive_extract_report
        .as_ref()
        .unwrap()
        .recovery_paths
        .is_empty());
    assert!(std::fs::read_dir(temp.path().join("state"))
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".recovery.json")));
}

#[test]
fn crc_im_0755_ziel_erhaelt_unbelegten_nachbartemp_mit_queue_recovery() {
    let (temp, request, queue) = crc_queue_target_namespace(0o755);
    let paths: Vec<_> = std::fs::read_dir(&request.target_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let retained: Vec<_> = paths
        .iter()
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("fileop-tmp")
        })
        .collect();
    assert_eq!(retained.len(), 1);
    assert_eq!(std::fs::metadata(retained[0]).unwrap().len(), 7);
    let indexes: Vec<_> = std::fs::read_dir(temp.path().join("state"))
        .unwrap()
        .map(|entry| std::fs::canonicalize(entry.unwrap().path()).unwrap())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".recovery.json")
        })
        .collect();
    assert_eq!(indexes.len(), 1);
    let snapshot = queue.snapshot();
    let report = snapshot.entries[0].archive_extract_report.as_ref().unwrap();
    for path in [retained[0], &indexes[0]] {
        assert!(report
            .recovery_paths
            .iter()
            .any(|record| Path::new(record) == path));
    }
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&indexes[0]).unwrap()).unwrap();
    assert_eq!(index["candidate"].as_str(), retained[0].to_str());
    assert_eq!(
        index["owner"]["operation"].as_str(),
        Some(snapshot.entries[0].operation_id.as_str())
    );
}

#[test]
fn auswahl_datei_und_unterbaum_bilden_nur_ihre_relative_zielwurzel_ab() {
    let bytes = fixture(&[
        (b"outer/sub/file", b"yes", 0o100644),
        (b"outer/sub/empty/", b"", 0o040755),
        (b"other", b"no", 0o100644),
    ]);
    for subtree in [false, true] {
        let (_temp, mut request) = request(&bytes);
        request.selection = if subtree {
            ArchiveExtractSelection::Subtree {
                prefix: "outer/sub".into(),
            }
        } else {
            ArchiveExtractSelection::Entry {
                ordinal: 0,
                raw_name: b"outer/sub/file".to_vec(),
            }
        };
        let prepared = request.prepare(&Cancel::new()).unwrap();
        assert_eq!(
            prepared.preview().relative_root,
            if subtree { "sub" } else { "file" }
        );
        assert_eq!(
            prepared.preview().selection_path,
            if subtree {
                "outer/sub"
            } else {
                "outer/sub/file"
            }
        );
        prepared.execute(None, &mut |_| {}).unwrap();
        assert_eq!(
            std::fs::read(
                request
                    .target_dir
                    .join(if subtree { "sub/file" } else { "file" })
            )
            .unwrap(),
            b"yes"
        );
        assert!(!request.target_dir.join("outer").exists());
        assert!(!request.target_dir.join("other").exists());
        assert_eq!(request.target_dir.join("sub/empty").is_dir(), subtree);
    }
}

#[test]
fn auswahl_macht_keinen_gewinner_aus_globalen_namensgruppen() {
    for (left, right, selected) in [
        ("outer/sub/file", "outer/sub/file", "outer/sub/file"),
        ("outer/sub/file", "outer/Sub/other", "outer/sub/file"),
        ("outer/é/file", "outer/e\u{301}/other", "outer/é/file"),
        ("outer/sub", "outer/sub/file", "outer/sub/file"),
    ] {
        let (_temp, mut request) = request(&fixture(&[
            (left.as_bytes(), b"one", 0o100644),
            (right.as_bytes(), b"two", 0o100644),
        ]));
        let ordinal = usize::from(selected == right);
        request.selection = ArchiveExtractSelection::Entry {
            ordinal,
            raw_name: selected.as_bytes().to_vec(),
        };
        let prepared = request.prepare(&Cancel::new()).unwrap();
        assert_eq!(prepared.preview().files, 0);
        assert!(prepared.preview().omitted_entries > 0);
        prepared.execute(None, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read_dir(&request.target_dir).unwrap().count(), 0);
    }
}

#[test]
fn auswahl_ordinaldrift_waehlt_nie_einen_anderen_eintrag() {
    let (_temp, mut request) = request(&fixture(&[(b"outer/file", b"old", 0o100644)]));
    request.selection = ArchiveExtractSelection::Entry {
        ordinal: 0,
        raw_name: b"outer/file".to_vec(),
    };
    let prepared = request.prepare(&Cancel::new()).unwrap();
    let original = prepared.source_digest;
    std::fs::write(
        &request.source_path,
        fixture(&[
            (b"other", b"no", 0o100644),
            (b"outer/file", b"new", 0o100644),
        ]),
    )
    .unwrap();
    assert_eq!(
        error_code(&request.prepare(&Cancel::new()).err().unwrap()),
        ErrorCode::PlanDrift
    );
    assert!(prepared.verify_selection_source(original).is_err());
    assert_eq!(std::fs::read_dir(&request.target_dir).unwrap().count(), 0);
}

#[test]
fn auswahl_binding_scope_optionen_und_browserquellversion_bleiben_getrennt_gebunden() {
    let (_temp, mut request) = request(&fixture(&[
        (b"left/file", b"same", 0o100644),
        (b"right/file", b"same", 0o100644),
    ]));
    let cancel = Cancel::new();
    let reader = ArchiveRead::open(
        &request.source_root,
        Path::new("a.zip"),
        CatalogBudget::default(),
        &cancel,
    )
    .unwrap();
    let source_digest = reader.source_binding_digest(&cancel).unwrap();
    request.selection = reader
        .extract_entry_selection(reader.entries()[0].id, &cancel)
        .unwrap();
    let (root, path) = reader.source_paths();
    assert_eq!(root, request.source_root);
    assert_eq!(path, request.source_path);
    drop(reader);
    let left = request.prepare(&cancel).unwrap();
    left.verify_selection_source(source_digest).unwrap();
    assert_eq!(left.preview().selection_path, "left/file");
    let left_digest = left.binding().unwrap().digest;
    let changed_options = left
        .with_options(
            ArchiveExtractOptions {
                overwrite: true,
                allow_partial: false,
            },
            &cancel,
        )
        .unwrap();
    assert_ne!(changed_options.binding().unwrap().digest, left_digest);
    assert_eq!(changed_options.preview().relative_root, "file");
    assert_eq!(changed_options.preview().selection_path, "left/file");
    request.selection = ArchiveExtractSelection::Entry {
        ordinal: 1,
        raw_name: b"right/file".to_vec(),
    };
    let right = request.prepare(&cancel).unwrap();
    assert_ne!(right.binding().unwrap().digest, left_digest);
    assert_eq!(right.preview().relative_root, "file");
    assert_eq!(right.preview().selection_path, "right/file");
    std::fs::write(
        &request.source_path,
        fixture(&[
            (b"left/file", b"same", 0o100644),
            (b"right/file", b"new!", 0o100644),
        ]),
    )
    .unwrap();
    let fresh = request.prepare(&cancel).unwrap();
    assert_eq!(
        error_code(&fresh.verify_selection_source(source_digest).unwrap_err()),
        ErrorCode::PlanDrift
    );
}

#[test]
fn auswahl_ids_ordnertyp_root_und_unsichere_praefixe_werden_nicht_umgedeutet() {
    let (_temp, request) = request(&fixture(&[
        (b"outer/file", b"data", 0o100644),
        (b"outer/empty/", b"", 0o040755),
    ]));
    let cancel = Cancel::new();
    let first = ArchiveRead::open(
        &request.source_root,
        Path::new("a.zip"),
        CatalogBudget::default(),
        &cancel,
    )
    .unwrap();
    let second = ArchiveRead::open(
        &request.source_root,
        Path::new("a.zip"),
        CatalogBudget::default(),
        &cancel,
    )
    .unwrap();
    assert!(first
        .extract_entry_selection(second.entries()[0].id, &cancel)
        .is_err());
    assert!(first
        .extract_entry_selection(first.entries()[1].id, &cancel)
        .is_err());
    for prefix in [
        "",
        "/",
        "outer/../outer",
        "outer/file",
        "outer/empty/",
        "missing",
    ] {
        assert!(
            first.extract_subtree_selection(prefix, &cancel).is_err(),
            "{prefix}"
        );
    }
    let mut selected = request.clone();
    selected.selection = first
        .extract_subtree_selection("outer/empty", &cancel)
        .unwrap();
    let prepared = selected.prepare(&cancel).unwrap();
    assert_eq!(
        (prepared.preview().files, prepared.preview().directories),
        (0, 1)
    );
    prepared.execute(None, &mut |_| {}).unwrap();
    assert!(request.target_dir.join("empty").is_dir());
    assert!(!request.target_dir.join("outer").exists());
}

#[test]
fn auswahl_unterbaum_aus_teilkatalog_bleibt_explizit_unvollstaendig() {
    let mut names = vec!["outer/scope/file".to_owned()];
    names.extend((0..20_000).map(|index| format!("outside{index}")));
    let items = names
        .iter()
        .map(|name| (name.as_bytes(), b"data".as_slice(), 0o100644))
        .collect::<Vec<_>>();
    let (_temp, mut request) = request(&fixture(&items));
    request.selection = ArchiveExtractSelection::Subtree {
        prefix: "outer/scope".into(),
    };
    let prepared = request.prepare(&Cancel::new()).unwrap();
    assert!(!prepared.preview().catalog_complete);
    assert_eq!(
        (prepared.preview().known_entries, prepared.preview().files),
        (1, 1)
    );
    let failure = prepared.execute(None, &mut |_| {}).unwrap_err();
    assert!(!failure.report.has_effect);
    assert_eq!(std::fs::read_dir(&request.target_dir).unwrap().count(), 0);
    request.options.allow_partial = true;
    let prepared = request.prepare(&Cancel::new()).unwrap();
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert!(!report.preview.catalog_complete);
    assert_eq!(
        std::fs::read(request.target_dir.join("scope/file")).unwrap(),
        b"data"
    );
    assert_eq!(std::fs::read_dir(&request.target_dir).unwrap().count(), 1);
}

fn snapshot_request(
    bytes: &[u8],
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    ArchiveExtractRequest,
    crate::remote_snapshot::ValidatedSnapshot,
) {
    let (temp, mut request) = request(bytes);
    let cache = tempfile::tempdir().unwrap();
    let downloaded = crate::remote_snapshot::download_remote_snapshot(
        &LocalVfs::new(),
        &request.source_path,
        cache.path(),
        &Cancel::new(),
    )
    .unwrap();
    let store =
        crate::remote_snapshot::SnapshotStore::open(&cache.path().canonicalize().unwrap()).unwrap();
    let validated = store.capture(&downloaded, &Cancel::new()).unwrap();
    request.source_root = PathBuf::new();
    request.source_path = PathBuf::new();
    request.snapshot = Some(ArchiveSnapshotSource {
        reference: validated.reference().clone(),
        origin: "SFTP · /artificial/a.zip".into(),
        profile: crate::RemoteProfileBinding::new(
            crate::RemoteProfileProtocol::Sftp,
            "artificial",
            1,
        )
        .unwrap(),
    });
    let reader =
        ArchiveRead::open_snapshot(validated.clone(), CatalogBudget::default(), &Cancel::new())
            .unwrap();
    request.selection = reader
        .extract_entry_selection(reader.entries()[0].id, &Cancel::new())
        .unwrap();
    (temp, cache, request, validated)
}
#[test]
fn snapshot_ausserhalb_quellsandbox_braucht_gehaltenen_beleg_und_behaelt_zielgrenze() {
    let (_temp, _cache, request, validated) =
        snapshot_request(&fixture(&[(b"A/file", b"abc", 0o100644)]));
    assert!(request.prepare(&Cancel::new()).is_err());
    let prepared = request
        .prepare_with_snapshot(validated.clone(), &Cancel::new())
        .unwrap();
    let bound = prepared.binding().unwrap();
    assert!(bound.preview.source_path.as_os_str().is_empty());
    assert_eq!(bound.preview.files, 1);
    let prepared = prepared
        .with_options(
            ArchiveExtractOptions {
                overwrite: true,
                allow_partial: false,
            },
            &Cancel::new(),
        )
        .unwrap();
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(
        std::fs::read(request.target_dir.join("file")).unwrap(),
        b"abc"
    );
    let outside = tempfile::tempdir().unwrap();
    let mut forbidden = request.clone();
    forbidden.target_dir = outside.path().canonicalize().unwrap();
    assert!(forbidden
        .prepare_with_snapshot(validated.clone(), &Cancel::new())
        .is_err());
    let mut injected = request;
    injected.source_path = outside.path().join("forged.zip");
    assert!(injected
        .prepare_with_snapshot(validated, &Cancel::new())
        .is_err());
}
#[test]
fn snapshot_alle_auswahlen_uebernehmen_keinen_gewinner_einer_namensgruppe() {
    for scope in ["entry", "all", "subtree"] {
        let (_temp, _cache, mut request, validated) = snapshot_request(&fixture(&[
            (b"A/file", b"abc", 0o100644),
            (b"A/FILE", b"other", 0o100644),
            (b"A/empty/", b"", 0o040755),
            (b"outside/ok", b"independent", 0o100644),
        ]));
        request.selection = match scope {
            "all" => ArchiveExtractSelection::All,
            "subtree" => ArchiveExtractSelection::Subtree { prefix: "A".into() },
            _ => request.selection,
        };
        let prepared = request
            .prepare_with_snapshot(validated, &Cancel::new())
            .unwrap();
        assert_eq!(prepared.preview().files, u64::from(scope == "all"));
        assert_eq!(
            prepared.preview().omitted_entries,
            if scope == "entry" { 1 } else { 2 }
        );
        prepared.execute(None, &mut |_| {}).unwrap();
        assert!(!request.target_dir.join("file").exists());
        assert!(!request.target_dir.join("A/file").exists());
        assert!(!request.target_dir.join("A/FILE").exists());
        if scope != "entry" {
            assert!(request.target_dir.join("A/empty").is_dir());
        }
        if scope == "all" {
            assert_eq!(
                std::fs::read(request.target_dir.join("outside/ok")).unwrap(),
                b"independent"
            );
        } else {
            assert!(!request.target_dir.join("outside").exists());
        }
    }
}

#[test]
fn snapshot_crc_abbruch_und_gehaltener_quellentausch_erhalten_altziel() {
    for attack in ["crc", "cancel", "source_swap"] {
        let data = vec![7; 50000];
        let mut bytes = fixture(&[(b"file", &data, 0o100644)]);
        if attack == "crc" {
            bytes[34] ^= 1;
        }
        let (_temp, _cache, mut request, validated) = snapshot_request(&bytes);
        request.options.overwrite = true;
        std::fs::write(request.target_dir.join("file"), b"old").unwrap();
        let cancel = Cancel::new();
        let prepared = request
            .prepare_with_snapshot(validated.clone(), &cancel)
            .unwrap();
        assert_eq!(prepared.preview().conflicts.len(), 1);
        let (root, _, name) = validated.source_parts().unwrap();
        let path = root.path().join(name);
        let result = prepared.execute(None, &mut |event| {
            if attack == "cancel" {
                cancel.cancel();
            }
            if attack == "source_swap" && matches!(event.phase, ArchiveExtractPhase::Verifying) {
                let replacement = root.path().join("replacement");
                std::fs::write(&replacement, b"replacement").unwrap();
                std::fs::rename(replacement, &path).unwrap();
            }
        });
        let unchanged = match result {
            Ok(report) => {
                assert_eq!(report.status, ArchiveExtractStatus::Cancelled);
                report.files == 0
            }
            Err(failure) => failure.report.files == 0,
        };
        assert!(unchanged, "{attack}");
        assert_eq!(
            std::fs::read(request.target_dir.join("file")).unwrap(),
            b"old",
            "{attack}"
        );
    }
}

#[test]
fn snapshot_unterbaum_teilkatalog_braucht_bestaetigten_bekannten_umfang() {
    let mut names = vec!["outer/scope/file".to_owned()];
    names.extend((0..20_000).map(|index| format!("outside{index}")));
    let items = names
        .iter()
        .map(|name| (name.as_bytes(), b"data".as_slice(), 0o100644))
        .collect::<Vec<_>>();
    let (_temp, _cache, mut request, validated) = snapshot_request(&fixture(&items));
    request.selection = ArchiveExtractSelection::Subtree {
        prefix: "outer/scope".into(),
    };
    let prepared = request
        .prepare_with_snapshot(validated.clone(), &Cancel::new())
        .unwrap();
    assert!(!prepared.preview().catalog_complete);
    assert_eq!(prepared.preview().known_entries, 1);
    let failure = prepared.execute(None, &mut |_| {}).unwrap_err();
    assert!(!failure.report.has_effect);
    assert!(std::fs::read_dir(&request.target_dir)
        .unwrap()
        .next()
        .is_none());
    request.options.allow_partial = true;
    let prepared = request
        .prepare_with_snapshot(validated, &Cancel::new())
        .unwrap();
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert!(!report.preview.catalog_complete);
    assert_eq!(
        std::fs::read(request.target_dir.join("scope/file")).unwrap(),
        b"data"
    );
}

thread_local! {
    static INDEX_CANCEL: std::cell::RefCell<Option<(usize, Cancel)>> = const { std::cell::RefCell::new(None) };
    static PARENT_LISTINGS: std::cell::RefCell<Option<BTreeMap<PathBuf, usize>>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn target_name_indexed() {
    INDEX_CANCEL.with(|state| {
        if let Some((remaining, cancel)) = state.borrow_mut().as_mut() {
            *remaining -= 1;
            if *remaining == 0 {
                cancel.cancel();
            }
        }
    });
}
pub(super) fn record_parent_listing(parent: &Path) {
    PARENT_LISTINGS.with(|counts| {
        if let Some(counts) = counts.borrow_mut().as_mut() {
            *counts.entry(parent.into()).or_default() += 1;
        }
    });
}
fn count_parent_listings<T>(test: impl FnOnce() -> T) -> (T, BTreeMap<PathBuf, usize>) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PARENT_LISTINGS.with(|counts| *counts.borrow_mut() = None);
        }
    }
    PARENT_LISTINGS.with(|counts| {
        assert!(counts.replace(Some(BTreeMap::new())).is_none());
    });
    let _reset = Reset;
    let result = test();
    let counts = PARENT_LISTINGS.with(|counts| counts.borrow_mut().take().unwrap());
    (result, counts)
}
#[test]
fn vorbereitung_listet_gemeinsamen_zielparent_nur_einmal() {
    let names: Vec<_> = (0..24).map(|i| format!("file-{i:02}")).collect();
    let items: Vec<_> = names
        .iter()
        .map(|n| (n.as_bytes(), b"new".as_slice(), 0o100644))
        .collect();
    let (_temp, request) = request(&fixture(&items));
    for i in 0..32 {
        std::fs::write(request.target_dir.join(format!("existing-{i:02}")), b"old").unwrap();
    }
    let (prepared, counts) = count_parent_listings(|| request.prepare(&Cancel::new()).unwrap());
    assert_eq!(prepared.preview().files, 24);
    assert_eq!(counts, BTreeMap::from([(request.target_dir.clone(), 1)]));
}

#[test]
fn vorbereitung_teilt_nur_vorhandene_parentlistings_und_entpackt_fehlende_eltern() {
    let (_temp, request) = request(&fixture(&[
        (b"a/one", b"1", 0o100644),
        (b"a/two", b"2", 0o100644),
        (b"b/three", b"3", 0o100644),
        (b"a/missing/deep/four", b"4", 0o100644),
        (b"a/missing/deep/five", b"5", 0o100644),
    ]));
    for parent in ["a", "b"] {
        std::fs::create_dir(request.target_dir.join(parent)).unwrap();
        std::fs::write(request.target_dir.join(parent).join("sentinel"), b"old").unwrap();
    }
    let (prepared, counts) = count_parent_listings(|| request.prepare(&Cancel::new()).unwrap());
    assert_eq!(
        counts,
        BTreeMap::from([
            (request.target_dir.clone(), 1),
            (request.target_dir.join("a"), 1),
            (request.target_dir.join("b"), 1),
        ])
    );
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 5);
    assert_eq!(report.directories, 2);
    assert_eq!(
        std::fs::read(request.target_dir.join("a/missing/deep/five")).unwrap(),
        b"5"
    );
    assert_eq!(
        std::fs::read(request.target_dir.join("a/sentinel")).unwrap(),
        b"old"
    );
    assert_eq!(
        std::fs::read(request.target_dir.join("b/sentinel")).unwrap(),
        b"old"
    );
}

#[test]
fn optionenwechsel_baut_zielnamen_frisch_und_erkennt_neuen_casekonflikt() {
    let (_temp, request) = request(&fixture(&[(b"file", b"new", 0o100644)]));
    let (error, counts) = count_parent_listings(|| {
        let prepared = request.prepare(&Cancel::new()).unwrap();
        std::fs::write(request.target_dir.join("FILE"), b"late").unwrap();
        prepared
            .with_options(ArchiveExtractOptions::default(), &Cancel::new())
            .err()
            .unwrap()
    });
    assert_eq!(error_code(&error), ErrorCode::PlanDrift);
    assert_eq!(counts, BTreeMap::from([(request.target_dir.clone(), 2)]));
    assert_eq!(
        std::fs::read(request.target_dir.join("FILE")).unwrap(),
        b"late"
    );
}

#[test]
fn zielindex_belaesst_case_unicode_und_exakte_konfliktentscheidungen() {
    let (_temp, request) = request(&fixture(&[
        (b"CASE", b"new", 0o100644),
        ("é".as_bytes(), b"new", 0o100644),
        (b"exact", b"new", 0o100644),
        (b"good", b"new", 0o100644),
    ]));
    std::fs::write(request.target_dir.join("Case"), b"old case").unwrap();
    std::fs::write(request.target_dir.join("e\u{301}"), b"old unicode").unwrap();
    std::fs::write(request.target_dir.join("exact"), b"old exact").unwrap();
    let prepared = request.prepare(&Cancel::new()).unwrap();
    assert_eq!(prepared.preview().files, 2);
    assert_eq!(prepared.preview().omitted_entries, 2);
    assert_eq!(
        prepared.preview().conflicts,
        vec![request.target_dir.join("exact").to_str().unwrap()]
    );
    let report = prepared.execute(None, &mut |_| {}).unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(
        std::fs::read(request.target_dir.join("Case")).unwrap(),
        b"old case"
    );
    assert_eq!(
        std::fs::read(request.target_dir.join("e\u{301}")).unwrap(),
        b"old unicode"
    );
    assert_eq!(
        std::fs::read(request.target_dir.join("exact")).unwrap(),
        b"old exact"
    );
    assert_eq!(
        std::fs::read(request.target_dir.join("good")).unwrap(),
        b"new"
    );
}

fn name_listing(names: &[&str]) -> crate::vfs::CompleteDirectoryListing {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("seed"), b"data").unwrap();
    let template = LocalVfs::new()
        .list_complete(&std::fs::canonicalize(temp.path()).unwrap())
        .unwrap()
        .entries
        .remove(0);
    crate::vfs::CompleteDirectoryListing {
        entries: names
            .iter()
            .map(|name| {
                let mut item = template.clone();
                item.entry.name = (*name).into();
                item.raw_name = name.as_bytes().to_vec();
                item
            })
            .collect(),
        provider_token: None,
        complete: true,
    }
}

#[test]
fn zielindex_entspricht_bisherigem_scan_auch_fuer_mehrdeutige_gruppen() {
    let names = ["Case", "CASE", "e\u{301}", "é", "exact", "İ", "i\u{307}"];
    let mut charged = 0;
    let groups = index_target_names(name_listing(&names), &mut charged, &Cancel::new()).unwrap();
    for candidate in [
        "Case", "case", "CASE", "é", "e\u{301}", "exact", "EXACT", "absent", "İ", "i\u{307}",
    ] {
        let old: Vec<_> = names
            .iter()
            .filter(|name| crate::remote_rename::names_may_collide(name, candidate))
            .collect();
        let expected_blocked = old.len() > 1 || old.first().is_some_and(|name| **name != candidate);
        let actual_blocked = groups
            .get(&folded_name(candidate))
            .is_some_and(|group| group.count > 1 || group.exact_name != candidate);
        assert_eq!(actual_blocked, expected_blocked, "{candidate}");
    }
}

#[test]
fn zielindex_belastet_gemeinsames_budget_und_erhaelt_listingfehler() {
    let mut charged = INDEX_BUDGET - 1;
    let error = index_target_names(name_listing(&["entry"]), &mut charged, &Cancel::new())
        .err()
        .unwrap();
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    assert!(error.to_string().contains("Metadaten-/Knotenbudget"));
    let mut charged = INDEX_BUDGET;
    assert!(charge_index(&mut charged, 1).is_err());
    let mut charged = usize::MAX;
    assert!(charge_index(&mut charged, 1).is_err());
    for unsafe_raw in [false, true] {
        let mut listing = name_listing(&["entry"]);
        if unsafe_raw {
            listing.entries[0].raw_name = vec![0xff];
        } else {
            listing.complete = false;
        }
        let error = index_target_names(listing, &mut 0, &Cancel::new())
            .err()
            .unwrap();
        assert_eq!(error_code(&error), ErrorCode::InvalidInput);
        assert!(error.to_string().contains("Ziel-Elternlisting"));
    }
}

#[test]
fn abbruch_waehrend_zielindexaufbau_laesst_ziel_unveraendert() {
    let (_temp, request) = request(&fixture(&[(b"new", b"new", 0o100644)]));
    for name in ["one", "two", "three", "four"] {
        std::fs::write(request.target_dir.join(name), b"old").unwrap();
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            INDEX_CANCEL.with(|state| *state.borrow_mut() = None);
        }
    }
    let cancel = Cancel::new();
    INDEX_CANCEL.with(|state| *state.borrow_mut() = Some((2, cancel.clone())));
    let _reset = Reset;
    let error = request.prepare(&cancel).err().unwrap();
    assert_eq!(error_code(&error), ErrorCode::Interrupted);
    assert!(cancel.is_cancelled());
    assert_eq!(
        INDEX_CANCEL.with(|state| state.borrow().as_ref().unwrap().0),
        0
    );
    assert!(!request.target_dir.join("new").exists());
    for name in ["one", "two", "three", "four"] {
        assert_eq!(
            std::fs::read(request.target_dir.join(name)).unwrap(),
            b"old"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn unsicherer_zielrohname_wird_nicht_als_leeres_listing_gecacht() {
    use std::os::unix::ffi::OsStringExt;
    let (_temp, request) = request(&fixture(&[(b"new", b"new", 0o100644)]));
    let unsafe_path = request
        .target_dir
        .join(std::ffi::OsString::from_vec(vec![0xff]));
    std::fs::write(&unsafe_path, b"old").unwrap();
    let error = request.prepare(&Cancel::new()).err().unwrap();
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    assert_eq!(std::fs::read(unsafe_path).unwrap(), b"old");
    assert!(!request.target_dir.join("new").exists());
}
