//! Build a fresh MFT FILE record from scratch. Used by `write::create_file`
//! and `write::mkdir` to synthesize new MFT entries.
//!
//! Emits a minimal but spec-conformant layout:
//!   FILE header + USA + $STANDARD_INFORMATION + $FILE_NAME + empty $DATA
//!   + 0xFFFFFFFF end marker.
//!
//! The caller then writes the bytes to disk (raw) and separately flips
//! the `$MFT:$Bitmap` bit + inserts a parent-directory index entry.
//!
//! References (no GPL code consulted): FILE_RECORD_SEGMENT_HEADER and
//! the $STANDARD_INFORMATION / $FILE_NAME / $DATA attribute layouts per
//! Windows Internals 7th ed. ch. "NTFS On-Disk Structure" and MS-FSCC.

/// Encode an NTFS file-reference from (record_number, sequence_number).
/// The low 48 bits are the record number; the high 16 are the sequence.
pub fn encode_file_reference(record_number: u64, sequence: u16) -> u64 {
    (record_number & 0x0000_FFFF_FFFF_FFFF) | ((sequence as u64) << 48)
}

/// "Now" as NT FILETIME (100 ns since 1601-01-01 UTC).
pub fn nt_time_now() -> u64 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs_since_1601 = dur.as_secs() + EPOCH_DIFF;
    secs_since_1601 * 10_000_000 + (dur.subsec_nanos() as u64) / 100
}

const FILE_MAGIC: &[u8; 4] = b"FILE";

/// Fixed header-layout constants.
const REC_OFF_USA_OFFSET: usize = 0x04;
const REC_OFF_USA_COUNT: usize = 0x06;
const REC_OFF_LSN: usize = 0x08;
const REC_OFF_SEQ: usize = 0x10;
const REC_OFF_LINK_COUNT: usize = 0x12;
const REC_OFF_ATTRS_OFFSET: usize = 0x14;
const REC_OFF_FLAGS: usize = 0x16;
const REC_OFF_BYTES_USED: usize = 0x18;
const REC_OFF_BYTES_ALLOCATED: usize = 0x1C;
const REC_OFF_BASE_FILE_REF: usize = 0x20;
const REC_OFF_NEXT_ATTR_ID: usize = 0x28;
const REC_OFF_MFT_REC_NUM: usize = 0x2C;
const USA_OFFSET: usize = 0x30;
// NOTE: `attrs_offset` is computed per-record from `record_size /
// bytes_per_sector`. The previous hardcoded `0x38` happened to be
// correct only for 1024-byte records (sectors=2 → USA spans
// 0x30..0x36 → align8 → 0x38). For 4096-byte / 512-sector records
// the USA spans 0x30..0x42 (1 USN + 8 sector-saved-words), so attrs
// must start at align8(0x42) = 0x48; the hardcoded 0x38 collided with
// USA[4..8] and `apply_fixup_on_write` clobbered the freshly-written
// $STANDARD_INFORMATION attribute. Surfaced when running write_ntfs
// against a default-format (4096) image; tests/write_root_ops.rs
// passes because it uses 1024-byte records.

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END_MARKER: u32 = 0xFFFF_FFFF;

/// File-attribute bits stored in `$STANDARD_INFORMATION` and `$FILE_NAME`
/// (MS-FSCC §2.6). The first three are standard Win32 values; the last two
/// are NTFS-internal bits observed in Microsoft's `format.com` and
/// `chkdsk` reference output but not documented in MS-FSCC.
pub(crate) const FA_HIDDEN: u32 = 0x0000_0002;
pub(crate) const FA_SYSTEM: u32 = 0x0000_0004;
pub(crate) const FA_ARCHIVE: u32 = 0x0000_0020;
/// Set on directory `$FILE_NAME` entries by `format.com` (bit 28).
pub(crate) const FA_NTFS_DIRECTORY: u32 = 0x1000_0000;
/// Set on view-index records (`$ObjId`, `$Reparse`) after `chkdsk` repair (bit 29).
pub(crate) const FA_NTFS_VIEW_INDEX: u32 = 0x2000_0000;

/// `$FILE_NAME.namespace` values per MS-FSCC §2.4.4. Picking the
/// wrong value for a given name is what chkdsk reports as
/// "An invalid filename X (N) was found in directory M / All
/// filenames for File N are invalid / Minor file name errors"
/// (matrix scenario `mac-format-mac-write-win-repeat-mount-3-win-chkdsk`
/// 2026-05-23). Conventions we have to match:
///
/// * `POSIX` (0) — case-sensitive, no DOS alias required. We use it
///   for long user names because it sidesteps the WIN32+DOS pairing
///   requirement.
/// * `WIN32` (1) — case-preserving, requires a paired DOS namespace
///   entry with the 8.3 short name. Avoided here because we'd have
///   to also synthesise the short name + emit a second $FILE_NAME.
/// * `DOS` (2) — the 8.3 short name half of a WIN32+DOS pair.
/// * `WIN32_AND_DOS` (3) — the name fits 8.3 *and* is the unique
///   user-visible representation. Strict DOS-8.3 rule:
///   ≤ 8 stem chars + ≤ 3 extension chars, no other dots, all
///   chars valid in DOS short names. chkdsk rejects this namespace
///   on names that don't fit (e.g. "persistent.txt" — 10-char stem).
const FILE_NAME_NAMESPACE_POSIX: u8 = 0;
const FILE_NAME_NAMESPACE_WIN32_AND_DOS: u8 = 3;

/// Permitted DOS 8.3 short-name characters (the canonical FAT/NTFS
/// short-name alphabet): ASCII alphanumerics plus a small set of
/// punctuation. Excludes spaces, lower-case letters in the strict
/// reading, control chars, Unicode, and reserved metachars. We accept
/// lowercase here because NTFS short names are case-insensitive
/// internally and the upcase table normalises them on disk.
fn is_dos83_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '$' | '%'
                | '\''
                | '-'
                | '_'
                | '@'
                | '~'
                | '`'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '^'
                | '#'
                | '&'
        )
}

