//! C-ABI tests for `fs_ntfs_rename2_h` with `FS_NTFS_RENAME_REPLACE`.
//!
//! Covers the cases that in-place editors (write-temp-then-rename-over-
//! original) depend on:
//!   - file replaces file (atomic, frees the old MFT record)
//!   - replace=false on an existing dst still errors (EEXIST guard)
//!   - empty-dir replaces empty-dir
//!   - non-empty-dir target → ENOTEMPTY
//!   - file → dir / dir → file → EISDIR / ENOTDIR
//!   - unknown flag bits → EINVAL (forward-compat)
//!   - replace survives an unmount + remount
//!
//! Each behaviour is exercised with both a same-length and a
//! variable-length destination name, since those hit different code
//! paths in `write.rs` (`rename_same_length_io` vs the variable-length
//! rebuild).
//!
//! The image is generated in-process with `mkfs::format_filesystem`, so
//! the test is self-contained — no on-disk fixture or VM required. A
//! freshly-formatted volume's root directory is resident (no
//! `$INDEX_ALLOCATION` overflow), so both rename paths are usable there.

#![allow(unused_unsafe)]

use std::ffi::{c_int, c_void, CStr, CString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{
    fs_ntfs_clear_last_error, fs_ntfs_create_file_h, fs_ntfs_last_errno, fs_ntfs_last_error,
    fs_ntfs_mkdir_h, fs_ntfs_mount_with_callbacks, fs_ntfs_rename2_h, fs_ntfs_rename_h,
    fs_ntfs_umount, fs_ntfs_write_file_contents_h, FsNtfsBlockdevCfg, FsNtfsHandle,
    FS_NTFS_RENAME_REPLACE,
};

const VOL_SIZE: u64 = 32 * 1024 * 1024;

// POSIX errno values (matched by `infer_errno_from_message`).
const EEXIST: c_int = 17;
const ENOTDIR: c_int = 20;
const EISDIR: c_int = 21;
const EINVAL: c_int = 22;
const ENOTEMPTY: c_int = 66;

// --- image generation -----------------------------------------------------

fn fresh_volume(tag: &str) -> ImgGuard {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dst = format!("test-disks/_capi_rename_overwrite_{tag}_{n}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        4096,
        4096,
        Some("RNM"),
        Some(0x0B_AD_F0_0D),
    )
    .expect("mkfs");
    io.sync().expect("sync");
    drop(io);
    ImgGuard(dst)
}

// --- callback block device (read/write over a host File) ------------------

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

/// Mount `img` read/write over file-backed callbacks, run `f` with the
/// live handle, then unmount. The closure's value is returned so callers
/// can capture return codes / errno recorded during the session.
fn rw_session<R>(img: &str, f: impl FnOnce(*mut FsNtfsHandle) -> R) -> R {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(img)
        .expect("open rw");
    let size = file.metadata().expect("stat").len();
    let ctx = FileCtx {
        file: Mutex::new(file),
    };
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
        "mount failed (errno={})",
        fs_ntfs_last_errno()
    );
    let r = f(fs);
    unsafe { fs_ntfs_umount(fs) };
    r
}

// --- _h call wrappers ------------------------------------------------------

fn create_file(fs: *mut FsNtfsHandle, parent: &str, base: &str) {
    let p = CString::new(parent).unwrap();
    let b = CString::new(base).unwrap();
    let rn = unsafe { fs_ntfs_create_file_h(fs, p.as_ptr(), b.as_ptr()) };
    assert!(rn > 0, "create_file {base}: errno={}", fs_ntfs_last_errno());
}

fn write_content(fs: *mut FsNtfsHandle, path: &str, data: &[u8]) {
    let p = CString::new(path).unwrap();
    let n = unsafe {
        fs_ntfs_write_file_contents_h(
            fs,
            p.as_ptr(),
            data.as_ptr() as *const c_void,
            data.len() as u64,
        )
    };
    assert_eq!(
        n,
        data.len() as i64,
        "write {path}: errno={}",
        fs_ntfs_last_errno()
    );
}

fn create_file_with(fs: *mut FsNtfsHandle, parent: &str, base: &str, data: &[u8]) {
    create_file(fs, parent, base);
    let full = if parent == "/" {
        format!("/{base}")
    } else {
        format!("{}/{base}", parent.trim_end_matches('/'))
    };
    write_content(fs, &full, data);
}

