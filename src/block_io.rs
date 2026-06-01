//! Block-device I/O abstraction shared by the read and write paths.
//!
//! Path-based callers (the existing `image: &Path` APIs) construct a
//! [`PathIo`] which opens the file RW on construction and translates
//! offset-based reads/writes to `seek + read_exact / write_all`.
//!
//! Callback-based callers (FSKit / the SDK consumer that wires up an
//! `FSBlockDeviceResource`) construct a [`CallbackBlockIo`] over the
//! `read` / `write` function pointers in [`crate::FsNtfsBlockdevCfg`].
//!
//! The trait deliberately models *positioned* I/O (`read_at` /
//! `write_at`) rather than `Read + Seek`, because both kinds of
//! consumer naturally support positioned access and it keeps the write
//! sites simple (no shared mutable cursor state to thread through).
//!
//! `read_exact_at`, `write_all_at`, `size`, and `sync` are exactly
//! what `crate::fsck::FsckIo` already exposes — that trait predates
//! this one and remains live for the fsck-only entry points so it
//! doesn't break callers that already implement it. Internally the
//! mutator stack uses [`BlockIo`] directly.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::c_int;
use std::path::Path;

/// Positioned block-device-style I/O.
///
/// Implementors serve `read_exact_at` / `write_all_at` reads and writes
/// against whatever storage backs them (a file, an FSKit
/// `FSBlockDeviceResource`, an in-memory buffer for tests). `size` is
/// the total byte length of the device and is used by the
/// [`IoReadSeek`] adapter to back `Seek::End` semantics for upstream
/// `ntfs::Ntfs::new`.
pub trait BlockIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String>;
    fn size(&self) -> u64;
    /// Flush pending writes to stable storage. Path-backed impls call
    /// `fsync`. Callback-backed impls let the host (FSKit / Go backend)
    /// drain on its own sync barrier, so this is a no-op there.
    fn sync(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Forward `BlockIo` through `&mut T` so functions can take either
/// `&mut PathIo` or `&mut dyn BlockIo` interchangeably.
impl<T: BlockIo + ?Sized> BlockIo for &mut T {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        (**self).read_exact_at(offset, buf)
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        (**self).write_all_at(offset, buf)
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn sync(&mut self) -> Result<(), String> {
        (**self).sync()
    }
}

// ---------------------------------------------------------------------------
// Path-backed impl
// ---------------------------------------------------------------------------

/// `BlockIo` backed by a real filesystem path. Holds a single RW
/// `File` open for the duration so the existing path-based mutator
/// API doesn't pay the open-then-close cost on every record write the
/// way the previous (per-call) implementations did.
pub struct PathIo {
    file: File,
    size: u64,
}

impl PathIo {
    pub fn open_rw(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open rw '{}': {e}", path.display()))?;
        let size = file
            .metadata()
            .map_err(|e| format!("stat '{}': {e}", path.display()))?
            .len();
        Ok(Self { file, size })
    }

    /// Path-backed read-only handle. Used by routines that conceptually
    /// only read but go through the same trait surface (e.g. resolve
    /// path → MFT record number).
    pub fn open_ro(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("open ro '{}': {e}", path.display()))?;
        let size = file
            .metadata()
            .map_err(|e| format!("stat '{}': {e}", path.display()))?
            .len();
        Ok(Self { file, size })
    }
}

