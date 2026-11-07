//! Enger Zeitsetter: genau eine bedingte Propertymutation, niemals Wiederholung.
use super::*;

pub(super) fn set_time(
    session: &WebDavSession,
    remote: &str,
    etag: &str,
    parents: &[WebDavResourceIdentity],
    mtime: i64,
    collection: bool,
) -> io::Result<Option<String>> {
    if !(0..=253_402_300_799).contains(&mtime) {
        return Ok(Some(
            "WebDAV-HTTP-Datum kann diesen Zeitwert außerhalb 1970–9999 nicht darstellen".into(),
        ));
    }
    let timestamp = u64::try_from(mtime)
        .ok()
        .and_then(|seconds| std::time::UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
        .ok_or_else(|| invalid("WebDAV-Dateizeit liegt außerhalb des HTTP-Datumsbereichs"))?;
    let date = httpdate::fmt_http_date(timestamp);
    let body = format!("<?xml version=\"1.0\"?><D:propertyupdate xmlns:D=\"DAV:\"><D:set><D:prop><D:getlastmodified>{date}</D:getlastmodified></D:prop></D:set></D:propertyupdate>");
    let response = with_parent_condition(
        authenticated(
            session.http.mutation_client.request(
                custom_method(b"PROPPATCH")?,
                resource_url(&session.identity.origin, remote)?,
            ),
            &session.http,
        ),
        &session.identity.origin,
        parents,
        remote,
        if collection {
            WebDavRequestClass::OrdinaryCollection
        } else {
            WebDavRequestClass::OrdinaryFile
        },
    )?;
    let response = if collection {
        response
    } else {
        response.header(IF_MATCH, etag)
    };
    let mut response = response
        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
        .timeout(TLS_IO_TIMEOUT)
        .body(body)
        .send()
        .map_err(http_error)?;
    let status = response.status();
    if matches!(status.as_u16(), 403 | 405 | 409 | 422 | 501) {
        return Ok(Some(format!(
            "WebDAV lehnt PROPPATCH für DAV:getlastmodified mit HTTP {} ab",
            status.as_u16()
        )));
    }
    require_status(status, &[StatusCode::MULTI_STATUS], "WebDAV-Zeit-PROPPATCH")?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut response)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(invalid("WebDAV-Zeitantwort überschreitet 64 KiB"));
    }
    let status = property_status(&bytes, &session.identity.origin, remote)?;
    if (200..300).contains(&status) {
        return Ok(None);
    }
    if matches!(status, 403 | 405 | 409 | 422 | 424 | 501) {
        return Ok(Some(format!("WebDAV lehnt DAV:getlastmodified in der vollständigen Propertyantwort mit HTTP {status} ab")));
    }
    Err(invalid(format!(
        "WebDAV-Zeitproperty meldet unerwarteten Status {status}"
    )))
}

