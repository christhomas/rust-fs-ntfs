//! Tests for $OBJECT_ID reading + writing (§3.5).

#![allow(unused_unsafe)]

use fs_ntfs::facade::Filesystem;
use fs_ntfs::write::{
    read_object_id, read_object_id_extended, remove_object_id, write_object_id,
    write_object_id_extended,
};
use fs_ntfs::{
    fs_ntfs_last_error, fs_ntfs_read_object_id, fs_ntfs_read_object_id_extended,
    fs_ntfs_remove_object_id, fs_ntfs_write_object_id, fs_ntfs_write_object_id_extended,
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

// -- Extended (64-byte) form: object_id + Birth GUIDs ---------------------

#[test]
fn write_extended_roundtrips_all_four_guids() {
    let img = working_copy("ext_roundtrip");
    let object_id: [u8; 16] = [0xAA; 16];
    let birth_vol: [u8; 16] = [0xBB; 16];
    let birth_obj: [u8; 16] = [0xCC; 16];
    let birth_dom: [u8; 16] = [0xDD; 16];
    write_object_id_extended(
        std::path::Path::new(&img),
        "/hello.txt",
        &object_id,
        &birth_vol,
        &birth_obj,
        &birth_dom,
    )
    .unwrap();
    let ext = read_object_id_extended(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .expect("present");
    assert_eq!(ext.object_id, object_id);
    let (bv, bo, bd) = ext.birth_ids.expect("64-byte form");
    assert_eq!(bv, birth_vol);
    assert_eq!(bo, birth_obj);
    assert_eq!(bd, birth_dom);
}

#[test]
fn short_form_reads_back_with_no_birth_ids() {
    let img = working_copy("short_no_birth");
    let object_id: [u8; 16] = [0x11; 16];
    write_object_id(std::path::Path::new(&img), "/hello.txt", &object_id).unwrap();
    let ext = read_object_id_extended(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .expect("present");
    assert_eq!(ext.object_id, object_id);
    assert!(ext.birth_ids.is_none(), "16-byte form must have no Birth IDs");
}

#[test]
fn write_extended_then_overwrite_with_short_shrinks_to_16() {
    let img = working_copy("ext_then_short");
    let g16: [u8; 16] = [0xEE; 16];
    write_object_id_extended(
        std::path::Path::new(&img),
        "/hello.txt",
        &g16,
        &[0xF0; 16],
        &[0xF1; 16],
        &[0xF2; 16],
    )
    .unwrap();
    // Overwrite with the short form. The attribute MUST shrink back
    // to 16 bytes — the Birth IDs from the prior write should be
    // gone, not left dangling.
    write_object_id(std::path::Path::new(&img), "/hello.txt", &g16).unwrap();
    let ext = read_object_id_extended(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .unwrap();
    assert_eq!(ext.object_id, g16);
    assert!(ext.birth_ids.is_none(), "Birth IDs must clear on short overwrite");
}

#[test]
fn short_read_still_works_after_extended_write() {
    // Existing fs_ntfs_read_object_id (16-byte read) must still
    // return the object_id even when the on-disk attribute is the
    // 64-byte extended form — it just reads the first 16 bytes.
    let img = working_copy("short_read_ext");
    let object_id: [u8; 16] = [0x22; 16];
    write_object_id_extended(
        std::path::Path::new(&img),
        "/hello.txt",
        &object_id,
        &[0x33; 16],
        &[0x44; 16],
        &[0x55; 16],
    )
    .unwrap();
    let got = read_object_id(std::path::Path::new(&img), "/hello.txt")
        .unwrap()
        .expect("present");
    assert_eq!(got, object_id);
}

#[test]
fn upstream_mounts_after_extended_write() {
    let img = working_copy("ext_upstream_mount");
    write_object_id_extended(
        std::path::Path::new(&img),
        "/hello.txt",
        &[0xAB; 16],
        &[0xCD; 16],
        &[0xEF; 16],
        &[0x01; 16],
    )
    .unwrap();
    let _fs = Filesystem::mount(&img).expect("upstream re-mount");
}

#[test]
fn capi_extended_64byte_roundtrip() {
    let img = working_copy("capi_ext_64");
    let img_c = CString::new(img.clone()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let oid: [u8; 16] = [0xA1; 16];
    let bv: [u8; 16] = [0xB1; 16];
    let bo: [u8; 16] = [0xC1; 16];
    let bd: [u8; 16] = [0xD1; 16];
    let rc = unsafe {
        fs_ntfs_write_object_id_extended(
            img_c.as_ptr(),
            p_c.as_ptr(),
            oid.as_ptr(),
            bv.as_ptr(),
            bo.as_ptr(),
            bd.as_ptr(),
        )
    };
    assert_eq!(rc, 0, "err={}", last_error());
    let mut out = [0u8; 64];
    let rc = unsafe {
        fs_ntfs_read_object_id_extended(img_c.as_ptr(), p_c.as_ptr(), out.as_mut_ptr(), out.len())
    };
    assert_eq!(rc, 64, "err={}", last_error());
    assert_eq!(&out[0..16], &oid);
    assert_eq!(&out[16..32], &bv);
    assert_eq!(&out[32..48], &bo);
    assert_eq!(&out[48..64], &bd);
}

#[test]
fn capi_extended_short_buf_truncates() {
    // out_buf_len = 16 with a 64-byte on-disk form returns just the
    // object_id (16 bytes). The Birth GUIDs are silently dropped.
    let img = working_copy("capi_short_buf");
    let img_c = CString::new(img.clone()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    unsafe {
        fs_ntfs_write_object_id_extended(
            img_c.as_ptr(),
            p_c.as_ptr(),
            [0x9A; 16].as_ptr(),
            [0x9B; 16].as_ptr(),
            [0x9C; 16].as_ptr(),
            [0x9D; 16].as_ptr(),
        );
    }
    let mut out = [0xFFu8; 16];
    let rc =
        unsafe { fs_ntfs_read_object_id_extended(img_c.as_ptr(), p_c.as_ptr(), out.as_mut_ptr(), 16) };
    assert_eq!(rc, 16);
    assert_eq!(out, [0x9A; 16]);
}

#[test]
fn capi_extended_absent_returns_zero() {
    let img = working_copy("capi_ext_absent");
    let img_c = CString::new(img.clone()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let mut out = [0u8; 64];
    let rc = unsafe {
        fs_ntfs_read_object_id_extended(img_c.as_ptr(), p_c.as_ptr(), out.as_mut_ptr(), 64)
    };
    assert_eq!(rc, 0);
}

#[test]
fn capi_extended_too_small_buf_rejected() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let mut out = [0u8; 8]; // < 16
    let rc = unsafe {
        fs_ntfs_read_object_id_extended(img_c.as_ptr(), p_c.as_ptr(), out.as_mut_ptr(), 8)
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("16"));
}
