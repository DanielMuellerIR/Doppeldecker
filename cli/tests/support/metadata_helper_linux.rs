#![cfg(target_os = "linux")]
use doppeldecker_core::{
    file_properties::admin::{AdminMetadataAction, AdminMetadataRequest},
    Cancel,
};
use std::{
    io::{Read, Write},
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    process::{Command, Stdio},
};

fn helper(sandbox: &super::CliSandbox) -> Command {
    let mut command = sandbox.command_for(env!("CARGO_BIN_EXE_doppeldecker-metadata-helper"));
    command.env_remove("PKEXEC_UID");
    command
}

#[test]
fn helper_refuses_direct_start_without_polkit() {
    let sandbox = super::CliSandbox::new();
    let result = helper(&sandbox).output().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["error"]["code"], "permission_denied");
}

fn authorized_request(request: AdminMetadataRequest, gate: u8) -> serde_json::Value {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "Nur im isolierten Root-Testcontainer ausführen"
    );
    let sandbox = super::CliSandbox::new();
    let mut child = helper(&sandbox)
        .env("PKEXEC_UID", "1000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let bytes = serde_json::to_vec(&request).unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    input.write_all(&bytes).unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut ready = [0; 6];
    output.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"READY\n");
    input.write_all(&[gate]).unwrap();
    let mut response = Vec::new();
    output.read_to_end(&mut response).unwrap();
    assert!(child.wait().unwrap().success());
    serde_json::from_slice(&response).unwrap()
}

#[test]
#[ignore = "Benötigt einen isolierten Linux-Root-Testcontainer"]
fn privileged_metadata_changes_and_cancel_preserve_content_and_symlinks() {
    let area = tempfile::tempdir().unwrap();
    let root = area.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("folder")).unwrap();
    std::fs::write(root.join("folder/file"), b"content").unwrap();
    std::fs::write(root.join("outside"), b"untouched").unwrap();
    symlink(root.join("outside"), root.join("folder/link")).unwrap();
    let before = std::fs::metadata(root.join("outside"))
        .unwrap()
        .permissions()
        .mode();
    let directory_mode = std::fs::metadata(root.join("folder"))
        .unwrap()
        .permissions()
        .mode();
    let capture = |action| {
        AdminMetadataRequest::capture(
            "linux-admin-test".into(),
            &root,
            vec!["folder".into()],
            action,
            Some(&root),
            &Cancel::new(),
        )
        .unwrap()
    };
    let cancelled = authorized_request(
        capture(AdminMetadataAction::Permissions {
            mode: 0o700,
            recursive: true,
        }),
        b'C',
    );
    assert_eq!(cancelled["error"]["code"], "interrupted");
    assert_eq!(
        std::fs::metadata(root.join("folder"))
            .unwrap()
            .permissions()
            .mode(),
        directory_mode
    );
    let edited = authorized_request(
        capture(AdminMetadataAction::Permissions {
            mode: 0o750,
            recursive: true,
        }),
        b'E',
    );
    assert!(edited["ok"].is_object(), "{edited}");
    assert_eq!(
        std::fs::metadata(root.join("folder/file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    let owned = authorized_request(
        capture(AdminMetadataAction::Ownership {
            uid: Some(12345),
            gid: Some(12345),
        }),
        b'E',
    );
    assert!(owned["ok"].is_object(), "{owned}");
    assert_eq!(std::fs::metadata(root.join("folder")).unwrap().uid(), 12345);
    assert_eq!(std::fs::read(root.join("folder/file")).unwrap(), b"content");
    assert_eq!(std::fs::read(root.join("outside")).unwrap(), b"untouched");
    assert_eq!(
        std::fs::metadata(root.join("outside"))
            .unwrap()
            .permissions()
            .mode(),
        before
    );
}
