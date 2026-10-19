use doppeldecker_core::{BidirectionalBaseline, BidirectionalRunJournal, MetadataManifest};

#[test]
fn versionierte_fuzz_seeds_bleiben_schema_und_digestgueltig() {
    let baseline: BidirectionalBaseline =
        serde_json::from_slice(include_bytes!("../seeds/bidirectional_baseline/valid.json"))
            .unwrap();
    baseline.validate().unwrap();

    let journal: BidirectionalRunJournal = serde_json::from_slice(include_bytes!(
        "../seeds/bidirectional_run_journal/valid.json"
    ))
    .unwrap();
    journal.validate().unwrap();

    let manifest: MetadataManifest =
        serde_json::from_slice(include_bytes!("../seeds/metadata_manifest/valid.json")).unwrap();
    manifest.validate().unwrap();
}