impl BlockIo for PathIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek {offset}: {e}"))?;
        self.file
            .read_exact(buf)
            .map_err(|e| format!("read_exact @{offset} len {}: {e}", buf.len()))
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek {offset}: {e}"))?;
        self.file
            .write_all(buf)
            .map_err(|e| format!("write_all @{offset} len {}: {e}", buf.len()))
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn sync(&mut self) -> Result<(), String> {
        self.file.sync_all().map_err(|e| format!("fsync: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Callback-backed impl
// ---------------------------------------------------------------------------

/// Read callback signature, mirroring `fs_ntfs_read_fn`.
pub type ReadCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u64) -> c_int;
/// Write callback signature, mirroring `fs_ntfs_write_fn`.
pub type WriteCallback = unsafe extern "C" fn(*mut c_void, *const c_void, u64, u64) -> c_int;

/// `BlockIo` backed by a `(read_fn, write_fn, context, size)` tuple.
/// Used by FSKit / any consumer that mounted the volume via
/// `fs_ntfs_mount_with_callbacks`.
///
/// **Safety**: the `context` pointer must remain valid (and the
/// callbacks remain dispatchable) for as long as any operation that
/// holds a `CallbackBlockIo` is in flight. FSKit honors this — the
/// `FSBlockDeviceResource` outlives any FSKit op handler.
pub struct CallbackBlockIo {
    pub read_fn: ReadCallback,
    /// `None` when the handle was mounted read-only; any write attempt
    /// returns an error rather than dispatching to a stub callback.
    pub write_fn: Option<WriteCallback>,
    pub context: *mut c_void,
    pub size: u64,
}

unsafe impl Send for CallbackBlockIo {}

impl BlockIo for CallbackBlockIo {
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
        let Some(write_fn) = self.write_fn else {
            return Err("write attempted on read-only callback I/O".to_string());
        };
        let rc = unsafe {
            (write_fn)(
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
}

// ---------------------------------------------------------------------------
// Read+Seek adapter (for upstream ntfs::Ntfs)
// ---------------------------------------------------------------------------

/// Wrap a `BlockIo` so it can be passed to upstream `ntfs::Ntfs::new`
/// or any other API expecting `Read + Seek`. Maintains an internal
/// cursor that's updated on every `read` / `seek`.
pub struct IoReadSeek<'a, T: BlockIo + ?Sized> {
    pub io: &'a mut T,
    position: u64,
}

impl<'a, T: BlockIo + ?Sized> IoReadSeek<'a, T> {
    pub fn new(io: &'a mut T) -> Self {
        Self { io, position: 0 }
    }
}

impl<T: BlockIo + ?Sized> Read for IoReadSeek<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = self.io.size();
        if self.position >= size {
            return Ok(0);
        }
        let want = std::cmp::min(buf.len() as u64, size - self.position) as usize;
        if want == 0 {
            return Ok(0);
        }
        self.io
            .read_exact_at(self.position, &mut buf[..want])
            .map_err(std::io::Error::other)?;
        self.position += want as u64;
        Ok(want)
    }
}

impl<T: BlockIo + ?Sized> Seek for IoReadSeek<'_, T> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(p) => self.io.size() as i64 + p,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    // -------------------------------------------------------------------------
    // Minimal in-memory BlockIo for IoReadSeek tests (no disk needed).
    // -------------------------------------------------------------------------

    struct MemDev {
        buf: Vec<u8>,
    }
    impl MemDev {
        fn with_bytes(bytes: impl Into<Vec<u8>>) -> Self {
            Self { buf: bytes.into() }
        }
    }
    impl BlockIo for MemDev {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            let off = offset as usize;
            if off + buf.len() > self.buf.len() {
                return Err(format!("read past end: off={off} len={}", buf.len()));
            }
            buf.copy_from_slice(&self.buf[off..off + buf.len()]);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            let off = offset as usize;
            if off + buf.len() > self.buf.len() {
                return Err(format!("write past end: off={off} len={}", buf.len()));
            }
            self.buf[off..off + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn size(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    // -------------------------------------------------------------------------
    // IoReadSeek: Read trait.
    // -------------------------------------------------------------------------

    #[test]
    fn ioreadseek_read_all_bytes_from_start() {
        let mut dev = MemDev::with_bytes(b"hello world" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        let mut out = Vec::new();
        rs.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn ioreadseek_read_partial_buffer() {
        let mut dev = MemDev::with_bytes(b"abcdefgh" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        let mut buf = [0u8; 4];
        let n = rs.read(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf, b"abcd");
    }

    #[test]
    fn ioreadseek_read_at_end_returns_zero() {
        let mut dev = MemDev::with_bytes(b"xy" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        // Seek to end.
        rs.seek(SeekFrom::End(0)).unwrap();
        let mut buf = [0u8; 4];
        let n = rs.read(&mut buf).unwrap();
        assert_eq!(n, 0, "read at end returns 0 bytes");
    }

    #[test]
    fn ioreadseek_sequential_reads_advance_cursor() {
        let mut dev = MemDev::with_bytes(b"ABCDE" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        let mut buf = [0u8; 2];
        rs.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"AB");
        rs.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"CD");
    }

    // -------------------------------------------------------------------------
    // IoReadSeek: Seek trait.
    // -------------------------------------------------------------------------

    #[test]
    fn ioreadseek_seek_start_positions_cursor() {
        let mut dev = MemDev::with_bytes(b"0123456789" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        let pos = rs.seek(SeekFrom::Start(5)).unwrap();
        assert_eq!(pos, 5);
        let mut buf = [0u8; 3];
        rs.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"567");
    }

    #[test]
    fn ioreadseek_seek_end_minus_n_reads_tail() {
        let mut dev = MemDev::with_bytes(b"0123456789" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        rs.seek(SeekFrom::End(-3)).unwrap();
        let mut buf = [0u8; 3];
        rs.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"789");
    }

    #[test]
    fn ioreadseek_seek_current_advances_relative() {
        let mut dev = MemDev::with_bytes(b"ABCDE" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        rs.seek(SeekFrom::Start(1)).unwrap();
        rs.seek(SeekFrom::Current(2)).unwrap();
        let mut buf = [0u8; 2];
        rs.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"DE");
    }

    #[test]
    fn ioreadseek_seek_before_start_returns_error() {
        let mut dev = MemDev::with_bytes(b"ABC" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        rs.seek(SeekFrom::Start(1)).unwrap();
        let err = rs.seek(SeekFrom::Current(-5));
        assert!(err.is_err(), "seek before start must fail");
    }

    #[test]
    fn ioreadseek_seek_end_zero_is_at_size() {
        let mut dev = MemDev::with_bytes(b"HELLO" as &[u8]);
        let mut rs = IoReadSeek::new(&mut dev);
        let pos = rs.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(pos, 5);
    }

    // -------------------------------------------------------------------------
    // PathIo: file-backed read/write via temp files.
    // -------------------------------------------------------------------------

    fn temp_path(suffix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("block_io_test_{suffix}_{}", std::process::id()));
        p
    }

    #[test]
    fn path_io_open_rw_size_matches_file_length() {
        let path = temp_path("size");
        std::fs::write(&path, b"0123456789").unwrap();
        let io = PathIo::open_rw(&path).unwrap();
        assert_eq!(io.size(), 10);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn path_io_open_ro_size_matches_file_length() {
        let path = temp_path("ro_size");
        std::fs::write(&path, [0u8; 64]).unwrap();
        let io = PathIo::open_ro(&path).unwrap();
        assert_eq!(io.size(), 64);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn path_io_write_then_read_roundtrip() {
        let path = temp_path("rw");
        std::fs::write(&path, [0u8; 512]).unwrap();
        let mut io = PathIo::open_rw(&path).unwrap();
        io.write_all_at(16, b"MAGIC").unwrap();
        let mut buf = [0u8; 5];
        io.read_exact_at(16, &mut buf).unwrap();
        assert_eq!(&buf, b"MAGIC");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn path_io_read_from_known_offset() {
        let path = temp_path("read_offset");
        let mut data = vec![0u8; 64];
        data[32..36].copy_from_slice(b"TEST");
        std::fs::write(&path, &data).unwrap();
        let mut io = PathIo::open_ro(&path).unwrap();
        let mut buf = [0u8; 4];
        io.read_exact_at(32, &mut buf).unwrap();
        assert_eq!(&buf, b"TEST");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn path_io_open_missing_file_returns_error() {
        let path = temp_path("nonexistent_xyz");
        assert!(PathIo::open_rw(&path).is_err());
        assert!(PathIo::open_ro(&path).is_err());
    }

    // -------------------------------------------------------------------------
    // BlockIo for &mut T blanket impl: passes through correctly.
    // -------------------------------------------------------------------------

    fn read_via_ref<T: BlockIo>(io: &mut T, offset: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        io.read_exact_at(offset, &mut buf).unwrap();
        buf
    }

    #[test]
    fn blockio_ref_mut_blanket_impl_delegates_correctly() {
        let mut dev = MemDev::with_bytes(b"payload" as &[u8]);
        // Pass &mut dev — exercises BlockIo for &mut T.
        let out = read_via_ref(&mut dev, 0, 7);
        assert_eq!(out, b"payload");
    }

    #[test]
    fn blockio_ref_mut_size_delegates() {
        let mut dev = MemDev::with_bytes(b"abc" as &[u8]);
        let r: &mut dyn BlockIo = &mut dev;
        assert_eq!(r.size(), 3);
    }

    // --- PathIo::sync --------------------------------------------------------

    #[test]
    fn path_io_sync_succeeds_on_open_file() {
        let path = temp_path("sync_test");
        std::fs::write(&path, b"data").unwrap();
        let mut io = PathIo::open_rw(&path).unwrap();
        assert!(io.sync().is_ok());
        let _ = std::fs::remove_file(&path);
    }

    // --- CallbackBlockIo -----------------------------------------------------

    // A MemDev-backed read callback: context is *mut Vec<u8>.
    unsafe extern "C" fn mem_read(
        ctx: *mut c_void,
        buf: *mut c_void,
        offset: u64,
        len: u64,
    ) -> c_int {
        let storage = &*(ctx as *const Vec<u8>);
        let off = offset as usize;
        let n = len as usize;
        if off + n > storage.len() {
            return -1;
        }
        std::ptr::copy_nonoverlapping(storage.as_ptr().add(off), buf as *mut u8, n);
        0
    }

    unsafe extern "C" fn mem_write(
        ctx: *mut c_void,
        buf: *const c_void,
        offset: u64,
        len: u64,
    ) -> c_int {
        let storage = &mut *(ctx as *mut Vec<u8>);
        let off = offset as usize;
        let n = len as usize;
        if off + n > storage.len() {
            return -1;
        }
        std::ptr::copy_nonoverlapping(buf as *const u8, storage.as_mut_ptr().add(off), n);
        0
    }

    unsafe extern "C" fn always_fail_read(
        _ctx: *mut c_void,
        _buf: *mut c_void,
        _offset: u64,
        _len: u64,
    ) -> c_int {
        -1
    }

    unsafe extern "C" fn always_fail_write(
        _ctx: *mut c_void,
        _buf: *const c_void,
        _offset: u64,
        _len: u64,
    ) -> c_int {
        -1
    }

    #[test]
    fn callback_block_io_read_returns_data() {
        let mut storage = b"Hello, NTFS!".to_vec();
        let mut cb = CallbackBlockIo {
            read_fn: mem_read,
            write_fn: Some(mem_write),
            context: &mut storage as *mut Vec<u8> as *mut c_void,
            size: storage.len() as u64,
        };
        let mut buf = [0u8; 5];
        cb.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"Hello");
    }

    #[test]
    fn callback_block_io_read_at_offset() {
        let mut storage = b"Hello, NTFS!".to_vec();
        let mut cb = CallbackBlockIo {
            read_fn: mem_read,
            write_fn: Some(mem_write),
            context: &mut storage as *mut Vec<u8> as *mut c_void,
            size: storage.len() as u64,
        };
        let mut buf = [0u8; 4];
        cb.read_exact_at(7, &mut buf).unwrap();
        assert_eq!(&buf, b"NTFS");
    }

    #[test]
    fn callback_block_io_write_then_read() {
        let mut storage = vec![0u8; 16];
        let mut cb = CallbackBlockIo {
            read_fn: mem_read,
            write_fn: Some(mem_write),
            context: &mut storage as *mut Vec<u8> as *mut c_void,
            size: storage.len() as u64,
        };
        cb.write_all_at(4, b"TEST").unwrap();
        let mut buf = [0u8; 4];
        cb.read_exact_at(4, &mut buf).unwrap();
        assert_eq!(&buf, b"TEST");
    }

    #[test]
    fn callback_block_io_write_on_readonly_fails() {
        let mut storage = vec![0u8; 16];
        let mut cb = CallbackBlockIo {
            read_fn: mem_read,
            write_fn: None, // read-only
            context: &mut storage as *mut Vec<u8> as *mut c_void,
            size: storage.len() as u64,
        };
        assert!(cb.write_all_at(0, b"x").is_err());
    }

    #[test]
    fn callback_block_io_read_failure_propagates() {
        let mut cb = CallbackBlockIo {
            read_fn: always_fail_read,
            write_fn: None,
            context: std::ptr::null_mut(),
            size: 64,
        };
        assert!(cb.read_exact_at(0, &mut [0u8; 4]).is_err());
    }

    #[test]
    fn callback_block_io_write_failure_propagates() {
        let mut cb = CallbackBlockIo {
            read_fn: always_fail_read,
            write_fn: Some(always_fail_write),
            context: std::ptr::null_mut(),
            size: 64,
        };
        let err = cb.write_all_at(0, b"data").unwrap_err();
        assert!(err.contains("write callback failed"), "{err}");
    }

    #[test]
    fn callback_block_io_size_returns_configured_size() {
        let cb = CallbackBlockIo {
            read_fn: always_fail_read,
            write_fn: None,
            context: std::ptr::null_mut(),
            size: 1234567,
        };
        assert_eq!(cb.size(), 1234567);
    }
}
