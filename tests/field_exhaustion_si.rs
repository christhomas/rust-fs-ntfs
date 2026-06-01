//! Phase 2 field-exhaustion tests for `$STANDARD_INFORMATION`.
//!
//! Verifies that every settable field in `$STANDARD_INFORMATION` round-trips
//! correctly: all four timestamps (independently, at boundary values), DOS
//! file attribute bits, and the v3-form security_id. Also confirms the default
//! values for v3 trailer fields (owner_id, quota, usn) on freshly-created
//! files. All tests format their own in-memory volume — no fixture images
//! required.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write::{self, file_attr, read_si_full, FileAttributesChange, FileTimes};
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

/// Format a fresh volume into a temp image and return its path.
fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_fex_si_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("SITEST"),
        Some(0xFEED_DEAD),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Create a file in the fresh volume and return (img_path, "/filename").
fn fresh_file(tag: &str) -> (String, String) {
    let img = fresh_vol(tag);
    let path = format!("/si_{tag}.bin");
    write::create_file(Path::new(&img), "/", &format!("si_{tag}.bin")).expect("create_file");
    (img, path)
}

// Arbitrary post-2000 NT timestamp: 2022-07-04 12:00:00 UTC.
const T_BASE: u64 = 132_700_320_000_000_000u64;

// ---------------------------------------------------------------------------
// Timestamp fields
// ---------------------------------------------------------------------------

#[test]
fn all_four_timestamps_set_to_distinct_values() {
    let (img, path) = fresh_file("4times");
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(T_BASE),
            modification: Some(T_BASE + 1_000),
            mft_record_modification: Some(T_BASE + 2_000),
            access: Some(T_BASE + 3_000),
        },
    )
    .expect("set_times");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_eq!(si.creation_time, T_BASE, "creation_time");
    assert_eq!(si.modification_time, T_BASE + 1_000, "modification_time");
    assert_eq!(
        si.mft_modification_time,
        T_BASE + 2_000,
        "mft_modification_time"
    );
    assert_eq!(si.access_time, T_BASE + 3_000, "access_time");
}

#[test]
fn creation_time_only_set_other_times_unchanged() {
    let (img, path) = fresh_file("crtime");
    // Set all times to a known baseline first.
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(T_BASE),
            modification: Some(T_BASE + 100),
            mft_record_modification: Some(T_BASE + 200),
            access: Some(T_BASE + 300),
        },
    )
    .expect("baseline");

    // Now update only creation.
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(T_BASE + 9_999),
            modification: None,
            mft_record_modification: None,
            access: None,
        },
    )
    .expect("set creation only");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_eq!(si.creation_time, T_BASE + 9_999, "creation updated");
    assert_eq!(si.modification_time, T_BASE + 100, "modification unchanged");
    assert_eq!(
        si.mft_modification_time,
        T_BASE + 200,
        "mft_modification unchanged"
    );
    assert_eq!(si.access_time, T_BASE + 300, "access unchanged");
}

#[test]
fn modification_time_only_set_others_unchanged() {
    let (img, path) = fresh_file("modtime");
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(T_BASE),
            modification: Some(T_BASE),
            mft_record_modification: Some(T_BASE),
            access: Some(T_BASE),
        },
    )
    .expect("baseline");

    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: None,
            modification: Some(T_BASE + 77_777),
            mft_record_modification: None,
            access: None,
        },
    )
    .expect("set mod only");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_eq!(si.creation_time, T_BASE, "creation unchanged");
    assert_eq!(
        si.modification_time,
        T_BASE + 77_777,
        "modification updated"
    );
    assert_eq!(
        si.mft_modification_time, T_BASE,
        "mft_modification unchanged"
    );
    assert_eq!(si.access_time, T_BASE, "access unchanged");
}

#[test]
fn timestamp_at_epoch_zero_roundtrips() {
    let (img, path) = fresh_file("epoch");
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(0),
            modification: Some(0),
            mft_record_modification: Some(0),
            access: Some(0),
        },
    )
    .expect("set to epoch");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_eq!(si.creation_time, 0, "creation at epoch");
    assert_eq!(si.modification_time, 0, "modification at epoch");
    assert_eq!(si.mft_modification_time, 0, "mft_modification at epoch");
    assert_eq!(si.access_time, 0, "access at epoch");
}

#[test]
fn timestamp_at_max_u64_roundtrips() {
    let (img, path) = fresh_file("maxtime");
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(u64::MAX),
            modification: Some(u64::MAX),
            mft_record_modification: Some(u64::MAX),
            access: Some(u64::MAX),
        },
    )
    .expect("set to max");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_eq!(si.creation_time, u64::MAX, "creation at max");
    assert_eq!(si.modification_time, u64::MAX, "modification at max");
}

// ---------------------------------------------------------------------------
// DOS file attribute bits
// ---------------------------------------------------------------------------

#[test]
fn si_archive_bit_set_on_create() {
    let (img, path) = fresh_file("si_archive");
    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_ne!(
        si.file_attributes & file_attr::ARCHIVE,
        0,
        "ARCHIVE must be set on new file"
    );
}

