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
    directory_stream_report: crate::directory_streams::DirectoryStreamReport,
    directory_dacl_report: crate::directory_dacl::DirectoryDaclReport,
    file_dacl_report: crate::file_dacl::FileDaclReport,
    created_directory_dacl_report: crate::created_directory_dacl::CreatedDirectoryDaclReport,
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
            directory_stream_report: Default::default(),
            directory_dacl_report: Default::default(),
            file_dacl_report: Default::default(),
            created_directory_dacl_report: Default::default(),
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
            directory_stream_report: directory_stream_report(&error),
            directory_dacl_report: directory_dacl_report(&error),
            file_dacl_report: file_dacl_report(&error),
            created_directory_dacl_report: created_directory_dacl_report(&error),
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
    let mut report = directory_stream_report(&error);
    report.merge(&directory_stream_report(cleanup));
    let mut dacl_report = directory_dacl_report(&error);
    dacl_report.merge(&directory_dacl_report(cleanup));
    let mut file_report = file_dacl_report(&error);
    file_report.merge(&file_dacl_report(cleanup));
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
            directory_stream_report: report,
            directory_dacl_report: dacl_report,
            file_dacl_report: file_report,
            created_directory_dacl_report: {
                let mut report = created_directory_dacl_report(&error);
                report.merge(&created_directory_dacl_report(cleanup));
                report
            },
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
            directory_stream_report: directory_stream_report(&error),
            directory_dacl_report: directory_dacl_report(&error),
            file_dacl_report: file_dacl_report(&error),
            created_directory_dacl_report: created_directory_dacl_report(&error),
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
    let mut report = directory_stream_report(&error);
    let mut dacl_report = directory_dacl_report(&error);
    let mut file_report = file_dacl_report(&error);
    for cause in causes {
        file_report.merge(&file_dacl_report(cause));
        report.merge(&directory_stream_report(cause));
        dacl_report.merge(&directory_dacl_report(cause));
    }
    let mut created_report = created_directory_dacl_report(&error);
    for cause in causes {
        created_report.merge(&created_directory_dacl_report(cause));
    }
    with_created_directory_dacl_report(
        with_file_dacl_report(
            with_directory_dacl_report(
                with_directory_stream_report(with_recovery_paths(error, &recovery), &report),
                &dacl_report,
            ),
            &file_report,
        ),
        &created_report,
    )
}

/// Erhält bestätigte und unklare dauerhafte Einzelwirkungen auch beim Ersetzen
/// einer Fehlerhülle. Derselbe Beleg wird nur einmal geführt.
pub fn with_directory_stream_report(
    error: io::Error,
    additional: &crate::directory_streams::DirectoryStreamReport,
) -> io::Error {
    let mut report = directory_stream_report(&error);
    report.merge(additional);
    if report.is_empty() || report == directory_stream_report(&error) {
        return error;
    }
    io::Error::new(
        error.kind(),
        CodedError {
            code: error_code(&error),
            message: format!("{error}; {}", directory_stream_report_text(&report)),
            recovery_paths: recovery_paths(&error),
            directory_stream_report: report,
            directory_dacl_report: directory_dacl_report(&error),
            file_dacl_report: file_dacl_report(&error),
            created_directory_dacl_report: created_directory_dacl_report(&error),
        },
    )
}

/// Maschinenlesbare Einzelwirkungen; ein leerer Report behauptet keine Wirkung.
pub fn directory_stream_report(
    error: &io::Error,
) -> crate::directory_streams::DirectoryStreamReport {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CodedError>())
        .map(|coded| coded.directory_stream_report.clone())
        .unwrap_or_default()
}

