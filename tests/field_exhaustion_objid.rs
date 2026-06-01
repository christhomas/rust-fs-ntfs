//! Phase 2.8 field-exhaustion tests for `$OBJECT_ID`.
//!
//! Verifies the 16-byte short form and the 64-byte extended form
//! (object_id + birth-volume + birth-object + birth-domain GUIDs)
//! round-trip byte-for-byte, including all-zero birth GUIDs, mixed
//! zero/non-zero GUIDs, overwrite semantics, and that the short form
//! reads back with `birth_ids = None`. Every test formats its own
//! in-memory volume — no fixture images required.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

/// Format a fresh volume into a temp image and return its path.
fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_fex_oid_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("OIDTEST"),
        Some(0xFEED_0B1D),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Create a file in a fresh volume; return (img_path, "/name").
fn fresh_file(tag: &str) -> (String, String) {
    let img = fresh_vol(tag);
    let name = format!("oid_{tag}.bin");
    write::create_file(Path::new(&img), "/", &name).expect("create_file");
    (img, format!("/{name}"))
}

/// A recognisable 16-byte GUID pattern with `seed` mixed into each byte.
fn guid(seed: u8) -> [u8; 16] {
    let mut g = [0u8; 16];
    for (i, b) in g.iter_mut().enumerate() {
        *b = seed.wrapping_add(i as u8);
    }
    g
}

// ---------------------------------------------------------------------------
// 16-byte short form
// ---------------------------------------------------------------------------

#[test]
fn short_form_roundtrips() {
    let (img, path) = fresh_file("short");
    let oid = guid(0x10);
    write::write_object_id(Path::new(&img), &path, &oid).expect("write_object_id");
    let back = write::read_object_id(Path::new(&img), &path)
        .expect("read_object_id")
        .expect("present");
    assert_eq!(back, oid, "16-byte object_id round-trip");
}

#[test]
fn short_form_reads_back_with_no_birth_ids() {
    let (img, path) = fresh_file("short_nobirth");
    let oid = guid(0x20);
    write::write_object_id(Path::new(&img), &path, &oid).expect("write");
    let ext = write::read_object_id_extended(Path::new(&img), &path)
        .expect("read_extended")
        .expect("present");
    assert_eq!(ext.object_id, oid, "object_id matches");
    assert!(
        ext.birth_ids.is_none(),
        "16-byte form must read back with birth_ids = None"
    );
}

#[test]
fn short_form_all_zero_guid_roundtrips() {
    let (img, path) = fresh_file("short_zero");
    let oid = [0u8; 16];
    write::write_object_id(Path::new(&img), &path, &oid).expect("write");
    let back = write::read_object_id(Path::new(&img), &path)
        .expect("read")
        .expect("present");
    assert_eq!(back, [0u8; 16], "all-zero object_id round-trip");
}

#[test]
fn short_form_all_ff_guid_roundtrips() {
    let (img, path) = fresh_file("short_ff");
    let oid = [0xFFu8; 16];
    write::write_object_id(Path::new(&img), &path, &oid).expect("write");
    let back = write::read_object_id(Path::new(&img), &path)
        .expect("read")
        .expect("present");
    assert_eq!(back, [0xFFu8; 16], "all-0xFF object_id round-trip");
}

// ---------------------------------------------------------------------------
// 64-byte extended form
// ---------------------------------------------------------------------------

#[test]
fn extended_form_all_four_guids_roundtrip() {
    let (img, path) = fresh_file("ext_all");
    let oid = guid(0x01);
    let bv = guid(0x40);
    let bo = guid(0x80);
    let bd = guid(0xC0);
    write::write_object_id_extended(Path::new(&img), &path, &oid, &bv, &bo, &bd)
        .expect("write_extended");
    let ext = write::read_object_id_extended(Path::new(&img), &path)
        .expect("read_extended")
        .expect("present");
    assert_eq!(ext.object_id, oid, "object_id");
    let (rbv, rbo, rbd) = ext.birth_ids.expect("birth_ids present for 64-byte form");
    assert_eq!(rbv, bv, "birth_volume_id");
    assert_eq!(rbo, bo, "birth_object_id");
    assert_eq!(rbd, bd, "birth_domain_id");
}

