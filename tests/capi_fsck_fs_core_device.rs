//! Smoke tests for the `FsCoreDevice`-based fsck C ABI:
//! `fs_ntfs_is_dirty_with_fs_core_device` + `fs_ntfs_fsck_with_fs_core_device`.
//!
//! Mirrors `capi_fsck_callbacks.rs` but exercises the path consumers
//! take when the underlying device is already an `FsCoreDevice` —
//! typically because a virtual-disk container (qcow2, vhd, …) was
//! opened first and the resulting handle is being threaded through
//! fs-ntfs without unwrapping back to a callback pair.
//!
//! Routes via `fs_core_file_open` for the device, since that's the
//! cheapest writable `FsCoreDevice` we can build in process.

#![allow(unused_unsafe)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{Read, Seek, SeekFrom, Write};

use ntfs::structured_values::NtfsVolumeFlags;
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType};

use fs_core::ffi::{fs_core_device_close, fs_core_file_open, FsCoreDevice};
use fs_ntfs::{
    fs_ntfs_fsck_with_fs_core_device, fs_ntfs_is_dirty_with_fs_core_device, fs_ntfs_last_error,
};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

// --------------------------------------------------------------------------
// Fixture helpers — copied from `capi_fsck_callbacks.rs`. The two test
// files exercise different FFI shapes against the same upstream
// fixture; copy-paste is cheaper than carving out a shared helper just
// for two callers.
// --------------------------------------------------------------------------

fn dirty_copy(tag: &str, dirty_flag: bool, corrupt_log: bool) -> String {
    let dst = format!("test-disks/_capi_fsck_fs_core_{tag}.img");
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

fn last_error() -> String {
    unsafe {
        let p = fs_ntfs_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn open_dev(path: &str, writable: bool) -> *mut FsCoreDevice {
    let cpath = CString::new(path).expect("cstring");
    let h = unsafe { fs_core_file_open(cpath.as_ptr(), writable) };
    assert!(!h.is_null(), "fs_core_file_open failed for {path}");
    h
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[test]
fn is_dirty_with_fs_core_device_returns_0_on_clean() {
    let img = dirty_copy("clean", false, false);
    let h = open_dev(&img, false);
    let rc = unsafe { fs_ntfs_is_dirty_with_fs_core_device(h) };
    unsafe { fs_core_device_close(h) };
    assert_eq!(rc, 0, "expected clean; last_error={}", last_error());
}

#[test]
fn is_dirty_with_fs_core_device_returns_1_on_dirty() {
    let img = dirty_copy("dirty", true, false);
    let h = open_dev(&img, false);
    let rc = unsafe { fs_ntfs_is_dirty_with_fs_core_device(h) };
    unsafe { fs_core_device_close(h) };
    assert_eq!(rc, 1, "expected dirty; last_error={}", last_error());
}

#[test]
fn is_dirty_with_fs_core_device_neg1_on_null() {
    let rc = unsafe { fs_ntfs_is_dirty_with_fs_core_device(std::ptr::null_mut()) };
    assert_eq!(rc, -1);
}

#[test]
fn fsck_with_fs_core_device_clears_dirty_and_resets_log() {
    let img = dirty_copy("full", true, true);
    let h = open_dev(&img, true); // writable

    let mut bytes: u64 = 0;
    let mut cleared: u8 = 0;
    let rc = unsafe {
        fs_ntfs_fsck_with_fs_core_device(h, None, std::ptr::null_mut(), &mut bytes, &mut cleared)
    };
    unsafe { fs_core_device_close(h) };

    assert_eq!(rc, 0, "expected success; last_error={}", last_error());
    assert!(bytes > 0, "expected non-zero logfile bytes overwritten");
    assert_eq!(cleared, 1, "expected dirty bit cleared");

    // Verify the writes hit disk: re-read flags by parsing the file again.
    let flags = read_volume_flags(&img);
    assert!(
        !flags.contains(NtfsVolumeFlags::IS_DIRTY),
        "dirty bit should be cleared on disk after fsck; flags={flags:?}"
    );
}

#[test]
fn fsck_with_fs_core_device_no_op_on_clean() {
    let img = dirty_copy("noop", false, false);
    let h = open_dev(&img, true);

    let mut bytes: u64 = 0;
    let mut cleared: u8 = 0;
    let rc = unsafe {
        fs_ntfs_fsck_with_fs_core_device(h, None, std::ptr::null_mut(), &mut bytes, &mut cleared)
    };
    unsafe { fs_core_device_close(h) };

    assert_eq!(
        rc,
        0,
        "expected success on clean; last_error={}",
        last_error()
    );
    assert_eq!(cleared, 0, "expected no dirty-bit clear");
}

#[test]
fn fsck_with_fs_core_device_rejects_ro_handle() {
    let img = dirty_copy("ro_reject", true, false);
    // Open RO — fsck must refuse since it needs to write.
    let h = open_dev(&img, false);

    let mut bytes: u64 = 0;
    let mut cleared: u8 = 0;
    let rc = unsafe {
        fs_ntfs_fsck_with_fs_core_device(h, None, std::ptr::null_mut(), &mut bytes, &mut cleared)
    };
    unsafe { fs_core_device_close(h) };

    assert_eq!(rc, -1, "expected -1 on RO device");
    let err = last_error();
    assert!(
        err.contains("not writable") || err.contains("RW"),
        "expected writability complaint; got: {err}"
    );
}

#[test]
fn fsck_with_fs_core_device_neg1_on_null() {
    let mut bytes: u64 = 0;
    let mut cleared: u8 = 0;
    let rc = unsafe {
        fs_ntfs_fsck_with_fs_core_device(
            std::ptr::null_mut(),
            None,
            std::ptr::null_mut(),
            &mut bytes,
            &mut cleared,
        )
    };
    assert_eq!(rc, -1);
}

#[test]
fn fsck_with_fs_core_device_progress_callback_fires() {
    use std::sync::Mutex;

    static EVENTS: Mutex<Vec<(String, u64, u64)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn on_progress(
        _ctx: *mut c_void,
        phase: *const c_char,
        done: u64,
        total: u64,
    ) -> i32 {
        let s = CStr::from_ptr(phase).to_string_lossy().into_owned();
        EVENTS.lock().expect("lock").push((s, done, total));
        0
    }

    let img = dirty_copy("progress", true, true);
    let h = open_dev(&img, true);

    EVENTS.lock().expect("lock").clear();

    let mut bytes: u64 = 0;
    let mut cleared: u8 = 0;
    let rc = unsafe {
        fs_ntfs_fsck_with_fs_core_device(
            h,
            Some(on_progress),
            std::ptr::null_mut(),
            &mut bytes,
            &mut cleared,
        )
    };
    unsafe { fs_core_device_close(h) };

    assert_eq!(rc, 0, "fsck failed; last_error={}", last_error());
    let events = EVENTS.lock().expect("lock");
    assert!(
        !events.is_empty(),
        "expected progress callbacks to fire during fsck"
    );
    // Sanity: at least one phase identifier should appear.
    let phases: Vec<&str> = events.iter().map(|(p, _, _)| p.as_str()).collect();
    assert!(
        phases.iter().any(|p| !p.is_empty()),
        "phases were all empty: {phases:?}"
    );
}
