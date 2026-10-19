#![no_main]

use doppeldecker_core::MetadataManifest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 * 1024 {
        return;
    }
    // Binäre Namen/Werte, ACL-Varianten und Sparse-/Hardlinkdaten dürfen auch bei
    // beliebig beschädigten Plattendaten weder panicen noch Grenzen umgehen.
    if let Ok(value) = serde_json::from_slice::<MetadataManifest>(data) {
        let _ = value.validate();
    }
});
