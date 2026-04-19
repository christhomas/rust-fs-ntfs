//! C-ABI tests for reparse + symlink write functions.

#![allow(unused_unsafe)]

use fs_ntfs::{
    fs_ntfs_create_symlink, fs_ntfs_last_error, fs_ntfs_remove_reparse_point,
    fs_ntfs_write_reparse_point,
};
use std::ffi::{c_void, CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_reparse_{tag}.img");
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

/// Build a minimal junction (MountPointReparseBuffer) payload.
fn mountpoint_buf(target: &str) -> Vec<u8> {
    let target_utf16: Vec<u16> = target.encode_utf16().collect();
    let sub_len_bytes = (target_utf16.len() * 2) as u16;
    let print_len_bytes = sub_len_bytes;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    out.extend_from_slice(&sub_len_bytes.to_le_bytes());
    out.extend_from_slice(&(sub_len_bytes + 2).to_le_bytes()); // PrintNameOffset
    out.extend_from_slice(&print_len_bytes.to_le_bytes());
    for c in &target_utf16 {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out.extend_from_slice(&[0u8, 0]); // sub null
    for c in &target_utf16 {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out.extend_from_slice(&[0u8, 0]); // print null
    out
}

#[test]
fn capi_write_reparse_point_happy() {
    let img = working_copy("mp_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let buf = mountpoint_buf("\\??\\C:\\target");
    let rc = unsafe {
        fs_ntfs_write_reparse_point(
            img_c.as_ptr(),
            p_c.as_ptr(),
            IO_REPARSE_TAG_MOUNT_POINT,
            buf.as_ptr() as *const c_void,
            buf.len() as u64,
        )
    };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_write_reparse_point_rejects_null_buf_with_len() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe {
        fs_ntfs_write_reparse_point(
            img_c.as_ptr(),
            p_c.as_ptr(),
            IO_REPARSE_TAG_SYMLINK,
            std::ptr::null(),
            10,
        )
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("null"));
}

#[test]
fn capi_remove_reparse_point_roundtrip() {
    let img = working_copy("mp_remove");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let buf = mountpoint_buf("\\??\\C:\\x");
    let rc = unsafe {
        fs_ntfs_write_reparse_point(
            img_c.as_ptr(),
            p_c.as_ptr(),
            IO_REPARSE_TAG_MOUNT_POINT,
            buf.as_ptr() as *const c_void,
            buf.len() as u64,
        )
    };
    assert_eq!(rc, 0);
    let rc = unsafe { fs_ntfs_remove_reparse_point(img_c.as_ptr(), p_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
}

#[test]
fn capi_remove_reparse_point_missing_fails() {
    let img = working_copy("mp_remove_missing");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe { fs_ntfs_remove_reparse_point(img_c.as_ptr(), p_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}

#[test]
fn capi_create_symlink_happy() {
    let img = working_copy("symlink_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("link_to_hello.txt").unwrap();
    let tgt_c = CString::new("/hello.txt").unwrap();
    let rn = unsafe {
        fs_ntfs_create_symlink(
            img_c.as_ptr(),
            parent_c.as_ptr(),
            name_c.as_ptr(),
            tgt_c.as_ptr(),
            0,
        )
    };
    assert!(rn >= 16, "err={}", last_error());
}

#[test]
fn capi_create_symlink_rejects_null_target() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("bad").unwrap();
    let rn = unsafe {
        fs_ntfs_create_symlink(
            img_c.as_ptr(),
            parent_c.as_ptr(),
            name_c.as_ptr(),
            std::ptr::null(),
            0,
        )
    };
    assert_eq!(rn, -1);
    assert!(last_error().contains("target"));
}
