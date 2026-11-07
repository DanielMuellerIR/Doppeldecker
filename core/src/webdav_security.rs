//! Schreibgeschützte DAV-Sicherheitsbeobachtungen ohne ACL- oder Owner-Abbildung.
//!
//! HTTP-Antwort, Ressourcenstatus und Property-Status bleiben getrennt. Auch ein
//! erfolgreicher, leerer DAV:acl-Wert belegt hier keine vollständige leere ACL.
//! Werte werden nur über ihren kanonischen Digest gebunden; Principal-URLs und
//! andere möglicherweise vertrauliche XML-Inhalte gehören nicht ins Manifest.

use std::io;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WebDavPropertyObservation {
    Missing,
    Unavailable { status: u16 },
    Observed { empty: bool, digest: [u8; 32] },
}

impl WebDavPropertyObservation {
    pub fn validate(&self) -> io::Result<()> {
        if let Self::Unavailable { status } = self {
            if !(100..=599).contains(status) || *status == 200 {
                return Err(invalid("Ungültiger DAV-Property-Status"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebDavSecurityObservation {
    /// Providergebundener Namensraum, keine URL oder numerische Identitätsdomäne.
    pub namespace: [u8; 32],
    pub query_status: u16,
    /// Ausschließlich ein ausdrücklich gelieferter DAV:response/DAV:status.
    pub resource_status: Option<u16>,
    pub acl: WebDavPropertyObservation,
    pub owner: WebDavPropertyObservation,
}

impl WebDavSecurityObservation {
    pub fn validate(&self) -> io::Result<()> {
        if self.namespace == [0; 32] {
            return Err(invalid(
                "DAV-Sicherheitsbeobachtung benötigt einen Namensraum",
            ));
        }
        if !matches!(self.query_status, 207 | 403 | 404 | 405 | 501) {
            return Err(invalid("Ungültiger HTTP-Status der DAV-Sicherheitsabfrage"));
        }
        self.acl.validate()?;
        self.owner.validate()?;
        let properties_missing = self.acl == WebDavPropertyObservation::Missing
            && self.owner == WebDavPropertyObservation::Missing;
        if self.query_status != 207 && (self.resource_status.is_some() || !properties_missing) {
            return Err(invalid(
                "HTTP-Abfragefehler darf keine DAV-Ressourcenwerte enthalten",
            ));
        }
        if let Some(status) = self.resource_status {
            if !(100..=599).contains(&status)
                || (!(200..=299).contains(&status) && !properties_missing)
            {
                return Err(invalid(
                    "DAV-Ressourcenstatus widerspricht den Property-Werten",
                ));
            }
        }
        Ok(())
    }

    pub fn acl_reason(&self) -> &'static str {
        self.reason(&self.acl, true)
    }

    pub fn owner_reason(&self) -> &'static str {
        self.reason(&self.owner, false)
    }

    fn reason(&self, property: &WebDavPropertyObservation, acl: bool) -> &'static str {
        match self.query_status {
            403 => return "HTTP-Zugriff auf die DAV-Sicherheitsabfrage wurde verweigert",
            404 => return "Die HTTP-DAV-Sicherheitsabfrage ist nicht verfügbar (404)",
            405 => {
                return "Die HTTP-Methode der DAV-Sicherheitsabfrage wird nicht unterstützt (405)"
            }
            501 => return "Die HTTP-DAV-Sicherheitsabfrage ist nicht implementiert (501)",
            _ => {}
        }
        if let Some(status) = self.resource_status {
            if !(200..=299).contains(&status) {
                return match status {
                    403 => "Zugriff auf die DAV-Ressource wurde verweigert (Ressourcenstatus 403)",
                    404 => "Die DAV-Ressource wurde als nicht vorhanden gemeldet (Ressourcenstatus 404)",
                    _ => "Die DAV-Ressource meldet einen nicht erfolgreichen Ressourcenstatus",
                };
            }
        }
        match property {
            WebDavPropertyObservation::Missing => "Die angefragte DAV-Sicherheitsproperty wurde nicht zurückgegeben",
            WebDavPropertyObservation::Unavailable { status: 403 } => "Die DAV-Sicherheitsproperty ist wegen fehlender Berechtigung nicht lesbar (Property-Status 403)",
            WebDavPropertyObservation::Unavailable { status: 404 } => "Die DAV-Sicherheitsproperty wird nicht bereitgestellt (Property-Status 404)",
            WebDavPropertyObservation::Unavailable { .. } => "Die DAV-Sicherheitsproperty liefert bei diesem Property-Status keinen modellierten Wert",
            WebDavPropertyObservation::Observed { .. } if acl => "Der DAV:acl-Wert wurde nur opak beobachtet; eine vollständige übertragbare ACL ist nicht modelliert",
            WebDavPropertyObservation::Observed { .. } => "Der DAV:owner-Wert wurde nur opak beobachtet; eine Zuordnung zu numerischen UID/GID ist nicht modelliert",
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
