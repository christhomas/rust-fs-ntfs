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
use std::slice;

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace, NtfsStandardInformation};
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType, NtfsFile, NtfsReadSeek};

pub mod attr_io;
pub mod attr_resize;
pub mod bitmap;
pub mod data_runs;
pub mod ea_io;
pub mod facade;
pub mod fsck;
pub mod idx_block;
pub mod index_io;
pub mod mft_bitmap;
pub mod mft_io;
pub mod record_build;
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
// Callback-based reader for FSKit integration
// ---------------------------------------------------------------------------

type ReadCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u64) -> c_int;

struct CallbackReader {
    read_fn: ReadCallback,
    context: *mut c_void,
    size: u64,
    position: u64,
}

// Safety: The context pointer is managed by the caller (Swift/FSKit) and
// is valid for the lifetime of the mount. FSKit guarantees serial access.
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
            return Err(std::io::Error::other(
                "read callback failed",
            ));
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
// Bridge filesystem handle
// ---------------------------------------------------------------------------

enum ReaderKind {
    File(BufReader<File>),
    Callback(BufReader<CallbackReader>),
}

impl Read for ReaderKind {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ReaderKind::File(r) => r.read(buf),
            ReaderKind::Callback(r) => r.read(buf),
        }
    }
}

impl Seek for ReaderKind {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            ReaderKind::File(r) => r.seek(pos),
            ReaderKind::Callback(r) => r.seek(pos),
        }
    }
}

pub struct FsNtfsHandle {
    ntfs: Ntfs,
    reader: ReaderKind,
}

// ---------------------------------------------------------------------------
// C types matching fs_ntfs.h
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct FsNtfsAttr {
    file_record_number: u64,
    size: u64,
    atime: u32,
    mtime: u32,
    ctime: u32,
    crtime: u32,
    mode: u16,
    link_count: u16,
    file_type: u32,
    attributes: u32,
}

#[repr(C)]
pub struct FsNtfsDirent {
    file_record_number: u64,
    file_type: u8,
    name_len: u16,
    name: [u8; 256],
}

#[repr(C)]
pub struct FsNtfsVolumeInfo {
    volume_name: [u8; 128],
    cluster_size: u32,
    total_clusters: u64,
    ntfs_version_major: u16,
    ntfs_version_minor: u16,
    serial_number: u64,
    total_size: u64,
}

#[repr(C)]
pub struct FsNtfsBlockdevCfg {
    read: ReadCallback,
    context: *mut c_void,
    size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Directory iterator
// ---------------------------------------------------------------------------

pub struct FsNtfsDirIter {
    entries: Vec<FsNtfsDirent>,
    index: usize,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert an NTFS timestamp (100ns intervals since 1601-01-01) to UNIX epoch.
fn ntfs_time_to_unix(ntfs_time: ntfs::NtfsTime) -> u32 {
    // NTFS epoch is 1601-01-01, UNIX epoch is 1970-01-01.
    // Difference is 11644473600 seconds.
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let secs = ntfs_time.nt_timestamp() / 10_000_000;
    secs.saturating_sub(EPOCH_DIFF) as u32
}

/// Build a synthesized dirent (used for "." / "..").
fn make_dirent(file_record_number: u64, file_type: u8, name: &[u8]) -> FsNtfsDirent {
    let mut out = FsNtfsDirent {
        file_record_number,
        file_type,
        name_len: std::cmp::min(name.len(), 255) as u16,
        name: [0u8; 256],
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
                    attr.crtime = ntfs_time_to_unix(std_info.creation_time());
                    attr.mtime = ntfs_time_to_unix(std_info.modification_time());
                    attr.atime = ntfs_time_to_unix(std_info.access_time());
                    attr.ctime = ntfs_time_to_unix(std_info.mft_record_modification_time());
                    attr.attributes = std_info.file_attributes().bits();
                }
            }
            Ok(NtfsAttributeType::Data) => {
                if attribute.name().map(|n| n.is_empty()).unwrap_or(true) {
                    attr.size = attribute.value_length();
                }
            }
            Ok(NtfsAttributeType::ReparsePoint) => {
                // Read the 32-bit reparse tag and dispatch. The
                // REPARSE_POINT flag on $FILE_NAME is an "SOME reparse
                // kind" marker; only the tag tells us *which*. See
                // docs/NEXT_PLAN.md §1.2 / docs/STATUS.md §cross-check.
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

    let bridge = Box::new(FsNtfsHandle { ntfs, reader });
    Box::into_raw(bridge)
}

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

