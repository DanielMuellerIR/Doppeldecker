//! End-to-End-Tests des echten CLI-Protokolls. Alle Schreibfälle verwenden
//! ausschließlich temporäre künstliche Bäume.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_doppeldecker-cli")
}

/// Wegwerf-Umgebung für genau einen Testlauf.
///
/// **Regel dieser Datei: Jeder CLI-Prozess entsteht über `CliSandbox`.** Ohne
/// `--config-dir` benutzt die CLI absichtlich den echten Konfigurationsordner der
/// Desktop-App und öffnet dort das echte Queue-Journal. Am 2026-08-02 hat genau das ein
/// Testlauf getan und Daniels Journal auf eine neuere Journalversion migriert; danach
/// blieb `cargo test -p doppeldecker-cli` liegen. Die Sandbox sperrt das dreifach ab:
///
/// 1. `--config-dir` zeigt immer in den Wegwerfbaum.
/// 2. `HOME` und die XDG-Variablen zeigen ebenfalls dorthin. Selbst ein Pfad, der
///    intern doch am Benutzerordner hängt, landet damit im Wegwerfbaum.
/// 3. `DOPPELDECKER_NO_DEFAULT_USER_PATHS` lässt die CLI einen fehlenden
///    `--config-dir` sichtbar abweisen, statt still den echten Ordner zu nehmen.
///
/// Der Test `jeder_cli_start_bekommt_einen_isolierten_konfigurationsordner` hält Regel 1
/// mechanisch fest, `fehlender_konfigurationsordner_wird_abgewiesen` belegt Regel 3.
struct CliSandbox {
    temp: tempfile::TempDir,
}

impl CliSandbox {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(temp.path().join("home"))
            .unwrap();
        Self { temp }
    }

    /// Wurzel des Wegwerfbaums: Quelle, Ziel, Auditdateien und künstlicher
    /// Benutzerordner liegen darunter.
    fn path(&self) -> &Path {
        self.temp.path()
    }

    /// Künstlicher Benutzerordner für `HOME`.
    fn home(&self) -> PathBuf {
        // macOS stellt den Temp-Baum oft über `/var` bereit, das selbst ein
        // Symlink auf `/private/var` ist. Der Papierkorb-Core folgt absichtlich
        // keinen Elternsymlinks; die Testumgebung reicht deshalb wie ein echter
        // Benutzerordner den kanonischen absoluten Pfad weiter.
        std::fs::canonicalize(self.temp.path())
            .unwrap()
            .join("home")
    }

    /// Isolierter Konfigurationsordner: Queue-Journal, Profile und Operations-Audit.
    fn config_dir(&self) -> PathBuf {
        std::fs::canonicalize(self.temp.path())
            .unwrap()
            .join("cli-config")
    }

    /// Einziger Ort dieser Datei, an dem ein CLI-Prozess entsteht — hier noch ohne
    /// `--config-dir`. Nur `cli()` und der Gegentest, der die Abweisung belegt, dürfen
    /// diese Funktion benutzen.
    fn command_without_config_dir(&self) -> Command {
        let mut command = Command::new(binary());
        command
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("XDG_STATE_HOME", self.home().join(".local/state"))
            .env("XDG_DATA_HOME", self.home().join(".local/share"))
            .env("DOPPELDECKER_NO_DEFAULT_USER_PATHS", "1");
        command
    }

    /// Gewöhnlicher CLI-Aufruf mit isoliertem Konfigurationsordner.
    fn cli(&self) -> Command {
        let mut command = self.command_without_config_dir();
        command.arg("--config-dir").arg(self.config_dir());
        command
    }

    /// Wie `cli()`, zusätzlich mit maschinenlesbaren JSON-Lines auf stdout.
    fn json_cli(&self) -> Command {
        let mut command = self.cli();
        command.arg("--json");
        command
    }
}

#[test]
fn jeder_cli_start_bekommt_einen_isolierten_konfigurationsordner() {
    // Diese Datei prüft ihren eigenen Quelltext. Die Suchtexte werden aus zwei Stücken
    // zusammengesetzt, damit der Test nicht seine eigenen Zeilen als Treffer zählt.
    let source = include_str!("cli_tests.rs");
    let process_start = concat!("Command", "::new(");
    let lines: Vec<&str> = source.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(process_start))
        .map(|(index, _)| index + 1)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "Ein CLI-Prozess darf nur in CliSandbox entstehen, gefunden in Zeile(n) {hits:?}"
    );

    // Der eine erlaubte Startpunkt muss die Umgebung tatsächlich umlenken.
    let start = hits[0] - 1;
    let window = lines[start..(start + 15).min(lines.len())].join("\n");
    assert!(
        window.contains("DOPPELDECKER_NO_DEFAULT_USER_PATHS"),
        "Der CLI-Start muss den Rückfall auf die echten Nutzerpfade verbieten"
    );
    assert!(
        window.contains("\"HOME\""),
        "Der CLI-Start muss HOME in den Wegwerfbaum umlenken"
    );

    // Genau drei Vorkommen: Definition, Aufruf in `cli()` und der Gegentest.
    let raw_name = concat!("command_without", "_config_dir");
    assert_eq!(
        source.matches(raw_name).count(),
        3,
        "Nur cli() und der Gegentest dürfen einen Aufruf ohne Konfigurationsordner bauen"
    );

    // Eine später hinzugefügte Testdatei darf keinen eigenen Prozessstart mitbringen.
    // Sie hätte weder die Sandbox noch deren Umleitungen und würde damit wieder am
    // echten Konfigurationsordner landen.
    //
    // Der Scan läuft **rekursiv** über `tests/` und `src/`. Genau der Ort, den der
    // Kommentar hier früher empfahl — ein geteiltes `tests/common/mod.rs` —, lag
    // vorher im toten Winkel: `read_dir` liefert das Verzeichnis `common` als
    // Eintrag ohne Dateiendung, und der Endungsfilter übersprang es samt Inhalt.
    // Unit-Tests unter `src/` fielen aus demselben Grund gar nicht erst auf.
    fn sammle_rust_dateien(verzeichnis: &Path, ziel: &mut Vec<PathBuf>) {
        let Ok(eintraege) = std::fs::read_dir(verzeichnis) else {
            return;
        };
        for eintrag in eintraege {
            let pfad = eintrag.unwrap().path();
            if pfad.is_dir() {
                sammle_rust_dateien(&pfad, ziel);
            } else if pfad.extension().is_some_and(|kind| kind == "rs") {
                ziel.push(pfad);
            }
        }
    }

    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut dateien = Vec::new();
    sammle_rust_dateien(&crate_dir.join("tests"), &mut dateien);
    sammle_rust_dateien(&crate_dir.join("src"), &mut dateien);
    let eigene = crate_dir.join("tests").join("cli_tests.rs");
    for pfad in dateien {
        if pfad == eigene {
            continue;
        }
        let other = std::fs::read_to_string(&pfad).unwrap();
        assert!(
            !other.contains(process_start),
            "{} startet einen CLI-Prozess an CliSandbox vorbei",
            pfad.display()
        );
    }
}

#[test]
fn fehlender_konfigurationsordner_wird_abgewiesen() {
    let sandbox = CliSandbox::new();
    let root = sandbox.path().join("root");
    std::fs::create_dir(&root).unwrap();

    // `capabilities` würde den Queue-Prozessbesitz im Konfigurationsordner nehmen.
    // Fehlt der Ordner, bricht die CLI vor jeder Wirkung maschinenlesbar ab.
    let output = sandbox
        .command_without_config_dir()
        .args(["--json", "capabilities", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "error");
    assert_eq!(event["code"], "invalid_input");
    // Weder ein Konfigurationsordner noch irgendetwas im Benutzerordner ist entstanden.
    assert!(!sandbox.config_dir().exists());
    assert!(std::fs::read_dir(sandbox.home()).unwrap().next().is_none());
}

#[test]
fn json_plan_ist_reiner_dry_run() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("neu.txt"), b"inhalt").unwrap();
    std::fs::write(source.join("ausgelassen.tmp"), b"unsichtbar").unwrap();

    let output = sandbox
        .json_cli()
        .args(["plan", "--source"])
        .arg(&source)
        .arg("--target")
        .arg(&target)
        .args(["--exclude", "*.tmp", "--exclude", ".DS_Store"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "plan");
    assert_eq!(event["plan"]["copy_count"], 1);
    assert_eq!(
        event["options"]["exclude"],
        serde_json::json!(["*.tmp", ".DS_Store"])
    );
    assert!(event["plan"]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|action| action["path"] != "ausgelassen.tmp"));
    assert!(!target.join("neu.txt").exists());
    assert!(!target.join("ausgelassen.tmp").exists());
}

#[test]
fn lokaler_papierkorb_nutzt_den_core_vertrag_und_isolierte_nutzerpfade() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("papierkorb.txt");
    std::fs::write(&source, b"wiederherstellbar").unwrap();

    let output = sandbox
        .json_cli()
        .args(["trash", "--item"])
        .arg(&source)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "trash_result");
    assert_eq!(event["stats"]["trashed"], 1);
    assert_eq!(event["stats"]["failed"], 0);
    assert!(!source.exists());

    #[cfg(target_os = "macos")]
    let trashed = sandbox.home().join(".Trash/papierkorb.txt");
    #[cfg(target_os = "linux")]
    let trashed = sandbox
        .home()
        .join(".local/share/Trash/files/papierkorb.txt");
    assert_eq!(std::fs::read(trashed).unwrap(), b"wiederherstellbar");
    let journals = sandbox.config_dir().join("trash-journals");
    assert!(journals.is_dir());
    assert!(std::fs::read_dir(journals).unwrap().next().is_none());
}

