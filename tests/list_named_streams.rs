//! Tests for `write::list_named_streams` — enumerate every named
//! `$DATA` stream (ADS) on a file, excluding the unnamed primary
//! `$DATA`.

use fs_ntfs::write;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_lns_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn file_with_no_ads_returns_empty() {
    let img = working_copy("empty");
    let names = write::list_named_streams(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert!(names.is_empty(), "expected no named streams, got {names:?}");
}

#[test]
fn lists_one_named_stream() {
    let img = working_copy("one");
    write::write_named_stream(Path::new(&img), "/Documents/readme.txt", "tags", b"hello")
        .expect("write_named_stream");
    let names = write::list_named_streams(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(names, vec!["tags".to_string()]);
}

#[test]
fn lists_multiple_named_streams_in_insertion_order() {
    let img = working_copy("many");
    // Our writer appends in insertion order (the NTFS spec sorts by
    // name; we don't enforce that here — list_named_streams returns
    // the on-disk record order verbatim, and callers can sort).
    write::write_named_stream(Path::new(&img), "/Documents/readme.txt", "zeta", b"z").unwrap();
    write::write_named_stream(Path::new(&img), "/Documents/readme.txt", "alpha", b"a").unwrap();
    write::write_named_stream(Path::new(&img), "/Documents/readme.txt", "mu", b"m").unwrap();
    let names = write::list_named_streams(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(
        names,
        vec!["zeta".to_string(), "alpha".to_string(), "mu".to_string()]
    );
    // And callers can produce a canonical ordering by sorting.
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec!["alpha".to_string(), "mu".to_string(), "zeta".to_string()]
    );
}

#[test]
fn excludes_unnamed_primary_data() {
    // The basic image has /hello.txt with body content; the unnamed
    // $DATA carrying that body must NOT appear in the list.
    let img = working_copy("primary");
    let names = write::list_named_streams(Path::new(&img), "/hello.txt").unwrap();
    assert!(
        names.is_empty(),
        "unnamed primary $DATA leaked into the named-stream list: {names:?}"
    );

    // Add one named stream and verify only the named one appears.
    write::write_named_stream(Path::new(&img), "/hello.txt", "side", b"x").unwrap();
    let names = write::list_named_streams(Path::new(&img), "/hello.txt").unwrap();
    assert_eq!(names, vec!["side".to_string()]);
}

#[test]
fn reflects_state_after_delete() {
    let img = working_copy("after_delete");
    write::write_named_stream(Path::new(&img), "/Documents/readme.txt", "a", b"1").unwrap();
    write::write_named_stream(Path::new(&img), "/Documents/readme.txt", "b", b"2").unwrap();
    write::delete_named_stream(Path::new(&img), "/Documents/readme.txt", "a").unwrap();
    let names = write::list_named_streams(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(names, vec!["b".to_string()]);
}

#[test]
fn unicode_stream_name_roundtrips() {
    let img = working_copy("unicode");
    write::write_named_stream(Path::new(&img), "/Documents/readme.txt", "café", b"c").unwrap();
    let names = write::list_named_streams(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(names, vec!["café".to_string()]);
}
