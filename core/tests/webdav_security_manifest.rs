//! DAV-Werte bleiben Beobachtungen; HTTP-, Ressourcen- und Property-Status sind getrennt.

use doppeldecker_core::metadata::{
    Acl, BlobSummary, MetadataManifest, NamedBlobSummary, Owner, PosixAcl,
    WindowsSecurityObservation, WindowsSecurityReadIssue,
};
use doppeldecker_core::windows_security::{WindowsDacl, WindowsDaclState};
use doppeldecker_core::{WebDavPropertyObservation as Property, WebDavSecurityObservation};
use std::collections::HashSet;

// Unabhängig aus der V4-Bytekodierung von 77ff2194 berechnet, vor der V5-Erweiterung.
const V4_DENIED: &str = r#"{"owner":null,"mode":416,"acl":null,"windows_security":{"namespace":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],"owner":null,"group":null,"issue":"ReadControlDenied","invalid_descriptor_digest":null},"xattrs":[],"resource_fork":null,"sparse_extents":null,"hardlink_group":null,"digest":[32,8,143,28,7,207,105,118,139,151,202,22,253,198,224,108,112,232,20,156,89,68,217,4,209,133,213,181,127,161,230,110]}"#;

fn base() -> MetadataManifest {
    MetadataManifest::new(None, 0o640, None, vec![], None, None, None).unwrap()
}

fn observation() -> WebDavSecurityObservation {
    WebDavSecurityObservation {
        namespace: [7; 32],
        query_status: 207,
        resource_status: None,
        acl: Property::Missing,
        owner: Property::Missing,
    }
}

fn manifest(observed: WebDavSecurityObservation) -> MetadataManifest {
    base().with_webdav_security(Some(observed)).unwrap()
}

fn windows_observation() -> WindowsSecurityObservation {
    WindowsSecurityObservation {
        namespace: [7; 16],
        owner: None,
        group: None,
        issue: Some(WindowsSecurityReadIssue::ReadControlDenied),
        invalid_descriptor_digest: None,
    }
}

#[test]
fn absent_webdav_preserves_v4_golden_json_and_digest() {
    let old: MetadataManifest = serde_json::from_str(V4_DENIED).unwrap();
    old.validate().unwrap();
    assert_eq!(old.webdav_security, None);
    assert_eq!(serde_json::to_string(&old).unwrap(), V4_DENIED);
    let rebuilt = base()
        .with_windows_security(Some(windows_observation()))
        .unwrap();
    assert_eq!(rebuilt, old);
    assert_eq!(old.clone().with_webdav_security(None).unwrap(), old);
    assert_eq!(
        base()
            .with_webdav_security(Some(observation()))
            .unwrap()
            .with_webdav_security(None)
            .unwrap(),
        base()
    );
}

#[test]
fn v5_binds_status_layers_namespace_values_empty_and_stream_presence() {
    let mut observations = vec![observation()];
    for query_status in [403, 404, 405, 501] {
        observations.push(WebDavSecurityObservation {
            query_status,
            ..observation()
        });
    }
    for resource_status in [200, 201, 403, 404, 500] {
        observations.push(WebDavSecurityObservation {
            resource_status: Some(resource_status),
            ..observation()
        });
    }
    for status in [100, 201, 207, 403, 404, 500, 599] {
        observations.push(WebDavSecurityObservation {
            acl: Property::Unavailable { status },
            ..observation()
        });
    }
    for (empty, digest) in [(false, [3; 32]), (true, [3; 32]), (false, [4; 32])] {
        observations.push(WebDavSecurityObservation {
            acl: Property::Observed { empty, digest },
            ..observation()
        });
    }
    observations.push(WebDavSecurityObservation {
        owner: Property::Observed {
            empty: false,
            digest: [3; 32],
        },
        ..observation()
    });
    observations.push(WebDavSecurityObservation {
        namespace: [8; 32],
        ..observation()
    });
    let mut digests = HashSet::from([base().digest]);
    for observed in observations {
        let manifest = manifest(observed);
        manifest.validate().unwrap();
        assert!(digests.insert(manifest.digest));
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(json.contains("webdav_security"));
        let decoded: MetadataManifest = serde_json::from_str(&json).unwrap();
        decoded.validate().unwrap();
        assert_eq!(manifest, decoded);
    }
    let unobserved_streams = manifest(observation());
    let empty_streams = unobserved_streams
        .clone()
        .with_named_streams(Some(vec![]))
        .unwrap();
    assert!(digests.insert(empty_streams.digest));
    assert_eq!(
        empty_streams.with_named_streams(None).unwrap(),
        unobserved_streams
    );
}

#[test]
fn v5_detects_deserialized_status_and_value_drift() {
    let original = manifest(WebDavSecurityObservation {
        acl: Property::Observed {
            empty: false,
            digest: [4; 32],
        },
        ..observation()
    });
    let mut changed = original.clone();
    changed.webdav_security.as_mut().unwrap().namespace[0] ^= 1;
    assert!(!changed.has_valid_digest());
    assert!(changed.validate().is_err());
    changed = original.clone();
    changed.webdav_security.as_mut().unwrap().resource_status = Some(200);
    assert!(!changed.has_valid_digest());
    changed = original;
    changed.webdav_security.as_mut().unwrap().acl = Property::Observed {
        empty: true,
        digest: [4; 32],
    };
    assert!(!changed.has_valid_digest());
}

