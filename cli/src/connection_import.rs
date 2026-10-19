//! Deterministischer, pass-store-unabhängiger Parser für Verbindungsdaten auf stdin.
//!
//! Dieses Modul kennt weder `pass` noch einen Pfad im Passwortspeicher. Es nimmt nur
//! einen begrenzten UTF-8-Text entgegen und gibt erkannte Geheimnisse niemals über
//! `Debug`, Fehlermeldungen oder eine Vorschaufunktion aus.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::net::Ipv6Addr;
use std::str::FromStr;

use percent_encoding::percent_decode_str;
use url::Url;
use zeroize::Zeroizing;

const MAX_INPUT_BYTES: u64 = 64 * 1024;
const MAX_LINES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImportProtocol {
    Sftp,
    Ftp,
    Ftps,
}

impl ImportProtocol {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Ftp => "ftp",
            Self::Ftps => "ftps",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Sftp => 22,
            Self::Ftp | Self::Ftps => 21,
        }
    }
}

pub(crate) struct ParsedConnection {
    pub(crate) username: String,
    pub(crate) password: Zeroizing<String>,
    endpoints: Vec<ParsedEndpoint>,
    explicit_protocol: Option<ImportProtocol>,
}

pub(crate) struct ResolvedConnection {
    pub(crate) protocol: ImportProtocol,
    pub(crate) host: String,
    pub(crate) alternate_hosts: Vec<String>,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) password: Zeroizing<String>,
    pub(crate) requested_root: String,
    pub(crate) name: String,
    pub(crate) id: String,
}

struct ParsedEndpoint {
    host: String,
    port: Option<u16>,
    requested_root: String,
    root_specified: bool,
    protocol: Option<ImportProtocol>,
    primary: bool,
}

#[derive(Clone, Copy)]
enum Field {
    Username,
    Password,
    Endpoint { primary: bool },
}

pub(crate) fn read_and_parse(reader: impl Read) -> io::Result<ParsedConnection> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(bytes.as_mut())
        .map_err(|_| invalid("Verbindungsdaten konnten nicht von stdin gelesen werden"))?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(invalid("Verbindungsdaten auf stdin sind zu groß"));
    }
    let text = match String::from_utf8(std::mem::take(bytes.as_mut())) {
        Ok(text) => text,
        Err(error) => {
            let _invalid_bytes = Zeroizing::new(error.into_bytes());
            return Err(invalid(
                "Verbindungsdaten auf stdin sind kein gültiges UTF-8",
            ));
        }
    };
    parse_text(Zeroizing::new(text))
}

fn parse_text(text: Zeroizing<String>) -> io::Result<ParsedConnection> {
    if text.contains('\0') {
        return Err(invalid("Verbindungsdaten enthalten ein NUL-Zeichen"));
    }
    let lines = text
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() || lines.len() > MAX_LINES {
        return Err(invalid(
            "Verbindungsdaten fehlen oder enthalten zu viele Zeilen",
        ));
    }

    let mut username = None;
    let mut password = None;
    let mut endpoint_values = Vec::new();
    let mut unlabeled = Vec::new();
    for line in lines {
        match split_labeled_line(line) {
            Some((Field::Username, value)) => set_once(&mut username, value.trim(), "Nutzername")?,
            Some((Field::Password, value)) => set_once(&mut password, value, "Passwort")?,
            Some((Field::Endpoint { primary }, value)) => {
                let value = value.trim();
                if value.is_empty() {
                    return Err(invalid("Eine Verbindungsadresse ist leer"));
                }
                endpoint_values.push((value, primary));
            }
            None => unlabeled.push(line),
        }
    }

    // Übliche `pass`-Form: erste unbenannte Zeile ist das Passwort, danach folgen
    // benannte Felder. Fehlen alle Bezeichner, wird nur die eindeutige Dreierform
    // Passwort, Nutzername, Adresse akzeptiert.
    if username.is_none() && password.is_none() && endpoint_values.is_empty() {
        if unlabeled.len() != 3 || !looks_like_endpoint(unlabeled[2]) {
            return Err(ambiguous());
        }
        password = Some(unlabeled[0]);
        username = Some(unlabeled[1].trim());
        endpoint_values.push((unlabeled[2].trim(), true));
        unlabeled.clear();
    }

    if !unlabeled.is_empty() {
        if unlabeled.len() != 1 {
            return Err(ambiguous());
        }
        let value = unlabeled[0];
        match (
            username.is_none(),
            password.is_none(),
            endpoint_values.is_empty(),
        ) {
            (true, false, false) => username = Some(value.trim()),
            (false, true, false) => password = Some(value),
            (false, false, true) => endpoint_values.push((value.trim(), true)),
            _ => return Err(ambiguous()),
        }
    }

    let username = required(username, "Nutzername")?;
    let password = required(password, "Passwort")?;
    if endpoint_values.is_empty() {
        return Err(invalid("Verbindungsadresse fehlt"));
    }
    validate_username(username)?;

    let mut endpoints = endpoint_values
        .into_iter()
        .map(|(value, primary)| parse_endpoint(value, primary))
        .collect::<io::Result<Vec<_>>>()?;
    let explicit_protocol = endpoints
        .iter()
        .filter_map(|endpoint| endpoint.protocol)
        .try_fold(None, |current, protocol| match current {
            None => Ok(Some(protocol)),
            Some(existing) if existing == protocol => Ok(current),
            Some(_) => Err(invalid(
                "Verbindungsadressen nennen widersprüchliche Protokolle",
            )),
        })?;

    // Eine allgemeine beziehungsweise IPv4-Adresse ist der logische TLS-/SSH-Name.
    // Eine ausdrücklich als IPv6 gekennzeichnete Zeile bleibt Alternative.
    if let Some(primary_index) = endpoints.iter().position(|endpoint| endpoint.primary) {
        endpoints.swap(0, primary_index);
    }

    Ok(ParsedConnection {
        username: username.to_owned(),
        password: Zeroizing::new(password.to_owned()),
        endpoints,
        explicit_protocol,
    })
}

