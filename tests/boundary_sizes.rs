//! Phase 2.3 + Phase 4.2 — `$DATA` resident/non-resident forms and
//! file-size / filename-length boundary coverage.
//!
//! Each test formats a fresh temp-file volume, writes via our own write API,
//! then reads back through the upstream `ntfs` crate (independent of our
//! parsers) to verify the on-disk form: resident vs non-resident, value
//! length, allocated-size cluster alignment, and exact byte round-trip.
//!
//! Resident vs non-resident is NOT a fixed byte threshold — it is bounded by
//! free space in the MFT record (`bytes_allocated - bytes_used`). So instead
//! of hard-coding a number, `resident_ceiling()` probes it empirically: the
//! largest size `write_resident_contents` accepts. One byte over must be
//! rejected by the resident path and require promotion.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;
/// MFT record size this suite formats with (4th arg to `format_filesystem`).
/// The resident ceiling for any attribute is bounded by this, minus the
/// FILE header + other attributes' overhead.
const MFT_RECORD_SIZE: usize = 4096;

/// Format a fresh NTFS volume into a temp image file and return its path.
fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_bnd_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create temp image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("BNDTEST"),
        Some(0xB0DA_5125),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// The unnamed `$DATA` attribute's on-disk shape after read-back.
struct DataAttr {
    is_resident: bool,
    value_length: u64,
    allocated_length: u64,
    bytes: Vec<u8>,
}

/// Navigate to `file_path` and return its unnamed `$DATA` attribute's form
/// + full byte contents, read via the upstream `ntfs` crate.
fn read_data_attr(img: &str, file_path: &str) -> DataAttr {
    let f = std::fs::File::open(img).expect("open image");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut reader).expect("upcase");

    let mut cur = ntfs.root_directory(&mut reader).expect("root dir");
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut reader).expect("dir_index");
        let mut finder = idx.finder();
        let entry = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .expect("find result")
            .expect("find ok");
        cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
    }

    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let attr = item.to_attribute().expect("to_attribute");
        if attr.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !attr.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue; // named stream (ADS) — skip, we want the default stream
        }
        let is_resident = attr.is_resident();
        let value_length = attr.value_length();
        // Read full contents.
        let mut value = attr.value(&mut reader).expect("value");
        // For non-resident data, the true on-disk allocation is the
        // cluster-rounded `allocated_size()` — NOT value_length (the logical
        // size). Reading value_length here would make the alignment assertion
        // trivially equal to the value_length check.
        // True on-disk allocation = sum of the data runs' cluster-aligned
        // sizes (the crate exposes allocated_size() per NtfsDataRun, not on
        // the value as a whole). value_length/len() would be the logical size.
        let allocated_length: u64 = match &value {
            ntfs::attribute_value::NtfsAttributeValue::NonResident(nr) => nr
                .data_runs()
                .map(|r| r.expect("data run").allocated_size())
                .sum(),
            _ => 0,
        };
        let mut bytes = vec![0u8; value_length as usize];
        let mut off = 0usize;
        while off < bytes.len() {
            let n = value.read(&mut reader, &mut bytes[off..]).expect("read");
            if n == 0 {
                break;
            }
            off += n;
        }
        return DataAttr {
            is_resident,
            value_length,
            allocated_length,
            bytes,
        };
    }
    panic!("no unnamed $DATA attribute for {file_path}");
}

