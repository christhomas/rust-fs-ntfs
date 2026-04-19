//! Smoke tests for the callback-based fsck C ABI added in v0.1.1:
//! `fs_ntfs_is_dirty_with_callbacks` + `fs_ntfs_fsck_with_callbacks`.
//!
//! These drive the FFI layer via Rust paths (same convention as
//! `capi_fsck.rs`). The callbacks are closures wrapped in a raw fn
//! pointer + context, which is exactly how a Swift/FSKit or Go consumer
//! would plumb them.

#![allow(unused_unsafe)]

mod common;

use std::ffi::{c_char, c_int, c_void, CStr};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use ntfs::structured_values::NtfsVolumeFlags;
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType};

use fs_ntfs::{
    fs_ntfs_fsck_with_callbacks, fs_ntfs_is_dirty_with_callbacks, fs_ntfs_last_error,
    FsNtfsBlockdevCfg,
};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

// --------------------------------------------------------------------------
// Fixture helpers (same shape as capi_fsck.rs; copy-paste is cheaper than
// sharing because tests/common/mod.rs is mount-centric).
// --------------------------------------------------------------------------

fn dirty_copy(tag: &str, dirty_flag: bool, corrupt_log: bool) -> String {
    let dst = format!("test-disks/_capi_fsck_cb_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy fixture");

    if dirty_flag {
        let off = upstream_volume_flags_offset(&dst);
        patch_u16_le(&dst, off, |f| f | NtfsVolumeFlags::IS_DIRTY.bits());
    }
    if corrupt_log {
        let (off, _) = upstream_logfile_data_start(&dst);
        let junk = b"TXN leftover mid-transaction bytes XXXXXXX".repeat(32);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&dst)
            .expect("open rw");
        f.seek(SeekFrom::Start(off)).expect("seek");
        f.write_all(&junk).expect("write");
        f.sync_all().expect("fsync");
    }
    dst
}

fn upstream_volume_flags_offset(path: &str) -> u64 {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse");
    let vol = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::Volume as u64)
        .expect("open $Volume");
    let mut attrs = vol.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::VolumeInformation) {
            continue;
        }
        let attr_pos = a.position().value().expect("pos").get();
        drop(reader);
        let value_offset = read_u16_le_at(path, attr_pos + 0x14);
        return attr_pos + value_offset as u64 + 10;
    }
    panic!("no $VOLUME_INFORMATION");
}

fn upstream_logfile_data_start(path: &str) -> (u64, u64) {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse");
    let log = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::LogFile as u64)
        .expect("open $LogFile");
    let mut attrs = log.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let v = a.value(&mut reader).expect("value");
        let pos = v.data_position().value().expect("pos").get();
        return (pos, a.value_length());
    }
    panic!("no unnamed $DATA on $LogFile");
}

fn read_u16_le_at(path: &str, offset: u64) -> u16 {
    let mut f = std::fs::File::open(path).expect("open");
    f.seek(SeekFrom::Start(offset)).expect("seek");
    let mut buf = [0u8; 2];
    f.read_exact(&mut buf).expect("read");
    u16::from_le_bytes(buf)
}

fn patch_u16_le(path: &str, offset: u64, mutate: impl FnOnce(u16) -> u16) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open");
    f.seek(SeekFrom::Start(offset)).expect("seek read");
    let mut b = [0u8; 2];
    f.read_exact(&mut b).expect("read");
    let new = mutate(u16::from_le_bytes(b));
    f.seek(SeekFrom::Start(offset)).expect("seek write");
    f.write_all(&new.to_le_bytes()).expect("write");
    f.sync_all().expect("fsync");
}

fn read_volume_flags(path: &str) -> NtfsVolumeFlags {
    let f = std::fs::File::open(path).expect("open");
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).expect("parse");
    ntfs.volume_info(&mut reader).expect("vi").flags()
}

fn read_logfile_first_page(path: &str) -> Vec<u8> {
    let (pos, _) = upstream_logfile_data_start(path);
    let mut f = std::fs::File::open(path).expect("open");
    f.seek(SeekFrom::Start(pos)).expect("seek");
    let mut buf = vec![0u8; 4096];
    f.read_exact(&mut buf).expect("read");
    buf
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

// --------------------------------------------------------------------------
// Callback context: a file handle plus its length, shared across
// read/write. Wrapped in a Mutex because Rust's borrow checker doesn't
// trust the FFI contract (serial access per mount).
// --------------------------------------------------------------------------

struct FileCtx {
    file: Mutex<std::fs::File>,
    // Total size of the underlying file. Not read in tests (callers
    // read the size_bytes field on the cfg struct) but kept so the
    // ctx matches the shape a real consumer would build.
    #[allow(dead_code)]
    size: u64,
}

unsafe extern "C" fn read_cb(
    ctx: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> c_int {
    let ctx = &*(ctx as *const FileCtx);
    let mut f = ctx.file.lock().expect("lock");
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return 1;
    }
    let slice = std::slice::from_raw_parts_mut(buf as *mut u8, length as usize);
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
    let ctx = &*(ctx as *const FileCtx);
    let mut f = ctx.file.lock().expect("lock");
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return 1;
    }
    let slice = std::slice::from_raw_parts(buf as *const u8, length as usize);
    if f.write_all(slice).is_err() {
        return 2;
    }
    0
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
            size,
        },
        size,
    )
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[test]
fn is_dirty_with_callbacks_returns_0_on_clean() {
    let img = dirty_copy("cb_clean", false, false);
    let (ctx, size) = make_ctx(&img);
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const FileCtx as *mut c_void,
        size_bytes: size,
        write: None, // read-only suffices
    };
    let rc = unsafe { fs_ntfs_is_dirty_with_callbacks(&cfg) };
    assert_eq!(rc, 0, "expected clean; last_error={}", last_error());
}

