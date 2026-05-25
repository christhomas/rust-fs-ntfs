//! Cluster-size matrix.
//!
//! NTFS supports cluster sizes from 512 B (rare, mostly legacy floppy-
//! style media) to 64 KiB (large modern volumes). Each cluster size
//! changes:
//!   * the BPB layout (sectors_per_cluster differs)
//!   * data-run encoding (cluster numbers in different ranges)
//!   * the resident → non-resident threshold
//!   * the size of every $DATA cluster reservation
//!
//! Bugs in `mkfs` and the read/write path frequently lurk at the
//! extreme cluster sizes — the default 4 KiB path gets all the love.
//! This sweep proves format + create + write + read + remount works
//! at every supported cluster size.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

/// `(cluster_size, vol_size_bytes, label)`. Volume size is sized so
/// every cluster size has at least 1024 clusters (mkfs's minimum) plus
/// headroom for $MFT, $MFTMirr, $LogFile. $LogFile is fixed at
/// `0x3B0000` bytes (the size encoded in the canonical RSTR pages —
/// see `LOGFILE_TARGET_BYTES` in `mkfs.rs`); for the smallest cluster
/// sizes the volume must therefore be a few × the log size to fit
/// the rest of the layout under MFTMirr at `cluster_count/2`.
fn cases() -> Vec<(u32, u64, &'static str)> {
    vec![
        (512, 32 * 1024 * 1024, "C512"),
        (1024, 32 * 1024 * 1024, "C1K"),
        (2048, 32 * 1024 * 1024, "C2K"),
        (4096, 32 * 1024 * 1024, "C4K"),
        (8192, 64 * 1024 * 1024, "C8K"),
        (16384, 128 * 1024 * 1024, "C16K"),
        (32768, 256 * 1024 * 1024, "C32K"),
        (65536, 512 * 1024 * 1024, "C64K"),
    ]
}

fn fresh_volume(cluster: u32, size: u64, label: &str) -> String {
    let dst = format!("test-disks/_csize_{}.img", label.to_lowercase());
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(size).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, size, cluster, 4096, Some(label), Some(0xCAFEBABE))
        .unwrap_or_else(|e| panic!("format cluster={cluster}: {e}"));
    io.sync().expect("sync");
    drop(io);
    dst
}

fn round_trip(cluster: u32, vol_size: u64, label: &str) {
    let img = fresh_volume(cluster, vol_size, label);

    {
        let fs = Filesystem::mount(&img).unwrap_or_else(|e| panic!("mount cluster={cluster}: {e}"));
        let info = fs.volume_info().unwrap();
        assert_eq!(info.cluster_size, cluster, "cluster_size in BPB");
        // NTFS BPB encodes total_sectors = N-1 (the last sector holds
        // the backup boot sector). Reported total_size is therefore
        // vol_size minus one 512-byte sector.
        assert_eq!(info.total_size, vol_size - 512, "total volume size");

        // Resident-only payload (small enough to stay inline at any
        // cluster size).
        fs.create_file("/", "tiny.txt").unwrap();
        fs.write_file_contents("/tiny.txt", b"hello\n").unwrap();

        // Non-resident payload large enough to span >1 cluster at the
        // largest tested cluster size.
        let big = vec![0xA5u8; 256 * 1024];
        fs.create_file("/", "big.bin").unwrap();
        fs.write_file_contents("/big.bin", &big).unwrap();

        // Subdir and a nested file — exercises $INDEX_ROOT for two
        // separate directories.
        fs.mkdir("/", "sub").unwrap();
        fs.create_file("/sub", "leaf.txt").unwrap();
        fs.write_file_contents("/sub/leaf.txt", b"leaf").unwrap();

        // Read-back inline and walk the data runs immediately.
        let mut t = vec![0u8; 6];
        let n = fs.read_file("/tiny.txt", 0, &mut t).unwrap();
        assert_eq!(&t[..n], b"hello\n");

        let mut chunk = vec![0u8; 1024];
        let n = fs.read_file("/big.bin", 0, &mut chunk).unwrap();
        assert_eq!(n, 1024);
        assert!(chunk.iter().all(|&b| b == 0xA5), "non-resident bleed");

        // Read from beyond the first cluster boundary so the data-run
        // walker has to step at least once.
        let off = (cluster as u64) + 17;
        let mut win = vec![0u8; 64];
        let n = fs.read_file("/big.bin", off, &mut win).unwrap();
        assert_eq!(n, 64);
        assert!(win.iter().all(|&b| b == 0xA5));
    }

    // Remount and re-verify — catches "wrote it to disk inconsistently
    // but in-memory state was OK" classes of bug.
    let fs = Filesystem::mount(&img).unwrap();
    let info = fs.volume_info().unwrap();
    assert_eq!(info.cluster_size, cluster, "cluster_size after remount");

    let mut t = vec![0u8; 6];
    let n = fs.read_file("/tiny.txt", 0, &mut t).unwrap();
    assert_eq!(&t[..n], b"hello\n");
    assert_eq!(fs.stat("/big.bin").unwrap().size, 256 * 1024);
    assert_eq!(read_string(&fs, "/sub/leaf.txt"), "leaf");
}

