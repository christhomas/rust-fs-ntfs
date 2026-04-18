//! Write operations that modify existing MFT records in-place. No
//! attribute resize, no cluster allocation, no new files. See STATUS.md
//! Phase W1 for the exact scope.
//!
//! Path resolution uses upstream `ntfs` (read-only). The actual write
//! goes through [`mft_io::update_mft_record`] which handles USA fixup
//! and `fsync`. The mutator closures here use [`attr_io`] to locate
//! attributes without touching upstream.

use crate::attr_io::{self, AttrType};
use crate::bitmap;
use crate::data_runs::{self, DataRun};
use crate::idx_block;
use crate::index_io;
use crate::mft_bitmap;
use crate::mft_io::{read_mft_record, update_mft_record, MFT_FLAG_DIRECTORY};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsFile};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Seek, SeekFrom, Write};
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

// $STANDARD_INFORMATION field offsets (Flatcap /ntfs/attributes/standard_information.html).
const SI_CREATION: usize = 0x00;
const SI_MODIFICATION: usize = 0x08;
const SI_MFT_MODIFICATION: usize = 0x10;
const SI_ACCESS: usize = 0x18;
const SI_FILE_ATTRIBUTES: usize = 0x20;

/// Set file times on the file at `file_path`. Only modifies
/// `$STANDARD_INFORMATION`; does not touch the duplicate times in the
/// parent directory's `$FILE_NAME` index (Windows itself only updates
/// them on rename/create).
pub fn set_times(path: &Path, file_path: &str, times: FileTimes) -> Result<(), String> {
    let rec = resolve_path_to_record_number(path, file_path)?;
    set_times_by_record_number(path, rec, times)
}

