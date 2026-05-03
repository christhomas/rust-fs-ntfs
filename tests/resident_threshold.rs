//! Resident / non-resident `$DATA` boundary sweep.
//!
//! NTFS keeps small `$DATA` payloads inline in the MFT record (resident)
//! and promotes to `$INDEX_ALLOCATION`-style cluster lists once the
//! payload outgrows the record. The exact threshold depends on cluster
//! size, MFT record size, and how full the record already is from
//! `$STANDARD_INFORMATION` / `$FILE_NAME` / etc.
//!
//! Bugs cluster at this boundary: the resident → non-resident promotion
//! reshuffles attribute layout, allocates clusters, rewrites the
//! resident-flag byte, and may reorder following attributes. Off-by-one
//! sizes (one byte under vs one byte over the threshold) are a classic
//! escape hatch.
//!
//! These tests sweep file sizes through the threshold zone and around
//! cluster boundaries. The contract under test:
//!
//!   for any size N: write_file_contents(path, [byte]*N)
//!                   followed by read_file(path, 0, ..) returns the
//!                   same N bytes, byte-for-byte, regardless of whether
//!                   the driver chose resident or non-resident layout.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

const VOL_SIZE: u64 = 32 * 1024 * 1024;

fn fresh_volume(tag: &str) -> String {
    let dst = format!("test-disks/_resthresh_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        4096,
        4096,
        Some("RTHRESH"),
        Some(0xCAFEF00D),
    )
    .expect("format");
    io.sync().expect("sync");
    drop(io);
    dst
}

/// Pseudo-random fill so a stale read can't pass by reading uninitialized
/// bytes that happen to be zero. Deterministic per (size, seed_byte).
fn pattern(size: usize, seed: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut s = seed as u32;
    for _ in 0..size {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        out.push((s >> 16) as u8);
    }
    out
}

fn write_and_read_back(fs: &Filesystem, path: &str, data: &[u8]) {
    fs.write_file_contents(path, data).expect("write");
    let stat = fs.stat(path).expect("stat");
    assert_eq!(stat.size, data.len() as u64, "stat size mismatch");

    let mut buf = vec![0u8; data.len()];
    let n = fs.read_file(path, 0, &mut buf).expect("read");
    assert_eq!(
        n,
        data.len(),
        "read truncated: got {n}, expected {}",
        data.len()
    );
    if buf != data {
        let first_diff = buf
            .iter()
            .zip(data.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "content mismatch at offset {first_diff} (file size {}): got {:#x}, want {:#x}",
            data.len(),
            buf[first_diff],
            data[first_diff],
        );
    }
}

/// Sweep through the resident / non-resident boundary in 1-byte
/// increments. Each size gets its own fresh file so a write-induced
/// promotion in one iteration can't poison the next.
///
/// Chunked into per-dir batches of 8 so the directory's resident
/// $INDEX_ROOT never overflows — the driver doesn't yet promote
/// $INDEX_ROOT → $INDEX_ALLOCATION on growth via create_file.
#[test]
fn sweep_around_resident_threshold() {
    let img = fresh_volume("sweep_threshold");
    let fs = Filesystem::mount(&img).unwrap();

    // The threshold for 4096-byte MFT records sits in the 600..1100
    // band. Walk a ±100-byte window around the most likely values; if
    // the boundary moves we still cover both sides.
    // mkfs.ntfs reserves a fixed initial MFT of 64 records (cf.
    // src/mkfs.rs:191) and the driver doesn't grow it. With ~12
    // system records that leaves ~50 user records. Stride is set so
    // total file+dir count stays under that ceiling.
    let sizes: Vec<usize> = (500..=1200).step_by(17).collect();
    const PER_DIR: usize = 8;
    for (i, &size) in sizes.iter().enumerate() {
        let bucket = i / PER_DIR;
        let parent = format!("/b{bucket:02}");
        if i % PER_DIR == 0 {
            fs.mkdir("/", &parent[1..]).unwrap();
        }
        let name = format!("rt_{i:03}.bin");
        fs.create_file(&parent, &name).unwrap();
        let payload = pattern(size, (i & 0xFF) as u8);
        write_and_read_back(&fs, &format!("{parent}/{name}"), &payload);
    }
}