    let bridge = Box::new(FsNtfsHandle { ntfs, reader });
    Box::into_raw(bridge)
}

#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_umount(fs: *mut FsNtfsHandle) {
    if !fs.is_null() {
        unsafe {
            drop(Box::from_raw(fs));
        }
    }
}

// ---------------------------------------------------------------------------
// Volume info
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Stat
// ---------------------------------------------------------------------------

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
        atime: 0,
        mtime: 0,
        ctime: 0,
        crtime: 0,
        mode: 0,
        link_count: 0,
        file_type: 0,
        attributes: 0,
    };

    let file = match navigate_to_path(&bridge.ntfs, &mut bridge.reader, path_str) {
        Ok(f) => f,
        Err(e) => {
            set_error(&e);
            return -1;
        }
    };

    if let Err(e) = fill_attr(&file, &mut bridge.reader, out) {
        set_error(&e);
        return -1;
    }

    0
}

// ---------------------------------------------------------------------------
// Directory listing
// ---------------------------------------------------------------------------

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
        Err(e) => {
            set_error(&e);
            return std::ptr::null_mut();
        }
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

    let index = match dir_file.directory_index(&mut bridge.reader) {
        Ok(i) => i,
        Err(e) => {
            set_error(&format!("directory index: {e}"));
            return std::ptr::null_mut();
        }
    };

    let mut entries = Vec::new();
    // Synthesized entries for "." and ".."
    entries.push(make_dirent(current_record_number, 2, b"."));
    entries.push(make_dirent(parent_record_number, 2, b".."));

    let mut iter = index.entries();

    while let Some(entry_result) = iter.next(&mut bridge.reader) {
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };

        let file_name = match entry.key() {
            Some(Ok(name)) => name,
            _ => continue,
        };

        // Skip DOS-only names to avoid duplicates
        if file_name.namespace() == NtfsFileNamespace::Dos {
            continue;
        }

        let name_str = file_name.name().to_string_lossy();
        let name_bytes = name_str.as_bytes();

        let mut dirent = FsNtfsDirent {
            file_record_number: entry.file_reference().file_record_number(),
            file_type: if file_name.is_directory() { 2 } else { 1 },
            name_len: std::cmp::min(name_bytes.len(), 255) as u16,
            name: [0u8; 256],
        };

        let copy_len = dirent.name_len as usize;
        dirent.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        entries.push(dirent);
    }

    let iter = Box::new(FsNtfsDirIter { entries, index: 0 });
    Box::into_raw(iter)
}

#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_dir_next(iter: *mut FsNtfsDirIter) -> *const FsNtfsDirent {
    if iter.is_null() {
        return std::ptr::null();
    }

    let it = unsafe { &mut *iter };
    if it.index >= it.entries.len() {
        return std::ptr::null();
    }

    let ptr = &it.entries[it.index] as *const FsNtfsDirent;
    it.index += 1;
    ptr
}

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
        Err(e) => {
            set_error(&e);
            return -1;
        }
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
        Err(e) => {
            set_error(&e);
            return -1;
        }
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
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_free_clusters: null or non-UTF-8 path");
        return -1;
    };
    match crate::bitmap::locate_bitmap(std::path::Path::new(p))
        .and_then(|bm| crate::bitmap::count_free(std::path::Path::new(p), &bm))
    {
        Ok(n) => n as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Count free MFT records in `$MFT:$Bitmap`. Returns the count on
/// success, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_mft_free_records(path: *const c_char) -> i64 {
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_mft_free_records: null or non-UTF-8 path");
        return -1;
    };
    match crate::mft_bitmap::locate(std::path::Path::new(p))
        .and_then(|bm| crate::mft_bitmap::count_free(std::path::Path::new(p), &bm))
    {
        Ok(n) => n as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Check whether the volume's `VOLUME_IS_DIRTY` flag is set.
/// Returns `1` if dirty, `0` if clean, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_is_dirty(path: *const c_char) -> c_int {
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_is_dirty: null or non-UTF-8 path");
        return -1;
    };
    match fsck::is_dirty(p) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Clear the `VOLUME_IS_DIRTY` flag on an NTFS image.
