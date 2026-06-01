//! Phase 3.7 — alternate data streams (ADS), comprehensive.
//!
//! Self-generated volumes (no prebuilt fixture). Every write goes through the
//! public `write::*` path API; every verification reads back through the
//! independent upstream `ntfs` parser, so these catch on-disk shapes our own
//! read path might accept but the canonical parser would reject.
//!
//! Covers: create+read a named stream, multiple streams on one file, the
//! resident→non-resident promotion threshold, delete (default + sibling
//! streams survive), zero-length streams, and long stream names.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_adsx_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("ADSX"),
        Some(0xAD5_C0DE),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Read a named `$DATA` stream's bytes back through the upstream parser.
/// Returns None if the file has no `$DATA` attribute with that name.
fn read_stream_via_upstream(img: &str, file: &str, stream: &str) -> Option<Vec<u8>> {
    let f = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("ntfs");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let root = ntfs.root_directory(&mut reader).expect("root");
    let idx = root.directory_index(&mut reader).expect("idx");
    let mut finder = idx.finder();
    let name = file.trim_start_matches('/');
    let entry = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, name)
        .expect("present")
        .expect("find ok");
    let fobj = entry.to_file(&ntfs, &mut reader).expect("to_file");

    let mut attrs = fobj.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        // `a.name()` is a Result; the unnamed $DATA (primary stream) yields
        // Ok("") here, so an empty `stream` matches it. Treat a name error as
        // the unnamed case too, for robustness.
        let is_match = match a.name() {
            Ok(n) => n.to_string_lossy() == stream,
            Err(_) => stream.is_empty(),
        };
        if !is_match {
            continue;
        }
        let mut v = a.value(&mut reader).expect("value");
        let mut buf = vec![0u8; v.len() as usize];
        let mut pos = 0;
        while pos < buf.len() {
            let n = v.read(&mut reader, &mut buf[pos..]).expect("read");
            if n == 0 {
                break;
            }
            pos += n;
        }
        return Some(buf);
    }
    None
}

#[test]
fn create_and_read_named_stream() {
    let img = fresh_vol("create");
    write::create_file(Path::new(&img), "/", "f.txt").expect("create");
    let payload = b"alternate stream contents";
    write::write_named_stream(Path::new(&img), "/f.txt", "meta", payload).expect("write ads");
    assert_eq!(
        read_stream_via_upstream(&img, "/f.txt", "meta").as_deref(),
        Some(payload.as_slice())
    );
}

#[test]
fn multiple_streams_on_one_file() {
    let img = fresh_vol("multi");
    write::create_file(Path::new(&img), "/", "f.txt").expect("create");
    for (name, data) in [
        ("a", &b"aaa"[..]),
        ("bb", &b"bbbb"[..]),
        ("ccc", &b"ccccc"[..]),
    ] {
        write::write_named_stream(Path::new(&img), "/f.txt", name, data).expect("write ads");
    }
    let listed = write::list_named_streams(Path::new(&img), "/f.txt").expect("list");
    for name in ["a", "bb", "ccc"] {
        assert!(
            listed.iter().any(|s| s == name),
            "stream {name} missing from {listed:?}"
        );
    }
    assert_eq!(
        read_stream_via_upstream(&img, "/f.txt", "bb").as_deref(),
        Some(&b"bbbb"[..])
    );
}

#[test]
fn stream_promotes_resident_to_nonresident_past_threshold() {
    let img = fresh_vol("promote");
    write::create_file(Path::new(&img), "/", "big.txt").expect("create");
    // Larger than a resident value can hold → must go non-resident, intact.
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    write::write_named_stream(Path::new(&img), "/big.txt", "data", &payload).expect("write ads");
    assert_eq!(
        read_stream_via_upstream(&img, "/big.txt", "data").as_deref(),
        Some(payload.as_slice()),
        "non-resident ADS content must round-trip exactly"
    );
}

#[test]
fn delete_stream_leaves_default_and_siblings_intact() {
    let img = fresh_vol("delete");
    write::create_file(Path::new(&img), "/", "f.txt").expect("create");
    write::write_resident_contents(Path::new(&img), "/f.txt", b"primary").expect("primary");
    write::write_named_stream(Path::new(&img), "/f.txt", "keep", b"KEEP").expect("keep");
    write::write_named_stream(Path::new(&img), "/f.txt", "drop", b"DROP").expect("drop");

    write::delete_named_stream(Path::new(&img), "/f.txt", "drop").expect("delete");

    // Dropped stream gone; sibling + primary unnamed $DATA intact.
    assert_eq!(read_stream_via_upstream(&img, "/f.txt", "drop"), None);
    assert_eq!(
        read_stream_via_upstream(&img, "/f.txt", "keep").as_deref(),
        Some(&b"KEEP"[..])
    );
    assert_eq!(
        read_stream_via_upstream(&img, "/f.txt", "").as_deref(),
        Some(&b"primary"[..])
    );
}

#[test]
fn delete_missing_stream_errors() {
    let img = fresh_vol("delmissing");
    write::create_file(Path::new(&img), "/", "f.txt").expect("create");
    assert!(write::delete_named_stream(Path::new(&img), "/f.txt", "nope").is_err());
}

#[test]
fn zero_length_stream_exists_and_reads_empty() {
    let img = fresh_vol("zero");
    write::create_file(Path::new(&img), "/", "f.txt").expect("create");
    write::write_named_stream(Path::new(&img), "/f.txt", "empty", b"").expect("write empty ads");
    let listed = write::list_named_streams(Path::new(&img), "/f.txt").expect("list");
    assert!(
        listed.iter().any(|s| s == "empty"),
        "zero-length stream must still be listed"
    );
    assert_eq!(
        read_stream_via_upstream(&img, "/f.txt", "empty").as_deref(),
        Some(&b""[..])
    );
}

#[test]
fn long_stream_name_roundtrips() {
    let img = fresh_vol("longname");
    write::create_file(Path::new(&img), "/", "f.txt").expect("create");
    // 64-char stream name (well within NTFS's 255-char attribute-name limit).
    let name: String = "s".repeat(64);
    write::write_named_stream(Path::new(&img), "/f.txt", &name, b"x").expect("write long-name ads");
    let listed = write::list_named_streams(Path::new(&img), "/f.txt").expect("list");
    assert!(
        listed.iter().any(|s| s == &name),
        "long stream name missing from {listed:?}"
    );
}
