//! Windows-Beobachtungen binden Rechtekontext und Auslassungen an den Manifestdigest.
//! Die V2-/V3-Goldwerte stammen aus dem unveränderten Metadatenmodul von 92dc.

use doppeldecker_core::metadata::{
    Acl, AclModel, Capability, MetadataCapabilities, MetadataManifest, Nfs4Acl, Owner,
    WindowsSecurityObservation, WindowsSecurityReadIssue,
};
use doppeldecker_core::windows_security::{WindowsAce, WindowsDacl, WindowsDaclState, WindowsSid};
use std::collections::HashSet;

const V2_BASIC: &str = r#"{"owner":null,"mode":416,"acl":null,"xattrs":[],"resource_fork":null,"sparse_extents":null,"hardlink_group":null,"digest":[160,162,182,85,118,189,24,59,76,182,133,111,18,66,15,48,218,116,70,105,37,188,32,237,56,246,73,20,198,130,98,217]}"#;
const V2_RICH: &str = r#"{"owner":{"uid":1000,"gid":100},"mode":3560,"acl":{"model":"Nfs4","value":{"entries":[{"qualifier":[18,18,18,18,18,18,18,18,18,18,18,18,18,18,18,18],"kind":"Deny","permissions":66051,"flags":16}],"flags":131072}},"xattrs":[{"name":[117,115,101,114,46,97],"value":{"size":1,"digest":[23,118,47,221,217,105,164,83,146,93,101,113,122,195,238,162,19,32,182,107,84,52,47,222,21,18,141,108,175,33,33,95]}},{"name":[117,115,101,114,46,122],"value":{"size":1,"digest":[17,4,144,138,185,48,230,113,0,44,124,215,243,252,146,21,112,177,191,100,236,250,18,254,54,53,133,198,48,234,202,107]}}],"resource_fork":{"size":4,"digest":[28,244,120,198,153,49,166,171,75,101,233,57,42,4,17,207,27,49,234,250,239,88,230,32,235,101,15,10,103,254,42,19]},"sparse_extents":[{"offset":0,"length":16},{"offset":32,"length":8}],"hardlink_group":7,"digest":[92,71,81,182,241,62,132,210,215,39,218,208,219,203,217,157,55,18,4,65,61,27,243,24,167,131,63,205,149,1,93,23]}"#;
const V3_EMPTY: &str = r#"{"owner":null,"mode":416,"acl":null,"xattrs":[],"resource_fork":null,"named_streams":[],"sparse_extents":null,"hardlink_group":null,"digest":[45,115,105,117,169,99,167,42,86,27,159,57,175,101,20,209,232,154,8,1,115,191,176,137,64,98,68,90,66,95,5,133]}"#;
const V3_RICH: &str = r#"{"owner":{"uid":1000,"gid":100},"mode":3560,"acl":{"model":"Nfs4","value":{"entries":[{"qualifier":[18,18,18,18,18,18,18,18,18,18,18,18,18,18,18,18],"kind":"Deny","permissions":66051,"flags":16}],"flags":131072}},"xattrs":[{"name":[117,115,101,114,46,97],"value":{"size":1,"digest":[23,118,47,221,217,105,164,83,146,93,101,113,122,195,238,162,19,32,182,107,84,52,47,222,21,18,141,108,175,33,33,95]}},{"name":[117,115,101,114,46,122],"value":{"size":1,"digest":[17,4,144,138,185,48,230,113,0,44,124,215,243,252,146,21,112,177,191,100,236,250,18,254,54,53,133,198,48,234,202,107]}}],"resource_fork":{"size":4,"digest":[28,244,120,198,153,49,166,171,75,101,233,57,42,4,17,207,27,49,234,250,239,88,230,32,235,101,15,10,103,254,42,19]},"named_streams":[{"name":[97,108,112,104,97],"value":{"size":7,"digest":[164,36,151,101,149,35,212,64,238,53,128,129,133,170,76,234,227,2,128,28,53,48,188,75,194,197,116,38,87,88,7,85]}},{"name":[98,101,116,97],"value":{"size":9,"digest":[90,38,152,138,209,84,5,15,177,50,108,86,193,96,107,251,227,226,88,67,191,210,248,27,22,211,147,226,58,185,59,118]}}],"sparse_extents":[{"offset":0,"length":16},{"offset":32,"length":8}],"hardlink_group":7,"digest":[164,49,19,252,58,68,45,128,143,48,61,15,136,227,118,53,33,50,29,233,67,63,67,41,243,188,48,205,8,11,190,193]}"#;