///
/// Returns `1` if the flag was set and has been cleared, `0` if the
/// volume was already clean, `-1` on error. Call
/// `fs_ntfs_last_error()` for the error message.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_clear_dirty(path: *const c_char) -> c_int {
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_clear_dirty: null or non-UTF-8 path");
        return -1;
    };
    match fsck::clear_dirty(p) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Overwrite `$LogFile` with the NTFS "empty log" pattern (all `0xFF`).
///
/// Returns the number of bytes overwritten on success, `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_reset_logfile(path: *const c_char) -> i64 {
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_reset_logfile: null or non-UTF-8 path");
        return -1;
    };
    match fsck::reset_logfile(p) {
        Ok(n) => n as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_set_times: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_set_times: null or non-UTF-8 path");
        return -1;
    };
    let times = write::FileTimes {
        creation: unsafe { creation.as_ref() }.map(|v| *v as u64),
        modification: unsafe { modification.as_ref() }.map(|v| *v as u64),
        mft_record_modification: unsafe { mft_record_modification.as_ref() }.map(|v| *v as u64),
        access: unsafe { access.as_ref() }.map(|v| *v as u64),
    };
    match write::set_times(std::path::Path::new(img), fp, times) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_create_file: null or non-UTF-8 image");
        return -1;
    };
    let Some(pp) = cstr_to_path(parent_path) else {
        set_error("fs_ntfs_create_file: null or non-UTF-8 parent_path");
        return -1;
    };
    let Some(bn) = cstr_to_path(basename) else {
        set_error("fs_ntfs_create_file: null or non-UTF-8 basename");
        return -1;
    };
    match write::create_file(std::path::Path::new(img), pp, bn) {
        Ok(rn) => rn as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_ea: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_ea: null or non-UTF-8 path");
        return -1;
    };
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
    match write::write_ea(std::path::Path::new(img), p, name_bytes, data, flags) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_remove_ea: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_remove_ea: null or non-UTF-8 path");
        return -1;
    };
    if ea_name.is_null() {
        set_error("fs_ntfs_remove_ea: null ea_name");
        return -1;
    }
    let name_bytes = unsafe { CStr::from_ptr(ea_name) }.to_bytes();
    match write::remove_ea(std::path::Path::new(img), p, name_bytes) {
        Ok(()) => 0,
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_reparse_point: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_reparse_point: null or non-UTF-8 path");
        return -1;
    };
    let data: &[u8] = if len == 0 {
        &[]
    } else if buf.is_null() {
        set_error("fs_ntfs_write_reparse_point: null buf with non-zero len");
        return -1;
    } else {
        unsafe { slice::from_raw_parts(buf as *const u8, len as usize) }
    };
    match write::write_reparse_point(std::path::Path::new(img), p, reparse_tag, data) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Remove a file's `$REPARSE_POINT` attribute and clear the reparse
