// fs_ntfs — C FFI bridge for the ntfs Rust crate.
//
// Exposes a Swift-friendly C API matching the pattern of ext4_bridge.
//
// MIT License — see LICENSE

// FFI entry points intentionally take *mut/*const pointers and
// dereference them without marking the function `unsafe`. Marking
// them `unsafe extern "C"` is an ABI-visible change for consumers;
// until we're ready to bundle that with the other ABI-breaking
// changes (§1.3 + §1.4 + §4.3 struct growth), suppress the lint.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// NTFS record builders take many geometry parameters by necessity
// (record_size, sequence, parent_ref, name, time, sector geometry,
// ...). Clippy's "too many arguments" limit flags these but any
// struct wrapper is purely cosmetic and worsens call-site clarity.
#![allow(clippy::too_many_arguments)]
// The mapping-pairs zero-fill loops index `record` by range and
// conditionally write. Clippy prefers an iterator but the readable
// loop form matches the NTFS on-disk layout doc.
#![allow(clippy::needless_range_loop)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::slice;

use crate::block_io::{BlockIo as BlockIoTrait, CallbackBlockIo, PathIo as RwPathIo};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace, NtfsStandardInformation};
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType, NtfsFile, NtfsReadSeek};

pub mod attr_io;
pub mod attr_resize;
pub mod bitmap;
pub mod block_io;
pub mod data_runs;
pub mod ea_io;
pub mod facade;
pub mod fs_core_bridge;
pub mod fsck;
pub mod idx_block;
pub mod index_io;
pub mod mft_bitmap;
pub mod mft_io;
pub mod mkfs;
pub mod record_build;
pub mod sds;
pub mod sparse;
pub mod upcase;
pub mod write;

// ---------------------------------------------------------------------------
// Thread-local error string
// ---------------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
    static LAST_ERRNO: RefCell<c_int> = const { RefCell::new(0) };
}

fn set_error(msg: &str) {
    let errno = infer_errno_from_message(msg);
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = CString::new(msg).unwrap_or_else(|_| CString::new("unknown").unwrap());
    });
    LAST_ERRNO.with(|cell| *cell.borrow_mut() = errno);
}

/// Heuristic mapping from our error message content to a POSIX errno.
/// Not exhaustive — falls back to EIO for unmatched cases. Intended
/// as a convenience companion to `fs_ntfs_last_error` so FFI consumers
/// can dispatch on a small numeric space.
fn infer_errno_from_message(msg: &str) -> c_int {
    // Values picked to match <errno.h> on POSIX (identical across
    // Linux + macOS + Windows UCRT for these common codes).
    const EIO: c_int = 5;
    const ENOENT: c_int = 2;
    const EEXIST: c_int = 17;
    const ENOSPC: c_int = 28;
    const EINVAL: c_int = 22;
    const ENOTDIR: c_int = 20;
    const EISDIR: c_int = 21;
    const ENOTEMPTY: c_int = 66; // macOS; Linux is 39; both non-zero, good enough
    const EPERM: c_int = 1;

    let m = msg;
    if m.contains("not found")
        || m.contains("nonexistent")
        || m.contains("ENOENT")
        || m.contains("not mapped")
    {
        ENOENT
    } else if m.contains("already exists") || m.contains("EEXIST") {
        EEXIST
    } else if m.contains("no room")
        || m.contains("full")
        || m.contains("out of space")
        || m.contains("exceeds record capacity")
    {
        ENOSPC
    } else if m.contains("invalid")
        || m.contains("invalid basename")
        || m.contains("null or non-UTF-8")
        || m.contains("null ")
    {
        EINVAL
    } else if m.contains("not a directory") {
        ENOTDIR
    } else if m.contains("is a directory") {
        EISDIR
    } else if m.contains("not empty") {
        ENOTEMPTY
    } else if m.contains("refuse") || m.contains("permission") {
        EPERM
    } else {
        EIO
    }
}

/// Return the last error message recorded on this thread as a NUL-terminated
/// C string, or an empty string if no error has been recorded.  The pointer
/// is valid until the next FFI call on this thread that may set an error (i.e.
/// any non-trivial call).  Copy the string before making further calls if you
/// need it to persist.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| cell.borrow().as_ptr())
}

/// Companion to `fs_ntfs_last_error`. Returns a POSIX-style errno
/// inferred from the most recent error message. `0` means "no error
/// recorded on this thread." FFI consumers can dispatch on this value
/// without parsing the error string.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_last_errno() -> c_int {
    LAST_ERRNO.with(|cell| *cell.borrow())
}

/// Reset the thread-local error state. Primarily useful in tests /
/// after a caller has consumed the last error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_clear_last_error() {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = CString::new("").unwrap();
    });
    LAST_ERRNO.with(|cell| *cell.borrow_mut() = 0);
}

// ---------------------------------------------------------------------------
// FFI return-translation helpers
// ---------------------------------------------------------------------------
// FFI bodies share two pervasive patterns:
//
//   match thing(...) {
//       Ok(()) => 0,
//       Err(e) => { set_error(&e); -1 }
//   }
//
// and the C-string argument validation:
//
//   let Some(img) = cstr_to_path(image) else {
//       set_error("fs_ntfs_<name>: null or non-UTF-8 image");
//       return -1;
//   };
//
// The helpers below collapse those into one call/macro each. They do
// not change error-string content or return values — purely textual
// compression. See callers throughout this file.

/// Record `e` as the thread-local last-error and return the int sentinel.
/// Use in `Err` arms returning an `int` from an FFI function.
#[inline]
fn err_int<E: AsRef<str>>(e: E) -> c_int {
    set_error(e.as_ref());
    -1
}

/// Record `e` as the thread-local last-error and return the `i64` sentinel.
/// Use in `Err` arms returning an `i64` from an FFI function.
#[inline]
fn err_i64<E: AsRef<str>>(e: E) -> i64 {
    set_error(e.as_ref());
    -1
}

/// Record `e` as the thread-local last-error and return a null pointer.
/// Use in `Err` arms returning a `*mut T` from an FFI function.
#[inline]
fn err_ptr<T, E: AsRef<str>>(e: E) -> *mut T {
    set_error(e.as_ref());
    std::ptr::null_mut()
}

/// `cstr_to_path` + last-error + early-return in one line.
/// Expands to a `let` binding shadowing the pointer with its &str form.
///
/// Usage:
///   cstr_or_return!(image, "fs_ntfs_create_file", "image", -1);
///   // `image` is now a `&str`, having been the `*const c_char` arg.
macro_rules! cstr_or_return {
    ($ptr:ident, $ctx:literal, $param:literal, $ret:expr) => {
        let $ptr = match cstr_to_path($ptr) {
            Some(s) => s,
            None => {
                set_error(concat!($ctx, ": null or non-UTF-8 ", $param));
                return $ret;
            }
        };
    };
}

// ---------------------------------------------------------------------------
// Callback-based reader for FSKit integration
// ---------------------------------------------------------------------------

type ReadCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u64) -> c_int;

struct CallbackReader {
    read_fn: ReadCallback,
    context: *mut c_void,
    size: u64,
    position: u64,
}

// Safety contract for `unsafe impl Send`:
//
// `context: *mut c_void` is an opaque pointer the caller (Swift /
// FSKit, Go, C, …) hands to `fs_ntfs_mount_with_callbacks` and gets
// back unchanged on every read invocation. The pointer's lifetime
// MUST cover the mount: i.e. the caller MUST NOT free the pointee
// until `fs_ntfs_umount` returns.
//
// What FSKit's serialisation actually guarantees:
//   - Per-volume callback serialisation. Two callbacks against the
//     same handle never run concurrently. This is what makes a raw
//     pointer safe to dereference from inside `read_fn` without
//     synchronisation around `position`.
//
// What it does NOT guarantee, and what callers must arrange:
//   - **Thread-confined contexts** (e.g. an `@MainActor`-bound Swift
//     `FSBlockDeviceResource` that requires drop on the main
//     thread): if the consumer's `context` points at a
//     thread-confined object, the consumer MUST wrap that object in
//     a thread-safe shell (e.g. an `Arc` or a Sendable proxy)
//     before passing the pointer here. fs_ntfs may drop the
//     handle on any thread.
//   - **Re-entrancy**: the read callback must not call back into
//     fs_ntfs against the same handle.
unsafe impl Send for CallbackReader {}

impl Read for CallbackReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.size {
            return Ok(0);
        }
        let to_read = std::cmp::min(buf.len() as u64, self.size - self.position);
        let rc = unsafe {
            (self.read_fn)(
                self.context,
                buf.as_mut_ptr() as *mut c_void,
                self.position,
                to_read,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::other("read callback failed"));
        }
        self.position += to_read;
        Ok(to_read as usize)
    }
}

impl Seek for CallbackReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(p) => self.size as i64 + p,
            SeekFrom::Current(p) => self.position as i64 + p,
        };
        if new_pos < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.position = new_pos as u64;
        Ok(self.position)
    }
}

// ---------------------------------------------------------------------------
// fs-core-backed reader — drives reads through an Arc<dyn fs_core::BlockDevice>
// ---------------------------------------------------------------------------

/// Bridges any `fs_core::BlockDevice` into the `Read + Seek` shape ntfs
/// needs. Used by `fs_ntfs_mount_with_fs_core_device` so callers can
/// mount NTFS off a generic device handle (a qcow2 reader, a partition
/// slice, etc.) without writing per-source glue.
struct FsCoreReader {
    inner: std::sync::Arc<dyn fs_core::BlockDevice>,
    size: u64,
    position: u64,
}

impl Read for FsCoreReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.size {
            return Ok(0);
        }
        let to_read = std::cmp::min(buf.len() as u64, self.size - self.position) as usize;
        let slice = &mut buf[..to_read];
        match fs_core::BlockRead::read_at(&self.inner, self.position, slice) {
            Ok(()) => {
                self.position += to_read as u64;
                Ok(to_read)
            }
            Err(e) => Err(std::io::Error::other(format!("fs_core read: {e}"))),
        }
    }
}

impl Seek for FsCoreReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(p) => self.size as i64 + p,
            SeekFrom::Current(p) => self.position as i64 + p,
        };
        if new_pos < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.position = new_pos as u64;
        Ok(self.position)
    }
}

// ---------------------------------------------------------------------------
// Bridge filesystem handle
// ---------------------------------------------------------------------------

enum ReaderKind {
    File(BufReader<File>),
    Callback(BufReader<CallbackReader>),
    FsCore(BufReader<FsCoreReader>),
}

impl Read for ReaderKind {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ReaderKind::File(r) => r.read(buf),
            ReaderKind::Callback(r) => r.read(buf),
            ReaderKind::FsCore(r) => r.read(buf),
        }
    }
}

impl Seek for ReaderKind {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            ReaderKind::File(r) => r.seek(pos),
            ReaderKind::Callback(r) => r.seek(pos),
            ReaderKind::FsCore(r) => r.seek(pos),
        }
    }
}

pub struct FsNtfsHandle {
    ntfs: Ntfs,
    reader: ReaderKind,
    /// Source of the mount, used to build a [`BlockIoTrait`] on demand
    /// for the handle-based mutator API. `None` for callers that built
    /// the handle through some path that didn't record this (shouldn't
    /// happen in practice — both mount entry points fill it in).
    source: Option<MountSource>,
}

/// Tracks whether the handle was mounted from a filesystem path
/// (`Path`), a caller-supplied callback pair (`Callbacks`), or a shared
/// `fs_core::BlockDevice` handle (`FsCore`). Used by the handle-based
/// mutator API to construct a fresh `BlockIo`-impl for each mutation
/// call without duplicating the underlying device.
enum MountSource {
    Path(PathBuf),
    Callbacks {
        read_fn: ReadCallback,
        /// `None` ⇒ handle was mounted read-only via callbacks (cfg.write
        /// was NULL). Mutation calls return EINVAL in that case.
        write_fn: Option<WriteCallback>,
        context: *mut c_void,
        size: u64,
    },
    /// Mount sourced from a shared `fs_core::BlockDevice` (a qcow2
    /// reader, a partition slice, an in-process file device, …). The
    /// `Arc` is cloned per mutation call so the underlying device is
    /// kept alive for the duration of each write without locking the
    /// handle for the whole mount lifetime. Writability is decided
    /// per-call via `fs_core::BlockDevice::is_writable` so RO devices
    /// behave the same way as RO-via-callbacks: mutators surface
    /// EINVAL with a descriptive error string.
    FsCore {
        device: std::sync::Arc<dyn fs_core::BlockDevice>,
    },
}

// Safety: the `*mut c_void` context pointer is opaque to us; the
// caller (FSKit / Go backend) is responsible for keeping it alive
// for the duration of the handle, just like for `CallbackReader`
// already (see the comment on `unsafe impl Send for CallbackReader`).
unsafe impl Send for MountSource {}
unsafe impl Sync for MountSource {}

// ---------------------------------------------------------------------------
// C types matching fs_ntfs.h
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FsNtfsAttr {
    file_record_number: u64,
    size: u64,
    /// Seconds since the UNIX epoch (1970-01-01 UTC), signed so
    /// pre-1970 NTFS timestamps are representable as negative values.
    atime_sec: i64,
    mtime_sec: i64,
    ctime_sec: i64,
    crtime_sec: i64,
    /// Sub-second component in nanoseconds (0 ≤ nsec < 1_000_000_000).
    atime_nsec: u32,
    mtime_nsec: u32,
    ctime_nsec: u32,
    crtime_nsec: u32,
    mode: u16,
    link_count: u16,
    file_type: u32,
    attributes: u32,
}

/// Max bytes a filename can occupy in `FsNtfsDirent::name`, including
/// the trailing NUL the C-side documents. NTFS allows up to 255
/// UTF-16 code units; UTF-8 worst-case encoding is 4 bytes per code
/// unit → 1020 bytes content + 1 NUL → 1024 (rounded up for alignment).
pub const FS_NTFS_DIRENT_NAME_BYTES: usize = 1024;

#[repr(C)]
pub struct FsNtfsDirent {
    file_record_number: u64,
    file_type: u8,
    name_len: u16,
    name: [u8; FS_NTFS_DIRENT_NAME_BYTES],
}

#[repr(C)]
pub struct FsNtfsVolumeInfo {
    // Fields are public so callers using the Rust binding can read
    // them directly. The C ABI side reads them via the struct's
    // C-layout offsets — there's no behavioural difference between
    // pub vs private at the binary level, only at the Rust source
    // level.
    pub volume_name: [u8; 128],
    pub cluster_size: u32,
    pub total_clusters: u64,
    pub ntfs_version_major: u16,
    pub ntfs_version_minor: u16,
    pub serial_number: u64,
    pub total_size: u64,
}

/// Extended volume info — v2 of `FsNtfsVolumeInfo`. Keeps every v1
/// field at the same offset so a callee that allocates this struct
/// and casts to v1 in a legacy code path still gets the v1 data;
/// then continues with v2-specific fields after the v1 footprint.
///
/// **Why a new struct instead of growing v1**: `FsNtfsVolumeInfo`
/// is public C ABI; widening it would silently break any caller
/// compiled against the older struct size. Existing callers stay
/// on v1; new callers opt into v2.
#[repr(C)]
pub struct FsNtfsVolumeInfoV2 {
    // -- v1 fields, identical offsets ------------------------------------
    pub volume_name: [u8; 128],
    pub cluster_size: u32,
    pub total_clusters: u64,
    pub ntfs_version_major: u16,
    pub ntfs_version_minor: u16,
    pub serial_number: u64,
    pub total_size: u64,
    // -- v2 additions ----------------------------------------------------
    /// Raw `$VOLUME_INFORMATION.flags` bits (NtfsVolumeFlags). Public
    /// flags include `VOLUME_IS_DIRTY = 0x0001`.
    pub volume_flags: u16,
    /// 1 iff `volume_flags & 0x0001 != 0` (convenience for callers
    /// that just want the dirty bit without a bitmask).
    pub is_dirty: u8,
    /// 5 bytes of explicit padding for the full gap between `is_dirty`
    /// (offset 170, 1 byte) and `mft_record_size` (offset 176, u32
    /// requires 4-byte alignment). Making the entire gap explicit
    /// avoids hidden compiler padding and keeps the layout stable
    /// across compilers / target triples.
    pub _pad: [u8; 5],
    /// Size of one MFT record in bytes (typically 1024 or 4096).
    pub mft_record_size: u32,
    /// Size of one disk sector in bytes (typically 512 or 4096).
    pub bytes_per_sector: u32,
}

/// Write callback matching the `fs_ntfs_write_fn` C typedef. The
/// trailing `write` field on [`FsNtfsBlockdevCfg`] is an *optional*
/// `Option<WriteCallback>` so existing read-only callers that
/// memset/zero-init their config are unaffected — `None` is the
/// null-function-pointer representation in the C ABI under `#[repr(C)]`.
type WriteCallback = unsafe extern "C" fn(*mut c_void, *const c_void, u64, u64) -> c_int;

