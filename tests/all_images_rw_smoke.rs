//! Per-fixture RW smoke test for NTFS: for every `test-disks/ntfs-*.img`,
//! copy it to a scratch path, mount it RW via `fs_ntfs_mount_with_callbacks`
//! (with `cfg.write` set), exercise the new handle-based mutation API end to
//! end (`fs_ntfs_create_file_h` -> `fs_ntfs_write_file_contents_h` ->
//! `fs_ntfs_unlink_h`), and verify each step.
//!
//! One #[test] fn per image so cargo's PASS/FAIL output is per-image.

#![allow(unused_unsafe)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use fs_ntfs::{
    fs_ntfs_clear_last_error, fs_ntfs_create_file_h, fs_ntfs_dir_close, fs_ntfs_dir_next,
    fs_ntfs_dir_open, fs_ntfs_get_volume_info, fs_ntfs_last_errno, fs_ntfs_mount_with_callbacks,
    fs_ntfs_read_file, fs_ntfs_umount, fs_ntfs_unlink_h, fs_ntfs_write_file_contents_h,
    FsNtfsBlockdevCfg, FsNtfsVolumeInfo,
};

/// Mirror of `fs_ntfs::FsNtfsDirent` for tests. The crate-local type has
/// private fields, but it's `#[repr(C)]` and the layout is part of the
/// crate's stable C ABI (header: `fs_ntfs_dirent_t`), so this projection
/// is sound.
#[repr(C)]
struct DirentMirror {
    file_record_number: u64,
    file_type: u8,
    name_len: u16,
    name: [u8; 256],
}

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
    let mut f = ctx.file.lock().unwrap();
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
    let mut f = ctx.file.lock().unwrap();
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(buf as *const u8, length as usize) };
    if f.write_all(slice).is_err() {
        return 2;
    }
    0
}

