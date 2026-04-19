//! Tests for NTFS upcase-table loading + collation (§1.1).

use fs_ntfs::upcase::UpcaseTable;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

#[test]
fn load_upcase_from_basic_fixture() {
    let t = UpcaseTable::load(Path::new(BASIC_IMG)).expect("load");
    // ASCII upper-casing must work.
    assert_eq!(t.upcase(b'a' as u16), b'A' as u16);
    assert_eq!(t.upcase(b'Z' as u16), b'Z' as u16);
    assert_eq!(t.upcase(b'0' as u16), b'0' as u16);
}

#[test]
fn upcase_handles_latin_extended() {
    let t = UpcaseTable::load(Path::new(BASIC_IMG)).expect("load");
    // 'ä' (U+00E4) should upcase to 'Ä' (U+00C4) under NTFS's table.
    assert_eq!(t.upcase(0x00E4), 0x00C4);
    // 'ß' (U+00DF) is usually preserved (no single-codepoint uppercase
    // in NTFS's table — Unicode convention is it upcases to "SS" which
    // a single-u16 map can't represent).
    let beta = t.upcase(0x00DF);
    assert!(beta == 0x00DF || beta == 0x1E9E, "ß upcase = {beta:#x}");
}

#[test]
fn case_insensitive_compare_matches_upstream_expectations() {
    let t = UpcaseTable::load(Path::new(BASIC_IMG)).expect("load");
    let a: Vec<u16> = "hello".encode_utf16().collect();
    let b: Vec<u16> = "HELLO".encode_utf16().collect();
    assert_eq!(t.cmp_names(&a, &b), std::cmp::Ordering::Equal);

    let c: Vec<u16> = "abc".encode_utf16().collect();
    let d: Vec<u16> = "ABD".encode_utf16().collect();
    assert_eq!(t.cmp_names(&c, &d), std::cmp::Ordering::Less);
}

#[test]
fn upcase_ordering_handles_non_ascii() {
    let t = UpcaseTable::load(Path::new(BASIC_IMG)).expect("load");
    // "über" vs "zebra" — ü (U+00FC) upcases to Ü (U+00DC), which is
    // > 'Z' (0x5A). So "über" > "zebra".
    let a: Vec<u16> = "über".encode_utf16().collect();
    let b: Vec<u16> = "zebra".encode_utf16().collect();
    assert_eq!(t.cmp_names(&a, &b), std::cmp::Ordering::Greater);
}

#[test]
fn prefix_is_less_than_full_name() {
    let t = UpcaseTable::load(Path::new(BASIC_IMG)).expect("load");
    let a: Vec<u16> = "read".encode_utf16().collect();
    let b: Vec<u16> = "readme".encode_utf16().collect();
    assert_eq!(t.cmp_names(&a, &b), std::cmp::Ordering::Less);
}