#[test]
fn si_readonly_bit_roundtrip() {
    let (img, path) = fresh_file("si_ro");
    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: file_attr::READONLY,
            remove: 0,
        },
    )
    .expect("set READONLY");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_ne!(
        si.file_attributes & file_attr::READONLY,
        0,
        "READONLY set in $SI"
    );
}

#[test]
fn si_hidden_bit_roundtrip() {
    let (img, path) = fresh_file("si_hidden");
    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: file_attr::HIDDEN,
            remove: 0,
        },
    )
    .expect("set HIDDEN");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_ne!(
        si.file_attributes & file_attr::HIDDEN,
        0,
        "HIDDEN set in $SI"
    );
}

#[test]
fn si_system_bit_roundtrip() {
    let (img, path) = fresh_file("si_system");
    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: file_attr::SYSTEM,
            remove: 0,
        },
    )
    .expect("set SYSTEM");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_ne!(
        si.file_attributes & file_attr::SYSTEM,
        0,
        "SYSTEM set in $SI"
    );
}

#[test]
fn si_multiple_attribute_bits_in_single_call() {
    let (img, path) = fresh_file("si_multi");
    let combined = file_attr::READONLY | file_attr::HIDDEN | file_attr::SYSTEM;
    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: combined,
            remove: 0,
        },
    )
    .expect("set combined");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_ne!(
        si.file_attributes & file_attr::READONLY,
        0,
        "READONLY in combined"
    );
    assert_ne!(
        si.file_attributes & file_attr::HIDDEN,
        0,
        "HIDDEN in combined"
    );
    assert_ne!(
        si.file_attributes & file_attr::SYSTEM,
        0,
        "SYSTEM in combined"
    );
}

#[test]
fn si_remove_attribute_bit_roundtrip() {
    let (img, path) = fresh_file("si_remove");
    // Add HIDDEN, then remove it.
    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: file_attr::HIDDEN,
            remove: 0,
        },
    )
    .expect("add HIDDEN");
    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: 0,
            remove: file_attr::HIDDEN,
        },
    )
    .expect("remove HIDDEN");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_eq!(
        si.file_attributes & file_attr::HIDDEN,
        0,
        "HIDDEN cleared from $SI"
    );
}

// ---------------------------------------------------------------------------
// v3-form fields (security_id, owner_id defaults, quota, usn)
// ---------------------------------------------------------------------------

#[test]
fn fresh_file_has_v3_form_with_zero_defaults() {
    let (img, path) = fresh_file("v3_defaults");
    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    let v3 = si.v3.expect("fresh runtime file must have v3 form");
    assert_eq!(v3.owner_id, 0, "owner_id default = 0");
    assert_eq!(v3.quota, 0, "quota default = 0");
    assert_eq!(v3.usn, 0, "usn default = 0");
    // security_id = 0 on fresh file (no DACL inherited from parent).
    assert_eq!(
        v3.security_id, 0,
        "security_id default = 0 before explicit set"
    );
}

#[test]
fn security_id_roundtrip_via_set_security_id() {
    let (img, path) = fresh_file("secid");
    write::set_security_id(Path::new(&img), &path, 0x100).expect("set_security_id");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    let v3 = si.v3.expect("must have v3 form");
    assert_eq!(v3.security_id, 0x100, "security_id round-trip");
}

#[test]
fn security_id_can_be_changed_twice() {
    let (img, path) = fresh_file("secid2");
    write::set_security_id(Path::new(&img), &path, 0x100).expect("first set");
    write::set_security_id(Path::new(&img), &path, 0x200).expect("second set");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    let v3 = si.v3.expect("v3");
    assert_eq!(v3.security_id, 0x200, "security_id reflects second write");
}

// ---------------------------------------------------------------------------
// Timestamp + attribute independence
// ---------------------------------------------------------------------------

#[test]
fn setting_times_does_not_change_file_attributes() {
    let (img, path) = fresh_file("indep_times");
    // Set HIDDEN attribute.
    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: file_attr::HIDDEN,
            remove: 0,
        },
    )
    .expect("set hidden");

    // Now overwrite all times.
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(T_BASE),
            modification: Some(T_BASE),
            mft_record_modification: Some(T_BASE),
            access: Some(T_BASE),
        },
    )
    .expect("set_times");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_ne!(
        si.file_attributes & file_attr::HIDDEN,
        0,
        "HIDDEN survives set_times"
    );
    assert_eq!(si.creation_time, T_BASE, "creation_time set");
}

#[test]
fn setting_attributes_does_not_change_timestamps() {
    let (img, path) = fresh_file("indep_attrs");
    write::set_times(
        Path::new(&img),
        &path,
        FileTimes {
            creation: Some(T_BASE),
            modification: Some(T_BASE),
            mft_record_modification: Some(T_BASE),
            access: Some(T_BASE),
        },
    )
    .expect("set_times");

    write::set_file_attributes(
        Path::new(&img),
        &path,
        FileAttributesChange {
            add: file_attr::READONLY,
            remove: 0,
        },
    )
    .expect("set attr");

    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert_eq!(
        si.creation_time, T_BASE,
        "creation_time survives set_file_attributes"
    );
    assert_eq!(
        si.modification_time, T_BASE,
        "modification_time survives set_file_attributes"
    );
}
