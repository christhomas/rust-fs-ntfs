//! C-ABI tests for named-stream (ADS) functions.

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_delete_named_stream, fs_ntfs_last_error, fs_ntfs_write_named_stream};
use std::ffi::{c_void, CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_ads_{tag}.img");
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
fn capi_write_named_stream_happy() {
    let img = working_copy("happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let sn_c = CString::new("foo").unwrap();
    let data = b"stream data";
    let rc = unsafe {
        fs_ntfs_write_named_stream(
            img_c.as_ptr(),
            p_c.as_ptr(),
            sn_c.as_ptr(),
            data.as_ptr() as *const c_void,
            data.len() as u64,
        )
    };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_write_named_stream_rejects_null_name() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe {
        fs_ntfs_write_named_stream(
            img_c.as_ptr(),
            p_c.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("stream_name"));
}

#[test]
fn capi_write_named_stream_rejects_null_buf_with_len() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let sn_c = CString::new("foo").unwrap();
    let rc = unsafe {
        fs_ntfs_write_named_stream(
            img_c.as_ptr(),
            p_c.as_ptr(),
            sn_c.as_ptr(),
            std::ptr::null(),
            10,
        )
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("null"));
}

#[test]
fn capi_delete_named_stream_roundtrip() {
    let img = working_copy("del_rt");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let sn_c = CString::new("foo").unwrap();
    let data = b"x";
    unsafe {
        fs_ntfs_write_named_stream(
            img_c.as_ptr(),
            p_c.as_ptr(),
            sn_c.as_ptr(),
            data.as_ptr() as *const c_void,
            data.len() as u64,
        )
    };
    let rc = unsafe { fs_ntfs_delete_named_stream(img_c.as_ptr(), p_c.as_ptr(), sn_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_delete_named_stream_missing_fails() {
    let img = working_copy("del_missing");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let sn_c = CString::new("nope").unwrap();
    let rc = unsafe { fs_ntfs_delete_named_stream(img_c.as_ptr(), p_c.as_ptr(), sn_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}
