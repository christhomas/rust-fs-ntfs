//! Tests driving the fs_ntfs_* C ABI for the fsck functions directly —
//! not the Rust `fsck::*` helpers. Validates that the FFI layer (CString
//! marshalling, out-pointer filling, return-code semantics, error
//! propagation through fs_ntfs_last_error) does what the header promises.

// The C ABI functions aren't marked `unsafe extern "C"` (separate issue
// tracked in STATUS.md), so Rust sees `unsafe { fs_ntfs_foo(...) }` as
// unnecessary. Keep the unsafe blocks here because they are the correct
// semantic annotation — the functions deref raw pointers internally.
#![allow(unused_unsafe)]

mod common;

use std::ffi::{CStr, CString};
use std::io::{Read, Seek, SeekFrom, Write};

use ntfs::structured_values::NtfsVolumeFlags;
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType};

// Call the shipped `pub extern "C" fn` items via their Rust paths. The
// ABI surface (C signature, CString paths, raw out-pointers, last_error
// thread-local) is identical to what a Swift/C consumer sees — only
// the symbol-lookup mechanism differs. Downstream consumers that link
// `libfs_ntfs.a` cover the actual link-time symbol path.
// See also: rust-fs-ext4 `tests/capi_basic.rs` for the same convention.
use fs_ntfs::{fs_ntfs_clear_dirty, fs_ntfs_fsck, fs_ntfs_last_error, fs_ntfs_reset_logfile};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn dirty_copy(tag: &str, dirty_flag: bool, corrupt_log: bool) -> String {
    let dst = format!("test-disks/_capi_fsck_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy fixture");

    if dirty_flag {
        let off = upstream_volume_flags_offset(&dst);
        patch_u16_le(&dst, off, |f| f | NtfsVolumeFlags::IS_DIRTY.bits());
    }
    if corrupt_log {
        let (off, _) = upstream_logfile_data_start(&dst);
        let junk = b"TXN leftover mid-transaction bytes XXXXXXX".repeat(32);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&dst)
            .expect("open rw");
        f.seek(SeekFrom::Start(off)).expect("seek");
        f.write_all(&junk).expect("write");
        f.sync_all().expect("fsync");
    }
    dst
}

fn upstream_volume_flags_offset(path: &str) -> u64 {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse");
    let vol = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::Volume as u64)
        .expect("open $Volume");
    let mut attrs = vol.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::VolumeInformation) {
            continue;
        }
        let attr_pos = a.position().value().expect("pos").get();
        drop(reader);
        let value_offset = read_u16_le_at(path, attr_pos + 0x14);
        return attr_pos + value_offset as u64 + 10;
    }
    panic!("no $VOLUME_INFORMATION");
}

fn upstream_logfile_data_start(path: &str) -> (u64, u64) {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse");
    let log = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::LogFile as u64)
        .expect("open $LogFile");
    let mut attrs = log.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let v = a.value(&mut reader).expect("value");
        let pos = v.data_position().value().expect("pos").get();
        return (pos, a.value_length());
    }
    panic!("no unnamed $DATA on $LogFile");
}

fn read_u16_le_at(path: &str, offset: u64) -> u16 {
    let mut f = std::fs::File::open(path).expect("open");
    f.seek(SeekFrom::Start(offset)).expect("seek");
    let mut buf = [0u8; 2];
    f.read_exact(&mut buf).expect("read");
    u16::from_le_bytes(buf)
}

fn patch_u16_le(path: &str, offset: u64, mutate: impl FnOnce(u16) -> u16) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open");
    f.seek(SeekFrom::Start(offset)).expect("seek read");
    let mut b = [0u8; 2];
    f.read_exact(&mut b).expect("read");
    let new = mutate(u16::from_le_bytes(b));
    f.seek(SeekFrom::Start(offset)).expect("seek write");
    f.write_all(&new.to_le_bytes()).expect("write");
    f.sync_all().expect("fsync");
}

fn read_volume_flags(path: &str) -> NtfsVolumeFlags {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse");
    ntfs.volume_info(&mut reader).expect("vi").flags()
}

