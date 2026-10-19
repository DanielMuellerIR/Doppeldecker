//! Tests für die interaktiven Datei-Operationen (Kopieren/Verschieben/Umbenennen/
//! Ordner anlegen). Wie die Sync-Tests komplett headless über echte temporäre
//! Verzeichnisse → identisch auf macOS und Linux (`cargo test -p doppeldecker-core`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use doppeldecker_core::fileops::rename;
use doppeldecker_core::{
    copy_items, create_dir, error_code, find_recursive_conflicts, move_items, Cancel, ErrorCode,
    FileOperationRequest, LocalVfs, OverwritePolicy, Progress,
};

// macOS stellt `/var` als Symlink auf `/private/var` bereit. Die App kanonisiert ihre
// Pane-Wurzeln ebenfalls; dieser lokale Wrapper lässt TempDir dieselbe Voraussetzung
// erfüllen, ohne jeden einzelnen Test unleserlich zu machen.
mod canonical_tempfile {
    pub fn tempdir() -> std::io::Result<::tempfile::TempDir> {
        let root = std::fs::canonicalize(std::env::temp_dir())?;
        ::tempfile::tempdir_in(root)
    }
}
use canonical_tempfile as tempfile;

// --- Helfer ----------------------------------------------------------------

fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
}

fn read_file(root: &Path, rel: &str) -> Vec<u8> {
    fs::read(root.join(rel)).unwrap()
}

fn mode(root: &Path, rel: &str) -> u32 {
    fs::metadata(root.join(rel)).unwrap().permissions().mode() & 0o777
}

fn full_mode(root: &Path, rel: &str) -> u32 {
    fs::metadata(root.join(rel)).unwrap().permissions().mode() & 0o7777
}

fn ignore_progress(_p: &Progress) {}

/// Absolute Item-Pfade (Root + Namen) bequem bauen.
fn items(root: &Path, names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(|n| root.join(n)).collect()
}

/// Keine Sandbox (normaler Dateimanager-Betrieb).
fn no_sandbox() -> Option<PathBuf> {
    None
}

// --- Kopieren --------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn additive_ordnerkopie_bilanziert_echte_nicht_utf8_namen() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("quelle");
    let selected = source.join("ordner");
    let destination = root.path().join("ziel");
    fs::create_dir_all(selected.join("unterordner")).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(selected.join("sicher.txt"), b"sicher").unwrap();
    fs::write(selected.join("unterordner/kind.txt"), b"kind").unwrap();
    let raw = OsString::from_vec(b"latin-\xff.txt".to_vec());
    fs::write(selected.join(&raw), b"ausgelassen").unwrap();
    fs::write(selected.join("unterordner").join(&raw), b"ebenfalls").unwrap();
    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: std::slice::from_ref(&selected),
            dst_dir: &destination,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .expect("Unabhängige sichere Additionen müssen trotz ausgelassener Rohnamen laufen");
    assert_eq!(stats.files, 2);
    assert_eq!(stats.bytes, 10);
    assert_eq!(stats.skipped, 2);
    assert_eq!(
        fs::read(destination.join("ordner/sicher.txt")).unwrap(),
        b"sicher"
    );
    assert_eq!(
        fs::read(destination.join("ordner/unterordner/kind.txt")).unwrap(),
        b"kind"
    );
    assert_eq!(fs::read(selected.join(&raw)).unwrap(), b"ausgelassen");
    assert_eq!(
        fs::read(selected.join("unterordner").join(&raw)).unwrap(),
        b"ebenfalls"
    );
    assert_eq!(fs::read_dir(destination.join("ordner")).unwrap().count(), 2);
    assert_eq!(
        fs::read_dir(destination.join("ordner/unterordner"))
            .unwrap()
            .count(),
        1
    );
    assert!(stats
        .source_read_limitations
        .join("\n")
        .contains("6c6174696e2dff2e747874"));
    fs::write(selected.join("neu.txt"), b"neu").unwrap();
    let merged = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: std::slice::from_ref(&selected),
            dst_dir: &destination,
            policy: OverwritePolicy::Skip,
            sandbox: &None,
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(merged.files, 1);
    assert_eq!(merged.skipped, 4);
    assert_eq!(
        fs::read(destination.join("ordner/neu.txt")).unwrap(),
        b"neu"
    );
    assert_eq!(
        fs::read(destination.join("ordner/sicher.txt")).unwrap(),
        b"sicher"
    );
    assert_eq!(fs::read_dir(destination.join("ordner")).unwrap().count(), 3);
}

