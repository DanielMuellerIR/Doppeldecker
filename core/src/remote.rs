//! Gemeinsame, geheimnisfreie Verträge für entfernte Dateisysteme.
//!
//! Persistente Pläne und Queue-Einträge dürfen Verbindungsdaten und Referenzen auf
//! Zugangsdaten enthalten, aber niemals das Geheimnis selbst. Die konkreten Provider
//! lösen [`SecretReference`] deshalb erst unmittelbar vor dem Verbindungsaufbau über
//! einen kurzlebigen Resolver auf.

use std::io;
use url::Url;

/// Fasst Probenergebnis und Cleanup so zusammen, dass **kein** Fehler verlorengeht.
///
/// `let cleanup = …; result?; cleanup?;` sieht symmetrisch aus, ist es aber nicht:
/// Scheitert `result`, kehrt schon das erste `?` zurück, und der Cleanup-Fehler fällt
/// weg — samt dem einzigen Hinweis, dass unter dem gewählten Root ein Probenrest
/// liegengeblieben ist. Künstliche Capability-Proben legen echte Verzeichnisse und
/// Dateien an; ein verschwiegener Rest ist deshalb sichtbarer fremder Zustand im
/// Ordner des Nutzers.
///
/// Ein unklarer Ausgang hat Vorrang vor jedem anderen Fehler: Er darf nie hinter
/// einer gewöhnlichen Fehlermeldung verschwinden, weil nur er einen
/// Wiederholversuch verbietet. Der zweite Fehler geht deshalb trotzdem nicht
/// verloren — er steht im Text des zurückgegebenen Fehlers.
///
/// Beide Fehler behalten außerdem ihren maschinenlesbaren [`crate::ErrorCode`]:
/// Der zusammengesetzte Fehler übernimmt Kind und Code des Fehlers, der die
/// Reaktion bestimmt. Vorher entstand hier ein rohes `InvalidData`, wodurch aus
/// `connection_lost` oder `permission_denied` ein `invalid_data` wurde und CLI wie
/// GUI Reconnect und Nutzerhinweis falsch wählten (Review-Fund 2026-09-04).
pub fn combine_probe_outcome<T>(
    provider: &str,
    result: io::Result<T>,
    cleanup: io::Result<()>,
    recovery_path: &str,
) -> io::Result<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(crate::error::with_recovery_paths(
            carry_error_code(
                &cleanup_error,
                format!(
                    "{provider}-Probe-Cleanup scheiterte; Recoverypfad {recovery_path}: {cleanup_error}"
                ),
            ),
            &[recovery_path.to_owned()],
        )),
        (Err(error), Err(cleanup_error)) => {
            // Nur der unklare Ausgang darf den Code stellen, wenn die Probe selbst
            // einen gewöhnlichen Fehler meldet: Er verbietet den Wiederholversuch.
            let leading = if is_outcome_unclear(&cleanup_error) && !is_outcome_unclear(&error) {
                &cleanup_error
            } else {
                &error
            };
            let combined = carry_error_code(
                leading,
                format!(
                    "{provider}-Probe scheiterte: {error}; Cleanup scheiterte ebenfalls; Recoverypfad {recovery_path}: {cleanup_error}"
                ),
            );
            let mut paths = crate::error::recovery_paths(&error);
            paths.extend(crate::error::recovery_paths(&cleanup_error));
            paths.push(recovery_path.to_owned());
            Err(crate::error::with_recovery_paths(combined, &paths))
        }
    }
}

/// Fasst mehrere Cleanup-Fehler einer Probe zu genau einem Fehler zusammen.
///
/// Eine Probe, die mehrere Reste aufräumt, sammelt dabei mehrere Fehler. Sie zu
/// Text zu verketten verliert den [`crate::ErrorCode`] jedes einzelnen — und damit
/// gerade den einen, der zählt: `OutcomeUnclear` verbietet den Wiederholversuch und
/// darf nicht von einem gewöhnlichen Fehler daneben verdrängt werden. Führend ist
/// deshalb der erste unklare Ausgang, sonst der erste Fehler überhaupt; der Text
/// nennt weiterhin alle Ursachen mit ihrem Pfad.
pub fn fold_cleanup_errors(errors: Vec<io::Error>) -> io::Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    let leading = errors.iter().position(is_outcome_unclear).unwrap_or(0);
    let text = errors
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let paths = errors
        .iter()
        .flat_map(crate::error::recovery_paths)
        .collect::<Vec<_>>();
    Err(crate::error::with_recovery_paths(
        carry_error_code(&errors[leading], text),
        &paths,
    ))
}

/// Erhält den Cleanup-Pfad als Text und strukturierten Prüfhinweis, ohne Kind und
/// [`crate::ErrorCode`] zu verlieren. Der Hinweis erteilt kein Entfernungsrecht.
pub fn error_with_path(path: &std::path::Path, error: io::Error) -> io::Error {
    crate::error::with_recovery_paths(
        carry_error_code(&error, format!("{}: {error}", path.display())),
        &[path.to_string_lossy().into_owned()],
    )
}

/// Wahr, wenn der Fehler einen Ausgang meldet, der weder bestätigt noch
/// ausgeschlossen werden konnte.
fn is_outcome_unclear(error: &io::Error) -> bool {
    crate::error::error_code(error) == crate::ErrorCode::OutcomeUnclear
}

/// Baut den zusammengefassten Fehler so, dass Kind und [`crate::ErrorCode`] des
/// führenden Fehlers erhalten bleiben. Der Text nennt weiterhin beide Ursachen.
fn carry_error_code(leading: &io::Error, message: String) -> io::Error {
    crate::error::with_recovery_paths(
        crate::error::coded_error(leading.kind(), crate::error::error_code(leading), message),
        &crate::error::recovery_paths(leading),
    )
}

/// Unterstützte Remote-Protokolle. Die Variante ist Teil jeder stabilen Identität,
/// damit beispielsweise derselbe Hostname über SFTP und WebDAV nie verwechselt wird.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProtocol {
    Sftp,
    Ftp,
    Ftps,
    WebDav,
    Smb,
}

/// Öffentlicher SHA-256-Fingerabdruck des vollständigen DER-Serverzertifikats.
/// Der Pin ist kein Geheimnis und darf deshalb in Profilen und Identitäten stehen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FtpsCertificatePin {
    pub sha256: [u8; 32],
}

impl FtpsCertificatePin {
    pub fn validate(&self) -> io::Result<()> {
        require_digest("FTPS-Zertifikat", &self.sha256)
    }
}

/// Transportmodus der FTP-Providerfamilie. Der Default bleibt explizites TLS, damit
/// vorhandene Profile ohne das neuere Feld unverändert als FTPS geladen werden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FtpTransportSecurity {
    Plaintext,
    #[default]
    ExplicitTls,
}

/// Vertrauensmodell für explizites FTPS. `WebPkiAndPin` verlangt zusätzlich zur
/// exakten Zertifikatsbindung eine gültige öffentliche Kette und den Hostnamen.
/// `PinnedCertificate` ist für bewusst extern geprüfte private/self-signed Server.
/// `UnverifiedCertificate` verschlüsselt und pinnt ebenfalls exakt, behauptet aber
/// ausdrücklich keine geprüfte Serveridentität. Die TLS-Handshake-Signatur bleibt
/// in allen drei Modi kryptografisch geprüft.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FtpsCertificatePolicy {
    WebPkiAndPin,
    PinnedCertificate,
    UnverifiedCertificate,
}

/// Für UI, CLI und Statuszeile abgeleitete Transportaussage. Sie enthält weder Host
/// noch Konto und kann deshalb gefahrlos angezeigt oder maschinenlesbar ausgegeben
/// werden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FtpSecurityStatus {
    Plaintext,
    TlsIdentityUnverified,
    TlsIdentityExternallyVerified,
    TlsIdentityVerified,
}

