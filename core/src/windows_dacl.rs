//! Getrennte Windows-DACL-Verträge für eigene Datei-Temps und bestehende Ordner.
use crate::windows_security::{WindowsDaclWriteIssue, WindowsSid};
use std::io;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowsDaclWriteContext {
    pub namespace: [u8; 16],
    pub owner: WindowsSid,
    pub group: Option<WindowsSid>,
}

impl WindowsDaclWriteContext {
    pub fn validate(&self) -> io::Result<()> {
        if self.namespace == [0; 16] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows-DACL-Zielkontext hat keinen Servernamensraum",
            ));
        }
        self.owner.validate()?;
        if let Some(group) = &self.group {
            group.validate()?;
        }
        Ok(())
    }
}

/// Reine Laufzeitbeobachtung: ID und Security stammen aus demselben gehaltenen
/// Ordnerhandle. Dies ist weder ein Schreibpermit noch ein persistiertes DTO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryDaclRecoveryObservation {
    pub base_id: u64,
    pub context: WindowsDaclWriteContext,
    pub dacl: crate::windows_security::WindowsDacl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WindowsDaclTargetIssue {
    NotPrepared,
    UnsupportedBackend,
    NoStagingOwner,
    RequiredAccessDenied,
    SecurityUnsupported,
    MissingOwner,
    DifferentTempContext,
    DirectoryContextMismatch,
    Normalized,
    ReopenDenied,
    AlreadyDesired,
    OptionalResources,
    OptionalObservation,
}

impl WindowsDaclTargetIssue {
    pub fn is_access_denied(self) -> bool {
        matches!(self, Self::RequiredAccessDenied | Self::ReopenDenied)
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::OptionalResources => "Zusätzliche Handles für die Rechteübernahme sind derzeit nicht verfügbar; die normale Ordneranlage und Zeitübernahme bleiben erhalten",
            Self::OptionalObservation => "Die zusätzliche Bindung für die Rechteübernahme konnte nicht vollständig beobachtet werden; die normale Ordneranlage bleibt erhalten",
            Self::AlreadyDesired => "Der neue Ordner besitzt bereits die gewünschten Rechte; keine Rechteänderung nötig",
            Self::NotPrepared => "Für diesen Zielkontext ist kein exakter DACL-Roundtrip vorbereitet",
            Self::UnsupportedBackend => "Der Zieladapter besitzt keinen Windows-DACL-Schreibvertrag",
            Self::NoStagingOwner => "Die DACL-Probe benötigt einen wiederherstellbar gebundenen Queue-Temp",
            Self::RequiredAccessDenied => "Der Server verweigert WRITE_DAC oder einen für den Objektabschluss benötigten Zugriff",
            Self::SecurityUnsupported => "Der Server unterstützt die benötigte Security-Abfrage oder DACL-Wirkung nicht",
            Self::MissingOwner => "Der Owner-SID-Kontext des neuen Zielobjekts konnte nicht vollständig beobachtet werden",
            Self::DirectoryContextMismatch => "Der bestehende Zielordner besitzt einen anderen Owner-/Group-Kontext als die Verzeichnis-DACL-Probe; die ACL wird ausgelassen",
            Self::DifferentTempContext => "Die neue Zieldatei besitzt einen anderen Owner-/Group-Kontext als die DACL-Probe; die ACL wird ausgelassen",
            Self::Normalized => "Der Server verändert die angefragte DACL; ein exakter Roundtrip ist nicht belegt",
            Self::ReopenDenied => "Die DACL-Probe erlaubt das frische Öffnen für Readback und Objektabschluss nicht",
        }
    }
}

/// Nur der vorbereitete Backendkontext gewährt den Writer. Dieses Ergebnis ist
/// kein persistierbares Permit und keine allgemeine SMB-Fähigkeitszusage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsDaclWriteSupport {
    Unavailable(WindowsDaclTargetIssue),
    Ready(WindowsDaclWriteContext),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WindowsDaclOmission {
    Target(WindowsDaclTargetIssue),
    Source(WindowsDaclWriteIssue),
    TargetDacl(WindowsDaclWriteIssue),
    Directory,
    CreatedDirectoryNonempty,
    CreatedDirectoryListingUnobserved,
    CreatedDirectoryStreamsUnobserved,
    CreatedDirectoryStreamsNonempty,
    AclOnlyChange,
    FileSidContextMismatch,
    FileStreamMismatch,
    FileStreamsUnobserved,
    FileTargetIdentityMissing,
    MissingObservation,
    MissingTargetObservation,
}

