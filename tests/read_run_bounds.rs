//! The read path's cluster arithmetic has to land on the volume.
//!
//! A `$DATA` mapping-pair list is disk-supplied and `decode_runs` rejects
//! only a *negative* absolute LCN, so any value up to `i64::MAX` reaches
//! the read sites in `read.rs`. Those sites multiply the LCN by the
//! cluster size and hand the product to `read_exact_at` without checking
//! either the multiply or the result against the volume.
//!
//! Two shapes matter and both are exercised here:
//!
//! * an LCN that is *on the device but off the volume* — the product is a
//!   perfectly good `u64`, the device read succeeds, and the driver
//!   returns bytes that are not part of the filesystem as the file's
//!   contents, with no error anywhere;
//! * an LCN large enough that `lcn * cluster_size` leaves the address
//!   space — a panic under `cargo test`, and a wrap to a *low* offset in
//!   a release build, where this crate ships with `overflow-checks` off.
//!
//! Both the whole-value read (`read_attribute_value`) and the ranged read
//! (`read_attribute_range`, which the file-read API uses) are covered.

use fs_ntfs::attr_io::{self, attr_off, AttrType};
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::data_runs;
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{mft_io, read, record_build};
use std::path::Path;

/// The volume, as the boot sector will describe it.
const VOL_SIZE: u64 = 16 * 1024 * 1024;
/// The image file: bigger than the volume, so there is somewhere on the
/// device that is provably not on the volume.
const DEVICE_SIZE: u64 = 24 * 1024 * 1024;
const CLUSTER: u32 = 4096;
/// Written into the off-volume region so a read landing there is
/// recognisable in the failure message.
const OFF_VOLUME_FILL: u8 = 0xAB;
/// A cluster in the image's tail: past `VOL_SIZE`, well inside
/// `DEVICE_SIZE`.
const OFF_VOLUME_LCN: u64 = (VOL_SIZE / CLUSTER as u64) + 64;

const FILE_SIZE: usize = 8192;

/// Format `VOL_SIZE` bytes of NTFS at the front of a `DEVICE_SIZE` image,
/// mark the off-volume tail, and put one non-resident file in the root.
fn volume_with_a_file(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_runbounds_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(DEVICE_SIZE).expect("set_len");
    drop(f);

    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("RUNB"), Some(1))
        .expect("format_filesystem");
    let marker = vec![OFF_VOLUME_FILL; (DEVICE_SIZE - VOL_SIZE) as usize];
    io.write_all_at(VOL_SIZE, &marker).expect("mark the tail");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);

    let fs = Filesystem::mount_rw(&dst).expect("mount_rw");
    fs.create_file("/", "victim.bin").expect("create_file");
    fs.write_file_contents("/victim.bin", &vec![b'Z'; FILE_SIZE])
        .expect("write_file_contents");
    drop(fs);
    dst
}

