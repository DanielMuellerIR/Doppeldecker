//! Passive Inhaltsvorschau aus derselben gebundenen Momentaufnahme wie Dateiöffnen.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::{Cancel, ProviderEndpoint, SnapshotOptions};

pub const PREVIEW_MAX_BYTES: u64 = 8 * 1024 * 1024;
const TEXT_MAX_BYTES: usize = 1024 * 1024;
const IMAGE_MAX_PIXELS: u64 = 24_000_000;

#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewContent {
    Text {
        text: String,
    },
    Image {
        data_url: String,
        width: u32,
        height: u32,
    },
    Unsupported {
        reason: String,
    },
}

#[derive(Debug, serde::Serialize)]
pub struct FilePreview {
    pub bytes: u64,
    pub content: PreviewContent,
    pub limitations: Vec<String>,
}

pub fn prepare_file_preview(
    endpoint: &ProviderEndpoint,
    source: &Path,
    cache: &Path,
    cancel: &Cancel,
) -> io::Result<FilePreview> {
    let normalized = crate::validate_file_operation_root(source)?;
    if normalized.as_os_str() != source.as_os_str()
        || source == endpoint.root()
        || !source.starts_with(endpoint.root())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Vorschaudatei muss innerhalb des gebundenen Pane-Roots liegen",
        ));
    }
    let (vfs, _) = endpoint.interactive_file_read_context(&[source.to_path_buf()], cancel)?;
    let snapshot = crate::remote_snapshot::download_remote_snapshot_bounded(
        vfs.as_ref(),
        source,
        cache,
        cancel,
        SnapshotOptions::default(),
        PREVIEW_MAX_BYTES,
    )?;
    // Keine beliebige Frontend-Datei wird gelesen. Der Pfad stammt ausschließlich
    // aus dem gerade erzeugten privaten Snapshot; Hash und Längenlimit binden
    // auch die Übergabe vom Cache an den Betrachter.
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&snapshot.path)?
        .take(PREVIEW_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != snapshot.bytes
        || blake3::hash(&bytes).to_hex().as_str() != snapshot.blake3
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Vorschaumomentaufnahme wurde verändert",
        ));
    }
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Vorschau wurde abgebrochen",
        ));
    }
    Ok(FilePreview {
        bytes: snapshot.bytes,
        content: classify_preview_bytes(&bytes),
        limitations: endpoint.interactive_file_read_limitations(),
    })
}

fn unsupported(reason: &str) -> PreviewContent {
    PreviewContent::Unsupported {
        reason: reason.into(),
    }
}

/// Klassifiziert ausschließlich bereits verifizierte Bytes für passive Darstellung.
pub fn classify_preview_bytes(bytes: &[u8]) -> PreviewContent {
    if bytes.len() as u64 > PREVIEW_MAX_BYTES {
        return unsupported("Der Inhalt überschreitet die Vorschaugrenze von 8 MiB.");
    }
    if let Some((mime, width, height)) = image_dimensions(bytes) {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > IMAGE_MAX_PIXELS {
            return unsupported("Das Bild überschreitet die Vorschaugrenze von 24 Millionen Pixeln oder hat ungültige Abmessungen.");
        }
        return PreviewContent::Image {
            data_url: format!("data:{mime};base64,{}", base64(bytes)),
            width,
            height,
        };
    }
    if bytes.len() > TEXT_MAX_BYTES {
        return unsupported("Textvorschauen sind auf 1 MiB begrenzt. Bilder werden als PNG oder JPEG bis 8 MiB angezeigt.");
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.chars().any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t')) => {
            PreviewContent::Text { text: text.into() }
        }
        _ => unsupported("Keine passive Vorschau verfügbar: unterstützt werden UTF-8-Text sowie PNG- und JPEG-Bilder. Binärdateien, andere Zeichencodierungen und weitere Formate werden nicht dargestellt."),
    }
}

