//! Verlustfreie Konfliktanzeige aus Backendbelegen, ohne ausführbare Frontendpfade.

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct ConflictEntryView {
    pub endpoint: String,
    pub exists: bool,
    pub kind: Option<String>,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

impl ConflictEntryView {
    pub fn absent(endpoint: String) -> Self {
        Self {
            endpoint,
            exists: false,
            kind: None,
            size: None,
            mtime: None,
        }
    }
}

/// UTF-8 bleibt lesbar; Steuerzeichen und fremde Bytes werden eindeutig escaped.
/// Anders als `to_string_lossy` können zwei verschiedene Namen nie zusammenfallen.
pub fn display_relative_name(parts: &[Vec<u8>]) -> String {
    parts
        .iter()
        .map(|part| match std::str::from_utf8(part) {
            Ok(name) => name
                .chars()
                .flat_map(char::escape_debug)
                .collect::<String>(),
            Err(_) => part.iter().map(|byte| format!("\\x{byte:02x}")).collect(),
        })
        .collect::<Vec<_>>()
        .join("/")
}
