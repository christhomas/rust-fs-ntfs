//! Round-trip tests for `write::set_times`. Strategy: write, then read
//! back via upstream `ntfs` (independent of our attr/mft_io parsers) and
//! confirm the value matches.

use fs_ntfs::write::{self, FileTimes};
use ntfs::structured_values::NtfsStandardInformation;
use ntfs::{Ntfs, NtfsAttributeType};
use std::io::BufReader;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_write_times_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy basic fixture");
    dst
}

/// Read the four timestamps for the file at `file_path` via upstream.
fn read_times_via_upstream(img: &str, file_path: &str) -> (u64, u64, u64, u64) {
    let f = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("parse");
    ntfs.read_upcase_table(&mut reader).expect("upcase");

    let mut current = ntfs.root_directory(&mut reader).expect("root");
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = current.directory_index(&mut reader).expect("idx");
        let mut finder = idx.finder();
        let entry = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .expect("find some")
            .expect("find ok");
        current = entry.to_file(&ntfs, &mut reader).expect("to_file");
    }
    let mut attrs = current.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::StandardInformation) {
            continue;
        }
        let si = a
            .resident_structured_value::<NtfsStandardInformation>()
            .expect("si");
        return (
            si.creation_time().nt_timestamp(),
            si.modification_time().nt_timestamp(),
            si.mft_record_modification_time().nt_timestamp(),
            si.access_time().nt_timestamp(),
        );
    }
    panic!("no $STANDARD_INFORMATION");
}

#[test]
fn set_all_four_times() {
    let img = working_copy("all_four");
    // Arbitrary distinct FILETIMEs (all post-2000 to avoid wrap concerns).
    let times = FileTimes {
        creation: Some(130_000_000_000_000_000),
        modification: Some(130_100_000_000_000_000),
        mft_record_modification: Some(130_200_000_000_000_000),
        access: Some(130_300_000_000_000_000),
    };
    write::set_times(std::path::Path::new(&img), "/hello.txt", times).expect("set_times");

    let (cr, m, c, a) = read_times_via_upstream(&img, "/hello.txt");
    assert_eq!(cr, times.creation.unwrap());
    assert_eq!(m, times.modification.unwrap());
    assert_eq!(c, times.mft_record_modification.unwrap());
    assert_eq!(a, times.access.unwrap());
}

#[test]
fn set_only_modification_leaves_others_intact() {
    let img = working_copy("only_mod");
    let (before_c, _, before_m, before_a) = read_times_via_upstream(&img, "/hello.txt");
    let times = FileTimes {
        modification: Some(131_234_567_890_000_000),
        ..Default::default()
    };
    write::set_times(std::path::Path::new(&img), "/hello.txt", times).expect("set_times");

    let (after_c, after_m, after_mc, after_a) = read_times_via_upstream(&img, "/hello.txt");
    assert_eq!(after_m, times.modification.unwrap());
    assert_eq!(after_c, before_c, "creation should not have changed");
    assert_eq!(after_mc, before_m, "mft_mod time should not have changed");
    assert_eq!(after_a, before_a, "access should not have changed");
}

#[test]
fn set_times_survives_re_mount() {
    // Round-trip: write times, re-open volume from scratch, re-read.
    // Catches any issue where upstream's re-parse sees different bytes
    // than our write landed.
    let img = working_copy("remount");
    let t = 135_000_000_000_000_000u64;
    let times = FileTimes {
        creation: Some(t),
        modification: Some(t + 1),
        mft_record_modification: Some(t + 2),
        access: Some(t + 3),
        ..Default::default()
    };
    write::set_times(std::path::Path::new(&img), "/hello.txt", times).expect("set");

    // Drop the image handle implicitly, re-open.
    let (cr, m, mc, a) = read_times_via_upstream(&img, "/hello.txt");
    assert_eq!(cr, t);
    assert_eq!(m, t + 1);
    assert_eq!(mc, t + 2);
    assert_eq!(a, t + 3);
}

#[test]
fn set_times_on_nested_path() {
    let img = working_copy("nested");
    let t = 136_000_000_000_000_000u64;
    let times = FileTimes {
        modification: Some(t),
        ..Default::default()
    };
    write::set_times(std::path::Path::new(&img), "/Documents/readme.txt", times).expect("set");
    let (_cr, m, _mc, _a) = read_times_via_upstream(&img, "/Documents/readme.txt");
    assert_eq!(m, t);

    // Unrelated file's mtime must not have changed.
    let (_, other_m, _, _) = read_times_via_upstream(&img, "/hello.txt");
    assert_ne!(other_m, t, "collateral damage: /hello.txt mtime changed");
}

#[test]
fn set_times_rejects_missing_path() {
    let img = working_copy("missing");
    let err = write::set_times(
        std::path::Path::new(&img),
        "/nonexistent.txt",
        FileTimes {
            access: Some(42),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        err.contains("not found") || err.contains("nonexistent"),
        "expected not-found error; got {err:?}"
    );
}

#[test]
fn upstream_mount_still_succeeds_after_write() {
    let img = working_copy("post_write_mount");
    write::set_times(
        std::path::Path::new(&img),
        "/hello.txt",
        FileTimes {
            access: Some(137_000_000_000_000_000),
            ..Default::default()
        },
    )
    .expect("set");

    // Upstream must still mount cleanly and list the root directory.
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).expect("mount post-write");
    ntfs.read_upcase_table(&mut r).expect("upcase");
    let root = ntfs.root_directory(&mut r).expect("root");
    let idx = root.directory_index(&mut r).expect("idx");
    let mut it = idx.entries();
    let mut saw = false;
    while let Some(entry) = it.next(&mut r) {
        let entry = entry.expect("entry");
        if let Some(Ok(name)) = entry.key() {
            if name.name().to_string_lossy() == "hello.txt" {
                saw = true;
            }
        }
    }
    assert!(saw, "hello.txt missing from root after write");
}
