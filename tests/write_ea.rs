//! Tests for W4.3 Extended Attributes.

use fs_ntfs::{ea_io, write};
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_ea_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn encode_decode_roundtrip() {
    let entries = vec![
        ea_io::Ea {
            flags: 0,
            name: b"FOO".to_vec(),
            value: b"bar".to_vec(),
        },
        ea_io::Ea {
            flags: ea_io::FLAG_NEED_EA,
            name: b"HELLO.WORLD".to_vec(),
            value: (0u8..32).collect(),
        },
    ];
    let packed = ea_io::encode(&entries).unwrap();
    let decoded = ea_io::decode(&packed).unwrap();
    assert_eq!(decoded, entries);
    assert_eq!(ea_io::count_need_ea(&entries), 1);
}

#[test]
fn write_single_ea_roundtrip() {
    let img = working_copy("single");
    write::write_ea(
        Path::new(&img),
        "/Documents/readme.txt",
        b"AUTHOR",
        b"alice@example.com",
        0,
    )
    .expect("write ea");

    let eas = write::list_eas(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(eas.len(), 1);
    assert_eq!(eas[0].name, b"AUTHOR");
    assert_eq!(eas[0].value, b"alice@example.com");
}

#[test]
fn write_multiple_eas() {
    let img = working_copy("multi");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"A", b"aaa", 0).unwrap();
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"B", b"bbbb", 0).unwrap();
    write::write_ea(
        Path::new(&img),
        "/Documents/readme.txt",
        b"C",
        b"ccccc",
        ea_io::FLAG_NEED_EA,
    )
    .unwrap();

    let eas = write::list_eas(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(eas.len(), 3);
    let by_name: std::collections::HashMap<&[u8], &[u8]> = eas
        .iter()
        .map(|e| (e.name.as_slice(), e.value.as_slice()))
        .collect();
    assert_eq!(by_name[b"A".as_slice()], b"aaa");
    assert_eq!(by_name[b"B".as_slice()], b"bbbb");
    assert_eq!(by_name[b"C".as_slice()], b"ccccc");
    // NEED_EA count is visible via upstream-readable $EA_INFORMATION.
    assert_eq!(ea_io::count_need_ea(&eas), 1);
}

#[test]
fn upsert_replaces_same_name() {
    let img = working_copy("upsert");
    write::write_ea(
        Path::new(&img),
        "/Documents/readme.txt",
        b"KEY",
        b"first",
        0,
    )
    .unwrap();
    write::write_ea(
        Path::new(&img),
        "/Documents/readme.txt",
        b"KEY",
        b"second",
        0,
    )
    .unwrap();
    let eas = write::list_eas(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(eas.len(), 1);
    assert_eq!(eas[0].value, b"second");
}

#[test]
fn upsert_is_case_insensitive_on_name() {
    let img = working_copy("case");
    write::write_ea(
        Path::new(&img),
        "/Documents/readme.txt",
        b"Key",
        b"lower",
        0,
    )
    .unwrap();
    write::write_ea(
        Path::new(&img),
        "/Documents/readme.txt",
        b"KEY",
        b"upper",
        0,
    )
    .unwrap();
    let eas = write::list_eas(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(eas.len(), 1);
    assert_eq!(eas[0].value, b"upper");
}

#[test]
fn remove_ea_works() {
    let img = working_copy("remove");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"X", b"xxx", 0).unwrap();
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"Y", b"yyy", 0).unwrap();
    write::remove_ea(Path::new(&img), "/Documents/readme.txt", b"X").unwrap();

    let eas = write::list_eas(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert_eq!(eas.len(), 1);
    assert_eq!(eas[0].name, b"Y");
}

#[test]
fn remove_missing_errors() {
    let img = working_copy("remove_missing");
    let err = write::remove_ea(Path::new(&img), "/Documents/readme.txt", b"NOPE").unwrap_err();
    assert!(err.contains("not found"), "{err:?}");
}

#[test]
fn remove_last_ea_clears_both_attributes() {
    let img = working_copy("clear");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"ONLY", b"x", 0).unwrap();
    write::remove_ea(Path::new(&img), "/Documents/readme.txt", b"ONLY").unwrap();
    let eas = write::list_eas(Path::new(&img), "/Documents/readme.txt").unwrap();
    assert!(eas.is_empty());
}

#[test]
fn reject_empty_ea_name() {
    let img = working_copy("empty_name");
    let err = write::write_ea(Path::new(&img), "/Documents/readme.txt", b"", b"v", 0).unwrap_err();
    assert!(err.contains("invalid"), "{err:?}");
}

#[test]
fn upstream_mounts_after_ea_churn() {
    let img = working_copy("churn");
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"A", b"a", 0).unwrap();
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"B", b"b", 0).unwrap();
    write::remove_ea(Path::new(&img), "/Documents/readme.txt", b"A").unwrap();
    write::write_ea(Path::new(&img), "/Documents/readme.txt", b"C", b"c", 0).unwrap();

    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
