//! Phase 3.10 — targeted structural corruption resistance.
//!
//! Complements `corruption_fuzz.rs` (random bit-flips → "no panic"). These
//! corrupt SPECIFIC on-disk structures (boot BPB, $MFT magic, image length)
//! and assert graceful handling: an invalid boot is REJECTED with an error,
//! and no corruption — however malformed — causes a panic, hang, or OOB.
//! Self-generating (format a fresh volume, then corrupt it).

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_vol(tag: &str) -> String {
    // Self-generating: don't assume test-disks/ already exists (clean checkout).
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_corrupt_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create temp image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("CRPT"),
        Some(0xC0_44_C0_44),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

fn read_all(img: &str) -> Vec<u8> {
    std::fs::read(img).expect("read image")
}
fn write_all(img: &str, bytes: &[u8]) {
    std::fs::write(img, bytes).expect("write image");
}

/// Mount and exercise basic reads; returns Ok(()) if it completed (whether
/// the FS itself returned errors), Err(()) if it PANICKED. A panic on
/// malformed input is the robustness bug we're guarding against.
fn mount_and_probe_caught(img: &str) -> Result<(), ()> {
    let img = img.to_string();
    catch_unwind(AssertUnwindSafe(move || {
        if let Ok(fs) = Filesystem::mount(&img) {
            // Touch a few read paths; ignore their Result — we only care
            // that none of them panic on a corrupt volume.
            let _ = fs.stat("/");
            // Directory-index traversal is a distinct code path (historically a
            // source of bounds bugs on malformed NTFS), so exercise it too.
            let _ = fs.read_dir("/");
            let _ = fs.stat("/probe.txt");
            let mut buf = [0u8; 64];
            let _ = fs.read_file("/probe.txt", 0, &mut buf);
        }
    }))
    .map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Boot BPB — invalid geometry must be REJECTED (not panic, not accepted).
// ---------------------------------------------------------------------------

#[test]
fn non_power_of_two_sectors_per_cluster_rejected() {
    let img = fresh_vol("npot_spc");
    let mut bytes = read_all(&img);
    bytes[0x0D] = 3; // sectors_per_cluster = 3 (not a power of two)
    write_all(&img, &bytes);
    assert!(
        Filesystem::mount(&img).is_err(),
        "mount must reject a non-power-of-two cluster geometry"
    );
}

#[test]
fn zero_bytes_per_sector_rejected() {
    let img = fresh_vol("zero_bps");
    let mut bytes = read_all(&img);
    bytes[0x0B] = 0;
    bytes[0x0C] = 0; // bytes_per_sector = 0
    write_all(&img, &bytes);
    assert!(
        Filesystem::mount(&img).is_err(),
        "mount must reject bytes_per_sector = 0"
    );
}

// ---------------------------------------------------------------------------
// No corruption may panic / hang / read OOB.
// ---------------------------------------------------------------------------

#[test]
fn wiped_boot_sector_resistance_no_panic() {
    let img = fresh_vol("wipe_boot");
    let mut bytes = read_all(&img);
    for b in &mut bytes[0..512] {
        *b = 0;
    }
    write_all(&img, &bytes);
    assert!(
        mount_and_probe_caught(&img).is_ok(),
        "wiped boot sector must not panic"
    );
}

#[test]
fn corrupt_mft_magic_does_not_panic() {
    let img = fresh_vol("mft_magic");
    let mut bytes = read_all(&img);
    // MFT byte offset = mft_lcn * cluster_size. mft_lcn is u64 LE at 0x30.
    let mft_lcn = u64::from_le_bytes(bytes[0x30..0x38].try_into().unwrap());
    let mft_off = (mft_lcn * CLUSTER as u64) as usize;
    // Fail loudly if the MFT offset isn't inside the image — otherwise the
    // corruption below would be silently skipped and the test would prove
    // nothing (it'd just mount a pristine volume).
    assert!(
        mft_off + 4 <= bytes.len(),
        "MFT offset {mft_off} outside image ({}); test would be vacuous",
        bytes.len()
    );
    bytes[mft_off..mft_off + 4].copy_from_slice(b"XXXX"); // clobber "FILE"
    write_all(&img, &bytes);
    assert!(
        mount_and_probe_caught(&img).is_ok(),
        "corrupt $MFT magic must not panic"
    );
}

#[test]
fn truncated_image_resistance_no_panic() {
    let img = fresh_vol("trunc");
    let bytes = read_all(&img);
    write_all(&img, &bytes[..bytes.len() / 2]); // chop the image in half
    assert!(
        mount_and_probe_caught(&img).is_ok(),
        "truncated image must not panic"
    );
}

#[test]
fn zeroed_mft_region_does_not_panic() {
    let img = fresh_vol("zero_mft");
    let mut bytes = read_all(&img);
    let mft_lcn = u64::from_le_bytes(bytes[0x30..0x38].try_into().unwrap());
    let mft_off = (mft_lcn * CLUSTER as u64) as usize;
    let end = (mft_off + 64 * 1024).min(bytes.len());
    // Fail loudly rather than silently skip — otherwise a pristine volume
    // would be tested and the invariant would go unverified.
    assert!(
        mft_off < end,
        "MFT offset {mft_off} outside image ({}); test would be vacuous",
        bytes.len()
    );
    for b in &mut bytes[mft_off..end] {
        *b = 0;
    }
    write_all(&img, &bytes);
    assert!(
        mount_and_probe_caught(&img).is_ok(),
        "zeroed MFT region must not panic"
    );
}
