//! An allocation the volume cannot honour must fail before it is written.
//!
//! `bitmap::find_free_run_io` searches `[0, total_bits)`, and
//! `total_bits` was `$Bitmap`'s own declared `data_length` times eight
//! with nothing tying it to the volume. NTFS sets the padding bits past
//! the last real cluster so a search cannot walk off the end; a volume
//! where those are clear — a truncated image, a `$Bitmap` whose length
//! field is wrong, an image from another tool — hands back an LCN past
//! the end of the volume.
//!
//! Three write sites then multiplied that LCN by the cluster size raw
//! and handed the product to `write_all_at`. On a file-backed image
//! that silently extends the file, and the `$DATA` run list ends up
//! naming clusters the volume does not have; the file reads back as
//! garbage on Windows. On a real device the write fails, but only after
//! `bitmap::allocate_io` has marked the clusters in use, so the failure
//! leaves the leak behind.
//!
//! `$MFT:$Bitmap` has clamped its declared bit count by what the volume
//! could hold since the same class of bug was found there. `$Bitmap`
//! had no equivalent.

mod common;

use fs_ntfs::attr_io::{self, attr_off, AttrType};
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{bitmap, mft_io, read, write};
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;
const CLUSTER_COUNT: u64 = VOL_SIZE / CLUSTER as u64;
/// MFT record number of `$Bitmap`.
const BITMAP_RECORD: u64 = 6;

/// A volume whose `$Bitmap` claims a full cluster of bits — eight times
/// more clusters than the volume has — with every real cluster marked
/// allocated, so the only "free" bits a search can find are the ones
/// past the end of the volume.
fn volume_with_an_overlong_bitmap(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = common::temp_image_path(format!("allocbounds_{tag}"));
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("ALOC"), Some(7))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");

    // Where $Bitmap's single extent lives, and how long it says it is.
    let (start, declared) =
        read::nonresident_contiguous_disk_range(&mut io, BITMAP_RECORD, AttrType::Data, None)
            .expect("$Bitmap is one extent");
    assert_eq!(
        declared * 8,
        CLUSTER_COUNT,
        "mkfs should declare exactly one bit per cluster"
    );

    // Say the bitmap is a whole cluster long. The extra bytes are inside
    // the extent $Bitmap already owns, so every read still lands on real
    // clusters -- only the declared bit count changes.
    let overlong = CLUSTER as u64;
    mft_io::update_mft_record_io(&mut io, BITMAP_RECORD, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None).ok_or("no $DATA")?;
        for field in [
            attr_off::NONRES_DATA_LENGTH,
            attr_off::NONRES_INITIALIZED_LENGTH,
        ] {
            record[loc.attr_offset + field..loc.attr_offset + field + 8]
                .copy_from_slice(&overlong.to_le_bytes());
        }
        Ok(())
    })
    .expect("lengthen $Bitmap");

    // Every real cluster allocated; the bits past the volume left clear,
    // which is the state NTFS's tail padding exists to prevent.
    let real_bytes = (CLUSTER_COUNT / 8) as usize;
    io.write_all_at(start, &vec![0xFFu8; real_bytes])
        .expect("mark every real cluster allocated");
    io.write_all_at(
        start + real_bytes as u64,
        &vec![0u8; overlong as usize - real_bytes],
    )
    .expect("leave the padding bits clear");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// A volume whose `$Bitmap` is the right length but says the last
