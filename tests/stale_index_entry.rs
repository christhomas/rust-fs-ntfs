//! A name resolves to a record only if the record is still that file.
//!
//! An MFT slot is reused, and a recycled record continues its sequence
//! rather than restarting at 1 (#256). An index entry left behind by an
//! interrupted unlink still names the slot and still carries the OLD
//! sequence, so following it without comparing hands back whatever now
//! lives there — under the name the caller asked for. Not a missing file:
//! the wrong file's contents, served as the right one (#257).

mod common;

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{mft_io, read, write};
use std::path::Path;

const VOL: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = common::temp_image_path(format!("stale_{tag}"));
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL, CLUSTER, CLUSTER, Some("STALE"), Some(9)).expect("format");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// An ordinary lookup is unaffected: the entry's sequence and the
/// record's agree, because the writer put both there.
#[test]
fn a_live_entry_still_resolves() {
    let img = volume("live");
    let p = Path::new(&img);
    write::create_file(p, "/", "live.txt").expect("create");
    write::write_file_contents(p, "/live.txt", b"here").expect("write");

    let mut io = PathIo::open_ro(p).expect("open_ro");
    let rec = read::resolve_path(&mut io, "/live.txt").expect("resolve a live entry");
    assert!(rec > 0);
}

/// The record's sequence and type are changed behind the index's back, which
/// models the consistency boundary left by an interrupted metadata update:
/// the entry still points at the slot and retains its old duplicate fields,
/// while the slot has moved on. Listing is index-only and still returns that
/// snapshot; lookup reads the target and must refuse it.
#[test]
fn a_stale_entry_is_listed_from_index_bytes_but_refused_when_followed() {
    let img = volume("stale");
    let p = Path::new(&img);
    write::create_file(p, "/", "victim.txt").expect("create");

    let mut io = PathIo::open_rw(p).expect("open_rw");
    let rec = read::resolve_path(&mut io, "/victim.txt").expect("resolve before");

    // Bump the record's sequence and change its authoritative type, leaving
    // the index entry as it was (a regular file with the old sequence).
    let (_params, record) = mft_io::read_mft_record_io(&mut io, rec).expect("read record");
    let was = mft_io::record_sequence(&record);
    let now = was.wrapping_add(1).max(1);
    mft_io::update_mft_record_io(&mut io, rec, |r| {
        r[0x10..0x12].copy_from_slice(&now.to_le_bytes());
        let flags = u16::from_le_bytes([r[0x16], r[0x17]]) | 0x0002;
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        Ok(())
    })
    .expect("write the record back with a new sequence");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");

    let root = read::read_dir_entries(&mut io, read::ROOT_RECORD_NUMBER)
        .expect("an index-only listing does not read the stale target");
    let listed = root
        .iter()
        .find(|entry| entry.name == "victim.txt")
        .expect("the stale index row remains visible");
    assert_eq!(listed.record_number, rec);
    assert!(
        !listed.is_dir,
        "is_dir is the stale regular-file bit copied from the index entry"
    );

    let (_, changed_record) =
        mft_io::read_mft_record_io(&mut io, rec).expect("read changed record");
    assert_ne!(
        mft_io::record_flags(&changed_record) & 0x0002,
        0,
        "the target record now says directory, proving listing did not stat it"
    );

    let err = read::resolve_path(&mut io, "/victim.txt")
        .expect_err("a stale index entry must not resolve");
    assert!(
        err.contains("stale") && err.contains(&rec.to_string()),
        "the error names the record and says the entry is stale, got: {err}"
    );
}
