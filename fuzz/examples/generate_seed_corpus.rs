//! Erzeugt je ein gültiges Startdokument für die Parser-Fuzz-Targets.
//!
//! Die lokalen Rootidentitäten stammen aus einem frischen temporären Baum. Nach der
//! Serialisierung werden sie nur noch strukturell geprüft; der Fuzzer öffnet diese
//! Pfade niemals wieder.

use std::fs;
use std::path::Path;

use doppeldecker_core::{
    prepare_local_bidirectional, BaselineStore, Cancel, MetadataManifest, PairId,
};
use serde::Serialize;

fn write_json(root: &Path, target: &str, value: &impl Serialize) {
    let directory = root.join(target);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("valid.json"),
        serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}

fn write_bytes(root: &Path, target: &str, value: &[u8]) {
    let directory = root.join(target);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("valid.json"), value).unwrap();
}

fn main() {
    let canonical_temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    let temp = tempfile::tempdir_in(canonical_temp).unwrap();
    let left = temp.path().join("left");
    let right = temp.path().join("right");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    fs::write(left.join("gleich.txt"), b"Fuzz-Start\n").unwrap();
    fs::write(right.join("gleich.txt"), b"Fuzz-Start\n").unwrap();
    let baseline_path = temp.path().join("state/pair.json");
    let prepared = prepare_local_bidirectional(
        PairId([0x42; 16]),
        "Fuzz-Startpaar",
        &left,
        &right,
        &baseline_path,
    )
    .unwrap();

    let seeds = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
    let journal = fs::read(baseline_path.with_extension("run.json")).unwrap();
    write_bytes(&seeds, "bidirectional_run_journal", &journal);

    let phrase = prepared.preview().confirmation_phrase.clone();
    prepared
        .confirm(&phrase, true)
        .unwrap()
        .execute(&Cancel::new(), &mut |_| {})
        .unwrap();
    let baseline = BaselineStore::open(&baseline_path)
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
    write_json(&seeds, "bidirectional_baseline", &baseline);

    let manifest = MetadataManifest::new(None, 0o640, None, Vec::new(), None, None, None).unwrap();
    write_json(&seeds, "metadata_manifest", &manifest);
}