/// `tail_free` clusters are free — so a search *succeeds*, and the run it
/// returns runs past the end of the volume.
///
/// THIS IS THE FIXTURE THE THREE WRITE TESTS BELOW NEEDED.
///
/// `volume_with_an_overlong_bitmap` marks every real cluster allocated,
/// which makes `find_free_run_io` return `None` — so the three tests that
/// exist to verify the `cluster_span` guards were failing at the
/// allocator and never reaching a single one of them. Deleting every
/// guard PR #200 added left all three green. See #224.
///
/// The bound the run crosses is real and not contrived: mkfs writes
/// `number_sectors = volume_sectors - 1`, so `volume_bytes()` is
/// `cluster_count * cluster_size - bytes_per_sector` and the last
/// cluster is **inside** the allocator's capacity and **outside** every
/// transfer bound. Measured on a 16 MiB volume at 4 KiB clusters:
/// `total_bits` is 4096, `cluster_span` accepts LCN 4094 and refuses
/// 4095 with "a transfer spans [16773120, 16777216) on a volume of
/// 16776704 bytes". mkfs marks 4095 in use, which is why this has to
/// clear it: the trigger is a foreign, truncated or corrupt `$Bitmap`,
/// exactly the class the guards were written for. See #223.
fn volume_with_only_the_tail_free(tag: &str, tail_free: u64) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = common::temp_image_path(format!("allocbounds_{tag}"));
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("ALOC"), Some(7))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");

    let (start, declared) =
        read::nonresident_contiguous_disk_range(&mut io, BITMAP_RECORD, AttrType::Data, None)
            .expect("$Bitmap is one extent");
    assert_eq!(
        declared * 8,
        CLUSTER_COUNT,
        "mkfs should declare exactly one bit per cluster"
    );

    // Everything allocated, then the tail bits cleared. The declared
    // length is left exactly as mkfs wrote it, so this is a volume whose
    // geometry is correct and whose free-space map is not -- which is the
    // shape a foreign formatter or an interrupted write produces.
    let real_bytes = (CLUSTER_COUNT / 8) as usize;
    io.write_all_at(start, &vec![0xFFu8; real_bytes])
        .expect("mark every cluster allocated");
    let bm = bitmap::locate_bitmap_io(&mut io).expect("locate $Bitmap");
    for lcn in (CLUSTER_COUNT - tail_free)..CLUSTER_COUNT {
        bitmap::free_io(&mut io, &bm, lcn, 1).expect("clear a tail bit");
    }
    <PathIo as BlockIo>::sync(&mut io).expect("sync");

    // The search must now succeed, or the test below it is measuring the
    // allocator again rather than the guard.
    let found = bitmap::find_free_run_io(&mut io, &bm, tail_free, 0)
        .expect("search")
        .expect("the tail run must be findable, or the guard is never reached");
    assert_eq!(
        found,
        CLUSTER_COUNT - tail_free,
        "the only free run is the tail one"
    );
    drop(io);
    dst
}

/// Free clusters as a consumer sees them through `volume_stats`.
fn free_clusters(img: &str) -> u64 {
    let bm = bitmap::locate_bitmap(Path::new(img)).expect("locate_bitmap");
    bitmap::count_free(Path::new(img), &bm).expect("count_free")
}

fn image_len(img: &str) -> u64 {
    std::fs::metadata(img).expect("stat image").len()
}

/// The search must not offer a cluster the volume does not have.
#[test]
fn the_free_search_is_bounded_by_the_volume() {
    let img = volume_with_an_overlong_bitmap("search");
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let bm = bitmap::locate_bitmap_io(&mut io).expect("locate $Bitmap");
    assert!(
        bm.total_bits <= CLUSTER_COUNT,
        "$Bitmap declares {} bits on a volume of {CLUSTER_COUNT} clusters",
        bm.total_bits
    );
    let found = bitmap::find_free_run_io(&mut io, &bm, 1, 0).expect("search");
    if let Some(lcn) = found {
        assert!(
            lcn < CLUSTER_COUNT,
            "the search offered LCN {lcn}, past the volume's {CLUSTER_COUNT} clusters"
        );
    }
}

/// Promoting a resident value allocates, then writes at the LCN it was
/// given. Both promotion paths and the sparse writer did the multiply
/// raw.
#[test]
fn promotion_refuses_rather_than_writing_off_the_volume() {
    let img = volume_with_only_the_tail_free("promote", 2);
    write::create_file(Path::new(&img), "/", "f.bin").expect("create");
    let before = image_len(&img);
    let free_before = free_clusters(&img);

    let wrote = write::write_file_contents(Path::new(&img), "/f.bin", &vec![b'x'; 8192]);
    assert!(
        wrote.is_err(),
        "the only free run ends past the volume, yet the promotion reported {wrote:?}"
    );
    assert_eq!(
        image_len(&img),
        before,
        "the write extended the image past the volume it was formatted on"
    );
    // And the clusters it took before the refusal are back. Until the
    // fixture above reached the guard, no test in this file could see
    // this either. See #251.
    assert_eq!(
        free_clusters(&img),
        free_before,
        "a refused promotion left its allocation marked in use"
    );
}