#[cfg(target_os = "linux")]
#[test]
fn quellversionsbeleg_bindet_ausgelassene_rohnamen_ohne_sie_zu_oeffnen() {
    use doppeldecker_core::fileops::{capture_file_source_proofs, verify_file_source_proofs};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let selected = root.path().join("ordner");
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("sicher.txt"), b"sicher").unwrap();
    let raw = OsString::from_vec(b"latin-\xff.txt".to_vec());
    fs::write(selected.join(&raw), b"ausgelassen").unwrap();
    let vfs = LocalVfs::new();
    let proof = capture_file_source_proofs(&vfs, &[selected.to_str().unwrap().into()])
        .expect("Die Queue muss einen sicheren Teilbaum samt Rohbestand binden können");
    verify_file_source_proofs(&vfs, &proof).unwrap();
    fs::rename(
        selected.join(&raw),
        selected.join(OsString::from_vec(b"anders-\xff.txt".to_vec())),
    )
    .unwrap();
    let drift = verify_file_source_proofs(&vfs, &proof).unwrap_err();
    assert_eq!(error_code(&drift), ErrorCode::PlanDrift);
}

#[cfg(target_os = "linux")]
#[test]
fn rohname_erteilt_keine_move_ersetzungs_oder_konfliktkopiefreigabe() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let selected = root.path().join("ordner");
    let target = root.path().join("ziel");
    fs::create_dir(&selected).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(selected.join("sicher.txt"), b"sicher").unwrap();
    let raw = std::ffi::OsString::from_vec(b"latin-\xff.txt".to_vec());
    fs::write(selected.join(&raw), b"ausgelassen").unwrap();
    let vfs = LocalVfs::new();
    let request = |policy| FileOperationRequest {
        src: &vfs,
        dst: &vfs,
        items: std::slice::from_ref(&selected),
        dst_dir: &target,
        policy,
        sandbox: &None,
    };
    assert!(move_items(
        request(OverwritePolicy::Skip),
        &Cancel::new(),
        &mut ignore_progress
    )
    .is_err());
    assert!(!target.join("ordner").exists());
    fs::create_dir(target.join("ordner")).unwrap();
    fs::write(target.join("ordner/sicher.txt"), b"alt").unwrap();
    assert!(copy_items(
        request(OverwritePolicy::Overwrite),
        &Cancel::new(),
        &mut ignore_progress
    )
    .is_err());
    let aliases = doppeldecker_core::fileops::prepare_keep_both_targets(
        &vfs,
        &vfs,
        &target,
        std::slice::from_ref(&selected),
        &[target.join("ordner/sicher.txt").to_str().unwrap().into()],
    )
    .unwrap();
    assert!(aliases.is_empty());
    assert_eq!(fs::read(target.join("ordner/sicher.txt")).unwrap(), b"alt");
    assert_eq!(fs::read(selected.join(&raw)).unwrap(), b"ausgelassen");
    assert_eq!(fs::read(selected.join("sicher.txt")).unwrap(), b"sicher");
}

#[test]
fn kopiert_datei_und_laesst_quelle_stehen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"hallo");

    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(stats.files, 1);
    assert_eq!(stats.bytes, 5);
    assert_eq!(read_file(dst.path(), "a.txt"), b"hallo");
    assert!(
        src.path().join("a.txt").exists(),
        "Quelle bleibt beim Kopieren erhalten"
    );
}

#[test]
fn auswahl_verwirft_duplikate_und_vorfahr_nachfahr_vor_mutation() {
    let src = tempfile::tempdir().unwrap();
    write_file(src.path(), "ordner/kind.txt", b"quelle");
    let source_dir = src.path().join("ordner");
    let source_child = source_dir.join("kind.txt");
    let vfs = LocalVfs::new();

    for selected in [
        vec![source_child.clone(), source_child.clone()],
        vec![source_dir.clone(), source_child.clone()],
    ] {
        let dst = tempfile::tempdir().unwrap();
        let error = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &selected,
                dst_dir: dst.path(),
                policy: OverwritePolicy::Skip,
                sandbox: &no_sandbox(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .expect_err("überlappende Auswahl muss vor dem ersten Zielschreiben scheitern");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_dir(dst.path()).unwrap().count(), 0);
        assert_eq!(read_file(src.path(), "ordner/kind.txt"), b"quelle");
    }
}