fn run_round_trip(image_basename: &str) {
    let src = format!(
        "{}/test-disks/{}.img",
        env!("CARGO_MANIFEST_DIR"),
        image_basename
    );
    if !Path::new(&src).exists() {
        eprintln!("SKIP {image_basename}: fixture missing at {src}");
        return;
    }
    let scratch = format!(
        "{}/test-disks/_smoke_{}.img",
        env!("CARGO_MANIFEST_DIR"),
        image_basename
    );
    std::fs::copy(&src, &scratch).expect("copy fixture to scratch");

    let result = std::panic::catch_unwind(|| {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&scratch)
            .expect("open scratch RW");
        let size = f.metadata().expect("stat").len();
        let ctx = Box::new(FileCtx {
            file: Mutex::new(f),
        });

        let cfg = FsNtfsBlockdevCfg {
            read: read_cb,
            context: ctx.as_ref() as *const FileCtx as *mut c_void,
            size_bytes: size,
            write: Some(write_cb),
        };

        fs_ntfs_clear_last_error();
        let fs = unsafe { fs_ntfs_mount_with_callbacks(&cfg) };
        assert!(
            !fs.is_null(),
            "[{image_basename}] rw callback mount failed (errno={})",
            fs_ntfs_last_errno()
        );

        // Volume info diagnostics — surface cluster size + total clusters.
        let mut vinfo: FsNtfsVolumeInfo = unsafe { std::mem::zeroed() };
        unsafe { fs_ntfs_get_volume_info(fs, &mut vinfo) };
        // FsNtfsVolumeInfo fields are private; project via mirror struct.
        #[repr(C)]
        struct VolInfoMirror {
            volume_name: [u8; 128],
            cluster_size: u32,
            total_clusters: u64,
            ntfs_version_major: u16,
            ntfs_version_minor: u16,
            serial_number: u64,
            total_size: u64,
        }
        let vmirror = unsafe { &*(&vinfo as *const _ as *const VolInfoMirror) };
        let vname = std::ffi::CStr::from_bytes_until_nul(&vmirror.volume_name)
            .map(|c| c.to_string_lossy().to_string())
            .unwrap_or_default();
        eprintln!(
            "[{image_basename}] volume: name={:?} cluster_size={} total_clusters={} ntfs={}.{} size={}",
            vname,
            vmirror.cluster_size,
            vmirror.total_clusters,
            vmirror.ntfs_version_major,
            vmirror.ntfs_version_minor,
            vmirror.total_size
        );

        // List root + read first regular file.
        let root = CString::new("/").unwrap();
        let it = unsafe { fs_ntfs_dir_open(fs, root.as_ptr() as *const c_char) };
        assert!(!it.is_null(), "[{image_basename}] dir_open / failed");

        let mut entries = 0;
        let mut first_existing_read_bytes = 0i64;
        let mut first_file_name: Option<String> = None;
        loop {
            let de = unsafe { fs_ntfs_dir_next(it) };
            if de.is_null() {
                break;
            }
            entries += 1;
            // Read via the mirror struct since the real fields are private.
            let mirror = unsafe { &*(de as *const DirentMirror) };
            let ft = mirror.file_type;
            let name_ptr = mirror.name.as_ptr() as *const c_char;
            let name = unsafe { std::ffi::CStr::from_ptr(name_ptr) }
                .to_string_lossy()
                .to_string();
            if ft == 1 && first_file_name.is_none() && !name.is_empty() {
                first_file_name = Some(name);
            }
        }
        unsafe { fs_ntfs_dir_close(it) };

        if let Some(name) = &first_file_name {
            let path = format!("/{name}");
            let cpath = CString::new(path.clone()).unwrap();
            let mut buf = vec![0u8; 64];
            let n = unsafe {
                fs_ntfs_read_file(
                    fs,
                    cpath.as_ptr() as *const c_char,
                    buf.as_mut_ptr() as *mut c_void,
                    0,
                    64,
                )
            };
            if n >= 0 {
                first_existing_read_bytes = n;
                eprintln!("[{image_basename}] read {n} bytes from existing file {path}");
            }
        }

        // create_file_h
        let parent = CString::new("/").unwrap();
        let base = CString::new("__rw_smoke_probe").unwrap();
        let full = CString::new("/__rw_smoke_probe").unwrap();

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
            "[{image_basename}] create_file_h returned {rn} (errno={})",
            fs_ntfs_last_errno()
        );

        // write_file_contents_h with 64 bytes
        let payload: Vec<u8> = (0..64u8).collect();
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
            "[{image_basename}] write_file_contents_h returned {n} (errno={})",
            fs_ntfs_last_errno()
        );

        // read it back
        let mut readback = vec![0u8; 64];
        let n = unsafe {
            fs_ntfs_read_file(
                fs,
                full.as_ptr() as *const c_char,
                readback.as_mut_ptr() as *mut c_void,
                0,
                64,
            )
        };
        assert_eq!(
            n,
            64,
            "[{image_basename}] read_file returned {n} on probe (errno={})",
            fs_ntfs_last_errno()
        );
        assert_eq!(
            readback, payload,
            "[{image_basename}] readback content mismatch"
        );

        // unlink_h
        fs_ntfs_clear_last_error();
        let rc = unsafe { fs_ntfs_unlink_h(fs, full.as_ptr() as *const c_char) };
        assert_eq!(
            rc,
            0,
            "[{image_basename}] unlink_h returned {rc} (errno={})",
            fs_ntfs_last_errno()
        );

        unsafe { fs_ntfs_umount(fs) };

        eprintln!(
            "[{image_basename}] PASS: list({} entries), read({} pre-existing bytes), create+write+read+verify+unlink",
            entries, first_existing_read_bytes
        );
    });

    let _ = std::fs::remove_file(&scratch);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn ntfs_basic() {
    run_round_trip("ntfs-basic")
}
#[test]
fn ntfs_ads() {
    run_round_trip("ntfs-ads")
}
#[test]
fn ntfs_deep() {
    run_round_trip("ntfs-deep")
}
#[test]
fn ntfs_large_file() {
    run_round_trip("ntfs-large-file")
}
#[test]
fn ntfs_manyfiles() {
    run_round_trip("ntfs-manyfiles")
}
#[test]
fn ntfs_sparse() {
    run_round_trip("ntfs-sparse")
}
#[test]
fn ntfs_unicode() {
    run_round_trip("ntfs-unicode")
}
