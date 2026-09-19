//! `$MFTMirr` must hold what `$MFT` holds.
//!
//! The mirror carries copies of the first four MFT records so a volume
//! whose `$MFT` head is unreadable can still be mounted and repaired.
//! `mkfs` wrote it once and nothing updated it afterwards — while this
//! crate writes into `$Volume` (record 3) routinely: `set_dirty`,
//! `clear_dirty` and `upgrade_volume_version` all edit it. chkdsk
//! compares the two copies, and a recovery that trusts a stale mirror
//! restores a `$Volume` with the dirty bit this driver had just changed
//! (#145).

mod common;

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{fsck, mft_io};
use std::path::Path;

const VOL: u64 = 32 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = common::temp_image_path(format!("mirror_{tag}"));
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL, CLUSTER, CLUSTER, Some("MIRROR"), Some(3)).expect("format");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// The bytes `$MFTMirr` holds for `record_number`, read straight off the
/// mirror's first run rather than through any helper that might paper
/// over a difference.
fn mirrored_record(img: &str, record_number: u64) -> Vec<u8> {
    let mut io = PathIo::open_ro(Path::new(img)).expect("open_ro");
    let params = mft_io::read_boot_params_io(&mut io).expect("boot params");
    let (_p, mirr) = mft_io::read_mft_record_io(&mut io, 1).expect("read $MFTMirr");
    let loc = fs_ntfs::attr_io::find_attribute(&mirr, fs_ntfs::attr_io::AttrType::Data, None)
        .expect("$MFTMirr $DATA");
    let mpo = loc
        .non_resident_mapping_pairs_offset
        .expect("mapping pairs") as usize;
    let runs = fs_ntfs::data_runs::decode_runs(
        &mirr[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length],
    )
    .expect("decode runs");
    let lcn = runs[0].lcn.expect("mirror is not sparse");
    let at = lcn * params.cluster_size + record_number * params.file_record_size;
    let mut buf = vec![0u8; params.file_record_size as usize];
    io.read_exact_at(at, &mut buf).expect("read the mirror");
    buf
}

fn live_record(img: &str, record_number: u64) -> Vec<u8> {
    let mut io = PathIo::open_ro(Path::new(img)).expect("open_ro");
    let params = mft_io::read_boot_params_io(&mut io).expect("boot params");
    let at = mft_io::mft_record_offset(&params, record_number);
    let mut buf = vec![0u8; params.file_record_size as usize];
    io.read_exact_at(at, &mut buf).expect("read $MFT");
    buf
}

#[test]
fn setting_and_clearing_the_dirty_bit_keeps_the_mirror_in_step() {
    let img = volume("dirty");
    let p = Path::new(&img);

    assert_eq!(
        mirrored_record(&img, 3),
        live_record(&img, 3),
        "mkfs leaves the mirror matching"
    );

    assert!(fsck::set_dirty(p).expect("set_dirty"), "the bit was clear");
    assert_eq!(
        mirrored_record(&img, 3),
        live_record(&img, 3),
        "after set_dirty the mirror still holds what $MFT holds"
    );

    assert!(
        fsck::clear_dirty(p).expect("clear_dirty"),
        "the bit was set"
    );
    assert_eq!(
        mirrored_record(&img, 3),
        live_record(&img, 3),
        "after clear_dirty too"
    );
}
