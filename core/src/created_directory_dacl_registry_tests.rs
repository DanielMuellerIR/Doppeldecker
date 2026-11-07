use super::*;
use crate::created_directory_dacl::*;
use crate::file_dacl::tests::{context, fixture};
fn identity(path: &str) -> crate::RootIdentity {
    let mut identity = crate::file_dacl::tests::identity(path);
    let crate::RootIdentity::Smb(root) = &mut identity else {
        panic!()
    };
    root.root_file_id = match path {
        "/" => 100,
        "/pane" | "/alias" => 1,
        "/pane/child" | "/alias-child" => 4,
        "/independent" => 2,
        "/pane-sibling" => 3,
        _ => 9,
    };
    identity
}
fn scope(identity: &crate::RootIdentity) -> CreatedDirectoryRootScope {
    let crate::RootIdentity::Smb(root) = identity else {
        panic!()
    };
    CreatedDirectoryRootScope {
        object_namespace: object_namespace(identity).unwrap(),
        root_file_id: root.root_file_id,
        ancestor_file_ids: match root.virtual_root.as_str() {
            "/" => vec![],
            "/pane/child" | "/alias-child" => vec![100, 1],
            _ => vec![100],
        },
    }
}
fn observed_check(
    config: &RootHandle,
    identities: &[crate::RootIdentity],
    owner: Option<&StagingOwner>,
) -> io::Result<Option<[u8; 32]>> {
    check_registry_observed(
        config,
        identities,
        owner,
        |index| Ok(Some(scope(&identities[index]))),
        |root| Ok(Some(scope(root))),
    )
}

