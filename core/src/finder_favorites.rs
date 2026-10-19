//! Finder-Seitenleisten-Favoriten (macOS) lesen.
//!
//! macOS speichert die Favoriten der Finder-Seitenleiste unter
//! `~/Library/Application Support/com.apple.sharedfilelist/` als
//! `com.apple.LSSharedFileList.FavoriteItems.sfl2/.sfl3/.sfl4`. Alle drei Varianten
//! sind NSKeyedArchiver-Binär-Plists mit derselben Grundstruktur: ein Wurzel-Dictionary
//! mit einer `items`-Liste; jeder Eintrag trägt einen „Bookmark“-Blob in Apples
//! Bookmark-Format (Magic `book`), in dem der Pfad als Liste seiner Komponenten liegt.
//!
//! Dieses Modul parst NUR — es löst Bookmarks bewusst nicht über macOS-APIs auf.
//! Dadurch bleibt es GUI-frei, plattformunabhängig kompilier- und headless testbar,
//! und ein manipuliertes oder beschädigtes Eingabefile kann höchstens dazu führen,
//! dass Einträge übersprungen werden (fail-closed), niemals zu einem Absturz.
//! Die Datei stammt zwar vom eigenen macOS, wird hier aber trotzdem wie fremde
//! Eingabe behandelt: jede Länge und jeder Offset wird geprüft.

use std::io;

use plist::Value;

/// Obergrenzen gegen unplausible oder böswillig aufgeblähte Eingaben. Reale
/// Favoritenlisten haben ein bis zwei Dutzend Einträge und Pfade mit wenigen
/// Komponenten; die Grenzen liegen bewusst weit darüber.
const MAX_ITEMS: usize = 512;
const MAX_TOCS: usize = 64;
const MAX_TOC_ENTRIES: usize = 4096;
const MAX_PATH_COMPONENTS: usize = 256;

/// Bookmark-Format: TOC-Schlüssel der Pfadkomponenten-Liste.
const KEY_PATH_COMPONENTS: u32 = 0x1004;
/// Bookmark-Format: Trennmarke zwischen TOC-Kopf und Einträgen.
const TOC_MAGIC: u32 = 0xffff_fffe;
/// Bookmark-Format: Record-Typ „UTF-8-String“.
const RECORD_STRING: u32 = 0x0101;
/// Bookmark-Format: Record-Typ „Array von Record-Offsets“.
const RECORD_ARRAY: u32 = 0x0601;

/// Property-Schlüssel, mit dem der Finder ausgeblendete Seitenleisten-Einträge markiert.
const PROP_ITEM_IS_HIDDEN: &str = "com.apple.LSSharedFileList.ItemIsHidden";

/// Ein Seitenleisten-Eintrag des Finders, soweit für einen Import relevant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinderSidebarItem {
    /// Absoluter Pfad, zusammengesetzt aus den Bookmark-Pfadkomponenten.
    pub path: String,
    /// Vom Finder als ausgeblendet markiert (taucht in der Seitenleiste nicht auf).
    pub hidden: bool,
}

/// Ergebnis eines Parse-Laufs: die lesbaren Einträge plus die Zahl der Einträge,
/// die übersprungen wurden (fehlender/unlesbarer Bookmark, unsichere Namen …).
/// Übersprungenes wird gezählt statt verschwiegen, damit die Oberfläche es
/// sichtbar bilanzieren kann.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FinderSidebarList {
    pub items: Vec<FinderSidebarItem>,
    pub skipped_unreadable: usize,
}

