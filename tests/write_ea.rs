//! Tests for W4.3 Extended Attributes.

mod common;

use fs_ntfs::{ea_io, write};
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = common::temp_image_path(format!("ea_{tag}"));
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
    assert_eq!(ea_io::count_need_ea(&eas), 1);

    // Read the raw summary through the independent `ntfs` crate. With exactly
    // one NEED_EA entry, this distinguishes the u16 count at 0x02 from both
    // length fields.
    let query_len = ea_io::encode(&eas).unwrap().len() as u32;
    let packed_len = ea_io::packed_ea_length(&eas).unwrap();
    let (ntfs, mut reader) = common::open(&img);
    let file = common::navigate(&ntfs, &mut reader, "/Documents/readme.txt");
    let mut attributes = file.attributes();
    let mut information = None;
    while let Some(item) = attributes.next(&mut reader) {
        let item = item.expect("attribute item");
        let attribute = item.to_attribute().expect("attribute");
        if attribute.ty().expect("attribute type") == NtfsAttributeType::EAInformation {
            assert_eq!(attribute.value_length(), 8);
            let mut value = attribute.value(&mut reader).expect("$EA_INFORMATION value");
            let mut bytes = [0u8; 8];
            assert_eq!(value.read(&mut reader, &mut bytes).expect("read value"), 8);
            information = Some(bytes);
            break;
        }
    }
    let information = information.expect("$EA_INFORMATION attribute");
    assert_eq!(
        u16::from_le_bytes(information[0..2].try_into().unwrap()),
        packed_len
    );
    assert_eq!(u16::from_le_bytes(information[2..4].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(information[4..8].try_into().unwrap()),
        query_len
    );
    assert_ne!(u32::from(packed_len), query_len);
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

#[test]
fn ea_value_persists_after_remount() {
    // Write an EA, then re-open the image from scratch via our own API
    // and verify the value is still there. Catches flush-without-sync
    // or header-only-write bugs that survive in-session reads.
    let img = working_copy("remount");
    write::write_ea(
        std::path::Path::new(&img),
        "/Documents/readme.txt",
        b"PERSIST_KEY",
        b"persist_value_123",
        0,
    )
    .expect("write ea");

    // Re-open: use our list_eas entry point which re-parses the MFT.
    let eas = write::list_eas(std::path::Path::new(&img), "/Documents/readme.txt")
        .expect("list_eas after remount");
    let found = eas.iter().find(|e| e.name == b"PERSIST_KEY");
    assert!(found.is_some(), "EA not found after remount");
    assert_eq!(found.unwrap().value, b"persist_value_123");
}

#[test]
fn ea_survives_second_ea_added_and_remount() {
    // Two EAs written; verify both survive a re-open.
    let img = working_copy("two_remount");
    write::write_ea(
        std::path::Path::new(&img),
        "/Documents/readme.txt",
        b"FIRST",
        b"aaa",
        0,
    )
    .unwrap();
    write::write_ea(
        std::path::Path::new(&img),
        "/Documents/readme.txt",
        b"SECOND",
        b"bbb",
        0,
    )
    .unwrap();

    let eas =
        write::list_eas(std::path::Path::new(&img), "/Documents/readme.txt").expect("list_eas");
    assert_eq!(eas.len(), 2);
    assert!(eas.iter().any(|e| e.name == b"FIRST" && e.value == b"aaa"));
    assert!(eas.iter().any(|e| e.name == b"SECOND" && e.value == b"bbb"));
}