fn read_string(fs: &Filesystem, path: &str) -> String {
    let size = fs.stat(path).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read_file(path, 0, &mut buf).unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[test]
fn round_trip_512() {
    let (c, v, l) = cases()[0];
    round_trip(c, v, l);
}

#[test]
fn round_trip_1k() {
    let (c, v, l) = cases()[1];
    round_trip(c, v, l);
}

#[test]
fn round_trip_2k() {
    let (c, v, l) = cases()[2];
    round_trip(c, v, l);
}

#[test]
fn round_trip_4k() {
    let (c, v, l) = cases()[3];
    round_trip(c, v, l);
}

#[test]
fn round_trip_8k() {
    let (c, v, l) = cases()[4];
    round_trip(c, v, l);
}

#[test]
fn round_trip_16k() {
    let (c, v, l) = cases()[5];
    round_trip(c, v, l);
}

#[test]
fn round_trip_32k() {
    let (c, v, l) = cases()[6];
    round_trip(c, v, l);
}

#[test]
fn round_trip_64k() {
    let (c, v, l) = cases()[7];
    round_trip(c, v, l);
}

/// mkfs must reject power-of-two cluster sizes outside [512, 65536].
#[test]
fn mkfs_rejects_invalid_cluster_sizes() {
    let dst = "test-disks/_csize_invalid.img";
    let f = std::fs::File::create(dst).unwrap();
    f.set_len(8 * 1024 * 1024).unwrap();
    drop(f);

    for bad in &[256u32, 128, 131072, 262144, 4097, 6144] {
        let mut io = PathIo::open_rw(Path::new(dst)).unwrap();
        let err = format_filesystem(&mut io, 8 * 1024 * 1024, *bad, 4096, Some("X"), Some(0))
            .unwrap_err();
        assert!(
            err.contains("cluster_size"),
            "expected cluster_size complaint for {bad}, got: {err}"
        );
    }
}

/// mkfs must reject volumes too small for a viable layout.
#[test]
fn mkfs_rejects_tiny_volumes() {
    let dst = "test-disks/_csize_too_small.img";
    let f = std::fs::File::create(dst).unwrap();
    f.set_len(64 * 1024).unwrap(); // 64 KiB — well under mkfs's floor
    drop(f);

    let mut io = PathIo::open_rw(Path::new(dst)).unwrap();
    let err = format_filesystem(&mut io, 64 * 1024, 4096, 4096, Some("X"), Some(0)).unwrap_err();
    assert!(
        err.contains("too small") || err.contains("clusters"),
        "expected size-floor complaint, got: {err}"
    );
}
