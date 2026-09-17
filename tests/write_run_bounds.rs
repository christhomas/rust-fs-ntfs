//! The write path's run-length arithmetic has to stay inside the address
//! space.
//!
//! `write_at`'s non-resident loop chops the caller's data at each run
//! boundary, and the bound it chops on is computed from the run list:
//!
//! ```text
//! run_end_vcn    = starting_vcn + length          (disk-supplied)
//! run_end_offset = run_end_vcn * cluster_size      (disk-supplied)
//! max_in_this_run = run_end_offset - file_offset
//! ```
//!
//! `decode_runs` bounds the *accumulated* VCN of the runs it emits, so
//! the add cannot carry for a list that came off a disk — but nothing
//! bounds a single run's `length`. One run of `2^52` clusters at a 4 KiB
//! cluster size makes the product exactly `2^64`.
//!
//! Unguarded, that had two symptoms and no error: a panic under `cargo
//! test`, where overflow checks are on, and in the shipped release
//! profile (`overflow-checks` off) a wrap of `run_end_offset` to a low
//! value — for a write at offset 0, to exactly 0, which makes
//! `max_in_this_run` 0, `chunk` 0, and the loop advance neither its
//! cursor nor its offset. A hang, on a volume a reader can mount.
//!
//! The companion for the read side is `read_run_bounds.rs`, which does
//! the same thing to an LCN; this file is about the run's *length*, the
//! operand that bounds the transfer rather than placing it. See #243.

mod common;

use fs_ntfs::attr_io::{self, attr_off, AttrType};
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::data_runs;
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{mft_io, read, record_build, write};
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;
const FILE_SIZE: usize = 8192;

/// A 16 MiB NTFS volume with one non-resident file in the root.
fn volume_with_a_file(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = common::temp_image_path(format!("writerunbounds_{tag}"));
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);

    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("WRUN"), Some(1))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);

    let fs = Filesystem::mount_rw(&dst).expect("mount_rw");
    fs.create_file("/", "victim.bin").expect("create_file");
    fs.write_file_contents("/victim.bin", &vec![b'Z'; FILE_SIZE])
        .expect("write_file_contents");
    drop(fs);
    dst
}

/// Rewrite `/victim.bin`'s unnamed `$DATA` so its single run claims
/// `length` clusters, leaving every declared length field — and the LCN —
/// exactly as they were.
///
/// Only the run list changes, so the attribute still says it is an 8 KiB
/// file: a write of 512 bytes at offset 0 is inside `value_length` and
/// inside the run, and the only thing that can refuse it is the bound
/// this test is about.
fn stretch_data_run(img: &str, length: u64) {
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
        runs[0].length = length;
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

    // Read it back through the driver's own decoder, so a later refusal
    // is about the bound and not about a record this helper mangled.
    let (_, record) = mft_io::read_mft_record_io(&mut io, record_number).expect("re-read record");
    let loc = attr_io::find_attribute(&record, AttrType::Data, None).expect("$DATA still there");
    let mpo = loc.non_resident_mapping_pairs_offset.unwrap() as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])
            .expect("decode the rewritten runs");
    assert_eq!(runs.len(), 1, "still one extent");
    assert_eq!(
        runs[0].length, length,
        "the record should now claim a run of {length} clusters"
    );
    assert_eq!(
        loc.non_resident_value_length,
        Some(FILE_SIZE as u64),
        "the rebuilt attribute must keep its declared data length"
    );
}

#[test]
fn a_run_whose_end_offset_overflows_is_refused_rather_than_wrapped() {
    let img = volume_with_a_file("overflow");
    // 2^52 clusters of 4096 bytes is exactly 2^64.
    let length = 1u64 << 52;
    stretch_data_run(&img, length);

    let got = write::write_at(Path::new(&img), "/victim.bin", 0, &[b'A'; 512]);
    let err = match got {
        Err(e) => e,
        Ok(n) => panic!(
            "a $DATA run claiming {length} clusters at a {CLUSTER}-byte cluster size \
             puts its end byte past 2^64, yet the write reported {n} bytes written"
        ),
    };
    assert!(
        err.contains("past the addressable end"),
        "expected the run-end refusal, got: {err}"
    );
}

/// The wrap's low-value half, reached without leaving the address space:
/// `2^52 + 1` clusters is `2^64 + 4096` bytes, which wrapped to 4096 —
/// *below* a write positioned at 8 KiB, so the subtraction underflowed
/// and `remaining.min(max_in_this_run)` stopped bounding the write to the
/// run at all. Both halves are one `checked_mul` now, and this asserts
/// the second one is covered rather than assuming the first stands for
/// it.
#[test]
fn a_run_whose_end_offset_wraps_low_is_refused_too() {
    let img = volume_with_a_file("wrap_low");
    let length = (1u64 << 52) + 1;
    stretch_data_run(&img, length);

    let got = write::write_at(Path::new(&img), "/victim.bin", 4096, &[b'A'; 512]);
    assert!(
        got.is_err(),
        "a run of {length} clusters wraps its end offset to 4096; a write at 4096 \
         must be refused, not bounded by an underflowed count"
    );
}

/// The control. Same image, same entry point, a run list left alone —
/// so the refusals above are about the hostile run and not about
/// `write_at` having stopped working.
#[test]
fn a_well_formed_run_still_takes_the_write() {
    let img = volume_with_a_file("control");
    let payload = [b'A'; 512];
    let n = write::write_at(Path::new(&img), "/victim.bin", 0, &payload)
        .expect("a write inside a well-formed single-extent file");
    assert_eq!(n, payload.len() as u64);

    let fs = Filesystem::mount(&img).expect("mount");
    let mut back = vec![0u8; FILE_SIZE];
    let read = fs
        .read_file("/victim.bin", 0, &mut back)
        .expect("read_file");
    assert_eq!(
        read, FILE_SIZE,
        "the write must not have changed the file's length"
    );
    assert_eq!(&back[..512], &payload[..], "the bytes we wrote");
    assert!(
        back[512..].iter().all(|&b| b == b'Z'),
        "the rest of the file must be untouched"
    );
}
