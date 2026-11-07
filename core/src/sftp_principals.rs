//! Optionale OpenSSH-Namensanzeige; numerische Identitäten und Schreibverträge bleiben unberührt.
use super::*;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
const EXTENSION: &str = "users-groups-by-id@openssh.com";

pub(super) async fn resolve_display_names(
    raw: &RawSftpSession,
    records: &mut [(String, FileAttributes)],
    enabled: &AtomicBool,
    cancel: &crate::Cancel,
) -> io::Result<()> {
    if !enabled.load(Ordering::SeqCst) {
        return Ok(());
    }
    check_sftp_cancel(Some(cancel))?;
    let users: Vec<u32> = records
        .iter()
        .filter(|(_, attr)| attr.user.is_none())
        .filter_map(|(_, attr)| attr.uid)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_IDS)
        .collect();
    let groups: Vec<u32> = records
        .iter()
        .filter(|(_, attr)| attr.group.is_none())
        .filter_map(|(_, attr)| attr.gid)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_IDS)
        .collect();
    if users.is_empty() && groups.is_empty() {
        return Ok(());
    }
    let result = async {
        let payload = encode_ids(&users, &groups)?;
        let reply = raw.extended(EXTENSION, payload).await.map_err(sftp_error)?;
        check_sftp_cancel(Some(cancel))?;
        match reply {
            russh_sftp::protocol::Packet::ExtendedReply(reply) => {
                decode_reply(&reply.data, users.len(), groups.len())
            }
            russh_sftp::protocol::Packet::Status(status) => {
                Err(sftp_error(russh_sftp::client::error::Error::Status(status)))
            }
            _ => Err(invalid()),
        }
    }
    .await;
    let names = match result {
        Ok(names) => names,
        Err(error) => {
            if !cancel.is_cancelled() {
                enabled.store(false, Ordering::SeqCst);
            }
            return Err(error);
        }
    };
    let users: std::collections::HashMap<_, _> = users.into_iter().zip(names.users).collect();
    let groups: std::collections::HashMap<_, _> = groups.into_iter().zip(names.groups).collect();
    for (_, attr) in records {
        if attr.user.is_none() {
            attr.user = attr
                .uid
                .and_then(|id| users.get(&id).and_then(Clone::clone));
        }
        if attr.group.is_none() {
            attr.group = attr
                .gid
                .and_then(|id| groups.get(&id).and_then(Clone::clone));
        }
    }
    Ok(())
}

use std::io;
const MAX_IDS: usize = 32;
const MAX_REPLY_BYTES: usize = 32 * 1024;
const MAX_NAME_BYTES: usize = 256;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Ungültige SFTP-Namensantwort")
}
fn encode_ids(users: &[u32], groups: &[u32]) -> io::Result<Vec<u8>> {
    if users.len() > MAX_IDS || groups.len() > MAX_IDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Zu viele SFTP-Identitäten je Namensabfrage",
        ));
    }
    let mut result = Vec::new();
    for ids in [users, groups] {
        result.extend_from_slice(&u32::try_from(ids.len() * 4).unwrap().to_be_bytes());
        for id in ids {
            result.extend_from_slice(&id.to_be_bytes());
        }
    }
    Ok(result)
}
fn take_string<'a>(input: &mut &'a [u8]) -> io::Result<&'a [u8]> {
    let header = input.get(..4).ok_or_else(invalid)?;
    let size = u32::from_be_bytes(header.try_into().unwrap()) as usize;
    *input = &input[4..];
    let value = input.get(..size).ok_or_else(invalid)?;
    *input = &input[size..];
    Ok(value)
}
fn decode_names(mut block: &[u8], count: usize) -> io::Result<Vec<Option<String>>> {
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        let bytes = take_string(&mut block)?;
        let name = std::str::from_utf8(bytes).ok().filter(|name| {
            !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
        });
        names.push(name.map(str::to_owned));
    }
    if !block.is_empty() {
        return Err(invalid());
    }
    Ok(names)
}
#[derive(Debug, PartialEq, Eq)]
struct Names {
    users: Vec<Option<String>>,
    groups: Vec<Option<String>>,
}
fn decode_reply(mut data: &[u8], users: usize, groups: usize) -> io::Result<Names> {
    if data.len() > MAX_REPLY_BYTES || users > MAX_IDS || groups > MAX_IDS {
        return Err(invalid());
    }
    let usernames = decode_names(take_string(&mut data)?, users)?;
    let groupnames = decode_names(take_string(&mut data)?, groups)?;
    if !data.is_empty() {
        return Err(invalid());
    }
    Ok(Names {
        users: usernames,
        groups: groupnames,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn string(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    fn reply(users: &[&[u8]], groups: &[&[u8]]) -> Vec<u8> {
        let mut result = Vec::new();
        for names in [users, groups] {
            let mut block = Vec::new();
            for name in names {
                string(&mut block, name);
            }
            string(&mut result, &block);
        }
        result
    }
    #[test]
    fn request_uses_two_bounded_network_order_integer_strings() {
        assert_eq!(
            encode_ids(&[1000, u32::MAX], &[20]).unwrap(),
            [0, 0, 0, 8, 0, 0, 3, 232, 255, 255, 255, 255, 0, 0, 0, 4, 0, 0, 0, 20]
        );
        assert_eq!(encode_ids(&[], &[]).unwrap(), [0; 8]);
        assert!(encode_ids(&[0; 33], &[]).is_err());
    }
    #[test]
    fn response_preserves_order_and_empty_names_without_local_lookup() {
        let result = decode_reply(
            &reply(&[b"remote-alice", b"", b"root"], &[b"remote-staff"]),
            3,
            1,
        )
        .unwrap();
        assert_eq!(
            result,
            Names {
                users: vec![Some("remote-alice".into()), None, Some("root".into())],
                groups: vec![Some("remote-staff".into())]
            }
        );
        assert_eq!(
            decode_reply(&reply(&[], &[b"group"]), 0, 1).unwrap().groups,
            [Some("group".into())]
        );
    }
    #[test]
    fn bad_names_leave_independent_valid_principals_available() {
        let long = vec![b'x'; 257];
        let result =
            decode_reply(&reply(&[b"bad\nname", &[255], &long, b"valid"], &[]), 4, 0).unwrap();
        assert_eq!(result.users, vec![None, None, None, Some("valid".into())]);
    }
    #[test]
    fn malformed_lengths_counts_and_trailing_data_never_produce_partial_mappings() {
        let good = reply(&[b"user"], &[b"group"]);
        for size in 0..good.len() {
            assert!(decode_reply(&good[..size], 1, 1).is_err(), "{size}");
        }
        let mut extra = good.clone();
        extra.push(0);
        assert!(decode_reply(&extra, 1, 1).is_err());
        assert!(decode_reply(&good, 0, 1).is_err());
        assert!(decode_reply(&good, 2, 1).is_err());
        let mut enormous = good;
        enormous[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_reply(&enormous, 1, 1).is_err());
        assert!(decode_reply(&vec![0; MAX_REPLY_BYTES + 1], 0, 0).is_err());
    }
}