const NAMESPACE: [u8; 16] = [0x31; 16];

fn sid(authority: u64, sub_authorities: &[u32]) -> WindowsSid {
    WindowsSid {
        revision: 1,
        authority,
        sub_authorities: sub_authorities.to_vec(),
    }
}

fn observation() -> WindowsSecurityObservation {
    WindowsSecurityObservation {
        namespace: NAMESPACE,
        owner: Some(sid(5, &[21, 1, 2, 3, 1000])),
        group: Some(sid(22, &[2, 100])),
        issue: None,
        invalid_descriptor_digest: None,
    }
}

fn basic() -> MetadataManifest {
    MetadataManifest::new(None, 0o640, None, vec![], None, None, None).unwrap()
}

fn dacl(state: WindowsDaclState) -> WindowsDacl {
    WindowsDacl {
        namespace: NAMESPACE,
        control: if matches!(state, WindowsDaclState::Absent) {
            0
        } else {
            4
        },
        state,
    }
}

fn entries() -> Vec<WindowsAce> {
    vec![
        WindowsAce::Allow {
            flags: 0,
            mask: 0x120089,
            sid: sid(5, &[21, 1, 2, 3, 1000]),
        },
        WindowsAce::Deny {
            flags: 3,
            mask: 0x10000,
            sid: sid(1, &[0]),
        },
        WindowsAce::Opaque {
            ace_type: 0xfe,
            flags: 0x10,
            data: vec![0xde, 0xad, 0xbe, 0xef],
        },
    ]
}

fn windows_manifest(acl: WindowsDacl, observed: WindowsSecurityObservation) -> MetadataManifest {
    MetadataManifest::new(
        None,
        0o640,
        Some(Acl::WindowsDacl(acl)),
        vec![],
        None,
        None,
        None,
    )
    .unwrap()
    .with_windows_security(Some(observed))
    .unwrap()
}

fn full() -> MetadataManifest {
    windows_manifest(
        dacl(WindowsDaclState::Acl {
            revision: 2,
            aces: entries(),
        }),
        observation(),
    )
}

fn issue(issue: WindowsSecurityReadIssue, raw: Option<&[u8]>) -> WindowsSecurityObservation {
    WindowsSecurityObservation {
        namespace: NAMESPACE,
        owner: None,
        group: None,
        issue: Some(issue),
        invalid_descriptor_digest: raw.map(|bytes| *blake3::hash(bytes).as_bytes()),
    }
}

fn assert_roundtrip(manifest: &MetadataManifest) {
    manifest.validate().unwrap();
    let encoded = serde_json::to_string(manifest).unwrap();
    let decoded: MetadataManifest = serde_json::from_str(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(&decoded, manifest);
    assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
}

#[test]
fn legacy_v2_digest_and_json_match_pre_windows_gold_values() {
    for encoded in [V2_BASIC, V2_RICH] {
        let manifest: MetadataManifest = serde_json::from_str(encoded).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.windows_security, None);
        assert_eq!(manifest.named_streams, None);
        assert_eq!(serde_json::to_string(&manifest).unwrap(), encoded);
        let rebuilt = MetadataManifest::new(
            manifest.owner,
            manifest.mode,
            manifest.acl.clone(),
            manifest.xattrs.clone(),
            manifest.resource_fork.clone(),
            manifest.sparse_extents.clone(),
            manifest.hardlink_group,
        )
        .unwrap();
        assert_eq!(rebuilt.digest, manifest.digest);
        assert_eq!(serde_json::to_string(&rebuilt).unwrap(), encoded);
    }
    assert_eq!(serde_json::to_string(&basic()).unwrap(), V2_BASIC);
}

#[test]
fn legacy_v3_digest_and_json_match_pre_windows_gold_values() {
    for encoded in [V3_EMPTY, V3_RICH] {
        let manifest: MetadataManifest = serde_json::from_str(encoded).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.named_streams.is_some());
        assert_eq!(manifest.windows_security, None);
        assert_eq!(serde_json::to_string(&manifest).unwrap(), encoded);
        let mut reversed = manifest.named_streams.clone().unwrap();
        reversed.reverse();
        let rebuilt = manifest.clone().with_named_streams(Some(reversed)).unwrap();
        assert_eq!(serde_json::to_string(&rebuilt).unwrap(), encoded);
    }
    assert_eq!(
        serde_json::to_string(&basic().with_named_streams(Some(vec![])).unwrap()).unwrap(),
        V3_EMPTY
    );
}