/// Rebuild `/victim.bin`'s unnamed `$DATA` so its single run starts at
/// `lcn`, keeping every declared length exactly as it was. The attribute
/// is rebuilt rather than patched in place because a far LCN needs a
/// wider mapping pair than the one it replaces.
fn repoint_data_run(img: &str, lcn: u64) {
    let mut io = PathIo::open_rw(Path::new(img)).expect("open_rw");
    let record_number = read::resolve_path(&mut io, "/victim.bin").expect("resolve_path");

    mft_io::update_mft_record_io(&mut io, record_number, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or("no unnamed $DATA in the victim's record")?;
        assert!(!loc.is_resident, "the victim's $DATA must be non-resident");
        let mpo =
            loc.non_resident_mapping_pairs_offset
                .expect("a non-resident attribute has a mapping-pairs offset") as usize;
        let field = |off: usize| -> u64 {
            u64::from_le_bytes(
                record[loc.attr_offset + off..loc.attr_offset + off + 8]
                    .try_into()
                    .unwrap(),
            )
        };
        let last_vcn = field(attr_off::NONRES_LAST_VCN) as i64;
        let allocated = field(attr_off::NONRES_ALLOCATED_LENGTH);
        let data_length = field(attr_off::NONRES_DATA_LENGTH);
        let initialized = field(attr_off::NONRES_INITIALIZED_LENGTH);
        let attr_id = u16::from_le_bytes([
            record[loc.attr_offset + attr_off::ATTRIBUTE_ID],
            record[loc.attr_offset + attr_off::ATTRIBUTE_ID + 1],
        ]);

        let mut runs = data_runs::decode_runs(
            &record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length],
        )?;
        assert_eq!(runs.len(), 1, "expected the victim to be a single extent");
        runs[0].lcn = Some(lcn);
        let pairs = data_runs::encode_runs(&runs)?;

        let rebuilt = record_build::build_nonresident_data_attribute(
            attr_id,
            data_length,
            allocated,
            initialized,
            last_vcn,
            &pairs,
        )?;
        fs_ntfs::attr_resize::replace_attribute(record, loc.attr_offset, &rebuilt)
    })
    .expect("update_mft_record_io");

    // Read the record back and confirm the driver's own decoder now sees
    // the LCN we planted, so a later refusal is about the bounds check
    // and not about a record we mangled.
    let (_, record) = mft_io::read_mft_record_io(&mut io, record_number).expect("re-read record");
    let loc = attr_io::find_attribute(&record, AttrType::Data, None).expect("$DATA still there");
    let mpo = loc.non_resident_mapping_pairs_offset.unwrap() as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])
            .expect("decode the rewritten runs");
    assert_eq!(
        runs[0].lcn,
        Some(lcn),
        "the record should now name LCN {lcn}"
    );
    assert_eq!(
        loc.non_resident_value_length,
        Some(FILE_SIZE as u64),
        "the rebuilt attribute must keep its declared data length"
    );
}

/// Both read entry points, so a guard added to one of them is not
/// mistaken for a guard on the read path.
fn read_both_ways(
    img: &str,
    record_number: u64,
) -> (Result<Vec<u8>, String>, Result<Vec<u8>, String>) {
    let mut io = PathIo::open_ro(Path::new(img)).expect("open_ro");
    let whole = read::read_attribute_value(&mut io, record_number, AttrType::Data, None);
    let ranged = read::read_attribute_range(&mut io, record_number, AttrType::Data, None, 0, 512);
    (whole, ranged)
}

#[test]
fn a_run_off_the_volume_is_refused_rather_than_read() {
    let img = volume_with_a_file("off_volume");
    repoint_data_run(&img, OFF_VOLUME_LCN);
    let mut probe = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let rec = read::resolve_path(&mut probe, "/victim.bin").expect("resolve_path");
    drop(probe);

    let (whole, ranged) = read_both_ways(&img, rec);
    for (what, got) in [
        ("read_attribute_value", whole),
        ("read_attribute_range", ranged),
    ] {
        match got {
            Err(_) => {}
            Ok(bytes) => panic!(
                "{what}: a $DATA run at LCN {OFF_VOLUME_LCN} starts past the \
                 {VOL_SIZE}-byte volume, yet the read returned {} bytes: {:02x?}...",
                bytes.len(),
                &bytes[..8.min(bytes.len())]
            ),
        }
    }
}

#[test]
fn a_run_whose_offset_overflows_is_refused_rather_than_wrapped() {
    let img = volume_with_a_file("overflow");
    // 2^52 clusters of 4096 bytes is exactly 2^64: the product leaves the
    // address space. `cargo test` builds with overflow checks on, so the
    // unguarded multiply panics here; the shipped release profile turns
    // them off, and there the same input wraps to offset 0 — the boot
    // sector, returned as the file's first cluster.
    let lcn = 1u64 << 52;
    repoint_data_run(&img, lcn);
    let mut probe = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let rec = read::resolve_path(&mut probe, "/victim.bin").expect("resolve_path");
    drop(probe);

    let (whole, ranged) = read_both_ways(&img, rec);
    assert!(
        whole.is_err(),
        "read_attribute_value: an LCN of {lcn} at a {CLUSTER}-byte cluster \
         overflows a u64 byte offset; the read must refuse it"
    );
    assert!(
        ranged.is_err(),
        "read_attribute_range: an LCN of {lcn} at a {CLUSTER}-byte cluster \
         overflows a u64 byte offset; the read must refuse it"
    );
}
