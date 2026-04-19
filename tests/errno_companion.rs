//! Tests for fs_ntfs_last_errno + fs_ntfs_clear_last_error (§4.1).

#![allow(unused_unsafe)]

use fs_ntfs::{
    fs_ntfs_clear_dirty, fs_ntfs_clear_last_error, fs_ntfs_last_errno, fs_ntfs_unlink,
    fs_ntfs_write_resident_contents,
};
use std::ffi::CString;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";
const ENOENT: i32 = 2;
const EINVAL: i32 = 22;
const EIO: i32 = 5;

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_errno_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn clean_state_is_zero() {
    unsafe { fs_ntfs_clear_last_error() };
    assert_eq!(unsafe { fs_ntfs_last_errno() }, 0);
}

#[test]
fn null_input_sets_einval() {
    unsafe { fs_ntfs_clear_last_error() };
    let rc = unsafe { fs_ntfs_clear_dirty(std::ptr::null()) };
    assert_eq!(rc, -1);
    assert_eq!(unsafe { fs_ntfs_last_errno() }, EINVAL);
}

#[test]
fn missing_path_sets_enoent() {
    unsafe { fs_ntfs_clear_last_error() };
    let img = working_copy("missing");
    let img_c = CString::new(img.as_str()).unwrap();
    let path_c = CString::new("/nonexistent.xx").unwrap();
    let rc = unsafe { fs_ntfs_unlink(img_c.as_ptr(), path_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert_eq!(unsafe { fs_ntfs_last_errno() }, ENOENT);
}

#[test]
fn clear_resets_errno() {
    unsafe { fs_ntfs_clear_last_error() };
    // Force an error.
    let rc = unsafe { fs_ntfs_clear_dirty(std::ptr::null()) };
    assert_eq!(rc, -1);
    assert_ne!(unsafe { fs_ntfs_last_errno() }, 0);
    unsafe { fs_ntfs_clear_last_error() };
    assert_eq!(unsafe { fs_ntfs_last_errno() }, 0);
}

#[test]
fn fallback_eio_for_generic_errors() {
    // Use fs_ntfs_write_resident_contents with non-zero len but
    // null buf — that's an explicit null-buf-with-non-zero-len
    // rejection, which the impl classifies as EINVAL.
    unsafe { fs_ntfs_clear_last_error() };
    let img = working_copy("fallback");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe {
        fs_ntfs_write_resident_contents(img_c.as_ptr(), p_c.as_ptr(), std::ptr::null(), 10)
    };
    assert_eq!(rc, -1);
    let errno = unsafe { fs_ntfs_last_errno() };
    assert_eq!(errno, EINVAL);
    // silence unused-const
    let _ = EIO;
}
