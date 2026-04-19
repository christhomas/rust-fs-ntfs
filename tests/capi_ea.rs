//! C-ABI tests for `fs_ntfs_write_ea` and `fs_ntfs_remove_ea`.

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_last_error, fs_ntfs_remove_ea, fs_ntfs_write_ea};
use std::ffi::{c_void, CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_ea_{tag}.img");
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
fn capi_write_ea_happy_path() {
    let img = working_copy("write_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let name_c = CString::new("user.test").unwrap();
    let value = b"value bytes";
    let rc = unsafe {
        fs_ntfs_write_ea(
            img_c.as_ptr(),
            p_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const c_void,
            value.len() as u64,
            0,
        )
    };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_write_ea_zero_length_ok() {
    let img = working_copy("write_zero");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let name_c = CString::new("user.empty").unwrap();
    let rc = unsafe {
        fs_ntfs_write_ea(
            img_c.as_ptr(),
            p_c.as_ptr(),
            name_c.as_ptr(),
            std::ptr::null(),
            0,
            0,
        )
    };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_write_ea_rejects_null_value_with_len() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let name_c = CString::new("user.bad").unwrap();
    let rc = unsafe {
        fs_ntfs_write_ea(
            img_c.as_ptr(),
            p_c.as_ptr(),
            name_c.as_ptr(),
            std::ptr::null(),
            10,
            0,
        )
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("null"));
}

#[test]
fn capi_write_ea_rejects_null_ea_name() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe {
        fs_ntfs_write_ea(
            img_c.as_ptr(),
            p_c.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
        )
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("ea_name"));
}

#[test]
fn capi_remove_ea_roundtrip() {
    let img = working_copy("remove_rt");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let name_c = CString::new("user.roundtrip").unwrap();
    let value = b"vv";
    unsafe {
        fs_ntfs_write_ea(
            img_c.as_ptr(),
            p_c.as_ptr(),
            name_c.as_ptr(),
            value.as_ptr() as *const c_void,
            value.len() as u64,
            0,
        )
    };
    let rc = unsafe { fs_ntfs_remove_ea(img_c.as_ptr(), p_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_remove_ea_missing_fails() {
    let img = working_copy("remove_missing");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let name_c = CString::new("user.never_existed").unwrap();
    let rc = unsafe { fs_ntfs_remove_ea(img_c.as_ptr(), p_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}
