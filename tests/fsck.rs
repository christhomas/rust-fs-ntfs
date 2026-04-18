//! Regression tests for `fs_ntfs::fsck` (clear_dirty / reset_logfile / fsck).
//!
//! We can't get ntfs-3g to produce a dirty fixture reliably — it either
//! doesn't set the flag, or it resets `$LogFile` on mount. So each test
//! starts from a clean copy of `ntfs-basic.img` and synthesizes the
//! dirty state itself, **using upstream `ntfs` crate APIs only**. That
//! keeps test setup independent of the code under test.

mod common;

use std::io::{Read, Seek, SeekFrom, Write};

use fs_ntfs::fsck;
use ntfs::structured_values::NtfsVolumeFlags;
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

/// Copy basic.img into a unique working file so destructive tests don't
/// race with each other, then patch in the requested dirty state.
fn dirty_copy(tag: &str, dirty_flag: bool, corrupt_logfile: bool) -> String {
    let dst = format!("test-disks/_fsck_test_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy basic fixture");

    if dirty_flag {
        let offset = upstream_volume_flags_offset(&dst);
        patch_u16_le(&dst, offset, |cur| cur | NtfsVolumeFlags::IS_DIRTY.bits());
    }

    if corrupt_logfile {
        let (offset, _len) = upstream_logfile_data_start(&dst);
        // Write a recognizable non-0xFF pattern into the first page — this
        // is what reset_logfile is supposed to overwrite. Using "RSTR"
        // header bytes so it also resembles a partial log restart page.
        let junk = b"RSTR\xde\xad\xbe\xef leftover transaction data \x00".repeat(64);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&dst)
            .expect("open rw");
        f.seek(SeekFrom::Start(offset)).expect("seek");
        f.write_all(&junk).expect("write");
        f.sync_all().expect("fsync");
    }

    dst
}

/// Locate the on-disk byte offset of the 2-byte `flags` field inside
/// `$Volume`'s `$VOLUME_INFORMATION` attribute, using upstream only.
///
/// Note: upstream `NtfsResidentAttributeValue::data_position()` is named
/// misleadingly — it returns the attribute *header* start, not the
/// resident data start. Real data starts at `attribute.position() +
/// value_offset`, where `value_offset` is the u16 at offset +0x14 of
/// the attribute header (standard NTFS resident attribute layout).
fn upstream_volume_flags_offset(path: &str) -> u64 {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse ntfs");
    let vol = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::Volume as u64)
        .expect("open $Volume");

    let mut attrs = vol.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let attribute = item.to_attribute().expect("to_attr");
        if attribute.ty().ok() != Some(NtfsAttributeType::VolumeInformation) {
            continue;
        }
        let attr_pos = attribute.position().value().expect("attr pos").get();
        drop(reader);
        let value_offset = read_u16_le_at(path, attr_pos + 0x14);
        let data_start = attr_pos + value_offset as u64;
        // VI layout: reserved(8) + major(1) + minor(1) + flags(2).
        return data_start + 10;
    }
    panic!("no $VOLUME_INFORMATION");
}

fn read_u16_le_at(path: &str, offset: u64) -> u16 {
    let mut f = std::fs::File::open(path).expect("open");
    f.seek(SeekFrom::Start(offset)).expect("seek");
    let mut buf = [0u8; 2];
    f.read_exact(&mut buf).expect("read");
    u16::from_le_bytes(buf)
}

/// Locate `$LogFile`'s first-data-byte on-disk offset and total length.
fn upstream_logfile_data_start(path: &str) -> (u64, u64) {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse ntfs");
    let log = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::LogFile as u64)
        .expect("open $LogFile");

    let mut attrs = log.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let attribute = item.to_attribute().expect("to_attr");
        if attribute.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !attribute.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let value = attribute.value(&mut reader).expect("value");
        let pos = value.data_position().value().expect("pos").get();
        return (pos, attribute.value_length());
    }
    panic!("no unnamed $DATA on $LogFile");
}

fn patch_u16_le(path: &str, offset: u64, mutate: impl FnOnce(u16) -> u16) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open rw");
    f.seek(SeekFrom::Start(offset)).expect("seek read");
    let mut buf = [0u8; 2];
    f.read_exact(&mut buf).expect("read");
    let new = mutate(u16::from_le_bytes(buf));
    f.seek(SeekFrom::Start(offset)).expect("seek write");
    f.write_all(&new.to_le_bytes()).expect("write");
    f.sync_all().expect("fsync");
}

fn read_volume_flags(path: &str) -> NtfsVolumeFlags {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse");
    ntfs.volume_info(&mut reader).expect("volume_info").flags()
}

fn read_logfile_first_page(path: &str) -> Vec<u8> {
    let (pos, _len) = upstream_logfile_data_start(path);
    let mut f = std::fs::File::open(path).expect("open");
    f.seek(SeekFrom::Start(pos)).expect("seek");
    let mut buf = vec![0u8; 4096];
    f.read_exact(&mut buf).expect("read");
    buf
}

