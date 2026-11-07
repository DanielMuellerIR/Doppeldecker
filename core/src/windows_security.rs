//! Windows-SIDs und geordnete DACLs mit begrenzten Encodern, ohne Schreibfreigabe.
//!
//! Drahtformat: MS-DTYP 2.4.2.1/2, 2.4.4.1/2/4, 2.4.5 und 2.4.6. SMB2
//! QUERY_INFO verweist für Security-Antworten auf dieses selbstrelative Format.
//! Owner/Group bleiben eigene Beobachtungen; SACLs werden nur strukturell geprüft.
//! Das Modell berechnet weder effektive Rechte noch eine kanonische ACE-Reihenfolge.
//!
//! Primärquellen:
//! - [SID-Paket](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/f992ad60-0fe4-4b87-9fed-beb478836861)
//! - [ACE_HEADER](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/628ebb1d-c509-4ea0-a10f-77ef97ca4586)
//! - [ACL](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/20233ed8-a6c6-4097-aafa-dd545ed24428)
//! - [Security-Descriptor](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/7d4dac05-9cef-4563-a058-f108abecce1d)
//! - [ACL-Ausrichtung und Freiraum](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-acl)
//! - [SMB2-QUERY_INFO-Antwort](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/3b1b3598-a898-44ca-bfac-2dcae065247f)

use serde::{Deserialize, Serialize};
use std::{fmt, io, ops::Range};

const MAX_DESCRIPTOR_SIZE: usize = 65_536;
const DESCRIPTOR_HEADER_SIZE: usize = 20;
const ACL_HEADER_SIZE: usize = 8;
const ACE_HEADER_SIZE: usize = 4;
const SELF_RELATIVE: u16 = 0x8000;
const DACL_PRESENT: u16 = 0x0004;
const SACL_PRESENT: u16 = 0x0010;
const FILE_WRITE_CONTROL: u16 = 0x1004;
// Daten, Attribute, DELETE, READ_CONTROL, WRITE_DAC und SYNCHRONIZE; kein WRITE_OWNER.
const FILE_OWNER_ACCESS: u32 = 0x0017_01ff;
const FILE_ACCESS_MASK: u32 = 0x001f_01ff;
const FILE_DATA_WRITE_ACCESS: u32 = 0x0000_0006;
const FILE_TEMP_OPTIONAL_ACCESS: u32 = FILE_DATA_WRITE_ACCESS | 0x0000_0038;
// WRITE_DATA, APPEND_DATA, READ_EA, WRITE_EA, EXECUTE, WRITE_ATTRIBUTES,
// DELETE, WRITE_DAC und WRITE_OWNER. FILE_DELETE_CHILD und generische Rechte fehlen bewusst.
const FILE_INPLACE_OPTIONAL_ACCESS: u32 = 0x000d_013e;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
/// DP, DD, DT, SS, DC, DI und PD aus MS-DTYP 2.4.6. Kein Owner-, SACL- oder RM-Bit.
pub const WINDOWS_DACL_CONTROL_MASK: u16 = 0x15cc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SidData")]
pub struct WindowsSid {
    pub revision: u8,
    /// Die sechs IdentifierAuthority-Bytes sind eine Big-Endian-Zahl; RIDs sind LE.
    pub authority: u64,
    pub sub_authorities: Vec<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SidData {
    revision: u8,
    authority: u64,
    sub_authorities: Vec<u32>,
}

impl TryFrom<SidData> for WindowsSid {
    type Error = io::Error;
    fn try_from(value: SidData) -> io::Result<Self> {
        let sid = Self {
            revision: value.revision,
            authority: value.authority,
            sub_authorities: value.sub_authorities,
        };
        sid.validate()?;
        Ok(sid)
    }
}

impl WindowsSid {
    pub fn validate(&self) -> io::Result<()> {
        if self.revision != 1 || self.authority > 0xffff_ffff_ffff {
            return Err(invalid("Ungültige SID-Revision oder IdentifierAuthority"));
        }
        // Die binäre Form erlaubt auch null Subauthorities (MS-DTYP 2.4.2.2).
        // Die engere Stringgrammatik darf eine beobachtete binäre SID nicht verlieren.
        if self.sub_authorities.len() > 15 {
            return Err(invalid("Eine SID erlaubt höchstens 15 Subauthorities"));
        }
        Ok(())
    }

    fn wire_size(&self) -> usize {
        8 + self.sub_authorities.len() * 4
    }
}

impl fmt::Display for WindowsSid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "S-{}-", self.revision)?;
        // MS-DTYP 2.4.2.1: große Authorities mit allen zwölf Hexziffern ausgeben.
        if self.authority < (1_u64 << 32) {
            write!(f, "{}", self.authority)?;
        } else {
            write!(f, "0x{:012x}", self.authority)?;
        }
        for sub_authority in &self.sub_authorities {
            write!(f, "-{sub_authority}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", try_from = "AceData")]
pub enum WindowsAce {
    Allow {
        flags: u8,
        mask: u32,
        sid: WindowsSid,
    },
    Deny {
        flags: u8,
        mask: u32,
        sid: WindowsSid,
    },
    /// Vollständiger ACE-Inhalt hinter dem vier Byte langen ACE_HEADER.
    /// Auch einfache ACEs mit zusätzlichen Bytes bleiben opaque, damit kein Byte
    /// verschwindet. Ihr SID wird trotzdem strukturell geprüft.
    Opaque {
        ace_type: u8,
        flags: u8,
        data: Vec<u8>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum AceData {
    Allow {
        flags: u8,
        mask: u32,
        sid: WindowsSid,
    },
    Deny {
        flags: u8,
        mask: u32,
        sid: WindowsSid,
    },
    Opaque {
        ace_type: u8,
        flags: u8,
        data: Vec<u8>,
    },
}

impl TryFrom<AceData> for WindowsAce {
    type Error = io::Error;
    fn try_from(value: AceData) -> io::Result<Self> {
        let ace = match value {
            AceData::Allow { flags, mask, sid } => Self::Allow { flags, mask, sid },
            AceData::Deny { flags, mask, sid } => Self::Deny { flags, mask, sid },
            AceData::Opaque {
                ace_type,
                flags,
                data,
            } => Self::Opaque {
                ace_type,
                flags,
                data,
            },
        };
        ace.validate()?;
        Ok(ace)
    }
}

impl WindowsAce {
    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::Allow { sid, .. } | Self::Deny { sid, .. } => sid.validate(),
            Self::Opaque { ace_type, data, .. } => {
                let size = data
                    .len()
                    .checked_add(ACE_HEADER_SIZE)
                    .ok_or_else(|| invalid("ACE-Länge läuft über"))?;
                if size > u16::MAX as usize || !size.is_multiple_of(4) {
                    return Err(invalid("Ungültige opaque ACE-Länge"));
                }
                if matches!(ace_type, 0 | 1) {
                    simple_ace_sid(data)?;
                }
                Ok(())
            }
        }
    }

    fn wire_size(&self) -> usize {
        match self {
            Self::Allow { sid, .. } | Self::Deny { sid, .. } => 8 + sid.wire_size(),
            Self::Opaque { data, .. } => ACE_HEADER_SIZE + data.len(),
        }
    }