/// Set file times on an MFT record by number (bypasses path resolution).
pub fn set_times_by_record_number(
    path: &Path,
    record_number: u64,
    times: FileTimes,
) -> Result<(), String> {
    update_mft_record(path, record_number, |record| {
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

/// Modify the `file_attributes` field in `$STANDARD_INFORMATION`. Bits in
/// `add` are ORed on; bits in `remove` are ANDed off. `add` and `remove`
/// overlap is not allowed (caller must not ask to both add and remove
/// the same bit).
pub fn set_file_attributes(
    path: &Path,
    file_path: &str,
    change: FileAttributesChange,
) -> Result<(), String> {
    let rec = resolve_path_to_record_number(path, file_path)?;
    set_file_attributes_by_record_number(path, rec, change)
}

/// See [`set_file_attributes`].
pub fn set_file_attributes_by_record_number(
    path: &Path,
    record_number: u64,
    change: FileAttributesChange,
) -> Result<(), String> {
    if change.add & change.remove != 0 {
        return Err(format!(
            "add and remove overlap: add={:#x} remove={:#x}",
            change.add, change.remove
        ));
    }
    update_mft_record(path, record_number, |record| {
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
    let rec = resolve_path_to_record_number(image, file_path)?;
    write_at_by_record_number(image, rec, offset, data)
}

/// See [`write_at`]. Takes a record number instead of a path.
pub fn write_at_by_record_number(
    image: &Path,
    record_number: u64,
    offset: u64,
    data: &[u8],
) -> Result<u64, String> {
    let (params, record) = read_mft_record(image, record_number)?;
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
    let mut fh = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .map_err(|e| format!("open rw: {e}"))?;

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
        fh.seek(SeekFrom::Start(disk_offset))
            .map_err(|e| format!("seek write: {e}"))?;
        fh.write_all(&data[cursor_in_data..cursor_in_data + chunk])
            .map_err(|e| format!("write: {e}"))?;

        cursor_in_data += chunk;
        file_offset += chunk as u64;
    }

    fh.sync_all().map_err(|e| format!("fsync: {e}"))?;
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
    let rec = resolve_path_to_record_number(image, file_path)?;
    truncate_by_record_number(image, rec, new_size)
}

/// Attribute-header offsets for non-resident lengths (Flatcap).
const NONRES_ALLOCATED_LENGTH: usize = 0x28;
const NONRES_DATA_LENGTH: usize = 0x30;
const NONRES_INITIALIZED_LENGTH: usize = 0x38;
const NONRES_LAST_VCN: usize = 0x18;

pub fn truncate_by_record_number(
    image: &Path,
    record_number: u64,
    new_size: u64,
) -> Result<u64, String> {
    let (params, record) = read_mft_record(image, record_number)?;
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
    update_mft_record(image, record_number, |record| {
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
        let bm = bitmap::locate_bitmap(image)?;
        for (lcn, n) in &clusters_to_free {
            // Best-effort: if a double-free occurs we report, but don't
            // undo the earlier record update — the file size change has
            // committed.
            bitmap::free(image, &bm, *lcn, *n)
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
    let rec = resolve_path_to_record_number(image, file_path)?;
    grow_nonresident_by_record_number(image, rec, new_size)
}

pub fn grow_nonresident_by_record_number(
    image: &Path,
    record_number: u64,
    new_size: u64,
) -> Result<u64, String> {
    let (params, record) = read_mft_record(image, record_number)?;
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
        return apply_grow_lengths(image, record_number, new_size, new_allocated);
    }

    // Ask bitmap for a contiguous run.
    let bm = bitmap::locate_bitmap(image)?;
    let hint = runs
        .iter()
        .rev()
        .find_map(|r| r.lcn.map(|lcn| lcn + r.length))
        .unwrap_or(params.mft_lcn.saturating_add(32));
    let new_lcn = bitmap::find_free_run(image, &bm, need_clusters, hint)?
        .ok_or_else(|| format!("no contiguous free run of {need_clusters} clusters available"))?;
    bitmap::allocate(image, &bm, new_lcn, need_clusters)?;

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
        bitmap::free(image, &bm, new_lcn, need_clusters)?;
        return Err(format!(
            "new mapping_pairs ({} bytes) exceed attr capacity ({}). Attribute resize (W2.1) required.",
            new_mapping.len(),
            mapping_capacity
        ));
    }

    // Commit: rewrite MFT record with new mapping + lengths.
    update_mft_record(image, record_number, |record| {
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
fn apply_grow_lengths(
    image: &Path,
    record_number: u64,
    new_size: u64,
    new_allocated: u64,
) -> Result<u64, String> {
    update_mft_record(image, record_number, |record| {
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
/// spillover). For a fresh mkntfs volume this holds for small
/// subdirectories (e.g. `/Documents`) but NOT for the root directory
/// — mkntfs lays out `/` with `$INDEX_ALLOCATION` even when small.
/// Walking + patching `$INDEX_ALLOCATION` blocks is a separate
/// primitive (index_io::find_in_index_allocation, future work).
pub fn rename_same_length(image: &Path, old_path: &str, new_name: &str) -> Result<(), String> {
    if new_name.contains('/') || new_name.is_empty() {
        return Err("new_name must be a basename (no slashes, non-empty)".to_string());
    }
    let (parent_rec, file_rec, current_basename) = resolve_parent_and_child(image, old_path)?;

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
    let (_, parent_record_bytes) = read_mft_record(image, parent_rec)?;
    let ir_flags = index_io::index_root_flags(&parent_record_bytes)
        .ok_or_else(|| "no $INDEX_ROOT on parent".to_string())?;

    let in_root = index_io::find_index_entry(&parent_record_bytes, &current_basename)?;
    if let Some(entry_found) = in_root {
        if entry_found.file_record_number != file_rec {
            return Err(format!(
                "parent's $INDEX_ROOT entry for '{current_basename}' points at record {} \
                 but the resolved path points at {file_rec}",
                entry_found.file_record_number
            ));
        }
        update_mft_record(image, parent_rec, |record| {
            let entry = index_io::find_index_entry(record, &current_basename)?
                .ok_or_else(|| "race: $INDEX_ROOT entry vanished during RMW".to_string())?;
            index_io::rename_index_entry_same_length(record, &entry, new_name)
        })?;
    } else if ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
        // Linear scan of allocated INDX blocks.
        let ia = idx_block::load_for_directory(image, parent_rec)?;
        let mut patched = false;
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block(image, &ia, vcn)?;
            if let Some(entry) = index_io::find_entry_in_indx_block(&block, &current_basename)? {
                if entry.file_record_number != file_rec {
                    return Err(format!(
                        "INDX entry at VCN {vcn} points at {} but resolved {file_rec}",
                        entry.file_record_number
                    ));
                }
                idx_block::update_indx_block(image, &ia, vcn, |block| {
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
    update_mft_record(image, file_rec, |record| {
        index_io::rename_filename_attribute_same_length(record, &current_basename, new_name)
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Unlink (W3)
// ---------------------------------------------------------------------------

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
    let (parent_rec, file_rec, basename) = resolve_parent_and_child(image, file_path)?;

    // Refuse directory targets.
    let (_, file_record_bytes) = read_mft_record(image, file_rec)?;
    let flags = crate::mft_io::record_flags(&file_record_bytes);
    if flags & MFT_FLAG_DIRECTORY != 0 {
        return Err(format!(
            "unlink: '{file_path}' is a directory — use rmdir (not implemented)"
        ));
    }

    // 1) Remove the parent's index entry. Dispatch on IR flags.
    let (_, parent_record_bytes) = read_mft_record(image, parent_rec)?;
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
        update_mft_record(image, parent_rec, |record| {
            let e = index_io::find_index_entry(record, &basename)?
                .ok_or_else(|| "race: $INDEX_ROOT entry vanished".to_string())?;
            index_io::remove_index_entry(record, &e, index_io::BlockKind::IndexRoot)
        })?;
    } else if ir_flags & index_io::IH_FLAG_HAS_SUBNODES != 0 {
        let ia = idx_block::load_for_directory(image, parent_rec)?;
        let mut removed = false;
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block(image, &ia, vcn)?;
            if let Some(entry) = index_io::find_entry_in_indx_block(&block, &basename)? {
                if entry.file_record_number != file_rec {
                    return Err(format!(
                        "INDX entry at VCN {vcn} points at {} but resolved {file_rec}",
                        entry.file_record_number
                    ));
                }
                idx_block::update_indx_block(image, &ia, vcn, |block| {
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

    // 2) Free data clusters. Only if non-resident — resident $DATA lives
    //    inside the MFT record and is freed as part of the record itself.
    //    truncate to 0 is a no-op for resident data anyway.
    let data_loc = attr_io::find_attribute(&file_record_bytes, AttrType::Data, None);
    if let Some(loc) = data_loc {
        if !loc.is_resident && loc.non_resident_value_length.unwrap_or(0) > 0 {
            truncate_by_record_number(image, file_rec, 0)?;
        }
    }

    // 3) Clear IN_USE flag in the file's MFT record.
    update_mft_record(image, file_rec, |record| {
        let flags_off = 0x16;
        let cur = u16::from_le_bytes([record[flags_off], record[flags_off + 1]]);
        let new = cur & !crate::mft_io::MFT_FLAG_IN_USE;
        record[flags_off..flags_off + 2].copy_from_slice(&new.to_le_bytes());
        Ok(())
    })?;

    // 4) Free the MFT record bit.
    let mbm = mft_bitmap::locate(image)?;
    mft_bitmap::free(image, &mbm, file_rec)?;

    Ok(())
}

/// Resolve `old_path` to `(parent_record_number, file_record_number, basename)`.
fn resolve_parent_and_child(image: &Path, old_path: &str) -> Result<(u64, u64, String), String> {
    let p = old_path.trim_start_matches('/');
    if p.is_empty() {
        return Err("cannot rename root".to_string());
    }
    let (parent_path, basename) = match p.rsplit_once('/') {
        Some((par, base)) => (par, base),
        None => ("", p),
    };
    let parent_full = format!("/{parent_path}");
    let parent_rec = resolve_path_to_record_number(image, &parent_full)?;
    let file_rec = resolve_path_to_record_number(image, old_path)?;
    Ok((parent_rec, file_rec, basename.to_string()))
}

/// Walk `file_path` via upstream and return the target's MFT record number.
pub fn resolve_path_to_record_number(path: &Path, file_path: &str) -> Result<u64, String> {
    let f = File::open(path).map_err(|e| format!("open ro: {e}"))?;
    let mut reader = BufReader::new(f);
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