#[repr(C)]
pub struct FsNtfsBlockdevCfg {
    pub read: ReadCallback,
    pub context: *mut c_void,
    pub size_bytes: u64,
    /// NEW in v0.1.1. NULL / `None` = read-only. Required by the
    /// callback-based fsck entry points.
    pub write: Option<WriteCallback>,
}

// ---------------------------------------------------------------------------
// Directory iterator — lazy
// ---------------------------------------------------------------------------

/// Heap state for the lazy NTFS index traversal. All four raw pointers form
/// a borrow chain: `entries` → `index` → `file` → `ntfs`. They are all kept
/// alive for the lifetime of this struct and are manually dropped (via the
/// `Drop` impl) in reverse-chain order.
///
/// # Safety
///
/// The NTFS upstream types carry lifetime parameters (`'n`, `'f`, `'i`) that
/// we extend to `'static` via `mem::transmute`. This is sound because:
///
/// 1. **All referenced objects are heap-pinned** inside this struct.
///    Moving `LazyDirState` (or `Box<LazyDirState>`) does not move the
///    heap-allocated `Ntfs`/`NtfsFile`/`NtfsIndex` objects — only the raw
///    pointers to them move.
///
/// 2. **Every reference in these types is non-owning** (`&T`, not `Box<T>`),
///    so dropping a reference (which Rust does as a no-op) never tries to
///    free the referent. The only owned allocations are the `Record` buffer
///    inside `NtfsFile` and the `Vec`s inside `NtfsIndex`/`NtfsIndexEntries`,
///    which are freed in correct order by the explicit `Drop` impl.
///
/// 3. **Drop order** is explicit in `Drop`: `entries` is dropped before
///    `index`, `index` before `file`, `file` before `ntfs`, which is the
///    correct order for the borrow chain.
///
/// 4. **`NtfsIndexEntries::next` accesses `ntfs` through the chain** for the
///    upcase table. The `ntfs` Box is alive for the full lifetime of the
///    struct, so every `next()` call is safe.
struct LazyDirState {
    // Raw pointers to heap-pinned NTFS objects; see safety comment above.
    // Stored as raw pointers (not Boxes) so Rust's drop glue doesn't run
    // automatically — we drop them manually in the correct order.
    entries_ptr:
        *mut ntfs::NtfsIndexEntries<'static, 'static, 'static, ntfs::indexes::NtfsFileNameIndex>,
    index_ptr: *mut ntfs::NtfsIndex<'static, 'static, ntfs::indexes::NtfsFileNameIndex>,
    file_ptr: *mut ntfs::NtfsFile<'static>,
    ntfs_ptr: *mut ntfs::Ntfs,
    reader: ReaderKind,
}

// Safety: `context: *mut c_void` inside a `CallbackReader` is treated as
// a handle managed by the caller.  Same contract as `MountSource::Callbacks`.
unsafe impl Send for LazyDirState {}

impl Drop for LazyDirState {
    fn drop(&mut self) {
        unsafe {
            // Must drop in borrow-chain order: entries → index → file → ntfs.
            drop(Box::from_raw(self.entries_ptr));
            drop(Box::from_raw(self.index_ptr));
            drop(Box::from_raw(self.file_ptr));
            drop(Box::from_raw(self.ntfs_ptr));
        }
    }
}

impl LazyDirState {
    fn new(source: &MountSource, record_number: u64) -> Result<Self, String> {
        // 1. Open an independent reader for this iterator so the mount handle
        //    remains usable for concurrent stat/read calls.
        let mut reader = open_reader_from_source(source)?;

        // 2. Parse a fresh Ntfs header + upcase table from the new reader.
        let ntfs_box = {
            let mut ntfs = Ntfs::new(&mut reader).map_err(|e| format!("ntfs init: {e}"))?;
            ntfs.read_upcase_table(&mut reader)
                .map_err(|e| format!("upcase: {e}"))?;
            Box::new(ntfs)
        };
        let ntfs_ptr = Box::into_raw(ntfs_box);

        // 3. Navigate to the directory by record number.
        let file = {
            // Safety: ntfs_ptr was just allocated and is alive here.
            let ntfs_ref: &ntfs::Ntfs = unsafe { &*ntfs_ptr };
            ntfs_ref
                .file(&mut reader, record_number)
                .map_err(|e| format!("file record {record_number}: {e}"))?
        };
        // Transmute 'n lifetime to 'static; safe because ntfs_ptr outlives the Box.
        let file_box: Box<ntfs::NtfsFile<'static>> = unsafe { std::mem::transmute(Box::new(file)) };
        let file_ptr = Box::into_raw(file_box);

        // 4. Build the directory index.
        let index = {
            let file_ref: &ntfs::NtfsFile<'static> = unsafe { &*file_ptr };
            file_ref
                .directory_index(&mut reader)
                .map_err(|e| format!("directory_index: {e}"))?
        };
        let index_box: Box<ntfs::NtfsIndex<'static, 'static, ntfs::indexes::NtfsFileNameIndex>> =
            unsafe { std::mem::transmute(Box::new(index)) };
        let index_ptr = Box::into_raw(index_box);

        // 5. Create the entry iterator.
        let entries = {
            let index_ref: &ntfs::NtfsIndex<'static, 'static, ntfs::indexes::NtfsFileNameIndex> =
                unsafe { &*index_ptr };
            index_ref.entries()
        };
        let entries_box: Box<
            ntfs::NtfsIndexEntries<'static, 'static, 'static, ntfs::indexes::NtfsFileNameIndex>,
        > = unsafe { std::mem::transmute(Box::new(entries)) };
        let entries_ptr = Box::into_raw(entries_box);

        Ok(LazyDirState {
            entries_ptr,
            index_ptr,
            file_ptr,
            ntfs_ptr,
            reader,
        })
    }

    fn next_entry(
        &mut self,
    ) -> Option<Result<ntfs::NtfsIndexEntry<'_, ntfs::indexes::NtfsFileNameIndex>, ntfs::NtfsError>>
    {
        let entries = unsafe { &mut *self.entries_ptr };
        entries.next(&mut self.reader)
    }
}

/// Open a fresh `ReaderKind` from a `MountSource` without touching the
/// existing mount handle's reader.
fn open_reader_from_source(source: &MountSource) -> Result<ReaderKind, String> {
    match source {
        MountSource::Path(path) => {
            let f = File::open(path).map_err(|e| format!("open '{}': {e}", path.display()))?;
            Ok(ReaderKind::File(BufReader::new(f)))
        }
        MountSource::Callbacks {
            read_fn,
            context,
            size,
            ..
        } => Ok(ReaderKind::Callback(BufReader::new(CallbackReader {
            read_fn: *read_fn,
            context: *context,
            size: *size,
            position: 0,
        }))),
        MountSource::FsCore { device } => {
            let size = fs_core::BlockRead::size_bytes(device.as_ref());
            Ok(ReaderKind::FsCore(BufReader::new(FsCoreReader {
                inner: device.clone(),
                size,
                position: 0,
            })))
        }
    }
}

/// Directory iterator. Yields `.` and `..` first (synthesized), then real
/// entries lazily from the on-disk index via [`LazyDirState`].
pub struct FsNtfsDirIter {
    /// Synthesized `.` and `..` entries; yielded before `lazy`.
    dot: FsNtfsDirent,
    dotdot: FsNtfsDirent,
    /// Scratch buffer for the most recently yielded real entry. The C caller
    /// may hold a pointer into this between calls to `fs_ntfs_dir_next`.
    current: FsNtfsDirent,
    /// Phase: 0 = dot pending, 1 = dotdot pending, 2 = lazy entries, 3 = done.
    phase: u8,
    /// Number of real index entries silently skipped (e.g. DOS-only names,
    /// malformed entries). Surfaced via `fs_ntfs_dir_skipped`.
    skipped_count: u64,
    /// Lazy index state; `None` only while `phase < 2`.
    lazy: Option<Box<LazyDirState>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert an NTFS timestamp (100 ns intervals since 1601-01-01 UTC) to a
/// `(seconds, nanoseconds)` UNIX pair. Seconds are signed so pre-1970
/// timestamps are representable as negative values. The nsec component is
/// always in `[0, 1_000_000_000)`.
fn ntfs_time_to_unix(ntfs_time: ntfs::NtfsTime) -> (i64, u32) {
    // NTFS epoch is 1601-01-01 UTC; UNIX epoch is 1970-01-01 UTC.
    // Difference = 11 644 473 600 seconds.
    const EPOCH_DIFF: i64 = 11_644_473_600;
    let ts = ntfs_time.nt_timestamp();
    let secs = (ts / 10_000_000) as i64 - EPOCH_DIFF;
    let nsec = ((ts % 10_000_000) * 100) as u32;
    (secs, nsec)
}

/// Build a synthesized dirent (used for "." / "..").
fn make_dirent(file_record_number: u64, file_type: u8, name: &[u8]) -> FsNtfsDirent {
    let mut out = FsNtfsDirent {
        file_record_number,
        file_type,
        name_len: std::cmp::min(name.len(), FS_NTFS_DIRENT_NAME_BYTES - 1) as u16,
        name: [0u8; FS_NTFS_DIRENT_NAME_BYTES],
    };
    let n = out.name_len as usize;
    out.name[..n].copy_from_slice(&name[..n]);
    out
}

/// Read the parent-directory record number from a file's
/// `$FILE_NAME` attribute. Used to walk `..` path components.
fn parent_record_number_of(file: &NtfsFile, reader: &mut ReaderKind) -> Result<u64, String> {
    let mut attrs = file.attributes();
    while let Some(item) = attrs.next(reader) {
        let item = item.map_err(|e| format!("attr iter: {e}"))?;
        let attribute = item.to_attribute().map_err(|e| format!("to_attr: {e}"))?;
        if attribute.ty().ok() != Some(NtfsAttributeType::FileName) {
            continue;
        }
        if let Ok(file_name) = attribute.structured_value::<_, NtfsFileName>(reader) {
            let parent_ref = file_name.parent_directory_reference();
            return Ok(parent_ref.file_record_number());
        }
    }
    Err("no $FILE_NAME attribute to find parent via".to_string())
}

/// Navigate to a file by path from the root directory.
fn navigate_to_path<'n>(
    ntfs: &'n Ntfs,
    reader: &mut ReaderKind,
    path: &str,
) -> Result<NtfsFile<'n>, String> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return ntfs
            .root_directory(reader)
            .map_err(|e| format!("root directory: {e}"));
    }

    let mut current = ntfs
        .root_directory(reader)
        .map_err(|e| format!("root directory: {e}"))?;

    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            // Walk to parent via $FILE_NAME.parent_directory_reference.
            // At the root, ".." stays at root (standard POSIX behavior).
            if current.file_record_number() == KnownNtfsFileRecordNumber::RootDirectory as u64 {
                continue;
            }
            let parent_rn = parent_record_number_of(&current, reader)
                .map_err(|e| format!("parent of record {}: {e}", current.file_record_number()))?;
            current = ntfs
                .file(reader, parent_rn)
                .map_err(|e| format!("open parent record {parent_rn}: {e}"))?;
            continue;
        }

        let index = current
            .directory_index(reader)
            .map_err(|e| format!("directory index for '{}': {e}", component))?;

        let mut finder = index.finder();
        let entry = NtfsFileNameIndex::find(&mut finder, ntfs, reader, component)
            .ok_or_else(|| format!("not found: '{component}'"))?
            .map_err(|e| format!("find '{component}': {e}"))?;

        current = entry
            .to_file(ntfs, reader)
            .map_err(|e| format!("to_file '{component}': {e}"))?;
    }

    Ok(current)
}

/// Fill an FsNtfsAttr from an NtfsFile.
fn fill_attr(
    file: &NtfsFile,
    reader: &mut ReaderKind,
    attr: &mut FsNtfsAttr,
) -> Result<(), String> {
    attr.file_record_number = file.file_record_number();
    attr.link_count = file.hard_link_count();

    if file.is_directory() {
        attr.file_type = 2; // FS_NTFS_FT_DIR
        attr.mode = 0o40755;
    } else {
        attr.file_type = 1; // FS_NTFS_FT_REG_FILE
        attr.mode = 0o100644;
    }

    // Read StandardInformation for timestamps and NTFS attributes
    let mut attributes = file.attributes();
    while let Some(item) = attributes.next(reader) {
        let item = match item {
            Ok(i) => i,
            Err(_) => continue,
        };
        let attribute = match item.to_attribute() {
            Ok(a) => a,
            Err(_) => continue,
        };

        match attribute.ty() {
            Ok(NtfsAttributeType::StandardInformation) => {
                if let Ok(std_info) =
                    attribute.resident_structured_value::<NtfsStandardInformation>()
                {
                    (attr.crtime_sec, attr.crtime_nsec) =
                        ntfs_time_to_unix(std_info.creation_time());
                    (attr.mtime_sec, attr.mtime_nsec) =
                        ntfs_time_to_unix(std_info.modification_time());
                    (attr.atime_sec, attr.atime_nsec) = ntfs_time_to_unix(std_info.access_time());
                    (attr.ctime_sec, attr.ctime_nsec) =
                        ntfs_time_to_unix(std_info.mft_record_modification_time());
                    attr.attributes = std_info.file_attributes().bits();
                }
            }
            Ok(NtfsAttributeType::Data)
                if attribute.name().map(|n| n.is_empty()).unwrap_or(true) =>
            {
                attr.size = attribute.value_length();
            }
            Ok(NtfsAttributeType::ReparsePoint) => {
                // Read the 32-bit reparse tag and dispatch. The
                // REPARSE_POINT flag on $FILE_NAME is an "SOME reparse
                // kind" marker; only the tag tells us *which*. See
                // docs/NEXT_PLAN.md §1.2 / docs/status.md §cross-check.
                if attr.file_type != 2 {
                    // Not a directory — default regular. The actual
                    // type depends on the tag below.
                    attr.file_type = 1;
                    attr.mode = 0o100644;
                }
                // Read up to 4 bytes of the attribute value for the tag.
                if let Ok(mut value) = attribute.value(reader) {
                    let mut tag_buf = [0u8; 4];
                    if value.read(reader, &mut tag_buf).is_ok() {
                        let tag = u32::from_le_bytes(tag_buf);
                        match tag {
                            0xA000_000C /* SYMLINK */ => {
                                attr.file_type = 7;
                                attr.mode = 0o120777;
                            }
                            0xA000_0003 /* MOUNT_POINT */ => {
                                attr.file_type = 8; // FS_NTFS_FT_JUNCTION
                            }
                            // WOF / LX_SYMLINK / APPEXECLINK / dedup etc. —
                            // leave file_type at whatever it was
                            // (directory or regular) so POSIX callers
                            // can access the underlying data.
                            _ => {}
                        }
                    }
                }
            }
            Ok(NtfsAttributeType::FileName) => {
                // Historically we used the REPARSE_POINT flag on
                // $FILE_NAME as a symlink signal. That's wrong — it
                // marks any reparse type. The real dispatch happens
                // in the ReparsePoint case above when the actual
                // attribute is present.
            }
            _ => {}
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Mount / Unmount
// ---------------------------------------------------------------------------

/// Mount an NTFS volume from a filesystem path (raw image file, block device,
/// etc.). Returns an opaque handle on success, NULL on error; call
/// [`fs_ntfs_last_error`] for the message.  The handle supports both read and
/// write operations.  Release with [`fs_ntfs_umount`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mount(device_path: *const c_char) -> *mut FsNtfsHandle {
    if device_path.is_null() {
        set_error("null device path");
        return std::ptr::null_mut();
    }

    let path = match unsafe { CStr::from_ptr(device_path) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid path: {e}"));
            return std::ptr::null_mut();
        }
    };

    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            set_error(&format!("open '{path}': {e}"));
            return std::ptr::null_mut();
        }
    };

    let mut reader = ReaderKind::File(BufReader::new(file));

    let mut ntfs = match Ntfs::new(&mut reader) {
        Ok(n) => n,
        Err(e) => {
            set_error(&format!("ntfs init: {e}"));
            return std::ptr::null_mut();
        }
    };

    if let Err(e) = ntfs.read_upcase_table(&mut reader) {
        set_error(&format!("upcase table: {e}"));
        return std::ptr::null_mut();
    }

    // Mimic `ntfs.sys`'s "upgrade on mount". Path-based mount is
    // RW-capable (the mutator API reopens RW per call), so apply the
    // 1.2 -> 3.1 upgrade best-effort. Failure is logged at `warn` and
    // does not fail the mount.
    match crate::fsck::upgrade_volume_version(path) {
        Ok(true) => {
            log::info!(target: "fs_ntfs", "upgraded $VOLUME_INFORMATION 1.2 -> 3.1 on {path}")
        }
        Ok(false) => {
            log::debug!(target: "fs_ntfs", "no $VOLUME_INFORMATION upgrade needed on {path}")
        }
        Err(e) => {
            log::warn!(target: "fs_ntfs", "$VOLUME_INFORMATION upgrade skipped on {path}: {e}")
        }
    }

    let bridge = Box::new(FsNtfsHandle {
        ntfs,
        reader,
        source: Some(MountSource::Path(PathBuf::from(path))),
    });
    log::info!(target: "fs_ntfs", "mount path={path}");
    Box::into_raw(bridge)
}