use crate::windows_security::{WindowsAce, WindowsDaclState};
fn effect() -> CreatedDirectoryDaclPlan {
    let mut before = crate::file_dacl::tests::acl(false);
    before.control = 4;
    let mut after = crate::file_dacl::tests::acl(true);
    let WindowsDaclState::Acl { aces, .. } = &mut after.state else {
        panic!()
    };
    let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
        panic!()
    };
    *mask = 0x0016_00a1;
    CreatedDirectoryDaclPlan {
        origin: CreatedDirectoryOrigin::ExclusiveMinimalCreate,
        file_id: 42,
        object_namespace: object_namespace(&identity("/pane")).unwrap(),
        before,
        after,
        context: context(),
    }
}
fn applied(owner: &StagingOwner) -> Intent {
    let i = Intent::create(
        owner,
        Path::new("/pane/new"),
        &effect(),
        &identity("/pane"),
        [2; 32],
        &scope(&identity("/pane")),
    )
    .unwrap();
    i.phase(Phase::Applying).unwrap();
    i.phase(Phase::Applied).unwrap();
    i
}
#[test]
fn v7_before_is_observed_unprotected_and_is_not_a_v5_write_form() {
    effect().validate().unwrap();
    assert!(crate::directory_dacl::DirectoryDaclPlan {
        before: effect().before,
        after: effect().after,
        context: context(),
        target_proof: Some(crate::file_dacl::tests::proof("new", 42))
    }
    .validate()
    .is_err());
    assert_ne!(
        dacl_digest(&effect().after).unwrap(),
        crate::directory_dacl::dacl_digest(&effect().after).unwrap()
    );
}
#[test]
fn v7_direct_finish_keeps_proof_until_exact_origin_object_checkpoint() {
    let (_area, owner, config) = fixture();
    let i = applied(&owner);
    i.finish(true).unwrap();
    assert_eq!(
        recover_queue(&owner.queue, &config).unwrap()[0]
            .created_directory_dacl_report
            .completed
            .len(),
        1
    );
    let mut wrong = i.report();
    wrong.completed[0].file_id += 1;
    checkpoint(&owner, &config, &wrong, &[], false).unwrap();
    assert_eq!(
        recover_queue(&owner.queue, &config).unwrap()[0]
            .created_directory_dacl_report
            .completed
            .len(),
        1
    );
    checkpoint(&owner, &config, &i.report(), &[], false).unwrap();
    assert!(recover_queue(&owner.queue, &config).unwrap().is_empty());
    assert!(observed_check(&config, &[identity("/pane")], Some(&owner))
        .unwrap()
        .is_none());
    assert!(observed_check(&config, &[identity("/pane")], None)
        .unwrap()
        .is_some());
}
#[test]
fn v7_root_scopes_keep_independent_roots_available_and_overlaps_blocked() {
    let (_area, owner, config) = fixture();
    let i = applied(&owner);
    for root in ["/independent", "/pane-sibling"] {
        assert!(observed_check(&config, &[identity(root)], None)
            .unwrap()
            .is_none());
    }
    for root in ["/pane", "/pane/child", "/", "/alias", "/alias-child"] {
        assert!(observed_check(&config, &[identity(root)], None).is_err());
    }
    checkpoint(&owner, &config, &i.report(), &[], false).unwrap();
    i.finish(true).unwrap();
    assert!(observed_check(&config, &[identity("/independent")], None)
        .unwrap()
        .is_none());
    assert!(observed_check(&config, &[identity("/pane")], None)
        .unwrap()
        .is_some());
}
#[test]
fn v7_many_checkpointed_objects_leave_bounded_hot_state_and_foreign_aba() {
    let (_area, mut owner, config) = fixture();
    for index in 0..40 {
        owner.operation = format!("{index:032x}");
        let i = applied(&owner);
        checkpoint(&owner, &config, &i.report(), &[], false).unwrap();
        i.finish(true).unwrap();
        assert!(std::fs::metadata(config.path().join(LEDGER)).unwrap().len() < 2048);
    }
    assert!(recover_queue(&owner.queue, &config).unwrap().is_empty());
    assert!(observed_check(&config, &[identity("/pane")], Some(&owner))
        .unwrap()
        .is_some());
}
#[test]
fn v7_missing_closed_proof_and_private_namespace_never_become_empty() {
    use std::os::unix::fs::PermissionsExt;
    let (_area, owner, config) = fixture();
    let i = applied(&owner);
    let name = i.path().file_name().unwrap().to_str().unwrap().to_owned();
    i.finish(true).unwrap();
    std::fs::remove_file(closed_path(&config, &owner, &name, StoreKind::V7).unwrap()).unwrap();
    assert!(recover_queue(&owner.queue, &config).is_err());
    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(crate::local_staging::recover_queue_beneath(&owner.queue, &config).is_err());
}
#[test]
fn v7_archive_and_retirement_crashes_preserve_or_retire_only_checkpointed_proofs() {
    for stage in [
        "after_archive",
        "after_hot_commit",
        "after_owner",
        "after_cleanup",
    ] {
        let (_area, owner, config) = fixture();
        let i = applied(&owner);
        TEST_FAULT.with(|f| f.set(Some(stage)));
        assert!(i.finish(true).is_err(), "{stage}");
        TEST_FAULT.with(|f| f.set(None));
        drop(i);
        let recovered = recover_queue(&owner.queue, &config).unwrap();
        assert_eq!(
            recovered[0].created_directory_dacl_report.completed.len(),
            1,
            "{stage}"
        );
    }
    for stage in ["after_retire_commit", "after_owner", "after_cleanup"] {
        let (_area, owner, config) = fixture();
        let i = applied(&owner);
        i.finish(true).unwrap();
        TEST_FAULT.with(|f| f.set(Some(stage)));
        assert!(
            checkpoint(&owner, &config, &i.report(), &[], false).is_err(),
            "{stage}"
        );
        TEST_FAULT.with(|f| f.set(None));
        assert!(
            recover_queue(&owner.queue, &config).unwrap().is_empty(),
            "{stage}"
        );
    }
}

#[test]
fn v7_missing_lineage_never_claims_disjointness_for_an_alias_pair() {
    let (_area, owner, config) = fixture();
    let _i = applied(&owner);
    assert!(check_registry(&config, &[identity("/alias")], None).is_err());
    assert!(check_registry(&config, &[identity("/independent")], None).is_err());
    assert!(observed_check(&config, &[identity("/independent")], None)
        .unwrap()
        .is_none());
}

#[test]
fn v7_root_moved_after_crash_cannot_hide_under_previously_independent_root() {
    let (_area, owner, config) = fixture();
    let i = applied(&owner);
    drop(i);
    let selected = [identity("/independent")];
    assert!(check_registry_observed(
        &config,
        &selected,
        None,
        |index| Ok(Some(scope(&selected[index]))),
        |_| Ok(None)
    )
    .is_err());
    assert!(check_registry_observed(
        &config,
        &selected,
        None,
        |index| Ok(Some(scope(&selected[index]))),
        |root| {
            let mut moved = scope(root);
            moved.ancestor_file_ids = vec![100, 2];
            Ok(Some(moved))
        }
    )
    .is_err());
    assert!(observed_check(&config, &selected, None).unwrap().is_none());
}
#[test]
fn v7_closed_epoch_does_not_require_retired_root_to_remain_reachable() {
    let (_area, owner, config) = fixture();
    let i = applied(&owner);
    checkpoint(&owner, &config, &i.report(), &[], false).unwrap();
    i.finish(true).unwrap();
    let selected = [identity("/independent")];
    assert!(check_registry_observed(
        &config,
        &selected,
        None,
        |index| Ok(Some(scope(&selected[index]))),
        |_| panic!("Geschlossene Historie darf keinen Foreign-Root-Read erzwingen")
    )
    .unwrap()
    .is_none());
}

