use super::*;

fn id() -> FileId {
    FileId {
        persistent: 7,
        volatile: 13,
    }
}
fn body(ranges: &[(u64, u64)], padding: usize) -> Vec<u8> {
    let mut body = vec![0; 48 + padding + ranges.len() * 16];
    body[..2].copy_from_slice(&49u16.to_le_bytes());
    body[4..8].copy_from_slice(&FSCTL_QUERY_ALLOCATED_RANGES.to_le_bytes());
    body[8..16].copy_from_slice(&id().persistent.to_le_bytes());
    body[16..24].copy_from_slice(&id().volatile.to_le_bytes());
    if !ranges.is_empty() {
        body[32..36].copy_from_slice(&(112 + padding as u32).to_le_bytes());
    }
    body[36..40].copy_from_slice(&(ranges.len() as u32 * 16).to_le_bytes());
    for (slot, (start, length)) in body[48 + padding..].chunks_exact_mut(16).zip(ranges) {
        slot[..8].copy_from_slice(&start.to_le_bytes());
        slot[8..].copy_from_slice(&length.to_le_bytes());
    }
    body
}
fn error_body() -> Vec<u8> {
    vec![9, 0, 0, 0, 0, 0, 0, 0, 0]
}
#[test]
fn ranges_bind_header_offsets_and_intersection_without_inventing_holes() {
    assert_eq!(
        parse_ranges(&body(&[], 0), id(), 64, 0, 1024).unwrap(),
        vec![]
    );
    assert_eq!(
        parse_ranges(&body(&[(0, 1024)], 8), id(), 64, 7, 100).unwrap(),
        vec![DataExtent {
            offset: 7,
            length: 93
        }]
    );
    let exact = vec![
        DataExtent {
            offset: 7,
            length: 1,
        },
        DataExtent {
            offset: 11,
            length: 2,
        },
    ];
    assert_eq!(
        parse_ranges(&body(&[(7, 1), (11, 2)], 0), id(), 32, 0, 100).unwrap(),
        exact
    );
}
#[test]
fn malformed_headers_and_bounds_are_never_complete_empty_lists() {
    for (offset, bytes) in [
        (0, vec![48, 0]),
        (4, 0u32.to_le_bytes().to_vec()),
        (8, 0u64.to_le_bytes().to_vec()),
        (16, 0u64.to_le_bytes().to_vec()),
        (28, 1u32.to_le_bytes().to_vec()),
        (32, 100u32.to_le_bytes().to_vec()),
        (32, u32::MAX.to_le_bytes().to_vec()),
        (36, 17u32.to_le_bytes().to_vec()),
        (40, 1u32.to_le_bytes().to_vec()),
    ] {
        let mut reply = body(&[(0, 1)], 0);
        reply[offset..offset + bytes.len()].copy_from_slice(&bytes);
        assert!(
            parse_ranges(&reply, id(), 64, 0, 100).is_err(),
            "offset={offset}"
        );
    }
    let reply = body(&[(0, 1)], 0);
    assert!(parse_ranges(&reply[..reply.len() - 1], id(), 64, 0, 100).is_err());
    assert!(parse_ranges(&reply, id(), 15, 0, 100).is_err());
    assert!(parse_ranges(&[0; 47], id(), 64, 0, 100).is_err());
    for ranges in [
        vec![(0, 0)],
        vec![(u64::MAX, 1)],
        vec![(1, u64::MAX)],
        vec![(i64::MAX as u64, 1)],
        vec![(100, 1)],
        vec![(0, 1), (0, 1)],
        vec![(0, 10), (9, 2)],
        vec![(20, 1), (10, 1)],
    ] {
        assert!(
            parse_ranges(&body(&ranges, 0), id(), 64, 0, 100).is_err(),
            "{ranges:?}"
        );
    }
    assert!(parse_ranges(&body(&[(0, 10)], 0), id(), 64, 10, 100).is_err());
}
#[test]
fn overflow_pages_need_progress_and_success_before_completion() {
    let mut scan = RangeScan::new(100).unwrap();
    assert!(scan
        .accept(
            Command::Ioctl,
            NtStatus(BUFFER_OVERFLOW),
            &body(&[(0, 8)], 0),
            id()
        )
        .unwrap()
        .is_none());
    assert_eq!(scan.start, 8);
    let result = scan
        .accept(
            Command::Ioctl,
            NtStatus::SUCCESS,
            &body(&[(8, 8), (32, 8)], 0),
            id(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        result.extents,
        Some(vec![
            DataExtent {
                offset: 0,
                length: 16
            },
            DataExtent {
                offset: 32,
                length: 8
            }
        ])
    );
    for ranges in [vec![], vec![(0, 100)]] {
        assert_eq!(
            RangeScan::new(100).unwrap().accept(
                Command::Ioctl,
                NtStatus(BUFFER_OVERFLOW),
                &body(&ranges, 0),
                id()
            ),
            Err(SparseReadIssue::InvalidResponse)
        );
    }
}
#[test]
fn small_buffers_and_total_work_are_bounded_and_status_preserving() {
    let mut scan = RangeScan::new(100).unwrap();
    assert!(scan
        .accept(
            Command::Ioctl,
            NtStatus(BUFFER_TOO_SMALL),
            &error_body(),
            id()
        )
        .unwrap()
        .is_none());
    assert_eq!(scan.output_limit, 8192);
    assert_eq!(scan.start, 0);
    scan.output_limit = RESPONSE_LIMIT;
    assert_eq!(
        scan.accept(
            Command::Ioctl,
            NtStatus(BUFFER_TOO_SMALL),
            &error_body(),
            id()
        ),
        Err(SparseReadIssue::BufferLimit)
    );
    for status in [
        0xc000_0002,
        0xc000_0003,
        0xc000_0010,
        0xc000_000d,
        0xc000_0022,
        0xc000_00bb,
    ] {
        assert_eq!(
            RangeScan::new(100).unwrap().accept(
                Command::Ioctl,
                NtStatus(status),
                &error_body(),
                id()
            ),
            Err(SparseReadIssue::ServerStatus(status))
        );
    }
    assert_eq!(
        RangeScan::new(100)
            .unwrap()
            .accept(Command::Read, NtStatus::SUCCESS, &body(&[], 0), id()),
        Err(SparseReadIssue::InvalidResponse)
    );
    let mut scan = RangeScan::new(100).unwrap();
    scan.pages = PAGE_LIMIT;
    assert_eq!(
        scan.accept(Command::Ioctl, NtStatus::SUCCESS, &body(&[], 0), id()),
        Err(SparseReadIssue::BufferLimit)
    );
    let mut scan = RangeScan::new(EXTENT_LIMIT as u64 * 2 + 10).unwrap();
    scan.start = EXTENT_LIMIT as u64 * 2;
    scan.extents = (0..EXTENT_LIMIT)
        .map(|n| DataExtent {
            offset: n as u64 * 2,
            length: 1,
        })
        .collect();
    assert_eq!(
        scan.accept(
            Command::Ioctl,
            NtStatus::SUCCESS,
            &body(&[(scan.start, 1)], 0),
            id()
        ),
        Err(SparseReadIssue::BufferLimit)
    );
}
#[test]
fn seek_bounds_allow_eof_and_return_without_unsigned_wrap() {
    assert_eq!(seek_position(0, 100, SeekFrom::End(0)).unwrap(), 100);
    assert_eq!(seek_position(100, 100, SeekFrom::Current(-100)).unwrap(), 0);
    assert_eq!(seek_position(0, 100, SeekFrom::Start(101)).unwrap(), 101);
    for position in [
        SeekFrom::Start(u64::MAX),
        SeekFrom::End(i64::MAX),
        SeekFrom::Current(-1),
    ] {
        assert!(seek_position(0, 100, position).is_err());
    }
    assert!(RangeScan::new(u64::MAX).is_err());
}
