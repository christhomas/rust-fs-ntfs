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
pub const IH_FLAGS_OFFSET: usize = 0x0C;
/// INDEX_HEADER bit: any of the entries has a subnode pointer (i.e.
/// the index overflows into `$INDEX_ALLOCATION`).
pub const IH_FLAG_HAS_SUBNODES: u8 = 0x01;
/// Offsets inside `INDEX_HEADER`:
const IH_FIRST_ENTRY_OFFSET: usize = 0;
const IH_TOTAL_SIZE_OF_ENTRIES: usize = 4;
/// Size of an INDEX_HEADER. Entries begin at `first_entry_offset` bytes
/// from the header's start, and that offset is measured past this.
const INDEX_HEADER_SIZE: usize = 0x10;
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

/// The UTF-16 name of the index entry at `cursor`, bounded by the entry.
///
/// `name_length` is one unvalidated byte, so the name it describes can
/// be up to 510 bytes long -- and two of the four entry walks in this
/// file sliced `record[name_start .. name_start + name_length * 2]`
/// with no bound at all, having proved only that the entry's own
/// `length` fits in the buffer. `collect_entries` is the walk that does
/// bound every read; this is that bound, written once.
fn entry_name(buf: &[u8], cursor: usize, length: usize) -> Result<Vec<u16>, String> {
    let name_length = usize::from(
        *buf.get(cursor + IE_KEY_START + FN_NAME_LENGTH_OFFSET)
            .ok_or("index entry ends before its name length")?,
    );
    let name_start = cursor + IE_KEY_START + FN_NAME_OFFSET;
    let name_end = name_start
        .checked_add(name_length * 2)
        .ok_or("index entry name length overflows")?;
    if name_end > cursor + length || name_end > buf.len() {
        return Err(format!(
            "index entry at {cursor} says its name is {name_length} characters, which \
             runs past the {length}-byte entry"
        ));
    }
    Ok(buf[name_start..name_end]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect())
}

/// One located index entry inside an `$INDEX_ROOT`.
///
/// `#[non_exhaustive]`: this type is returned, not built, by anyone
/// outside the crate, and it gained a field once already. Sealing it
/// against literal construction means the next field costs a minor
/// version rather than a downstream compile error -- which is what
/// adding `sequence` cost, and the reason the seal arrives with it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct IndexEntryLocation {
    /// Byte offset within the MFT record where the entry starts.
    pub record_offset: usize,
    /// Entry length in bytes.
    pub length: usize,
    /// Length of the $FILE_NAME key in bytes.
    pub key_length: usize,
    /// File record number this entry points to (low 48 bits of file_reference).
    pub file_record_number: u64,
    /// The reference's SEQUENCE NUMBER: the high 16 bits, which used to
    /// be masked off here and discarded.
    ///
    /// It is carried, not yet checked. The record it names has a
    /// sequence of its own at header offset `0x10`, and a mismatch
    /// means the entry is stale -- it refers to a file that no longer
    /// occupies the slot. Deciding what a mismatch DOES (skip the entry
    /// or fail the call) is a behavioural choice on dirty volumes and
    /// belongs to its own change; discarding the value made that choice
    /// impossible to implement at all.
    pub sequence: u16,
    /// Length of the filename in UTF-16 code units.
    pub name_length: u8,
}

/// The exclusive end of an `$INDEX_ROOT`'s resident value.
///
/// THE INDEX ENDS WHERE THE THING HOLDING IT ENDS. `remove_index_entry`
/// and `insert_entry_into_index_root_with_collation` state that rule
/// from the writing side, and the comment on the first of them names
/// the readers as the source of the entries it has to defend against --
/// because the readers walked to `ih_start + total_size` bounded only by
/// the MFT record. An `$INDEX_ROOT` whose `total_size` exceeds its own
/// value then has `readdir` decoding whatever follows the attribute in
/// the same record as index entries, each carrying a 48-bit record
/// number taken from those bytes, which the caller opens.
fn index_root_value_end(record: &[u8], ir: &attr_io::AttrLocation) -> Result<usize, String> {
    let off = ir
        .resident_value_offset
        .ok_or("$INDEX_ROOT has no value_offset")? as usize;
    let len = ir
        .resident_value_length
        .ok_or("$INDEX_ROOT has no value_length")? as usize;
    ir.attr_offset
        .checked_add(off)
        .and_then(|start| start.checked_add(len))
        .filter(|&end| end <= record.len())
        .ok_or_else(|| {
            format!(
                "$INDEX_ROOT value [{}+{off}, +{len}) runs past the {}-byte record",
                ir.attr_offset,
                record.len()
            )
        })
}