/// Geheimnisfreie Konfiguration für FTP oder explizites FTPS (`AUTH TLS`, `PROT P`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FtpsConnectionConfig {
    pub host: String,
    /// Alternative Netzwerkziele derselben logischen Serveridentität, zum Beispiel
    /// eine zusätzlich im Zugangsdokument genannte IPv6-Adresse. TLS prüft weiterhin
    /// den primären `host` als Servernamen; jede Alternative muss vorher denselben
    /// Zertifikatspin geliefert haben.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternate_hosts: Vec<String>,
    pub port: u16,
    pub username: String,
    pub requested_root: String,
    #[serde(default)]
    pub transport_security: FtpTransportSecurity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<FtpsCertificatePin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_policy: Option<FtpsCertificatePolicy>,
    pub password_reference: KeychainSecretReference,
}

impl FtpsConnectionConfig {
    pub fn validate(&self) -> io::Result<()> {
        validate_host(&self.host)?;
        validate_alternate_hosts(&self.host, &self.alternate_hosts)?;
        if self.port == 0 {
            return Err(invalid("FTP-Port 0 ist ungültig"));
        }
        validate_reference_part("FTP-Nutzername", &self.username)?;
        if self.requested_root.is_empty()
            || self.requested_root.len() > 4096
            || !self.requested_root.starts_with('/')
            || self.requested_root.chars().any(char::is_control)
            || self.requested_root.split('/').any(|part| part == "..")
        {
            return Err(invalid("FTP-Root ist ungültig"));
        }
        match self.transport_security {
            FtpTransportSecurity::Plaintext => {
                if self.certificate.is_some() || self.certificate_policy.is_some() {
                    return Err(invalid(
                        "Klartext-FTP darf keine TLS-Zertifikatsbindung vortäuschen",
                    ));
                }
            }
            FtpTransportSecurity::ExplicitTls => {
                self.certificate
                    .as_ref()
                    .ok_or_else(|| invalid("FTPS-Zertifikat fehlt"))?
                    .validate()?;
                if self.certificate_policy.is_none() {
                    return Err(invalid("FTPS-Zertifikatsrichtlinie fehlt"));
                }
            }
        }
        self.password_reference.validate()
    }

    pub fn normalized_host(&self) -> String {
        self.host.trim_end_matches('.').to_ascii_lowercase()
    }

    pub(crate) fn candidate_hosts(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.host.as_str()).chain(self.alternate_hosts.iter().map(String::as_str))
    }

    pub fn security_status(&self) -> FtpSecurityStatus {
        match (self.transport_security, self.certificate_policy) {
            (FtpTransportSecurity::Plaintext, _) => FtpSecurityStatus::Plaintext,
            (FtpTransportSecurity::ExplicitTls, Some(FtpsCertificatePolicy::WebPkiAndPin)) => {
                FtpSecurityStatus::TlsIdentityVerified
            }
            (FtpTransportSecurity::ExplicitTls, Some(FtpsCertificatePolicy::PinnedCertificate)) => {
                FtpSecurityStatus::TlsIdentityExternallyVerified
            }
            (
                FtpTransportSecurity::ExplicitTls,
                Some(FtpsCertificatePolicy::UnverifiedCertificate) | None,
            ) => FtpSecurityStatus::TlsIdentityUnverified,
        }
    }
}

/// Stabile Identität einer angemeldeten FTP-/FTPS-Sitzung. Transportmodus, optionales
/// Zertifikat, Root und maschinenlesbare Serverfähigkeiten bleiben beim Reconnect
/// gebunden. Klartext-FTP kann dabei ausdrücklich keine kryptografische
/// Serveridentität behaupten.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FtpsServerIdentity {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub transport_security: FtpTransportSecurity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<FtpsCertificatePin>,
    pub canonical_root: String,
    pub features_digest: [u8; 32],
    pub welcome_digest: [u8; 32],
    /// Nur im Speicher gehaltener Kompatibilitätsbeleg für authentisierte
    /// Migrationen alter Bindungen. Er gehört weder zur aktuellen Identität noch
    /// in Pläne, Baselines oder Queuejournale.
    #[serde(skip)]
    pub legacy_welcome_digest: [u8; 32],
}

impl PartialEq for FtpsServerIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host
            && self.port == other.port
            && self.transport_security == other.transport_security
            && self.certificate == other.certificate
            && self.canonical_root == other.canonical_root
            && self.features_digest == other.features_digest
            && self.welcome_digest == other.welcome_digest
    }
}

impl Eq for FtpsServerIdentity {}

impl FtpsServerIdentity {
    pub fn validate(&self) -> io::Result<()> {
        validate_host(&self.host)?;
        if self.port == 0 {
            return Err(invalid("FTP-Identität enthält Port 0"));
        }
        match self.transport_security {
            FtpTransportSecurity::Plaintext if self.certificate.is_some() => {
                return Err(invalid(
                    "Klartext-FTP-Identität darf kein TLS-Zertifikat enthalten",
                ));
            }
            FtpTransportSecurity::ExplicitTls => self
                .certificate
                .as_ref()
                .ok_or_else(|| invalid("FTPS-Identität enthält kein Zertifikat"))?
                .validate()?,
            FtpTransportSecurity::Plaintext => {}
        }
        if !self.canonical_root.starts_with('/')
            || self.canonical_root.len() > 4096
            || self.canonical_root.chars().any(char::is_control)
            || self.canonical_root.split('/').any(|part| part == "..")
        {
            return Err(invalid("FTP-Identität enthält keinen absoluten Root"));
        }
        require_digest("FTP-Fähigkeiten", &self.features_digest)?;
        require_digest("FTP-Willkommenstext", &self.welcome_digest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FtpsRootIdentity {
    pub server: FtpsServerIdentity,
    pub virtual_root: String,
    pub session_generation: u64,
}

impl FtpsRootIdentity {
    pub fn validate(&self) -> io::Result<()> {
        self.server.validate()?;
        if !self.virtual_root.starts_with('/')
            || self.virtual_root.chars().any(char::is_control)
            || self.virtual_root.split('/').any(|part| part == "..")
            || self.session_generation == 0
        {
            return Err(invalid("FTP-Root-Identität ist ungültig"));
        }
        Ok(())
    }
}

pub type WebDavCertificatePin = FtpsCertificatePin;
pub type WebDavCertificatePolicy = FtpsCertificatePolicy;

/// Geheimnisfreie Konfiguration einer einzelnen HTTPS-WebDAV-Collection. Die Origin
/// enthält bewusst weder Nutzerinfo noch Pfad; Zugangsdaten bleiben referenziert.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebDavConnectionConfig {
    pub origin: String,
    pub username: String,
    pub requested_root: String,
    pub certificate: WebDavCertificatePin,
    pub certificate_policy: WebDavCertificatePolicy,
    pub password_reference: KeychainSecretReference,
}

impl WebDavConnectionConfig {
    pub fn validate(&self) -> io::Result<()> {
        validate_webdav_origin(&self.origin)?;
        validate_reference_part("WebDAV-Nutzername", &self.username)?;
        if self.certificate_policy == FtpsCertificatePolicy::UnverifiedCertificate {
            return Err(invalid(
                "WebDAV unterstützt keine ungeprüfte Zertifikatsidentität",
            ));
        }
        if self.username.contains(':') {
            return Err(invalid(
                "WebDAV-Nutzername darf für Basic-Authentifizierung keinen Doppelpunkt enthalten",
            ));
        }
        validate_collection_root(&self.requested_root)?;
        self.certificate.validate()?;
        self.password_reference.validate()
    }

    pub fn normalized_origin(&self) -> io::Result<String> {
        normalized_origin(validate_webdav_origin(&self.origin)?)
    }
}

/// Stabile Identität einer authentisierten WebDAV-Sitzung. Zertifikat, Konto,
/// kanonischer Collection-Root und Serverfähigkeiten müssen beim Reconnect gleich sein.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WebDavServerIdentity {
    pub origin: String,
    pub certificate: WebDavCertificatePin,
    pub account_digest: [u8; 32],
    pub canonical_root: String,
    pub dav_header_digest: [u8; 32],
    pub server_header_digest: [u8; 32],
    /// Im Speicher verfügbare Digests der früheren Headeralgorithmen. Sie werden
    /// ausschließlich nach einer frischen authentisierten Verbindung verwendet,
    /// um alte Endpunktbindungen zu prüfen und anschließend neu zu schreiben.
    #[serde(skip)]
    pub legacy_server_header_digests: Vec<[u8; 32]>,
}

impl PartialEq for WebDavServerIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.origin == other.origin
            && self.certificate == other.certificate
            && self.account_digest == other.account_digest
            && self.canonical_root == other.canonical_root
            && self.dav_header_digest == other.dav_header_digest
            && self.server_header_digest == other.server_header_digest
    }
}

