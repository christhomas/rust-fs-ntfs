//! sparse fixture: 4 MB file with marker bytes at 1 MB intervals; the rest
//! is holes. Exercises reads that span unallocated clusters.

mod common;

use ntfs::NtfsReadSeek;

const IMG: &str = "test-disks/ntfs-sparse.img";

#[test]
fn logical_size_is_4mb() {
    let (ntfs, mut reader) = common::open(IMG);
    let file = common::navigate(&ntfs, &mut reader, "/sparse.bin");
    let data_item = file
        .data(&mut reader, "")
        .expect("no data")
        .expect("data err");
    let data_attr = data_item.to_attribute().expect("to_attr");
    assert_eq!(data_attr.value_length(), 4 * 1024 * 1024);
}

#[test]
fn marker_and_hole_reads() {
    let (ntfs, mut reader) = common::open(IMG);
    let file = common::navigate(&ntfs, &mut reader, "/sparse.bin");
    let data_item = file
        .data(&mut reader, "")
        .expect("no data")
        .expect("data err");
    let data_attr = data_item.to_attribute().expect("to_attr");
    let mut value = data_attr.value(&mut reader).expect("value");

    // Markers at 0, 1 MB, 2 MB, 3 MB (from build_sparse).
    for i in 0u64..4 {
        let off = i * 1024 * 1024;
        let mut buf = [0u8; 1];
        value
            .seek(&mut reader, std::io::SeekFrom::Start(off))
            .expect("seek marker");
        value.read(&mut reader, &mut buf).expect("read marker");
        assert_eq!(buf[0], b'X', "marker byte at {off} should be 'X'");
    }

    // Mid-hole read: 512 KB offset is in a hole — must return zeros.
    let mut hole = [0xFFu8; 64];
    value
        .seek(&mut reader, std::io::SeekFrom::Start(512 * 1024))
        .expect("seek hole");
    value.read(&mut reader, &mut hole).expect("read hole");
    assert!(hole.iter().all(|&b| b == 0), "hole bytes must be zero");
}
