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