/// Sichtbarer Beleg ohne Wiederherstellungsbehauptung. Die vollständigen Hashes
/// und Größen bleiben zusätzlich im strukturierten Report erhalten.
pub fn directory_stream_report_text(
    report: &crate::directory_streams::DirectoryStreamReport,
) -> String {
    use crate::directory_streams::DirectoryStreamEffect;
    let describe = |outcome: &crate::directory_streams::DirectoryStreamOutcome| {
        let action = match &outcome.effect {
            DirectoryStreamEffect::Add { after } => {
                format!("angelegt ({} Bytes)", after.value.size)
            }
            DirectoryStreamEffect::Replace { before, after } => format!(
                "dauerhaft ersetzt ({} → {} Bytes)",
                before.value.size, after.value.size
            ),
            DirectoryStreamEffect::Remove { before } => {
                format!("dauerhaft entfernt ({} Bytes)", before.value.size)
            }
        };
        format!(
            "{}: Stream {:?} {action}",
            outcome.directory,
            String::from_utf8_lossy(outcome.effect.name())
        )
    };
    let mut parts = Vec::new();
    for outcome in &report.completed {
        parts.push(format!("Bestätigt: {}", describe(outcome)));
    }
    for outcome in &report.uncertain {
        parts.push(format!("Ausgang unklar: {}", describe(outcome)));
    }
    parts.join("; ")
}

/// Bewahrt bestätigte und unklare DACL-Einzelwirkungen beim Weiterreichen.
pub fn with_directory_dacl_report(
    error: io::Error,
    additional: &crate::directory_dacl::DirectoryDaclReport,
) -> io::Error {
    let mut report = directory_dacl_report(&error);
    report.merge(additional);
    if report.is_empty() || report == directory_dacl_report(&error) {
        return error;
    }
    io::Error::new(
        error.kind(),
        CodedError {
            code: error_code(&error),
            message: format!("{error}; {}", directory_dacl_report_text(&report)),
            recovery_paths: recovery_paths(&error),
            directory_stream_report: directory_stream_report(&error),
            directory_dacl_report: report,
            file_dacl_report: file_dacl_report(&error),
            created_directory_dacl_report: created_directory_dacl_report(&error),
        },
    )
}

pub fn directory_dacl_report(error: &io::Error) -> crate::directory_dacl::DirectoryDaclReport {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CodedError>())
        .map(|coded| coded.directory_dacl_report.clone())
        .unwrap_or_default()
}

