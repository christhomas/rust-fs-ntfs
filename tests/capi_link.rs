//! C-ABI tests for `fs_ntfs_link`.

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_last_error, fs_ntfs_link, fs_ntfs_mkdir};
use std::ffi::{CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_link_{tag}.img");
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
fn capi_link_happy() {
    let img = working_copy("happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let root_c = CString::new("/").unwrap();
    let d_c = CString::new("d").unwrap();
    unsafe { fs_ntfs_mkdir(img_c.as_ptr(), root_c.as_ptr(), d_c.as_ptr()) };

    let src_c = CString::new("/hello.txt").unwrap();
    let parent_c = CString::new("/d").unwrap();
    let name_c = CString::new("aliased.txt").unwrap();
    let rc = unsafe {
        fs_ntfs_link(
            img_c.as_ptr(),
            src_c.as_ptr(),
            parent_c.as_ptr(),
            name_c.as_ptr(),
        )
    };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_link_rejects_null_existing() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("x").unwrap();
    let rc = unsafe {
        fs_ntfs_link(
            img_c.as_ptr(),
            std::ptr::null(),
            parent_c.as_ptr(),
            name_c.as_ptr(),
        )
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("existing_path"));
}

#[test]
fn capi_link_rejects_null_new_basename() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let src_c = CString::new("/hello.txt").unwrap();
    let parent_c = CString::new("/").unwrap();
    let rc = unsafe {
        fs_ntfs_link(
            img_c.as_ptr(),
            src_c.as_ptr(),
            parent_c.as_ptr(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("new_basename"));
}

#[test]
fn capi_link_nonexistent_src_fails() {
    let img = working_copy("nosrc");
    let img_c = CString::new(img.as_str()).unwrap();
    let src_c = CString::new("/no_such.txt").unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("xx").unwrap();
    let rc = unsafe {
        fs_ntfs_link(
            img_c.as_ptr(),
            src_c.as_ptr(),
            parent_c.as_ptr(),
            name_c.as_ptr(),
        )
    };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}
