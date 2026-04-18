//! Tests for `write::write_at` — in-place content rewrite of non-resident
//! data, size-preserving. Uses ntfs-large-file.img (8 MiB big.bin with
//! marker bytes at each MiB boundary) as the primary fixture since it's
//! guaranteed non-resident.

use fs_ntfs::write;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;

const LARGE_IMG: &str = "test-disks/ntfs-large-file.img";
const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(base: &str, tag: &str) -> String {
    let dst = format!("test-disks/_write_content_{tag}.img");
    std::fs::copy(base, &dst).expect("copy fixture");
    dst
}

/// Read `len` bytes from `file_path` at `offset` via upstream.
fn read_range_via_upstream(img: &str, file_path: &str, offset: u64, len: usize) -> Vec<u8> {
    let f = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("parse");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let mut cur = ntfs.root_directory(&mut reader).expect("root");
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut reader).expect("idx");
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .expect("some")
            .expect("ok");
        cur = e.to_file(&ntfs, &mut reader).expect("to_file");
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let mut v = a.value(&mut reader).expect("value");
        v.seek(&mut reader, std::io::SeekFrom::Start(offset))
            .expect("seek");
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = v.read(&mut reader, &mut buf[filled..]).expect("read");
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        return buf;
    }
    panic!("no unnamed $DATA");
}

#[test]
fn rewrite_bytes_at_mb_boundary() {
    let img = working_copy(LARGE_IMG, "boundary");
    // Marker at offset 1 MiB is 'B' ('A' + 1) per the fixture. Rewrite
    // the next few bytes with a distinct pattern.
    let at = 1024u64 * 1024u64;
    let payload = b"HELLO_AT_MB1";

    let n = write::write_at(std::path::Path::new(&img), "/big.bin", at, payload).expect("write_at");
    assert_eq!(n as usize, payload.len());

    let readback = read_range_via_upstream(&img, "/big.bin", at, payload.len());
    assert_eq!(readback, payload);
}

#[test]
fn rewrite_preserves_surrounding_bytes() {
    let img = working_copy(LARGE_IMG, "preserve");
    // The fixture guarantees byte at offset `k*MB` is 'A' + k and every
    // other byte in the 8 MiB file is 0. Verify the surrounding bytes
    // are untouched after a small write.
    let at = 3u64 * 1024 * 1024;
    let before_after = read_range_via_upstream(&img, "/big.bin", at - 4, 16);
    // 4 bytes before the marker should be zero; the marker should be 'D'.
    assert_eq!(&before_after[0..4], &[0; 4]);
    assert_eq!(before_after[4], b'D');

    write::write_at(std::path::Path::new(&img), "/big.bin", at, b"XYZ").expect("write_at");

    let after = read_range_via_upstream(&img, "/big.bin", at - 4, 16);
    assert_eq!(
        &after[0..4],
        &[0; 4],
        "bytes before write must be untouched"
    );
    assert_eq!(&after[4..7], b"XYZ");
    // After the 3-byte write, original fixture bytes (0x00 fill) resume.
    assert_eq!(after[7], 0);
}

#[test]
fn rewrite_rejects_past_eof() {
    let img = working_copy(LARGE_IMG, "past_eof");
    let file_size = 8 * 1024 * 1024u64;
    let err = write::write_at(
        std::path::Path::new(&img),
        "/big.bin",
        file_size - 2,
        b"overflow!",
    )
    .unwrap_err();
    assert!(err.contains("past EOF"), "{err:?}");
}

#[test]
fn rewrite_rejects_resident_data() {
    // hello.txt is only 17 bytes — its $DATA is resident in basic.img.
    let img = working_copy(BASIC_IMG, "resident");
    let err = write::write_at(std::path::Path::new(&img), "/hello.txt", 0, b"X").unwrap_err();
    assert!(
        err.contains("non-resident") || err.contains("resident"),
        "{err:?}"
    );
}

#[test]
fn zero_length_write_is_noop() {
    let img = working_copy(LARGE_IMG, "noop");
    let n = write::write_at(std::path::Path::new(&img), "/big.bin", 0, &[]).expect("write_at");
    assert_eq!(n, 0);
    // File unchanged.
    let first = read_range_via_upstream(&img, "/big.bin", 0, 1);
    assert_eq!(first[0], b'A');
}

#[test]
fn upstream_still_mounts_after_content_write() {
    let img = working_copy(LARGE_IMG, "remount");
    write::write_at(
        std::path::Path::new(&img),
        "/big.bin",
        2 * 1024 * 1024,
        b"REMOUNT_TEST",
    )
    .expect("write");
    let readback = read_range_via_upstream(&img, "/big.bin", 2 * 1024 * 1024, 12);
    assert_eq!(readback, b"REMOUNT_TEST");
}

#[test]
fn write_spanning_cluster_boundary() {
    // A 12-byte write straddling a 4 KiB cluster boundary exercises the
    // run-walker's "clip to end of this cluster" logic.
    let img = working_copy(LARGE_IMG, "span_cluster");
    let cluster_size = 4096u64;
    // 2 MiB is exactly on a cluster boundary; write starting 4 bytes
    // before it so 4 bytes land in cluster-before, 8 bytes in cluster-after.
    let at = 2 * 1024 * 1024 - 4;
    let payload = b"SPAN12345678";
    write::write_at(std::path::Path::new(&img), "/big.bin", at, payload).expect("write");
    let readback = read_range_via_upstream(&img, "/big.bin", at, payload.len());
    assert_eq!(readback, payload);

    // Marker 'C' at offset 2 MiB got overwritten by payload[4] = '1'.
    let byte_at_2mb = read_range_via_upstream(&img, "/big.bin", 2 * 1024 * 1024, 1);
    assert_eq!(byte_at_2mb[0], b'1');
    // Beyond the payload, fixture continues (zero pad).
    let after = read_range_via_upstream(&img, "/big.bin", at + payload.len() as u64, 4);
    assert_eq!(after, &[0, 0, 0, 0]);
    // unused
    let _ = cluster_size;
}