#[test]
fn windows_v4_separately_binds_stream_presence_and_security_presence() {
    let unobserved = basic();
    let unknown_streams = unobserved
        .clone()
        .with_windows_security(Some(issue(
            WindowsSecurityReadIssue::ReadControlDenied,
            None,
        )))
        .unwrap();
    let empty_streams = unknown_streams
        .clone()
        .with_named_streams(Some(vec![]))
        .unwrap();
    assert_ne!(unknown_streams.digest, empty_streams.digest);
    assert_ne!(unobserved.digest, unknown_streams.digest);
    assert_ne!(
        basic().with_named_streams(Some(vec![])).unwrap().digest,
        empty_streams.digest
    );
    assert_eq!(
        unknown_streams.clone().with_windows_security(None).unwrap(),
        unobserved
    );
    let unknown_json = serde_json::to_value(&unknown_streams).unwrap();
    let empty_json = serde_json::to_value(&empty_streams).unwrap();
    assert!(unknown_json.get("named_streams").is_none());
    assert_eq!(empty_json["named_streams"], serde_json::json!([]));
    assert!(unknown_json.get("windows_security").is_some());
    assert_roundtrip(&unknown_streams);
    assert_roundtrip(&empty_streams);

    let mut building = full();
    building = building.with_windows_security(None).unwrap();
    assert_roundtrip(&building);
    assert_ne!(building.digest, full().digest);
    assert_eq!(
        building.with_windows_security(Some(observation())).unwrap(),
        full()
    );
}

#[test]
fn windows_absent_null_empty_and_control_have_distinct_digests() {
    let states = [
        WindowsDaclState::Absent,
        WindowsDaclState::Null,
        WindowsDaclState::Acl {
            revision: 2,
            aces: vec![],
        },
    ];
    let mut digests = HashSet::new();
    for state in states {
        let manifest = windows_manifest(dacl(state), observation());
        assert!(digests.insert(manifest.digest));
        assert_roundtrip(&manifest);
    }
    let mut protected = dacl(WindowsDaclState::Null);
    protected.control |= 0x1000;
    let protected = windows_manifest(protected, observation());
    assert!(digests.insert(protected.digest));
    assert_roundtrip(&protected);
    assert_eq!(digests.len(), 4);
}

#[test]
fn ace_order_kind_flags_mask_sid_and_opaque_bytes_each_bind_digest() {
    let original = full();
    for field in [
        "order",
        "kind",
        "flags",
        "mask",
        "sid",
        "opaque_type",
        "opaque_flags",
        "opaque_data",
    ] {
        let mut aces = entries();
        match field {
            "order" => aces.swap(0, 1),
            "kind" => {
                aces[0] = WindowsAce::Deny {
                    flags: 0,
                    mask: 0x120089,
                    sid: sid(5, &[21, 1, 2, 3, 1000]),
                }
            }
            "flags" => {
                if let WindowsAce::Allow { flags, .. } = &mut aces[0] {
                    *flags = 0x10
                }
            }
            "mask" => {
                if let WindowsAce::Allow { mask, .. } = &mut aces[0] {
                    *mask ^= 1
                }
            }
            "sid" => {
                if let WindowsAce::Allow { sid, .. } = &mut aces[0] {
                    sid.sub_authorities[4] += 1
                }
            }
            "opaque_type" => {
                if let WindowsAce::Opaque { ace_type, .. } = &mut aces[2] {
                    *ace_type = 0xfd
                }
            }
            "opaque_flags" => {
                if let WindowsAce::Opaque { flags, .. } = &mut aces[2] {
                    *flags = 0
                }
            }
            "opaque_data" => {
                if let WindowsAce::Opaque { data, .. } = &mut aces[2] {
                    data[0] ^= 1
                }
            }
            _ => unreachable!(),
        }
        let changed = windows_manifest(
            dacl(WindowsDaclState::Acl { revision: 2, aces }),
            observation(),
        );
        assert_ne!(original.digest, changed.digest, "field {field}");
        assert_roundtrip(&changed);
    }
}