// ---------- tests ----------

#[test]
fn clear_dirty_flips_flag_on_dirty_volume() {
    let img = dirty_copy("clear", true, false);
    assert!(
        read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY),
        "setup: image should start dirty"
    );

    let changed = fsck::clear_dirty(&img).expect("clear_dirty");
    assert!(changed, "should report it cleared the flag");

    let after = read_volume_flags(&img);
    assert!(
        !after.contains(NtfsVolumeFlags::IS_DIRTY),
        "IS_DIRTY should be clear; got {after:?}"
    );
}

#[test]
fn clear_dirty_is_idempotent_on_clean_volume() {
    // Fresh copy — IS_DIRTY not set by our synth step.
    let img = dirty_copy("idempotent", false, false);
    assert!(!read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY));

    let changed = fsck::clear_dirty(&img).expect("clear_dirty");
    assert!(!changed, "clean volume should be no-op");

    // Image is still a valid NTFS volume.
    let (_ntfs, _r) = common::open(&img);
}

#[test]
fn clear_dirty_preserves_other_volume_flags() {
    // Set IS_DIRTY plus an unrelated marker bit (MOUNTED_ON_NT4 = 0x0008)
    // and confirm clear_dirty only removes IS_DIRTY.
    let img = dirty_copy("preserve", false, false);
    let offset = upstream_volume_flags_offset(&img);
    patch_u16_le(&img, offset, |f| {
        f | NtfsVolumeFlags::IS_DIRTY.bits() | NtfsVolumeFlags::MOUNTED_ON_NT4.bits()
    });

    let before = read_volume_flags(&img);
    assert!(before.contains(NtfsVolumeFlags::IS_DIRTY));
    assert!(before.contains(NtfsVolumeFlags::MOUNTED_ON_NT4));

    fsck::clear_dirty(&img).expect("clear_dirty");
    let after = read_volume_flags(&img);
    assert!(!after.contains(NtfsVolumeFlags::IS_DIRTY));
    assert!(
        after.contains(NtfsVolumeFlags::MOUNTED_ON_NT4),
        "unrelated flag must survive; got {after:?}"
    );
}

#[test]
fn reset_logfile_fills_with_0xff() {
    let img = dirty_copy("reset_log", false, true);

    let before = read_logfile_first_page(&img);
    assert!(
        !before.iter().all(|&b| b == 0xFF),
        "setup: $LogFile should have junk before reset"
    );

    let n = fsck::reset_logfile(&img).expect("reset_logfile");
    assert!(n > 0);

    let after = read_logfile_first_page(&img);
    assert!(
        after.iter().all(|&b| b == 0xFF),
        "$LogFile first page must be 0xFF; first byte = {:#x}",
        after[0]
    );
}

#[test]
fn reset_logfile_fills_entire_logfile() {
    // Check the tail too, not just the first page. Catches off-by-one /
    // early-exit bugs in the chunked-write loop.
    let img = dirty_copy("reset_log_full", false, true);

    let (pos, len) = upstream_logfile_data_start(&img);
    assert!(len >= 4096, "sanity: $LogFile at least 4 KiB");

    fsck::reset_logfile(&img).expect("reset_logfile");

    // Sample: first byte, last byte, mid byte.
    let mut f = std::fs::File::open(&img).expect("open");
    let offsets = [pos, pos + len / 2, pos + len - 1];
    for off in offsets {
        f.seek(SeekFrom::Start(off)).expect("seek");
        let mut b = [0u8; 1];
        f.read_exact(&mut b).expect("read");
        assert_eq!(b[0], 0xFF, "byte at offset {off} not 0xFF");
    }
}

#[test]
fn fsck_convenience_resets_both() {
    let img = dirty_copy("fsck_both", true, true);
    assert!(read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY));

    let report = fsck::fsck(&img).expect("fsck");
    assert!(report.dirty_cleared);
    assert!(report.logfile_bytes > 0);

    assert!(!read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY));
    assert!(read_logfile_first_page(&img).iter().all(|&b| b == 0xFF));
}

#[test]
fn repaired_image_is_readable_via_upstream() {
    // End-to-end: synth dirty → fsck → upstream mounts → content intact.
    let img = dirty_copy("e2e", true, true);
    fsck::fsck(&img).expect("fsck");

    let (ntfs, mut reader) = common::open(&img);

    let names = common::list_names(&ntfs, &mut reader, "/");
    assert!(
        names.iter().any(|n| n == "hello.txt"),
        "expected hello.txt after repair; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "Documents"),
        "expected Documents after repair; got {names:?}"
    );

    let content = common::read_file_all(&ntfs, &mut reader, "/hello.txt");
    assert_eq!(content, b"Hello from NTFS!\n");
    let nested = common::read_file_all(&ntfs, &mut reader, "/Documents/readme.txt");
    assert_eq!(nested, b"Test document content\n");
}
