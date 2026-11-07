use super::*;
fn document(body: &str) -> String {
    format!(
        r#"<D:multistatus xmlns:D="DAV:" xmlns:X="urn:foreign"><D:response><D:href>/dav/file.txt</D:href>{body}</D:response></D:multistatus>"#
    )
}
fn props(properties: &str, status: u16) -> String {
    format!("<D:propstat><D:prop>{properties}</D:prop><D:status>HTTP/1.1 {status} Status</D:status></D:propstat>")
}
fn parse_doc(xml: &str) -> io::Result<WebDavSecurityObservation> {
    parse(xml.as_bytes(), "https://dav.test", "/dav/file.txt", [1; 32])
}
#[test]
fn fehlend_verweigert_nicht_exponiert_und_leer_bleiben_unterscheidbar() {
    let missing = parse_doc(&document(&props("<X:acl/><X:owner/>", 200))).unwrap();
    assert_eq!(missing.acl, WebDavPropertyObservation::Missing);
    assert_eq!(missing.owner, WebDavPropertyObservation::Missing);
    let absent = parse_doc(&document(
        &(props("<D:acl/>", 403) + &props("<D:owner/>", 404)),
    ))
    .unwrap();
    assert_eq!(
        absent.acl,
        WebDavPropertyObservation::Unavailable { status: 403 }
    );
    assert_eq!(
        absent.owner,
        WebDavPropertyObservation::Unavailable { status: 404 }
    );
    let empty = parse_doc(&document(&props("<D:acl/><D:owner/>", 200))).unwrap();
    assert!(matches!(
        empty.acl,
        WebDavPropertyObservation::Observed { empty: true, .. }
    ));
    assert!(matches!(
        empty.owner,
        WebDavPropertyObservation::Observed { empty: true, .. }
    ));
    assert_ne!(empty.acl, missing.acl);
}
#[test]
fn principal_href_ist_keine_ressourcenadresse_und_werte_bleiben_opaque() {
    let xml = document(&props("<D:acl><D:ace><D:principal><D:href>https://principal.test/private</D:href></D:principal><D:grant><D:privilege><D:read/></D:privilege></D:grant></D:ace></D:acl><D:owner><D:href>/principals/private</D:href></D:owner>", 200));
    let observation = parse_doc(&xml).unwrap();
    assert!(matches!(
        observation.acl,
        WebDavPropertyObservation::Observed { empty: false, .. }
    ));
    let serialized = serde_json::to_string(&observation).unwrap();
    assert!(!serialized.contains("private"));
    assert!(!serialized.contains("principal.test"));
}
#[test]
fn kanonischer_digest_ignoriert_praefixe_und_attributreihenfolge() {
    let first = document(&props(
        "<D:acl><X:unknown X:z='two' a='one'>Text&amp;more</X:unknown></D:acl>",
        200,
    ));
    let second = first
        .replace("xmlns:X=\"urn:foreign\"", "xmlns:Y=\"urn:foreign\"")
        .replace("X:", "Y:")
        .replace("Y:z='two' a='one'", "a='one' Y:z='two'");
    assert_eq!(
        parse_doc(&first).unwrap().acl,
        parse_doc(&second).unwrap().acl
    );
    let changed = first.replace("Text&amp;more", "changed");
    assert_ne!(
        parse_doc(&first).unwrap().acl,
        parse_doc(&changed).unwrap().acl
    );
}
#[test]
fn ace_reihenfolge_namespace_und_attribute_sind_digestgebunden() {
    let first = document(&props("<D:acl><X:a/><X:b/></D:acl>", 200));
    for changed in [
        first.replace("<X:a/><X:b/>", "<X:b/><X:a/>"),
        first.replace("urn:foreign", "urn:other"),
        first.replace("<X:a/>", "<X:a a='1'/>"),
    ] {
        assert_ne!(
            parse_doc(&first).unwrap().acl,
            parse_doc(&changed).unwrap().acl
        );
    }
}
#[test]
fn doppelte_status_und_falsche_strukturen_sind_fehler() {
    let good = document(&props("<D:acl/>", 200));
    for bad in [
        good.replace("<D:acl/>", "<D:acl/><D:acl/>"),
        document(&(props("<D:acl/>", 200) + &props("<D:acl/>", 404))),
        good.replace(
            "<D:status>",
            "<D:status>HTTP/1.1 200 OK</D:status><D:status>",
        ),
        good.replace("<D:propstat>", "<D:propstat><D:prop/>"),
        good.replace("<D:prop>", "<D:prop><D:prop>"),
        good.replace("</D:prop>", "</D:prop></D:prop>"),
        good.replace("<D:propstat>", "<X:propstat>"),
        good.replace(
            "<D:href>/dav/file.txt</D:href>",
            "<D:href>/dav/file.txt</D:href><D:href>/dav/file.txt</D:href>",
        ),
        good.replace(
            "<D:response>",
            "<D:response><D:status>HTTP/1.1 404 Missing</D:status>",
        ),
        document(&props("<D:acl><D:ace/></D:acl>", 403)),
        document(""),
    ] {
        assert!(parse_doc(&bad).is_err(), "{bad}");
    }
}
#[test]
fn falsche_ressourcen_herkunft_und_literal_prozentnamen() {
    let good = document(&props("<D:acl/>", 404));
    for href in [
        "https://foreign.test/dav/file.txt",
        "/dav/other",
        "https://user@dav.test/dav/file.txt",
        "/dav/file.txt?query=1",
        "/dav/file.txt#fragment",
    ] {
        assert!(
            parse_doc(&good.replace("/dav/file.txt", href)).is_err(),
            "{href}"
        );
    }
    let literal = good.replace("/dav/file.txt", "/dav/literal%252F.txt");
    assert!(parse(
        literal.as_bytes(),
        "https://dav.test",
        "/dav/literal%2F.txt",
        [1; 32]
    )
    .is_ok());
    assert!(parse(
        literal.as_bytes(),
        "https://dav.test",
        "/dav/literal/name.txt",
        [1; 32]
    )
    .is_err());
}
#[test]
fn ressourcenstatus_wird_separat_bewahrt() {
    let got = parse_doc(&document("<D:status>HTTP/1.1 404 Missing</D:status>")).unwrap();
    assert_eq!(got.query_status, 207);
    assert_eq!(got.resource_status, Some(404));
    assert_eq!(got.acl, WebDavPropertyObservation::Missing);
}
#[test]
fn xml_grenzen_dtd_und_fremde_struktur_werden_abgelehnt() {
    let good = document(&props("<D:acl/>", 200));
    for bad in [
        format!("<!DOCTYPE multistatus [<!ENTITY a 'secret'>]>{good}"),
        good.replace("<D:acl/>", "<D:acl>&unknown;</D:acl>"),
        good.replace("DAV:", "urn:wrong"),
        good.replace("</D:multistatus>", ""),
        format!("{good}{good}"),
        good.replace("<D:acl/>", &format!("<D:acl>{}</D:acl>", "x".repeat(LIMIT))),
        good.replace(
            "<D:acl/>",
            &format!(
                "<D:acl>{}{}</D:acl>",
                "<D:n>".repeat(128),
                "</D:n>".repeat(128)
            ),
        ),
        good.replace("<D:acl/>", "<unknown:acl/>"),
        good.replace("urn:foreign", "&unknown;"),
        good.replace("<D:acl/>", "<D:acl>\0</D:acl>"),
        good.replace("<D:acl/>", "<D:acl>&#0;</D:acl>"),
        good.replace("<D:acl/>", "<D:acl x='&#0;'/>"),
    ] {
        assert!(parse_doc(&bad).is_err());
    }
}

