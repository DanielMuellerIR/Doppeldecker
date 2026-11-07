//! Begrenzte echte SMB-Quelle für den nativen F5-Gegenlauf.
use super::*;

#[test]
#[ignore = "benötigt eigenen Samba-Server und markierte native GUI-Sandbox"]
fn smb_selected_copy_live_native_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let server_root = required("DOPPELDECKER_SMB_TEST_ROOT");
    assert!(server_root.starts_with("/tmp/doppeldecker-smb-e2e."));
    let server_root = Path::new(&server_root).canonicalize().unwrap();
    assert_eq!(std::fs::read_dir(&server_root).unwrap().count(), 0);
    let root = std::path::PathBuf::from(required("DOPPELDECKER_NATIVE_PROPERTY_ROOT"))
        .canonicalize()
        .unwrap();
    assert!(root.starts_with(Path::new("/tmp").canonicalize().unwrap()));
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(
        std::fs::read(root.join(".doppeldecker-test-sandbox")).unwrap(),
        b"DOPPELDECKER TEST SANDBOX\n"
    );
    let config_dir = root.join("app-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(std::fs::read_dir(&config_dir).unwrap().count(), 0);
    std::fs::create_dir(server_root.join("source")).unwrap();
    let bytes = b"Native SMB copy keeps these bytes\n";
    let source = server_root.join("source/SMB-Datei.txt");
    std::fs::write(&source, bytes).unwrap();
    let host = required("DOPPELDECKER_SMB_TEST_HOST");
    let port = required("DOPPELDECKER_SMB_TEST_PORT").parse().unwrap();
    let discovery = discover_smb_server(&host, port).unwrap();
    assert_eq!(discovery.dialect, 0x0311);
    assert!(discovery.encryption_capable);
    let connection = SmbConnectionConfig {
        host,
        port,
        server_guid: discovery.server_guid,
        share: required("DOPPELDECKER_SMB_TEST_SHARE"),
        username: required("DOPPELDECKER_SMB_TEST_USER"),
        domain: String::new(),
        requested_root: "/source".into(),
        password_reference: KeychainSecretReference::new("Doppeldecker-Test", "smb-native-copy")
            .unwrap(),
    };
    doppeldecker_core::save_smb_profile(
        &config_dir.join("smb-profiles.json"),
        doppeldecker_core::SmbProfile {
            id: "native-smb".into(),
            name: "SMB-Quelle (Test)".into(),
            connection: connection.clone(),
        },
    )
    .unwrap();
    std::fs::write(root.join("native-server.json"), serde_json::to_vec(&serde_json::json!({"source":source,"secretService":connection.password_reference.service,"secretAccount":connection.password_reference.account})).unwrap()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        match std::fs::symlink_metadata(root.join("native-done")) {
            Ok(metadata) => {
                assert!(metadata.is_file());
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("{error}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Native SMB-Prüfung überschritt ihre Frist"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert_eq!(
        std::fs::read(root.join("ziel/SMB-Datei.txt")).unwrap(),
        bytes
    );
    std::fs::write(
        root.join("native-smb-server-result.json"),
        b"{\"sourceUnchanged\":true,\"targetMatches\":true}\n",
    )
    .unwrap();
}