impl ParsedConnection {
    pub(crate) fn candidate_protocols(&self) -> &'static [ImportProtocol] {
        const SFTP: &[ImportProtocol] = &[ImportProtocol::Sftp];
        const FTP: &[ImportProtocol] = &[ImportProtocol::Ftp];
        const FTPS: &[ImportProtocol] = &[ImportProtocol::Ftps];
        const AUTO: &[ImportProtocol] = &[ImportProtocol::Sftp, ImportProtocol::Ftps];
        match self.explicit_protocol {
            Some(ImportProtocol::Sftp) => SFTP,
            Some(ImportProtocol::Ftp) => FTP,
            Some(ImportProtocol::Ftps) => FTPS,
            None => AUTO,
        }
    }

    pub(crate) fn resolve(&self, protocol: ImportProtocol) -> io::Result<ResolvedConnection> {
        if self
            .explicit_protocol
            .is_some_and(|explicit| explicit != protocol)
        {
            return Err(invalid("Verbindungsprotokoll passt nicht zur Adresse"));
        }
        let primary = self
            .endpoints
            .first()
            .ok_or_else(|| invalid("Verbindungsadresse fehlt"))?;
        let port = primary.port.unwrap_or_else(|| protocol.default_port());
        let root = primary.requested_root.clone();
        let mut hosts = Vec::new();
        let mut seen = BTreeSet::new();
        for endpoint in &self.endpoints {
            let endpoint_root = if endpoint.root_specified {
                endpoint.requested_root.as_str()
            } else {
                root.as_str()
            };
            if endpoint
                .protocol
                .is_some_and(|explicit| explicit != protocol)
                || endpoint.port.unwrap_or(port) != port
                || endpoint_root != root
            {
                return Err(invalid(
                    "Alternativadressen müssen Protokoll, Port und Root teilen",
                ));
            }
            let normalized = endpoint.host.trim_end_matches('.').to_ascii_lowercase();
            if seen.insert(normalized.clone()) {
                hosts.push(normalized);
            }
        }
        let host = hosts
            .first()
            .cloned()
            .ok_or_else(|| invalid("Verbindungsadresse fehlt"))?;
        let alternate_hosts = hosts.into_iter().skip(1).collect::<Vec<_>>();
        let name = generated_name(&host, &self.username);
        let id = stable_id(
            protocol,
            &host,
            &alternate_hosts,
            port,
            &root,
            &self.username,
        );
        Ok(ResolvedConnection {
            protocol,
            host,
            alternate_hosts,
            port,
            username: self.username.clone(),
            password: self.password.clone(),
            requested_root: root,
            name,
            id,
        })
    }
}