fn mkdir(fs: *mut FsNtfsHandle, parent: &str, base: &str) {
    let p = CString::new(parent).unwrap();
    let b = CString::new(base).unwrap();
    let rn = unsafe { fs_ntfs_mkdir_h(fs, p.as_ptr(), b.as_ptr()) };
    assert!(rn > 0, "mkdir {base}: errno={}", fs_ntfs_last_errno());
}

fn rename2(fs: *mut FsNtfsHandle, old: &str, new_base: &str, flags: c_int) -> c_int {
    let o = CString::new(old).unwrap();
    let nb = CString::new(new_base).unwrap();
    unsafe { fs_ntfs_rename2_h(fs, o.as_ptr(), nb.as_ptr(), flags) }
}

fn last_err() -> String {
    unsafe {
        let p = fs_ntfs_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

// --- verification helpers (read-only, path-based) -------------------------

fn record_of(img: &str, path: &str) -> Option<u64> {
    let fs = Filesystem::mount(img).ok()?;
    fs.stat(path).ok().map(|a| a.file_record_number)
}

fn exists(img: &str, path: &str) -> bool {
    record_of(img, path).is_some()
}

fn read_all(img: &str, path: &str) -> Vec<u8> {
    let fs = Filesystem::mount(img).expect("mount ro");
    let size = fs.stat(path).expect("stat").size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read_file(path, 0, &mut buf).expect("read_file");
    buf.truncate(n);
    buf
}

fn record_in_use(img: &str, rec: u64) -> bool {
    let bm = fs_ntfs::mft_bitmap::locate(Path::new(img)).expect("locate mft bitmap");
    fs_ntfs::mft_bitmap::is_allocated(Path::new(img), &bm, rec).expect("is_allocated")
}

/// Panic-safe cleanup: removes the backing image file when the test
/// scope unwinds, so a failed `assert!` doesn't litter `test-disks/`.
/// Derefs to `str` so it drops in anywhere an `&str` image path is
/// expected.
struct ImgGuard(String);

impl Drop for ImgGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl std::ops::Deref for ImgGuard {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

// ===========================================================================
// file replaces file — frees the old record (same-length + variable-length)
// ===========================================================================

fn file_replaces_file_case(tag: &str, src: &str, dst: &str) {
    let img = fresh_volume(tag);

    // Setup: src (distinctive content) + victim dst (different content).
    rw_session(&img, |fs| {
        create_file_with(fs, "/", src, b"SRC-PAYLOAD-AAA");
        create_file_with(fs, "/", dst, b"VICTIM-BBBBBBBBBBB");
    });

    let src_rec = record_of(&img, &format!("/{src}")).expect("src record");
    let victim_rec = record_of(&img, &format!("/{dst}")).expect("victim record");
    assert_ne!(src_rec, victim_rec);
    assert!(record_in_use(&img, victim_rec));

    // Atomic replace.
    rw_session(&img, |fs| {
        let rc = rename2(fs, &format!("/{src}"), dst, FS_NTFS_RENAME_REPLACE);
        assert_eq!(rc, 0, "rename2 replace: {}", last_err());
    });

    // src gone; dst now resolves to the src record + src content.
    assert!(!exists(&img, &format!("/{src}")), "src should be gone");
    assert!(exists(&img, &format!("/{dst}")), "dst should remain");
    assert_eq!(
        record_of(&img, &format!("/{dst}")).unwrap(),
        src_rec,
        "dst must now point at the source's MFT record"
    );
    assert_eq!(read_all(&img, &format!("/{dst}")), b"SRC-PAYLOAD-AAA");
    // The victim's old MFT record must have been freed.
    assert!(
        !record_in_use(&img, victim_rec),
        "old destination record {victim_rec} must be freed"
    );
}

#[test]
fn file_replaces_file_same_length_frees_old_record() {
    // "alpha.txt" / "bravo.txt" — equal UTF-16 length → same-length path.
    file_replaces_file_case("ff_sl", "alpha.txt", "bravo.txt");
}

#[test]
fn file_replaces_file_variable_length_frees_old_record() {
    // Differing lengths → variable-length rebuild path.
    file_replaces_file_case("ff_vl", "s.dat", "destination_file.dat");
}

// ===========================================================================
// replace=false on an existing destination still errors (EEXIST guard)
// ===========================================================================

#[test]
fn replace_false_on_existing_dest_still_errors() {
    let img = fresh_volume("eexist");
    rw_session(&img, |fs| {
        create_file_with(fs, "/", "src_a.txt", b"new");
        create_file_with(fs, "/", "dst_b.txt", b"old");

        // rename2 without the flag rejects the existing destination.
        fs_ntfs_clear_last_error();
        let rc = rename2(fs, "/src_a.txt", "dst_b.txt", 0);
        assert_eq!(rc, -1, "rename2 flags=0 onto existing must fail");
        assert_eq!(fs_ntfs_last_errno(), EEXIST, "{}", last_err());

        // Plain rename_h keeps the same reject-existing semantics.
        let o = CString::new("/src_a.txt").unwrap();
        let nb = CString::new("dst_b.txt").unwrap();
        fs_ntfs_clear_last_error();
        let rc = unsafe { fs_ntfs_rename_h(fs, o.as_ptr(), nb.as_ptr()) };
        assert_eq!(rc, -1);
        assert_eq!(fs_ntfs_last_errno(), EEXIST, "{}", last_err());
    });

    // Both names survive untouched.
    assert!(exists(&img, "/src_a.txt"));
    assert!(exists(&img, "/dst_b.txt"));
    assert_eq!(read_all(&img, "/dst_b.txt"), b"old");
}

// ===========================================================================
// empty-dir replaces empty-dir (same-length + variable-length)
// ===========================================================================

fn empty_dir_replaces_empty_dir_case(tag: &str, src: &str, dst: &str) {
    let img = fresh_volume(tag);
    rw_session(&img, |fs| {
        mkdir(fs, "/", src);
        mkdir(fs, "/", dst);
    });
    let src_rec = record_of(&img, &format!("/{src}")).unwrap();
    let dst_rec = record_of(&img, &format!("/{dst}")).unwrap();

    rw_session(&img, |fs| {
        let rc = rename2(fs, &format!("/{src}"), dst, FS_NTFS_RENAME_REPLACE);
        assert_eq!(rc, 0, "rename2 dir->dir: {}", last_err());
    });

    assert!(!exists(&img, &format!("/{src}")));
    assert!(exists(&img, &format!("/{dst}")));
    assert_eq!(record_of(&img, &format!("/{dst}")).unwrap(), src_rec);
    assert!(
        !record_in_use(&img, dst_rec),
        "old destination dir record {dst_rec} must be freed"
    );
}

#[test]
fn empty_dir_replaces_empty_dir_same_length() {
    empty_dir_replaces_empty_dir_case("dd_sl", "dir_aa", "dir_bb");
}

#[test]
fn empty_dir_replaces_empty_dir_variable_length() {
    empty_dir_replaces_empty_dir_case("dd_vl", "d1", "directory_two");
}

// ===========================================================================
// non-empty-dir target → ENOTEMPTY
// ===========================================================================

#[test]
fn non_empty_dir_target_errors() {
    let img = fresh_volume("notempty");
    rw_session(&img, |fs| {
        mkdir(fs, "/", "src_dir");
        mkdir(fs, "/", "dst_dir");
        create_file_with(fs, "/dst_dir", "inner.txt", b"keep me");

        fs_ntfs_clear_last_error();
        let rc = rename2(fs, "/src_dir", "dst_dir", FS_NTFS_RENAME_REPLACE);
        assert_eq!(rc, -1, "non-empty replace must fail");
        assert_eq!(fs_ntfs_last_errno(), ENOTEMPTY, "{}", last_err());
    });

    // Nothing was removed.
    assert!(exists(&img, "/src_dir"));
    assert!(exists(&img, "/dst_dir"));
    assert!(exists(&img, "/dst_dir/inner.txt"));
}

// ===========================================================================
// file/dir boundary → EISDIR / ENOTDIR
// ===========================================================================

#[test]
fn file_replace_dir_errors_eisdir() {
    let img = fresh_volume("eisdir");
    rw_session(&img, |fs| {
        create_file_with(fs, "/", "a_file.txt", b"x");
        mkdir(fs, "/", "a_dir");

        fs_ntfs_clear_last_error();
        let rc = rename2(fs, "/a_file.txt", "a_dir", FS_NTFS_RENAME_REPLACE);
        assert_eq!(rc, -1);
        assert_eq!(fs_ntfs_last_errno(), EISDIR, "{}", last_err());
    });
    assert!(exists(&img, "/a_file.txt"));
    assert!(exists(&img, "/a_dir"));
}

#[test]
fn dir_replace_file_errors_enotdir() {
    let img = fresh_volume("enotdir");
    rw_session(&img, |fs| {
        mkdir(fs, "/", "a_dir");
        create_file_with(fs, "/", "a_file.txt", b"x");

        fs_ntfs_clear_last_error();
        let rc = rename2(fs, "/a_dir", "a_file.txt", FS_NTFS_RENAME_REPLACE);
        assert_eq!(rc, -1);
        assert_eq!(fs_ntfs_last_errno(), ENOTDIR, "{}", last_err());
    });
    assert!(exists(&img, "/a_dir"));
    assert!(exists(&img, "/a_file.txt"));
}

// ===========================================================================
// unknown flag bits → EINVAL (forward-compat)
// ===========================================================================

#[test]
fn unknown_flag_bits_rejected_einval() {
    let img = fresh_volume("einval");
    rw_session(&img, |fs| {
        create_file_with(fs, "/", "src.txt", b"a");
        create_file_with(fs, "/", "dst.txt", b"b");

        fs_ntfs_clear_last_error();
        // 0x02 is not a defined flag bit.
        let rc = rename2(fs, "/src.txt", "dst.txt", 0x02);
        assert_eq!(rc, -1, "unknown flag must be rejected");
        assert_eq!(fs_ntfs_last_errno(), EINVAL, "{}", last_err());
    });
    // Rejected before any mutation: both names intact.
    assert!(exists(&img, "/src.txt"));
    assert!(exists(&img, "/dst.txt"));
    assert_eq!(read_all(&img, "/dst.txt"), b"b");
}

// ===========================================================================
// replace survives unmount + remount
// ===========================================================================

#[test]
fn replace_persists_across_remount() {
    let img = fresh_volume("persist");

    rw_session(&img, |fs| {
        create_file_with(fs, "/", "s.bin", b"final-payload-XYZ");
        create_file_with(fs, "/", "destination_target.bin", b"will-be-overwritten");
        let rc = rename2(
            fs,
            "/s.bin",
            "destination_target.bin",
            FS_NTFS_RENAME_REPLACE,
        );
        assert_eq!(rc, 0, "rename2 replace: {}", last_err());
    });

    // Fresh remount (separate process-level open) sees the replace.
    assert!(!exists(&img, "/s.bin"));
    assert!(exists(&img, "/destination_target.bin"));
    assert_eq!(
        read_all(&img, "/destination_target.bin"),
        b"final-payload-XYZ"
    );
}

// ===========================================================================
// renaming a file onto one of its own hard links is a POSIX no-op
// (regression: with replace=true the same-record case must NOT trip the
// duplicate-name check, and must NOT free the shared record)
// ===========================================================================

#[test]
fn replace_onto_own_hard_link_is_noop() {
    use fs_ntfs::write::{link_io, rename_replace_io};

    let img = fresh_volume("hardlink_noop");

    // /orig.txt and /alias.txt become two names for the same MFT record.
    rw_session(&img, |fs| {
        create_file_with(fs, "/", "orig.txt", b"shared");
    });
    {
        let mut io = PathIo::open_rw(Path::new(&*img)).expect("open_rw");
        link_io(&mut io, "/orig.txt", "/", "alias.txt").expect("link");
        io.sync().expect("sync");
    }
    let rec = record_of(&img, "/orig.txt").expect("orig record");
    assert_eq!(record_of(&img, "/alias.txt"), Some(rec), "shared record");

    // rename(orig -> alias, replace) where the destination is just
    // another link to the source must succeed as a terminal no-op —
    // NOT error "already exists", NOT free the shared record.
    {
        let mut io = PathIo::open_rw(Path::new(&*img)).expect("open_rw");
        rename_replace_io(&mut io, "/orig.txt", "alias.txt", true).expect("same-record no-op");
        io.sync().expect("sync");
    }

    // Both names still resolve to the shared record; nothing was freed.
    assert_eq!(record_of(&img, "/orig.txt"), Some(rec));
    assert_eq!(record_of(&img, "/alias.txt"), Some(rec));
    assert!(record_in_use(&img, rec), "shared record must remain in use");
}
