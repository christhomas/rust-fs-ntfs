//! Tests for `set_security_id` / `read_security_id` (§3.4 minimal retarget).

#![allow(unused_unsafe)]

use fs_ntfs::facade::Filesystem;
use fs_ntfs::write::{create_file, read_security_id, set_security_id};
use fs_ntfs::{fs_ntfs_last_error, fs_ntfs_read_security_id, fs_ntfs_set_security_id};
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

/// Build a working-copy of ntfs-basic.img and create a runtime file
/// `/runtime.bin` inside it so the test exercises the 72-byte
/// NTFS 3.x `$STANDARD_INFORMATION` shape (which is what our runtime
/// create-file path emits). The fixture's pre-existing `/hello.txt`
/// uses the 48-byte v1.x form so it doesn't have a `security_id`
/// field at all.
fn working_copy(tag: &str) -> (String, &'static str) {
    let dst = format!("test-disks/_secid_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    create_file(std::path::Path::new(&dst), "/", "runtime.bin").expect("runtime create_file");
    (dst, "/runtime.bin")
}

#[test]
fn read_default_security_id_on_runtime_file() {
    let (img, path) = working_copy("read_default");
    // Fresh runtime-created files have $STANDARD_INFORMATION in the
    // 72-byte v3.x form with security_id = 0 (default — points at the
    // built-in inherited DACL).
    let id = read_security_id(std::path::Path::new(&img), path)
        .unwrap()
        .expect("runtime file has the 72-byte SI form");
    assert_eq!(id, 0);
}

#[test]
fn legacy_v1x_file_returns_none() {
    // The fixture's pre-existing /hello.txt uses the 48-byte v1.x form
    // (built by the external qemu+Alpine fixture pipeline, not our
    // mkfs). The reader correctly returns None because the field is
    // structurally absent.
    let id = read_security_id(std::path::Path::new(BASIC_IMG), "/hello.txt").unwrap();
    assert!(id.is_none(), "expected None for 48-byte SI, got {id:?}");
}

#[test]
fn write_then_read_roundtrips() {
    let (img, path) = working_copy("roundtrip");
    set_security_id(std::path::Path::new(&img), path, 0x100).unwrap();
    let id = read_security_id(std::path::Path::new(&img), path)
        .unwrap()
        .expect("present");
    assert_eq!(id, 0x100);
}

#[test]
fn write_then_overwrite_keeps_latest() {
    let (img, path) = working_copy("overwrite");
    set_security_id(std::path::Path::new(&img), path, 0x100).unwrap();
    set_security_id(std::path::Path::new(&img), path, 0x200).unwrap();
    let id = read_security_id(std::path::Path::new(&img), path)
        .unwrap()
        .unwrap();
    assert_eq!(id, 0x200);
}

#[test]
fn write_on_v1x_file_errors() {
    let (img, _runtime_path) = working_copy("v1x_reject");
    // /hello.txt is 48-byte v1.x — writing should fail loudly rather
    // than silently growing the attribute or scribbling past its end.
    let err = set_security_id(std::path::Path::new(&img), "/hello.txt", 1).unwrap_err();
    assert!(
        err.contains("too small"),
        "expected 'too small' in error, got: {err}"
    );
}

#[test]
fn missing_file_errors_on_read() {
    assert!(read_security_id(std::path::Path::new(BASIC_IMG), "/no_such.txt").is_err());
}

#[test]
fn missing_file_errors_on_write() {
    let (img, _) = working_copy("missing_write");
    assert!(set_security_id(std::path::Path::new(&img), "/no_such.txt", 1).is_err());
}

#[test]
fn upstream_mounts_after_security_id_write() {
    let (img, path) = working_copy("upstream_mount");
    set_security_id(std::path::Path::new(&img), path, 0x100).unwrap();
    // Re-mount via the upstream facade — proves we didn't corrupt the
    // record's bytes_used / fixup / etc.
    let _fs = Filesystem::mount(&img).expect("upstream re-mount");
}

#[test]
fn capi_read_security_id_returns_value() {
    let (img, path) = working_copy("capi_read");
    let img_c = CString::new(img).unwrap();
    let p_c = CString::new(path).unwrap();
    let mut out: u32 = 0xDEADBEEF;
    let rc = unsafe { fs_ntfs_read_security_id(img_c.as_ptr(), p_c.as_ptr(), &mut out) };
    assert_eq!(rc, 1, "err={}", last_error());
    assert_eq!(out, 0);
}

#[test]
fn capi_read_returns_zero_on_v1x_file() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let mut out: u32 = 0xDEADBEEF;
    let rc = unsafe { fs_ntfs_read_security_id(img_c.as_ptr(), p_c.as_ptr(), &mut out) };
    assert_eq!(rc, 0, "err={}", last_error());
    assert_eq!(out, 0);
}

#[test]
fn capi_set_security_id_roundtrips() {
    let (img, path) = working_copy("capi_roundtrip");
    let img_c = CString::new(img).unwrap();
    let p_c = CString::new(path).unwrap();
    let rc = unsafe { fs_ntfs_set_security_id(img_c.as_ptr(), p_c.as_ptr(), 0x100) };
    assert_eq!(rc, 0, "err={}", last_error());
    let mut out: u32 = 0;
    let rc = unsafe { fs_ntfs_read_security_id(img_c.as_ptr(), p_c.as_ptr(), &mut out) };
    assert_eq!(rc, 1, "err={}", last_error());
    assert_eq!(out, 0x100);
}

#[test]
fn capi_null_out_buf_rejected() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc =
        unsafe { fs_ntfs_read_security_id(img_c.as_ptr(), p_c.as_ptr(), std::ptr::null_mut()) };
    assert_eq!(rc, -1);
    assert!(last_error().contains("out"));
}
