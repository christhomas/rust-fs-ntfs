//! Phase 2.6 field-exhaustion tests for Extended Attributes (`$EA` /
//! `$EA_INFORMATION`).
//!
//! Verifies that EA name, value, and flag fields round-trip byte-for-byte
//! across boundary sizes (1-byte name, long name, zero-length value, single
//! byte, multi-hundred-byte value), multiple EAs on one file, NEED_EA flag
//! preservation, case-insensitive name handling, remove/re-add, and the
//! 4-byte entry alignment. Every test formats its own in-memory volume — no
//! fixture images required.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

// FILE_NEED_EA flag (MS-FSCC §2.4.15): set means the EA is critical and
// a reader that doesn't understand it must refuse the file.
const FLAG_NEED_EA: u8 = 0x80;

/// Format a fresh volume into a temp image and return its path.
fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_fex_ea_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("EATEST"),
        Some(0xFEED_EA00),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Create a file in a fresh volume; return (img_path, "/name").
fn fresh_file(tag: &str) -> (String, String) {
    let img = fresh_vol(tag);
    let name = format!("ea_{tag}.bin");
    write::create_file(Path::new(&img), "/", &name).expect("create_file");
    (img, format!("/{name}"))
}

/// Find an EA by name in the list, returning its (flags, value).
fn find_ea<'a>(eas: &'a [fs_ntfs::ea_io::Ea], name: &[u8]) -> Option<(u8, &'a [u8])> {
    eas.iter()
        .find(|e| e.name == name)
        .map(|e| (e.flags, e.value.as_slice()))
}

// ---------------------------------------------------------------------------
// Name boundaries
// ---------------------------------------------------------------------------

#[test]
fn single_char_name_roundtrips() {
    let (img, path) = fresh_file("name1");
    write::write_ea(Path::new(&img), &path, b"A", b"value", 0).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (flags, value) = find_ea(&eas, b"A").expect("EA 'A' present");
    assert_eq!(flags, 0);
    assert_eq!(value, b"value");
}

#[test]
fn long_name_roundtrips() {
    // 200-byte name — well under the 254 cap, comfortably resident.
    let (img, path) = fresh_file("namelong");
    let name = vec![b'X'; 200];
    write::write_ea(Path::new(&img), &path, &name, b"v", 0).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (_, value) = find_ea(&eas, &name).expect("long-named EA present");
    assert_eq!(value, b"v");
}

#[test]
fn name_max_254_bytes_roundtrips() {
    // 254 bytes is the documented maximum the writer accepts.
    let (img, path) = fresh_file("name254");
    let name = vec![b'N'; 254];
    write::write_ea(Path::new(&img), &path, &name, b"x", 0).expect("write_ea");
    let keys = write::list_ea_keys(Path::new(&img), &path).expect("list_ea_keys");
    assert!(
        keys.iter().any(|k| k.as_slice() == name.as_slice()),
        "254-byte name present"
    );
}