/// Parst den Inhalt einer `FavoriteItems`-Datei (sfl2/sfl3/sfl4).
///
/// Harte Strukturfehler (kein Binär-Plist, kein NSKeyedArchiver-Wurzelobjekt,
/// unplausibel viele Einträge) sind ein Fehler. Einzelne unlesbare Einträge
/// brechen dagegen nicht den ganzen Import ab, sondern werden gezählt.
pub fn parse_favorite_items(bytes: &[u8]) -> io::Result<FinderSidebarList> {
    let value = Value::from_reader(io::Cursor::new(bytes))
        .map_err(|error| invalid(format!("Kein lesbares Binär-Plist: {error}")))?;
    let document = value
        .as_dictionary()
        .ok_or_else(|| invalid("Plist-Wurzel ist kein Dictionary"))?;
    let objects = document
        .get("$objects")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("NSKeyedArchiver-Objekttabelle ($objects) fehlt"))?;
    let root = document
        .get("$top")
        .and_then(Value::as_dictionary)
        .and_then(|top| top.get("root"))
        .and_then(|root| deref(objects, root))
        .ok_or_else(|| invalid("NSKeyedArchiver-Wurzelverweis ($top.root) fehlt"))?;
    let items = ns_dictionary_get(objects, root, "items")
        .and_then(|items| ns_array(objects, items))
        .ok_or_else(|| invalid("Favoritenliste (items) fehlt oder hat falsche Form"))?;
    if items.len() > MAX_ITEMS {
        return Err(invalid(format!(
            "Unplausibel viele Einträge ({} > {MAX_ITEMS})",
            items.len()
        )));
    }

    let mut result = FinderSidebarList::default();
    for item in items {
        let Some(item) = deref(objects, item) else {
            result.skipped_unreadable += 1;
            continue;
        };
        let path = ns_dictionary_get(objects, item, "Bookmark")
            .and_then(Value::as_data)
            .and_then(bookmark_path);
        let Some(path) = path else {
            result.skipped_unreadable += 1;
            continue;
        };
        let hidden = ns_dictionary_get(objects, item, "CustomItemProperties")
            .and_then(|properties| ns_dictionary_get(objects, properties, PROP_ITEM_IS_HIDDEN))
            .and_then(Value::as_boolean)
            .unwrap_or(false);
        result.items.push(FinderSidebarItem { path, hidden });
    }
    Ok(result)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Finder-Favoriten: {}", message.into()),
    )
}

/// Löst einen NSKeyedArchiver-Verweis (UID in die Objekttabelle) auf; alle anderen
/// Werte gehen unverändert durch. Ein Verweis ins Leere ergibt `None`.
fn deref<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a Value> {
    match value {
        Value::Uid(uid) => objects.get(usize::try_from(uid.get()).ok()?),
        other => Some(other),
    }
}

/// Liest aus einem archivierten NSDictionary (`NS.keys`/`NS.objects`) den Wert zum
/// String-Schlüssel `wanted` — bereits dereferenziert.
fn ns_dictionary_get<'a>(
    objects: &'a [Value],
    value: &'a Value,
    wanted: &str,
) -> Option<&'a Value> {
    let dictionary = value.as_dictionary()?;
    let keys = dictionary.get("NS.keys")?.as_array()?;
    let values = dictionary.get("NS.objects")?.as_array()?;
    for (key, entry) in keys.iter().zip(values.iter()) {
        if deref(objects, key)?.as_string() == Some(wanted) {
            return deref(objects, entry);
        }
    }
    None
}

/// Liest ein archiviertes NSArray (`NS.objects`) als Slice seiner (noch nicht
/// dereferenzierten) Elemente.
fn ns_array<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a [Value]> {
    let value = deref(objects, value)?;
    let dictionary = value.as_dictionary()?;
    Some(dictionary.get("NS.objects")?.as_array()?.as_slice())
}

/// Liest ein `u32` (little-endian, wie im Bookmark-Format) an `offset` — ohne je
/// über das Slice-Ende hinauszugreifen.
fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let raw: [u8; 4] = data.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

/// Ein Bookmark-Record: `[Länge u32][Typ u32][Nutzdaten …]`, Offsets relativ zum
/// Datenbereich hinter dem Header.
fn read_record(data: &[u8], offset: usize) -> Option<(u32, &[u8])> {
    let length = u32_at(data, offset)? as usize;
    let kind = u32_at(data, offset.checked_add(4)?)?;
    let start = offset.checked_add(8)?;
    Some((kind, data.get(start..start.checked_add(length)?)?))
}

