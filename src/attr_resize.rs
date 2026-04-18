//! Resize a resident attribute's value within its MFT record. Shifts
//! subsequent attributes so the end-of-attributes sentinel stays in
//! place, updates the attribute header `length` + `value_length` fields,
//! and updates the record header's `bytes_used`.
//!
//! Operates on a post-fixup `record: &mut [u8]`. Call from inside an
//! [`mft_io::update_mft_record`](crate::mft_io::update_mft_record)
//! mutator so fixup is re-applied and the record is fsync'd atomically.
//!
//! References (no GPL code consulted):
//! * [Flatcap File Record](https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html)
//! * [Flatcap Attribute Header](https://flatcap.github.io/linux-ntfs/ntfs/concepts/attribute_header.html)
//! * MS-FSCC

use crate::attr_io::attr_off;

/// File-record header offsets (subset).
const REC_OFF_BYTES_USED: usize = 0x18;
const REC_OFF_BYTES_ALLOCATED: usize = 0x1C;

/// Round up to the next multiple of 8. NTFS requires every attribute's
/// `length` field to be 8-byte aligned.
fn align_up_8(n: usize) -> usize {
    (n + 7) & !7
}

/// Resize a resident attribute so its value becomes `new_value_length`
/// bytes. Does not touch the value's contents — caller writes those
/// after (via `write_resident_value_bytes` or by hand).
///
/// Fails if:
/// * the attribute is non-resident
/// * the new size requires more room than the record has
/// * the record layout is malformed (negative sentinel position etc.)
pub fn resize_resident_value(
    record: &mut [u8],
    attr_offset: usize,
    new_value_length: u32,
) -> Result<(), String> {
    if record[attr_offset + attr_off::NON_RESIDENT] != 0 {
        return Err("attribute is non-resident".to_string());
    }
    let old_attr_length = u32::from_le_bytes([
        record[attr_offset + attr_off::LENGTH],
        record[attr_offset + attr_off::LENGTH + 1],
        record[attr_offset + attr_off::LENGTH + 2],
        record[attr_offset + attr_off::LENGTH + 3],
    ]) as usize;
    let value_offset = u16::from_le_bytes([
        record[attr_offset + attr_off::RESIDENT_VALUE_OFFSET],
        record[attr_offset + attr_off::RESIDENT_VALUE_OFFSET + 1],
    ]) as usize;

    let new_attr_length = align_up_8(value_offset + new_value_length as usize);
    if new_attr_length == old_attr_length {
        // Same size. Just write the new length field (already the same).
        record[attr_offset + attr_off::RESIDENT_VALUE_LENGTH
            ..attr_offset + attr_off::RESIDENT_VALUE_LENGTH + 4]
            .copy_from_slice(&new_value_length.to_le_bytes());
        return Ok(());
    }

    let bytes_used = u32::from_le_bytes([
        record[REC_OFF_BYTES_USED],
        record[REC_OFF_BYTES_USED + 1],
        record[REC_OFF_BYTES_USED + 2],
        record[REC_OFF_BYTES_USED + 3],
    ]) as usize;
    let bytes_allocated = u32::from_le_bytes([
        record[REC_OFF_BYTES_ALLOCATED],
        record[REC_OFF_BYTES_ALLOCATED + 1],
        record[REC_OFF_BYTES_ALLOCATED + 2],
        record[REC_OFF_BYTES_ALLOCATED + 3],
    ]) as usize;

    if new_attr_length > old_attr_length {
        let diff = new_attr_length - old_attr_length;
        if bytes_used + diff > bytes_allocated {
            return Err(format!(
                "growing attribute by {diff} bytes exceeds record capacity \
                 (bytes_used={bytes_used} + diff > bytes_allocated={bytes_allocated})"
            ));
        }
        // Shift [attr_offset + old_attr_length .. bytes_used) forward by `diff`.
        record.copy_within(
            attr_offset + old_attr_length..bytes_used,
            attr_offset + old_attr_length + diff,
        );
        // Zero the new-value region (caller will overwrite useful bytes).
        for byte in &mut record[attr_offset + old_attr_length..attr_offset + new_attr_length] {
            *byte = 0;
        }
        // Update bytes_used.
        let new_bytes_used = (bytes_used + diff) as u32;
        record[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4]
            .copy_from_slice(&new_bytes_used.to_le_bytes());
    } else {
        let diff = old_attr_length - new_attr_length;
        // Shift [attr_offset + old_attr_length .. bytes_used) back by `diff`.
        record.copy_within(
            attr_offset + old_attr_length..bytes_used,
            attr_offset + old_attr_length - diff,
        );
        // Zero the bytes at the tail that are no longer used.
        for byte in &mut record[bytes_used - diff..bytes_used] {
            *byte = 0;
        }
        let new_bytes_used = (bytes_used - diff) as u32;
        record[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4]
            .copy_from_slice(&new_bytes_used.to_le_bytes());
    }

    // Rewrite attr header length + value_length.
    let new_attr_length_u32 = new_attr_length as u32;
    record[attr_offset + attr_off::LENGTH..attr_offset + attr_off::LENGTH + 4]
        .copy_from_slice(&new_attr_length_u32.to_le_bytes());
    record[attr_offset + attr_off::RESIDENT_VALUE_LENGTH
        ..attr_offset + attr_off::RESIDENT_VALUE_LENGTH + 4]
        .copy_from_slice(&new_value_length.to_le_bytes());

    Ok(())
}