/// flag. Returns 0 on success, -1 on error (e.g. no reparse point).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_remove_reparse_point(image: *const c_char, path: *const c_char) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_remove_reparse_point: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_remove_reparse_point: null or non-UTF-8 path");
        return -1;
    };
    match write::remove_reparse_point(std::path::Path::new(img), p) {
        Ok(()) => 0,
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_create_symlink: null or non-UTF-8 image");
        return -1;
    };
    let Some(pp) = cstr_to_path(parent_path) else {
        set_error("fs_ntfs_create_symlink: null or non-UTF-8 parent_path");
        return -1;
    };
    let Some(bn) = cstr_to_path(basename) else {
        set_error("fs_ntfs_create_symlink: null or non-UTF-8 basename");
        return -1;
    };
    let Some(tg) = cstr_to_path(target) else {
        set_error("fs_ntfs_create_symlink: null or non-UTF-8 target");
        return -1;
    };
    match write::create_symlink(std::path::Path::new(img), pp, bn, tg, relative != 0) {
        Ok(rn) => rn as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_named_stream: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_named_stream: null or non-UTF-8 path");
        return -1;
    };
    let Some(sn) = cstr_to_path(stream_name) else {
        set_error("fs_ntfs_write_named_stream: null or non-UTF-8 stream_name");
        return -1;
    };
    let data: &[u8] = if len == 0 {
        &[]
    } else if buf.is_null() {
        set_error("fs_ntfs_write_named_stream: null buf with non-zero len");
        return -1;
    } else {
        unsafe { slice::from_raw_parts(buf as *const u8, len as usize) }
    };
    match write::write_named_stream(std::path::Path::new(img), p, sn, data) {
        Ok(()) => 0,
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_delete_named_stream: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_delete_named_stream: null or non-UTF-8 path");
        return -1;
    };
    let Some(sn) = cstr_to_path(stream_name) else {
        set_error("fs_ntfs_delete_named_stream: null or non-UTF-8 stream_name");
        return -1;
    };
    match write::delete_named_stream(std::path::Path::new(img), p, sn) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_file_contents: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_file_contents: null or non-UTF-8 path");
        return -1;
    };
    if len == 0 {
        return match write::write_file_contents(std::path::Path::new(img), p, &[]) {
            Ok(n) => n as i64,
            Err(e) => {
                set_error(&e);
                -1
            }
        };
    }
    if buf.is_null() {
        set_error("fs_ntfs_write_file_contents: null buf with non-zero len");
        return -1;
    }
    let data = unsafe { slice::from_raw_parts(buf as *const u8, len as usize) };
    match write::write_file_contents(std::path::Path::new(img), p, data) {
        Ok(n) => n as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Delete an empty directory. Returns 0 on success, -1 on error.
/// Fails if the directory is non-empty or has `$INDEX_ALLOCATION`
/// overflow (for MVP).
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_rmdir(image: *const c_char, path: *const c_char) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_rmdir: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_rmdir: null or non-UTF-8 path");
        return -1;
    };
    match write::rmdir(std::path::Path::new(img), p) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_mkdir: null or non-UTF-8 image");
        return -1;
    };
    let Some(pp) = cstr_to_path(parent_path) else {
        set_error("fs_ntfs_mkdir: null or non-UTF-8 parent_path");
        return -1;
    };
    let Some(bn) = cstr_to_path(basename) else {
        set_error("fs_ntfs_mkdir: null or non-UTF-8 basename");
        return -1;
    };
    match write::mkdir(std::path::Path::new(img), pp, bn) {
        Ok(rn) => rn as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_resident_contents: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_resident_contents: null or non-UTF-8 path");
        return -1;
    };
    if len == 0 {
        return match write::write_resident_contents(std::path::Path::new(img), p, &[]) {
            Ok(n) => n as i64,
            Err(e) => {
                set_error(&e);
                -1
            }
        };
    }
    if buf.is_null() {
        set_error("fs_ntfs_write_resident_contents: null buf with non-zero len");
        return -1;
    }
    let data = unsafe { slice::from_raw_parts(buf as *const u8, len as usize) };
    match write::write_resident_contents(std::path::Path::new(img), p, data) {
        Ok(n) => n as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
    }
}

