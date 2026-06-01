//! Write operations that modify existing MFT records in-place. No
//! attribute resize, no cluster allocation, no new files. See status.md
//! Phase W1 for the exact scope.
//!
//! Path resolution uses upstream `ntfs` (read-only). The actual write
//! goes through `mft_io::update_mft_record` which handles USA fixup
//! and `fsync`. The mutator closures here use [`attr_io`] to locate
//! attributes without touching upstream.

use crate::attr_io::{self, AttrType};
use crate::bitmap;
use crate::block_io::{BlockIo, IoReadSeek, PathIo};
use crate::data_runs::{self, DataRun};
use crate::idx_block;
use crate::index_io;
use crate::mft_bitmap;
use crate::mft_io::{read_mft_record_io, update_mft_record_io, MFT_FLAG_DIRECTORY};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsFile};
use std::path::Path;

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// NTFS file times in 100 ns intervals since 1601-01-01 UTC.
/// See [MS-DTYP FILETIME](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/2c57429b-fdd4-488f-b5fc-9e4cf020fcdf).
/// Any `None` field is left unchanged by [`set_times_by_record_number`].
#[derive(Debug, Clone, Copy, Default)]
pub struct FileTimes {
    pub creation: Option<u64>,
    pub modification: Option<u64>,
    pub mft_record_modification: Option<u64>,
    pub access: Option<u64>,
}

// $STANDARD_INFORMATION field offsets (per Windows Internals 7th ed. and MS-FSCC).
const SI_CREATION: usize = 0x00;
const SI_MODIFICATION: usize = 0x08;
const SI_MFT_MODIFICATION: usize = 0x10;
const SI_ACCESS: usize = 0x18;
const SI_FILE_ATTRIBUTES: usize = 0x20;

/// `$STANDARD_INFORMATION.security_id` offset (NTFS 3.x form only —
/// the 48-byte v1.x form omits this field). u32 at value bytes
/// 0x34..0x38 per MS-FSCC §2.4.2.
const SI_SECURITY_ID: usize = 0x34;

/// Set file times on the file at `file_path`. Only modifies
/// `$STANDARD_INFORMATION`; does not touch the duplicate times in the
/// parent directory's `$FILE_NAME` index (Windows itself only updates
/// them on rename/create).
pub fn set_times(path: &Path, file_path: &str, times: FileTimes) -> Result<(), String> {
    let mut io = PathIo::open_rw(path)?;
    set_times_io(&mut io, file_path, times)
}

pub fn set_times_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    times: FileTimes,
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    set_times_by_record_number_io(io, rec, times)
}

/// Set file times on an MFT record by number (bypasses path resolution).
pub fn set_times_by_record_number(
    path: &Path,
    record_number: u64,
    times: FileTimes,
) -> Result<(), String> {
    let mut io = PathIo::open_rw(path)?;
    set_times_by_record_number_io(&mut io, record_number, times)
}

pub fn set_times_by_record_number_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    times: FileTimes,
) -> Result<(), String> {
    update_mft_record_io(io, record_number, |record| {
        let loc = attr_io::find_attribute(record, AttrType::StandardInformation, None)
            .ok_or_else(|| "$STANDARD_INFORMATION not found".to_string())?;
        let data_start = attr_io::resident_value_start(&loc)
            .ok_or_else(|| "$STANDARD_INFORMATION not resident".to_string())?;
        let value_length = loc.resident_value_length.ok_or("no value length")? as usize;
        // First 32 bytes hold the four u64 timestamps; present in every
        // NTFS version (1.x = 48 bytes, 3.x+ = 72 bytes).
        if value_length < 0x20 {
            return Err(format!(
                "$STANDARD_INFORMATION unexpectedly short: {value_length}"
            ));
        }
        write_u64_at(record, data_start + SI_CREATION, times.creation);
        write_u64_at(record, data_start + SI_MODIFICATION, times.modification);
        write_u64_at(
            record,
            data_start + SI_MFT_MODIFICATION,
            times.mft_record_modification,
        );
        write_u64_at(record, data_start + SI_ACCESS, times.access);
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// File attributes (FILE_ATTRIBUTE_* bits)
// ---------------------------------------------------------------------------

/// NTFS `$STANDARD_INFORMATION.file_attributes` flag bits. Values match
/// Windows FILE_ATTRIBUTE_* constants
/// ([MS-FSCC 2.6](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/ca28ec38-f155-4768-81d6-4bfeb8586fc9)).
#[derive(Debug, Clone, Copy, Default)]
pub struct FileAttributesChange {
    pub add: u32,
    pub remove: u32,
}

/// Common flag values for convenience.
pub mod file_attr {
    pub const READONLY: u32 = 0x0000_0001;
    pub const HIDDEN: u32 = 0x0000_0002;
    pub const SYSTEM: u32 = 0x0000_0004;
    pub const ARCHIVE: u32 = 0x0000_0020;
    pub const NORMAL: u32 = 0x0000_0080;
    pub const TEMPORARY: u32 = 0x0000_0100;
    pub const NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
}

/// Read the `security_id` field from a file's `$STANDARD_INFORMATION`.
/// Returns `Ok(None)` if the file's `$STANDARD_INFORMATION` value uses
/// the 48-byte NTFS 1.x form (which omits the security_id field) —
/// only the 72-byte NTFS 3.x form has it.
///
/// The returned `u32` is the index into `$Secure:$SDS` / `$Secure:$SII`;
/// `0` is "no security descriptor assigned" (treated as the default
/// inherited DACL), and `0x100` is the canonical entry mkfs ships for
/// system files.
pub fn read_security_id(path: &Path, file_path: &str) -> Result<Option<u32>, String> {
    let mut io = PathIo::open_ro(path)?;
    read_security_id_io(&mut io, file_path)
}

pub fn read_security_id_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Option<u32>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    let loc = attr_io::find_attribute(&record, AttrType::StandardInformation, None)
        .ok_or_else(|| "$STANDARD_INFORMATION not found".to_string())?;
    let data_start = attr_io::resident_value_start(&loc)
        .ok_or_else(|| "$STANDARD_INFORMATION not resident".to_string())?;
    let value_length = loc.resident_value_length.ok_or("no value length")? as usize;
    if value_length < SI_SECURITY_ID + 4 {
        // 48-byte v1.x form: security_id field is absent. Caller can
        // either accept this as "default DACL" or call set_security_id
        // to grow the attribute to 72 bytes.
        return Ok(None);
    }
    let off = data_start + SI_SECURITY_ID;
    let id = u32::from_le_bytes([
        record[off],
        record[off + 1],
        record[off + 2],
        record[off + 3],
    ]);
    Ok(Some(id))
}

/// Full decoded `$STANDARD_INFORMATION` value (MS-FSCC §2.4.2).
///
/// The optional `v3` block carries the 24 trailing bytes (`owner_id`,
/// `security_id`, `quota`, `usn`) that only exist in the 72-byte
/// NTFS 3.x form; it is `None` for the 48-byte 1.x form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardInformationFull {
    pub creation_time: u64,
    pub modification_time: u64,
    pub mft_modification_time: u64,
    pub access_time: u64,
    pub file_attributes: u32,
    pub maximum_versions: u32,
    pub version_number: u32,
    pub class_id: u32,
    pub v3: Option<StandardInformationV3>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardInformationV3 {
    pub owner_id: u32,
    pub security_id: u32,
    pub quota: u64,
    pub usn: u64,
}

// $STANDARD_INFORMATION v3.x field offsets (continuing from
// SI_CREATION..SI_SECURITY_ID above).
const SI_MAX_VERSIONS: usize = 0x24;
const SI_VERSION: usize = 0x28;
const SI_CLASS_ID: usize = 0x2C;
const SI_OWNER_ID: usize = 0x30;
const SI_QUOTA: usize = 0x38;
const SI_USN: usize = 0x40;

/// Read every field of a file's `$STANDARD_INFORMATION`. Unlike the
/// targeted `read_security_id`, this exposes the full 48-byte common
/// header plus the optional 24-byte NTFS 3.x trailer (Owner/Security
/// IDs, Quota, USN) when present.
pub fn read_si_full(image: &Path, file_path: &str) -> Result<StandardInformationFull, String> {
    let mut io = PathIo::open_ro(image)?;
    read_si_full_io(&mut io, file_path)
}

pub fn read_si_full_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<StandardInformationFull, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    let loc = attr_io::find_attribute(&record, AttrType::StandardInformation, None)
        .ok_or_else(|| "$STANDARD_INFORMATION not found".to_string())?;
    let data_start = attr_io::resident_value_start(&loc)
        .ok_or_else(|| "$STANDARD_INFORMATION not resident".to_string())?;
    let value_length = loc.resident_value_length.ok_or("no value length")? as usize;
    // v1.x = 48 bytes (stops after class_id at 0x30). v3.x = 72 bytes
    // (adds owner_id/security_id/quota/usn). Anything below 48 is
    // structurally broken.
    if value_length < 0x30 {
        return Err(format!(
            "$STANDARD_INFORMATION value too short: {value_length} bytes (need ≥ 48)"
        ));
    }
    let read_u32 = |off: usize| {
        u32::from_le_bytes([
            record[data_start + off],
            record[data_start + off + 1],
            record[data_start + off + 2],
            record[data_start + off + 3],
        ])
    };
    let read_u64 = |off: usize| {
        u64::from_le_bytes([
            record[data_start + off],
            record[data_start + off + 1],
            record[data_start + off + 2],
            record[data_start + off + 3],
            record[data_start + off + 4],
            record[data_start + off + 5],
            record[data_start + off + 6],
            record[data_start + off + 7],
        ])
    };
    let common = StandardInformationFull {
        creation_time: read_u64(SI_CREATION),
        modification_time: read_u64(SI_MODIFICATION),
        mft_modification_time: read_u64(SI_MFT_MODIFICATION),
        access_time: read_u64(SI_ACCESS),
        file_attributes: read_u32(SI_FILE_ATTRIBUTES),
        maximum_versions: read_u32(SI_MAX_VERSIONS),
        version_number: read_u32(SI_VERSION),
        class_id: read_u32(SI_CLASS_ID),
        v3: None,
    };
    if value_length >= 0x48 {
        Ok(StandardInformationFull {
            v3: Some(StandardInformationV3 {
                owner_id: read_u32(SI_OWNER_ID),
                security_id: read_u32(SI_SECURITY_ID),
                quota: read_u64(SI_QUOTA),
                usn: read_u64(SI_USN),
            }),
            ..common
        })
    } else {
        Ok(common)
    }
}

/// Write the `security_id` field in `$STANDARD_INFORMATION`. The
/// `security_id` is an index into `$Secure:$SDS` / `$Secure:$SII`;
/// mkfs ships a single canonical entry at `0x100` (the system-files
/// DACL), so a typical use is `set_security_id(image, path, 0x100)`
/// to point a runtime-created file at that catalog entry.
///
/// Requires the file's `$STANDARD_INFORMATION` to be in the 72-byte
/// NTFS 3.x form. Runtime-created files (via `create_file` /
/// `mkdir`) ship that form unconditionally; system files written by
/// mkfs use the 48-byte NTFS 1.x form and can't be retargeted via
/// this API (`security_id` field is absent). Returns
/// `Err("STANDARD_INFORMATION too small …")` in that case.
///
/// NOTE: this writer assumes the new `security_id` already has a
/// corresponding entry in `$Secure:$SDS` / `$SDH` / `$SII`. Adding
/// new SD entries is a larger piece of work (§3.4 "full ACL
/// support") — this API is the minimal "point a file at the
/// existing catalog entry" surface.
pub fn set_security_id(path: &Path, file_path: &str, security_id: u32) -> Result<(), String> {
    let mut io = PathIo::open_rw(path)?;
    set_security_id_io(&mut io, file_path, security_id)
}

pub fn set_security_id_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    security_id: u32,
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        let loc = attr_io::find_attribute(record, AttrType::StandardInformation, None)
            .ok_or_else(|| "$STANDARD_INFORMATION not found".to_string())?;
        let data_start = attr_io::resident_value_start(&loc)
            .ok_or_else(|| "$STANDARD_INFORMATION not resident".to_string())?;
        let value_length = loc.resident_value_length.ok_or("no value length")? as usize;
        if value_length < SI_SECURITY_ID + 4 {
            return Err(format!(
                "$STANDARD_INFORMATION too small for security_id: {value_length} bytes (need ≥ {})",
                SI_SECURITY_ID + 4
            ));
        }
        let off = data_start + SI_SECURITY_ID;
        record[off..off + 4].copy_from_slice(&security_id.to_le_bytes());
        Ok(())
    })
}

/// Modify the `file_attributes` field in `$STANDARD_INFORMATION`. Bits in
/// `add` are ORed on; bits in `remove` are ANDed off. `add` and `remove`
/// overlap is not allowed (caller must not ask to both add and remove
/// the same bit).
pub fn set_file_attributes(
    path: &Path,
    file_path: &str,
    change: FileAttributesChange,
) -> Result<(), String> {
    let mut io = PathIo::open_rw(path)?;
    set_file_attributes_io(&mut io, file_path, change)
}

pub fn set_file_attributes_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    change: FileAttributesChange,
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    set_file_attributes_by_record_number_io(io, rec, change)
}

/// See [`set_file_attributes`].
pub fn set_file_attributes_by_record_number(
    path: &Path,
    record_number: u64,
    change: FileAttributesChange,
) -> Result<(), String> {
    let mut io = PathIo::open_rw(path)?;
    set_file_attributes_by_record_number_io(&mut io, record_number, change)
}

pub fn set_file_attributes_by_record_number_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    change: FileAttributesChange,
) -> Result<(), String> {
    if change.add & change.remove != 0 {
        return Err(format!(
            "add and remove overlap: add={:#x} remove={:#x}",
            change.add, change.remove
        ));
    }
    update_mft_record_io(io, record_number, |record| {
        let loc = attr_io::find_attribute(record, AttrType::StandardInformation, None)
            .ok_or_else(|| "$STANDARD_INFORMATION not found".to_string())?;
        let data_start = attr_io::resident_value_start(&loc)
            .ok_or_else(|| "$STANDARD_INFORMATION not resident".to_string())?;
        let value_length = loc.resident_value_length.ok_or("no value length")? as usize;
        if value_length < SI_FILE_ATTRIBUTES + 4 {
            return Err(format!(
                "$STANDARD_INFORMATION too short for file_attributes field: {value_length}"
            ));
        }
        let off = data_start + SI_FILE_ATTRIBUTES;
        let current = u32::from_le_bytes([
            record[off],
            record[off + 1],
            record[off + 2],
            record[off + 3],
        ]);
        let new = (current | change.add) & !change.remove;
        record[off..off + 4].copy_from_slice(&new.to_le_bytes());
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Content rewrite (non-resident $DATA, size-preserving)
// ---------------------------------------------------------------------------

/// Rewrite bytes inside the existing logical range of a non-resident
/// `$DATA` attribute. Does not extend the file; does not touch
/// compressed or sparse-range bytes (those require W2 machinery).
///
/// `offset` is a byte offset within the file's logical data; `data` is
/// written starting there. Returns the number of bytes written on
/// success. A zero-length write is a no-op.
pub fn write_at(image: &Path, file_path: &str, offset: u64, data: &[u8]) -> Result<u64, String> {
    if data.is_empty() {
        return Ok(0);
    }
    let mut io = PathIo::open_rw(image)?;
    write_at_io(&mut io, file_path, offset, data)
}

pub fn write_at_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    offset: u64,
    data: &[u8],
) -> Result<u64, String> {
    if data.is_empty() {
        return Ok(0);
    }
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    write_at_by_record_number_io(io, rec, offset, data)
}

/// See [`write_at`]. Takes a record number instead of a path.
pub fn write_at_by_record_number(
    image: &Path,
    record_number: u64,
    offset: u64,
    data: &[u8],
) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    write_at_by_record_number_io(&mut io, record_number, offset, data)
}

pub fn write_at_by_record_number_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    offset: u64,
    data: &[u8],
) -> Result<u64, String> {
    let (params, record) = read_mft_record_io(io, record_number)?;
    let cluster_size = params.cluster_size;

    let loc = attr_io::find_attribute(&record, AttrType::Data, None)
        .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
    if loc.is_resident {
        return Err(
            "write_at only supports non-resident $DATA in W1 (resident grow lands in W2)"
                .to_string(),
        );
    }

    // Reject compressed (has compression_unit != 0). We don't decompress
    // in W1. Check flags field in the attribute header (+0x0C).
    let flags = u16::from_le_bytes([
        record[loc.attr_offset + attr_io::attr_off::FLAGS],
        record[loc.attr_offset + attr_io::attr_off::FLAGS + 1],
    ]);
    if flags & 0x00FF != 0 {
        // Low byte of flags carries compression_unit encoding + sparse + encrypted.
        return Err(format!(
            "non-resident $DATA is compressed/sparse/encrypted (flags={flags:#06x})"
        ));
    }

    let value_length = loc
        .non_resident_value_length
        .ok_or("missing non-resident value_length")?;
    let end = offset
        .checked_add(data.len() as u64)
        .ok_or("offset + len overflow")?;
    if end > value_length {
        return Err(format!(
            "write past EOF: offset={offset} len={} > value_length={value_length}",
            data.len()
        ));
    }

    let mapping_offset = loc
        .non_resident_mapping_pairs_offset
        .ok_or("missing mapping_pairs_offset")? as usize;
    let mapping_start = loc.attr_offset + mapping_offset;
    let mapping_end = loc.attr_offset + loc.attr_length;
    if mapping_end > record.len() || mapping_start >= mapping_end {
        return Err("mapping_pairs range out of record".to_string());
    }
    let runs = data_runs::decode_runs(&record[mapping_start..mapping_end])?;

    let vcn_first = offset / cluster_size;
    let vcn_last = (end - 1) / cluster_size;
    let n_clusters = vcn_last - vcn_first + 1;
    if data_runs::range_has_hole_or_past_end(&runs, vcn_first, n_clusters) {
        return Err(format!(
            "write range covers a sparse hole or extends past mapped clusters \
             (vcn {vcn_first}..{}); W2 will handle allocation",
            vcn_first + n_clusters
        ));
    }

    // Walk the runs for the target range and write each contiguous span.
    let mut cursor_in_data = 0usize;
    let mut file_offset = offset;

    while cursor_in_data < data.len() {
        let vcn = file_offset / cluster_size;
        let off_in_cluster = file_offset % cluster_size;
        let run = find_run_for_vcn(&runs, vcn).ok_or_else(|| format!("no run for VCN {vcn}"))?;
        let lcn = run.lcn.expect("hole already rejected");
        // bytes we can write without crossing this run's end:
        let run_end_vcn = run.starting_vcn + run.length;
        let run_end_offset = run_end_vcn * cluster_size;
        let max_in_this_run = run_end_offset - file_offset;
        let remaining = (data.len() - cursor_in_data) as u64;
        let chunk = remaining.min(max_in_this_run) as usize;

        let disk_offset = (lcn + (vcn - run.starting_vcn)) * cluster_size + off_in_cluster;
        io.write_all_at(disk_offset, &data[cursor_in_data..cursor_in_data + chunk])
            .map_err(|e| format!("write: {e}"))?;

        cursor_in_data += chunk;
        file_offset += chunk as u64;
    }

    io.sync()?;
    Ok(data.len() as u64)
}

fn find_run_for_vcn(runs: &[DataRun], vcn: u64) -> Option<&DataRun> {
    runs.iter()
        .find(|r| vcn >= r.starting_vcn && vcn < r.starting_vcn + r.length)
}

// ---------------------------------------------------------------------------
// Truncate (shrink only — W2.5 MVP)
// ---------------------------------------------------------------------------

