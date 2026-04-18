//! Tests for the resident-attribute resize primitive. The easiest
//! attribute to mutate safely is `$VOLUME_NAME` on record 3 — changing
//! its value renames the volume label, which upstream reads back.

use fs_ntfs::attr_io::{self, AttrType};
use fs_ntfs::attr_resize::{resize_resident_value, set_resident_value};
use fs_ntfs::mft_io::{read_mft_record, update_mft_record};
use ntfs::{KnownNtfsFileRecordNumber, Ntfs};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_attr_resize_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn read_volume_name(img: &str) -> String {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).unwrap();
    let name = ntfs.volume_name(&mut r).expect("present").expect("ok");
    name.name().to_string_lossy()
}

fn rename_volume(img: &str, new_name: &str) -> Result<(), String> {
    // Encode as UTF-16 LE.
    let utf16: Vec<u16> = new_name.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(utf16.len() * 2);
    for c in utf16 {
        bytes.extend_from_slice(&c.to_le_bytes());
    }
    update_mft_record(
        Path::new(img),
        KnownNtfsFileRecordNumber::Volume as u64,
        |record| {
            let loc = attr_io::find_attribute(record, AttrType::VolumeName, None)
                .ok_or("$VOLUME_NAME not found")?;
            set_resident_value(record, loc.attr_offset, &bytes)?;
            Ok(())
        },
    )
}

#[test]
fn resize_noop_same_length() {
    // Renaming to the same length name must be a pure byte overwrite.
    let img = working_copy("same_len");
    let before = read_volume_name(&img);
    assert_eq!(before, "BasicNTFS");
    rename_volume(&img, "ReplaceLE").expect("rename"); // also 9 chars
    assert_eq!(read_volume_name(&img), "ReplaceLE");
}

#[test]
fn resize_shrink_smaller_name() {
    let img = working_copy("shrink");
    rename_volume(&img, "Tiny").expect("rename");
    assert_eq!(read_volume_name(&img), "Tiny");
}

#[test]
fn resize_grow_larger_name() {
    let img = working_copy("grow");
    // A much longer name than "BasicNTFS" (9 chars) — 32 chars.
    let new_name = "VolumeLabelAfterGrowingTheRecord";
    rename_volume(&img, new_name).expect("rename");
    assert_eq!(read_volume_name(&img), new_name);
}

#[test]
fn resize_to_zero_length() {
    let img = working_copy("zero_len");
    rename_volume(&img, "").expect("rename");
    // Upstream may either return an empty name or Ok(empty). Volume
    // label can legitimately be empty.
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).unwrap();
    match ntfs.volume_name(&mut r) {
        Some(Ok(n)) => assert!(n.name().to_string_lossy().is_empty()),
        None => {} // also fine — no label.
        Some(Err(_)) => panic!("upstream errored reading empty name"),
    }
}

#[test]
fn resize_round_trip_shrink_then_grow() {
    let img = working_copy("shrink_grow");
    rename_volume(&img, "X").expect("shrink");
    assert_eq!(read_volume_name(&img), "X");
    rename_volume(&img, "BackToMedium").expect("grow");
    assert_eq!(read_volume_name(&img), "BackToMedium");
}

#[test]
fn resize_huge_grow_rejected_if_no_space() {
    let img = working_copy("too_big");
    // Try a 512-char name. The MFT record is 1024 bytes total, so 1024
    // bytes of UTF-16 = 2048 bytes definitely won't fit.
    let huge = "A".repeat(512);
    let err = rename_volume(&img, &huge).unwrap_err();
    assert!(
        err.contains("capacity") || err.contains("exceeds"),
        "{err:?}"
    );
    // Record should still be mountable at its original name.
    assert_eq!(read_volume_name(&img), "BasicNTFS");
}

#[test]
fn resize_preserves_subsequent_attributes() {
    // Renaming the volume changes $VOLUME_NAME (attribute order roughly:
    // SI, VolumeName, VolumeInformation, end). After a resize, the
    // $VOLUME_INFORMATION attribute must still be present and parseable.
    let img = working_copy("preserve_next");
    rename_volume(&img, "NewShorter").expect("rename");

    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).unwrap();
    let vi = ntfs.volume_info(&mut r).expect("still parseable");
    assert!(vi.major_version() >= 3);
}

#[test]
fn resize_low_level_primitive_shrink_and_grow() {
    // Exercise resize_resident_value directly on a scratch buffer.
    // Build a minimal record with a single resident attribute.
    let img = working_copy("low_level_direct");
    let (_, mut record) =
        read_mft_record(Path::new(&img), KnownNtfsFileRecordNumber::Volume as u64).unwrap();

    let loc = attr_io::find_attribute(&record, AttrType::VolumeName, None).unwrap();
    let orig_len = loc.resident_value_length.unwrap();

    // Shrink by 2 bytes (1 UTF-16 char).
    resize_resident_value(&mut record, loc.attr_offset, orig_len - 2).unwrap();
    let loc2 = attr_io::find_attribute(&record, AttrType::VolumeName, None).unwrap();
    assert_eq!(loc2.resident_value_length.unwrap(), orig_len - 2);

    // Then grow by 4 bytes beyond original.
    resize_resident_value(&mut record, loc2.attr_offset, orig_len + 4).unwrap();
    let loc3 = attr_io::find_attribute(&record, AttrType::VolumeName, None).unwrap();
    assert_eq!(loc3.resident_value_length.unwrap(), orig_len + 4);
}
