//! Direct smoke test for the handle-based RW C ABIs added alongside the
//! v0.1.x callback-mount path:
//!   * `fs_ntfs_create_file_h`
//!   * `fs_ntfs_write_file_contents_h`
//!   * `fs_ntfs_unlink_h`
//!
//! Mounts `test-disks/ntfs-basic.img` over a `Mutex<File>` read/write
//! callback pair (same shape as `capi_fsck_callbacks.rs`), exercises the
//! three `_h` siblings on a fresh `/tmp_test_file`, and verifies each
//! call sets `fs_ntfs_last_errno() == 0` on the way out.
//!
//! The fixture is copied to a per-test path first so we don't mutate the
//! shared image.

#![allow(unused_unsafe)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use fs_ntfs::{
    fs_ntfs_clear_last_error, fs_ntfs_create_file_h, fs_ntfs_last_errno,
    fs_ntfs_mount_with_callbacks, fs_ntfs_umount, fs_ntfs_unlink_h, fs_ntfs_write_file_contents_h,
    FsNtfsBlockdevCfg,
};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

struct FileCtx {
    file: Mutex<std::fs::File>,
}

unsafe extern "C" fn read_cb(
    ctx: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> c_int {
    let ctx = unsafe { &*(ctx as *const FileCtx) };
    let mut f = ctx.file.lock().expect("read_cb lock");
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return 1;
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, length as usize) };
    if f.read_exact(slice).is_err() {
        return 2;
    }
    0
}

unsafe extern "C" fn write_cb(
    ctx: *mut c_void,
    buf: *const c_void,
    offset: u64,
    length: u64,
) -> c_int {
    let ctx = unsafe { &*(ctx as *const FileCtx) };
    let mut f = ctx.file.lock().expect("write_cb lock");
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(buf as *const u8, length as usize) };
    if f.write_all(slice).is_err() {
        return 2;
    }
    0
}

fn copy_fixture(tag: &str) -> String {
    let dst = format!("test-disks/_capi_handle_rw_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy fixture");
    dst
}

fn make_ctx(path: &str) -> (FileCtx, u64) {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open rw");
    let size = f.metadata().expect("stat").len();
    (
        FileCtx {
            file: Mutex::new(f),
        },
        size,
    )
}

#[test]
fn handle_rw_create_write_unlink_round_trip() {
    let img = copy_fixture("crwu");
    let (ctx, size) = make_ctx(&img);
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const FileCtx as *mut c_void,
        size_bytes: size,
        write: Some(write_cb),
    };

    fs_ntfs_clear_last_error();
    let fs = unsafe { fs_ntfs_mount_with_callbacks(&cfg) };
    assert!(
        !fs.is_null(),
        "rw callback mount failed (errno={})",
        fs_ntfs_last_errno()
    );
    assert_eq!(
        fs_ntfs_last_errno(),
        0,
        "errno after successful mount must be 0"
    );

    let parent = CString::new("/").unwrap();
    let base = CString::new("tmp_test_file").unwrap();
    let full = CString::new("/tmp_test_file").unwrap();

    // create_file_h
    fs_ntfs_clear_last_error();
    let rn = unsafe {
        fs_ntfs_create_file_h(
            fs,
            parent.as_ptr() as *const c_char,
            base.as_ptr() as *const c_char,
        )
    };
    assert!(
        rn > 0,
        "create_file_h returned {rn}; errno={}",
        fs_ntfs_last_errno()
    );
    assert_eq!(
        fs_ntfs_last_errno(),
        0,
        "errno after create_file_h must be 0"
    );

    // write_file_contents_h with exactly 16 bytes
    let payload: &[u8] = b"Hello, NTFS!\n\0\0\0";
    assert_eq!(payload.len(), 16);
    fs_ntfs_clear_last_error();
    let n = unsafe {
        fs_ntfs_write_file_contents_h(
            fs,
            full.as_ptr() as *const c_char,
            payload.as_ptr() as *const c_void,
            payload.len() as u64,
        )
    };
    assert_eq!(
        n,
        payload.len() as i64,
        "write_file_contents_h returned {n}; errno={}",
        fs_ntfs_last_errno()
    );
    assert_eq!(fs_ntfs_last_errno(), 0, "errno after write must be 0");

    // unlink_h
    fs_ntfs_clear_last_error();
    let rc = unsafe { fs_ntfs_unlink_h(fs, full.as_ptr() as *const c_char) };
    assert_eq!(
        rc,
        0,
        "unlink_h returned {rc}; errno={}",
        fs_ntfs_last_errno()
    );
    assert_eq!(fs_ntfs_last_errno(), 0, "errno after unlink must be 0");

    unsafe { fs_ntfs_umount(fs) };

    // Best-effort cleanup of the per-test image.
    let _ = std::fs::remove_file(&img);
}