#[test]
fn papierkorb_journalfehler_erhaelt_den_unklaren_core_code() {
    use std::os::unix::process::CommandExt;

    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("papierkorb.txt");
    std::fs::write(&source, b"unveraendert").unwrap();
    let mut command = sandbox.json_cli();
    command.args(["trash", "--item"]).arg(&source);
    // Nur das Kind begrenzen: Die kleine Queue passt, das v2-Papierkorbjournal
    // scheitert beim echten Write. Kein Claim darf danach beginnen.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGXFSZ, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            let limit = libc::rlimit {
                rlim_cur: 1024,
                rlim_max: 1024,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "error");
    assert_eq!(event["code"], "outcome_unclear");
    assert_eq!(std::fs::read(&source).unwrap(), b"unveraendert");
    assert!(event["message"]
        .as_str()
        .unwrap()
        .contains("trash-journals"));
}

#[test]
fn ungueltiges_cli_ausschlussmuster_scheitert_vor_der_pfadaufloesung() {
    let sandbox = CliSandbox::new();
    let output = sandbox
        .json_cli()
        .args([
            "plan",
            "--source",
            "fehlt-quelle",
            "--target",
            "fehlt-ziel",
            "--exclude",
            "ordner/**rest",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "error");
    assert_eq!(event["code"], "invalid_input");
    assert!(event["message"]
        .as_str()
        .unwrap()
        .contains("Ausschlussmuster"));
}

#[test]
fn real_add_only_bleibt_im_selben_prozess_und_schreibt_audit() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    let audit = sandbox.path().join("audit.jsonl");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("neu.txt"), b"inhalt").unwrap();
    std::fs::write(source.join("lokal.tmp"), b"nicht kopieren").unwrap();

    let (status, events, stderr) = run_real(
        &sandbox,
        &source,
        &target,
        &audit,
        &["--mode", "add-only", "--exclude", "*.tmp"],
        |preview| format!("{}\n", preview["confirmation_phrase"].as_str().unwrap()),
    );
    assert!(status.success(), "{stderr}");
    assert!(events.iter().any(|event| event["event"] == "real_preview"));
    assert!(events.iter().any(|event| event["event"] == "result"));
    assert_eq!(std::fs::read(target.join("neu.txt")).unwrap(), b"inhalt");
    assert!(!target.join("lokal.tmp").exists());
    let audit_text = std::fs::read_to_string(audit).unwrap();
    assert_eq!(audit_text.lines().count(), 2);
    assert!(!audit_text.contains("neu.txt"));
}

#[test]
fn provider_sync_cli_nutzt_den_geteilten_realmodusvertrag() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    let audit = sandbox.path().join("provider-audit.jsonl");
    std::fs::create_dir_all(source.join("unterordner")).unwrap();
    std::fs::create_dir_all(source.join("cache")).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("unterordner/datei.txt"), b"provider-sync").unwrap();
    std::fs::write(source.join("cache/quelle.bin"), b"nicht kopieren").unwrap();
    std::fs::create_dir_all(target.join("cache")).unwrap();
    std::fs::write(target.join("cache/ziel.bin"), b"behalten").unwrap();

    let (status, events, stderr) = run_provider_sync(
        &sandbox,
        &source,
        &target,
        &audit,
        &["--exclude", "cache"],
        |preview| format!("{}\n", preview["confirmation_phrase"].as_str().unwrap()),
    );
    assert!(status.success(), "{stderr}");
    assert_eq!(status.code(), Some(0));
    let event_names = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        &event_names[..2],
        ["provider_sync_preparing", "provider_sync_preview"]
    );
    assert_eq!(event_names.last(), Some(&"provider_sync_result"));
    assert!(event_names[2..event_names.len() - 1]
        .iter()
        .all(|event| *event == "progress"));
    assert!(event_names.len() >= 4);
    assert_eq!(
        std::fs::read(target.join("unterordner/datei.txt")).unwrap(),
        b"provider-sync"
    );
    assert!(!target.join("cache/quelle.bin").exists());
    assert_eq!(
        std::fs::read(target.join("cache/ziel.bin")).unwrap(),
        b"behalten"
    );
    let audit_text = std::fs::read_to_string(audit).unwrap();
    assert_eq!(audit_text.lines().count(), 2);
    assert!(!audit_text.contains("datei.txt"));
}

#[test]
fn falsche_phrase_mutiert_nichts() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("neu.txt"), b"inhalt").unwrap();

    let (status, events, _) = run_real(
        &sandbox,
        &source,
        &target,
        &sandbox.path().join("audit.jsonl"),
        &[],
        |_| "REAL falsch ABC123\n".into(),
    );
    assert!(!status.success());
    assert!(events.iter().any(|event| event["event"] == "error"));
    assert_eq!(
        events
            .iter()
            .find(|event| event["event"] == "error")
            .unwrap()["code"],
        "confirmation_mismatch"
    );
    assert!(!target.join("neu.txt").exists());
}

#[test]
fn provider_sync_validiert_beide_roots_vor_queue_und_profilauflosung() {
    for (source_root, target_root) in [
        (PathBuf::from("relativ/quelle"), PathBuf::from("/ziel")),
        (PathBuf::from("/quelle"), PathBuf::from("relativ/ziel")),
    ] {
        let sandbox = CliSandbox::new();
        let output = sandbox
            .json_cli()
            .args([
                "sync",
                "--source-provider",
                "sftp",
                "--source-profile",
                "fehlt",
                "--source-root",
            ])
            .arg(&source_root)
            .arg("--target-root")
            .arg(&target_root)
            .output()
            .unwrap();

        assert!(!output.status.success());
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["code"], "invalid_input");
        assert!(
            !sandbox.config_dir().exists(),
            "ungültige Roots dürfen das Queue-/Profilverzeichnis nicht anlegen"
        );
    }
}

#[test]
fn providerbefehle_validieren_alle_endpunkte_vor_dem_queuejournal() {
    for command in ["sync", "copy", "move", "rename", "delete"] {
        let sandbox = CliSandbox::new();
        let source = std::fs::canonicalize(sandbox.path())
            .unwrap()
            .join("quelle");
        let target = std::fs::canonicalize(sandbox.path()).unwrap().join("ziel");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let item = source.join("datei.txt");
        std::fs::write(&item, b"inhalt").unwrap();
        let mut invocation = sandbox.json_cli();
        match command {
            "sync" => {
                invocation
                    .args(["sync", "--source-profile", "unzulässig", "--source-root"])
                    .arg(&source)
                    .arg("--target-root")
                    .arg(&target);
            }
            "copy" | "move" => {
                invocation
                    .args([command, "--source-profile", "unzulässig", "--source-root"])
                    .arg(&source)
                    .arg("--target-root")
                    .arg(&target)
                    .arg("--item")
                    .arg(&item);
            }
            "rename" => {
                invocation
                    .args(["rename", "--profile", "unzulässig", "--root"])
                    .arg(&source)
                    .arg("--path")
                    .arg(&item)
                    .args(["--new-name", "neu.txt"]);
            }
            "delete" => {
                invocation
                    .args(["delete", "--provider", "sftp", "--root"])
                    .arg(&source)
                    .arg("--item")
                    .arg(&item)
                    .args(["--mode", "permanent"]);
            }
            _ => unreachable!(),
        }
        let output = invocation.output().unwrap();

        assert!(!output.status.success(), "{command}");
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["code"], "invalid_input", "{command}: {event}");
        assert!(
            !sandbox.config_dir().exists(),
            "Endpunktfehler darf kein Queuejournal anlegen: {command}"
        );
    }
}

#[cfg(unix)]
#[test]
fn provider_sync_validiert_den_auditpfad_vor_queue_und_providerwirkung() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = CliSandbox::new();
    let audit_directory = sandbox.path().join("audit-als-ordner");
    let readonly_parent = sandbox.path().join("audit-nicht-schreibbar");
    std::fs::create_dir(&audit_directory).unwrap();
    std::fs::create_dir(&readonly_parent).unwrap();
    std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o500)).unwrap();
    let cases = [
        PathBuf::from("relatives-audit.jsonl"),
        audit_directory,
        readonly_parent.join("audit.jsonl"),
    ];

    for audit in cases {
        let output = sandbox
            .json_cli()
            .args([
                "sync",
                "--source-provider",
                "sftp",
                "--source-profile",
                "fehlt",
                "--source-root",
                "/quelle",
                "--target-provider",
                "sftp",
                "--target-profile",
                "fehlt-auch",
                "--target-root",
                "/ziel",
                "--audit",
            ])
            .arg(&audit)
            .output()
            .unwrap();

        assert!(!output.status.success(), "{}", audit.display());
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            event["code"],
            "invalid_input",
            "{}: {event}",
            audit.display()
        );
        assert!(
            !sandbox.config_dir().exists(),
            "Auditfehler darf kein Queuejournal oder Profilzugriff auslösen"
        );
    }

    std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn provider_sync_meldet_bestaetigungsfehler_ohne_mutation_oder_audit() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    let audit = sandbox.path().join("provider-audit.jsonl");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("neu.txt"), b"inhalt").unwrap();

    let (status, events, _) = run_provider_sync(&sandbox, &source, &target, &audit, &[], |_| {
        "falsch\n".into()
    });
    assert_eq!(status.code(), Some(2));
    assert_eq!(
        events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["provider_sync_preparing", "provider_sync_preview", "error"]
    );
    let error = events
        .iter()
        .find(|event| event["event"] == "error")
        .unwrap();
    assert_eq!(error["code"], "confirmation_mismatch");
    assert!(!target.join("neu.txt").exists());
    assert!(!audit.exists());
}

