//! Read + mutate entries inside an `$INDEX_ROOT` attribute. Works on a
//! post-fixup MFT record buffer — used from inside `update_mft_record`
//! mutator closures.
//!
//! Reference (no GPL code consulted): [Flatcap Indexes](https://flatcap.github.io/linux-ntfs/ntfs/concepts/indexes.html),
//! [Flatcap $INDEX_ROOT](https://flatcap.github.io/linux-ntfs/ntfs/attributes/index_root.html),
//! [Flatcap $FILE_NAME](https://flatcap.github.io/linux-ntfs/ntfs/attributes/file_name.html).

use crate::attr_io::{self, AttrType};

/// Offsets inside `$INDEX_ROOT`'s resident value.
/// Layout: `INDEX_ROOT_HEADER (16 bytes) + INDEX_HEADER (16 bytes) + entries…`.
const IR_INDEX_HEADER_OFFSET: usize = 16;
/// Offsets inside `INDEX_HEADER`:
const IH_FIRST_ENTRY_OFFSET: usize = 0;
const IH_TOTAL_SIZE_OF_ENTRIES: usize = 4;

/// Offsets inside an index entry.
const IE_FILE_REFERENCE: usize = 0x00;
const IE_LENGTH: usize = 0x08;
const IE_KEY_LENGTH: usize = 0x0A;
const IE_FLAGS: usize = 0x0C;
const IE_KEY_START: usize = 0x10;

/// Entry flag bits.
const IE_FLAG_HAS_SUBNODE: u16 = 0x01;
const IE_FLAG_LAST: u16 = 0x02;

/// Offsets within a `$FILE_NAME` key (the "filename namespace" layout).
/// Parent reference (8) + 4×NT time (32) + alloc_size (8) + real_size (8)
/// + file_attributes (4) + reserved (4) + name_length (1) + namespace (1).
const FN_NAME_LENGTH_OFFSET: usize = 0x40;
#[allow(dead_code)]
const FN_NAMESPACE_OFFSET: usize = 0x41;
const FN_NAME_OFFSET: usize = 0x42;

/// One located index entry inside an `$INDEX_ROOT`.
#[derive(Debug, Clone, Copy)]
pub struct IndexEntryLocation {
    /// Byte offset within the MFT record where the entry starts.
    pub record_offset: usize,
    /// Entry length in bytes.
    pub length: usize,
    /// Length of the $FILE_NAME key in bytes.
    pub key_length: usize,
    /// File record number this entry points to (low 48 bits of file_reference).
    pub file_record_number: u64,
    /// Length of the filename in UTF-16 code units.
    pub name_length: u8,
}

/// Walk the `$INDEX_ROOT` for `$FILE_NAME` (i.e. the `$I30` index of a
/// directory), returning the located entry whose filename matches
/// `wanted` (case-sensitive UTF-16 equality). Returns `None` if not
/// present.
///
/// The walk stops when it encounters an entry with the
/// `IE_FLAG_LAST` bit. Nested `$INDEX_ALLOCATION` blocks are not
/// searched here — this primitive only works for small directories
/// whose index fits entirely in `$INDEX_ROOT`.
pub fn find_index_entry(record: &[u8], wanted: &str) -> Result<Option<IndexEntryLocation>, String> {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some("$I30"))
        .ok_or_else(|| "$INDEX_ROOT:$I30 not found".to_string())?;
    if !ir.is_resident {
        return Err("$INDEX_ROOT is non-resident (impossible per spec)".to_string());
    }
    let ir_value_offset = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let ir_data_start = ir.attr_offset + ir_value_offset;

    let ih_start = ir_data_start + IR_INDEX_HEADER_OFFSET;
    let first_entry_rel = u32::from_le_bytes([
        record[ih_start + IH_FIRST_ENTRY_OFFSET],
        record[ih_start + IH_FIRST_ENTRY_OFFSET + 1],
        record[ih_start + IH_FIRST_ENTRY_OFFSET + 2],
        record[ih_start + IH_FIRST_ENTRY_OFFSET + 3],
    ]) as usize;
    let total_size = u32::from_le_bytes([
        record[ih_start + IH_TOTAL_SIZE_OF_ENTRIES],
        record[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 1],
        record[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 2],
        record[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 3],
    ]) as usize;

    let mut cursor = ih_start + first_entry_rel;
    let end = ih_start + total_size;

    let wanted_utf16: Vec<u16> = wanted.encode_utf16().collect();

    while cursor < end && cursor + IE_KEY_START <= record.len() {
        let length =
            u16::from_le_bytes([record[cursor + IE_LENGTH], record[cursor + IE_LENGTH + 1]])
                as usize;
        let key_length = u16::from_le_bytes([
            record[cursor + IE_KEY_LENGTH],
            record[cursor + IE_KEY_LENGTH + 1],
        ]) as usize;
        let flags = u16::from_le_bytes([record[cursor + IE_FLAGS], record[cursor + IE_FLAGS + 1]]);

        // The trailing entry has flags & IE_FLAG_LAST and key_length = 0.
        // It's a no-match sentinel — stop scanning.
        if flags & IE_FLAG_LAST != 0 {
            break;
        }
        if length == 0 || cursor + length > record.len() {
            return Err(format!("malformed index entry at {cursor}"));
        }
        if key_length >= FN_NAME_OFFSET {
            let key_start = cursor + IE_KEY_START;
            let name_length = record[key_start + FN_NAME_LENGTH_OFFSET] as usize;
            let name_start = key_start + FN_NAME_OFFSET;
            if name_start + name_length * 2 <= record.len() && name_length == wanted_utf16.len() {
                let name_u16: Vec<u16> = record[name_start..name_start + name_length * 2]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                if name_u16 == wanted_utf16 {
                    let file_ref = u64::from_le_bytes(
                        record[cursor + IE_FILE_REFERENCE..cursor + IE_FILE_REFERENCE + 8]
                            .try_into()
                            .unwrap(),
                    );
                    let file_record_number = file_ref & 0x0000_FFFF_FFFF_FFFF;
                    return Ok(Some(IndexEntryLocation {
                        record_offset: cursor,
                        length,
                        key_length,
                        file_record_number,
                        name_length: name_length as u8,
                    }));
                }
            }
        }
        // Ignore the subnode VCN tail if present — for resident-only walk.
        let _ = flags & IE_FLAG_HAS_SUBNODE;
        cursor += length;
    }
    Ok(None)
}