/// Mount an NTFS volume via caller-supplied read (and optionally write)
/// callbacks described by `cfg`.  Useful when the backing store is not a
/// filesystem path (e.g. an in-memory buffer, a network device, or a custom
/// block-layer abstraction).  Returns NULL on error; call
/// [`fs_ntfs_last_error`] for the message.  The resulting handle supports
/// the full read API; mutators work when a `write` callback is provided.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mount_with_callbacks(cfg: *const FsNtfsBlockdevCfg) -> *mut FsNtfsHandle {
    if cfg.is_null() {
        set_error("null config");
        return std::ptr::null_mut();
    }

    let cfg = unsafe { &*cfg };

    let cb_reader = CallbackReader {
        read_fn: cfg.read,
        context: cfg.context,
        size: cfg.size_bytes,
        position: 0,
    };

    let mut reader = ReaderKind::Callback(BufReader::new(cb_reader));

    let mut ntfs = match Ntfs::new(&mut reader) {
        Ok(n) => n,
        Err(e) => {
            set_error(&format!("ntfs init: {e}"));
            return std::ptr::null_mut();
        }
    };

    if let Err(e) = ntfs.read_upcase_table(&mut reader) {
        set_error(&format!("upcase table: {e}"));
        return std::ptr::null_mut();
    }

    // Mimic `ntfs.sys`'s "upgrade on mount". Only fire when the
    // caller supplied a write callback — otherwise the mount is
    // effectively RO and the upgrade write would fail. Best-effort;
    // failure logged at `warn` and does not fail the mount.
    if cfg.write.is_some() {
        let mut io = CallbackBlockIo {
            read_fn: cfg.read,
            write_fn: cfg.write,
            context: cfg.context,
            size: cfg.size_bytes,
        };
        match crate::fsck::upgrade_volume_version_io(&mut io) {
            Ok(true) => {
                log::info!(target: "fs_ntfs", "upgraded $VOLUME_INFORMATION 1.2 -> 3.1 (callback mount)")
            }
            Ok(false) => {
                log::debug!(target: "fs_ntfs", "no $VOLUME_INFORMATION upgrade needed (callback mount)")
            }
            Err(e) => {
                log::warn!(target: "fs_ntfs", "$VOLUME_INFORMATION upgrade skipped (callback mount): {e}")
            }
        }
    }

    let bridge = Box::new(FsNtfsHandle {
        ntfs,
        reader,
        source: Some(MountSource::Callbacks {
            read_fn: cfg.read,
            write_fn: cfg.write,
            context: cfg.context,
            size: cfg.size_bytes,
        }),
    });
    Box::into_raw(bridge)
}

/// Mount via an `FsCoreDevice` handle from a sister crate
/// (`qcow2_open` from am-img-qcow2, `partitions_open_slice` from
/// am-partitions, `fs_core_file_open` from am-fs-core).
///
/// Returns NULL on failure; consult `fs_ntfs_last_error()` for detail.
///
/// The handle's reference count is incremented internally; the caller
/// still owns their `*FsCoreDevice` and frees it via
/// `fs_core_device_close`. Closing the resulting `*FsNtfsHandle` via
/// `fs_ntfs_umount` drops the mount's own reference.
///
/// This entry point provides **read-only** access. Mutator API calls
/// (`fs_ntfs_unlink_h`, `fs_ntfs_mkdir_h`, `fs_ntfs_write_file_contents_h`,
/// etc.) return EINVAL with `"handle has no recorded mount source"`.
/// Read paths (`fs_ntfs_dir_open`, `fs_ntfs_read_file`, …) are
/// unaffected. For RW use [`fs_ntfs_mount_rw_with_fs_core_device`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mount_with_fs_core_device(
    handle: *mut fs_core::ffi::FsCoreDevice,
) -> *mut FsNtfsHandle {
    if handle.is_null() {
        set_error("null fs_core handle");
        return std::ptr::null_mut();
    }

    // Safety: `handle` is non-null (checked above) and the C ABI
    // contract documented in `include/fs_ntfs.h` requires the caller to
    // keep the pointer valid for the duration of this call. The clone
    // takes a fresh `Arc` reference so the underlying device outlives
    // any subsequent mutation of the handle.
    let inner = unsafe { (*handle).inner().clone() };
    let size = fs_core::BlockRead::size_bytes(&inner);

    let fs_core_reader = FsCoreReader {
        inner,
        size,
        position: 0,
    };
    let mut reader = ReaderKind::FsCore(BufReader::new(fs_core_reader));

    let mut ntfs = match Ntfs::new(&mut reader) {
        Ok(n) => n,
        Err(e) => {
            set_error(&format!("ntfs init: {e}"));
            return std::ptr::null_mut();
        }
    };

    if let Err(e) = ntfs.read_upcase_table(&mut reader) {
        set_error(&format!("upcase table: {e}"));
        return std::ptr::null_mut();
    }

    let bridge = Box::new(FsNtfsHandle {
        ntfs,
        reader,
        // No MountSource — mutator API returns EINVAL for fs-core RO
        // mounts. Callers that need RW must use
        // `fs_ntfs_mount_rw_with_fs_core_device` instead.
        source: None,
    });
    log::info!(target: "fs_ntfs", "mount via fs_core handle (size={size})");
    Box::into_raw(bridge)
}

/// Mount an NTFS volume RW via an `FsCoreDevice` handle from a sister
/// crate. Same shape as [`fs_ntfs_mount_with_fs_core_device`], but the
/// underlying device is recorded as the handle's mount source so the
/// `_h` mutator family (`fs_ntfs_create_file_h`, `fs_ntfs_mkdir_h`,
/// `fs_ntfs_write_file_contents_h`, `fs_ntfs_unlink_h`, …) can write
/// through it.
///
/// The supplied device must report `is_writable() == true`; otherwise
/// the mount succeeds (the device itself is parsable) but the first
/// mutator call returns EINVAL with a descriptive error string. The
/// mount itself does not pre-flight writability so callers can mount
/// hybrid devices that gate writability per-region (e.g. a disk image
/// reader whose RW support is enabled lazily). Read paths work
/// regardless of writability.
///
/// Returns NULL on failure; consult `fs_ntfs_last_error()` for detail.
///
/// The handle's reference count is incremented internally; the caller
/// still owns their `*FsCoreDevice` and frees it via
/// `fs_core_device_close`. Closing the resulting `*FsNtfsHandle` via
/// `fs_ntfs_umount` drops the mount's own reference.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mount_rw_with_fs_core_device(
    handle: *mut fs_core::ffi::FsCoreDevice,
) -> *mut FsNtfsHandle {
    if handle.is_null() {
        set_error("null fs_core handle");
        return std::ptr::null_mut();
    }

    // Safety: same contract as the RO sibling above. The Arc clone
    // here is the source-of-truth reference recorded in MountSource;
    // the second clone below is for the BufReader-backed reader used
    // by the read path.
    let device = unsafe { (*handle).inner().clone() };
    let size = fs_core::BlockRead::size_bytes(&device);

    let fs_core_reader = FsCoreReader {
        inner: device.clone(),
        size,
        position: 0,
    };
    let mut reader = ReaderKind::FsCore(BufReader::new(fs_core_reader));

    let mut ntfs = match Ntfs::new(&mut reader) {
        Ok(n) => n,
        Err(e) => {
            set_error(&format!("ntfs init: {e}"));
            return std::ptr::null_mut();
        }
    };

    if let Err(e) = ntfs.read_upcase_table(&mut reader) {
        set_error(&format!("upcase table: {e}"));
        return std::ptr::null_mut();
    }

    // Mimic `ntfs.sys`'s "upgrade on mount": if the volume was
    // fresh-formatted (major=1, minor=2, UPGRADE_ON_MOUNT set —
    // what Microsoft `format.com` and our `mkfs` produce), rewrite
    // it to 3.1 with the flag cleared so volumes touched by our
    // driver look "already upgraded" to Windows, parallel to what
    // `ntfs.sys` would have done on first RW mount.
    //
    // Best-effort: log on failure but don't fail the mount. The
    // volume is still usable in its pre-upgrade form.
    if fs_core::BlockDevice::is_writable(&device) {
        let mut fsck_io = FsCoreBlockIo {
            device: device.clone(),
            size,
        };
        match crate::fsck::upgrade_volume_version_io(&mut fsck_io) {
            Ok(true) => log::info!(target: "fs_ntfs", "upgraded $VOLUME_INFORMATION 1.2 -> 3.1"),
            Ok(false) => log::debug!(target: "fs_ntfs", "no $VOLUME_INFORMATION upgrade needed"),
            Err(e) => log::warn!(target: "fs_ntfs", "$VOLUME_INFORMATION upgrade skipped: {e}"),
        }
    }

    let bridge = Box::new(FsNtfsHandle {
        ntfs,
        reader,
        source: Some(MountSource::FsCore { device }),
    });
    log::info!(target: "fs_ntfs", "mount rw via fs_core handle (size={size})");
    Box::into_raw(bridge)
}

/// Owned `BlockIo` constructed from a [`FsNtfsHandle`] for the
/// duration of a single mutator call.
///
/// Path-mounted handles open the file RW for each mutation; the
/// kernel page cache amortises the cost so the open isn't observably
/// slower than threading a long-lived `File` through the call stack.
/// Callback-mounted handles wrap the existing read/write callback
/// pair without touching the underlying device.
enum HandleIo {
    Path(RwPathIo),
    Callback(CallbackBlockIo),
    FsCore(FsCoreBlockIo),
}

/// `BlockIo` adapter over a shared `Arc<dyn fs_core::BlockDevice>`.
///
/// The trait is `&mut self` for legacy reasons but every method only
/// needs shared access to the underlying device — `BlockRead::read_at`
/// and `BlockDevice::write_at` both take `&self`. Using `Arc` rather
/// than a generic parameter keeps the `HandleIo` enum object-safe and
/// matches how the device crosses the FFI boundary.
struct FsCoreBlockIo {
    device: std::sync::Arc<dyn fs_core::BlockDevice>,
    size: u64,
}

impl BlockIoTrait for FsCoreBlockIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        fs_core::BlockRead::read_at(&self.device, offset, buf).map_err(|e| e.to_string())
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        // Mirror `fs_core_bridge::CoreDevice` — refuse the write up
        // front when the device reports itself read-only, so the
        // failure mode is deterministic regardless of which
        // implementor we wrap.
        if !fs_core::BlockDevice::is_writable(&self.device) {
            return Err("device is not writable".to_string());
        }
        fs_core::BlockDevice::write_at(&self.device, offset, buf).map_err(|e| e.to_string())
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn sync(&mut self) -> Result<(), String> {
        fs_core::BlockDevice::flush(&self.device).map_err(|e| e.to_string())
    }
}

/// Same shape as the `BlockIo` impl above — fsck and the mutator API
/// have parallel trait surfaces (intentional, see `src/fsck.rs`),
/// so the adapter has to satisfy both. Bodies are identical.
impl crate::fsck::FsckIo for FsCoreBlockIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        fs_core::BlockRead::read_at(&self.device, offset, buf).map_err(|e| e.to_string())
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        if !fs_core::BlockDevice::is_writable(&self.device) {
            return Err("device is not writable".to_string());
        }
        fs_core::BlockDevice::write_at(&self.device, offset, buf).map_err(|e| e.to_string())
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn sync(&mut self) -> Result<(), String> {
        fs_core::BlockDevice::flush(&self.device).map_err(|e| e.to_string())
    }
}

/// Same pattern as the `FsCoreBlockIo` impl above. Lets the
/// upgrade-on-mount path drive a callback-backed volume via fsck's
/// `FsckIo`. Bodies delegate straight to the underlying `BlockIo`
/// impl in `block_io.rs`.
///
/// `sync` delegates explicitly even though both `FsckIo::sync` and
/// `BlockIo::sync` default to `Ok(())` — the delegation is for
/// future-proofing: if `BlockIo::sync` on `CallbackBlockIo` ever
/// grows a real flush (e.g. a host-supplied flush callback), the
/// fsck path picks it up automatically. Today it's still a no-op
/// (callback-backed mounts let the host drain on its own barrier).
impl crate::fsck::FsckIo for CallbackBlockIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        <Self as BlockIoTrait>::read_exact_at(self, offset, buf)
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        <Self as BlockIoTrait>::write_all_at(self, offset, buf)
    }
    fn size(&self) -> u64 {
        <Self as BlockIoTrait>::size(self)
    }
    fn sync(&mut self) -> Result<(), String> {
        <Self as BlockIoTrait>::sync(self)
    }
}

impl BlockIoTrait for HandleIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        match self {
            HandleIo::Path(p) => p.read_exact_at(offset, buf),
            HandleIo::Callback(c) => c.read_exact_at(offset, buf),
            HandleIo::FsCore(f) => f.read_exact_at(offset, buf),
        }
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        match self {
            HandleIo::Path(p) => p.write_all_at(offset, buf),
            HandleIo::Callback(c) => c.write_all_at(offset, buf),
            HandleIo::FsCore(f) => f.write_all_at(offset, buf),
        }
    }
    fn size(&self) -> u64 {
        match self {
            HandleIo::Path(p) => p.size(),
            HandleIo::Callback(c) => c.size(),
            HandleIo::FsCore(f) => f.size(),
        }
    }
    fn sync(&mut self) -> Result<(), String> {
        match self {
            HandleIo::Path(p) => p.sync(),
            HandleIo::Callback(c) => c.sync(),
            HandleIo::FsCore(f) => f.sync(),
        }
    }
}

/// Build a `HandleIo` from a `FsNtfsHandle` ready for a mutation call.
/// Returns `Err(message)` if the handle was mounted read-only via
/// callbacks (`cfg.write` was NULL) or has no recorded source.
fn handle_to_rw_io(handle: &FsNtfsHandle) -> Result<HandleIo, String> {
    match &handle.source {
        Some(MountSource::Path(p)) => RwPathIo::open_rw(p).map(HandleIo::Path),
        Some(MountSource::Callbacks {
            read_fn,
            write_fn,
            context,
            size,
        }) => {
            if write_fn.is_none() {
                return Err(
                    "handle mounted read-only via callbacks (cfg.write was NULL)".to_string(),
                );
            }
            Ok(HandleIo::Callback(CallbackBlockIo {
                read_fn: *read_fn,
                write_fn: *write_fn,
                context: *context,
                size: *size,
            }))
        }
        Some(MountSource::FsCore { device }) => {
            if !fs_core::BlockDevice::is_writable(device) {
                return Err(
                    "handle mounted read-only via fs_core device (is_writable=false)".to_string(),
                );
            }
            let size = fs_core::BlockRead::size_bytes(device);
            Ok(HandleIo::FsCore(FsCoreBlockIo {
                device: device.clone(),
                size,
            }))
        }
        None => Err("handle has no recorded mount source".to_string()),
    }
}

/// Close a mount handle returned by any `fs_ntfs_mount*` function and free
/// all associated memory.  Passing NULL is a no-op.  Do not use the handle
/// after calling this.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_umount(fs: *mut FsNtfsHandle) {
    if !fs.is_null() {
        log::info!(target: "fs_ntfs", "umount handle={fs:p}");
        unsafe {
            drop(Box::from_raw(fs));
        }
    }
}

// ---------------------------------------------------------------------------
// Volume info
// ---------------------------------------------------------------------------

/// Fill `*info` with volume-level metadata (label, cluster size, serial
/// number, NTFS version). Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_get_volume_info(
    fs: *mut FsNtfsHandle,
    info: *mut FsNtfsVolumeInfo,
) -> c_int {
    if fs.is_null() || info.is_null() {
        return -1;
    }

    let bridge = unsafe { &mut *fs };
    let out = unsafe { &mut *info };

    // Zero out
    out.volume_name = [0u8; 128];
    out.cluster_size = bridge.ntfs.cluster_size();
    out.total_size = bridge.ntfs.size();
    out.total_clusters = bridge.ntfs.size() / bridge.ntfs.cluster_size() as u64;
    out.serial_number = bridge.ntfs.serial_number();

    // Volume name
    if let Some(Ok(vol_name)) = bridge.ntfs.volume_name(&mut bridge.reader) {
        let name_str = vol_name.name().to_string_lossy();
        let name_bytes = name_str.as_bytes();
        let copy_len = std::cmp::min(name_bytes.len(), 127);
        out.volume_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
    }

    // Version
    if let Ok(vol_info) = bridge.ntfs.volume_info(&mut bridge.reader) {
        out.ntfs_version_major = vol_info.major_version() as u16;
        out.ntfs_version_minor = vol_info.minor_version() as u16;
    }

    0
}