#[test]
fn provider_sync_meldet_falsches_backup_gate_ohne_ersetzung() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    let audit = sandbox.path().join("provider-audit.jsonl");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("datei.txt"), b"neu").unwrap();
    std::fs::write(target.join("datei.txt"), b"alt").unwrap();

    let (status, events, _) = run_provider_sync(
        &sandbox,
        &source,
        &target,
        &audit,
        &["--mode", "update", "--compare", "content"],
        |preview| {
            assert_eq!(preview["backup_confirmation_required"], true);
            format!(
                "{}\nNICHT BESTÄTIGT\n",
                preview["confirmation_phrase"].as_str().unwrap()
            )
        },
    );
    assert_eq!(status.code(), Some(2));
    assert_eq!(
        events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["provider_sync_preparing", "provider_sync_preview", "error"]
    );
    let error = events
        .iter()
        .find(|event| event["event"] == "error")
        .unwrap();
    assert_eq!(error["code"], "confirmation_mismatch");
    assert_eq!(std::fs::read(target.join("datei.txt")).unwrap(), b"alt");
    assert!(!audit.exists());
}

#[test]
fn provider_sync_meldet_listing_drift_als_neuplanungssignal() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    let audit = sandbox.path().join("provider-audit.jsonl");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("vorher.txt"), b"vorher").unwrap();

    let (status, events, _) =
        run_provider_sync(&sandbox, &source, &target, &audit, &[], |preview| {
            std::fs::write(source.join("spaeter.txt"), b"spaeter").unwrap();
            format!("{}\n", preview["confirmation_phrase"].as_str().unwrap())
        });
    assert_eq!(status.code(), Some(2));
    assert_eq!(
        events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["provider_sync_preparing", "provider_sync_preview", "error"]
    );
    let error = events
        .iter()
        .find(|event| event["event"] == "error")
        .unwrap();
    assert_eq!(error["code"], "plan_drift");
    assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    assert!(!audit.exists());
}

