//! Tests for the MFT read-modify-write primitive.
//!
//! Correctness requires: (a) fixup roundtrip is byte-preserving (modulo
//! USN bump), (b) writes to the USA-controlled sector-end positions do
//! not corrupt record contents, (c) upstream `ntfs` still parses a volume
//! we've touched with an identity RMW.

use fs_ntfs::mft_io::{
    apply_fixup_on_read, apply_fixup_on_write, read_boot_params, read_mft_record,
    update_mft_record, MFT_FLAG_IN_USE,
};
use ntfs::KnownNtfsFileRecordNumber;
use std::io::{Read, Seek, SeekFrom};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_mft_io_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy basic fixture");
    dst
}

#[test]
fn boot_params_match_fixture_geometry() {
    let params = read_boot_params(std::path::Path::new(BASIC_IMG)).expect("boot");
    // mkntfs on a 16 MiB image with `-c 4096` defaults:
    assert_eq!(params.bytes_per_sector, 512);
    assert_eq!(params.sectors_per_cluster, 8);
    assert_eq!(params.cluster_size, 4096);
    // file_record_size is typically 1024 bytes on small volumes (clusters_per_mft_record = -10).
    assert_eq!(params.file_record_size, 1024);
    // Plausible MFT LCN range (mkntfs lays MFT a few clusters in).
    assert!(params.mft_lcn > 0 && params.mft_lcn < 100);
}

#[test]
fn read_mft_record_returns_volume_record_with_magic() {
    let (_params, record) = read_mft_record(
        std::path::Path::new(BASIC_IMG),
        KnownNtfsFileRecordNumber::Volume as u64,
    )
    .expect("read $Volume record");
    assert_eq!(&record[0..4], b"FILE", "record must have FILE magic");
    let flags = u16::from_le_bytes([record[0x16], record[0x17]]);
    assert!(flags & MFT_FLAG_IN_USE != 0, "$Volume must be in use");
}

#[test]
fn fixup_roundtrip_is_byte_preserving_modulo_usn_bump() {
    // Identity RMW bumps the USN but should keep all non-USN-controlled
    // bytes byte-identical. Read the raw (pre-fixup) record before and
    // after; compare everything outside of the USA and sector-end slots.
    let img = working_copy("identity");

    let before_raw = read_raw_record(&img, KnownNtfsFileRecordNumber::Volume as u64);
    update_mft_record(
        std::path::Path::new(&img),
        KnownNtfsFileRecordNumber::Volume as u64,
        |_| Ok(()),
    )
    .expect("identity rmw");
    let after_raw = read_raw_record(&img, KnownNtfsFileRecordNumber::Volume as u64);

    let usa_offset = u16::from_le_bytes([before_raw[0x04], before_raw[0x05]]) as usize;
    let usa_count = u16::from_le_bytes([before_raw[0x06], before_raw[0x07]]) as usize;
    let bps = 512usize;

    // Every byte NOT in (USA array ∪ sector-end USN slots) must be identical.
    let mut diffs = 0;
    for i in 0..before_raw.len() {
        if in_usa_or_sector_end(i, usa_offset, usa_count, bps) {
            continue;
        }
        if before_raw[i] != after_raw[i] {
            diffs += 1;
            if diffs < 5 {
                eprintln!(
                    "byte {i:#x}: before={:#x} after={:#x}",
                    before_raw[i], after_raw[i]
                );
            }
        }
    }
    assert_eq!(diffs, 0, "unexpected byte changes outside USA/sector-end");

    // USN must have advanced by exactly 1.
    let before_usn = u16::from_le_bytes([before_raw[usa_offset], before_raw[usa_offset + 1]]);
    let after_usn = u16::from_le_bytes([after_raw[usa_offset], after_raw[usa_offset + 1]]);
    assert_eq!(
        after_usn,
        before_usn.wrapping_add(1).max(1),
        "USN should bump by 1"
    );

    // Each sector-end slot must now carry the new USN.
    let sectors = usa_count - 1;
    for s in 0..sectors {
        let end = (s + 1) * bps - 2;
        let observed = u16::from_le_bytes([after_raw[end], after_raw[end + 1]]);
        assert_eq!(observed, after_usn, "sector {s} end should hold new USN");
    }
}

#[test]
fn upstream_still_parses_image_after_identity_rmw() {
    let img = working_copy("upstream_e2e");
    update_mft_record(
        std::path::Path::new(&img),
        KnownNtfsFileRecordNumber::Volume as u64,
        |_| Ok(()),
    )
    .expect("identity rmw");

    // Upstream mount + volume_info must still succeed and return the
    // original label.
    let f = std::fs::File::open(&img).expect("open");
    let mut r = std::io::BufReader::new(f);
    let ntfs = ntfs::Ntfs::new(&mut r).expect("parse post-RMW");
    let vn = ntfs
        .volume_name(&mut r)
        .expect("volume name present")
        .expect("volume name read");
    assert_eq!(vn.name().to_string_lossy(), "BasicNTFS");
}