impl Eq for WebDavServerIdentity {}

impl WebDavServerIdentity {
    pub fn validate(&self) -> io::Result<()> {
        if normalized_origin(validate_webdav_origin(&self.origin)?)? != self.origin {
            return Err(invalid("WebDAV-Identität enthält keine kanonische Origin"));
        }
        self.certificate.validate()?;
        validate_collection_root(&self.canonical_root)?;
        if self.canonical_root != "/" && self.canonical_root.ends_with('/') {
            return Err(invalid("WebDAV-Identität enthält keinen kanonischen Root"));
        }
        require_digest("WebDAV-Konto", &self.account_digest)?;
        require_digest("WebDAV-DAV-Header", &self.dav_header_digest)?;
        require_digest("WebDAV-Server-Header", &self.server_header_digest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebDavRootIdentity {
    pub server: WebDavServerIdentity,
    pub virtual_root: String,
    /// Starke ETag der Collection beim Plan-/Permit-Snapshot. Derselbe Pfad in
    /// derselben Sitzung ist ohne diesen Token keine gebundene Rootidentität.
    ///
    /// `default` dient ausschließlich dazu, ältere Persistenzstände bis zu ihrer
    /// versionsgebundenen Migration lesen zu können. Jede fachliche Verwendung
    /// ruft anschließend `validate()` auf und lehnt den leeren Wert ab.
    #[serde(default)]
    pub root_etag: String,
    pub session_generation: u64,
}

/// Geheimnisfreie SMB3-Verbindung. Das Passwort bleibt ausschließlich über seinen
/// Schlüsselbundverweis erreichbar; DFS wird im ersten Schnitt nicht verfolgt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmbConnectionConfig {
    pub host: String,
    pub port: u16,
    pub server_guid: [u8; 16],
    pub share: String,
    pub username: String,
    pub domain: String,
    pub requested_root: String,
    pub password_reference: KeychainSecretReference,
}

impl SmbConnectionConfig {
    pub fn validate(&self) -> io::Result<()> {
        validate_host(&self.host)?;
        if self.port == 0 {
            return Err(invalid("SMB-Port 0 ist ungültig"));
        }
        if self.server_guid == [0; 16] {
            return Err(invalid("SMB-Server-GUID darf nicht leer sein"));
        }
        validate_reference_part("SMB-Freigabe", &self.share)?;
        if self.share.contains(['/', '\\']) {
            return Err(invalid("SMB-Freigabe darf keinen Pfad enthalten"));
        }
        validate_reference_part("SMB-Nutzername", &self.username)?;
        if !self.domain.is_empty() {
            validate_reference_part("SMB-Domäne", &self.domain)?;
        }
        validate_smb_root(&self.requested_root)?;
        self.password_reference.validate()
    }

    pub fn normalized_host(&self) -> String {
        self.host.trim_end_matches('.').to_ascii_lowercase()
    }
}

/// Stabile SMB-Server-/Freigabeidentität. Die sitzungsgebundene Tree-ID steht
/// getrennt in [`SmbRootIdentity`], damit ein geprüfter Reconnect möglich bleibt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmbServerIdentity {
    pub host: String,
    pub port: u16,
    pub server_guid: [u8; 16],
    pub share: String,
    pub account_digest: [u8; 32],
    pub canonical_root: String,
    pub dialect: u16,
    pub signing_active: bool,
    pub encryption_active: bool,
    pub signing_algorithm: String,
    pub encryption_cipher: String,
}

impl SmbServerIdentity {
    pub fn validate(&self) -> io::Result<()> {
        validate_host(&self.host)?;
        if self.port == 0
            || self.server_guid == [0; 16]
            || self.dialect != 0x0311
            || !self.signing_active
            || !self.encryption_active
        {
            return Err(invalid("SMB3-Serveridentität ist unvollständig"));
        }
        validate_reference_part("SMB-Freigabe", &self.share)?;
        validate_smb_root(&self.canonical_root)?;
        require_digest("SMB-Konto", &self.account_digest)?;
        validate_reference_part("SMB-Signing-Algorithmus", &self.signing_algorithm)?;
        validate_reference_part("SMB-Verschlüsselung", &self.encryption_cipher)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmbRootIdentity {
    pub server: SmbServerIdentity,
    pub virtual_root: String,
    pub root_file_id: u64,
    pub tree_id: u32,
    pub session_generation: u64,
}

impl SmbRootIdentity {
    pub fn validate(&self) -> io::Result<()> {
        self.server.validate()?;
        validate_smb_root(&self.virtual_root)?;
        if self.root_file_id == 0 || self.tree_id == 0 || self.session_generation == 0 {
            return Err(invalid("SMB-Root-Identität ist ungültig"));
        }
        Ok(())
    }
}

impl WebDavRootIdentity {
    pub fn validate(&self) -> io::Result<()> {
        self.server.validate()?;
        validate_collection_root(&self.virtual_root)?;
        if (self.virtual_root != "/" && self.virtual_root.ends_with('/'))
            || self.root_etag.starts_with("W/")
            || self.root_etag.len() < 2
            || !self.root_etag.starts_with('"')
            || !self.root_etag.ends_with('"')
            || self.root_etag.chars().any(char::is_control)
            || self.session_generation == 0
        {
            return Err(invalid("WebDAV-Root-Identität ist ungültig"));
        }
        Ok(())
    }
}

/// Verweis auf ein Geheimnis im System-Schlüsselbund. `service` und `account` sind
/// nur Suchschlüssel; Passwort, Token oder Schlüsselmaterial gehören nie in dieses DTO.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeychainSecretReference {
    pub service: String,
    pub account: String,
}

impl KeychainSecretReference {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> io::Result<Self> {
        let reference = Self {
            service: service.into(),
            account: account.into(),
        };
        validate_reference_part("Schlüsselbund-Dienst", &reference.service)?;
        validate_reference_part("Schlüsselbund-Konto", &reference.account)?;
        Ok(reference)
    }

    pub fn validate(&self) -> io::Result<()> {
        validate_reference_part("Schlüsselbund-Dienst", &self.service)?;
        validate_reference_part("Schlüsselbund-Konto", &self.account)
    }
}

/// Dauerhaft speicherbare Authentifizierungsreferenz für SFTP.
///
/// Ein privater Schlüsselpfad ist kein Geheimnis. Eine optionale Passphrase wird aber
/// ausschließlich über einen Schlüsselbund-Verweis bezeichnet. Beim SSH-Agenten wird
/// der erwartete öffentliche Schlüssel per SHA-256 gebunden, damit ein später anderer
/// Agent-Schlüssel nicht unbemerkt verwendet werden kann.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SftpAuthenticationReference {
    Password {
        secret: KeychainSecretReference,
    },
    PrivateKey {
        path: String,
        passphrase: Option<KeychainSecretReference>,
        public_key_sha256: [u8; 32],
    },
    SshAgent {
        public_key_sha256: [u8; 32],
    },
}

impl SftpAuthenticationReference {
    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::Password { secret } => secret.validate(),
            Self::PrivateKey {
                path,
                passphrase,
                public_key_sha256,
            } => {
                if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
                    return Err(invalid("Privater Schlüsselpfad ist ungültig"));
                }
                require_digest("Öffentlicher Schlüssel", public_key_sha256)?;
                if let Some(reference) = passphrase {
                    reference.validate()?;
                }
                Ok(())
            }
            Self::SshAgent { public_key_sha256 } => {
                require_digest("SSH-Agent-Schlüssel", public_key_sha256)
            }
        }
    }
}

