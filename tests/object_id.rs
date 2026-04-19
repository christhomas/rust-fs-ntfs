//! Tests for $OBJECT_ID reading (§3.5).

#![allow(unused_unsafe)]

use fs_ntfs::facade::Filesystem;
use fs_ntfs::{fs_ntfs_last_error, fs_ntfs_read_object_id};
use std::ffi::{CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

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
fn object_id_absent_returns_none() {
    // ntfs-basic fixture has no $OBJECT_ID on /hello.txt.
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    let oid = fs.object_id("/hello.txt").unwrap();
    assert!(oid.is_none());
}

#[test]
fn object_id_missing_file_errors() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    assert!(fs.object_id("/no_such.txt").is_err());
}

#[test]
fn capi_read_object_id_absent_returns_zero() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let mut buf = [0u8; 16];
    let rc = unsafe { fs_ntfs_read_object_id(img_c.as_ptr(), p_c.as_ptr(), buf.as_mut_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    assert_eq!(buf, [0u8; 16]);
}

#[test]
fn capi_read_object_id_null_outbuf_rejected() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe { fs_ntfs_read_object_id(img_c.as_ptr(), p_c.as_ptr(), std::ptr::null_mut()) };
    assert_eq!(rc, -1);
    assert!(last_error().contains("out_buf"));
}
