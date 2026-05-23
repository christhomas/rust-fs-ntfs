//! Tests for $OBJECT_ID reading + writing (§3.5).

#![allow(unused_unsafe)]

use fs_ntfs::facade::Filesystem;
use fs_ntfs::write::{read_object_id, remove_object_id, write_object_id};
use fs_ntfs::{
    fs_ntfs_last_error, fs_ntfs_read_object_id, fs_ntfs_remove_object_id,
    fs_ntfs_write_object_id,
};
use std::ffi::{CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_objid_{tag}.img");
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

#[test]
fn write_object_id_roundtrips() {
    let img = working_copy("write_roundtrip");
    let guid: [u8; 16] = [
        0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe,
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
    ];
    write_object_id(std::path::Path::new(&img), "/hello.txt", &guid).unwrap();
    let got = read_object_id(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .expect("just written");
    assert_eq!(got, guid);
}

#[test]
fn write_object_id_replaces_existing() {
    let img = working_copy("write_replace");
    let g1: [u8; 16] = [1; 16];
    let g2: [u8; 16] = [2; 16];
    write_object_id(std::path::Path::new(&img), "/hello.txt", &g1).unwrap();
    write_object_id(std::path::Path::new(&img), "/hello.txt", &g2).unwrap();
    let got = read_object_id(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .expect("present");
    assert_eq!(got, g2);
}

#[test]
fn remove_object_id_idempotent() {
    let img = working_copy("remove_idem");
    // no $OBJECT_ID yet — should still succeed.
    remove_object_id(std::path::Path::new(&img), "/hello.txt").unwrap();
    // add then remove.
    let guid: [u8; 16] = [0xAB; 16];
    write_object_id(std::path::Path::new(&img), "/hello.txt", &guid).unwrap();
    let removed = remove_object_id(std::path::Path::new(&img), "/hello.txt").unwrap();
    assert!(removed, "expected something to remove");
    let got = read_object_id(std::path::Path::new(&img), "/hello.txt").unwrap();
    assert!(got.is_none());
}

#[test]
fn capi_write_object_id_roundtrips() {
    let img = working_copy("capi_write");
    let img_c = CString::new(img.clone()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let guid: [u8; 16] = [0x11; 16];
    let rc = unsafe { fs_ntfs_write_object_id(img_c.as_ptr(), p_c.as_ptr(), guid.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    let mut buf = [0u8; 16];
    let rc = unsafe { fs_ntfs_read_object_id(img_c.as_ptr(), p_c.as_ptr(), buf.as_mut_ptr()) };
    assert_eq!(rc, 1, "err={}", last_error());
    assert_eq!(buf, guid);
}

#[test]
fn capi_remove_object_id_idempotent() {
    let img = working_copy("capi_remove");
    let img_c = CString::new(img.clone()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    // remove when absent — exit 0.
    let rc = unsafe { fs_ntfs_remove_object_id(img_c.as_ptr(), p_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    // write + remove + read returns 0 (absent).
    let guid: [u8; 16] = [0x22; 16];
    unsafe { fs_ntfs_write_object_id(img_c.as_ptr(), p_c.as_ptr(), guid.as_ptr()) };
    unsafe { fs_ntfs_remove_object_id(img_c.as_ptr(), p_c.as_ptr()) };
    let mut buf = [0u8; 16];
    let rc = unsafe { fs_ntfs_read_object_id(img_c.as_ptr(), p_c.as_ptr(), buf.as_mut_ptr()) };
    assert_eq!(rc, 0);
}