fn read_logfile_first_page(path: &str) -> Vec<u8> {
    let (pos, _) = upstream_logfile_data_start(path);
    let mut f = std::fs::File::open(path).expect("open");
    f.seek(SeekFrom::Start(pos)).expect("seek");
    let mut buf = vec![0u8; 4096];
    f.read_exact(&mut buf).expect("read");
    buf
}

/// Snapshot the thread-local last_error string via the C ABI.
fn last_error() -> String {
    unsafe {
        let p = fs_ntfs_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

// ---- tests ----

#[test]
fn clear_dirty_returns_1_when_dirty() {
    let img = dirty_copy("cleared", true, false);
    let c = CString::new(img.as_str()).unwrap();
    let rc = unsafe { fs_ntfs_clear_dirty(c.as_ptr()) };
    assert_eq!(rc, 1, "expected 1 (cleared); last_error={}", last_error());
    assert!(!read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY));
}

#[test]
fn clear_dirty_returns_0_when_clean() {
    let img = dirty_copy("already_clean", false, false);
    let c = CString::new(img.as_str()).unwrap();
    let rc = unsafe { fs_ntfs_clear_dirty(c.as_ptr()) };
    assert_eq!(
        rc,
        0,
        "expected 0 (already clean); last_error={}",
        last_error()
    );
}

#[test]
fn clear_dirty_returns_neg1_on_null_path() {
    let rc = unsafe { fs_ntfs_clear_dirty(std::ptr::null()) };
    assert_eq!(rc, -1);
    let err = last_error();
    assert!(
        err.contains("null") || err.contains("UTF-8"),
        "expected null-path error; got {err:?}"
    );
}

#[test]
fn clear_dirty_returns_neg1_on_bad_path() {
    let c = CString::new("/nonexistent/definitely-missing.img").unwrap();
    let rc = unsafe { fs_ntfs_clear_dirty(c.as_ptr()) };
    assert_eq!(rc, -1);
    let err = last_error();
    assert!(
        !err.is_empty(),
        "last_error should carry a message after failure"
    );
}

#[test]
fn reset_logfile_returns_bytes_written() {
    let img = dirty_copy("reset_log", false, true);
    let c = CString::new(img.as_str()).unwrap();
    let n = unsafe { fs_ntfs_reset_logfile(c.as_ptr()) };
    assert!(
        n > 0,
        "expected >0 bytes; got {n}; last_error={}",
        last_error()
    );
    assert!(read_logfile_first_page(&img).iter().all(|&b| b == 0xFF));
}

#[test]
fn fsck_fills_out_params() {
    let img = dirty_copy("fsck_out", true, true);
    let c = CString::new(img.as_str()).unwrap();
    let mut bytes: u64 = 0;
    let mut cleared: u8 = 0;
    let rc = unsafe { fs_ntfs_fsck(c.as_ptr(), &mut bytes, &mut cleared) };
    assert_eq!(rc, 0, "expected success; last_error={}", last_error());
    assert!(bytes > 0);
    assert_eq!(cleared, 1);

    assert!(!read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY));
    assert!(read_logfile_first_page(&img).iter().all(|&b| b == 0xFF));
}

#[test]
fn fsck_accepts_null_out_params() {
    // Consumers that only want the repair and don't care about details
    // should be able to pass NULL for both out-params.
    let img = dirty_copy("fsck_null_out", true, true);
    let c = CString::new(img.as_str()).unwrap();
    let rc = unsafe { fs_ntfs_fsck(c.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, 0);
    assert!(!read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY));
}

#[test]
fn fsck_end_to_end_with_upstream_mount() {
    // The whole point: a dirty image goes through fsck, then upstream
    // parses + reads it fine (matching what FSKit would do on re-mount).
    let img = dirty_copy("fsck_e2e", true, true);
    let c = CString::new(img.as_str()).unwrap();
    let rc = unsafe { fs_ntfs_fsck(c.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, 0);

    let (ntfs, mut reader) = common::open(&img);
    let names = common::list_names(&ntfs, &mut reader, "/");
    assert!(names.iter().any(|n| n == "hello.txt"), "{names:?}");
}