/// Extended volume info — populates `FsNtfsVolumeInfoV2`. Adds the
/// `volume_flags` (including the dirty bit), `mft_record_size`, and
/// `bytes_per_sector` fields on top of everything v1 exposes. See
/// `FsNtfsVolumeInfoV2` for ABI-compat rationale (v1 fields land at
/// identical offsets).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_get_volume_info_v2(
    fs: *mut FsNtfsHandle,
    info: *mut FsNtfsVolumeInfoV2,
) -> c_int {
    if fs.is_null() || info.is_null() {
        return -1;
    }
    let bridge = unsafe { &mut *fs };
    let out = unsafe { &mut *info };
    out.volume_name = [0u8; 128];
    out.cluster_size = bridge.ntfs.cluster_size();
    out.total_size = bridge.ntfs.size();
    out.total_clusters = bridge.ntfs.size() / bridge.ntfs.cluster_size() as u64;
    out.serial_number = bridge.ntfs.serial_number();
    out.mft_record_size = bridge.ntfs.file_record_size();
    out.bytes_per_sector = bridge.ntfs.sector_size() as u32;
    out.volume_flags = 0;
    out.is_dirty = 0;
    out.ntfs_version_major = 0;
    out.ntfs_version_minor = 0;
    out._pad = [0u8; 5];

    if let Some(Ok(vol_name)) = bridge.ntfs.volume_name(&mut bridge.reader) {
        let name_str = vol_name.name().to_string_lossy();
        let name_bytes = name_str.as_bytes();
        let copy_len = std::cmp::min(name_bytes.len(), 127);
        out.volume_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
    }

    if let Ok(vol_info) = bridge.ntfs.volume_info(&mut bridge.reader) {
        out.ntfs_version_major = vol_info.major_version() as u16;
        out.ntfs_version_minor = vol_info.minor_version() as u16;
        let flags = vol_info.flags();
        out.volume_flags = flags.bits();
        // VOLUME_IS_DIRTY = 0x0001
        out.is_dirty = if flags.bits() & 0x0001 != 0 { 1 } else { 0 };
    }

    0
}

/// Set the volume label on an unmounted NTFS image. Empty `label`
/// removes the `$VOLUME_NAME` attribute entirely. Returns 0 on
/// success, -1 on error (e.g. label too long, or image cannot be
/// opened for writing). NTFS labels are conventionally capped at 32
/// UTF-16 code units; longer labels are rejected.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_set_volume_label(image: *const c_char, label: *const c_char) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_set_volume_label: null or non-UTF-8 image");
        return -1;
    };
    let label_str = if label.is_null() {
        ""
    } else {
        match unsafe { CStr::from_ptr(label) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_error("fs_ntfs_set_volume_label: non-UTF-8 label");
                return -1;
            }
        }
    };
    match write::set_volume_label(std::path::Path::new(img), label_str) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Read the volume label from an unmounted NTFS image into `out_buf`
/// (UTF-8, no terminating NUL written by this function — caller is
/// responsible for null-termination if needed). Returns the number of
/// UTF-8 bytes written on success (0 if the volume has no label),
/// or -1 on error.
///
/// If the on-disk label is longer than `out_buf_len`, the result is
/// truncated and the truncated byte count is returned; no error is
/// signalled. Callers wanting the full label should pre-size
/// `out_buf` to at least 128 bytes (32 UTF-16 code units * up to 4
/// UTF-8 bytes each).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_read_volume_label(
    image: *const c_char,
    out_buf: *mut c_char,
    out_buf_len: usize,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_read_volume_label: null or non-UTF-8 image");
        return -1;
    };
    if out_buf.is_null() {
        set_error("fs_ntfs_read_volume_label: null out_buf");
        return -1;
    }
    match write::read_volume_label(std::path::Path::new(img)) {
        Ok(label) => {
            let bytes = label.as_bytes();
            let n = std::cmp::min(bytes.len(), out_buf_len);
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf as *mut u8, n) };
            n as c_int
        }
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Stat
// ---------------------------------------------------------------------------

/// Stat a file by path using an open mount handle. Fills `*attr` on success
/// and returns 0; returns -1 on error. The handle variant of
/// `fs_ntfs_stat_h` (path-based) accepts a null-terminated UTF-8 path
/// rooted at the volume root (leading `\` or `/` optional).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_stat(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
    attr: *mut FsNtfsAttr,
) -> c_int {
    if fs.is_null() || path.is_null() || attr.is_null() {
        return -1;
    }

    let bridge = unsafe { &mut *fs };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let out = unsafe { &mut *attr };

    // Zero out
    *out = FsNtfsAttr {
        file_record_number: 0,
        size: 0,
        atime_sec: 0,
        mtime_sec: 0,
        ctime_sec: 0,
        crtime_sec: 0,
        atime_nsec: 0,
        mtime_nsec: 0,
        ctime_nsec: 0,
        crtime_nsec: 0,
        mode: 0,
        link_count: 0,
        file_type: 0,
        attributes: 0,
    };

    let file = match navigate_to_path(&bridge.ntfs, &mut bridge.reader, path_str) {
        Ok(f) => f,
        Err(e) => return err_int(e),
    };

    if let Err(e) = fill_attr(&file, &mut bridge.reader, out) {
        return err_int(e);
    }

    0
}

// ---------------------------------------------------------------------------
// Directory listing
// ---------------------------------------------------------------------------

/// Open a directory for iteration. Returns an opaque [`FsNtfsDirIter`]
/// pointer on success, NULL on error.  The first two entries are always
/// synthesized `"."` and `".."`.  Advance with [`fs_ntfs_dir_next`]; free
/// with [`fs_ntfs_dir_close`].  Check [`fs_ntfs_dir_skipped`] after
/// exhaustion to detect corrupt index rows that were silently skipped.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_dir_open(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
) -> *mut FsNtfsDirIter {
    if fs.is_null() || path.is_null() {
        return std::ptr::null_mut();
    }

    let bridge = unsafe { &mut *fs };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let dir_file = match navigate_to_path(&bridge.ntfs, &mut bridge.reader, path_str) {
        Ok(f) => f,
        Err(e) => return err_ptr(e),
    };

    let current_record_number = dir_file.file_record_number();
    // Synthesize "." and ".." at the head of the listing. NTFS indexes
    // don't store them; POSIX readdir callers expect them.
    let parent_record_number =
        if current_record_number == KnownNtfsFileRecordNumber::RootDirectory as u64 {
            current_record_number
        } else {
            match parent_record_number_of(&dir_file, &mut bridge.reader) {
                Ok(p) => p,
                Err(_) => current_record_number, // fall back; better to show self-ref than error out
            }
        };

    drop(dir_file);

    let source = match bridge.source.as_ref() {
        Some(s) => s,
        None => {
            set_error("no mount source for lazy dir iteration");
            return std::ptr::null_mut();
        }
    };

    let lazy = match LazyDirState::new(source, current_record_number) {
        Ok(s) => s,
        Err(e) => return err_ptr(e),
    };

    let iter = Box::new(FsNtfsDirIter {
        dot: make_dirent(current_record_number, 2, b"."),
        dotdot: make_dirent(parent_record_number, 2, b".."),
        current: make_dirent(0, 0, b""),
        phase: 0,
        skipped_count: 0,
        lazy: Some(Box::new(lazy)),
    });
    Box::into_raw(iter)
}

/// How many index entries were silently skipped during the most recent
/// `fs_ntfs_dir_open` materialisation of `iter`. Returns -1 on a NULL
/// iterator. Skipped entries are typically corrupt rows on a dirty
/// volume; a non-zero value means the listing is incomplete.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_dir_skipped(iter: *const FsNtfsDirIter) -> i64 {
    if iter.is_null() {
        return -1;
    }
    let it = unsafe { &*iter };
    it.skipped_count as i64
}

/// Advance the iterator and return a pointer to the next [`FsNtfsDirent`], or
/// NULL when the listing is exhausted.  The returned pointer is valid until
/// [`fs_ntfs_dir_close`] is called on the same iterator.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_dir_next(iter: *mut FsNtfsDirIter) -> *const FsNtfsDirent {
    if iter.is_null() {
        return std::ptr::null();
    }

    let it = unsafe { &mut *iter };
    loop {
        match it.phase {
            0 => {
                it.phase = 1;
                return &it.dot as *const FsNtfsDirent;
            }
            1 => {
                it.phase = 2;
                return &it.dotdot as *const FsNtfsDirent;
            }
            2 => {
                let lazy = match it.lazy.as_mut() {
                    Some(l) => l,
                    None => {
                        it.phase = 3;
                        return std::ptr::null();
                    }
                };
                match lazy.next_entry() {
                    None => {
                        it.phase = 3;
                        return std::ptr::null();
                    }
                    Some(Err(_)) => {
                        it.skipped_count = it.skipped_count.saturating_add(1);
                        continue;
                    }
                    Some(Ok(entry)) => {
                        let file_name = match entry.key() {
                            Some(Ok(name)) => name,
                            _ => {
                                it.skipped_count = it.skipped_count.saturating_add(1);
                                continue;
                            }
                        };
                        // Skip DOS-only names to avoid duplicates.
                        if file_name.namespace() == NtfsFileNamespace::Dos {
                            continue;
                        }
                        let name_str = file_name.name().to_string_lossy();
                        let name_bytes = name_str.as_bytes();
                        let name_len =
                            std::cmp::min(name_bytes.len(), FS_NTFS_DIRENT_NAME_BYTES - 1) as u16;
                        let mut dirent = FsNtfsDirent {
                            file_record_number: entry.file_reference().file_record_number(),
                            file_type: if file_name.is_directory() { 2 } else { 1 },
                            name_len,
                            name: [0u8; FS_NTFS_DIRENT_NAME_BYTES],
                        };
                        dirent.name[..name_len as usize]
                            .copy_from_slice(&name_bytes[..name_len as usize]);
                        // Store dirent in the lazy state so the pointer remains
                        // valid until the next call.  We use a field on the
                        // iterator itself via a small scratch buffer.
                        it.current = dirent;
                        return &it.current as *const FsNtfsDirent;
                    }
                }
            }
            _ => return std::ptr::null(),
        }
    }
}

/// Free a directory iterator returned by [`fs_ntfs_dir_open`]. Passing NULL
/// is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_dir_close(iter: *mut FsNtfsDirIter) {
    if !iter.is_null() {
        unsafe {
            drop(Box::from_raw(iter));
        }
    }
}

// ---------------------------------------------------------------------------
// File reading
// ---------------------------------------------------------------------------

/// Read up to `length` bytes from the unnamed `$DATA` stream of `path`,
/// starting at byte `offset`, into `buf`.  Returns bytes read on success,
/// `-1` on error (check `fs_ntfs_last_error`).  Sparse holes read as zeroes.
/// Compressed and encrypted files return an error; use the raw-read path for
/// those after the appropriate decompress/decrypt layer.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_read_file(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> i64 {
    if fs.is_null() || path.is_null() || buf.is_null() {
        return -1;
    }

    let bridge = unsafe { &mut *fs };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };

    let file = match navigate_to_path(&bridge.ntfs, &mut bridge.reader, path_str) {
        Ok(f) => f,
        Err(e) => return err_i64(e),
    };

    // Find the unnamed $DATA attribute
    let data_item = match file.data(&mut bridge.reader, "") {
        Some(Ok(item)) => item,
        Some(Err(e)) => {
            set_error(&format!("data attribute: {e}"));
            return -1;
        }
        None => {
            set_error("no data attribute");
            return -1;
        }
    };

    let data_attr = match data_item.to_attribute() {
        Ok(a) => a,
        Err(e) => {
            set_error(&format!("to_attribute: {e}"));
            return -1;
        }
    };

    // §3.2 NTFS LZNT1 compression: upstream `ntfs` 0.4 does not
    // decompress LZNT1, so reading the bytes of a compressed `$DATA`
    // would silently return the raw compressed stream — garbage to
    // the caller. Fail loudly until we ship a real decompressor (see
    // docs/future-features.md §3.2).
    let attr_flags = data_attr.flags();
    if attr_flags.contains(ntfs::NtfsAttributeFlags::COMPRESSED) {
        set_error("file is compressed (LZNT1); decompression not yet supported");
        return -1;
    }
    if attr_flags.contains(ntfs::NtfsAttributeFlags::ENCRYPTED) {
        set_error("file is encrypted ($EFS); decryption not supported");
        return -1;
    }

    // §3.8 WOF (Windows Overlay Filter): a WOF-compressed file's
    // unnamed `$DATA` is empty + sparse and the real bytes live in a
    // `WofCompressedData` ADS, with the file carrying an
    // `IO_REPARSE_TAG_WOF` (0x80000017) `$REPARSE_POINT`. Reading the
    // empty unnamed `$DATA` today would return 0 bytes — also silent
    // data loss. Detect via the reparse tag and fail loudly until we
    // ship XPRESS/LZX decompression (see docs/future-features.md §3.8).
    for attr_res in file.attributes_raw() {
        let a = match attr_res {
            Ok(a) => a,
            Err(_) => continue,
        };
        if a.ty().ok() != Some(NtfsAttributeType::ReparsePoint) {
            continue;
        }
        if let Ok(mut v) = a.value(&mut bridge.reader) {
            let mut tag_buf = [0u8; 4];
            if v.read(&mut bridge.reader, &mut tag_buf).is_ok() {
                let tag = u32::from_le_bytes(tag_buf);
                if tag == 0x8000_0017 {
                    set_error(
                        "file is WOF-compressed (IO_REPARSE_TAG_WOF); decompression not yet supported",
                    );
                    return -1;
                }
            }
        }
        break;
    }

    let mut data_value = match data_attr.value(&mut bridge.reader) {
        Ok(v) => v,
        Err(e) => {
            set_error(&format!("attribute value: {e}"));
            return -1;
        }
    };

    // Seek to offset via upstream's NtfsReadSeek — O(data-runs), not
    // O(offset). The previous read-and-discard loop was quadratic on
    // large pread offsets; this replacement uses the real seek path.
    if offset > 0 {
        if let Err(e) = data_value.seek(&mut bridge.reader, SeekFrom::Start(offset)) {
            set_error(&format!("seek: {e}"));
            return -1;
        }
    }

    // Read data
    let out_buf = unsafe { slice::from_raw_parts_mut(buf as *mut u8, length as usize) };
    let mut total_read = 0usize;

    while total_read < length as usize {
        match data_value.read(&mut bridge.reader, &mut out_buf[total_read..]) {
            Ok(0) => break,
            Ok(n) => total_read += n,
            Err(e) => {
                set_error(&format!("read: {e}"));
                return -1;
            }
        }
    }

    total_read as i64
}

// ---------------------------------------------------------------------------
// Symlink / reparse point reading (stub for now)
// ---------------------------------------------------------------------------

/// Read the symlink target of a reparse-point file into `buf` (NUL-terminated).
/// `bufsize` must include room for the NUL terminator.
/// Returns the number of bytes written (excluding NUL) on success, `-1` on
/// error. The path must refer to a file with a `$REPARSE_POINT` attribute
/// whose tag is `IO_REPARSE_TAG_SYMLINK`; non-symlink reparse tags return -1.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_readlink(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
    buf: *mut c_char,
    bufsize: usize,
) -> c_int {
    if fs.is_null() || path.is_null() || buf.is_null() {
        set_error("fs_ntfs_readlink: null argument");
        return -1;
    }
    let bridge = unsafe { &mut *fs };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_error("fs_ntfs_readlink: non-UTF-8 path");
            return -1;
        }
    };
    let file = match navigate_to_path(&bridge.ntfs, &mut bridge.reader, path_str) {
        Ok(f) => f,
        Err(e) => return err_int(e),
    };
    // Find the $REPARSE_POINT attribute.
    let mut reparse_bytes: Option<Vec<u8>> = None;
    let mut attrs = file.attributes();
    while let Some(item) = attrs.next(&mut bridge.reader) {
        let item = match item {
            Ok(i) => i,
            Err(_) => continue,
        };
        let attribute = match item.to_attribute() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if attribute.ty().ok() != Some(NtfsAttributeType::ReparsePoint) {
            continue;
        }
        let mut value = match attribute.value(&mut bridge.reader) {
            Ok(v) => v,
            Err(e) => {
                set_error(&format!("$REPARSE_POINT value: {e}"));
                return -1;
            }
        };
        let total = attribute.value_length() as usize;
        let mut out = vec![0u8; total];
        let mut filled = 0;
        while filled < total {
            match value.read(&mut bridge.reader, &mut out[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    set_error(&format!("$REPARSE_POINT read: {e}"));
                    return -1;
                }
            }
        }
        out.truncate(filled);
        reparse_bytes = Some(out);
        break;
    }
    let reparse = match reparse_bytes {
        Some(b) => b,
        None => {
            set_error("not a reparse point");
            return -1;
        }
    };
    // Decode tag + tag-specific path.
    if reparse.len() < 8 {
        set_error("reparse data too short");
        return -1;
    }
    let tag = u32::from_le_bytes([reparse[0], reparse[1], reparse[2], reparse[3]]);
    let data_len = u16::from_le_bytes([reparse[4], reparse[5]]) as usize;
    let data_start = 8usize;
    if data_start + data_len > reparse.len() {
        set_error("reparse data_length runs past attribute value");
        return -1;
    }
    let data = &reparse[data_start..data_start + data_len];
    let target = match tag {
        0xA000_000C /* SYMLINK */ => decode_symlink_print_name(data),
        0xA000_0003 /* MOUNT_POINT */ => decode_mount_point_print_name(data),
        other => {
            set_error(&format!("unsupported reparse tag {other:#010x}"));
            return -1;
        }
    };
    let target = match target {
        Some(t) => t,
        None => {
            set_error("reparse print name is empty");
            return -1;
        }
    };
    // Strip NT-path prefix "\\??\\" so POSIX callers see a sensible path.
    let cleaned = target
        .strip_prefix(r"\??\")
        .map(String::from)
        .unwrap_or(target);
    let bytes = cleaned.as_bytes();
    if bytes.len() + 1 > bufsize {
        set_error(&format!(
            "readlink: buffer too small (need {}, have {bufsize})",
            bytes.len() + 1
        ));
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
        *(buf.add(bytes.len())) = 0; // NUL terminator
    }
    bytes.len() as c_int
}

