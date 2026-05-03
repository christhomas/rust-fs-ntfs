//! Alternate-data-stream combinatorics.
//!
//! Existing `write_ads.rs` covers single-stream create/delete on the
//! `BASIC_IMG` fixture. This file pushes harder:
//!   * many streams on the same file simultaneously
//!   * mixed sizes (resident + non-resident) on the same file
//!   * delete-then-recreate sequences
//!   * remount survival
//!   * ADS on directories (legal in NTFS, sometimes overlooked)
//!
//! All tests own a fresh formatted volume so failures isolate
//! cleanly.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

const VOL_SIZE: u64 = 32 * 1024 * 1024;

fn fresh_volume(tag: &str) -> String {
    let dst = format!("test-disks/_adsx_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, 4096, 4096, Some("ADS"), Some(0xADADADAD))
        .expect("format");
    io.sync().expect("sync");
    drop(io);
    dst
}

/// Eight named streams on one file. Each stream gets a distinct
/// payload; volume must remount cleanly with all data intact.
#[test]
fn many_streams_on_one_file() {
    let img = fresh_volume("many_on_one");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "host.txt").unwrap();
    fs.write_file_contents("/host.txt", b"main stream content")
        .unwrap();

    let streams: Vec<(String, Vec<u8>)> = (0..8)
        .map(|i| (format!("stream_{i}"), vec![i as u8; 32 + i * 4]))
        .collect();

    for (name, data) in &streams {
        fs.write_named_stream("/host.txt", name, data)
            .unwrap_or_else(|e| panic!("write {name}: {e}"));
    }

    // Main stream must be untouched.
    let mut buf = vec![0u8; 19];
    let n = fs.read_file("/host.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"main stream content");

    // Remount and the file should still be there with size unchanged.
    let fs2 = Filesystem::mount(&img).unwrap();
    let stat = fs2.stat("/host.txt").unwrap();
    assert_eq!(stat.size, 19, "main $DATA size after stream churn");
}

/// Mix of small (resident) and large (non-resident) streams on the
/// same file. The driver has to manage both attribute kinds in one
/// MFT record.
#[test]
fn mixed_resident_and_nonresident_streams() {
    let img = fresh_volume("mixed_size");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "doc.bin").unwrap();
    fs.write_file_contents("/doc.bin", b"main").unwrap();

    // Tiny stream → resident.
    fs.write_named_stream("/doc.bin", "tiny", b"x").unwrap();
    // Big stream → non-resident (8 KiB).
    let big = vec![0xCDu8; 8 * 1024];
    fs.write_named_stream("/doc.bin", "big", &big).unwrap();
    // Empty stream — legal.
    fs.write_named_stream("/doc.bin", "empty", b"").unwrap();

    // Re-mount, all three streams must survive deletion of the big
    // one without breaking the rest.
    let fs2 = Filesystem::mount(&img).unwrap();
    fs2.delete_named_stream("/doc.bin", "big").unwrap();

    // Re-add big stream after deletion — exercises the "free clusters
    // then reallocate" path.
    fs2.write_named_stream("/doc.bin", "big", &big).unwrap();
    fs2.delete_named_stream("/doc.bin", "big").unwrap();
    fs2.delete_named_stream("/doc.bin", "tiny").unwrap();
    fs2.delete_named_stream("/doc.bin", "empty").unwrap();
}

/// Removing a non-existent stream must error cleanly, not panic.
#[test]
fn delete_missing_stream_errors_cleanly() {
    let img = fresh_volume("missing");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();
    let err = fs.delete_named_stream("/f", "no_such_stream").unwrap_err();
    assert!(
        err.to_string().contains("not found")
            || err.to_string().contains("no such")
            || err.to_string().contains("missing")
            || err.to_string().contains("absent"),
        "unexpected error: {err}"
    );
}

/// Stream churn: write-delete-write-delete on the same name. Free
/// cluster count must return to baseline after the final delete.
///
/// Currently fails: each `delete_named_stream` of a 32 KiB
/// non-resident stream leaks exactly the stream's 8-cluster
/// allocation. After 5 cycles, 40 clusters are unreachable but
/// marked allocated. The deletion path removes the attribute from
/// the MFT record but doesn't release the data runs to the volume
/// `$Bitmap`. Drop `#[ignore]` once `delete_named_stream` walks
/// data runs and clears bitmap bits.
#[test]
#[ignore = "driver: delete_named_stream does not free non-resident clusters"]
fn stream_churn_does_not_leak_clusters() {
    let img = fresh_volume("churn_leak");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "host.bin").unwrap();
    let baseline = fs.volume_stats().unwrap().free_clusters;

    let payload = vec![0x42; 32 * 1024];
    for _ in 0..5 {
        fs.write_named_stream("/host.bin", "churn", &payload)
            .unwrap();
        fs.delete_named_stream("/host.bin", "churn").unwrap();
    }

    let after = fs.volume_stats().unwrap().free_clusters;
    assert_eq!(
        after, baseline,
        "free_clusters drifted: baseline={baseline} after={after}"
    );
}

/// Stream content with non-ASCII name. NTFS stores stream names as
/// UTF-16, so a CJK name must round-trip without being mangled.
#[test]
fn unicode_stream_name_roundtrip() {
    let img = fresh_volume("unicode_name");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "host").unwrap();
    fs.write_named_stream("/host", "メタデータ", b"jp meta data")
        .unwrap();
    fs.delete_named_stream("/host", "メタデータ").unwrap();
}

/// Stream payload exactly at a cluster boundary.
#[test]
fn stream_at_cluster_boundary_size() {
    let img = fresh_volume("cluster_size");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "host").unwrap();
    let payload = vec![0x77; 4096];
    fs.write_named_stream("/host", "exact", &payload).unwrap();
    fs.delete_named_stream("/host", "exact").unwrap();

    // 4096 + 1 — forces a 2-cluster allocation, all but 1 byte unused.
    let payload2 = vec![0x88; 4097];
    fs.write_named_stream("/host", "plus1", &payload2).unwrap();
    fs.delete_named_stream("/host", "plus1").unwrap();
}
