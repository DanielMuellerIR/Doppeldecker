//! Echter Reader plus ausdrücklich synthetische validierte Statusantworten.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}

fn seed_stream(provider: &SmbVfs, remote: &str) {
    let state = lock(&provider.state);
    let mut connection = state.session.connection.clone();
    let tree = state.session.tree.clone();
    drop(state);
    let file_id = provider
        .runtime
        .block_on(open_handle(
            &mut connection,
            &tree,
            remote,
            FileAccessMask::FILE_WRITE_DATA | FileAccessMask::SYNCHRONIZE,
            CreateDisposition::FileCreate,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        ))
        .unwrap();
    let mut writer = SmbWrite {
        runtime: provider.runtime.clone(),
        tree,
        connection,
        file_id,
        offset: 0,
        max_write: 64 * 1024,
        finished: false,
    };
    writer.write_all(b"reader-ads").unwrap();
    writer.flush().unwrap();
}

fn injected<T>(mode: &'static str, run: impl FnOnce() -> T) -> T {
    TEST_QUERY_COUNT.with(|count| count.set(0));
    TEST_RESPONSE.with(|value| value.set(Some(mode)));
    let result = run();
    TEST_RESPONSE.with(|value| value.set(None));
    result
}

#[test]
#[ignore = "benötigt frischen privaten Samba-Loopback mit streams_xattr"]
fn windows_security_manifest_reader_roundtrip() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("{name} fehlt"));
    let physical = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(physical.starts_with("/tmp/doppeldecker-smb-acl-probe-20261029"));
    assert_eq!(physical.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&physical).unwrap().next().is_none());
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    assert_eq!(host, "127.0.0.1");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    assert!(port > 1024);
    let config = SmbConnectionConfig {
        server_guid: discover_smb_server(&host, port).unwrap().server_guid,
        host,
        port,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/".into(),
        password_reference: crate::KeychainSecretReference::new("Doppeldecker-Test", "smb")
            .unwrap(),
    };
    let provider = Arc::new(SmbVfs::connect(config, Arc::new(TestSecrets)).unwrap());
    std::fs::create_dir(physical.join("reader")).unwrap();
    std::fs::create_dir(physical.join("reader/directory")).unwrap();
    std::fs::write(physical.join("reader/file"), b"reader-main").unwrap();
    std::fs::write(physical.join("reader/directory/child"), b"reader-child").unwrap();
    let no_streams = std::env::var_os("DOPPELDECKER_SMB_ACL_NO_STREAMS").is_some();
    if !no_streams {
        for remote in ["reader\\file:probe:$DATA", "reader\\directory:probe:$DATA"] {
            seed_stream(&provider, remote);
        }
    }
    let ordinary = provider
        .content_sync_reader(Path::new("/reader"), &crate::Cancel::new(), None)
        .unwrap();
    for vfs in [&*provider as &dyn Vfs, ordinary.as_ref()] {
        let caps = vfs.metadata_capabilities(Path::new("/reader")).unwrap();
        assert_eq!(caps.acl_model, AclModel::WindowsSid);
        assert_eq!(caps.acl, Capability::ReadOnly);
        assert_eq!(caps.owner, Capability::Unsupported);
        for path in [Path::new("/reader/file"), Path::new("/reader/directory")] {
            let manifest = vfs.metadata_manifest(path).unwrap();
            manifest.validate().unwrap();
            assert!(matches!(manifest.acl, Some(Acl::WindowsDacl(_))));
            let security = manifest.windows_security.as_ref().unwrap();
            assert!(security.issue.is_none());
            assert!(security.owner.is_some() && security.group.is_some());
            assert!(manifest.owner.is_none());
            assert_eq!(security.namespace, provider.config.server_guid);
            if no_streams {
                assert!(manifest.named_streams.is_none());
            } else {
                assert_eq!(manifest.named_streams.as_ref().unwrap().len(), 1);
            }
            assert_eq!(vfs.metadata_manifest(path).unwrap().digest, manifest.digest);
            assert!(vfs.write_acl_exact(path, manifest.acl.as_ref()).is_err());
            for (mode, issue) in [
                ("denied", WindowsSecurityReadIssue::ReadControlDenied),
                ("unsupported", WindowsSecurityReadIssue::QueryUnsupported),
                ("invalid", WindowsSecurityReadIssue::InvalidDescriptor),
            ] {
                let omitted = injected(mode, || vfs.metadata_manifest(path)).unwrap();
                omitted.validate().unwrap();
                assert_eq!(
                    omitted.windows_security.as_ref().unwrap().issue,
                    Some(issue)
                );
                assert!(omitted.acl.is_none());
                assert_eq!(omitted.named_streams, manifest.named_streams);
                assert_ne!(omitted.digest, manifest.digest);
                if !no_streams {
                    let mut bytes = Vec::new();
                    vfs.open_named_stream_read(path, b"probe")
                        .unwrap()
                        .read_to_end(&mut bytes)
                        .unwrap();
                    assert_eq!(bytes, b"reader-ads");
                }
            }
            let drift = injected("drift", || vfs.metadata_manifest(path)).unwrap_err();
            assert_eq!(crate::error_code(&drift), crate::ErrorCode::PlanDrift);
        }
    }
    let file = Path::new("/reader/file");
    let before = ordinary.metadata_manifest(file).unwrap();
    std::fs::set_permissions(
        physical.join("reader/file"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let changed = ordinary.metadata_manifest(file).unwrap();
    assert_ne!(before.acl, changed.acl);
    assert_ne!(before.digest, changed.digest);
    assert_eq!(
        before.windows_security.as_ref().unwrap().owner,
        changed.windows_security.as_ref().unwrap().owner
    );
    assert_eq!(
        before.windows_security.as_ref().unwrap().group,
        changed.windows_security.as_ref().unwrap().group
    );
    assert_eq!(before.named_streams, changed.named_streams);
    assert_eq!(
        std::fs::read(physical.join("reader/file")).unwrap(),
        b"reader-main"
    );
    assert_eq!(
        std::fs::read(physical.join("reader/directory/child")).unwrap(),
        b"reader-child"
    );
    println!("SMB_WINDOWS_SECURITY_READER_OK named_streams_unsupported={no_streams} file_directory=1 strong_ordinary=1 acl_readonly=1 owner_numeric_unsupported=1 actual_chmod_digest_drift=1 injected_validated_denied_unsupported_invalid=1 two_pass_security_drift=1 data_ads_unchanged=1");
}