/// File sizes at exact powers of two — these are the values most often
/// hard-coded in user code (4 KiB, 8 KiB, …). Each must round-trip
/// exactly.
#[test]
fn power_of_two_sizes_roundtrip() {
    let img = fresh_volume("po2_sizes");
    let fs = Filesystem::mount(&img).unwrap();
    for shift in 0..=18u32 {
        // 1 byte .. 256 KiB
        let size = 1usize << shift;
        let name = format!("po2_{shift:02}.bin");
        fs.create_file("/", &name).unwrap();
        let payload = pattern(size, shift as u8);
        write_and_read_back(&fs, &format!("/{name}"), &payload);
    }
}

/// Files sized at exactly cluster_size, ±1 byte. Forces the
/// non-resident allocator to deal with both "fits in N clusters
/// exactly" and "needs N+1 clusters with most of the last empty".
#[test]
fn cluster_boundary_sizes_roundtrip() {
    let img = fresh_volume("cluster_boundary");
    let fs = Filesystem::mount(&img).unwrap();
    let cluster = 4096usize;
    let multipliers: &[usize] = &[1, 2, 3, 4, 8, 16, 32];
    for (i, &m) in multipliers.iter().enumerate() {
        for delta in [-1i64, 0, 1] {
            let size = ((m * cluster) as i64 + delta) as usize;
            let name = format!(
                "cb_{i:02}_{}.bin",
                if delta < 0 {
                    "m1"
                } else if delta == 0 {
                    "0"
                } else {
                    "p1"
                }
            );
            fs.create_file("/", &name).unwrap();
            let payload = pattern(size, ((i * 3) as u8).wrapping_add(delta as u8));
            write_and_read_back(&fs, &format!("/{name}"), &payload);
        }
    }
}

/// Promotion direction: write a small (resident) payload, then a much
/// larger one to the same file. The new contents must read back
/// without bleed-through from the old resident bytes.
#[test]
fn resident_then_promote_no_bleedthrough() {
    let img = fresh_volume("promote_bleed");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f.bin").unwrap();

    let small = pattern(64, 0xAA);
    fs.write_file_contents("/f.bin", &small).unwrap();
    let mut buf = vec![0u8; 64];
    fs.read_file("/f.bin", 0, &mut buf).unwrap();
    assert_eq!(buf, small);

    // Promote.
    let large = pattern(64 * 1024, 0x55);
    fs.write_file_contents("/f.bin", &large).unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let n = fs.read_file("/f.bin", 0, &mut buf).unwrap();
    assert_eq!(n, 64 * 1024);
    assert_eq!(
        buf, large,
        "non-resident read shouldn't echo resident bytes"
    );
}

/// Read past EOF on a resident file must return only the bytes that
/// exist (no panic, no buffer overrun, no garbage tail).
#[test]
fn read_past_eof_resident_returns_short_read() {
    let img = fresh_volume("eof_resident");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "tiny.bin").unwrap();
    fs.write_file_contents("/tiny.bin", b"abc").unwrap();

    let mut buf = [0xFFu8; 100];
    let n = fs.read_file("/tiny.bin", 0, &mut buf).unwrap();
    assert_eq!(n, 3);
    assert_eq!(&buf[..3], b"abc");
}

/// Zero-byte file: create then read. Must report size 0 and read 0
/// bytes without erroring.
#[test]
fn zero_byte_file_is_well_formed() {
    let img = fresh_volume("zero_file");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "empty.bin").unwrap();
    assert_eq!(fs.stat("/empty.bin").unwrap().size, 0);
    let mut buf = [0u8; 16];
    let n = fs.read_file("/empty.bin", 0, &mut buf).unwrap();
    assert_eq!(n, 0);
}

/// Read at a non-zero offset on a resident file. Must position into
/// the inline bytes correctly, not return from offset 0.
#[test]
fn read_at_offset_resident() {
    let img = fresh_volume("offset_resident");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f.txt").unwrap();
    fs.write_file_contents("/f.txt", b"abcdefghij").unwrap();

    let mut buf = [0u8; 4];
    let n = fs.read_file("/f.txt", 3, &mut buf).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf, b"defg");
}

/// Read at a non-zero offset on a non-resident file. Must walk the
/// data run list to land at the right cluster.
#[test]
fn read_at_offset_nonresident() {
    let img = fresh_volume("offset_nonres");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f.bin").unwrap();
    let payload = pattern(32 * 1024, 0x42);
    fs.write_file_contents("/f.bin", &payload).unwrap();

    // Read a 16-byte window straddling the second cluster boundary.
    let off = 4096u64 - 4;
    let mut buf = [0u8; 16];
    let n = fs.read_file("/f.bin", off, &mut buf).unwrap();
    assert_eq!(n, 16);
    assert_eq!(buf, payload[off as usize..off as usize + 16]);
}
