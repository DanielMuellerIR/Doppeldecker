//! Wirkungsspezifischer Vertrag für eine interaktiv angeforderte Ordneraddition.

use std::io;
use std::path::{Path, PathBuf};

use crate::{coded_error, ErrorCode, ProviderKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryCreateExecutionClass {
    Assured,
    BestEffort,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DirectoryCreateContract {
    pub execution_class: Option<DirectoryCreateExecutionClass>,
    pub limitations: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DirectoryCreateReport {
    pub path: PathBuf,
    pub execution_class: DirectoryCreateExecutionClass,
    pub limitations: Vec<String>,
}

/// Diese reine Beschreibung erteilt keine Fähigkeit. Die Ausführung prüft den
/// tatsächlichen Endpunkt, Elternbestand und Serverbedingungen erneut.
pub fn directory_create_contract(provider: ProviderKind) -> DirectoryCreateContract {
    match provider {
        ProviderKind::WebDav => DirectoryCreateContract {
            execution_class: Some(DirectoryCreateExecutionClass::BestEffort),
            limitations: vec![
                "Best Effort: Der Server erhält genau eine bedingte Ordneranlage. Seine Zusagen werden ohne künstliche Schreib- oder Löschprobe verwendet; vorhandene Namen werden vorher geprüft und das Ergebnis danach zurückgelesen.".into(),
                "Anlegeantwort und Rücklesen beweisen nicht in jedem Fall denselben neuen Ordner oder eine dauerhafte Speicherung. Bei unklarem Ausgang bleibt der mögliche neue Ordner zur Prüfung stehen; kein automatisches Wiederholen oder Entfernen.".into(),
                crate::webdav::NAMESPACE_BINDING_LIMITATION.into(),
            ],
            blockers: Vec::new(),
        },
        ProviderKind::Sftp => DirectoryCreateContract {
            execution_class: None,
            limitations: Vec::new(),
            blockers: vec!["SFTP v3 bietet kein an den Elternordner gebundenes MKDIR. Ein Pfadtausch könnte außerhalb des gewählten Bereichs anlegen; diese Wirkung bleibt gesperrt.".into()],
        },
        _ => DirectoryCreateContract {
            execution_class: Some(DirectoryCreateExecutionClass::Assured),
            limitations: Vec::new(),
            blockers: Vec::new(),
        },
    }
}

pub fn validate_directory_create_class(
    provider: ProviderKind,
    expected: DirectoryCreateExecutionClass,
) -> io::Result<()> {
    let contract = directory_create_contract(provider);
    match contract.execution_class {
        Some(class) if class == expected => Ok(()),
        Some(_) => Err(coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift, "Die angezeigte Ausführungsklasse der Ordneranlage stimmt nicht überein; erneut vorbereiten")),
        None => Err(coded_error(io::ErrorKind::Unsupported, ErrorCode::Unsupported, contract.blockers.join("; "))),
    }
}

pub fn validate_directory_create_name(name: &str) -> io::Result<String> {
    let name = crate::validate_file_name(name)?;
    if name.len() > 4096 || name.chars().any(char::is_control) {
        return Err(invalid("Ordnername ist zu lang oder enthält Steuerzeichen"));
    }
    Ok(name)
}

/// Vor Profilauflösung, Verbindung oder Probe prüfen. Insbesondere darf ein
/// relativer Elternpfad nicht still am Profilroot landen.
pub fn validate_directory_create_request(parent: &Path, name: &str) -> io::Result<String> {
    let name = validate_directory_create_name(name)?;
    let text = parent
        .to_str()
        .ok_or_else(|| invalid("Ordner-Elternpfad ist nicht UTF-8"))?;
    if !text.starts_with('/')
        || text.contains("//")
        || text.len().saturating_add(name.len()).saturating_add(1) > 4096
        || text.chars().any(char::is_control)
    {
        return Err(invalid(
            "Ordneranlage benötigt einen sicheren absoluten Elternpfad und Namen",
        ));
    }
    let parts = text
        .trim_end_matches('/')
        .split('/')
        .skip(1)
        .collect::<Vec<_>>();
    if parts.len() >= 64
        || parts
            .iter()
            .any(|part| part.is_empty() || matches!(*part, "." | ".."))
    {
        return Err(invalid(
            "Ordner-Elternpfad ist nicht normalisiert oder überschreitet 64 Komponenten",
        ));
    }
    Ok(name)
}

fn invalid(message: &str) -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ungueltige_ordnerabsichten_und_nicht_angezeigte_klassen_bleiben_vor_providerzugriff_gesperrt(
    ) {
        for parent in [
            "relativ",
            "/pane//unter",
            "/pane/../fremd",
            "/pane/./unter",
            "/pane/\n",
        ] {
            assert_eq!(
                crate::error_code(
                    &validate_directory_create_request(Path::new(parent), "Neu").unwrap_err()
                ),
                ErrorCode::InvalidInput
            );
        }
        let too_deep = format!("/{}", vec!["unter"; 64].join("/"));
        assert!(validate_directory_create_request(Path::new(&too_deep), "Neu").is_err());
        assert!(validate_directory_create_class(
            ProviderKind::Sftp,
            DirectoryCreateExecutionClass::BestEffort
        )
        .is_err());
        assert_eq!(
            crate::error_code(
                &validate_directory_create_class(
                    ProviderKind::WebDav,
                    DirectoryCreateExecutionClass::Assured
                )
                .unwrap_err()
            ),
            ErrorCode::PlanDrift
        );
        assert_eq!(
            validate_directory_create_request(Path::new("/pane"), " Grüße %2F ").unwrap(),
            "Grüße %2F"
        );
    }
}
