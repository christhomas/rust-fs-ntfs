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
//! References (no GPL code consulted):
//! * [Flatcap File Record](https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html)
//! * [Flatcap $STANDARD_INFORMATION](https://flatcap.github.io/linux-ntfs/ntfs/attributes/standard_information.html)
//! * [Flatcap $FILE_NAME](https://flatcap.github.io/linux-ntfs/ntfs/attributes/file_name.html)
//! * [Flatcap $DATA](https://flatcap.github.io/linux-ntfs/ntfs/attributes/data.html)

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
const ATTRS_OFFSET: usize = 0x38;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END_MARKER: u32 = 0xFFFF_FFFF;

/// Namespace for a synthesized $FILE_NAME. Value 3 = "Win32 + DOS" (most
/// compatible — Windows treats one entry as both the Win32 and DOS name).
const FILE_NAME_NAMESPACE_WIN32_AND_DOS: u8 = 3;

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

/// Build an MFT record for a directory (with empty $INDEX_ROOT:$I30).
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
    let mut rec = build_record_inner(
        record_size,
        record_number,
        sequence,
        parent_reference,
        name,
        nt_time,
        bytes_per_sector,
        /* is_dir */ true,
    )?;
    // Replace the empty $DATA emitted by build_record_inner with
    // $INDEX_ROOT($I30) + end marker.
    let _ = index_block_size;
    // For MVP, we don't emit $INDEX_ROOT here — mkdir is not yet
    // implemented, keep this shell.
    rec.clear();
    Err("build_directory_record: mkdir not yet implemented".to_string())
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
    if record_size < 512 || record_size % bytes_per_sector as usize != 0 {
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
    rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
        .copy_from_slice(&(ATTRS_OFFSET as u16).to_le_bytes());
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
    let mut cursor = ATTRS_OFFSET;

    cursor = write_standard_information(&mut rec, cursor, 0, nt_time, is_dir);
    cursor = write_file_name(
        &mut rec,
        cursor,
        1,
        parent_reference,
        &utf16,
        nt_time,
        is_dir,
    );
    cursor = write_empty_data(&mut rec, cursor, 2);

    // End marker.
    rec[cursor..cursor + 4].copy_from_slice(&ATTR_END_MARKER.to_le_bytes());
    cursor += 4;
    let bytes_used = cursor as u32;
    rec[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&bytes_used.to_le_bytes());

    Ok(rec)
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
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
    let fa: u32 = if is_dir { 0x10000000 | 0x20 } else { 0x20 }; // FILE_ATTRIBUTE_ARCHIVE
    rec[v + 32..v + 36].copy_from_slice(&fa.to_le_bytes()); // file_attributes
                                                            // rest (max_versions .. usn) stays 0
    at + attr_length
}

fn write_file_name(
    rec: &mut [u8],
    at: usize,
    attr_id: u16,
    parent_reference: u64,
    name_utf16: &[u16],
    nt_time: u64,
    is_dir: bool,
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
    rec[at + 22] = 0;
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
    let fa: u32 = if is_dir { 0x10000000 | 0x20 } else { 0x20 };
    rec[v + 56..v + 60].copy_from_slice(&fa.to_le_bytes()); // file_attributes
    rec[v + 60..v + 64].copy_from_slice(&0u32.to_le_bytes()); // ea/reparse
    rec[v + 64] = name_utf16.len() as u8; // name_length
    rec[v + 65] = FILE_NAME_NAMESPACE_WIN32_AND_DOS; // namespace
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
