//! Opt-in-Server für den echten Tauri-Editor; ausschließlich künstlicher Bestand.
use super::*;

#[test]
#[ignore = "nativer Informationsdialog in einem markierten privaten Wegwerfbereich"]
fn remote_properties_live_native_fixture() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    let root = PathBuf::from(
        std::env::var_os("DOPPELDECKER_NATIVE_PROPERTY_ROOT").expect("markierter Testroot"),
    )
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
    let config = root.join("app-config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(std::fs::read_dir(&config).unwrap().count(), 0);
    let (port, control) = fault_ftp_server(&[]);
    control.accept_chmod.store(true, Ordering::SeqCst);
    control.omit_mode_readback.store(false, Ordering::SeqCst);
    control.require_mode_selection.store(true, Ordering::SeqCst);
    {
        let mut fs = control.fs.lock().unwrap();
        fs.get_mut("/").unwrap().insert("Rechtebaum".into(), true);
        fs.insert(
            "/Rechtebaum".into(),
            [("Datei.txt".into(), false), ("Leer".into(), true)].into(),
        );
        fs.insert("/Rechtebaum/Leer".into(), Default::default());
    }
    control.contents.lock().unwrap().insert(
        "/Rechtebaum/Datei.txt".into(),
        b"native unchanged bytes".to_vec(),
    );
    control.mode_overrides.lock().unwrap().extend([
        ("/Rechtebaum".into(), 0o2755),
        ("/Rechtebaum/Datei.txt".into(), 0o4644),
    ]);
    let connection = fault_config(port);
    doppeldecker_core::save_ftps_profile(
        &config.join("ftps-profiles.json"),
        doppeldecker_core::FtpsProfile {
            id: "native-ftps".into(),
            name: "Rechteziel (Test)".into(),
            connection: connection.clone(),
        },
    )
    .unwrap();
    std::fs::write(root.join("native-server.json"), serde_json::to_vec(&serde_json::json!({"secretService":connection.password_reference.service,"secretAccount":connection.password_reference.account,"port":port})).unwrap()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let result = serde_json::json!({"modes":control.mode_overrides.lock().unwrap().clone(),"mutations":control.mutations.lock().unwrap().clone(),"content":control.contents.lock().unwrap().get("/Rechtebaum/Datei.txt").cloned(),"readPaths":control.retr_paths.lock().unwrap().clone()});
        let temporary = root.join("native-property-state.tmp");
        std::fs::write(&temporary, serde_json::to_vec(&result).unwrap()).unwrap();
        std::fs::rename(&temporary, root.join("native-property-state.json")).unwrap();
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
            "Native GUI-Prüfung überschritt ihre Frist"
        );
        std::thread::sleep(Duration::from_millis(80));
    }
    let modes = control.mode_overrides.lock().unwrap();
    assert_eq!(modes["/Rechtebaum"], 0o2750);
    assert_eq!(modes["/Rechtebaum/Datei.txt"], 0o4750);
    assert_eq!(modes["/Rechtebaum/Leer"], 0o750);
    assert_eq!(
        control.contents.lock().unwrap()["/Rechtebaum/Datei.txt"],
        b"native unchanged bytes"
    );
    assert!(control.retr_paths.lock().unwrap().is_empty());
    assert_eq!(
        control
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("SITE CHMOD "))
            .count(),
        3
    );
}