#[test]
fn v7_first_created_checkpoint_without_any_intent_accepts_only_empty_inventory() {
    let (_area, owner, config) = fixture();
    let mut report = CreatedDirectoryDaclReport::default();
    report.observe_created(CreatedDirectoryObservation {
        effect_key: CreatedDirectoryEffectKey {
            origin: CreatedDirectoryOrigin::ExclusiveMinimalCreate,
            directory: "/pane/new".into(),
            file_id: 42,
            object_namespace: effect().object_namespace,
        },
        ordinary_metadata_applied: false,
    });
    checkpoint(&owner, &config, &report, &[], false).unwrap();
    assert!(!config.path().join(LEDGER).exists());
    assert!(!config.path().join(OWNERS).exists());
    assert!(!config.path().join(CLOSED).exists());
    assert!(recover_queue(&owner.queue, &config).unwrap().is_empty());
    let i = applied(&owner);
    i.finish(true).unwrap();
    let name = i.path().file_name().unwrap().to_str().unwrap();
    std::fs::remove_file(closed_path(&config, &owner, name, StoreKind::V7).unwrap()).unwrap();
    assert!(checkpoint(&owner, &config, &report, &[], false).is_err());
}
#[test]
fn v7_normal_remote_intent_finish_remains_non_idempotent() {
    let (_area, owner, _config) = fixture();
    let intent = crate::local_staging::RemoteTempIntent::create_bound_with_root(
        &owner,
        Path::new("/pane/new"),
        Path::new("/pane/new"),
        &identity("/pane"),
        [2; 32],
    )
    .unwrap();
    intent.finish().unwrap();
    assert_eq!(intent.finish().unwrap_err().kind(), io::ErrorKind::NotFound);
}

#[test]
fn v7_guard_never_attributes_foreign_rights_to_the_blocked_window() {
    let (_area, owner, config) = fixture();
    let _i = applied(&owner);
    let error = observed_check(&config, &[identity("/pane")], None).unwrap_err();
    assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
    assert!(!crate::error::recovery_paths(&error).is_empty());
    assert!(crate::error::created_directory_dacl_report(&error).is_empty());
}

#[test]
fn v7_store_codec_domains_and_paths_remain_golden() {
    let owner = StagingOwner {
        queue: "/private/config/operations-queue.json".into(),
        operation: "v7-golden-operation".into(),
    };
    let root = identity("/pane");
    let record = Record {
        version: 7,
        owner: owner.clone(),
        endpoint: crate::EndpointBinding::remote(&root, [2; 32]).unwrap(),
        root: root.clone(),
        scope: scope(&root),
        file: "/pane/new".into(),
        plan: effect(),
        phase: Phase::Prepared,
        closed: false,
        generation: 0,
        closed_observation: None,
    };
    let mut ledger = Ledger {
        format: 1,
        generation: 1,
        ..Default::default()
    };
    ledger
        .records
        .insert(format!("{PREFIX}golden.json"), record.clone());
    ledger
        .namespaces
        .insert(scope_key(&root, &scope(&root)).unwrap(), 1);
    let state = OwnerState::new(&owner, StoreKind::V7);
    let epoch = digest(
        b"doppeldecker-created-directory-dacl-registry-epoch-v7\0",
        &ledger.namespaces,
    )
    .unwrap();
    let actual = [
        blake3::hash(&serde_json::to_vec(&record).unwrap())
            .to_hex()
            .to_string(),
        blake3::Hash::from_bytes(ledger_digest(&ledger).unwrap())
            .to_hex()
            .to_string(),
        blake3::Hash::from_bytes(owner_digest(&state).unwrap())
            .to_hex()
            .to_string(),
        blake3::Hash::from_bytes(closed_digest(&format!("{PREFIX}golden.json"), &record).unwrap())
            .to_hex()
            .to_string(),
        blake3::Hash::from_bytes(epoch).to_hex().to_string(),
        owner_relative(&owner, StoreKind::V7)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        blake3::Hash::from_bytes(object_namespace(&root).unwrap())
            .to_hex()
            .to_string(),
        blake3::Hash::from_bytes(dacl_digest(&effect().after).unwrap())
            .to_hex()
            .to_string(),
    ];
    assert_eq!(actual, [
        "0fe69fb2bf1667ce75109e9d830efc7b32ded54358d8f648245cf155df0f35ae",
        "e5678d1725d93731b45126cf162393ff646c942f76e6852ba85268279aa990eb",
        "479af10b9ca1a99b20e4fd777ce2d8105ea31c752a58fde4e4b45d6981e818c4",
        "d6c93d2dc7f5f3aa9918f6fbccbc60ca5b78ecc928a8c8bbbbd4ff23c683a6e3",
        "75787500def203c68422a4d136fb8935b0e6ea437ba296258d7cb42c5b360323",
        "65b0c1b9f21f16af29131c2c17ca5d1b817bc0fcf6d7e390a09dfa1c5d358deb/9d8e676d21fddb2092f655bde7bd6de39b617bb062a2cfa1e091ed6c8b10243d",
        "8d8af62205b66aaece5b1d39756a0c288c7cbc196bc0dd83a93c5a26f8a42629",
        "b46f1267de38451ed1b68af343f7122454d966998139364a0d614b8872b1c7c8",
    ]);
    assert_eq!(StoreKind::V7.prefix(), ".created-directory-dacl-v7-");
    assert_eq!(
        StoreKind::V7.ledger(),
        ".created-directory-dacl-v7-ledger.json"
    );
    assert_eq!(StoreKind::V7.owners(), ".created-directory-dacl-v7-owners");
    assert_eq!(StoreKind::V7.closed(), ".created-directory-dacl-v7-closed");
    assert_eq!(
        StoreKind::V7.write_prefix(),
        ".created-directory-dacl-v7-write-"
    );
    let mut unexpected = serde_json::to_value(&ledger).unwrap();
    unexpected["kind"] = serde_json::json!("V8");
    assert!(serde_json::from_value::<Ledger>(unexpected).is_err());
}

