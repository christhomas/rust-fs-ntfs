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
/// Flags byte within INDEX_HEADER.
const IH_FLAGS_OFFSET: usize = 0x0C;
/// INDEX_HEADER bit: any of the entries has a subnode pointer (i.e.
/// the index overflows into `$INDEX_ALLOCATION`).
pub const IH_FLAG_HAS_SUBNODES: u8 = 0x01;
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

/// True if the resident `$INDEX_ROOT:$I30` has any non-LAST entries.
/// Used by `rmdir` to verify a directory is empty.
pub fn index_root_has_real_entries(record: &[u8]) -> Result<bool, String> {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some("$I30"))
        .ok_or_else(|| "$INDEX_ROOT:$I30 not found".to_string())?;
    if !ir.is_resident {
        return Err("$INDEX_ROOT unexpectedly non-resident".to_string());
    }
    let val_off = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let ir_data_start = ir.attr_offset + val_off;
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
    let first_entry = ih_start + first_entry_rel;
    let end = ih_start + total_size;
    if first_entry + IE_KEY_START > record.len() || first_entry + 0x10 > end {
        return Ok(false);
    }
    // If the very first entry has the LAST flag, the dir is empty.
    let flags = u16::from_le_bytes([
        record[first_entry + IE_FLAGS],
        record[first_entry + IE_FLAGS + 1],
    ]);
    Ok(flags & IE_FLAG_LAST == 0)
}

/// Read the INDEX_HEADER flags byte from an `$INDEX_ROOT`. Returns
/// `Some(flags)` if the record contains `$INDEX_ROOT:$I30`,
/// otherwise `None`.
pub fn index_root_flags(record: &[u8]) -> Option<u8> {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some("$I30"))?;
    if !ir.is_resident {
        return None;
    }
    let val_off = ir.resident_value_offset? as usize;
    let ih_start = ir.attr_offset + val_off + IR_INDEX_HEADER_OFFSET;
    Some(record[ih_start + IH_FLAGS_OFFSET])
}

/// Scan a clean (post-fixup) INDX block buffer for the entry whose
/// filename matches `wanted`. Returns `Some(location)` on hit,
/// `None` otherwise.
///
/// The returned `record_offset` is relative to the start of the INDX
/// block — callers use it to patch bytes within the same buffer.
pub fn find_entry_in_indx_block(
    block: &[u8],
    wanted: &str,
) -> Result<Option<IndexEntryLocation>, String> {
    use crate::idx_block::{
        IH_FIRST_ENTRY_OFFSET, IH_TOTAL_SIZE_OF_ENTRIES, INDX_INDEX_HEADER_OFFSET,
    };
    if &block[0..4] != b"INDX" {
        return Err("not an INDX block (fixup missing?)".to_string());
    }
    let ih_start = INDX_INDEX_HEADER_OFFSET;
    let first_entry_rel = u32::from_le_bytes([
        block[ih_start + IH_FIRST_ENTRY_OFFSET],
        block[ih_start + IH_FIRST_ENTRY_OFFSET + 1],
        block[ih_start + IH_FIRST_ENTRY_OFFSET + 2],
        block[ih_start + IH_FIRST_ENTRY_OFFSET + 3],
    ]) as usize;
    let total_size = u32::from_le_bytes([
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES],
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 1],
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 2],
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 3],
    ]) as usize;
    let mut cursor = ih_start + first_entry_rel;
    let end = ih_start + total_size;
    scan_entries_for_name(block, &mut cursor, end, wanted)
}