/// Decode the PrintName field from a SymbolicLinkReparseBuffer
/// (MS-FSCC 2.1.2.4). `data` is the tag-specific payload (not including
/// the 8-byte reparse header).
fn decode_symlink_print_name(data: &[u8]) -> Option<String> {
    if data.len() < 12 {
        return None;
    }
    let print_name_offset = u16::from_le_bytes([data[4], data[5]]) as usize;
    let print_name_length = u16::from_le_bytes([data[6], data[7]]) as usize;
    let header = 12usize; // to PathBuffer start
    if header + print_name_offset + print_name_length > data.len() {
        return None;
    }
    let start = header + print_name_offset;
    utf16_le_bytes_to_string(&data[start..start + print_name_length])
}

/// Decode the PrintName field from a MountPointReparseBuffer
/// (MS-FSCC 2.1.2.5). Same layout but no Flags field, so the PathBuffer
/// starts at header offset 8.
fn decode_mount_point_print_name(data: &[u8]) -> Option<String> {
    if data.len() < 8 {
        return None;
    }
    let print_name_offset = u16::from_le_bytes([data[4], data[5]]) as usize;
    let print_name_length = u16::from_le_bytes([data[6], data[7]]) as usize;
    let header = 8usize;
    if header + print_name_offset + print_name_length > data.len() {
        return None;
    }
    let start = header + print_name_offset;
    utf16_le_bytes_to_string(&data[start..start + print_name_length])
}

fn utf16_le_bytes_to_string(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let u16s: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    if u16s.is_empty() {
        return None;
    }
    String::from_utf16(&u16s).ok()
}

// ---------------------------------------------------------------------------
// Recovery / fsck — write operations. Must NOT be called on a mounted volume.
// ---------------------------------------------------------------------------

fn cstr_to_path<'a>(path: *const c_char) -> Option<&'a str> {
    if path.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(path) }.to_str().ok()
}

/// Count free clusters in the volume bitmap. Returns the count on
/// success, `-1` on error. Scans the entire `$Bitmap`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_free_clusters(path: *const c_char) -> i64 {
    cstr_or_return!(path, "fs_ntfs_free_clusters", "path", -1);
    match crate::bitmap::locate_bitmap(std::path::Path::new(path))
        .and_then(|bm| crate::bitmap::count_free(std::path::Path::new(path), &bm))
    {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Count free MFT records in `$MFT:$Bitmap`. Returns the count on
/// success, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mft_free_records(path: *const c_char) -> i64 {
    cstr_or_return!(path, "fs_ntfs_mft_free_records", "path", -1);
    match crate::mft_bitmap::locate(std::path::Path::new(path))
        .and_then(|bm| crate::mft_bitmap::count_free(std::path::Path::new(path), &bm))
    {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Check whether the volume's `VOLUME_IS_DIRTY` flag is set.
/// Returns `1` if dirty, `0` if clean, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_is_dirty(path: *const c_char) -> c_int {
    cstr_or_return!(path, "fs_ntfs_is_dirty", "path", -1);
    match fsck::is_dirty(path) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => err_int(e),
    }
}

/// Clear the `VOLUME_IS_DIRTY` flag on an NTFS image.
///
/// Returns `1` if the flag was set and has been cleared, `0` if the
/// volume was already clean, `-1` on error. Call
/// `fs_ntfs_last_error()` for the error message.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_clear_dirty(path: *const c_char) -> c_int {
    cstr_or_return!(path, "fs_ntfs_clear_dirty", "path", -1);
    match fsck::clear_dirty(path) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => err_int(e),
    }
}

/// Overwrite `$LogFile` with the NTFS "empty log" pattern (all `0xFF`).
///
/// Returns the number of bytes overwritten on success, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_reset_logfile(path: *const c_char) -> i64 {
    cstr_or_return!(path, "fs_ntfs_reset_logfile", "path", -1);
    match fsck::reset_logfile(path) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Set any combination of the four NTFS timestamps on a file. Each time
/// is NT FILETIME (100 ns since 1601-01-01 UTC). Pass `NULL` for any
/// pointer to leave that field unchanged. Non-`NULL` pointers point at
/// an `int64_t` (cast up to u64 for the on-disk write — NTFS FILETIME
/// is unsigned, but we take `int64_t` to match POSIX time APIs).
///
/// Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_set_times(
    image: *const c_char,
    path: *const c_char,
    creation: *const i64,
    modification: *const i64,
    mft_record_modification: *const i64,
    access: *const i64,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_set_times", "image", -1);
    cstr_or_return!(path, "fs_ntfs_set_times", "path", -1);
    let times = write::FileTimes {
        creation: unsafe { creation.as_ref() }.map(|v| *v as u64),
        modification: unsafe { modification.as_ref() }.map(|v| *v as u64),
        mft_record_modification: unsafe { mft_record_modification.as_ref() }.map(|v| *v as u64),
        access: unsafe { access.as_ref() }.map(|v| *v as u64),
    };
    match write::set_times(std::path::Path::new(image), path, times) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Create an empty regular file. `parent_path` is the absolute
/// directory path; `basename` is the new filename (no slashes).
/// Returns the new file's MFT record number on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_create_file(
    image: *const c_char,
    parent_path: *const c_char,
    basename: *const c_char,
) -> i64 {
    cstr_or_return!(image, "fs_ntfs_create_file", "image", -1);
    cstr_or_return!(parent_path, "fs_ntfs_create_file", "parent_path", -1);
    cstr_or_return!(basename, "fs_ntfs_create_file", "basename", -1);
    match write::create_file(std::path::Path::new(image), parent_path, basename) {
        Ok(rn) => rn as i64,
        Err(e) => err_i64(e),
    }
}

/// Upsert a single NTFS Extended Attribute. The EA is stored resident
/// — large EAs that can't fit in the MFT record are rejected (non-
/// resident EA support is future work). Returns 0 on success, -1 on
/// error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_ea(
    image: *const c_char,
    path: *const c_char,
    ea_name: *const c_char,
    value: *const c_void,
    value_len: u64,
    flags: u8,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_write_ea", "image", -1);
    cstr_or_return!(path, "fs_ntfs_write_ea", "path", -1);
    if ea_name.is_null() {
        set_error("fs_ntfs_write_ea: null ea_name");
        return -1;
    }
    let name_bytes = unsafe { CStr::from_ptr(ea_name) }.to_bytes();
    let data: &[u8] = if value_len == 0 {
        &[]
    } else if value.is_null() {
        set_error("fs_ntfs_write_ea: null value with non-zero len");
        return -1;
    } else {
        unsafe { slice::from_raw_parts(value as *const u8, value_len as usize) }
    };
    match write::write_ea(std::path::Path::new(image), path, name_bytes, data, flags) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Remove a single Extended Attribute by name. Returns 0 on success,
/// -1 on error (e.g. not found).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_remove_ea(
    image: *const c_char,
    path: *const c_char,
    ea_name: *const c_char,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_remove_ea", "image", -1);
    cstr_or_return!(path, "fs_ntfs_remove_ea", "path", -1);
    if ea_name.is_null() {
        set_error("fs_ntfs_remove_ea: null ea_name");
        return -1;
    }
    let name_bytes = unsafe { CStr::from_ptr(ea_name) }.to_bytes();
    match write::remove_ea(std::path::Path::new(image), path, name_bytes) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Enumerate the names of every Extended Attribute on the file at
/// `path`. Returns names as a sequence of NUL-terminated byte
/// strings packed into `out_buf` (in on-disk order — EAs are stored
/// in the order they were written). EA names cannot contain NUL by
/// the EA wire format, so the NUL terminator is unambiguous.
///
/// Always writes the required byte count to `*out_total_len` so
/// callers can size-query (pass `out_buf = NULL`, `out_buf_len = 0`).
///
/// Returns:
///   * `N >= 0` — number of EAs (also = count of NUL terminators);
///     0 means the file has no EAs
///   * `-2`     — EAs exist but `out_buf_len` was too small;
///     `*out_total_len` holds the required size and names are NOT written
///   * `-1`     — error (use `fs_ntfs_last_error`)
///
/// `out_total_len` must be non-null. `out_buf` may be NULL only when
/// `out_buf_len == 0`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_list_ea_keys(
    image: *const c_char,
    path: *const c_char,
    out_buf: *mut u8,
    out_buf_len: usize,
    out_total_len: *mut usize,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_list_ea_keys: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_list_ea_keys: null or non-UTF-8 path");
        return -1;
    };
    if out_total_len.is_null() {
        set_error("fs_ntfs_list_ea_keys: null out_total_len");
        return -1;
    }
    if out_buf.is_null() && out_buf_len != 0 {
        set_error("fs_ntfs_list_ea_keys: null out_buf with non-zero out_buf_len");
        return -1;
    }
    match write::list_ea_keys(std::path::Path::new(img), p) {
        Ok(keys) => {
            let total: usize = keys.iter().map(|k| k.len() + 1).sum();
            unsafe { *out_total_len = total };
            if total > out_buf_len {
                return -2;
            }
            let mut cursor = 0usize;
            for key in &keys {
                unsafe {
                    std::ptr::copy_nonoverlapping(key.as_ptr(), out_buf.add(cursor), key.len());
                    *out_buf.add(cursor + key.len()) = 0;
                }
                cursor += key.len() + 1;
            }
            keys.len() as c_int
        }
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Write a resident `$REPARSE_POINT` attribute with `reparse_tag` and
/// `len` bytes of tag-specific data from `buf`. Sets
/// FILE_ATTRIBUTE_REPARSE_POINT on the file. If the file already has a
/// reparse point, it's replaced. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_reparse_point(
    image: *const c_char,
    path: *const c_char,
    reparse_tag: u32,
    buf: *const c_void,
    len: u64,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_write_reparse_point", "image", -1);
    cstr_or_return!(path, "fs_ntfs_write_reparse_point", "path", -1);
    let data: &[u8] = if len == 0 {
        &[]
    } else if buf.is_null() {
        set_error("fs_ntfs_write_reparse_point: null buf with non-zero len");
        return -1;
    } else {
        unsafe { slice::from_raw_parts(buf as *const u8, len as usize) }
    };
    match write::write_reparse_point(std::path::Path::new(image), path, reparse_tag, data) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Remove a file's `$REPARSE_POINT` attribute and clear the reparse
/// flag. Returns 0 on success, -1 on error (e.g. no reparse point).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_remove_reparse_point(image: *const c_char, path: *const c_char) -> c_int {
    cstr_or_return!(image, "fs_ntfs_remove_reparse_point", "image", -1);
    cstr_or_return!(path, "fs_ntfs_remove_reparse_point", "path", -1);
    match write::remove_reparse_point(std::path::Path::new(image), path) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Read a file's `$REPARSE_POINT` attribute as raw `(reparse_tag,
/// data)`. Unlike `fs_ntfs_readlink` which only handles symlinks /
/// mount points, this exposes the raw payload for any reparse type.
///
/// On success: writes the 32-bit reparse tag to `*out_tag`, writes the
/// actual data length to `*out_data_len`, and — if `out_data_len <=
/// out_buf_len` — copies the data bytes into `out_buf[0..out_data_len]`.
/// Always writes `*out_data_len` so the caller can resize on truncation.
///
/// Returns:
///   *  `1` — attribute present, fully copied
///   *  `2` — attribute present but `out_buf_len` was too small (data truncated; `*out_data_len` holds the required length)
///   *  `0` — no `$REPARSE_POINT` attribute on this file
///   * `-1` — error (use `fs_ntfs_last_error`)
///
/// `out_tag` and `out_data_len` must be non-null. `out_buf` may be
/// null only if `out_buf_len == 0` (size-query mode).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_read_reparse_point(
    image: *const c_char,
    path: *const c_char,
    out_tag: *mut u32,
    out_buf: *mut u8,
    out_buf_len: usize,
    out_data_len: *mut usize,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_read_reparse_point: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_read_reparse_point: null or non-UTF-8 path");
        return -1;
    };
    if out_tag.is_null() || out_data_len.is_null() {
        set_error("fs_ntfs_read_reparse_point: null out_tag / out_data_len");
        return -1;
    }
    if out_buf.is_null() && out_buf_len != 0 {
        set_error("fs_ntfs_read_reparse_point: null out_buf with non-zero out_buf_len");
        return -1;
    }
    match write::read_reparse_point(std::path::Path::new(img), p) {
        Ok(Some(rp)) => {
            unsafe {
                *out_tag = rp.reparse_tag;
                *out_data_len = rp.data.len();
            }
            if rp.data.len() > out_buf_len {
                return 2;
            }
            if !rp.data.is_empty() {
                unsafe {
                    std::ptr::copy_nonoverlapping(rp.data.as_ptr(), out_buf, rp.data.len());
                }
            }
            1
        }
        Ok(None) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Create a symlink at `parent_path/basename` pointing to `target`.
/// Set `relative` != 0 for relative-style target paths. Returns the
/// new MFT record number on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_create_symlink(
    image: *const c_char,
    parent_path: *const c_char,
    basename: *const c_char,
    target: *const c_char,
    relative: c_int,
) -> i64 {
    cstr_or_return!(image, "fs_ntfs_create_symlink", "image", -1);
    cstr_or_return!(parent_path, "fs_ntfs_create_symlink", "parent_path", -1);
    cstr_or_return!(basename, "fs_ntfs_create_symlink", "basename", -1);
    cstr_or_return!(target, "fs_ntfs_create_symlink", "target", -1);
    match write::create_symlink(
        std::path::Path::new(image),
        parent_path,
        basename,
        target,
        relative != 0,
    ) {
        Ok(rn) => rn as i64,
        Err(e) => err_i64(e),
    }
}

