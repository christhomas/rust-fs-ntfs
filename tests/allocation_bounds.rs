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
    let dst = format!("test-disks/_allocbounds_{tag}.img");
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
    let img = volume_with_an_overlong_bitmap("promote");
    write::create_file(Path::new(&img), "/", "f.bin").expect("create");
    let before = image_len(&img);

    let wrote = write::write_file_contents(Path::new(&img), "/f.bin", &vec![b'x'; 8192]);
    assert!(
        wrote.is_err(),
        "the volume has no free cluster, yet the promotion reported {wrote:?}"
    );
    assert_eq!(
        image_len(&img),
        before,
        "the write extended the image past the volume it was formatted on"
    );
}

/// The sparse writer takes the same LCN from the same search.
#[test]
fn a_sparse_write_refuses_rather_than_writing_off_the_volume() {
    let img = volume_with_an_overlong_bitmap("sparse");
    write::create_file(Path::new(&img), "/", "s.bin").expect("create");
    let before = image_len(&img);

    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 3 * cs];
    data[0..cs].fill(0xAA);
    data[2 * cs..3 * cs].fill(0xBB);
    let wrote = write::write_sparse_file(Path::new(&img), "/s.bin", &data);
    assert!(
        wrote.is_err(),
        "the volume has no free cluster, yet the sparse write reported {wrote:?}"
    );
    assert_eq!(
        image_len(&img),
        before,
        "the sparse write extended the image past the volume"
    );
}

/// The named-stream promotion path is the third site.
#[test]
fn attribute_promotion_refuses_rather_than_writing_off_the_volume() {
    let img = volume_with_an_overlong_bitmap("attr");
    write::create_file(Path::new(&img), "/", "a.bin").expect("create");
    let before = image_len(&img);

    let wrote = write::promote_attribute_to_nonresident(
        Path::new(&img),
        "/a.bin",
        AttrType::Data,
        Some("stream"),
        &vec![b'y'; 8192],
    );
    assert!(
        wrote.is_err(),
        "the volume has no free cluster, yet the promotion reported {wrote:?}"
    );
    assert_eq!(
        image_len(&img),
        before,
        "the write extended the image past the volume"
    );
}
