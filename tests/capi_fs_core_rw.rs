//! C-ABI smoke for the RW `FsCoreDevice` mount entry point.
//!
//! Mirrors `capi_handle_rw.rs` but routes the underlying device through
//! the shared `fs_core` block-device handle instead of a direct
//! `(read_fn, write_fn)` callback pair. This is the path consumers take
//! when they want to mount NTFS off a generic device (a virtual disk
//! image reader, a partition slice, an in-process file device) without
//! writing per-source FFI glue.
//!
//! Flow:
//!   1. Copy the shared NTFS fixture to a per-test image so we don't
//!      mutate it.
//!   2. `fs_core_file_open(..., writable=true)` to get an
//!      `*FsCoreDevice` handle for the copy.
//!   3. `fs_ntfs_mount_rw_with_fs_core_device` to mount.
//!   4. Exercise `fs_ntfs_create_file_h` + `fs_ntfs_write_file_contents_h`
//!      and verify the bytes round-trip.
//!   5. Unmount, close the device, reopen RO via the existing
//!      `fs_ntfs_mount_with_fs_core_device` entry, and read the file
//!      back to confirm the writes hit the device.

#![allow(unused_unsafe)]

use std::ffi::{c_char, c_void, CString};

use fs_ntfs::{
    fs_ntfs_clear_last_error, fs_ntfs_create_file_h, fs_ntfs_last_errno, fs_ntfs_last_error,
    fs_ntfs_mount_rw_with_fs_core_device, fs_ntfs_mount_with_fs_core_device, fs_ntfs_read_file,
    fs_ntfs_umount, fs_ntfs_write_file_contents_h,
};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn copy_fixture(tag: &str) -> String {
    let dst = format!("test-disks/_capi_fs_core_rw_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy fixture");
    dst
}

fn last_error() -> String {
    unsafe {
        let p = fs_ntfs_last_error();
        if p.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[test]
fn fs_core_rw_mount_round_trip_via_file_device() {
    let img = copy_fixture("round_trip");
    let img_c = CString::new(img.as_str()).unwrap();

    // -- Open the image as a writable FsCoreDevice -------------------
    let dev = unsafe { fs_core::ffi::fs_core_file_open(img_c.as_ptr(), true) };
    assert!(!dev.is_null(), "fs_core_file_open(rw) returned NULL");

    fs_ntfs_clear_last_error();
    let fs = unsafe { fs_ntfs_mount_rw_with_fs_core_device(dev) };
    assert!(
        !fs.is_null(),
        "fs_ntfs_mount_rw_with_fs_core_device failed: errno={} err={}",
        fs_ntfs_last_errno(),
        last_error()
    );

    // -- create_file_h ----------------------------------------------
    let parent = CString::new("/").unwrap();
    let base = CString::new("fs_core_rw_test.txt").unwrap();
    let full = CString::new("/fs_core_rw_test.txt").unwrap();

    fs_ntfs_clear_last_error();
    let rn = unsafe {
        fs_ntfs_create_file_h(
            fs,
            parent.as_ptr() as *const c_char,
            base.as_ptr() as *const c_char,
        )
    };
    assert!(
        rn > 0,
        "create_file_h returned {rn}; errno={} err={}",
        fs_ntfs_last_errno(),
        last_error()
    );

    // -- write_file_contents_h --------------------------------------
    // 32 bytes — comfortably under any record-resident threshold so we
    // don't get tangled up in non-resident promotion semantics here.
    let payload: &[u8] = b"fs-core RW round-trip OK!!!\n\0\0\0\0";
    assert_eq!(payload.len(), 32);

    fs_ntfs_clear_last_error();
    let n = unsafe {
        fs_ntfs_write_file_contents_h(
            fs,
            full.as_ptr() as *const c_char,
            payload.as_ptr() as *const c_void,
            payload.len() as u64,
        )
    };
    assert_eq!(
        n,
        payload.len() as i64,
        "write_file_contents_h returned {n}; errno={} err={}",
        fs_ntfs_last_errno(),
        last_error()
    );

    // -- Unmount + close device --------------------------------------
    unsafe { fs_ntfs_umount(fs) };
    unsafe { fs_core::ffi::fs_core_device_close(dev) };

    // -- Re-open RO via the existing entry point and read it back ----
    let dev_ro = unsafe { fs_core::ffi::fs_core_file_open(img_c.as_ptr(), false) };
    assert!(!dev_ro.is_null(), "fs_core_file_open(ro) returned NULL");

    fs_ntfs_clear_last_error();
    let fs_ro = unsafe { fs_ntfs_mount_with_fs_core_device(dev_ro) };
    assert!(
        !fs_ro.is_null(),
        "RO remount failed: errno={} err={}",
        fs_ntfs_last_errno(),
        last_error()
    );

    let mut buf = vec![0u8; payload.len()];
    let read_n = unsafe {
        fs_ntfs_read_file(
            fs_ro,
            full.as_ptr() as *const c_char,
            buf.as_mut_ptr() as *mut c_void,
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(
        read_n,
        payload.len() as i64,
        "read_file returned {read_n}; errno={} err={}",
        fs_ntfs_last_errno(),
        last_error()
    );
    assert_eq!(&buf[..], payload, "round-tripped bytes don't match");

    unsafe { fs_ntfs_umount(fs_ro) };
    unsafe { fs_core::ffi::fs_core_device_close(dev_ro) };

    // Best-effort cleanup of the per-test image.
    let _ = std::fs::remove_file(&img);
}

#[test]
fn fs_core_rw_rejects_null_handle() {
    fs_ntfs_clear_last_error();
    let fs = unsafe { fs_ntfs_mount_rw_with_fs_core_device(std::ptr::null_mut()) };
    assert!(fs.is_null(), "expected NULL on null handle");
    assert!(
        last_error().contains("null"),
        "expected null-handle error, got: {}",
        last_error()
    );
}

#[test]
fn fs_core_ro_handle_mutator_returns_einval_with_helpful_message() {
    // Open RO via the read-only mount entry; mutator must report the
    // documented "handle mounted read-only" error so consumers can
    // distinguish a read-only mount from a missing mount source
    // ("handle has no recorded mount source") or a generic write failure.
    let img = copy_fixture("ro_einval");
    let img_c = CString::new(img.as_str()).unwrap();

    let dev = unsafe { fs_core::ffi::fs_core_file_open(img_c.as_ptr(), false) };
    assert!(!dev.is_null());
    let fs = unsafe { fs_ntfs_mount_with_fs_core_device(dev) };
    assert!(!fs.is_null());

    let parent = CString::new("/").unwrap();
    let base = CString::new("should_fail.txt").unwrap();

    fs_ntfs_clear_last_error();
    let rn = unsafe {
        fs_ntfs_create_file_h(
            fs,
            parent.as_ptr() as *const c_char,
            base.as_ptr() as *const c_char,
        )
    };
    assert_eq!(rn, -1, "RO mount mutator should fail, got rn={rn}");
    assert!(
        last_error().contains("handle mounted read-only"),
        "expected 'handle mounted read-only' error, got: {}",
        last_error()
    );

    unsafe { fs_ntfs_umount(fs) };
    unsafe { fs_core::ffi::fs_core_device_close(dev) };
    let _ = std::fs::remove_file(&img);
}
