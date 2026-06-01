//! Phase 2.7 field-exhaustion tests for `$REPARSE_POINT`.
//!
//! Verifies that the reparse tag and reparse-data buffer round-trip
//! byte-for-byte across the full range of known tags (symlink, mount
//! point, WOF, LX_SYMLINK, APPEXECLINK) plus arbitrary/unknown tags,
//! and across data-buffer boundary sizes (zero-length, 1 byte, large).
//! Also confirms the IS_REPARSE_POINT bit propagates into
//! `$STANDARD_INFORMATION` file attributes. Every test formats its own
//! in-memory volume — no fixture images required.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write::{self, read_si_full};
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

// NTFS reparse tags (MS-FSCC §2.1.2.1). Mirrors record_build::reparse_tag.
const TAG_SYMLINK: u32 = 0xA000_000C;
const TAG_MOUNT_POINT: u32 = 0xA000_0003;
const TAG_WOF: u32 = 0x8000_0017;
const TAG_LX_SYMLINK: u32 = 0xA000_001D;
const TAG_APPEXECLINK: u32 = 0x8000_001B;

// FILE_ATTRIBUTE_REPARSE_POINT (MS-FSCC) — denormalized into $STD_INFO.
const FA_REPARSE_POINT: u32 = 0x0000_0400;

/// Format a fresh volume into a temp image and return its path.
fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_fex_rp_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("RPTEST"),
        Some(0xFEED_BEEF),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Create a file in a fresh volume; return (img_path, "/name").
fn fresh_file(tag: &str) -> (String, String) {
    let img = fresh_vol(tag);
    let name = format!("rp_{tag}.bin");
    write::create_file(Path::new(&img), "/", &name).expect("create_file");
    (img, format!("/{name}"))
}

/// Write a reparse point and read it back, asserting tag + data match.
fn assert_reparse_roundtrip(tag_name: &str, reparse_tag: u32, data: &[u8]) {
    let (img, path) = fresh_file(tag_name);
    write::write_reparse_point(Path::new(&img), &path, reparse_tag, data)
        .expect("write_reparse_point");
    let rp = write::read_reparse_point(Path::new(&img), &path)
        .expect("read_reparse_point")
        .expect("reparse point present");
    assert_eq!(rp.reparse_tag, reparse_tag, "{tag_name}: tag round-trip");
    assert_eq!(rp.data, data, "{tag_name}: data round-trip");
}

// ---------------------------------------------------------------------------
// Known reparse tags — each must round-trip with a representative payload.
// ---------------------------------------------------------------------------

#[test]
fn symlink_tag_roundtrips() {
    // Minimal symlink reparse data (substitute + print name offsets/lengths).
    let data = vec![0x00, 0x00, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00];
    assert_reparse_roundtrip("symlink", TAG_SYMLINK, &data);
}

#[test]
fn mount_point_tag_roundtrips() {
    let data = vec![0x00, 0x00, 0x0C, 0x00, 0x0C, 0x00, 0x00, 0x00];
    assert_reparse_roundtrip("mountpoint", TAG_MOUNT_POINT, &data);
}

#[test]
fn wof_tag_roundtrips_opaque_buffer() {
    // WOF buffer is opaque to us — must survive verbatim.
    let data: Vec<u8> = (0..32u8).collect();
    assert_reparse_roundtrip("wof", TAG_WOF, &data);
}

#[test]
fn lx_symlink_tag_roundtrips() {
    // WSL LX symlink: 4-byte version prefix + UTF-8 target.
    let mut data = vec![0x02, 0x00, 0x00, 0x00];
    data.extend_from_slice(b"/usr/bin/sh");
    assert_reparse_roundtrip("lxsymlink", TAG_LX_SYMLINK, &data);
}

#[test]
fn appexeclink_tag_roundtrips() {
    let data: Vec<u8> = (0..48u8).collect();
    assert_reparse_roundtrip("appexec", TAG_APPEXECLINK, &data);
}

// ---------------------------------------------------------------------------
// Arbitrary / unknown tags — driver must store the raw bytes verbatim.
// ---------------------------------------------------------------------------

#[test]
fn unknown_tag_roundtrips_verbatim() {
    let data = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    assert_reparse_roundtrip("unknown", 0xC0DE_1234, &data);
}

#[test]
fn microsoft_high_bit_tag_roundtrips() {
    // 0x8000_xxxx = Microsoft-reserved, no name-surrogate bit.
    let data = vec![1, 2, 3, 4, 5, 6, 7, 8];
    assert_reparse_roundtrip("msbit", 0x8000_00FF, &data);
}

// ---------------------------------------------------------------------------
// Data-buffer boundary sizes.
// ---------------------------------------------------------------------------

#[test]
fn zero_length_data_roundtrips() {
    // Tag-only reparse point — zero data bytes.
    assert_reparse_roundtrip("zerolen", TAG_SYMLINK, &[]);
}

#[test]
fn single_byte_data_roundtrips() {
    assert_reparse_roundtrip("onebyte", TAG_WOF, &[0x5A]);
}

#[test]
fn large_data_buffer_roundtrips() {
    // 256-byte buffer — comfortably resident, exercises larger copy.
    let data: Vec<u8> = (0..=255u8).collect();
    assert_reparse_roundtrip("large", TAG_WOF, &data);
}

// ---------------------------------------------------------------------------
// IS_REPARSE_POINT flag propagation into $STANDARD_INFORMATION.
// ---------------------------------------------------------------------------

#[test]
fn writing_reparse_sets_reparse_point_flag_in_std_info() {
    let (img, path) = fresh_file("flagprop");
    let data = vec![0u8; 8];
    write::write_reparse_point(Path::new(&img), &path, TAG_SYMLINK, &data)
        .expect("write_reparse_point");
    let si = read_si_full(Path::new(&img), &path).expect("read_si_full");
    assert!(
        si.file_attributes & FA_REPARSE_POINT != 0,
        "IS_REPARSE_POINT bit must be set in $STD_INFO after writing reparse point; \
         got attrs={:#010x}",
        si.file_attributes
    );
}

// ---------------------------------------------------------------------------
// Overwrite: writing a second reparse point replaces the first.
// ---------------------------------------------------------------------------

#[test]
fn second_write_replaces_first_reparse_point() {
    let (img, path) = fresh_file("overwrite");
    write::write_reparse_point(
        Path::new(&img),
        &path,
        TAG_SYMLINK,
        &[1, 2, 3, 4, 5, 6, 7, 8],
    )
    .expect("first write");
    write::write_reparse_point(Path::new(&img), &path, TAG_WOF, &[9, 8, 7, 6])
        .expect("second write");
    let rp = write::read_reparse_point(Path::new(&img), &path)
        .expect("read")
        .expect("present");
    assert_eq!(rp.reparse_tag, TAG_WOF, "tag replaced");
    assert_eq!(rp.data, vec![9, 8, 7, 6], "data replaced");
}