#[test]
fn alle_gueltigen_property_und_ressourcenstatus_bleiben_erhalten() {
    for status in [100, 201, 401, 405, 500, 501, 599] {
        let got = parse_doc(&document(&props("<D:acl/>", status))).unwrap();
        assert_eq!(got.acl, WebDavPropertyObservation::Unavailable { status });
    }
    for status in [100, 200, 405, 500, 501, 599] {
        let got = parse_doc(&document(&format!(
            "<D:status>HTTP/1.1 {status} Status</D:status>"
        )))
        .unwrap();
        assert_eq!(got.resource_status, Some(status));
        assert_eq!(got.acl, WebDavPropertyObservation::Missing);
    }
    for status in [99, 600, 2000] {
        assert!(parse_doc(&document(&props("<D:acl/>", status))).is_err());
    }
}

#[test]
fn namespace_attribute_werden_einmal_xml_normalisiert() {
    let first = document(&props("<D:acl><X:ä X:é='value'/></D:acl>", 200))
        .replace("urn:foreign", "urn:ä&#58;foreign");
    let second = first
        .replace("DAV:", "DAV&#58;")
        .replace("urn:ä&#58;foreign", "urn:ä:foreign")
        .replace("xmlns:X=", "xmlns:Y=")
        .replace("X:", "Y:");
    assert_eq!(
        parse_doc(&first).unwrap().acl,
        parse_doc(&second).unwrap().acl
    );
    let single_escape = first.replace("urn:ä&#58;foreign", "urn:a&amp;#58;");
    let numeric_escape = first.replace("urn:ä&#58;foreign", "urn:a&#38;#58;");
    let unescaped_colon = first.replace("urn:ä&#58;foreign", "urn:a:");
    assert_eq!(
        parse_doc(&single_escape).unwrap().acl,
        parse_doc(&numeric_escape).unwrap().acl
    );
    assert_ne!(
        parse_doc(&single_escape).unwrap().acl,
        parse_doc(&unescaped_colon).unwrap().acl
    );
}
#[test]
fn qnames_mit_leeren_oder_mehreren_doppelpunkten_sind_fehler() {
    let good = document(&props("<D:acl/>", 200));
    for value in [
        "<D::acl/>",
        "<D:/>",
        "<:acl/>",
        "<D:acl D::x='value'/>",
        "<D:acl xmlns:X:Y='urn:bad'/>",
    ] {
        assert!(
            parse_doc(&good.replace("<D:acl/>", value)).is_err(),
            "{value}"
        );
    }
}