pub fn directory_dacl_report_text(report: &crate::directory_dacl::DirectoryDaclReport) -> String {
    report
        .completed
        .iter()
        .map(|outcome| {
            format!(
                "Bestätigt: {}: Verzeichnis-DACL dauerhaft gesetzt",
                outcome.directory
            )
        })
        .chain(report.uncertain.iter().map(|outcome| {
            format!(
                "Ausgang unklar: {}: Verzeichnis-DACL; vor weiteren Änderungen prüfen",
                outcome.directory
            )
        }))
        .collect::<Vec<_>>()
        .join("; ")
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

pub fn with_file_dacl_report(
    error: io::Error,
    additional: &crate::file_dacl::FileDaclReport,
) -> io::Error {
    let mut report = file_dacl_report(&error);
    report.merge(additional);
    if report.is_empty() || report == file_dacl_report(&error) {
        return error;
    }
    io::Error::new(
        error.kind(),
        CodedError {
            code: error_code(&error),
            message: format!("{error}; {}", file_dacl_report_text(&report)),
            recovery_paths: recovery_paths(&error),
            directory_stream_report: directory_stream_report(&error),
            directory_dacl_report: directory_dacl_report(&error),
            file_dacl_report: report,
            created_directory_dacl_report: created_directory_dacl_report(&error),
        },
    )
}
pub fn file_dacl_report(error: &io::Error) -> crate::file_dacl::FileDaclReport {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CodedError>())
        .map(|coded| coded.file_dacl_report.clone())
        .unwrap_or_default()
}
pub fn file_dacl_report_text(report: &crate::file_dacl::FileDaclReport) -> String {
    let describe = |outcome: &crate::file_dacl::FileDaclOutcome| {
        format!("{}: objektweite Datei-DACL ({} ausgewählte Namen; bekannte Namensliste unvollständig, alle bestehenden und neuen Namen auch außerhalb des Scanroots betroffen)",outcome.file,outcome.alias_scope.selected_paths.len())
    };
    report
        .completed
        .iter()
        .map(|outcome| format!("Bestätigt: {}", describe(outcome)))
        .chain(
            report
                .uncertain
                .iter()
                .map(|outcome| format!("Ausgang unklar: {}", describe(outcome))),
        )
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn with_created_directory_dacl_report(
    error: io::Error,
    additional: &crate::created_directory_dacl::CreatedDirectoryDaclReport,
) -> io::Error {
    let mut report = created_directory_dacl_report(&error);
    report.merge(additional);
    if report.is_empty() || report == created_directory_dacl_report(&error) {
        return error;
    }
    io::Error::new(
        error.kind(),
        CodedError {
            code: error_code(&error),
            message: format!("{error}; {}", created_directory_dacl_report_text(&report)),
            recovery_paths: recovery_paths(&error),
            directory_stream_report: directory_stream_report(&error),
            directory_dacl_report: directory_dacl_report(&error),
            file_dacl_report: file_dacl_report(&error),
            created_directory_dacl_report: report,
        },
    )
}
pub fn created_directory_dacl_report(
    error: &io::Error,
) -> crate::created_directory_dacl::CreatedDirectoryDaclReport {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CodedError>())
        .map(|coded| coded.created_directory_dacl_report.clone())
        .unwrap_or_default()
}
pub fn created_directory_dacl_report_text(
    report: &crate::created_directory_dacl::CreatedDirectoryDaclReport,
) -> String {
    let describe = |outcome: &crate::created_directory_dacl::CreatedDirectoryDaclOutcome| {
        format!(
            "{}: {} (Objekt {})",
            outcome.directory,
            if outcome.origin == crate::created_directory_dacl::CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren {
                "Rechte vor der Kinderarbeit am selbst neu erzeugten Ordner"
            } else { "Rechte des selbst neu erzeugten leeren Ordners" },
            outcome.file_id
        )
    };
    report
        .created
        .iter()
        .map(|observation| {
            format!(
                "{}: {} (Objekt {}; gewöhnliche Metadaten {})",
                if observation.effect_key.origin == crate::created_directory_dacl::CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren { "Vor Kinderarbeit angelegt" } else { "Angelegt" },
                observation.effect_key.directory,
                observation.effect_key.file_id,
                if observation.ordinary_metadata_applied {
                    "bestätigt"
                } else {
                    "noch nicht bestätigt"
                }
            )
        })
        .chain(
            report
                .completed
                .iter()
                .map(|outcome| format!("Bestätigt: {}", describe(outcome))),
        )
        .chain(
            report
                .uncertain
                .iter()
                .map(|outcome| format!("Ausgang unklar: {}", describe(outcome))),
        )
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_report(uncertain: bool) -> crate::directory_streams::DirectoryStreamReport {
        use crate::directory_streams::{
            DirectoryStreamEffect, DirectoryStreamOutcome, DirectoryStreamReport,
        };
        let outcome = DirectoryStreamOutcome {
            directory: "ordner".into(),
            effect: DirectoryStreamEffect::Remove {
                before: crate::metadata::NamedBlobSummary {
                    name: b"Altstream".to_vec(),
                    value: crate::metadata::BlobSummary::from_bytes(b"alt"),
                },
            },
        };
        DirectoryStreamReport {
            completed: if uncertain {
                vec![]
            } else {
                vec![outcome.clone()]
            },
            uncertain: if uncertain { vec![outcome] } else { vec![] },
        }
    }

    #[test]
    fn verzeichnisstream_bilanz_ueberlebt_kontext_codeersatz_und_cleanup() {
        let report = stream_report(false);
        let error = with_directory_stream_report(io::Error::other("nach Teilwirkung"), &report);
        let error = with_context(error, "Kontext");
        let error = with_recovery_paths(error, &["rest".into()]);
        let cleanup =
            with_directory_stream_report(io::Error::other("cleanup"), &stream_report(true));
        let error = with_recovery_path(error, std::path::Path::new("temp"), &cleanup);
        let error = with_recovery_context(
            coded_error(
                io::ErrorKind::Other,
                ErrorCode::OutcomeUnclear,
                "neue Hülle",
            ),
            &[&error, &cleanup],
            &[],
        );
        assert_eq!(directory_stream_report(&error), report);
        assert_eq!(error_code(&error), ErrorCode::OutcomeUnclear);
        assert_eq!(recovery_paths(&error), vec!["rest", "temp"]);
        assert!(error.to_string().contains("Altstream"));
        assert!(error.to_string().contains("dauerhaft entfernt"));
        let before = error.to_string();
        let error = with_directory_stream_report(error, &report);
        assert_eq!(error.to_string(), before);
    }

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