/// Shrink a non-resident file's `$DATA` to `new_size` bytes. Clusters
/// that fall past the new end are freed in `$Bitmap`; the attribute's
/// mapping-pairs are rewritten; `value_length` / `allocated_length` /
/// `initialized_length` are updated.
///
/// The MFT record is updated BEFORE the bitmap bits are freed so a
/// mid-operation crash leaves the volume in the worst case with some
/// orphaned still-allocated clusters (wasted space, recoverable by
/// scan) — not with the file pointing at clusters that another
/// allocation could claim.
///
/// Rejects: grow (new_size &gt; current size), resident `$DATA`,
/// compressed / sparse / encrypted flag set.
pub fn truncate(image: &Path, file_path: &str, new_size: u64) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    truncate_io(&mut io, file_path, new_size)
}

pub fn truncate_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    new_size: u64,
) -> Result<u64, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    truncate_by_record_number_io(io, rec, new_size)
}

/// Attribute-header offsets for non-resident lengths (per Windows Internals 7th ed.).
const NONRES_ALLOCATED_LENGTH: usize = 0x28;
const NONRES_DATA_LENGTH: usize = 0x30;
const NONRES_INITIALIZED_LENGTH: usize = 0x38;
const NONRES_LAST_VCN: usize = 0x18;

pub fn truncate_by_record_number(
    image: &Path,
    record_number: u64,
    new_size: u64,
) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    truncate_by_record_number_io(&mut io, record_number, new_size)
}

pub fn truncate_by_record_number_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    new_size: u64,
) -> Result<u64, String> {
    let (params, record) = read_mft_record_io(io, record_number)?;
    let cluster_size = params.cluster_size;

    let loc = attr_io::find_attribute(&record, AttrType::Data, None)
        .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
    if loc.is_resident {
        return Err("truncate: resident $DATA unsupported in W2 MVP".to_string());
    }
    let flags = u16::from_le_bytes([
        record[loc.attr_offset + attr_io::attr_off::FLAGS],
        record[loc.attr_offset + attr_io::attr_off::FLAGS + 1],
    ]);
    if flags & 0x00FF != 0 {
        return Err(format!(
            "compressed/sparse/encrypted non-resident $DATA (flags={flags:#06x})"
        ));
    }

    let current_len = loc.non_resident_value_length.ok_or("no value_length")?;
    if new_size > current_len {
        return Err(format!(
            "truncate: grow not yet implemented ({new_size} > {current_len})"
        ));
    }
    if new_size == current_len {
        return Ok(new_size);
    }

    // Decode existing runs.
    let mapping_offset = loc
        .non_resident_mapping_pairs_offset
        .ok_or("missing mapping_pairs_offset")? as usize;
    let mapping_start = loc.attr_offset + mapping_offset;
    let mapping_end = loc.attr_offset + loc.attr_length;
    let runs = data_runs::decode_runs(&record[mapping_start..mapping_end])?;

    // new_last_vcn: None if new_size == 0; else ceil(new_size / cs) - 1.
    let new_last_vcn: Option<u64> = if new_size == 0 {
        None
    } else {
        Some((new_size - 1) / cluster_size)
    };

    let (new_runs, clusters_to_free) = split_runs_for_shrink(&runs, new_last_vcn);

    // Encode the trimmed runs.
    let new_mapping = data_runs::encode_runs(&new_runs)?;
    let mapping_capacity = mapping_end - mapping_start;
    if new_mapping.len() > mapping_capacity {
        return Err(format!(
            "trimmed mapping_pairs exceed attr header capacity ({} > {})",
            new_mapping.len(),
            mapping_capacity
        ));
    }

    let new_allocated = new_last_vcn.map(|lv| (lv + 1) * cluster_size).unwrap_or(0);
    let new_last_vcn_field = new_last_vcn.map(|lv| lv as i64).unwrap_or(-1);

    // Rewrite the MFT record FIRST. Once this sync completes, the file's
    // logical size + mapping no longer references the clusters we're
    // about to free. Then free clusters in $Bitmap.
    update_mft_record_io(io, record_number, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
        let attr_start = loc.attr_offset;
        let mapping_offset = loc
            .non_resident_mapping_pairs_offset
            .ok_or("missing mapping_pairs_offset")? as usize;
        let mapping_abs = attr_start + mapping_offset;
        let mapping_end_abs = attr_start + loc.attr_length;

        // Write new mapping; zero-pad the tail of the mapping region so
        // the attribute length stays the same (we avoid resizing the
        // attribute here; that's W2.1 work).
        record[mapping_abs..mapping_abs + new_mapping.len()].copy_from_slice(&new_mapping);
        for i in (mapping_abs + new_mapping.len())..mapping_end_abs {
            record[i] = 0;
        }

        // Patch lengths. `initialized_length` is clamped to the new size
        // (can't be past EOF).
        let init_off = attr_start + NONRES_INITIALIZED_LENGTH;
        let cur_init = u64::from_le_bytes(record[init_off..init_off + 8].try_into().unwrap());
        let new_init = cur_init.min(new_size);

        record[attr_start + NONRES_ALLOCATED_LENGTH..attr_start + NONRES_ALLOCATED_LENGTH + 8]
            .copy_from_slice(&new_allocated.to_le_bytes());
        record[attr_start + NONRES_DATA_LENGTH..attr_start + NONRES_DATA_LENGTH + 8]
            .copy_from_slice(&new_size.to_le_bytes());
        record[attr_start + NONRES_INITIALIZED_LENGTH..attr_start + NONRES_INITIALIZED_LENGTH + 8]
            .copy_from_slice(&new_init.to_le_bytes());

        // last_vcn (i64 LE at +0x18). For empty files, spec uses -1.
        record[attr_start + NONRES_LAST_VCN..attr_start + NONRES_LAST_VCN + 8]
            .copy_from_slice(&new_last_vcn_field.to_le_bytes());

        Ok(())
    })?;

    // Now free the freed clusters.
    if !clusters_to_free.is_empty() {
        let bm = bitmap::locate_bitmap_io(io)?;
        for (lcn, n) in &clusters_to_free {
            // Best-effort: if a double-free occurs we report, but don't
            // undo the earlier record update — the file size change has
            // committed.
            bitmap::free_io(io, &bm, *lcn, *n)
                .map_err(|e| format!("free clusters [{lcn}..{}]: {e}", lcn + n))?;
        }
    }

    Ok(new_size)
}

/// Grow a non-resident file's `$DATA` to `new_size` bytes. Allocates
/// enough contiguous free clusters to cover the new end-of-file,
/// appends them to the run list, rewrites mapping-pairs and lengths.
///
/// Bytes in the newly-allocated range read as zero via NTFS's
/// `initialized_length` mechanism: we leave `initialized_length`
/// where it was so readers zero-fill the tail.
///
/// **Limits in this MVP:**
/// * One contiguous allocation only. If the volume doesn't have a
///   contiguous free run large enough, returns an error — the caller
///   can retry with a smaller `new_size` or `fsck`.
/// * The new mapping-pairs must fit within the existing attribute
///   header's reserved space (we don't shift following attributes —
///   that's W2.1 work). Most grows need 0 or 1 extra encoded bytes.
pub fn grow_nonresident(image: &Path, file_path: &str, new_size: u64) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    grow_nonresident_io(&mut io, file_path, new_size)
}

pub fn grow_nonresident_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    new_size: u64,
) -> Result<u64, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    grow_nonresident_by_record_number_io(io, rec, new_size)
}

pub fn grow_nonresident_by_record_number(
    image: &Path,
    record_number: u64,
    new_size: u64,
) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    grow_nonresident_by_record_number_io(&mut io, record_number, new_size)
}

pub fn grow_nonresident_by_record_number_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    new_size: u64,
) -> Result<u64, String> {
    let (params, record) = read_mft_record_io(io, record_number)?;
    let cluster_size = params.cluster_size;

    let loc = attr_io::find_attribute(&record, AttrType::Data, None)
        .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
    if loc.is_resident {
        return Err("grow_nonresident: refusing resident $DATA (use W2.2 promotion)".to_string());
    }
    let flags = u16::from_le_bytes([
        record[loc.attr_offset + attr_io::attr_off::FLAGS],
        record[loc.attr_offset + attr_io::attr_off::FLAGS + 1],
    ]);
    if flags & 0x00FF != 0 {
        return Err(format!(
            "compressed/sparse/encrypted non-resident $DATA (flags={flags:#06x})"
        ));
    }

    let current_len = loc.non_resident_value_length.ok_or("no value_length")?;
    if new_size <= current_len {
        return Err(format!(
            "grow: new_size {new_size} not greater than current {current_len}"
        ));
    }

    let mapping_offset = loc
        .non_resident_mapping_pairs_offset
        .ok_or("missing mapping_pairs_offset")? as usize;
    let mapping_start = loc.attr_offset + mapping_offset;
    let mapping_end = loc.attr_offset + loc.attr_length;
    let mapping_capacity = mapping_end - mapping_start;
    let runs = data_runs::decode_runs(&record[mapping_start..mapping_end])?;

    // New allocated end (VCN+1 count) = ceil(new_size / cluster_size).
    let new_last_vcn = (new_size - 1) / cluster_size;
    let new_allocated = (new_last_vcn + 1) * cluster_size;

    let current_last_vcn: u64 = runs
        .iter()
        .map(|r| r.starting_vcn + r.length)
        .max()
        .unwrap_or(0);
    let need_clusters = (new_last_vcn + 1).saturating_sub(current_last_vcn);
    if need_clusters == 0 {
        // Partial-cluster grow — no new clusters, just extend lengths.
        return apply_grow_lengths_io(io, record_number, new_size, new_allocated);
    }

    // Ask bitmap for a contiguous run.
    let bm = bitmap::locate_bitmap_io(io)?;
    let hint = runs
        .iter()
        .rev()
        .find_map(|r| r.lcn.map(|lcn| lcn + r.length))
        .unwrap_or(params.mft_lcn.saturating_add(32));
    let new_lcn = bitmap::find_free_run_io(io, &bm, need_clusters, hint)?
        .ok_or_else(|| format!("no contiguous free run of {need_clusters} clusters available"))?;
    bitmap::allocate_io(io, &bm, new_lcn, need_clusters)?;

    // Build new run list. If the new allocation is contiguous with the
    // last dense run, extend that run; otherwise append a new run.
    let mut new_runs = runs.clone();
    let extend_last = new_runs
        .last()
        .and_then(|r| r.lcn.map(|lcn| lcn + r.length == new_lcn))
        .unwrap_or(false);
    if extend_last {
        let last = new_runs.last_mut().unwrap();
        last.length += need_clusters;
    } else {
        new_runs.push(DataRun {
            starting_vcn: current_last_vcn,
            length: need_clusters,
            lcn: Some(new_lcn),
        });
    }

    let new_mapping = data_runs::encode_runs(&new_runs)?;
    if new_mapping.len() > mapping_capacity {
        // Need attribute resize (W2.1) — undo the bitmap allocation so we
        // don't leak clusters.
        bitmap::free_io(io, &bm, new_lcn, need_clusters)?;
        return Err(format!(
            "new mapping_pairs ({} bytes) exceed attr capacity ({}). Attribute resize (W2.1) required.",
            new_mapping.len(),
            mapping_capacity
        ));
    }

    // Commit: rewrite MFT record with new mapping + lengths.
    update_mft_record_io(io, record_number, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
        let attr_start = loc.attr_offset;
        let mapping_offset = loc
            .non_resident_mapping_pairs_offset
            .ok_or("missing mapping_pairs_offset")? as usize;
        let mapping_abs = attr_start + mapping_offset;
        let mapping_end_abs = attr_start + loc.attr_length;

        record[mapping_abs..mapping_abs + new_mapping.len()].copy_from_slice(&new_mapping);
        for i in (mapping_abs + new_mapping.len())..mapping_end_abs {
            record[i] = 0;
        }

        // Lengths:
        //   allocated_length = new_allocated
        //   data_length = new_size
        //   initialized_length unchanged (new bytes zero-fill via spec)
        record[attr_start + NONRES_ALLOCATED_LENGTH..attr_start + NONRES_ALLOCATED_LENGTH + 8]
            .copy_from_slice(&new_allocated.to_le_bytes());
        record[attr_start + NONRES_DATA_LENGTH..attr_start + NONRES_DATA_LENGTH + 8]
            .copy_from_slice(&new_size.to_le_bytes());
        let new_last_vcn_i = new_last_vcn as i64;
        record[attr_start + NONRES_LAST_VCN..attr_start + NONRES_LAST_VCN + 8]
            .copy_from_slice(&new_last_vcn_i.to_le_bytes());
        Ok(())
    })?;

    Ok(new_size)
}

/// Used by grow when only lengths change (no new clusters needed).
fn apply_grow_lengths_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    new_size: u64,
    new_allocated: u64,
) -> Result<u64, String> {
    update_mft_record_io(io, record_number, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
        let attr_start = loc.attr_offset;
        record[attr_start + NONRES_ALLOCATED_LENGTH..attr_start + NONRES_ALLOCATED_LENGTH + 8]
            .copy_from_slice(&new_allocated.to_le_bytes());
        record[attr_start + NONRES_DATA_LENGTH..attr_start + NONRES_DATA_LENGTH + 8]
            .copy_from_slice(&new_size.to_le_bytes());
        Ok(())
    })?;
    Ok(new_size)
}

