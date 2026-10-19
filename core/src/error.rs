//! Maschinenlesbare Fehlercodes, die **im `io::Error` mitfahren**.
//!
//! Der Kern behält bewusst `io::Result` in allen Signaturen (kein API-Umbau).
//! Stattdessen wird an den domänenrelevanten Stellen ein [`ErrorCode`] als
//! Fehler-Payload in den `io::Error` eingebettet. GUI und CLI können den Code
//! über [`error_code`] wieder herausziehen und unterschiedlich reagieren
//! (z. B. bei [`ErrorCode::PlanDrift`] automatisch eine neue Vorschau anbieten),
//! statt fragil auf deutsche Fehlertexte zu matchen. Der Text bleibt für die
//! Anzeige erhalten; die Codes sind stabil und serialisierbar.

use std::fmt;
use std::io;

/// Stabiler, maschinenlesbarer Code für Domänenfehler des Kerns.
///
/// Bewusst klein gehalten: Ein Code kommt nur hinzu, wenn ein Aufrufer darauf
/// tatsächlich anders reagieren soll als mit „Fehlertext anzeigen“.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Der bestätigte Plan entspricht nicht mehr dem Ist-Zustand — Quelle oder
    /// Ziel haben sich seit der Vorschau geändert. Richtige Reaktion: neue
    /// Vorschau erstellen und erneut bestätigen lassen.
    PlanDrift,
    /// Ein persistierter Altplan wurde sicher vor seiner ersten Mutation gestoppt,
    /// kann aber wegen eines geänderten Bindungsschemas nicht erneut bestätigt
    /// werden. Richtige Reaktion: Eintrag verwerfen und eine neue Vorschau anlegen.
    ReplanRequired,
    /// Ein Schreib-/Lösch-Ziel liegt außerhalb der aktiven Sicherheits-Sandbox.
    SandboxViolation,
    /// Ein Sicherheits-Gate hat reale Mirror-Entfernungen gesperrt (leere
    /// Quelle, ausgelassene Einträge oder überschrittene Löschgrenzen).
    MirrorBlocked,
    /// Eine verlangte Bestätigungsphrase stimmt nicht überein.
    ConfirmationMismatch,
    /// Eingabe oder persistierter Zustand ist strukturell ungültig.
    InvalidInput,
    InvalidData,
    /// Ein gebundener Pfad beziehungsweise ein gespeichertes Profil fehlt.
    NotFound,
    /// Der Zielname entstand bereits oder kollidiert mit der bestätigten Absicht.
    AlreadyExists,
    /// Protokoll oder Produktvertrag bieten die angeforderte Wirkung dauerhaft
    /// nicht an; eine erneute Capability-Probe ändert daran nichts.
    Unsupported,
    /// Eine grundsätzlich vorgesehene Providerwirkung ist für diese konkrete
    /// Sitzung noch nicht durch ihre Verhaltensprobe freigeschaltet. Aufrufer
    /// sollen die passende Capability-Probe ausführen beziehungsweise nach einem
    /// Reconnect neu vorbereiten; das ist keine Server-Berechtigungsablehnung.
    CapabilityProbeRequired,
    /// Betriebssystem, Server oder Schlüsselbund verweigert den Zugriff.
    PermissionDenied,
    /// Zielmedium oder lokaler Snapshot-Cache besitzt nicht genug Platz.
    StorageFull,
    /// Lauf wurde kontrolliert unterbrochen oder die Sitzung wechselte.
    Interrupted,
    /// Ein Quarantänelauf wird in diesem Prozess noch von einer laufenden Mutation
    /// gehalten. Restore und endgültiges Leeren müssen bis zur Freigabe warten.
    QuarantineBusy,
    /// Eine Transport- oder Kontrollsitzung ist abgebrochen und kann nach erneuter
    /// Identitätsprüfung wiederaufgebaut werden.
    ConnectionLost,
    /// Eine Mutation wurde genau einmal gesendet, ihr Ergebnis ließ sich danach aber
    /// weder bestätigen noch ausschließen (z. B. Antwortverlust und anschließend kein
    /// eindeutiger Listing-Beleg). Aufrufer dürfen die Mutation niemals blind
    /// wiederholen; zuerst muss der tatsächliche Zielzustand geklärt werden.
    OutcomeUnclear,
    /// TLS funktioniert, aber die öffentliche Zertifikatsprüfung bestätigt den
    /// angesprochenen Servernamen beziehungsweise die IP-Adresse nicht. Aufrufer
    /// dürfen daraufhin ausdrücklich den sichtbaren, weiterhin gepinnten
    /// FTPS-Modus ohne Identitätsbehauptung anbieten.
    TlsIdentityUnverified,
    /// Kein spezifischer Domänenfehler — gewöhnlicher I/O-Fehler.
    Io,
}