#[test]
fn optionale_dav_fehlerdetails_und_location_sind_keine_ressourcenadresse() {
    let details = "<D:error><D:need-privileges><D:resource><D:href>https://principal.test/private</D:href><D:privilege><D:read-acl/></D:privilege></D:resource></D:need-privileges></D:error>";
    let plain = document(&props("<D:acl/><D:owner/>", 403));
    let with_details = plain.replace(
        "</D:propstat>",
        &format!(
            "{details}<D:responsedescription>Access denied</D:responsedescription></D:propstat>"
        ),
    );
    assert_eq!(
        parse_doc(&plain).unwrap(),
        parse_doc(&with_details).unwrap()
    );
    let resource = document("<D:status>HTTP/1.1 403 Forbidden</D:status>");
    let diagnostic = resource.replace("</D:response>", &format!("{details}<D:location><D:href>https://other.test/elsewhere</D:href></D:location></D:response>"));
    assert_eq!(
        parse_doc(&resource).unwrap(),
        parse_doc(&diagnostic).unwrap()
    );
    let serialized = serde_json::to_string(&parse_doc(&diagnostic).unwrap()).unwrap();
    assert!(!serialized.contains("private"));
    assert!(!serialized.contains("other.test"));
}

#[test]
fn multistatus_beschreibung_aendert_keine_ressource_oder_sicherheitsbeobachtung() {
    let plain = document(&props("<D:acl/><D:owner/>", 200));
    let expected = parse_doc(&plain).unwrap();
    for xml in [
        plain.replace(
            "<D:response>",
            "<D:responsedescription>Diagnose &amp; Text</D:responsedescription><D:response>",
        ),
        plain.replace(
            "</D:multistatus>",
            "<D:responsedescription>Diagnose &amp; Text</D:responsedescription></D:multistatus>",
        ),
    ] {
        assert_eq!(parse_doc(&xml).unwrap(), expected);
    }
}

