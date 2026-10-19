#![no_main]

use doppeldecker_core::BidirectionalRunJournal;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 * 1024 {
        return;
    }
    // Der Validator bindet den eingebetteten Plan, beide Snapshots, den geordneten
    // Projektionspräfix und alle Recovery-Zwischenphasen an ihre Digests.
    if let Ok(value) = serde_json::from_slice::<BidirectionalRunJournal>(data) {
        let _ = value.validate();
    }
});
