//! Bedingtes Lesen exakt begrenzter WebDAV-Bytebereiche.
use super::*;
use crate::Cancel;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, RANGE};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebDavFileVersion {
    pub(crate) etag: String,
    pub(crate) size: u64,
    pub(crate) mtime: Option<i64>,
}

impl WebDavFileVersion {
    pub(crate) fn validate(&self) -> io::Result<()> {
        validate_strong_etag(&self.etag)
    }
}

impl WebDavVfs {
    pub(crate) fn resume_file_version(&self, path: &Path) -> io::Result<WebDavFileVersion> {
        interactive_namespace::validate_path(path)?;
        let proof = self.stat_proof(path)?;
        if !proof.entry.is_file || proof.entry.is_symlink {
            return Err(invalid(
                "Ein fortsetzbarer WebDAV-Download benötigt eine reguläre Datei",
            ));
        }
        let version = WebDavFileVersion {
            etag: proof
                .provider_token
                .and_then(|value| String::from_utf8(value).ok())
                .ok_or_else(|| invalid("WebDAV-Datei besitzt keine starke ETag"))?,
            size: proof.entry.size,
            mtime: proof.entry.mtime,
        };
        version.validate()?;
        Ok(version)
    }

    pub(crate) fn read_resume_range(
        &self,
        path: &Path,
        version: &WebDavFileVersion,
        offset: u64,
        buffer: &mut [u8],
        cancel: &Cancel,
    ) -> io::Result<()> {
        check_cancel(cancel)?;
        version.validate()?;
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|end| *end <= version.size)
            .ok_or_else(|| invalid("WebDAV-Lesebereich verlässt die gebundene Datei"))?;
        let empty = buffer.is_empty();
        if empty && (offset != 0 || version.size != 0) {
            return Err(invalid("Leerer Bereich einer nicht leeren WebDAV-Datei"));
        }
        let (session, generation) = {
            let state = lock(&self.state);
            (state.session.clone(), state.generation)
        };
        let remote = map_virtual_path(&session.identity.canonical_root, path)?;
        let url = resource_url(&session.identity.origin, &remote)?;
        let mut request = authenticated(session.http.client.get(url), &session.http)
            .header(IF_MATCH, version.etag.as_str())
            .header(ACCEPT_ENCODING, "identity");
        if !empty {
            request = request.header(RANGE, format!("bytes={offset}-{}", end - 1));
        }
        let mut response = request.send().map_err(http_error)?;
        check_cancel(cancel)?;
        verify_range_reply(response.status(), response.headers(), version, offset, end)?;
        for part in buffer.chunks_mut(256 * 1024) {
            check_cancel(cancel)?;
            response.read_exact(part).map_err(|error| io::Error::new(
                error.kind(),
                "WebDAV-Teilantwort konnte nicht vollständig gelesen werden. Der gesicherte Abschnitt bleibt erhalten.",
            ))?;
        }
        check_cancel(cancel)?;
        let remaining = response.read(&mut [0]).map_err(|error| {
            io::Error::new(
                error.kind(),
                "Das Ende der WebDAV-Teilantwort konnte nicht bestätigt werden.",
            )
        })?;
        if remaining != 0 {
            return Err(invalid(
                "WebDAV-Teilantwort liefert mehr Bytes als angekündigt",
            ));
        }
        if self.session_generation() != generation {
            return Err(crate::real_mode::plan_drift(
                "WebDAV-Sitzung wechselte während des Lesens",
            ));
        }
        Ok(())
    }
}

fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "WebDAV-Bytebereich wurde abgebrochen",
        ));
    }
    Ok(())
}

fn verify_range_reply(
    status: StatusCode,
    headers: &HeaderMap,
    version: &WebDavFileVersion,
    start: u64,
    end: u64,
) -> io::Result<()> {
    if matches!(
        status,
        StatusCode::PRECONDITION_FAILED | StatusCode::RANGE_NOT_SATISFIABLE
    ) {
        return Err(crate::real_mode::plan_drift(
            "WebDAV-Dateiversion oder Dateigröße änderte sich seit der Vorbereitung.",
        ));
    }
    let expected = if end == start {
        StatusCode::OK
    } else {
        StatusCode::PARTIAL_CONTENT
    };
    if status == StatusCode::OK && expected != StatusCode::OK {
        return Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "Der WebDAV-Server liefert keinen bedingten Bytebereich. Gewöhnliches Kopieren bleibt verfügbar.",
        ));
    }
    require_status(status, &[expected], "WebDAV-Download")?;
    require_matching_etag(headers, &version.etag, "WebDAV-Download")?;
    if headers.contains_key(CONTENT_ENCODING)
        && !required_header(headers, CONTENT_ENCODING, "Content-Encoding")?
            .eq_ignore_ascii_case("identity")
    {
        return Err(invalid(
            "WebDAV-Teilantwort verwendet eine andere Inhaltskodierung",
        ));
    }
    if end != start {
        let expected_range = format!("bytes {start}-{}/{}", end - 1, version.size);
        if !required_header(headers, CONTENT_RANGE, "Content-Range")?
            .eq_ignore_ascii_case(&expected_range)
        {
            return Err(invalid(
                "WebDAV-Teilantwort stimmt nicht mit dem angeforderten Bytebereich überein",
            ));
        }
    } else if headers.contains_key(CONTENT_RANGE) {
        return Err(invalid(
            "WebDAV lieferte bei leerer Datei eine unerwartete Bereichsangabe",
        ));
    }
    if headers.contains_key(CONTENT_LENGTH)
        && required_header(headers, CONTENT_LENGTH, "Content-Length")?
            .parse::<u64>()
            .ok()
            != Some(end - start)
    {
        return Err(invalid("WebDAV-Teilantwort besitzt eine abweichende Länge"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_reply_requires_exact_partial_content_and_original_version() {
        let version = WebDavFileVersion {
            etag: "\"v1\"".into(),
            size: 100,
            mtime: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("\"v1\""));
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 20-49/100"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("30"));
        assert!(
            verify_range_reply(StatusCode::PARTIAL_CONTENT, &headers, &version, 20, 50).is_ok()
        );
        assert_eq!(
            crate::error_code(
                &verify_range_reply(StatusCode::OK, &headers, &version, 20, 50).unwrap_err()
            ),
            ErrorCode::Unsupported,
        );
        assert_eq!(
            crate::error_code(
                &verify_range_reply(StatusCode::PRECONDITION_FAILED, &headers, &version, 20, 50)
                    .unwrap_err()
            ),
            ErrorCode::PlanDrift,
        );
        for (name, wrong) in [
            (ETAG, "\"v2\""),
            (CONTENT_RANGE, "bytes 0-29/100"),
            (CONTENT_RANGE, "bytes 20-49/101"),
            (CONTENT_LENGTH, "31"),
            (CONTENT_ENCODING, "gzip"),
        ] {
            let mut changed = headers.clone();
            changed.insert(name, HeaderValue::from_static(wrong));
            assert!(
                verify_range_reply(StatusCode::PARTIAL_CONTENT, &changed, &version, 20, 50)
                    .is_err()
            );
        }
        let mut ambiguous = headers.clone();
        ambiguous.append(CONTENT_RANGE, HeaderValue::from_static("bytes 20-49/100"));
        assert!(
            verify_range_reply(StatusCode::PARTIAL_CONTENT, &ambiguous, &version, 20, 50).is_err()
        );
    }
}