#[test]
fn update_backup_gate_und_cli_quarantaene_restore_funktionieren() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("datei"), b"neu").unwrap();
    std::fs::write(target.join("datei"), b"alt").unwrap();
    filetime::set_file_mtime(
        source.join("datei"),
        filetime::FileTime::from_unix_time(20, 0),
    )
    .unwrap();
    filetime::set_file_mtime(
        target.join("datei"),
        filetime::FileTime::from_unix_time(10, 0),
    )
    .unwrap();

    let (status, _, stderr) = run_real(
        &sandbox,
        &source,
        &target,
        &sandbox.path().join("audit.jsonl"),
        &["--mode", "update", "--compare", "content"],
        |preview| {
            assert_eq!(preview["backup_confirmation_required"], true);
            format!(
                "{}\nBACKUP UND RESTORE BESTÄTIGT\n",
                preview["confirmation_phrase"].as_str().unwrap()
            )
        },
    );
    assert!(status.success(), "{stderr}");
    assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"neu");

    let list = sandbox
        .json_cli()
        .args(["quarantine", "list", "--target"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(list.status.success());
    let event: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let run = event["runs"][0]["id"].as_str().unwrap();
    let entries = sandbox
        .cli()
        .args(["quarantine", "entries", "--target"])
        .arg(&target)
        .args(["--run", run])
        .output()
        .unwrap();
    assert!(entries.status.success());
    let entries_text = String::from_utf8(entries.stdout).unwrap();
    assert!(entries_text.contains("datei"));
    assert!(entries_text.contains("physisch vorhanden · automatisch wiederherstellbar"));

    // Restore überschreibt nicht. Die neue Version wird deshalb für diesen Test nur
    // unter einem anderen Namen aufbewahrt.
    std::fs::rename(target.join("datei"), target.join("neue-version")).unwrap();
    let phrase = format!("RESTORE {run} datei\n");
    let mut child = sandbox
        .json_cli()
        .args(["quarantine", "restore", "--target"])
        .arg(&target)
        .args(["--run", run, "--path", "datei"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(phrase.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let restored_event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(restored_event["outcome"], "restored");
    assert_eq!(std::fs::read(target.join("datei")).unwrap(), b"alt");

    // Auch das endgültige Leeren braucht eine eigene exakte Bestätigung.
    let phrase = format!("LEEREN {run}\n");
    let mut child = sandbox
        .json_cli()
        .args(["quarantine", "purge", "--target"])
        .arg(&target)
        .args(["--run", run])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(phrase.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "purged");
    assert_eq!(event["contents_empty"], true);
    assert_eq!(event["recovery_tombstone_visible"], true);

    let list = sandbox
        .cli()
        .args(["quarantine", "list", "--target"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("Purged"));
}

#[test]
fn quarantine_text_macht_version_zwei_als_nur_manuell_rettbar_sichtbar() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(source.join("datei"), b"neu").unwrap();
    std::fs::write(target.join("datei"), b"alt").unwrap();
    filetime::set_file_mtime(
        source.join("datei"),
        filetime::FileTime::from_unix_time(20, 0),
    )
    .unwrap();
    filetime::set_file_mtime(
        target.join("datei"),
        filetime::FileTime::from_unix_time(10, 0),
    )
    .unwrap();
    let (status, _, stderr) = run_real(
        &sandbox,
        &source,
        &target,
        &sandbox.path().join("audit.jsonl"),
        &["--mode", "update", "--compare", "content"],
        |preview| {
            format!(
                "{}\nBACKUP UND RESTORE BESTÄTIGT\n",
                preview["confirmation_phrase"].as_str().unwrap()
            )
        },
    );
    assert!(status.success(), "{stderr}");
    let listed = sandbox
        .json_cli()
        .args(["quarantine", "list", "--target"])
        .arg(&target)
        .output()
        .unwrap();
    let event: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let run = event["runs"][0]["id"].as_str().unwrap().to_owned();
    let sidecar = target
        .parent()
        .unwrap()
        .join(".doppeldecker-quarantine")
        .join(format!("{run}.json"));
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    manifest["version"] = 2.into();
    for record in manifest["records"].as_array_mut().unwrap() {
        let record = record.as_object_mut().unwrap();
        record.remove("source_identity");
        record.remove("source_digest");
        record.remove("source_digest_version");
    }
    std::fs::write(&sidecar, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let list = sandbox
        .cli()
        .args(["quarantine", "list", "--target"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(list.status.success());
    let list_text = String::from_utf8(list.stdout).unwrap();
    assert!(list_text.contains("1/1 physisch vorhanden"));
    assert!(list_text.contains("0 automatisch wiederherstellbar"));
    assert!(list_text.contains("1 nur manuell rettbar"));

    let entries = sandbox
        .cli()
        .args(["quarantine", "entries", "--target"])
        .arg(&target)
        .args(["--run", &run])
        .output()
        .unwrap();
    assert!(entries.status.success());
    assert!(String::from_utf8(entries.stdout)
        .unwrap()
        .contains("physisch vorhanden · nur manuell rettbar"));
}

#[cfg(unix)]
#[test]
fn sigint_nutzt_den_kontrollierten_abbruchpfad() {
    use std::os::unix::process::ExitStatusExt;

    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    let audit = sandbox.path().join("audit.jsonl");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    // Eine große, künstliche Datei hält die Ausführung lange genug offen, damit der
    // Test das Signal nach dem ersten Fortschrittsereignis zustellen kann.
    let file = std::fs::File::create(source.join("abbruch.bin")).unwrap();
    file.set_len(512 * 1024 * 1024).unwrap();

    let mut child = sandbox
        .json_cli()
        .args(["real", "--source"])
        .arg(&source)
        .arg("--target")
        .arg(&target)
        .arg("--audit")
        .arg(&audit)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut preview = None;
    while preview.is_none() {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "real_preview" {
            preview = Some(event["preview"].clone());
        }
    }
    let phrase = format!(
        "{}\n",
        preview.unwrap()["confirmation_phrase"].as_str().unwrap()
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(phrase.as_bytes()).unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let mut events = Vec::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        let is_progress = event["event"] == "progress";
        events.push(event);
        if is_progress {
            break;
        }
    }
    // SAFETY: Die PID gehört dem gerade gestarteten Kindprozess; SIGINT verändert
    // ausschließlich dessen Abbruchflag über den installierten Handler.
    let signal_result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(signal_result, 0);

    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    for line in rest.lines().filter(|line| !line.is_empty()) {
        events.push(serde_json::from_str(line).unwrap());
    }
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(130),
        "Signalstatus: {:?}",
        status.signal()
    );
    assert!(events
        .iter()
        .any(|event| { event["event"] == "result" && event["stats"]["cancelled"] == true }));
    assert!(!target.join("abbruch.bin").exists());
    let audit_text = std::fs::read_to_string(audit).unwrap();
    assert!(audit_text.contains("\"status\":\"cancelled\""));
}

#[test]
fn bidirektionaler_plan_verwirft_mutationsloses_laufjournal() {
    let sandbox = CliSandbox::new();
    let left = sandbox.path().join("links");
    let right = sandbox.path().join("rechts");
    let baseline = sandbox.path().join("state/paar.json");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    let output = sandbox
        .json_cli()
        .args([
            "bidirectional",
            "plan",
            "--pair-id",
            "11111111111111111111111111111111",
            "--pair-name",
            "CLI-Paar",
            "--left",
        ])
        .arg(&left)
        .arg("--right")
        .arg(&right)
        .arg("--baseline")
        .arg(&baseline)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "bidirectional_preview");
    assert!(!baseline.exists());
    assert!(!baseline.with_extension("run.json").exists());
}

#[test]
fn bidirektionaler_cli_lauf_haelt_phrase_und_handles_im_selben_prozess() {
    let sandbox = CliSandbox::new();
    let left = sandbox.path().join("links");
    let right = sandbox.path().join("rechts");
    let baseline = sandbox.path().join("state/paar.json");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();

    let (status, events, stderr) =
        run_bidirectional(&sandbox, &left, &right, &baseline, |preview| {
            format!("{}\n", preview["confirmation_phrase"].as_str().unwrap())
        });

    assert!(status.success(), "{stderr}");
    assert!(events
        .iter()
        .any(|event| event["event"] == "bidirectional_result"));
    assert!(baseline.exists());
    assert!(!baseline.with_extension("run.json").exists());
}

#[test]
fn bidirektionaler_cli_resume_entfernt_das_abgeschlossene_laufjournal_vor_der_ausgabe() {
    let sandbox = CliSandbox::new();
    let left = sandbox.path().join("links");
    let right = sandbox.path().join("rechts");
    let baseline = sandbox.path().join("state/resume-paar.json");
    create_interrupted_bidirectional_run(&left, &right, &baseline, 0x33);

    let (status, events, stderr) =
        run_bidirectional_recovery(&sandbox, "resume", &left, &right, &baseline);

    assert!(status.success(), "{stderr}");
    assert!(events
        .iter()
        .any(|event| event["event"] == "bidirectional_result"));
    assert!(!baseline.with_extension("run.json").exists());
}

#[test]
fn bidirektionaler_cli_rollback_entfernt_das_abgeschlossene_laufjournal_vor_der_ausgabe() {
    let sandbox = CliSandbox::new();
    let left = sandbox.path().join("links");
    let right = sandbox.path().join("rechts");
    let baseline = sandbox.path().join("state/rollback-paar.json");
    create_interrupted_bidirectional_run(&left, &right, &baseline, 0x34);

    let (status, events, stderr) =
        run_bidirectional_recovery(&sandbox, "rollback", &left, &right, &baseline);

    assert!(status.success(), "{stderr}");
    assert!(events
        .iter()
        .any(|event| event["event"] == "bidirectional_rollback_result"));
    assert!(!baseline.with_extension("run.json").exists());
}

#[test]
fn json_mkdir_legt_ordner_an() {
    let sandbox = CliSandbox::new();
    let parent = sandbox.path().join("eltern");
    std::fs::create_dir_all(&parent).unwrap();

    let output = sandbox
        .json_cli()
        .args(["mkdir", "--parent"])
        .arg(&parent)
        .args(["--name", "neuer ordner"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "mkdir");
    assert_eq!(event["execution_class"], "assured");
    assert_eq!(event["limitations"], serde_json::json!([]));
    let created = parent.join("neuer ordner");
    assert!(created.is_dir());
    // Der gemeldete Pfad zeigt exakt auf den neuen Ordner (Elternpfad kanonisiert).
    assert!(event["path"].as_str().unwrap().ends_with("neuer ordner"));
}

#[test]
fn text_list_benennt_dateien_ordner_und_symlinks() {
    let sandbox = CliSandbox::new();
    let root = sandbox.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("ordner")).unwrap();
    std::fs::write(root.join("alpha.txt"), b"inhalt").unwrap();
    std::os::unix::fs::symlink("alpha.txt", root.join("verweis")).unwrap();
    let output = sandbox
        .cli()
        .args(["list", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    for name in ["alpha.txt", "ordner", "verweis"] {
        assert!(text.contains(name), "{name} fehlt in {text}");
    }
    for kind in ["Datei", "Ordner", "Symlink"] {
        assert!(text.contains(kind), "{kind} fehlt in {text}");
    }
}

#[test]
fn json_list_prueft_den_vollstaendigen_bestand_unabhaengig() {
    let sandbox = CliSandbox::new();
    let root = sandbox.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("ordner")).unwrap();
    std::fs::write(root.join("datei.txt"), b"inhalt").unwrap();

    let output = sandbox
        .json_cli()
        .args(["list", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "listing");
    assert_eq!(event["complete"], true);
    let entries = event["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|listed| {
        listed["entry"]["name"] == "datei.txt" && listed["entry"]["is_file"] == true
    }));
    assert!(entries.iter().any(|listed| {
        listed["entry"]["name"] == "ordner" && listed["entry"]["is_dir"] == true
    }));
}

#[test]
fn mkdir_lehnt_pfadtrenner_im_namen_ab() {
    let sandbox = CliSandbox::new();
    let parent = sandbox.path().join("eltern");
    std::fs::create_dir_all(&parent).unwrap();

    let output = sandbox
        .json_cli()
        .args(["mkdir", "--parent"])
        .arg(&parent)
        .args(["--name", "unten/tiefer"])
        .output()
        .unwrap();
    // Ein Trenner im Namen ist ein ungültiger Name: kein Ausbruch aus dem Elternordner,
    // nichts wird angelegt, und der Fehler kommt maschinenlesbar als Event.
    assert!(!output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "error");
    assert!(!parent.join("unten").exists());

    // Der reine Name wird vor Profilauflösung und jeder Providerprobe geprüft.
    let remote = sandbox
        .json_cli()
        .args([
            "mkdir",
            "--provider",
            "sftp",
            "--profile",
            "nicht-vorhanden",
            "--parent",
            "/",
            "--name",
            "unten/tiefer",
        ])
        .output()
        .unwrap();
    assert!(!remote.status.success());
    let remote_event: serde_json::Value = serde_json::from_slice(&remote.stdout).unwrap();
    assert_eq!(remote_event["code"], "invalid_input");
    assert!(remote_event["message"]
        .as_str()
        .unwrap()
        .contains("Ungültiger Name"));
}

#[test]
fn mkdir_validiert_endpoint_und_root_vor_dem_queuejournal() {
    let cases = [
        (vec!["--profile", "nicht-erlaubt"], false, "invalid_input"),
        (Vec::new(), true, "invalid_input"),
        (vec!["--provider", "sftp"], false, "invalid_input"),
        (
            vec!["--provider", "sftp", "--profile", "nicht-vorhanden"],
            false,
            "unsupported",
        ),
    ];

    for (extra, non_normalized_root, expected_code) in cases {
        let sandbox = CliSandbox::new();
        let parent = sandbox.path().join("eltern");
        std::fs::create_dir(&parent).unwrap();
        let supplied_parent = if non_normalized_root {
            parent.join("unter/..")
        } else {
            parent.clone()
        };
        if non_normalized_root {
            std::fs::create_dir(parent.join("unter")).unwrap();
        }
        let mut command = sandbox.json_cli();
        command
            .args(["mkdir", "--parent"])
            .arg(&supplied_parent)
            .args(["--name", "neu"])
            .args(extra);

        let output = command.output().unwrap();

        assert!(!output.status.success());
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["code"], expected_code, "{event}");
        assert!(!parent.join("neu").exists());
        assert!(
            !sandbox.config_dir().exists(),
            "reine Endpunktfehler dürfen kein Queuejournal anlegen"
        );
    }
}

#[test]
fn capabilities_validiert_endpoint_vor_dem_queuejournal() {
    for (provider, profile, root) in [
        ("local", Some("nicht-erlaubt"), PathBuf::from("/")),
        ("sftp", None, PathBuf::from("/")),
        ("local", None, PathBuf::from("relativ/ungueltig")),
        ("local", None, PathBuf::from("/daten/../fremd")),
    ] {
        let sandbox = CliSandbox::new();
        let mut command = sandbox.json_cli();
        command
            .args(["capabilities", "--provider", provider, "--root"])
            .arg(&root);
        if let Some(profile) = profile {
            command.args(["--profile", profile]);
        }

        let output = command.output().unwrap();

        assert!(!output.status.success());
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["code"], "invalid_input", "{event}");
        assert!(
            !sandbox.config_dir().exists(),
            "ungültige Capability-Endpunkte dürfen kein Queuejournal anlegen"
        );
    }
}

#[test]
fn capabilities_loest_profile_geheimnisfrei_vor_dem_queuejournal_auf() {
    let missing = CliSandbox::new();
    let missing_output = missing
        .json_cli()
        .args([
            "capabilities",
            "--provider",
            "sftp",
            "--profile",
            "fehlt",
            "--root",
            "/",
        ])
        .output()
        .unwrap();
    assert!(!missing_output.status.success());
    let missing_event: serde_json::Value = serde_json::from_slice(&missing_output.stdout).unwrap();
    assert_eq!(missing_event["code"], "not_found");
    assert!(
        !missing.config_dir().exists(),
        "ein fehlendes Profil darf den Konfigurationsordner nicht für die Queue anlegen"
    );

    let invalid = CliSandbox::new();
    std::fs::create_dir_all(invalid.config_dir()).unwrap();
    let profile = doppeldecker_core::SftpProfile {
        id: "kaputt".into(),
        name: "Kaputtes Testprofil".into(),
        connection: doppeldecker_core::SftpConnectionConfig {
            host: "sftp.example.test".into(),
            alternate_hosts: Vec::new(),
            port: 0,
            username: "tester".into(),
            requested_root: "/".into(),
            host_key: doppeldecker_core::SftpHostKeyPin {
                algorithm: "ssh-ed25519".into(),
                sha256: [7; 32],
            },
            authentication: doppeldecker_core::SftpAuthenticationReference::Password {
                secret: doppeldecker_core::KeychainSecretReference::new(
                    "DarfNichtImFehlerStehen",
                    "GeheimesTestkonto",
                )
                .unwrap(),
            },
        },
    };
    std::fs::write(
        invalid.config_dir().join("sftp-profiles.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "profiles": [profile],
        }))
        .unwrap(),
    )
    .unwrap();

    let invalid_output = invalid
        .json_cli()
        .args([
            "capabilities",
            "--provider",
            "sftp",
            "--profile",
            "kaputt",
            "--root",
            "/",
        ])
        .output()
        .unwrap();

    assert!(!invalid_output.status.success());
    let invalid_event: serde_json::Value = serde_json::from_slice(&invalid_output.stdout).unwrap();
    assert_eq!(invalid_event["code"], "invalid_input");
    assert!(!invalid.config_dir().join("operations-queue.json").exists());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&invalid_output.stdout),
        String::from_utf8_lossy(&invalid_output.stderr)
    );
    assert!(!combined.contains("DarfNichtImFehlerStehen"));
    assert!(!combined.contains("GeheimesTestkonto"));
}

