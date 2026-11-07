//! Kurzlebiger Polkit-Transport; keine Zugangsdaten, Shell oder persistente Freigabe.
use super::*;
use std::{
    io::{BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESULT_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn helper_path() -> Result<PathBuf, AppError> {
    let executable = std::env::current_exe()
        .map_err(|_| AppError::new("unsupported", "App-Pfad nicht verfügbar"))?;
    let helper = executable.with_file_name("doppeldecker-metadata-helper");
    if !helper.is_file() {
        return Err(AppError::new(
            "unsupported",
            "Mitgelieferter Metadatenhelfer fehlt",
        ));
    }
    Ok(helper)
}

pub(super) fn perform(
    request: &AdminMetadataRequest,
    cancel: &Cancel,
) -> Result<serde_json::Value, AppError> {
    if cancel.is_cancelled() {
        return Err(AppError::new(
            "interrupted",
            "Administratoränderung abgebrochen",
        ));
    }
    let bytes = serde_json::to_vec(request)
        .map_err(|_| AppError::new("invalid_input", "Administratorauftrag nicht darstellbar"))?;
    // Root kann ein benutzereigenes AppImage-FUSE-Mount nicht zuverlässig
    // betreten. Der eine Helfer läuft deshalb aus einer exklusiven, kurzlebigen
    // Kopie; der TempDir bleibt bis zum vollständigen Prozessergebnis gehalten.
    let helper = stage_helper(&helper_path()?)?;
    let mut command = Command::new("/usr/bin/pkexec");
    command
        .arg("--disable-internal-agent")
        .arg(helper.path().join("doppeldecker-metadata-helper"));
    exchange(command, bytes, cancel)
}

fn stage_helper(source: &std::path::Path) -> Result<tempfile::TempDir, AppError> {
    use std::os::unix::fs::PermissionsExt;
    let prepare = || -> std::io::Result<tempfile::TempDir> {
        let directory = tempfile::Builder::new()
            .prefix("doppeldecker-admin-")
            .tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let staged = directory.path().join("doppeldecker-metadata-helper");
        std::fs::copy(source, &staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o500))?;
        Ok(directory)
    };
    prepare().map_err(|_| AppError::new("io", "Metadatenhelfer konnte nicht bereitgestellt werden"))
}

fn exchange(
    mut command: Command,
    bytes: Vec<u8>,
    cancel: &Cancel,
) -> Result<serde_json::Value, AppError> {
    if bytes.is_empty() || bytes.len() > MAX_REQUEST_BYTES {
        return Err(AppError::new(
            "invalid_input",
            "Administratorauftrag zu groß oder leer",
        ));
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| AppError::new("unsupported", "Polkit konnte nicht gestartet werden"))?;
    let mut input = child.stdin.take().expect("Private Eingabepipe");
    let output = child.stdout.take().expect("Private Ergebnispipe");
    let finished = AtomicBool::new(false);
    let ready = AtomicBool::new(false);
    let started = AtomicBool::new(false);
    let (sender, receiver) = mpsc::channel();
    let deadline = Instant::now() + Duration::from_secs(120);
    std::thread::scope(|scope| {
        let finished_ref = &finished;
        let started_ref = &started;
        let writer = scope.spawn(move || -> std::io::Result<()> {
            input.write_all(&(bytes.len() as u32).to_be_bytes())?;
            input.write_all(&bytes)?;
            loop {
                if cancel.is_cancelled() || finished_ref.load(Ordering::Acquire) {
                    return Ok(());
                }
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => return Ok(()),
                }
            }
            if cancel.is_cancelled() {
                return Ok(());
            }
            started_ref.store(true, Ordering::Release);
            input.write_all(b"E")?;
            while !finished_ref.load(Ordering::Acquire) {
                if cancel.is_cancelled() {
                    input.write_all(b"C")?;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(())
        });
        let reader = scope.spawn(|| -> std::io::Result<Vec<u8>> {
            let mut output = BufReader::new(output).take(MAX_RESULT_BYTES + 1);
            let mut prefix = [0; 6];
            output.read_exact(&mut prefix)?;
            let mut result = Vec::new();
            if &prefix == b"READY\n" {
                ready.store(true, Ordering::Release);
                let _ = sender.send(());
            } else {
                result.extend(prefix);
            }
            output.read_to_end(&mut result)?;
            Ok(result)
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    // Vor READY läuft noch die Systemanmeldung. Ein Abbruch
                    // darf danach niemals einen bereits gepufferten Auftrag starten.
                    if !ready.load(Ordering::Acquire)
                        && (cancel.is_cancelled() || Instant::now() >= deadline)
                    {
                        let _ = child.kill();
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => {
                    break Err(AppError::new(
                        "outcome_unclear",
                        "Administratorprozess nicht mehr beobachtbar; Teilwirkungen prüfen",
                    ))
                }
            }
        };
        finished.store(true, Ordering::Release);
        let write_result = writer.join();
        let read_result = reader.join();
        let status = status?;
        let executed = started.load(Ordering::Acquire);
        if !executed {
            return Err(if cancel.is_cancelled() || status.code() == Some(126) {
                AppError::new("interrupted", "Administratorfreigabe abgebrochen")
            } else {
                AppError::new(
                    "permission_denied",
                    "Administratorfreigabe nicht erteilt oder Polkit nicht verfügbar",
                )
            });
        }
        let result = read_result
            .ok()
            .and_then(Result::ok)
            .filter(|bytes| bytes.len() as u64 <= MAX_RESULT_BYTES)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        if !status.success() || !matches!(write_result, Ok(Ok(()))) || result.is_none() {
            return Err(AppError::new(
                "outcome_unclear",
                "Administratorergebnis fehlt oder ist beschädigt; Teilwirkungen prüfen",
            ));
        }
        Ok(result.expect("Geprüftes Administratorergebnis"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_copy_is_private_executable_and_lives_until_transport_finishes() {
        use std::os::unix::fs::PermissionsExt;
        let area = tempfile::tempdir().unwrap();
        let source = area.path().join("helper");
        std::fs::write(&source, b"packaged helper").unwrap();
        let directory = stage_helper(&source).unwrap();
        let path = directory.path().join("doppeldecker-metadata-helper");
        assert_eq!(std::fs::read(&path).unwrap(), b"packaged helper");
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o500
        );
        drop(directory);
        assert!(!path.exists());
    }

    fn peer(script: &str) -> Command {
        let mut command = Command::new("python3");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn transport_returns_typed_response_and_preserves_lost_response_as_unclear() {
        let prelude = "import sys; n=int.from_bytes(sys.stdin.buffer.read(4),'big'); sys.stdin.buffer.read(n); print('READY',flush=True); assert sys.stdin.buffer.read(1)==b'E'; ";
        let value = exchange(
            peer(&format!("{prelude}print('{{\"ok\":true}}')")),
            b"{}".to_vec(),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(value["ok"], true);
        let error = exchange(peer(prelude), b"{}".to_vec(), &Cancel::new()).unwrap_err();
        assert_eq!(error.code, "outcome_unclear");
    }

    #[test]
    fn cancelled_authorization_never_sends_execution_gate() {
        let cancel = Cancel::new();
        cancel.cancel();
        let command = peer("import sys; n=int.from_bytes(sys.stdin.buffer.read(4),'big'); sys.stdin.buffer.read(n); print('READY',flush=True); assert sys.stdin.buffer.read(1)==b''");
        assert_eq!(
            exchange(command, b"{}".to_vec(), &cancel).unwrap_err().code,
            "interrupted"
        );
    }

    #[test]
    fn polkit_refusal_is_not_reported_as_a_completed_change() {
        assert_eq!(
            exchange(
                peer("raise SystemExit(127)"),
                b"{}".to_vec(),
                &Cancel::new()
            )
            .unwrap_err()
            .code,
            "permission_denied"
        );
    }

    #[test]
    fn cancellation_reaches_an_already_started_helper() {
        let area = tempfile::tempdir().unwrap();
        let marker = area.path().join("executing");
        let script = "import sys,pathlib; n=int.from_bytes(sys.stdin.buffer.read(4),'big'); sys.stdin.buffer.read(n); print('READY',flush=True); assert sys.stdin.buffer.read(1)==b'E'; pathlib.Path(sys.argv[1]).touch(); assert sys.stdin.buffer.read(1)==b'C'; print('{\"error\":{\"code\":\"interrupted\",\"message\":\"cancelled\"}}')";
        let mut command = peer(script);
        command.arg(&marker);
        let cancel = Cancel::new();
        std::thread::scope(|scope| {
            let result = scope.spawn(|| exchange(command, b"{}".to_vec(), &cancel));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let started = marker.exists();
            cancel.cancel();
            let value = result.join().unwrap().unwrap();
            assert!(started);
            assert_eq!(value["error"]["code"], "interrupted");
        });
    }
}