#[test]
fn apply_fixup_round_trip_in_memory() {
    // Build a minimal synthetic FILE record (1024 bytes, 2 sectors).
    let mut rec = vec![0u8; 1024];
    rec[0..4].copy_from_slice(b"FILE");
    rec[0x04..0x06].copy_from_slice(&42u16.to_le_bytes()); // USA at +0x2A
    rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // USA count: 1 USN + 2 sectors
                                                          // USA[0] = USN
    rec[42..44].copy_from_slice(&0x1234u16.to_le_bytes());
    // USA[1..3] = saved sector-end values
    rec[44..46].copy_from_slice(&0xAABBu16.to_le_bytes());
    rec[46..48].copy_from_slice(&0xCCDDu16.to_le_bytes());
    // Sector-end slots carry the USN (as the on-disk encoding requires).
    rec[510..512].copy_from_slice(&0x1234u16.to_le_bytes());
    rec[1022..1024].copy_from_slice(&0x1234u16.to_le_bytes());
    // Known interior bytes we'll verify don't change through the round trip.
    rec[100] = 0x55;
    rec[600] = 0x66;

    // Read fixup: sector-end slots should become 0xAABB / 0xCCDD.
    apply_fixup_on_read(&mut rec, 512).expect("fixup on read");
    assert_eq!(u16::from_le_bytes([rec[510], rec[511]]), 0xAABB);
    assert_eq!(u16::from_le_bytes([rec[1022], rec[1023]]), 0xCCDD);
    assert_eq!(rec[100], 0x55);
    assert_eq!(rec[600], 0x66);

    // Write fixup: USN bumps to 0x1235, sector-ends flip to new USN.
    apply_fixup_on_write(&mut rec, 512).expect("fixup on write");
    assert_eq!(u16::from_le_bytes([rec[42], rec[43]]), 0x1235);
    assert_eq!(u16::from_le_bytes([rec[510], rec[511]]), 0x1235);
    assert_eq!(u16::from_le_bytes([rec[1022], rec[1023]]), 0x1235);
    // USA slots 1..3 store the values we just overwrote (0xAABB / 0xCCDD).
    assert_eq!(u16::from_le_bytes([rec[44], rec[45]]), 0xAABB);
    assert_eq!(u16::from_le_bytes([rec[46], rec[47]]), 0xCCDD);
    // Interior bytes untouched.
    assert_eq!(rec[100], 0x55);
    assert_eq!(rec[600], 0x66);
}

#[test]
fn apply_fixup_on_read_rejects_non_file_record() {
    let mut rec = vec![0u8; 1024];
    rec[0..4].copy_from_slice(b"INDX");
    let err = apply_fixup_on_read(&mut rec, 512).unwrap_err();
    assert!(err.contains("FILE"), "{err:?}");
}

#[test]
fn apply_fixup_on_read_detects_torn_write() {
    // Corrupt one sector's end bytes so they no longer match the USN —
    // must produce a USN mismatch error (not silently succeed).
    let mut rec = vec![0u8; 1024];
    rec[0..4].copy_from_slice(b"FILE");
    rec[0x04..0x06].copy_from_slice(&42u16.to_le_bytes());
    rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
    rec[42..44].copy_from_slice(&0x1234u16.to_le_bytes());
    rec[44..46].copy_from_slice(&0xAABBu16.to_le_bytes());
    rec[46..48].copy_from_slice(&0xCCDDu16.to_le_bytes());
    rec[510..512].copy_from_slice(&0x1234u16.to_le_bytes());
    // Second sector's USN is tampered — simulating a torn write.
    rec[1022..1024].copy_from_slice(&0x9999u16.to_le_bytes());

    let err = apply_fixup_on_read(&mut rec, 512).unwrap_err();
    assert!(err.contains("USN mismatch"), "{err:?}");
}

#[test]
fn update_refuses_to_write_free_record() {
    // MFT record 0 (i.e. $MFT itself) is always in-use; we need an
    // unused record to exercise the guard. Since fresh volumes don't
    // have obvious free records we can target by number, build a test
    // by clearing the in-use flag of a scratch record in memory and
    // using the in-memory primitives. That's apply_fixup; for the
    // file-level guard, we instead attempt to write to a record number
    // beyond any plausibly-allocated range.
    // MFT record 1000 on a 16 MiB volume is almost certainly free
    // (fixture has <50 real files).
    let img = working_copy("free_record");
    let err = update_mft_record(std::path::Path::new(&img), 1000, |_| Ok(())).unwrap_err();
    // Either the record doesn't parse (no FILE magic / all zeros) or
    // it's recognized as free — either is acceptable. Both are refusals,
    // which is the contract.
    assert!(
        err.contains("IN_USE") || err.contains("FILE") || err.contains("USA"),
        "expected refusal-to-write; got {err:?}"
    );
}

#[test]
fn update_propagates_mutator_error() {
    let img = working_copy("propagate_err");
    let err = update_mft_record(
        std::path::Path::new(&img),
        KnownNtfsFileRecordNumber::Volume as u64,
        |_| Err("test-only sentinel".to_string()),
    )
    .unwrap_err();
    assert!(err.contains("sentinel"), "{err:?}");
}

// ---- helpers ----

/// Read the raw (pre-fixup) record bytes straight from disk, without
/// running any fixup. Used by tests to verify our writes by inspecting
/// the raw on-disk state.
fn read_raw_record(path: &str, record_number: u64) -> Vec<u8> {
    let params = read_boot_params(std::path::Path::new(path)).expect("boot");
    let off = params.mft_lcn * params.cluster_size + record_number * params.file_record_size;
    let mut f = std::fs::File::open(path).expect("open");
    f.seek(SeekFrom::Start(off)).expect("seek");
    let mut buf = vec![0u8; params.file_record_size as usize];
    f.read_exact(&mut buf).expect("read");
    buf
}

fn in_usa_or_sector_end(i: usize, usa_offset: usize, usa_count: usize, bps: usize) -> bool {
    let usa_end = usa_offset + usa_count * 2;
    if i >= usa_offset && i < usa_end {
        return true;
    }
    // Sector-end 2-byte slots.
    let pos_in_sector = i % bps;
    if pos_in_sector == bps - 2 || pos_in_sector == bps - 1 {
        return true;
    }
    false
}