#[test]
fn providerfaehiges_copy_und_move_nutzen_queue_und_stdin() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("copy.txt"), b"copy").unwrap();

    let copy = sandbox
        .json_cli()
        .args(["copy", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(source.join("copy.txt"))
        .output()
        .unwrap();
    assert!(
        copy.status.success(),
        "{}",
        String::from_utf8_lossy(&copy.stderr)
    );
    assert_eq!(std::fs::read(target.join("copy.txt")).unwrap(), b"copy");
    let events = json_lines(&copy.stdout);
    assert!(events.iter().any(|event| event["event"] == "file_preview"));
    assert!(events.iter().any(|event| event["event"] == "result"));
    let audit =
        std::fs::read_to_string(sandbox.config_dir().join("operations-audit.jsonl")).unwrap();
    assert!(audit.contains("\"status\":\"started\""));
    assert!(audit.contains("\"status\":\"completed\""));
    let audit_records = json_lines(audit.as_bytes());
    assert_eq!(audit_records.len(), 2);
    for record in audit_records {
        // Die Providerarten stammen aus den frisch verbundenen Endpunkten. Die
        // lokale Gegenprobe bindet zugleich die abgesicherte Ausführungsklasse,
        // ohne einen echten Remote-Server oder Nutzerdaten anzufassen.
        assert_eq!(record["version"], 4);
        assert_eq!(record["source_provider"], "local");
        assert_eq!(record["target_provider"], "local");
        assert_eq!(record["execution_class"], "assured");
    }
    assert!(!audit.contains("copy.txt"));
    assert!(!audit.contains(source.to_string_lossy().as_ref()));

    std::fs::write(source.join("move.txt"), b"move").unwrap();
    let mut child = sandbox
        .json_cli()
        .args(["move", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(source.join("move.txt"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"VERSCHIEBEN local - 1\n")
        .unwrap();
    let moved = child.wait_with_output().unwrap();
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    assert!(!source.join("move.txt").exists());
    assert_eq!(std::fs::read(target.join("move.txt")).unwrap(), b"move");
    let move_events = json_lines(&moved.stdout);
    let move_result = move_events
        .iter()
        .find(|event| event["event"] == "result")
        .expect("Move-Ergebnis fehlt");
    assert_eq!(
        move_result["stats"]["source_removal_best_effort"], true,
        "Der maschinenlesbare Ausgang muss den tatsächlich genutzten Quellabbau ausweisen"
    );
    assert_eq!(move_result["stats"]["target_copy_new_best_effort"], false);
    assert_eq!(move_result["stats"]["target_replace_best_effort"], false);
    let audit_records =
        json_lines(&std::fs::read(sandbox.config_dir().join("operations-audit.jsonl")).unwrap());
    assert_eq!(
        audit_records.last().unwrap()["execution_class"],
        "best_effort",
        "Der abschließende Move-Auditdatensatz darf den Quellabbau nicht als assured ausgeben"
    );
    let move_started = audit_records
        .iter()
        .rev()
        .find(|record| record["status"] == "started")
        .unwrap();
    assert_eq!(move_started["source_removal_best_effort_expected"], true);
    assert_eq!(
        audit_records.last().unwrap()["source_removal_best_effort_actual"],
        true
    );

    std::fs::write(source.join("human.txt"), b"human").unwrap();
    let mut human_child = sandbox
        .cli()
        .args(["move", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(source.join("human.txt"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    human_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"VERSCHIEBEN local - 1\n")
        .unwrap();
    let human = human_child.wait_with_output().unwrap();
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("Best Effort"),
        "Die menschliche Ausgabe muss den schwächeren Quellabbau ausdrücklich nennen"
    );
    assert!(
        String::from_utf8_lossy(&human.stderr).contains("Best Effort"),
        "Die Einschränkung muss schon vor der Bestätigung sichtbar sein"
    );
    assert!(sandbox.config_dir().join("operations-queue.json").is_file());
}

#[cfg(target_os = "linux")]
#[test]
fn additive_ordnerkopie_meldet_rohnamensauslassung_in_json_und_text() {
    use std::os::unix::ffi::OsStringExt;
    for json in [true, false] {
        let sandbox = CliSandbox::new();
        let source = sandbox.path().join("quelle");
        let selected = source.join("ordner");
        let target = sandbox.path().join("ziel");
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(selected.join("sicher.txt"), b"sicher").unwrap();
        let raw = std::ffi::OsString::from_vec(b"latin-\xff.txt".to_vec());
        std::fs::write(selected.join(&raw), b"ausgelassen").unwrap();
        let mut command = if json {
            sandbox.json_cli()
        } else {
            sandbox.cli()
        };
        let output = command
            .args(["copy", "--source-root"])
            .arg(&source)
            .arg("--target-root")
            .arg(&target)
            .arg("--item")
            .arg(&selected)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(target.join("ordner/sicher.txt")).unwrap(),
            b"sicher"
        );
        assert_eq!(std::fs::read(selected.join(&raw)).unwrap(), b"ausgelassen");
        assert_eq!(std::fs::read_dir(target.join("ordner")).unwrap().count(), 1);
        if json {
            let events = json_lines(&output.stdout);
            let result = events.iter().find(|row| row["event"] == "result").unwrap();
            assert_eq!(result["stats"]["files"], 1);
            assert_eq!(result["stats"]["skipped"], 1);
            assert!(result["stats"]["source_read_limitations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|note| note.as_str().unwrap().contains("6c6174696e2dff2e747874")));
        } else {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("1 übersprungen"), "{text}");
            assert!(text.contains("6c6174696e2dff2e747874"), "{text}");
        }
    }
}

#[test]
fn lokaler_cli_move_folgt_dem_finalen_symlink_nicht() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    let real_source = source.join("bestand.txt");
    let selected_link = source.join("auswahl.txt");
    std::fs::write(&real_source, b"bleibt-an-der-quelle").unwrap();
    std::os::unix::fs::symlink("bestand.txt", &selected_link).unwrap();

    let mut child = sandbox
        .json_cli()
        .args(["move", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(&selected_link)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"VERSCHIEBEN local - 1\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&real_source).unwrap(),
        b"bleibt-an-der-quelle"
    );
    assert!(std::fs::symlink_metadata(&selected_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    let events = json_lines(&output.stdout);
    let result = events
        .iter()
        .find(|event| event["event"] == "result")
        .expect("Move-Ergebnis fehlt");
    assert_eq!(result["stats"]["symlinks_skipped"], 1);

    // Die Vorschau nennt die verlangte Phrase maschinenlesbar. Ein Controller im
    // JSON-Modus musste sie sonst aus dem deutschen stderr-Text lesen oder aus den
    // eigenen Argumenten rekonstruieren.
    let preview = events
        .iter()
        .find(|event| event["event"] == "file_preview")
        .expect("Vorschau fehlt");
    assert_eq!(preview["move_confirmation"], "VERSCHIEBEN local - 1");
    assert!(preview["replace_confirmation"].is_null());
    assert!(preview.get("source_profile_id").is_some());
    assert!(preview.get("target_profile_id").is_some());
}

/// Ohne diese Zeile las sich ein Move über einen Symlink als „Fertig: 0 Dateien,
/// 0 Bytes" mit Exitcode 0 — obwohl nichts verschoben wurde. Das JSONL nannte den
/// ausgelassenen Eintrag längst; nur der Textmodus verschwieg ihn. Die GUI weist
/// beide Zahlen aus.
#[test]
fn textbilanz_nennt_ausgelassene_eintraege() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    let real_source = source.join("echt.txt");
    std::fs::write(&real_source, b"bleibt-an-der-quelle").unwrap();
    let selected_link = source.join("verweis.txt");
    std::os::unix::fs::symlink(&real_source, &selected_link).unwrap();

    let mut child = sandbox
        .cli()
        .args(["move", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(&selected_link)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"VERSCHIEBEN local - 1\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("1 Symlink(s) ausgelassen"),
        "Textbilanz nennt den ausgelassenen Symlink nicht: {text}"
    );
    assert!(std::fs::symlink_metadata(&selected_link)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn nicht_claimbare_queue_hinterlaesst_keinen_eigenen_pending_eintrag() {
    // Regression: Ein getrenntes `enqueue` plus Claim konnte zwischen beiden Schritten
    // ein ausführbares `Pending` persistieren. Die CLI nutzt deshalb eine einzige
    // Transaktion, die hier wegen der pausierten Queue überhaupt keinen Eintrag schreibt.
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("copy.txt"), b"copy").unwrap();
    std::fs::create_dir_all(sandbox.config_dir()).unwrap();
    let journal = sandbox.config_dir().join("operations-queue.json");
    std::fs::write(
        &journal,
        serde_json::to_vec(&serde_json::json!({
            "version": doppeldecker_core::QUEUE_JOURNAL_VERSION,
            "paused": true,
            "stop_on_error": false,
            "entries": [],
        }))
        .unwrap(),
    )
    .unwrap();

    let copy = sandbox
        .json_cli()
        .args(["copy", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(source.join("copy.txt"))
        .output()
        .unwrap();

    assert!(
        !copy.status.success(),
        "Eine pausierte Queue darf die Kopie nicht als Erfolg melden"
    );
    let events = json_lines(&copy.stdout);
    assert!(events.iter().any(|event| event["event"] == "error"));
    assert!(!target.join("copy.txt").exists());
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    assert_eq!(
        snapshot["entries"].as_array().unwrap().len(),
        0,
        "Der eigene Eintrag muss entfernt sein, sonst führt der GUI-Worker ihn später aus"
    );
    assert_eq!(snapshot["paused"], true);
}

#[test]
fn lokaler_realmodus_lehnt_offene_queue_vor_jeder_wirkung_ab() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("neu.txt"), b"neu").unwrap();

    let queue =
        doppeldecker_core::OperationQueue::open(sandbox.config_dir().join("operations-queue.json"))
            .unwrap();
    queue
        .enqueue(doppeldecker_core::QueueOperation::File {
            kind: doppeldecker_core::FileQueueKind::Copy,
            items: vec![source.join("neu.txt").to_string_lossy().into_owned()],
            destination: target.to_string_lossy().into_owned(),
            overwrite: false,
            expected_conflicts: Vec::new(),
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            local_replace_best_effort_expected: Some(false),
            local_copy_new_best_effort_expected: None,
        })
        .unwrap();
    drop(queue);

    let audit = sandbox.path().join("real-audit.jsonl");
    let output = sandbox
        .json_cli()
        .args(["real", "--source"])
        .arg(&source)
        .arg("--target")
        .arg(&target)
        .arg("--mode")
        .arg("add-only")
        .arg("--audit")
        .arg(&audit)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let events = json_lines(&output.stdout);
    let error = events
        .iter()
        .find(|event| event["event"] == "error")
        .expect("Fehlerereignis fehlt");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("Operations-Queue"));
    assert!(!target.join("neu.txt").exists());
    assert!(!audit.exists());
}

/// `sync` war der einzige mutierende CLI-Befehl ohne diese Schranke. Er lief an
/// einem wartenden Auftrag vorbei und veränderte genau die Roots und
/// Quarantänestände, die dessen späterer Claim erwartet.
#[test]
fn provider_sync_lehnt_offene_queue_vor_jeder_wirkung_ab() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("neu.txt"), b"neu").unwrap();

    let queue =
        doppeldecker_core::OperationQueue::open(sandbox.config_dir().join("operations-queue.json"))
            .unwrap();
    queue
        .enqueue(doppeldecker_core::QueueOperation::File {
            kind: doppeldecker_core::FileQueueKind::Copy,
            items: vec![source.join("neu.txt").to_string_lossy().into_owned()],
            destination: target.to_string_lossy().into_owned(),
            overwrite: false,
            expected_conflicts: Vec::new(),
            keep_both_targets: Vec::new(),
            expected_conflict_proofs: Vec::new(),
            local_replace_best_effort_expected: Some(false),
            local_copy_new_best_effort_expected: None,
        })
        .unwrap();
    drop(queue);

    let audit = sandbox.path().join("sync-audit.jsonl");
    let output = sandbox
        .json_cli()
        .args(["sync", "--source-provider", "local", "--source-root"])
        .arg(&source)
        .arg("--target-provider")
        .arg("local")
        .arg("--target-root")
        .arg(&target)
        .arg("--mode")
        .arg("add-only")
        .arg("--audit")
        .arg(&audit)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Ein wartender Queue-Eintrag darf vom Provider-Sync nicht überholt werden"
    );
    let events = json_lines(&output.stdout);
    let error = events
        .iter()
        .find(|event| event["event"] == "error")
        .expect("Fehlerereignis fehlt");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("Operations-Queue"));
    assert!(!target.join("neu.txt").exists());
    assert!(!audit.exists());
}

/// Die Auditdatei darf nicht im synchronisierten Baum liegen: `append_audit` öffnet
/// sie je Datensatz neu, und Mirror würde sie im zweiten Lauf als überzähligen
/// Eintrag in die Quarantäne verschieben.
#[test]
fn auditdatei_im_zielbaum_wird_vor_jeder_wirkung_abgelehnt() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("neu.txt"), b"neu").unwrap();

    for (name, root) in [("ziel", &target), ("quelle", &source)] {
        let audit = root.join("audit.jsonl");
        let output = sandbox
            .json_cli()
            .args(["real", "--source"])
            .arg(&source)
            .arg("--target")
            .arg(&target)
            .arg("--mode")
            .arg("add-only")
            .arg("--audit")
            .arg(&audit)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "Auditdatei im {name}-Baum muss abgelehnt werden"
        );
        let events = json_lines(&output.stdout);
        let error = events
            .iter()
            .find(|event| event["event"] == "error")
            .expect("Fehlerereignis fehlt");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("innerhalb eines synchronisierten Roots"),
            "unerwartete Meldung: {}",
            error["message"]
        );
        assert!(!audit.exists());
    }
}