#[test]
fn status_validation_rejects_contradictory_or_invalid_observations() {
    for status in [0, 99, 200, 600, u16::MAX] {
        assert!(Property::Unavailable { status }.validate().is_err());
    }
    for query_status in [0, 200, 201, 301, 401, 500, 599, 600] {
        assert!(WebDavSecurityObservation {
            query_status,
            ..observation()
        }
        .validate()
        .is_err());
    }
    assert!(WebDavSecurityObservation {
        namespace: [0; 32],
        ..observation()
    }
    .validate()
    .is_err());
    for query_status in [403, 404, 405, 501] {
        assert!(WebDavSecurityObservation {
            query_status,
            resource_status: Some(200),
            ..observation()
        }
        .validate()
        .is_err());
        assert!(WebDavSecurityObservation {
            query_status,
            owner: Property::Unavailable { status: 404 },
            ..observation()
        }
        .validate()
        .is_err());
    }
    for status in [0, 99, 600, u16::MAX] {
        assert!(WebDavSecurityObservation {
            resource_status: Some(status),
            ..observation()
        }
        .validate()
        .is_err());
    }
    for status in [100, 301, 403, 404, 500, 599] {
        for property in [
            Property::Unavailable { status: 404 },
            Property::Observed {
                empty: true,
                digest: [0; 32],
            },
        ] {
            assert!(WebDavSecurityObservation {
                resource_status: Some(status),
                acl: property,
                ..observation()
            }
            .validate()
            .is_err());
        }
    }
}

#[test]
fn webdav_never_coexists_with_numeric_owner_or_any_acl_model() {
    let owner = MetadataManifest::new(
        Some(Owner { uid: 1, gid: 2 }),
        0o640,
        None,
        vec![],
        None,
        None,
        None,
    )
    .unwrap();
    assert!(owner.with_webdav_security(Some(observation())).is_err());
    for acl in [
        Acl::Posix(PosixAcl {
            access: vec![],
            default: vec![],
        }),
        Acl::Nfs4(doppeldecker_core::Nfs4Acl {
            entries: vec![],
            flags: 0,
        }),
        Acl::WindowsDacl(WindowsDacl {
            namespace: [7; 16],
            control: 0,
            state: WindowsDaclState::Absent,
        }),
    ] {
        let mut with_acl = manifest(observation());
        with_acl.acl = Some(acl);
        assert!(with_acl.validate().is_err());
        assert!(with_acl.with_webdav_security(Some(observation())).is_err());
    }
    assert!(base()
        .with_windows_security(Some(windows_observation()))
        .unwrap()
        .with_webdav_security(Some(observation()))
        .is_err());
    assert!(manifest(observation())
        .with_windows_security(Some(windows_observation()))
        .is_err());
}

#[test]
fn unrelated_metadata_remains_available() {
    let value = BlobSummary::from_bytes(b"value");
    let metadata = MetadataManifest::new(
        None,
        0o640,
        None,
        vec![NamedBlobSummary {
            name: b"user.example".to_vec(),
            value: value.clone(),
        }],
        Some(value.clone()),
        Some(vec![]),
        Some(4),
    )
    .unwrap()
    .with_named_streams(Some(vec![NamedBlobSummary {
        name: b"example".to_vec(),
        value,
    }]))
    .unwrap()
    .with_webdav_security(Some(observation()))
    .unwrap();
    metadata.validate().unwrap();
}

#[test]
fn reasons_preserve_layers_and_never_claim_complete_acl_or_numeric_owner() {
    let denied = WebDavSecurityObservation {
        query_status: 403,
        ..observation()
    };
    assert!(denied.acl_reason().contains("HTTP"));
    let missing = WebDavSecurityObservation {
        acl: Property::Unavailable { status: 404 },
        ..observation()
    };
    assert!(missing.acl_reason().contains("nicht bereitgestellt"));
    assert!(observation().acl_reason().contains("nicht zurückgegeben"));
    let denied = WebDavSecurityObservation {
        owner: Property::Unavailable { status: 403 },
        ..observation()
    };
    assert!(denied.owner_reason().contains("Property-Status 403"));
    let resource_missing = WebDavSecurityObservation {
        resource_status: Some(404),
        ..observation()
    };
    assert!(resource_missing
        .acl_reason()
        .contains("Ressourcenstatus 404"));
    for empty in [true, false] {
        let observed = WebDavSecurityObservation {
            acl: Property::Observed {
                empty,
                digest: [3; 32],
            },
            owner: Property::Observed {
                empty,
                digest: [4; 32],
            },
            ..observation()
        };
        assert!(observed.acl_reason().contains("nicht modelliert"));
        assert!(observed.owner_reason().contains("UID/GID"));
    }
}
