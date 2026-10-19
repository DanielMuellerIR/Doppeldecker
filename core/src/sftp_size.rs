//! Größen aus derselben begrenzten Metadatenvorbereitung wie SFTP-Downloads.
//! Es entsteht weder ein vollständiges Sync-Listing noch ein Datei-READ-Handle.

use super::*;
use crate::directory_size::{
    record_result, DirectorySizeIssue, DirectorySizeIssueKind, DirectorySizeResult,
    DirectorySizeStats, DirectorySizeStatus,
};
use crate::{Cancel, ErrorCode};
use std::path::PathBuf;

pub const SFTP_DIRECTORY_SIZE_LIMITATION: &str = "SFTP-Ordnergröße · Best Effort: beobachteter Bestand, keine atomare Momentaufnahme. SFTP v3 bindet OPENDIR nicht atomar an den Pane-Root; ein unbemerkter Pfadtausch zwischen den Prüfungen bleibt möglich. Pro Ordner höchstens 1000 Einträge und 16 Unterordnerebenen; höchstens 100 direkte Ordner und 60 Sekunden je Anfrage zuzüglich laufendem Request/Cleanup. Symlinks und nicht belegbare Metadaten werden ausgelassen. Kein Dateiinhalt wird geöffnet. Jede Berechnung liest frisch; kein dauerhafter Größencache.";

impl SftpVfs {
    /// Liefert pro ausgewähltem Ordner eine frische beobachtete Bilanz. Ein Fehler
    /// entwertet nur diesen Ordner; Abbruch und Sitzungsgeneration gelten für den
    /// gesamten Lauf. Eingaben werden vor Providerzugriff geprüft. Der gemeinsame
    /// Metadatenscan öffnet keine Dateiinhalte und gibt kein VFS nach außen frei.
    pub fn measure_interactive_directory_sizes(
        self: &Arc<Self>,
        root: &Path,
        names: &[String],
        cancel: &Cancel,
        on_result: &mut dyn FnMut(&DirectorySizeResult),
    ) -> io::Result<DirectorySizeStats> {
        let paths = names.iter().map(|name| root.join(name)).collect::<Vec<_>>();
        crate::sftp::validate_interactive_content_selection(root, &paths)?;
        let bounded = cancel.until(std::time::Instant::now() + Duration::from_secs(60));
        let generation = self.session_generation();
        let mut stats = DirectorySizeStats {
            requested: paths.len() as u64,
            ..Default::default()
        };
        for path in paths {
            let measured = (|| {
                check_generation(self, generation)?;
                let tree = self.interactive_metadata_summary(root, &path, &bounded)?;
                check_generation(self, generation)?;
                let issues = tree
                    .omitted
                    .iter()
                    .map(|omitted| DirectorySizeIssue {
                        path: PathBuf::from(&omitted.path),
                        entry_name: None,
                        kind: DirectorySizeIssueKind::Listing,
                        code: ErrorCode::Unsupported,
                        message: omitted.reason.clone(),
                    })
                    .collect::<Vec<_>>();
                Ok(DirectorySizeResult {
                    path: path.clone(),
                    bytes: tree.bytes,
                    files: tree.files,
                    directories: tree.directories.saturating_sub(1),
                    skipped: tree.omitted.len() as u64,
                    status: if issues.is_empty() {
                        DirectorySizeStatus::Observed
                    } else {
                        DirectorySizeStatus::Partial
                    },
                    issues,
                    cached: false,
                    limitations: vec![SFTP_DIRECTORY_SIZE_LIMITATION.into()],
                })
            })();
            let measured = measured.map_err(|error| {
                if bounded.is_cancelled() && !cancel.is_cancelled() {
                    crate::coded_error(
                        io::ErrorKind::TimedOut,
                        ErrorCode::ConnectionLost,
                        "SFTP-Größenberechnung erreichte die Gesamtfrist von 60 Sekunden",
                    )
                } else {
                    error
                }
            });
            let result = measured.unwrap_or_else(|error| DirectorySizeResult {
                path: path.clone(),
                bytes: 0,
                files: 0,
                directories: 0,
                skipped: 0,
                status: if cancel.is_cancelled() {
                    DirectorySizeStatus::Cancelled
                } else {
                    DirectorySizeStatus::Failed
                },
                issues: vec![DirectorySizeIssue {
                    path,
                    entry_name: None,
                    kind: DirectorySizeIssueKind::Listing,
                    code: crate::error_code(&error),
                    message: error.to_string(),
                }],
                cached: false,
                limitations: vec![SFTP_DIRECTORY_SIZE_LIMITATION.into()],
            });
            record_result(&mut stats, &result);
            on_result(&result);
        }
        Ok(stats)
    }
}

fn check_generation(provider: &SftpVfs, expected: u64) -> io::Result<()> {
    if provider.session_generation() != expected {
        Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::PlanDrift,
            "SFTP-Sitzung wechselte während der Größenberechnung",
        ))
    } else {
        Ok(())
    }
}
