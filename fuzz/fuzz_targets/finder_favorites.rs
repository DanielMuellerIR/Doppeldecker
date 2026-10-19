//! Fuzzt den Finder-Favoriten-Parser (NSKeyedArchiver-Plist + Bookmark-Blobs).
//! Der Parser liest eine fremdformatige Systemdatei und muss auf beliebigen
//! Bytes ohne Panic, Endlosschleife oder unbegrenzte Allokation antworten.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = doppeldecker_core::finder_favorites::parse_favorite_items(data);
});