/// Create or replace a resident named `$DATA` stream (Alternate Data
/// Stream) on the file at `path`. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_named_stream(
    image: *const c_char,
    path: *const c_char,
    stream_name: *const c_char,
    buf: *const c_void,
    len: u64,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_write_named_stream", "image", -1);
    cstr_or_return!(path, "fs_ntfs_write_named_stream", "path", -1);
    cstr_or_return!(stream_name, "fs_ntfs_write_named_stream", "stream_name", -1);
    let data: &[u8] = if len == 0 {
        &[]
    } else if buf.is_null() {
        set_error("fs_ntfs_write_named_stream: null buf with non-zero len");
        return -1;
    } else {
        unsafe { slice::from_raw_parts(buf as *const u8, len as usize) }
    };
    match write::write_named_stream(std::path::Path::new(image), path, stream_name, data) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Enumerate the names of every *named* `$DATA` stream (alternate
/// data streams) on the file at `path`. Excludes the unnamed primary
/// `$DATA`.
///
/// Writes names as a sequence of NUL-terminated UTF-8 strings packed
/// into `out_buf` (in MFT record order, which matches NTFS sort order
/// — names appear sorted by binary $DATA-attribute name). Always
/// writes the actual required byte count to `*out_total_len` so the
/// caller can size-query (pass `out_buf = NULL`, `out_buf_len = 0`).
///
/// Returns:
///   * `N >= 0` — number of named streams (matches the count of
///     NUL terminators in the written buffer); 0 means the file has no ADS
///   * `-2`     — at least one named stream exists but `out_buf_len`
///     was too small; `*out_total_len` holds the required size and names
///     are NOT written
///   * `-1`     — error (use `fs_ntfs_last_error`)
///
/// `out_total_len` must be non-null. `out_buf` may be NULL only if
/// `out_buf_len == 0`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_list_named_streams(
    image: *const c_char,
    path: *const c_char,
    out_buf: *mut c_char,
    out_buf_len: usize,
    out_total_len: *mut usize,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_list_named_streams: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_list_named_streams: null or non-UTF-8 path");
        return -1;
    };
    if out_total_len.is_null() {
        set_error("fs_ntfs_list_named_streams: null out_total_len");
        return -1;
    }
    if out_buf.is_null() && out_buf_len != 0 {
        set_error("fs_ntfs_list_named_streams: null out_buf with non-zero out_buf_len");
        return -1;
    }
    match write::list_named_streams(std::path::Path::new(img), p) {
        Ok(names) => {
            let total: usize = names.iter().map(|n| n.len() + 1).sum();
            unsafe { *out_total_len = total };
            if total > out_buf_len {
                return -2;
            }
            let mut cursor = 0usize;
            for name in &names {
                let bytes = name.as_bytes();
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        (out_buf as *mut u8).add(cursor),
                        bytes.len(),
                    );
                    *(out_buf as *mut u8).add(cursor + bytes.len()) = 0;
                }
                cursor += bytes.len() + 1;
            }
            names.len() as c_int
        }
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Delete a named `$DATA` stream. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_delete_named_stream(
    image: *const c_char,
    path: *const c_char,
    stream_name: *const c_char,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_delete_named_stream", "image", -1);
    cstr_or_return!(path, "fs_ntfs_delete_named_stream", "path", -1);
    cstr_or_return!(
        stream_name,
        "fs_ntfs_delete_named_stream",
        "stream_name",
        -1
    );
    match write::delete_named_stream(std::path::Path::new(image), path, stream_name) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Write `new_data` as the entire contents of the file at `path`.
/// Stays resident if it fits; promotes to non-resident (allocating
/// clusters) if the data exceeds the MFT record's free space.
/// Returns bytes written, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_file_contents(
    image: *const c_char,
    path: *const c_char,
    buf: *const c_void,
    len: u64,
) -> i64 {
    cstr_or_return!(image, "fs_ntfs_write_file_contents", "image", -1);
    cstr_or_return!(path, "fs_ntfs_write_file_contents", "path", -1);
    if len == 0 {
        return match write::write_file_contents(std::path::Path::new(image), path, &[]) {
            Ok(n) => n as i64,
            Err(e) => err_i64(e),
        };
    }
    if buf.is_null() {
        set_error("fs_ntfs_write_file_contents: null buf with non-zero len");
        return -1;
    }
    let data = unsafe { slice::from_raw_parts(buf as *const u8, len as usize) };
    match write::write_file_contents(std::path::Path::new(image), path, data) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Delete an empty directory. Returns 0 on success, -1 on error.
/// Fails if the directory is non-empty or has `$INDEX_ALLOCATION`
/// overflow (for MVP).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_rmdir(image: *const c_char, path: *const c_char) -> c_int {
    cstr_or_return!(image, "fs_ntfs_rmdir", "image", -1);
    cstr_or_return!(path, "fs_ntfs_rmdir", "path", -1);
    match write::rmdir(std::path::Path::new(image), path) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Create a new empty directory. Returns the new MFT record number on
/// success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mkdir(
    image: *const c_char,
    parent_path: *const c_char,
    basename: *const c_char,
) -> i64 {
    cstr_or_return!(image, "fs_ntfs_mkdir", "image", -1);
    cstr_or_return!(parent_path, "fs_ntfs_mkdir", "parent_path", -1);
    cstr_or_return!(basename, "fs_ntfs_mkdir", "basename", -1);
    match write::mkdir(std::path::Path::new(image), parent_path, basename) {
        Ok(rn) => rn as i64,
        Err(e) => err_i64(e),
    }
}

/// Write `new_data` as the full content of the file's unnamed `$DATA`
/// attribute while it remains resident. Works only if the new length
/// fits in the file's MFT record — larger writes require W2.2
/// promotion to non-resident. Returns bytes written, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_resident_contents(
    image: *const c_char,
    path: *const c_char,
    buf: *const c_void,
    len: u64,
) -> i64 {
    cstr_or_return!(image, "fs_ntfs_write_resident_contents", "image", -1);
    cstr_or_return!(path, "fs_ntfs_write_resident_contents", "path", -1);
    if len == 0 {
        return match write::write_resident_contents(std::path::Path::new(image), path, &[]) {
            Ok(n) => n as i64,
            Err(e) => err_i64(e),
        };
    }
    if buf.is_null() {
        set_error("fs_ntfs_write_resident_contents: null buf with non-zero len");
        return -1;
    }
    let data = unsafe { slice::from_raw_parts(buf as *const u8, len as usize) };
    match write::write_resident_contents(std::path::Path::new(image), path, data) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Delete a regular file. Refuses directories. Returns 0 on success,
/// -1 on error. On success the file's data-run clusters and MFT
/// record are freed.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_unlink(image: *const c_char, path: *const c_char) -> c_int {
    cstr_or_return!(image, "fs_ntfs_unlink", "image", -1);
    cstr_or_return!(path, "fs_ntfs_unlink", "path", -1);
    match write::unlink(std::path::Path::new(image), path) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Read a file's 16-byte `$OBJECT_ID` into `out_buf[0..16]`. Returns
/// `1` if the file has an object ID (buffer filled), `0` if it does
/// not, `-1` on error. `out_buf` must be at least 16 bytes.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_read_object_id(
    image: *const c_char,
    path: *const c_char,
    out_buf: *mut u8,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_read_object_id", "image", -1);
    cstr_or_return!(path, "fs_ntfs_read_object_id", "path", -1);
    if out_buf.is_null() {
        set_error("fs_ntfs_read_object_id: null out_buf");
        return -1;
    }
    match write::read_object_id(std::path::Path::new(image), path) {
        Ok(Some(guid)) => {
            unsafe { std::ptr::copy_nonoverlapping(guid.as_ptr(), out_buf, 16) };
            1
        }
        Ok(None) => 0,
        Err(e) => err_int(e),
    }
}

/// Write a file's 16-byte `$OBJECT_ID` from `in_buf[0..16]`. Adds the
/// attribute if absent, replaces in place if present. Returns 0 on
/// success, -1 on error. `in_buf` must be at least 16 bytes.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_object_id(
    image: *const c_char,
    path: *const c_char,
    in_buf: *const u8,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_object_id: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_object_id: null or non-UTF-8 path");
        return -1;
    };
    if in_buf.is_null() {
        set_error("fs_ntfs_write_object_id: null in_buf");
        return -1;
    }
    let mut object_id = [0u8; 16];
    unsafe { std::ptr::copy_nonoverlapping(in_buf, object_id.as_mut_ptr(), 16) };
    match write::write_object_id(std::path::Path::new(img), p, &object_id) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Write the 64-byte extended `$OBJECT_ID` carrying the mandatory
/// `object_id` (16 bytes from `in_buf`) plus the three optional DLT
/// Birth GUIDs (16 bytes each from `birth_volume`, `birth_object`,
/// `birth_domain`). All four pointers must be non-null and reference
/// at least 16 readable bytes. Adds the attribute if absent,
/// replaces in place if present. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_object_id_extended(
    image: *const c_char,
    path: *const c_char,
    in_buf: *const u8,
    birth_volume: *const u8,
    birth_object: *const u8,
    birth_domain: *const u8,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_object_id_extended: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_object_id_extended: null or non-UTF-8 path");
        return -1;
    };
    if in_buf.is_null()
        || birth_volume.is_null()
        || birth_object.is_null()
        || birth_domain.is_null()
    {
        set_error("fs_ntfs_write_object_id_extended: null GUID pointer");
        return -1;
    }
    let mut object_id = [0u8; 16];
    let mut bv = [0u8; 16];
    let mut bo = [0u8; 16];
    let mut bd = [0u8; 16];
    unsafe {
        std::ptr::copy_nonoverlapping(in_buf, object_id.as_mut_ptr(), 16);
        std::ptr::copy_nonoverlapping(birth_volume, bv.as_mut_ptr(), 16);
        std::ptr::copy_nonoverlapping(birth_object, bo.as_mut_ptr(), 16);
        std::ptr::copy_nonoverlapping(birth_domain, bd.as_mut_ptr(), 16);
    }
    match write::write_object_id_extended(std::path::Path::new(img), p, &object_id, &bv, &bo, &bd) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Read the full `$OBJECT_ID` attribute into `out_buf`. Caller passes
/// the buffer length in `out_buf_len` (must be ≥ 16; pass 64 to also
/// receive the Birth GUIDs when present). On success, returns the
/// number of bytes written (16 or 64); on absent attribute, 0; on
/// error, -1.
///
/// If `out_buf_len < 64` but the on-disk attribute is the 64-byte
/// extended form, only the first 16 bytes (`object_id`) are written
/// and 16 is returned — the Birth GUIDs are silently truncated.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_read_object_id_extended(
    image: *const c_char,
    path: *const c_char,
    out_buf: *mut u8,
    out_buf_len: usize,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_read_object_id_extended: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_read_object_id_extended: null or non-UTF-8 path");
        return -1;
    };
    if out_buf.is_null() {
        set_error("fs_ntfs_read_object_id_extended: null out_buf");
        return -1;
    }
    if out_buf_len < 16 {
        set_error("fs_ntfs_read_object_id_extended: out_buf_len < 16");
        return -1;
    }
    match write::read_object_id_extended(std::path::Path::new(img), p) {
        Ok(Some(ext)) => {
            unsafe { std::ptr::copy_nonoverlapping(ext.object_id.as_ptr(), out_buf, 16) };
            if let Some((bv, bo, bd)) = ext.birth_ids {
                if out_buf_len >= 64 {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bv.as_ptr(), out_buf.add(16), 16);
                        std::ptr::copy_nonoverlapping(bo.as_ptr(), out_buf.add(32), 16);
                        std::ptr::copy_nonoverlapping(bd.as_ptr(), out_buf.add(48), 16);
                    }
                    64
                } else {
                    16
                }
            } else {
                16
            }
        }
        Ok(None) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Remove a file's `$OBJECT_ID` attribute. Idempotent: returns 0
/// whether or not the attribute was present beforehand. Returns -1
/// on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_remove_object_id(image: *const c_char, path: *const c_char) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_remove_object_id: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_remove_object_id: null or non-UTF-8 path");
        return -1;
    };
    match write::remove_object_id(std::path::Path::new(img), p) {
        Ok(_removed) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Add a hard link `new_parent_path/new_basename` pointing at the
/// same file as `existing_path`. Returns 0 on success, -1 on error.
/// Refuses directories. The target file's hard-link count is
/// incremented and a new `$FILE_NAME` attribute is appended to its
/// MFT record; an index entry is inserted in `new_parent_path`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_link(
    image: *const c_char,
    existing_path: *const c_char,
    new_parent_path: *const c_char,
    new_basename: *const c_char,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_link", "image", -1);
    cstr_or_return!(existing_path, "fs_ntfs_link", "existing_path", -1);
    cstr_or_return!(new_parent_path, "fs_ntfs_link", "new_parent_path", -1);
    cstr_or_return!(new_basename, "fs_ntfs_link", "new_basename", -1);
    match write::link(
        std::path::Path::new(image),
        existing_path,
        new_parent_path,
        new_basename,
    ) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Rename a file (variable length). Returns 0 on success, -1 on error.
/// Handles both same-length (delegates to the fast path, incl.
/// `$INDEX_ALLOCATION` parents) and length-changing renames
/// (resident-`$INDEX_ROOT` parents only in this MVP).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_rename(
    image: *const c_char,
    old_path: *const c_char,
    new_basename: *const c_char,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_rename", "image", -1);
    cstr_or_return!(old_path, "fs_ntfs_rename", "old_path", -1);
    cstr_or_return!(new_basename, "fs_ntfs_rename", "new_basename", -1);
    match write::rename(std::path::Path::new(image), old_path, new_basename) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Rename a file in place. `new_name` is the new basename (no `/`).
/// Requires the new name have the same UTF-16 length as the current
/// name. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_rename_same_length(
    image: *const c_char,
    old_path: *const c_char,
    new_name: *const c_char,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_rename_same_length", "image", -1);
    cstr_or_return!(old_path, "fs_ntfs_rename_same_length", "old_path", -1);
    cstr_or_return!(new_name, "fs_ntfs_rename_same_length", "new_name", -1);
    match write::rename_same_length(std::path::Path::new(image), old_path, new_name) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Grow a non-resident `$DATA` to `new_size` bytes. Allocates the
/// needed contiguous clusters from `$Bitmap`. Fails if the volume
/// doesn't have enough contiguous free space, or if the new
/// mapping-pairs don't fit in the existing attribute header
/// (attribute resize is separate future work).
///
/// Returns the new size on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_grow(image: *const c_char, path: *const c_char, new_size: u64) -> i64 {
    cstr_or_return!(image, "fs_ntfs_grow", "image", -1);
    cstr_or_return!(path, "fs_ntfs_grow", "path", -1);
    match write::grow_nonresident(std::path::Path::new(image), path, new_size) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Shrink a file's non-resident `$DATA` to `new_size` bytes. Clusters
/// past the new end are freed. Growing is not supported in W2; will
/// return -1 if `new_size > current_size`. Returns the new size on
/// success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_truncate(
    image: *const c_char,
    path: *const c_char,
    new_size: u64,
) -> i64 {
    cstr_or_return!(image, "fs_ntfs_truncate", "image", -1);
    cstr_or_return!(path, "fs_ntfs_truncate", "path", -1);
    match write::truncate(std::path::Path::new(image), path, new_size) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Rewrite `len` bytes at `offset` within an existing non-resident
/// `$DATA` attribute. Does not extend the file, does not touch sparse
/// or compressed ranges. Returns bytes written on success, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_file(
    image: *const c_char,
    path: *const c_char,
    offset: u64,
    buf: *const c_void,
    len: u64,
) -> i64 {
    cstr_or_return!(image, "fs_ntfs_write_file", "image", -1);
    cstr_or_return!(path, "fs_ntfs_write_file", "path", -1);
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        set_error("fs_ntfs_write_file: null buffer with non-zero length");
        return -1;
    }
    let data = unsafe { slice::from_raw_parts(buf as *const u8, len as usize) };
    match write::write_at(std::path::Path::new(image), path, offset, data) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Read the file's `$STANDARD_INFORMATION.security_id` (the index into
/// `$Secure:$SDS` / `$Secure:$SII`). Writes the 32-bit value to `*out`.
/// Returns:
///    1  — security_id read into `*out`
///    0  — file's $STANDARD_INFORMATION is the 48-byte v1.x form (no
///         security_id field). `*out` is set to 0.
///   -1  — error
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_read_security_id(
    image: *const c_char,
    path: *const c_char,
    out: *mut u32,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_read_security_id: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_read_security_id: null or non-UTF-8 path");
        return -1;
    };
    if out.is_null() {
        set_error("fs_ntfs_read_security_id: null out");
        return -1;
    }
    match write::read_security_id(std::path::Path::new(img), fp) {
        Ok(Some(id)) => {
            unsafe { *out = id };
            1
        }
        Ok(None) => {
            unsafe { *out = 0 };
            0
        }
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Full `$STANDARD_INFORMATION` value (MS-FSCC §2.4.2). The four
/// timestamps are NT 100-nanosecond intervals since 1601-01-01 UTC.
/// `file_attributes` is the FILE_ATTRIBUTE_* bitmask. The trailing
/// `owner_id` / `security_id` / `quota` / `usn` fields only have
/// meaning when `has_v3 != 0` (the 72-byte 3.x form); the 48-byte
/// 1.x form leaves them zeroed.
#[repr(C)]
pub struct FsNtfsStandardInfo {
    pub creation_time: u64,
    pub modification_time: u64,
    pub mft_modification_time: u64,
    pub access_time: u64,
    pub file_attributes: u32,
    pub maximum_versions: u32,
    pub version_number: u32,
    pub class_id: u32,
    pub owner_id: u32,
    pub security_id: u32,
    pub quota: u64,
    pub usn: u64,
    /// 1 iff this image's $STANDARD_INFORMATION was the 72-byte 3.x
    /// form (owner_id/security_id/quota/usn carry decoded values);
    /// 0 for the 48-byte 1.x form (those four fields are zero).
    pub has_v3: u8,
    _pad: [u8; 7],
}

/// Read every field of a file's `$STANDARD_INFORMATION`. Unlike the
/// targeted `fs_ntfs_read_security_id`, this exposes the full common
/// header plus the optional NTFS 3.x trailer (when present).
///
/// Returns 0 on success, -1 on error. `out` must be non-null and
/// large enough for `FsNtfsStandardInfo`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_read_si_full(
    image: *const c_char,
    path: *const c_char,
    out: *mut FsNtfsStandardInfo,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_read_si_full: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_read_si_full: null or non-UTF-8 path");
        return -1;
    };
    if out.is_null() {
        set_error("fs_ntfs_read_si_full: null out");
        return -1;
    }
    match write::read_si_full(std::path::Path::new(img), p) {
        Ok(si) => {
            let out_ref = unsafe { &mut *out };
            out_ref.creation_time = si.creation_time;
            out_ref.modification_time = si.modification_time;
            out_ref.mft_modification_time = si.mft_modification_time;
            out_ref.access_time = si.access_time;
            out_ref.file_attributes = si.file_attributes;
            out_ref.maximum_versions = si.maximum_versions;
            out_ref.version_number = si.version_number;
            out_ref.class_id = si.class_id;
            if let Some(v3) = si.v3 {
                out_ref.owner_id = v3.owner_id;
                out_ref.security_id = v3.security_id;
                out_ref.quota = v3.quota;
                out_ref.usn = v3.usn;
                out_ref.has_v3 = 1;
            } else {
                out_ref.owner_id = 0;
                out_ref.security_id = 0;
                out_ref.quota = 0;
                out_ref.usn = 0;
                out_ref.has_v3 = 0;
            }
            out_ref._pad = [0u8; 7];
            0
        }
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Point a file at an existing `$Secure:$SDS` entry by writing the
/// `security_id` field in its `$STANDARD_INFORMATION`. mkfs ships
/// the canonical system-files DACL at id `0x100`; pointing a runtime-
/// created file there grants the same ACL. Adding new SD entries is
/// a separate (larger) piece of work — this writer only retargets.
///
/// Requires the file's $STANDARD_INFORMATION to be in the 72-byte
/// NTFS 3.x form. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_set_security_id(
    image: *const c_char,
    path: *const c_char,
    security_id: u32,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_set_security_id: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_set_security_id: null or non-UTF-8 path");
        return -1;
    };
    match write::set_security_id(std::path::Path::new(img), fp, security_id) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Add / remove bits in `$STANDARD_INFORMATION.file_attributes`. Bits in