#[test]
fn kopiert_ordner_rekursiv() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "ordner/tief/x.txt", b"drin");
    fs::create_dir_all(src.path().join("ordner/leer")).unwrap();

    let vfs = LocalVfs::new();
    copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["ordner"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "ordner/tief/x.txt"), b"drin");
    assert!(
        dst.path().join("ordner/leer").is_dir(),
        "leerer Unterordner muss mitkommen"
    );
}

#[test]
fn skip_fuehrt_ordner_zusammen_und_laesst_nur_kollidierende_blaetter_aus() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "ordner/neu.txt", b"neu");
    write_file(src.path(), "ordner/kollision.txt", b"quelle");
    write_file(dst.path(), "ordner/kollision.txt", b"ziel");
    write_file(dst.path(), "ordner/nur-ziel.txt", b"bleibt");
    let vfs = LocalVfs::new();
    let selected = items(src.path(), &["ordner"]);

    let conflicts = find_recursive_conflicts(&vfs, &vfs, dst.path(), &selected).unwrap();
    assert_eq!(
        conflicts,
        vec![dst
            .path()
            .join("ordner/kollision.txt")
            .to_string_lossy()
            .into_owned()]
    );

    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &selected,
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(stats.skipped, 1);
    assert_eq!(read_file(dst.path(), "ordner/neu.txt"), b"neu");
    assert_eq!(read_file(dst.path(), "ordner/kollision.txt"), b"ziel");
    assert_eq!(read_file(dst.path(), "ordner/nur-ziel.txt"), b"bleibt");
}

#[test]
fn kopieren_in_eigenen_unterordner_scheitert_vor_mutation() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "quelle/datei.txt", b"bleibt");
    fs::create_dir(root.path().join("quelle/ziel")).unwrap();
    let source = root.path().join("quelle");
    let destination = source.join("ziel");
    let vfs = LocalVfs::new();

    let error = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: std::slice::from_ref(&source),
            dst_dir: &destination,
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(read_file(root.path(), "quelle/datei.txt"), b"bleibt");
    assert!(!destination.join("quelle").exists());
}

#[test]
fn kopieren_auf_identisches_ziel_scheitert_vor_mutation() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "datei.txt", b"bleibt");
    let source = root.path().join("datei.txt");
    let vfs = LocalVfs::new();

    let error = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: std::slice::from_ref(&source),
            dst_dir: root.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(read_file(root.path(), "datei.txt"), b"bleibt");
}

#[test]
fn kopieren_erhaelt_posix_rechte_von_datei_und_ordner() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "paket/start.sh", b"#!/bin/sh\nexit 0\n");
    fs::set_permissions(src.path().join("paket"), fs::Permissions::from_mode(0o1750)).unwrap();
    fs::set_permissions(
        src.path().join("paket/start.sh"),
        fs::Permissions::from_mode(0o551),
    )
    .unwrap();

    let vfs = LocalVfs::new();
    copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["paket"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(mode(dst.path(), "paket"), 0o750);
    assert_eq!(full_mode(dst.path(), "paket"), 0o750);
    assert_eq!(mode(dst.path(), "paket/start.sh"), 0o551);
}

#[test]
fn konflikt_skip_laesst_ziel_unberuehrt() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"NEU");
    write_file(dst.path(), "a.txt", b"ALT");

    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(stats.skipped, 1);
    assert_eq!(
        read_file(dst.path(), "a.txt"),
        b"ALT",
        "Skip darf nichts überschreiben"
    );
}

#[test]
fn konflikt_overwrite_ueberschreibt_datei_und_fuehrt_ordner_zusammen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // Datei-Konflikt:
    write_file(src.path(), "a.txt", b"NEU");
    write_file(dst.path(), "a.txt", b"ALT");
    // Ordner-Konflikt (Merge): Quelle bringt neu.txt + überschreibt gleich.txt;
    // ziel-nur.txt muss ERHALTEN bleiben (Merge löscht nichts).
    write_file(src.path(), "d/neu.txt", b"n");
    write_file(src.path(), "d/gleich.txt", b"src");
    write_file(dst.path(), "d/gleich.txt", b"dst");
    write_file(dst.path(), "d/ziel-nur.txt", b"bleibt");

    let vfs = LocalVfs::new();
    copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt", "d"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(
        read_file(dst.path(), "a.txt"),
        b"NEU",
        "Datei überschrieben"
    );
    assert_eq!(read_file(dst.path(), "d/neu.txt"), b"n");
    assert_eq!(
        read_file(dst.path(), "d/gleich.txt"),
        b"src",
        "gleichnamige Datei überschrieben"
    );
    assert_eq!(
        read_file(dst.path(), "d/ziel-nur.txt"),
        b"bleibt",
        "Merge löscht Ziel-Extras NICHT"
    );
}