/// Explizit bestätigter SSH-Hostkey. Der Digest enthält nur den öffentlichen
/// Serverschlüssel und kann daher gefahrlos in Plänen und Queue-Journalen stehen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SftpHostKeyPin {
    pub algorithm: String,
    pub sha256: [u8; 32],
}

impl SftpHostKeyPin {
    pub fn validate(&self) -> io::Result<()> {
        validate_reference_part("Hostkey-Algorithmus", &self.algorithm)?;
        require_digest("Hostkey", &self.sha256)
    }
}

/// Geheimnisfreie Nutzereinstellung für eine SFTP-Verbindung. `requested_root` wird
/// nach der Anmeldung per REALPATH kanonisiert; erst dieser Serverwert geht in die
/// laufzeitgebundene Identität ein.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SftpConnectionConfig {
    pub host: String,
    /// Alternative Netzwerkziele derselben gepinnten SSH-Serveridentität. Sie
    /// werden nur nach einem Transportfehler des primären Hosts versucht.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternate_hosts: Vec<String>,
    pub port: u16,
    pub username: String,
    pub requested_root: String,
    pub host_key: SftpHostKeyPin,
    pub authentication: SftpAuthenticationReference,
}

impl SftpConnectionConfig {
    pub fn validate(&self) -> io::Result<()> {
        validate_host(&self.host)?;
        validate_alternate_hosts(&self.host, &self.alternate_hosts)?;
        if self.port == 0 {
            return Err(invalid("SFTP-Port 0 ist ungültig"));
        }
        validate_reference_part("SFTP-Nutzername", &self.username)?;
        if self.requested_root.is_empty()
            || self.requested_root.len() > 4096
            || self.requested_root.contains('\0')
        {
            return Err(invalid("SFTP-Root ist ungültig"));
        }
        self.host_key.validate()?;
        self.authentication.validate()
    }

    /// DNS-Namen sind case-insensitiv. Die normalisierte Form verhindert, dass bloße
    /// Schreibweisenänderungen zwei verschiedene Queue-Ziele vortäuschen.
    pub fn normalized_host(&self) -> String {
        self.host.trim_end_matches('.').to_ascii_lowercase()
    }

    pub(crate) fn candidate_hosts(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.host.as_str()).chain(self.alternate_hosts.iter().map(String::as_str))
    }
}

/// Stabile, nach erfolgreichem Handshake und REALPATH ermittelte SFTP-Identität.
/// Reconnects dürfen nur weiterarbeiten, wenn exakt diese Identität erneut entsteht.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SftpServerIdentity {
    pub host: String,
    pub port: u16,
    pub host_key: SftpHostKeyPin,
    pub canonical_root: String,
    pub sftp_version: u32,
    /// Kanonischer Digest aller vom Server gemeldeten SFTP-Erweiterungen.
    pub extensions_digest: [u8; 32],
}

/// Identität eines konkreten virtuellen Roots innerhalb einer SFTP-Verbindung.
/// Die Sitzungsgeneration ist absichtlich enthalten: Ein Reconnect darf zwar dieselbe
/// Serveridentität erneut bestätigen, macht aber jeden bereits sichtbaren Plan ungültig.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SftpRootIdentity {
    pub server: SftpServerIdentity,
    pub virtual_root: String,
    pub session_generation: u64,
}

impl SftpRootIdentity {
    pub fn validate(&self) -> io::Result<()> {
        self.server.validate()?;
        if !self.virtual_root.starts_with('/')
            || self.virtual_root.contains('\0')
            || self.virtual_root.contains("/../")
            || self.session_generation == 0
        {
            return Err(invalid("SFTP-Root-Identität ist ungültig"));
        }
        Ok(())
    }
}

impl SftpServerIdentity {
    pub fn validate(&self) -> io::Result<()> {
        validate_host(&self.host)?;
        if self.port == 0 {
            return Err(invalid("SFTP-Identität enthält Port 0"));
        }
        self.host_key.validate()?;
        if !self.canonical_root.starts_with('/')
            || self.canonical_root.contains('\0')
            || self.canonical_root.len() > 4096
        {
            return Err(invalid(
                "SFTP-Identität enthält keinen absoluten kanonischen Root",
            ));
        }
        if self.sftp_version == 0 {
            return Err(invalid("SFTP-Protokollversion 0 ist ungültig"));
        }
        require_digest("SFTP-Erweiterungen", &self.extensions_digest)
    }
}

/// Sicherheitsrelevante Fähigkeiten einer konkreten, bereits geprüften Sitzung.
/// Jede Fähigkeit startet `false` und wird ausschließlich durch Protokollangaben plus
/// zerstörungsfreie Probe unter einem künstlichen Provider-Testordner freigeschaltet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteCapabilities {
    pub complete_listing: bool,
    /// Ein neuer Ordner kann unter einem vollständig als fehlend gelisteten Namen
    /// angelegt, als echtes Verzeichnis rückgelesen und anschließend gezielt entfernt
    /// werden. Ein idempotentes Provider-MKDIR ist zulässig: Eine späte gleichnamige
    /// Verzeichniskollision ersetzt weder Dateien noch vorhandenen Verzeichnisinhalt.
    /// Diese Fähigkeit ist bewusst unabhängig vom Datei-Upload-Vertrag.
    #[serde(default)]
    pub exclusive_directory_create: bool,
    pub exclusive_create: bool,
    pub atomic_noreplace_rename: bool,
    /// Der Provider führt ein gewöhnliches Protokoll-Rename aus und der Readback
    /// bestätigt es. Ein bereits vorhandenes Ziel kann dabei aber serverseitig
    /// ersetzt werden; zwischen frischer Bestandsprüfung und Ausführung bleibt ein
    /// kleines Kollisionsfenster. Diese Fähigkeit trägt deshalb ausschließlich die
    /// interaktive Best-Effort-Umbenennung nach sichtbarer Kollisionprüfung — nie
    /// einen unbeaufsichtigten Sync-Commit und nie `permits_update()`.
    #[serde(default)]
    pub best_effort_rename: bool,
    /// Exklusives Temp-Anlegen, unabhängiger Inhalts-Readback und ein einmaliger,
    /// rückgelesener Commit an den endgültigen Namen sind bewiesen. Mindestens eine
    /// stärkere Garantie fehlt aber: atomarer No-Replace-Commit oder dauerhaftes
    /// Datei-Flush. Ein neuer interaktiver Upload kann damit sichtbar als Best Effort
    /// laufen; eine fehlende Einzelgarantie steht in der Provider-Einschränkung.
    /// Diese Fähigkeit trägt niemals `permits_add_only()` oder einen Sync-Modus.
    #[serde(default)]
    pub best_effort_file_commit: bool,
    /// Der Provider hat das Ersetzen genau einer künstlichen vorhandenen Datei samt
    /// Inhalts-Readback belegt, bietet dafür aber keine wiederherstellbare
    /// Altversion. Trägt ausschließlich ausdrücklich bestätigte interaktive
    /// Ersetzungen, nie Sync, Mirror oder Quarantäne.
    #[serde(default)]
    pub best_effort_file_replace: bool,
    pub atomic_replace_rename: bool,
    pub durable_file_flush: bool,
    /// Der Provider kann einen neuen Datenstrom nach Transferabschluss unabhängig
    /// rücklesen und erst danach exklusiv an den endgültigen Namen hängen. Das ist
    /// etwa bei FTPS die sichere Alternative zu einem nicht vorhandenen Datei-fsync.
    pub verified_file_commit: bool,
    pub durable_directory_flush: bool,
    pub quarantine: bool,
    /// Der Provider hat das gezielte Entfernen einer künstlichen regulären Datei
    /// und eines künstlichen leeren Verzeichnisses in dieser Sitzung bestanden.
    /// Das ist nur die interne Löschprimitive für Quarantäne-Cleanup und
    /// Quellentfernung; sie gibt niemals von selbst „Dauerhaft löschen" frei.
    #[serde(default)]
    pub entry_delete: bool,
    /// Eine reguläre Datei und ein leeres Verzeichnis können nach frischem,
    /// vollständig gebundenem Scan gezielt entfernt werden. Das ist ausdrücklich
    /// keine Papierkorb- oder Quarantänegarantie.
    #[serde(default)]
    pub permanent_delete: bool,
    pub available_space: bool,
}

