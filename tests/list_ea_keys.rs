//! Tests for `write::list_ea_keys` — cheap enumeration that returns
//! only the EA name bytes (skipping the values).

use fs_ntfs::write;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_lek_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn file_with_no_eas_returns_empty() {
    let img = working_copy("empty");
    let keys = write::list_ea_keys(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert!(keys.is_empty(), "expected no EAs, got {keys:?}");
}

#[test]
fn lists_single_ea_key() {
    let img = working_copy("one");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"FOO", b"bar", 0).unwrap();
    let keys = write::list_ea_keys(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(keys, vec![b"FOO".to_vec()]);
}

#[test]
fn lists_multiple_ea_keys_in_insertion_order() {
    let img = working_copy("many");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"ALPHA", b"a", 0).unwrap();
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"BETA", b"b", 0).unwrap();
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"GAMMA", b"g", 0).unwrap();
    let keys = write::list_ea_keys(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(
        keys,
        vec![b"ALPHA".to_vec(), b"BETA".to_vec(), b"GAMMA".to_vec()]
    );
}

#[test]
fn returns_keys_only_not_values() {
    // Even with large values, list_ea_keys must not materialise them.
    let img = working_copy("only_keys");
    let big = vec![0xAAu8; 200];
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"HEAVY", &big, 0).unwrap();
    let keys = write::list_ea_keys(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(keys, vec![b"HEAVY".to_vec()]);
    // The key vec carries no value bytes — length is just the name.
    assert_eq!(keys[0].len(), 5);
}

#[test]
fn reflects_state_after_remove() {
    let img = working_copy("after_remove");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"KEEP", b"k", 0).unwrap();
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"GONE", b"x", 0).unwrap();
    write::remove_ea(Path::new(&img), "/Documents/readme.txt", b"GONE").unwrap();
    let keys = write::list_ea_keys(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(keys, vec![b"KEEP".to_vec()]);
}

#[test]
fn empty_after_removing_last_ea() {
    let img = working_copy("removed_all");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"SOLE", b"s", 0).unwrap();
    write::remove_ea(Path::new(&img), "/Documents/readme.txt", b"SOLE").unwrap();
    let keys = write::list_ea_keys(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert!(
        keys.is_empty(),
        "expected no EAs after final remove, got {keys:?}"
    );
}
