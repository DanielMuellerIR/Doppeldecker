//! Kleine, plattformneutrale Helfer für vom Nutzer eingegebene Pfade.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

/// Ersetzt nur die Shell-üblichen Formen `~` und `~/…` durch `$HOME`.
///
/// Namen wie `~archiv` bleiben absichtlich unverändert: Doppeldecker löst keine
/// fremden Benutzerkonten auf und darf daraus insbesondere nicht `$HOME/archiv`
/// machen. Nicht als Unicode darstellbare Pfade bleiben durch `OsStr` vollständig
/// erhalten.
pub fn expand_home_path(path: impl AsRef<Path>) -> PathBuf {
    let home = std::env::var_os("HOME");
    expand_home_path_with(path.as_ref(), home.as_deref())
}

/// Prüft einen internen Plan-/Manifestpfad als echte Folge normaler relativer
/// Komponenten. Leere, absolute, Eltern- und Plattformpräfix-Pfade sind verboten.
/// Die Komponentenprüfung ist keine byteweise Kanonisierung: Rust normalisiert
/// interne `./`-Abschnitte und mehrfache Trenner, erhält aber jedes `..`.
pub(crate) fn validate_relative_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Unsicherer relativer Pfad: {}", path.display()),
        ));
    }
    Ok(())
}

/// Hängt erst nach vollständiger Komponentenprüfung an eine gebundene Wurzel an.
pub(crate) fn checked_join(root: &Path, relative: impl AsRef<Path>) -> io::Result<PathBuf> {
    let relative = relative.as_ref();
    validate_relative_path(relative)?;
    Ok(root.join(relative))
}

/// Validiert einen relativen Pfad und wandelt ihn nur dann verlustfrei in den
/// Stringvertrag der JSON-Manifeste um.
pub(crate) fn relative_path_string(path: &Path) -> io::Result<String> {
    validate_relative_path(path)?;
    path.to_str().map(str::to_owned).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Relativer Pfad ist kein gültiges UTF-8: {}", path.display()),
        )
    })
}

fn expand_home_path_with(path: &Path, home: Option<&OsStr>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    if path == Path::new("~") {
        return PathBuf::from(home);
    }
    match path.strip_prefix(Path::new("~")) {
        Ok(rest) if !rest.as_os_str().is_empty() => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// Weist einen Pfad ab, der eine `..`-Komponente enthält.
///
/// Die Sandbox-Schranken lösen einen Zielpfad über den nächsten **vorhandenen**
/// Vorfahren auf und verwerfen den Rest. Ein `..` nach einer noch nicht
/// existierenden Komponente käme dadurch ungeprüft durch: Bei
/// `<sandbox>/fehlt/../../draussen.txt` ist der nächste vorhandene Vorfahre die
/// Sandbox selbst, und sobald `fehlt` später entsteht, zeigt derselbe Pfad nach
/// draußen. Ein rechtmäßiges Ziel braucht nie ein `..`; die Prüfung sitzt deshalb
/// vor der Auflösung und gilt für beide Schranken gleichermaßen.
pub(crate) fn reject_parent_traversal(
    target: &Path,
    describe: impl FnOnce() -> String,
) -> io::Result<()> {
    if target
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(crate::error::coded_error(
            io::ErrorKind::PermissionDenied,
            crate::error::ErrorCode::SandboxViolation,
            describe(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expandiert_nur_eigenes_home_kuerzel() {
        let home = OsStr::new("/home/tester");
        assert_eq!(
            expand_home_path_with(Path::new("~"), Some(home)),
            PathBuf::from("/home/tester")
        );
        assert_eq!(
            expand_home_path_with(Path::new("~/Daten/Plan"), Some(home)),
            PathBuf::from("/home/tester/Daten/Plan")
        );
        assert_eq!(
            expand_home_path_with(Path::new("~archiv"), Some(home)),
            PathBuf::from("~archiv")
        );
        assert_eq!(
            expand_home_path_with(Path::new("relativ/Datei"), Some(home)),
            PathBuf::from("relativ/Datei")
        );
    }

    #[test]
    fn fehlendes_home_laesst_eingabe_unveraendert() {
        assert_eq!(
            expand_home_path_with(Path::new("~/Daten"), None),
            PathBuf::from("~/Daten")
        );
    }

    #[test]
    fn relativer_join_blockt_traversal_und_punktpfade() {
        let root = Path::new("/sicher");
        assert_eq!(
            checked_join(root, Path::new("unter/datei")).unwrap(),
            PathBuf::from("/sicher/unter/datei")
        );
        for unsafe_path in ["", "/absolut", "../raus", "./mehrdeutig", "a/../../raus"] {
            assert!(checked_join(root, Path::new(unsafe_path)).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn manifest_string_ist_nie_verlustbehaftet() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"ungueltig-\xff"));
        assert!(relative_path_string(path).is_err());
    }
}