// --- Verschieben -----------------------------------------------------------

#[test]
fn verschiebt_datei_quelle_verschwindet() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"inhalt");

    let vfs = LocalVfs::new();
    move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "a.txt"), b"inhalt");
    assert!(
        !src.path().join("a.txt").exists(),
        "Quelle muss nach dem Verschieben weg sein"
    );
}

#[test]
fn verschiebt_ordner_rekursiv() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "ord/tief/x.txt", b"y");

    let vfs = LocalVfs::new();
    move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["ord"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "ord/tief/x.txt"), b"y");
    assert!(
        !src.path().join("ord").exists(),
        "Quell-Ordner muss weg sein"
    );
}

#[test]
fn verschieben_in_eigenen_unterordner_scheitert_vor_mutation() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "quelle/datei.txt", b"bleibt");
    fs::create_dir(root.path().join("quelle/ziel")).unwrap();
    let source = root.path().join("quelle");
    let destination = source.join("ziel");
    let vfs = LocalVfs::new();

    let error = move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: std::slice::from_ref(&source),
            dst_dir: &destination,
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(read_file(root.path(), "quelle/datei.txt"), b"bleibt");
    assert!(!destination.join("quelle").exists());
}

#[test]
fn verschiebt_mit_overwrite_nach_frischem_quell_readback() {
    // Ziel existiert bereits → Fallback-Weg. Ohne starken Providerbeleg wird die
    // einzelne Quelle unmittelbar vor dem begrenzten Entfernen nochmals vollständig
    // gelesen. So bleibt der normale F6-Weg praktisch nutzbar.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"NEU");
    write_file(dst.path(), "a.txt", b"ALT");

    let vfs = LocalVfs::new();
    move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "a.txt"), b"NEU");
    assert!(!src.path().join("a.txt").exists());
}

#[test]
fn verschiebt_skip_laesst_quelle_und_ziel() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "a.txt", b"NEU");
    write_file(dst.path(), "a.txt", b"ALT");

    let vfs = LocalVfs::new();
    let stats = move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(stats.skipped, 1);
    assert_eq!(read_file(dst.path(), "a.txt"), b"ALT");
    assert!(src.path().join("a.txt").exists(), "Skip: Quelle bleibt");
}

// --- Umbenennen ------------------------------------------------------------

#[test]
fn benennt_um() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "alt.txt", b"x");

    let vfs = LocalVfs::new();
    rename(&vfs, &dir.path().join("alt.txt"), "neu.txt", &no_sandbox()).unwrap();

    assert!(!dir.path().join("alt.txt").exists());
    assert_eq!(read_file(dir.path(), "neu.txt"), b"x");
}

#[test]
fn umbenennen_auf_vorhandenen_namen_scheitert() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.txt", b"a");
    write_file(dir.path(), "b.txt", b"b");

    let vfs = LocalVfs::new();
    let res = rename(&vfs, &dir.path().join("a.txt"), "b.txt", &no_sandbox());
    assert!(res.is_err(), "darf vorhandenes b.txt nicht überschreiben");
    assert_eq!(read_file(dir.path(), "b.txt"), b"b", "b.txt unangetastet");
}

#[test]
fn umbenennen_mit_ungueltigem_namen_scheitert() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.txt", b"a");
    let vfs = LocalVfs::new();
    for bad in ["", "..", "unter/pfad.txt"] {
        assert!(
            rename(&vfs, &dir.path().join("a.txt"), bad, &no_sandbox()).is_err(),
            "Name „{bad}“ sollte abgelehnt werden"
        );
    }
    assert!(dir.path().join("a.txt").exists(), "nichts kaputt gemacht");
}

// --- Ordner anlegen --------------------------------------------------------

