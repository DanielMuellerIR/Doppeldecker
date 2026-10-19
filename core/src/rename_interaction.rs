//! Anzeige- und Klassenvertrag der gewöhnlichen Umbenennung vor der Namenseingabe.

use crate::{coded_error, ErrorCode, ProviderKind, RenameExecutionClass};
use std::io;
use std::path::Path;

pub fn validate_ordinary_rename_request(
    provider: ProviderKind,
    root: &Path,
    source: &Path,
    new_name: &str,
    expected: Option<RenameExecutionClass>,
) -> io::Result<String> {
    let name = crate::validate_interactive_rename_request(root, source, new_name)?;
    if source.file_name().and_then(|v| v.to_str()) == Some(name.as_str()) {
        return Err(coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            "Der neue Name ist mit dem bisherigen identisch",
        ));
    }
    validate_ordinary_rename_class(provider, expected)?;
    // Derselbe Providerparser muss vor Profilauflösung und mutierenden Proben
    // bereits Root, Quelle und vollständiges Ziel akzeptieren.
    let destination = source.with_file_name(&name);
    // Die Parser melden sonst ungültige Serverdaten; hier stammen sie vom Nutzer.
    let invalid_input = |error: io::Error| {
        coded_error(
            io::ErrorKind::InvalidInput,
            ErrorCode::InvalidInput,
            error.to_string(),
        )
    };
    match provider {
        ProviderKind::Ftps => {
            for path in [root, source, destination.as_path()] {
                let _ = crate::ftps::map_virtual_path("/", path).map_err(invalid_input)?;
            }
        }
        ProviderKind::Smb => {
            for path in [root, source, destination.as_path()] {
                let _ = crate::smb::map_virtual_path("/", path).map_err(invalid_input)?;
            }
        }
        ProviderKind::WebDav => crate::webdav::validate_ordinary_rename_path(source, &name)?,
        _ => {}
    }
    Ok(name)
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenameInteractionContract {
    Existing,
    Confirmed {
        execution_class: RenameExecutionClass,
        limitations: Vec<String>,
    },
    Blocked {
        blockers: Vec<String>,
    },
}

/// Beschreibt die eigene WebDAV-Wirkung, ohne eine Sitzung oder Sync-Fähigkeit
/// freizugeben. Andere Provider behalten ihre bisherigen frischen Rename-Proben.
pub fn ordinary_rename_contract(provider: ProviderKind) -> RenameInteractionContract {
    match provider {
        ProviderKind::WebDav => RenameInteractionContract::Confirmed {
            execution_class: RenameExecutionClass::BestEffort,
            limitations: vec![
                "Genau ein bedingtes MOVE verwendet Overwrite: F für den Zielnamen und If-Match für die Quelle. Die Elternkette wird frisch geprüft und das Ergebnis unabhängig zurückgelesen, ohne künstliche Schreib- oder Löschprobe.".into(),
                "Die Serverbedingungen sind nicht verhaltensgeprüft: Missachtet der Server Overwrite: F, könnte ein gerade entstandenes Ziel unwiederbringlich ersetzt werden. Dauerhafte Speicherung ist nicht bewiesen. Bei unklarem Ausgang beide Namen prüfen; kein automatisches Wiederholen oder Entfernen.".into(),
                crate::SOURCE_PATH_BINDING_LIMITATION.into(),
                crate::webdav::NAMESPACE_BINDING_LIMITATION.into(),
            ],
        },
        ProviderKind::Sftp => RenameInteractionContract::Blocked {
            blockers: vec!["SFTP v3 bindet Rename nicht an die Root-/Elternordner. Ein Pfadtausch könnte außerhalb des gewählten Bereichs verändern; diese Wirkung bleibt gesperrt.".into()],
        },
        _ => RenameInteractionContract::Existing,
    }
}

pub fn validate_ordinary_rename_class(
    provider: ProviderKind,
    expected: Option<RenameExecutionClass>,
) -> io::Result<()> {
    match ordinary_rename_contract(provider) {
        RenameInteractionContract::Confirmed { execution_class, .. }
            if expected == Some(execution_class) => Ok(()),
        RenameInteractionContract::Existing if expected.is_none() => Ok(()),
        RenameInteractionContract::Blocked { blockers } => Err(coded_error(
            io::ErrorKind::Unsupported,
            ErrorCode::Unsupported,
            blockers.join("; "),
        )),
        _ => Err(coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::PlanDrift,
            "Die angezeigte Ausführungsklasse der Umbenennung stimmt nicht überein; erneut vorbereiten",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providerungueltige_pfade_scheitern_vor_profil_und_proben() {
        let mut accepted = Vec::new();
        for (provider, root, source) in [
            (ProviderKind::Ftps, "/pa\nne", "/pa\nne/alt.txt".to_owned()),
            (ProviderKind::Ftps, "/pane", "/pane/al\nt.txt".to_owned()),
            (ProviderKind::Smb, "/pa\\ne", "/pa\\ne/alt.txt".to_owned()),
            (ProviderKind::Smb, "/pane", "/pane/alt\\txt".to_owned()),
            (
                ProviderKind::Smb,
                "/pane",
                format!("/pane/{}", "a".repeat(256)),
            ),
            (
                ProviderKind::Smb,
                "/",
                format!("/{}", vec!["a".repeat(255); 129].join("/")),
            ),
        ] {
            match validate_ordinary_rename_request(
                provider,
                Path::new(root),
                Path::new(&source),
                "neu.txt",
                None,
            ) {
                Err(error) => assert_eq!(crate::error_code(&error), ErrorCode::InvalidInput),
                Ok(_) => accepted.push(format!("{provider:?}: {source:?}")),
            }
        }
        assert!(
            accepted.is_empty(),
            "Providerungültige Pfade passierten die reine Vorprüfung: {accepted:?}"
        );
    }

    #[test]
    fn providerungueltige_namen_scheitern_vor_profil_und_proben() {
        let mut accepted = Vec::new();
        for (provider, name) in [
            (ProviderKind::Ftps, "ungueltig\nname".to_owned()),
            (ProviderKind::Ftps, "ungueltig\rname".to_owned()),
            (ProviderKind::Smb, "ungueltig\nname".to_owned()),
            (ProviderKind::Smb, "ungueltig\\name".to_owned()),
            (ProviderKind::Smb, "a".repeat(256)),
        ] {
            match validate_ordinary_rename_request(
                provider,
                Path::new("/pane"),
                Path::new("/pane/alt.txt"),
                &name,
                None,
            ) {
                Err(error) => assert_eq!(crate::error_code(&error), ErrorCode::InvalidInput),
                Ok(_) => accepted.push(format!("{provider:?}: {name:?}")),
            }
        }
        assert!(
            accepted.is_empty(),
            "Providerungültige Namen passierten die reine Vorprüfung: {accepted:?}"
        );
        for (provider, name) in [
            (ProviderKind::Ftps, "Grüße.txt"),
            (ProviderKind::Ftps, "literal\\name"),
            (ProviderKind::Smb, "Grüße.txt"),
            (ProviderKind::Local, "lokal\nzulässig"),
        ] {
            assert_eq!(
                validate_ordinary_rename_request(
                    provider,
                    Path::new("/pane"),
                    Path::new("/pane/alt.txt"),
                    name,
                    None
                )
                .unwrap(),
                name
            );
        }
    }
}