/// Split `runs` into (kept, freed) for a shrink to `new_last_vcn`.
/// `None` ⇒ free everything (truncate to 0).
fn split_runs_for_shrink(
    runs: &[DataRun],
    new_last_vcn: Option<u64>,
) -> (Vec<DataRun>, Vec<(u64, u64)>) {
    let mut kept = Vec::new();
    let mut freed = Vec::new();
    for r in runs {
        match new_last_vcn {
            None => {
                if let Some(lcn) = r.lcn {
                    freed.push((lcn, r.length));
                }
            }
            Some(lv) => {
                if r.starting_vcn > lv {
                    if let Some(lcn) = r.lcn {
                        freed.push((lcn, r.length));
                    }
                } else if r.starting_vcn + r.length - 1 <= lv {
                    kept.push(*r);
                } else {
                    let keep_len = lv + 1 - r.starting_vcn;
                    let free_len = r.length - keep_len;
                    kept.push(DataRun {
                        starting_vcn: r.starting_vcn,
                        length: keep_len,
                        lcn: r.lcn,
                    });
                    if let Some(lcn) = r.lcn {
                        freed.push((lcn + keep_len, free_len));
                    }
                }
            }
        }
    }
    (kept, freed)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_u64_at(record: &mut [u8], off: usize, v: Option<u64>) {
    if let Some(v) = v {
        record[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// Rename (same-length, same-parent) — W3 first primitive
// ---------------------------------------------------------------------------

/// Rename a file in place. `old_path` is the current absolute path;
/// `new_name` is the new basename (no `/`). The new name must have the
/// **same UTF-16 length** as the current name — full-length-change
/// rename requires index-entry resize (future work).
///
/// Patches both:
///   1. the parent directory's `$INDEX_ROOT` entry for the file (the
///      name that gets returned from `readdir`)
///   2. each matching `$FILE_NAME` attribute in the file's own MFT
///      record
///
/// **Limitation (MVP):** the parent directory's index must fit
/// entirely in resident `$INDEX_ROOT` (no `$INDEX_ALLOCATION`
/// spillover). For a fresh NTFS formatter volume this holds for small
/// subdirectories (e.g. `/Documents`) but NOT for the root directory
/// — NTFS formatter lays out `/` with `$INDEX_ALLOCATION` even when small.
/// Walking + patching `$INDEX_ALLOCATION` blocks is a separate
/// primitive (index_io::find_in_index_allocation, future work).
pub fn rename_same_length(image: &Path, old_path: &str, new_name: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    rename_same_length_io(&mut io, old_path, new_name)
}

pub fn rename_same_length_io<T: BlockIo + ?Sized>(
    io: &mut T,
    old_path: &str,
    new_name: &str,
) -> Result<(), String> {
    if new_name.contains('/') || new_name.is_empty() {
        return Err("new_name must be a basename (no slashes, non-empty)".to_string());
    }
    let (parent_rec, file_rec, current_basename) = resolve_parent_and_child_io(io, old_path)?;

    // Pre-check lengths (UTF-16 code units).
    let old_u16_len = current_basename.encode_utf16().count();
    let new_u16_len = new_name.encode_utf16().count();
    if old_u16_len != new_u16_len {
        return Err(format!(
            "same-length rename required (old {old_u16_len}, new {new_u16_len} UTF-16 units)"
        ));
    }

    // 1) Patch the parent's index entry. First check whether it lives
    //    in the resident $INDEX_ROOT or spills into $INDEX_ALLOCATION;
    //    dispatch accordingly.
    let (_, parent_record_bytes) = read_mft_record_io(io, parent_rec)?;
    let ir_flags = index_io::index_root_flags(&parent_record_bytes)
        .ok_or_else(|| "no $INDEX_ROOT on parent".to_string())?;

    // Reject a destination that already exists: two $I30 entries with the
    // same key is corruption (chkdsk flags it) and a silent clobber leaks
    // the target. An exact-equal rename is a no-op, handled above only in
    // rename_io; guard it here too since this is also a public entrypoint.
    if new_name != current_basename {
        if index_io::find_index_entry(&parent_record_bytes, new_name)?.is_some() {
            return Err(format!("'{new_name}' already exists"));
        }
        if ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
            let ia = idx_block::load_for_directory_io(io, parent_rec)?;
            for vcn in ia.allocated_block_vcns() {
                let blk = idx_block::read_indx_block_io(io, &ia, vcn)?;
                if index_io::find_entry_in_indx_block(&blk, new_name)?.is_some() {
                    return Err(format!("'{new_name}' already exists"));
                }
            }
        }
    } else {
        return Ok(());
    }

    let in_root = index_io::find_index_entry(&parent_record_bytes, &current_basename)?;
    if let Some(entry_found) = in_root {
        if entry_found.file_record_number != file_rec {
            return Err(format!(
                "parent's $INDEX_ROOT entry for '{current_basename}' points at record {} \
                 but the resolved path points at {file_rec}",
                entry_found.file_record_number
            ));
        }
        update_mft_record_io(io, parent_rec, |record| {
            let entry = index_io::find_index_entry(record, &current_basename)?
                .ok_or_else(|| "race: $INDEX_ROOT entry vanished during RMW".to_string())?;
            index_io::rename_index_entry_same_length(record, &entry, new_name)
        })?;
    } else if ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
        // Linear scan of allocated INDX blocks.
        let ia = idx_block::load_for_directory_io(io, parent_rec)?;
        let mut patched = false;
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block_io(io, &ia, vcn)?;
            if let Some(entry) = index_io::find_entry_in_indx_block(&block, &current_basename)? {
                if entry.file_record_number != file_rec {
                    return Err(format!(
                        "INDX entry at VCN {vcn} points at {} but resolved {file_rec}",
                        entry.file_record_number
                    ));
                }
                idx_block::update_indx_block_io(io, &ia, vcn, |block| {
                    let entry = index_io::find_entry_in_indx_block(block, &current_basename)?
                        .ok_or_else(|| "race: INDX entry vanished".to_string())?;
                    index_io::rename_index_entry_same_length(block, &entry, new_name)
                })?;
                patched = true;
                break;
            }
        }
        if !patched {
            return Err(format!(
                "no matching index entry for '{current_basename}' in parent record {parent_rec}"
            ));
        }
    } else {
        return Err(format!(
            "no entry for '{current_basename}' in parent's resident $INDEX_ROOT \
             (parent has no $INDEX_ALLOCATION spillover)"
        ));
    }

    // 2) Patch the file's own $FILE_NAME attributes.
    update_mft_record_io(io, file_rec, |record| {
        index_io::rename_filename_attribute_same_length(record, &current_basename, new_name)
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Create regular file (W3)
// ---------------------------------------------------------------------------

/// Create an empty regular file named `basename` inside the directory
/// at `parent_path`. Returns the new file's MFT record number.
///
/// **Limitations (MVP):**
/// * Parent must currently have a resident `$INDEX_ROOT:$I30` with no
///   `$INDEX_ALLOCATION` overflow. (Same limitation as
///   `rename_same_length` pre-W3-full: root-dir creates aren't
///   supported on NTFS formatter-laid volumes because the root is split.)
/// * MFT must have a free record. Growing `$MFT` itself is W2.6.
/// * Filename collation is case-insensitive ASCII-only (proper
///   NTFS upcase-table collation is future work).
pub fn create_file(image: &Path, parent_path: &str, basename: &str) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    create_file_io(&mut io, parent_path, basename)
}

pub fn create_file_io<T: BlockIo + ?Sized>(
    io: &mut T,
    parent_path: &str,
    basename: &str,
) -> Result<u64, String> {
    if basename.is_empty() || basename == "." || basename == ".." || basename.contains('/') {
        return Err(format!("invalid basename: '{basename}'"));
    }

    let parent_rec = resolve_path_to_record_number_io(io, parent_path)?;

    // Read parent; check it's a directory with a resident-only index.
    let (params, parent_record_bytes) = read_mft_record_io(io, parent_rec)?;
    let parent_flags = crate::mft_io::record_flags(&parent_record_bytes);
    if parent_flags & crate::mft_io::MFT_FLAG_DIRECTORY == 0 {
        return Err(format!("parent '{parent_path}' is not a directory"));
    }
    let ir_flags = index_io::index_root_flags(&parent_record_bytes)
        .ok_or_else(|| "parent has no $INDEX_ROOT".to_string())?;
    let parent_has_overflow = ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0;

    // Reject if the entry already exists anywhere (resident or INDX block).
    if index_io::find_index_entry(&parent_record_bytes, basename)?.is_some() {
        return Err(format!("'{basename}' already exists in '{parent_path}'"));
    }
    if parent_has_overflow {
        let ia = idx_block::load_for_directory_io(io, parent_rec)?;
        for vcn in ia.allocated_block_vcns() {
            let blk = idx_block::read_indx_block_io(io, &ia, vcn)?;
            if index_io::find_entry_in_indx_block(&blk, basename)?.is_some() {
                return Err(format!("'{basename}' already exists in '{parent_path}'"));
            }
        }
    }

    // Allocate a free MFT record.
    let mbm = crate::mft_bitmap::locate_io(io)?;
    let new_rec = crate::mft_bitmap::find_free_record_io(io, &mbm, 24)?
        .ok_or_else(|| "MFT has no free records (and we don't grow it yet)".to_string())?;

    // Get the parent's sequence number for the file-name attribute's
    // parent_reference. Sequence is at record header offset +0x10.
    let parent_seq = u16::from_le_bytes([parent_record_bytes[0x10], parent_record_bytes[0x11]]);
    let parent_reference = crate::record_build::encode_file_reference(parent_rec, parent_seq);

    // Build the new record.
    let nt_time = crate::record_build::nt_time_now();
    let new_seq: u16 = 1;
    let mut new_record = crate::record_build::build_regular_file_record(
        params.file_record_size as usize,
        new_rec as u32,
        new_seq,
        parent_reference,
        basename,
        nt_time,
        params.bytes_per_sector,
    )?;
    // Apply fixup before writing.
    crate::mft_io::apply_fixup_on_write(&mut new_record, params.bytes_per_sector)?;

    // Mark the MFT bitmap bit BEFORE writing the record bytes, so that
    // another concurrent allocator can't grab the same slot. (For our
    // single-writer model this is belt-and-suspenders; important when
    // we eventually support concurrency.)
    crate::mft_bitmap::allocate_io(io, &mbm, new_rec)?;

    // Write the record bytes at the correct disk offset.
    let rec_offset = crate::mft_io::mft_record_offset(&params, new_rec);
    if let Err(e) = io.write_all_at(rec_offset, &new_record) {
        let _ = crate::mft_bitmap::free_io(io, &mbm, new_rec);
        return Err(format!("write new record: {e}"));
    }
    if let Err(e) = io.sync() {
        let _ = crate::mft_bitmap::free_io(io, &mbm, new_rec);
        return Err(format!("fsync new record: {e}"));
    }

    // Insert index entry into parent.
    let new_file_reference = crate::record_build::encode_file_reference(new_rec, new_seq);
    let entry_bytes = index_io::build_file_name_index_entry(
        new_file_reference,
        parent_reference,
        basename,
        nt_time,
        /* is_dir */ false,
    )?;
    let insert_res =
        insert_entry_in_parent_io(io, parent_rec, parent_has_overflow, &entry_bytes, basename);
    if let Err(e) = insert_res {
        // Roll back: clear IN_USE on the new record + free the bitmap bit.
        let _ = update_mft_record_io(io, new_rec, |record| {
            let cur = u16::from_le_bytes([record[0x16], record[0x17]]);
            let new = cur & !crate::mft_io::MFT_FLAG_IN_USE;
            record[0x16..0x18].copy_from_slice(&new.to_le_bytes());
            Ok(())
        });
        let _ = crate::mft_bitmap::free_io(io, &mbm, new_rec);
        return Err(format!("insert index entry: {e}"));
    }

    Ok(new_rec)
}

/// Insert a new index entry into a parent directory, dispatching
/// between resident `$INDEX_ROOT` and `$INDEX_ALLOCATION` INDX blocks.
/// For overflowed parents, scans allocated INDX blocks for one with
/// room and inserts there.
#[allow(dead_code)]
fn insert_entry_in_parent(
    image: &Path,
    parent_rec: u64,
    parent_has_overflow: bool,
    entry_bytes: &[u8],
    basename: &str,
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    insert_entry_in_parent_io(
        &mut io,
        parent_rec,
        parent_has_overflow,
        entry_bytes,
        basename,
    )
}

fn insert_entry_in_parent_io<T: BlockIo + ?Sized>(
    io: &mut T,
    parent_rec: u64,
    parent_has_overflow: bool,
    entry_bytes: &[u8],
    basename: &str,
) -> Result<(), String> {
    // Load the upcase table once so sorted insertion matches NTFS
    // collation (COLLATION_FILE_NAME) on non-ASCII names. Falls back
    // to the ASCII upcase-fold if $UpCase can't be loaded (shouldn't
    // happen on a well-formed volume).
    let upcase = crate::upcase::UpcaseTable::load_io(io).ok();
    if !parent_has_overflow {
        return update_mft_record_io(io, parent_rec, |record| {
            index_io::insert_entry_into_index_root_with_collation(
                record,
                entry_bytes,
                basename,
                upcase.as_ref(),
            )
        });
    }
    // Parent has $INDEX_ALLOCATION: find an INDX block with room.
    let ia = idx_block::load_for_directory_io(io, parent_rec)?;
    for vcn in ia.allocated_block_vcns() {
        // Peek at free space — avoid unnecessary RMW work for blocks
        // that can't fit the entry.
        let block = idx_block::read_indx_block_io(io, &ia, vcn)?;
        let ih_start = idx_block::INDX_INDEX_HEADER_OFFSET;
        let total_size = u32::from_le_bytes([
            block[ih_start + 4],
            block[ih_start + 5],
            block[ih_start + 6],
            block[ih_start + 7],
        ]) as usize;
        let allocated_size = u32::from_le_bytes([
            block[ih_start + 8],
            block[ih_start + 9],
            block[ih_start + 10],
            block[ih_start + 11],
        ]) as usize;
        if total_size + entry_bytes.len() > allocated_size {
            continue;
        }
        // This block has room. RMW + insert.
        return idx_block::update_indx_block_io(io, &ia, vcn, |block| {
            index_io::insert_entry_into_indx_block_with_collation(
                block,
                entry_bytes,
                basename,
                upcase.as_ref(),
            )
        });
    }
    Err(
        "no INDX block with room for new entry (would need B+ tree split / new block allocation)"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// mkdir (W3)
// ---------------------------------------------------------------------------

/// Create a new empty directory `basename` inside `parent_path`.
/// Returns the new directory's MFT record number on success.
///
/// Shares the limitation set of [`create_file`] — the parent must hold
/// its index entirely in `$INDEX_ROOT`.
pub fn mkdir(image: &Path, parent_path: &str, basename: &str) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    mkdir_io(&mut io, parent_path, basename)
}

pub fn mkdir_io<T: BlockIo + ?Sized>(
    io: &mut T,
    parent_path: &str,
    basename: &str,
) -> Result<u64, String> {
    if basename.is_empty() || basename == "." || basename == ".." || basename.contains('/') {
        return Err(format!("invalid basename: '{basename}'"));
    }

    let parent_rec = resolve_path_to_record_number_io(io, parent_path)?;

    let (params, parent_record_bytes) = read_mft_record_io(io, parent_rec)?;
    let parent_flags = crate::mft_io::record_flags(&parent_record_bytes);
    if parent_flags & crate::mft_io::MFT_FLAG_DIRECTORY == 0 {
        return Err(format!("parent '{parent_path}' is not a directory"));
    }
    let ir_flags = index_io::index_root_flags(&parent_record_bytes)
        .ok_or_else(|| "parent has no $INDEX_ROOT".to_string())?;
    let parent_has_overflow = ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0;
    if index_io::find_index_entry(&parent_record_bytes, basename)?.is_some() {
        return Err(format!("'{basename}' already exists in '{parent_path}'"));
    }
    if parent_has_overflow {
        let ia = idx_block::load_for_directory_io(io, parent_rec)?;
        for vcn in ia.allocated_block_vcns() {
            let blk = idx_block::read_indx_block_io(io, &ia, vcn)?;
            if index_io::find_entry_in_indx_block(&blk, basename)?.is_some() {
                return Err(format!("'{basename}' already exists in '{parent_path}'"));
            }
        }
    }

    let mbm = crate::mft_bitmap::locate_io(io)?;
    let new_rec = crate::mft_bitmap::find_free_record_io(io, &mbm, 24)?
        .ok_or_else(|| "MFT full — would need to grow $MFT (W2.6)".to_string())?;

    let parent_seq = u16::from_le_bytes([parent_record_bytes[0x10], parent_record_bytes[0x11]]);
    let parent_reference = crate::record_build::encode_file_reference(parent_rec, parent_seq);

    let nt_time = crate::record_build::nt_time_now();
    let new_seq: u16 = 1;
    // For a fresh directory, use cluster_size as the index block size —
    // matches what NTFS formatter does for small volumes.
    let index_block_size = params.cluster_size as u32;
    let mut new_record = crate::record_build::build_directory_record(
        params.file_record_size as usize,
        new_rec as u32,
        new_seq,
        parent_reference,
        basename,
        nt_time,
        params.bytes_per_sector,
        index_block_size,
    )?;
    crate::mft_io::apply_fixup_on_write(&mut new_record, params.bytes_per_sector)?;

    crate::mft_bitmap::allocate_io(io, &mbm, new_rec)?;

    let rec_offset = crate::mft_io::mft_record_offset(&params, new_rec);
    if let Err(e) = io.write_all_at(rec_offset, &new_record) {
        let _ = crate::mft_bitmap::free_io(io, &mbm, new_rec);
        return Err(format!("write new dir record: {e}"));
    }
    if let Err(e) = io.sync() {
        let _ = crate::mft_bitmap::free_io(io, &mbm, new_rec);
        return Err(format!("fsync new dir record: {e}"));
    }

    let new_file_reference = crate::record_build::encode_file_reference(new_rec, new_seq);
    let entry_bytes = index_io::build_file_name_index_entry(
        new_file_reference,
        parent_reference,
        basename,
        nt_time,
        /* is_dir */ true,
    )?;
    let insert_res =
        insert_entry_in_parent_io(io, parent_rec, parent_has_overflow, &entry_bytes, basename);
    if let Err(e) = insert_res {
        let _ = update_mft_record_io(io, new_rec, |record| {
            let cur = u16::from_le_bytes([record[0x16], record[0x17]]);
            let new = cur & !crate::mft_io::MFT_FLAG_IN_USE;
            record[0x16..0x18].copy_from_slice(&new.to_le_bytes());
            Ok(())
        });
        let _ = crate::mft_bitmap::free_io(io, &mbm, new_rec);
        return Err(format!("insert dir index entry: {e}"));
    }

    Ok(new_rec)
}

// ---------------------------------------------------------------------------
// W4.3: Extended Attributes ($EA + $EA_INFORMATION)
// ---------------------------------------------------------------------------

/// Upsert (add or replace by case-insensitive name) a single extended
/// attribute. EAs are stored resident only in this MVP.
pub fn write_ea(
    image: &Path,
    file_path: &str,
    ea_name: &[u8],
    ea_value: &[u8],
    flags: u8,
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    write_ea_io(&mut io, file_path, ea_name, ea_value, flags)
}

pub fn write_ea_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    ea_name: &[u8],
    ea_value: &[u8],
    flags: u8,
) -> Result<(), String> {
    if ea_name.is_empty() || ea_name.len() > 254 {
        return Err(format!("invalid EA name length {}", ea_name.len()));
    }
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        let mut eas = crate::ea_io::read_from_record(record)?;
        crate::ea_io::upsert(
            &mut eas,
            crate::ea_io::Ea {
                flags,
                name: ea_name.to_vec(),
                value: ea_value.to_vec(),
            },
        );
        commit_eas(record, &eas)
    })
}

/// Remove an EA by name (case-insensitive). Errors if not found.
pub fn remove_ea(image: &Path, file_path: &str, ea_name: &[u8]) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    remove_ea_io(&mut io, file_path, ea_name)
}

pub fn remove_ea_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    ea_name: &[u8],
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        let mut eas = crate::ea_io::read_from_record(record)?;
        if !crate::ea_io::remove_by_name(&mut eas, ea_name) {
            return Err(format!(
                "EA '{}' not found",
                String::from_utf8_lossy(ea_name)
            ));
        }
        commit_eas(record, &eas)
    })
}

/// Return all EAs on `file_path` (empty vec if none).
pub fn list_eas(image: &Path, file_path: &str) -> Result<Vec<crate::ea_io::Ea>, String> {
    let mut io = PathIo::open_ro(image)?;
    list_eas_io(&mut io, file_path)
}

pub fn list_eas_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Vec<crate::ea_io::Ea>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    crate::ea_io::read_from_record(&record)
}

/// Return just the key (name) bytes of every EA on `file_path`,
/// preserving on-disk order. Useful for cheap enumeration when the
/// caller only needs the names and would rather not pay for the
/// values (which can be up to 64KB each).
///
/// EA names are byte-strings on disk (not strictly UTF-8) — they
/// cannot contain NUL but otherwise carry arbitrary bytes. Callers
/// that need UTF-8 should validate at the API boundary.
pub fn list_ea_keys(image: &Path, file_path: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut io = PathIo::open_ro(image)?;
    list_ea_keys_io(&mut io, file_path)
}

pub fn list_ea_keys_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Vec<Vec<u8>>, String> {
    let eas = list_eas_io(io, file_path)?;
    Ok(eas.into_iter().map(|ea| ea.name).collect())
}

/// Rewrite `$EA` + `$EA_INFORMATION`. Empty list ⇒ both removed.
fn commit_eas(record: &mut [u8], eas: &[crate::ea_io::Ea]) -> Result<(), String> {
    let packed = crate::ea_io::encode(eas)?;
    let need = crate::ea_io::count_need_ea(eas);
    let ea_info_value = crate::ea_io::build_ea_information_value(packed.len() as u16, need);

    if eas.is_empty() {
        remove_unnamed_attr(record, AttrType::ExtendedAttribute)?;
        remove_unnamed_attr(record, AttrType::ExtendedAttributeInformation)?;
        return Ok(());
    }

    // Upsert $EA_INFORMATION (0xD0) first so resize_attribute shifts
    // don't invalidate the $EA offset.
    upsert_unnamed_resident_attr(
        record,
        AttrType::ExtendedAttributeInformation,
        &ea_info_value,
        &crate::record_build::build_resident_ea_information_attribute,
    )?;
    upsert_unnamed_resident_attr(
        record,
        AttrType::ExtendedAttribute,
        &packed,
        &crate::record_build::build_resident_ea_attribute,
    )?;
    Ok(())
}

/// Remove the attribute at `attr_offset` (length `attr_length`) from
/// `record` in place: shift subsequent bytes down, zero-fill the
/// trailing slot, and update the record's `bytes_in_use` field.
///
/// Validates that the declared lengths are consistent with the record
/// before touching memory — guards against malformed on-disk records
/// that would otherwise panic in `copy_within`.
pub(crate) fn remove_attribute_at(
    record: &mut [u8],
    attr_offset: usize,
    attr_length: usize,
) -> Result<(), String> {
    let bytes_used =
        u32::from_le_bytes([record[0x18], record[0x19], record[0x1A], record[0x1B]]) as usize;
    if attr_length == 0
        || bytes_used > record.len()
        || attr_offset
            .checked_add(attr_length)
            .is_none_or(|end| end > bytes_used)
    {
        return Err(format!(
            "remove_attribute: invalid range (off={attr_offset}, len={attr_length}, bytes_used={bytes_used}, record_len={})",
            record.len()
        ));
    }
    record.copy_within(attr_offset + attr_length..bytes_used, attr_offset);
    for byte in &mut record[bytes_used - attr_length..bytes_used] {
        *byte = 0;
    }
    let new_bu = (bytes_used - attr_length) as u32;
    record[0x18..0x1C].copy_from_slice(&new_bu.to_le_bytes());
    Ok(())
}

fn remove_unnamed_attr(record: &mut [u8], ty: AttrType) -> Result<(), String> {
    let Some(loc) = attr_io::find_attribute(record, ty, None) else {
        return Ok(());
    };
    remove_attribute_at(record, loc.attr_offset, loc.attr_length)
}

fn upsert_unnamed_resident_attr<F>(
    record: &mut [u8],
    ty: AttrType,
    value: &[u8],
    build: &F,
) -> Result<(), String>
where
    F: Fn(u16, &[u8]) -> Result<Vec<u8>, String>,
{
    if let Some(loc) = attr_io::find_attribute(record, ty, None) {
        let attr_id = loc.attribute_id;
        let new_attr = build(attr_id, value)?;
        crate::attr_resize::replace_attribute(record, loc.attr_offset, &new_attr)?;
    } else {
        let attr_id = crate::attr_resize::allocate_attribute_id(record);
        let new_attr = build(attr_id, value)?;
        crate::attr_resize::insert_attribute_before_end(record, &new_attr)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// W4.2: Reparse points (incl. symlinks)
// ---------------------------------------------------------------------------

/// FILE_ATTRIBUTE_REPARSE_POINT bit per MS-FSCC 2.6.
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Write a resident `$REPARSE_POINT` attribute on an existing file
/// with the given tag and tag-specific data. Sets the
/// `FILE_ATTRIBUTE_REPARSE_POINT` flag on `$STANDARD_INFORMATION`.
/// If the file already has a `$REPARSE_POINT`, it's replaced.
pub fn write_reparse_point(
    image: &Path,
    file_path: &str,
    reparse_tag: u32,
    data: &[u8],
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    write_reparse_point_io(&mut io, file_path, reparse_tag, data)
}

pub fn write_reparse_point_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    reparse_tag: u32,
    data: &[u8],
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        let existing = attr_io::find_attribute(record, AttrType::ReparsePoint, None);
        if let Some(loc) = existing {
            let attr_id = loc.attribute_id;
            let new_attr = crate::record_build::build_resident_reparse_point_attribute(
                attr_id,
                reparse_tag,
                data,
            )?;
            crate::attr_resize::replace_attribute(record, loc.attr_offset, &new_attr)?;
        } else {
            let attr_id = crate::attr_resize::allocate_attribute_id(record);
            let new_attr = crate::record_build::build_resident_reparse_point_attribute(
                attr_id,
                reparse_tag,
                data,
            )?;
            crate::attr_resize::insert_attribute_before_end(record, &new_attr)?;
        }
        set_si_file_attributes_bit(record, FILE_ATTRIBUTE_REPARSE_POINT, true)?;
        Ok(())
    })
}