/// `add_flags` are ORed on; bits in `remove_flags` are ANDed off.
/// Overlap is rejected. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_set_file_attributes(
    image: *const c_char,
    path: *const c_char,
    add_flags: u32,
    remove_flags: u32,
) -> c_int {
    cstr_or_return!(image, "fs_ntfs_set_file_attributes", "image", -1);
    cstr_or_return!(path, "fs_ntfs_set_file_attributes", "path", -1);
    match write::set_file_attributes(
        std::path::Path::new(image),
        path,
        write::FileAttributesChange {
            add: add_flags,
            remove: remove_flags,
        },
    ) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

// ---------------------------------------------------------------------------
// Handle-based mutation siblings (`_h`)
//
// These mirror the path-based mutators above but take an already-mounted
// `*mut FsNtfsHandle` instead of a `const char *image`. They construct a
// fresh `BlockIo` impl from the handle's recorded `MountSource` for the
// duration of one call — for path-mounted handles this means a per-call
// `OpenOptions::read.write.open` (kernel page-cache amortizes); for
// callback-mounted handles it wraps the existing `(read_fn, write_fn,
// context, size)` tuple. Sandboxed FSKit hosts can only use the
// callback path — the path-based siblings will fail under the FSKit
// sandbox because they re-open `/dev/diskN`.
//
// Callback-mounted handles must have been mounted with a non-NULL
// `cfg.write`; otherwise `_h` mutators return -1 with EINVAL-flavored
// error text.
// ---------------------------------------------------------------------------

/// Convert a handle pointer to a mutable reference and a fresh
/// `HandleIo`. On any failure sets the thread-local error and returns
/// `Err(())` — caller returns `-1`.
fn handle_io_from_ptr(fs: *mut FsNtfsHandle) -> Result<HandleIo, ()> {
    if fs.is_null() {
        set_error("null fs handle");
        return Err(());
    }
    let handle = unsafe { &*fs };
    handle_to_rw_io(handle).map_err(|e| {
        set_error(&e);
    })
}

/// Handle-based [`fs_ntfs_create_file`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_create_file_h(
    fs: *mut FsNtfsHandle,
    parent_path: *const c_char,
    basename: *const c_char,
) -> i64 {
    cstr_or_return!(parent_path, "fs_ntfs_create_file_h", "parent_path", -1);
    cstr_or_return!(basename, "fs_ntfs_create_file_h", "basename", -1);
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::create_file_io(&mut io, parent_path, basename) {
        Ok(rn) => rn as i64,
        Err(e) => err_i64(e),
    }
}

