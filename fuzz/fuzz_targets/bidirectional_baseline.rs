#![no_main]

use doppeldecker_core::BidirectionalBaseline;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 * 1024 {
        return;
    }
    // Deserialisierung allein reicht nicht: Ein syntaktisch gültiges Dokument muss
    // zusätzlich Version, Pfade, Reihenfolge und Eigendigest fail-closed prüfen.
    if let Ok(value) = serde_json::from_slice::<BidirectionalBaseline>(data) {
        let _ = value.validate();
    }
});