/// Remove the `$REPARSE_POINT` attribute and clear the
/// `FILE_ATTRIBUTE_REPARSE_POINT` flag.
pub fn remove_reparse_point(image: &Path, file_path: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    remove_reparse_point_io(&mut io, file_path)
}

pub fn remove_reparse_point_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        let loc = attr_io::find_attribute(record, AttrType::ReparsePoint, None)
            .ok_or_else(|| "no $REPARSE_POINT to remove".to_string())?;
        remove_attribute_at(record, loc.attr_offset, loc.attr_length)?;

        set_si_file_attributes_bit(record, FILE_ATTRIBUTE_REPARSE_POINT, false)?;
        Ok(())
    })
}

/// Decoded `$REPARSE_POINT` attribute: the 32-bit reparse tag plus the
/// raw tag-specific data bytes (the on-disk `DataBuffer` field — *not*
/// including the 8-byte REPARSE_DATA_BUFFER header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReparsePoint {
    pub reparse_tag: u32,
    pub data: Vec<u8>,
}

/// Read a file's `$REPARSE_POINT` attribute and return the tag plus the
/// tag-specific data bytes. Unlike [`fs_ntfs_readlink`] which only
/// handles symlinks / mount points, this returns the raw payload for
/// any reparse type (including third-party tags like dedup, HSM, etc.).
///
/// Resident-only: this crate writes reparse points resident; reading
/// non-resident on-disk reparse data would require run-list decoding
/// and is not yet supported.
pub fn read_reparse_point(image: &Path, file_path: &str) -> Result<Option<ReparsePoint>, String> {
    let mut io = PathIo::open_ro(image)?;
    read_reparse_point_io(&mut io, file_path)
}

pub fn read_reparse_point_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Option<ReparsePoint>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    let Some(loc) = attr_io::find_attribute(&record, AttrType::ReparsePoint, None) else {
        return Ok(None);
    };
    if !loc.is_resident {
        return Err("$REPARSE_POINT is non-resident (not yet supported)".to_string());
    }
    let val_off = loc.attr_offset
        + loc
            .resident_value_offset
            .ok_or("$REPARSE_POINT has no value_offset")? as usize;
    let val_len = loc.resident_value_length.unwrap_or(0) as usize;
    // REPARSE_DATA_BUFFER (MS-FSCC §2.1.2): u32 ReparseTag, u16 ReparseDataLength,
    // u16 Reserved, then `ReparseDataLength` bytes of tag-specific data.
    if val_len < 8 {
        return Err(format!("$REPARSE_POINT value too short: {val_len} bytes"));
    }
    if val_off
        .checked_add(val_len)
        .is_none_or(|end| end > record.len())
    {
        return Err(format!(
            "$REPARSE_POINT value range out of record: val_off={val_off}, val_len={val_len}, record_len={}",
            record.len()
        ));
    }
    let buf = &record[val_off..val_off + val_len];
    let reparse_tag = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let data_len = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    let data_start = 8usize;
    if data_start + data_len > val_len {
        return Err(format!(
            "$REPARSE_POINT data_length ({data_len}) runs past attribute value ({val_len})"
        ));
    }
    let data = buf[data_start..data_start + data_len].to_vec();
    Ok(Some(ReparsePoint { reparse_tag, data }))
}

/// Convenience: create a symbolic link at `parent/basename` pointing
/// to `target`.
pub fn create_symlink(
    image: &Path,
    parent_path: &str,
    basename: &str,
    target: &str,
    relative: bool,
) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    create_symlink_io(&mut io, parent_path, basename, target, relative)
}

pub fn create_symlink_io<T: BlockIo + ?Sized>(
    io: &mut T,
    parent_path: &str,
    basename: &str,
    target: &str,
    relative: bool,
) -> Result<u64, String> {
    let rec = create_file_io(io, parent_path, basename)?;
    let data = crate::record_build::build_symlink_reparse_data(target, None, relative);
    let child_path = if parent_path == "/" {
        format!("/{basename}")
    } else {
        format!("{parent_path}/{basename}")
    };
    if let Err(e) = write_reparse_point_io(
        io,
        &child_path,
        crate::record_build::reparse_tag::SYMLINK,
        &data,
    ) {
        let _ = unlink_io(io, &child_path);
        return Err(format!("write_reparse_point: {e}"));
    }
    Ok(rec)
}

/// Describe every attribute in a file's MFT record. Returns a
/// list of [`crate::attr_io::AttrDescription`] — type code + decoded
/// name + dimensions, suitable for human inspection / diagnostics
/// (e.g. matching what reference volumes ship vs. what our mkfs
/// emits when investigating chkdsk disagreements).
///
/// Does NOT follow `$ATTRIBUTE_LIST` extension records — callers
/// interested in the full attribute set of a multi-record file must
/// chase those references explicitly. For files that fit in a single
/// MFT record (the common case in this crate today), this returns
/// the complete picture.
pub fn read_attributes(
    image: &Path,
    file_path: &str,
) -> Result<Vec<crate::attr_io::AttrDescription>, String> {
    let mut io = PathIo::open_ro(image)?;
    read_attributes_io(&mut io, file_path)
}

pub fn read_attributes_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Vec<crate::attr_io::AttrDescription>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    Ok(crate::attr_io::describe_attributes(&record))
}

/// One entry returned by [`read_file_names`] — a single `$FILE_NAME`
/// attribute on a file's MFT record. NTFS files often carry multiple
/// `$FILE_NAME` attributes — one per namespace (POSIX / WIN32 / DOS /
/// WIN32_AND_DOS) — when the long Win32 name doesn't fit DOS 8.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNameRecord {
    /// `$FILE_NAME.namespace` byte at value +0x41 per MS-FSCC §2.4.4:
    /// `0 = POSIX`, `1 = WIN32`, `2 = DOS`, `3 = WIN32_AND_DOS`.
    pub namespace: u8,
    /// The decoded UTF-16 name (lossy on invalid surrogates).
    pub name: String,
    /// `$FILE_NAME.parent_directory_reference` (low 48 bits = MFT
    /// record number, high 16 = sequence). The caller can decode via
    /// `(parent_ref & 0xFFFF_FFFF_FFFF) as u64` to get the parent
    /// record number.
    pub parent_reference: u64,
    /// `$FILE_NAME.file_attributes` (the denormalised copy of the
    /// SI bits — useful for spotting per-file flags like
    /// `FILE_ATTRIBUTE_DIRECTORY` (0x10000000) without re-reading SI).
    pub file_attributes: u32,
}

/// List every `$FILE_NAME` attribute on a file's MFT record. Returns
/// one entry per attribute, in record order — so a file with separate
/// WIN32 + DOS names ships two entries, while a single WIN32_AND_DOS
/// name ships one.
///
/// Useful for diagnostic tooling (e.g. confirming that a runtime
/// `create_file` emitted the right namespace for a long name), for
/// the case-sensitive-dir investigation, and for visualising how
/// `$FILE_NAME` entries differ between system records (where mkfs
/// uses skeleton streams) and user records.
pub fn read_file_names(image: &Path, file_path: &str) -> Result<Vec<FileNameRecord>, String> {
    let mut io = PathIo::open_ro(image)?;
    read_file_names_io(&mut io, file_path)
}

pub fn read_file_names_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Vec<FileNameRecord>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;

    let mut out = Vec::new();
    for loc in attr_io::iter_attributes(&record) {
        if loc.type_code != AttrType::FileName as u32 {
            continue;
        }
        if !loc.is_resident {
            // $FILE_NAME is required to be resident per MS-FSCC; a
            // non-resident one would itself be a corruption we don't
            // want to silently elide. Skip it but flag in the error
            // string of the next caller if surprising. For now: skip.
            continue;
        }
        let val_off = loc.attr_offset
            + loc
                .resident_value_offset
                .ok_or("$FILE_NAME no value_offset")? as usize;
        let val_len = loc.resident_value_length.unwrap_or(0) as usize;
        // $FILE_NAME value layout per MS-FSCC §2.4.4:
        //   +0x00 parent_directory_reference (u64)
        //   +0x08..+0x40 timestamps + sizes + attributes
        //   +0x40 name_length (u8, UTF-16 code units)
        //   +0x41 namespace (u8)
        //   +0x42..+0x42+2*name_length name bytes
        if val_len < 0x42 {
            continue;
        }
        let v = val_off;
        if v + val_len > record.len() {
            continue;
        }
        let parent_reference = u64::from_le_bytes([
            record[v],
            record[v + 1],
            record[v + 2],
            record[v + 3],
            record[v + 4],
            record[v + 5],
            record[v + 6],
            record[v + 7],
        ]);
        let file_attributes = u32::from_le_bytes([
            record[v + 0x38],
            record[v + 0x39],
            record[v + 0x3A],
            record[v + 0x3B],
        ]);
        let name_length = record[v + 0x40] as usize;
        let namespace = record[v + 0x41];
        let name_bytes_end = v + 0x42 + name_length * 2;
        if name_bytes_end > v + val_len {
            continue;
        }
        let utf16: Vec<u16> = record[v + 0x42..name_bytes_end]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&utf16);
        out.push(FileNameRecord {
            namespace,
            name,
            parent_reference,
            file_attributes,
        });
    }
    Ok(out)
}

/// Maximum volume-label length per Microsoft tools convention: 32
/// UTF-16 code units (64 bytes on disk). The on-disk format places no
/// length cap, but Windows Explorer / Disk Management refuse to
/// display labels longer than this.
pub const VOLUME_LABEL_MAX_UTF16: usize = 32;

/// Read the volume label from `$Volume:$VOLUME_NAME`. Returns the
/// decoded UTF-8 string. Returns an empty `String` if the volume has
/// no label set (the `$VOLUME_NAME` attribute is absent or
/// zero-length).
pub fn read_volume_label(image: &Path) -> Result<String, String> {
    let mut io = PathIo::open_ro(image)?;
    read_volume_label_io(&mut io)
}

pub fn read_volume_label_io<T: BlockIo + ?Sized>(io: &mut T) -> Result<String, String> {
    // $Volume is at MFT slot 3 per canonical NTFS layout.
    let (_, record) = read_mft_record_io(io, 3)?;
    let Some(loc) = attr_io::find_attribute(&record, AttrType::VolumeName, None) else {
        return Ok(String::new());
    };
    if !loc.is_resident {
        return Err("$VOLUME_NAME is non-resident (unexpected)".to_string());
    }
    let val_off = loc.attr_offset
        + loc
            .resident_value_offset
            .ok_or("$VOLUME_NAME has no value_offset")? as usize;
    let val_len = loc.resident_value_length.unwrap_or(0) as usize;
    if val_off + val_len > record.len() {
        return Err(format!(
            "$VOLUME_NAME range out of record (val_off={val_off}, val_len={val_len})"
        ));
    }
    if val_len == 0 {
        return Ok(String::new());
    }
    if !val_len.is_multiple_of(2) {
        return Err(format!(
            "$VOLUME_NAME has odd byte length: {val_len} (must be multiple of 2 for UTF-16)"
        ));
    }
    let utf16: Vec<u16> = record[val_off..val_off + val_len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&utf16))
}

/// Set the volume label on the `$Volume` MFT record (slot 3).
/// Passes the new label through UTF-16 encoding; an empty string
/// removes the `$VOLUME_NAME` attribute entirely (no zero-length
/// attribute left behind — Windows tools treat both states as
/// "unnamed" so the simpler representation is to omit the attribute).
///
/// Returns `Err` if the encoded label exceeds
/// [`VOLUME_LABEL_MAX_UTF16`] code units.
pub fn set_volume_label(image: &Path, label: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    set_volume_label_io(&mut io, label)
}

pub fn set_volume_label_io<T: BlockIo + ?Sized>(io: &mut T, label: &str) -> Result<(), String> {
    let label_utf16: Vec<u16> = label.encode_utf16().collect();
    if label_utf16.len() > VOLUME_LABEL_MAX_UTF16 {
        return Err(format!(
            "volume label too long: {} UTF-16 code units (max {})",
            label_utf16.len(),
            VOLUME_LABEL_MAX_UTF16
        ));
    }
    let mut label_bytes: Vec<u8> = Vec::with_capacity(label_utf16.len() * 2);
    for c in &label_utf16 {
        label_bytes.extend_from_slice(&c.to_le_bytes());
    }
    update_mft_record_io(io, 3, |record| {
        if label_bytes.is_empty() {
            // Empty label: remove the $VOLUME_NAME attribute entirely.
            if let Some(loc) = attr_io::find_attribute(record, AttrType::VolumeName, None) {
                remove_attribute_at(record, loc.attr_offset, loc.attr_length)?;
            }
            return Ok(());
        }
        if let Some(loc) = attr_io::find_attribute(record, AttrType::VolumeName, None) {
            let attr_id = loc.attribute_id;
            let new_attr =
                crate::record_build::build_resident_volume_name_attribute(attr_id, &label_bytes);
            crate::attr_resize::replace_attribute(record, loc.attr_offset, &new_attr)?;
        } else {
            let attr_id = crate::attr_resize::allocate_attribute_id(record);
            let new_attr =
                crate::record_build::build_resident_volume_name_attribute(attr_id, &label_bytes);
            crate::attr_resize::insert_attribute_before_end(record, &new_attr)?;
        }
        Ok(())
    })
}

/// Read the 16-byte object ID (`$OBJECT_ID` attribute value) for a
/// file. Returns `Ok(None)` if the file has no `$OBJECT_ID`.
pub fn read_object_id(image: &Path, file_path: &str) -> Result<Option<[u8; 16]>, String> {
    let mut io = PathIo::open_ro(image)?;
    read_object_id_io(&mut io, file_path)
}

pub fn read_object_id_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Option<[u8; 16]>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    let Some(loc) = attr_io::find_attribute(&record, AttrType::ObjectId, None) else {
        return Ok(None);
    };
    if !loc.is_resident {
        return Err("$OBJECT_ID is non-resident (unexpected)".to_string());
    }
    let val_off = loc.attr_offset
        + loc
            .resident_value_offset
            .ok_or("$OBJECT_ID has no value_offset")? as usize;
    let val_len = loc.resident_value_length.unwrap_or(0) as usize;
    if val_len < 16 {
        return Err(format!("$OBJECT_ID value too short: {val_len} bytes"));
    }
    // Guard against a corrupt on-disk value_offset that lands 16 bytes
    // can't be read from — independent of val_len, which is only the
    // declared size field and could disagree with the attribute's
    // actual placement in the record.
    if val_off.checked_add(16).is_none_or(|end| end > record.len()) {
        return Err(format!(
            "$OBJECT_ID value range out of record: val_off={val_off}, record_len={}",
            record.len()
        ));
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&record[val_off..val_off + 16]);
    Ok(Some(out))
}

/// Result of [`read_object_id_extended`]: the file's `$OBJECT_ID`
/// attribute decoded as the full 64-byte form when present, or the
/// 16-byte-only form when the Birth GUIDs are absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectIdExtended {
    pub object_id: [u8; 16],
    /// `Some((bv, bo, bd))` when the on-disk attribute is the
    /// 64-byte extended form; `None` for the 16-byte short form.
    pub birth_ids: Option<([u8; 16], [u8; 16], [u8; 16])>,
}

/// Read the full `$OBJECT_ID` attribute, decoding the optional Birth
/// GUIDs (MS-FSCC §2.4.6) when present. Returns:
///   * `Ok(None)` — file has no `$OBJECT_ID` attribute.
///   * `Ok(Some(ext))` — attribute present. `ext.birth_ids` is
///     `Some(...)` when `value_length == 64`, `None` otherwise.
pub fn read_object_id_extended(
    image: &Path,
    file_path: &str,
) -> Result<Option<ObjectIdExtended>, String> {
    let mut io = PathIo::open_ro(image)?;
    read_object_id_extended_io(&mut io, file_path)
}

pub fn read_object_id_extended_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Option<ObjectIdExtended>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    let Some(loc) = attr_io::find_attribute(&record, AttrType::ObjectId, None) else {
        return Ok(None);
    };
    if !loc.is_resident {
        return Err("$OBJECT_ID is non-resident (unexpected)".to_string());
    }
    let val_off = loc.attr_offset
        + loc
            .resident_value_offset
            .ok_or("$OBJECT_ID has no value_offset")? as usize;
    let val_len = loc.resident_value_length.unwrap_or(0) as usize;
    // MS-FSCC §2.4.6: $OBJECT_ID is either 16 bytes (object_id only) or
    // 64 bytes (object_id + 3 Birth GUIDs). Reject anything else as
    // malformed instead of silently downgrading.
    if val_len != 16 && val_len != 64 {
        return Err(format!(
            "unexpected $OBJECT_ID length: {val_len} (expected 16 or 64)"
        ));
    }
    if val_off
        .checked_add(val_len)
        .is_none_or(|end| end > record.len())
    {
        return Err(format!(
            "$OBJECT_ID value range out of record: val_off={val_off}, val_len={val_len}, record_len={}",
            record.len()
        ));
    }
    let mut object_id = [0u8; 16];
    object_id.copy_from_slice(&record[val_off..val_off + 16]);
    let birth_ids = if val_len == 64 {
        let mut bv = [0u8; 16];
        let mut bo = [0u8; 16];
        let mut bd = [0u8; 16];
        bv.copy_from_slice(&record[val_off + 16..val_off + 32]);
        bo.copy_from_slice(&record[val_off + 32..val_off + 48]);
        bd.copy_from_slice(&record[val_off + 48..val_off + 64]);
        Some((bv, bo, bd))
    } else {
        None
    };
    Ok(Some(ObjectIdExtended {
        object_id,
        birth_ids,
    }))
}

/// Write a 16-byte `$OBJECT_ID` to a file. Adds the attribute if absent,
/// replaces the existing value in place if present. To also write the
/// 48 bytes of DLT Birth-volume / Birth-object / Birth-domain GUIDs
/// (MS-FSCC §2.4.6), use [`write_object_id_extended`].
pub fn write_object_id(image: &Path, file_path: &str, object_id: &[u8; 16]) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    write_object_id_io(&mut io, file_path, object_id)
}

pub fn write_object_id_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    object_id: &[u8; 16],
) -> Result<(), String> {
    write_object_id_inner(io, file_path, object_id, None)
}

/// Write a 64-byte extended `$OBJECT_ID` carrying the mandatory
/// `object_id` plus the three optional DLT Birth GUIDs
/// (`birth_volume_id`, `birth_object_id`, `birth_domain_id`).
/// Adds the attribute if absent, replaces in place if present.
///
/// DLT (Distributed Link Tracking, the Windows shortcut-resolution
/// service) uses the Birth fields to chase moved files across
/// volumes and machines. Most consumers don't read them; the
/// 16-byte short form from `write_object_id` is functionally
/// equivalent for the common case.
pub fn write_object_id_extended(
    image: &Path,
    file_path: &str,
    object_id: &[u8; 16],
    birth_volume_id: &[u8; 16],
    birth_object_id: &[u8; 16],
    birth_domain_id: &[u8; 16],
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    write_object_id_extended_io(
        &mut io,
        file_path,
        object_id,
        birth_volume_id,
        birth_object_id,
        birth_domain_id,
    )
}

pub fn write_object_id_extended_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    object_id: &[u8; 16],
    birth_volume_id: &[u8; 16],
    birth_object_id: &[u8; 16],
    birth_domain_id: &[u8; 16],
) -> Result<(), String> {
    write_object_id_inner(
        io,
        file_path,
        object_id,
        Some((birth_volume_id, birth_object_id, birth_domain_id)),
    )
}