/// The sparse writer takes the same LCN from the same search.
#[test]
fn a_sparse_write_refuses_rather_than_writing_off_the_volume() {
    let img = volume_with_only_the_tail_free("sparse", 2);
    write::create_file(Path::new(&img), "/", "s.bin").expect("create");
    let before = image_len(&img);
    let free_before = free_clusters(&img);

    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 3 * cs];
    data[0..cs].fill(0xAA);
    data[2 * cs..3 * cs].fill(0xBB);
    let wrote = write::write_sparse_file(Path::new(&img), "/s.bin", &data);
    assert!(
        wrote.is_err(),
        "the only free run ends past the volume, yet the sparse write reported {wrote:?}"
    );
    assert_eq!(
        image_len(&img),
        before,
        "the sparse write extended the image past the volume"
    );
    assert_eq!(
        free_clusters(&img),
        free_before,
        "a refused sparse write left its allocation marked in use"
    );
}

/// The named-stream promotion path is the third site.
#[test]
fn attribute_promotion_refuses_rather_than_writing_off_the_volume() {
    let img = volume_with_only_the_tail_free("attr", 2);
    write::create_file(Path::new(&img), "/", "a.bin").expect("create");
    let before = image_len(&img);
    let free_before = free_clusters(&img);

    let wrote = write::promote_attribute_to_nonresident(
        Path::new(&img),
        "/a.bin",
        AttrType::Data,
        Some("stream"),
        &vec![b'y'; 8192],
    );
    assert!(
        wrote.is_err(),
        "the only free run ends past the volume, yet the promotion reported {wrote:?}"
    );
    assert_eq!(
        image_len(&img),
        before,
        "the write extended the image past the volume"
    );
    assert_eq!(
        free_clusters(&img),
        free_before,
        "a refused attribute promotion left its allocation marked in use"
    );
}

/// THE GROW PATH COMMITTED WITH NO BOUNDS CHECK AT ALL.
///
/// PR #200 gave the two promotion paths and the sparse writer a
/// `cluster_span` call before their writes. `grow` has no data write —
/// the clusters it takes are uninitialised by design — so it wrote no
/// bytes to check, and it committed the run list naming them with no
/// check anywhere. A free bit on a cluster the volume cannot address
/// therefore produced a **committed, permanently allocated, unreadable
/// extent, reported as a successful grow**: the file grows, the
/// allocation persists, and nothing can ever read or write those bytes.
///
/// One cluster free, and it is the tail one — inside the allocator's
/// capacity, outside every transfer bound. See #223.
#[test]
fn a_grow_onto_a_cluster_the_volume_cannot_address_is_refused_and_rolled_back() {
    // Two clusters free at the tail. The first, LCN 4094, is one every
    // bound accepts, and it is what makes the file non-resident so there
    // is something to grow. The second, LCN 4095, is the one no transfer
    // can land on.
    let img = volume_with_only_the_tail_free("grow", 2);
    write::create_file(Path::new(&img), "/", "g.bin").expect("create");
    write::write_file_contents(Path::new(&img), "/g.bin", &vec![b'g'; CLUSTER as usize])
        .expect("promote onto the first tail cluster");

    let free_before = free_clusters(&img);
    assert_eq!(
        free_before,
        1,
        "exactly LCN {} should be left free",
        CLUSTER_COUNT - 1
    );

    let last = CLUSTER_COUNT - 1;
    let grew = write::grow_nonresident(Path::new(&img), "/g.bin", CLUSTER as u64 * 2);
    let err = match grew {
        Err(e) => e,
        Ok(n) => panic!(
            "LCN {last} is the only free cluster and no transfer on it can land on \
             the volume, yet the grow reported {n} bytes"
        ),
    };
    assert!(
        err.contains("not on the volume"),
        "expected the grow's own bounds refusal, got: {err}"
    );
    assert_eq!(
        free_clusters(&img),
        free_before,
        "a refused grow left LCN {last} marked in use -- allocated, unreadable, and \
         referenced by nothing"
    );
}