/// Overwrite the UTF-16 name bytes inside an existing index entry's
/// `$FILE_NAME` key. Requires `new_name.encode_utf16().count() ==
/// entry.name_length`; other cases need entry resize, which is future
/// work.
pub fn rename_index_entry_same_length(
    record: &mut [u8],
    entry: &IndexEntryLocation,
    new_name: &str,
) -> Result<(), String> {
    let utf16: Vec<u16> = new_name.encode_utf16().collect();
    if utf16.len() != entry.name_length as usize {
        return Err(format!(
            "same-length rename required (got {} u16 code units, expected {})",
            utf16.len(),
            entry.name_length
        ));
    }
    let name_start = entry.record_offset + IE_KEY_START + FN_NAME_OFFSET;
    for (i, c) in utf16.iter().enumerate() {
        let off = name_start + i * 2;
        record[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    Ok(())
}

/// Overwrite the UTF-16 name bytes inside the file's own
/// `$FILE_NAME` attribute (there may be multiple `$FILE_NAME`s — one
/// per namespace). Uses the first one whose current name matches
/// `old_name` and whose length matches `new_name.encode_utf16().len()`.
pub fn rename_filename_attribute_same_length(
    record: &mut [u8],
    old_name: &str,
    new_name: &str,
) -> Result<(), String> {
    let old_utf16: Vec<u16> = old_name.encode_utf16().collect();
    let new_utf16: Vec<u16> = new_name.encode_utf16().collect();
    if new_utf16.len() != old_utf16.len() {
        return Err("same-length rename required on $FILE_NAME".to_string());
    }
    let mut patched = false;
    for loc in attr_io::iter_attributes(record).collect::<Vec<_>>() {
        if loc.type_code != AttrType::FileName as u32 {
            continue;
        }
        let value_offset = match loc.resident_value_offset {
            Some(v) => v as usize,
            None => continue,
        };
        let data_start = loc.attr_offset + value_offset;
        let name_length_byte = record[data_start + FN_NAME_LENGTH_OFFSET] as usize;
        if name_length_byte != old_utf16.len() {
            continue;
        }
        let name_start = data_start + FN_NAME_OFFSET;
        let cur: Vec<u16> = record[name_start..name_start + name_length_byte * 2]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if cur != old_utf16 {
            continue;
        }
        for (i, c) in new_utf16.iter().enumerate() {
            let off = name_start + i * 2;
            record[off..off + 2].copy_from_slice(&c.to_le_bytes());
        }
        patched = true;
    }
    if !patched {
        return Err(format!(
            "no matching $FILE_NAME attribute with old name '{old_name}' of length {}",
            old_utf16.len()
        ));
    }
    Ok(())
}
