//! Kleine, providerunabhängige Ausschlussmuster für Sync-Pläne.
//!
//! Die Sprache bleibt absichtlich überschaubar: `*` und `?` gelten innerhalb einer
//! Pfadkomponente, `**` als ganze Komponente über beliebig viele Verzeichnisebenen.
//! Ein Muster ohne `/` wird gegen jeden einzelnen Datei- oder Ordnernamen geprüft.

use std::io;

const MAX_PATTERNS: usize = 128;
const MAX_PATTERN_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub(super) struct ExcludeMatcher {
    patterns: Vec<ExcludePattern>,
}

#[derive(Clone, Debug)]
enum ExcludePattern {
    Basename(String),
    Path(Vec<PatternComponent>),
}

#[derive(Clone, Debug)]
enum PatternComponent {
    Recursive,
    Wildcard(String),
}

impl ExcludeMatcher {
    pub(super) fn new(patterns: &[String]) -> io::Result<Self> {
        if patterns.len() > MAX_PATTERNS {
            return Err(invalid_pattern(
                "höchstens 128 Ausschlussmuster sind erlaubt",
            ));
        }

        let patterns = patterns
            .iter()
            .enumerate()
            .map(|(index, pattern)| parse_pattern(index, pattern))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { patterns })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// `relative_path` ist immer ein sicher dekodierter, relativer Planpfad mit `/`.
    /// Nicht sicher dekodierbare Namen ruft der Planer bewusst nie hiermit auf.
    pub(super) fn matches(&self, relative_path: &str) -> bool {
        let components = relative_path.split('/').collect::<Vec<_>>();
        self.patterns.iter().any(|pattern| match pattern {
            ExcludePattern::Basename(pattern) => components
                .last()
                .is_some_and(|name| wildcard_component_matches(pattern, name)),
            ExcludePattern::Path(pattern) => path_components_match(pattern, &components),
        })
    }
}

fn parse_pattern(index: usize, pattern: &str) -> io::Result<ExcludePattern> {
    if pattern.is_empty() {
        return Err(indexed_invalid_pattern(index, "das Muster ist leer"));
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(indexed_invalid_pattern(
            index,
            "das Muster ist länger als 256 Bytes",
        ));
    }
    if pattern.starts_with('/') {
        return Err(indexed_invalid_pattern(
            index,
            "absolute Pfade sind nicht erlaubt",
        ));
    }
    if pattern.contains('\0') || pattern.contains('\\') {
        return Err(indexed_invalid_pattern(
            index,
            "NUL und Rückwärtsschrägstriche sind nicht erlaubt",
        ));
    }

    let raw_components = pattern.split('/').collect::<Vec<_>>();
    for component in &raw_components {
        if component.is_empty() || matches!(*component, "." | "..") {
            return Err(indexed_invalid_pattern(
                index,
                "leere, .- und ..-Pfadkomponenten sind nicht erlaubt",
            ));
        }
        validate_stars(index, component)?;
    }

    if raw_components.len() == 1 {
        return Ok(ExcludePattern::Basename(pattern.to_owned()));
    }

    Ok(ExcludePattern::Path(
        raw_components
            .into_iter()
            .map(|component| {
                if component == "**" {
                    PatternComponent::Recursive
                } else {
                    PatternComponent::Wildcard(component.to_owned())
                }
            })
            .collect(),
    ))
}

fn validate_stars(index: usize, component: &str) -> io::Result<()> {
    let mut run = 0;
    for character in component.chars() {
        if character == '*' {
            run += 1;
            if run > 2 {
                return Err(indexed_invalid_pattern(
                    index,
                    "mehr als zwei aufeinanderfolgende * sind nicht erlaubt",
                ));
            }
        } else {
            run = 0;
        }
    }
    if component.contains("**") && component != "**" {
        return Err(indexed_invalid_pattern(
            index,
            "** ist nur als vollständige Pfadkomponente erlaubt",
        ));
    }
    Ok(())
}

fn path_components_match(pattern: &[PatternComponent], value: &[&str]) -> bool {
    let mut matches = vec![vec![false; value.len() + 1]; pattern.len() + 1];
    matches[0][0] = true;

    for pattern_index in 0..pattern.len() {
        for value_index in 0..=value.len() {
            if !matches[pattern_index][value_index] {
                continue;
            }
            match &pattern[pattern_index] {
                PatternComponent::Recursive => {
                    // `**` darf keine oder beliebig viele Komponenten aufnehmen.
                    matches[pattern_index + 1][value_index] = true;
                    if value_index < value.len() {
                        matches[pattern_index][value_index + 1] = true;
                    }
                }
                PatternComponent::Wildcard(component) if value_index < value.len() => {
                    if wildcard_component_matches(component, value[value_index]) {
                        matches[pattern_index + 1][value_index + 1] = true;
                    }
                }
                PatternComponent::Wildcard(_) => {}
            }
        }
    }
    matches[pattern.len()][value.len()]
}

fn wildcard_component_matches(pattern: &str, value: &str) -> bool {
    let value = value.chars().collect::<Vec<_>>();
    let mut current = vec![false; value.len() + 1];
    current[0] = true;

    for character in pattern.chars() {
        let mut next = vec![false; value.len() + 1];
        if character == '*' {
            next[0] = current[0];
            for index in 1..=value.len() {
                next[index] = current[index] || next[index - 1];
            }
        } else {
            for index in 1..=value.len() {
                if current[index - 1] && (character == '?' || character == value[index - 1]) {
                    next[index] = true;
                }
            }
        }
        current = next;
    }
    current[value.len()]
}

fn indexed_invalid_pattern(index: usize, reason: &str) -> io::Error {
    invalid_pattern(&format!(
        "Ausschlussmuster {} ist ungültig: {reason}",
        index + 1
    ))
}

fn invalid_pattern(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(patterns: &[&str]) -> ExcludeMatcher {
        ExcludeMatcher::new(
            &patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn basename_muster_gelten_auf_jeder_ebene() {
        let matcher = matcher(&[".DS_Store", "cache"]);
        assert!(matcher.matches(".DS_Store"));
        assert!(matcher.matches("bilder/.DS_Store"));
        assert!(matcher.matches("a/cache"));
        assert!(!matcher.matches("cache-datei"));
    }

    #[test]
    fn pfadmuster_begrenzen_stern_auf_eine_komponente() {
        let matcher = matcher(&["data/file?.txt", "build/*"]);
        assert!(matcher.matches("data/file1.txt"));
        assert!(!matcher.matches("data/file10.txt"));
        assert!(matcher.matches("build/eins"));
        assert!(!matcher.matches("build/eins/zwei"));
    }

    #[test]
    fn doppelstern_deckt_null_bis_viele_ebenen_ab() {
        let matcher = matcher(&["**/*.tmp", "build/**"]);
        assert!(matcher.matches("datei.tmp"));
        assert!(matcher.matches("a/b/datei.tmp"));
        assert!(matcher.matches("build"));
        assert!(matcher.matches("build/a/b"));
        assert!(!matcher.matches("bilder/datei.jpg"));
    }

    #[test]
    fn unklare_oder_unbegrenzte_muster_werden_abgelehnt() {
        for pattern in ["", "/tmp", "a//b", "a/../b", "a\\b", "ab**cd", "***"] {
            assert_eq!(
                ExcludeMatcher::new(&[pattern.to_owned()])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
