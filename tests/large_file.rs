//! large-file fixture: 8 MB file with marker bytes at each MB boundary.
//! Verifies the non-resident $DATA read path across multiple data runs.

mod common;

use ntfs::{NtfsAttributeType, NtfsReadSeek};

const IMG: &str = "test-disks/ntfs-large-file.img";

#[test]
fn data_attribute_is_non_resident() {
    let (ntfs, mut reader) = common::open(IMG);
    let file = common::navigate(&ntfs, &mut reader, "/big.bin");

    let mut attrs = file.attributes();
    let mut found = false;
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let attr = item.to_attribute().expect("to_attribute");
        if attr.ty().expect("ty") == NtfsAttributeType::Data
            && attr.name().map(|n| n.is_empty()).unwrap_or(true)
        {
            // 8 MiB cannot be resident (resident ceiling is <1 KiB).
            assert!(!attr.is_resident(), "8 MiB $DATA must be non-resident");
            assert_eq!(attr.value_length(), 8 * 1024 * 1024);
            found = true;
        }
    }
    assert!(found, "no unnamed $DATA attribute");
}

#[test]
fn marker_bytes_at_mb_boundaries() {
    let (ntfs, mut reader) = common::open(IMG);
    let file = common::navigate(&ntfs, &mut reader, "/big.bin");
    let data_item = file
        .data(&mut reader, "")
        .expect("no data")
        .expect("data err");
    let data_attr = data_item.to_attribute().expect("to_attr");
    let mut value = data_attr.value(&mut reader).expect("value");

    // Every MB boundary holds a distinct marker byte ('A' + i), everything
    // else is zero. Seek-read each marker + an adjacent zero byte to prove
    // the data-run traversal lands at the right offset.
    let mut buf = [0u8; 2];
    for i in 0u64..8 {
        let off = i * 1024 * 1024;
        value
            .seek(&mut reader, std::io::SeekFrom::Start(off))
            .expect("seek");
        let n = value.read(&mut reader, &mut buf).expect("read");
        assert_eq!(n, 2);
        assert_eq!(buf[0], b'A' + i as u8, "marker byte at MB {i} mismatch");
        assert_eq!(buf[1], 0, "byte after marker at MB {i} must be zero");
    }
}

#[test]
fn small_control_file_still_reads() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/small.txt");
    assert_eq!(content, b"small control file\n");
}