#[test]
fn legt_ordner_an() {
    let dir = tempfile::tempdir().unwrap();
    let vfs = LocalVfs::new();
    let created = create_dir(&vfs, dir.path(), "Neuer Ordner", &no_sandbox()).unwrap();
    assert!(created.is_dir());
    assert!(dir.path().join("Neuer Ordner").is_dir());
}

#[test]
fn ordner_anlegen_bei_kollision_scheitert() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("da")).unwrap();
    let vfs = LocalVfs::new();
    assert!(create_dir(&vfs, dir.path(), "da", &no_sandbox()).is_err());
}

// codereview-ok: Der leere Abschnittsanker trennt die abgeschlossenen
// Kollisionsfälle von den folgenden Symlink-Sicherheitsfällen (2026-09-12).
// --- Konflikt-Erkennung ----------------------------------------------------

// --- Symlinks --------------------------------------------------------------

#[test]
fn symlink_wird_beim_kopieren_uebersprungen() {
    #[cfg(unix)]
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "echt.txt", b"inhalt");
        std::os::unix::fs::symlink(src.path().join("echt.txt"), src.path().join("link.txt"))
            .unwrap();

        let vfs = LocalVfs::new();
        let stats = copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &items(src.path(), &["echt.txt", "link.txt"]),
                dst_dir: dst.path(),
                policy: OverwritePolicy::Skip,
                sandbox: &no_sandbox(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();

        assert_eq!(stats.symlinks_skipped, 1);
        assert!(dst.path().join("echt.txt").exists());
        assert!(
            !dst.path().join("link.txt").exists(),
            "Symlink nicht kopiert"
        );
    }
}

#[test]
fn overwrite_abbruch_erhaelt_altes_ziel_und_laesst_temp_als_recoverypfad() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "gross.bin", &vec![b'N'; 512 * 1024]);
    write_file(dst.path(), "gross.bin", b"ALTER WICHTIGER INHALT");

    let cancel = Cancel::new();
    let cancel_from_callback = cancel.clone();
    let mut cancel_after_first_chunk = move |progress: &Progress| {
        if progress.bytes_done > 0 {
            cancel_from_callback.cancel();
        }
    };
    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["gross.bin"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &cancel,
        &mut cancel_after_first_chunk,
    )
    .unwrap();

    assert!(stats.cancelled);
    assert_eq!(
        read_file(dst.path(), "gross.bin"),
        b"ALTER WICHTIGER INHALT",
        "die direkte Zielkopie darf vor dem atomaren Commit nie angerührt werden"
    );
    assert_eq!(stats.temp_recovery_paths.len(), 1);
    let recovery = std::path::PathBuf::from(&stats.temp_recovery_paths[0]);
    assert!(recovery.exists());
}

#[test]
fn spaet_entstandenes_ziel_bleibt_und_temp_wird_recoverypfad() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "rennen.bin", &vec![b'N'; 512 * 1024]);

    let late_target = dst.path().join("rennen.bin");
    let mut target_created = false;
    let mut create_during_copy = |progress: &Progress| {
        if progress.bytes_done > 0 && !target_created {
            fs::write(&late_target, b"PARALLEL ENTSTANDEN").unwrap();
            target_created = true;
        }
    };
    let vfs = LocalVfs::new();
    let result = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["rennen.bin"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut create_during_copy,
    );

    let error = result.unwrap_err();
    assert_eq!(error_code(&error), ErrorCode::AlreadyExists);
    assert_eq!(read_file(dst.path(), "rennen.bin"), b"PARALLEL ENTSTANDEN");
    let recovery = fs::read_dir(dst.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("doppeldecker-fileop-tmp"))
        })
        .expect("Die ungebundene Tempdatei muss als Recoverybestand erhalten bleiben");
    assert!(error.to_string().contains(&recovery.display().to_string()));
}

#[test]
fn typwechsel_abbruch_laesst_beide_recoverypfade_stehen() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "Projekt/gross.bin", &vec![b'N'; 512 * 1024]);
    write_file(dst.path(), "Projekt", b"ALTE DATEI");

    let cancel = Cancel::new();
    let cancel_from_callback = cancel.clone();
    let mut cancel_after_first_chunk = move |progress: &Progress| {
        if progress.bytes_done > 0 {
            cancel_from_callback.cancel();
        }
    };
    let vfs = LocalVfs::new();
    let error = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["Projekt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &cancel,
        &mut cancel_after_first_chunk,
    )
    .unwrap_err();

    let backup = fs::read_dir(dst.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("doppeldecker-replaced"))
        })
        .expect("Geparktes Altziel muss als Recoverypfad erhalten bleiben");
    let visible_target = dst.path().join("Projekt");
    assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
    assert!(
        visible_target.is_dir(),
        "Der neue Zielbaum bleibt unangetastet"
    );
    assert_eq!(fs::read(&backup).unwrap(), b"ALTE DATEI");
    assert!(error
        .to_string()
        .contains(&visible_target.display().to_string()));
    assert!(error.to_string().contains(&backup.display().to_string()));
}