impl ErrorCode {
    /// Stabiler String für Schnittstellen, die kein serde nutzen (z. B. der
    /// `AppError` der Tauri-Commands oder das JSONL-Protokoll des CLI).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::PlanDrift => "plan_drift",
            ErrorCode::ReplanRequired => "replan_required",
            ErrorCode::SandboxViolation => "sandbox_violation",
            ErrorCode::MirrorBlocked => "mirror_blocked",
            ErrorCode::ConfirmationMismatch => "confirmation_mismatch",
            ErrorCode::InvalidInput => "invalid_input",
            ErrorCode::InvalidData => "invalid_data",
            ErrorCode::NotFound => "not_found",
            ErrorCode::AlreadyExists => "already_exists",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::CapabilityProbeRequired => "capability_probe_required",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::StorageFull => "storage_full",
            ErrorCode::Interrupted => "interrupted",
            ErrorCode::QuarantineBusy => "quarantine_busy",
            ErrorCode::ConnectionLost => "connection_lost",
            ErrorCode::OutcomeUnclear => "outcome_unclear",
            ErrorCode::TlsIdentityUnverified => "tls_identity_unverified",
            ErrorCode::Io => "io",
        }
    }
}

/// Baut den einheitlichen Fehler für ein noch nicht sitzungsgebundenes
/// Provider-Gate. `Other` verhindert, dass ältere Aufrufer den Fehler über den
/// groben `io::ErrorKind` als echte Zugriffsablehnung missverstehen; der stabile
/// [`ErrorCode`] trägt die konkrete Reaktion.
pub fn capability_probe_required(message: impl Into<String>) -> io::Error {
    coded_error(
        io::ErrorKind::Other,
        ErrorCode::CapabilityProbeRequired,
        message,
    )
}

/// Interner Träger: hängt den Code als Fehler-Payload an den `io::Error`.
#[derive(Debug)]
struct CodedError {
    code: ErrorCode,
    message: String,
    recovery_paths: Vec<String>,
}

impl fmt::Display for CodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Nur die Botschaft — der Code ist Maschinen-, nicht Anzeigematerial.
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

/// Baut einen `io::Error`, der zusätzlich zum Anzeigetext einen stabilen
/// [`ErrorCode`] trägt.
pub fn coded_error(kind: io::ErrorKind, code: ErrorCode, message: impl Into<String>) -> io::Error {
    io::Error::new(
        kind,
        CodedError {
            code,
            message: message.into(),
            recovery_paths: Vec::new(),
        },
    )
}

/// Ergänzt einen Fehlerkontext, ohne Fehlerart, Domänencode oder Prüfpfade durch
/// einen neuen unstrukturierten I/O-Fehler zu ersetzen.
pub(crate) fn with_context(error: io::Error, context: impl fmt::Display) -> io::Error {
    io::Error::new(
        error.kind(),
        CodedError {
            code: error_code(&error),
            message: format!("{context}: {error}"),
            recovery_paths: recovery_paths(&error),
        },
    )
}

/// Erhält den ursprünglichen Fehlercode und führt einen nicht aufgeräumten Temp
/// als maschinenlesbaren Rest sowie im sichtbaren Fehlertext weiter.
pub(crate) fn with_recovery_path(
    error: io::Error,
    path: &std::path::Path,
    cleanup: &io::Error,
) -> io::Error {
    let mut paths = recovery_paths(&error);
    for path in recovery_paths(cleanup) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let path = path.to_string_lossy().into_owned();
    if !paths.contains(&path) {
        paths.push(path.clone());
    }
    io::Error::new(
        error.kind(),
        CodedError {
            code: error_code(&error),
            message: format!(
                "{error}; Bereinigung fehlgeschlagen ({cleanup}); Recoverypfad: {path}"
            ),
            recovery_paths: paths,
        },
    )
}