impl WindowsDaclOmission {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Target(value) => value.reason(),
            Self::Source(value) => value.reason(),
            Self::TargetDacl(value) => value.reason(),
            Self::CreatedDirectoryListingUnobserved => "Die zusätzliche vollständige Leerheitsbeobachtung des Quellordners ist nicht verfügbar; die Rechte des neuen Ordners werden ausgelassen",
            Self::CreatedDirectoryNonempty => "Rechte neuer Ordner werden derzeit nur für nachweislich leere Quellordner übernommen; ausgeblendete Kinder zählen weiterhin als Inhalt",
            Self::CreatedDirectoryStreamsUnobserved => "Die vollständige Menge der Ordnerdatenströme ist nicht beobachtet; die Rechte des neuen Ordners werden ausgelassen",
            Self::CreatedDirectoryStreamsNonempty => "Rechte neuer Ordner mit Datenströmen werden in dieser Implementierung noch nicht übernommen",
            Self::Directory => "Verzeichnis-DACLs benötigen einen getrennten Vertrag für die dauerhafte Einzelwirkung und werden hier nicht geschrieben",
            Self::AclOnlyChange => "Diese Ausführung unterstützt die begrenzte Rechteänderung an bestehenden Dateien nicht; gleicher Hauptinhalt wird dafür nicht ersetzt",
            Self::FileSidContextMismatch => "Die vorhandene Datei besitzt einen anderen Owner-/Group-SID-Kontext als die Quelle; die objektweiten Dateirechte werden ausgelassen",
            Self::FileStreamMismatch => "Quelle und Ziel besitzen unterschiedliche benannte Datenströme; die begrenzte Dateirechteänderung darf diese Abweichung nicht durch einen Inhaltsaustausch lösen",
            Self::FileStreamsUnobserved => "Die vollständige Menge benannter Datenströme ist nicht auf beiden Seiten beobachtet; die vorhandenen Dateirechte bleiben erhalten",
            Self::FileTargetIdentityMissing => "Die vorhandene Zieldatei besitzt keinen vollständigen Datei-ID- und Inhaltsbeleg für eine objektweite Rechteänderung",
            Self::MissingTargetObservation => "Die Windows-DACL oder der Owner-/Group-Kontext des Zielobjekts ist nicht vollständig beobachtet",
            Self::MissingObservation => "Die Windows-DACL der Quelle ist nicht vollständig beobachtet",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WindowsDaclTransfer {
    Exact(WindowsDaclWriteContext),
    DirectoryExact(Box<crate::directory_dacl::DirectoryDaclPlan>),
    CreatedDirectoryExact(Box<crate::created_directory_dacl::CreatedDirectoryDaclSelection>),
    CreatedDirectoryBeforeChildrenExact(
        Box<crate::created_directory_dacl::CreatedDirectoryBeforeChildrenSelection>,
    ),
    Omitted(WindowsDaclOmission),
}

#[derive(Debug)]
struct KnownWriteOmission(WindowsDaclTargetIssue);
impl std::fmt::Display for KnownWriteOmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.reason())
    }
}
impl std::error::Error for KnownWriteOmission {}

/// Ausschließlich bekannte Fehler vor einem SET dürfen als Metadatenauslassung
/// fortsetzen. Nach einem möglicherweise wirksamen SET bleibt der Ausgang unklar.
pub fn write_omission(issue: WindowsDaclTargetIssue) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, KnownWriteOmission(issue))
}

pub fn known_write_omission(error: &io::Error) -> Option<WindowsDaclTargetIssue> {
    error
        .get_ref()?
        .downcast_ref::<KnownWriteOmission>()
        .map(|value| value.0)
}