fn write_object_id_inner<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    object_id: &[u8; 16],
    birth_ids: Option<(&[u8; 16], &[u8; 16], &[u8; 16])>,
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        if let Some(loc) = attr_io::find_attribute(record, AttrType::ObjectId, None) {
            let attr_id = loc.attribute_id;
            let new_attr = crate::record_build::build_resident_object_id_attribute_full(
                attr_id, object_id, birth_ids,
            );
            crate::attr_resize::replace_attribute(record, loc.attr_offset, &new_attr)?;
        } else {
            let attr_id = crate::attr_resize::allocate_attribute_id(record);
            let new_attr = crate::record_build::build_resident_object_id_attribute_full(
                attr_id, object_id, birth_ids,
            );
            crate::attr_resize::insert_attribute_before_end(record, &new_attr)?;
        }
        Ok(())
    })
}

/// Remove the `$OBJECT_ID` attribute. Returns `Ok(false)` if the file
/// had no `$OBJECT_ID` (idempotent — not an error). Returns `Ok(true)`
/// if an attribute was removed.
pub fn remove_object_id(image: &Path, file_path: &str) -> Result<bool, String> {
    let mut io = PathIo::open_rw(image)?;
    remove_object_id_io(&mut io, file_path)
}

pub fn remove_object_id_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<bool, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let mut removed = false;
    update_mft_record_io(io, rec, |record| {
        let Some(loc) = attr_io::find_attribute(record, AttrType::ObjectId, None) else {
            return Ok(());
        };
        remove_attribute_at(record, loc.attr_offset, loc.attr_length)?;
        removed = true;
        Ok(())
    })?;
    Ok(removed)
}

/// Add a new hard link to an existing file. The new link lives at
/// `new_parent_path/new_basename`. The target file's MFT record gains
/// a new `$FILE_NAME` attribute and its hard-link count is incremented;
/// the parent directory's index gains a matching entry.
///
/// Refuses directories (NTFS disallows hardlinked directories).
/// Refuses if the new basename already exists in the target parent.
pub fn link(
    image: &Path,
    existing_path: &str,
    new_parent_path: &str,
    new_basename: &str,
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    link_io(&mut io, existing_path, new_parent_path, new_basename)
}

pub fn link_io<T: BlockIo + ?Sized>(
    io: &mut T,
    existing_path: &str,
    new_parent_path: &str,
    new_basename: &str,
) -> Result<(), String> {
    if new_basename.is_empty()
        || new_basename == "."
        || new_basename == ".."
        || new_basename.contains('/')
    {
        return Err(format!("invalid basename: '{new_basename}'"));
    }
    let target_rec = resolve_path_to_record_number_io(io, existing_path)?;
    let (_, target_record_bytes) = read_mft_record_io(io, target_rec)?;
    let target_flags = crate::mft_io::record_flags(&target_record_bytes);
    if target_flags & crate::mft_io::MFT_FLAG_DIRECTORY != 0 {
        return Err(format!(
            "link: refusing to hardlink directory '{existing_path}'"
        ));
    }

    let new_parent_rec = resolve_path_to_record_number_io(io, new_parent_path)?;
    let (_, parent_record_bytes) = read_mft_record_io(io, new_parent_rec)?;
    let parent_flags = crate::mft_io::record_flags(&parent_record_bytes);
    if parent_flags & crate::mft_io::MFT_FLAG_DIRECTORY == 0 {
        return Err(format!(
            "link: new parent '{new_parent_path}' is not a directory"
        ));
    }
    let ir_flags = index_io::index_root_flags(&parent_record_bytes)
        .ok_or_else(|| "parent has no $INDEX_ROOT".to_string())?;
    let parent_has_overflow = ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0;
    if index_io::find_index_entry(&parent_record_bytes, new_basename)?.is_some() {
        return Err(format!(
            "'{new_basename}' already exists in '{new_parent_path}'"
        ));
    }
    if parent_has_overflow {
        let ia = idx_block::load_for_directory_io(io, new_parent_rec)?;
        for vcn in ia.allocated_block_vcns() {
            let blk = idx_block::read_indx_block_io(io, &ia, vcn)?;
            if index_io::find_entry_in_indx_block(&blk, new_basename)?.is_some() {
                return Err(format!(
                    "'{new_basename}' already exists in '{new_parent_path}'"
                ));
            }
        }
    }

    let parent_seq = u16::from_le_bytes([parent_record_bytes[0x10], parent_record_bytes[0x11]]);
    let parent_reference = crate::record_build::encode_file_reference(new_parent_rec, parent_seq);
    let target_seq = u16::from_le_bytes([target_record_bytes[0x10], target_record_bytes[0x11]]);
    let target_reference = crate::record_build::encode_file_reference(target_rec, target_seq);
    let nt_time = crate::record_build::nt_time_now();

    // Step 1: append a new $FILE_NAME to the target record + bump
    // hard_link_count.
    update_mft_record_io(io, target_rec, |record| {
        let attr_id = crate::attr_resize::allocate_attribute_id(record);
        let fn_attr = crate::record_build::build_file_name_attribute(
            attr_id,
            parent_reference,
            new_basename,
            nt_time,
            /* is_dir */ false,
        )?;
        crate::attr_resize::insert_attribute_before_end(record, &fn_attr)?;
        let cur = u16::from_le_bytes([record[0x12], record[0x13]]);
        record[0x12..0x14].copy_from_slice(&cur.saturating_add(1).to_le_bytes());
        Ok(())
    })?;

    // Step 2: insert index entry in new parent.
    let entry = index_io::build_file_name_index_entry(
        target_reference,
        parent_reference,
        new_basename,
        nt_time,
        /* is_dir */ false,
    )?;
    let insert_res = insert_entry_in_parent_io(
        io,
        new_parent_rec,
        parent_has_overflow,
        &entry,
        new_basename,
    );
    if let Err(e) = insert_res {
        // Roll back: remove the $FILE_NAME we added and decrement the
        // link count. Best-effort; mostly for tests since insertion
        // failures here would indicate a full directory.
        let _ = update_mft_record_io(io, target_rec, |record| {
            // Find the last $FILE_NAME matching new_basename+parent_ref
            // and strip it.
            if let Some(loc) = find_file_name_attr(record, parent_reference, new_basename) {
                let bytes_used =
                    u32::from_le_bytes([record[0x18], record[0x19], record[0x1A], record[0x1B]])
                        as usize;
                record.copy_within(
                    loc.attr_offset + loc.attr_length..bytes_used,
                    loc.attr_offset,
                );
                for byte in &mut record[bytes_used - loc.attr_length..bytes_used] {
                    *byte = 0;
                }
                let new_bu = (bytes_used - loc.attr_length) as u32;
                record[0x18..0x1C].copy_from_slice(&new_bu.to_le_bytes());
                let cur = u16::from_le_bytes([record[0x12], record[0x13]]);
                record[0x12..0x14].copy_from_slice(&cur.saturating_sub(1).to_le_bytes());
            }
            Ok(())
        });
        return Err(format!("link: insert index entry: {e}"));
    }
    Ok(())
}

/// Find the `$FILE_NAME` attribute whose parent_reference + name match
/// the given pair. Used to unwind a partial `link` or to remove a
/// specific hardlink. Returns the attribute location or `None`.
fn find_file_name_attr(
    record: &[u8],
    parent_reference: u64,
    name: &str,
) -> Option<attr_io::AttrLocation> {
    let name_utf16: Vec<u16> = name.encode_utf16().collect();
    for loc in attr_io::iter_attributes(record) {
        if loc.type_code != attr_io::AttrType::FileName as u32 {
            continue;
        }
        let val_off = loc.attr_offset + loc.resident_value_offset? as usize;
        let pr = u64::from_le_bytes([
            record[val_off],
            record[val_off + 1],
            record[val_off + 2],
            record[val_off + 3],
            record[val_off + 4],
            record[val_off + 5],
            record[val_off + 6],
            record[val_off + 7],
        ]);
        if pr != parent_reference {
            continue;
        }
        let name_len = record[val_off + 64] as usize;
        if name_len != name_utf16.len() {
            continue;
        }
        let mut ok = true;
        for (i, expected) in name_utf16.iter().enumerate() {
            let off = val_off + 66 + i * 2;
            let got = u16::from_le_bytes([record[off], record[off + 1]]);
            if got != *expected {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(loc);
        }
    }
    None
}

fn set_si_file_attributes_bit(record: &mut [u8], bit: u32, set: bool) -> Result<(), String> {
    let loc = attr_io::find_attribute(record, AttrType::StandardInformation, None)
        .ok_or_else(|| "$STANDARD_INFORMATION not found".to_string())?;
    let data_start = attr_io::resident_value_start(&loc)
        .ok_or_else(|| "$STANDARD_INFORMATION not resident".to_string())?;
    let off = data_start + SI_FILE_ATTRIBUTES;
    let current = u32::from_le_bytes([
        record[off],
        record[off + 1],
        record[off + 2],
        record[off + 3],
    ]);
    let new = if set { current | bit } else { current & !bit };
    record[off..off + 4].copy_from_slice(&new.to_le_bytes());
    Ok(())
}

// ---------------------------------------------------------------------------
// W4.1: Alternate Data Streams (named $DATA)
// ---------------------------------------------------------------------------

/// Create or replace a named `$DATA` stream (an Alternate Data Stream
/// in NTFS parlance) with resident data. If the stream already exists,
/// its body is overwritten (resizing the attribute as needed).
/// Otherwise a new resident named `$DATA` attribute is appended to
/// the file's MFT record.
///
/// MVP scope:
/// * Resident only — stream content must fit in the file's free MFT
///   record space. Promotion of named streams to non-resident is a
///   later step.
/// * Stream name must be non-empty.
pub fn write_named_stream_resident(
    image: &Path,
    file_path: &str,
    stream_name: &str,
    data: &[u8],
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    write_named_stream_resident_io(&mut io, file_path, stream_name, data)
}

pub fn write_named_stream_resident_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    stream_name: &str,
    data: &[u8],
) -> Result<(), String> {
    if stream_name.is_empty() {
        return Err("stream_name must be non-empty".to_string());
    }
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        // Existing stream?
        let existing = attr_io::find_attribute(record, AttrType::Data, Some(stream_name));
        if let Some(loc) = existing {
            if !loc.is_resident {
                return Err(format!(
                    "named stream '{stream_name}' is non-resident; use write_at + grow instead"
                ));
            }
            crate::attr_resize::set_resident_value(record, loc.attr_offset, data)
        } else {
            let attr_id = crate::attr_resize::allocate_attribute_id(record);
            let new_attr = crate::record_build::build_named_resident_data_attribute(
                attr_id,
                stream_name,
                data,
            )?;
            crate::attr_resize::insert_attribute_before_end(record, &new_attr)
        }
    })
}

/// High-level: write a named `$DATA` stream (alternate data stream).
/// Stays resident if the new data fits; promotes to non-resident by
/// allocating clusters and emitting a single-run mapping-pairs blob
/// otherwise. Replaces the existing stream body if one already exists.
pub fn write_named_stream(
    image: &Path,
    file_path: &str,
    stream_name: &str,
    data: &[u8],
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    write_named_stream_io(&mut io, file_path, stream_name, data)
}

pub fn write_named_stream_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    stream_name: &str,
    data: &[u8],
) -> Result<(), String> {
    match write_named_stream_resident_io(io, file_path, stream_name, data) {
        Ok(()) => Ok(()),
        Err(e)
            if e.contains("capacity")
                || e.contains("exceeds")
                || e.contains("no room")
                || e.contains("non-resident") =>
        {
            // If a resident version exists, remove it first so the
            // non-resident replacement inserts cleanly.
            let _ = delete_named_stream_io(io, file_path, stream_name);
            promote_attribute_to_nonresident_io(
                io,
                file_path,
                AttrType::Data,
                Some(stream_name),
                data,
            )
        }
        Err(e) => Err(e),
    }
}

/// Enumerate the names of every *named* `$DATA` stream on a file
/// (i.e. alternate data streams — excludes the unnamed primary
/// `$DATA`). Returns names in on-disk MFT record order.
///
/// Note: the NTFS spec keeps `$DATA` attributes sorted by attribute
/// name (with the unnamed primary first), but enforcement is a
/// writer concern — this crate's `write_named_stream` currently
/// appends in insertion order. Callers that need a canonical
/// ordering should sort the returned `Vec`.
///
/// Resident and non-resident streams are both reported (this is a
/// header-only walk; we don't read the bodies).
pub fn list_named_streams(image: &Path, file_path: &str) -> Result<Vec<String>, String> {
    let mut io = PathIo::open_ro(image)?;
    list_named_streams_io(&mut io, file_path)
}

pub fn list_named_streams_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<Vec<String>, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (_, record) = read_mft_record_io(io, rec)?;
    let mut names = Vec::new();
    for loc in attr_io::iter_attributes(&record) {
        if loc.type_code != AttrType::Data as u32 {
            continue;
        }
        if loc.name_length == 0 {
            // Unnamed primary $DATA — not an ADS.
            continue;
        }
        names.push(attr_io::decode_attr_name(&record, &loc));
    }
    Ok(names)
}

/// Delete a named `$DATA` stream from a file. Fails if the stream
/// doesn't exist.
pub fn delete_named_stream(image: &Path, file_path: &str, stream_name: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    delete_named_stream_io(&mut io, file_path, stream_name)
}

pub fn delete_named_stream_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    stream_name: &str,
) -> Result<(), String> {
    if stream_name.is_empty() {
        return Err("stream_name must be non-empty".to_string());
    }
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, Some(stream_name))
            .ok_or_else(|| format!("named stream '{stream_name}' not found"))?;
        remove_attribute_at(record, loc.attr_offset, loc.attr_length)
    })
}

// ---------------------------------------------------------------------------
// W2.2: promote resident $DATA to non-resident (optionally writing new content)
// ---------------------------------------------------------------------------

/// Promote a file's resident `$DATA` to non-resident, setting its
/// content to `new_data`. Allocates clusters, writes data, rewrites
/// the attribute header as non-resident.
///
/// Useful on its own (e.g. as a staging step) but also the building
/// block for `write_file_contents` that transparently dispatches
/// between resident + non-resident.
pub fn promote_resident_data_to_nonresident(
    image: &Path,
    file_path: &str,
    new_data: &[u8],
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    promote_resident_data_to_nonresident_io(&mut io, file_path, new_data)
}

pub fn promote_resident_data_to_nonresident_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    new_data: &[u8],
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (params, record) = read_mft_record_io(io, rec)?;
    let cluster_size = params.cluster_size;

    let loc = attr_io::find_attribute(&record, AttrType::Data, None)
        .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
    if !loc.is_resident {
        return Err("$DATA is already non-resident".to_string());
    }

    // Allocate clusters for the new data.
    let new_size = new_data.len() as u64;
    let n_clusters = new_size.div_ceil(cluster_size).max(1);
    let bm = crate::bitmap::locate_bitmap_io(io)?;
    let new_lcn = crate::bitmap::find_free_run_io(io, &bm, n_clusters, params.mft_lcn)?
        .ok_or_else(|| format!("no contiguous free run of {n_clusters} clusters"))?;
    crate::bitmap::allocate_io(io, &bm, new_lcn, n_clusters)?;

    // Write the data (zero-padded to cluster boundary).
    let allocated_length = n_clusters * cluster_size;
    {
        let disk_offset = new_lcn * cluster_size;
        if let Err(e) = io.write_all_at(disk_offset, new_data) {
            let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
            return Err(format!("write data: {e}"));
        }
        let pad = (allocated_length - new_size) as usize;
        if pad > 0 {
            let zeros = vec![0u8; pad];
            if let Err(e) = io.write_all_at(disk_offset + new_size, &zeros) {
                let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
                return Err(format!("write zero-pad: {e}"));
            }
        }
        if let Err(e) = io.sync() {
            let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
            return Err(format!("fsync data: {e}"));
        }
    }

    // Build mapping_pairs (single run).
    let runs = vec![DataRun {
        starting_vcn: 0,
        length: n_clusters,
        lcn: Some(new_lcn),
    }];
    let mapping_pairs = data_runs::encode_runs(&runs)?;

    // Build new non-resident $DATA attribute.
    let last_vcn = if new_size == 0 {
        -1i64
    } else {
        (n_clusters - 1) as i64
    };
    let attr_id = loc.attribute_id;
    let new_attr_bytes = crate::record_build::build_nonresident_data_attribute(
        attr_id,
        new_size,
        allocated_length,
        new_size, // initialized_length = data_length for fully-written new data
        last_vcn,
        &mapping_pairs,
    )?;

    // Replace $DATA in the MFT record.
    let replace_res = update_mft_record_io(io, rec, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or_else(|| "$DATA vanished during RMW".to_string())?;
        crate::attr_resize::replace_attribute(record, loc.attr_offset, &new_attr_bytes)
    });
    if let Err(e) = replace_res {
        let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
        return Err(format!("replace $DATA: {e}"));
    }

    Ok(())
}

/// High-level: write `new_data` as the entire content of the file.
/// Dispatches between resident rewrite and promotion-to-non-resident
/// based on whether the data still fits inside the MFT record.
///
/// The heuristic is: attempt resident write first. If it fails with a
/// record-capacity error, retry with promotion.
pub fn write_file_contents(image: &Path, file_path: &str, new_data: &[u8]) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    write_file_contents_io(&mut io, file_path, new_data)
}

pub fn write_file_contents_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    new_data: &[u8],
) -> Result<u64, String> {
    match write_resident_contents_io(io, file_path, new_data) {
        Ok(n) => Ok(n),
        Err(e) if e.contains("capacity") || e.contains("exceeds") => {
            promote_resident_data_to_nonresident_io(io, file_path, new_data)?;
            Ok(new_data.len() as u64)
        }
        Err(e) => Err(e),
    }
}

/// Generalized non-resident promotion for any attribute type + optional
/// name. Allocates clusters, writes `new_data` (zero-padded to cluster
/// boundary), then replaces the existing resident attribute with a
/// non-resident one. If no existing attribute of that `(type, name)`
/// exists, a fresh non-resident attribute is inserted.
///
/// Used by §2.3 to grow named `$DATA`, `$REPARSE_POINT`, and `$EA`
/// past resident capacity.
pub fn promote_attribute_to_nonresident(
    image: &Path,
    file_path: &str,
    attr_type: AttrType,
    attr_name: Option<&str>,
    new_data: &[u8],
) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    promote_attribute_to_nonresident_io(&mut io, file_path, attr_type, attr_name, new_data)
}