/// Extrahiert den absoluten Pfad aus einem Bookmark-Blob (Magic `book`).
///
/// Aufbau: fester Header (Größe steht bei Offset 12), danach der Datenbereich.
/// Dessen erste 4 Bytes zeigen auf eine TOC-Kette; der TOC-Schlüssel `0x1004`
/// verweist auf ein Array von String-Records mit den Pfadkomponenten.
/// Jede Abweichung (fremdes Magic, Offsets außerhalb, Zyklen, unsichere Namen)
/// ergibt `None` — der Eintrag wird dann als unlesbar bilanziert.
fn bookmark_path(blob: &[u8]) -> Option<String> {
    if blob.get(0..4)? != b"book" {
        return None;
    }
    let header_size = u32_at(blob, 12)? as usize;
    if header_size < 16 {
        return None;
    }
    let data = blob.get(header_size..)?;

    // TOC-Kette ablaufen und den Pfadkomponenten-Record suchen. Die Kette ist
    // in der Länge begrenzt, damit ein manipulierter Blob keine Endlosschleife
    // erzeugen kann.
    let mut toc_offset = u32_at(data, 0)? as usize;
    let mut path_record = None;
    let mut visited = 0usize;
    while toc_offset != 0 {
        visited += 1;
        if visited > MAX_TOCS {
            return None;
        }
        // TOC-Kopf: Größe, Trennmarke, Kennung, nächster TOC, Eintragszahl.
        if u32_at(data, toc_offset.checked_add(4)?)? != TOC_MAGIC {
            return None;
        }
        let next = u32_at(data, toc_offset.checked_add(12)?)? as usize;
        let count = u32_at(data, toc_offset.checked_add(16)?)? as usize;
        if count > MAX_TOC_ENTRIES {
            return None;
        }
        for index in 0..count {
            // Jeder Eintrag: Schlüssel, Record-Offset, Flags (12 Bytes).
            let entry = toc_offset.checked_add(20)?.checked_add(index * 12)?;
            if u32_at(data, entry)? == KEY_PATH_COMPONENTS {
                path_record = Some(u32_at(data, entry.checked_add(4)?)? as usize);
            }
        }
        toc_offset = next;
    }

    let (kind, offsets) = read_record(data, path_record?)?;
    if kind != RECORD_ARRAY || !offsets.len().is_multiple_of(4) {
        return None;
    }
    let count = offsets.len() / 4;
    if count == 0 || count > MAX_PATH_COMPONENTS {
        return None;
    }
    let mut path = String::new();
    for index in 0..count {
        let offset = u32_at(offsets, index * 4)? as usize;
        let (kind, name) = read_record(data, offset)?;
        if kind != RECORD_STRING {
            return None;
        }
        // Namen fail-closed prüfen: nur eindeutig dekodierbares UTF-8 ohne
        // Pfadtrenner, NUL oder Verzeichnis-Aufstieg wird zu einem Pfad gebaut.
        let name = std::str::from_utf8(name).ok()?;
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            return None;
        }
        path.push('/');
        path.push_str(name);
    }
    Some(path)
}

