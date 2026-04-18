//! ads fixture: file with two named $DATA streams (author, summary).

mod common;

use ntfs::{NtfsAttributeType, NtfsReadSeek};

const IMG: &str = "test-disks/ntfs-ads.img";

#[test]
fn primary_data_unchanged() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/tagged.txt");
    assert_eq!(content, b"primary data\n");
}

#[test]
fn named_streams_enumerable() {
    let (ntfs, mut reader) = common::open(IMG);
    let file = common::navigate(&ntfs, &mut reader, "/tagged.txt");

    let mut stream_names = Vec::new();
    let mut attrs = file.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let attr = item.to_attribute().expect("to_attr");
        if attr.ty().expect("ty") != NtfsAttributeType::Data {
            continue;
        }
        match attr.name() {
            Ok(n) if !n.is_empty() => stream_names.push(n.to_string_lossy()),
            _ => {}
        }
    }

    assert!(
        stream_names.iter().any(|n| n == "author"),
        "streams={stream_names:?}"
    );
    assert!(
        stream_names.iter().any(|n| n == "summary"),
        "streams={stream_names:?}"
    );
}

#[test]
fn read_named_stream_content() {
    let (ntfs, mut reader) = common::open(IMG);
    let file = common::navigate(&ntfs, &mut reader, "/tagged.txt");

    let item = file
        .data(&mut reader, "author")
        .expect("author stream missing")
        .expect("author stream err");
    let attr = item.to_attribute().expect("to_attr");
    let mut value = attr.value(&mut reader).expect("value");

    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let n = value.read(&mut reader, &mut chunk).expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(buf, b"alice author stream\n");
}

#[test]
fn file_without_streams_has_none() {
    let (ntfs, mut reader) = common::open(IMG);
    let file = common::navigate(&ntfs, &mut reader, "/plain.txt");

    let mut attrs = file.attributes();
    let mut named_count = 0;
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let attr = item.to_attribute().expect("to_attr");
        if attr.ty().expect("ty") == NtfsAttributeType::Data
            && attr.name().map(|n| !n.is_empty()).unwrap_or(false)
        {
            named_count += 1;
        }
    }
    assert_eq!(named_count, 0);
}