#[test]
fn mutierende_sonderpfade_respektieren_den_gui_cli_prozessbesitz() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    let _owner =
        doppeldecker_core::OperationQueue::open(sandbox.config_dir().join("operations-queue.json"))
            .unwrap();

    let real = sandbox
        .json_cli()
        .args(["real", "--source"])
        .arg(&source)
        .arg("--target")
        .arg(&target)
        .arg("--audit")
        .arg(sandbox.path().join("audit.jsonl"))
        .output()
        .unwrap();
    assert_processbesitz_blockiert(&real);

    let bidirectional = sandbox
        .json_cli()
        .args([
            "bidirectional",
            "plan",
            "--pair-id",
            "11111111111111111111111111111111",
            "--pair-name",
            "testpaar",
            "--left",
        ])
        .arg(&source)
        .arg("--right")
        .arg(&target)
        .arg("--baseline")
        .arg(sandbox.path().join("baseline.json"))
        .output()
        .unwrap();
    assert_processbesitz_blockiert(&bidirectional);

    let restore = sandbox
        .json_cli()
        .args(["quarantine", "restore", "--target"])
        .arg(&target)
        .args(["--run", "nicht-vorhanden", "--path", "datei.txt"])
        .output()
        .unwrap();
    assert_processbesitz_blockiert(&restore);
}

fn assert_processbesitz_blockiert(output: &std::process::Output) {
    assert!(!output.status.success());
    let events = json_lines(&output.stdout);
    let error = events
        .iter()
        .find(|event| event["event"] == "error")
        .expect("Fehlerereignis fehlt");
    assert_eq!(error["code"], "io");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("bereits von einem anderen Doppeldecker-Prozess"));
}

#[test]
fn copy_replace_klassifiziert_lokale_wirkung_sichtbar_als_best_effort() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("datei.txt"), b"neu").unwrap();
    std::fs::write(target.join("datei.txt"), b"alt").unwrap();

    let wrong = sandbox
        .json_cli()
        .args(["copy", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(source.join("datei.txt"))
        .arg("--replace")
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert_eq!(std::fs::read(target.join("datei.txt")).unwrap(), b"alt");

    let mut child = sandbox
        .json_cli()
        .args(["copy", "--source-root"])
        .arg(&source)
        .arg("--target-root")
        .arg(&target)
        .arg("--item")
        .arg(source.join("datei.txt"))
        .arg("--replace")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"DAUERHAFT ERSETZEN local - 1\n").unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let events = json_lines(&output.stdout);
    let result = events
        .iter()
        .find(|event| event["event"] == "result")
        .expect("maschinenlesbares Dateiergebnis fehlt");
    assert_eq!(result["stats"]["target_replace_best_effort"], true);
    assert_eq!(std::fs::read(target.join("datei.txt")).unwrap(), b"neu");
    let recovery = std::fs::read_dir(&target)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".datei.txt.doppeldecker-fileop-tmp-"))
        })
        .collect::<Vec<_>>();
    assert!(recovery.is_empty());
    let queue =
        doppeldecker_core::OperationQueue::open(sandbox.config_dir().join("operations-queue.json"))
            .unwrap();
    assert_eq!(
        queue.snapshot().entries.last().unwrap().state,
        doppeldecker_core::QueueState::Completed
    );
}