fn kind_effect(kind: StoreKind) -> CreatedDirectoryDaclPlan {
    let mut plan = effect();
    plan.origin = kind.origin();
    if kind == StoreKind::V8 {
        let WindowsDaclState::Acl { aces, .. } = &mut plan.after.state else {
            panic!()
        };
        let WindowsAce::Allow { mask, .. } = &mut aces[0] else {
            panic!()
        };
        *mask = 0x0016_01e7;
    }
    plan
}
fn applied_kind(owner: &StagingOwner, kind: StoreKind) -> Intent {
    let intent = Intent::create(
        owner,
        Path::new("/pane/new"),
        &kind_effect(kind),
        &identity("/pane"),
        [2; 32],
        &scope(&identity("/pane")),
    )
    .unwrap();
    intent.phase(Phase::Applying).unwrap();
    intent.phase(Phase::Applied).unwrap();
    intent
}

#[test]
fn v8_store_separates_envelopes_and_domains_but_keeps_object_namespace() {
    let (_area, owner, config) = fixture();
    let intent = applied_kind(&owner, StoreKind::V8);
    let record = intent.record.lock().unwrap().clone();
    assert_eq!(record.version, 8);
    assert_eq!(record.plan.origin, StoreKind::V8.origin());
    assert_eq!(record.plan.object_namespace, effect().object_namespace);
    assert_ne!(
        record.plan.outcome("/pane/new").unwrap().after_digest,
        dacl_digest(&record.plan.after).unwrap()
    );
    assert!(intent
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with(StoreKind::V8.prefix()));
    assert!(config.path().join(StoreKind::V8.ledger()).is_file());
    assert!(!config.path().join(LEDGER).exists());
    let envelope: serde_json::Value =
        read_json(&config, &config.path().join(StoreKind::V8.ledger())).unwrap();
    assert_eq!(envelope["version"], 8);
    assert!(envelope["ledger"].get("kind").is_none());
    let mut wrong = record;
    wrong.version = 7;
    assert!(wrong.validate(&config).is_err());
    assert!(has_open(&owner, &config).unwrap());
    assert!(observed_check(&config, &[identity("/alias")], None).is_err());
    assert!(observed_check(&config, &[identity("/independent")], None)
        .unwrap()
        .is_none());
}

#[test]
fn v7_and_v8_reject_each_others_open_claim_for_the_same_object() {
    for first in StoreKind::ALL {
        let (_area, owner, _config) = fixture();
        let _first = applied_kind(&owner, first);
        let second = if first == StoreKind::V7 {
            StoreKind::V8
        } else {
            StoreKind::V7
        };
        let error = Intent::create(
            &owner,
            Path::new("/pane/alias-new"),
            &kind_effect(second),
            &identity("/pane"),
            [2; 32],
            &scope(&identity("/pane")),
        )
        .err()
        .unwrap();
        assert_eq!(crate::error_code(&error), crate::ErrorCode::OutcomeUnclear);
    }
}