#[test]
fn extended_form_zero_birth_guids_roundtrip() {
    // Object ID present, all three birth GUIDs zero — still the 64-byte form.
    let (img, path) = fresh_file("ext_zerobirth");
    let oid = guid(0x05);
    let zero = [0u8; 16];
    write::write_object_id_extended(Path::new(&img), &path, &oid, &zero, &zero, &zero)
        .expect("write_extended");
    let ext = write::read_object_id_extended(Path::new(&img), &path)
        .expect("read_extended")
        .expect("present");
    assert_eq!(ext.object_id, oid);
    let (rbv, rbo, rbd) = ext.birth_ids.expect("64-byte form keeps birth_ids slot");
    assert_eq!(rbv, zero);
    assert_eq!(rbo, zero);
    assert_eq!(rbd, zero);
}

#[test]
fn extended_form_mixed_zero_and_nonzero_birth_guids() {
    let (img, path) = fresh_file("ext_mixed");
    let oid = guid(0x07);
    let zero = [0u8; 16];
    let nonzero = guid(0x33);
    // birth_volume = nonzero, birth_object = zero, birth_domain = nonzero.
    write::write_object_id_extended(Path::new(&img), &path, &oid, &nonzero, &zero, &nonzero)
        .expect("write_extended");
    let ext = write::read_object_id_extended(Path::new(&img), &path)
        .expect("read_extended")
        .expect("present");
    let (rbv, rbo, rbd) = ext.birth_ids.expect("birth_ids present");
    assert_eq!(rbv, nonzero, "birth_volume nonzero");
    assert_eq!(rbo, zero, "birth_object zero");
    assert_eq!(rbd, nonzero, "birth_domain nonzero");
}

#[test]
fn extended_form_object_id_readable_via_short_reader() {
    // The 16-byte short reader must still return the leading object_id
    // even when the on-disk attribute is the 64-byte extended form.
    let (img, path) = fresh_file("ext_shortread");
    let oid = guid(0x09);
    let bv = guid(0x11);
    let bo = guid(0x22);
    let bd = guid(0x44);
    write::write_object_id_extended(Path::new(&img), &path, &oid, &bv, &bo, &bd)
        .expect("write_extended");
    let short = write::read_object_id(Path::new(&img), &path)
        .expect("read short")
        .expect("present");
    assert_eq!(
        short, oid,
        "short reader returns leading object_id of extended form"
    );
}

// ---------------------------------------------------------------------------
// Overwrite + absence
// ---------------------------------------------------------------------------

#[test]
fn overwrite_short_with_different_short() {
    let (img, path) = fresh_file("overwrite");
    write::write_object_id(Path::new(&img), &path, &guid(0x01)).expect("first");
    write::write_object_id(Path::new(&img), &path, &guid(0x99)).expect("second");
    let back = write::read_object_id(Path::new(&img), &path)
        .expect("read")
        .expect("present");
    assert_eq!(back, guid(0x99), "second write wins");
}

#[test]
fn file_without_object_id_reads_none() {
    let (img, path) = fresh_file("noid");
    // Never write an object id.
    let back = write::read_object_id(Path::new(&img), &path).expect("read");
    assert!(back.is_none(), "file with no $OBJECT_ID must read None");
}

#[test]
fn random_guid_bytes_stored_verbatim() {
    // No validation constraints on object_id — arbitrary bytes survive.
    let (img, path) = fresh_file("random");
    let oid: [u8; 16] = [
        0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x13,
        0x37,
    ];
    write::write_object_id(Path::new(&img), &path, &oid).expect("write");
    let back = write::read_object_id(Path::new(&img), &path)
        .expect("read")
        .expect("present");
    assert_eq!(back, oid, "arbitrary GUID bytes stored verbatim");
}