pub fn promote_attribute_to_nonresident_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    attr_type: AttrType,
    attr_name: Option<&str>,
    new_data: &[u8],
) -> Result<(), String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    let (params, _) = read_mft_record_io(io, rec)?;
    let cluster_size = params.cluster_size;

    let new_size = new_data.len() as u64;
    let n_clusters = new_size.div_ceil(cluster_size).max(1);
    let bm = crate::bitmap::locate_bitmap_io(io)?;
    let new_lcn = crate::bitmap::find_free_run_io(io, &bm, n_clusters, params.mft_lcn)?
        .ok_or_else(|| format!("no contiguous free run of {n_clusters} clusters"))?;
    crate::bitmap::allocate_io(io, &bm, new_lcn, n_clusters)?;

    let allocated_length = n_clusters * cluster_size;
    {
        let disk_offset = new_lcn * cluster_size;
        if let Err(e) = io.write_all_at(disk_offset, new_data) {
            let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
            return Err(format!("write data: {e}"));
        }
        let pad = (allocated_length - new_size) as usize;
        if pad > 0 {
            let zeros = vec![0u8; pad];
            if let Err(e) = io.write_all_at(disk_offset + new_size, &zeros) {
                let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
                return Err(format!("write zero-pad: {e}"));
            }
        }
        if let Err(e) = io.sync() {
            let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
            return Err(format!("fsync data: {e}"));
        }
    }

    let runs = vec![DataRun {
        starting_vcn: 0,
        length: n_clusters,
        lcn: Some(new_lcn),
    }];
    let mapping_pairs = data_runs::encode_runs(&runs)?;
    let last_vcn = if new_size == 0 {
        -1i64
    } else {
        (n_clusters - 1) as i64
    };

    let replace_res = update_mft_record_io(io, rec, |record| {
        let attr_id = match attr_io::find_attribute(record, attr_type, attr_name) {
            Some(loc) => loc.attribute_id,
            None => crate::attr_resize::allocate_attribute_id(record),
        };
        let new_attr_bytes = crate::record_build::build_nonresident_attribute(
            attr_type as u32,
            attr_name,
            attr_id,
            new_size,
            allocated_length,
            new_size,
            last_vcn,
            &mapping_pairs,
        )?;
        if let Some(loc) = attr_io::find_attribute(record, attr_type, attr_name) {
            crate::attr_resize::replace_attribute(record, loc.attr_offset, &new_attr_bytes)?;
        } else {
            crate::attr_resize::insert_attribute_before_end(record, &new_attr_bytes)?;
        }
        Ok(())
    });
    if let Err(e) = replace_res {
        let _ = crate::bitmap::free_io(io, &bm, new_lcn, n_clusters);
        return Err(format!("replace attribute: {e}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// rmdir (W3)
// ---------------------------------------------------------------------------

/// Delete an empty directory. Fails if the directory has any entries
/// (other than the implicit LAST sentinel) or if it's overflowed to
/// `$INDEX_ALLOCATION`. Returns `Ok(())` on success.
pub fn rmdir(image: &Path, dir_path: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    rmdir_io(&mut io, dir_path)
}

pub fn rmdir_io<T: BlockIo + ?Sized>(io: &mut T, dir_path: &str) -> Result<(), String> {
    let (parent_rec, dir_rec, basename) = resolve_parent_and_child_io(io, dir_path)?;
    let (_, dir_record_bytes) = read_mft_record_io(io, dir_rec)?;
    let flags = crate::mft_io::record_flags(&dir_record_bytes);
    if flags & MFT_FLAG_DIRECTORY == 0 {
        return Err(format!("rmdir: '{dir_path}' is not a directory"));
    }

    // Emptiness check: walk $INDEX_ROOT entries, require only the LAST
    // sentinel is present. Also reject $INDEX_ALLOCATION spillover — a
    // non-empty overflowed dir is definitely non-empty; a claimed-empty
    // but overflowed dir is suspicious anyway, refuse for MVP.
    let ir_flags = index_io::index_root_flags(&dir_record_bytes).ok_or("dir has no $INDEX_ROOT")?;
    if ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
        return Err(format!(
            "rmdir: '{dir_path}' has $INDEX_ALLOCATION overflow (probably not empty)"
        ));
    }
    if index_io::index_root_has_real_entries(&dir_record_bytes)? {
        return Err(format!("rmdir: '{dir_path}' is not empty"));
    }

    // Remove from parent's index. Parent's index may or may not be
    // overflowed — dispatch like unlink.
    let (_, parent_record_bytes) = read_mft_record_io(io, parent_rec)?;
    let parent_ir_flags =
        index_io::index_root_flags(&parent_record_bytes).ok_or("parent has no $INDEX_ROOT")?;
    let in_root = index_io::find_index_entry(&parent_record_bytes, &basename)?;
    if let Some(entry) = in_root {
        if entry.file_record_number != dir_rec {
            return Err(format!(
                "parent index entry for '{basename}' points at {} but resolved {dir_rec}",
                entry.file_record_number
            ));
        }
        update_mft_record_io(io, parent_rec, |record| {
            let e = index_io::find_index_entry(record, &basename)?
                .ok_or_else(|| "race: IR entry vanished".to_string())?;
            index_io::remove_index_entry(record, &e, index_io::BlockKind::IndexRoot)
        })?;
    } else if parent_ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
        let ia = idx_block::load_for_directory_io(io, parent_rec)?;
        let mut removed = false;
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block_io(io, &ia, vcn)?;
            if let Some(entry) = index_io::find_entry_in_indx_block(&block, &basename)? {
                if entry.file_record_number != dir_rec {
                    return Err(format!(
                        "INDX entry at VCN {vcn} points at {} but resolved {dir_rec}",
                        entry.file_record_number
                    ));
                }
                idx_block::update_indx_block_io(io, &ia, vcn, |block| {
                    let e = index_io::find_entry_in_indx_block(block, &basename)?
                        .ok_or_else(|| "race: INDX entry vanished".to_string())?;
                    index_io::remove_index_entry(block, &e, index_io::BlockKind::IndexAllocation)
                })?;
                removed = true;
                break;
            }
        }
        if !removed {
            return Err(format!(
                "no index entry for '{basename}' in parent record {parent_rec}"
            ));
        }
    } else {
        return Err(format!("no entry for '{basename}' in parent's $INDEX_ROOT"));
    }

    // Clear IN_USE flag + free MFT bit.
    update_mft_record_io(io, dir_rec, |record| {
        let cur = u16::from_le_bytes([record[0x16], record[0x17]]);
        let new = cur & !crate::mft_io::MFT_FLAG_IN_USE;
        record[0x16..0x18].copy_from_slice(&new.to_le_bytes());
        Ok(())
    })?;
    let mbm = crate::mft_bitmap::locate_io(io)?;
    crate::mft_bitmap::free_io(io, &mbm, dir_rec)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Write resident data (W3 convenience)
// ---------------------------------------------------------------------------

/// Write `new_data` as the entire content of the file's unnamed
/// `$DATA`. Works only while the data can remain resident (fits in
/// free MFT record space). Returns bytes written on success.
///
/// For growing past the resident ceiling, W2.2 promotion is required;
/// this primitive returns an error in that case.
pub fn write_resident_contents(
    image: &Path,
    file_path: &str,
    new_data: &[u8],
) -> Result<u64, String> {
    let mut io = PathIo::open_rw(image)?;
    write_resident_contents_io(&mut io, file_path, new_data)
}

pub fn write_resident_contents_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
    new_data: &[u8],
) -> Result<u64, String> {
    let rec = resolve_path_to_record_number_io(io, file_path)?;
    update_mft_record_io(io, rec, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or_else(|| "unnamed $DATA attribute not found".to_string())?;
        if !loc.is_resident {
            return Err("$DATA is already non-resident; use write_at + grow instead".to_string());
        }
        crate::attr_resize::set_resident_value(record, loc.attr_offset, new_data)
    })?;
    Ok(new_data.len() as u64)
}

// ---------------------------------------------------------------------------
// Rename, variable-length (W3 full)
// ---------------------------------------------------------------------------

/// Rename a file to a new basename that may differ in length from the
/// current one. Same parent directory.
///
/// For same-length renames this delegates to [`rename_same_length`]
/// (which handles both `$INDEX_ROOT` and `$INDEX_ALLOCATION` parents).
/// For length changes, the parent must currently have a
/// resident-only `$INDEX_ROOT` (same MVP limitation as `create_file`).
/// Timestamps are refreshed to "now" in the updated index entry and
/// `$FILE_NAME` attribute(s), matching Windows' observable behavior.
pub fn rename(image: &Path, old_path: &str, new_basename: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    rename_io(&mut io, old_path, new_basename)
}

pub fn rename_io<T: BlockIo + ?Sized>(
    io: &mut T,
    old_path: &str,
    new_basename: &str,
) -> Result<(), String> {
    if new_basename.is_empty()
        || new_basename == "."
        || new_basename == ".."
        || new_basename.contains('/')
    {
        return Err(format!("invalid basename: '{new_basename}'"));
    }
    let (parent_rec, file_rec, old_basename) = resolve_parent_and_child_io(io, old_path)?;
    if old_basename == new_basename {
        return Ok(());
    }

    let old_u16_len = old_basename.encode_utf16().count();
    let new_u16_len = new_basename.encode_utf16().count();
    if old_u16_len == new_u16_len {
        return rename_same_length_io(io, old_path, new_basename);
    }

    let (_, parent_record_bytes) = read_mft_record_io(io, parent_rec)?;
    let ir_flags = index_io::index_root_flags(&parent_record_bytes)
        .ok_or_else(|| "parent has no $INDEX_ROOT".to_string())?;
    if ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
        return Err(
            "variable-length rename MVP: parent has $INDEX_ALLOCATION overflow — not yet supported"
                .to_string(),
        );
    }
    if index_io::find_index_entry(&parent_record_bytes, new_basename)?.is_some() {
        return Err(format!("'{new_basename}' already exists"));
    }

    let (_, file_record_bytes) = read_mft_record_io(io, file_rec)?;
    let file_seq = u16::from_le_bytes([file_record_bytes[0x10], file_record_bytes[0x11]]);
    let parent_seq = u16::from_le_bytes([parent_record_bytes[0x10], parent_record_bytes[0x11]]);
    let file_reference = crate::record_build::encode_file_reference(file_rec, file_seq);
    let parent_reference = crate::record_build::encode_file_reference(parent_rec, parent_seq);

    let nt_time = crate::record_build::nt_time_now();
    let is_dir = crate::mft_io::record_flags(&file_record_bytes) & MFT_FLAG_DIRECTORY != 0;

    let new_entry_bytes = index_io::build_file_name_index_entry(
        file_reference,
        parent_reference,
        new_basename,
        nt_time,
        is_dir,
    )?;

    // 1) Swap the parent's $INDEX_ROOT entry.
    let upcase = crate::upcase::UpcaseTable::load_io(io).ok();
    update_mft_record_io(io, parent_rec, |record| {
        let old_entry = index_io::find_index_entry(record, &old_basename)?
            .ok_or_else(|| format!("old entry '{old_basename}' not found"))?;
        index_io::remove_index_entry(record, &old_entry, index_io::BlockKind::IndexRoot)?;
        index_io::insert_entry_into_index_root_with_collation(
            record,
            &new_entry_bytes,
            new_basename,
            upcase.as_ref(),
        )
    })?;

    // 2) Update the file's own $FILE_NAME attribute(s).
    update_mft_record_io(io, file_rec, |record| {
        replace_file_name_with_new_name(
            record,
            &old_basename,
            new_basename,
            parent_reference,
            nt_time,
            is_dir,
        )
    })?;

    Ok(())
}

fn replace_file_name_with_new_name(
    record: &mut [u8],
    old_name: &str,
    new_name: &str,
    parent_reference: u64,
    nt_time: u64,
    is_dir: bool,
) -> Result<(), String> {
    let old_utf16: Vec<u16> = old_name.encode_utf16().collect();
    let new_utf16: Vec<u16> = new_name.encode_utf16().collect();
    if new_utf16.is_empty() || new_utf16.len() > 255 {
        return Err("invalid new name length".to_string());
    }

    loop {
        let mut target: Option<(usize, u8)> = None;
        for loc in attr_io::iter_attributes(record) {
            if loc.type_code != attr_io::AttrType::FileName as u32 {
                continue;
            }
            let val_off = match loc.resident_value_offset {
                Some(v) => v as usize,
                None => continue,
            };
            let data_start = loc.attr_offset + val_off;
            let name_length_byte = record[data_start + 0x40] as usize;
            if name_length_byte != old_utf16.len() {
                continue;
            }
            let name_start = data_start + 0x42;
            let cur: Vec<u16> = record[name_start..name_start + name_length_byte * 2]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            if cur == old_utf16 {
                let ns = record[data_start + 0x41];
                target = Some((loc.attr_offset, ns));
                break;
            }
        }
        let Some((attr_offset, namespace)) = target else {
            break;
        };
        let value = build_file_name_value(parent_reference, &new_utf16, nt_time, is_dir, namespace);
        crate::attr_resize::set_resident_value(record, attr_offset, &value)?;
    }
    Ok(())
}

fn build_file_name_value(
    parent_reference: u64,
    name_utf16: &[u16],
    nt_time: u64,
    is_dir: bool,
    namespace: u8,
) -> Vec<u8> {
    let v_len = 0x42 + name_utf16.len() * 2;
    let mut v = vec![0u8; v_len];
    v[0..8].copy_from_slice(&parent_reference.to_le_bytes());
    v[8..16].copy_from_slice(&nt_time.to_le_bytes());
    v[16..24].copy_from_slice(&nt_time.to_le_bytes());
    v[24..32].copy_from_slice(&nt_time.to_le_bytes());
    v[32..40].copy_from_slice(&nt_time.to_le_bytes());
    v[40..48].copy_from_slice(&0u64.to_le_bytes());
    v[48..56].copy_from_slice(&0u64.to_le_bytes());
    let fa: u32 = if is_dir { 0x10000000 | 0x20 } else { 0x20 };
    v[56..60].copy_from_slice(&fa.to_le_bytes());
    v[60..64].copy_from_slice(&0u32.to_le_bytes());
    v[0x40] = name_utf16.len() as u8;
    v[0x41] = namespace;
    for (i, c) in name_utf16.iter().enumerate() {
        let off = 0x42 + i * 2;
        v[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    v
}

/// Delete a regular file:
/// 1. Remove the parent dir's index entry for the file.
/// 2. Free the file's data-run clusters via `truncate` to zero.
/// 3. Clear the IN_USE flag in the file's MFT record.
/// 4. Flip the file's bit in `$MFT:$Bitmap` to free.
///
/// Refuses directories (use `rmdir` — not implemented yet).
///
/// Order matters: step 1 makes the file unreachable by name, then
/// steps 2-4 free the backing storage. A crash between 1 and 4 leaks
/// an MFT record + clusters (recoverable by scan); a reversed order
/// could leave the file name pointing at a freed+reallocated record.
pub fn unlink(image: &Path, file_path: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    unlink_io(&mut io, file_path)
}

pub fn unlink_io<T: BlockIo + ?Sized>(io: &mut T, file_path: &str) -> Result<(), String> {
    let (parent_rec, file_rec, basename) = resolve_parent_and_child_io(io, file_path)?;

    // Refuse directory targets.
    let (_, file_record_bytes) = read_mft_record_io(io, file_rec)?;
    let flags = crate::mft_io::record_flags(&file_record_bytes);
    if flags & MFT_FLAG_DIRECTORY != 0 {
        return Err(format!(
            "unlink: '{file_path}' is a directory — use rmdir (not implemented)"
        ));
    }

    // 1) Remove the parent's index entry. Dispatch on IR flags.
    let (_, parent_record_bytes) = read_mft_record_io(io, parent_rec)?;
    let ir_flags = index_io::index_root_flags(&parent_record_bytes)
        .ok_or_else(|| "no $INDEX_ROOT on parent".to_string())?;

    let in_root = index_io::find_index_entry(&parent_record_bytes, &basename)?;
    if let Some(entry) = in_root {
        if entry.file_record_number != file_rec {
            return Err(format!(
                "parent's $INDEX_ROOT entry for '{basename}' points at {} but resolved {file_rec}",
                entry.file_record_number
            ));
        }
        update_mft_record_io(io, parent_rec, |record| {
            let e = index_io::find_index_entry(record, &basename)?
                .ok_or_else(|| "race: $INDEX_ROOT entry vanished".to_string())?;
            index_io::remove_index_entry(record, &e, index_io::BlockKind::IndexRoot)
        })?;
    } else if ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
        let ia = idx_block::load_for_directory_io(io, parent_rec)?;
        let mut removed = false;
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block_io(io, &ia, vcn)?;
            if let Some(entry) = index_io::find_entry_in_indx_block(&block, &basename)? {
                if entry.file_record_number != file_rec {
                    return Err(format!(
                        "INDX entry at VCN {vcn} points at {} but resolved {file_rec}",
                        entry.file_record_number
                    ));
                }
                idx_block::update_indx_block_io(io, &ia, vcn, |block| {
                    let e = index_io::find_entry_in_indx_block(block, &basename)?
                        .ok_or_else(|| "race: INDX entry vanished".to_string())?;
                    index_io::remove_index_entry(block, &e, index_io::BlockKind::IndexAllocation)
                })?;
                removed = true;
                break;
            }
        }
        if !removed {
            return Err(format!(
                "no index entry for '{basename}' in parent record {parent_rec}"
            ));
        }
    } else {
        return Err(format!(
            "no entry for '{basename}' in parent's $INDEX_ROOT (no spillover)"
        ));
    }

    // 1b) If the file has multiple hard links, removing one name must NOT
    //     free the record or its clusters — only drop the matching
    //     $FILE_NAME attribute and decrement hard_link_count. Storage is
    //     freed only when the LAST link goes away (the count==1 path below).
    let hard_link_count = u16::from_le_bytes([file_record_bytes[0x12], file_record_bytes[0x13]]);
    if hard_link_count > 1 {
        let parent_seq = u16::from_le_bytes([parent_record_bytes[0x10], parent_record_bytes[0x11]]);
        let parent_reference = crate::record_build::encode_file_reference(parent_rec, parent_seq);
        update_mft_record_io(io, file_rec, |record| {
            let loc =
                find_file_name_attr(record, parent_reference, &basename).ok_or_else(|| {
                    format!("unlink: no $FILE_NAME for '{basename}' under parent {parent_rec}")
                })?;
            let bytes_used =
                u32::from_le_bytes([record[0x18], record[0x19], record[0x1A], record[0x1B]])
                    as usize;
            record.copy_within(
                loc.attr_offset + loc.attr_length..bytes_used,
                loc.attr_offset,
            );
            for byte in &mut record[bytes_used - loc.attr_length..bytes_used] {
                *byte = 0;
            }
            let new_bu = (bytes_used - loc.attr_length) as u32;
            record[0x18..0x1C].copy_from_slice(&new_bu.to_le_bytes());
            let cur = u16::from_le_bytes([record[0x12], record[0x13]]);
            record[0x12..0x14].copy_from_slice(&cur.saturating_sub(1).to_le_bytes());
            Ok(())
        })?;
        return Ok(());
    }

    // 2) Free data clusters. Only if non-resident — resident $DATA lives
    //    inside the MFT record and is freed as part of the record itself.
    //    truncate to 0 is a no-op for resident data anyway.
    let data_loc = attr_io::find_attribute(&file_record_bytes, AttrType::Data, None);
    if let Some(loc) = data_loc {
        if !loc.is_resident && loc.non_resident_value_length.unwrap_or(0) > 0 {
            truncate_by_record_number_io(io, file_rec, 0)?;
        }
    }

    // 3) Clear IN_USE flag in the file's MFT record.
    update_mft_record_io(io, file_rec, |record| {
        let flags_off = 0x16;
        let cur = u16::from_le_bytes([record[flags_off], record[flags_off + 1]]);
        let new = cur & !crate::mft_io::MFT_FLAG_IN_USE;
        record[flags_off..flags_off + 2].copy_from_slice(&new.to_le_bytes());
        Ok(())
    })?;

    // 4) Free the MFT record bit.
    let mbm = mft_bitmap::locate_io(io)?;
    mft_bitmap::free_io(io, &mbm, file_rec)?;

    Ok(())
}

/// POSIX-style `remove`: dispatch by type. Directories go through
/// [`rmdir_io`] (which enforces emptiness), regular files through
/// [`unlink_io`]. Both `unlink` and `rmdir` deliberately refuse the
/// other's type; this is the single entrypoint that routes correctly.
pub fn remove(image: &Path, path: &str) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    remove_io(&mut io, path)
}

pub fn remove_io<T: BlockIo + ?Sized>(io: &mut T, path: &str) -> Result<(), String> {
    let (_, rec, _) = resolve_parent_and_child_io(io, path)?;
    let (_, record_bytes) = read_mft_record_io(io, rec)?;
    if crate::mft_io::record_flags(&record_bytes) & MFT_FLAG_DIRECTORY != 0 {
        rmdir_io(io, path)
    } else {
        unlink_io(io, path)
    }
}

/// Resolve `old_path` to `(parent_record_number, file_record_number, basename)`.
#[allow(dead_code)]
fn resolve_parent_and_child(image: &Path, old_path: &str) -> Result<(u64, u64, String), String> {
    let mut io = PathIo::open_ro(image)?;
    resolve_parent_and_child_io(&mut io, old_path)
}

