//! Finder-Tags und alte Farbmarkierungen; andere Metadaten bleiben unberührt.
use serde::de::{Error as _, SeqAccess, Visitor};
use std::{
    collections::HashSet,
    fmt,
    io::{self, Cursor},
};

const MAX_TAGS: usize = 128;
const MAX_NAME_BYTES: usize = 1024;
pub(crate) const MAX_TAG_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct FinderTag {
    pub name: String,
    pub color: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinderTagState {
    pub tags: Vec<FinderTag>,
    pub legacy_color: u8,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinderTagReadRow {
    pub name: String,
    pub state: Option<FinderTagState>,
    pub code: Option<crate::ErrorCode>,
    pub message: String,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinderTagEditRow {
    pub name: String,
    pub changed: bool,
    pub code: Option<crate::ErrorCode>,
    pub message: String,
}

fn validate_selection(
    root: &std::path::Path,
    names: &[String],
    sandbox: Option<&std::path::Path>,
) -> io::Result<()> {
    super::validate_local_selection(root, names, sandbox)
}
pub fn read_local_finder_tags(
    root: &std::path::Path,
    names: &[String],
    sandbox: Option<&std::path::Path>,
    cancel: &crate::Cancel,
) -> io::Result<Vec<FinderTagReadRow>> {
    validate_selection(root, names, sandbox)?;
    if names.len() > 40 {
        return Err(invalid("Tagansicht benötigt höchstens 40 Objekte je Seite"));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cancel;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Finder-Tags werden nur auf macOS bearbeitet",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let held = crate::local_fs::RootHandle::open_nofollow(root)?;
        Ok(names
            .iter()
            .map(|name| {
                let result = (|| {
                    super::check_cancel(cancel)?;
                    let path = root.join(name);
                    let entry = crate::local_fs::stat_beneath(&held, &path)?;
                    crate::local_fs::read_interactive_finder_tags_beneath(&held, &path, &entry)
                })();
                match result {
                    Ok(state) => FinderTagReadRow {
                        name: name.clone(),
                        state: Some(state),
                        code: None,
                        message: String::new(),
                    },
                    Err(error) => FinderTagReadRow {
                        name: name.clone(),
                        state: None,
                        code: Some(crate::error_code(&error)),
                        message: error.to_string(),
                    },
                }
            })
            .collect())
    }
}
pub fn change_local_finder_tags(
    root: &std::path::Path,
    names: &[String],
    edit: &FinderTagEdit,
    sandbox: Option<&std::path::Path>,
    cancel: &crate::Cancel,
) -> io::Result<Vec<FinderTagEditRow>> {
    edit.validate()?;
    validate_selection(root, names, sandbox)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cancel;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Finder-Tags werden nur auf macOS bearbeitet",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let held = crate::local_fs::RootHandle::open_nofollow(root)?;
        Ok(names
            .iter()
            .map(|name| {
                let result = (|| {
                    super::check_cancel(cancel)?;
                    let path = root.join(name);
                    let entry = crate::local_fs::stat_beneath(&held, &path)?;
                    crate::local_fs::edit_interactive_finder_tags_beneath(
                        &held, &path, &entry, edit, cancel,
                    )
                })();
                match result {
                    Ok((_, modified)) => FinderTagEditRow {
                        name: name.clone(),
                        changed: modified,
                        code: None,
                        message: if modified {
                            "Finder-Tags und Farbmarkierung bestätigt"
                        } else {
                            "Tags bereits passend; unverändert"
                        }
                        .into(),
                    },
                    Err(error) => FinderTagEditRow {
                        name: name.clone(),
                        changed: false,
                        code: Some(crate::error_code(&error)),
                        message: error.to_string(),
                    },
                }
            })
            .collect())
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum FinderTagEdit {
    Upsert {
        tag: FinderTag,
        #[serde(default)]
        previous_name: Option<String>,
    },
    Remove {
        name: String,
    },
    Clear,
    ClearLegacy,
}

fn invalid(message: &str) -> io::Error {
    crate::coded_error(
        io::ErrorKind::InvalidInput,
        crate::ErrorCode::InvalidInput,
        message,
    )
}
fn invalid_data(message: &str) -> io::Error {
    crate::coded_error(
        io::ErrorKind::InvalidData,
        crate::ErrorCode::InvalidData,
        message,
    )
}
fn validate_name(name: &str) -> io::Result<()> {
    if name.trim().is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(invalid(
            "Ein Tag benötigt einen Namen ohne Steuerzeichen mit höchstens 1024 UTF-8-Bytes",
        ));
    }
    Ok(())
}
fn validate_tag(tag: &FinderTag) -> io::Result<()> {
    validate_name(&tag.name)?;
    if tag.color > 7 {
        return Err(invalid("Unbekannte Finder-Tagfarbe"));
    }
    Ok(())
}
impl FinderTagEdit {
    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::Upsert { tag, previous_name } => {
                validate_tag(tag)?;
                if let Some(name) = previous_name {
                    validate_name(name)?;
                }
            }
            Self::Remove { name } => validate_name(name)?,
            Self::Clear | Self::ClearLegacy => {}
        }
        Ok(())
    }
    pub fn apply(&self, before: &FinderTagState) -> io::Result<FinderTagState> {
        self.validate()?;
        if before.tags.len() > MAX_TAGS || before.legacy_color > 7 {
            return Err(invalid_data("Unzulässiger Finder-Tagbestand"));
        }
        let mut names = HashSet::new();
        for tag in &before.tags {
            validate_tag(tag).map_err(|_| invalid_data("Unzulässiger Finder-Tagbestand"))?;
            if !names.insert(&tag.name) {
                return Err(invalid_data("Doppelte Finder-Tagnamen"));
            }
        }
        let mut after = before.clone();
        match self {
            Self::Upsert { tag, previous_name } => {
                let old = previous_name.as_deref().unwrap_or(&tag.name);
                let index = after.tags.iter().position(|existing| existing.name == old);
                if previous_name.is_some() && index.is_none() {
                    return Ok(after);
                }
                let index = index.unwrap_or(after.tags.len());
                let index = after
                    .tags
                    .iter()
                    .take(index)
                    .filter(|existing| existing.name != old && existing.name != tag.name)
                    .count();
                after
                    .tags
                    .retain(|existing| existing.name != old && existing.name != tag.name);
                after.tags.insert(index.min(after.tags.len()), tag.clone());
            }
            Self::Remove { name } => after.tags.retain(|tag| &tag.name != name),
            Self::Clear => {
                after.tags.clear();
                after.legacy_color = 0;
            }
            Self::ClearLegacy => after.legacy_color = 0,
        }
        if after.tags.len() > MAX_TAGS {
            return Err(invalid("Höchstens 128 Tags je Objekt"));
        }
        if after.tags != before.tags {
            encode_tags(&after.tags)?;
        }
        Ok(after)
    }
}

impl FinderTagState {
    pub fn from_raw(tags: Option<&[u8]>, finder_info: Option<&[u8]>) -> io::Result<Self> {
        decode_state(tags, finder_info)
    }
}

fn parse_tag(value: &str) -> io::Result<FinderTag> {
    let (name, color) = match value.rsplit_once('\n') {
        Some((name, value)) if value.len() == 1 && (b'0'..=b'7').contains(&value.as_bytes()[0]) => {
            (name, value.as_bytes()[0] - b'0')
        }
        Some(_) => {
            return Err(invalid_data(
                "Finder-Tags enthalten ein unbekanntes Farbformat; unverändert",
            ))
        }
        None => (value, 0),
    };
    let tag = FinderTag {
        name: name.to_owned(),
        color,
    };
    validate_tag(&tag)
        .map_err(|_| invalid_data("Finder-Tags enthalten einen ungültigen Namen; unverändert"))?;
    Ok(tag)
}

struct TagList(Vec<FinderTag>);
impl<'de> serde::Deserialize<'de> for TagList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FlatTags;
        impl<'de> Visitor<'de> for FlatTags {
            type Value = TagList;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("eine flache Liste mit höchstens 128 Finder-Tags")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                if sequence.size_hint().is_some_and(|size| size > MAX_TAGS) {
                    return Err(A::Error::custom("Zu viele Finder-Tags"));
                }
                let mut tags = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX_TAGS));
                let mut names = HashSet::new();
                while let Some(value) = sequence.next_element::<String>()? {
                    if tags.len() == MAX_TAGS {
                        return Err(A::Error::custom("Zu viele Finder-Tags"));
                    }
                    let tag = parse_tag(&value).map_err(A::Error::custom)?;
                    if !names.insert(tag.name.clone()) {
                        return Err(A::Error::custom("Doppelte Finder-Tagnamen"));
                    }
                    tags.push(tag);
                }
                Ok(TagList(tags))
            }
        }
        deserializer.deserialize_seq(FlatTags)
    }
}