/// Replace the entire attribute at `attr_offset` with the bytes in
/// `new_attr`. The caller is responsible for providing a correctly-
/// formed attribute whose `length` field matches `new_attr.len()` (and
/// that length is 8-byte aligned).
///
/// Used for resident↔non-resident promotion, where the new attribute
/// has a different layout than the old.
pub fn replace_attribute(
    record: &mut [u8],
    attr_offset: usize,
    new_attr: &[u8],
) -> Result<(), String> {
    let new_attr_length = new_attr.len();
    if new_attr_length == 0 || new_attr_length % 8 != 0 {
        return Err(format!(
            "replace_attribute: new_attr length {new_attr_length} not 8-aligned non-zero"
        ));
    }
    // Sanity: the header's own `length` field must match new_attr.len().
    let header_len =
        u32::from_le_bytes([new_attr[4], new_attr[5], new_attr[6], new_attr[7]]) as usize;
    if header_len != new_attr_length {
        return Err(format!(
            "replace_attribute: header length {header_len} != buffer length {new_attr_length}"
        ));
    }

    let old_attr_length = u32::from_le_bytes([
        record[attr_offset + attr_off::LENGTH],
        record[attr_offset + attr_off::LENGTH + 1],
        record[attr_offset + attr_off::LENGTH + 2],
        record[attr_offset + attr_off::LENGTH + 3],
    ]) as usize;
    let bytes_used = u32::from_le_bytes([
        record[REC_OFF_BYTES_USED],
        record[REC_OFF_BYTES_USED + 1],
        record[REC_OFF_BYTES_USED + 2],
        record[REC_OFF_BYTES_USED + 3],
    ]) as usize;
    let bytes_allocated = u32::from_le_bytes([
        record[REC_OFF_BYTES_ALLOCATED],
        record[REC_OFF_BYTES_ALLOCATED + 1],
        record[REC_OFF_BYTES_ALLOCATED + 2],
        record[REC_OFF_BYTES_ALLOCATED + 3],
    ]) as usize;

    if new_attr_length > old_attr_length {
        let diff = new_attr_length - old_attr_length;
        if bytes_used + diff > bytes_allocated {
            return Err(format!(
                "replace_attribute: growing by {diff} exceeds record capacity \
                 (bytes_used={bytes_used} bytes_allocated={bytes_allocated})"
            ));
        }
        record.copy_within(
            attr_offset + old_attr_length..bytes_used,
            attr_offset + old_attr_length + diff,
        );
        let new_bu = (bytes_used + diff) as u32;
        record[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&new_bu.to_le_bytes());
    } else if new_attr_length < old_attr_length {
        let diff = old_attr_length - new_attr_length;
        record.copy_within(
            attr_offset + old_attr_length..bytes_used,
            attr_offset + old_attr_length - diff,
        );
        for byte in &mut record[bytes_used - diff..bytes_used] {
            *byte = 0;
        }
        let new_bu = (bytes_used - diff) as u32;
        record[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&new_bu.to_le_bytes());
    }

    // Overwrite the attribute with the new bytes.
    record[attr_offset..attr_offset + new_attr_length].copy_from_slice(new_attr);
    Ok(())
}