fn split_labeled_line(line: &str) -> Option<(Field, &str)> {
    for (index, character) in line.char_indices() {
        if character != ':' && character != '=' {
            continue;
        }
        let label = normalize_label(&line[..index]);
        let field = match label.as_str() {
            "user" | "username" | "name" | "benutzer" => Field::Username,
            "passwort" | "pw" | "pass" | "password" | "secret" => Field::Password,
            "url" | "server" | "adresse" | "uri" | "urlipv4" | "serveripv4" | "adresseipv4"
            | "uriipv4" => Field::Endpoint { primary: true },
            "urlipv6" | "serveripv6" | "adresseipv6" | "uriipv6" | "ipv6" => {
                Field::Endpoint { primary: false }
            }
            _ => return None,
        };
        return Some((field, line[index + character.len_utf8()..].trim_start()));
    }
    None
}

fn normalize_label(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_endpoint(raw: &str, primary: bool) -> io::Result<ParsedEndpoint> {
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return Err(invalid("Verbindungsadresse ist ungültig"));
    }
    let explicit_scheme = raw
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    let protocol = match explicit_scheme.as_deref() {
        Some("sftp") => Some(ImportProtocol::Sftp),
        Some("ftp") => Some(ImportProtocol::Ftp),
        Some("ftps") => Some(ImportProtocol::Ftps),
        Some(_) => return Err(invalid("Verbindungsprotokoll wird nicht unterstützt")),
        None => None,
    };
    let parseable = if explicit_scheme.is_some() {
        raw.to_owned()
    } else if Ipv6Addr::from_str(raw).is_ok() {
        format!("sftp://[{raw}]")
    } else {
        format!("sftp://{raw}")
    };
    let url = Url::parse(&parseable).map_err(|_| invalid("Verbindungsadresse ist ungültig"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "Verbindungsadresse darf keine Zugangsdaten, Query oder Fragment enthalten",
        ));
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| invalid("Verbindungsadresse enthält keinen Host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let root_specified = !url.path().is_empty();
    let requested_root = percent_decode_str(url.path())
        .decode_utf8()
        .map_err(|_| invalid("Remote-Root ist nicht gültiges UTF-8"))?
        .into_owned();
    let requested_root = if requested_root.is_empty() {
        "/".to_owned()
    } else {
        requested_root
    };
    if !requested_root.starts_with('/')
        || requested_root.len() > 4096
        || requested_root.split('/').any(|part| part == "..")
    {
        return Err(invalid("Remote-Root ist ungültig"));
    }
    Ok(ParsedEndpoint {
        host,
        port: url.port(),
        requested_root,
        root_specified,
        protocol,
        primary,
    })
}

fn looks_like_endpoint(value: &str) -> bool {
    value.contains("://")
        || value.parse::<std::net::IpAddr>().is_ok()
        || value.starts_with('[')
        || value.contains('.')
}

fn validate_username(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(invalid("Nutzername ist ungültig"));
    }
    Ok(())
}

fn generated_name(host: &str, username: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in format!("{host}-{username}").chars() {
        if character.is_alphanumeric() || matches!(character, '.' | '_' | '@') {
            result.push(character);
            separator = false;
        } else if !separator && !result.is_empty() {
            result.push('-');
            separator = true;
        }
        if result.len() >= 120 {
            break;
        }
    }
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "remote-verbindung".to_owned()
    } else {
        result
    }
}

fn stable_id(
    protocol: ImportProtocol,
    host: &str,
    alternates: &[String],
    port: u16,
    root: &str,
    username: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [protocol.label(), host, &port.to_string(), root, username] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    for alternate in alternates {
        hasher.update(&(alternate.len() as u64).to_be_bytes());
        hasher.update(alternate.as_bytes());
    }
    format!("import-{}", &hasher.finalize().to_hex()[..24])
}

fn set_once<'text>(
    target: &mut Option<&'text str>,
    value: &'text str,
    field: &str,
) -> io::Result<()> {
    if target.replace(value).is_some() {
        return Err(invalid(format!("{field} ist mehrfach angegeben")));
    }
    if value.is_empty() {
        return Err(invalid(format!("{field} ist leer")));
    }
    Ok(())
}

fn required<'text>(value: Option<&'text str>, field: &str) -> io::Result<&'text str> {
    value.ok_or_else(|| invalid(format!("{field} fehlt")))
}

