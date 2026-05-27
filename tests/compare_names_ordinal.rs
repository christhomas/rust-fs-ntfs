//! Tests for `compare_names_ordinal` — the case-sensitive comparator
//! that a `FILE_ATTRIBUTE_CASE_SENSITIVE_DIR` directory would use.
//! Wiring it into `find_index_entry` is the follow-up (future-features.md
//! §3.9); for now this just exercises the building-block function.

use fs_ntfs::index_io::compare_names_ordinal;
use std::cmp::Ordering;

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[test]
fn ordinal_distinguishes_case() {
    let a = utf16("foo.txt");
    let b = utf16("FOO.TXT");
    assert_ne!(compare_names_ordinal(&a, &b), Ordering::Equal);
    // 'f' (0x66) > 'F' (0x46) under raw UTF-16 ordering.
    assert_eq!(compare_names_ordinal(&a, &b), Ordering::Greater);
}

#[test]
fn ordinal_equal_for_same_bytes() {
    let a = utf16("matched.txt");
    let b = utf16("matched.txt");
    assert_eq!(compare_names_ordinal(&a, &b), Ordering::Equal);
}

#[test]
fn ordinal_shorter_name_sorts_first() {
    let a = utf16("foo");
    let b = utf16("foobar");
    assert_eq!(compare_names_ordinal(&a, &b), Ordering::Less);
    assert_eq!(compare_names_ordinal(&b, &a), Ordering::Greater);
}

#[test]
fn ordinal_handles_non_ascii() {
    // 'é' = U+00E9 (sorts after pure ASCII). 'É' = U+00C9.
    let lower = utf16("café");
    let upper = utf16("CAFÉ");
    // 'é' (0xE9) > 'É' (0xC9) by code-point; 'c' > 'C' too.
    assert_eq!(compare_names_ordinal(&lower, &upper), Ordering::Greater);
}