/// Insert a new attribute into the MFT record immediately before the
/// end-of-attributes sentinel (`0xFFFFFFFF`). The caller supplies a
/// fully-formed, 8-byte-aligned attribute blob — including its
/// attribute-header `length` field set to the buffer's length.
pub fn insert_attribute_before_end(record: &mut [u8], new_attr: &[u8]) -> Result<(), String> {
    let new_len = new_attr.len();
    if new_len == 0 || new_len % 8 != 0 {
        return Err(format!(
            "insert_attribute: length {new_len} not 8-aligned non-zero"
        ));
    }
    let header_len =
        u32::from_le_bytes([new_attr[4], new_attr[5], new_attr[6], new_attr[7]]) as usize;
    if header_len != new_len {
        return Err(format!(
            "insert_attribute: header length {header_len} != buffer length {new_len}"
        ));
    }

    let bytes_used = u32::from_le_bytes([
        record[REC_OFF_BYTES_USED],
        record[REC_OFF_BYTES_USED + 1],
        record[REC_OFF_BYTES_USED + 2],
        record[REC_OFF_BYTES_USED + 3],
    ]) as usize;
    let bytes_allocated = u32::from_le_bytes([
        record[REC_OFF_BYTES_ALLOCATED],
        record[REC_OFF_BYTES_ALLOCATED + 1],
        record[REC_OFF_BYTES_ALLOCATED + 2],
        record[REC_OFF_BYTES_ALLOCATED + 3],
    ]) as usize;
    if bytes_used + new_len > bytes_allocated {
        return Err(format!(
            "no room for new attribute: need {new_len} more, have {}",
            bytes_allocated - bytes_used
        ));
    }

    // Scan for the end-of-attributes marker. It lives at or before
    // bytes_used — `bytes_used` is 8-byte aligned so there may be 0–7
    // bytes of trailing zero padding after the 0xFFFFFFFF sentinel.
    let attrs_offset = u16::from_le_bytes([record[0x14], record[0x15]]) as usize;
    let mut end_marker_pos: Option<usize> = None;
    let scan_end = bytes_used.min(record.len().saturating_sub(4));
    let mut cursor = attrs_offset;
    while cursor + 4 <= scan_end {
        let marker = u32::from_le_bytes([
            record[cursor],
            record[cursor + 1],
            record[cursor + 2],
            record[cursor + 3],
        ]);
        if marker == 0xFFFF_FFFF {
            end_marker_pos = Some(cursor);
            break;
        }
        if marker == 0 {
            break; // hit zero padding before finding marker
        }
        // skip this attribute via its `length` field.
        let attr_len = u32::from_le_bytes([
            record[cursor + 4],
            record[cursor + 5],
            record[cursor + 6],
            record[cursor + 7],
        ]) as usize;
        if attr_len == 0 || cursor + attr_len > scan_end {
            break;
        }
        cursor += attr_len;
    }
    let end_marker_pos = end_marker_pos
        .ok_or_else(|| format!("no 0xFFFFFFFF end marker found before bytes_used {bytes_used}"))?;

    // Shift the end marker forward by new_len.
    record.copy_within(end_marker_pos..end_marker_pos + 4, end_marker_pos + new_len);
    // Zero the area we're about to fill (defensive; overwritten below).
    for byte in &mut record[end_marker_pos..end_marker_pos + new_len] {
        *byte = 0;
    }
    // Copy the new attribute.
    record[end_marker_pos..end_marker_pos + new_len].copy_from_slice(new_attr);

    // Update bytes_used += new_len.
    let new_bu = (bytes_used + new_len) as u32;
    record[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&new_bu.to_le_bytes());

    Ok(())
}

/// Allocate a new attribute_id by bumping the record header's
/// next_attr_id field (+0x28, u16 LE). Returns the allocated id.
pub fn allocate_attribute_id(record: &mut [u8]) -> u16 {
    let off = 0x28;
    let cur = u16::from_le_bytes([record[off], record[off + 1]]);
    let next = cur.wrapping_add(1);
    record[off..off + 2].copy_from_slice(&next.to_le_bytes());
    cur
}

/// Convenience: resize then copy `new_value` into the attribute's value
/// region. Equivalent to `resize_resident_value(record, off,
/// new_value.len()) + memcpy`.
pub fn set_resident_value(
    record: &mut [u8],
    attr_offset: usize,
    new_value: &[u8],
) -> Result<(), String> {
    resize_resident_value(record, attr_offset, new_value.len() as u32)?;
    let value_offset = u16::from_le_bytes([
        record[attr_offset + attr_off::RESIDENT_VALUE_OFFSET],
        record[attr_offset + attr_off::RESIDENT_VALUE_OFFSET + 1],
    ]) as usize;
    let dst_start = attr_offset + value_offset;
    record[dst_start..dst_start + new_value.len()].copy_from_slice(new_value);
    Ok(())
}