/// Shared scanner: sweep entries starting at `cursor`, stopping at
/// `end` or IE_FLAG_LAST, returning the matching entry's location.
fn scan_entries_for_name(
    buf: &[u8],
    cursor: &mut usize,
    end: usize,
    wanted: &str,
) -> Result<Option<IndexEntryLocation>, String> {
    let wanted_utf16: Vec<u16> = wanted.encode_utf16().collect();
    while *cursor < end && *cursor + IE_KEY_START <= buf.len() {
        let length =
            u16::from_le_bytes([buf[*cursor + IE_LENGTH], buf[*cursor + IE_LENGTH + 1]]) as usize;
        let key_length = u16::from_le_bytes([
            buf[*cursor + IE_KEY_LENGTH],
            buf[*cursor + IE_KEY_LENGTH + 1],
        ]) as usize;
        let flags = u16::from_le_bytes([buf[*cursor + IE_FLAGS], buf[*cursor + IE_FLAGS + 1]]);

        if flags & IE_FLAG_LAST != 0 {
            break;
        }
        if length == 0 || *cursor + length > buf.len() {
            return Err(format!("malformed index entry at {cursor}"));
        }
        if key_length >= FN_NAME_OFFSET {
            let key_start = *cursor + IE_KEY_START;
            let name_length = buf[key_start + FN_NAME_LENGTH_OFFSET] as usize;
            let name_start = key_start + FN_NAME_OFFSET;
            if name_start + name_length * 2 <= buf.len() && name_length == wanted_utf16.len() {
                let name_u16: Vec<u16> = buf[name_start..name_start + name_length * 2]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                if name_u16 == wanted_utf16 {
                    let file_ref = u64::from_le_bytes(
                        buf[*cursor + IE_FILE_REFERENCE..*cursor + IE_FILE_REFERENCE + 8]
                            .try_into()
                            .unwrap(),
                    );
                    let file_record_number = file_ref & 0x0000_FFFF_FFFF_FFFF;
                    return Ok(Some(IndexEntryLocation {
                        record_offset: *cursor,
                        length,
                        key_length,
                        file_record_number,
                        name_length: name_length as u8,
                    }));
                }
            }
        }
        let _ = flags & IE_FLAG_HAS_SUBNODE;
        *cursor += length;
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

/// Remove an index entry from its containing block (either
/// `$INDEX_ROOT`'s resident value or an INDX block). Shifts following
/// entries back by `entry.length` bytes, updates the INDEX_HEADER's
/// `total_size` field, and zero-fills the tail.
///
/// For `$INDEX_ROOT` this also updates the attribute's resident
/// `value_length` via [`crate::attr_resize::resize_resident_value`]
/// so the MFT record's `bytes_used` stays consistent.
///
/// `block_kind` controls which header layout is expected:
/// * [`BlockKind::IndexRoot`] — the buffer is the whole MFT record;
///   entries are inside `$INDEX_ROOT`'s resident value.
/// * [`BlockKind::IndexAllocation`] — the buffer is a full INDX block.
pub fn remove_index_entry(
    buf: &mut [u8],
    entry: &IndexEntryLocation,
    block_kind: BlockKind,
) -> Result<(), String> {
    let (ih_start, _ir_attr_offset) = match block_kind {
        BlockKind::IndexRoot => {
            let ir = attr_io::find_attribute(buf, AttrType::IndexRoot, Some("$I30"))
                .ok_or_else(|| "$INDEX_ROOT:$I30 missing".to_string())?;
            let val_off = ir.resident_value_offset.ok_or("no value_offset")? as usize;
            (
                ir.attr_offset + val_off + IR_INDEX_HEADER_OFFSET,
                ir.attr_offset,
            )
        }
        BlockKind::IndexAllocation => (crate::idx_block::INDX_INDEX_HEADER_OFFSET, 0),
    };

    let total_size_pos = ih_start + IH_TOTAL_SIZE_OF_ENTRIES;
    let total_size = u32::from_le_bytes([
        buf[total_size_pos],
        buf[total_size_pos + 1],
        buf[total_size_pos + 2],
        buf[total_size_pos + 3],
    ]) as usize;
    let entry_end = entry.record_offset + entry.length;
    let tail_end = ih_start + total_size;
    if entry_end > tail_end {
        return Err("entry extends past total_size".to_string());
    }

    // Shift following entries back.
    let tail_len = tail_end - entry_end;
    buf.copy_within(entry_end..tail_end, entry.record_offset);
    // Zero-fill vacated range.
    let new_tail_end = entry.record_offset + tail_len;
    for byte in &mut buf[new_tail_end..tail_end] {
        *byte = 0;
    }

    // Update INDEX_HEADER.total_size.
    let new_total_size = (total_size - entry.length) as u32;
    buf[total_size_pos..total_size_pos + 4].copy_from_slice(&new_total_size.to_le_bytes());

    // For $INDEX_ROOT, also shrink the resident attribute so bytes_used
    // in the MFT record stays in sync.
    if matches!(block_kind, BlockKind::IndexRoot) {
        let ir = attr_io::find_attribute(buf, AttrType::IndexRoot, Some("$I30"))
            .ok_or("$INDEX_ROOT re-find failed")?;
        let old_val_len = ir.resident_value_length.ok_or("no value_length")?;
        let new_val_len = old_val_len.saturating_sub(entry.length as u32);
        crate::attr_resize::resize_resident_value(buf, ir.attr_offset, new_val_len)?;
    }

    Ok(())
}

/// Which kind of container holds the index entries being mutated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    IndexRoot,
    IndexAllocation,
}

/// Build an index entry for a new `$FILE_NAME` record. Returns an
/// 8-byte-aligned byte blob ready to insert into `$INDEX_ROOT` or an
/// INDX block.
///
/// Mirrors the `$FILE_NAME` attribute layout but wrapped in an index-
/// entry header.
pub fn build_file_name_index_entry(
    file_reference: u64,
    parent_reference: u64,
    name: &str,
    nt_time: u64,
    is_dir: bool,
) -> Result<Vec<u8>, String> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    if utf16.is_empty() || utf16.len() > 255 {
        return Err(format!("invalid name length {}", utf16.len()));
    }
    let key_fixed = 0x42usize;
    let key_len = key_fixed + utf16.len() * 2;
    let entry_len = (IE_KEY_START + key_len + 7) & !7; // align to 8

    let mut e = vec![0u8; entry_len];
    e[IE_FILE_REFERENCE..IE_FILE_REFERENCE + 8].copy_from_slice(&file_reference.to_le_bytes());
    e[IE_LENGTH..IE_LENGTH + 2].copy_from_slice(&(entry_len as u16).to_le_bytes());
    e[IE_KEY_LENGTH..IE_KEY_LENGTH + 2].copy_from_slice(&(key_len as u16).to_le_bytes());
    e[IE_FLAGS..IE_FLAGS + 2].copy_from_slice(&0u16.to_le_bytes());

    let k = IE_KEY_START;
    e[k..k + 8].copy_from_slice(&parent_reference.to_le_bytes());
    e[k + 8..k + 16].copy_from_slice(&nt_time.to_le_bytes()); // creation
    e[k + 16..k + 24].copy_from_slice(&nt_time.to_le_bytes()); // modification
    e[k + 24..k + 32].copy_from_slice(&nt_time.to_le_bytes()); // mft_mod
    e[k + 32..k + 40].copy_from_slice(&nt_time.to_le_bytes()); // access
                                                               // alloc_size + real_size = 0
    e[k + 40..k + 48].copy_from_slice(&0u64.to_le_bytes());
    e[k + 48..k + 56].copy_from_slice(&0u64.to_le_bytes());
    let fa: u32 = if is_dir { 0x10000000 | 0x20 } else { 0x20 };
    e[k + 56..k + 60].copy_from_slice(&fa.to_le_bytes());
    e[k + 60..k + 64].copy_from_slice(&0u32.to_le_bytes()); // ea/reparse
    e[k + FN_NAME_LENGTH_OFFSET] = utf16.len() as u8;
    e[k + FN_NAMESPACE_OFFSET] = 3; // Win32+DOS
    for (i, c) in utf16.iter().enumerate() {
        let off = k + FN_NAME_OFFSET + i * 2;
        e[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    Ok(e)
}

/// Insert a freshly-built index entry into a resident `$INDEX_ROOT:$I30`.
///
/// Performs:
/// 1. Grows the `$INDEX_ROOT` attribute by `entry_bytes.len()` via
///    [`crate::attr_resize::resize_resident_value`].
/// 2. Shifts existing entries forward so the new entry lands at the
///    correct sorted position (byte-order comparison on filename —
///    adequate for ASCII names, case-insensitive NTFS collation is a
///    future refinement).
/// 3. Patches INDEX_HEADER.total_size.
///
/// Returns Err if the resulting `$INDEX_ROOT` wouldn't fit in the
/// record.
pub fn insert_entry_into_index_root(
    record: &mut [u8],
    entry_bytes: &[u8],
    new_name: &str,
) -> Result<(), String> {
    insert_entry_into_index_root_with_collation(record, entry_bytes, new_name, None)
}

/// See [`insert_entry_into_index_root`]. Accepts an optional upcase
/// table for NTFS-compliant collation on non-ASCII names. Pass `None`
/// for the ASCII-fallback comparator (sufficient for plain English
/// filenames but incorrect for non-ASCII).
pub fn insert_entry_into_index_root_with_collation(
    record: &mut [u8],
    entry_bytes: &[u8],
    new_name: &str,
    upcase: Option<&crate::upcase::UpcaseTable>,
) -> Result<(), String> {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some("$I30"))
        .ok_or_else(|| "$INDEX_ROOT:$I30 missing".to_string())?;
    let val_off = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let old_val_len = ir.resident_value_length.ok_or("no value_length")? as usize;
    let ih_start = ir.attr_offset + val_off + IR_INDEX_HEADER_OFFSET;

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

    // Find sorted insertion position. Walk existing entries until we
    // find one whose name is >= new_name or hit the LAST sentinel.
    let mut cursor = ih_start + first_entry_rel;
    let end = ih_start + total_size;
    let new_utf16: Vec<u16> = new_name.encode_utf16().collect();

    while cursor < end && cursor + IE_KEY_START <= record.len() {
        let length =
            u16::from_le_bytes([record[cursor + IE_LENGTH], record[cursor + IE_LENGTH + 1]])
                as usize;
        let flags = u16::from_le_bytes([record[cursor + IE_FLAGS], record[cursor + IE_FLAGS + 1]]);
        if flags & IE_FLAG_LAST != 0 {
            break; // insertion point is immediately before LAST
        }
        if length == 0 || cursor + length > record.len() {
            return Err("malformed index during insert".to_string());
        }
        let key_start = cursor + IE_KEY_START;
        let name_len = record[key_start + FN_NAME_LENGTH_OFFSET] as usize;
        let name_start = key_start + FN_NAME_OFFSET;
        let existing_utf16: Vec<u16> = record[name_start..name_start + name_len * 2]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if compare_names(&new_utf16, &existing_utf16, upcase) != std::cmp::Ordering::Greater {
            // new goes before existing
            break;
        }
        cursor += length;
    }
    let insertion_point = cursor;

    // Grow $INDEX_ROOT by entry_bytes.len() bytes.
    let new_val_len = (old_val_len + entry_bytes.len()) as u32;
    // resize_resident_value may move the attribute's following bytes.
    // Capture the insertion_point's offset RELATIVE to the attribute's
    // value start so we can recompute after resize.
    let attr_val_start_old = ir.attr_offset + val_off;
    let insertion_in_value = insertion_point - attr_val_start_old;

    crate::attr_resize::resize_resident_value(record, ir.attr_offset, new_val_len)?;

    // Recompute the attribute value start (resize shifted nothing
    // because we grew by an amount that preserves existing attr offset —
    // but compute defensively anyway).
    let ir2 = attr_io::find_attribute(record, AttrType::IndexRoot, Some("$I30"))
        .ok_or("$INDEX_ROOT vanished")?;
    let val_off2 = ir2.resident_value_offset.ok_or("no value_offset")? as usize;
    let attr_val_start = ir2.attr_offset + val_off2;
    let insertion_point = attr_val_start + insertion_in_value;

    // Shift existing bytes from insertion_point forward by entry_bytes.len().
    let shift_src_end = attr_val_start + old_val_len;
    record.copy_within(
        insertion_point..shift_src_end,
        insertion_point + entry_bytes.len(),
    );
    // Copy the new entry in.
    record[insertion_point..insertion_point + entry_bytes.len()].copy_from_slice(entry_bytes);

    // Bump total_size in INDEX_HEADER.
    let ih_start2 = attr_val_start + IR_INDEX_HEADER_OFFSET;
    let new_total_size = (total_size + entry_bytes.len()) as u32;
    record[ih_start2 + IH_TOTAL_SIZE_OF_ENTRIES..ih_start2 + IH_TOTAL_SIZE_OF_ENTRIES + 4]
        .copy_from_slice(&new_total_size.to_le_bytes());

    Ok(())
}

/// Insert a new `$FILE_NAME` index entry into an INDX block at the
/// correct sorted position. Fails if the block doesn't have room.
///
/// The caller is responsible for INDX USA fixup on read + write (use
/// `idx_block::update_indx_block`).
pub fn insert_entry_into_indx_block(
    block: &mut [u8],
    entry_bytes: &[u8],
    new_name: &str,
) -> Result<(), String> {
    insert_entry_into_indx_block_with_collation(block, entry_bytes, new_name, None)
}

/// See [`insert_entry_into_indx_block`]. Accepts an optional upcase
/// table for correct NTFS collation.
pub fn insert_entry_into_indx_block_with_collation(
    block: &mut [u8],
    entry_bytes: &[u8],
    new_name: &str,
    upcase: Option<&crate::upcase::UpcaseTable>,
) -> Result<(), String> {
    use crate::idx_block::{
        IH_FIRST_ENTRY_OFFSET, IH_TOTAL_SIZE_OF_ENTRIES, INDX_INDEX_HEADER_OFFSET,
    };
    if &block[0..4] != b"INDX" {
        return Err("not an INDX block".to_string());
    }
    let ih_start = INDX_INDEX_HEADER_OFFSET;
    let first_entry_rel = u32::from_le_bytes([
        block[ih_start + IH_FIRST_ENTRY_OFFSET],
        block[ih_start + IH_FIRST_ENTRY_OFFSET + 1],
        block[ih_start + IH_FIRST_ENTRY_OFFSET + 2],
        block[ih_start + IH_FIRST_ENTRY_OFFSET + 3],
    ]) as usize;
    let total_size = u32::from_le_bytes([
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES],
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 1],
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 2],
        block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 3],
    ]) as usize;
    let allocated_size = u32::from_le_bytes([
        block[ih_start + 8],
        block[ih_start + 9],
        block[ih_start + 10],
        block[ih_start + 11],
    ]) as usize;
    let new_len = entry_bytes.len();
    if total_size + new_len > allocated_size {
        return Err(format!(
            "INDX block has no room: total_size={total_size} + new={new_len} > allocated={allocated_size}"
        ));
    }

    // Find sorted insertion position.
    let mut cursor = ih_start + first_entry_rel;
    let end = ih_start + total_size;
    let new_utf16: Vec<u16> = new_name.encode_utf16().collect();

    while cursor < end && cursor + IE_KEY_START <= block.len() {
        let length =
            u16::from_le_bytes([block[cursor + IE_LENGTH], block[cursor + IE_LENGTH + 1]]) as usize;
        let flags = u16::from_le_bytes([block[cursor + IE_FLAGS], block[cursor + IE_FLAGS + 1]]);
        if flags & IE_FLAG_LAST != 0 {
            break;
        }
        if length == 0 || cursor + length > block.len() {
            return Err("malformed INDX entry during insert".to_string());
        }
        let key_start = cursor + IE_KEY_START;
        let name_len = block[key_start + FN_NAME_LENGTH_OFFSET] as usize;
        let name_start = key_start + FN_NAME_OFFSET;
        let existing_utf16: Vec<u16> = block[name_start..name_start + name_len * 2]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if compare_names(&new_utf16, &existing_utf16, upcase) != std::cmp::Ordering::Greater {
            break;
        }
        cursor += length;
    }
    let insertion_point = cursor;

    // Shift [insertion_point .. end) forward by new_len; the LAST
    // sentinel (or end-of-entries) moves with it.
    block.copy_within(insertion_point..end, insertion_point + new_len);
    block[insertion_point..insertion_point + new_len].copy_from_slice(entry_bytes);

    // Update total_size.
    let new_total = (total_size + new_len) as u32;
    block[ih_start + IH_TOTAL_SIZE_OF_ENTRIES..ih_start + IH_TOTAL_SIZE_OF_ENTRIES + 4]
        .copy_from_slice(&new_total.to_le_bytes());

    Ok(())
}

/// Compare two UTF-16 names under the callers's chosen collation.
/// `None` falls back to an ASCII-only upcase-fold (works for plain
/// English names but mis-orders anything with non-ASCII). `Some(table)`
/// uses the NTFS `$UpCase` table for COLLATION_FILE_NAME correctness.
pub fn compare_names(
    a: &[u16],
    b: &[u16],
    upcase: Option<&crate::upcase::UpcaseTable>,
) -> std::cmp::Ordering {
    if let Some(t) = upcase {
        return t.cmp_names(a, b);
    }
    let map = |c: u16| -> u16 {
        if (c as u32) < 128 {
            (c as u8).to_ascii_uppercase() as u16
        } else {
            c
        }
    };
    let iter = a.iter().copied().map(map).zip(b.iter().copied().map(map));
    for (ac, bc) in iter {
        match ac.cmp(&bc) {
            std::cmp::Ordering::Equal => continue,
            ord => return ord,
        }
    }
    a.len().cmp(&b.len())
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