pub(crate) fn decode_state(
    tags: Option<&[u8]>,
    finder_info: Option<&[u8]>,
) -> io::Result<FinderTagState> {
    let tags = match tags {
        None => Vec::new(),
        Some(value) if value.len() <= MAX_TAG_BYTES => {
            plist::from_reader::<_, TagList>(Cursor::new(value))
                .map_err(|_| {
                    invalid_data(
                        "Finder-Tags sind ungültig oder überschreiten ihre Grenzen; unverändert",
                    )
                })?
                .0
        }
        Some(_) => {
            return Err(invalid_data(
                "Finder-Tags überschreiten 64 KiB; unverändert",
            ))
        }
    };
    let legacy_color = match finder_info {
        None => 0,
        Some(value) if value.len() == 32 => {
            ((u16::from_be_bytes([value[8], value[9]]) & 0x000e) >> 1) as u8
        }
        Some(_) => {
            return Err(invalid_data(
                "FinderInfo besitzt eine unbekannte Länge; unverändert",
            ))
        }
    };
    Ok(FinderTagState { tags, legacy_color })
}

pub(crate) fn encode_tags(tags: &[FinderTag]) -> io::Result<Vec<u8>> {
    if tags.len() > MAX_TAGS {
        return Err(invalid("Höchstens 128 Tags je Objekt"));
    }
    let values = tags
        .iter()
        .map(|tag| {
            validate_tag(tag)?;
            Ok(format!("{}\n{}", tag.name, tag.color))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut result = Vec::new();
    plist::to_writer_binary(&mut result, &values).map_err(io::Error::other)?;
    if result.len() > MAX_TAG_BYTES {
        return Err(invalid("Finder-Tags überschreiten 64 KiB"));
    }
    Ok(result)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn clear_legacy_color(value: &[u8]) -> io::Result<Vec<u8>> {
    if value.len() != 32 {
        return Err(invalid_data(
            "FinderInfo besitzt eine unbekannte Länge; unverändert",
        ));
    }
    let mut result = value.to_vec();
    let flags = u16::from_be_bytes([result[8], result[9]]) & !0x000e;
    result[8..10].copy_from_slice(&flags.to_be_bytes());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tag(name: &str, color: u8) -> FinderTag {
        FinderTag {
            name: name.into(),
            color,
        }
    }
    #[test]
    fn binary_and_xml_keep_unicode_names_colors_and_separate_legacy_label() {
        let values = vec![tag("Projekt Ä", 4), tag("Archiv", 0)];
        let mut finder = [0_u8; 32];
        finder[8] = 0x40;
        finder[9] = 12;
        finder[16..20].copy_from_slice(b"TEST");
        let state = decode_state(Some(&encode_tags(&values).unwrap()), Some(&finder)).unwrap();
        assert_eq!(state.tags, values);
        assert_eq!(state.legacy_color, 6);
        assert_eq!(
            decode_state(None, Some(&finder)).unwrap().tags,
            Vec::<FinderTag>::new()
        );
        let mut xml = Vec::new();
        plist::to_writer_xml(&mut xml, &vec!["Projekt Ä\n4", "Archiv\n0"]).unwrap();
        assert_eq!(decode_state(Some(&xml), None).unwrap().tags, values);
        let cleared = clear_legacy_color(&finder).unwrap();
        assert_eq!(cleared[8], 0x40);
        assert_eq!(cleared[9], 0);
        assert_eq!(&cleared[16..20], b"TEST");
        assert_eq!(
            decode_state(Some(&xml), Some(&cleared))
                .unwrap()
                .legacy_color,
            0
        );
    }
    #[test]
    fn malformed_nested_duplicate_and_oversized_tags_are_rejected_without_reformatting() {
        for value in [
            plist::Value::Array(vec![plist::Value::Array(vec![])]),
            plist::Value::Array(vec![plist::Value::Integer(1.into())]),
            plist::Value::Array(vec!["a\n8".into()]),
            plist::Value::Array(vec!["line\nname\n4".into()]),
            plist::Value::Array(vec!["a\n4".into(), "a\n6".into()]),
            plist::Value::Array((0..129).map(|i| format!("Tag{i}\n0").into()).collect()),
        ] {
            let mut bytes = Vec::new();
            value.to_writer_binary(&mut bytes).unwrap();
            assert!(decode_state(Some(&bytes), None).is_err());
        }
        assert!(decode_state(Some(&vec![0; MAX_TAG_BYTES + 1]), None).is_err());
        assert!(decode_state(Some(b"broken"), None).is_err());
        assert!(decode_state(None, Some(&[0; 31])).is_err());
    }
    #[test]
    fn edits_preserve_other_tags_and_legacy_until_explicit_removal() {
        let state = FinderTagState {
            tags: vec![tag("A", 4), tag("B", 6)],
            legacy_color: 6,
        };
        let renamed = FinderTagEdit::Upsert {
            tag: tag("B", 2),
            previous_name: Some("A".into()),
        }
        .apply(&state)
        .unwrap();
        assert_eq!(renamed.tags, vec![tag("B", 2)]);
        assert_eq!(renamed.legacy_color, 6);
        let absent = FinderTagEdit::Upsert {
            tag: tag("C", 2),
            previous_name: Some("absent".into()),
        }
        .apply(&state)
        .unwrap();
        assert_eq!(absent, state);
        let removed = FinderTagEdit::Remove { name: "A".into() }
            .apply(&state)
            .unwrap();
        assert_eq!(removed.tags, vec![tag("B", 6)]);
        assert_eq!(removed.legacy_color, 6);
        assert_eq!(
            FinderTagEdit::ClearLegacy.apply(&state).unwrap(),
            FinderTagState {
                tags: state.tags.clone(),
                legacy_color: 0
            }
        );
        assert_eq!(
            FinderTagEdit::Clear.apply(&state).unwrap(),
            FinderTagState {
                tags: vec![],
                legacy_color: 0
            }
        );
    }
}
