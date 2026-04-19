//! Tests for non-resident promotion of named $DATA streams (§2.3).

use fs_ntfs::write;
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_ads_promote_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

struct StreamInfo {
    is_resident: bool,
    data: Vec<u8>,
}

fn read_named_stream(img: &str, path: &str, stream: &str) -> StreamInfo {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        let name_matches = a
            .name()
            .ok()
            .map(|n| n.to_string_lossy() == stream)
            .unwrap_or(false);
        if !name_matches {
            continue;
        }
        let is_resident = a.is_resident();
        let mut v = a.value(&mut r).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = v.read(&mut r, &mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        return StreamInfo {
            is_resident,
            data: buf,
        };
    }
    panic!("stream '{stream}' not found on {path}");
}

#[test]
fn write_named_stream_small_stays_resident() {
    let img = working_copy("small");
    write::write_named_stream(Path::new(&img), "/hello.txt", "small", b"tiny").unwrap();
    let info = read_named_stream(&img, "/hello.txt", "small");
    assert!(info.is_resident, "small stream should be resident");
    assert_eq!(info.data, b"tiny");
}

#[test]
fn write_named_stream_oversize_gets_promoted() {
    let img = working_copy("big");
    // 8 KiB payload cannot fit resident in a 1 KiB MFT record.
    let payload: Vec<u8> = (0..8192).map(|i| (i & 0xff) as u8).collect();
    write::write_named_stream(Path::new(&img), "/hello.txt", "bigads", &payload).unwrap();
    let info = read_named_stream(&img, "/hello.txt", "bigads");
    assert!(!info.is_resident, "oversized stream must be non-resident");
    assert_eq!(info.data, payload);
}

#[test]
fn promote_attribute_generic_round_trip() {
    use fs_ntfs::attr_io::AttrType;
    let img = working_copy("generic");
    let payload = vec![0xAB; 5000];
    write::promote_attribute_to_nonresident(
        Path::new(&img),
        "/hello.txt",
        AttrType::Data,
        Some("ads_generic"),
        &payload,
    )
    .unwrap();
    let info = read_named_stream(&img, "/hello.txt", "ads_generic");
    assert!(!info.is_resident);
    assert_eq!(info.data, payload);
}
