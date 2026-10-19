//! Begrenzte DNS-SD-/mDNS-Erkennung für die drei unterstützten LAN-Protokolle.
//!
//! Ein Fund ist ausdrücklich kein Identitäts- oder Erreichbarkeitsbeweis. Diese
//! Schicht liefert nur Anzeigename, angekündigten `.local`-Host und Port an den
//! bestehenden Verbindungseditor; dessen Hostkey-, Zertifikats- beziehungsweise
//! SMB-GUID-Prüfung bleibt unverändert Pflicht.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const MDNS_ADDRESS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353);
const MAX_PACKET_BYTES: usize = 65_535;
const MAX_RECORDS: usize = 512;
/// Harte Grenze über das gesamte Suchfenster. Ohne sie könnte ein LAN-Teilnehmer viele
/// einzeln gültige Pakete schicken und Speicher sowie spätere Auswertung ungebunden
/// wachsen lassen.
const MAX_DISCOVERY_RECORDS: usize = 2_048;
const MAX_DISCOVERY_INSTANCES: usize = 256;
const MAX_DISCOVERED_SERVICES: usize = 256;
const MAX_NAME_POINTERS: usize = 32;
const MAX_DISCOVERY_WINDOW: Duration = Duration::from_secs(5);
const PTR_RECORD: u16 = 12;
const SRV_RECORD: u16 = 33;
const INTERNET_CLASS: u16 = 1;