#[test]
fn multistatus_beschreibung_lockert_weder_kinder_noch_depth_null_grenzen() {
    let plain = document(&props("<D:acl/><D:owner/>", 200));
    for suffix in [
        "<X:responsedescription>Fremd</X:responsedescription>",
        "<D:unknown/>",
        "<D:responsedescription bad='attribute'>Text</D:responsedescription>",
        "<D:responsedescription><D:href>/dav/other</D:href></D:responsedescription>",
        "<D:responsedescription>eins</D:responsedescription><D:responsedescription>zwei</D:responsedescription>",
        "<D:response><D:href>/dav/other</D:href></D:response>",
    ] {
        let xml = plain.replace("</D:multistatus>", &format!("{suffix}</D:multistatus>"));
        assert!(parse_doc(&xml).is_err(), "{suffix}");
    }
    assert!(parse_doc(r#"<D:multistatus xmlns:D="DAV:"><D:responsedescription>Nur Diagnose</D:responsedescription></D:multistatus>"#).is_err());
}

#[test]
fn sprachmarkierte_antwortbeschreibung_bleibt_an_allen_diagnosekanten_ohne_wirkung() {
    let plain = document(&props("<D:acl/><D:owner/>", 200));
    let expected = parse_doc(&plain).unwrap();
    for edge in ["</D:multistatus>", "</D:response>", "</D:propstat>"] {
        for language in ["de", ""] {
            let description = format!("<D:responsedescription xml:lang='{language}'>Diagnose &amp; Text</D:responsedescription>");
            let xml = plain.replace(edge, &format!("{description}{edge}"));
            assert_eq!(
                parse_doc(&xml).unwrap(),
                expected,
                "{edge}, xml:lang={language:?}"
            );
        }
    }
    let combined = ["</D:multistatus>", "</D:response>", "</D:propstat>"]
        .into_iter()
        .fold(plain, |xml, edge| {
            xml.replace(
                edge,
                &format!(
                    "<D:responsedescription xml:lang='de'>Diagnose</D:responsedescription>{edge}"
                ),
            )
        });
    assert_eq!(parse_doc(&combined).unwrap(), expected);
}

#[test]
fn antwortbeschreibung_erlaubt_nur_gebundenes_xml_lang_und_einfachen_text() {
    let plain = document(&props("<D:acl/><D:owner/>", 200));
    for edge in ["</D:multistatus>", "</D:response>", "</D:propstat>"] {
        for description in [
            "<D:responsedescription xml:base='/dav/other'>Text</D:responsedescription>",
            "<D:responsedescription lang='de'>Text</D:responsedescription>",
            "<D:responsedescription X:lang='de'>Text</D:responsedescription>",
            "<D:responsedescription other='value'>Text</D:responsedescription>",
            "<D:responsedescription xml:lang='de' xml:space='preserve'>Text</D:responsedescription>",
            "<D:responsedescription xml:lang='de'><D:href>/dav/other</D:href></D:responsedescription>",
            "<D:responsedescription xml:lang='de'>eins</D:responsedescription><D:responsedescription xml:lang=''>zwei</D:responsedescription>",
        ] {
            let xml = plain.replace(edge, &format!("{description}{edge}"));
            assert!(parse_doc(&xml).is_err(), "{edge}: {description}");
        }
    }
    let rebound = plain
        .replace("<D:response>", "<D:response xmlns:xml='urn:foreign'>")
        .replace(
            "</D:response>",
            "<D:responsedescription xml:lang='de'>Text</D:responsedescription></D:response>",
        );
    assert!(parse_doc(&rebound).is_err());
    let extra_response = plain.replace("</D:multistatus>", "<D:responsedescription xml:lang='de'>Text</D:responsedescription><D:response><D:href>/dav/file.txt</D:href></D:response></D:multistatus>");
    assert!(parse_doc(&extra_response).is_err());
}

#[test]
fn sprachmarkierung_lockert_keine_ressourcen_oder_strukturskalare() {
    let plain = document(&props("<D:acl/><D:owner/>", 200));
    for (before, after) in [
        (
            "<D:multistatus xmlns:D=",
            "<D:multistatus xml:lang='de' xmlns:D=",
        ),
        ("<D:href>", "<D:href xml:lang='de'>"),
        ("<D:status>", "<D:status xml:lang='de'>"),
    ] {
        assert!(
            parse_doc(&plain.replace(before, after)).is_err(),
            "{before}"
        );
    }
}
