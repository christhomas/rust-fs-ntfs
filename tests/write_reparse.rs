//! Tests for W4.2 reparse points + symlinks.

use fs_ntfs::write;
use ntfs::structured_values::{NtfsFileAttributeFlags, NtfsStandardInformation};
use ntfs::{Ntfs, NtfsAttributeType};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_reparse_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn has_reparse_flag(img: &str, file_path: &str) -> bool {
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
        if a.ty().ok() != Some(NtfsAttributeType::StandardInformation) {
            continue;
        }
        let si = a
            .resident_structured_value::<NtfsStandardInformation>()
            .unwrap();
        return si
            .file_attributes()
            .contains(NtfsFileAttributeFlags::REPARSE_POINT);
    }
    false
}

fn has_reparse_point_attr(img: &str, file_path: &str) -> bool {
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
        if a.ty().ok() == Some(NtfsAttributeType::ReparsePoint) {
            return true;
        }
    }
    false
}

#[test]
fn write_reparse_point_sets_flag_and_attribute() {
    let img = working_copy("basic");
    let data = b"some.tag-specific.bytes";
    write::write_reparse_point(Path::new(&img), "/Documents/readme.txt", 0xA000_000C, data)
        .expect("write_reparse_point");

    assert!(has_reparse_flag(&img, "/Documents/readme.txt"));
    assert!(has_reparse_point_attr(&img, "/Documents/readme.txt"));
}

#[test]
fn remove_reparse_point_clears_flag_and_attribute() {
    let img = working_copy("remove");
    write::write_reparse_point(
        Path::new(&img),
        "/Documents/readme.txt",
        0xA000_000C,
        b"tag data",
    )
    .unwrap();
    assert!(has_reparse_point_attr(&img, "/Documents/readme.txt"));

    write::remove_reparse_point(Path::new(&img), "/Documents/readme.txt").unwrap();

    assert!(!has_reparse_point_attr(&img, "/Documents/readme.txt"));
    assert!(!has_reparse_flag(&img, "/Documents/readme.txt"));
}

#[test]
fn remove_missing_reparse_point_errors() {
    let img = working_copy("no_reparse");
    let err = write::remove_reparse_point(Path::new(&img), "/Documents/readme.txt").unwrap_err();
    assert!(err.contains("no $REPARSE_POINT"), "{err:?}");
}

#[test]
fn create_symlink_sets_reparse_state() {
    let img = working_copy("symlink");
    write::create_symlink(
        Path::new(&img),
        "/Documents",
        "shortcut",
        r"\??\C:\Windows\System32",
        false,
    )
    .expect("create_symlink");

    assert!(has_reparse_flag(&img, "/Documents/shortcut"));
    assert!(has_reparse_point_attr(&img, "/Documents/shortcut"));
}

#[test]
fn upstream_mounts_after_reparse_churn() {
    let img = working_copy("churn");
    write::write_reparse_point(
        Path::new(&img),
        "/Documents/readme.txt",
        0xA000_000C,
        b"first",
    )
    .unwrap();
    write::write_reparse_point(
        Path::new(&img),
        "/Documents/readme.txt",
        0xA000_0003,
        b"longer replacement data",
    )
    .unwrap();
    write::remove_reparse_point(Path::new(&img), "/Documents/readme.txt").unwrap();

    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}

#[test]
fn read_reparse_point_roundtrips_tag_and_data() {
    let img = working_copy("read_rt");
    let data: &[u8] = b"opaque.third.party.payload";
    let tag: u32 = 0xA000_0017; // arbitrary non-symlink tag
    write::write_reparse_point(Path::new(&img), "/Documents/readme.txt", tag, data)
        .expect("write_reparse_point");

    let rp = write::read_reparse_point(Path::new(&img), "/Documents/readme.txt")
        .expect("read_reparse_point")
        .expect("attribute should exist");
    assert_eq!(rp.reparse_tag, tag);
    assert_eq!(rp.data, data);
}

#[test]
fn read_reparse_point_returns_none_when_absent() {
    let img = working_copy("read_none");
    let result = write::read_reparse_point(Path::new(&img), "/Documents/readme.txt")
        .expect("read_reparse_point");
    assert!(result.is_none(), "expected None, got {result:?}");
}

#[test]
fn read_reparse_point_after_replace_reflects_new_payload() {
    let img = working_copy("read_replace");
    write::write_reparse_point(Path::new(&img), "/Documents/readme.txt", 0xA000_000C, b"first")
        .unwrap();
    write::write_reparse_point(
        Path::new(&img),
        "/Documents/readme.txt",
        0xA000_0003,
        b"second longer payload",
    )
    .unwrap();
    let rp = write::read_reparse_point(Path::new(&img), "/Documents/readme.txt")
        .unwrap()
        .unwrap();
    assert_eq!(rp.reparse_tag, 0xA000_0003);
    assert_eq!(rp.data, b"second longer payload");
}

#[test]
fn read_reparse_point_handles_empty_data() {
    let img = working_copy("read_empty");
    write::write_reparse_point(Path::new(&img), "/Documents/readme.txt", 0xA000_0099, b"")
        .expect("write_reparse_point with empty data");
    let rp = write::read_reparse_point(Path::new(&img), "/Documents/readme.txt")
        .unwrap()
        .unwrap();
    assert_eq!(rp.reparse_tag, 0xA000_0099);
    assert!(rp.data.is_empty());
}