fn ambiguous() -> io::Error {
    invalid("Verbindungsdaten sind mehrdeutig; bitte user, password und url beschriften")
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_and_ipv6_alternative() {
        let parsed = read_and_parse(
            b"Benutzer: marion\nsecret: eine-testfolge\nAdresse: ftps://files.example.test/daten\nurl ipv6: 2001:db8::7\n"
                .as_slice(),
        )
        .unwrap();
        let resolved = parsed.resolve(ImportProtocol::Ftps).unwrap();
        assert_eq!(resolved.host, "files.example.test");
        assert_eq!(resolved.alternate_hosts, ["2001:db8::7"]);
        assert_eq!(resolved.port, 21);
        assert_eq!(resolved.requested_root, "/daten");
        assert_eq!(resolved.name, "files.example.test-marion");
        assert!(!resolved.id.contains("marion"));
    }

    #[test]
    fn accepts_unlabeled_password_with_named_fields() {
        let parsed = read_and_parse(
            b"eine-testfolge\nuser name: alice\nserver: sftp://host.example.test/\n".as_slice(),
        )
        .unwrap();
        assert_eq!(parsed.candidate_protocols(), &[ImportProtocol::Sftp]);
        assert_eq!(parsed.username, "alice");
    }

    #[test]
    fn unbeschriftete_passwortzeile_bleibt_bytegenau() {
        let parsed = read_and_parse(
            b"  eine testfolge  \nuser: alice\nurl: sftp://host.example.test/\n".as_slice(),
        )
        .unwrap();
        assert_eq!(parsed.password.as_str(), "  eine testfolge  ");
    }

    #[test]
    fn accepts_three_line_pass_convention() {
        let parsed =
            read_and_parse(b"eine-testfolge\nalice\nhost.example.test\n".as_slice()).unwrap();
        assert_eq!(
            parsed.candidate_protocols(),
            &[ImportProtocol::Sftp, ImportProtocol::Ftps]
        );
        let resolved = parsed.resolve(ImportProtocol::Sftp).unwrap();
        assert_eq!(resolved.port, 22);
    }

    #[test]
    fn ftp_und_ftps_schemata_waehlen_getrennte_transportmodi() {
        let ftp = read_and_parse(b"pw: x\nuser: alice\nurl: ftp://host.example.test/\n".as_slice())
            .unwrap();
        assert_eq!(ftp.candidate_protocols(), &[ImportProtocol::Ftp]);
        assert_eq!(ftp.resolve(ImportProtocol::Ftp).unwrap().port, 21);

        let ftps =
            read_and_parse(b"pw: x\nuser: alice\nurl: ftps://host.example.test/\n".as_slice())
                .unwrap();
        assert_eq!(ftps.candidate_protocols(), &[ImportProtocol::Ftps]);
        assert!(ftps.resolve(ImportProtocol::Ftp).is_err());
    }

    #[test]
    fn genau_ein_fehlendes_feld_wird_per_ausschluss_erkannt() {
        let endpoint =
            read_and_parse(b"user: alice\npw: eine-testfolge\nhost.example.test\n".as_slice())
                .unwrap();
        assert_eq!(
            endpoint.resolve(ImportProtocol::Sftp).unwrap().host,
            "host.example.test"
        );

        let username =
            read_and_parse(b"password: eine-testfolge\nuri: host.example.test\nalice\n".as_slice())
                .unwrap();
        assert_eq!(username.username, "alice");
    }

    #[test]
    fn rejects_ambiguous_text_without_echoing_input() {
        let marker = "VERTRAULICHER-MARKER";
        let error = read_and_parse(format!("{marker}\nalice\nnot-an-address\n").as_bytes())
            .err()
            .unwrap();
        assert!(!error.to_string().contains(marker));
    }

    #[test]
    fn rejects_query_and_conflicting_protocols() {
        let with_query =
            read_and_parse(b"pw: x\nuser: alice\nurl: sftp://host.example.test/?x=1\n".as_slice())
                .err()
                .unwrap();
        assert_eq!(with_query.kind(), io::ErrorKind::InvalidInput);

        let conflicting = read_and_parse(
            b"pw: x\nuser: alice\nurl: sftp://host.example.test/\nurl ipv6: ftps://[2001:db8::7]/\n"
                .as_slice(),
        )
        .err()
        .unwrap();
        assert_eq!(conflicting.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn generated_identity_is_stable_and_sanitized() {
        let first =
            read_and_parse(b"pw: x\nuser: name / test\nurl: host.example.test\n".as_slice())
                .unwrap()
                .resolve(ImportProtocol::Sftp)
                .unwrap();
        let second = read_and_parse(
            b"password: x\nusername: name / test\nserver: host.example.test\n".as_slice(),
        )
        .unwrap()
        .resolve(ImportProtocol::Sftp)
        .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.name, "host.example.test-name-test");
    }
}
