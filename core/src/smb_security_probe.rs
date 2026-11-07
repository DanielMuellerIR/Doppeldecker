//! Kleine opt-in-Rawprobe; sie erteilt keine produktive ACL-Fähigkeit.
use super::*;
use std::path::PathBuf;

struct TestSecrets;
impl SecretResolver for TestSecrets {
    fn resolve(&self, _: &crate::KeychainSecretReference) -> io::Result<crate::SecretString> {
        crate::SecretString::new("Doppeldecker-SMB-E2E-wertlos-2026".into())
    }
}

async fn security_query(
    connection: &mut Connection,
    tree: &Tree,
    file_id: FileId,
) -> io::Result<Vec<u8>> {
    let request = QueryInfoRequest {
        info_type: InfoType::Security,
        file_info_class: 0,
        output_buffer_length: 64 * 1024,
        additional_information: 0x7,
        flags: 0,
        file_id,
        input_buffer: Vec::new(),
    };
    let frame = connection
        .execute(Command::QueryInfo, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::QueryInfo)?;
    QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
        .map(|response| response.output_buffer)
        .map_err(smb_error)
}

async fn bound_open(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    directory: bool,
    read_control: bool,
) -> io::Result<FileId> {
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(
            FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE
                | if read_control {
                    FileAccessMask::READ_CONTROL
                } else {
                    FileAccessMask::FILE_READ_DATA
                },
        ),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        // Die gehaltenen Eltern und die Basis verhindern einen Namespace-Rename.
        share_access: ShareAccess(ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_WRITE),
        create_disposition: CreateDisposition::FileOpen,
        create_options: FILE_OPEN_REPARSE_POINT
            | if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            },
        name: path.into(),
        create_contexts: Vec::new(),
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::Create)?;
    Ok(CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
        .map_err(smb_error)?
        .file_id)
}

fn stream_seed(provider: &SmbVfs, remote: &str) {
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
    writer.write_all(b"unchanged-ads").unwrap();
    writer.flush().unwrap();
}

