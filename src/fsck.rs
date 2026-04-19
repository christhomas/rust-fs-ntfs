//! Narrow write operations for recovering dirty volumes.
//!
//! Everything in this module breaks the otherwise read-only invariant of
//! the crate. Use only on volumes that are **not** currently mounted.
//!
//! The two operations we support are the minimum needed to recover from a
//! crashed drive-handle close:
//!
//! 1. [`clear_dirty`] — clear the `VOLUME_IS_DIRTY` flag in
//!    `$Volume/$VOLUME_INFORMATION` (bit `0x0001` of the u16 `flags` field,
//!    per MS-FSCC / [Flatcap $VOLUME_INFORMATION]). Without this, Windows
//!    and several NTFS drivers refuse to mount.
//! 2. [`reset_logfile`] — overwrite `$LogFile`'s `$DATA` with `0xFF` bytes.
//!    The all-`0xFF` pattern is the format-level "no transactions pending,
//!    reinitialize on mount" signal documented at
//!    [Flatcap $LogFile](https://flatcap.github.io/linux-ntfs/ntfs/files/logfile.html).
//!
//! Neither operation replays in-progress transactions. If the crash
//! happened mid-MFT-update, whatever metadata hit the disk survives; the
//! log is discarded. This is the weakest possible recovery; it trades
//! some data recoverability for the ability to remount at all.
//!
//! All writes are bounded, well-located, and immediately `fsync`'d. No
//! MFT-record USA fixup recompute is required here because:
//! * `clear_dirty` patches a byte that lives well inside the MFT record,
//!   not in the last 2 bytes of any 512-byte sector (the positions USA
//!   replaces). A 2-byte write inside a single sector is atomic on any
//!   modern storage.
//! * `reset_logfile` writes to non-resident data clusters entirely outside
//!   the MFT.
//!
//! # I/O abstraction
//!
//! The core recovery steps are implemented against an [`FsckIo`] trait so
//! callers with a non-file block device (FSKit extension, Go backend,
//! memory-mapped image) can drive the same logic via callbacks. See the
//! C ABI `fs_ntfs_fsck_with_callbacks` for the external entry point.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use ntfs::structured_values::NtfsVolumeFlags;
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType};

/// Offset of the 2-byte `flags` field within the `$VOLUME_INFORMATION` structure.
/// Layout (MS-FSCC / Flatcap): reserved(8) + major(1) + minor(1) + flags(2).
const VOLUME_FLAGS_OFFSET: u64 = 10;

/// Offset of the `value_offset` u16 field inside an NTFS resident attribute
/// header ([Flatcap Resident Attribute](https://flatcap.github.io/linux-ntfs/ntfs/concepts/attribute_header.html)).
/// Layout: type(4) + length(4) + non_resident(1) + name_length(1) +
/// name_offset(2) + flags(2) + attribute_id(2) + value_length(4) +
/// value_offset(2) + resident_flags(1) + reserved(1).
const RESIDENT_ATTR_VALUE_OFFSET_FIELD: u64 = 0x14;

/// NTFS format-level "empty log" sentinel byte. An all-`0xFF` `$LogFile`
/// is the documented "no transactions pending" signal at
/// [Flatcap $LogFile](https://flatcap.github.io/linux-ntfs/ntfs/files/logfile.html).
const LOGFILE_EMPTY_FILL: u8 = 0xFF;

/// Chunk size used when overwriting `$LogFile` and when emitting progress
/// updates during that overwrite.
const LOGFILE_CHUNK: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// FsckIo trait + impls
// ---------------------------------------------------------------------------