#[test]
fn rename_meldet_klasse_und_capabilities_sind_jsonl() {
    let sandbox = CliSandbox::new();
    let root = sandbox.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let old = root.join("alt.txt");
    std::fs::write(&old, b"inhalt").unwrap();
    // Umbenennen ohne Kollision verlangt keine Bestätigungsphrase mehr: Es gibt
    // weder Überschreib- noch Löschrisiko, und der Kern belegt das Ergebnis per
    // frischem Elternlisting plus Readback.
    let renamed = sandbox
        .json_cli()
        .args(["rename", "--root"])
        .arg(&root)
        .arg("--path")
        .arg(&old)
        .args(["--new-name", "neu.txt"])
        .output()
        .unwrap();
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    assert_eq!(std::fs::read(root.join("neu.txt")).unwrap(), b"inhalt");
    let event: serde_json::Value = serde_json::from_slice(&renamed.stdout).unwrap();
    assert_eq!(event["event"], "rename_result");
    assert_eq!(event["outcome"], "renamed");
    assert_eq!(event["class"], "assured");
    assert_eq!(event["verified_after_reconnect"], false);

    // Eine Kollision stoppt sichtbar mit `already_exists`, statt zu ersetzen.
    std::fs::write(root.join("belegt.txt"), b"anders").unwrap();
    let collision = sandbox
        .json_cli()
        .args(["rename", "--root"])
        .arg(&root)
        .arg("--path")
        .arg(root.join("neu.txt"))
        .args(["--new-name", "belegt.txt"])
        .output()
        .unwrap();
    assert!(!collision.status.success());
    let collision_event: serde_json::Value = serde_json::from_slice(&collision.stdout).unwrap();
    assert_eq!(collision_event["event"], "error");
    assert_eq!(collision_event["code"], "already_exists");
    assert_eq!(std::fs::read(root.join("belegt.txt")).unwrap(), b"anders");
    assert_eq!(std::fs::read(root.join("neu.txt")).unwrap(), b"inhalt");

    let status = sandbox
        .json_cli()
        .args(["capabilities", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(status.status.success());
    let event: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(event["event"], "capabilities");
    assert_eq!(event["provider"], "local");
    assert_eq!(event["directory_create"]["available"], true);
    assert_eq!(event["files"]["details"]["copy_new"], true);
    assert_eq!(event["rename"]["available"], true);
    assert_eq!(event["rename"]["details"]["assured"], true);
    assert_eq!(event["interactive_rename"]["kind"], "existing");
}

#[test]
fn rename_validiert_name_und_pfade_vor_der_queueoeffnung() {
    let sandbox = CliSandbox::new();
    let root = sandbox.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let old = root.join("alt.txt");
    std::fs::write(&old, b"inhalt").unwrap();
    // Ein regulärer Eintrag an Stelle des Konfigurationsordners würde jede
    // Queueöffnung scheitern lassen. Der präzisere Namensfehler muss trotzdem
    // zuerst kommen und darf den Eintrag nicht ersetzen.
    std::fs::write(sandbox.config_dir(), b"kein Verzeichnis").unwrap();

    let output = sandbox
        .json_cli()
        .args(["rename", "--root"])
        .arg(&root)
        .arg("--path")
        .arg(&old)
        .args(["--new-name", "../ausbruch"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "error");
    assert_eq!(event["code"], "invalid_input");
    assert!(event["message"]
        .as_str()
        .unwrap()
        .contains("Ungültiger Name"));
    assert_eq!(std::fs::read(&old).unwrap(), b"inhalt");
    assert_eq!(
        std::fs::read(sandbox.config_dir()).unwrap(),
        b"kein Verzeichnis"
    );
}

#[test]
fn dateibefehle_validieren_pfade_vor_queue_profil_und_netzwerk() {
    let sandbox = CliSandbox::new();
    for command in ["copy", "move"] {
        let output = sandbox
            .json_cli()
            .args([
                command,
                "--source-provider",
                "sftp",
                "--source-profile",
                "fehlt",
                "--source-root",
                "/remote",
                "--target-provider",
                "webdav",
                "--target-profile",
                "fehlt",
                "--target-root",
                "/ziel",
                "--item",
                "relativ.txt",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{command}");
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["code"], "invalid_input", "{command}: {event}");
        assert!(event["message"]
            .as_str()
            .unwrap()
            .contains("absoluten Pfad"));
    }

    for command in ["copy", "move"] {
        let output = sandbox
            .json_cli()
            .args([
                command,
                "--source-provider",
                "sftp",
                "--source-profile",
                "fehlt",
                "--source-root",
                "~",
                "--target-root",
                "/ziel",
                "--item",
                "~/datei.txt",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{command}");
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["code"], "invalid_input", "{command}: {event}");
    }

    let rename = sandbox
        .json_cli()
        .args([
            "rename",
            "--provider",
            "webdav",
            "--profile",
            "fehlt",
            "--root",
            "~",
            "--path",
            "~/datei.txt",
            "--new-name",
            "neu.txt",
        ])
        .output()
        .unwrap();
    assert!(!rename.status.success());
    let event: serde_json::Value = serde_json::from_slice(&rename.stdout).unwrap();
    assert_eq!(event["code"], "invalid_input");

    let delete = sandbox
        .json_cli()
        .args([
            "delete",
            "--provider",
            "ftps",
            "--profile",
            "fehlt",
            "--root",
            "/remote",
            "--item",
            "relativ.txt",
            "--mode",
            "permanent",
        ])
        .output()
        .unwrap();
    assert!(!delete.status.success());
    let event: serde_json::Value = serde_json::from_slice(&delete.stdout).unwrap();
    assert_eq!(event["code"], "invalid_input");
    assert!(event["message"]
        .as_str()
        .unwrap()
        .contains("absoluten Pfad"));

    let delete_home = sandbox
        .json_cli()
        .args([
            "delete",
            "--provider",
            "ftps",
            "--profile",
            "fehlt",
            "--root",
            "~",
            "--item",
            "~/datei.txt",
            "--mode",
            "permanent",
        ])
        .output()
        .unwrap();
    assert!(!delete_home.status.success());
    let event: serde_json::Value = serde_json::from_slice(&delete_home.stdout).unwrap();
    assert_eq!(event["code"], "invalid_input");
    assert!(
        !sandbox.config_dir().exists(),
        "reine Eingabefehler dürfen noch kein Queuejournal anlegen"
    );
}

#[test]
fn sftp_rename_meldet_die_konkrete_sperre_vor_queue_und_profilzugriff() {
    let sandbox = CliSandbox::new();
    std::fs::write(sandbox.config_dir(), b"kein Verzeichnis").unwrap();
    let output = sandbox
        .json_cli()
        .args([
            "rename",
            "--provider",
            "sftp",
            "--profile",
            "fehlt",
            "--root",
            "/pane",
            "--path",
            "/pane/Alt",
            "--new-name",
            "Neu",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["code"], "unsupported");
    assert!(event["message"]
        .as_str()
        .unwrap()
        .contains("Root-/Elternordner"));
    assert_eq!(
        std::fs::read(sandbox.config_dir()).unwrap(),
        b"kein Verzeichnis"
    );
}

#[test]
fn remote_cli_akzeptiert_keine_secrets_und_meldet_fehlendes_profil() {
    let sandbox = CliSandbox::new();
    let output = sandbox
        .json_cli()
        .args([
            "capabilities",
            "--provider",
            "sftp",
            "--profile",
            "nicht-da",
            "--root",
            "/",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let event: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["event"], "error");
    assert!(!text.to_lowercase().contains("password"));
    assert!(!text.to_lowercase().contains("secret"));

    // Auch der neue Sync-Binärpfad muss die Remote-Profilauflösung tatsächlich
    // durchlaufen und vor Vorschau, Audit oder Zielmutation scheitern.
    let target = sandbox.path().join("ziel");
    std::fs::create_dir(&target).unwrap();
    let sync = sandbox
        .json_cli()
        .args([
            "sync",
            "--source-provider",
            "sftp",
            "--source-profile",
            "nicht-da",
            "--source-root",
            "/",
            "--target-root",
        ])
        .arg(&target)
        .output()
        .unwrap();
    assert!(!sync.status.success());
    let sync_event: serde_json::Value = serde_json::from_slice(&sync.stdout).unwrap();
    assert_eq!(sync_event["event"], "error");
    assert_eq!(sync_event["code"], "not_found");
    assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    assert!(!sandbox
        .config_dir()
        .join("provider-real-runs.jsonl")
        .exists());
}

#[test]
fn verbindungsimport_leakt_bei_fehler_keine_stdin_werte() {
    let sandbox = CliSandbox::new();
    let mut child = sandbox
        .json_cli()
        .args(["connections", "import"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"pw: GEHEIMMARKER\nuser: NUTZERMARKER\nurl: HOSTMARKER.example.test\nUNERWARTETMARKER\n",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let event: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(event["event"], "error");
    for marker in [
        "GEHEIMMARKER",
        "NUTZERMARKER",
        "HOSTMARKER",
        "UNERWARTETMARKER",
    ] {
        assert!(!stdout.contains(marker));
        assert!(!stderr.contains(marker));
    }
    assert!(!sandbox.config_dir().join("sftp-profiles.json").exists());
    assert!(!sandbox.config_dir().join("ftps-profiles.json").exists());
}

#[test]
fn vierzeiliger_pass_import_meldet_nur_feste_pruefstufen() {
    let sandbox = CliSandbox::new();
    let mut child = sandbox
        .cli()
        .args(["connections", "import"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"GEHEIMMARKER\nuser: NUTZERMARKER\nurl: 127.0.0.1:1\nurl ipv6: [::1]:1\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.is_empty());
    assert!(stderr.contains("SFTP: Dienst nicht erreichbar"));
    assert!(stderr.contains("FTPS: explizites TLS nicht erreichbar"));
    for marker in ["GEHEIMMARKER", "NUTZERMARKER", "127.0.0.1", "::1"] {
        assert!(!stderr.contains(marker));
    }
    assert!(!sandbox.config_dir().join("sftp-profiles.json").exists());
    assert!(!sandbox.config_dir().join("ftps-profiles.json").exists());
}

fn json_lines(bytes: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn run_real(
    sandbox: &CliSandbox,
    source: &Path,
    target: &Path,
    audit: &Path,
    extra: &[&str],
    response: impl FnOnce(&serde_json::Value) -> String,
) -> (std::process::ExitStatus, Vec<serde_json::Value>, String) {
    let mut command = sandbox.json_cli();
    command
        .args(["real", "--source"])
        .arg(source)
        .arg("--target")
        .arg(target)
        .arg("--audit")
        .arg(audit)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut events = Vec::new();
    let preview = loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "CLI endete vor Vorschau"
        );
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "real_preview" {
            let preview = event["preview"].clone();
            events.push(event);
            break preview;
        }
        events.push(event);
    };
    let answer = response(&preview);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(answer.as_bytes()).unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    for line in rest.lines().filter(|line| !line.is_empty()) {
        events.push(serde_json::from_str(line).unwrap());
    }
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, events, stderr)
}

fn run_provider_sync(
    sandbox: &CliSandbox,
    source: &Path,
    target: &Path,
    audit: &Path,
    extra: &[&str],
    response: impl FnOnce(&serde_json::Value) -> String,
) -> (std::process::ExitStatus, Vec<serde_json::Value>, String) {
    let mut command = sandbox.json_cli();
    command
        .args(["sync", "--source-root"])
        .arg(source)
        .arg("--target-root")
        .arg(target)
        .arg("--audit")
        .arg(audit)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut events = Vec::new();
    let preview = loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "CLI endete vor Provider-Sync-Vorschau"
        );
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "provider_sync_preview" {
            let preview = event["preview"].clone();
            events.push(event);
            break preview;
        }
        events.push(event);
    };
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(response(&preview).as_bytes()).unwrap();
    stdin.flush().unwrap();
    drop(stdin);
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    for line in rest.lines().filter(|line| !line.is_empty()) {
        events.push(serde_json::from_str(line).unwrap());
    }
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, events, stderr)
}

fn run_bidirectional(
    sandbox: &CliSandbox,
    left: &Path,
    right: &Path,
    baseline: &Path,
    response: impl FnOnce(&serde_json::Value) -> String,
) -> (std::process::ExitStatus, Vec<serde_json::Value>, String) {
    let mut command = sandbox.json_cli();
    command
        .args([
            "bidirectional",
            "run",
            "--pair-id",
            "22222222222222222222222222222222",
            "--pair-name",
            "CLI-Paar",
            "--left",
        ])
        .arg(left)
        .arg("--right")
        .arg(right)
        .arg("--baseline")
        .arg(baseline)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut events = Vec::new();
    let preview = loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "bidirectional_preview" {
            let preview = event["preview"].clone();
            events.push(event);
            break preview;
        }
        events.push(event);
    };
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(response(&preview).as_bytes()).unwrap();
    stdin.flush().unwrap();
    drop(stdin);
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    for line in rest.lines().filter(|line| !line.is_empty()) {
        events.push(serde_json::from_str(line).unwrap());
    }
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, events, stderr)
}

fn create_interrupted_bidirectional_run(
    left: &Path,
    right: &Path,
    baseline: &Path,
    pair_marker: u8,
) {
    std::fs::create_dir(left).unwrap();
    std::fs::create_dir(right).unwrap();
    // Die beiden Recovery-Tests laufen parallel. Eine gleiche Paar-ID würde die
    // absichtlich prozessweite Single-Flight-Schranke testen statt ihren jeweils
    // isolierten CLI-Ablauf und kann auf schnellen Linux-Hosts `WouldBlock` liefern.
    let pair = doppeldecker_core::PairId([pair_marker; 16]);
    let initial = doppeldecker_core::prepare_local_bidirectional(
        pair,
        "CLI-Recovery-Paar",
        left,
        right,
        baseline,
    )
    .unwrap();
    let phrase = initial.preview().confirmation_phrase.clone();
    initial
        .confirm(&phrase, false)
        .unwrap()
        .execute(&doppeldecker_core::Cancel::new(), &mut |_| {})
        .unwrap();
    doppeldecker_core::bidirectional::finish_bidirectional_run_completion(baseline).unwrap();

    std::fs::write(left.join("a-von-links"), b"links").unwrap();
    std::fs::write(right.join("b-von-rechts"), b"rechts").unwrap();
    let prepared = doppeldecker_core::prepare_local_bidirectional(
        pair,
        "CLI-Recovery-Paar",
        left,
        right,
        baseline,
    )
    .unwrap();
    let phrase = prepared.preview().confirmation_phrase.clone();
    let run = prepared.confirm(&phrase, true).unwrap();
    // Die Kollision entsteht nach der Permit-Ausgabe auf der zweiten Projektion.
    // Damit bleibt eine echte erste Wirkung im gebundenen Recovery-Journal.
    std::fs::write(left.join("b-von-rechts"), b"fremd").unwrap();
    assert!(run
        .execute(&doppeldecker_core::Cancel::new(), &mut |_| {})
        .is_err());
    std::fs::remove_file(left.join("b-von-rechts")).unwrap();
    assert!(baseline.with_extension("run.json").exists());
}

fn run_bidirectional_recovery(
    sandbox: &CliSandbox,
    command_name: &str,
    left: &Path,
    right: &Path,
    baseline: &Path,
) -> (std::process::ExitStatus, Vec<serde_json::Value>, String) {
    let mut command = sandbox.json_cli();
    command
        .args([
            "bidirectional",
            command_name,
            "--pair-name",
            "CLI-Recovery-Paar",
            "--left",
        ])
        .arg(left)
        .arg("--right")
        .arg(right)
        .arg("--baseline")
        .arg(baseline)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let preview_event = if command_name == "rollback" {
        "bidirectional_rollback_preview"
    } else {
        "bidirectional_preview"
    };
    let result_event = if command_name == "rollback" {
        "bidirectional_rollback_result"
    } else {
        "bidirectional_result"
    };
    let mut events = Vec::new();
    let preview = loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["event"] == preview_event {
            let preview = event["preview"].clone();
            events.push(event);
            break preview;
        }
        events.push(event);
    };
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        preview["confirmation_phrase"].as_str().unwrap()
    )
    .unwrap();
    if command_name == "resume" && preview["backup_confirmation_required"] == true {
        writeln!(stdin, "BACKUP UND RESTORE BESTÄTIGT").unwrap();
    }
    drop(stdin);

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["event"] == result_event {
            assert!(
                !baseline.with_extension("run.json").exists(),
                "Erfolgsausgabe kam vor dem mutationslosen Journalabschluss"
            );
        }
        events.push(event);
    }
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, events, stderr)
}