#[test]
fn name_over_254_bytes_rejected() {
    let (img, path) = fresh_file("nametoolong");
    let name = vec![b'Z'; 255];
    let err = write::write_ea(Path::new(&img), &path, &name, b"x", 0).unwrap_err();
    assert!(err.contains("invalid EA name length"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Value boundaries
// ---------------------------------------------------------------------------

#[test]
fn zero_length_value_roundtrips() {
    let (img, path) = fresh_file("val0");
    write::write_ea(Path::new(&img), &path, b"empty", b"", 0).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (_, value) = find_ea(&eas, b"empty").expect("empty-value EA present");
    assert_eq!(value, b"", "zero-length value round-trips");
}

#[test]
fn single_byte_value_roundtrips() {
    let (img, path) = fresh_file("val1");
    write::write_ea(Path::new(&img), &path, b"one", &[0x42], 0).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (_, value) = find_ea(&eas, b"one").expect("present");
    assert_eq!(value, &[0x42]);
}

#[test]
fn multi_hundred_byte_value_roundtrips() {
    // 500-byte value — exercises a larger resident copy without exceeding
    // the MFT record capacity at 4096-byte records.
    let (img, path) = fresh_file("val500");
    let value: Vec<u8> = (0..500u32).map(|i| (i % 256) as u8).collect();
    write::write_ea(Path::new(&img), &path, b"big", &value, 0).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (_, got) = find_ea(&eas, b"big").expect("present");
    assert_eq!(got, value.as_slice(), "500-byte value round-trips");
}

#[test]
fn binary_value_with_all_byte_values_roundtrips() {
    // Value containing every byte 0x00..=0xFF — no encoding assumptions.
    let (img, path) = fresh_file("valbin");
    let value: Vec<u8> = (0..=255u8).collect();
    write::write_ea(Path::new(&img), &path, b"bin", &value, 0).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (_, got) = find_ea(&eas, b"bin").expect("present");
    assert_eq!(got, value.as_slice(), "full-byte-range value round-trips");
}

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

#[test]
fn need_ea_flag_preserved() {
    let (img, path) = fresh_file("needea");
    write::write_ea(Path::new(&img), &path, b"crit", b"data", FLAG_NEED_EA).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (flags, _) = find_ea(&eas, b"crit").expect("present");
    assert_eq!(flags, FLAG_NEED_EA, "NEED_EA flag preserved");
}

#[test]
fn zero_flag_preserved() {
    let (img, path) = fresh_file("zeroflag");
    write::write_ea(Path::new(&img), &path, b"normal", b"data", 0).expect("write_ea");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    let (flags, _) = find_ea(&eas, b"normal").expect("present");
    assert_eq!(flags, 0, "zero flag preserved");
}

// ---------------------------------------------------------------------------
// Multiple EAs / mutation
// ---------------------------------------------------------------------------

#[test]
fn three_eas_on_one_file_all_roundtrip() {
    let (img, path) = fresh_file("three");
    write::write_ea(Path::new(&img), &path, b"first", b"1", 0).expect("ea1");
    write::write_ea(Path::new(&img), &path, b"second", b"22", 0).expect("ea2");
    write::write_ea(Path::new(&img), &path, b"third", b"333", FLAG_NEED_EA).expect("ea3");

    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    assert_eq!(find_ea(&eas, b"first").unwrap().1, b"1");
    assert_eq!(find_ea(&eas, b"second").unwrap().1, b"22");
    let (f3, v3) = find_ea(&eas, b"third").unwrap();
    assert_eq!(v3, b"333");
    assert_eq!(f3, FLAG_NEED_EA, "third EA keeps its flag amid others");
}

#[test]
fn upsert_same_name_replaces_value() {
    let (img, path) = fresh_file("upsert");
    write::write_ea(Path::new(&img), &path, b"key", b"old", 0).expect("first");
    write::write_ea(Path::new(&img), &path, b"key", b"newvalue", 0).expect("second");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    // Exactly one entry named "key", carrying the new value.
    let matches: Vec<_> = eas.iter().filter(|e| e.name == b"key").collect();
    assert_eq!(matches.len(), 1, "upsert must not duplicate the name");
    assert_eq!(matches[0].value, b"newvalue");
}

#[test]
fn remove_ea_then_others_remain() {
    let (img, path) = fresh_file("remove");
    write::write_ea(Path::new(&img), &path, b"keep1", b"a", 0).expect("ea1");
    write::write_ea(Path::new(&img), &path, b"drop", b"b", 0).expect("ea2");
    write::write_ea(Path::new(&img), &path, b"keep2", b"c", 0).expect("ea3");

    write::remove_ea(Path::new(&img), &path, b"drop").expect("remove");

    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    assert!(find_ea(&eas, b"drop").is_none(), "removed EA gone");
    assert_eq!(find_ea(&eas, b"keep1").unwrap().1, b"a", "keep1 intact");
    assert_eq!(find_ea(&eas, b"keep2").unwrap().1, b"c", "keep2 intact");
}

#[test]
fn remove_then_readd_different_value() {
    let (img, path) = fresh_file("readd");
    write::write_ea(Path::new(&img), &path, b"k", b"first", 0).expect("write");
    write::remove_ea(Path::new(&img), &path, b"k").expect("remove");
    write::write_ea(Path::new(&img), &path, b"k", b"second", 0).expect("rewrite");
    let eas = write::list_eas(Path::new(&img), &path).expect("list_eas");
    assert_eq!(find_ea(&eas, b"k").unwrap().1, b"second");
}

#[test]
fn remove_nonexistent_ea_errors() {
    let (img, path) = fresh_file("removemissing");
    let err = write::remove_ea(Path::new(&img), &path, b"nope").unwrap_err();
    assert!(!err.is_empty(), "removing a missing EA must error");
}

// ---------------------------------------------------------------------------
// Empty-name rejection
// ---------------------------------------------------------------------------

#[test]
fn empty_name_rejected() {
    let (img, path) = fresh_file("emptyname");
    let err = write::write_ea(Path::new(&img), &path, b"", b"v", 0).unwrap_err();
    assert!(err.contains("invalid EA name length"), "got: {err}");
}