#[test]
fn is_dirty_with_callbacks_returns_1_on_dirty() {
    let img = dirty_copy("cb_dirty", true, false);
    let (ctx, size) = make_ctx(&img);
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const FileCtx as *mut c_void,
        size_bytes: size,
        write: None,
    };
    let rc = unsafe { fs_ntfs_is_dirty_with_callbacks(&cfg) };
    assert_eq!(rc, 1, "expected dirty; last_error={}", last_error());
}

#[test]
fn is_dirty_with_callbacks_neg1_on_null_cfg() {
    let rc = unsafe { fs_ntfs_is_dirty_with_callbacks(std::ptr::null()) };
    assert_eq!(rc, -1);
}

#[test]
fn fsck_with_callbacks_clears_dirty_and_resets_log() {
    let img = dirty_copy("cb_full", true, true);
    let (ctx, size) = make_ctx(&img);
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const FileCtx as *mut c_void,
        size_bytes: size,
        write: Some(write_cb),
    };

    let mut bytes: u64 = 0;
    let mut cleared: u8 = 0;
    let rc = unsafe {
        fs_ntfs_fsck_with_callbacks(&cfg, None, std::ptr::null_mut(), &mut bytes, &mut cleared)
    };
    assert_eq!(rc, 0, "expected success; last_error={}", last_error());
    assert!(bytes > 0);
    assert_eq!(cleared, 1);

    // The callbacks wrote through to the file — re-read via path to
    // confirm the on-disk result.
    drop(ctx); // release the file handle so the verification reads see the final state
    assert!(!read_volume_flags(&img).contains(NtfsVolumeFlags::IS_DIRTY));
    assert!(read_logfile_first_page(&img).iter().all(|&b| b == 0xFF));
}

#[test]
fn fsck_with_callbacks_errors_without_write_cb() {
    let img = dirty_copy("cb_no_write", true, true);
    let (ctx, size) = make_ctx(&img);
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const FileCtx as *mut c_void,
        size_bytes: size,
        write: None, // no write callback → fsck must refuse
    };
    let rc = unsafe {
        fs_ntfs_fsck_with_callbacks(
            &cfg,
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, -1);
    let err = last_error();
    assert!(
        err.contains("NULL") || err.contains("write") || err.contains("RW"),
        "expected write-nullity error; got {err:?}"
    );
}

// --- progress callback -----------------------------------------------------

struct ProgressLog(Vec<(String, u64, u64)>);

unsafe extern "C" fn progress_cb(
    ctx: *mut c_void,
    phase: *const c_char,
    done: u64,
    total: u64,
) -> c_int {
    let log = &mut *(ctx as *mut ProgressLog);
    let phase = if phase.is_null() {
        "".to_string()
    } else {
        CStr::from_ptr(phase).to_string_lossy().into_owned()
    };
    log.0.push((phase, done, total));
    0
}

#[test]
fn fsck_with_callbacks_emits_progress() {
    let img = dirty_copy("cb_progress", true, true);
    let (ctx, size) = make_ctx(&img);
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const FileCtx as *mut c_void,
        size_bytes: size,
        write: Some(write_cb),
    };

    let mut log = ProgressLog(Vec::new());
    let rc = unsafe {
        fs_ntfs_fsck_with_callbacks(
            &cfg,
            Some(progress_cb),
            &mut log as *mut ProgressLog as *mut c_void,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "last_error={}", last_error());

    // Both phases must appear.
    assert!(
        log.0.iter().any(|(p, _, _)| p == "reset_logfile"),
        "expected reset_logfile phase; got {:?}",
        log.0
    );
    assert!(
        log.0.iter().any(|(p, _, _)| p == "clear_dirty"),
        "expected clear_dirty phase; got {:?}",
        log.0
    );

    // The last reset_logfile tick should have done == total (>0).
    let last_reset = log
        .0
        .iter()
        .rev()
        .find(|(p, _, _)| p == "reset_logfile")
        .expect("some reset_logfile tick");
    assert!(last_reset.1 > 0 && last_reset.1 == last_reset.2);
}