#[test]
fn sftp_inhaltsklasse_verweigert_move_ersetzung_und_ungebundene_auswahl_vor_profil_oder_queue() {
    for (command, item, replace) in [
        ("move", "/pane/a.txt", false),
        ("copy", "/pane/a.txt", true),
        ("copy", "/pane/sub/a.txt", false),
    ] {
        let sandbox = CliSandbox::new();
        let target = sandbox.path().canonicalize().unwrap().join("ziel");
        std::fs::create_dir(&target).unwrap();
        let mut invocation = sandbox.json_cli();
        invocation
            .args([
                command,
                "--source-provider",
                "sftp",
                "--source-profile",
                "artificial",
                "--source-root",
                "/pane",
                "--target-root",
            ])
            .arg(&target)
            .args(["--item", item]);
        if replace {
            invocation.arg("--replace");
        }
        let output = invocation.output().unwrap();
        assert!(!output.status.success());
        let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            events.iter().any(|event| event["code"]
                == if command == "copy" && !replace {
                    "invalid_input"
                } else {
                    "unsupported"
                }),
            "{events:?}"
        );
        assert!(!sandbox.config_dir().join("operations-queue.json").exists());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
}

#[test]
fn webdav_ersetzen_scheitert_vor_profil_schluesselbund_und_queue() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("Datei.txt"), b"neu").unwrap();
    let output = sandbox
        .json_cli()
        .args(["copy", "--source-root"])
        .arg(&source)
        .args([
            "--target-provider",
            "webdav",
            "--target-profile",
            "artificial",
            "--target-root",
            "/pane",
            "--replace",
            "--item",
        ])
        .arg(source.join("Datei.txt"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events.iter().any(|event| event["code"] == "unsupported"),
        "{events:?}"
    );
    assert!(serde_json::to_string(&events).unwrap().contains("ODER"));
    assert!(!sandbox.config_dir().join("operations-queue.json").exists());
    assert_eq!(std::fs::read(source.join("Datei.txt")).unwrap(), b"neu");
}

#[test]
fn cli_delta_dateibefehle_pruefen_das_zielprofil_vor_dem_queuejournal() {
    let sandbox = CliSandbox::new();
    let source = sandbox.path().join("quelle");
    std::fs::create_dir(&source).unwrap();
    let item = source.join("datei.txt");
    std::fs::write(&item, b"unveraendert").unwrap();
    for command in ["copy", "move"] {
        let output = sandbox
            .json_cli()
            .arg(command)
            .arg("--source-root")
            .arg(&source)
            .args([
                "--target-provider",
                "webdav",
                "--target-profile",
                "fehlendes-ziel",
                "--target-root",
                "/ziel",
                "--item",
            ])
            .arg(&item)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["code"], "not_found", "{command}: {event}");
        assert!(!sandbox.config_dir().join("operations-queue.json").exists());
        assert_eq!(std::fs::read(&item).unwrap(), b"unveraendert");
    }
}

#[test]
fn cli_delta_zielprofil_scheitert_vor_quellverbindung_und_queuejournal() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    let sandbox = CliSandbox::new();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let contacts = Arc::new(AtomicUsize::new(0));
    struct Peer {
        stopped: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Peer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    let stop = stopped.clone();
    let count = contacts.clone();
    let _peer = Peer {
        stopped,
        thread: Some(std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        count.fetch_add(1, Ordering::SeqCst);
                        stream.shutdown(std::net::Shutdown::Both).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2))
                    }
                    Err(error) => panic!("Eigener Loopback-Peer scheiterte: {error}"),
                }
            }
        })),
    };
    doppeldecker_core::save_sftp_profile(
        &sandbox.config_dir().join("sftp-profiles.json"),
        doppeldecker_core::SftpProfile {
            id: "kuenstliche-quelle".into(),
            name: "Künstliche Quelle".into(),
            connection: doppeldecker_core::SftpConnectionConfig {
                host: "127.0.0.1".into(),
                alternate_hosts: Vec::new(),
                port,
                username: "artificial".into(),
                requested_root: "/".into(),
                host_key: doppeldecker_core::SftpHostKeyPin {
                    algorithm: "ssh-ed25519".into(),
                    sha256: [1; 32],
                },
                authentication: doppeldecker_core::SftpAuthenticationReference::SshAgent {
                    public_key_sha256: [2; 32],
                },
            },
        },
    )
    .unwrap();
    let output = sandbox
        .json_cli()
        .env("SSH_AUTH_SOCK", sandbox.home().join("kein-agent.sock"))
        .args([
            "sync",
            "--source-provider",
            "sftp",
            "--source-profile",
            "kuenstliche-quelle",
            "--source-root",
            "/",
            "--target-provider",
            "ftps",
            "--target-profile",
            "fehlendes-ziel",
            "--target-root",
            "/",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let contacted = contacts.load(Ordering::SeqCst);
    assert!(events.iter().any(|event|event["code"]=="not_found"), "Fehlendes Zielprofil wurde von Quellfehler überholt; Quellkontakte={contacted}: {events:?}");
    assert_eq!(
        contacted, 0,
        "Ungültige Zielkonfiguration darf die Quelle nicht kontaktieren"
    );
    assert!(!sandbox.config_dir().join("operations-queue.json").exists());
}
