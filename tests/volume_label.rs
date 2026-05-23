//! Tests for `set_volume_label` / `read_volume_label` (audit follow-up).

#![allow(unused_unsafe)]

use fs_ntfs::facade::Filesystem;
use fs_ntfs::write::{read_volume_label, set_volume_label, VOLUME_LABEL_MAX_UTF16};
use fs_ntfs::{fs_ntfs_last_error, fs_ntfs_read_volume_label, fs_ntfs_set_volume_label};
use std::ffi::{CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_vollabel_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
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

#[test]
fn read_returns_fixture_label() {
    let label = read_volume_label(std::path::Path::new(BASIC_IMG)).unwrap();
    // ntfs-basic fixture's label is "BASIC" (per the qemu+Alpine
    // pipeline naming convention). Don't pin the exact bytes — the
    // fixture script might change — but assert non-empty.
    assert!(!label.is_empty(), "fixture should ship a non-empty label");
}

#[test]
fn write_then_read_roundtrips() {
    let img = working_copy("write_roundtrip");
    set_volume_label(std::path::Path::new(&img), "MYLABEL").unwrap();
    let got = read_volume_label(std::path::Path::new(&img)).unwrap();
    assert_eq!(got, "MYLABEL");
}

#[test]
fn write_empty_removes_label() {
    let img = working_copy("write_empty");
    set_volume_label(std::path::Path::new(&img), "SOMETHING").unwrap();
    assert_eq!(read_volume_label(std::path::Path::new(&img)).unwrap(), "SOMETHING");
    set_volume_label(std::path::Path::new(&img), "").unwrap();
    assert_eq!(read_volume_label(std::path::Path::new(&img)).unwrap(), "");
}

#[test]
fn write_at_max_length() {
    let img = working_copy("write_max");
    // Exactly 32 UTF-16 code units (all ASCII = 32 chars).
    let max = "A".repeat(VOLUME_LABEL_MAX_UTF16);
    set_volume_label(std::path::Path::new(&img), &max).unwrap();
    assert_eq!(read_volume_label(std::path::Path::new(&img)).unwrap(), max);
}

#[test]
fn write_too_long_rejected() {
    let img = working_copy("write_too_long");
    let too_long = "X".repeat(VOLUME_LABEL_MAX_UTF16 + 1);
    let err = set_volume_label(std::path::Path::new(&img), &too_long).unwrap_err();
    assert!(err.contains("too long"), "expected 'too long' in error: {err}");
}

#[test]
fn write_unicode_label() {
    let img = working_copy("write_unicode");
    let label = "café-CJK-🦀-名前"; // mixed BMP + non-BMP
    set_volume_label(std::path::Path::new(&img), label).unwrap();
    assert_eq!(read_volume_label(std::path::Path::new(&img)).unwrap(), label);
}

#[test]
fn upstream_mounts_after_label_write() {
    let img = working_copy("upstream_mount");
    set_volume_label(std::path::Path::new(&img), "REMOUNT").unwrap();
    let _fs = Filesystem::mount(&img).expect("upstream re-mount");
}

#[test]
fn upstream_mounts_after_label_remove() {
    let img = working_copy("upstream_mount_empty");
    set_volume_label(std::path::Path::new(&img), "").unwrap();
    let _fs = Filesystem::mount(&img).expect("upstream re-mount");
}

#[test]
fn capi_set_label_roundtrips() {
    let img = working_copy("capi_roundtrip");
    let img_c = CString::new(img.clone()).unwrap();
    let label_c = CString::new("CAPILBL").unwrap();
    let rc = unsafe { fs_ntfs_set_volume_label(img_c.as_ptr(), label_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    let mut buf = vec![0u8; 128];
    let n = unsafe {
        fs_ntfs_read_volume_label(img_c.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len())
    };
    assert_eq!(n, 7);
    assert_eq!(&buf[..7], b"CAPILBL");
}

#[test]
fn capi_set_label_null_removes() {
    let img = working_copy("capi_null_removes");
    let img_c = CString::new(img.clone()).unwrap();
    let rc = unsafe { fs_ntfs_set_volume_label(img_c.as_ptr(), std::ptr::null()) };
    assert_eq!(rc, 0, "err={}", last_error());
    let mut buf = [0u8; 128];
    let n = unsafe {
        fs_ntfs_read_volume_label(img_c.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len())
    };
    assert_eq!(n, 0);
}

#[test]
fn capi_read_truncates_silently() {
    let img = working_copy("capi_truncate");
    let img_c = CString::new(img.clone()).unwrap();
    let label_c = CString::new("LONGER_LABEL").unwrap();
    unsafe { fs_ntfs_set_volume_label(img_c.as_ptr(), label_c.as_ptr()) };
    let mut buf = [0u8; 4]; // only 4 bytes
    let n = unsafe {
        fs_ntfs_read_volume_label(img_c.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len())
    };
    assert_eq!(n, 4);
    assert_eq!(&buf, b"LONG");
}
