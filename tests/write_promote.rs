//! Tests for W2.2 promotion (resident → non-resident) and the
//! high-level `write_file_contents` dispatcher.

use fs_ntfs::write;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_promote_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn read_data(img: &str, file_path: &str) -> Vec<u8> {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
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
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let mut v = a.value(&mut r).unwrap();
        v.seek(&mut r, std::io::SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0u8; a.value_length() as usize];
        let mut filled = 0;
        while filled < buf.len() {
            let n = v.read(&mut r, &mut buf[filled..]).unwrap();
            if n == 0 {
                break;
            }
            filled += n;
        }
        return buf;
    }
    panic!("no $DATA");
}

fn data_is_nonresident(img: &str, file_path: &str) -> bool {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
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
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        return !a.is_resident();
    }
    panic!("no $DATA");
}

#[test]
fn promote_small_file_to_nonresident() {
    let img = working_copy("small_promote");
    write::create_file(Path::new(&img), "/Documents", "promoted.txt").expect("create");

    let payload = b"promoted content staying small but forced nonresident";
    write::promote_resident_data_to_nonresident(
        Path::new(&img),
        "/Documents/promoted.txt",
        payload,
    )
    .expect("promote");

    assert!(data_is_nonresident(&img, "/Documents/promoted.txt"));
    assert_eq!(read_data(&img, "/Documents/promoted.txt"), payload);
}

#[test]
fn promote_with_large_content() {
    let img = working_copy("large_promote");
    write::create_file(Path::new(&img), "/Documents", "big.txt").expect("create");
    // 10 KiB — comfortably over any resident ceiling.
    let payload: Vec<u8> = (0..10_240).map(|i| (i & 0xff) as u8).collect();
    write::promote_resident_data_to_nonresident(Path::new(&img), "/Documents/big.txt", &payload)
        .expect("promote");

    assert!(data_is_nonresident(&img, "/Documents/big.txt"));
    assert_eq!(read_data(&img, "/Documents/big.txt"), payload);
}

#[test]
fn promote_rejects_already_nonresident() {
    let img = working_copy("already_nonres");
    let lg = "test-disks/_promote_already_nonres_src.img";
    std::fs::copy("test-disks/ntfs-large-file.img", lg).unwrap();
    let err =
        write::promote_resident_data_to_nonresident(Path::new(lg), "/big.bin", b"xx").unwrap_err();
    assert!(err.contains("non-resident"), "{err:?}");
    // Silence unused-var in case test dir wasn't setup.
    let _ = img;
}

#[test]
fn write_file_contents_dispatches_small_resident() {
    let img = working_copy("dispatch_small");
    write::create_file(Path::new(&img), "/Documents", "small.txt").expect("create");
    let payload = b"stays resident";
    write::write_file_contents(Path::new(&img), "/Documents/small.txt", payload).expect("write");
    assert_eq!(read_data(&img, "/Documents/small.txt"), payload);
    assert!(!data_is_nonresident(&img, "/Documents/small.txt"));
}

#[test]
fn write_file_contents_dispatches_large_to_nonresident() {
    let img = working_copy("dispatch_large");
    write::create_file(Path::new(&img), "/Documents", "grown.txt").expect("create");
    // 2 KiB — exceeds the 1 KiB MFT record's resident capacity.
    let payload: Vec<u8> = (0..2048).map(|i| (i & 0xff) as u8).collect();
    write::write_file_contents(Path::new(&img), "/Documents/grown.txt", &payload).expect("write");
    assert_eq!(read_data(&img, "/Documents/grown.txt"), payload);
    assert!(data_is_nonresident(&img, "/Documents/grown.txt"));
}

#[test]
fn upstream_mounts_after_promote() {
    let img = working_copy("remount");
    write::create_file(Path::new(&img), "/Documents", "m.txt").expect("create");
    write::promote_resident_data_to_nonresident(Path::new(&img), "/Documents/m.txt", b"hello")
        .expect("promote");
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
