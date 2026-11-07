//! Begrenzte, rein lesende DAV-Sicherheitsbeobachtung; kein ACL-Ausführungsvertrag.
use super::*;
use crate::webdav_security::{WebDavPropertyObservation, WebDavSecurityObservation};
use quick_xml::events::BytesStart;

const LIMIT: usize = 1024 * 1024;
const REQUEST: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:acl/><D:owner/></D:prop></D:propfind>"#;

#[derive(Debug, serde::Serialize)]
struct Name(String, String);
#[derive(Debug, serde::Serialize)]
enum Child {
    Element(Node),
    Text(String),
}
#[derive(Debug, serde::Serialize)]
struct Node {
    name: Name,
    attributes: Vec<((String, String), String)>,
    children: Vec<Child>,
}
impl Node {
    fn dav(&self, local: &str) -> bool {
        self.name.0 == "DAV:" && self.name.1 == local
    }
    fn elements(&self) -> impl Iterator<Item = &Node> {
        self.children.iter().filter_map(|c| match c {
            Child::Element(n) => Some(n),
            _ => None,
        })
    }
    fn structure(&self) -> io::Result<()> {
        if !self.attributes.is_empty()
            || self
                .children
                .iter()
                .any(|c| matches!(c, Child::Text(t) if !t.trim().is_empty()))
        {
            return Err(invalid(
                "WebDAV-Sicherheitsantwort hat ungültige Strukturwerte",
            ));
        }
        Ok(())
    }
    fn scalar(&self) -> io::Result<String> {
        if !self.attributes.is_empty() || self.elements().next().is_some() {
            return Err(invalid(
                "WebDAV-Sicherheitsskalar enthält Elemente oder Attribute",
            ));
        }
        Ok(self
            .children
            .iter()
            .filter_map(|c| match c {
                Child::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect())
    }
}
// Die Diagnose wird ignoriert. Ihre Sprachmarkierung darf weder allgemeine
// Ressourcen-/Propertyskalare lockern noch ein neues href als Adresse einführen.
fn validate_response_description(node: &Node) -> io::Result<()> {
    if node.elements().next().is_some()
        || node.attributes.iter().any(|((namespace, local), _)| {
            namespace != "http://www.w3.org/XML/1998/namespace" || local != "lang"
        })
    {
        return Err(invalid(
            "DAV-Antwortbeschreibung enthält ungültige Elemente oder Attribute",
        ));
    }
    Ok(())
}

fn valid_xml_text(value: &str) -> io::Result<()> {
    if value.chars().any(|c| !matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')) {
        return Err(invalid("Ungültiges XML-Zeichen in DAV-Sicherheitsantwort"));
    }
    Ok(())
}
fn valid_qname(raw: &[u8]) -> io::Result<()> {
    let parts = raw.split(|byte| *byte == b':').collect::<Vec<_>>();
    if parts.len() > 2 || parts.iter().any(|part| part.is_empty()) {
        return Err(invalid("XML-QName hat leere oder mehrere Präfixteile"));
    }
    Ok(())
}
fn expanded(
    namespace: ResolveResult<'_>,
    local: &[u8],
    decoder: quick_xml::encoding::Decoder,
) -> io::Result<Name> {
    let ns = match namespace {
        ResolveResult::Bound(ns) => {
            // NsReader liefert rohe xmlns-Attributwerte. Einmalige XML-
            // Normalisierung bindet gleichwertige Namespace-Schreibweisen gleich.
            quick_xml::events::attributes::Attribute {
                key: quick_xml::name::QName(b"xmlns"),
                value: std::borrow::Cow::Borrowed(ns.as_ref()),
            }
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
            .map_err(|_| invalid("Ungültiger XML-Namespace"))?
            .into_owned()
        }
        ResolveResult::Unbound => String::new(),
        ResolveResult::Unknown(_) => return Err(invalid("Ungebundener XML-Namespacepräfix")),
    };
    valid_xml_text(&ns)?;
    Ok(Name(
        ns,
        std::str::from_utf8(local)
            .map_err(|_| invalid("Ungültiger XML-Name"))?
            .to_owned(),
    ))
}
fn node(reader: &NsReader<&[u8]>, event: &BytesStart<'_>) -> io::Result<Node> {
    valid_qname(event.name().as_ref())?;
    let (ns, local) = reader.resolver().resolve_element(event.name());
    let name = expanded(ns, local.as_ref(), reader.decoder())?;
    let mut attributes = BTreeMap::new();
    for attr in event.attributes() {
        let attr = attr.map_err(|_| invalid("Ungültiges oder doppeltes XML-Attribut"))?;
        valid_qname(attr.key.as_ref())?;
        if attr.key.as_ref() == b"xmlns" || attr.key.as_ref().starts_with(b"xmlns:") {
            let value = attr
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                .map_err(|_| invalid("Ungültige XML-Namespace-Deklaration"))?;
            valid_xml_text(&value)?;
            continue;
        }
        let (ns, local) = reader.resolver().resolve_attribute(attr.key);
        let name = expanded(ns, local.as_ref(), reader.decoder())?;
        let value = attr
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| invalid("Ungültiger XML-Attributwert"))?
            .into_owned();
        valid_xml_text(&value)?;
        if attributes.insert((name.0, name.1), value).is_some() {
            return Err(invalid("Doppeltes expandiertes XML-Attribut"));
        }
    }
    Ok(Node {
        name,
        attributes: attributes.into_iter().collect(),
        children: Vec::new(),
    })
}
fn append(stack: &mut [Node], root: &mut Option<Node>, node: Node) -> io::Result<()> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(Child::Element(node));
    } else if root.replace(node).is_some() {
        return Err(invalid("Mehrere XML-Wurzelelemente"));
    }
    Ok(())
}
fn tree(bytes: &[u8]) -> io::Result<Node> {
    if bytes.len() > LIMIT {
        return Err(invalid("WebDAV-Sicherheitsantwort überschreitet 1 MiB"));
    }
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    let mut declaration_seen = false;
    loop {
        match reader
            .read_event()
            .map_err(|_| invalid("Ungültiges WebDAV-Sicherheits-XML"))?
        {
            Event::Start(e) => {
                if stack.len() >= 128 {
                    return Err(invalid("WebDAV-Sicherheits-XML ist zu tief"));
                }
                stack.push(node(&reader, &e)?);
            }
            Event::Empty(e) => append(&mut stack, &mut root, node(&reader, &e)?)?,
            Event::End(e) => {
                let n = stack.pop().ok_or_else(|| invalid("XML-Ende ohne Anfang"))?;
                let (ns, local) = reader.resolver().resolve_element(e.name());
                valid_qname(e.name().as_ref())?;
                let closing = expanded(ns, local.as_ref(), reader.decoder())?;
                if n.name.0 != closing.0 || n.name.1 != closing.1 {
                    return Err(invalid("Widersprüchliches XML-Ende"));
                }
                append(&mut stack, &mut root, n)?;
            }
            Event::Text(e) => {
                let text = e
                    .xml_content(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|_| invalid("Ungültiger XML-Text"))?;
                let text = quick_xml::escape::unescape(&text)
                    .map_err(|_| invalid("Ungültige XML-Entität"))?
                    .into_owned();
                append_text(&mut stack, &text)?;
            }
            Event::CData(e) => append_text(
                &mut stack,
                &e.xml_content(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|_| invalid("Ungültiger XML-CDATA-Text"))?,
            )?,
            Event::GeneralRef(e) => {
                let value = e.decode().map_err(|_| invalid("Ungültige XML-Referenz"))?;
                let escaped = format!("&{value};");
                let value = quick_xml::escape::unescape(&escaped)
                    .map_err(|_| invalid("Unbekannte XML-Entität"))?;
                append_text(&mut stack, &value)?;
            }
            Event::DocType(_) | Event::PI(_) => {
                return Err(invalid(
                    "DTD und Verarbeitungsanweisungen sind in DAV-Sicherheitsantworten unzulässig",
                ))
            }
            Event::Decl(declaration) if !declaration_seen && root.is_none() && stack.is_empty() => {
                declaration_seen = true;
                if declaration
                    .version()
                    .map_err(|_| invalid("Ungültige XML-Deklaration"))?
                    .as_ref()
                    != b"1.0"
                {
                    return Err(invalid("WebDAV-Sicherheitsantwort benötigt XML 1.0"));
                }
            }
            Event::Decl(_) => return Err(invalid("XML-Deklaration an ungültiger Position")),
            Event::Comment(_) => {}
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(invalid("Unvollständiges WebDAV-Sicherheits-XML"));
    }
    root.ok_or_else(|| invalid("Leere WebDAV-Sicherheitsantwort"))
}
fn append_text(stack: &mut [Node], value: &str) -> io::Result<()> {
    valid_xml_text(value)?;
    if let Some(n) = stack.last_mut() {
        if let Some(Child::Text(previous)) = n.children.last_mut() {
            previous.push_str(value);
        } else {
            n.children.push(Child::Text(value.to_owned()));
        }
    } else if !value.trim().is_empty() {
        return Err(invalid("XML-Text außerhalb der Wurzel"));
    }
    Ok(())
}
fn code(node: &Node) -> io::Result<u16> {
    let value = node.scalar()?;
    let code = status_line_code(&value)?;
    let number = value.split_whitespace().nth(1).unwrap_or_default();
    if number.len() != 3
        || !number.bytes().all(|b| b.is_ascii_digit())
        || !(100..=599).contains(&code)
    {
        return Err(invalid("Ungültiger HTTP-Status in DAV-Sicherheitsantwort"));
    }
    Ok(code)
}
fn observation(property: &Node, status: u16) -> io::Result<WebDavPropertyObservation> {
    if status == 200 {
        let empty = property.attributes.is_empty()
            && property
                .children
                .iter()
                .all(|c| matches!(c, Child::Text(t) if t.trim().is_empty()));
        let bytes = serde_json::to_vec(property).map_err(io::Error::other)?;
        Ok(WebDavPropertyObservation::Observed {
            empty,
            digest: *blake3::hash(&bytes).as_bytes(),
        })
    } else if (100..=599).contains(&status) {
        property.structure()?;
        if property.elements().next().is_some() {
            return Err(invalid(
                "Nicht verfügbare DAV-Eigenschaft enthält einen Wert",
            ));
        }
        Ok(WebDavPropertyObservation::Unavailable { status })
    } else {
        Err(invalid("Unerwarteter DAV-Sicherheits-Propertystatus"))
    }
}
fn parse(
    bytes: &[u8],
    origin: &str,
    remote: &str,
    namespace: [u8; 32],
) -> io::Result<WebDavSecurityObservation> {
    let root = tree(bytes)?;
    if !root.dav("multistatus") {
        return Err(invalid("DAV:multistatus fehlt"));
    }
    root.structure()?;
    let mut responses = Vec::new();
    let mut description_seen = false;
    for child in root.elements() {
        if child.dav("response") {
            responses.push(child);
        } else if child.dav("responsedescription") {
            if description_seen {
                return Err(invalid(
                    "Doppelte DAV:responsedescription unter multistatus",
                ));
            }
            validate_response_description(child)?;
            description_seen = true;
        } else {
            return Err(invalid("Ungültiges multistatus-Kind"));
        }
    }
    if responses.len() != 1 {
        return Err(invalid(
            "DAV-Sicherheits-Depth-0 benötigt genau eine response",
        ));
    }
    let response = responses[0];
    response.structure()?;
    let mut href = None;
    let mut resource_status = None;
    let mut acl = None;
    let mut owner = None;
    let mut propstat_count = 0;
    let mut response_description_seen = false;
    let mut property_names = BTreeSet::new();
    for child in response.elements() {
        if child.dav("href") {
            if href.replace(child.scalar()?).is_some() {
                return Err(invalid("Doppelter Ressourcen-href"));
            }
        } else if child.dav("status") {
            if resource_status.replace(code(child)?).is_some() {
                return Err(invalid("Doppelter Ressourcenstatus"));
            }
        } else if child.dav("propstat") {
            propstat_count += 1;
            child.structure()?;
            let mut prop = None;
            let mut status = None;
            let mut propstat_description_seen = false;
            for element in child.elements() {
                if element.dav("prop") {
                    if prop.replace(element).is_some() {
                        return Err(invalid("Doppeltes DAV:prop"));
                    }
                } else if element.dav("status") {
                    if status.replace(code(element)?).is_some() {
                        return Err(invalid("Doppelter propstat-Status"));
                    }
                } else if element.dav("error") {
                    // RFC 4918: optionale Fehlerdetails ändern den Property-Status
                    // nicht. Darin enthaltene hrefs sind keine Ressourcenadresse.
                } else if !element.dav("responsedescription") {
                    return Err(invalid("Ungültiges propstat-Kind"));
                } else {
                    if propstat_description_seen {
                        return Err(invalid("Doppelte DAV:responsedescription unter propstat"));
                    }
                    validate_response_description(element)?;
                    propstat_description_seen = true;
                }
            }
            let prop = prop.ok_or_else(|| invalid("DAV:prop fehlt"))?;
            prop.structure()?;
            let status = status.ok_or_else(|| invalid("DAV:propstat-Status fehlt"))?;
            for property in prop.elements() {
                if !property_names.insert((property.name.0.clone(), property.name.1.clone())) {
                    return Err(invalid("Doppelte DAV-Eigenschaft"));
                }
                let slot = if property.dav("acl") {
                    Some(&mut acl)
                } else if property.dav("owner") {
                    Some(&mut owner)
                } else {
                    None
                };
                if let Some(slot) = slot {
                    if slot.replace(observation(property, status)?).is_some() {
                        return Err(invalid("DAV-Eigenschaft in mehreren propstat-Blöcken"));
                    }
                }
            }
        } else if child.dav("responsedescription") {
            if response_description_seen {
                return Err(invalid("Doppelte DAV:responsedescription unter response"));
            }
            validate_response_description(child)?;
            response_description_seen = true;
        } else if child.dav("error") || child.dav("location") {
            // Diagnose und Umleitungsziel bleiben opak; niemals verfolgen.
        } else {
            return Err(invalid("Ungültiges response-Kind"));
        }
    }
    let href = href.ok_or_else(|| invalid("Ressourcen-href fehlt"))?;
    if href_path(origin, &href)?.trim_end_matches('/') != remote.trim_end_matches('/') {
        return Err(invalid(
            "DAV-Sicherheitsantwort gehört zu einer anderen Ressource",
        ));
    }
    if resource_status.is_some() && propstat_count > 0 {
        return Err(invalid("DAV-response vermischt status und propstat"));
    }
    if resource_status.is_none() && propstat_count == 0 {
        return Err(invalid("DAV-response enthält keinen Status"));
    }
    Ok(WebDavSecurityObservation {
        namespace,
        query_status: 207,
        resource_status,
        acl: acl.unwrap_or(WebDavPropertyObservation::Missing),
        owner: owner.unwrap_or(WebDavPropertyObservation::Missing),
    })
}
fn namespace(session: &WebDavSession) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"doppeldecker-webdav-security-namespace-v1\0");
    for field in [
        session.identity.origin.as_bytes(),
        session.identity.canonical_root.as_bytes(),
        session.http.username.as_bytes(),
        &session.identity.certificate.sha256,
    ] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    *hash.finalize().as_bytes()
}
fn read(session: &WebDavSession, remote: &str) -> io::Result<WebDavSecurityObservation> {
    let method = Method::from_bytes(b"PROPFIND").map_err(io::Error::other)?;
    let url = resource_url(&session.identity.origin, remote)?;
    let mut response = send_metadata_read(url, |url| {
        authenticated(
            session.http.client.request(method.clone(), url),
            &session.http,
        )
        .header(HeaderName::from_static("depth"), "0")
        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(REQUEST)
    })?;
    let status = response.status();
    if status != StatusCode::MULTI_STATUS && !matches!(status.as_u16(), 403 | 404 | 405 | 501) {
        require_status(
            status,
            &[StatusCode::MULTI_STATUS],
            "WebDAV-Sicherheits-PROPFIND",
        )?;
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut response)
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(invalid("WebDAV-Sicherheitsantwort überschreitet 1 MiB"));
    }
    // Auch Absagen werden vollständig und begrenzt gelesen: Ein abgeschnittener
    // HTTP-Body bleibt ein Transportfehler. Der umgebende Leser belegt separat,
    // dass die Ressource vor und nach der ergänzenden Abfrage fortbesteht.
    if matches!(status.as_u16(), 403 | 404 | 405 | 501) {
        return Ok(WebDavSecurityObservation {
            namespace: namespace(session),
            query_status: status.as_u16(),
            resource_status: None,
            acl: WebDavPropertyObservation::Missing,
            owner: WebDavPropertyObservation::Missing,
        });
    }
    parse(&bytes, &session.identity.origin, remote, namespace(session))
}
pub(super) fn manifest(
    session: &WebDavSession,
    remote: &str,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<crate::metadata::MetadataManifest> {
    check()?;
    let before = stat_proof_in_session(session, remote)?;
    let security = read(session, remote)?;
    let after = stat_proof_in_session(session, remote)?;
    check()?;
    // Collection-ETags besitzen hier weiterhin nur den bestehenden Best-Effort-
    // Vertrag; dieser Vergleich behauptet keine atomare Ordneridentität.
    if before.provider_token != after.provider_token
        || serde_json::to_vec(&before.entry).map_err(io::Error::other)?
            != serde_json::to_vec(&after.entry).map_err(io::Error::other)?
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "WebDAV-Ressource änderte sich während der Sicherheitsbeobachtung",
        ));
    }
    crate::metadata::MetadataManifest::new(
        None,
        before.entry.mode.unwrap_or_default() & 0o7777,
        None,
        vec![],
        None,
        None,
        None,
    )?
    .with_webdav_security(Some(security))
}
#[cfg(test)]
#[path = "webdav_security_read_tests.rs"]
mod tests;