    fn ace_type(&self) -> u8 {
        match self {
            Self::Allow { .. } => 0,
            Self::Deny { .. } => 1,
            Self::Opaque { ace_type, .. } => *ace_type,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", try_from = "DaclStateData")]
pub enum WindowsDaclState {
    Absent,
    Null,
    Acl { revision: u8, aces: Vec<WindowsAce> },
}

#[derive(Deserialize)]
#[serde(tag = "state", deny_unknown_fields)]
enum DaclStateData {
    Absent,
    Null,
    Acl { revision: u8, aces: Vec<WindowsAce> },
}

impl TryFrom<DaclStateData> for WindowsDaclState {
    type Error = io::Error;
    fn try_from(value: DaclStateData) -> io::Result<Self> {
        let state = match value {
            DaclStateData::Absent => Self::Absent,
            DaclStateData::Null => Self::Null,
            DaclStateData::Acl { revision, aces } => Self::Acl { revision, aces },
        };
        state.validate()?;
        Ok(state)
    }
}

impl WindowsDaclState {
    pub fn validate(&self) -> io::Result<()> {
        if let Self::Acl { revision, aces } = self {
            validate_acl_revision(*revision)?;
            let mut size = ACL_HEADER_SIZE;
            for ace in aces {
                ace.validate()?;
                validate_dacl_type(ace.ace_type(), *revision)?;
                size = size
                    .checked_add(ace.wire_size())
                    .ok_or_else(|| invalid("ACL-Länge läuft über"))?;
            }
            // Durch die Größenbegrenzung ist auch die Anzahl kleiner als u16::MAX.
            if size > u16::MAX as usize {
                return Err(invalid("ACL überschreitet das 16-Bit-Längenfeld"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "DaclData")]
pub struct WindowsDacl {
    /// Gepinnte SMB-ServerGUID; keine behauptete Gleichheit numerischer UID/GID.
    pub namespace: [u8; 16],
    pub control: u16,
    pub state: WindowsDaclState,
}

/// Konkrete Auslassungsgründe der begrenzten Datei- und Verzeichnis-Encoder.
/// Eine erfolgreiche Prüfung ist keine Provider-Capability und keine Schreibfreigabe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsDaclWriteIssue {
    InvalidDacl,
    InvalidTargetOwner,
    NamespaceMismatch,
    AbsentDacl,
    NullDacl,
    EmptyDacl,
    UnsupportedRevision,
    UnsupportedControl,
    UnsupportedAceType,
    UnsupportedAceFlags,
    UnsupportedMask,
    RelativeAceSid,
    RelativeTargetOwner,
    MissingOwnerAccess,
    UnsafeDeny,
    DescriptorTooLarge,
}

impl WindowsDaclWriteIssue {
    pub fn reason(self) -> &'static str {
        match self {
            Self::InvalidDacl => "Das DACL-Modell ist strukturell ungültig",
            Self::InvalidTargetOwner => "Die beobachtete Owner-SID des Zielobjekts ist ungültig",
            Self::NamespaceMismatch => "Quell- und Ziel-DACL gehören nicht zum selben SID-Namensraum",
            Self::AbsentDacl => "Eine fehlende DACL wird nicht auf das Zielobjekt übertragen",
            Self::NullDacl => "Eine NULL-DACL wird nicht auf das Zielobjekt übertragen",
            Self::EmptyDacl => "Eine leere DACL wird nicht auf das Zielobjekt übertragen",
            Self::UnsupportedRevision => "Der DACL-Encoder unterstützt nur ACL-Revision 2",
            Self::UnsupportedControl => "Die DACL muss ausschließlich present und protected sein",
            Self::UnsupportedAceType => "Der DACL-Encoder unterstützt nur einfache Allow- und Deny-ACEs",
            Self::UnsupportedAceFlags => "Der DACL-Encoder unterstützt keine ACE-Flags oder Vererbung",
            Self::UnsupportedMask => "Ein Allow-ACE enthält generische oder nicht unterstützte Objektrechte",
            Self::RelativeAceSid => "Eine ACE enthält eine objekt- oder ownerabhängige SID",
            Self::RelativeTargetOwner => "Die Owner-SID des Zielobjekts ist objekt- oder ownerabhängig",
            Self::MissingOwnerAccess => "Der Ziel-Owner braucht einen expliziten Allow-ACE für alle benötigten Objektzugriffe",
            Self::UnsafeDeny => "Ein Deny-ACE verweigert im gewählten Schreibvertrag benötigte oder nicht freigegebene Rechte",
            Self::DescriptorTooLarge => "Der codierte Security-Descriptor überschreitet 64 KiB",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DaclData {
    namespace: [u8; 16],
    control: u16,
    state: WindowsDaclState,
}

impl TryFrom<DaclData> for WindowsDacl {
    type Error = io::Error;
    fn try_from(value: DaclData) -> io::Result<Self> {
        let dacl = Self {
            namespace: value.namespace,
            control: value.control,
            state: value.state,
        };
        dacl.validate()?;
        Ok(dacl)
    }
}

impl WindowsDacl {
    pub fn validate(&self) -> io::Result<()> {
        if self.namespace == [0; 16] {
            return Err(invalid("Eine DACL braucht einen belegten SID-Namensraum"));
        }
        if self.control & !WINDOWS_DACL_CONTROL_MASK != 0 {
            return Err(invalid(
                "DACL enthält fremde Security-Descriptor-Controlbits",
            ));
        }
        if matches!(self.state, WindowsDaclState::Absent) != (self.control & DACL_PRESENT == 0) {
            return Err(invalid(
                "DACL-Present-Bit und DACL-Zustand widersprechen sich",
            ));
        }
        self.state.validate()
    }

    /// Prüft die unveränderte DACL für eine gewöhnliche, bereits angelegte Zieldatei.
    /// `target_owner` muss die frisch beobachtete Owner-SID dieser Temp-Datei sein.
    /// Der Aufrufer bindet Dateiart, Identität, Namespace und Handle selbst; diese
    /// reine Formatprüfung berechnet weder Gruppenmitgliedschaften noch effektive Rechte.
    /// Nach SET werden weder EA-Zugriffe, Dateiausführung noch WRITE_DAC angefragt.
    /// Daten-Denys verlangen im Backend einen vor SET gewährten, danach bis zum
    /// echten FLUSH gehaltenen Schreibzugriff. Ein frischer Daten-OPEN genügt nicht.
    pub fn file_write_issue(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
    ) -> Option<WindowsDaclWriteIssue> {
        self.write_issue(
            target_namespace,
            target_owner,
            FILE_OWNER_ACCESS,
            WRITE_OWNER | WRITE_DAC | FILE_TEMP_OPTIONAL_ACCESS,
        )
    }

    /// Bestehende Verzeichnisse benötigen Traversal und Readback, aber weder
    /// DELETE noch Schreibrechte für Hauptdaten oder Owner.
    pub fn directory_write_issue(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
    ) -> Option<WindowsDaclWriteIssue> {
        self.write_issue(target_namespace, target_owner, 0x0016_00a1, WRITE_OWNER)
    }

    /// Frühe Rechte müssen zusätzlich die nachfolgende normale Kinderarbeit und
    /// den letzten Attributabschluss zulassen. Die V7-Maske bleibt unverändert.
    pub fn directory_before_children_write_issue(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
    ) -> Option<WindowsDaclWriteIssue> {
        self.write_issue(target_namespace, target_owner, 0x0016_01e7, WRITE_OWNER)
    }

    /// Bestehende Dateien erhalten WRITE_DAC vor SET; danach wird nur gelesen.
    /// Daten-Schreiben, Anhängen, EA-Zugriff, Ausführung, Attributschreiben und
    /// DELETE dürfen verweigert werden: Dieser Abschluss liest Inhalt, Attribute
    /// und Security und setzt ausschließlich die DACL. Generisches
    /// WRITE enthält dagegen auch SYNCHRONIZE und bleibt ausgeschlossen.
    /// Ein eigenes Temp mit späterem Rename besitzt weiterhin seinen stärkeren Vertrag.
    pub fn file_inplace_write_issue(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
    ) -> Option<WindowsDaclWriteIssue> {
        self.write_issue(
            target_namespace,
            target_owner,
            0x0016_0081,
            FILE_INPLACE_OPTIONAL_ACCESS,
        )
    }

    pub fn encode_for_file_inplace_write(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
    ) -> io::Result<Vec<u8>> {
        if let Some(issue) = self.file_inplace_write_issue(target_namespace, target_owner) {
            return Err(invalid(issue.reason()));
        }
        self.encode_write_descriptor()
    }

    fn write_issue(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
        required_owner_access: u32,
        allowed_deny_access: u32,
    ) -> Option<WindowsDaclWriteIssue> {
        use WindowsDaclWriteIssue as Issue;
        if self.validate().is_err() {
            return Some(Issue::InvalidDacl);
        }
        if target_owner.validate().is_err() {
            return Some(Issue::InvalidTargetOwner);
        }
        if self.namespace != target_namespace {
            return Some(Issue::NamespaceMismatch);
        }
        if relative_sid(target_owner) {
            return Some(Issue::RelativeTargetOwner);
        }
        let (revision, aces) = match &self.state {
            WindowsDaclState::Absent => return Some(Issue::AbsentDacl),
            WindowsDaclState::Null => return Some(Issue::NullDacl),
            WindowsDaclState::Acl { revision, aces } => (revision, aces),
        };
        if aces.is_empty() {
            return Some(Issue::EmptyDacl);
        }
        if *revision != 2 {
            return Some(Issue::UnsupportedRevision);
        }
        if self.control != FILE_WRITE_CONTROL {
            return Some(Issue::UnsupportedControl);
        }
        let mut owner_access = false;
        let mut descriptor_size = DESCRIPTOR_HEADER_SIZE + ACL_HEADER_SIZE;
        for ace in aces {
            let (flags, mask, sid) = match ace {
                WindowsAce::Allow { flags, mask, sid } | WindowsAce::Deny { flags, mask, sid } => {
                    (flags, mask, sid)
                }
                WindowsAce::Opaque { .. } => return Some(Issue::UnsupportedAceType),
            };
            if *flags != 0 {
                return Some(Issue::UnsupportedAceFlags);
            }
            if relative_sid(sid) {
                return Some(Issue::RelativeAceSid);
            }
            if matches!(ace, WindowsAce::Allow { .. }) && mask & !FILE_ACCESS_MASK != 0 {
                return Some(Issue::UnsupportedMask);
            }
            // Jeder passende Deny kann auch den Owner über eine Gruppe treffen.
            // Deshalb nur ausdrücklich freigegebene, im jeweiligen Abschluss
            // nicht angefragte Rechte zulassen; generische Masken bleiben gesperrt.
            if matches!(ace, WindowsAce::Deny { .. }) && mask & !allowed_deny_access != 0 {
                return Some(Issue::UnsafeDeny);
            }
            if matches!(ace, WindowsAce::Allow { .. })
                && sid == target_owner
                && mask & required_owner_access == required_owner_access
            {
                owner_access = true;
            }
            // validate() hat die ACL bereits auf ein 16-Bit-Längenfeld begrenzt.
            descriptor_size += ace.wire_size();
        }
        if !owner_access {
            return Some(Issue::MissingOwnerAccess);
        }
        if descriptor_size > MAX_DESCRIPTOR_SIZE {
            return Some(Issue::DescriptorTooLarge);
        }
        None
    }

    /// Codiert nur die geprüfte DACL; Owner, Group und SACL bleiben ungesetzt.
    /// ACE-Reihenfolge, Masken und SIDs werden ohne Übersetzung übernommen.
    pub fn encode_for_file_write(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
    ) -> io::Result<Vec<u8>> {
        if let Some(issue) = self.file_write_issue(target_namespace, target_owner) {
            return Err(invalid(issue.reason()));
        }
        self.encode_write_descriptor()
    }

    /// Ausschließlich DACL und PROTECTED, ohne Owner-, Group- oder SACL-Felder.
    pub fn encode_for_directory_write(
        &self,
        target_namespace: [u8; 16],
        target_owner: &WindowsSid,
    ) -> io::Result<Vec<u8>> {
        if let Some(issue) = self.directory_write_issue(target_namespace, target_owner) {
            return Err(invalid(issue.reason()));
        }
        self.encode_write_descriptor()
    }

    fn encode_write_descriptor(&self) -> io::Result<Vec<u8>> {
        let WindowsDaclState::Acl { revision, aces } = &self.state else {
            return Err(invalid(WindowsDaclWriteIssue::InvalidDacl.reason()));
        };
        let acl_size = ACL_HEADER_SIZE + aces.iter().map(WindowsAce::wire_size).sum::<usize>();
        let mut bytes = vec![0; DESCRIPTOR_HEADER_SIZE];
        bytes.reserve(acl_size);
        bytes[0] = 1;
        bytes[2..4].copy_from_slice(&(SELF_RELATIVE | FILE_WRITE_CONTROL).to_le_bytes());
        bytes[16..20].copy_from_slice(&(DESCRIPTOR_HEADER_SIZE as u32).to_le_bytes());
        bytes.extend_from_slice(&[*revision, 0]);
        bytes.extend_from_slice(
            &u16::try_from(acl_size)
                .map_err(|_| invalid("ACL zu groß"))?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(
            &u16::try_from(aces.len())
                .map_err(|_| invalid("Zu viele ACEs"))?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&[0, 0]);
        for ace in aces {
            let (flags, mask, sid) = match ace {
                WindowsAce::Allow { flags, mask, sid } | WindowsAce::Deny { flags, mask, sid } => {
                    (flags, mask, sid)
                }
                WindowsAce::Opaque { .. } => {
                    return Err(invalid(WindowsDaclWriteIssue::UnsupportedAceType.reason()))
                }
            };
            bytes.extend_from_slice(&[ace.ace_type(), *flags]);
            bytes.extend_from_slice(
                &u16::try_from(ace.wire_size())
                    .map_err(|_| invalid("ACE zu groß"))?
                    .to_le_bytes(),
            );
            bytes.extend_from_slice(&mask.to_le_bytes());
            bytes.extend_from_slice(&[sid.revision, sid.sub_authorities.len() as u8]);
            bytes.extend_from_slice(&sid.authority.to_be_bytes()[2..]);
            for sub_authority in &sid.sub_authorities {
                bytes.extend_from_slice(&sub_authority.to_le_bytes());
            }
        }
        Ok(bytes)
    }
}

fn relative_sid(sid: &WindowsSid) -> bool {
    // SECURITY_CREATOR_SID_AUTHORITY enthält auch OWNER_RIGHTS (S-1-3-4).
    // PRINCIPAL_SELF (S-1-5-10) wird beim Access-Check durch die Objekt-SID ersetzt.
    // Primärquelle: learn.microsoft.com/en-us/windows-server/identity/ad-ds/manage/understand-security-identifiers
    sid.authority == 3 || sid.authority == 5 && sid.sub_authorities == [10]
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsSecurityDescriptor {
    pub owner: Option<WindowsSid>,
    pub group: Option<WindowsSid>,
    pub dacl: WindowsDacl,
}

/// Parst ausschließlich einen selbstrelativen Security-Descriptor (höchstens 64 KiB).
///
/// Alle referenzierten Komponenten müssen außerhalb des Headers und vollständig
/// innerhalb des Puffers liegen. Owner und Group dürfen dieselbe vollständige SID
/// referenzieren; Teilüberlappungen und überlappende ACLs werden abgelehnt. Freiraum
/// in einer ACL ist gemäß AclSize kein weiterer ACE und wird nicht ins Modell kopiert.
/// Unbekannte ACEs bleiben begrenzt opaque und erhalten dadurch keine Schreibfähigkeit.
pub fn parse_security_descriptor(
    bytes: &[u8],
    namespace: [u8; 16],
) -> io::Result<WindowsSecurityDescriptor> {
    if !(DESCRIPTOR_HEADER_SIZE..=MAX_DESCRIPTOR_SIZE).contains(&bytes.len()) {
        return Err(invalid(
            "Security-Descriptor außerhalb der 20- bis 65536-Byte-Grenze",
        ));
    }
    if bytes[0] != 1 {
        return Err(invalid("Unbekannte Security-Descriptor-Revision"));
    }
    let control = read_u16(bytes, 2)?;
    if control & SELF_RELATIVE == 0 {
        return Err(invalid("Security-Descriptor ist nicht selbstrelativ"));
    }
    // MS-DTYP definiert alle 16 Controlbits. Sbz1 besitzt ohne RM keine Bedeutung;
    // mit RM gehört es dem Resource Manager. Beides wird nicht zur DACL umgedeutet.
    let owner_offset = component_offset(bytes, 4)?;
    let group_offset = component_offset(bytes, 8)?;
    let sacl_offset = component_offset(bytes, 12)?;
    let dacl_offset = component_offset(bytes, 16)?;
    if control & SACL_PRESENT == 0 && sacl_offset.is_some()
        || control & DACL_PRESENT == 0 && dacl_offset.is_some()
    {
        return Err(invalid("ACL-Offset ohne zugehöriges Present-Bit"));
    }
    let mut components: Vec<(Range<usize>, bool)> = Vec::new();
    let owner = parse_component_sid(bytes, owner_offset, &mut components)?;
    let group = parse_component_sid(bytes, group_offset, &mut components)?;
    if let Some(offset) = sacl_offset {
        let (_, _, size) = parse_acl(bytes, offset, false)?;
        register_component(&mut components, offset..offset + size, false)?;
    }
    let state = match dacl_offset {
        Some(offset) => {
            let (revision, aces, size) = parse_acl(bytes, offset, true)?;
            register_component(&mut components, offset..offset + size, false)?;
            WindowsDaclState::Acl { revision, aces }
        }
        None if control & DACL_PRESENT != 0 => WindowsDaclState::Null,
        None => WindowsDaclState::Absent,
    };
    let dacl = WindowsDacl {
        namespace,
        control: control & WINDOWS_DACL_CONTROL_MASK,
        state,
    };
    dacl.validate()?;
    Ok(WindowsSecurityDescriptor { owner, group, dacl })
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value = bytes
        .get(
            offset
                ..offset
                    .checked_add(2)
                    .ok_or_else(|| invalid("Offset läuft über"))?,
        )
        .ok_or_else(|| invalid("Abgeschnittenes 16-Bit-Feld"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(
            offset
                ..offset
                    .checked_add(4)
                    .ok_or_else(|| invalid("Offset läuft über"))?,
        )
        .ok_or_else(|| invalid("Abgeschnittenes 32-Bit-Feld"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn component_offset(bytes: &[u8], field: usize) -> io::Result<Option<usize>> {
    let raw = read_u32(bytes, field)?;
    if raw == 0 {
        return Ok(None);
    }
    let offset =
        usize::try_from(raw).map_err(|_| invalid("Offset passt nicht in den Adressraum"))?;
    if offset < DESCRIPTOR_HEADER_SIZE || offset >= bytes.len() {
        return Err(invalid(
            "Komponentenoffset verweist auf Header oder außerhalb des Puffers",
        ));
    }
    Ok(Some(offset))
}

fn parse_sid(bytes: &[u8]) -> io::Result<(WindowsSid, usize)> {
    let header = bytes
        .get(..8)
        .ok_or_else(|| invalid("Abgeschnittener SID-Header"))?;
    let count = usize::from(header[1]);
    if count > 15 {
        return Err(invalid("Ungültige SID-Subauthority-Anzahl"));
    }
    let size = 8 + count * 4;
    let bytes = bytes
        .get(..size)
        .ok_or_else(|| invalid("Abgeschnittene SID-Subauthorities"))?;
    let authority = header[2..8]
        .iter()
        .fold(0_u64, |value, byte| value << 8 | u64::from(*byte));
    let mut sub_authorities = Vec::with_capacity(count);
    for index in 0..count {
        sub_authorities.push(read_u32(bytes, 8 + index * 4)?);
    }
    let sid = WindowsSid {
        revision: header[0],
        authority,
        sub_authorities,
    };
    sid.validate()?;
    Ok((sid, size))
}

fn parse_component_sid(
    bytes: &[u8],
    offset: Option<usize>,
    components: &mut Vec<(Range<usize>, bool)>,
) -> io::Result<Option<WindowsSid>> {
    let Some(offset) = offset else {
        return Ok(None);
    };
    let (sid, size) = parse_sid(&bytes[offset..])?;
    register_component(components, offset..offset + size, true)?;
    Ok(Some(sid))
}

fn register_component(
    components: &mut Vec<(Range<usize>, bool)>,
    range: Range<usize>,
    is_sid: bool,
) -> io::Result<()> {
    for (other, other_is_sid) in components.iter() {
        if range.start < other.end
            && other.start < range.end
            && !(is_sid && *other_is_sid && range == *other)
        {
            return Err(invalid("Security-Descriptor-Komponenten überlappen sich"));
        }
    }
    components.push((range, is_sid));
    Ok(())
}

fn validate_acl_revision(revision: u8) -> io::Result<()> {
    if !matches!(revision, 2 | 4) {
        return Err(invalid("Unbekannte ACL-Revision"));
    }
    Ok(())
}

fn validate_dacl_type(ace_type: u8, revision: u8) -> io::Result<()> {
    if matches!(ace_type, 2 | 7 | 0x0d | 0x0f | 0x11 | 0x12 | 0x13) {
        return Err(invalid("SACL-ACE innerhalb einer DACL"));
    }
    if matches!(ace_type, 5..=8 | 0x0b | 0x0c | 0x0f | 0x10) && revision != 4 {
        return Err(invalid("Object-ACE verlangt ACL-Revision 4"));
    }
    Ok(())
}

fn simple_ace_sid(data: &[u8]) -> io::Result<(WindowsSid, usize)> {
    let sid_bytes = data
        .get(4..)
        .ok_or_else(|| invalid("Einfacher ACE ohne Zugriffsmaske"))?;
    let (sid, size) = parse_sid(sid_bytes)?;
    Ok((sid, 4 + size))
}

fn parse_acl(
    bytes: &[u8],
    offset: usize,
    is_dacl: bool,
) -> io::Result<(u8, Vec<WindowsAce>, usize)> {
    // Microsoft ACL/ACE: jeder ACL- und ACE-Anfang liegt auf einer DWORD-Grenze.
    // Für separate SID-Offsets fordert das Drahtformat dagegen kein zusätzliches Padding.
    if !offset.is_multiple_of(4) {
        return Err(invalid("ACL beginnt nicht auf einer DWORD-Grenze"));
    }
    let header = bytes
        .get(offset..offset + ACL_HEADER_SIZE)
        .ok_or_else(|| invalid("Abgeschnittener ACL-Header"))?;
    let revision = header[0];
    validate_acl_revision(revision)?;
    if header[1] != 0 || read_u16(header, 6)? != 0 {
        return Err(invalid("Reservierte ACL-Felder sind nicht null"));
    }
    let size = usize::from(read_u16(header, 2)?);
    if size < ACL_HEADER_SIZE {
        return Err(invalid("ACL ist kürzer als ihr Header"));
    }
    let acl = bytes
        .get(
            offset
                ..offset
                    .checked_add(size)
                    .ok_or_else(|| invalid("ACL-Ende läuft über"))?,
        )
        .ok_or_else(|| invalid("ACL reicht über das Pufferende"))?;
    let count = usize::from(read_u16(header, 4)?);
    if count > (size - ACL_HEADER_SIZE) / ACE_HEADER_SIZE {
        return Err(invalid("ACE-Anzahl passt nicht in die ACL"));
    }
    let mut aces = Vec::with_capacity(count);
    let mut cursor = ACL_HEADER_SIZE;
    for _ in 0..count {
        let ace_header = acl
            .get(cursor..cursor + ACE_HEADER_SIZE)
            .ok_or_else(|| invalid("Abgeschnittener ACE-Header"))?;
        let ace_type = ace_header[0];
        if is_dacl {
            validate_dacl_type(ace_type, revision)?;
        } else if matches!(ace_type, 0 | 1 | 5 | 6 | 9..=0x0c) {
            return Err(invalid("DACL-ACE innerhalb einer SACL"));
        }
        let ace_size = usize::from(read_u16(ace_header, 2)?);
        if ace_size < ACE_HEADER_SIZE || !ace_size.is_multiple_of(4) {
            return Err(invalid(
                "ACE-Länge verletzt Headergröße oder DWORD-Ausrichtung",
            ));
        }
        let end = cursor
            .checked_add(ace_size)
            .ok_or_else(|| invalid("ACE-Ende läuft über"))?;
        let data = acl
            .get(cursor + ACE_HEADER_SIZE..end)
            .ok_or_else(|| invalid("ACE reicht über ihre ACL hinaus"))?;
        let flags = ace_header[1];
        let ace = if matches!(ace_type, 0 | 1) {
            let (sid, consumed) = simple_ace_sid(data)?;
            if consumed == data.len() {
                let mask = read_u32(data, 0)?;
                if ace_type == 0 {
                    WindowsAce::Allow { flags, mask, sid }
                } else {
                    WindowsAce::Deny { flags, mask, sid }
                }
            } else {
                WindowsAce::Opaque {
                    ace_type,
                    flags,
                    data: data.to_vec(),
                }
            }
        } else {
            WindowsAce::Opaque {
                ace_type,
                flags,
                data: data.to_vec(),
            }
        };
        aces.push(ace);
        cursor = end;
    }
    // AclSize umfasst auch freie Bytes. ACE_HEADER gestattet zusätzliche Bytes
    // innerhalb eines ACE; nur dort werden sie als opaque Inhalt erhalten.
    Ok((revision, aces, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMESPACE: [u8; 16] = [0x42; 16];

    fn descriptor(control: u16) -> Vec<u8> {
        let mut bytes = vec![0; 20];
        bytes[0] = 1;
        bytes[2..4].copy_from_slice(&(control | SELF_RELATIVE).to_le_bytes());
        bytes
    }

    fn sid(authority: [u8; 6], subs: &[u32]) -> Vec<u8> {
        let mut bytes = vec![1, subs.len() as u8];
        bytes.extend_from_slice(&authority);
        for value in subs {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn append_component(bytes: &mut Vec<u8>, field: usize, data: &[u8]) {
        let offset = u32::try_from(bytes.len()).unwrap();
        bytes[field..field + 4].copy_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(data);
    }

    fn ace(ace_type: u8, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![ace_type, flags];
        bytes.extend_from_slice(&u16::try_from(4 + data.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    fn simple(ace_type: u8, mask: u32, flags: u8) -> Vec<u8> {
        let mut data = mask.to_le_bytes().to_vec();
        data.extend_from_slice(&sid([0, 0, 0, 0, 0, 5], &[32, 544]));
        ace(ace_type, flags, &data)
    }

    fn acl(revision: u8, aces: &[Vec<u8>], slack: &[u8]) -> Vec<u8> {
        let mut bytes = vec![revision, 0, 0, 0];
        bytes.extend_from_slice(&u16::try_from(aces.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        for ace in aces {
            bytes.extend_from_slice(ace);
        }
        bytes.extend_from_slice(slack);
        let size = u16::try_from(bytes.len()).unwrap();
        bytes[2..4].copy_from_slice(&size.to_le_bytes());
        bytes
    }

    fn with_dacl(aces: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = descriptor(DACL_PRESENT | 0x1000);
        append_component(&mut bytes, 16, &acl(2, aces, &[]));
        bytes
    }

    fn parse(bytes: &[u8]) -> io::Result<WindowsSecurityDescriptor> {
        parse_security_descriptor(bytes, NAMESPACE)
    }

    fn file_owner() -> WindowsSid {
        WindowsSid {
            revision: 1,
            authority: 5,
            sub_authorities: vec![1000],
        }
    }

    fn writable_dacl() -> WindowsDacl {
        WindowsDacl {
            namespace: NAMESPACE,
            control: FILE_WRITE_CONTROL,
            state: WindowsDaclState::Acl {
                revision: 2,
                aces: vec![WindowsAce::Allow {
                    flags: 0,
                    mask: FILE_OWNER_ACCESS,
                    sid: file_owner(),
                }],
            },
        }
    }

    #[test]
    fn minimal_existing_file_dacl_is_not_a_temp_or_directory_dacl() {
        let mut dacl = writable_dacl();
        let WindowsDaclState::Acl { aces, .. } = &mut dacl.state else {
            panic!()
        };
        let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
            panic!()
        };
        *mask = 0x0016_0081;
        assert!(dacl
            .encode_for_file_inplace_write(NAMESPACE, &file_owner())
            .is_ok());
        assert_eq!(
            dacl.file_write_issue(NAMESPACE, &file_owner()),
            Some(WindowsDaclWriteIssue::MissingOwnerAccess)
        );
        assert_eq!(
            dacl.directory_write_issue(NAMESPACE, &file_owner()),
            Some(WindowsDaclWriteIssue::MissingOwnerAccess)
        );
        for bit in [1, 0x80, 0x20000, 0x40000, 0x100000] {
            let mut missing = dacl.clone();
            let WindowsDaclState::Acl { aces, .. } = &mut missing.state else {
                panic!()
            };
            let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
                panic!()
            };
            *mask &= !bit;
            assert_eq!(
                missing.file_inplace_write_issue(NAMESPACE, &file_owner()),
                Some(WindowsDaclWriteIssue::MissingOwnerAccess)
            );
        }
    }

    fn file_aces(dacl: &mut WindowsDacl) -> &mut Vec<WindowsAce> {
        let WindowsDaclState::Acl { aces, .. } = &mut dacl.state else {
            panic!()
        };
        aces
    }

    fn assert_write_issue(dacl: &WindowsDacl, expected: WindowsDaclWriteIssue) {
        assert_eq!(
            dacl.file_write_issue(NAMESPACE, &file_owner()),
            Some(expected)
        );
        assert_eq!(
            dacl.encode_for_file_write(NAMESPACE, &file_owner())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn file_encoder_roundtrips_order_masks_and_sid_bytes_without_other_components() {
        let mut dacl = writable_dacl();
        file_aces(&mut dacl).extend([
            WindowsAce::Deny {
                flags: 0,
                mask: WRITE_OWNER,
                sid: WindowsSid {
                    revision: 1,
                    authority: 22,
                    sub_authorities: vec![2, 1000],
                },
            },
            WindowsAce::Allow {
                flags: 0,
                mask: FILE_ACCESS_MASK,
                sid: WindowsSid {
                    revision: 1,
                    authority: 0x0102_0304_0506,
                    sub_authorities: vec![0x1122_3344, u32::MAX],
                },
            },
        ]);
        for reverse in [false, true] {
            if reverse {
                file_aces(&mut dacl).reverse();
            }
            let bytes = dacl
                .encode_for_file_write(NAMESPACE, &file_owner())
                .unwrap();
            assert_eq!(
                &bytes[..20],
                &[1, 0, 4, 144, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 20, 0, 0, 0]
            );
            let parsed = parse(&bytes).unwrap();
            assert_eq!(parsed.dacl, dacl);
            assert_eq!(parsed.owner, None);
            assert_eq!(parsed.group, None);
            let encoded = serde_json::to_value(parsed.dacl).unwrap();
            assert_eq!(encoded["control"], FILE_WRITE_CONTROL);
        }
    }

    #[test]
    fn file_encoder_rejects_absent_null_empty_revision_and_control_separately() {
        use WindowsDaclWriteIssue as Issue;
        let mut absent = writable_dacl();
        absent.control = 0;
        absent.state = WindowsDaclState::Absent;
        assert_write_issue(&absent, Issue::AbsentDacl);
        let mut null = writable_dacl();
        null.state = WindowsDaclState::Null;
        assert_write_issue(&null, Issue::NullDacl);
        let mut empty = writable_dacl();
        file_aces(&mut empty).clear();
        assert_write_issue(&empty, Issue::EmptyDacl);
        let mut revision = writable_dacl();
        if let WindowsDaclState::Acl { revision, .. } = &mut revision.state {
            *revision = 4;
        }
        assert_write_issue(&revision, Issue::UnsupportedRevision);
        for control in [
            DACL_PRESENT,
            FILE_WRITE_CONTROL | 0x8,
            FILE_WRITE_CONTROL | 0x400,
        ] {
            let mut dacl = writable_dacl();
            dacl.control = control;
            assert_write_issue(&dacl, Issue::UnsupportedControl);
        }
    }

    #[test]
    fn file_encoder_validates_public_models_before_encoding() {
        use WindowsDaclWriteIssue as Issue;
        let mut dacl = writable_dacl();
        dacl.namespace = [0; 16];
        assert_write_issue(&dacl, Issue::InvalidDacl);
        let mut dacl = writable_dacl();
        dacl.control |= SELF_RELATIVE;
        assert_write_issue(&dacl, Issue::InvalidDacl);
        let mut dacl = writable_dacl();
        if let WindowsAce::Allow { sid, .. } = &mut file_aces(&mut dacl)[0] {
            sid.sub_authorities = vec![1; 16];
        }
        assert_write_issue(&dacl, Issue::InvalidDacl);
        let mut invalid_owner = file_owner();
        invalid_owner.revision = 2;
        assert_eq!(
            writable_dacl().file_write_issue(NAMESPACE, &invalid_owner),
            Some(Issue::InvalidTargetOwner)
        );
        assert!(writable_dacl()
            .encode_for_file_write(NAMESPACE, &invalid_owner)
            .is_err());
    }

    #[test]
    fn file_encoder_binds_namespace_and_actual_target_owner() {
        use WindowsDaclWriteIssue as Issue;
        let dacl = writable_dacl();
        for namespace in [[0; 16], [0x43; 16]] {
            assert_eq!(
                dacl.file_write_issue(namespace, &file_owner()),
                Some(Issue::NamespaceMismatch)
            );
            assert!(dacl
                .encode_for_file_write(namespace, &file_owner())
                .is_err());
        }
        let mut other_owner = file_owner();
        other_owner.sub_authorities[0] += 1;
        assert_eq!(
            dacl.file_write_issue(NAMESPACE, &other_owner),
            Some(Issue::MissingOwnerAccess)
        );
        assert!(dacl.encode_for_file_write(NAMESPACE, &other_owner).is_err());
    }

    #[test]
    fn file_encoder_rejects_relative_sids_in_aces_and_owner_context() {
        use WindowsDaclWriteIssue as Issue;
        let relative = (0..=4)
            .map(|rid| WindowsSid {
                revision: 1,
                authority: 3,
                sub_authorities: vec![rid],
            })
            .chain([WindowsSid {
                revision: 1,
                authority: 5,
                sub_authorities: vec![10],
            }]);
        for sid in relative {
            let mut dacl = writable_dacl();
            file_aces(&mut dacl).push(WindowsAce::Allow {
                flags: 0,
                mask: 0,
                sid: sid.clone(),
            });
            assert_write_issue(&dacl, Issue::RelativeAceSid);
            assert_eq!(
                writable_dacl().file_write_issue(NAMESPACE, &sid),
                Some(Issue::RelativeTargetOwner)
            );
        }
        // Kein Präfixvergleich: S-1-5-10-1 ist nicht PRINCIPAL_SELF.
        let mut dacl = writable_dacl();
        file_aces(&mut dacl).push(WindowsAce::Allow {
            flags: 0,
            mask: 0,
            sid: WindowsSid {
                revision: 1,
                authority: 5,
                sub_authorities: vec![10, 1],
            },
        });
        assert!(dacl.encode_for_file_write(NAMESPACE, &file_owner()).is_ok());
    }

    #[test]
    fn file_encoder_omits_all_flags_and_opaque_aces() {
        use WindowsDaclWriteIssue as Issue;
        for bit in 0..8 {
            let mut dacl = writable_dacl();
            if let WindowsAce::Allow { flags, .. } = &mut file_aces(&mut dacl)[0] {
                *flags = 1 << bit;
            }
            assert_write_issue(&dacl, Issue::UnsupportedAceFlags);
        }
        for ace_type in [0xfe, 9, 0x0a] {
            let mut dacl = writable_dacl();
            file_aces(&mut dacl).push(WindowsAce::Opaque {
                ace_type,
                flags: 0,
                data: vec![0; 4],
            });
            assert_write_issue(&dacl, Issue::UnsupportedAceType);
        }
        let mut object = writable_dacl();
        if let WindowsDaclState::Acl { revision, aces } = &mut object.state {
            *revision = 4;
            aces.push(WindowsAce::Opaque {
                ace_type: 5,
                flags: 0,
                data: vec![0; 4],
            });
        }
        assert_write_issue(&object, Issue::UnsupportedRevision);
        let mut padded = simple(0, 1, 0);
        padded.extend_from_slice(&[0; 4]);
        let padded_size = u16::try_from(padded.len()).unwrap();
        padded[2..4].copy_from_slice(&padded_size.to_le_bytes());
        let mut dacl = writable_dacl();
        let WindowsDaclState::Acl { mut aces, .. } =
            parse(&with_dacl(&[padded])).unwrap().dacl.state
        else {
            panic!()
        };
        file_aces(&mut dacl).push(aces.remove(0));
        assert_write_issue(&dacl, Issue::UnsupportedAceType);
    }

    #[test]
    fn file_encoder_requires_one_complete_explicit_owner_allow() {
        use WindowsDaclWriteIssue as Issue;
        for bit in 0..32 {
            let removed = 1 << bit;
            if FILE_OWNER_ACCESS & removed == 0 {
                continue;
            }
            let mut dacl = writable_dacl();
            if let WindowsAce::Allow { mask, .. } = &mut file_aces(&mut dacl)[0] {
                *mask &= !removed;
            }
            assert_write_issue(&dacl, Issue::MissingOwnerAccess);
        }
        let mut dacl = writable_dacl();
        file_aces(&mut dacl).clear();
        for mask in [0x0007_01ff, 0x0010_0000] {
            file_aces(&mut dacl).push(WindowsAce::Allow {
                flags: 0,
                mask,
                sid: file_owner(),
            });
        }
        assert_write_issue(&dacl, Issue::MissingOwnerAccess);
    }

    #[test]
    fn file_encoder_rejects_generic_and_non_file_allow_rights() {
        for bit in 0..32 {
            let unknown = 1 << bit;
            if unknown & FILE_ACCESS_MASK != 0 {
                continue;
            }
            let mut dacl = writable_dacl();
            if let WindowsAce::Allow { mask, .. } = &mut file_aces(&mut dacl)[0] {
                *mask |= unknown;
            }
            assert_write_issue(&dacl, WindowsDaclWriteIssue::UnsupportedMask);
        }
    }

    #[test]
    fn file_temp_encoder_keeps_required_read_attributes_delete_and_security_rights() {
        use WindowsDaclWriteIssue as Issue;
        for bit in 0..32 {
            let mut dacl = writable_dacl();
            let mask = 1 << bit;
            file_aces(&mut dacl).insert(
                0,
                WindowsAce::Deny {
                    flags: 0,
                    mask,
                    sid: file_owner(),
                },
            );
            if matches!(bit, 1 | 2 | 3 | 4 | 5 | 18 | 19) {
                assert!(dacl.encode_for_file_write(NAMESPACE, &file_owner()).is_ok());
            } else {
                assert_write_issue(&dacl, Issue::UnsafeDeny);
            }
        }
        let mut dacl = writable_dacl();
        file_aces(&mut dacl).push(WindowsAce::Deny {
            flags: 0,
            mask: 0,
            sid: file_owner(),
        });
        assert!(dacl.encode_for_file_write(NAMESPACE, &file_owner()).is_ok());
    }

    #[test]
    fn file_encoder_enforces_descriptor_and_acl_size_boundaries() {
        use WindowsDaclWriteIssue as Issue;
        let filler = WindowsAce::Allow {
            flags: 0,
            mask: 0,
            sid: WindowsSid {
                revision: 1,
                authority: 1,
                sub_authorities: vec![],
            },
        };
        let mut dacl = writable_dacl();
        file_aces(&mut dacl).extend(vec![filler.clone(); 4093]);
        let bytes = dacl
            .encode_for_file_write(NAMESPACE, &file_owner())
            .unwrap();
        assert_eq!(bytes.len(), MAX_DESCRIPTOR_SIZE);
        assert_eq!(parse(&bytes).unwrap().dacl, dacl);
        if let WindowsAce::Allow { sid, .. } = &mut file_aces(&mut dacl)[1] {
            sid.sub_authorities.push(1);
        }
        assert_write_issue(&dacl, Issue::DescriptorTooLarge);
        file_aces(&mut dacl).extend([filler.clone(), filler]);
        assert_write_issue(&dacl, Issue::InvalidDacl);
    }

    #[test]
    fn file_write_issues_have_stable_serializable_reasons() {
        let issue = WindowsDaclWriteIssue::MissingOwnerAccess;
        assert_eq!(
            serde_json::to_string(&issue).unwrap(),
            "\"missing_owner_access\""
        );
        assert_eq!(
            serde_json::from_str::<WindowsDaclWriteIssue>("\"missing_owner_access\"").unwrap(),
            issue
        );
        assert!(!issue.reason().is_empty());
    }

    #[test]
    fn distinguishes_absent_null_and_empty_dacl() {
        let absent = parse(&descriptor(0)).unwrap().dacl;
        let null = parse(&descriptor(DACL_PRESENT)).unwrap().dacl;
        let empty = parse(&with_dacl(&[])).unwrap().dacl;
        assert_eq!(absent.state, WindowsDaclState::Absent);
        assert_eq!(null.state, WindowsDaclState::Null);
        assert_eq!(
            empty.state,
            WindowsDaclState::Acl {
                revision: 2,
                aces: vec![]
            }
        );
        assert_ne!(null, empty);
        assert_ne!(absent, null);
    }

    #[test]
    fn reads_owner_group_both_endiannesses_and_full_authority() {
        let mut bytes = descriptor(0);
        // Authority läuft höchstwertig zuerst; Subauthority 0x11223344 niederwertig zuerst.
        append_component(
            &mut bytes,
            8,
            &sid([1, 2, 3, 4, 5, 6], &[0x11223344, u32::MAX]),
        );
        append_component(&mut bytes, 4, &sid([0, 0, 0, 0, 0, 5], &[21, 1000]));
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.owner.unwrap().to_string(), "S-1-5-21-1000");
        let group = parsed.group.unwrap();
        assert_eq!(group.authority, 0x010203040506);
        assert_eq!(group.sub_authorities, [0x11223344, u32::MAX]);
        assert_eq!(group.to_string(), "S-1-0x010203040506-287454020-4294967295");
    }

    #[test]
    fn accepts_binary_sid_without_subauthorities_and_shared_owner_group() {
        let mut bytes = descriptor(0);
        append_component(&mut bytes, 4, &sid([0, 0, 0, 0, 0, 5], &[]));
        bytes[8..12].copy_from_slice(&20_u32.to_le_bytes());
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.owner, parsed.group);
        let owner = parsed.owner.unwrap();
        assert!(owner.sub_authorities.is_empty());
        assert_eq!(owner.to_string(), "S-1-5");
        assert_eq!(
            serde_json::from_str::<WindowsSid>(&serde_json::to_string(&owner).unwrap()).unwrap(),
            owner
        );
    }

    #[test]
    fn retains_access_order_masks_and_flags_without_canonicalization() {
        let bytes = with_dacl(&[
            simple(0, 0xffff_ffff, 0x10),
            simple(1, 0x1234_5678, 3),
            simple(0, 7, 0),
        ]);
        let parsed = parse(&bytes).unwrap();
        let WindowsDaclState::Acl { aces, .. } = parsed.dacl.state else {
            panic!()
        };
        assert!(matches!(
            &aces[0],
            WindowsAce::Allow {
                mask: u32::MAX,
                flags: 0x10,
                ..
            }
        ));
        assert!(matches!(
            &aces[1],
            WindowsAce::Deny {
                mask: 0x1234_5678,
                flags: 3,
                ..
            }
        ));
        assert!(matches!(
            &aces[2],
            WindowsAce::Allow {
                mask: 7,
                flags: 0,
                ..
            }
        ));
    }

    #[test]
    fn preserves_unknown_complex_and_padded_simple_aces_through_json() {
        let unknown = ace(0xfe, 0xa5, &[0xde, 0xad, 0xbe, 0xef]);
        let callback = ace(9, 3, &[7, 8, 9, 10]);
        let mut padded = simple(0, 1, 0);
        padded.extend_from_slice(&[1, 2, 3, 4]);
        let padded_size = u16::try_from(padded.len()).unwrap();
        padded[2..4].copy_from_slice(&padded_size.to_le_bytes());
        let parsed = parse(&with_dacl(&[unknown, callback, padded.clone()]))
            .unwrap()
            .dacl;
        let encoded = serde_json::to_string(&parsed).unwrap();
        assert_eq!(
            serde_json::from_str::<WindowsDacl>(&encoded).unwrap(),
            parsed
        );
        let WindowsDaclState::Acl { aces, .. } = parsed.state else {
            panic!()
        };
        assert_eq!(
            aces[0],
            WindowsAce::Opaque {
                ace_type: 0xfe,
                flags: 0xa5,
                data: vec![0xde, 0xad, 0xbe, 0xef]
            }
        );
        assert!(matches!(&aces[1], WindowsAce::Opaque { ace_type: 9, .. }));
        assert_eq!(
            aces[2],
            WindowsAce::Opaque {
                ace_type: 0,
                flags: 0,
                data: padded[4..].to_vec()
            }
        );
    }

    #[test]
    fn allows_acl_free_space_without_interpreting_it_as_aces() {
        let mut bytes = descriptor(DACL_PRESENT);
        append_component(&mut bytes, 16, &acl(2, &[], &[1, 2, 3]));
        assert_eq!(
            parse(&bytes).unwrap().dacl.state,
            WindowsDaclState::Acl {
                revision: 2,
                aces: vec![]
            }
        );
    }

    #[test]
    fn strips_non_dacl_control_and_checks_sacl_without_transferring_it() {
        let mut bytes = descriptor(0xffff);
        bytes[1] = 0x88; // RM ist gesetzt: Resource-Manager-Daten bleiben außerhalb der DACL.
        append_component(&mut bytes, 12, &acl(2, &[simple(2, 7, 0xc0)], &[]));
        let result = parse(&bytes).unwrap();
        assert_eq!(result.dacl.control, WINDOWS_DACL_CONTROL_MASK);
        assert_eq!(result.dacl.state, WindowsDaclState::Null);
        let encoded = serde_json::to_value(result.dacl).unwrap();
        assert_eq!(encoded.as_object().unwrap().len(), 3);
        bytes[22] = 0xff;
        bytes[23] = 0xff;
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn rejects_header_offsets_outside_offsets_and_present_mismatch() {
        for offset in [1, 16, 19, 20, u32::MAX] {
            let mut bytes = descriptor(0);
            bytes[4..8].copy_from_slice(&offset.to_le_bytes());
            assert!(parse(&bytes).is_err(), "offset {offset}");
        }
        let mut bytes = with_dacl(&[]);
        bytes[2..4].copy_from_slice(&SELF_RELATIVE.to_le_bytes());
        assert!(parse(&bytes).is_err());
        let mut bytes = descriptor(0);
        bytes[2..4].fill(0);
        assert!(parse(&bytes).is_err());
        assert!(parse(&vec![0; MAX_DESCRIPTOR_SIZE + 1]).is_err());
    }

    #[test]
    fn rejects_partial_sid_overlap_and_any_acl_alias() {
        let mut bytes = descriptor(0);
        // Die zweite SID bei24 ist ebenfalls strukturell gültig, überlappt aber die erste.
        append_component(
            &mut bytes,
            4,
            &[1, 2, 0, 0, 1, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0],
        );
        bytes[8..12].copy_from_slice(&24_u32.to_le_bytes());
        assert!(parse(&bytes).is_err());
        let mut bytes = descriptor(DACL_PRESENT | SACL_PRESENT);
        append_component(&mut bytes, 16, &acl(2, &[], &[]));
        bytes[12..16].copy_from_slice(&20_u32.to_le_bytes());
        assert!(parse(&bytes).is_err());
        let mut bytes = descriptor(DACL_PRESENT);
        let embedded_sid = sid([0, 0, 0, 0, 0, 5], &[21]);
        append_component(&mut bytes, 16, &acl(2, &[], &embedded_sid));
        bytes[4..8].copy_from_slice(&28_u32.to_le_bytes());
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn rejects_unaligned_acl_bad_revision_reserved_fields_and_ace_bounds() {
        let original = with_dacl(&[simple(0, 1, 0)]);
        let mut unaligned = descriptor(DACL_PRESENT);
        unaligned.push(0);
        append_component(&mut unaligned, 16, &acl(2, &[], &[]));
        assert!(parse(&unaligned).is_err());
        for (offset, value) in [(20, 3), (21, 1), (26, 1), (30, 3), (30, 0xfc), (31, 0xff)] {
            let mut bytes = original.clone();
            bytes[offset] = value;
            assert!(parse(&bytes).is_err(), "offset {offset}, value {value}");
        }
        let mut bytes = original.clone();
        bytes[24..26].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(parse(&bytes).is_err());
        for length in 0..original.len() {
            assert!(parse(&original[..length]).is_err(), "length {length}");
        }
    }

    #[test]
    fn rejects_malformed_sid_in_owner_and_known_ace_even_when_opaque() {
        let mut owner = descriptor(0);
        append_component(&mut owner, 4, &sid([0, 0, 0, 0, 0, 5], &[1]));
        for (offset, value) in [(20, 2), (21, 16)] {
            let mut bytes = owner.clone();
            bytes[offset] = value;
            assert!(parse(&bytes).is_err());
        }
        let mut bytes = with_dacl(&[simple(0, 7, 0)]);
        bytes[36] = 2;
        assert!(parse(&bytes).is_err());
        let opaque = WindowsAce::Opaque {
            ace_type: 1,
            flags: 0,
            data: vec![0; 16],
        };
        assert!(opaque.validate().is_err());
    }

    #[test]
    fn serde_and_validation_reject_tampered_models() {
        let dacl = parse(&with_dacl(&[simple(0, 1, 0)])).unwrap().dacl;
        let original = serde_json::to_value(dacl.clone()).unwrap();
        for change in ["namespace", "control", "state", "sid", "opaque"] {
            let mut value = original.clone();
            match change {
                "namespace" => value["namespace"] = serde_json::json!(vec![0; 16]),
                "control" => value["control"] = serde_json::json!(DACL_PRESENT | SELF_RELATIVE),
                "state" => value["state"] = serde_json::json!({"state":"Absent"}),
                "sid" => {
                    value["state"]["aces"][0]["sid"]["authority"] = serde_json::json!(1_u64 << 48)
                }
                "opaque" => {
                    value["state"]["aces"][0] =
                        serde_json::json!({"type":"Opaque", "ace_type":254, "flags":0, "data":[1]})
                }
                _ => unreachable!(),
            }
            assert!(
                serde_json::from_value::<WindowsDacl>(value).is_err(),
                "change {change}"
            );
        }
        let mut direct = dacl;
        direct.namespace.fill(0);
        assert!(direct.validate().is_err());
    }

    #[test]
    fn rejects_sacl_aces_in_dacl_and_dacl_aces_in_sacl() {
        assert!(parse(&with_dacl(&[simple(2, 7, 0xc0)])).is_err());
        let mut bytes = descriptor(SACL_PRESENT);
        append_component(&mut bytes, 12, &acl(2, &[simple(0, 7, 0)], &[]));
        assert!(parse(&bytes).is_err());
        assert!(parse(&with_dacl(&[ace(5, 0, &[0; 4])])).is_err());
    }

    #[test]
    fn parses_samba_query_info_layout_with_distinct_owner_group_namespaces() {
        // Layout einer realen 152-Byte-QUERY_INFO-Antwort; der künstliche Server-SID
        // ist hier durch feste Testwerte ersetzt. Datei und Ordner unterscheiden Masken.
        let fixture: [u8; 152] = [
            1, 0, 4, 144, 20, 0, 0, 0, 48, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 1, 5, 0, 0, 0, 0, 0,
            5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 184, 11, 0, 0, 1, 2, 0, 0, 0, 0, 0,
            22, 2, 0, 0, 0, 232, 3, 0, 0, 2, 0, 88, 0, 3, 0, 0, 0, 0, 0, 36, 0, 159, 1, 18, 0, 1,
            5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 184, 11, 0, 0, 0,
            0, 24, 0, 159, 1, 18, 0, 1, 2, 0, 0, 0, 0, 0, 22, 2, 0, 0, 0, 232, 3, 0, 0, 0, 0, 20,
            0, 137, 0, 18, 0, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,
        ];
        for directory in [false, true] {
            let mut bytes = fixture;
            let expected_masks = if directory {
                [0x1f01ff_u32, 0x1f01ff, 0x1200a9]
            } else {
                [0x12019f, 0x12019f, 0x120089]
            };
            for (offset, mask) in [76, 112, 136].into_iter().zip(expected_masks) {
                bytes[offset..offset + 4].copy_from_slice(&mask.to_le_bytes());
            }
            let parsed = parse(&bytes).unwrap();
            assert_eq!(
                parsed.owner.as_ref().unwrap().to_string(),
                "S-1-5-21-1-2-3-3000"
            );
            assert_eq!(parsed.group.as_ref().unwrap().to_string(), "S-1-22-2-1000");
            let WindowsDaclState::Acl { revision, aces } = parsed.dacl.state else {
                panic!()
            };
            assert_eq!(revision, 2);
            assert_eq!(aces.len(), 3);
            for (ace, expected) in aces.iter().zip(expected_masks) {
                assert!(
                    matches!(ace, WindowsAce::Allow { mask, flags: 0, .. } if *mask == expected)
                );
            }
        }
    }
    #[test]
    fn file_inplace_optional_denies_keep_temp_and_directory_boundaries() {
        for mask in [
            2,
            4,
            6,
            8,
            0x10,
            0x20,
            0x100,
            0x10000,
            WRITE_OWNER | 6,
            WRITE_DAC,
            FILE_INPLACE_OPTIONAL_ACCESS,
        ] {
            for sid in [
                file_owner(),
                WindowsSid {
                    revision: 1,
                    authority: 1,
                    sub_authorities: vec![0],
                },
            ] {
                for first in [true, false] {
                    let mut dacl = writable_dacl();
                    let deny = WindowsAce::Deny {
                        flags: 0,
                        mask,
                        sid: sid.clone(),
                    };
                    if first {
                        file_aces(&mut dacl).insert(0, deny);
                    } else {
                        file_aces(&mut dacl).push(deny);
                    }
                    let bytes = dacl
                        .encode_for_file_inplace_write(NAMESPACE, &file_owner())
                        .unwrap();
                    assert_eq!(
                        parse_security_descriptor(&bytes, NAMESPACE).unwrap().dacl,
                        dacl
                    );
                    assert_eq!(
                        dacl.file_write_issue(NAMESPACE, &file_owner()),
                        if mask & !(WRITE_OWNER | WRITE_DAC | 0x3e) == 0 {
                            None
                        } else {
                            Some(WindowsDaclWriteIssue::UnsafeDeny)
                        }
                    );
                    assert_eq!(
                        dacl.directory_write_issue(NAMESPACE, &file_owner()),
                        Some(WindowsDaclWriteIssue::UnsafeDeny)
                    );
                    assert_eq!(
                        dacl.directory_before_children_write_issue(NAMESPACE, &file_owner()),
                        Some(WindowsDaclWriteIssue::UnsafeDeny)
                    );
                }
            }
        }
    }

    #[test]
    fn file_inplace_deny_bit_boundary_keeps_read_security_and_generic_rights_required() {
        for bit in 0..32 {
            for mask in [1u32 << bit, (1u32 << bit) | FILE_INPLACE_OPTIONAL_ACCESS] {
                let mut dacl = writable_dacl();
                file_aces(&mut dacl).insert(
                    0,
                    WindowsAce::Deny {
                        flags: 0,
                        mask,
                        sid: file_owner(),
                    },
                );
                let issue = dacl.file_inplace_write_issue(NAMESPACE, &file_owner());
                if matches!(bit, 1 | 2 | 3 | 4 | 5 | 8 | 16 | 18 | 19) {
                    assert_eq!(
                        issue, None,
                        "Nicht benötigtes konkretes Dateirecht: {mask:#x}"
                    );
                } else {
                    assert_eq!(
                        issue,
                        Some(WindowsDaclWriteIssue::UnsafeDeny),
                        "Nicht freigegebenes Deny-Bit {bit}: {mask:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn file_inplace_data_denies_keep_flags_and_relative_trustees_outside_the_contract() {
        for bit in 0..8 {
            let mut dacl = writable_dacl();
            file_aces(&mut dacl).insert(
                0,
                WindowsAce::Deny {
                    flags: 1 << bit,
                    mask: 6,
                    sid: file_owner(),
                },
            );
            assert_eq!(
                dacl.file_inplace_write_issue(NAMESPACE, &file_owner()),
                Some(WindowsDaclWriteIssue::UnsupportedAceFlags)
            );
        }
        let mut dacl = writable_dacl();
        file_aces(&mut dacl).insert(
            0,
            WindowsAce::Deny {
                flags: 0,
                mask: 6,
                sid: WindowsSid {
                    revision: 1,
                    authority: 3,
                    sub_authorities: vec![0],
                },
            },
        );
        assert_eq!(
            dacl.file_inplace_write_issue(NAMESPACE, &file_owner()),
            Some(WindowsDaclWriteIssue::RelativeAceSid)
        );
    }
}