/// Handle-based [`fs_ntfs_write_file_contents`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_write_file_contents_h(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
    buf: *const c_void,
    len: u64,
) -> i64 {
    cstr_or_return!(path, "fs_ntfs_write_file_contents_h", "path", -1);
    let data: &[u8] = if len == 0 {
        &[]
    } else if buf.is_null() {
        set_error("fs_ntfs_write_file_contents_h: null buf with non-zero len");
        return -1;
    } else {
        unsafe { slice::from_raw_parts(buf as *const u8, len as usize) }
    };
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::write_file_contents_io(&mut io, path, data) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Handle-based [`fs_ntfs_unlink`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_unlink_h(fs: *mut FsNtfsHandle, path: *const c_char) -> c_int {
    cstr_or_return!(path, "fs_ntfs_unlink_h", "path", -1);
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::unlink_io(&mut io, path) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Handle-based [`fs_ntfs_rename`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_rename_h(
    fs: *mut FsNtfsHandle,
    old_path: *const c_char,
    new_basename: *const c_char,
) -> c_int {
    cstr_or_return!(old_path, "fs_ntfs_rename_h", "old_path", -1);
    cstr_or_return!(new_basename, "fs_ntfs_rename_h", "new_basename", -1);
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::rename_io(&mut io, old_path, new_basename) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Handle-based [`fs_ntfs_mkdir`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mkdir_h(
    fs: *mut FsNtfsHandle,
    parent_path: *const c_char,
    basename: *const c_char,
) -> i64 {
    cstr_or_return!(parent_path, "fs_ntfs_mkdir_h", "parent_path", -1);
    cstr_or_return!(basename, "fs_ntfs_mkdir_h", "basename", -1);
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::mkdir_io(&mut io, parent_path, basename) {
        Ok(rn) => rn as i64,
        Err(e) => err_i64(e),
    }
}

/// Handle-based [`fs_ntfs_rmdir`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_rmdir_h(fs: *mut FsNtfsHandle, path: *const c_char) -> c_int {
    cstr_or_return!(path, "fs_ntfs_rmdir_h", "path", -1);
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::rmdir_io(&mut io, path) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Handle-based [`fs_ntfs_truncate`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_truncate_h(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
    new_size: u64,
) -> i64 {
    cstr_or_return!(path, "fs_ntfs_truncate_h", "path", -1);
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::truncate_io(&mut io, path, new_size) {
        Ok(n) => n as i64,
        Err(e) => err_i64(e),
    }
}

/// Handle-based [`fs_ntfs_set_times`].
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_set_times_h(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
    creation: *const i64,
    modification: *const i64,
    mft_record_modification: *const i64,
    access: *const i64,
) -> c_int {
    cstr_or_return!(path, "fs_ntfs_set_times_h", "path", -1);
    let times = write::FileTimes {
        creation: unsafe { creation.as_ref() }.map(|v| *v as u64),
        modification: unsafe { modification.as_ref() }.map(|v| *v as u64),
        mft_record_modification: unsafe { mft_record_modification.as_ref() }.map(|v| *v as u64),
        access: unsafe { access.as_ref() }.map(|v| *v as u64),
    };
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::set_times_io(&mut io, path, times) {
        Ok(()) => 0,
        Err(e) => err_int(e),
    }
}

/// Handle-based sibling of [`fs_ntfs_write_object_id_extended`]. Writes
/// the 64-byte extended `$OBJECT_ID` carrying the mandatory `object_id`
/// (16 bytes from `in_buf`) plus the three optional DLT Birth GUIDs
/// (16 bytes each from `birth_volume`, `birth_object`, `birth_domain`).
///
/// All four GUID pointers must be non-null and reference at least 16
/// readable bytes. Adds the attribute if absent, replaces in place if
/// present. Returns 0 on success, -1 on error.
///
/// Use this when you already hold an open filesystem handle (mounted
/// via the callback interface) and would rather not pay the cost of
/// re-opening the underlying image on every call.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_set_object_id_extended_h(
    fs: *mut FsNtfsHandle,
    path: *const c_char,
    in_buf: *const u8,
    birth_volume: *const u8,
    birth_object: *const u8,
    birth_domain: *const u8,
) -> c_int {
    if fs.is_null() {
        set_error("fs_ntfs_set_object_id_extended_h: null handle");
        return -1;
    }
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_set_object_id_extended_h: null or non-UTF-8 path");
        return -1;
    };
    if in_buf.is_null()
        || birth_volume.is_null()
        || birth_object.is_null()
        || birth_domain.is_null()
    {
        set_error("fs_ntfs_set_object_id_extended_h: null GUID pointer");
        return -1;
    }
    let mut object_id = [0u8; 16];
    let mut bv = [0u8; 16];
    let mut bo = [0u8; 16];
    let mut bd = [0u8; 16];
    unsafe {
        std::ptr::copy_nonoverlapping(in_buf, object_id.as_mut_ptr(), 16);
        std::ptr::copy_nonoverlapping(birth_volume, bv.as_mut_ptr(), 16);
        std::ptr::copy_nonoverlapping(birth_object, bo.as_mut_ptr(), 16);
        std::ptr::copy_nonoverlapping(birth_domain, bd.as_mut_ptr(), 16);
    }
    let Ok(mut io) = handle_io_from_ptr(fs) else {
        return -1;
    };
    match write::write_object_id_extended_io(&mut io, fp, &object_id, &bv, &bo, &bd) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Combined recovery: reset `$LogFile` and clear the dirty flag.
///
/// Optional out-params report what the call did:
/// * `out_logfile_bytes`: bytes of `$LogFile` overwritten (non-null to receive)
/// * `out_dirty_cleared`: `1` if the dirty flag was found set and cleared,
///   `0` if the volume was already clean (non-null to receive)
///
/// Returns `0` on success, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_fsck(
    path: *const c_char,
    out_logfile_bytes: *mut u64,
    out_dirty_cleared: *mut u8,
) -> c_int {
    cstr_or_return!(path, "fs_ntfs_fsck", "path", -1);
    match fsck::fsck(path) {
        Ok(report) => {
            if !out_logfile_bytes.is_null() {
                unsafe { *out_logfile_bytes = report.logfile_bytes };
            }
            if !out_dirty_cleared.is_null() {
                unsafe { *out_dirty_cleared = u8::from(report.dirty_cleared) };
            }
            0
        }
        Err(e) => err_int(e),
    }
}

// ---------------------------------------------------------------------------
// Callback-based fsck (v0.1.1)
// ---------------------------------------------------------------------------

/// `FsckIo` backed by the read/write callback pair on
/// [`FsNtfsBlockdevCfg`]. `size` is taken from the config's `size_bytes`.
///
/// Safety: the context pointer must remain valid for the duration of
/// any fsck call. That's the same contract FSKit / the Go backend
/// already honor for `fs_ntfs_mount_with_callbacks`.
struct CallbackIo {
    read_fn: ReadCallback,
    write_fn: WriteCallback,
    context: *mut c_void,
    size: u64,
}

unsafe impl Send for CallbackIo {}

impl fsck::FsckIo for CallbackIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let rc = unsafe {
            (self.read_fn)(
                self.context,
                buf.as_mut_ptr() as *mut c_void,
                offset,
                buf.len() as u64,
            )
        };
        if rc != 0 {
            return Err(format!(
                "read callback failed: rc={rc} @{offset} len={}",
                buf.len()
            ));
        }
        Ok(())
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        let rc = unsafe {
            (self.write_fn)(
                self.context,
                buf.as_ptr() as *const c_void,
                offset,
                buf.len() as u64,
            )
        };
        if rc != 0 {
            return Err(format!(
                "write callback failed: rc={rc} @{offset} len={}",
                buf.len()
            ));
        }
        Ok(())
    }

    fn size(&self) -> u64 {
        self.size
    }

    // `sync` is a no-op: the host (FSKit / Go backend) owns the
    // underlying file handle and is responsible for barrier semantics.
}

/// Check the dirty flag over a callback-based block device. Only the
/// `read` callback is used; `write` may be `NULL`. Returns `1` if dirty,
/// `0` if clean, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_is_dirty_with_callbacks(cfg: *const FsNtfsBlockdevCfg) -> c_int {
    if cfg.is_null() {
        set_error("fs_ntfs_is_dirty_with_callbacks: null config");
        return -1;
    }
    let cfg = unsafe { &*cfg };

    // The write callback is never invoked by `is_dirty`; wire up a
    // never-called stub so the struct stays total. We picked an unsafe
    // extern "C" fn that returns EIO if somehow invoked — defense in
    // depth against a future caller accidentally reaching through it.
    unsafe extern "C" fn write_stub(
        _ctx: *mut c_void,
        _buf: *const c_void,
        _off: u64,
        _len: u64,
    ) -> c_int {
        5 /* EIO */
    }

    let mut io = CallbackIo {
        read_fn: cfg.read,
        write_fn: cfg.write.unwrap_or(write_stub),
        context: cfg.context,
        size: cfg.size_bytes,
    };

    match fsck::is_dirty_io(&mut io) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Progress callback matching the `fs_ntfs_fsck_progress_fn` C typedef.
type FsckProgressCallback = unsafe extern "C" fn(*mut c_void, *const c_char, u64, u64) -> c_int;

/// Combined recovery via callbacks: reset `$LogFile` + clear the dirty
/// bit. Requires both `cfg->read` and `cfg->write` to be set. Emits
/// progress via `progress_cb` when non-NULL. Returns `0` on success,
/// `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_fsck_with_callbacks(
    cfg: *const FsNtfsBlockdevCfg,
    progress_cb: Option<FsckProgressCallback>,
    progress_ctx: *mut c_void,
    out_logfile_bytes: *mut u64,
    out_dirty_cleared: *mut u8,
) -> c_int {
    if cfg.is_null() {
        set_error("fs_ntfs_fsck_with_callbacks: null config");
        return -1;
    }
    let cfg = unsafe { &*cfg };

    let Some(write_fn) = cfg.write else {
        set_error("fs_ntfs_fsck_with_callbacks: cfg.write is NULL (fsck requires RW)");
        return -1;
    };

    let mut io = CallbackIo {
        read_fn: cfg.read,
        write_fn,
        context: cfg.context,
        size: cfg.size_bytes,
    };

    // Adapter: Rust closure → C function pointer. We need a single
    // CString per phase transition so the `*const c_char` stays valid
    // for the duration of the callback invocation. Done by allocating
    // inside the closure (one CString per emission); acceptable
    // overhead given progress fires at most a few hundred times per
    // fsck.
    //
    // SendCtx: `*mut c_void` isn't Send, but the closure is kept local
    // to this stack frame and never crosses a thread boundary, so we
    // don't need Send. The callback fn pointer + ctx are captured by
    // move into the closure.
    struct PtrWrap(*mut c_void);
    let pctx = PtrWrap(progress_ctx);
    let pcb = progress_cb;

    let mut progress_closure = move |phase: &str, done: u64, total: u64| {
        if let Some(cb) = pcb {
            if let Ok(cstr) = CString::new(phase) {
                let _ = unsafe { cb(pctx.0, cstr.as_ptr(), done, total) };
            }
        }
    };

    #[allow(clippy::type_complexity)]
    let progress: Option<&mut (dyn FnMut(&str, u64, u64) + '_)> = if pcb.is_some() {
        Some(&mut progress_closure)
    } else {
        None
    };

    match fsck::fsck_io(&mut io, progress) {
        Ok(report) => {
            if !out_logfile_bytes.is_null() {
                unsafe { *out_logfile_bytes = report.logfile_bytes };
            }
            if !out_dirty_cleared.is_null() {
                unsafe { *out_dirty_cleared = u8::from(report.dirty_cleared) };
            }
            0
        }
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// `fs_core` counterpart of [`fs_ntfs_is_dirty_with_callbacks`]. Reads
/// the dirty flag through an `FsCoreDevice` handle from a sister crate
/// (`qcow2_open_rw_on_device`, `partitions_open_slice`,
/// `fs_core_file_open`, ...). Returns `1` if dirty, `0` if clean,
/// `-1` on error.
///
/// The handle is borrowed (its inner `Arc` is cloned for the duration
/// of the call). The caller still owns the handle and frees it via
/// `fs_core_device_close`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_is_dirty_with_fs_core_device(
    handle: *mut fs_core::ffi::FsCoreDevice,
) -> c_int {
    if handle.is_null() {
        set_error("fs_ntfs_is_dirty_with_fs_core_device: null handle");
        return -1;
    }
    // Safety: handle non-null per the check above; caller-owned per the
    // doc contract. The Arc clone keeps the device alive through the
    // call independent of any caller-side close.
    let device = unsafe { (*handle).inner().clone() };
    let size = fs_core::BlockRead::size_bytes(&device);
    let mut io = FsCoreBlockIo { device, size };

    match fsck::is_dirty_io(&mut io) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// `fs_core` counterpart of [`fs_ntfs_fsck_with_callbacks`]. Replays
/// `$LogFile` and clears the dirty bit through an `FsCoreDevice`
/// handle. The device must report `is_writable() == true`; otherwise
/// the call fails up front.
///
/// On success `out_logfile_bytes` (if non-NULL) receives the byte
/// count overwritten in `$LogFile` during recovery, and
/// `out_dirty_cleared` (if non-NULL) is set to `1` if the dirty bit
/// was actually cleared (meaning the volume was dirty before the
/// call). Returns `0` on success, `-1` on error.
///
/// `progress_cb` (if non-NULL) is invoked from the calling thread as
/// fsck advances; the `phase` C string is owned by fs_ntfs and only
/// valid for the duration of the callback.
///
/// The handle is borrowed (its inner `Arc` is cloned for the duration
/// of the call). The caller still owns the handle and frees it via
/// `fs_core_device_close`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_fsck_with_fs_core_device(
    handle: *mut fs_core::ffi::FsCoreDevice,
    progress_cb: Option<FsckProgressCallback>,
    progress_ctx: *mut c_void,
    out_logfile_bytes: *mut u64,
    out_dirty_cleared: *mut u8,
) -> c_int {
    if handle.is_null() {
        set_error("fs_ntfs_fsck_with_fs_core_device: null handle");
        return -1;
    }
    let device = unsafe { (*handle).inner().clone() };
    if !fs_core::BlockDevice::is_writable(&device) {
        set_error("fs_ntfs_fsck_with_fs_core_device: device is not writable (fsck requires RW)");
        return -1;
    }
    let size = fs_core::BlockRead::size_bytes(&device);
    let mut io = FsCoreBlockIo { device, size };

    // Same closure-to-fn-pointer adapter as fs_ntfs_fsck_with_callbacks
    // — see that function for the lifetime / Send rationale.
    struct PtrWrap(*mut c_void);
    let pctx = PtrWrap(progress_ctx);
    let pcb = progress_cb;
    let mut progress_closure = move |phase: &str, done: u64, total: u64| {
        if let Some(cb) = pcb {
            if let Ok(cstr) = CString::new(phase) {
                let _ = unsafe { cb(pctx.0, cstr.as_ptr(), done, total) };
            }
        }
    };
    #[allow(clippy::type_complexity)]
    let progress: Option<&mut (dyn FnMut(&str, u64, u64) + '_)> = if pcb.is_some() {
        Some(&mut progress_closure)
    } else {
        None
    };

    match fsck::fsck_io(&mut io, progress) {
        Ok(report) => {
            if !out_logfile_bytes.is_null() {
                unsafe { *out_logfile_bytes = report.logfile_bytes };
            }
            if !out_dirty_cleared.is_null() {
                unsafe { *out_dirty_cleared = u8::from(report.dirty_cleared) };
            }
            0
        }
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// mkfs (volume formatter)
// ---------------------------------------------------------------------------

/// Format an NTFS filesystem on the device backed by `cfg`. Both
/// `cfg->read` and `cfg->write` must be set. Picks a default 4 KiB
/// cluster size and 4096-byte MFT records, no volume label, random
/// serial. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mkfs(cfg: *const FsNtfsBlockdevCfg) -> c_int {
    if cfg.is_null() {
        set_error("fs_ntfs_mkfs: null config");
        return -1;
    }
    let cfg = unsafe { &*cfg };
    let Some(write_fn) = cfg.write else {
        set_error("fs_ntfs_mkfs: cfg.write is NULL (mkfs requires RW)");
        return -1;
    };
    let mut io = block_io::CallbackBlockIo {
        read_fn: cfg.read,
        write_fn: Some(write_fn),
        context: cfg.context,
        size: cfg.size_bytes,
    };
    match mkfs::format_filesystem(&mut io, cfg.size_bytes, 4096, 4096, None, None) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

#[cfg(test)]
mod pure_fn_tests {
    use super::{
        cstr_to_path, decode_mount_point_print_name, decode_symlink_print_name, err_i64, err_int,
        err_ptr, fs_ntfs_clear_last_error, fs_ntfs_last_errno, infer_errno_from_message,
        make_dirent, set_error, utf16_le_bytes_to_string, FS_NTFS_DIRENT_NAME_BYTES,
    };
    use std::ffi::CString;

    // --- infer_errno_from_message ---

    #[test]
    fn infer_errno_not_found_variants() {
        assert_eq!(infer_errno_from_message("file not found"), 2);
        assert_eq!(infer_errno_from_message("path is nonexistent"), 2);
        assert_eq!(infer_errno_from_message("ENOENT: no such file"), 2);
        assert_eq!(infer_errno_from_message("record not mapped"), 2);
    }

    #[test]
    fn infer_errno_already_exists() {
        assert_eq!(infer_errno_from_message("already exists"), 17);
        assert_eq!(infer_errno_from_message("EEXIST in index"), 17);
    }

    #[test]
    fn infer_errno_no_space() {
        assert_eq!(infer_errno_from_message("no room in record"), 28);
        assert_eq!(infer_errno_from_message("volume is full"), 28);
        assert_eq!(infer_errno_from_message("out of space"), 28);
        assert_eq!(infer_errno_from_message("exceeds record capacity"), 28);
    }

    #[test]
    fn infer_errno_invalid() {
        assert_eq!(infer_errno_from_message("invalid basename"), 22);
        assert_eq!(infer_errno_from_message("null or non-UTF-8 path"), 22);
        assert_eq!(infer_errno_from_message("null pointer"), 22);
    }

    #[test]
    fn infer_errno_directory_errors() {
        assert_eq!(infer_errno_from_message("not a directory"), 20);
        assert_eq!(infer_errno_from_message("target is a directory"), 21);
        assert_eq!(infer_errno_from_message("directory not empty"), 66);
    }

    #[test]
    fn infer_errno_permission() {
        assert_eq!(infer_errno_from_message("refuse to overwrite"), 1);
        assert_eq!(infer_errno_from_message("permission denied"), 1);
    }

    #[test]
    fn infer_errno_fallback_is_eio() {
        assert_eq!(infer_errno_from_message("some weird I/O error"), 5);
        assert_eq!(infer_errno_from_message(""), 5);
    }

    // --- utf16_le_bytes_to_string ---

    #[test]
    fn utf16_le_empty_returns_none() {
        assert_eq!(utf16_le_bytes_to_string(&[]), None);
    }

    #[test]
    fn utf16_le_odd_length_returns_none() {
        assert_eq!(utf16_le_bytes_to_string(&[0x48]), None);
        assert_eq!(utf16_le_bytes_to_string(&[0x48, 0x00, 0x00]), None);
    }

    #[test]
    fn utf16_le_valid_ascii_string() {
        // "Hi" in UTF-16 LE
        let bytes = [0x48u8, 0x00, 0x69, 0x00];
        assert_eq!(utf16_le_bytes_to_string(&bytes), Some("Hi".to_string()));
    }

    #[test]
    fn utf16_le_valid_unicode() {
        // U+00E9 (é) in UTF-16 LE
        let bytes = [0xE9u8, 0x00];
        assert_eq!(utf16_le_bytes_to_string(&bytes), Some("é".to_string()));
    }

    #[test]
    fn utf16_le_invalid_surrogates_returns_none() {
        // Lone high surrogate U+D800 — invalid UTF-16
        let bytes = [0x00u8, 0xD8];
        assert_eq!(utf16_le_bytes_to_string(&bytes), None);
    }

    // --- decode_symlink_print_name ---

    fn symlink_buf(print_name_offset: u16, print_name: &str) -> Vec<u8> {
        let pn_utf16: Vec<u16> = print_name.encode_utf16().collect();
        let pn_bytes: Vec<u8> = pn_utf16.iter().flat_map(|c| c.to_le_bytes()).collect();
        // substitute_name_offset=0, substitute_name_length=0, print_name_offset, print_name_length, Flags(4)
        let mut buf = vec![0u8; 12 + print_name_offset as usize + pn_bytes.len()];
        buf[4..6].copy_from_slice(&print_name_offset.to_le_bytes());
        buf[6..8].copy_from_slice(&(pn_bytes.len() as u16).to_le_bytes());
        // Flags at [8..12] = 0
        let start = 12 + print_name_offset as usize;
        buf[start..start + pn_bytes.len()].copy_from_slice(&pn_bytes);
        buf
    }

    #[test]
    fn decode_symlink_basic() {
        let buf = symlink_buf(0, "C:\\target");
        assert_eq!(
            decode_symlink_print_name(&buf),
            Some("C:\\target".to_string())
        );
    }

    #[test]
    fn decode_symlink_with_offset() {
        // Print name starts after 4 bytes (e.g. substitute name occupies first 4 bytes)
        let buf = symlink_buf(4, "C:\\link");
        assert_eq!(
            decode_symlink_print_name(&buf),
            Some("C:\\link".to_string())
        );
    }

    #[test]
    fn decode_symlink_too_short_returns_none() {
        assert_eq!(decode_symlink_print_name(&[0u8; 11]), None);
        assert_eq!(decode_symlink_print_name(&[]), None);
    }

    #[test]
    fn decode_symlink_out_of_bounds_returns_none() {
        // print_name_offset says data starts at offset 100 but buffer is tiny
        let mut buf = vec![0u8; 12];
        buf[4..6].copy_from_slice(&100u16.to_le_bytes()); // offset=100
        buf[6..8].copy_from_slice(&2u16.to_le_bytes()); // length=2
        assert_eq!(decode_symlink_print_name(&buf), None);
    }

    // --- decode_mount_point_print_name ---

    fn mount_point_buf(print_name_offset: u16, print_name: &str) -> Vec<u8> {
        let pn_utf16: Vec<u16> = print_name.encode_utf16().collect();
        let pn_bytes: Vec<u8> = pn_utf16.iter().flat_map(|c| c.to_le_bytes()).collect();
        // substitute_name_offset=0, substitute_name_length=0, print_name_offset, print_name_length
        // PathBuffer starts at offset 8 (no Flags field unlike symlink)
        let mut buf = vec![0u8; 8 + print_name_offset as usize + pn_bytes.len()];
        buf[4..6].copy_from_slice(&print_name_offset.to_le_bytes());
        buf[6..8].copy_from_slice(&(pn_bytes.len() as u16).to_le_bytes());
        let start = 8 + print_name_offset as usize;
        buf[start..start + pn_bytes.len()].copy_from_slice(&pn_bytes);
        buf
    }

    #[test]
    fn decode_mount_point_basic() {
        let buf = mount_point_buf(0, "\\??\\Volume{abc}\\");
        assert_eq!(
            decode_mount_point_print_name(&buf),
            Some("\\??\\Volume{abc}\\".to_string())
        );
    }

    #[test]
    fn decode_mount_point_too_short_returns_none() {
        assert_eq!(decode_mount_point_print_name(&[0u8; 7]), None);
        assert_eq!(decode_mount_point_print_name(&[]), None);
    }

    #[test]
    fn decode_mount_point_out_of_bounds_returns_none() {
        let mut buf = vec![0u8; 8];
        buf[4..6].copy_from_slice(&200u16.to_le_bytes()); // offset=200, far out
        buf[6..8].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(decode_mount_point_print_name(&buf), None);
    }

    // --- thread-local error state ---

    #[test]
    fn set_error_records_errno() {
        set_error("file not found");
        assert_eq!(fs_ntfs_last_errno(), 2); // ENOENT
    }

    #[test]
    fn set_error_records_eexist() {
        set_error("already exists in directory");
        assert_eq!(fs_ntfs_last_errno(), 17); // EEXIST
    }

    #[test]
    fn set_error_records_enospc() {
        set_error("no room for new attribute");
        assert_eq!(fs_ntfs_last_errno(), 28); // ENOSPC
    }

    #[test]
    fn set_error_records_einval() {
        set_error("invalid basename provided");
        assert_eq!(fs_ntfs_last_errno(), 22); // EINVAL
    }

    #[test]
    fn set_error_records_enotdir() {
        set_error("path component is not a directory");
        assert_eq!(fs_ntfs_last_errno(), 20); // ENOTDIR
    }

    #[test]
    fn set_error_records_eisdir() {
        set_error("target is a directory, not a file");
        assert_eq!(fs_ntfs_last_errno(), 21); // EISDIR
    }

    #[test]
    fn set_error_records_enotempty() {
        set_error("directory is not empty");
        assert_eq!(fs_ntfs_last_errno(), 66); // ENOTEMPTY
    }

    #[test]
    fn set_error_records_eperm() {
        set_error("refuse to overwrite special file");
        assert_eq!(fs_ntfs_last_errno(), 1); // EPERM
    }

    #[test]
    fn set_error_fallback_is_eio() {
        set_error("some unrecognised error");
        assert_eq!(fs_ntfs_last_errno(), 5); // EIO
    }

    #[test]
    fn clear_last_error_resets_errno_to_zero() {
        set_error("not found");
        fs_ntfs_clear_last_error();
        assert_eq!(fs_ntfs_last_errno(), 0);
    }

    // --- err_int / err_i64 / err_ptr ---

    #[test]
    fn err_int_returns_negative_one() {
        let ret = err_int("file not found in err_int test");
        assert_eq!(ret, -1);
        assert_eq!(fs_ntfs_last_errno(), 2); // ENOENT
    }

    #[test]
    fn err_i64_returns_negative_one() {
        let ret = err_i64("already exists in err_i64 test");
        assert_eq!(ret, -1i64);
        assert_eq!(fs_ntfs_last_errno(), 17);
    }

    #[test]
    fn err_ptr_returns_null() {
        let ret: *mut u8 = err_ptr("no room in err_ptr test");
        assert!(ret.is_null());
        assert_eq!(fs_ntfs_last_errno(), 28);
    }

    // --- make_dirent ---

    #[test]
    fn make_dirent_sets_fields() {
        let d = make_dirent(42, 2, b"hello");
        assert_eq!(d.file_record_number, 42);
        assert_eq!(d.file_type, 2);
        assert_eq!(d.name_len, 5);
        assert_eq!(&d.name[..5], b"hello");
    }

    #[test]
    fn make_dirent_zero_fills_trailing_name() {
        let d = make_dirent(1, 0, b"hi");
        assert_eq!(d.name_len, 2);
        assert_eq!(d.name[2], 0);
        assert_eq!(d.name[FS_NTFS_DIRENT_NAME_BYTES - 1], 0);
    }

    #[test]
    fn make_dirent_truncates_long_name() {
        let long_name = vec![b'X'; FS_NTFS_DIRENT_NAME_BYTES + 10];
        let d = make_dirent(1, 0, &long_name);
        // name_len capped at FS_NTFS_DIRENT_NAME_BYTES - 1
        assert_eq!(d.name_len as usize, FS_NTFS_DIRENT_NAME_BYTES - 1);
    }

    #[test]
    fn make_dirent_empty_name() {
        let d = make_dirent(99, 1, b"");
        assert_eq!(d.name_len, 0);
        assert_eq!(d.file_record_number, 99);
    }

    // --- cstr_to_path ---

    #[test]
    fn cstr_to_path_null_returns_none() {
        let result = cstr_to_path(std::ptr::null());
        assert!(result.is_none());
    }

    #[test]
    fn cstr_to_path_valid_string() {
        let s = CString::new("/mnt/ntfs.img").unwrap();
        let result = cstr_to_path(s.as_ptr());
        assert_eq!(result, Some("/mnt/ntfs.img"));
    }

    #[test]
    fn cstr_to_path_empty_string() {
        let s = CString::new("").unwrap();
        let result = cstr_to_path(s.as_ptr());
        assert_eq!(result, Some(""));
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::ntfs_time_to_unix;

    fn make_ts(hundred_ns: u64) -> ntfs::NtfsTime {
        ntfs::NtfsTime::from(hundred_ns)
    }

    #[test]
    fn unix_epoch_itself() {
        // 1970-01-01T00:00:00Z = 11644473600 seconds after 1601-01-01
        let ts = make_ts(11_644_473_600u64 * 10_000_000);
        let (sec, nsec) = ntfs_time_to_unix(ts);
        assert_eq!(sec, 0);
        assert_eq!(nsec, 0);
    }

    #[test]
    fn positive_timestamp_with_subseconds() {
        // 1 second + 250 ms after UNIX epoch
        let hundred_ns = (11_644_473_600u64 + 1) * 10_000_000 + 2_500_000; // +250ms
        let (sec, nsec) = ntfs_time_to_unix(make_ts(hundred_ns));
        assert_eq!(sec, 1);
        assert_eq!(nsec, 250_000_000);
    }

    #[test]
    fn pre_epoch_timestamp() {
        // 1 second before UNIX epoch → sec = -1, nsec = 0
        let hundred_ns = (11_644_473_600u64 - 1) * 10_000_000;
        let (sec, nsec) = ntfs_time_to_unix(make_ts(hundred_ns));
        assert_eq!(sec, -1);
        assert_eq!(nsec, 0);
    }

    #[test]
    fn nsec_max_value() {
        // 999_999_900 ns = 9_999_999 × 100 ns intervals (just under 1 second)
        let hundred_ns = 11_644_473_600u64 * 10_000_000 + 9_999_999;
        let (sec, nsec) = ntfs_time_to_unix(make_ts(hundred_ns));
        assert_eq!(sec, 0);
        assert_eq!(nsec, 999_999_900);
    }
}