fn stream_read(provider: &SmbVfs, path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    provider
        .open_named_stream_read(path, b"probe")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
#[ignore = "benötigt frischen privaten Samba-Loopback mit streams_xattr"]
fn raw_security_owner_group_dacl_query_file_and_directory() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("{name} fehlt"));
    let root = PathBuf::from(required("DOPPELDECKER_SMB_TEST_ROOT"));
    assert!(root.starts_with("/tmp/doppeldecker-smb-acl-probe-20261029"));
    assert_eq!(root.file_name().unwrap(), "share");
    assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    let evidence = PathBuf::from(required("DOPPELDECKER_SMB_ACL_EVIDENCE"));
    assert!(evidence.starts_with("/tmp/doppeldecker-smb-acl-evidence-20261029"));
    std::fs::create_dir_all(&evidence).unwrap();
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
    let provider = SmbVfs::connect(config, Arc::new(TestSecrets)).unwrap();
    std::fs::create_dir(root.join("probe")).unwrap();
    std::fs::create_dir(root.join("probe/directory")).unwrap();
    std::fs::write(root.join("probe/file"), b"unchanged-main").unwrap();
    std::fs::write(root.join("probe/directory/child"), b"unchanged-child").unwrap();
    for remote in ["probe\\file:probe:$DATA", "probe\\directory:probe:$DATA"] {
        stream_seed(&provider, remote);
    }
    for path in [Path::new("/probe/file"), Path::new("/probe/directory")] {
        assert_eq!(stream_read(&provider, path), b"unchanged-ads");
    }
    let mut state = lock(&provider.state);
    let tree = state.session.tree.clone();
    assert!(state.session.identity.signing_active && state.session.identity.encryption_active);
    provider.runtime.block_on(async {
        let connection = &mut state.session.connection;
        let mut parents = Vec::new();
        for parent in ["", "probe"] {
            let id = bound_open(connection, &tree, parent, true, false).await.unwrap();
            let before = query_handle_entry(connection, &tree, id, String::new()).await.unwrap();
            assert!(before.is_dir && !before.is_symlink);
            parents.push((parent, id, before.file_id));
        }
        for (name, directory) in [("file", false), ("directory", true)] {
            let path = format!("probe\\{name}");
            let id = bound_open(connection, &tree, &path, directory, true).await.unwrap();
            let before = query_handle_entry(connection, &tree, id, String::new()).await.unwrap();
            assert_eq!(before.is_dir, directory);
            assert!(!before.is_symlink);
            let bytes = security_query(connection, &tree, id).await.unwrap();
            assert!(bytes.len() >= 20 && bytes[0] == 1);
            assert_ne!(u16::from_le_bytes([bytes[2], bytes[3]]) & 0x8000, 0);
            for offset in [4, 8, 16] { assert_ne!(read_u32(&bytes, offset).unwrap(), 0); }
            assert_eq!(read_u32(&bytes, 12).unwrap(), 0);
            let fresh = bound_open(connection, &tree, &path, directory, true).await.unwrap();
            let fresh_entry = query_handle_entry(connection, &tree, fresh, String::new()).await.unwrap();
            assert_eq!(before.file_id, fresh_entry.file_id);
            assert_eq!(bytes, security_query(connection, &tree, fresh).await.unwrap());
            tree.close_handle(connection, fresh).await.unwrap();
            let denied = bound_open(connection, &tree, &path, directory, false).await.unwrap();
            let denied_query = QueryInfoRequest {
                info_type: InfoType::Security, file_info_class: 0,
                output_buffer_length: 64 * 1024, additional_information: 0x7,
                flags: 0, file_id: denied, input_buffer: Vec::new(),
            };
            let denied_frame = connection.execute(Command::QueryInfo, &denied_query, Some(tree.tree_id)).await.unwrap();
            assert_eq!(denied_frame.header.status, NtStatus::ACCESS_DENIED);
            if !directory {
                let read = ReadRequest {
                    padding: 0, flags: 0, length: 64, offset: 0, file_id: denied,
                    minimum_count: 0, channel: SMB2_CHANNEL_NONE, remaining_bytes: 0,
                    read_channel_info: Vec::new(),
                };
                let frame = connection.execute(Command::Read, &read, Some(tree.tree_id)).await.unwrap();
                require_status(&frame, Command::Read).unwrap();
                assert_eq!(ReadResponse::unpack(&mut ReadCursor::new(&frame.body)).unwrap().data, b"unchanged-main");
                println!("SMB_SECURITY_ACCESS_MASK_OK read_data=success query_security=0xc0000022 same_handle=1");
            }
            tree.close_handle(connection, denied).await.unwrap();
            assert_eq!(before.file_id, query_handle_entry(connection, &tree, id, String::new()).await.unwrap().file_id);
            std::fs::write(evidence.join(format!("{name}-security.bin")), &bytes).unwrap();
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            println!("SMB_SECURITY_QUERY_OK kind={name} length={} internal_id={} owner_offset={} group_offset={} dacl_offset={} no_read_control=permission_denied raw={hex}", bytes.len(), before.file_id, read_u32(&bytes,4).unwrap(), read_u32(&bytes,8).unwrap(), read_u32(&bytes,16).unwrap());
            tree.close_handle(connection, id).await.unwrap();
        }
        for (path, id, file_id) in parents.into_iter().rev() {
            assert_eq!(file_id, query_handle_entry(connection, &tree, id, String::new()).await.unwrap().file_id);
            let fresh = query_entry(connection, &tree, path, String::new()).await.unwrap();
            assert_eq!(file_id, fresh.file_id);
            tree.close_handle(connection, id).await.unwrap();
        }
    });
    drop(state);
    assert_eq!(
        std::fs::read(root.join("probe/file")).unwrap(),
        b"unchanged-main"
    );
    assert_eq!(
        std::fs::read(root.join("probe/directory/child")).unwrap(),
        b"unchanged-child"
    );
    for path in [Path::new("/probe/file"), Path::new("/probe/directory")] {
        assert_eq!(stream_read(&provider, path), b"unchanged-ads");
    }
    println!("SMB_SECURITY_PROBE_OK signing=1 encryption=1 security_mask=0x7 data_ads_unchanged=1 fresh_ids_parents=1 product_capability_unchanged=1");
}
