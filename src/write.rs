//! Write operations that modify existing MFT records in-place. No
//! attribute resize, no cluster allocation, no new files. See STATUS.md
//! Phase W1 for the exact scope.
//!
//! Path resolution uses upstream `ntfs` (read-only). The actual write
//! goes through [`mft_io::update_mft_record`] which handles USA fixup
//! and `fsync`. The mutator closures here use [`attr_io`] to locate
//! attributes without touching upstream.

use crate::attr_io::{self, AttrType};
use crate::mft_io::update_mft_record;

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsFile};
use std::fs::File;
use std::io::BufReader;
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