fn property_status(bytes: &[u8], origin: &str, remote: &str) -> io::Result<u16> {
    let mut reader = NsReader::from_reader(bytes);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut buffer = Vec::new();
    let mut counts = BTreeMap::<Vec<u8>, usize>::new();
    let mut href = String::new();
    let mut status = String::new();
    let mut closed = false;
    loop {
        let (ns, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|e| invalid(e.to_string()))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                if closed || !is_dav_namespace(&ns) {
                    return Err(invalid(
                        "WebDAV-Zeitantwort besitzt fremde oder nachgeschobene Elemente",
                    ));
                }
                let name = e.local_name().as_ref().to_vec();
                let parent = stack.last().map(Vec::as_slice).unwrap_or_default();
                let allowed = matches!(
                    (parent, name.as_slice()),
                    (b"", b"multistatus")
                        | (b"multistatus", b"response")
                        | (b"response", b"href" | b"propstat" | b"responsedescription")
                        | (
                            b"propstat",
                            b"prop" | b"status" | b"error" | b"responsedescription"
                        )
                        | (b"prop", b"getlastmodified")
                        | (b"error", b"cannot-modify-protected-property")
                );
                if !allowed {
                    return Err(invalid(
                        "WebDAV-Zeitantwort hat eine unerwartete Propertystruktur",
                    ));
                }
                if name != b"responsedescription" {
                    let count = counts.entry(name.clone()).or_default();
                    *count += 1;
                    if *count != 1 {
                        return Err(invalid(
                            "WebDAV-Zeitantwort enthält doppelte Ressourcen oder Properties",
                        ));
                    }
                }
                if matches!(event, Event::Start(_)) {
                    stack.push(name);
                }
            }
            Event::End(ref e) => {
                if !is_dav_namespace(&ns) || stack.pop().as_deref() != Some(e.local_name().as_ref())
                {
                    return Err(invalid("WebDAV-Zeitantwort ist unbalanciert"));
                }
                if stack.is_empty() {
                    closed = true;
                }
            }
            Event::Text(e) => {
                let text = e.decode().map_err(|e| invalid(e.to_string()))?;
                match stack.last().map(Vec::as_slice) {
                    Some(b"href") => href.push_str(
                        &quick_xml::escape::unescape(&text).map_err(|e| invalid(e.to_string()))?,
                    ),
                    Some(b"status") => status.push_str(&text),
                    Some(b"responsedescription" | b"getlastmodified") => {}
                    _ if text.trim().is_empty() => {}
                    _ => return Err(invalid("WebDAV-Zeitantwort enthält unerwarteten Text")),
                }
            }
            Event::Decl(_) if stack.is_empty() && counts.is_empty() => {}
            Event::Eof => break,
            _ => {
                return Err(invalid(
                    "WebDAV-Zeitantwort enthält unerwartete XML-Inhalte",
                ))
            }
        }
        buffer.clear();
    }
    if !closed
        || !stack.is_empty()
        || [
            b"multistatus".as_slice(),
            b"response",
            b"href",
            b"propstat",
            b"prop",
            b"getlastmodified",
            b"status",
        ]
        .iter()
        .any(|key| counts.get(*key) != Some(&1))
        || href_path(origin, &href)?.trim_end_matches('/') != remote.trim_end_matches('/')
    {
        return Err(invalid(
            "WebDAV-Zeitantwort ist unvollständig oder gehört zu einem anderen Pfad",
        ));
    }
    let parts = status.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 2 || !matches!(parts[0], "HTTP/1.1" | "HTTP/1.0") || parts[1].len() != 3 {
        return Err(invalid("WebDAV-Propertystatus ist ungültig"));
    }
    parts[1]
        .parse()
        .map_err(|_| invalid("WebDAV-Propertystatus ist ungültig"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reply(property: &str, status: &str) -> String {
        format!("<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/pane/a</D:href><D:propstat><D:prop>{property}</D:prop><D:status>HTTP/1.1 {status}</D:status></D:propstat></D:response></D:multistatus>")
    }
    #[test]
    fn zeitantwort_verlangt_genau_eine_richtige_property_und_ressource() {
        assert_eq!(
            property_status(
                reply("<D:getlastmodified/>", "200 OK").as_bytes(),
                "https://dav.test",
                "/pane/a"
            )
            .unwrap(),
            200
        );
        assert_eq!(
            property_status(
                reply("<D:getlastmodified/>", "403 Forbidden").as_bytes(),
                "https://dav.test",
                "/pane/a"
            )
            .unwrap(),
            403
        );
        for xml in [
            reply("<D:getlastmodified/><D:getlastmodified/>", "200 OK"),
            reply("<D:getetag/>", "200 OK"),
            reply("<D:getlastmodified/>", "OK"),
            reply("<D:getlastmodified/>", "200 OK").replace("/pane/a", "/pane/b"),
            reply("<D:getlastmodified/>", "200 OK").replace("DAV:", "fremd:"),
        ] {
            assert!(
                property_status(xml.as_bytes(), "https://dav.test", "/pane/a").is_err(),
                "{xml}"
            );
        }
    }
}