/// Block-device-like I/O used by the fsck routines.
///
/// Implementors serve positioned reads/writes against whatever storage
/// layer they wrap — a file, a FSKit `FSBlockDeviceResource`, an in-memory
/// buffer. `read_exact_at` must read exactly `buf.len()` bytes starting at
/// `offset`; `write_all_at` must write exactly `buf.len()` bytes starting
/// at `offset`. Both return `Err(String)` on any error.
///
/// `size` is the total byte length of the device, used to back the NTFS
/// parser's `Seek::End` semantics.
pub trait FsckIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String>;
    fn size(&self) -> u64;
    /// Flush any pending writes to stable storage. Path-backed impls call
    /// `fsync`; callback-backed impls delegate to the host (FSKit drains
    /// on the sync barrier).
    fn sync(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// `FsckIo` backed by a real filesystem path. The file is opened RW on
/// construction; reads and writes are positioned via `Seek` + `read_exact`
/// / `write_all`.
pub struct PathIo {
    file: File,
    size: u64,
}

impl PathIo {
    pub fn open(path: &Path) -> Result<Self, String> {
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
}

impl FsckIo for PathIo {
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

/// Adapter that lets an `FsckIo` be used where `Read + Seek` is required
/// (specifically: upstream `ntfs::Ntfs::new` and `ntfs::Ntfs::file`).
///
/// Kept internal to this module because it only implements the subset of
/// `Read + Seek` that the NTFS parse path exercises.
struct IoReader<'a, T: FsckIo> {
    io: &'a mut T,
    position: u64,
}

impl<'a, T: FsckIo> IoReader<'a, T> {
    fn new(io: &'a mut T) -> Self {
        Self { io, position: 0 }
    }
}

impl<T: FsckIo> Read for IoReader<'_, T> {
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

impl<T: FsckIo> Seek for IoReader<'_, T> {
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

// ---------------------------------------------------------------------------
// Public path-based API (unchanged surface)
// ---------------------------------------------------------------------------

/// Return `true` if the volume's `VOLUME_IS_DIRTY` flag (0x0001) is
/// set. Lightweight probe — parses the boot sector + `$Volume` but
/// doesn't mount the volume or load the upcase table.
pub fn is_dirty(path: impl AsRef<Path>) -> Result<bool, String> {
    let mut io = PathIo::open(path.as_ref())?;
    is_dirty_io(&mut io)
}

/// Clear the `VOLUME_IS_DIRTY` flag on the given NTFS image.
///
/// Returns `Ok(true)` if the flag was set and has been cleared,
/// `Ok(false)` if the volume was already clean, `Err` otherwise.
pub fn clear_dirty(path: impl AsRef<Path>) -> Result<bool, String> {
    let mut io = PathIo::open(path.as_ref())?;
    clear_dirty_io(&mut io)
}

/// Overwrite `$LogFile` with `0xFF` bytes so Windows / ntfs-3g reinitialize
/// it on next mount (matches `ntfsfix -d`). Returns the number of bytes
/// overwritten.
pub fn reset_logfile(path: impl AsRef<Path>) -> Result<u64, String> {
    let mut io = PathIo::open(path.as_ref())?;
    reset_logfile_io(&mut io, None)
}

/// Convenience: run both [`clear_dirty`] and [`reset_logfile`] on the same
/// image. `reset_logfile` runs first because clearing the dirty bit without
/// also resetting the log would leave Windows thinking "clean volume" but
/// still finding stale log records on mount.
pub fn fsck(path: impl AsRef<Path>) -> Result<FsckReport, String> {
    let mut io = PathIo::open(path.as_ref())?;
    fsck_io(&mut io, None)
}

/// Summary of what [`fsck`] did.
#[derive(Debug, Clone, Copy)]
pub struct FsckReport {
    pub logfile_bytes: u64,
    pub dirty_cleared: bool,
}

// ---------------------------------------------------------------------------
// Generic (FsckIo-based) API
// ---------------------------------------------------------------------------

/// Progress callback type used by the long-running phases of [`fsck_io`]
/// / [`reset_logfile_io`]. Signature: `(phase, done, total)`:
/// * `phase` — short identifier string, e.g. `"reset_logfile"` or
///   `"clear_dirty"`.
/// * `done` / `total` — bytes (or 0/1 for trivial single-write phases)
///   completed in the named phase.
///
/// Passed as `Option<&mut dyn FnMut(&str, u64, u64)>` to the IO fns.
///
/// `is_dirty` over an arbitrary `FsckIo`.
pub fn is_dirty_io<T: FsckIo>(io: &mut T) -> Result<bool, String> {
    let (_, current) = locate_volume_flags_io(io)?;
    Ok(NtfsVolumeFlags::from_bits_truncate(current).contains(NtfsVolumeFlags::IS_DIRTY))
}

/// `clear_dirty` over an arbitrary `FsckIo`.
pub fn clear_dirty_io<T: FsckIo>(io: &mut T) -> Result<bool, String> {
    let (flag_disk_offset, current_flags) = locate_volume_flags_io(io)?;
    let flags = NtfsVolumeFlags::from_bits_truncate(current_flags);
    if !flags.contains(NtfsVolumeFlags::IS_DIRTY) {
        return Ok(false);
    }
    let new_flags = current_flags & !NtfsVolumeFlags::IS_DIRTY.bits();
    io.write_all_at(flag_disk_offset, &new_flags.to_le_bytes())
        .map_err(|e| format!("write volume flags: {e}"))?;
    io.sync().map_err(|e| format!("fsync: {e}"))?;
    Ok(true)
}

/// `reset_logfile` over an arbitrary `FsckIo`, with optional progress
/// emission. The callback is fired at least once (start, `done=0`) and
/// once at the end (`done=total`), plus one per `LOGFILE_CHUNK` of
/// overwrite in between.
#[allow(clippy::type_complexity)]
pub fn reset_logfile_io<'cb, T: FsckIo>(
    io: &mut T,
    mut progress: Option<&mut (dyn FnMut(&str, u64, u64) + 'cb)>,
) -> Result<u64, String> {
    let (logfile_disk_offset, logfile_size) = locate_logfile_data_io(io)?;

    if let Some(cb) = progress.as_deref_mut() {
        cb("reset_logfile", 0, logfile_size);
    }

    let buf = [LOGFILE_EMPTY_FILL; LOGFILE_CHUNK];
    let mut written: u64 = 0;
    while written < logfile_size {
        let n = std::cmp::min(logfile_size - written, LOGFILE_CHUNK as u64) as usize;
        io.write_all_at(logfile_disk_offset + written, &buf[..n])
            .map_err(|e| format!("write logfile: {e}"))?;
        written += n as u64;
        if let Some(cb) = progress.as_deref_mut() {
            // Emit progress after every chunk; for small $LogFiles this
            // is fine (a 2 MiB log → 32 callbacks at 64 KiB each).
            cb("reset_logfile", written, logfile_size);
        }
    }
    io.sync().map_err(|e| format!("fsync: {e}"))?;
    Ok(logfile_size)
}

/// `fsck` over an arbitrary `FsckIo`, with optional progress emission.
#[allow(clippy::type_complexity)]
pub fn fsck_io<'cb, T: FsckIo>(
    io: &mut T,
    mut progress: Option<&mut (dyn FnMut(&str, u64, u64) + 'cb)>,
) -> Result<FsckReport, String> {
    let logfile_bytes = reset_logfile_io(io, progress.as_deref_mut())?;

    if let Some(cb) = progress.as_mut().map(|c| &mut **c as &mut (dyn FnMut(&str, u64, u64) + 'cb)) {
        cb("clear_dirty", 0, 1);
    }
    let dirty_cleared = clear_dirty_io(io)?;
    if let Some(cb) = progress.as_mut().map(|c| &mut **c as &mut (dyn FnMut(&str, u64, u64) + 'cb)) {
        cb("clear_dirty", 1, 1);
    }

    Ok(FsckReport {
        logfile_bytes,
        dirty_cleared,
    })
}

// ---------------------------------------------------------------------------
// Internals — NTFS parsing via an IoReader<'_, T: FsckIo>
// ---------------------------------------------------------------------------

/// Parse the volume, locate `$Volume`'s `$VOLUME_INFORMATION` attribute,
/// and return (on-disk byte offset of the 2-byte flags field, current
/// flags value).
fn locate_volume_flags_io<T: FsckIo>(io: &mut T) -> Result<(u64, u16), String> {
    // Phase 1: parse NTFS to find the attribute header position.
    let attr_pos = {
        let mut reader = IoReader::new(io);
        let ntfs = Ntfs::new(&mut reader).map_err(|e| format!("parse ntfs: {e}"))?;
        let volume_file = ntfs
            .file(&mut reader, KnownNtfsFileRecordNumber::Volume as u64)
            .map_err(|e| format!("open $Volume: {e}"))?;

        let mut attrs = volume_file.attributes();
        let mut found = None;
        while let Some(item) = attrs.next(&mut reader) {
            let item = item.map_err(|e| format!("attr iter: {e}"))?;
            let attribute = item.to_attribute().map_err(|e| format!("to_attr: {e}"))?;
            if attribute.ty().ok() != Some(NtfsAttributeType::VolumeInformation) {
                continue;
            }
            // NOTE: upstream's `NtfsResidentAttributeValue::data_position()`
            // returns the *attribute header* position, not the *data*
            // position — the name is misleading. To find the actual disk
            // offset of the resident data, read the attribute's
            // `value_offset` field (u16 LE at header +0x14) and add it to
            // the attribute start.
            let pos = attribute
                .position()
                .value()
                .ok_or_else(|| "$VOLUME_INFORMATION attribute has no position".to_string())?
                .get();
            found = Some(pos);
            break;
        }
        found.ok_or_else(|| "$VOLUME_INFORMATION attribute not found on $Volume".to_string())?
    };

    // Phase 2: read value_offset + current flags directly through the IO.
    let value_offset = read_u16_le_io(io, attr_pos + RESIDENT_ATTR_VALUE_OFFSET_FIELD)
        .map_err(|e| format!("read value_offset: {e}"))?;
    let data_start = attr_pos + value_offset as u64;
    let flag_offset = data_start + VOLUME_FLAGS_OFFSET;

    let current = read_u16_le_io(io, flag_offset).map_err(|e| format!("read volume flags: {e}"))?;
    Ok((flag_offset, current))
}

/// Locate `$LogFile`'s `$DATA` on disk. Returns (on-disk byte offset of
/// the first data byte, total byte length to overwrite).
fn locate_logfile_data_io<T: FsckIo>(io: &mut T) -> Result<(u64, u64), String> {
    let mut reader = IoReader::new(io);
    let ntfs = Ntfs::new(&mut reader).map_err(|e| format!("parse ntfs: {e}"))?;
    let logfile = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::LogFile as u64)
        .map_err(|e| format!("open $LogFile: {e}"))?;

    let mut attrs = logfile.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.map_err(|e| format!("attr iter: {e}"))?;
        let attribute = item.to_attribute().map_err(|e| format!("to_attr: {e}"))?;
        if attribute.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !attribute.name().map(|n| n.is_empty()).unwrap_or(true) {
            // Skip named streams; we want only the unnamed $DATA.
            continue;
        }
        // $LogFile is large (typically 2 MiB+) and therefore always
        // non-resident.
        let value = attribute
            .value(&mut reader)
            .map_err(|e| format!("attr value: {e}"))?;
        let data_pos = value
            .data_position()
            .value()
            .ok_or_else(|| "$LogFile $DATA has no first-run position".to_string())?;
        let length = attribute.value_length();
        if length == 0 {
            return Err("$LogFile $DATA has zero length".to_string());
        }
        return Ok((data_pos.get(), length));
    }
    Err("unnamed $DATA attribute not found on $LogFile".to_string())
}

fn read_u16_le_io<T: FsckIo>(io: &mut T, offset: u64) -> Result<u16, String> {
    let mut buf = [0u8; 2];
    io.read_exact_at(offset, &mut buf)?;
    Ok(u16::from_le_bytes(buf))
}
