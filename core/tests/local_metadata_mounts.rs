//! Opt-in-Gegenlauf auf eigenen nativen und exFAT-Testmounts.
use doppeldecker_core::{
    execute, Capability, Compare, ExecuteRequest, LocalVfs, MetadataProfile, Safety, SyncOptions,
    Vfs,
};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

fn test_root() -> std::path::PathBuf {
    let root = std::env::var("DOPPELDECKER_METADATA_TEST_ROOT").unwrap();
    let root = Path::new(&root);
    assert!(root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("doppeldecker-metadata-e2e-"));
    assert_eq!(
        std::fs::read_to_string(root.join(".doppeldecker-metadata-test")).unwrap(),
        "doppeldecker-metadata-e2e-v1\n"
    );
    root.to_path_buf()
}

#[test]
#[cfg(debug_assertions)]
#[ignore = "benötigt eigene native/exFAT-Testmounts"]
fn local_metadata_mounts_report_matches_native_and_exfat() {
    let root_path = test_root();
    let root = root_path.as_path();
    let native = root.join("native");
    let exfat = root.join("exfat");
    assert!(native.canonicalize().unwrap().starts_with(root));
    assert!(exfat.canonicalize().unwrap().starts_with(root));
    let source = native.join("source");
    let target = exfat.join("target");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    assert!(std::fs::read_dir(&source).unwrap().next().is_none());
    assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    let unsupported = exfat.join("unsupported-probe");
    std::fs::write(&unsupported, b"Eigene exFAT-Probe").unwrap();
    assert!(std::fs::hard_link(&unsupported, exfat.join("hardlink-probe")).is_err());
    let _ = std::fs::set_permissions(&unsupported, std::fs::Permissions::from_mode(0o2640));
    assert_ne!(
        std::fs::metadata(&unsupported).unwrap().mode() & 0o7777,
        0o2640
    );
    std::fs::write(source.join("datei"), b"Metadaten-Gegenlauf\n").unwrap();
    let vfs = LocalVfs::new();
    let unsupported_manifest = vfs.metadata_manifest(&unsupported).expect("exFAT-Manifest");
    assert!(unsupported_manifest.owner.is_none());
    assert!(unsupported_manifest.sparse_extents.is_none());
    vfs.set_mtime(&unsupported, 1_700_000_000)
        .expect("exFAT-mtime");
    vfs.sync_file(&unsupported).expect("exFAT-Dateisync");
    vfs.sync_dir(&exfat).expect("exFAT-Ordnersync");
    let exclusive_rename = match vfs.rename_noreplace(&unsupported, &exfat.join("no-replace-probe"))
    {
        Ok(()) => true,
        #[cfg(target_os = "macos")]
        Err(error) if error.raw_os_error() == Some(libc::ENOTSUP) => {
            println!("exFAT: exklusiver Rename nicht unterstützt; Metadatenvorschau wird geprüft, Inhaltslauf bleibt offen");
            false
        }
        Err(error) => panic!("exFAT-exklusiver Rename: {error}"),
    };
    let caps = vfs.metadata_capabilities(&target).unwrap();
    println!(
        "exFAT-Fähigkeiten: {}",
        serde_json::to_string(&caps).unwrap()
    );
    let opts = SyncOptions {
        compare: Compare::ContentHash,
        metadata: MetadataProfile::NativeStrict,
        ..Default::default()
    };
    let plan = doppeldecker_core::plan(&vfs, &source, &vfs, &target, &opts);
    println!("NativeStrict-Vorschau: {plan:?}");
    assert!(
        plan.is_err(),
        "Nicht darstellbare exFAT-Metadaten dürfen keine strikte Vorschau erhalten"
    );
    assert_ne!(caps.owner, Capability::Exact);
    assert_ne!(caps.hardlinks, Capability::Exact);
    assert!(std::fs::read_dir(&target).unwrap().next().is_none());

    let mut best_effort = opts.clone();
    best_effort.metadata = MetadataProfile::NativeBestEffort;
    for mode in [
        doppeldecker_core::SyncMode::AddOnly,
        doppeldecker_core::SyncMode::Update,
    ] {
        best_effort.mode = mode;
        if mode == doppeldecker_core::SyncMode::Update {
            std::fs::write(source.join("datei"), b"Aktualisierter exFAT-Inhalt\n").unwrap();
        }
        let preview = doppeldecker_core::plan(&vfs, &source, &vfs, &target, &best_effort).unwrap();
        assert!(preview
            .metadata_report
            .issues
            .iter()
            .any(|issue| issue.class == doppeldecker_core::MetadataClass::Owner));
        if !exclusive_rename {
            assert!(std::fs::read_dir(&target).unwrap().next().is_none());
            break;
        }
        let stats = execute(
            ExecuteRequest {
                src: &vfs,
                src_root: &source,
                dst: &vfs,
                dst_root: &target,
                plan: &preview,
                safety: &Safety::headless_test(),
            },
            &doppeldecker_core::Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(stats.metadata_report, preview.metadata_report);
        assert_eq!(
            std::fs::read(source.join("datei")).unwrap(),
            std::fs::read(target.join("datei")).unwrap()
        );
    }

    let native_target = native.join("target");
    std::fs::create_dir_all(&native_target).unwrap();
    let native_caps = vfs.metadata_capabilities(&source).unwrap();
    println!("Native Fähigkeiten: {native_caps:?}");
    assert_eq!(native_caps.owner, Capability::Exact);
    assert_eq!(native_caps.hardlinks, Capability::Exact);
    let sparse = source.join("sparse");
    let mut file = std::fs::File::create(&sparse).unwrap();
    file.write_all(b"Anfang").unwrap();
    file.seek(SeekFrom::Start(1024 * 1024)).unwrap();
    file.write_all(b"Ende").unwrap();
    file.sync_all().unwrap();
    drop(file);
    std::fs::set_permissions(&sparse, std::fs::Permissions::from_mode(0o2640)).unwrap();
    std::fs::hard_link(&sparse, source.join("hardlink")).unwrap();
    #[cfg(target_os = "linux")]
    vfs.write_xattrs_exact(
        &sparse,
        &[doppeldecker_core::XattrValue {
            name: b"user.doppeldecker.mount".to_vec(),
            value: b"Testwert".to_vec(),
        }],
    )
    .unwrap();
    #[cfg(target_os = "macos")]
    {
        let mut fork = vfs
            .open_fork_write_new(&sparse, doppeldecker_core::ForkKind::Resource)
            .unwrap();
        fork.write_all(b"Ressourcen-Fork-Gegenlauf").unwrap();
        fork.flush().unwrap();
        drop(fork);
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(sparse.as_os_str().as_bytes()).unwrap();
        // Ein absichtlich vorhandenes xattr belegt die ReadOnly-Grenze auch auf
        // Systemen, die selbst kein com.apple.provenance ergänzen.
        assert_eq!(
            unsafe {
                libc::setxattr(
                    path.as_ptr(),
                    c"com.doppeldecker.mount".as_ptr(),
                    b"Testwert".as_ptr().cast(),
                    8,
                    0,
                    0,
                )
            },
            0
        );
    }
    let mut native_options = opts.clone();
    if native_caps.xattrs == Capability::ReadOnly {
        assert!(doppeldecker_core::plan(&vfs, &source, &vfs, &native_target, &opts).is_err());
        native_options.metadata = MetadataProfile::NativeBestEffort;
    }
    #[cfg(target_os = "linux")]
    {
        use doppeldecker_core::{Acl, PosixAcl, PosixAclEntry, PosixAclTag};
        let acl = Acl::Posix(PosixAcl {
            access: vec![
                PosixAclEntry {
                    tag: PosixAclTag::UserObject,
                    id: None,
                    permissions: 6,
                },
                PosixAclEntry {
                    tag: PosixAclTag::User,
                    id: Some(12345),
                    permissions: 4,
                },
                PosixAclEntry {
                    tag: PosixAclTag::GroupObject,
                    id: None,
                    permissions: 4,
                },
                PosixAclEntry {
                    tag: PosixAclTag::Mask,
                    id: None,
                    permissions: 4,
                },
                PosixAclEntry {
                    tag: PosixAclTag::Other,
                    id: None,
                    permissions: 0,
                },
            ],
            default: vec![],
        });
        vfs.write_acl_exact(&sparse, Some(&acl)).unwrap();
    }
    #[cfg(target_os = "macos")]
    {
        use doppeldecker_core::{Acl, Nfs4AceKind, Nfs4Acl, Nfs4AclEntry};
        unsafe extern "C" {
            fn mbr_uid_to_uuid(id: libc::uid_t, uuid: *mut u8) -> libc::c_int;
        }
        let mut qualifier = [0u8; 16];
        assert_eq!(
            unsafe { mbr_uid_to_uuid(libc::geteuid(), qualifier.as_mut_ptr()) },
            0
        );
        let acl = Acl::Nfs4(Nfs4Acl {
            entries: vec![Nfs4AclEntry {
                qualifier,
                kind: Nfs4AceKind::Allow,
                permissions: 1 << 1,
                flags: 0,
            }],
            flags: 0,
        });
        vfs.write_acl_exact(&sparse, Some(&acl)).unwrap();
    }
    // Schreiben an Daten-/Fork-/Attributströmen kann Sonderbits zurücksetzen.
    // Erst der vollständig vorbereitete Quellzustand ist die Kopierreferenz.
    std::fs::set_permissions(&sparse, std::fs::Permissions::from_mode(0o2640)).unwrap();
    let prepared_source = vfs.metadata_manifest(&sparse).unwrap();
    assert_eq!(prepared_source.mode, 0o2640);
    assert!(prepared_source.acl.is_some());
    let native_plan =
        doppeldecker_core::plan(&vfs, &source, &vfs, &native_target, &native_options).unwrap();
    assert_eq!(
        native_plan.metadata_report.strict_verified,
        native_options.metadata == MetadataProfile::NativeStrict
    );
    assert!(native_plan
        .metadata_report
        .issues
        .iter()
        .all(|issue| issue.class == doppeldecker_core::MetadataClass::Xattrs));
    assert!(std::fs::read_dir(&native_target).unwrap().next().is_none());
    let stats = execute(
        ExecuteRequest {
            src: &vfs,
            src_root: &source,
            dst: &vfs,
            dst_root: &native_target,
            plan: &native_plan,
            safety: &Safety::headless_test(),
        },
        &doppeldecker_core::Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        stats.metadata_report.issues,
        native_plan.metadata_report.issues
    );
    assert_eq!(
        std::fs::read(&sparse).unwrap(),
        std::fs::read(native_target.join("sparse")).unwrap()
    );
    let mut expected = vfs.metadata_manifest(&sparse).unwrap();
    let mut actual = vfs
        .metadata_manifest(&native_target.join("sparse"))
        .unwrap();
    if native_caps.xattrs == Capability::ReadOnly {
        expected.xattrs.clear();
        actual.xattrs.clear();
        expected = expected.with_hardlink_group(None).unwrap();
        actual = actual.with_hardlink_group(None).unwrap();
    }
    assert_eq!(expected, actual);
    assert_eq!(
        std::fs::metadata(native_target.join("sparse"))
            .unwrap()
            .ino(),
        std::fs::metadata(native_target.join("hardlink"))
            .unwrap()
            .ino()
    );
    // Jeder Aufruf bleibt auch im rooted Adapter unter dessen gehaltenem Root.
    let rooted = LocalVfs::rooted(&source).unwrap();
    assert!(rooted.metadata_capabilities(&target).is_err());
    std::fs::rename(&source, native.join("held-source")).unwrap();
    std::os::unix::fs::symlink(&target, &source).unwrap();
    assert!(vfs.metadata_capabilities(&source).is_err());
    assert_eq!(rooted.metadata_capabilities(&source).unwrap(), native_caps);
}

#[test]
#[ignore = "benötigt eigenen nativen, nur lesbar eingehängten Testmount"]
fn local_metadata_readonly_mount_never_claims_write() {
    let root = test_root();
    let native = root.join("native");
    assert!(native.canonicalize().unwrap().starts_with(&root));
    let caps = LocalVfs::new().metadata_capabilities(&native).unwrap();
    assert_eq!(caps.owner, Capability::ReadOnly);
    assert_eq!(caps.special_mode_bits, Capability::ReadOnly);
    assert_eq!(caps.sparse_extents, Capability::ReadOnly);
    assert_eq!(caps.hardlinks, Capability::ReadOnly);
    println!("Nur lesbarer Testmount: {caps:?}");
}
