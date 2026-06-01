//! Read + mutate entries inside an `$INDEX_ROOT` attribute. Works on a
//! post-fixup MFT record buffer — used from inside `update_mft_record`
//! mutator closures.
//!
//! References (no GPL code consulted): NTFS index B-tree layout,
//! $INDEX_ROOT, and $FILE_NAME attribute formats per Windows Internals
//! 7th ed. ch. "NTFS On-Disk Structure" and MS-FSCC.

use crate::attr_io::{self, read_u32_le, AttrType};
use crate::mkfs::stream;

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
/// Offset of `allocated_size_of_entries` within `INDEX_HEADER`.
/// Spec invariant: `allocated_size >= total_size`. When we grow the
/// $INDEX_ROOT's resident value (insert path), both fields move
/// together — only updating `total_size` makes ntfs.sys raise
/// Event 55 "A corruption was found in a file system index
/// structure ... :$I30:$INDEX_ROOT" against rec 5 (Iter "Group A"
/// trace 2026-05-23, scenario
/// `mac-format-mkdir-set-dirty-win-chkdsk`).
const IH_ALLOCATED_SIZE_OF_ENTRIES: usize = 8;

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
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
        .ok_or_else(|| "$INDEX_ROOT:$I30 not found".to_string())?;
    if !ir.is_resident {
        return Err("$INDEX_ROOT is non-resident (impossible per spec)".to_string());
    }
    let ir_value_offset = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let ir_data_start = ir.attr_offset + ir_value_offset;

    let ih_start = ir_data_start + IR_INDEX_HEADER_OFFSET;
    let first_entry_rel = read_u32_le(record, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or_else(|| "index header too short to read first_entry_offset".to_string())?
        as usize;
    let total_size = read_u32_le(record, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or_else(|| "index header too short to read total_size".to_string())?
        as usize;

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
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
        .ok_or_else(|| "$INDEX_ROOT:$I30 not found".to_string())?;
    if !ir.is_resident {
        return Err("$INDEX_ROOT unexpectedly non-resident".to_string());
    }
    let val_off = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let ir_data_start = ir.attr_offset + val_off;
    let ih_start = ir_data_start + IR_INDEX_HEADER_OFFSET;
    let first_entry_rel = read_u32_le(record, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or_else(|| "index header too short to read first_entry_offset".to_string())?
        as usize;
    let total_size = read_u32_le(record, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or_else(|| "index header too short to read total_size".to_string())?
        as usize;
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
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))?;
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
    let first_entry_rel = read_u32_le(block, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or_else(|| "INDX block too short to read first_entry_offset".to_string())?
        as usize;
    let total_size = read_u32_le(block, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or_else(|| "INDX block too short to read total_size".to_string())?
        as usize;
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

/// One enumerated `$FILE_NAME` directory entry. Unlike [`IndexEntryLocation`]
/// (which locates an entry for mutation), this carries the decoded name +
/// namespace for read-side directory listing.
#[derive(Debug, Clone)]
pub struct DirEntryRaw {
    /// Target file's MFT record number (low 48 bits of the file_reference).
    pub file_record_number: u64,
    /// Filename (lossy UTF-16 → UTF-8).
    pub name: String,
    /// `$FILE_NAME` namespace: 0=POSIX, 1=Win32, 2=DOS, 3=Win32+DOS.
    pub namespace: u8,
    /// `$FILE_NAME.file_attributes` (the index entry's duplicate copy). The
    /// `IS_DIRECTORY` bit (`0x1000_0000`) is how NTFS marks a directory in an
    /// index entry — this is what upstream's `is_directory()` reads.
    pub file_attributes: u32,
}

/// Append every real `$FILE_NAME` entry in one index node (an `$INDEX_ROOT`
/// value or an INDX block) to `out`. `ih_start` is the byte offset of the
/// node's INDEX_HEADER within `buf`. Stops at the `IE_FLAG_LAST` sentinel.
/// Shared by the two public enumerators below so the entry walk lives in one
/// place (mirrors [`scan_entries_for_name`] but collects instead of matching).
fn collect_entries(buf: &[u8], ih_start: usize, out: &mut Vec<DirEntryRaw>) -> Result<(), String> {
    let first_entry_rel = read_u32_le(buf, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or("index node too short to read first_entry_offset")?
        as usize;
    let total_size = read_u32_le(buf, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or("index node too short to read total_size")? as usize;
    let mut cursor = ih_start + first_entry_rel;
    let end = ih_start + total_size;
    while cursor < end && cursor + IE_KEY_START <= buf.len() {
        let length =
            u16::from_le_bytes([buf[cursor + IE_LENGTH], buf[cursor + IE_LENGTH + 1]]) as usize;
        let key_length =
            u16::from_le_bytes([buf[cursor + IE_KEY_LENGTH], buf[cursor + IE_KEY_LENGTH + 1]])
                as usize;
        let flags = u16::from_le_bytes([buf[cursor + IE_FLAGS], buf[cursor + IE_FLAGS + 1]]);
        if flags & IE_FLAG_LAST != 0 {
            break;
        }
        if length == 0 || cursor + length > buf.len() {
            return Err(format!("malformed index entry at {cursor}"));
        }
        // Bound every read to THIS entry [cursor, entry_end), not the whole
        // buffer: a corrupt key_length/name_length must not let us decode
        // bytes from the next entry/trailer into a fabricated DirEntryRaw.
        let entry_end = cursor + length; // already validated <= buf.len()
        let key_start = cursor + IE_KEY_START;
        if key_length >= FN_NAME_OFFSET && key_start + key_length <= entry_end {
            let name_length = buf[key_start + FN_NAME_LENGTH_OFFSET] as usize;
            let namespace = buf[key_start + FN_NAMESPACE_OFFSET];
            let name_start = key_start + FN_NAME_OFFSET;
            // $FILE_NAME.file_attributes: u32 at key+0x38 (after parent_ref(8)
            // + 4 timestamps(32) + alloc_size(8) + real_size(8)).
            let file_attributes = read_u32_le(buf, key_start + 0x38).unwrap_or(0);
            // The UTF-16 name must lie within the key (hence within the entry).
            if name_start + name_length * 2 <= key_start + key_length {
                let name: String = char::decode_utf16(
                    buf[name_start..name_start + name_length * 2]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                )
                .map(|r| r.unwrap_or('\u{FFFD}'))
                .collect();
                let file_ref = u64::from_le_bytes(
                    buf[cursor + IE_FILE_REFERENCE..cursor + IE_FILE_REFERENCE + 8]
                        .try_into()
                        .unwrap(),
                );
                out.push(DirEntryRaw {
                    file_record_number: file_ref & 0x0000_FFFF_FFFF_FFFF,
                    name,
                    namespace,
                    file_attributes,
                });
            }
        }
        cursor += length;
    }
    Ok(())
}

/// Enumerate the `$FILE_NAME` entries in a directory's resident `$INDEX_ROOT`.
pub fn collect_index_root_entries(record: &[u8], out: &mut Vec<DirEntryRaw>) -> Result<(), String> {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
        .ok_or_else(|| "$INDEX_ROOT:$I30 not found".to_string())?;
    let ir_value_offset = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let ih_start = ir.attr_offset + ir_value_offset + IR_INDEX_HEADER_OFFSET;
    collect_entries(record, ih_start, out)
}

/// Enumerate the `$FILE_NAME` entries in one `$INDEX_ALLOCATION` (INDX) block
/// (already read + USA-fixed).
pub fn collect_indx_block_entries(block: &[u8], out: &mut Vec<DirEntryRaw>) -> Result<(), String> {
    if block.len() < 4 || &block[0..4] != b"INDX" {
        return Err("not an INDX block (fixup missing?)".to_string());
    }
    collect_entries(block, crate::idx_block::INDX_INDEX_HEADER_OFFSET, out)
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
            let ir = attr_io::find_attribute(buf, AttrType::IndexRoot, Some(stream::I30))
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
    // in the MFT record stays in sync, and keep allocated_size ==
    // total_size. The resident index has no slack — its allocated region
    // IS the attribute's resident value size — so a stale allocated_size
    // left larger than total_size after a removal is exactly what chkdsk
    // flags as "Error detected in index $I30 for file <n>" (the mirror of
    // the insert path's invariant; see insert_entry_into_index_root). INDX
    // blocks keep their fixed allocated_size — slack there is normal and
    // expected, so this only applies to $INDEX_ROOT.
    if matches!(block_kind, BlockKind::IndexRoot) {
        let alloc_pos = ih_start + IH_ALLOCATED_SIZE_OF_ENTRIES;
        buf[alloc_pos..alloc_pos + 4].copy_from_slice(&new_total_size.to_le_bytes());

        let ir = attr_io::find_attribute(buf, AttrType::IndexRoot, Some(stream::I30))
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
    // The index entry's embedded $FILE_NAME copy must agree with the
    // MFT record's $FILE_NAME on namespace. Hardcoding WIN32_AND_DOS
    // here made chkdsk Stage 2 emit "Index entry X in index $I30 of
    // file M is incorrect" for any non-8.3 user name (matrix scenario
    // mac-format-mac-write-win-repeat-mount-3-win-chkdsk 2026-05-23,
    // after the MFT-side namespace fix landed).
    e[k + FN_NAMESPACE_OFFSET] = crate::record_build::fn_namespace_for(name);
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
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
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
    let ir2 = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
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

    // Bump both `total_size` and `allocated_size` in INDEX_HEADER.
    // For a resident $INDEX_ROOT every entry byte is part of the
    // allocated region (no slack — the attribute's resident value
    // size IS the alloc size). They must stay equal; updating only
    // `total_size` violates the spec invariant
    // `allocated_size >= total_size` and trips Event 55 on mount.
    let ih_start2 = attr_val_start + IR_INDEX_HEADER_OFFSET;
    let new_size = (total_size + entry_bytes.len()) as u32;
    record[ih_start2 + IH_TOTAL_SIZE_OF_ENTRIES..ih_start2 + IH_TOTAL_SIZE_OF_ENTRIES + 4]
        .copy_from_slice(&new_size.to_le_bytes());
    record[ih_start2 + IH_ALLOCATED_SIZE_OF_ENTRIES..ih_start2 + IH_ALLOCATED_SIZE_OF_ENTRIES + 4]
        .copy_from_slice(&new_size.to_le_bytes());

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
    let first_entry_rel = read_u32_le(block, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or_else(|| "INDX block too short to read first_entry_offset".to_string())?
        as usize;
    let total_size = read_u32_le(block, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or_else(|| "INDX block too short to read total_size".to_string())?
        as usize;
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

/// Compare two UTF-16 names byte-for-byte (no upcase folding) — the
/// comparator a case-sensitive directory should use. Win10 1803+
/// supports `FILE_ATTRIBUTE_CASE_SENSITIVE_DIR` on $FILE_NAME's
/// file_attributes (used by WSL and Docker-Desktop volumes for
/// container-image storage); inside such a directory, `foo.txt` and
/// `FOO.TXT` are distinct files.
///
/// Today this comparator is **not yet wired into `find_index_entry`
/// or the insert paths** — those still use `compare_names` (case-
/// insensitive) unconditionally. Plumbing the per-directory flag
/// through is the next step (future-features.md §3.9). This function
/// is the building block.
///
/// The bit position of `FILE_ATTRIBUTE_CASE_SENSITIVE_DIR` within
/// $FILE_NAME.file_attributes / $STANDARD_INFORMATION.file_attributes
/// is **not yet pinned** in our spec notes — multiple values circulate
/// across third-party documentation. Determining the right bit by
/// byte-diff against a reference WSL/Docker volume is part of the
/// follow-up.
pub fn compare_names_ordinal(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        match a[i].cmp(&b[i]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr_io::attr_off;

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    // --- MFT record builder for $INDEX_ROOT tests ---

    const ATTRS_START: usize = 0x38;
    const REC_SIZE: usize = 4096;
    const REC_OFF_ATTRS_OFFSET: usize = 0x14;
    const REC_OFF_BYTES_USED: usize = 0x18;
    const REC_OFF_BYTES_ALLOCATED: usize = 0x1C;

    /// Build the INDEX_ROOT value bytes: INDEX_ROOT_HEADER(16) + INDEX_HEADER(16) + entries.
    /// `entries` is already-serialized entry blobs. Appends the LAST sentinel automatically.
    fn build_index_root_value(entries: &[Vec<u8>]) -> Vec<u8> {
        // LAST sentinel: 16 bytes, flags = IE_FLAG_LAST = 0x02
        let mut last = vec![0u8; 16];
        last[IE_LENGTH] = 16;
        last[IE_FLAGS] = IE_FLAG_LAST as u8;

        let entries_bytes: Vec<u8> = entries.iter().flat_map(|e| e.iter().copied()).collect();
        let total_entries_size = entries_bytes.len() + last.len();
        // INDEX_HEADER (16 bytes): first_entry_offset=16, total_size, allocated_size, flags
        let mut ih = vec![0u8; 16];
        let first_entry_off = 16u32; // entries start 16 bytes into INDEX_HEADER
        let total_size = (first_entry_off as usize + total_entries_size) as u32;
        ih[0..4].copy_from_slice(&first_entry_off.to_le_bytes());
        ih[4..8].copy_from_slice(&total_size.to_le_bytes());
        ih[8..12].copy_from_slice(&total_size.to_le_bytes()); // allocated = total

        let mut value = vec![0u8; 16]; // INDEX_ROOT_HEADER (16 zeros are fine for tests)
        value.extend_from_slice(&ih);
        value.extend_from_slice(&entries_bytes);
        value.extend_from_slice(&last);
        value
    }

    /// Build a minimal MFT record with a named resident $INDEX_ROOT:$I30 attribute.
    fn index_root_record(entries: &[Vec<u8>]) -> Vec<u8> {
        let value = build_index_root_value(entries);
        let i30_utf16: Vec<u16> = "$I30".encode_utf16().collect();
        let i30_bytes: Vec<u8> = i30_utf16.iter().flat_map(|c| c.to_le_bytes()).collect();

        // Attribute header: type(4) + length(4) + non_res(1) + name_len(1) + name_off(2) + flags(2) + id(2)
        //                   + val_length(4) + val_offset(2) + indexed(1) + reserved(1) = 24 bytes fixed
        let header_fixed = 24usize;
        let name_offset = header_fixed as u16;
        let value_offset = (header_fixed + i30_bytes.len()) as u16;
        let attr_len = ((value_offset as usize + value.len()) + 7) & !7;
        let end_marker_pos = ATTRS_START + attr_len;
        let bytes_used = end_marker_pos + 4;

        let mut rec = vec![0u8; REC_SIZE];
        rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
            .copy_from_slice(&(ATTRS_START as u16).to_le_bytes());
        rec[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4]
            .copy_from_slice(&(bytes_used as u32).to_le_bytes());
        rec[REC_OFF_BYTES_ALLOCATED..REC_OFF_BYTES_ALLOCATED + 4]
            .copy_from_slice(&(REC_SIZE as u32).to_le_bytes());

        let a = ATTRS_START;
        rec[a..a + 4].copy_from_slice(&(AttrType::IndexRoot as u32).to_le_bytes());
        rec[a + attr_off::LENGTH..a + attr_off::LENGTH + 4]
            .copy_from_slice(&(attr_len as u32).to_le_bytes());
        rec[a + attr_off::NON_RESIDENT] = 0;
        rec[a + attr_off::NAME_LENGTH] = i30_utf16.len() as u8;
        rec[a + attr_off::NAME_OFFSET..a + attr_off::NAME_OFFSET + 2]
            .copy_from_slice(&name_offset.to_le_bytes());
        rec[a + attr_off::RESIDENT_VALUE_LENGTH..a + attr_off::RESIDENT_VALUE_LENGTH + 4]
            .copy_from_slice(&(value.len() as u32).to_le_bytes());
        rec[a + attr_off::RESIDENT_VALUE_OFFSET..a + attr_off::RESIDENT_VALUE_OFFSET + 2]
            .copy_from_slice(&value_offset.to_le_bytes());
        rec[a + header_fixed..a + header_fixed + i30_bytes.len()].copy_from_slice(&i30_bytes);
        let val_start = a + value_offset as usize;
        rec[val_start..val_start + value.len()].copy_from_slice(&value);
        rec[end_marker_pos..end_marker_pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        rec
    }

    /// Build a single $FILE_NAME index entry (matching build_file_name_index_entry layout).
    fn make_entry(file_ref: u64, parent_ref: u64, name: &str) -> Vec<u8> {
        build_file_name_index_entry(file_ref, parent_ref, name, 0, false).unwrap()
    }

    // --- find_index_entry ---

    #[test]
    fn find_index_entry_empty_dir_returns_none() {
        let rec = index_root_record(&[]);
        let result = find_index_entry(&rec, "foo").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_index_entry_finds_present_entry() {
        let entry = make_entry(42, 5, "hello");
        let rec = index_root_record(&[entry]);
        let loc = find_index_entry(&rec, "hello").unwrap().unwrap();
        assert_eq!(loc.file_record_number, 42);
        assert_eq!(loc.name_length, 5);
    }

    #[test]
    fn find_index_entry_missing_returns_none() {
        let entry = make_entry(42, 5, "hello");
        let rec = index_root_record(&[entry]);
        assert!(find_index_entry(&rec, "world").unwrap().is_none());
    }

    #[test]
    fn find_index_entry_multiple_entries_finds_correct_one() {
        let e1 = make_entry(10, 5, "alpha");
        let e2 = make_entry(20, 5, "beta");
        let e3 = make_entry(30, 5, "gamma");
        let rec = index_root_record(&[e1, e2, e3]);
        assert_eq!(
            find_index_entry(&rec, "alpha")
                .unwrap()
                .unwrap()
                .file_record_number,
            10
        );
        assert_eq!(
            find_index_entry(&rec, "beta")
                .unwrap()
                .unwrap()
                .file_record_number,
            20
        );
        assert_eq!(
            find_index_entry(&rec, "gamma")
                .unwrap()
                .unwrap()
                .file_record_number,
            30
        );
    }

    #[test]
    fn find_index_entry_is_case_sensitive() {
        let entry = make_entry(42, 5, "Hello");
        let rec = index_root_record(&[entry]);
        // find_index_entry uses exact UTF-16 code-unit match, so "HELLO" won't match "Hello"
        assert!(find_index_entry(&rec, "HELLO").unwrap().is_none());
        assert!(find_index_entry(&rec, "Hello").unwrap().is_some());
    }

    // --- index_root_has_real_entries ---

    #[test]
    fn index_root_has_real_entries_empty_is_false() {
        let rec = index_root_record(&[]);
        assert!(!index_root_has_real_entries(&rec).unwrap());
    }

    #[test]
    fn index_root_has_real_entries_with_entry_is_true() {
        let entry = make_entry(5, 5, "file");
        let rec = index_root_record(&[entry]);
        assert!(index_root_has_real_entries(&rec).unwrap());
    }

    // --- index_root_flags ---

    #[test]
    fn index_root_flags_returns_zero_for_small_dir() {
        let rec = index_root_record(&[]);
        let flags = index_root_flags(&rec).unwrap();
        assert_eq!(flags & IH_FLAG_HAS_SUBNODES, 0);
    }

    // --- compare_names (case-insensitive, no upcase table) ---

    #[test]
    fn compare_names_equal_ascii() {
        assert_eq!(
            compare_names(&utf16("foo"), &utf16("foo"), None),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn compare_names_case_insensitive_ascii() {
        assert_eq!(
            compare_names(&utf16("FOO"), &utf16("foo"), None),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_names(&utf16("foo"), &utf16("FOO"), None),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_names(&utf16("Hello"), &utf16("HELLO"), None),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn compare_names_less_than() {
        assert_eq!(
            compare_names(&utf16("abc"), &utf16("abd"), None),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_names(&utf16("a"), &utf16("b"), None),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn compare_names_greater_than() {
        assert_eq!(
            compare_names(&utf16("b"), &utf16("a"), None),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_names(&utf16("abd"), &utf16("abc"), None),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_names_prefix_ordering() {
        assert_eq!(
            compare_names(&utf16("ab"), &utf16("abc"), None),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_names(&utf16("abc"), &utf16("ab"), None),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_names_empty_slices() {
        assert_eq!(compare_names(&[], &[], None), std::cmp::Ordering::Equal);
        assert_eq!(
            compare_names(&[], &utf16("a"), None),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_names(&utf16("a"), &[], None),
            std::cmp::Ordering::Greater
        );
    }

    // --- compare_names_ordinal (case-sensitive) ---

    #[test]
    fn compare_names_ordinal_equal() {
        assert_eq!(
            compare_names_ordinal(&utf16("foo"), &utf16("foo")),
            std::cmp::Ordering::Equal
        );
        assert_eq!(compare_names_ordinal(&[], &[]), std::cmp::Ordering::Equal);
    }

    #[test]
    fn compare_names_ordinal_case_sensitive() {
        // 'A' = 0x0041, 'a' = 0x0061; uppercase sorts before lowercase
        assert_eq!(
            compare_names_ordinal(&utf16("FOO"), &utf16("foo")),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_names_ordinal(&utf16("foo"), &utf16("FOO")),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_names_ordinal(&utf16("Abc"), &utf16("abc")),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn compare_names_ordinal_length_tiebreak() {
        assert_eq!(
            compare_names_ordinal(&utf16("ab"), &utf16("abc")),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_names_ordinal(&utf16("abc"), &utf16("ab")),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_names_ordinal_empty_vs_nonempty() {
        assert_eq!(
            compare_names_ordinal(&[], &utf16("x")),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_names_ordinal(&utf16("x"), &[]),
            std::cmp::Ordering::Greater
        );
    }

    // --- build_file_name_index_entry ---

    #[test]
    fn build_entry_length_is_multiple_of_8() {
        for name in &["a", "ab", "abc", "abcdefgh", "hello world"] {
            let e = build_file_name_index_entry(1, 5, name, 0, false).unwrap();
            assert_eq!(e.len() % 8, 0, "name={name}");
        }
    }

    #[test]
    fn build_entry_file_reference_field() {
        let fref: u64 = 0x0001_0000_0000_0042;
        let e = build_file_name_index_entry(fref, 5, "x", 0, false).unwrap();
        let got = u64::from_le_bytes(
            e[IE_FILE_REFERENCE..IE_FILE_REFERENCE + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(got, fref);
    }

    #[test]
    fn build_entry_entry_length_field_matches_vec_len() {
        let e = build_file_name_index_entry(1, 5, "hi", 0, false).unwrap();
        let entry_len = u16::from_le_bytes([e[IE_LENGTH], e[IE_LENGTH + 1]]) as usize;
        assert_eq!(entry_len, e.len());
    }

    #[test]
    fn build_entry_key_length_field() {
        // key_len = 0x42 (fixed fields) + name.len() * 2
        let e = build_file_name_index_entry(1, 5, "abc", 0, false).unwrap();
        let key_len = u16::from_le_bytes([e[IE_KEY_LENGTH], e[IE_KEY_LENGTH + 1]]) as usize;
        assert_eq!(key_len, 0x42 + 3 * 2);
    }

    #[test]
    fn build_entry_name_embedded_correctly() {
        let e = build_file_name_index_entry(1, 5, "Hi", 0, false).unwrap();
        let name_len_byte = e[IE_KEY_START + FN_NAME_LENGTH_OFFSET] as usize;
        assert_eq!(name_len_byte, 2);
        let name_start = IE_KEY_START + FN_NAME_OFFSET;
        let name_u16: Vec<u16> = e[name_start..name_start + 4]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(name_u16, utf16("Hi"));
    }

    #[test]
    fn build_entry_file_attributes_for_file() {
        let e = build_file_name_index_entry(1, 5, "f", 0, false).unwrap();
        let fa = u32::from_le_bytes(e[IE_KEY_START + 56..IE_KEY_START + 60].try_into().unwrap());
        assert_eq!(fa, 0x20); // ARCHIVE only
    }

    #[test]
    fn build_entry_file_attributes_for_dir() {
        let e = build_file_name_index_entry(1, 5, "d", 0, true).unwrap();
        let fa = u32::from_le_bytes(e[IE_KEY_START + 56..IE_KEY_START + 60].try_into().unwrap());
        assert_eq!(fa, 0x10000000 | 0x20);
    }

    #[test]
    fn build_entry_empty_name_fails() {
        assert!(build_file_name_index_entry(1, 5, "", 0, false).is_err());
    }

    #[test]
    fn build_entry_too_long_name_fails() {
        let name: String = "A".repeat(256);
        assert!(build_file_name_index_entry(1, 5, &name, 0, false).is_err());
    }

    #[test]
    fn build_entry_exactly_255_chars_succeeds() {
        let name: String = "A".repeat(255);
        assert!(build_file_name_index_entry(1, 5, &name, 0, false).is_ok());
    }

    // --- rename_index_entry_same_length ---

    fn make_entry_buf(name: &str) -> (Vec<u8>, IndexEntryLocation) {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        let key_len = 0x42 + utf16.len() * 2;
        let entry_len = (IE_KEY_START + key_len + 7) & !7;
        let mut buf = vec![0u8; entry_len];
        buf[IE_LENGTH..IE_LENGTH + 2].copy_from_slice(&(entry_len as u16).to_le_bytes());
        buf[IE_KEY_LENGTH..IE_KEY_LENGTH + 2].copy_from_slice(&(key_len as u16).to_le_bytes());
        buf[IE_KEY_START + FN_NAME_LENGTH_OFFSET] = utf16.len() as u8;
        let name_start = IE_KEY_START + FN_NAME_OFFSET;
        for (i, &c) in utf16.iter().enumerate() {
            buf[name_start + i * 2..name_start + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let loc = IndexEntryLocation {
            record_offset: 0,
            length: entry_len,
            key_length: key_len,
            file_record_number: 0,
            name_length: utf16.len() as u8,
        };
        (buf, loc)
    }

    #[test]
    fn rename_index_entry_same_length_updates_name() {
        let (mut buf, loc) = make_entry_buf("foo");
        rename_index_entry_same_length(&mut buf, &loc, "bar").unwrap();
        let name_start = IE_KEY_START + FN_NAME_OFFSET;
        let got: Vec<u16> = buf[name_start..name_start + 6]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(got, utf16("bar"));
    }

    #[test]
    fn rename_index_entry_length_mismatch_fails() {
        let (mut buf, loc) = make_entry_buf("foo");
        assert!(rename_index_entry_same_length(&mut buf, &loc, "longer_name").is_err());
        assert!(rename_index_entry_same_length(&mut buf, &loc, "ab").is_err());
    }

    // --- insert_entry_into_index_root -------------------------------------

    #[test]
    fn insert_entry_into_index_root_entry_is_findable_afterwards() {
        let mut rec = index_root_record(&[]);
        let entry = make_entry(42, 5, "hello");
        insert_entry_into_index_root(&mut rec, &entry, "hello").unwrap();
        let loc = find_index_entry(&rec, "hello").unwrap().unwrap();
        assert_eq!(loc.file_record_number, 42);
    }

    #[test]
    fn insert_entry_into_index_root_multiple_entries_sorted() {
        let mut rec = index_root_record(&[]);
        // Insert in reverse alphabetical order — should land sorted.
        let e_zoo = make_entry(3, 5, "zoo");
        let e_bar = make_entry(2, 5, "bar");
        let e_apple = make_entry(1, 5, "apple");
        insert_entry_into_index_root(&mut rec, &e_zoo, "zoo").unwrap();
        insert_entry_into_index_root(&mut rec, &e_bar, "bar").unwrap();
        insert_entry_into_index_root(&mut rec, &e_apple, "apple").unwrap();
        // All three must be findable.
        assert_eq!(
            find_index_entry(&rec, "zoo")
                .unwrap()
                .unwrap()
                .file_record_number,
            3
        );
        assert_eq!(
            find_index_entry(&rec, "bar")
                .unwrap()
                .unwrap()
                .file_record_number,
            2
        );
        assert_eq!(
            find_index_entry(&rec, "apple")
                .unwrap()
                .unwrap()
                .file_record_number,
            1
        );
    }

    #[test]
    fn insert_entry_into_index_root_bumps_total_size() {
        let mut rec = index_root_record(&[]);
        // Grab INDEX_HEADER total_size before insert.
        let ir_before =
            attr_io::find_attribute(&rec, AttrType::IndexRoot, Some(stream::I30)).unwrap();
        let val_off_before = ir_before.resident_value_offset.unwrap() as usize;
        let ih_start_before = ir_before.attr_offset + val_off_before + IR_INDEX_HEADER_OFFSET;
        let total_before = u32::from_le_bytes([
            rec[ih_start_before + IH_TOTAL_SIZE_OF_ENTRIES],
            rec[ih_start_before + IH_TOTAL_SIZE_OF_ENTRIES + 1],
            rec[ih_start_before + IH_TOTAL_SIZE_OF_ENTRIES + 2],
            rec[ih_start_before + IH_TOTAL_SIZE_OF_ENTRIES + 3],
        ]);

        let entry = make_entry(1, 5, "x");
        let entry_len = entry.len() as u32;
        insert_entry_into_index_root(&mut rec, &entry, "x").unwrap();

        let ir_after =
            attr_io::find_attribute(&rec, AttrType::IndexRoot, Some(stream::I30)).unwrap();
        let val_off_after = ir_after.resident_value_offset.unwrap() as usize;
        let ih_start_after = ir_after.attr_offset + val_off_after + IR_INDEX_HEADER_OFFSET;
        let total_after = u32::from_le_bytes([
            rec[ih_start_after + IH_TOTAL_SIZE_OF_ENTRIES],
            rec[ih_start_after + IH_TOTAL_SIZE_OF_ENTRIES + 1],
            rec[ih_start_after + IH_TOTAL_SIZE_OF_ENTRIES + 2],
            rec[ih_start_after + IH_TOTAL_SIZE_OF_ENTRIES + 3],
        ]);
        assert_eq!(total_after, total_before + entry_len);
    }

    // --- remove_index_entry -----------------------------------------------

    #[test]
    fn remove_index_entry_makes_entry_unfindable() {
        let entry = make_entry(10, 5, "target");
        let mut rec = index_root_record(&[entry]);
        let loc = find_index_entry(&rec, "target").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();
        assert!(find_index_entry(&rec, "target").unwrap().is_none());
    }

    #[test]
    fn remove_index_entry_leaves_other_entries_intact() {
        let e1 = make_entry(1, 5, "alpha");
        let e2 = make_entry(2, 5, "beta");
        let e3 = make_entry(3, 5, "gamma");
        let mut rec = index_root_record(&[e1, e2, e3]);
        let loc = find_index_entry(&rec, "beta").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();
        assert!(find_index_entry(&rec, "beta").unwrap().is_none());
        assert!(find_index_entry(&rec, "alpha").unwrap().is_some());
        assert!(find_index_entry(&rec, "gamma").unwrap().is_some());
    }

    #[test]
    fn insert_then_remove_roundtrip_leaves_empty_dir() {
        let mut rec = index_root_record(&[]);
        let entry = make_entry(5, 5, "file");
        insert_entry_into_index_root(&mut rec, &entry, "file").unwrap();
        assert!(index_root_has_real_entries(&rec).unwrap());
        let loc = find_index_entry(&rec, "file").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();
        assert!(!index_root_has_real_entries(&rec).unwrap());
    }

    // --- rename_filename_attribute_same_length ----------------------------

    #[test]
    fn rename_filename_attribute_same_length_updates_name() {
        // Build a record with a $FILE_NAME attribute for "foo".
        use crate::attr_io::attr_off;
        let fn_name_utf16: Vec<u16> = "foo".encode_utf16().collect();
        let fn_name_bytes: Vec<u8> = fn_name_utf16.iter().flat_map(|c| c.to_le_bytes()).collect();
        // $FILE_NAME value: 66 fixed bytes + name bytes (FN_NAME_OFFSET=0x42, FN_NAME_LENGTH_OFFSET=0x40)
        let mut fn_value = vec![0u8; 0x42 + fn_name_bytes.len()];
        fn_value[FN_NAME_LENGTH_OFFSET] = fn_name_utf16.len() as u8;
        fn_value[FN_NAME_OFFSET..FN_NAME_OFFSET + fn_name_bytes.len()]
            .copy_from_slice(&fn_name_bytes);

        // Build a minimal MFT record with this $FILE_NAME attribute.
        const ATTRS_OFF: usize = 0x38;
        const REC_SIZE: usize = 4096;
        let header_size = 24usize;
        let val_off = header_size as u16;
        let attr_len = ((header_size + fn_value.len()) + 7) & !7;
        let end_pos = ATTRS_OFF + attr_len;
        let bytes_used = end_pos + 4;

        let mut rec = vec![0u8; REC_SIZE];
        rec[0x14..0x16].copy_from_slice(&(ATTRS_OFF as u16).to_le_bytes());
        rec[0x18..0x1C].copy_from_slice(&(bytes_used as u32).to_le_bytes());
        rec[0x1C..0x20].copy_from_slice(&(REC_SIZE as u32).to_le_bytes());
        let a = ATTRS_OFF;
        rec[a..a + 4].copy_from_slice(&(AttrType::FileName as u32).to_le_bytes());
        rec[a + attr_off::LENGTH..a + attr_off::LENGTH + 4]
            .copy_from_slice(&(attr_len as u32).to_le_bytes());
        rec[a + attr_off::RESIDENT_VALUE_LENGTH..a + attr_off::RESIDENT_VALUE_LENGTH + 4]
            .copy_from_slice(&(fn_value.len() as u32).to_le_bytes());
        rec[a + attr_off::RESIDENT_VALUE_OFFSET..a + attr_off::RESIDENT_VALUE_OFFSET + 2]
            .copy_from_slice(&val_off.to_le_bytes());
        rec[a + header_size..a + header_size + fn_value.len()].copy_from_slice(&fn_value);
        rec[end_pos..end_pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        rename_filename_attribute_same_length(&mut rec, "foo", "bar").unwrap();

        // Verify the name was updated.
        let loc = attr_io::find_attribute(&rec, AttrType::FileName, None).unwrap();
        let val_start = loc.attr_offset + loc.resident_value_offset.unwrap() as usize;
        let name_start = val_start + FN_NAME_OFFSET;
        let new_name: Vec<u16> = rec[name_start..name_start + 6]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(new_name, utf16("bar"));
    }

    #[test]
    fn rename_filename_attribute_length_mismatch_fails() {
        // No $FILE_NAME attribute at all — should error.
        let rec_empty = index_root_record(&[]);
        assert!(
            rename_filename_attribute_same_length(&mut rec_empty.clone(), "foo", "longer").is_err()
        );
    }

    // --- insert_entry_into_index_root ----------------------------------------

    #[test]
    fn insert_entry_adds_findable_entry() {
        let mut rec = index_root_record(&[]);
        assert!(find_index_entry(&rec, "newfile").unwrap().is_none());

        let entry = make_entry(99, 5, "newfile");
        insert_entry_into_index_root(&mut rec, &entry, "newfile").unwrap();

        let loc = find_index_entry(&rec, "newfile").unwrap().unwrap();
        assert_eq!(loc.file_record_number, 99);
    }

    #[test]
    fn insert_entry_preserves_existing_entries() {
        let e1 = make_entry(10, 5, "alpha");
        let mut rec = index_root_record(&[e1]);

        let e2 = make_entry(20, 5, "gamma");
        insert_entry_into_index_root(&mut rec, &e2, "gamma").unwrap();

        assert_eq!(
            find_index_entry(&rec, "alpha")
                .unwrap()
                .unwrap()
                .file_record_number,
            10
        );
        assert_eq!(
            find_index_entry(&rec, "gamma")
                .unwrap()
                .unwrap()
                .file_record_number,
            20
        );
    }

    #[test]
    fn insert_entry_maintains_sorted_order() {
        // Insert in reverse order; find both after.
        let mut rec = index_root_record(&[]);
        let ez = make_entry(3, 5, "z_file");
        let ea = make_entry(1, 5, "a_file");
        insert_entry_into_index_root(&mut rec, &ez, "z_file").unwrap();
        insert_entry_into_index_root(&mut rec, &ea, "a_file").unwrap();
        assert!(find_index_entry(&rec, "a_file").unwrap().is_some());
        assert!(find_index_entry(&rec, "z_file").unwrap().is_some());
    }

    #[test]
    fn insert_multiple_entries_all_findable() {
        let mut rec = index_root_record(&[]);
        for (i, name) in ["bravo", "charlie", "alpha", "delta"].iter().enumerate() {
            let entry = make_entry(i as u64 + 1, 5, name);
            insert_entry_into_index_root(&mut rec, &entry, name).unwrap();
        }
        for name in &["alpha", "bravo", "charlie", "delta"] {
            assert!(
                find_index_entry(&rec, name).unwrap().is_some(),
                "missing: {name}"
            );
        }
    }

    // --- remove_index_entry --------------------------------------------------

    #[test]
    fn remove_entry_makes_it_unfindable() {
        let entry = make_entry(42, 5, "removeme");
        let mut rec = index_root_record(&[entry]);
        assert!(find_index_entry(&rec, "removeme").unwrap().is_some());

        let loc = find_index_entry(&rec, "removeme").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();

        assert!(find_index_entry(&rec, "removeme").unwrap().is_none());
    }

    #[test]
    fn remove_entry_leaves_other_entries_intact() {
        let e1 = make_entry(10, 5, "keep");
        let e2 = make_entry(20, 5, "drop");
        let mut rec = index_root_record(&[e1, e2]);

        let loc = find_index_entry(&rec, "drop").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();

        assert!(find_index_entry(&rec, "keep").unwrap().is_some());
        assert!(find_index_entry(&rec, "drop").unwrap().is_none());
    }

    #[test]
    fn remove_then_insert_roundtrip() {
        let entry = make_entry(5, 5, "file");
        let mut rec = index_root_record(&[entry]);

        let loc = find_index_entry(&rec, "file").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();
        assert!(find_index_entry(&rec, "file").unwrap().is_none());

        let new_entry = make_entry(99, 5, "file");
        insert_entry_into_index_root(&mut rec, &new_entry, "file").unwrap();
        assert_eq!(
            find_index_entry(&rec, "file")
                .unwrap()
                .unwrap()
                .file_record_number,
            99
        );
    }

    #[test]
    fn empty_dir_after_removing_all_entries() {
        let e1 = make_entry(1, 5, "one");
        let e2 = make_entry(2, 5, "two");
        let mut rec = index_root_record(&[e1, e2]);

        let loc1 = find_index_entry(&rec, "one").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc1, BlockKind::IndexRoot).unwrap();
        let loc2 = find_index_entry(&rec, "two").unwrap().unwrap();
        remove_index_entry(&mut rec, &loc2, BlockKind::IndexRoot).unwrap();

        assert!(!index_root_has_real_entries(&rec).unwrap());
    }
}
