use doppeldecker_core::metadata::{DataExtent, MetadataManifest, SparseReadIssue};
use std::collections::HashSet;
#[test]
fn sparse_issues_preserve_legacy_vectors_and_bind_v7_presence_status_and_link_count() {
    let legacy = include_str!("fixtures/reported-links-legacy.jsonl");
    let v6 = include_str!("../../fuzz/seeds/metadata_manifest/reported-links.json").trim();
    let mut hashes = HashSet::new();
    for text in legacy.lines().chain(std::iter::once(v6)) {
        let old: MetadataManifest = serde_json::from_str(text).unwrap();
        old.validate().unwrap();
        assert_eq!(serde_json::to_string(&old).unwrap(), text);
        assert_eq!(old.sparse_read_issue, None);
        assert_eq!(
            old.clone().with_sparse_observation(None, None).unwrap(),
            old
        );
        for issue in [
            SparseReadIssue::ServerStatus(0xc000_0002),
            SparseReadIssue::ServerStatus(0xc000_0003),
            SparseReadIssue::ServerStatus(0xc000_0010),
            SparseReadIssue::ServerStatus(0xc000_000d),
            SparseReadIssue::ServerStatus(0xc000_0022),
            SparseReadIssue::ServerStatus(0xc000_00bb),
            SparseReadIssue::BufferLimit,
            SparseReadIssue::InvalidResponse,
        ] {
            let value = old
                .clone()
                .with_sparse_observation(None, Some(issue))
                .unwrap();
            value.validate().unwrap();
            assert!(hashes.insert(value.digest));
            let text = serde_json::to_string(&value).unwrap();
            let parsed: MetadataManifest = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed, value);
            let mut tampered = serde_json::to_value(&value).unwrap();
            tampered["sparse_read_issue"] = serde_json::Value::Null;
            assert!(serde_json::from_value::<MetadataManifest>(tampered)
                .unwrap()
                .validate()
                .is_err());
            assert_eq!(value.with_sparse_observation(None, None).unwrap(), old);
        }
    }
}
#[test]
fn sparse_ranges_and_read_failures_are_mutually_exclusive() {
    let base = MetadataManifest::new(None, 0, None, vec![], None, None, None).unwrap();
    for extents in [
        vec![],
        vec![DataExtent {
            offset: 0,
            length: 4,
        }],
    ] {
        assert!(base
            .clone()
            .with_sparse_observation(
                Some(extents.clone()),
                Some(SparseReadIssue::InvalidResponse)
            )
            .is_err());
        base.clone()
            .with_sparse_observation(Some(extents), None)
            .unwrap()
            .validate()
            .unwrap();
    }
    assert!(base
        .with_sparse_observation(None, Some(SparseReadIssue::ServerStatus(0)))
        .is_err());
}

#[test]
fn denied_sparse_fuzz_seed_has_a_valid_bound_digest() {
    let manifest: MetadataManifest = serde_json::from_str(include_str!(
        "../../fuzz/seeds/metadata_manifest/sparse-read-denied.json"
    ))
    .unwrap();
    manifest.validate().unwrap();
    assert_eq!(
        manifest.sparse_read_issue,
        Some(SparseReadIssue::ServerStatus(0xc0000022))
    );
}