/// Walk the `$INDEX_ROOT` for `$FILE_NAME` (i.e. the `$I30` index of a
/// directory), returning the located entry whose filename matches
/// `wanted` under [`compare_names`]. Returns `None` if not present.
///
/// `upcase` picks the collation, and it is the same argument the insert
/// paths take, deliberately: entries go into `$I30` ordered by
/// `COLLATION_FILE_NAME`, so a lookup that compared UTF-16 code units
/// for exact equality disagreed with the index it was reading. That let
/// `Foo.txt` past the collision check in a directory already holding
/// `foo.txt` — writing a second entry with an equal collation key into
/// an index NTFS requires to have unique ones — and made the lookup
/// that detaches a name on unlink miss a name path resolution had just
/// found. Pass `Some(table)`; `None` is the ASCII-only fold, for unit
/// tests that have no volume to load `$UpCase` from.
///
/// The walk stops when it encounters an entry with the
/// `IE_FLAG_LAST` bit. Nested `$INDEX_ALLOCATION` blocks are not
/// searched here — this primitive only works for small directories
/// whose index fits entirely in `$INDEX_ROOT`.
pub fn find_index_entry(
    record: &[u8],
    wanted: &str,
    upcase: Option<&crate::upcase::UpcaseTable>,
) -> Result<Option<IndexEntryLocation>, String> {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
        .ok_or_else(|| "$INDEX_ROOT:$I30 not found".to_string())?;
    if !ir.is_resident {
        return Err("$INDEX_ROOT is non-resident (impossible per spec)".to_string());
    }
    let ir_value_offset = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let ir_data_start = ir.attr_offset + ir_value_offset;
    let value_end = index_root_value_end(record, &ir)?;

    let ih_start = ir_data_start + IR_INDEX_HEADER_OFFSET;
    // THE HEADER MUST BE INSIDE THE VALUE, NOT MERELY INSIDE THE RECORD.
    //
    // `read_u32_le` is bounded by `record.len()`, so on a resident
    // `$INDEX_ROOT` whose value is shorter than the two headers these
    // reads came out of whatever attribute follows. `end` is then
    // clamped to `value_end` and lands below `cursor`, the entry loop
    // never runs, and a directory that has entries is answered
    // `Ok(None)` -- which the four collision checks in `write.rs` read
    // as "that name is free". `index_root_has_real_entries` was given
    // this check by #172; these two walks were not.
    if ih_start.saturating_add(INDEX_HEADER_SIZE) > value_end {
        return Err(format!(
            "$INDEX_ROOT value is too short for an index header: header at {ih_start}, \
             value ends at {value_end}"
        ));
    }
    let first_entry_rel = read_u32_le(record, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or_else(|| "index header too short to read first_entry_offset".to_string())?
        as usize;
    let total_size = read_u32_le(record, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or_else(|| "index header too short to read total_size".to_string())?
        as usize;

    let mut cursor = ih_start + first_entry_rel;
    // Whichever the header says, the entries stop at the value.
    let end = (ih_start + total_size).min(value_end);

    let wanted_utf16: Vec<u16> = wanted.encode_utf16().collect();

    while cursor < end && cursor + IE_KEY_START <= value_end {
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
        if length == 0 || cursor + length > value_end {
            return Err(format!("malformed index entry at {cursor}"));
        }
        if key_length >= FN_NAME_OFFSET {
            // Bounded by the entry rather than by the buffer: the
            // length byte itself sits 0x40 into the key, which the
            // entry's own `length` may not reach, and a name bounded
            // only by the buffer reads the next entry's bytes as this
            // one's name.
            let name_u16 = entry_name(record, cursor, length).unwrap_or_default();
            let name_length = name_u16.len();
            if compare_names(&name_u16, &wanted_utf16, upcase) == std::cmp::Ordering::Equal {
                {
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
                        sequence: (file_ref >> 48) as u16,
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
    let value_end = index_root_value_end(record, &ir)?;
    let ih_start = ir_data_start + IR_INDEX_HEADER_OFFSET;
    let first_entry_rel = read_u32_le(record, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or_else(|| "index header too short to read first_entry_offset".to_string())?
        as usize;
    let total_size = read_u32_le(record, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or_else(|| "index header too short to read total_size".to_string())?
        as usize;
    let first_entry = ih_start + first_entry_rel;
    let end = (ih_start + total_size).min(value_end);
    // AN UNREADABLE INDEX IS NOT AN EMPTY ONE.
    //
    // This used to answer Ok(false) here -- "no real entries" -- and
    // `rmdir` deletes a directory on that answer, orphaning every
    // child's MFT record. A header that does not parse is the ordinary
    // result of an interrupted write to a directory record, and it is
    // the one input where reporting emptiness is destructive: the
    // caller's response to emptiness is to delete.
    if first_entry + IE_KEY_START > value_end || first_entry + 0x10 > end {
        return Err(format!(
            "$INDEX_ROOT header is unreadable: first entry at {first_entry}, entries end \
             at {end}, value ends at {value_end}"
        ));
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
fn ih_start(ir: &attr_io::AttrLocation) -> Option<usize> {
    let val_off = ir.resident_value_offset? as usize;
    ir.attr_offset
        .checked_add(val_off)?
        .checked_add(IR_INDEX_HEADER_OFFSET)
}

pub fn index_root_flags(record: &[u8]) -> Option<u8> {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))?;
    if !ir.is_resident {
        return None;
    }
    // `.get()`, not an index. `AttrIter` guarantees the attribute is
    // inside the record and the value inside the attribute, but not
    // that the value is long enough to hold an index header -- so an
    // $INDEX_ROOT as the last attribute with a short value put this
    // read past the end. It is the first call in every mutating path,
    // and the root directory's own record is enough to reach it.
    //
    // `record.len()` is the wrong bound even so: with another
    // attribute after this one the read stays inside the record and
    // returns that attribute's byte as the index's flags. The value is
    // the bound, and a value too short for the header has no flags to
    // report. Every caller must treat that `None` as an error rather
    // than as "no subnodes" -- reporting no subnodes here is the same
    // silent empty listing the header-bounds fix exists to remove.
    let start = ih_start(&ir)?;
    let value_end = index_root_value_end(record, &ir).ok()?;
    if start.saturating_add(INDEX_HEADER_SIZE) > value_end {
        return None;
    }
    record.get(start + IH_FLAGS_OFFSET).copied()
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
    upcase: Option<&crate::upcase::UpcaseTable>,
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
    scan_entries_for_name(block, &mut cursor, end, wanted, upcase)
}

/// Shared scanner: sweep entries starting at `cursor`, stopping at
/// `end` or IE_FLAG_LAST, returning the matching entry's location.
fn scan_entries_for_name(
    buf: &[u8],
    cursor: &mut usize,
    end: usize,
    wanted: &str,
    upcase: Option<&crate::upcase::UpcaseTable>,
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
            // Bounded by the entry rather than by the buffer: the
            // length byte itself sits 0x40 into the key, which the
            // entry's own `length` may not reach, and a name bounded
            // only by the buffer reads the next entry's bytes as this
            // one's name.
            let name_u16 = entry_name(buf, *cursor, length).unwrap_or_default();
            let name_length = name_u16.len();
            if compare_names(&name_u16, &wanted_utf16, upcase) == std::cmp::Ordering::Equal {
                {
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
                        sequence: (file_ref >> 48) as u16,
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
fn collect_entries(
    buf: &[u8],
    ih_start: usize,
    // The exclusive end of the thing holding this index node: an
    // `$INDEX_ROOT`'s resident value, or an INDX block. `total_size`
    // comes off the disk and does not get to exceed it.
    limit: usize,
    out: &mut Vec<DirEntryRaw>,
) -> Result<(), String> {
    // Same rule as `find_index_entry`: `read_u32_le` is bounded by
    // `buf.len()` -- the whole MFT record for an `$INDEX_ROOT` -- while
    // the node ends at `limit`. A value too short for the header read
    // these two fields out of the next attribute, `end` clamped below
    // `cursor`, and the walk appended nothing: `readdir` reported an
    // empty directory for a directory that has entries.
    if ih_start.saturating_add(INDEX_HEADER_SIZE) > limit {
        return Err(format!(
            "index node is too short for an index header: header at {ih_start}, \
             node ends at {limit}"
        ));
    }
    let first_entry_rel = read_u32_le(buf, ih_start + IH_FIRST_ENTRY_OFFSET)
        .ok_or("index node too short to read first_entry_offset")?
        as usize;
    let total_size = read_u32_le(buf, ih_start + IH_TOTAL_SIZE_OF_ENTRIES)
        .ok_or("index node too short to read total_size")? as usize;
    let mut cursor = ih_start + first_entry_rel;
    let end = (ih_start + total_size).min(limit);
    while cursor < end && cursor + IE_KEY_START <= limit {
        let length =
            u16::from_le_bytes([buf[cursor + IE_LENGTH], buf[cursor + IE_LENGTH + 1]]) as usize;
        let key_length =
            u16::from_le_bytes([buf[cursor + IE_KEY_LENGTH], buf[cursor + IE_KEY_LENGTH + 1]])
                as usize;
        let flags = u16::from_le_bytes([buf[cursor + IE_FLAGS], buf[cursor + IE_FLAGS + 1]]);
        if flags & IE_FLAG_LAST != 0 {
            break;
        }
        if length == 0 || cursor + length > limit {
            return Err(format!("malformed index entry at {cursor}"));
        }
        // Bound every read to THIS entry [cursor, entry_end), not the whole
        // buffer: a corrupt key_length/name_length must not let us decode
        // bytes from the next entry/trailer into a fabricated DirEntryRaw.
        let entry_end = cursor + length; // already validated <= limit
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
    let value_end = index_root_value_end(record, &ir)?;
    let ih_start = ir.attr_offset + ir_value_offset + IR_INDEX_HEADER_OFFSET;
    collect_entries(record, ih_start, value_end, out)
}

/// Enumerate the `$FILE_NAME` entries in one `$INDEX_ALLOCATION` (INDX) block
/// (already read + USA-fixed).
pub fn collect_indx_block_entries(block: &[u8], out: &mut Vec<DirEntryRaw>) -> Result<(), String> {
    if block.len() < 4 || &block[0..4] != b"INDX" {
        return Err("not an INDX block (fixup missing?)".to_string());
    }
    // An INDX block is its own buffer; the block is what holds the node.
    collect_entries(
        block,
        crate::idx_block::INDX_INDEX_HEADER_OFFSET,
        block.len(),
        out,
    )
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
    // THE INDEX ENDS WHERE THE THING HOLDING IT ENDS.
    //
    // For an $INDEX_ROOT, `buf` is the whole MFT record and the index
    // lives inside one attribute's resident value -- so the entries
    // cannot reach past that value, however large `total_size` says
    // they are. Bounding by `buf.len()` instead let the shift and the
    // zero-fill below range over every other attribute in the record:
    // $FILE_NAME, $DATA, $INDEX_ALLOCATION. `find_index_entry` supplies
    // such an entry because its own walk is bounded the same way.
    let (ih_start, index_limit) = match block_kind {
        BlockKind::IndexRoot => {
            let ir = attr_io::find_attribute(buf, AttrType::IndexRoot, Some(stream::I30))
                .ok_or_else(|| "$INDEX_ROOT:$I30 missing".to_string())?;
            let val_off = ir.resident_value_offset.ok_or("no value_offset")? as usize;
            let val_len = ir.resident_value_length.ok_or("no value_length")? as usize;
            if val_len < IR_INDEX_HEADER_OFFSET {
                return Err(format!(
                    "$INDEX_ROOT:$I30 value is {val_len} bytes, too short to hold an index"
                ));
            }
            (
                ir.attr_offset + val_off + IR_INDEX_HEADER_OFFSET,
                ir.attr_offset + val_off + val_len,
            )
        }
        BlockKind::IndexAllocation => (crate::idx_block::INDX_INDEX_HEADER_OFFSET, buf.len()),
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
    // `total_size` is a raw u32 off the disk and `tail_end` is a bound
    // for two `copy_within` calls below. It was compared only against
    // `entry_end`, another number from the same index, so nothing tied
    // either of them to the buffer.
    if tail_end > index_limit
        || entry_end > tail_end
        || entry.record_offset < ih_start
        || entry.record_offset > entry_end
    {
        return Err(format!(
            "index says its entries end at {tail_end}, past the {index_limit} where the \
             index itself does"
        ));
    }

    // AN ENTRY WITH A CHILD IS NOT ONE TO SHIFT AWAY.
    //
    // In a B-tree index an entry may carry an 8-byte child VCN in its
    // tail, and every key strictly less than it lives down that child.
    // Removing the entry removes the pointer with it, so the whole
    // subtree is orphaned: those files vanish from the tree while
    // their $I30 bitmap bits stay set and their clusters stay
    // allocated. This driver's own reader hides it, because it
    // brute-force scans every allocated INDX block rather than
    // descending -- so it surfaces on Windows, in chkdsk.
    //
    // Refusing is what this crate already does for the other index
    // shapes it cannot maintain; splitting and rebalancing is the work
    // that would make it possible.
    let entry_flags = u16::from_le_bytes([
        buf[entry.record_offset + IE_FLAGS],
        buf[entry.record_offset + IE_FLAGS + 1],
    ]);
    if entry_flags & IE_FLAG_HAS_SUBNODE != 0 {
        return Err(
            "the entry to remove points at a sub-node; removing it would orphan that \
             subtree (index B-tree maintenance is not implemented)"
                .to_string(),
        );
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
    if old_val_len < IR_INDEX_HEADER_OFFSET {
        return Err(format!(
            "$INDEX_ROOT:$I30 value is {old_val_len} bytes, too short to hold an index"
        ));
    }
    let ih_start = ir.attr_offset + val_off + IR_INDEX_HEADER_OFFSET;
    let index_limit = ir.attr_offset + val_off + old_val_len;

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

    // BOTH ARE RAW u32s, and both bound a `copy_within` below.
    // Nothing tied either to the value the index lives in, so a
    // `first_entry_rel` past `total_size` produced a reversed range,
    // and one inside the value but not at an entry boundary spliced
    // the new entry into the middle of an existing one -- and THAT
    // record was written back.
    let entries_end = ih_start
        .checked_add(total_size)
        .filter(|end| *end <= index_limit)
        .ok_or_else(|| {
            format!("the index says its entries end past its own {old_val_len}-byte value")
        })?;
    if first_entry_rel > total_size {
        return Err(format!(
            "the index says its first entry is {first_entry_rel} bytes in, past the \
             {total_size} bytes of entries it has"
        ));
    }

    refuse_if_interior(record[ih_start + IH_FLAGS_OFFSET], "$INDEX_ROOT:$I30")?;

    // Find sorted insertion position. Walk existing entries until we
    // find one whose name is >= new_name or hit the LAST sentinel.
    let mut cursor = ih_start + first_entry_rel;
    let end = entries_end;
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
        let existing_utf16 = entry_name(record, cursor, length)?;
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

    // ALL THREE OF THOSE ARE RAW u32s OFF THE DISK, AND ALL THREE BOUND
    // THE `copy_within` BELOW.
    //
    // The only check this used to have compared two of them to each
    // other — `total_size + new_len > allocated_size` — and tied neither
    // to the block they describe. Its `$INDEX_ROOT` twin was hardened
    // against exactly this and says why in a comment; the same three
    // failures were reachable here:
    //
    //   * `first_entry_offset` past `total_size` left the sorted-position
    //     walk unrun and `insertion_point` above `end`, so the shift was
    //     asked for a range running backwards -- "slice index starts at
    //     88 but ends at 56".
    //   * a `total_size` of 0x100000 under an `allocated_size` of
    //     0xFFFFFFFF passed the room check and made the shift read from
    //     outside a 4096-byte block.
    //   * a `first_entry_offset` inside the entries but not on an entry
    //     boundary started the walk mid-entry and laid the new entry
    //     across the one already there -- and that one SUCCEEDS, so the
    //     block goes back to the disk and Windows finds it later.
    //
    // The alignment check catches the third only where the offset is
    // misaligned, which is most of the time but not all of it: an
    // 8-aligned offset that happens to fall inside an entry is not
    // distinguishable from a legitimate one without another source of
    // truth. The `$INDEX_ROOT` path has the same residual gap.
    if ih_start
        .checked_add(allocated_size)
        .is_none_or(|end| end > block.len())
    {
        return Err(format!(
            "the index says its entries are allocated {allocated_size} bytes, past the \
             {} the block has",
            block.len() - ih_start
        ));
    }
    // THIS ONE CANNOT CURRENTLY CHANGE THE OUTCOME, and is kept
    // deliberately. The room check below already refuses whenever
    // `total_size >= allocated_size`, because `new_len` is never zero —
    // removing this leaves every test green. It stays because the bound
    // on `end` a few lines down, which is what keeps `copy_within` inside
    // the block, would otherwise rest on a capacity test rather than on a
    // bounds test, and a capacity test is the kind of thing a later edit
    // reorders or relaxes without noticing what else depended on it.
    if total_size > allocated_size {
        return Err(format!(
            "the index says it holds {total_size} bytes of entries in {allocated_size} \
             bytes of space"
        ));
    }
    if first_entry_rel > total_size {
        return Err(format!(
            "the index says its first entry is {first_entry_rel} bytes in, past the \
             {total_size} bytes of entries it has"
        ));
    }
    if first_entry_rel < INDEX_HEADER_SIZE || !first_entry_rel.is_multiple_of(8) {
        return Err(format!(
            "the index says its first entry is {first_entry_rel} bytes in, which is not \
             an entry boundary"
        ));
    }

    // THE CHECK THIS FUNCTION WAS THE ONLY ONE WITHOUT. The $I30 bitmap
    // marks interior and leaf blocks alike, so the block that reached
    // here was chosen for having room, not for being a leaf.
    let flags = *block
        .get(ih_start + IH_FLAGS_OFFSET)
        .ok_or_else(|| "INDX block too short to read the index header flags".to_string())?;
    refuse_if_interior(flags, "this INDX block")?;

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
        let existing_utf16 = entry_name(block, cursor, length)?;
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

/// Refuse a splice into an interior node.
///
/// # THE ENTRY SHAPES DIFFER, SO THE NODE KIND IS NOT COSMETIC
///
/// Every entry in an interior node ends with the 8-byte VCN of the child
/// holding every key below it. [`build_file_name_index_entry`] emits a
/// leaf entry, which has no such tail. Splice one in and `ntfs.sys` reads
/// the child VCN at `entry_end - 8`, which for the new entry lands inside
/// its own UTF-16 filename, and descends to whatever VCN those bytes
/// spell. The directory stops being readable on Windows and `chkdsk`
/// reports index errors — from a plain file creation, with nothing
/// reported at the time.
///
/// # WHY IT IS ONE FUNCTION
///
/// Three functions here handle index entries and there are two block
/// kinds, so six combinations. The guard existed in four of them: both
/// halves of `remove_index_entry` and the `$INDEX_ROOT` insert. The
/// missing one was the INDX insert, and it was missing because each of
/// the other guards had been written separately. Written once, the sixth
/// cannot be forgotten.
///
/// `flags` is the INDEX_HEADER flags byte at [`IH_FLAGS_OFFSET`];
/// `what` names the structure for the error message.
pub fn refuse_if_interior(flags: u8, what: &str) -> Result<(), String> {
    if flags & IH_FLAG_HAS_SUBNODES != 0 {
        return Err(format!(
            "{what} has sub-nodes; inserting into an interior node needs \
             index B-tree maintenance, which is not implemented"
        ));
    }
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
/// or the insert paths** — both now use `compare_names` (case-
/// insensitive) unconditionally, which is what an index collated by
/// `COLLATION_FILE_NAME` requires. Plumbing the per-directory flag
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
        let value_length = loc.resident_value_length.unwrap_or(0) as usize;
        // The name length sits 0x40 into the $FILE_NAME value and the
        // name itself at 0x42, so a value shorter than that describes
        // no name to compare.
        if value_length < FN_NAME_OFFSET {
            continue;
        }
        let data_start = loc.attr_offset + value_offset;
        let name_length_byte = record[data_start + FN_NAME_LENGTH_OFFSET] as usize;
        if FN_NAME_OFFSET + name_length_byte * 2 > value_length {
            continue;
        }
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

    /// `name_length` is one unvalidated byte, so it describes up to
    /// 510 bytes of name, and two of the four entry walks in this file
    /// sliced with it having proved only that the entry's own `length`
    /// fits in the buffer. Reached from create, mkdir and rename.
    #[test]
    fn an_index_entry_name_that_runs_past_the_entry_is_refused() {
        let entry = make_entry(42, 5, "hello");
        let length = entry.len();
        let mut buf = entry.clone();

        // The control: the real name comes back.
        let name = entry_name(&buf, 0, length).expect("a well-formed entry");
        assert_eq!(String::from_utf16_lossy(&name), "hello");

        // A name longer than the entry that holds it.
        buf[IE_KEY_START + FN_NAME_LENGTH_OFFSET] = 0xFF;
        assert!(
            entry_name(&buf, 0, length).is_err(),
            "a 255-character name was read out of a {length}-byte entry"
        );

        // And an entry too short to reach its own name-length byte.
        assert!(entry_name(&entry, 0, IE_KEY_START).is_err());
        // The last entry in a block: nothing past it to read from.
        let at = buf.len() - 8;
        assert!(entry_name(&buf, at, 8).is_err());
    }

    #[test]
    fn find_index_entry_empty_dir_returns_none() {
        let rec = index_root_record(&[]);
        let result = find_index_entry(&rec, "foo", None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_index_entry_finds_present_entry() {
        let entry = make_entry(42, 5, "hello");
        let rec = index_root_record(&[entry]);
        let loc = find_index_entry(&rec, "hello", None).unwrap().unwrap();
        assert_eq!(loc.file_record_number, 42);
        assert_eq!(loc.name_length, 5);
    }

    #[test]
    fn find_index_entry_missing_returns_none() {
        let entry = make_entry(42, 5, "hello");
        let rec = index_root_record(&[entry]);
        assert!(find_index_entry(&rec, "world", None).unwrap().is_none());
    }

    #[test]
    fn find_index_entry_multiple_entries_finds_correct_one() {
        let e1 = make_entry(10, 5, "alpha");
        let e2 = make_entry(20, 5, "beta");
        let e3 = make_entry(30, 5, "gamma");
        let rec = index_root_record(&[e1, e2, e3]);
        assert_eq!(
            find_index_entry(&rec, "alpha", None)
                .unwrap()
                .unwrap()
                .file_record_number,
            10
        );
        assert_eq!(
            find_index_entry(&rec, "beta", None)
                .unwrap()
                .unwrap()
                .file_record_number,
            20
        );
        assert_eq!(
            find_index_entry(&rec, "gamma", None)
                .unwrap()
                .unwrap()
                .file_record_number,
            30
        );
    }

    #[test]
    fn find_index_entry_collates_the_way_the_index_is_ordered() {
        // This used to assert the opposite -- that "HELLO" would NOT
        // find "Hello" -- and pinned the disagreement that let a
        // duplicate collation key into $I30. The index is ordered by
        // COLLATION_FILE_NAME, so the lookup folds case the same way.
        // `None` is the ASCII fold, which is all these five letters
        // need; the volume's own $UpCase table is what production
        // callers pass.
        let entry = make_entry(42, 5, "Hello");
        let rec = index_root_record(&[entry]);
        assert!(find_index_entry(&rec, "HELLO", None).unwrap().is_some());
        assert!(find_index_entry(&rec, "hello", None).unwrap().is_some());
        assert!(find_index_entry(&rec, "Hello", None).unwrap().is_some());
        // Still not a match for a different name, or for a prefix.
        assert!(find_index_entry(&rec, "Hell", None).unwrap().is_none());
        assert!(find_index_entry(&rec, "Hello2", None).unwrap().is_none());
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

    // --- the index header must be inside the attribute value ---

    /// Shorten the resident `$INDEX_ROOT`'s `value_length` field to
    /// `new_len`, leaving every other byte of the record where it was.
    /// The index header and the entries stay physically present and
    /// unchanged; only the attribute's own statement of how far its
    /// value reaches moves. Returns the header offset and the new
    /// value end so a test can say what the old reads picked up.
    fn shorten_index_root_value(rec: &mut [u8], new_len: u32) -> (usize, usize) {
        let ir = attr_io::find_attribute(rec, AttrType::IndexRoot, Some(stream::I30)).unwrap();
        let val_start = ir.attr_offset + ir.resident_value_offset.unwrap() as usize;
        let at = ir.attr_offset + attr_off::RESIDENT_VALUE_LENGTH;
        rec[at..at + 4].copy_from_slice(&new_len.to_le_bytes());
        (
            val_start + IR_INDEX_HEADER_OFFSET,
            val_start + new_len as usize,
        )
    }

    /// A resident `$INDEX_ROOT` whose value is shorter than the root
    /// header plus the index header. `read_u32_le` is bounded by the
    /// MFT record, so `first_entry_offset` and `total_size` were read
    /// from past the value -- here, from the very bytes the shortened
    /// value abandoned. `end` was then clamped to the value and landed
    /// below `cursor`, so the entry loop never ran and both walks
    /// answered "nothing here" for a record that plainly holds an
    /// entry. The four `write.rs` collision checks read that `Ok(None)`
    /// as "the name is free".
    #[test]
    fn a_short_index_root_value_is_not_an_empty_directory() {
        // The control: the record is well formed and all three
        // readers agree there is one entry called `target`.
        let good = index_root_record(&[make_entry(10, 5, "target")]);
        assert!(
            find_index_entry(&good, "target", None).unwrap().is_some(),
            "the control record must hold `target`"
        );
        let mut listed = Vec::new();
        collect_index_root_entries(&good, &mut listed).unwrap();
        assert_eq!(
            listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["target"]
        );
        assert_eq!(index_root_flags(&good), Some(0));

        // Now say the value is 20 bytes: four bytes of index header,
        // no more. Nothing else about the record changes.
        let mut rec = good.clone();
        let (ih, value_end) = shorten_index_root_value(&mut rec, 20);
        assert_eq!(value_end, ih + 4, "20 bytes leaves 4 past the root header");

        // What the unbounded reads found there: the real header, whole
        // and plausible, sitting outside the value it was read for.
        assert_eq!(
            read_u32_le(&rec, ih + IH_FIRST_ENTRY_OFFSET),
            Some(INDEX_HEADER_SIZE as u32),
            "the bytes past the value still decode as a first_entry_offset"
        );
        assert!(
            read_u32_le(&rec, ih + IH_TOTAL_SIZE_OF_ENTRIES).unwrap() as usize > INDEX_HEADER_SIZE,
            "and as a total_size describing entries"
        );

        let why = find_index_entry(&rec, "target", None)
            .expect_err("a header outside the value is not a lookup miss");
        assert!(
            why.contains("too short for an index header"),
            "find_index_entry answered {why}"
        );

        let mut out = Vec::new();
        let why = collect_index_root_entries(&rec, &mut out)
            .expect_err("a header outside the value is not an empty directory");
        assert!(
            why.contains("too short for an index header"),
            "collect_index_root_entries answered {why}"
        );
        assert!(out.is_empty(), "and it appended nothing");

        // The flags byte is 12 bytes into a header that is not there.
        // `None` is the only honest answer, and `read.rs` turns it into
        // an error rather than into "no subnodes".
        assert_eq!(index_root_flags(&rec), None);
    }

    /// The acceptance half, and it pins the comparison rather than the
    /// direction. The guard's subject is the 16-byte INDEX_HEADER at
    /// `IR_INDEX_HEADER_OFFSET`, so a value of exactly 32 bytes holds
    /// one and must reach the entry walk; 31 must not. An empty
    /// directory's real value is 48 bytes -- root header, index header,
    /// LAST sentinel -- and is well clear of both.
    #[test]
    fn a_value_that_holds_an_index_header_is_still_read() {
        let smallest_legitimate = build_index_root_value(&[]).len();
        assert_eq!(
            smallest_legitimate,
            IR_INDEX_HEADER_OFFSET + INDEX_HEADER_SIZE + 16,
            "an empty directory's $INDEX_ROOT value"
        );
        let empty = index_root_record(&[]);
        assert!(find_index_entry(&empty, "anything", None)
            .unwrap()
            .is_none());
        let mut out = Vec::new();
        collect_index_root_entries(&empty, &mut out).unwrap();
        assert!(out.is_empty());
        assert_eq!(index_root_flags(&empty), Some(0));

        let exact = (IR_INDEX_HEADER_OFFSET + INDEX_HEADER_SIZE) as u32;
        let mut at_the_boundary = index_root_record(&[make_entry(10, 5, "target")]);
        shorten_index_root_value(&mut at_the_boundary, exact);
        assert!(
            find_index_entry(&at_the_boundary, "target", None).is_ok(),
            "a value holding a whole index header is not refused by the header guard"
        );
        assert_eq!(index_root_flags(&at_the_boundary), Some(0));

        let mut one_short = index_root_record(&[make_entry(10, 5, "target")]);
        shorten_index_root_value(&mut one_short, exact - 1);
        assert!(
            find_index_entry(&one_short, "target", None).is_err(),
            "one byte short of a header is refused"
        );
        assert_eq!(index_root_flags(&one_short), None);
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
            sequence: 0,
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
        let loc = find_index_entry(&rec, "hello", None).unwrap().unwrap();
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
            find_index_entry(&rec, "zoo", None)
                .unwrap()
                .unwrap()
                .file_record_number,
            3
        );
        assert_eq!(
            find_index_entry(&rec, "bar", None)
                .unwrap()
                .unwrap()
                .file_record_number,
            2
        );
        assert_eq!(
            find_index_entry(&rec, "apple", None)
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

    /// For an `$INDEX_ROOT` the buffer is the WHOLE MFT record, and
    /// the index lives inside one attribute's resident value. Bounding
    /// `total_size` by the record instead of by that value let the
    /// shift and the zero-fill range over every other attribute --
    /// $FILE_NAME, $DATA, $INDEX_ALLOCATION -- and
    /// `update_mft_record_io` then re-applied the fixup and wrote the
    /// shredded record back, reporting success.
    #[test]
    fn an_index_claiming_more_than_its_own_value_is_refused() {
        let entry = make_entry(10, 5, "target");
        let mut rec = index_root_record(&[entry]);
        let loc = find_index_entry(&rec, "target", None).unwrap().unwrap();

        // The control: it removes.
        let mut ok = rec.clone();
        remove_index_entry(&mut ok, &loc, BlockKind::IndexRoot).expect("a well-formed index");

        // Now say the entries run to the end of the record. The value
        // is a few dozen bytes; the record is 1024.
        let ir = attr_io::find_attribute(&rec, AttrType::IndexRoot, Some(stream::I30)).unwrap();
        let ih =
            ir.attr_offset + ir.resident_value_offset.unwrap() as usize + IR_INDEX_HEADER_OFFSET;
        let at = ih + IH_TOTAL_SIZE_OF_ENTRIES;
        rec[at..at + 4].copy_from_slice(&900u32.to_le_bytes());

        let why = remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap_err();
        assert!(
            why.contains("past the"),
            "an index claiming 900 bytes of entries inside a much smaller value was \
             answered with {why}"
        );
    }

    /// In a B-tree index an entry may carry a child VCN in its tail,
    /// and every key less than it lives down that child. Shifting the
    /// entry away takes the pointer with it and orphans the subtree:
    /// those files vanish from the tree while their bitmap bits stay
    /// set. This crate's own reader hides it -- it brute-force scans
    /// every allocated INDX block rather than descending -- so it
    /// surfaces in chkdsk.
    #[test]
    fn removing_an_entry_that_points_at_a_subtree_is_refused() {
        let entry = make_entry(10, 5, "target");
        let mut rec = index_root_record(&[entry]);
        let loc = find_index_entry(&rec, "target", None).unwrap().unwrap();

        // Give the entry a child pointer.
        let at = loc.record_offset + IE_FLAGS;
        let flags = u16::from_le_bytes([rec[at], rec[at + 1]]) | IE_FLAG_HAS_SUBNODE;
        rec[at..at + 2].copy_from_slice(&flags.to_le_bytes());

        let why = remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap_err();
        assert!(why.contains("orphan"), "{why}");
    }

    /// The mirror of the same gap: `build_file_name_index_entry` makes
    /// a leaf entry with no child VCN, so splicing one into an interior
    /// node leaves its key ordering describing children that are not
    /// there.
    #[test]
    fn inserting_into_an_interior_node_is_refused() {
        let existing = make_entry(10, 5, "aaa");
        let mut rec = index_root_record(&[existing]);
        let ir = attr_io::find_attribute(&rec, AttrType::IndexRoot, Some(stream::I30)).unwrap();
        let ih =
            ir.attr_offset + ir.resident_value_offset.unwrap() as usize + IR_INDEX_HEADER_OFFSET;
        rec[ih + IH_FLAGS_OFFSET] |= IH_FLAG_HAS_SUBNODES;

        let new_entry = make_entry(11, 5, "bbb");
        let why = insert_entry_into_index_root_with_collation(&mut rec, &new_entry, "bbb", None)
            .unwrap_err();
        assert!(why.contains("sub-nodes"), "{why}");
    }

    #[test]
    fn remove_index_entry_makes_entry_unfindable() {
        let entry = make_entry(10, 5, "target");
        let mut rec = index_root_record(&[entry]);
        let loc = find_index_entry(&rec, "target", None).unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();
        assert!(find_index_entry(&rec, "target", None).unwrap().is_none());
    }

    #[test]
    fn remove_index_entry_leaves_other_entries_intact() {
        let e1 = make_entry(1, 5, "alpha");
        let e2 = make_entry(2, 5, "beta");
        let e3 = make_entry(3, 5, "gamma");
        let mut rec = index_root_record(&[e1, e2, e3]);
        let loc = find_index_entry(&rec, "beta", None).unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();
        assert!(find_index_entry(&rec, "beta", None).unwrap().is_none());
        assert!(find_index_entry(&rec, "alpha", None).unwrap().is_some());
        assert!(find_index_entry(&rec, "gamma", None).unwrap().is_some());
    }

    #[test]
    fn insert_then_remove_roundtrip_leaves_empty_dir() {
        let mut rec = index_root_record(&[]);
        let entry = make_entry(5, 5, "file");
        insert_entry_into_index_root(&mut rec, &entry, "file").unwrap();
        assert!(index_root_has_real_entries(&rec).unwrap());
        let loc = find_index_entry(&rec, "file", None).unwrap().unwrap();
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
        assert!(find_index_entry(&rec, "newfile", None).unwrap().is_none());

        let entry = make_entry(99, 5, "newfile");
        insert_entry_into_index_root(&mut rec, &entry, "newfile").unwrap();

        let loc = find_index_entry(&rec, "newfile", None).unwrap().unwrap();
        assert_eq!(loc.file_record_number, 99);
    }

    #[test]
    fn insert_entry_preserves_existing_entries() {
        let e1 = make_entry(10, 5, "alpha");
        let mut rec = index_root_record(&[e1]);

        let e2 = make_entry(20, 5, "gamma");
        insert_entry_into_index_root(&mut rec, &e2, "gamma").unwrap();

        assert_eq!(
            find_index_entry(&rec, "alpha", None)
                .unwrap()
                .unwrap()
                .file_record_number,
            10
        );
        assert_eq!(
            find_index_entry(&rec, "gamma", None)
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
        assert!(find_index_entry(&rec, "a_file", None).unwrap().is_some());
        assert!(find_index_entry(&rec, "z_file", None).unwrap().is_some());
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
                find_index_entry(&rec, name, None).unwrap().is_some(),
                "missing: {name}"
            );
        }
    }

    // --- remove_index_entry --------------------------------------------------

    #[test]
    fn remove_entry_makes_it_unfindable() {
        let entry = make_entry(42, 5, "removeme");
        let mut rec = index_root_record(&[entry]);
        assert!(find_index_entry(&rec, "removeme", None).unwrap().is_some());

        let loc = find_index_entry(&rec, "removeme", None).unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();

        assert!(find_index_entry(&rec, "removeme", None).unwrap().is_none());
    }

    #[test]
    fn remove_entry_leaves_other_entries_intact() {
        let e1 = make_entry(10, 5, "keep");
        let e2 = make_entry(20, 5, "drop");
        let mut rec = index_root_record(&[e1, e2]);

        let loc = find_index_entry(&rec, "drop", None).unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();

        assert!(find_index_entry(&rec, "keep", None).unwrap().is_some());
        assert!(find_index_entry(&rec, "drop", None).unwrap().is_none());
    }

    #[test]
    fn remove_then_insert_roundtrip() {
        let entry = make_entry(5, 5, "file");
        let mut rec = index_root_record(&[entry]);

        let loc = find_index_entry(&rec, "file", None).unwrap().unwrap();
        remove_index_entry(&mut rec, &loc, BlockKind::IndexRoot).unwrap();
        assert!(find_index_entry(&rec, "file", None).unwrap().is_none());

        let new_entry = make_entry(99, 5, "file");
        insert_entry_into_index_root(&mut rec, &new_entry, "file").unwrap();
        assert_eq!(
            find_index_entry(&rec, "file", None)
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

        let loc1 = find_index_entry(&rec, "one", None).unwrap().unwrap();
        remove_index_entry(&mut rec, &loc1, BlockKind::IndexRoot).unwrap();
        let loc2 = find_index_entry(&rec, "two", None).unwrap().unwrap();
        remove_index_entry(&mut rec, &loc2, BlockKind::IndexRoot).unwrap();

        assert!(!index_root_has_real_entries(&rec).unwrap());
    }
}
