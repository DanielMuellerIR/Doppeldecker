use doppeldecker_core::metadata::MetadataManifest;
use std::collections::HashSet;

#[test]
fn reported_link_count_preserves_independent_legacy_v2_v3_v4_v5_vectors() {
    // Vor der Erweiterung mit e87dd185 erzeugt; keine Neuberechnung im Test.
    let fixtures = include_str!("fixtures/reported-links-legacy.jsonl");
    assert_eq!(fixtures.lines().count(), 4);
    for line in fixtures.lines() {
        let old: MetadataManifest = serde_json::from_str(line).unwrap();
        old.validate().unwrap();
        assert_eq!(old.reported_hard_links, None);
        assert_eq!(serde_json::to_string(&old).unwrap(), line);
        assert_eq!(old.clone().with_reported_hard_links(None).unwrap(), old);
        let current = old.clone().with_reported_hard_links(Some(2)).unwrap();
        current.validate().unwrap();
        assert_ne!(current.digest, old.digest);
        assert_eq!(current.with_reported_hard_links(None).unwrap(), old);
    }
}

#[test]
fn reported_link_counts_bind_values_presence_and_integer_bounds() {
    let bases: Vec<MetadataManifest> = include_str!("fixtures/reported-links-legacy.jsonl")
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let mut digests = HashSet::new();
    for old in bases {
        assert!(digests.insert(old.digest));
        for count in [0, 1, 2, u32::MAX] {
            let current = old.clone().with_reported_hard_links(Some(count)).unwrap();
            assert!(digests.insert(current.digest));
            let text = serde_json::to_string(&current).unwrap();
            let decoded: MetadataManifest = serde_json::from_str(&text).unwrap();
            decoded.validate().unwrap();
            assert_eq!(decoded, current);
            let mut tampered = serde_json::to_value(&current).unwrap();
            tampered["reported_hard_links"] = serde_json::json!(count ^ 1);
            let tampered: MetadataManifest = serde_json::from_value(tampered).unwrap();
            assert!(tampered.validate().is_err());
            let mut overflow = serde_json::to_value(&current).unwrap();
            overflow["reported_hard_links"] = serde_json::json!(u64::from(u32::MAX) + 1);
            assert!(serde_json::from_value::<MetadataManifest>(overflow).is_err());
        }
    }
    let base = MetadataManifest::new(None, 0o640, None, vec![], None, None, None).unwrap();
    assert!(base
        .clone()
        .with_reported_hard_links(Some(1))
        .unwrap()
        .with_hardlink_group(Some(7))
        .is_err());
    assert!(base
        .clone()
        .with_reported_hard_links(Some(0))
        .unwrap()
        .with_hardlink_group(Some(7))
        .is_err());
    base.clone()
        .with_reported_hard_links(Some(2))
        .unwrap()
        .with_hardlink_group(Some(7))
        .unwrap()
        .validate()
        .unwrap();
    let seeded: MetadataManifest = serde_json::from_str(include_str!(
        "../../fuzz/seeds/metadata_manifest/reported-links.json"
    ))
    .unwrap();
    seeded.validate().unwrap();
    assert_eq!(seeded, base.with_reported_hard_links(Some(2)).unwrap());
}