impl RemoteCapabilities {
    pub fn permits_directory_create(self) -> bool {
        self.complete_listing && self.exclusive_directory_create
    }

    pub fn permits_add_only(self) -> bool {
        self.complete_listing
            && self.exclusive_create
            && (self.durable_file_flush || self.verified_file_commit)
    }

    pub fn permits_entry_delete(self) -> bool {
        self.complete_listing && (self.entry_delete || self.permanent_delete)
    }

    pub fn permits_update(self) -> bool {
        self.permits_add_only()
            && self.atomic_noreplace_rename
            && self.atomic_replace_rename
            && self.durable_file_flush
            && self.permits_entry_delete()
            && self.quarantine
    }

    pub fn permits_mirror(self) -> bool {
        self.permits_update() && self.durable_directory_flush && self.available_space
    }
}

pub(crate) fn capabilities_digest(value: RemoteCapabilities) -> io::Result<[u8; 32]> {
    // Version 8: die getrennte interaktive Best-Effort-Ersetzung kam hinzu.
    // Version 7: `best_effort_file_commit` umfasst zusätzlich den verifizierten
    // SFTP-Commit ohne dauerhaftes Datei-Flush. In Version 6 kam die vom
    // Uploadvertrag getrennte Löschprimitive hinzu.
    // Der Versionssprung erzwingt, dass alte und neue Capability-Belege nie
    // denselben Digest liefern.
    let payload = serde_json::to_vec(&(8_u32, value)).map_err(io::Error::other)?;
    Ok(*blake3::hash(&payload).as_bytes())
}

pub(crate) fn validate_host(host: &str) -> io::Result<()> {
    if host.is_empty()
        || host.len() > 253
        || host.starts_with('.')
        || host.contains('/')
        || host
            .chars()
            .any(|value| value.is_control() || value.is_whitespace())
    {
        return Err(invalid("Remote-Hostname ist ungültig"));
    }
    Ok(())
}