#[test]
fn overwrite_ersetzt_ziel_symlink_ohne_dessen_ziel_zu_oeffnen() {
    #[cfg(unix)]
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_file(src.path(), "link.txt", b"NEUER INHALT");
        write_file(outside.path(), "wichtig.txt", b"AUSSEN BLEIBT");
        std::os::unix::fs::symlink(
            outside.path().join("wichtig.txt"),
            dst.path().join("link.txt"),
        )
        .unwrap();

        let vfs = LocalVfs::new();
        copy_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &items(src.path(), &["link.txt"]),
                dst_dir: dst.path(),
                policy: OverwritePolicy::Overwrite,
                sandbox: &no_sandbox(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();

        assert_eq!(read_file(outside.path(), "wichtig.txt"), b"AUSSEN BLEIBT");
        assert_eq!(read_file(dst.path(), "link.txt"), b"NEUER INHALT");
        assert!(
            !fs::symlink_metadata(dst.path().join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "ersetzt wird der Link als Verzeichniseintrag, nicht sein Ziel"
        );
    }
}

#[test]
fn verschieben_mit_uebersprungenem_symlink_behaelt_quellordner() {
    #[cfg(unix)]
    {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "Ordner/datei.txt", b"KOPIE");
        fs::create_dir(dst.path().join("Ordner")).unwrap();
        std::os::unix::fs::symlink("datei.txt", src.path().join("Ordner/link.txt")).unwrap();

        let vfs = LocalVfs::new();
        let stats = move_items(
            FileOperationRequest {
                src: &vfs,
                dst: &vfs,
                items: &items(src.path(), &["Ordner"]),
                dst_dir: dst.path(),
                policy: OverwritePolicy::Overwrite,
                sandbox: &no_sandbox(),
            },
            &Cancel::new(),
            &mut ignore_progress,
        )
        .unwrap();

        assert_eq!(stats.symlinks_skipped, 1);
        assert_eq!(read_file(dst.path(), "Ordner/datei.txt"), b"KOPIE");
        assert!(
            src.path().join("Ordner/datei.txt").exists(),
            "Quelle darf wegen des nicht kopierten Links nicht entfernt werden"
        );
        assert!(fs::symlink_metadata(src.path().join("Ordner/link.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

#[test]
fn verschieben_skip_behaelt_quelle_bei_verschachteltem_konflikt() {
    // Regression: Ein bei `Skip` bewusst nicht kopiertes Kind (Konflikt im
    // zusammengeführten Zielordner) darf die Quellentfernung nicht zulassen —
    // sonst löscht das Move genau den Eintrag, der nie am Ziel angekommen ist.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "Ordner/kollidiert.txt", b"QUELLE");
    write_file(src.path(), "Ordner/neu.txt", b"NEU");
    write_file(dst.path(), "Ordner/kollidiert.txt", b"ZIEL");

    let vfs = LocalVfs::new();
    let stats = move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["Ordner"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(stats.skipped, 1);
    assert_eq!(read_file(dst.path(), "Ordner/kollidiert.txt"), b"ZIEL");
    assert_eq!(read_file(dst.path(), "Ordner/neu.txt"), b"NEU");
    assert_eq!(
        read_file(src.path(), "Ordner/kollidiert.txt"),
        b"QUELLE",
        "Skip: Der nicht kopierte Quell-Eintrag muss erhalten bleiben"
    );
    assert!(
        src.path().join("Ordner").exists(),
        "Skip: Die Quellkopie bleibt vollständig, weil ein Kind übersprungen wurde"
    );
}

#[test]
fn verschieben_skip_behaelt_quelle_bei_verschachteltem_typkonflikt() {
    // Regression: Im zusammengeführten Zielordner liegt am Namen des
    // Quell-UNTERORDNERS eine Datei. Das ist ein echter Blattkonflikt, also muss
    // `Skip` ihn überspringen. Vorher parkte der Ordnerzweig von `copy_tree` die
    // Zieldatei ohne jede Policy-Abfrage, ersetzte sie durch den neuen Ordner und
    // ließ `stats.skipped` unverändert — das Move entfernte danach zusätzlich die
    // Quelle. Ergebnis: Die Zieldatei war weg, obwohl „Überspringen" gewählt war.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "Ordner/kind/drin.txt", b"QUELLE");
    write_file(src.path(), "Ordner/neu.txt", b"NEU");
    write_file(dst.path(), "Ordner/kind", b"ZIELDATEI");

    let vfs = LocalVfs::new();
    let stats = move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["Ordner"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(stats.skipped, 1);
    assert!(
        dst.path().join("Ordner/kind").is_file(),
        "Skip: Die kollidierende Zieldatei muss eine Datei bleiben"
    );
    assert_eq!(read_file(dst.path(), "Ordner/kind"), b"ZIELDATEI");
    assert_eq!(read_file(dst.path(), "Ordner/neu.txt"), b"NEU");
    assert_eq!(
        read_file(src.path(), "Ordner/kind/drin.txt"),
        b"QUELLE",
        "Skip: Der nicht kopierte Quell-Teilbaum muss erhalten bleiben"
    );
}

#[test]
fn kopieren_skip_laesst_verschachtelten_typkonflikt_unberuehrt() {
    // Dieselbe Stelle aus Sicht des reinen Kopierens: Ohne die Policy-Abfrage im
    // Ordnerzweig verlor das Ziel seine Datei, und `stats.skipped` blieb bei 0.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "Ordner/kind/drin.txt", b"QUELLE");
    write_file(dst.path(), "Ordner/kind", b"ZIELDATEI");

    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["Ordner"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(stats.skipped, 1);
    assert!(dst.path().join("Ordner/kind").is_file());
    assert_eq!(read_file(dst.path(), "Ordner/kind"), b"ZIELDATEI");
}

#[test]
fn verschieben_skip_fuehrt_ordner_zusammen_und_baut_quelle_blattweise_ab() {
    // Regression: Ein vorhandener Zielordner ohne Blattkonflikte wird beim Move
    // mit `Skip` zusammengeführt (nicht komplett übersprungen) und die Quelle
    // wird nach vollständigem Manifest-Readback blattweise und nur über leere Ordner
    // abgebaut. Der bestehende Zielbestand bleibt erhalten.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "Ordner/neu.txt", b"NEU");
    write_file(dst.path(), "Ordner/bestand.txt", b"BESTAND");

    let vfs = LocalVfs::new();
    move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["Ordner"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "Ordner/neu.txt"), b"NEU");
    assert_eq!(read_file(dst.path(), "Ordner/bestand.txt"), b"BESTAND");
    assert!(!src.path().join("Ordner").exists());
}

#[test]
fn verschieben_zwischen_getrennten_vfs_baut_frisch_gepruefte_quelle_ab() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(src.path(), "datei.txt", b"GETRENNTE VFS");
    let src_vfs = LocalVfs::rooted(src.path()).unwrap();
    let dst_vfs = LocalVfs::rooted(dst.path()).unwrap();

    move_items(
        FileOperationRequest {
            src: &src_vfs,
            dst: &dst_vfs,
            items: &items(src.path(), &["datei.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();

    assert_eq!(read_file(dst.path(), "datei.txt"), b"GETRENNTE VFS");
    assert!(!src.path().join("datei.txt").exists());
}

// --- Sandbox-Schranke ------------------------------------------------------

#[test]
fn sandbox_blockt_kopieren_ausserhalb() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap(); // außerhalb der Sandbox
    write_file(src.path(), "a.txt", b"neu");

    let vfs = LocalVfs::new();
    let res = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &Some(sandbox.path().to_path_buf()),
        },
        &Cancel::new(),
        &mut ignore_progress,
    );
    assert!(
        res.is_err(),
        "Ziel außerhalb der Sandbox muss abgelehnt werden"
    );
    assert!(!dst.path().join("a.txt").exists(), "nichts kopiert");
}

#[test]
fn sandbox_blockt_verschieben_wenn_quelle_ausserhalb() {
    // Ziel liegt in der Sandbox, Quelle NICHT → Verschieben würde die Quelle außerhalb
    // entfernen → muss abgelehnt werden, bevor etwas passiert.
    let sandbox = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap(); // außerhalb
    let dst_dir = sandbox.path().join("ziel");
    fs::create_dir_all(&dst_dir).unwrap();
    write_file(src.path(), "a.txt", b"wichtig");

    let vfs = LocalVfs::new();
    let res = move_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt"]),
            dst_dir: &dst_dir,
            policy: OverwritePolicy::Skip,
            sandbox: &Some(sandbox.path().to_path_buf()),
        },
        &Cancel::new(),
        &mut ignore_progress,
    );
    assert!(
        res.is_err(),
        "Quelle außerhalb der Sandbox darf nicht verschoben werden"
    );
    assert!(src.path().join("a.txt").exists(), "Quelle unangetastet");
}

#[test]
fn sandbox_erlaubt_innerhalb() {
    let sandbox = tempfile::tempdir().unwrap();
    let src_dir = sandbox.path().join("quelle");
    let dst_dir = sandbox.path().join("ziel");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();
    write_file(&src_dir, "a.txt", b"ok");

    let vfs = LocalVfs::new();
    copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(&src_dir, &["a.txt"]),
            dst_dir: &dst_dir,
            policy: OverwritePolicy::Skip,
            sandbox: &Some(sandbox.path().to_path_buf()),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(read_file(&dst_dir, "a.txt"), b"ok");
}

/// Auf einem case-sensitiven Dateisystem sind `datei.txt` und `Datei.txt` zwei
/// verschiedene Einträge. Die konservative Namensäquivalenz ist für normalisierende
/// **Server** gedacht; lokal brach sie eine gewöhnliche Kopie mit „Name nicht
/// eindeutig" ab — und zwar die gesamte Auswahl, bevor irgendetwas passierte.
#[test]
fn ersetzen_funktioniert_neben_einem_namen_mit_anderer_schreibweise() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::write(src.path().join("datei.txt"), b"neu").unwrap();
    fs::write(dst.path().join("datei.txt"), b"alt").unwrap();
    // Nur auf einem case-sensitiven Volume entsteht hier ein zweiter Eintrag. Auf
    // case-insensitivem APFS ersetzt das Schreiben denselben Eintrag; der Test bleibt
    // dann gültig, prüft aber nur den gewöhnlichen Fall.
    let _ = fs::write(dst.path().join("Datei.txt"), b"anderer eintrag");

    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["datei.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Overwrite,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .expect("Ein Nachbar mit anderer Schreibweise darf die Kopie nicht abbrechen");

    assert_eq!(stats.files, 1);
    assert_eq!(fs::read(dst.path().join("datei.txt")).unwrap(), b"neu");
}

/// Der Nachbarname ist rund dreissig Bytes länger als der Zielname. Ohne Kürzung
/// sprengt eine völlig legale Datei mit 225-Byte-Namen das `NAME_MAX` von 255;
/// `openat` liefert dann `ENAMETOOLONG`, was kein `AlreadyExists` ist — die
/// Suchschleife bricht ab und die Datei wird nie kopiert.
#[test]
fn sehr_langer_dateiname_wird_trotzdem_kopiert() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let name = format!("{}.txt", "a".repeat(221));
    assert_eq!(name.len(), 225);
    fs::write(src.path().join(&name), b"inhalt").unwrap();

    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &[&name]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .expect("Ein 225-Byte-Name ist legal und muss kopierbar bleiben");

    assert_eq!(stats.files, 1);
    assert_eq!(fs::read(dst.path().join(&name)).unwrap(), b"inhalt");
}

#[test]
fn mehrere_uebersprungene_dateien_hinterlassen_keinen_staging_rest() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        write_file(src.path(), name, b"QUELLE");
        write_file(dst.path(), name, b"ZIEL");
    }
    let vfs = LocalVfs::new();
    let stats = copy_items(
        FileOperationRequest {
            src: &vfs,
            dst: &vfs,
            items: &items(src.path(), &["a.txt", "b.txt", "c.txt"]),
            dst_dir: dst.path(),
            policy: OverwritePolicy::Skip,
            sandbox: &no_sandbox(),
        },
        &Cancel::new(),
        &mut ignore_progress,
    )
    .unwrap();
    assert_eq!(stats.skipped, 3);
    assert!(stats.temp_recovery_paths.is_empty());
    assert_eq!(std::fs::read_dir(dst.path()).unwrap().count(), 3);
}