/// Delete a regular file. Refuses directories. Returns 0 on success,
/// -1 on error. On success the file's data-run clusters and MFT
/// record are freed.
#[unsafe(no_mangle)]
pub extern "C" fn fs_ntfs_unlink(image: *const c_char, path: *const c_char) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_unlink: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_unlink: null or non-UTF-8 path");
        return -1;
    };
    match write::unlink(std::path::Path::new(img), fp) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_read_object_id: null or non-UTF-8 image");
        return -1;
    };
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_read_object_id: null or non-UTF-8 path");
        return -1;
    };
    if out_buf.is_null() {
        set_error("fs_ntfs_read_object_id: null out_buf");
        return -1;
    }
    match write::read_object_id(std::path::Path::new(img), p) {
        Ok(Some(guid)) => {
            unsafe { std::ptr::copy_nonoverlapping(guid.as_ptr(), out_buf, 16) };
            1
        }
        Ok(None) => 0,
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_link: null or non-UTF-8 image");
        return -1;
    };
    let Some(ep) = cstr_to_path(existing_path) else {
        set_error("fs_ntfs_link: null or non-UTF-8 existing_path");
        return -1;
    };
    let Some(npp) = cstr_to_path(new_parent_path) else {
        set_error("fs_ntfs_link: null or non-UTF-8 new_parent_path");
        return -1;
    };
    let Some(nb) = cstr_to_path(new_basename) else {
        set_error("fs_ntfs_link: null or non-UTF-8 new_basename");
        return -1;
    };
    match write::link(std::path::Path::new(img), ep, npp, nb) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_rename: null or non-UTF-8 image");
        return -1;
    };
    let Some(op) = cstr_to_path(old_path) else {
        set_error("fs_ntfs_rename: null or non-UTF-8 old_path");
        return -1;
    };
    let Some(nb) = cstr_to_path(new_basename) else {
        set_error("fs_ntfs_rename: null or non-UTF-8 new_basename");
        return -1;
    };
    match write::rename(std::path::Path::new(img), op, nb) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_rename_same_length: null or non-UTF-8 image");
        return -1;
    };
    let Some(op) = cstr_to_path(old_path) else {
        set_error("fs_ntfs_rename_same_length: null or non-UTF-8 old_path");
        return -1;
    };
    let Some(nn) = cstr_to_path(new_name) else {
        set_error("fs_ntfs_rename_same_length: null or non-UTF-8 new_name");
        return -1;
    };
    match write::rename_same_length(std::path::Path::new(img), op, nn) {
        Ok(()) => 0,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_grow: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_grow: null or non-UTF-8 path");
        return -1;
    };
    match write::grow_nonresident(std::path::Path::new(img), fp, new_size) {
        Ok(n) => n as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_truncate: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_truncate: null or non-UTF-8 path");
        return -1;
    };
    match write::truncate(std::path::Path::new(img), fp, new_size) {
        Ok(n) => n as i64,
        Err(e) => {
            set_error(&e);
            -1
        }
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
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_write_file: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_write_file: null or non-UTF-8 path");
        return -1;
    };
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        set_error("fs_ntfs_write_file: null buffer with non-zero length");
        return -1;
    }
    let data = unsafe { slice::from_raw_parts(buf as *const u8, len as usize) };
    match write::write_at(std::path::Path::new(img), fp, offset, data) {
        Ok(n) => n as i64,
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
pub extern "C" fn fs_ntfs_chattr(
    image: *const c_char,
    path: *const c_char,
    add_flags: u32,
    remove_flags: u32,
) -> c_int {
    let Some(img) = cstr_to_path(image) else {
        set_error("fs_ntfs_chattr: null or non-UTF-8 image");
        return -1;
    };
    let Some(fp) = cstr_to_path(path) else {
        set_error("fs_ntfs_chattr: null or non-UTF-8 path");
        return -1;
    };
    match write::set_file_attributes(
        std::path::Path::new(img),
        fp,
        write::FileAttributesChange {
            add: add_flags,
            remove: remove_flags,
        },
    ) {
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
    let Some(p) = cstr_to_path(path) else {
        set_error("fs_ntfs_fsck: null or non-UTF-8 path");
        return -1;
    };
    match fsck::fsck(p) {
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
