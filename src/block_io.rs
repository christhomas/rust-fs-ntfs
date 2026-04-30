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