const SERVICE_TYPES: [(&str, NetworkServiceProtocol); 3] = [
    ("_smb._tcp.local", NetworkServiceProtocol::Smb),
    ("_sftp-ssh._tcp.local", NetworkServiceProtocol::Sftp),
    ("_webdav._tcp.local", NetworkServiceProtocol::WebDav),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkServiceProtocol {
    Smb,
    Sftp,
    WebDav,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredNetworkService {
    pub protocol: NetworkServiceProtocol,
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DnsName {
    labels: Vec<Vec<u8>>,
}

impl DnsName {
    fn from_ascii(value: &str) -> Self {
        Self {
            labels: value
                .split('.')
                .map(|label| label.as_bytes().to_vec())
                .collect(),
        }
    }

    fn eq_ascii_case(&self, other: &Self) -> bool {
        self.labels.len() == other.labels.len()
            && self.labels.iter().zip(&other.labels).all(|(left, right)| {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(a, b)| a.eq_ignore_ascii_case(b))
            })
    }

    fn ends_with_ascii_case(&self, suffix: &Self) -> bool {
        self.labels.len() >= suffix.labels.len()
            && Self {
                labels: self.labels[self.labels.len() - suffix.labels.len()..].to_vec(),
            }
            .eq_ascii_case(suffix)
    }

    fn canonical(&self) -> Vec<u8> {
        let mut value = Vec::new();
        for label in &self.labels {
            value.extend(label.iter().map(u8::to_ascii_lowercase));
            value.push(0);
        }
        value
    }

    fn utf8_labels(labels: &[Vec<u8>]) -> Option<String> {
        let parts = labels
            .iter()
            .map(|label| std::str::from_utf8(label).ok())
            .collect::<Option<Vec<_>>>()?;
        let value = parts.join(".");
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            None
        } else {
            Some(value)
        }
    }

    fn service_name(&self, service_type: &Self) -> Option<String> {
        if self.labels.len() <= service_type.labels.len()
            || !self.ends_with_ascii_case(service_type)
        {
            return None;
        }
        Self::utf8_labels(&self.labels[..self.labels.len() - service_type.labels.len()])
    }

    fn local_host(&self) -> Option<String> {
        if self.labels.len() < 2
            || !self
                .labels
                .last()
                .is_some_and(|label| label.eq_ignore_ascii_case(b"local"))
            || self.labels.iter().any(|label| {
                label.is_empty()
                    || !label.is_ascii()
                    || label.first() == Some(&b'-')
                    || label.last() == Some(&b'-')
                    || label
                        .iter()
                        .any(|byte| !byte.is_ascii_alphanumeric() && *byte != b'-')
            })
        {
            return None;
        }
        Self::utf8_labels(&self.labels)
    }
}

#[derive(Debug, Default)]
struct DiscoveryRecords {
    pointers: Vec<(DnsName, DnsName)>,
    services: Vec<(DnsName, DnsName, u16)>,
}

impl DiscoveryRecords {
    fn len(&self) -> usize {
        self.pointers.len().saturating_add(self.services.len())
    }
}

fn extend_unique_bounded(
    records: &mut DiscoveryRecords,
    parsed: DiscoveryRecords,
    pointer_keys: &mut HashSet<(Vec<u8>, Vec<u8>)>,
    service_keys: &mut HashSet<(Vec<u8>, Vec<u8>, u16)>,
) -> bool {
    for (owner, target) in parsed.pointers {
        let key = (owner.canonical(), target.canonical());
        if pointer_keys.insert(key) {
            if records.len() >= MAX_DISCOVERY_RECORDS {
                return false;
            }
            records.pointers.push((owner, target));
        }
    }
    for (owner, target, port) in parsed.services {
        let key = (owner.canonical(), target.canonical(), port);
        if service_keys.insert(key) {
            if records.len() >= MAX_DISCOVERY_RECORDS {
                return false;
            }
            records.services.push((owner, target, port));
        }
    }
    true
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_u16(packet: &[u8], offset: usize) -> io::Result<u16> {
    let bytes = packet
        .get(offset..offset + 2)
        .ok_or_else(|| invalid_data("DNS-Paket endet mitten in einem u16"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(packet: &[u8], offset: usize) -> io::Result<u32> {
    let bytes = packet
        .get(offset..offset + 4)
        .ok_or_else(|| invalid_data("DNS-Paket endet mitten in einem u32"))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Liest einen möglicherweise komprimierten DNS-Namen. `next` bezeichnet immer
/// das erste Byte hinter der ursprünglichen Darstellung, nicht hinter einem
/// angesprungenen Ziel. Dadurch kann der Aufrufer die RDATA-Grenze exakt prüfen.
fn read_name(packet: &[u8], start: usize) -> io::Result<(DnsName, usize)> {
    let mut labels = Vec::new();
    let mut position = start;
    let mut next = None;
    let mut visited = HashSet::new();
    let mut expanded_bytes = 1usize;

    loop {
        if !visited.insert(position) || visited.len() > MAX_NAME_POINTERS {
            return Err(invalid_data("DNS-Namenskompression enthält eine Schleife"));
        }
        let length = *packet
            .get(position)
            .ok_or_else(|| invalid_data("DNS-Name endet ohne Abschluss"))?;
        if length & 0xc0 == 0xc0 {
            let low = *packet
                .get(position + 1)
                .ok_or_else(|| invalid_data("DNS-Kompressionszeiger ist abgeschnitten"))?;
            let pointer = (usize::from(length & 0x3f) << 8) | usize::from(low);
            if pointer >= packet.len() {
                return Err(invalid_data(
                    "DNS-Kompressionszeiger liegt außerhalb des Pakets",
                ));
            }
            next.get_or_insert(position + 2);
            position = pointer;
            continue;
        }
        if length & 0xc0 != 0 {
            return Err(invalid_data(
                "DNS-Name verwendet einen unbekannten Labeltyp",
            ));
        }
        position += 1;
        if length == 0 {
            return Ok((DnsName { labels }, next.unwrap_or(position)));
        }
        let length = usize::from(length);
        if length > 63 {
            return Err(invalid_data("DNS-Label ist länger als 63 Byte"));
        }
        let label = packet
            .get(position..position + length)
            .ok_or_else(|| invalid_data("DNS-Label ist abgeschnitten"))?;
        expanded_bytes = expanded_bytes.saturating_add(length + 1);
        if expanded_bytes > 255 {
            return Err(invalid_data(
                "Expandierter DNS-Name ist länger als 255 Byte",
            ));
        }
        labels.push(label.to_vec());
        position += length;
    }
}

fn parse_records(packet: &[u8]) -> io::Result<DiscoveryRecords> {
    if packet.len() < 12 {
        return Err(invalid_data("DNS-Paket ist kürzer als der Header"));
    }
    if read_u16(packet, 2)? & 0x8000 == 0 {
        return Err(invalid_data("DNS-Paket ist keine Antwort"));
    }
    let question_count = usize::from(read_u16(packet, 4)?);
    let answer_count = usize::from(read_u16(packet, 6)?);
    let authority_count = usize::from(read_u16(packet, 8)?);
    let additional_count = usize::from(read_u16(packet, 10)?);
    let record_count = answer_count
        .checked_add(authority_count)
        .and_then(|count| count.checked_add(additional_count))
        .ok_or_else(|| invalid_data("DNS-Recordanzahl läuft über"))?;
    if question_count > MAX_RECORDS || record_count > MAX_RECORDS {
        return Err(invalid_data("DNS-Paket enthält zu viele Einträge"));
    }

    let mut position = 12usize;
    for _ in 0..question_count {
        let (_, next) = read_name(packet, position)?;
        position = next
            .checked_add(4)
            .filter(|end| *end <= packet.len())
            .ok_or_else(|| invalid_data("DNS-Frage ist abgeschnitten"))?;
    }

    let mut records = DiscoveryRecords::default();
    for _ in 0..record_count {
        let (owner, next) = read_name(packet, position)?;
        position = next;
        let record_type = read_u16(packet, position)?;
        let class = read_u16(packet, position + 2)? & 0x7fff;
        let ttl = read_u32(packet, position + 4)?;
        let data_length = usize::from(read_u16(packet, position + 8)?);
        let data_start = position + 10;
        let data_end = data_start
            .checked_add(data_length)
            .filter(|end| *end <= packet.len())
            .ok_or_else(|| invalid_data("DNS-RDATA ist abgeschnitten"))?;
        position = data_end;
        if class != INTERNET_CLASS || ttl == 0 {
            continue;
        }
        match record_type {
            PTR_RECORD => {
                let (target, end) = read_name(packet, data_start)?;
                if end != data_end {
                    return Err(invalid_data("PTR-RDATA besitzt überzählige Bytes"));
                }
                records.pointers.push((owner, target));
            }
            SRV_RECORD if data_length >= 7 => {
                let port = read_u16(packet, data_start + 4)?;
                let (target, end) = read_name(packet, data_start + 6)?;
                if end != data_end {
                    return Err(invalid_data("SRV-RDATA besitzt überzählige Bytes"));
                }
                if port != 0 {
                    records.services.push((owner, target, port));
                }
            }
            _ => {}
        }
    }
    Ok(records)
}

fn append_name(buffer: &mut Vec<u8>, name: &DnsName) -> io::Result<()> {
    let mut encoded = 1usize;
    for label in &name.labels {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid_data("DNS-Abfragename enthält ein ungültiges Label"));
        }
        encoded = encoded.saturating_add(label.len() + 1);
        if encoded > 255 {
            return Err(invalid_data("DNS-Abfragename ist länger als 255 Byte"));
        }
        buffer.push(label.len() as u8);
        buffer.extend_from_slice(label);
    }
    buffer.push(0);
    Ok(())
}

fn query(names: &[(DnsName, u16)]) -> io::Result<Vec<u8>> {
    let count = u16::try_from(names.len()).map_err(|_| invalid_data("Zu viele DNS-Fragen"))?;
    let mut packet = vec![0, 0, 0, 0];
    packet.extend_from_slice(&count.to_be_bytes());
    packet.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    for (name, record_type) in names {
        append_name(&mut packet, name)?;
        packet.extend_from_slice(&record_type.to_be_bytes());
        packet.extend_from_slice(&INTERNET_CLASS.to_be_bytes());
    }
    Ok(packet)
}

fn services_from(records: &DiscoveryRecords) -> Vec<DiscoveredNetworkService> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    // SRV-Records einmal nach ihrem kanonischen Owner indexieren. Die frühere
    // Pointer×SRV-Schleife wurde bei vielen gültigen Records quadratisch.
    let mut services_by_owner: HashMap<Vec<u8>, Vec<(&DnsName, u16)>> = HashMap::new();
    for (owner, target, port) in &records.services {
        services_by_owner
            .entry(owner.canonical())
            .or_default()
            .push((target, *port));
    }
    'service_types: for (service_text, protocol) in SERVICE_TYPES {
        let service_type = DnsName::from_ascii(service_text);
        for (owner, instance) in &records.pointers {
            if !owner.eq_ascii_case(&service_type) {
                continue;
            }
            let Some(name) = instance.service_name(&service_type) else {
                continue;
            };
            let Some(services) = services_by_owner.get(&instance.canonical()) else {
                continue;
            };
            for (target, port) in services {
                let Some(host) = target.local_host() else {
                    continue;
                };
                let key = (protocol, instance.canonical(), target.canonical(), *port);
                if seen.insert(key) {
                    result.push(DiscoveredNetworkService {
                        protocol,
                        name: name.clone(),
                        host,
                        port: *port,
                    });
                    if result.len() >= MAX_DISCOVERED_SERVICES {
                        break 'service_types;
                    }
                }
            }
        }
    }
    result.sort_by(|left, right| {
        left.protocol
            .cmp(&right.protocol)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.host.cmp(&right.host))
            .then_with(|| left.port.cmp(&right.port))
    });
    result
}

/// Sendet genau drei feste mDNS-PTR-Anfragen. Fehlerhafte oder fremde Antworten
/// werden ignoriert; echte Socketfehler bleiben sichtbar. Das Suchfenster ist auf
/// höchstens fünf Sekunden begrenzt.
pub fn discover_network_services(window: Duration) -> io::Result<Vec<DiscoveredNetworkService>> {
    if window.is_zero() || window > MAX_DISCOVERY_WINDOW {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mDNS-Suche benötigt eine Laufzeit zwischen 1 ms und 5 s",
        ));
    }
    let initial_names = SERVICE_TYPES
        .iter()
        .map(|(service, _)| (DnsName::from_ascii(service), PTR_RECORD))
        .collect::<Vec<_>>();
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_nonblocking(true)?;
    socket.send_to(&query(&initial_names)?, MDNS_ADDRESS)?;

    let deadline = Instant::now() + window;
    let mut buffer = vec![0u8; MAX_PACKET_BYTES];
    let mut records = DiscoveryRecords::default();
    let mut queried_instances = HashSet::new();
    let mut pointer_keys = HashSet::new();
    let mut service_keys = HashSet::new();
    'search: while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match socket.recv_from(&mut buffer) {
            Ok((size, source)) => {
                if source.port() != MDNS_ADDRESS.port() {
                    continue;
                }
                let Ok(parsed) = parse_records(&buffer[..size]) else {
                    continue;
                };
                for (owner, instance) in &parsed.pointers {
                    if SERVICE_TYPES
                        .iter()
                        .any(|(service, _)| owner.eq_ascii_case(&DnsName::from_ascii(service)))
                        && queried_instances.len() < MAX_DISCOVERY_INSTANCES
                        && queried_instances.insert(instance.canonical())
                    {
                        // Manche Antworten enthalten zunächst nur den PTR. Eine
                        // einzelne gezielte SRV-Nachfrage holt dann Host und Port.
                        if let Ok(follow_up) = query(&[(instance.clone(), SRV_RECORD)]) {
                            let _ = socket.send_to(&follow_up, MDNS_ADDRESS);
                        }
                    }
                }
                if !extend_unique_bounded(
                    &mut records,
                    parsed,
                    &mut pointer_keys,
                    &mut service_keys,
                ) {
                    break 'search;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(services_from(&records))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(packet: &mut Vec<u8>, owner: &DnsName, record_type: u16, data: Vec<u8>) {
        append_name(packet, owner).unwrap();
        packet.extend_from_slice(&record_type.to_be_bytes());
        packet.extend_from_slice(&INTERNET_CLASS.to_be_bytes());
        packet.extend_from_slice(&120u32.to_be_bytes());
        packet.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
        packet.extend_from_slice(&data);
    }

    fn ptr_data(target: &DnsName) -> Vec<u8> {
        let mut data = Vec::new();
        append_name(&mut data, target).unwrap();
        data
    }

    fn srv_data(port: u16, target: &DnsName) -> Vec<u8> {
        let mut data = vec![0, 0, 0, 0];
        data.extend_from_slice(&port.to_be_bytes());
        append_name(&mut data, target).unwrap();
        data
    }

    fn response(records: Vec<(DnsName, u16, Vec<u8>)>) -> Vec<u8> {
        let count = u16::try_from(records.len()).unwrap();
        let mut packet = vec![0, 0, 0x84, 0, 0, 0];
        packet.extend_from_slice(&count.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, 0]);
        for (owner, record_type, data) in records {
            record(&mut packet, &owner, record_type, data);
        }
        packet
    }

    #[test]
    fn ptr_und_srv_werden_nur_fuer_feste_local_dienste_verbunden() {
        let smb = DnsName::from_ascii("_smb._tcp.local");
        let instance = DnsName::from_ascii("Arbeitszimmer._smb._tcp.local");
        let host = DnsName::from_ascii("nas.local");
        let foreign_instance = DnsName::from_ascii("Fremd._smb._tcp.local");
        let external_host = DnsName::from_ascii("example.com");
        let parsed = parse_records(&response(vec![
            (smb.clone(), PTR_RECORD, ptr_data(&instance)),
            (instance.clone(), SRV_RECORD, srv_data(445, &host)),
            (smb.clone(), PTR_RECORD, ptr_data(&foreign_instance)),
            (
                foreign_instance.clone(),
                SRV_RECORD,
                srv_data(445, &external_host),
            ),
            (
                DnsName::from_ascii("_printer._tcp.local"),
                PTR_RECORD,
                ptr_data(&DnsName::from_ascii("Drucker._printer._tcp.local")),
            ),
        ]))
        .unwrap();
        assert_eq!(
            services_from(&parsed),
            vec![DiscoveredNetworkService {
                protocol: NetworkServiceProtocol::Smb,
                name: "Arbeitszimmer".into(),
                host: "nas.local".into(),
                port: 445,
            }]
        );
    }

    #[test]
    fn kompressionsschleife_und_ueberzaehlige_rdata_scheitern_geschlossen() {
        let loop_packet = [0u8, 0, 0x80, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0xc0, 0x0c];
        assert_eq!(
            parse_records(&loop_packet).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let service = DnsName::from_ascii("_sftp-ssh._tcp.local");
        let instance = DnsName::from_ascii("Server._sftp-ssh._tcp.local");
        let mut data = ptr_data(&instance);
        data.push(7);
        assert_eq!(
            parse_records(&response(vec![(service, PTR_RECORD, data)]))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn gueltige_namenskompression_wird_expandiert() {
        let packet = [
            3, b'n', b'a', b's', 5, b'l', b'o', b'c', b'a', b'l', 0, 6, b'S', b'e', b'r', b'v',
            b'e', b'r', 0xc0, 0,
        ];
        let (name, next) = read_name(&packet, 11).unwrap();
        assert_eq!(next, packet.len());
        assert_eq!(
            DnsName::utf8_labels(&name.labels).unwrap(),
            "Server.nas.local"
        );
    }

    #[test]
    fn feste_abfrage_enthaelt_genau_die_drei_beschlossenen_ptr_fragen() {
        let names = SERVICE_TYPES
            .iter()
            .map(|(service, _)| (DnsName::from_ascii(service), PTR_RECORD))
            .collect::<Vec<_>>();
        let packet = query(&names).unwrap();
        assert_eq!(read_u16(&packet, 4).unwrap(), 3);
        let mut position = 12;
        for (expected, _) in names {
            let (actual, next) = read_name(&packet, position).unwrap();
            assert!(actual.eq_ascii_case(&expected));
            assert_eq!(read_u16(&packet, next).unwrap(), PTR_RECORD);
            assert_eq!(read_u16(&packet, next + 2).unwrap(), INTERNET_CLASS);
            position = next + 4;
        }
        assert_eq!(position, packet.len());
    }

    #[test]
    fn laufweite_recordgrenze_und_fruehe_deduplizierung_bleiben_hart() {
        let owner = DnsName::from_ascii("_smb._tcp.local");
        let mut parsed = DiscoveryRecords::default();
        for index in 0..(MAX_DISCOVERY_RECORDS + 100) {
            parsed.pointers.push((
                owner.clone(),
                DnsName::from_ascii(&format!("Dienst-{index}._smb._tcp.local")),
            ));
        }
        let duplicate = parsed.pointers[0].clone();
        parsed.pointers.insert(1, duplicate);

        let mut accumulated = DiscoveryRecords::default();
        let mut pointer_keys = HashSet::new();
        let mut service_keys = HashSet::new();
        assert!(!extend_unique_bounded(
            &mut accumulated,
            parsed,
            &mut pointer_keys,
            &mut service_keys,
        ));
        assert_eq!(accumulated.len(), MAX_DISCOVERY_RECORDS);
        assert_eq!(pointer_keys.len(), MAX_DISCOVERY_RECORDS + 1);
    }

    #[test]
    fn dienstabbildung_bleibt_auch_bei_vielen_records_begrenzt() {
        let owner = DnsName::from_ascii("_smb._tcp.local");
        let host = DnsName::from_ascii("nas.local");
        let mut records = DiscoveryRecords::default();
        for index in 0..(MAX_DISCOVERED_SERVICES + 50) {
            let instance = DnsName::from_ascii(&format!("Dienst-{index}._smb._tcp.local"));
            records.pointers.push((owner.clone(), instance.clone()));
            records.services.push((instance, host.clone(), 445));
        }

        let services = services_from(&records);
        assert_eq!(services.len(), MAX_DISCOVERED_SERVICES);
        assert!(services.iter().all(|service| {
            service.protocol == NetworkServiceProtocol::Smb
                && service.host == "nas.local"
                && service.port == 445
        }));
    }

    #[test]
    #[ignore = "sendet bewusst aktive mDNS-Anfragen ins aktuelle LAN"]
    fn reale_socket_suche_bleibt_auf_das_kurze_suchfenster_begrenzt() {
        discover_network_services(Duration::from_millis(150)).unwrap();
    }
}
