//! Lesende Windows-Sicherheitsbeobachtung am gehaltenen SMB-Basisobjekt.
use super::*;
use crate::metadata::{Acl, WindowsSecurityObservation, WindowsSecurityReadIssue};

#[cfg(test)]
thread_local! {
    static TEST_RESPONSE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static TEST_QUERY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn test_response(mut frame: smb2::client::connection::Frame) -> smb2::client::connection::Frame {
    use smb2::pack::Pack;
    let mode = TEST_RESPONSE.with(|mode| mode.get());
    let count = TEST_QUERY_COUNT.with(|count| {
        let value = count.get();
        count.set(value + 1);
        value
    });
    let mut bytes = smb2::pack::WriteCursor::new();
    match mode {
        Some("denied") | Some("drift") if mode == Some("denied") || count > 0 => {
            frame.header.status = NtStatus::ACCESS_DENIED;
            smb2::msg::header::ErrorResponse {
                error_context_count: 0,
                error_data: Vec::new(),
            }
            .pack(&mut bytes);
        }
        Some("unsupported") => {
            frame.header.status = NtStatus::NOT_SUPPORTED;
            smb2::msg::header::ErrorResponse {
                error_context_count: 0,
                error_data: Vec::new(),
            }
            .pack(&mut bytes);
        }
        Some("invalid") => {
            frame.header.status = NtStatus::SUCCESS;
            QueryInfoResponse {
                output_buffer: vec![0; 20],
            }
            .pack(&mut bytes);
        }
        _ => return frame,
    }
    frame.body = bytes.into_inner();
    frame
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SecurityObservation {
    pub acl: Option<Acl>,
    pub security: WindowsSecurityObservation,
}

fn unavailable(namespace: [u8; 16], issue: WindowsSecurityReadIssue) -> SecurityObservation {
    SecurityObservation {
        acl: None,
        security: WindowsSecurityObservation {
            namespace,
            owner: None,
            group: None,
            issue: Some(issue),
            invalid_descriptor_digest: None,
        },
    }
}

fn security_status(
    frame: &smb2::client::connection::Frame,
    command: Command,
) -> io::Result<Option<WindowsSecurityReadIssue>> {
    if frame.header.command != command {
        return Err(invalid(
            "SMB-Sicherheitsabfrage erhielt einen anderen Antworttyp",
        ));
    }
    let issue = if frame.header.status == NtStatus::ACCESS_DENIED {
        Some(WindowsSecurityReadIssue::ReadControlDenied)
    } else if command == Command::QueryInfo
        && matches!(
            frame.header.status.0,
            0xc000_0003 | 0xc000_0010 | 0xc000_00bb
        )
    {
        Some(WindowsSecurityReadIssue::QueryUnsupported)
    } else {
        None
    };
    if issue.is_some() {
        // Ein Status ohne vollständigen SMB-Fehlerbody ist ein Protokollfehler.
        smb2::msg::header::ErrorResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(smb_error)?;
    } else {
        require_status(frame, command)?;
    }
    Ok(issue)
}

pub(super) fn decode_security(
    frame: &smb2::client::connection::Frame,
    namespace: [u8; 16],
) -> io::Result<SecurityObservation> {
    if let Some(issue) = security_status(frame, Command::QueryInfo)? {
        return Ok(unavailable(namespace, issue));
    }
    // Der Rawtransport toleriert rückwärts gerichtete Outputoffsets. Hier ist
    // eine ungültige QUERY_INFO-Hülle ein Protokollfehler, kein ungültiger SD.
    if frame.body.len() < 8 {
        return Err(invalid("Unvollständige SMB-Sicherheitsantwort"));
    }
    let offset = usize::from(u16::from_le_bytes([frame.body[2], frame.body[3]]));
    let length = read_u32(&frame.body, 4)? as usize;
    if length > 64 * 1024
        || (length != 0
            && (offset < 72
                || offset
                    .checked_sub(64)
                    .and_then(|start| start.checked_add(length))
                    .is_none_or(|end| end > frame.body.len())))
    {
        return Err(invalid(
            "Ungültiger Outputbereich der SMB-Sicherheitsantwort",
        ));
    }
    let response =
        QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
    match crate::windows_security::parse_security_descriptor(&response.output_buffer, namespace) {
        Ok(descriptor) => Ok(SecurityObservation {
            acl: Some(Acl::WindowsDacl(descriptor.dacl)),
            security: WindowsSecurityObservation {
                namespace,
                owner: descriptor.owner,
                group: descriptor.group,
                issue: None,
                invalid_descriptor_digest: None,
            },
        }),
        Err(_) => {
            let mut result = unavailable(namespace, WindowsSecurityReadIssue::InvalidDescriptor);
            result.security.invalid_descriptor_digest =
                Some(*blake3::hash(&response.output_buffer).as_bytes());
            Ok(result)
        }
    }
}

fn same_security_base(expected: &SmbListedEntry, observed: &SmbListedEntry) -> bool {
    expected.file_id != 0
        && expected.file_id == observed.file_id
        && expected.is_dir == observed.is_dir
        && !observed.is_symlink
        && expected.size == observed.size
        && expected.change_time == observed.change_time
}

pub(super) async fn observe_security(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    expected: &SmbListedEntry,
    namespace: [u8; 16],
) -> io::Result<SecurityObservation> {
    if namespace == [0; 16] {
        return Err(invalid(
            "SMB-Sicherheitsbeobachtung benötigt einen Servernamensraum",
        ));
    }
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(
            FileAccessMask::READ_CONTROL
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
        ),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access: ShareAccess(ShareAccess::FILE_SHARE_READ | ShareAccess::FILE_SHARE_WRITE),
        create_disposition: CreateDisposition::FileOpen,
        create_options: FILE_OPEN_REPARSE_POINT
            | if expected.is_dir {
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
    if let Some(issue) = security_status(&frame, Command::Create)? {
        return Ok(unavailable(namespace, issue));
    }
    let opened = CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
    let result = async {
        let before = query_handle_entry(connection, tree, opened.file_id, String::new()).await?;
        if !same_security_base(expected, &before) {
            return Err(crate::real_mode::plan_drift(
                "SMB-Sicherheitsbasis änderte sich beim Öffnen",
            ));
        }
        let request = QueryInfoRequest {
            info_type: InfoType::Security,
            file_info_class: 0,
            output_buffer_length: 64 * 1024,
            additional_information: 0x7,
            flags: 0,
            file_id: opened.file_id,
            input_buffer: Vec::new(),
        };
        let frame = connection
            .execute(Command::QueryInfo, &request, Some(tree.tree_id))
            .await
            .map_err(smb_error)?;
        #[cfg(test)]
        let frame = test_response(frame);
        let observed = decode_security(&frame, namespace)?;
        let after = query_handle_entry(connection, tree, opened.file_id, String::new()).await?;
        if !same_security_base(expected, &after) {
            return Err(crate::real_mode::plan_drift(
                "SMB-Sicherheitsbasis änderte sich während der Beobachtung",
            ));
        }
        Ok(observed)
    }
    .await;
    let close = tree
        .close_handle(connection, opened.file_id)
        .await
        .map_err(smb_error);
    result.and_then(|observed| close.map(|()| observed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smb2::msg::header::{ErrorResponse, Header};
    use smb2::pack::Pack;
    use smb2::pack::WriteCursor;

    fn frame(status: NtStatus, bytes: Vec<u8>) -> smb2::client::connection::Frame {
        let mut header = Header::new_request(Command::QueryInfo);
        header.status = status;
        smb2::client::connection::Frame {
            header,
            body: bytes,
            raw: Vec::new(),
        }
    }
    fn packed(value: &impl Pack) -> Vec<u8> {
        let mut cursor = WriteCursor::new();
        value.pack(&mut cursor);
        cursor.into_inner()
    }
    fn error_frame(status: NtStatus) -> smb2::client::connection::Frame {
        frame(
            status,
            packed(&ErrorResponse {
                error_context_count: 0,
                error_data: Vec::new(),
            }),
        )
    }

    #[test]
    fn nur_validierte_statusantworten_bedeuten_acl_auslassung() {
        let namespace = [7; 16];
        let denied = decode_security(&error_frame(NtStatus::ACCESS_DENIED), namespace).unwrap();
        assert_eq!(
            denied.security.issue,
            Some(WindowsSecurityReadIssue::ReadControlDenied)
        );
        assert!(denied.acl.is_none());
        assert!(denied.security.owner.is_none() && denied.security.group.is_none());
        for status in [
            NtStatus(0xc000_0003),
            NtStatus::INVALID_DEVICE_REQUEST,
            NtStatus::NOT_SUPPORTED,
        ] {
            assert_eq!(
                decode_security(&error_frame(status), namespace)
                    .unwrap()
                    .security
                    .issue,
                Some(WindowsSecurityReadIssue::QueryUnsupported)
            );
        }
        for status in [
            NtStatus::INVALID_PARAMETER,
            NtStatus::BUFFER_TOO_SMALL,
            NtStatus::USER_SESSION_DELETED,
        ] {
            assert!(decode_security(&error_frame(status), namespace).is_err());
        }
        assert!(decode_security(&frame(NtStatus::ACCESS_DENIED, vec![]), namespace).is_err());
        let mut wrong = error_frame(NtStatus::ACCESS_DENIED);
        wrong.header.command = Command::Read;
        assert!(decode_security(&wrong, namespace).is_err());
        assert!(decode_security(&frame(NtStatus::SUCCESS, vec![]), namespace).is_err());
        let mut invalid_offset = packed(&QueryInfoResponse {
            output_buffer: vec![0; 20],
        });
        invalid_offset[2..4].copy_from_slice(&64u16.to_le_bytes());
        assert!(decode_security(&frame(NtStatus::SUCCESS, invalid_offset), namespace).is_err());
        let mut overrun = packed(&QueryInfoResponse {
            output_buffer: vec![0; 20],
        });
        overrun[4..8].copy_from_slice(&21u32.to_le_bytes());
        assert!(decode_security(&frame(NtStatus::SUCCESS, overrun), namespace).is_err());
    }

    #[test]
    fn ungueltiger_descriptor_bindet_rohbytes_statt_leerer_acl() {
        for bytes in [vec![], vec![1, 2, 3], vec![0; 20]] {
            let result = decode_security(
                &frame(
                    NtStatus::SUCCESS,
                    packed(&QueryInfoResponse {
                        output_buffer: bytes.clone(),
                    }),
                ),
                [7; 16],
            )
            .unwrap();
            assert_eq!(
                result.security.issue,
                Some(WindowsSecurityReadIssue::InvalidDescriptor)
            );
            assert_eq!(
                result.security.invalid_descriptor_digest,
                Some(*blake3::hash(&bytes).as_bytes())
            );
            assert!(result.acl.is_none());
        }
    }
}

#[cfg(test)]
#[path = "smb_security_tests.rs"]
mod e2e;