/// Empirically find the largest payload `write_resident_contents` accepts for a
/// file named `name` on volume `img` — the resident ceiling for THIS file's
/// layout. The ceiling depends on the filename: a longer name (or one needing a
/// separate DOS 8.3 `$FILE_NAME`) enlarges the record's other attributes and
/// shrinks the room left for resident `$DATA`. So callers MUST probe with the
/// exact name they then test, on a fresh volume of their own (tests run in
/// parallel; a shared probe file would race).
fn resident_ceiling_on(img: &str, name: &str) -> usize {
    let path = format!("/{name}");
    // Binary search the largest size that stays resident. Upper bound is the
    // MFT record size — no resident value can exceed the whole record.
    let (mut lo, mut hi) = (0usize, MFT_RECORD_SIZE);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        write::create_file(Path::new(img), "/", name).ok();
        let payload = vec![0x5Au8; mid];
        let ok = write::write_resident_contents(Path::new(img), &path, &payload).is_ok();
        // Remove for the next probe so we always start from a fresh file.
        write::unlink(Path::new(img), &path).ok();
        if ok {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

// ---------------------------------------------------------------------------
// Empty + tiny resident files
// ---------------------------------------------------------------------------

#[test]
fn empty_file_data_is_resident_zero_length() {
    let img = fresh_vol("empty");
    write::create_file(Path::new(&img), "/", "empty.bin").expect("create");
    let d = read_data_attr(&img, "/empty.bin");
    assert!(d.is_resident, "fresh empty file $DATA must be resident");
    assert_eq!(d.value_length, 0, "empty file value_length must be 0");
    assert!(d.bytes.is_empty());
}

#[test]
fn one_byte_file_is_resident() {
    let img = fresh_vol("onebyte");
    write::create_file(Path::new(&img), "/", "one.bin").expect("create");
    write::write_resident_contents(Path::new(&img), "/one.bin", &[0xAB]).expect("write");
    let d = read_data_attr(&img, "/one.bin");
    assert!(d.is_resident, "1-byte $DATA must stay resident");
    assert_eq!(d.value_length, 1);
    assert_eq!(d.bytes, vec![0xAB]);
}

#[test]
fn small_100_byte_file_is_resident_exact_roundtrip() {
    let img = fresh_vol("small100");
    write::create_file(Path::new(&img), "/", "s.bin").expect("create");
    let payload: Vec<u8> = (0u8..100).collect();
    write::write_resident_contents(Path::new(&img), "/s.bin", &payload).expect("write");
    let d = read_data_attr(&img, "/s.bin");
    assert!(d.is_resident);
    assert_eq!(d.value_length, 100);
    assert_eq!(d.bytes, payload, "100-byte content must round-trip exactly");
}

// ---------------------------------------------------------------------------
// Resident ceiling boundary: largest-resident vs ceiling+1
// ---------------------------------------------------------------------------

#[test]
fn resident_ceiling_is_positive_and_under_record_size() {
    let img = fresh_vol("ceiling_positive");
    let ceiling = resident_ceiling_on(&img, "p.bin");
    assert!(ceiling > 0, "resident ceiling must be > 0");
    assert!(
        ceiling < MFT_RECORD_SIZE,
        "resident ceiling {ceiling} must be < MFT record size ({MFT_RECORD_SIZE}) \
         — the FILE header + $STD_INFO + $FILE_NAME + $DATA header all share the record"
    );
}

#[test]
fn exactly_at_resident_ceiling_stays_resident() {
    let img = fresh_vol("at_ceiling");
    let ceiling = resident_ceiling_on(&img, "c.bin");
    write::create_file(Path::new(&img), "/", "c.bin").expect("create");
    let payload = vec![0xC3u8; ceiling];
    write::write_resident_contents(Path::new(&img), "/c.bin", &payload)
        .expect("write at ceiling must succeed");
    let d = read_data_attr(&img, "/c.bin");
    assert!(d.is_resident, "data exactly at ceiling must stay resident");
    assert_eq!(d.value_length, ceiling as u64);
    assert_eq!(d.bytes, payload);
}

#[test]
fn one_over_ceiling_rejected_by_resident_path() {
    let img = fresh_vol("over_ceiling");
    let ceiling = resident_ceiling_on(&img, "o.bin");
    write::create_file(Path::new(&img), "/", "o.bin").expect("create");
    let payload = vec![0x7Eu8; ceiling + 1];
    let res = write::write_resident_contents(Path::new(&img), "/o.bin", &payload);
    assert!(
        res.is_err(),
        "ceiling+1 ({}) must be rejected by the resident write path",
        ceiling + 1
    );
}

// ---------------------------------------------------------------------------
// Promotion to non-resident
// ---------------------------------------------------------------------------

#[test]
fn promote_makes_data_nonresident_with_exact_roundtrip() {
    let img = fresh_vol("promote");
    write::create_file(Path::new(&img), "/", "p.bin").expect("create");
    let payload: Vec<u8> = (0..8192u32).map(|i| (i % 256) as u8).collect();
    write::promote_resident_data_to_nonresident(Path::new(&img), "/p.bin", &payload)
        .expect("promote");
    let d = read_data_attr(&img, "/p.bin");
    assert!(!d.is_resident, "8192-byte $DATA must be non-resident");
    assert_eq!(d.value_length, 8192);
    assert_eq!(d.bytes, payload, "promoted content must round-trip exactly");
}

#[test]
fn exactly_one_cluster_is_nonresident() {
    let img = fresh_vol("one_cluster");
    write::create_file(Path::new(&img), "/", "cl.bin").expect("create");
    let payload = vec![0x42u8; CLUSTER as usize]; // exactly 4096 bytes
    write::promote_resident_data_to_nonresident(Path::new(&img), "/cl.bin", &payload)
        .expect("promote");
    let d = read_data_attr(&img, "/cl.bin");
    assert!(!d.is_resident);
    assert_eq!(d.value_length, CLUSTER as u64);
    assert_eq!(d.bytes, payload);
}

#[test]
fn multi_cluster_nonresident_spans_clusters() {
    let img = fresh_vol("multi_cluster");
    write::create_file(Path::new(&img), "/", "m.bin").expect("create");
    // 10000 bytes > 2 clusters (8192) and < 3 clusters (12288).
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 256) as u8).collect();
    write::promote_resident_data_to_nonresident(Path::new(&img), "/m.bin", &payload)
        .expect("promote");
    let d = read_data_attr(&img, "/m.bin");
    assert!(!d.is_resident);
    assert_eq!(d.value_length, 10_000);
    // 10 000 bytes rounds up to 3 clusters (12 288) on a 4096-byte-cluster
    // volume — this is what actually verifies cluster-boundary allocation,
    // distinct from the logical value_length above.
    assert_eq!(
        d.allocated_length, 12_288,
        "allocated size must be cluster-rounded (3 × 4096), not the logical 10 000"
    );
    assert_eq!(d.bytes, payload, "10KB multi-cluster content round-trips");
}