/// Erhält Code, Fehlerart und vorhandene Recoverypfade beim Ergänzen weiterer
/// belegter Reste. Die Pfade bleiben für Queue/Audit strukturiert und sichtbar.
pub fn with_recovery_paths(error: io::Error, additional: &[String]) -> io::Error {
    if additional.is_empty() {
        return error;
    }
    let mut paths = recovery_paths(&error);
    for path in additional {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    io::Error::new(
        error.kind(),
        CodedError {
            code: error_code(&error),
            message: format!("{error}; Recoverypfade: {}", paths.join("; ")),
            recovery_paths: paths,
        },
    )
}

/// Übernimmt die strukturierten Reste vor dem Ersetzen einer Fehlerhülle und
/// ergänzt die an dieser Wirkungsgrenze möglichen Namen. Der neue Code bleibt
/// erhalten, etwa wenn ein ursprünglicher I/O-Fehler jetzt unklare Wirkung meldet.
pub(crate) fn with_recovery_context(
    error: io::Error,
    causes: &[&io::Error],
    paths: &[&std::path::Path],
) -> io::Error {
    let mut recovery = causes
        .iter()
        .flat_map(|cause| recovery_paths(cause))
        .collect::<Vec<_>>();
    recovery.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));
    with_recovery_paths(error, &recovery)
}

/// Strukturierte Prüfhinweise für Queue, GUI und CLI. Sie erteilen kein Recht,
/// die benannten lokalen oder entfernten Objekte zu verändern.
pub fn recovery_paths(error: &io::Error) -> Vec<String> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CodedError>())
        .map(|coded| coded.recovery_paths.clone())
        .unwrap_or_default()
}

/// Liest den [`ErrorCode`] aus einem `io::Error` zurück; gewöhnliche Fehler
/// ohne eingebetteten Code gelten als [`ErrorCode::Io`].
pub fn error_code(error: &io::Error) -> ErrorCode {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CodedError>())
        .map(|coded| coded.code)
        .unwrap_or_else(|| match error.kind() {
            io::ErrorKind::InvalidInput => ErrorCode::InvalidInput,
            io::ErrorKind::InvalidData => ErrorCode::InvalidData,
            io::ErrorKind::NotFound => ErrorCode::NotFound,
            io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
            io::ErrorKind::Unsupported => ErrorCode::Unsupported,
            io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            io::ErrorKind::StorageFull => ErrorCode::StorageFull,
            io::ErrorKind::Interrupted => ErrorCode::Interrupted,
            io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof => ErrorCode::ConnectionLost,
            _ => ErrorCode::Io,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_und_text_ueberleben_den_io_error() {
        let error = coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::PlanDrift,
            "Plan ist nicht mehr aktuell",
        );
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert_eq!(error.to_string(), "Plan ist nicht mehr aktuell");
    }

    #[test]
    fn gewoehnliche_io_fehler_erhalten_stabile_aktionscodes() {
        let error = io::Error::new(io::ErrorKind::NotFound, "weg");
        assert_eq!(error_code(&error), ErrorCode::NotFound);
        let lost = io::Error::new(io::ErrorKind::UnexpectedEof, "leer");
        assert_eq!(error_code(&lost), ErrorCode::ConnectionLost);
        assert_eq!(error_code(&io::Error::other("kaputt")), ErrorCode::Io);
    }

    #[test]
    fn fehlende_capability_probe_ist_keine_berechtigungsablehnung() {
        let error = capability_probe_required("Sitzungsprobe fehlt");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error_code(&error), ErrorCode::CapabilityProbeRequired);
        assert_eq!(
            ErrorCode::CapabilityProbeRequired.as_str(),
            "capability_probe_required"
        );
    }

    #[test]
    fn lebende_quarantaene_besitzt_eigenen_maschinencode() {
        assert_eq!(ErrorCode::QuarantineBusy.as_str(), "quarantine_busy");
        assert_eq!(
            serde_json::to_string(&ErrorCode::QuarantineBusy).unwrap(),
            "\"quarantine_busy\""
        );
    }
}