#[test]
fn v7_epoch_without_visible_v8_contributions_remains_golden() {
    let (_area, owner, config) = fixture();
    let intent = applied(&owner);
    checkpoint(&owner, &config, &intent.report(), &[], false).unwrap();
    intent.finish(true).unwrap();
    let expected =
        blake3::Hash::from_hex("75787500def203c68422a4d136fb8935b0e6ea437ba296258d7cb42c5b360323")
            .unwrap();
    assert_eq!(
        observed_check(&config, &[identity("/pane")], None).unwrap(),
        Some(*expected.as_bytes())
    );
    assert!(observed_check(&config, &[identity("/pane")], Some(&owner))
        .unwrap()
        .is_none());
    let mut own_v8 = kind_effect(StoreKind::V8);
    own_v8.file_id = 43;
    let other = Intent::create(
        &owner,
        Path::new("/pane/other"),
        &own_v8,
        &identity("/pane"),
        [2; 32],
        &scope(&identity("/pane")),
    )
    .unwrap();
    other.phase(Phase::Applying).unwrap();
    other.phase(Phase::Applied).unwrap();
    checkpoint(&owner, &config, &other.report(), &[], false).unwrap();
    other.finish(true).unwrap();
    assert!(observed_check(&config, &[identity("/pane")], Some(&owner))
        .unwrap()
        .is_none());
    assert_ne!(
        observed_check(&config, &[identity("/pane")], None).unwrap(),
        Some(*expected.as_bytes())
    );
}

#[test]
fn v8_archive_and_retirement_crashes_keep_checkpointed_proofs() {
    for stage in [
        "after_archive",
        "after_hot_commit",
        "after_owner",
        "after_cleanup",
    ] {
        let (_area, owner, config) = fixture();
        let intent = applied_kind(&owner, StoreKind::V8);
        TEST_FAULT.with(|fault| fault.set(Some(stage)));
        assert!(intent.finish(true).is_err(), "{stage}");
        TEST_FAULT.with(|fault| fault.set(None));
        drop(intent);
        let recovered = recover_queue(&owner.queue, &config).unwrap();
        assert_eq!(
            recovered[0].created_directory_dacl_report.completed.len(),
            1,
            "{stage}"
        );
        assert_eq!(
            recovered[0].created_directory_dacl_report.completed[0].origin,
            StoreKind::V8.origin()
        );
    }
    for stage in ["after_retire_commit", "after_owner", "after_cleanup"] {
        let (_area, owner, config) = fixture();
        let intent = applied_kind(&owner, StoreKind::V8);
        intent.finish(true).unwrap();
        TEST_FAULT.with(|fault| fault.set(Some(stage)));
        assert!(
            checkpoint(&owner, &config, &intent.report(), &[], false).is_err(),
            "{stage}"
        );
        TEST_FAULT.with(|fault| fault.set(None));
        assert!(
            recover_queue(&owner.queue, &config).unwrap().is_empty(),
            "{stage}"
        );
    }
}

#[test]
fn v7_persisted_generations_keep_the_checkpoint_finish_sequence() {
    let (_area, owner, config) = fixture();
    let generation = || {
        let _lock = registry_lock(&config).unwrap();
        load_ledger(&config, None, StoreKind::V7)
            .unwrap()
            .generation
    };
    let intent = Intent::create(
        &owner,
        Path::new("/pane/new"),
        &effect(),
        &identity("/pane"),
        [2; 32],
        &scope(&identity("/pane")),
    )
    .unwrap();
    assert_eq!(
        (intent.record.lock().unwrap().generation, generation()),
        (0, 2)
    );
    intent.phase(Phase::Applying).unwrap();
    assert_eq!(
        (intent.record.lock().unwrap().generation, generation()),
        (1, 3)
    );
    intent.phase(Phase::Applied).unwrap();
    assert_eq!(
        (intent.record.lock().unwrap().generation, generation()),
        (2, 4)
    );
    checkpoint(&owner, &config, &intent.report(), &[], false).unwrap();
    assert_eq!(generation(), 5);
    intent.finish(true).unwrap();
    assert_eq!(
        (intent.record.lock().unwrap().generation, generation()),
        (3, 7)
    );
    assert!(!has_open(&owner, &config).unwrap());
    assert_eq!(generation(), 7);
    assert!(!config.path().join(StoreKind::V8.ledger()).exists());
}