fn resolve_parent_and_child_io<T: BlockIo + ?Sized>(
    io: &mut T,
    old_path: &str,
) -> Result<(u64, u64, String), String> {
    let p = old_path.trim_start_matches('/');
    if p.is_empty() {
        return Err("cannot rename root".to_string());
    }
    let (parent_path, basename) = match p.rsplit_once('/') {
        Some((par, base)) => (par, base),
        None => ("", p),
    };
    let parent_full = format!("/{parent_path}");
    let parent_rec = resolve_path_to_record_number_io(io, &parent_full)?;
    let file_rec = resolve_path_to_record_number_io(io, old_path)?;
    Ok((parent_rec, file_rec, basename.to_string()))
}

/// Walk `file_path` via upstream and return the target's MFT record number.
pub fn resolve_path_to_record_number(path: &Path, file_path: &str) -> Result<u64, String> {
    let mut io = PathIo::open_ro(path)?;
    resolve_path_to_record_number_io(&mut io, file_path)
}

/// `BlockIo`-based equivalent of [`resolve_path_to_record_number`]. Used
/// by the handle-based mutator path so a single `BlockIo` services both
/// the read (via `IoReadSeek`) and any subsequent writes.
pub fn resolve_path_to_record_number_io<T: BlockIo + ?Sized>(
    io: &mut T,
    file_path: &str,
) -> Result<u64, String> {
    let mut reader = IoReadSeek::new(io);
    let mut ntfs = Ntfs::new(&mut reader).map_err(|e| format!("parse: {e}"))?;
    ntfs.read_upcase_table(&mut reader)
        .map_err(|e| format!("upcase: {e}"))?;
    let file = navigate_to(&ntfs, &mut reader, file_path)?;
    Ok(file.file_record_number())
}

