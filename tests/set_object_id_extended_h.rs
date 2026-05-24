//! Tests for `fs_ntfs_set_object_id_extended_h` — the handle-based
//! sibling of `fs_ntfs_write_object_id_extended`. Confirms the writer
//! goes through the open-handle path (no per-call image reopen) and
//! ends up with the same on-disk shape as the path-based API.

#![allow(unused_unsafe)]

use fs_ntfs::write::read_object_id_extended;
use fs_ntfs::{
    fs_ntfs_last_error, fs_ntfs_mount, fs_ntfs_set_object_id_extended_h, fs_ntfs_umount,
};
use std::ffi::{CStr, CString};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_soeh_{tag}.img");
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
fn writes_64_byte_extended_form_via_handle() {
    let img = working_copy("write");
    // The handle has the image open RW — we must drop it before the
    // path-based reader opens its own RO handle on the same file.
    let img_c = CString::new(img.clone()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let oid: [u8; 16] = [0x11; 16];
    let bv: [u8; 16] = [0x22; 16];
    let bo: [u8; 16] = [0x33; 16];
    let bd: [u8; 16] = [0x44; 16];
    unsafe {
        let h = fs_ntfs_mount(img_c.as_ptr());
        assert!(!h.is_null(), "mount: {}", last_error());
        let rc = fs_ntfs_set_object_id_extended_h(
            h,
            p_c.as_ptr(),
            oid.as_ptr(),
            bv.as_ptr(),
            bo.as_ptr(),
            bd.as_ptr(),
        );
        assert_eq!(rc, 0, "err={}", last_error());
        fs_ntfs_umount(h);
    }
    // Now read back via the path API and confirm all four GUIDs roundtripped.
    let ext = read_object_id_extended(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .expect("attribute should exist after write");
    assert_eq!(ext.object_id, oid);
    let (got_bv, got_bo, got_bd) = ext.birth_ids.expect("64-byte form expected");
    assert_eq!(got_bv, bv);
    assert_eq!(got_bo, bo);
    assert_eq!(got_bd, bd);
}

#[test]
fn replaces_existing_extended_form() {
    let img = working_copy("replace");
    let img_c = CString::new(img.clone()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    unsafe {
        let h = fs_ntfs_mount(img_c.as_ptr());
        // First write — initial values.
        let rc = fs_ntfs_set_object_id_extended_h(
            h,
            p_c.as_ptr(),
            [0xAA; 16].as_ptr(),
            [0xBB; 16].as_ptr(),
            [0xCC; 16].as_ptr(),
            [0xDD; 16].as_ptr(),
        );
        assert_eq!(rc, 0, "first: {}", last_error());
        // Second write — replace in place via the same handle.
        let rc = fs_ntfs_set_object_id_extended_h(
            h,
            p_c.as_ptr(),
            [0x11; 16].as_ptr(),
            [0x22; 16].as_ptr(),
            [0x33; 16].as_ptr(),
            [0x44; 16].as_ptr(),
        );
        assert_eq!(rc, 0, "second: {}", last_error());
        fs_ntfs_umount(h);
    }
    let ext = read_object_id_extended(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .unwrap();
    assert_eq!(ext.object_id, [0x11u8; 16]);
    let (bv, bo, bd) = ext.birth_ids.unwrap();
    assert_eq!(bv, [0x22u8; 16]);
    assert_eq!(bo, [0x33u8; 16]);
    assert_eq!(bd, [0x44u8; 16]);
}

#[test]
fn rejects_null_guid_pointer() {
    let img = working_copy("null_gid");
    let img_c = CString::new(img).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let zero: [u8; 16] = [0; 16];
    unsafe {
        let h = fs_ntfs_mount(img_c.as_ptr());
        let rc = fs_ntfs_set_object_id_extended_h(
            h,
            p_c.as_ptr(),
            std::ptr::null(),
            zero.as_ptr(),
            zero.as_ptr(),
            zero.as_ptr(),
        );
        assert_eq!(rc, -1);
        assert!(last_error().contains("null GUID pointer"));
        fs_ntfs_umount(h);
    }
}

#[test]
fn rejects_null_path() {
    let img = working_copy("null_path");
    let img_c = CString::new(img).unwrap();
    let zero: [u8; 16] = [0; 16];
    unsafe {
        let h = fs_ntfs_mount(img_c.as_ptr());
        let rc = fs_ntfs_set_object_id_extended_h(
            h,
            std::ptr::null(),
            zero.as_ptr(),
            zero.as_ptr(),
            zero.as_ptr(),
            zero.as_ptr(),
        );
        assert_eq!(rc, -1);
        fs_ntfs_umount(h);
    }
}