#[test]
fn namespace_sid_owner_group_and_numeric_owner_remain_independent_digest_inputs() {
    let original = full();
    for field in [
        "namespace",
        "owner",
        "group",
        "owner_missing",
        "group_missing",
        "swap",
        "authority",
        "uid",
    ] {
        let mut observed = observation();
        let mut acl = dacl(WindowsDaclState::Acl {
            revision: 2,
            aces: entries(),
        });
        let mut owner = None;
        match field {
            "namespace" => {
                observed.namespace[0] ^= 1;
                acl.namespace = observed.namespace;
            }
            "owner" => observed.owner.as_mut().unwrap().sub_authorities[4] += 1,
            "group" => observed.group.as_mut().unwrap().sub_authorities[1] += 1,
            "owner_missing" => observed.owner = None,
            "group_missing" => observed.group = None,
            "swap" => std::mem::swap(&mut observed.owner, &mut observed.group),
            "authority" => observed.owner.as_mut().unwrap().authority += 1_u64 << 32,
            "uid" => {
                owner = Some(Owner {
                    uid: 1000,
                    gid: 100,
                })
            }
            _ => unreachable!(),
        }
        let changed = MetadataManifest::new(
            owner,
            0o640,
            Some(Acl::WindowsDacl(acl)),
            vec![],
            None,
            None,
            None,
        )
        .unwrap()
        .with_windows_security(Some(observed))
        .unwrap();
        assert_ne!(original.digest, changed.digest, "field {field}");
        assert_roundtrip(&changed);
    }
}

#[test]
fn read_issues_and_invalid_descriptor_bytes_are_digest_bound() {
    let observations = [
        issue(WindowsSecurityReadIssue::ReadControlDenied, None),
        issue(WindowsSecurityReadIssue::QueryUnsupported, None),
        issue(
            WindowsSecurityReadIssue::InvalidDescriptor,
            Some(b"first invalid descriptor"),
        ),
        issue(
            WindowsSecurityReadIssue::InvalidDescriptor,
            Some(b"second invalid descriptor"),
        ),
    ];
    let mut digests = HashSet::new();
    for observed in observations {
        let manifest = basic()
            .with_windows_security(Some(observed.clone()))
            .unwrap();
        assert!(digests.insert(manifest.digest));
        assert_eq!(manifest.acl, None);
        assert_eq!(manifest.windows_security.as_ref().unwrap(), &observed);
        assert_roundtrip(&manifest);
    }
    assert_eq!(digests.len(), 4);
}

#[test]
fn invalid_security_observations_are_rejected_after_json_and_before_digest_rebinding() {
    let denied = issue(WindowsSecurityReadIssue::ReadControlDenied, None);
    for field in [
        "namespace",
        "owner",
        "group",
        "invalid_without_digest",
        "digest_without_invalid",
        "unsupported_with_digest",
    ] {
        let mut invalid = denied.clone();
        match field {
            "namespace" => invalid.namespace = [0; 16],
            "owner" => invalid.owner = observation().owner,
            "group" => invalid.group = observation().group,
            "invalid_without_digest" => {
                invalid.issue = Some(WindowsSecurityReadIssue::InvalidDescriptor)
            }
            "digest_without_invalid" => {
                invalid.issue = None;
                invalid.invalid_descriptor_digest = Some([1; 32]);
            }
            "unsupported_with_digest" => {
                invalid.issue = Some(WindowsSecurityReadIssue::QueryUnsupported);
                invalid.invalid_descriptor_digest = Some([1; 32]);
            }
            _ => unreachable!(),
        }
        assert!(invalid.validate().is_err(), "field {field}");
        assert!(
            basic()
                .with_windows_security(Some(invalid.clone()))
                .is_err(),
            "field {field}"
        );
        let mut encoded =
            serde_json::to_value(basic().with_windows_security(Some(denied.clone())).unwrap())
                .unwrap();
        encoded["windows_security"] = serde_json::to_value(&invalid).unwrap();
        let decoded: MetadataManifest = serde_json::from_value(encoded).unwrap();
        assert!(decoded.validate().is_err(), "field {field}");
        assert!(decoded.with_hardlink_group(None).is_err(), "field {field}");
    }
}