/// Pick a `$FILE_NAME.namespace` for a user-supplied basename.
/// Returns `WIN32_AND_DOS` when the name fits the DOS 8.3 envelope
/// (stem 1..=8 DOS chars, ext 0..=3 DOS chars, exactly one or zero
/// dots, no leading or trailing dot), `POSIX` otherwise. chkdsk Stage 2
/// rejects WIN32_AND_DOS on a name that doesn't fit ("An invalid
/// filename X (N) was found in directory M"); see also
/// `mkfs::NAMESPACE_POSIX` doc-comment which records the same rule for
/// the system metafile path.
pub fn fn_namespace_for(name: &str) -> u8 {
    // Reject leading dot (".env", ".foo") — empty stem is not a valid
    // DOS 8.3 form and chkdsk treats it as POSIX-only.
    if name.starts_with('.') || name.ends_with('.') {
        return FILE_NAME_NAMESPACE_POSIX;
    }
    let (stem, ext) = match name.find('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    let stem_len = stem.chars().count();
    let ext_len = ext.chars().count();
    if stem_len == 0
        || stem_len > 8
        || ext_len > 3
        || ext.contains('.')
        || !stem.chars().all(is_dos83_char)
        || !ext.chars().all(is_dos83_char)
    {
        return FILE_NAME_NAMESPACE_POSIX;
    }
    FILE_NAME_NAMESPACE_WIN32_AND_DOS
}

/// Build an MFT record for a regular file. Returns the clean (unfixed-up)
/// buffer. Caller must apply fixup before writing to disk.
pub fn build_regular_file_record(
    record_size: usize,
    record_number: u32,
    sequence: u16,
    parent_reference: u64,
    name: &str,
    nt_time: u64,
    bytes_per_sector: u16,
) -> Result<Vec<u8>, String> {
    build_record_inner(
        record_size,
        record_number,
        sequence,
        parent_reference,
        name,
        nt_time,
        bytes_per_sector,
        /* is_dir */ false,
    )
}

/// Build an MFT record for a directory (with empty
/// `$INDEX_ROOT:$I30`). Directories have no `$DATA`.
pub fn build_directory_record(
    record_size: usize,
    record_number: u32,
    sequence: u16,
    parent_reference: u64,
    name: &str,
    nt_time: u64,
    bytes_per_sector: u16,
    index_block_size: u32,
) -> Result<Vec<u8>, String> {
    if record_size < 512 || !record_size.is_multiple_of(bytes_per_sector as usize) {
        return Err(format!("invalid record_size {record_size}"));
    }
    let utf16: Vec<u16> = name.encode_utf16().collect();
    if utf16.is_empty() || utf16.len() > 255 {
        return Err(format!("invalid name length {}", utf16.len()));
    }

    let mut rec = vec![0u8; record_size];
    rec[0..4].copy_from_slice(FILE_MAGIC);
    rec[REC_OFF_USA_OFFSET..REC_OFF_USA_OFFSET + 2]
        .copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
    let sectors = record_size / bytes_per_sector as usize;
    rec[REC_OFF_USA_COUNT..REC_OFF_USA_COUNT + 2]
        .copy_from_slice(&((sectors + 1) as u16).to_le_bytes());
    rec[REC_OFF_LSN..REC_OFF_LSN + 8].copy_from_slice(&0u64.to_le_bytes());
    rec[REC_OFF_SEQ..REC_OFF_SEQ + 2].copy_from_slice(&sequence.to_le_bytes());
    rec[REC_OFF_LINK_COUNT..REC_OFF_LINK_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());
    // attrs_offset = first 8-aligned byte past the USA. USA is 1 USN
    // plus one saved-word per sector, all u16. See note on USA_OFFSET.
    let attrs_offset = align8(USA_OFFSET + 2 + sectors * 2);
    rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
        .copy_from_slice(&(attrs_offset as u16).to_le_bytes());
    // IN_USE | DIRECTORY
    rec[REC_OFF_FLAGS..REC_OFF_FLAGS + 2].copy_from_slice(&0x0003u16.to_le_bytes());
    rec[REC_OFF_BYTES_ALLOCATED..REC_OFF_BYTES_ALLOCATED + 4]
        .copy_from_slice(&(record_size as u32).to_le_bytes());
    rec[REC_OFF_BASE_FILE_REF..REC_OFF_BASE_FILE_REF + 8].copy_from_slice(&0u64.to_le_bytes());
    rec[REC_OFF_NEXT_ATTR_ID..REC_OFF_NEXT_ATTR_ID + 2].copy_from_slice(&3u16.to_le_bytes());
    rec[REC_OFF_MFT_REC_NUM..REC_OFF_MFT_REC_NUM + 4].copy_from_slice(&record_number.to_le_bytes());
    // Initial USN = 1.
    rec[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());

    let mut cursor = attrs_offset;
    cursor = write_standard_information(&mut rec, cursor, 0, nt_time, /* is_dir */ true);
    cursor = write_file_name(
        &mut rec,
        cursor,
        1,
        parent_reference,
        &utf16,
        nt_time,
        /* is_dir */ true,
        fn_namespace_for(name),
    );
    cursor = write_empty_index_root(&mut rec, cursor, 2, index_block_size, bytes_per_sector)?;

    // W2.5 — bounds guard, same rationale as `build_record_inner`.
    if cursor + 8 > record_size {
        return Err(format!(
            "record overflow: attributes consumed {} bytes, no room for 8-byte END marker in {}-byte record",
            cursor, record_size
        ));
    }

    // END marker is 4 bytes magic + 4 bytes attribute_length=0 — see
    // the matching comment in `build_regular_file_record` above.
    // chkdsk's "First free byte offset corrected" complaint also fires
    // against new directories without the +8 inclusion.
    rec[cursor..cursor + 4].copy_from_slice(&ATTR_END_MARKER.to_le_bytes());
    cursor += 8;
    rec[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&(cursor as u32).to_le_bytes());

    Ok(rec)
}

const ATTR_INDEX_ROOT: u32 = 0x90;

/// Emit an `$INDEX_ROOT:$I30` resident attribute containing just the
/// LAST sentinel entry (empty directory).
fn write_empty_index_root(
    rec: &mut [u8],
    at: usize,
    attr_id: u16,
    index_block_size: u32,
    bytes_per_sector: u16,
) -> Result<usize, String> {
    // Name "$I30" in UTF-16 (4 chars × 2 bytes = 8 bytes).
    let name_u16: [u16; 4] = ['$' as u16, 'I' as u16, '3' as u16, '0' as u16];
    let name_bytes = 8usize;
    let header_size = 24usize;
    let name_offset = header_size; // name comes right after the resident header
    let value_offset = align8(header_size + name_bytes);

    // INDEX_ROOT value layout:
    //   IR_HEADER (16) + INDEX_HEADER (16) + LAST sentinel entry (16)
    let ir_value_size = 16 + 16 + 16;
    let attr_length = align8(value_offset + ir_value_size);

    if at + attr_length > rec.len() {
        return Err("$INDEX_ROOT doesn't fit in MFT record".to_string());
    }

    // Resident attribute header.
    rec[at..at + 4].copy_from_slice(&ATTR_INDEX_ROOT.to_le_bytes());
    rec[at + 4..at + 8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    rec[at + 8] = 0; // resident
    rec[at + 9] = name_u16.len() as u8;
    rec[at + 10..at + 12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    rec[at + 12..at + 14].copy_from_slice(&0u16.to_le_bytes());
    rec[at + 14..at + 16].copy_from_slice(&attr_id.to_le_bytes());
    rec[at + 16..at + 20].copy_from_slice(&(ir_value_size as u32).to_le_bytes());
    rec[at + 20..at + 22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    rec[at + 22] = 0;
    rec[at + 23] = 0;

    // Name bytes ("$I30" UTF-16 LE).
    for (i, c) in name_u16.iter().enumerate() {
        let off = at + name_offset + i * 2;
        rec[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }

    // INDEX_ROOT header (16 bytes) at attr_value_start.
    let v = at + value_offset;
    // attribute_type = $FILE_NAME (0x30)
    rec[v..v + 4].copy_from_slice(&0x30u32.to_le_bytes());
    // collation_rule = COLLATION_FILE_NAME (1)
    rec[v + 4..v + 8].copy_from_slice(&1u32.to_le_bytes());
    // index_block_size
    rec[v + 8..v + 12].copy_from_slice(&index_block_size.to_le_bytes());
    // clusters_per_index_block: if block_size >= cluster_size use clusters;
    // if smaller use blocks_per_cluster negative encoding. For our MVP
    // always set to 1 (assumes block_size == cluster_size).
    rec[v + 12] = 1;
    // 3 bytes padding
    rec[v + 13] = 0;
    rec[v + 14] = 0;
    rec[v + 15] = 0;

    // INDEX_HEADER (16 bytes).
    let ih = v + 16;
    // first_entry = 16 (immediately after INDEX_HEADER)
    rec[ih..ih + 4].copy_from_slice(&16u32.to_le_bytes());
    // total_size = 16 (header) + 16 (LAST entry) = 32? No — total_size is
    // measured from INDEX_HEADER start and covers the entries. That is,
    // INDEX_HEADER + entries = 16 + 16 = 32.
    rec[ih + 4..ih + 8].copy_from_slice(&32u32.to_le_bytes());
    // allocated_size same as total_size for a fresh IR.
    rec[ih + 8..ih + 12].copy_from_slice(&32u32.to_le_bytes());
    // flags = 0 (no subnode)
    rec[ih + 12] = 0;
    rec[ih + 13] = 0;
    rec[ih + 14] = 0;
    rec[ih + 15] = 0;

    // LAST sentinel entry (16 bytes).
    let le = ih + 16;
    rec[le..le + 8].copy_from_slice(&0u64.to_le_bytes()); // file_reference
    rec[le + 8..le + 10].copy_from_slice(&16u16.to_le_bytes()); // length
    rec[le + 10..le + 12].copy_from_slice(&0u16.to_le_bytes()); // key_length
    rec[le + 12..le + 14].copy_from_slice(&0x0002u16.to_le_bytes()); // flags = LAST
    rec[le + 14..le + 16].copy_from_slice(&0u16.to_le_bytes()); // reserved

    // Bytes-per-sector is not used here but kept in sig for future
    // INDX-block synthesis when mkdir wants $INDEX_ALLOCATION.
    let _ = bytes_per_sector;

    Ok(at + attr_length)
}

fn build_record_inner(
    record_size: usize,
    record_number: u32,
    sequence: u16,
    parent_reference: u64,
    name: &str,
    nt_time: u64,
    bytes_per_sector: u16,
    is_dir: bool,
) -> Result<Vec<u8>, String> {
    if record_size < 512 || !record_size.is_multiple_of(bytes_per_sector as usize) {
        return Err(format!("invalid record_size {record_size}"));
    }
    let utf16: Vec<u16> = name.encode_utf16().collect();
    if utf16.is_empty() || utf16.len() > 255 {
        return Err(format!("invalid name length {}", utf16.len()));
    }

    let mut rec = vec![0u8; record_size];

    // ----- FILE record header -----
    rec[0..4].copy_from_slice(FILE_MAGIC);
    rec[REC_OFF_USA_OFFSET..REC_OFF_USA_OFFSET + 2]
        .copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
    let sectors = record_size / bytes_per_sector as usize;
    let usa_count = sectors + 1; // 1 USN + N saved words
    rec[REC_OFF_USA_COUNT..REC_OFF_USA_COUNT + 2]
        .copy_from_slice(&(usa_count as u16).to_le_bytes());
    rec[REC_OFF_LSN..REC_OFF_LSN + 8].copy_from_slice(&0u64.to_le_bytes());
    rec[REC_OFF_SEQ..REC_OFF_SEQ + 2].copy_from_slice(&sequence.to_le_bytes());
    rec[REC_OFF_LINK_COUNT..REC_OFF_LINK_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());
    // attrs_offset = first 8-aligned byte past the USA (see USA_OFFSET).
    let attrs_offset = align8(USA_OFFSET + 2 + sectors * 2);
    rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
        .copy_from_slice(&(attrs_offset as u16).to_le_bytes());
    let flags = 0x0001u16 | if is_dir { 0x0002 } else { 0x0000 };
    rec[REC_OFF_FLAGS..REC_OFF_FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
    // bytes_used + bytes_allocated: filled below.
    rec[REC_OFF_BYTES_ALLOCATED..REC_OFF_BYTES_ALLOCATED + 4]
        .copy_from_slice(&(record_size as u32).to_le_bytes());
    rec[REC_OFF_BASE_FILE_REF..REC_OFF_BASE_FILE_REF + 8].copy_from_slice(&0u64.to_le_bytes());
    // next_attr_id: highest attr_id + 1 (we use IDs 0,1,2 → next = 3)
    rec[REC_OFF_NEXT_ATTR_ID..REC_OFF_NEXT_ATTR_ID + 2].copy_from_slice(&3u16.to_le_bytes());
    rec[REC_OFF_MFT_REC_NUM..REC_OFF_MFT_REC_NUM + 4].copy_from_slice(&record_number.to_le_bytes());

    // USA: initial USN = 1 (avoid 0).
    rec[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());
    // Saved words stay 0 (since the record is freshly zeroed, the
    // sector-end slots ARE 0 — which is what the USA should reflect).

    // ----- Attributes -----
    let mut cursor = attrs_offset;

    cursor = write_standard_information(&mut rec, cursor, 0, nt_time, is_dir);
    cursor = write_file_name(
        &mut rec,
        cursor,
        1,
        parent_reference,
        &utf16,
        nt_time,
        is_dir,
        fn_namespace_for(name),
    );
    cursor = write_empty_data(&mut rec, cursor, 2);

    // W2.5 — bounds guard. The resident-only attributes above
    // (`$STANDARD_INFORMATION` + `$FILE_NAME` + `$DATA`) plus the
    // 8-byte END marker must all fit in `record_size`. The
    // realistic exposure is small (4096-byte records have ~3700
    // bytes free), but a 1024-byte record + a 255-UTF-16-char name
    // + hard links could exhaust. Fail loudly before writing the
    // END marker would overflow.
    if cursor + 8 > record_size {
        return Err(format!(
            "record overflow: attributes consumed {} bytes, no room for 8-byte END marker in {}-byte record",
            cursor, record_size
        ));
    }

    // End marker. NTFS spec records the END marker as 4 bytes of
    // 0xFFFFFFFF *followed by* 4 bytes of `attribute_length = 0` —
    // the END "attribute" is technically 8 bytes total, even though
    // only the first 4 are the magic. chkdsk reports
    // `First free byte offset corrected in file record segment N`
    // when `used_size` includes only the 4-byte magic. Match
    // `mkfs::build_system_record` (cursor += 8 there).
    rec[cursor..cursor + 4].copy_from_slice(&ATTR_END_MARKER.to_le_bytes());
    cursor += 8;
    let bytes_used = cursor as u32;
    rec[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&bytes_used.to_le_bytes());

    Ok(rec)
}

/// Build a resident `$EA_INFORMATION` (type 0xD0) attribute blob.
/// Value is the 8-byte struct from `ea_io::build_ea_information_value`.
pub fn build_resident_ea_information_attribute(
    attr_id: u16,
    value: &[u8],
) -> Result<Vec<u8>, String> {
    if value.len() != 8 {
        return Err(format!(
            "$EA_INFORMATION value must be 8 bytes, got {}",
            value.len()
        ));
    }
    build_resident_unnamed_attribute(0xD0, attr_id, value)
}

/// Build a resident `$EA` (type 0xE0) attribute blob wrapping a packed
/// EA byte stream.
pub fn build_resident_ea_attribute(attr_id: u16, packed: &[u8]) -> Result<Vec<u8>, String> {
    build_resident_unnamed_attribute(0xE0, attr_id, packed)
}

/// Shared builder for resident unnamed attributes: 24-byte header +
/// value + padding to 8.
/// Build a resident `$VOLUME_NAME` (type 0x60) attribute carrying
/// the volume label as raw UTF-16 little-endian bytes. NTFS labels
/// are capped at 32 UTF-16 code units (64 bytes) by convention —
/// modern Windows tools refuse to display longer labels — but the
/// on-disk format places no length cap, so callers responsible for
/// validation. Pass `label_utf16` already encoded; the empty slice
/// produces an empty-but-present attribute (NOT a removed one — use
/// the `remove_volume_label` writer for that semantic).
pub fn build_resident_volume_name_attribute(attr_id: u16, label_utf16: &[u8]) -> Vec<u8> {
    let header_size = 24usize;
    let attr_length = align8(header_size + label_utf16.len());
    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&0x60u32.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0; // resident
    buf[9] = 0; // name_length (unnamed)
    buf[10..12].copy_from_slice(&(header_size as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(label_utf16.len() as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(header_size as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;
    buf[header_size..header_size + label_utf16.len()].copy_from_slice(label_utf16);
    buf
}

fn build_resident_unnamed_attribute(
    attr_type: u32,
    attr_id: u16,
    value: &[u8],
) -> Result<Vec<u8>, String> {
    let header_size = 24usize;
    let attr_length = align8(header_size + value.len());
    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&attr_type.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0; // resident
    buf[9] = 0; // name_length
    buf[10..12].copy_from_slice(&(header_size as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(header_size as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;
    buf[header_size..header_size + value.len()].copy_from_slice(value);
    Ok(buf)
}

/// Build a resident `$REPARSE_POINT` attribute with the given tag and
/// tag-specific data. Returns the attribute bytes (header + reparse
/// header + data, padded to 8).
///
/// Structure per [MS-FSCC 2.1.2](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/c8e77b37-3909-4fe6-a4ea-2b9d423b1ee4):
///   u32 reparse_tag
///   u16 reparse_data_length  (= data.len())
///   u16 reserved
///   u8\[reparse_data_length\] data
pub fn build_resident_reparse_point_attribute(
    attr_id: u16,
    reparse_tag: u32,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    if data.len() > u16::MAX as usize {
        return Err(format!(
            "reparse data {} bytes exceeds u16 ceiling",
            data.len()
        ));
    }
    let common_header = 16usize;
    let resident_fields = 8usize;
    let header_size = common_header + resident_fields;
    let value_offset = header_size;
    let reparse_header = 8usize; // tag(4) + data_len(2) + reserved(2)
    let value_size = reparse_header + data.len();
    let attr_length = align8(value_offset + value_size);

    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&ATTR_REPARSE_POINT.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0; // resident
    buf[9] = 0; // name_length (unnamed)
    buf[10..12].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(value_size as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;

    // Reparse header.
    buf[value_offset..value_offset + 4].copy_from_slice(&reparse_tag.to_le_bytes());
    buf[value_offset + 4..value_offset + 6].copy_from_slice(&(data.len() as u16).to_le_bytes());
    buf[value_offset + 6..value_offset + 8].copy_from_slice(&0u16.to_le_bytes());
    // Reparse data.
    buf[value_offset + 8..value_offset + 8 + data.len()].copy_from_slice(data);

    Ok(buf)
}

const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_OBJECT_ID: u32 = 0x40;

/// Build a resident `$OBJECT_ID` (type 0x40) attribute carrying a
/// 16-byte GUID. Per MS-FSCC §2.4.6 the on-disk layout starts with the
/// `object_id` GUID and may optionally carry three more 16-byte GUIDs
/// (`birth_volume_id`, `birth_object_id`, `birth_domain_id`); this
/// builder emits only the mandatory 16-byte prefix, which is all
/// modern Windows volumes need for the file to round-trip via
/// `FSCTL_GET_OBJECT_ID`. Use [`build_resident_object_id_attribute_full`]
/// to write the 64-byte extended form including the three Birth GUIDs.
pub fn build_resident_object_id_attribute(attr_id: u16, object_id: &[u8; 16]) -> Vec<u8> {
    build_resident_object_id_attribute_full(attr_id, object_id, None)
}

/// Full `$OBJECT_ID` attribute layout per MS-FSCC §2.4.6:
///
/// ```text
///   +0x00  object_id        u8[16]   (mandatory)
///   +0x10  birth_volume_id  u8[16]   (optional)
///   +0x20  birth_object_id  u8[16]   (optional)
///   +0x30  birth_domain_id  u8[16]   (optional)
/// ```
///
/// All three Birth fields are present together or not at all — that's
/// how Microsoft DLT (Distributed Link Tracking) interprets the
/// `value_length`: 16 = mandatory-only, 64 = full record. The 32- and
/// 48-byte forms are technically representable per spec but neither
/// chkdsk nor ntfs.sys document interpretation for them, so this
/// builder ships exactly the two well-formed shapes.
///
/// `birth_ids = Some((bv, bo, bd))` writes the 64-byte form;
/// `None` emits the 16-byte form (equivalent to
/// [`build_resident_object_id_attribute`]).
pub fn build_resident_object_id_attribute_full(
    attr_id: u16,
    object_id: &[u8; 16],
    birth_ids: Option<(&[u8; 16], &[u8; 16], &[u8; 16])>,
) -> Vec<u8> {
    let header_size = 24usize;
    let value_offset = header_size;
    let value_size = if birth_ids.is_some() {
        64usize
    } else {
        16usize
    };
    let attr_length = align8(value_offset + value_size);

    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&ATTR_OBJECT_ID.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0; // resident
    buf[9] = 0; // name_length
    buf[10..12].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes()); // flags
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(value_size as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[22] = 0; // indexed_flag
    buf[23] = 0;
    buf[value_offset..value_offset + 16].copy_from_slice(object_id);
    if let Some((bv, bo, bd)) = birth_ids {
        buf[value_offset + 16..value_offset + 32].copy_from_slice(bv);
        buf[value_offset + 32..value_offset + 48].copy_from_slice(bo);
        buf[value_offset + 48..value_offset + 64].copy_from_slice(bd);
    }
    buf
}

/// Common reparse tags (MS-FSCC 2.1.2).
pub mod reparse_tag {
    pub const SYMLINK: u32 = 0xA000_000C;
    pub const MOUNT_POINT: u32 = 0xA000_0003;
    pub const WOF: u32 = 0x8000_0017;
    pub const LX_SYMLINK: u32 = 0xA000_001D;
    pub const APPEXECLINK: u32 = 0x8000_001B;
}

/// Build a `SymbolicLinkReparseBuffer` (MS-FSCC 2.1.2.4) for an
/// `IO_REPARSE_TAG_SYMLINK` reparse point. `target` is the substitute
/// name (the NT-style path the symlink resolves to). `print_name` is
/// what Windows Explorer displays — defaults to `target` if `None`.
/// `relative` should be `true` for relative paths.
pub fn build_symlink_reparse_data(
    target: &str,
    print_name: Option<&str>,
    relative: bool,
) -> Vec<u8> {
    let print_name = print_name.unwrap_or(target);
    let sub_utf16: Vec<u16> = target.encode_utf16().collect();
    let print_utf16: Vec<u16> = print_name.encode_utf16().collect();
    let sub_len = sub_utf16.len() * 2;
    let print_len = print_utf16.len() * 2;

    // SymbolicLinkReparseBuffer header (12 bytes) + PathBuffer
    let header = 12usize;
    let path_buffer_len = sub_len + print_len;
    let mut out = vec![0u8; header + path_buffer_len];
    // SubstituteNameOffset (offset from start of PathBuffer)
    out[0..2].copy_from_slice(&0u16.to_le_bytes());
    // SubstituteNameLength (bytes)
    out[2..4].copy_from_slice(&(sub_len as u16).to_le_bytes());
    // PrintNameOffset
    out[4..6].copy_from_slice(&(sub_len as u16).to_le_bytes());
    // PrintNameLength
    out[6..8].copy_from_slice(&(print_len as u16).to_le_bytes());
    // Flags: 0x00 = absolute, 0x01 = relative
    let flags: u32 = if relative { 0x1 } else { 0x0 };
    out[8..12].copy_from_slice(&flags.to_le_bytes());
    // PathBuffer: SubstituteName then PrintName.
    let mut off = header;
    for c in &sub_utf16 {
        out[off..off + 2].copy_from_slice(&c.to_le_bytes());
        off += 2;
    }
    for c in &print_utf16 {
        out[off..off + 2].copy_from_slice(&c.to_le_bytes());
        off += 2;
    }
    out
}

/// Build a resident named `$DATA` attribute (alternate data stream).
/// Returns the attribute bytes (header + name + padding + value),
/// 8-byte aligned and with the header's `length` set correctly.
pub fn build_named_resident_data_attribute(
    attr_id: u16,
    stream_name: &str,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    let name_u16: Vec<u16> = stream_name.encode_utf16().collect();
    if name_u16.is_empty() || name_u16.len() > 255 {
        return Err(format!("invalid stream name length {}", name_u16.len()));
    }
    let common_header = 16usize;
    let resident_fields = 8usize;
    let header_size = common_header + resident_fields;
    let name_offset = header_size;
    let name_bytes = name_u16.len() * 2;
    let value_offset = align8(name_offset + name_bytes);
    let attr_length = align8(value_offset + data.len());

    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0; // resident
    buf[9] = name_u16.len() as u8;
    buf[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes()); // value_length
    buf[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes()); // value_offset
    buf[22] = 0; // resident_flags
    buf[23] = 0; // reserved

    for (i, c) in name_u16.iter().enumerate() {
        let off = name_offset + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    buf[value_offset..value_offset + data.len()].copy_from_slice(data);

    Ok(buf)
}

/// Round `n` up to the next 8-byte boundary.
///
/// NTFS requires all attribute headers and values to start at offsets that
/// are a multiple of 8 within the MFT record (MS-FSCC §2.3). The formula
/// `(n + 7) & !7` is the standard power-of-two alignment trick: adding 7
/// ensures we cross the next boundary, then masking off the low 3 bits
/// snaps back to it.
pub fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Build a non-resident unnamed `$DATA` attribute blob. `mapping_pairs`
/// must already be a terminator-appended mapping-pairs byte sequence
/// (see `data_runs::encode_runs`).
///
/// `last_vcn` is the 0-based index of the last cluster. For a
/// zero-length value, pass `-1` (will be encoded in the LAST_VCN
/// field as signed).
pub fn build_nonresident_data_attribute(
    attr_id: u16,
    data_length: u64,
    allocated_length: u64,
    initialized_length: u64,
    last_vcn: i64,
    mapping_pairs: &[u8],
) -> Result<Vec<u8>, String> {
    let common_header = 16usize;
    // Non-resident specific fields span +0x10..+0x40 (first_vcn/last_vcn
    // (16) + mapping_pairs_offset/compression_unit (4) + reserved (4) +
    // allocated/data/initialized lengths (24)) = 48 bytes.
    let nonres_fields = 48usize;
    let header_size = common_header + nonres_fields;
    let mapping_offset = header_size; // name = empty, mapping_pairs immediately after header
    let value_size = mapping_pairs.len();
    let attr_length = align8(header_size + value_size);

    let mut buf = vec![0u8; attr_length];

    buf[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 1; // non_resident
    buf[9] = 0; // name_length (unnamed stream)
    buf[10..12].copy_from_slice(&(mapping_offset as u16).to_le_bytes()); // name_offset
    buf[12..14].copy_from_slice(&0u16.to_le_bytes()); // flags (not compressed/sparse/encrypted)
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());

    // Non-resident fields:
    buf[16..24].copy_from_slice(&0u64.to_le_bytes()); // first_vcn
    buf[24..32].copy_from_slice(&last_vcn.to_le_bytes()); // last_vcn
    buf[32..34].copy_from_slice(&(mapping_offset as u16).to_le_bytes()); // mapping_pairs_offset
    buf[34..36].copy_from_slice(&0u16.to_le_bytes()); // compression_unit
    buf[36..40].copy_from_slice(&0u32.to_le_bytes()); // reserved
    buf[40..48].copy_from_slice(&allocated_length.to_le_bytes());
    buf[48..56].copy_from_slice(&data_length.to_le_bytes());
    buf[56..64].copy_from_slice(&initialized_length.to_le_bytes());

    buf[mapping_offset..mapping_offset + mapping_pairs.len()].copy_from_slice(mapping_pairs);
    // tail bytes from mapping_offset + mapping_pairs.len() .. attr_length remain 0

    Ok(buf)
}

/// Build a non-resident attribute of arbitrary type and optional name.
/// Generalizes `build_nonresident_data_attribute`. Caller supplies the
/// full attribute type code (e.g. `0x80` for `$DATA`, `0xC0` for
/// `$REPARSE_POINT`, `0xE0` for `$EA`) and an optional UTF-16 name
/// (used for alternate data streams).
pub fn build_nonresident_attribute(
    attr_type: u32,
    attr_name: Option<&str>,
    attr_id: u16,
    data_length: u64,
    allocated_length: u64,
    initialized_length: u64,
    last_vcn: i64,
    mapping_pairs: &[u8],
) -> Result<Vec<u8>, String> {
    let name_u16: Vec<u16> = attr_name
        .map(|s| s.encode_utf16().collect())
        .unwrap_or_default();
    if name_u16.len() > 255 {
        return Err(format!("attribute name too long: {}", name_u16.len()));
    }
    let common_header = 16usize;
    let nonres_fields = 48usize;
    let header_size = common_header + nonres_fields;
    let name_offset = header_size;
    let name_bytes = name_u16.len() * 2;
    let mapping_offset = align8(name_offset + name_bytes);
    let value_size = mapping_pairs.len();
    let attr_length = align8(mapping_offset + value_size);

    let mut buf = vec![0u8; attr_length];

    buf[0..4].copy_from_slice(&attr_type.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 1; // non_resident
    buf[9] = name_u16.len() as u8;
    buf[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes()); // flags
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());

    buf[16..24].copy_from_slice(&0u64.to_le_bytes()); // first_vcn
    buf[24..32].copy_from_slice(&last_vcn.to_le_bytes());
    buf[32..34].copy_from_slice(&(mapping_offset as u16).to_le_bytes());
    buf[34..36].copy_from_slice(&0u16.to_le_bytes()); // compression_unit
    buf[36..40].copy_from_slice(&0u32.to_le_bytes()); // reserved
    buf[40..48].copy_from_slice(&allocated_length.to_le_bytes());
    buf[48..56].copy_from_slice(&data_length.to_le_bytes());
    buf[56..64].copy_from_slice(&initialized_length.to_le_bytes());

    for (i, c) in name_u16.iter().enumerate() {
        let off = name_offset + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    buf[mapping_offset..mapping_offset + mapping_pairs.len()].copy_from_slice(mapping_pairs);

    Ok(buf)
}

fn write_standard_information(
    rec: &mut [u8],
    at: usize,
    attr_id: u16,
    nt_time: u64,
    is_dir: bool,
) -> usize {
    let header_size = 24usize;
    let value_size = 72usize; // NTFS 3.x+
    let attr_length = align8(header_size + value_size);
    // Header
    rec[at..at + 4].copy_from_slice(&ATTR_STANDARD_INFORMATION.to_le_bytes());
    rec[at + 4..at + 8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    rec[at + 8] = 0; // resident
    rec[at + 9] = 0; // name_length
    rec[at + 10..at + 12].copy_from_slice(&(header_size as u16).to_le_bytes()); // name_offset (unused, points past)
    rec[at + 12..at + 14].copy_from_slice(&0u16.to_le_bytes()); // flags
    rec[at + 14..at + 16].copy_from_slice(&attr_id.to_le_bytes()); // attribute_id
    rec[at + 16..at + 20].copy_from_slice(&(value_size as u32).to_le_bytes()); // value_length
    rec[at + 20..at + 22].copy_from_slice(&(header_size as u16).to_le_bytes()); // value_offset
    rec[at + 22] = 0; // resident_flags
    rec[at + 23] = 0; // reserved
                      // Value
    let v = at + header_size;
    rec[v..v + 8].copy_from_slice(&nt_time.to_le_bytes()); // creation
    rec[v + 8..v + 16].copy_from_slice(&nt_time.to_le_bytes()); // modification
    rec[v + 16..v + 24].copy_from_slice(&nt_time.to_le_bytes()); // mft change
    rec[v + 24..v + 32].copy_from_slice(&nt_time.to_le_bytes()); // access
    let fa: u32 = if is_dir {
        FA_NTFS_DIRECTORY | FA_ARCHIVE
    } else {
        FA_ARCHIVE
    };
    rec[v + 32..v + 36].copy_from_slice(&fa.to_le_bytes()); // file_attributes
                                                            // rest (max_versions .. usn) stays 0
    at + attr_length
}

/// Build a standalone `$FILE_NAME` attribute blob (header + value),
/// 8-byte aligned. Used for adding hard links.
pub fn build_file_name_attribute(
    attr_id: u16,
    parent_reference: u64,
    name: &str,
    nt_time: u64,
    is_dir: bool,
) -> Result<Vec<u8>, String> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    if utf16.is_empty() || utf16.len() > 255 {
        return Err(format!("invalid name length {}", utf16.len()));
    }
    let header_size = 24usize;
    let key_fixed = 0x42usize;
    let value_size = key_fixed + utf16.len() * 2;
    let attr_length = align8(header_size + value_size);
    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0;
    buf[9] = 0;
    buf[10..12].copy_from_slice(&(header_size as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(value_size as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(header_size as u16).to_le_bytes());
    // indexed_flag = 1 on every $FILE_NAME attribute. Without this byte
    // chkdsk reports `Attribute record (30, "") from file record
    // segment N is corrupt` against every newly-created file/dir
    // (matrix scenarios `mac-format-mkdir-set-dirty-win-chkdsk`,
    // `mac-format-write-set-dirty-win-chkdsk`,
    // `mac-format-mac-write-win-repeat-mount-3-win-chkdsk`). Same
    // finding as `mkfs::write_file_name`'s comment, originally
    // corroborated against `format.com`'s reference output in CI
    // iter8 — this builder predates that fix and was missing it.
    buf[22] = 1;
    buf[23] = 0;

    let v = header_size;
    buf[v..v + 8].copy_from_slice(&parent_reference.to_le_bytes());
    buf[v + 8..v + 16].copy_from_slice(&nt_time.to_le_bytes());
    buf[v + 16..v + 24].copy_from_slice(&nt_time.to_le_bytes());
    buf[v + 24..v + 32].copy_from_slice(&nt_time.to_le_bytes());
    buf[v + 32..v + 40].copy_from_slice(&nt_time.to_le_bytes());
    buf[v + 40..v + 48].copy_from_slice(&0u64.to_le_bytes());
    buf[v + 48..v + 56].copy_from_slice(&0u64.to_le_bytes());
    let fa: u32 = if is_dir {
        FA_NTFS_DIRECTORY | FA_ARCHIVE
    } else {
        FA_ARCHIVE
    };
    buf[v + 56..v + 60].copy_from_slice(&fa.to_le_bytes());
    buf[v + 60..v + 64].copy_from_slice(&0u32.to_le_bytes());
    buf[v + 64] = utf16.len() as u8;
    buf[v + 65] = fn_namespace_for(name);
    for (i, c) in utf16.iter().enumerate() {
        let off = v + 66 + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    Ok(buf)
}

fn write_file_name(
    rec: &mut [u8],
    at: usize,
    attr_id: u16,
    parent_reference: u64,
    name_utf16: &[u16],
    nt_time: u64,
    is_dir: bool,
    namespace: u8,
) -> usize {
    let header_size = 24usize;
    let key_fixed = 0x42usize; // parent_ref(8) + 4 times(32) + alloc_size(8) + real_size(8) + attr(4) + reparse/ea(4) + name_len(1) + namespace(1)
    let value_size = key_fixed + name_utf16.len() * 2;
    let attr_length = align8(header_size + value_size);
    // Header
    rec[at..at + 4].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes());
    rec[at + 4..at + 8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    rec[at + 8] = 0;
    rec[at + 9] = 0;
    rec[at + 10..at + 12].copy_from_slice(&(header_size as u16).to_le_bytes());
    rec[at + 12..at + 14].copy_from_slice(&0u16.to_le_bytes());
    rec[at + 14..at + 16].copy_from_slice(&attr_id.to_le_bytes());
    rec[at + 16..at + 20].copy_from_slice(&(value_size as u32).to_le_bytes());
    rec[at + 20..at + 22].copy_from_slice(&(header_size as u16).to_le_bytes());
    // indexed_flag = 1: see comment on `build_file_name_attribute`
    // above for the same fix — chkdsk reports `Attribute record (30,
    // "") is corrupt` when it differs.
    rec[at + 22] = 1;
    rec[at + 23] = 0;
    // Value
    let v = at + header_size;
    rec[v..v + 8].copy_from_slice(&parent_reference.to_le_bytes()); // parent_directory_reference
    rec[v + 8..v + 16].copy_from_slice(&nt_time.to_le_bytes()); // creation
    rec[v + 16..v + 24].copy_from_slice(&nt_time.to_le_bytes()); // modification
    rec[v + 24..v + 32].copy_from_slice(&nt_time.to_le_bytes()); // mft_change
    rec[v + 32..v + 40].copy_from_slice(&nt_time.to_le_bytes()); // access
    rec[v + 40..v + 48].copy_from_slice(&0u64.to_le_bytes()); // allocated_size
    rec[v + 48..v + 56].copy_from_slice(&0u64.to_le_bytes()); // real_size
    let fa: u32 = if is_dir {
        FA_NTFS_DIRECTORY | FA_ARCHIVE
    } else {
        FA_ARCHIVE
    };
    rec[v + 56..v + 60].copy_from_slice(&fa.to_le_bytes()); // file_attributes
    rec[v + 60..v + 64].copy_from_slice(&0u32.to_le_bytes()); // ea/reparse
    rec[v + 64] = name_utf16.len() as u8; // name_length
    rec[v + 65] = namespace;
    // name
    for (i, c) in name_utf16.iter().enumerate() {
        let off = v + 66 + i * 2;
        rec[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    at + attr_length
}

fn write_empty_data(rec: &mut [u8], at: usize, attr_id: u16) -> usize {
    let header_size = 24usize; // resident header
    let attr_length = align8(header_size);
    rec[at..at + 4].copy_from_slice(&ATTR_DATA.to_le_bytes());
    rec[at + 4..at + 8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    rec[at + 8] = 0;
    rec[at + 9] = 0;
    rec[at + 10..at + 12].copy_from_slice(&(header_size as u16).to_le_bytes());
    rec[at + 12..at + 14].copy_from_slice(&0u16.to_le_bytes());
    rec[at + 14..at + 16].copy_from_slice(&attr_id.to_le_bytes());
    rec[at + 16..at + 20].copy_from_slice(&0u32.to_le_bytes()); // value_length = 0
    rec[at + 20..at + 22].copy_from_slice(&(header_size as u16).to_le_bytes());
    rec[at + 22] = 0;
    rec[at + 23] = 0;
    at + attr_length
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pure utility functions --------------------------------------------

    #[test]
    fn align8_rounds_up_to_multiple_of_eight() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(7), 8);
        assert_eq!(align8(8), 8);
        assert_eq!(align8(9), 16);
        assert_eq!(align8(15), 16);
        assert_eq!(align8(16), 16);
        assert_eq!(align8(17), 24);
    }

    #[test]
    fn encode_file_reference_packs_record_number_in_low_48_and_sequence_in_high_16() {
        // record_number=0x1234_5678, sequence=0xAABB
        let r = encode_file_reference(0x0000_1234_5678, 0xAABB);
        assert_eq!(r >> 48, 0xAABB);
        assert_eq!(r & 0x0000_FFFF_FFFF_FFFF, 0x0000_1234_5678);
        // record_number that exceeds 48 bits is silently masked.
        let r = encode_file_reference(u64::MAX, 0);
        assert_eq!(r, 0x0000_FFFF_FFFF_FFFF);
    }

    #[test]
    fn nt_time_now_is_above_year_2020_floor() {
        // Year 2020 in NT FILETIME (100ns since 1601-01-01).
        // 2020-01-01 ≈ 132_223_104_000_000_000.
        let now = nt_time_now();
        assert!(
            now > 132_223_104_000_000_000,
            "nt_time_now={now} is before 2020"
        );
    }

    // --- fn_namespace_for: 8.3 detection -----------------------------------

    #[test]
    fn fn_namespace_for_short_simple_name_picks_win32_and_dos() {
        assert_eq!(
            fn_namespace_for("README"),
            FILE_NAME_NAMESPACE_WIN32_AND_DOS
        );
        assert_eq!(
            fn_namespace_for("README.TXT"),
            FILE_NAME_NAMESPACE_WIN32_AND_DOS
        );
        assert_eq!(fn_namespace_for("a.b"), FILE_NAME_NAMESPACE_WIN32_AND_DOS);
    }

    #[test]
    fn fn_namespace_for_stem_over_eight_chars_picks_posix() {
        assert_eq!(fn_namespace_for("ninechars"), FILE_NAME_NAMESPACE_POSIX);
        assert_eq!(
            fn_namespace_for("verylongname.txt"),
            FILE_NAME_NAMESPACE_POSIX
        );
    }

    #[test]
    fn fn_namespace_for_extension_over_three_chars_picks_posix() {
        assert_eq!(fn_namespace_for("README.MARK"), FILE_NAME_NAMESPACE_POSIX);
    }

    #[test]
    fn fn_namespace_for_multi_dot_picks_posix() {
        // "a.tar.gz" — extension contains a dot, picked POSIX.
        assert_eq!(fn_namespace_for("a.tar.gz"), FILE_NAME_NAMESPACE_POSIX);
    }

    // --- EA attribute builders ---------------------------------------------

    #[test]
    fn build_resident_ea_information_attribute_rejects_non_eight_byte_value() {
        let err = build_resident_ea_information_attribute(0, &[0u8; 7]).unwrap_err();
        assert!(err.contains("8 bytes"), "{err}");
    }

    #[test]
    fn build_resident_ea_information_attribute_layout() {
        let val = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let buf = build_resident_ea_information_attribute(7, &val).unwrap();
        // Type = 0xD0, length is align8(24+8)=32, attr_id at +14, value at +24.
        let typ = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        let aid = u16::from_le_bytes(buf[14..16].try_into().unwrap());
        assert_eq!(typ, 0xD0);
        assert_eq!(len, 32);
        assert_eq!(aid, 7);
        assert_eq!(&buf[24..32], &val);
    }

    #[test]
    fn build_resident_ea_attribute_uses_type_0xe0() {
        let packed = b"NAME\x00VALUE";
        let buf = build_resident_ea_attribute(2, packed).unwrap();
        let typ = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(typ, 0xE0);
    }

    // --- reparse-point builder ---------------------------------------------

    #[test]
    fn build_resident_reparse_point_attribute_writes_tag_and_data() {
        let data = b"hello-target";
        let buf = build_resident_reparse_point_attribute(3, reparse_tag::SYMLINK, data).unwrap();
        // Header type
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 0xC0);
        // Resident value lives at offset 24. First 4 bytes = reparse_tag.
        let tag = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        assert_eq!(tag, reparse_tag::SYMLINK);
        let data_len = u16::from_le_bytes(buf[28..30].try_into().unwrap()) as usize;
        assert_eq!(data_len, data.len());
        assert_eq!(&buf[32..32 + data.len()], data);
    }

    // --- symlink reparse data builder --------------------------------------

    #[test]
    fn build_symlink_reparse_data_relative_sets_flag_bit() {
        let buf = build_symlink_reparse_data("foo", None, true);
        let flags = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(flags & 0x1, 0x1);
    }

    #[test]
    fn build_symlink_reparse_data_absolute_clears_flag_bit() {
        let buf = build_symlink_reparse_data("foo", None, false);
        let flags = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(flags & 0x1, 0);
    }

    #[test]
    fn build_symlink_reparse_data_layout_substitute_then_print_name() {
        let buf = build_symlink_reparse_data("sub", Some("show"), false);
        let sub_off = u16::from_le_bytes(buf[0..2].try_into().unwrap());
        let sub_len = u16::from_le_bytes(buf[2..4].try_into().unwrap());
        let print_off = u16::from_le_bytes(buf[4..6].try_into().unwrap());
        let print_len = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        // 12-byte header then PathBuffer.
        assert_eq!(sub_off, 0);
        assert_eq!(sub_len, 6); // "sub" UTF-16 = 6 bytes
        assert_eq!(print_off, 6);
        assert_eq!(print_len, 8); // "show" UTF-16 = 8 bytes
                                  // Substitute first in PathBuffer.
        let sub = [
            u16::from_le_bytes([buf[12], buf[13]]),
            u16::from_le_bytes([buf[14], buf[15]]),
            u16::from_le_bytes([buf[16], buf[17]]),
        ];
        assert_eq!(String::from_utf16(&sub).unwrap(), "sub");
    }

    // --- named-stream resident $DATA ---------------------------------------

    #[test]
    fn build_named_resident_data_attribute_rejects_empty_name() {
        let err = build_named_resident_data_attribute(0, "", b"x").unwrap_err();
        assert!(err.contains("invalid stream name length"), "{err}");
    }

    #[test]
    fn build_named_resident_data_attribute_round_trip_via_attr_iter() {
        // Build inside an MFT record and ask attr_io to parse it back.
        use crate::attr_io::{find_attribute, iter_attributes, AttrType};
        let attr = build_named_resident_data_attribute(4, "stream", b"hi").unwrap();
        let mut rec = vec![0u8; 1024];
        let attrs_offset: u16 = 0x38;
        rec[0x14..0x16].copy_from_slice(&attrs_offset.to_le_bytes());
        let start = attrs_offset as usize;
        rec[start..start + attr.len()].copy_from_slice(&attr);
        rec[start + attr.len()..start + attr.len() + 4]
            .copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        rec[0x18..0x1C].copy_from_slice(&((start + attr.len() + 4) as u32).to_le_bytes());

        let attrs: Vec<_> = iter_attributes(&rec).collect();
        assert_eq!(attrs.len(), 1);
        let found = find_attribute(&rec, AttrType::Data, Some("stream")).expect("found");
        assert_eq!(found.name_length, 6);
        assert!(found.is_resident);
    }

    // --- $FILE_NAME builder ------------------------------------------------

    /// Regression: indexed_flag (offset +0x16 inside attribute header,
    /// = byte 22 from attr start) must be 1 — see
    /// `build_file_name_attribute`'s comment. Without it, chkdsk reports
    /// "Attribute record (30, "") from file record segment N is corrupt".
    #[test]
    fn build_file_name_attribute_sets_indexed_flag_to_one() {
        let buf =
            build_file_name_attribute(5, 0x12_3456_7890, "name.txt", 1_000_000, false).unwrap();
        assert_eq!(
            buf[22], 1,
            "indexed_flag must be 1; see chkdsk regression comment"
        );
    }

    #[test]
    fn build_file_name_attribute_writes_parent_reference_and_namespace() {
        let parent = 0x55_AABB_CCDDu64;
        let buf = build_file_name_attribute(5, parent, "longname.text", 0, false).unwrap();
        // Value starts at header_size=24. parent_ref is first 8 bytes.
        let got_parent = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        assert_eq!(got_parent, parent);
        // Namespace at offset 24+65 = 89.
        assert_eq!(buf[89], FILE_NAME_NAMESPACE_POSIX);
    }

    #[test]
    fn build_file_name_attribute_rejects_empty_and_oversize_names() {
        assert!(build_file_name_attribute(0, 0, "", 0, false).is_err());
        let huge: String = "a".repeat(256);
        assert!(build_file_name_attribute(0, 0, &huge, 0, false).is_err());
    }

    // --- regular file record builder smoke test ----------------------------

    #[test]
    fn build_regular_file_record_produces_well_formed_record() {
        let rec = build_regular_file_record(
            1024,
            /* record_number */ 100,
            /* sequence */ 1,
            encode_file_reference(5, 1),
            "test.txt",
            nt_time_now(),
            512,
        )
        .unwrap();
        assert_eq!(rec.len(), 1024);
        assert_eq!(&rec[0..4], b"FILE");
        // flags: IN_USE = 0x0001.
        let flags = u16::from_le_bytes([rec[0x16], rec[0x17]]);
        assert!(flags & 1 != 0);
        // bytes_used > attrs_offset and < record_size.
        let bu = u32::from_le_bytes(rec[0x18..0x1C].try_into().unwrap()) as usize;
        let ao = u16::from_le_bytes(rec[0x14..0x16].try_into().unwrap()) as usize;
        assert!(bu > ao);
        assert!(bu < rec.len());
    }
}
