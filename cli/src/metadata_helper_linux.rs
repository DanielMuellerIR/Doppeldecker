//! Ein Polkit-Aufruf bearbeitet genau einen typisierten Auftrag über private Pipes.
use doppeldecker_core::{file_properties::admin::AdminMetadataRequest, Cancel};
use std::io::{self, Read, Write};

const MAX_REQUEST_BYTES: u64 = 32 * 1024 * 1024;

fn error(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({"error":{"code":code,"message":message}})
}

fn read_request(input: &mut impl Read) -> io::Result<AdminMetadataRequest> {
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as u64;
    if length == 0 || length > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Ungültige Auftragsgröße",
        ));
    }
    let mut bytes = Vec::new();
    input.take(length).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Unvollständiger Auftrag",
        ));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn execute(input: &mut impl Read, cancel: &Cancel) -> serde_json::Value {
    let request = match read_request(input) {
        Ok(request) => request,
        Err(_) => {
            return error(
                "invalid_input",
                "Ungültiger typisierter Administratorauftrag",
            )
        }
    };
    // Erst nach der Systemfreigabe meldet der Helfer Bereitschaft. Der Client
    // kann einen während des Polkit-Dialogs abgebrochenen Auftrag noch ablehnen.
    if writeln!(io::stdout(), "READY")
        .and_then(|_| io::stdout().flush())
        .is_err()
    {
        return error("interrupted", "Administratorverbindung beendet");
    }
    let mut gate = [0];
    if input.read_exact(&mut gate).is_err() || gate[0] != b'E' {
        return error(
            "interrupted",
            "Administratoränderung vor Ausführung abgebrochen",
        );
    }
    let monitor_cancel = cancel.clone();
    std::thread::spawn(move || {
        let mut signal = [0];
        // Auch ein verlorener Client beendet die verbleibenden Änderungen.
        let _ = io::stdin().read_exact(&mut signal);
        monitor_cancel.cancel();
    });
    match request.bind(cancel).and_then(|bound| bound.execute(cancel)) {
        Ok(rows) => serde_json::json!({"ok":rows}),
        Err(error) => serde_json::json!({"error":{
            "code":doppeldecker_core::error_code(&error),"message":error.to_string()
        }}),
    }
}

pub(super) fn run() -> i32 {
    // Der Helfer wird nie als allgemeines Root-CLI freigegeben. pkexec prüft die
    // Systemanmeldung selbst; dessen UID kennzeichnet den ursprünglichen Benutzer.
    let authenticated = unsafe { libc::geteuid() } == 0
        && std::env::var("PKEXEC_UID")
            .ok()
            .and_then(|uid| uid.parse::<u32>().ok())
            .is_some();
    let result = if authenticated {
        std::panic::catch_unwind(|| execute(&mut io::stdin(), &Cancel::new())).unwrap_or_else(
            |_| {
                error(
                    "outcome_unclear",
                    "Administratoränderung unterbrochen; Teilwirkungen prüfen",
                )
            },
        )
    } else {
        error(
            "permission_denied",
            "Metadatenhelfer benötigt eine Polkit-Systemfreigabe",
        )
    };
    match serde_json::to_writer(io::stdout(), &result)
        .and_then(|_| io::stdout().flush().map_err(serde_json::Error::io))
    {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_truncated_and_untyped_requests() {
        for bytes in [
            (u32::MAX).to_be_bytes().to_vec(),
            [4_u32.to_be_bytes().as_slice(), b"{}"].concat(),
            [2_u32.to_be_bytes().as_slice(), b"{}"].concat(),
        ] {
            assert!(read_request(&mut bytes.as_slice()).is_err());
        }
    }
}