#[test]
fn nonresident_first_and_last_bytes_correct() {
    let img = fresh_vol("first_last");
    write::create_file(Path::new(&img), "/", "fl.bin").expect("create");
    let mut payload = vec![0u8; 9000];
    payload[0] = 0xFE;
    payload[8999] = 0xDC;
    write::promote_resident_data_to_nonresident(Path::new(&img), "/fl.bin", &payload)
        .expect("promote");
    let d = read_data_attr(&img, "/fl.bin");
    assert!(!d.is_resident);
    assert_eq!(d.bytes[0], 0xFE, "first byte across data runs");
    assert_eq!(d.bytes[8999], 0xDC, "last byte across data runs");
}

// ---------------------------------------------------------------------------
// Filename-length boundaries (Phase 4.2)
// ---------------------------------------------------------------------------

#[test]
fn filename_length_1_roundtrips() {
    let img = fresh_vol("fname1");
    write::create_file(Path::new(&img), "/", "x").expect("create 1-char");
    // Read back: the file must be navigable by its 1-char name.
    let d = read_data_attr(&img, "/x");
    assert_eq!(d.value_length, 0);
}

#[test]
fn filename_length_255_is_max_and_roundtrips() {
    let img = fresh_vol("fname255");
    let name = "a".repeat(255); // 255 UTF-16 code units = NTFS max
    write::create_file(Path::new(&img), "/", &name).expect("create 255-char");
    let path = format!("/{name}");
    let d = read_data_attr(&img, &path);
    assert_eq!(d.value_length, 0, "255-char-named file must be navigable");
}

#[test]
fn filename_length_256_is_rejected() {
    let img = fresh_vol("fname256");
    let name = "a".repeat(256); // one over the NTFS 255 limit
    let res = write::create_file(Path::new(&img), "/", &name);
    assert!(
        res.is_err(),
        "256-char filename must be rejected (NTFS max is 255)"
    );
}