fn validate_alternate_hosts(primary: &str, alternates: &[String]) -> io::Result<()> {
    if alternates.len() > 8 {
        return Err(invalid(
            "Remote-Endpunkt besitzt zu viele Alternativadressen",
        ));
    }
    let primary = primary.trim_end_matches('.').to_ascii_lowercase();
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(primary);
    for host in alternates {
        validate_host(host)?;
        let normalized = host.trim_end_matches('.').to_ascii_lowercase();
        if !seen.insert(normalized) {
            return Err(invalid(
                "Remote-Endpunkt enthält eine doppelte Alternativadresse",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_webdav_origin(raw: &str) -> io::Result<Url> {
    let url = Url::parse(raw).map_err(|_| invalid("WebDAV-Origin ist ungültig"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid(
            "WebDAV-Origin muss ausschließlich aus HTTPS-Schema, Host und Port bestehen",
        ));
    }
    Ok(url)
}

fn normalized_origin(mut url: Url) -> io::Result<String> {
    let host = url
        .host_str()
        .ok_or_else(|| invalid("WebDAV-Origin enthält keinen Host"))?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    url.set_host(Some(&host))
        .map_err(|_| invalid("WebDAV-Host lässt sich nicht normalisieren"))?;
    if url.port_or_known_default() == Some(443) {
        url.set_port(None)
            .map_err(|_| invalid("WebDAV-Origin-Port ist ungültig"))?;
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

/// Prüft die einzelnen Komponenten eines absoluten Remote-Roots.
///
/// Eine leere Komponente ist nur als abschließender Schrägstrich zulässig; sonst
/// stünde ein Doppel-Slash für einen anderen Pfad als gemeint. `.` und `..` sind
/// überall verboten, damit ein Root nicht aus seinem Namensraum heraustreten
/// kann. `label` benennt den Root im Fehlertext.
fn validate_root_components(label: &str, root: &str) -> io::Result<()> {
    let parts = root.split('/').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            if index + 1 == parts.len() {
                continue;
            }
            return Err(invalid(format!("{label} enthält eine leere Komponente")));
        }
        if matches!(*part, "." | "..") {
            return Err(invalid(format!("{label} enthält Traversal")));
        }
    }
    Ok(())
}

fn validate_collection_root(root: &str) -> io::Result<()> {
    // `?` und `#` würden in einer URL Query beziehungsweise Fragment einleiten und
    // damit einen anderen Pfad bezeichnen als den gespeicherten.
    if root.is_empty()
        || root.len() > 4096
        || !root.starts_with('/')
        || root.chars().any(char::is_control)
        || root.contains('?')
        || root.contains('#')
    {
        return Err(invalid("WebDAV-Collection-Root ist ungültig"));
    }
    validate_root_components("WebDAV-Collection-Root", root)
}

fn validate_smb_root(root: &str) -> io::Result<()> {
    // SMB trennt selbst mit Backslash; ein solcher im gespeicherten Root wäre ein
    // zweiter, ungeprüfter Trenner.
    if root.is_empty()
        || root.len() > 4096
        || !root.starts_with('/')
        || root.contains('\\')
        || root.chars().any(char::is_control)
    {
        return Err(invalid("SMB-Root ist ungültig"));
    }
    validate_root_components("SMB-Root", root)
}

fn validate_reference_part(label: &str, value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(invalid(format!("{label} ist ungültig")));
    }
    Ok(())
}

/// Digest über der leeren Eingabe.
///
/// Ein Digest, der über nichts gebildet wurde, ist für jede Eingabe derselbe und
/// taugt deshalb nicht zur Identitätsbindung. Er ist nicht null und würde die
/// Nullprüfung allein passieren.
fn empty_payload_digest() -> [u8; 32] {
    static DIGEST: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *DIGEST.get_or_init(|| *blake3::hash(&[]).as_bytes())
}

fn require_digest(label: &str, digest: &[u8; 32]) -> io::Result<()> {
    if digest.iter().all(|byte| *byte == 0) {
        return Err(invalid(format!("{label}-Digest darf nicht leer sein")));
    }
    if *digest == empty_payload_digest() {
        return Err(invalid(format!(
            "{label}-Digest wurde über eine leere Eingabe gebildet und bindet nichts"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {

    /// Ein Digest über der leeren Eingabe ist für jede Eingabe derselbe. Er darf
    /// deshalb keine Identität binden, auch wenn er nicht null ist.
    #[test]
    fn digest_ueber_leerer_eingabe_bindet_nichts() {
        let empty = *blake3::hash(&[]).as_bytes();
        assert!(empty.iter().any(|byte| *byte != 0), "nicht der Nullfall");
        let error = require_digest("Test", &empty).unwrap_err();
        assert!(error.to_string().contains("leere Eingabe"));

        assert!(require_digest("Test", &[0; 32]).is_err());
        assert!(require_digest("Test", &[7; 32]).is_ok());
    }
    use super::*;

    fn digest(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn verzeichnis_create_ist_nicht_an_datei_upload_gekoppelt() {
        let capabilities = RemoteCapabilities {
            complete_listing: true,
            exclusive_directory_create: true,
            ..RemoteCapabilities::default()
        };

        assert!(capabilities.permits_directory_create());
        assert!(!capabilities.permits_add_only());
    }

    #[test]
    fn eintrag_loeschen_ist_nicht_an_datei_upload_gekoppelt() {
        let capabilities = RemoteCapabilities {
            complete_listing: true,
            entry_delete: true,
            ..RemoteCapabilities::default()
        };

        assert!(capabilities.permits_entry_delete());
        assert!(!capabilities.permits_add_only());
    }

    #[test]
    fn sftp_und_smb_remove_methoden_verlangen_das_eigene_loesch_gate() {
        fn method<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
            let start = source.find(start).expect("VFS-Methode fehlt");
            let rest = &source[start..];
            let end = rest.find(end).expect("nächste VFS-Methode fehlt");
            &rest[..end]
        }

        for (provider, source, old_gate) in [
            ("SFTP", include_str!("sftp.rs"), "require_add_only"),
            ("SMB", include_str!("smb.rs"), "require_write_capability"),
        ] {
            for (start, end) in [
                (
                    "fn remove_file(&self, path: &Path)",
                    "fn remove_dir_all(&self, path: &Path)",
                ),
                (
                    "fn remove_dir_all(&self, path: &Path)",
                    "fn remove_dir(&self, path: &Path)",
                ),
                (
                    "fn remove_dir(&self, path: &Path)",
                    "fn rename(&self, from: &Path, to: &Path)",
                ),
            ] {
                let body = method(source, start, end);
                assert!(
                    body.contains("require_entry_delete"),
                    "{provider}-{start} umgeht das eigene Lösch-Gate"
                );
                assert!(
                    !body.contains(old_gate),
                    "{provider}-{start} hängt wieder am Upload-Gate"
                );
            }
        }
    }

    fn valid_config() -> SftpConnectionConfig {
        SftpConnectionConfig {
            host: "SFTP.Example.test.".into(),
            alternate_hosts: Vec::new(),
            port: 22,
            username: "testuser".into(),
            requested_root: "/backup".into(),
            host_key: SftpHostKeyPin {
                algorithm: "ssh-ed25519".into(),
                sha256: digest(1),
            },
            authentication: SftpAuthenticationReference::Password {
                secret: KeychainSecretReference::new("Doppeldecker", "sftp-example").unwrap(),
            },
        }
    }

    fn valid_ftps_config() -> FtpsConnectionConfig {
        FtpsConnectionConfig {
            host: "FTPS.Example.test.".into(),
            alternate_hosts: Vec::new(),
            port: 21,
            username: "testuser".into(),
            requested_root: "/backup".into(),
            transport_security: FtpTransportSecurity::ExplicitTls,
            certificate: Some(FtpsCertificatePin { sha256: digest(3) }),
            certificate_policy: Some(FtpsCertificatePolicy::WebPkiAndPin),
            password_reference: KeychainSecretReference::new("Doppeldecker", "ftps-example")
                .unwrap(),
        }
    }

    fn valid_webdav_config() -> WebDavConnectionConfig {
        WebDavConnectionConfig {
            origin: "https://DAV.Example.test.:443/".into(),
            username: "testuser".into(),
            requested_root: "/remote.php/dav/files/testuser/".into(),
            certificate: FtpsCertificatePin { sha256: digest(4) },
            certificate_policy: FtpsCertificatePolicy::WebPkiAndPin,
            password_reference: KeychainSecretReference::new("Doppeldecker", "dav-example")
                .unwrap(),
        }
    }

    fn valid_smb_config() -> SmbConnectionConfig {
        SmbConnectionConfig {
            host: "SMB.Example.test.".into(),
            port: 445,
            server_guid: [9; 16],
            share: "Daten".into(),
            username: "testuser".into(),
            domain: "WORKGROUP".into(),
            requested_root: "/backup".into(),
            password_reference: KeychainSecretReference::new("Doppeldecker", "smb-example")
                .unwrap(),
        }
    }

    #[test]
    fn konfiguration_enthaelt_nur_referenz_und_normalisiert_host() {
        let config = valid_config();
        config.validate().unwrap();
        assert_eq!(config.normalized_host(), "sftp.example.test");
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("sftp-example"));
        assert!(!json.contains("password\":"));

        let dav = valid_webdav_config();
        dav.validate().unwrap();
        assert_eq!(dav.normalized_origin().unwrap(), "https://dav.example.test");

        let smb = valid_smb_config();
        smb.validate().unwrap();
        assert_eq!(smb.normalized_host(), "smb.example.test");
        let json = serde_json::to_string(&smb).unwrap();
        assert!(json.contains("smb-example"));
        assert!(!json.contains("password\":"));
    }

    #[test]
    fn leere_digests_und_kontrollzeichen_werden_abgelehnt() {
        let mut config = valid_config();
        config.host_key.sha256 = [0; 32];
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.username = "name\nzweite-zeile".into();
        assert!(config.validate().is_err());

        let mut ftps = valid_ftps_config();
        ftps.requested_root = "/backup\r\nDELE fremd".into();
        assert!(ftps.validate().is_err());
        let mut ftps = valid_ftps_config();
        ftps.requested_root = "/backup/../fremd".into();
        assert!(ftps.validate().is_err());

        let mut dav = valid_webdav_config();
        dav.origin = "https://user:secret@dav.example.test/".into();
        assert!(dav.validate().is_err());

        let mut smb = valid_smb_config();
        smb.server_guid = [0; 16];
        assert!(smb.validate().is_err());
        let mut smb = valid_smb_config();
        smb.requested_root = "/backup//fremd".into();
        assert!(smb.validate().is_err());
        let mut smb = valid_smb_config();
        smb.share = "Daten/fremd".into();
        assert!(smb.validate().is_err());
        let mut dav = valid_webdav_config();
        dav.requested_root = "/dav/../fremd".into();
        assert!(dav.validate().is_err());
        let mut dav = valid_webdav_config();
        dav.requested_root = "/dav//mehrdeutig".into();
        assert!(dav.validate().is_err());
        let mut dav = valid_webdav_config();
        dav.username = "name:zweiter-teil".into();
        assert!(dav.validate().is_err());
    }

    #[test]
    fn ftp_transport_und_sichtbarer_sicherheitsstatus_bleiben_eindeutig() {
        let mut config = valid_ftps_config();
        assert_eq!(
            config.security_status(),
            FtpSecurityStatus::TlsIdentityVerified
        );
        config.certificate_policy = Some(FtpsCertificatePolicy::UnverifiedCertificate);
        assert_eq!(
            config.security_status(),
            FtpSecurityStatus::TlsIdentityUnverified
        );

        config.transport_security = FtpTransportSecurity::Plaintext;
        config.certificate = None;
        config.certificate_policy = None;
        assert!(config.validate().is_ok());
        assert_eq!(config.security_status(), FtpSecurityStatus::Plaintext);

        config.certificate = Some(FtpsCertificatePin { sha256: digest(3) });
        assert!(config.validate().is_err());
    }

    #[test]
    fn identitaet_verlangt_absoluten_server_root() {
        let identity = SftpServerIdentity {
            host: "sftp.example.test".into(),
            port: 22,
            host_key: valid_config().host_key,
            canonical_root: "relativ".into(),
            sftp_version: 3,
            extensions_digest: digest(2),
        };
        assert!(identity.validate().is_err());

        let config = valid_smb_config();
        let mut smb = SmbServerIdentity {
            host: config.normalized_host(),
            port: config.port,
            server_guid: config.server_guid,
            share: config.share,
            account_digest: digest(8),
            canonical_root: config.requested_root,
            dialect: 0x0311,
            signing_active: true,
            encryption_active: true,
            signing_algorithm: "AesCmac".into(),
            encryption_cipher: "Aes128Gcm".into(),
        };
        smb.validate().unwrap();
        smb.dialect = 0x0302;
        assert!(smb.validate().is_err());
    }

    #[test]
    fn webdav_root_identitaet_persistiert_und_validiert_die_starke_etag() {
        let identity = WebDavRootIdentity {
            server: WebDavServerIdentity {
                origin: "https://dav.example.test".into(),
                certificate: WebDavCertificatePin { sha256: digest(4) },
                account_digest: digest(5),
                canonical_root: "/dav".into(),
                dav_header_digest: digest(6),
                server_header_digest: digest(7),
                legacy_server_header_digests: Vec::new(),
            },
            virtual_root: "/auswahl".into(),
            root_etag: "\"root-42\"".into(),
            session_generation: 3,
        };
        identity.validate().unwrap();
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["root_etag"], "\"root-42\"");
        assert_eq!(
            serde_json::from_value::<WebDavRootIdentity>(json.clone()).unwrap(),
            identity
        );

        let mut weak = identity.clone();
        weak.root_etag = "W/\"root-42\"".into();
        assert!(weak.validate().is_err());
        let mut missing = json;
        missing.as_object_mut().unwrap().remove("root_etag");
        let legacy = serde_json::from_value::<WebDavRootIdentity>(missing).unwrap();
        assert!(legacy.validate().is_err());
    }

    #[test]
    fn faehigkeiten_schalten_modi_stufenweise_frei() {
        let mut capabilities = RemoteCapabilities {
            complete_listing: true,
            exclusive_create: true,
            durable_file_flush: true,
            ..RemoteCapabilities::default()
        };
        assert!(capabilities.permits_add_only());
        assert!(!capabilities.permits_update());

        capabilities.atomic_noreplace_rename = true;
        capabilities.atomic_replace_rename = true;
        capabilities.entry_delete = true;
        capabilities.quarantine = true;
        assert!(capabilities.permits_update());
        assert!(!capabilities.permits_mirror());

        let verified = RemoteCapabilities {
            complete_listing: true,
            exclusive_create: true,
            verified_file_commit: true,
            ..RemoteCapabilities::default()
        };
        assert!(verified.permits_add_only());
        assert!(!verified.permits_update());

        let best_effort = RemoteCapabilities {
            complete_listing: true,
            exclusive_create: true,
            atomic_noreplace_rename: true,
            best_effort_file_commit: true,
            ..RemoteCapabilities::default()
        };
        assert!(!best_effort.permits_add_only());
        assert!(!best_effort.permits_update());

        capabilities.durable_directory_flush = true;
        capabilities.available_space = true;
        assert!(capabilities.permits_mirror());
    }

    /// `result?; cleanup?;` verliert genau dann den Cleanup-Fehler, wenn er am
    /// meisten zählt: wenn auch die Probe selbst gescheitert ist und deshalb ein
    /// Rest unter dem gewählten Root liegt.
    #[test]
    fn cleanup_pfadanhaenge_bleiben_als_strukturierte_pruefhinweise_erhalten() {
        let root = tempfile::tempdir().unwrap();
        let retained = root.path().join("behalten.txt");
        std::fs::write(&retained, b"behalten").unwrap();
        let cleanup = std::fs::remove_dir(root.path()).unwrap_err();
        let kind = cleanup.kind();
        let cleanup = crate::error::with_recovery_paths(
            crate::coded_error(kind, crate::ErrorCode::OutcomeUnclear, cleanup.to_string()),
            &["/vorheriger-pruefpfad".into()],
        );
        let error = error_with_path(root.path(), cleanup);
        assert_eq!(error.kind(), kind);
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
        let paths = crate::error::recovery_paths(&error);
        assert!(paths.contains(&"/vorheriger-pruefpfad".into()));
        assert!(
            paths.contains(&root.path().to_string_lossy().into_owned()),
            "Der konkrete Restpfad fehlt außerhalb des deutschen Fehlertexts"
        );
        assert_eq!(std::fs::read(retained).unwrap(), b"behalten");
    }

    #[test]
    fn fehlgeschlagener_cleanup_meldet_den_belegten_restpfad_strukturiert() {
        let mut missing = Vec::new();
        for probe_failed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let base = root.path().join("probe");
            std::fs::create_dir(&base).unwrap();
            let retained = base.join("behalten.txt");
            std::fs::write(&retained, b"Rest zur manuellen Pruefung").unwrap();
            let cleanup = std::fs::remove_dir(&base).unwrap_err();
            let cleanup = crate::coded_error(
                cleanup.kind(),
                crate::ErrorCode::OutcomeUnclear,
                cleanup.to_string(),
            );
            let result = if probe_failed {
                std::fs::read(base.join("fehlende-quelle")).map(|_| ())
            } else {
                Ok(())
            };
            let error = combine_probe_outcome("Test", result, Err(cleanup), base.to_str().unwrap())
                .unwrap_err();
            assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
            assert_eq!(
                std::fs::read(&retained).unwrap(),
                b"Rest zur manuellen Pruefung"
            );
            if !crate::error::recovery_paths(&error).contains(&base.to_string_lossy().into_owned())
            {
                missing.push(probe_failed);
            }
        }
        assert!(missing.is_empty(), "Belegter Probenrest nur im Fehlertext; strukturierter Pfad fehlt bei probe_failed={missing:?}");
    }

    #[test]
    fn probe_und_cleanup_erhalten_alle_strukturierten_recoverypfade() {
        let error = |path: &str| {
            crate::error::with_recovery_paths(
                crate::coded_error(io::ErrorKind::Other, crate::ErrorCode::OutcomeUnclear, path),
                &[path.to_owned()],
            )
        };
        let combined = combine_probe_outcome(
            "FTPS",
            Err::<(), _>(error("/pane/first")),
            Err(error("/pane/second")),
            "/pane",
        )
        .unwrap_err();
        let mut paths = crate::error::recovery_paths(&combined);
        paths.sort();
        assert_eq!(paths, ["/pane", "/pane/first", "/pane/second"]);
        let folded =
            fold_cleanup_errors(vec![error("/pane/first"), error("/pane/second")]).unwrap_err();
        let mut paths = crate::error::recovery_paths(&folded);
        paths.sort();
        assert_eq!(paths, ["/pane/first", "/pane/second"]);
    }

    #[test]
    fn probenausgang_verliert_keinen_cleanup_fehler() {
        let probenfehler = || io::Error::other("Probe scheiterte");
        let cleanupfehler = || io::Error::other("Cleanup scheiterte");

        let beides = combine_probe_outcome(
            "Test",
            Err::<(), _>(probenfehler()),
            Err(cleanupfehler()),
            "/root/.probe",
        )
        .unwrap_err();
        let text = beides.to_string();
        assert!(text.contains("Probe scheiterte"), "{text}");
        assert!(text.contains("Cleanup scheiterte"), "{text}");
        assert!(text.contains("/root/.probe"), "{text}");

        // Nur der Cleanup scheitert: Der Recoverypfad muss trotzdem im Text stehen.
        let nur_cleanup =
            combine_probe_outcome("Test", Ok(()), Err(cleanupfehler()), "/root/.probe")
                .unwrap_err();
        assert!(nur_cleanup.to_string().contains("/root/.probe"));

        // Nur die Probe scheitert: Ihr Fehler bleibt unverändert erhalten.
        let nur_probe =
            combine_probe_outcome("Test", Err::<(), _>(probenfehler()), Ok(()), "/root/.probe")
                .unwrap_err();
        assert_eq!(nur_probe.to_string(), "Probe scheiterte");

        // Der Wert der Probe kommt unverändert durch.
        assert!(combine_probe_outcome("Test", Ok(true), Ok(()), "/root/.probe").unwrap());
    }

    /// Ein unklarer Ausgang verbietet jeden Wiederholversuch und darf deshalb nie
    /// hinter einer gewöhnlichen Fehlermeldung verschwinden.
    #[test]
    fn unklarer_ausgang_hat_vorrang() {
        let unklar = || {
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Ausgang unklar",
            )
        };
        let aus_cleanup = combine_probe_outcome(
            "Test",
            Err::<(), _>(io::Error::other("gewöhnlich")),
            Err(unklar()),
            "/root/.probe",
        )
        .unwrap_err();
        assert_eq!(
            crate::error_code(&aus_cleanup),
            crate::ErrorCode::OutcomeUnclear
        );

        let aus_probe = combine_probe_outcome(
            "Test",
            Err::<(), _>(unklar()),
            Err(io::Error::other("gewöhnlich")),
            "/root/.probe",
        )
        .unwrap_err();
        assert_eq!(
            crate::error_code(&aus_probe),
            crate::ErrorCode::OutcomeUnclear
        );
    }

    /// Der zusammengefasste Fehler muss beide Ursachen und den Code behalten, der
    /// über Reconnect, Nutzerhinweis und Wiederholung entscheidet.
    #[test]
    fn zusammengefasster_fehler_behaelt_beide_ursachen_und_den_errorcode() {
        let verbindungsfehler = || io::Error::new(io::ErrorKind::UnexpectedEof, "Leitung weg");
        let cleanupfehler =
            || io::Error::new(io::ErrorKind::PermissionDenied, "Cleanup verweigert");

        // Probe und Cleanup scheitern gewöhnlich: Der Probenfehler führt, sein Code
        // bleibt erhalten — vorher wurde daraus ein rohes `invalid_data`.
        let beides = combine_probe_outcome(
            "Test",
            Err::<(), _>(verbindungsfehler()),
            Err(cleanupfehler()),
            "/root/.probe",
        )
        .unwrap_err();
        assert_eq!(
            crate::error_code(&beides),
            crate::ErrorCode::ConnectionLost,
            "{beides}"
        );
        assert_eq!(beides.kind(), io::ErrorKind::UnexpectedEof);
        assert!(beides.to_string().contains("Leitung weg"), "{beides}");
        assert!(
            beides.to_string().contains("Cleanup verweigert"),
            "{beides}"
        );

        // Nur der Cleanup scheitert: Dann führt er und behält seinen Code.
        let nur_cleanup =
            combine_probe_outcome("Test", Ok(()), Err(cleanupfehler()), "/root/.probe")
                .unwrap_err();
        assert_eq!(
            crate::error_code(&nur_cleanup),
            crate::ErrorCode::PermissionDenied,
            "{nur_cleanup}"
        );
    }

    /// Der unklare Ausgang stellt den Code, verschluckt aber die zweite Ursache nicht.
    #[test]
    fn unklarer_ausgang_nennt_trotzdem_beide_ursachen() {
        let unklar = || {
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Ausgang unklar",
            )
        };

        for (probe, cleanup) in [
            (unklar(), io::Error::other("gewöhnlich")),
            (io::Error::other("gewöhnlich"), unklar()),
        ] {
            let fehler =
                combine_probe_outcome("Test", Err::<(), _>(probe), Err(cleanup), "/root/.probe")
                    .unwrap_err();
            assert_eq!(
                crate::error_code(&fehler),
                crate::ErrorCode::OutcomeUnclear,
                "{fehler}"
            );
            assert!(fehler.to_string().contains("Ausgang unklar"), "{fehler}");
            assert!(fehler.to_string().contains("gewöhnlich"), "{fehler}");
            assert!(fehler.to_string().contains("/root/.probe"), "{fehler}");
        }
    }

    #[test]
    fn mehrere_cleanup_fehler_behalten_den_ersten_unklaren_ausgang() {
        assert!(fold_cleanup_errors(Vec::new()).is_ok());

        let ordinary = fold_cleanup_errors(vec![
            io::Error::new(io::ErrorKind::PermissionDenied, "eins"),
            io::Error::new(io::ErrorKind::NotFound, "zwei"),
        ])
        .unwrap_err();
        assert_eq!(
            crate::error_code(&ordinary),
            crate::ErrorCode::PermissionDenied
        );
        assert!(ordinary.to_string().contains("eins"));
        assert!(ordinary.to_string().contains("zwei"));

        let unclear = fold_cleanup_errors(vec![
            io::Error::new(io::ErrorKind::PermissionDenied, "gewöhnlich davor"),
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                "Ausgang unklar",
            ),
            io::Error::new(io::ErrorKind::NotFound, "gewöhnlich danach"),
        ])
        .unwrap_err();
        assert_eq!(
            crate::error_code(&unclear),
            crate::ErrorCode::OutcomeUnclear
        );
        assert!(unclear.to_string().contains("gewöhnlich davor"));
        assert!(unclear.to_string().contains("gewöhnlich danach"));
    }

    /// Der unsymmetrische Zweizeiler darf in keiner Probe zurückkommen.
    /// Regression: Die Guard-Bedingung oben trifft nur das eine wörtliche Muster.
    /// Zwei SMB-Proben hatten die Zusammenfassung von Probe- und Cleanup-Fehler
    /// stattdessen von Hand nachgebaut — mit `io::Error::other` beziehungsweise
    /// `io::Error::new`, die beide den `ErrorCode` wegwerfen. Genau der verlorene
    /// `OutcomeUnclear` hätte dort einen Wiederholversuch verboten, und
    /// `ConnectionLost` wurde zur dauerhaften Fähigkeitsgrenze statt zu einem
    /// Reconnect. Dieser Test prüft deshalb die Regel selbst: Wo in einer
    /// Providerdatei von einem Cleanup die Rede ist, darf kein Fehler ohne Code
    /// gebaut werden.
    #[test]
    fn kein_provider_baut_seine_cleanup_zusammenfassung_von_hand() {
        // Zusammengesetzt, damit dieser Test seinen eigenen Quelltext nicht trifft.
        let konstruktor = concat!("io::Error", "::other(");
        let konstruktor_mit_kind = concat!("io::Error", "::new(");
        for (name, quelle) in [
            ("ftps.rs", include_str!("ftps.rs")),
            ("smb.rs", include_str!("smb.rs")),
            ("webdav.rs", include_str!("webdav.rs")),
            ("sftp.rs", include_str!("sftp.rs")),
            ("real_mode.rs", include_str!("real_mode.rs")),
        ] {
            let zeilen: Vec<&str> = quelle.lines().collect();
            for (index, zeile) in zeilen.iter().enumerate() {
                if !zeile.contains("Cleanup") && !zeile.contains("cleanup_error") {
                    continue;
                }
                let start = index.saturating_sub(4);
                let umfeld = zeilen[start..=index].join("\n");
                // Nur das Umverpacken eines schon bestehenden Fehlers ist gemeint:
                // ein neuer Fehler mit eigenem Kind und festem Text ist in Ordnung.
                let verpackt_fehler = umfeld.contains("format!(")
                    && (umfeld.contains(konstruktor) || umfeld.contains(konstruktor_mit_kind));
                assert!(
                    !verpackt_fehler,
                    "{name}:{} baut eine Cleanup-Zusammenfassung ohne erhaltenen ErrorCode; \
                     combine_probe_outcome, fold_cleanup_errors oder coded_error benutzen:\n{umfeld}",
                    index + 1
                );
            }
        }
    }

    #[test]
    fn keine_probe_verwirft_ihren_cleanup_mehr() {
        // Zusammengesetzt, damit das Muster nicht im eigenen Quelltext steht.
        let muster = format!("result?;\n{}cleanup?;", " ".repeat(8));
        for (name, quelle) in [
            ("ftps.rs", include_str!("ftps.rs")),
            ("smb.rs", include_str!("smb.rs")),
            ("webdav.rs", include_str!("webdav.rs")),
        ] {
            assert!(
                !quelle.contains(&muster),
                "{name} verwirft den Cleanup-Fehler wieder; combine_probe_outcome benutzen"
            );
        }
    }
}