// ---------------------------------------------------------------------------
// Tests (headless auf macOS UND Linux) — die Fixtures werden synthetisch im
// Test aufgebaut, damit keine Nutzerdaten oder Binärdateien im Repo liegen.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use plist::{Dictionary, Uid};

    /// Baut einen minimalen, aber formatechten Bookmark-Blob mit diesen
    /// Pfadkomponenten (Rohbytes, damit auch kaputtes UTF-8 testbar ist).
    fn bookmark_blob_raw(components: &[&[u8]]) -> Vec<u8> {
        let mut data = vec![0u8; 4]; // Platzhalter: Offset des ersten TOC.
        let mut string_offsets = Vec::new();
        for component in components {
            string_offsets.push(data.len() as u32);
            data.extend((component.len() as u32).to_le_bytes());
            data.extend(RECORD_STRING.to_le_bytes());
            data.extend(*component);
            while !data.len().is_multiple_of(4) {
                data.push(0); // Padding wie im Originalformat.
            }
        }
        let array_offset = data.len() as u32;
        data.extend(((string_offsets.len() * 4) as u32).to_le_bytes());
        data.extend(RECORD_ARRAY.to_le_bytes());
        for offset in &string_offsets {
            data.extend(offset.to_le_bytes());
        }
        let toc_offset = data.len() as u32;
        data.extend(32u32.to_le_bytes()); // TOC-Größe (Kopf + 1 Eintrag)
        data.extend(TOC_MAGIC.to_le_bytes());
        data.extend(1u32.to_le_bytes()); // Kennung
        data.extend(0u32.to_le_bytes()); // kein weiterer TOC
        data.extend(1u32.to_le_bytes()); // ein Eintrag
        data.extend(KEY_PATH_COMPONENTS.to_le_bytes());
        data.extend(array_offset.to_le_bytes());
        data.extend(0u32.to_le_bytes()); // Flags
        data[0..4].copy_from_slice(&toc_offset.to_le_bytes());

        let mut blob = Vec::new();
        blob.extend(b"book");
        blob.extend(((48 + data.len()) as u32).to_le_bytes());
        blob.extend(0u32.to_le_bytes());
        blob.extend(48u32.to_le_bytes()); // Header-Größe wie in echten Blobs
        blob.resize(48, 0);
        blob.extend(data);
        blob
    }

    fn bookmark_blob(components: &[&str]) -> Vec<u8> {
        let raw: Vec<&[u8]> = components.iter().map(|c| c.as_bytes()).collect();
        bookmark_blob_raw(&raw)
    }

    /// Archiviertes NSDictionary: Schlüssel inline, Werte wie übergeben.
    fn ns_dict(pairs: Vec<(&str, Value)>) -> Value {
        let mut dictionary = Dictionary::new();
        dictionary.insert(
            "NS.keys".into(),
            Value::Array(
                pairs
                    .iter()
                    .map(|(k, _)| Value::String((*k).into()))
                    .collect(),
            ),
        );
        dictionary.insert(
            "NS.objects".into(),
            Value::Array(pairs.into_iter().map(|(_, v)| v).collect()),
        );
        Value::Dictionary(dictionary)
    }

    /// Archiviertes NSArray.
    fn ns_list(values: Vec<Value>) -> Value {
        let mut dictionary = Dictionary::new();
        dictionary.insert("NS.objects".into(), Value::Array(values));
        Value::Dictionary(dictionary)
    }

    /// Serialisiert einen NSKeyedArchiver-Objektgraphen als Binär-Plist. Die Wurzel
    /// steht wie im Original als UID-Verweis in `$top.root`.
    fn keyed_archive(objects: Vec<Value>, root: u64) -> Vec<u8> {
        let mut top = Dictionary::new();
        top.insert("root".into(), Value::Uid(Uid::new(root)));
        let mut document = Dictionary::new();
        document.insert("$version".into(), Value::Integer(100_000.into()));
        document.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
        document.insert("$top".into(), Value::Dictionary(top));
        document.insert("$objects".into(), Value::Array(objects));
        let mut buffer = io::Cursor::new(Vec::new());
        Value::Dictionary(document)
            .to_writer_binary(&mut buffer)
            .unwrap();
        buffer.into_inner()
    }

    /// Standard-Fixture: Wurzel mit `items`-Liste; einzelne Werte dürfen als
    /// UID-Verweis in die Objekttabelle zeigen (wie in echten Dateien).
    fn favorite_items_archive(items: Vec<Value>, extra_objects: Vec<Value>) -> Vec<u8> {
        let root = ns_dict(vec![("items", ns_list(items))]);
        let mut objects = vec![Value::String("$null".into()), root];
        objects.extend(extra_objects);
        keyed_archive(objects, 1)
    }

    #[test]
    fn parst_pfade_und_hidden_flag() {
        // Ein Eintrag inline, einer mit Bookmark hinter einem UID-Verweis
        // (Index 2 der Objekttabelle) — beide Formen kommen in echten Dateien vor.
        let hidden_properties = ns_dict(vec![(PROP_ITEM_IS_HIDDEN, Value::Boolean(true))]);
        let items = vec![
            ns_dict(vec![
                (
                    "Bookmark",
                    Value::Data(bookmark_blob(&["Users", "demo", "Projekte"])),
                ),
                ("CustomItemProperties", hidden_properties),
            ]),
            ns_dict(vec![("Bookmark", Value::Uid(Uid::new(2)))]),
        ];
        let archive =
            favorite_items_archive(items, vec![Value::Data(bookmark_blob(&["Applications"]))]);

        let parsed = parse_favorite_items(&archive).unwrap();

        assert_eq!(parsed.skipped_unreadable, 0);
        assert_eq!(
            parsed.items,
            vec![
                FinderSidebarItem {
                    path: "/Users/demo/Projekte".into(),
                    hidden: true,
                },
                FinderSidebarItem {
                    path: "/Applications".into(),
                    hidden: false,
                },
            ],
        );
    }

    #[test]
    fn eintrag_ohne_bookmark_wird_gezaehlt_uebersprungen() {
        let items = vec![
            ns_dict(vec![("uuid", Value::String("ABC".into()))]),
            ns_dict(vec![("Bookmark", Value::Data(bookmark_blob(&["tmp"])))]),
        ];
        let parsed = parse_favorite_items(&favorite_items_archive(items, vec![])).unwrap();
        assert_eq!(parsed.skipped_unreadable, 1);
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].path, "/tmp");
    }

    #[test]
    fn unsichere_oder_kaputte_bookmarks_werden_uebersprungen() {
        let full = bookmark_blob(&["Users", "demo"]);
        let cases: Vec<Vec<u8>> = vec![
            full[..full.len() / 2].to_vec(),             // abgeschnitten
            b"nichtbook".to_vec(),                       // fremdes Magic
            bookmark_blob(&["Users", "..", "etc"]),      // Verzeichnis-Aufstieg
            bookmark_blob(&["Users", "a/b"]),            // Pfadtrenner im Namen
            bookmark_blob_raw(&[b"Users", b"\xff\xfe"]), // kein UTF-8
            bookmark_blob(&[]),                          // leere Komponentenliste
        ];
        let count = cases.len();
        let items = cases
            .into_iter()
            .map(|blob| ns_dict(vec![("Bookmark", Value::Data(blob))]))
            .collect();
        let parsed = parse_favorite_items(&favorite_items_archive(items, vec![])).unwrap();
        assert_eq!(parsed.items, vec![]);
        assert_eq!(parsed.skipped_unreadable, count);
    }

    #[test]
    fn toc_zyklus_terminiert_als_unlesbar() {
        // Der TOC verweist als „nächster TOC“ auf sich selbst — die Schleifen-
        // Obergrenze muss greifen, statt endlos zu laufen.
        let mut blob = bookmark_blob(&["Users"]);
        let data_start = 48;
        let toc_offset = u32_at(&blob[data_start..], 0).unwrap() as usize;
        let next_position = data_start + toc_offset + 12;
        blob[next_position..next_position + 4].copy_from_slice(&(toc_offset as u32).to_le_bytes());

        let items = vec![ns_dict(vec![("Bookmark", Value::Data(blob))])];
        let parsed = parse_favorite_items(&favorite_items_archive(items, vec![])).unwrap();
        assert_eq!(parsed.items, vec![]);
        assert_eq!(parsed.skipped_unreadable, 1);
    }

    #[test]
    fn strukturfehler_sind_harte_fehler() {
        // Gar kein Plist.
        assert!(parse_favorite_items(b"kein plist").is_err());
        // Plist, aber kein NSKeyedArchiver mit items-Liste.
        let mut document = Dictionary::new();
        document.insert("irgendwas".into(), Value::String("x".into()));
        let mut buffer = io::Cursor::new(Vec::new());
        Value::Dictionary(document)
            .to_writer_binary(&mut buffer)
            .unwrap();
        assert!(parse_favorite_items(&buffer.into_inner()).is_err());
    }

    #[test]
    fn unplausibel_viele_eintraege_sind_ein_harter_fehler() {
        let items = (0..(MAX_ITEMS + 1))
            .map(|_| ns_dict(vec![("Bookmark", Value::Data(bookmark_blob(&["tmp"])))]))
            .collect();
        assert!(parse_favorite_items(&favorite_items_archive(items, vec![])).is_err());
    }
}
