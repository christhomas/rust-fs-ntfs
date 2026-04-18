//! Tests for `write::write_named_stream_resident` + `delete_named_stream`
//! (W4.1 — Alternate Data Streams).

use fs_ntfs::write;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_ads_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn read_named_stream(img: &str, file_path: &str, stream_name: &str) -> Option<Vec<u8>> {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        let name_matches = match a.name() {
            Ok(n) => n.to_string_lossy() == stream_name,
            Err(_) => false,
        };
        if !name_matches {
            continue;
        }
        let mut v = a.value(&mut r).unwrap();
        v.seek(&mut r, std::io::SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0u8; a.value_length() as usize];
        let mut filled = 0;
        while filled < buf.len() {
            let n = v.read(&mut r, &mut buf[filled..]).unwrap();
            if n == 0 {
                break;
            }
            filled += n;
        }
        return Some(buf);
    }
    None
}

fn list_streams(img: &str, file_path: &str) -> Vec<(String, u64)> {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    let mut attrs = cur.attributes();
    let mut out = Vec::new();
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        let name = match a.name() {
            Ok(n) => n.to_string_lossy(),
            Err(_) => String::new(),
        };
        out.push((name, a.value_length()));
    }
    out
}

#[test]
fn create_new_ads() {
    let img = working_copy("new");
    let payload = b"alice author stream";
    write::write_named_stream_resident(Path::new(&img), "/Documents/readme.txt", "author", payload)
        .expect("write ads");

    let readback = read_named_stream(&img, "/Documents/readme.txt", "author");
    assert_eq!(readback.as_deref(), Some(payload.as_slice()));
}

#[test]
fn multiple_ads_on_same_file() {
    let img = working_copy("multi");
    write::write_named_stream_resident(
        Path::new(&img),
        "/Documents/readme.txt",
        "author",
        b"alice",
    )
    .expect("ads1");
    write::write_named_stream_resident(
        Path::new(&img),
        "/Documents/readme.txt",
        "summary",
        b"one-line summary",
    )
    .expect("ads2");

    assert_eq!(
        read_named_stream(&img, "/Documents/readme.txt", "author"),
        Some(b"alice".to_vec())
    );
    assert_eq!(
        read_named_stream(&img, "/Documents/readme.txt", "summary"),
        Some(b"one-line summary".to_vec())
    );
    // Unnamed $DATA still intact.
    let streams = list_streams(&img, "/Documents/readme.txt");
    assert!(
        streams.iter().any(|(n, l)| n.is_empty() && *l == 22),
        "unnamed $DATA must survive; streams={streams:?}"
    );
}

#[test]
fn ads_overwrite_replaces_content() {
    let img = working_copy("overwrite");
    write::write_named_stream_resident(Path::new(&img), "/Documents/readme.txt", "tag", b"first")
        .unwrap();
    write::write_named_stream_resident(
        Path::new(&img),
        "/Documents/readme.txt",
        "tag",
        b"second replacement",
    )
    .unwrap();

    assert_eq!(
        read_named_stream(&img, "/Documents/readme.txt", "tag"),
        Some(b"second replacement".to_vec())
    );
}

#[test]
fn delete_ads_removes_stream() {
    let img = working_copy("del");
    write::write_named_stream_resident(Path::new(&img), "/Documents/readme.txt", "tag", b"data")
        .unwrap();
    write::delete_named_stream(Path::new(&img), "/Documents/readme.txt", "tag").unwrap();
    assert!(read_named_stream(&img, "/Documents/readme.txt", "tag").is_none());
}

#[test]
fn delete_missing_stream_errors() {
    let img = working_copy("del_missing");
    let err =
        write::delete_named_stream(Path::new(&img), "/Documents/readme.txt", "nope").unwrap_err();
    assert!(err.contains("not found"), "{err:?}");
}

#[test]
fn reject_empty_stream_name() {
    let img = working_copy("empty_name");
    let err =
        write::write_named_stream_resident(Path::new(&img), "/Documents/readme.txt", "", b"x")
            .unwrap_err();
    assert!(err.contains("non-empty"), "{err:?}");
}

#[test]
fn upstream_mounts_after_ads_churn() {
    let img = working_copy("churn");
    write::write_named_stream_resident(Path::new(&img), "/Documents/readme.txt", "s1", b"a")
        .unwrap();
    write::write_named_stream_resident(Path::new(&img), "/Documents/readme.txt", "s2", b"bb")
        .unwrap();
    write::delete_named_stream(Path::new(&img), "/Documents/readme.txt", "s1").unwrap();
    write::write_named_stream_resident(Path::new(&img), "/Documents/readme.txt", "s3", b"ccc")
        .unwrap();

    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
    assert!(read_named_stream(&img, "/Documents/readme.txt", "s2").is_some());
    assert!(read_named_stream(&img, "/Documents/readme.txt", "s3").is_some());
    assert!(read_named_stream(&img, "/Documents/readme.txt", "s1").is_none());
}
