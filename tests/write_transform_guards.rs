//! The write paths refuse a `$DATA` they cannot correctly write to.
//!
//! `write_at`, `truncate` and `grow` each guard against a transformed
//! `$DATA` — compressed, sparse or encrypted — because none of them
//! knows how to maintain the extra state such an attribute carries.
//! All three tested `flags & 0x00FF`, which is the compression-unit
//! field alone; sparse is `0x8000` and encrypted is `0x4000`, the top
//! two bits, and neither is in that mask. The comment inside the guard
//! said the low byte carried all three, and the error message the guard
//! produced named two conditions it could not detect.
//!
//! A sparse file is what NTFS makes of anything written with holes, and
//! this crate writes them itself (`write::write_sparse_file`). An
//! encrypted one is a checkbox in the Windows file-properties dialog.

mod common;

use fs_ntfs::attr_io::{self, attr_off, AttrType};
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{mft_io, read, write};
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;
/// `$DATA` is encrypted ($EFS). `read.rs` has had a constant for this
/// since it learned to refuse such a value.
const ATTR_FLAG_ENCRYPTED: u16 = 0x4000;
/// `$DATA` is sparse. Nothing in the crate named this bit.
const ATTR_FLAG_SPARSE: u16 = 0x8000;

fn fresh_volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = common::temp_image_path(format!("xform_{tag}"));
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("XFRM"), Some(5))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// A 3-cluster non-resident file, all of it written.
fn plain_file(tag: &str) -> String {
    let img = fresh_volume(tag);
    write::create_file(Path::new(&img), "/", "f.bin").expect("create");
    write::write_file_contents(Path::new(&img), "/f.bin", &vec![b'x'; 3 * CLUSTER as usize])
        .expect("write contents");
    img
}

/// Set a bit in the unnamed `$DATA` attribute's flags field.
fn set_data_flag(img: &str, flag: u16) {
    let mut io = PathIo::open_rw(Path::new(img)).expect("open_rw");
    let rec = read::resolve_path(&mut io, "/f.bin").expect("resolve");
    mft_io::update_mft_record_io(&mut io, rec, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None).ok_or("no $DATA")?;
        assert!(
            !loc.is_resident,
            "the guards under test are non-resident ones"
        );
        let at = loc.attr_offset + attr_off::FLAGS;
        let cur = u16::from_le_bytes([record[at], record[at + 1]]);
        record[at..at + 2].copy_from_slice(&(cur | flag).to_le_bytes());
        Ok(())
    })
    .expect("set the flag");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
}

/// THE REFUSAL, NOT MERELY AN ERROR (#234). `is_err()` alone passes on
/// any failure -- a missing file, a bad path, a bug in the test's own
/// setup -- so it cannot tell the guard working from the guard never
/// running. `refuse_transformed_data` names the entry point and prints
/// the flags it read, and both are checked here: the entry point, so a
/// refusal from somewhere else does not count, and `flags=` so the
/// failure is the one that inspected the attribute.
fn assert_refused_by_the_transform_guard<T: std::fmt::Debug>(
    got: &Result<T, String>,
    entry_point: &str,
    what: &str,
) {
    let Err(e) = got else {
        panic!("{entry_point} accepted a {what} $DATA and reported {got:?}");
    };
    assert!(
        e.starts_with(&format!("{entry_point}: ")),
        "{entry_point} on a {what} $DATA failed for some other reason: {e}"
    );
    assert!(
        e.contains("non-resident $DATA is") && e.contains("flags="),
        "{entry_point} on a {what} $DATA did not fail in the transform guard: {e}"
    );
}

fn assert_all_three_refuse(img: &str, what: &str) {
    let p = Path::new(img);
    let wrote = write::write_at(p, "/f.bin", 0, &[b'Z'; 512]);
    assert_refused_by_the_transform_guard(&wrote, "write_at", what);
    let shrunk = write::truncate(p, "/f.bin", CLUSTER as u64);
    assert_refused_by_the_transform_guard(&shrunk, "truncate", what);
    let grown = write::grow_nonresident(p, "/f.bin", 8 * CLUSTER as u64);
    assert_refused_by_the_transform_guard(&grown, "grow", what);
}

#[test]
fn an_encrypted_data_attribute_is_refused_by_all_three_entry_points() {
    let img = plain_file("encrypted");
    set_data_flag(&img, ATTR_FLAG_ENCRYPTED);
    assert_all_three_refuse(&img, "0x4000 (encrypted)");
}

#[test]
fn a_sparse_flagged_data_attribute_is_refused_by_all_three_entry_points() {
    let img = plain_file("sparse_bit");
    set_data_flag(&img, ATTR_FLAG_SPARSE);
    assert_all_three_refuse(&img, "0x8000 (sparse)");
}

#[test]
fn a_real_sparse_file_is_refused_even_where_its_clusters_are_mapped() {
    // Not a hand-set bit: a file this crate wrote as sparse, with the
    // extended 0x48 header and a genuine hole. The existing hole check
    // rejects a write that lands IN the hole; the point here is the
    // first cluster, which is allocated, and writing it leaves the
    // sparse accounting (`total_allocated_size` at +0x40) untouched.
    let img = fresh_volume("real_sparse");
    write::create_file(Path::new(&img), "/", "f.bin").expect("create");
    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 3 * cs];
    data[0..cs].fill(0xAA);
    data[2 * cs..3 * cs].fill(0xBB); // cluster 1 stays a hole
    write::write_sparse_file(Path::new(&img), "/f.bin", &data).expect("write_sparse_file");

    let mut probe = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let rec = read::resolve_path(&mut probe, "/f.bin").expect("resolve");
    let (_, record) = mft_io::read_mft_record_io(&mut probe, rec).expect("read record");
    let loc = attr_io::find_attribute(&record, AttrType::Data, None).expect("$DATA");
    let flags = u16::from_le_bytes([
        record[loc.attr_offset + attr_off::FLAGS],
        record[loc.attr_offset + attr_off::FLAGS + 1],
    ]);
    assert_eq!(
        flags & ATTR_FLAG_SPARSE,
        ATTR_FLAG_SPARSE,
        "the fixture has to actually be sparse; flags={flags:#06x}"
    );
    drop(probe);

    let wrote = write::write_at(Path::new(&img), "/f.bin", 0, &[b'Z'; 512]);
    assert!(
        wrote.is_err(),
        "write_at accepted a real sparse $DATA at an allocated cluster and \
         reported {wrote:?}; the sparse size accounting is now stale"
    );
}

#[test]
fn an_ordinary_non_resident_file_is_still_writable() {
    // The mask got wider; it must not have got so wide that a plain
    // $DATA trips it.
    let img = plain_file("plain");
    let p = Path::new(&img);
    write::write_at(p, "/f.bin", 0, &[b'Z'; 512]).expect("write_at on a plain file");
    write::grow_nonresident(p, "/f.bin", 8 * CLUSTER as u64).expect("grow on a plain file");
    write::truncate(p, "/f.bin", CLUSTER as u64).expect("truncate on a plain file");
}