#[test]
fn nested_sid_and_dacl_tampering_is_rejected_by_deserialization_or_constructor() {
    let original = full();
    for field in [
        "sid_revision",
        "sid_authority",
        "sid_count",
        "dacl_namespace",
        "dacl_control",
        "dacl_present",
        "opaque_length",
    ] {
        let mut encoded = serde_json::to_value(&original).unwrap();
        match field {
            "sid_revision" => {
                encoded["windows_security"]["owner"]["revision"] = serde_json::json!(2)
            }
            "sid_authority" => {
                encoded["windows_security"]["group"]["authority"] = serde_json::json!(1_u64 << 48)
            }
            "sid_count" => {
                encoded["windows_security"]["owner"]["sub_authorities"] =
                    serde_json::json!(vec![1; 16])
            }
            "dacl_namespace" => {
                encoded["acl"]["value"]["namespace"] = serde_json::json!(vec![0; 16])
            }
            "dacl_control" => encoded["acl"]["value"]["control"] = serde_json::json!(0x8004),
            "dacl_present" => encoded["acl"]["value"]["control"] = serde_json::json!(0),
            "opaque_length" => {
                encoded["acl"]["value"]["state"]["aces"][2]["data"] = serde_json::json!([1])
            }
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_value::<MetadataManifest>(encoded).is_err(),
            "field {field}"
        );
    }
    let mut invalid = observation();
    invalid.owner.as_mut().unwrap().revision = 2;
    assert!(invalid.validate().is_err());
    assert!(full().with_windows_security(Some(invalid)).is_err());
}

#[test]
fn observed_windows_acl_requires_matching_successful_observation_and_compatible_model() {
    let complete = observation();
    assert!(basic()
        .with_windows_security(Some(complete.clone()))
        .is_err());
    let incompatible = MetadataManifest::new(
        None,
        0o640,
        Some(Acl::Nfs4(Nfs4Acl {
            entries: vec![],
            flags: 0,
        })),
        vec![],
        None,
        None,
        None,
    )
    .unwrap();
    assert!(incompatible
        .clone()
        .with_windows_security(Some(complete))
        .is_err());
    assert!(incompatible
        .with_windows_security(Some(issue(
            WindowsSecurityReadIssue::ReadControlDenied,
            None
        )))
        .is_err());
    let mut different_namespace = observation();
    different_namespace.namespace[0] ^= 1;
    assert!(full()
        .with_windows_security(Some(different_namespace))
        .is_err());
    for read_issue in [
        WindowsSecurityReadIssue::ReadControlDenied,
        WindowsSecurityReadIssue::QueryUnsupported,
        WindowsSecurityReadIssue::InvalidDescriptor,
    ] {
        let raw = if read_issue == WindowsSecurityReadIssue::InvalidDescriptor {
            Some(b"invalid".as_slice())
        } else {
            None
        };
        assert!(full()
            .with_windows_security(Some(issue(read_issue, raw)))
            .is_err());
    }
    let building = full().with_windows_security(None).unwrap();
    assert_roundtrip(&building);
}

#[test]
fn explicit_acl_read_only_capability_roundtrips_and_legacy_missing_stays_omitted_unknown() {
    const LEGACY: &str = concat!(
        "{\"owner\":\"Unknown\",\"special_mode_bits\":\"Unknown\",\"acl_model\":\"Unknown\",",
        "\"xattrs\":\"Unknown\",\"resource_fork\":\"Unknown\",\"sparse_extents\":\"Unknown\",\"hardlinks\":\"Unknown\"}"
    );
    let legacy: MetadataCapabilities = serde_json::from_str(LEGACY).unwrap();
    assert_eq!(legacy.acl, Capability::Unknown);
    assert_eq!(legacy.named_streams, Capability::Unknown);
    assert_eq!(serde_json::to_string(&legacy).unwrap(), LEGACY);
    assert_eq!(MetadataCapabilities::unknown(), legacy);
    let explicit = MetadataCapabilities {
        acl_model: AclModel::WindowsSid,
        acl: Capability::ReadOnly,
        ..legacy
    };
    let encoded = serde_json::to_string(&explicit).unwrap();
    assert!(encoded.contains("\"acl_model\":\"WindowsSid\",\"acl\":\"ReadOnly\""));
    assert!(!encoded.contains("named_streams"));
    assert_eq!(
        serde_json::from_str::<MetadataCapabilities>(&encoded).unwrap(),
        explicit
    );
}
