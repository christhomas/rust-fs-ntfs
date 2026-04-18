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
use crate::mft_io::{read_mft_record, update_mft_record};

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
