//! Gemeinsamer lesender READDIR-Transport. Namen bleiben bis zur Auswertung
//! Datenwerte; jeder geöffnete Handle wird auch im Fehlerpfad genau einmal geschlossen.

use super::*;
use crate::{Cancel, ErrorCode};
use russh_sftp::protocol::FileAttributes;
use std::collections::HashSet;

pub(super) struct DirectoryReadLimits {
    pub entries: usize,
    pub name_bytes: usize,
    pub pages: usize,
    pub duplicate_error_kind: io::ErrorKind,
}

pub(super) async fn read_records(
    raw: &RawSftpSession,
    remote: &str,
    cancel: &Cancel,
    limits: DirectoryReadLimits,
    record_close: impl FnOnce(io::Result<()>) -> io::Result<()>,
) -> io::Result<Vec<(String, FileAttributes)>> {
    check_sftp_cancel(Some(cancel))?;
    let handle = raw
        .opendir(remote.to_owned())
        .await
        .map_err(sftp_error)?
        .handle;
    let result = async {
        let mut entries = Vec::new();
        let mut names = HashSet::new();
        let mut name_bytes = 0usize;
        for _ in 0..limits.pages {
            check_sftp_cancel(Some(cancel))?;
            let page = match raw.readdir(handle.clone()).await {
                Ok(page) if !page.files.is_empty() => page,
                Ok(_) => {
                    return Err(crate::coded_error(
                        io::ErrorKind::InvalidData,
                        ErrorCode::PlanDrift,
                        "SFTP-READDIR lieferte eine leere NAME-Antwort statt EOF",
                    ))
                }
                Err(russh_sftp::client::error::Error::Status(status))
                    if status.status_code == StatusCode::Eof =>
                {
                    return Ok(entries)
                }
                Err(error) => return Err(sftp_error(error)),
            };
            check_sftp_cancel(Some(cancel))?;
            for file in page.files {
                if file.filename == "." || file.filename == ".." {
                    continue;
                }
                if !names.insert(file.filename.clone()) {
                    return Err(io::Error::new(
                        limits.duplicate_error_kind,
                        "SFTP-Listing enthält einen doppelten Namen",
                    ));
                }
                name_bytes = name_bytes
                    .checked_add(file.filename.len())
                    .ok_or_else(|| invalid("SFTP-Namensumfang überschreitet den Zahlenbereich"))?;
                if entries.len() >= limits.entries || name_bytes > limits.name_bytes {
                    return Err(invalid(format!(
                        "SFTP-Ordner überschreitet {} Einträge oder {} Bytes Namen",
                        limits.entries, limits.name_bytes
                    )));
                }
                // SFTP-Langnamen werden nicht ausgewertet. Der schmale Datensatz
                // verwirft sie pro Antwort statt sie bis zum EOF anzusammeln.
                entries.push((file.filename, file.attrs));
            }
        }
        Err(invalid(
            "SFTP-READDIR überschreitet die begrenzte Seitenzahl ohne EOF",
        ))
    }
    .await;
    // Ein sekundärer Closeverlust muss auch bei primärem Abbruch die nächste
    // Lesesitzung entwerten; der primäre Fehler bleibt dabei erhalten.
    let closed = record_close(raw.close(handle).await.map(|_| ()).map_err(sftp_error));
    result.and_then(|entries| closed.map(|_| entries))
}
