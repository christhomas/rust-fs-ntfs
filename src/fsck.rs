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

use std::fs::OpenOptions;
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

/// Return `true` if the volume's `VOLUME_IS_DIRTY` flag (0x0001) is
/// set. Lightweight probe — parses the boot sector + `$Volume` but
/// doesn't mount the volume or load the upcase table.
pub fn is_dirty(path: impl AsRef<Path>) -> Result<bool, String> {
    let path = path.as_ref();
    let (_, current_flags) = locate_volume_flags(path)?;
    Ok(NtfsVolumeFlags::from_bits_truncate(current_flags).contains(NtfsVolumeFlags::IS_DIRTY))
}

/// Clear the `VOLUME_IS_DIRTY` flag on the given NTFS image.
///
/// Returns `Ok(true)` if the flag was set and has been cleared,
/// `Ok(false)` if the volume was already clean, `Err` otherwise.
pub fn clear_dirty(path: impl AsRef<Path>) -> Result<bool, String> {
    let path = path.as_ref();

    // Phase 1: parse read-only to locate the on-disk byte offset we need
    // to patch. Drop the read-only handle before we open RW to avoid any
    // ambiguity about buffer coherency.
    let (flag_disk_offset, current_flags) = locate_volume_flags(path)?;

    let flags = NtfsVolumeFlags::from_bits_truncate(current_flags);
    if !flags.contains(NtfsVolumeFlags::IS_DIRTY) {
        return Ok(false);
    }

    // Phase 2: narrow RW patch.
    let new_flags = current_flags & !NtfsVolumeFlags::IS_DIRTY.bits();
    write_u16_le(path, flag_disk_offset, new_flags)
        .map_err(|e| format!("write volume flags: {e}"))?;
    Ok(true)
}

/// Overwrite `$LogFile` with `0xFF` bytes so Windows / ntfs-3g reinitialize
/// it on next mount (matches `ntfsfix -d`). Returns the number of bytes
/// overwritten.
pub fn reset_logfile(path: impl AsRef<Path>) -> Result<u64, String> {
    let path = path.as_ref();

    let (logfile_disk_offset, logfile_size) = locate_logfile_data(path)?;

    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open rw: {e}"))?;
    f.seek(SeekFrom::Start(logfile_disk_offset))
        .map_err(|e| format!("seek logfile: {e}"))?;

    // Write in 64 KiB chunks so we don't allocate a 64 MiB buffer for a
    // typical $LogFile.
    const CHUNK: usize = 64 * 1024;
    let buf = [LOGFILE_EMPTY_FILL; CHUNK];
    let mut remaining = logfile_size;
    while remaining > 0 {
        let n = remaining.min(CHUNK as u64) as usize;
        f.write_all(&buf[..n])
            .map_err(|e| format!("write logfile: {e}"))?;
        remaining -= n as u64;
    }
    f.sync_all().map_err(|e| format!("fsync: {e}"))?;
    Ok(logfile_size)
}

/// Convenience: run both [`clear_dirty`] and [`reset_logfile`] on the same
/// image. `reset_logfile` runs first because clearing the dirty bit without
/// also resetting the log would leave Windows thinking "clean volume" but
/// still finding stale log records on mount.
pub fn fsck(path: impl AsRef<Path>) -> Result<FsckReport, String> {
    let path = path.as_ref();
    let logfile_bytes = reset_logfile(path)?;
    let dirty_cleared = clear_dirty(path)?;
    Ok(FsckReport {
        logfile_bytes,
        dirty_cleared,
    })
}

/// Summary of what [`fsck`] did.
#[derive(Debug, Clone, Copy)]
pub struct FsckReport {
    pub logfile_bytes: u64,
    pub dirty_cleared: bool,
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Open read-only, parse with upstream, find $Volume's $VOLUME_INFORMATION
/// attribute, and return (on-disk byte offset of the 2-byte flags field,
/// current flags value).
fn locate_volume_flags(path: &Path) -> Result<(u64, u16), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open ro: {e}"))?;
    let mut reader = std::io::BufReader::new(f);
    let ntfs = Ntfs::new(&mut reader).map_err(|e| format!("parse ntfs: {e}"))?;
    // We don't need the upcase table for this operation; skip `read_upcase_table`.
    let volume_file = ntfs
        .file(&mut reader, KnownNtfsFileRecordNumber::Volume as u64)
        .map_err(|e| format!("open $Volume: {e}"))?;

    let mut attrs = volume_file.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.map_err(|e| format!("attr iter: {e}"))?;
        let attribute = item.to_attribute().map_err(|e| format!("to_attr: {e}"))?;
        if attribute.ty().ok() != Some(NtfsAttributeType::VolumeInformation) {
            continue;
        }
        // NOTE: upstream's `NtfsResidentAttributeValue::data_position()` returns
        // the *attribute header* position, not the *data* position — the name
        // is misleading. To find the actual disk offset of the resident data,
        // read the attribute's `value_offset` field (u16 LE at header +0x14)
        // and add it to the attribute start.
        let attr_pos = attribute
            .position()
            .value()
            .ok_or_else(|| "$VOLUME_INFORMATION attribute has no position".to_string())?
            .get();
        drop(reader); // release the RO borrow before we open a fresh handle

        let value_offset = read_u16_le(path, attr_pos + RESIDENT_ATTR_VALUE_OFFSET_FIELD)
            .map_err(|e| format!("read value_offset: {e}"))?;
        let data_start = attr_pos + value_offset as u64;
        let flag_offset = data_start + VOLUME_FLAGS_OFFSET;

        // Read current flags so we know whether anything actually needs to
        // change AND so `clear_dirty` can return an early "already clean"
        // without touching disk.
        let current = read_u16_le(path, flag_offset).map_err(|e| format!("read flags: {e}"))?;
        return Ok((flag_offset, current));
    }
    Err("$VOLUME_INFORMATION attribute not found on $Volume".to_string())
}

/// Locate `$LogFile`'s `$DATA` on disk. Returns (on-disk byte offset of the
/// first data byte, total byte length to overwrite).
fn locate_logfile_data(path: &Path) -> Result<(u64, u64), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open ro: {e}"))?;
    let mut reader = std::io::BufReader::new(f);
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
        // $LogFile is large (typically 2 MiB+) and therefore always non-resident.
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

fn write_u16_le(path: &Path, offset: u64, value: u16) -> std::io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&value.to_le_bytes())?;
    f.sync_all()
}

fn read_u16_le(path: &Path, offset: u64) -> std::io::Result<u16> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; 2];
    f.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}