// Nur Rasterformate werden dem Browserdecoder übergeben. Dateiendungen bestimmen
// keinen MIME-Typ; HTML und SVG bleiben ausschließlich inert dargestellter Text.
fn image_dimensions(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.get(8..16) != Some(b"\0\0\0\rIHDR") {
            return None;
        }
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        // APNG kann trotz kleiner Leinwand viele Frames halten. Diese erste
        // Vorschau unterstützt deshalb ausschließlich statische PNG-Dateien.
        let mut offset = 8;
        let mut image_data = false;
        while offset < bytes.len() {
            let length =
                u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize;
            let kind = bytes.get(offset + 4..offset + 8)?;
            let next = offset.checked_add(length)?.checked_add(12)?;
            if next > bytes.len()
                || matches!(kind, b"acTL" | b"fcTL" | b"fdAT")
                || (offset != 8 && kind == b"IHDR")
            {
                return None;
            }
            image_data |= kind == b"IDAT";
            if kind == b"IEND" {
                return (length == 0 && next == bytes.len() && image_data).then_some((
                    "image/png",
                    width,
                    height,
                ));
            }
            offset = next;
        }
        return None;
    }
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut offset = 2;
    while offset < bytes.len() {
        if *bytes.get(offset)? != 0xff {
            return None;
        }
        while *bytes.get(offset)? == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        let length = usize::from(u16::from_be_bytes(
            bytes.get(offset..offset + 2)?.try_into().ok()?,
        ));
        if length < 2 || offset.checked_add(length)? > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if length < 8 {
                return None;
            }
            let height = u32::from(u16::from_be_bytes(
                bytes.get(offset + 3..offset + 5)?.try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes.get(offset + 5..offset + 7)?.try_into().ok()?,
            ));
            return Some(("image/jpeg", width, height));
        }
        offset += length;
    }
    None
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderEndpoint;

    #[test]
    fn preview_text_bleibt_passiv_und_binaerdateien_werden_abgelehnt() {
        let text = "<svg onload='evil()'></svg>";
        assert!(
            matches!(classify_preview_bytes(text.as_bytes()), PreviewContent::Text { text: actual } if actual == text)
        );
        assert!(matches!(
            classify_preview_bytes(b"a\0b"),
            PreviewContent::Unsupported { .. }
        ));
        assert!(matches!(
            classify_preview_bytes(&[0xff, 0xfe]),
            PreviewContent::Unsupported { .. }
        ));
        assert!(matches!(
            classify_preview_bytes(&vec![b'a'; TEXT_MAX_BYTES + 1]),
            PreviewContent::Unsupported { .. }
        ));
        assert!(
            matches!(classify_preview_bytes(b""), PreviewContent::Text { text } if text.is_empty())
        );
    }

    #[test]
    fn klassifikation_begrenzt_bytes_vor_text_oder_base64() {
        let mut jpeg = vec![0; PREVIEW_MAX_BYTES as usize + 1];
        jpeg[..12].copy_from_slice(&[0xff, 0xd8, 0xff, 0xc0, 0, 8, 8, 0, 50, 0, 100, 0]);
        assert!(
            matches!(classify_preview_bytes(&jpeg), PreviewContent::Unsupported { reason } if reason.contains("8 MiB"))
        );
    }

    #[test]
    fn preview_bilder_pruefen_signatur_dimensionen_und_pixelbudget() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(100_u32.to_be_bytes());
        png.extend(50_u32.to_be_bytes());
        png.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
        png.extend(b"\0\0\0\0IDAT\0\0\0\0");
        png.extend(b"\0\0\0\0IEND\0\0\0\0");
        let mut animated = png.clone();
        animated.splice(33..33, *b"\0\0\0\0acTL\0\0\0\0");
        assert_eq!(image_dimensions(&animated), None);
        assert!(matches!(
            classify_preview_bytes(&png),
            PreviewContent::Image {
                width: 100,
                height: 50,
                ..
            }
        ));
        png[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            classify_preview_bytes(&png),
            PreviewContent::Unsupported { .. }
        ));
        let jpeg = [0xff, 0xd8, 0xff, 0xc0, 0, 8, 8, 0, 50, 0, 100, 0];
        assert_eq!(image_dimensions(&jpeg), Some(("image/jpeg", 100, 50)));
        for end in 0..jpeg.len() {
            assert_eq!(image_dimensions(&jpeg[..end]), None);
        }
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn preview_liest_gebundene_datei_und_verweigert_grosse_dateien_vor_cacheanlage() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let source = std::fs::canonicalize(source).unwrap();
        let file = source.join("file.txt");
        std::fs::write(&file, b"hello").unwrap();
        let cache = temp.path().join("cache");
        let endpoint = ProviderEndpoint::local(&source).unwrap();
        let result = prepare_file_preview(&endpoint, &file, &cache, &Cancel::new()).unwrap();
        assert!(matches!(result.content, PreviewContent::Text { text } if text == "hello"));
        let large = source.join("large");
        let large_file = std::fs::File::create(&large).unwrap();
        large_file.set_len(PREVIEW_MAX_BYTES + 1).unwrap();
        let unused = temp.path().join("unused");
        assert!(prepare_file_preview(&endpoint, &large, &unused, &Cancel::new()).is_err());
        assert!(!unused.exists());
        let link = source.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(prepare_file_preview(&endpoint, &link, &unused, &Cancel::new()).is_err());
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"private").unwrap();
        assert!(prepare_file_preview(&endpoint, &outside, &unused, &Cancel::new()).is_err());
    }
}