fn navigate_to<'n, T: std::io::Read + std::io::Seek>(
    ntfs: &'n Ntfs,
    reader: &mut T,
    file_path: &str,
) -> Result<NtfsFile<'n>, String> {
    let p = file_path.trim_start_matches('/');
    if p.is_empty() {
        return ntfs
            .root_directory(reader)
            .map_err(|e| format!("root: {e}"));
    }
    let mut current = ntfs
        .root_directory(reader)
        .map_err(|e| format!("root: {e}"))?;
    for comp in p.split('/') {
        if comp.is_empty() {
            continue;
        }
        let index = current
            .directory_index(reader)
            .map_err(|e| format!("dir index for '{comp}': {e}"))?;
        let mut finder = index.finder();
        let entry = NtfsFileNameIndex::find(&mut finder, ntfs, reader, comp)
            .ok_or_else(|| format!("not found: '{comp}'"))?
            .map_err(|e| format!("find '{comp}': {e}"))?;
        current = entry
            .to_file(ntfs, reader)
            .map_err(|e| format!("to_file '{comp}': {e}"))?;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr_io::attr_off;
    use crate::block_io::BlockIo;
    use crate::data_runs::DataRun;
    use crate::mkfs::format_filesystem;

    // --- in-memory BlockIo harness for I/O tests ----------------------------

    struct MemDev {
        buf: Vec<u8>,
    }

    impl BlockIo for MemDev {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            let off = offset as usize;
            buf.copy_from_slice(&self.buf[off..off + buf.len()]);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            let off = offset as usize;
            self.buf[off..off + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn size(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    fn fresh_vol() -> MemDev {
        // 16 MiB: mkfs places $MFTMirr at cluster_count/2 and requires the
        // primary metadata region (boot + 64-cluster $MFT + ~3.78 MiB
        // $LogFile + $UpCase + …) to end before that midpoint. With
        // mft_record_size = 4096 the region ends near LCN 1049, so the
        // midpoint must exceed it: 8 MiB (midpoint 1024) is too small and
        // mkfs returns "volume too small for chosen layout"; 16 MiB
        // (midpoint 2048) clears it with margin. Kept as small as
        // correctness allows so parallel runs under llvm-cov don't
        // exhaust memory.
        const SIZE: u64 = 16 * 1024 * 1024;
        let mut dev = MemDev {
            buf: vec![0u8; SIZE as usize],
        };
        format_filesystem(
            &mut dev as &mut dyn BlockIo,
            SIZE,
            4096,
            4096,
            Some("WRTEST"),
            Some(0xAABB_CCDD),
        )
        .expect("format_filesystem");
        dev
    }

    // --- create_file_io -------------------------------------------------------

    #[test]
    fn create_file_io_creates_findable_file() {
        let mut dev = fresh_vol();
        let rec_num = create_file_io(&mut dev, "/", "hello.txt").unwrap();
        assert!(rec_num >= 24, "user files start at record 24+");
        // Find it back in the root index.
        let (_, root_rec) = crate::mft_io::read_mft_record_io(&mut dev, 5).unwrap();
        let loc = crate::index_io::find_index_entry(&root_rec, "hello.txt").unwrap();
        assert!(loc.is_some(), "file must appear in root $INDEX_ROOT");
    }

    #[test]
    fn create_file_io_duplicate_fails() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "dup.txt").unwrap();
        assert!(create_file_io(&mut dev, "/", "dup.txt").is_err());
    }

    #[test]
    fn create_file_io_invalid_basename_fails() {
        let mut dev = fresh_vol();
        assert!(create_file_io(&mut dev, "/", "").is_err());
        assert!(create_file_io(&mut dev, "/", ".").is_err());
        assert!(create_file_io(&mut dev, "/", "a/b").is_err());
    }

    // --- mkdir_io -------------------------------------------------------------

    #[test]
    fn mkdir_io_creates_directory() {
        let mut dev = fresh_vol();
        let rec_num = mkdir_io(&mut dev, "/", "mydir").unwrap();
        assert!(rec_num >= 24);
        let (_, root_rec) = crate::mft_io::read_mft_record_io(&mut dev, 5).unwrap();
        let loc = crate::index_io::find_index_entry(&root_rec, "mydir").unwrap();
        assert!(loc.is_some());
    }

    #[test]
    fn mkdir_io_duplicate_fails() {
        let mut dev = fresh_vol();
        mkdir_io(&mut dev, "/", "sub").unwrap();
        assert!(mkdir_io(&mut dev, "/", "sub").is_err());
    }

    #[test]
    fn mkdir_io_nested_directory() {
        let mut dev = fresh_vol();
        mkdir_io(&mut dev, "/", "parent").unwrap();
        let rec_num = mkdir_io(&mut dev, "/parent", "child").unwrap();
        assert!(rec_num >= 24);
    }

    // --- write_at_io: empty data is always a no-op ----------------------------

    #[test]
    fn write_at_io_empty_data_returns_zero() {
        let mut dev = fresh_vol();
        let n = write_at_io(&mut dev, "/", 0, &[]).unwrap();
        assert_eq!(n, 0);
    }

    // --- set_times_by_record_number_io ----------------------------------------

    #[test]
    fn set_times_updates_standard_information() {
        let mut dev = fresh_vol();
        let rec = create_file_io(&mut dev, "/", "times.txt").unwrap();
        let ts: u64 = 132_000_000_000_000;
        let times = FileTimes {
            creation: Some(ts),
            modification: Some(ts),
            mft_record_modification: Some(ts),
            access: Some(ts),
        };
        set_times_by_record_number_io(&mut dev, rec, times).unwrap();
        let si = read_si_full_io(&mut dev, "/times.txt").unwrap();
        assert_eq!(si.creation_time, ts);
        assert_eq!(si.modification_time, ts);
    }

    // --- set_security_id_io / read_security_id_io -----------------------------

    #[test]
    fn set_and_read_security_id_roundtrip() {
        let mut dev = fresh_vol();
        let rec = create_file_io(&mut dev, "/", "sec.txt").unwrap();
        set_security_id_io(&mut dev, "/sec.txt", 0x1234).unwrap();
        let id = read_security_id_io(&mut dev, "/sec.txt").unwrap();
        assert_eq!(id, Some(0x1234));
        let _ = rec; // ensure rec is used
    }

    // --- set_file_attributes_by_record_number_io ------------------------------

    #[test]
    fn set_file_attributes_sets_hidden_bit() {
        let mut dev = fresh_vol();
        let rec = create_file_io(&mut dev, "/", "attr.txt").unwrap();
        set_file_attributes_by_record_number_io(
            &mut dev,
            rec,
            FileAttributesChange {
                add: file_attr::HIDDEN,
                remove: 0,
            },
        )
        .unwrap();
        let si = read_si_full_io(&mut dev, "/attr.txt").unwrap();
        assert_eq!(si.file_attributes & file_attr::HIDDEN, file_attr::HIDDEN);
    }

    // --- write_ea_io / list_eas_io --------------------------------------------

    #[test]
    fn write_and_list_eas_roundtrip() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "ea.txt").unwrap();
        write_ea_io(
            &mut dev,
            "/ea.txt",
            b"TestKey",
            &[0xDE, 0xAD, 0xBE, 0xEF],
            0,
        )
        .unwrap();
        let eas = list_eas_io(&mut dev, "/ea.txt").unwrap();
        assert_eq!(eas.len(), 1);
        assert_eq!(eas[0].name, b"TestKey");
        assert_eq!(eas[0].value, &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn write_ea_multiple_keys() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "multiea.txt").unwrap();
        write_ea_io(&mut dev, "/multiea.txt", b"Key1", b"val1", 0).unwrap();
        write_ea_io(&mut dev, "/multiea.txt", b"Key2", b"val2", 0).unwrap();
        let keys = list_ea_keys_io(&mut dev, "/multiea.txt").unwrap();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn remove_ea_removes_entry() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "rmea.txt").unwrap();
        write_ea_io(&mut dev, "/rmea.txt", b"Del", b"x", 0).unwrap();
        write_ea_io(&mut dev, "/rmea.txt", b"Keep", b"y", 0).unwrap();
        remove_ea_io(&mut dev, "/rmea.txt", b"Del").unwrap();
        let eas = list_eas_io(&mut dev, "/rmea.txt").unwrap();
        assert_eq!(eas.len(), 1);
        assert_eq!(eas[0].name, b"Keep");
    }

    // --- rename_same_length_io ------------------------------------------------

    #[test]
    fn rename_same_length_renames_file() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "abc.txt").unwrap();
        rename_same_length_io(&mut dev, "/abc.txt", "xyz.txt").unwrap();
        let (_, root) = crate::mft_io::read_mft_record_io(&mut dev, 5).unwrap();
        assert!(crate::index_io::find_index_entry(&root, "abc.txt")
            .unwrap()
            .is_none());
        assert!(crate::index_io::find_index_entry(&root, "xyz.txt")
            .unwrap()
            .is_some());
    }

    fn run(starting_vcn: u64, length: u64, lcn: u64) -> DataRun {
        DataRun {
            starting_vcn,
            length,
            lcn: Some(lcn),
        }
    }

    // --- find_run_for_vcn -----------------------------------------------------

    #[test]
    fn find_run_for_vcn_returns_matching_run() {
        let runs = vec![run(0, 4, 100), run(4, 8, 200)];
        let r = find_run_for_vcn(&runs, 5).unwrap();
        assert_eq!(r.starting_vcn, 4);
        assert_eq!(r.lcn, Some(200));
    }

    #[test]
    fn find_run_for_vcn_returns_none_when_past_end() {
        let runs = vec![run(0, 4, 100)];
        assert!(find_run_for_vcn(&runs, 4).is_none());
        assert!(find_run_for_vcn(&runs, 100).is_none());
    }

    #[test]
    fn find_run_for_vcn_returns_none_on_empty_list() {
        assert!(find_run_for_vcn(&[], 0).is_none());
    }

    // --- split_runs_for_shrink ------------------------------------------------

    #[test]
    fn split_runs_shrink_to_zero_frees_all() {
        let runs = vec![run(0, 4, 100), run(4, 4, 200)];
        let (kept, freed) = split_runs_for_shrink(&runs, None);
        assert!(kept.is_empty());
        assert_eq!(freed, vec![(100, 4), (200, 4)]);
    }

    #[test]
    fn split_runs_shrink_mid_run_splits_correctly() {
        // Two runs of 4 clusters each. Shrink to VCN 5 (last cluster = VCN 5).
        // Run 0 [0..4) kept whole; run 1 [4..8) split: keep VCNs 4-5, free VCNs 6-7.
        let runs = vec![run(0, 4, 100), run(4, 4, 200)];
        let (kept, freed) = split_runs_for_shrink(&runs, Some(5));
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0], run(0, 4, 100));
        assert_eq!(kept[1].starting_vcn, 4);
        assert_eq!(kept[1].length, 2);
        assert_eq!(freed, vec![(202, 2)]);
    }

    #[test]
    fn split_runs_no_shrink_keeps_all_runs() {
        let runs = vec![run(0, 4, 100), run(4, 4, 200)];
        let (kept, freed) = split_runs_for_shrink(&runs, Some(7));
        assert_eq!(kept, runs);
        assert!(freed.is_empty());
    }

    #[test]
    fn split_runs_beyond_all_runs_frees_entire_run() {
        // new_last_vcn cuts exactly before the second run entirely.
        let runs = vec![run(0, 4, 100), run(4, 4, 200)];
        let (kept, freed) = split_runs_for_shrink(&runs, Some(3));
        assert_eq!(kept, vec![run(0, 4, 100)]);
        assert_eq!(freed, vec![(200, 4)]);
    }

    // --- write_u64_at --------------------------------------------------------

    #[test]
    fn write_u64_at_some_writes_le_bytes() {
        let mut buf = vec![0u8; 16];
        write_u64_at(&mut buf, 4, Some(0x0102_0304_0506_0708));
        assert_eq!(
            &buf[4..12],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
    }

    #[test]
    fn write_u64_at_none_leaves_buffer_unchanged() {
        let mut buf = vec![0xFFu8; 16];
        write_u64_at(&mut buf, 0, None);
        assert_eq!(&buf, &[0xFFu8; 16]);
    }

    #[test]
    fn write_u64_at_zero_value() {
        let mut buf = vec![0xFFu8; 8];
        write_u64_at(&mut buf, 0, Some(0));
        assert_eq!(&buf, &[0u8; 8]);
    }

    #[test]
    fn write_u64_at_max_value() {
        let mut buf = vec![0u8; 8];
        write_u64_at(&mut buf, 0, Some(u64::MAX));
        assert_eq!(&buf, &[0xFFu8; 8]);
    }

    // --- remove_attribute_at -------------------------------------------------

    const ATTRS_OFF: usize = 0x38;
    const BUF_SIZE: usize = 4096;

    /// Build a buffer with bytes_used at [0x18..0x1C] and the given bytes
    /// placed at `attr_offset`. Trailing data follows immediately after.
    fn attr_buf(attr_offset: usize, attr_bytes: &[u8], trailing: &[u8]) -> Vec<u8> {
        let bytes_used = attr_offset + attr_bytes.len() + trailing.len();
        let mut buf = vec![0u8; BUF_SIZE];
        buf[0x18..0x1C].copy_from_slice(&(bytes_used as u32).to_le_bytes());
        buf[0x1C..0x20].copy_from_slice(&(BUF_SIZE as u32).to_le_bytes());
        buf[attr_offset..attr_offset + attr_bytes.len()].copy_from_slice(attr_bytes);
        let trailing_start = attr_offset + attr_bytes.len();
        buf[trailing_start..trailing_start + trailing.len()].copy_from_slice(trailing);
        buf
    }

    /// Build a minimal resident attribute blob (for use in attr_buf or as a real attr).
    fn minimal_resident_attr(type_code: u32, value: &[u8]) -> Vec<u8> {
        let header_size = 24usize;
        let total = (header_size + value.len() + 7) & !7;
        let mut attr = vec![0u8; total];
        attr[0..4].copy_from_slice(&type_code.to_le_bytes());
        attr[attr_off::LENGTH..attr_off::LENGTH + 4].copy_from_slice(&(total as u32).to_le_bytes());
        attr[attr_off::NON_RESIDENT] = 0;
        attr[attr_off::RESIDENT_VALUE_LENGTH..attr_off::RESIDENT_VALUE_LENGTH + 4]
            .copy_from_slice(&(value.len() as u32).to_le_bytes());
        attr[attr_off::RESIDENT_VALUE_OFFSET..attr_off::RESIDENT_VALUE_OFFSET + 2]
            .copy_from_slice(&(header_size as u16).to_le_bytes());
        attr[header_size..header_size + value.len()].copy_from_slice(value);
        attr
    }

    /// Build a full MFT record with one named or unnamed resident attribute + end marker.
    fn one_attr_record(type_code: u32, value: &[u8]) -> Vec<u8> {
        let attr = minimal_resident_attr(type_code, value);
        let attr_len = attr.len();
        let end_pos = ATTRS_OFF + attr_len;
        let bytes_used = end_pos + 4;

        let mut rec = vec![0u8; BUF_SIZE];
        rec[0x14..0x16].copy_from_slice(&(ATTRS_OFF as u16).to_le_bytes());
        rec[0x18..0x1C].copy_from_slice(&(bytes_used as u32).to_le_bytes());
        rec[0x1C..0x20].copy_from_slice(&(BUF_SIZE as u32).to_le_bytes());
        rec[ATTRS_OFF..ATTRS_OFF + attr_len].copy_from_slice(&attr);
        rec[end_pos..end_pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        rec
    }

    #[test]
    fn remove_attribute_at_shifts_trailing_data_back() {
        let attr_bytes = [0xAAu8; 8];
        let trailing = b"ENDMARK_";
        let mut buf = attr_buf(ATTRS_OFF, &attr_bytes, trailing);

        remove_attribute_at(&mut buf, ATTRS_OFF, 8).unwrap();

        assert_eq!(&buf[ATTRS_OFF..ATTRS_OFF + trailing.len()], trailing);
        let new_bu = u32::from_le_bytes([buf[0x18], buf[0x19], buf[0x1A], buf[0x1B]]) as usize;
        assert_eq!(new_bu, ATTRS_OFF + trailing.len());
    }

    #[test]
    fn remove_attribute_at_zeroes_vacated_tail() {
        let attr_bytes = [0xBBu8; 8];
        let trailing = b"TAIL";
        let mut buf = attr_buf(ATTRS_OFF, &attr_bytes, trailing);
        let bytes_used_before =
            u32::from_le_bytes([buf[0x18], buf[0x19], buf[0x1A], buf[0x1B]]) as usize;

        remove_attribute_at(&mut buf, ATTRS_OFF, 8).unwrap();

        let new_bu = u32::from_le_bytes([buf[0x18], buf[0x19], buf[0x1A], buf[0x1B]]) as usize;
        // Bytes [new_bu .. bytes_used_before] must be zeroed
        for &b in &buf[new_bu..bytes_used_before] {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn remove_attribute_at_zero_length_fails() {
        let mut buf = vec![0u8; 64];
        buf[0x18..0x1C].copy_from_slice(&32u32.to_le_bytes());
        assert!(remove_attribute_at(&mut buf, 0, 0).is_err());
    }

    #[test]
    fn remove_attribute_at_out_of_range_fails() {
        let mut buf = vec![0u8; 64];
        buf[0x18..0x1C].copy_from_slice(&32u32.to_le_bytes()); // bytes_used = 32
                                                               // attr_offset + attr_length = 0 + 40 = 40 > 32
        assert!(remove_attribute_at(&mut buf, 0, 40).is_err());
    }

    // --- remove_unnamed_attr -------------------------------------------------

    #[test]
    fn remove_unnamed_attr_removes_present_attribute() {
        let mut rec = one_attr_record(0x80 /* $DATA */, b"hello");
        let bu_before = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        remove_unnamed_attr(&mut rec, AttrType::Data).unwrap();
        let bu_after = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        assert!(bu_after < bu_before);
        // Attribute should be gone
        assert!(crate::attr_io::find_attribute(&rec, AttrType::Data, None).is_none());
    }

    #[test]
    fn remove_unnamed_attr_absent_is_noop() {
        let mut rec = one_attr_record(0x80 /* $DATA */, b"x");
        let bu_before = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        remove_unnamed_attr(&mut rec, AttrType::ReparsePoint).unwrap();
        let bu_after = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        assert_eq!(bu_before, bu_after);
    }

    // --- commit_eas ----------------------------------------------------------

    fn empty_record() -> Vec<u8> {
        // A record with no attributes — just the end marker.
        let end_pos = ATTRS_OFF;
        let bytes_used = end_pos + 4;
        let mut rec = vec![0u8; BUF_SIZE];
        rec[0x14..0x16].copy_from_slice(&(ATTRS_OFF as u16).to_le_bytes());
        rec[0x18..0x1C].copy_from_slice(&(bytes_used as u32).to_le_bytes());
        rec[0x1C..0x20].copy_from_slice(&(BUF_SIZE as u32).to_le_bytes());
        rec[0x28..0x2A].copy_from_slice(&0u16.to_le_bytes()); // next_attr_id
        rec[end_pos..end_pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        rec
    }

    fn make_ea(name: &[u8], value: &[u8]) -> crate::ea_io::Ea {
        crate::ea_io::Ea {
            flags: 0,
            name: name.to_vec(),
            value: value.to_vec(),
        }
    }

    #[test]
    fn commit_eas_empty_list_removes_ea_attrs() {
        // Start with $EA + $EA_INFORMATION already present, commit empty → both removed.
        let mut rec = empty_record();
        let eas = vec![make_ea(b"FOO", b"bar")];
        commit_eas(&mut rec, &eas).unwrap();
        // Verify $EA exists.
        assert!(crate::attr_io::find_attribute(&rec, AttrType::ExtendedAttribute, None).is_some());
        // Now commit empty list.
        commit_eas(&mut rec, &[]).unwrap();
        assert!(
            crate::attr_io::find_attribute(&rec, AttrType::ExtendedAttribute, None).is_none(),
            "$EA must be removed when EA list is empty"
        );
        assert!(
            crate::attr_io::find_attribute(&rec, AttrType::ExtendedAttributeInformation, None)
                .is_none(),
            "$EA_INFORMATION must be removed when EA list is empty"
        );
    }

    #[test]
    fn commit_eas_writes_ea_and_ea_information() {
        let mut rec = empty_record();
        let eas = vec![make_ea(b"MYATTR", b"hello")];
        commit_eas(&mut rec, &eas).unwrap();

        assert!(
            crate::attr_io::find_attribute(&rec, AttrType::ExtendedAttribute, None).is_some(),
            "$EA attribute must be present after commit"
        );
        assert!(
            crate::attr_io::find_attribute(&rec, AttrType::ExtendedAttributeInformation, None)
                .is_some(),
            "$EA_INFORMATION must be present after commit"
        );
    }

    #[test]
    fn commit_eas_roundtrip_decode() {
        let mut rec = empty_record();
        let eas = vec![make_ea(b"KEY1", b"value1"), make_ea(b"KEY2", &[0xDE, 0xAD])];
        commit_eas(&mut rec, &eas).unwrap();

        // Decode back via ea_io::read_from_record.
        let decoded = crate::ea_io::read_from_record(&rec).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].name, b"KEY1");
        assert_eq!(decoded[0].value, b"value1");
        assert_eq!(decoded[1].name, b"KEY2");
        assert_eq!(decoded[1].value, &[0xDE, 0xAD]);
    }

    #[test]
    fn commit_eas_update_replaces_existing() {
        let mut rec = empty_record();
        commit_eas(&mut rec, &[make_ea(b"K", b"old")]).unwrap();
        commit_eas(&mut rec, &[make_ea(b"K", b"new_value")]).unwrap();

        let decoded = crate::ea_io::read_from_record(&rec).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].value, b"new_value");
    }

    // --- upsert_unnamed_resident_attr ----------------------------------------

    #[test]
    fn upsert_unnamed_resident_attr_inserts_when_absent() {
        let mut rec = empty_record();
        // Use build_resident_ea_information_attribute as the builder.
        upsert_unnamed_resident_attr(
            &mut rec,
            AttrType::ExtendedAttributeInformation,
            &[0u8; 8],
            &crate::record_build::build_resident_ea_information_attribute,
        )
        .unwrap();
        assert!(
            crate::attr_io::find_attribute(&rec, AttrType::ExtendedAttributeInformation, None)
                .is_some()
        );
    }

    #[test]
    fn upsert_unnamed_resident_attr_replaces_when_present() {
        let mut rec = empty_record();
        let val1 = [1u8; 8];
        let val2 = [2u8; 8];
        upsert_unnamed_resident_attr(
            &mut rec,
            AttrType::ExtendedAttributeInformation,
            &val1,
            &crate::record_build::build_resident_ea_information_attribute,
        )
        .unwrap();
        upsert_unnamed_resident_attr(
            &mut rec,
            AttrType::ExtendedAttributeInformation,
            &val2,
            &crate::record_build::build_resident_ea_information_attribute,
        )
        .unwrap();

        let loc =
            crate::attr_io::find_attribute(&rec, AttrType::ExtendedAttributeInformation, None)
                .unwrap();
        let val_off = loc.resident_value_offset.unwrap() as usize;
        assert_eq!(
            &rec[loc.attr_offset + val_off..loc.attr_offset + val_off + 8],
            &val2
        );
    }

    // --- write_reparse_point_io / read_reparse_point_io / remove_reparse_point_io ---

    #[test]
    fn write_and_read_reparse_point_roundtrip() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "rp.txt").unwrap();
        let tag = 0xA000_000C_u32; // SYMLINK tag
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        write_reparse_point_io(&mut dev, "/rp.txt", tag, &data).unwrap();
        let rp = read_reparse_point_io(&mut dev, "/rp.txt").unwrap().unwrap();
        assert_eq!(rp.reparse_tag, tag);
        assert_eq!(rp.data, data);
    }

    #[test]
    fn read_reparse_point_absent_returns_none() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "norp.txt").unwrap();
        let result = read_reparse_point_io(&mut dev, "/norp.txt").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn remove_reparse_point_clears_attribute() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "rmrp.txt").unwrap();
        write_reparse_point_io(&mut dev, "/rmrp.txt", 0xA000_000C, &[1, 2, 3, 4]).unwrap();
        remove_reparse_point_io(&mut dev, "/rmrp.txt").unwrap();
        let result = read_reparse_point_io(&mut dev, "/rmrp.txt").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn write_reparse_point_twice_replaces() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "rp2.txt").unwrap();
        write_reparse_point_io(&mut dev, "/rp2.txt", 0xA000_000C, &[1, 2, 3, 4]).unwrap();
        write_reparse_point_io(&mut dev, "/rp2.txt", 0xA000_000C, &[0xAA, 0xBB]).unwrap();
        let rp = read_reparse_point_io(&mut dev, "/rp2.txt")
            .unwrap()
            .unwrap();
        assert_eq!(rp.data, vec![0xAA, 0xBB]);
    }

    #[test]
    fn remove_reparse_point_on_absent_file_fails() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "norprm.txt").unwrap();
        assert!(remove_reparse_point_io(&mut dev, "/norprm.txt").is_err());
    }

    // --- create_symlink_io ----------------------------------------------------

    #[test]
    fn create_symlink_io_creates_file_with_reparse_point() {
        let mut dev = fresh_vol();
        create_symlink_io(&mut dev, "/", "link.txt", "/target.txt", false).unwrap();
        let rp = read_reparse_point_io(&mut dev, "/link.txt")
            .unwrap()
            .unwrap();
        assert_eq!(rp.reparse_tag, crate::record_build::reparse_tag::SYMLINK);
    }

    #[test]
    fn create_symlink_io_relative() {
        let mut dev = fresh_vol();
        create_symlink_io(&mut dev, "/", "rellink", "target", true).unwrap();
        let rp = read_reparse_point_io(&mut dev, "/rellink")
            .unwrap()
            .unwrap();
        assert_eq!(rp.reparse_tag, crate::record_build::reparse_tag::SYMLINK);
    }

    #[test]
    fn create_symlink_io_duplicate_fails() {
        let mut dev = fresh_vol();
        create_symlink_io(&mut dev, "/", "dup_link", "/t", false).unwrap();
        assert!(create_symlink_io(&mut dev, "/", "dup_link", "/t", false).is_err());
    }

    // --- read_attributes_io ---------------------------------------------------

    #[test]
    fn read_attributes_io_fresh_file_has_standard_attrs() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "attrs.txt").unwrap();
        let descs = read_attributes_io(&mut dev, "/attrs.txt").unwrap();
        let type_names: Vec<&str> = descs.iter().map(|d| d.type_name.as_str()).collect();
        assert!(
            type_names.contains(&"$STANDARD_INFORMATION"),
            "missing $STD_INFO"
        );
        assert!(type_names.contains(&"$FILE_NAME"), "missing $FILE_NAME");
        assert!(type_names.contains(&"$DATA"), "missing $DATA");
    }

    #[test]
    fn read_attributes_io_dir_has_index_root() {
        let mut dev = fresh_vol();
        mkdir_io(&mut dev, "/", "subdir").unwrap();
        let descs = read_attributes_io(&mut dev, "/subdir").unwrap();
        let type_names: Vec<&str> = descs.iter().map(|d| d.type_name.as_str()).collect();
        assert!(
            type_names.contains(&"$INDEX_ROOT"),
            "dir must have $INDEX_ROOT"
        );
    }

    #[test]
    fn read_attributes_io_returns_count_and_attr_ids() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "idcheck.txt").unwrap();
        let descs = read_attributes_io(&mut dev, "/idcheck.txt").unwrap();
        // Attribute IDs must be unique within a record.
        let ids: Vec<u16> = descs.iter().map(|d| d.attribute_id).collect();
        let mut sorted = ids.clone();
        // dedup() only removes *consecutive* duplicates, so sort first to
        // catch IDs that repeat out of record order.
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "attribute IDs must be unique");
    }

    // --- path-based public wrappers ------------------------------------------
    //
    // The `*_io` variants above run against an in-memory `MemDev`. The
    // path-based public wrappers (`create_file`, `mkdir`, `write_at`, …) are
    // thin delegations that open a `PathIo` over a real file and forward to
    // the `_io` form. These tests exercise that delegation + `PathIo::open_rw`
    // against a freshly formatted temp-file image, then clean it up.

    /// A formatted NTFS image backed by a real temp file, deleted on drop.
    struct TmpImage {
        path: std::path::PathBuf,
    }

    impl TmpImage {
        fn new() -> Self {
            use std::io::Write as _;
            use std::sync::atomic::{AtomicU64, Ordering};
            // Unique name from pid + a monotonic counter (no rng/clock needed).
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("fs_ntfs_wrap_{pid}_{n}.img"));

            const SIZE: u64 = 16 * 1024 * 1024;
            // Create the backing file at the right size.
            {
                let mut f = std::fs::File::create(&path)
                    .unwrap_or_else(|e| panic!("create temp image {}: {e}", path.display()));
                f.set_len(SIZE).expect("set_len");
                f.flush().expect("flush");
            }
            // Format it through the path-backed BlockIo.
            {
                let mut io = PathIo::open_rw(&path).expect("open_rw temp image");
                format_filesystem(
                    &mut io as &mut dyn BlockIo,
                    SIZE,
                    4096,
                    4096,
                    Some("PATHWR"),
                    Some(0x1234_5678),
                )
                .expect("format_filesystem temp image");
            }
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TmpImage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn create_file_path_wrapper_creates_findable_file() {
        let img = TmpImage::new();
        let rec = create_file(img.path(), "/", "pathfile.txt").unwrap();
        assert!(rec >= 24, "user files start at record 24+");
        // Verify it landed in the root index.
        let mut io = PathIo::open_ro(img.path()).unwrap();
        let (_, root_rec) = crate::mft_io::read_mft_record_io(&mut io, 5).unwrap();
        assert!(crate::index_io::find_index_entry(&root_rec, "pathfile.txt")
            .unwrap()
            .is_some());
    }

    #[test]
    fn mkdir_path_wrapper_creates_directory() {
        let img = TmpImage::new();
        let rec = mkdir(img.path(), "/", "pathdir").unwrap();
        assert!(rec >= 24);
        let mut io = PathIo::open_ro(img.path()).unwrap();
        let (_, root_rec) = crate::mft_io::read_mft_record_io(&mut io, 5).unwrap();
        assert!(crate::index_io::find_index_entry(&root_rec, "pathdir")
            .unwrap()
            .is_some());
    }

    #[test]
    fn create_file_path_wrapper_duplicate_fails() {
        let img = TmpImage::new();
        create_file(img.path(), "/", "dup.txt").unwrap();
        assert!(create_file(img.path(), "/", "dup.txt").is_err());
    }

    #[test]
    fn write_at_and_truncate_path_wrappers() {
        let img = TmpImage::new();
        create_file(img.path(), "/", "data.txt").unwrap();
        // A fresh file's $DATA is resident; write_at/grow only operate on
        // non-resident $DATA, so promote first (this also exercises the
        // promote_resident_data_to_nonresident path wrapper).
        promote_resident_data_to_nonresident(img.path(), "/data.txt", &[0u8; 8192]).unwrap();
        let n = write_at(img.path(), "/data.txt", 0, b"hello").unwrap();
        assert_eq!(n, 5);
        let sz = truncate(img.path(), "/data.txt", 0).unwrap();
        assert_eq!(sz, 0);
    }

    #[test]
    fn rename_same_length_path_wrapper() {
        let img = TmpImage::new();
        // Create inside a subdir: rename patches the parent's resident
        // $INDEX_ROOT, which holds for a small subdir (the root dir uses
        // $INDEX_ALLOCATION and isn't supported by this MVP primitive).
        mkdir(img.path(), "/", "d").unwrap();
        create_file(img.path(), "/d", "aaa.txt").unwrap();
        rename_same_length(img.path(), "/d/aaa.txt", "bbb.txt").unwrap();
        // The renamed file must be resolvable; the old name must be gone.
        assert!(
            read_attributes(img.path(), "/d/bbb.txt").is_ok(),
            "renamed file must be resolvable"
        );
        assert!(read_attributes(img.path(), "/d/aaa.txt").is_err());
    }

    #[test]
    fn set_times_path_wrapper() {
        let img = TmpImage::new();
        create_file(img.path(), "/", "t.txt").unwrap();
        let ts: u64 = 132_000_000_000_000;
        let times = FileTimes {
            creation: Some(ts),
            modification: Some(ts),
            mft_record_modification: Some(ts),
            access: Some(ts),
        };
        set_times(img.path(), "/t.txt", times).unwrap();
    }

    #[test]
    fn set_security_id_and_attributes_path_wrappers() {
        let img = TmpImage::new();
        create_file(img.path(), "/", "s.txt").unwrap();
        set_security_id(img.path(), "/s.txt", 0x100).unwrap();
        // Add HIDDEN (0x02), remove nothing.
        let change = FileAttributesChange {
            add: 0x02,
            remove: 0,
        };
        set_file_attributes(img.path(), "/s.txt", change).unwrap();
    }

    #[test]
    fn write_and_list_eas_path_wrappers() {
        let img = TmpImage::new();
        create_file(img.path(), "/", "ea.txt").unwrap();
        write_ea(img.path(), "/ea.txt", b"USER.attr", b"value", 0).unwrap();
        let eas = list_eas(img.path(), "/ea.txt").unwrap();
        assert!(eas
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case(b"USER.attr")));
        remove_ea(img.path(), "/ea.txt", b"USER.attr").unwrap();
        let after = list_eas(img.path(), "/ea.txt").unwrap();
        assert!(!after
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case(b"USER.attr")));
    }

    #[test]
    fn read_attributes_path_wrapper() {
        let img = TmpImage::new();
        create_file(img.path(), "/", "ra.txt").unwrap();
        let descs = read_attributes(img.path(), "/ra.txt").unwrap();
        // A fresh file has at least $STANDARD_INFORMATION + $FILE_NAME + $DATA.
        assert!(descs.len() >= 3);
    }

    // --- named streams (ADS) -------------------------------------------------

    #[test]
    fn write_named_stream_resident_io_then_list() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "ads.txt").unwrap();
        write_named_stream_resident_io(&mut dev, "/ads.txt", "meta", b"hello").unwrap();
        let names = list_named_streams_io(&mut dev, "/ads.txt").unwrap();
        assert_eq!(names, vec!["meta".to_string()]);
    }

    #[test]
    fn write_named_stream_resident_io_empty_name_fails() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "ads.txt").unwrap();
        assert!(write_named_stream_resident_io(&mut dev, "/ads.txt", "", b"x").is_err());
    }

    #[test]
    fn write_named_stream_resident_io_replaces_existing() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "ads.txt").unwrap();
        write_named_stream_resident_io(&mut dev, "/ads.txt", "s", b"first").unwrap();
        write_named_stream_resident_io(&mut dev, "/ads.txt", "s", b"second-longer").unwrap();
        // Still exactly one stream named "s" (replaced, not duplicated).
        let names = list_named_streams_io(&mut dev, "/ads.txt").unwrap();
        assert_eq!(names.iter().filter(|n| *n == "s").count(), 1);
    }

    #[test]
    fn write_named_stream_high_level_io_resident() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "ads.txt").unwrap();
        // Small data stays resident via the high-level dispatcher.
        write_named_stream_io(&mut dev, "/ads.txt", "small", b"tiny").unwrap();
        let names = list_named_streams_io(&mut dev, "/ads.txt").unwrap();
        assert!(names.contains(&"small".to_string()));
    }

    #[test]
    fn list_named_streams_io_empty_when_no_ads() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "plain.txt").unwrap();
        let names = list_named_streams_io(&mut dev, "/plain.txt").unwrap();
        assert!(names.is_empty(), "fresh file has no ADS, got {names:?}");
    }

    #[test]
    fn list_named_streams_io_multiple() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "multi.txt").unwrap();
        write_named_stream_resident_io(&mut dev, "/multi.txt", "a", b"1").unwrap();
        write_named_stream_resident_io(&mut dev, "/multi.txt", "b", b"2").unwrap();
        let mut names = list_named_streams_io(&mut dev, "/multi.txt").unwrap();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn delete_named_stream_io_removes_stream() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "del.txt").unwrap();
        write_named_stream_resident_io(&mut dev, "/del.txt", "gone", b"data").unwrap();
        delete_named_stream_io(&mut dev, "/del.txt", "gone").unwrap();
        let names = list_named_streams_io(&mut dev, "/del.txt").unwrap();
        assert!(!names.contains(&"gone".to_string()));
    }

    #[test]
    fn delete_named_stream_io_absent_fails() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "del.txt").unwrap();
        assert!(delete_named_stream_io(&mut dev, "/del.txt", "nope").is_err());
    }

    #[test]
    fn delete_named_stream_io_empty_name_fails() {
        let mut dev = fresh_vol();
        create_file_io(&mut dev, "/", "del.txt").unwrap();
        assert!(delete_named_stream_io(&mut dev, "/del.txt", "").is_err());
    }

    #[test]
    fn named_stream_path_wrappers_roundtrip() {
        let img = TmpImage::new();
        create_file(img.path(), "/", "ads.txt").unwrap();
        write_named_stream(img.path(), "/ads.txt", "tag", b"value").unwrap();
        let names = list_named_streams(img.path(), "/ads.txt").unwrap();
        assert!(names.contains(&"tag".to_string()));
        delete_named_stream(img.path(), "/ads.txt", "tag").unwrap();
        let after = list_named_streams(img.path(), "/ads.txt").unwrap();
        assert!(!after.contains(&"tag".to_string()));
    }
}
