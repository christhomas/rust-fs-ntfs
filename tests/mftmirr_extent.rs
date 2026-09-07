//! `$MFTMirr` must not declare records it does not hold.
//!
//! The mirror's `$DATA` length is what a repair tool divides by the
//! record size to learn how many of `$MFT`'s records it can recover
//! from. `mkfs` rounded its allocation up to a cluster — which is
//! normal, and is what `allocated_length` is for — and then declared
//! that rounded allocation as the data and initialized lengths too,
//! while still copying four records into it.
//!
//! At any geometry where a cluster holds more than four MFT records the
//! two disagree, and the disagreement is made of zeros. A recovery that
//! trusted the declared length would write those zeros over live system
//! records: `$Volume`, `$AttrDef`, the root directory, `$Bitmap`,
//! `$Boot`, `$BadClus`, `$Secure`, `$UpCase`, `$Extend`. It would
//! destroy the volume it was invoked to save.
//!
//! This test does not assert a particular number. It asserts that the
//! declared length and the records actually written agree, which holds
//! whichever of the two the formatter is later decided to be right.

use fs_ntfs::attr_io::AttrType;
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{mft_io, read};
use std::path::Path;

const MFT_RECORD_SIZE: u32 = 4096;
/// `$MFTMirr` is MFT record 1.
const MFTMIRR_RECORD: u64 = 1;

/// `(cluster_size, volume_bytes)` — the same table `cluster_size_matrix`
/// uses, sized so every cluster size clears `mkfs`'s 1024-cluster
/// minimum with room for `$MFT`, `$MFTMirr` and the fixed-size
/// `$LogFile` under the midpoint.
fn cases() -> Vec<(u32, u64)> {
    vec![
        (512, 32 * 1024 * 1024),
        (1024, 32 * 1024 * 1024),
        (2048, 32 * 1024 * 1024),
        (4096, 32 * 1024 * 1024),
        (8192, 64 * 1024 * 1024),
        (16384, 128 * 1024 * 1024),
        (32768, 256 * 1024 * 1024),
        (65536, 512 * 1024 * 1024),
    ]
}

/// Format a volume at this cluster size and return the image path.
fn formatted(cluster_size: u32, size: u64) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_mirror_c{cluster_size}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(size).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        size,
        cluster_size,
        MFT_RECORD_SIZE,
        Some("MIRR"),
        Some(6),
    )
    .unwrap_or_else(|e| panic!("format at cluster {cluster_size}, size {size}: {e}"));
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// `(records the $DATA length declares, records actually written)`.
fn declared_and_written(img: &str) -> (u64, u64) {
    let mut io = PathIo::open_ro(Path::new(img)).expect("open_ro");
    // The mirror is a single extent by construction, so this both
    // locates it and hands back the declared data length.
    let (start, declared_bytes) =
        read::nonresident_contiguous_disk_range(&mut io, MFTMIRR_RECORD, AttrType::Data, None)
            .expect("$MFTMirr's $DATA is one extent");
    let (params, _) = mft_io::read_mft_record_io(&mut io, MFTMIRR_RECORD).expect("read record 1");
    let rs = params.file_record_size;
    assert_eq!(
        declared_bytes % rs,
        0,
        "a mirror of {declared_bytes} bytes is not a whole number of {rs}-byte records"
    );

    let mut bytes = vec![0u8; declared_bytes as usize];
    io.read_exact_at(start, &mut bytes)
        .expect("read the mirror");
    let written = bytes
        .chunks_exact(rs as usize)
        .take_while(|c| &c[0..4] == b"FILE")
        .count() as u64;
    (declared_bytes / rs, written)
}

#[test]
fn the_mirror_holds_every_record_it_declares() {
    // 512 up to 65536 is the range `format_filesystem` accepts. With
    // 4096-byte records the rounding first bites at 32768, where one
    // cluster holds eight records and four are written.
    for (cluster, size) in cases() {
        let img = formatted(cluster, size);
        let (declared, written) = declared_and_written(&img);
        assert_eq!(
            declared,
            written,
            "at cluster {cluster}: $MFTMirr declares {declared} records and holds \
             {written}, so {} of them are zeros a repair would write over live \
             system records",
            declared - written
        );
        // Whatever the count, it has to cover the four records the
        // mirror exists for.
        assert!(
            written >= 4,
            "at cluster {cluster}: the mirror holds only {written} records"
        );
    }
}
