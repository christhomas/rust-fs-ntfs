//! Resize a resident attribute's value within its MFT record. Shifts
//! subsequent attributes so the end-of-attributes sentinel stays in
//! place, updates the attribute header `length` + `value_length` fields,
//! and updates the record header's `bytes_used`.
//!
//! Operates on a post-fixup `record: &mut [u8]`. Call from inside an
//! [`mft_io::update_mft_record`](crate::mft_io::update_mft_record)
//! mutator so fixup is re-applied and the record is fsync'd atomically.
//!
//! References (no GPL code consulted): FILE_RECORD_SEGMENT_HEADER
//! and NTFS attribute-header layout per Windows Internals 7th ed.
//! ch. "NTFS On-Disk Structure" and MS-FSCC.

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
    if new_attr_length == 0 || !new_attr_length.is_multiple_of(8) {
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
pub fn insert_attribute_sorted(record: &mut [u8], new_attr: &[u8]) -> Result<(), String> {
    let new_len = new_attr.len();
    if new_len == 0 || !new_len.is_multiple_of(8) {
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

    // Find the sorted insertion offset. NTFS requires the attributes in a
    // FILE record to be ordered by type_code (chkdsk flags "attribute
    // records ... are unsorted" otherwise). The new attribute goes just
    // before the first existing attribute whose type_code is strictly
    // greater than its own; equal-type attributes keep their relative
    // order (the new one is appended after them). If nothing has a greater
    // type the insertion point is the end-of-attributes marker — so the
    // highest-type attributes (e.g. $DATA, $REPARSE_POINT) still land at
    // the end as before, while a $FILE_NAME (0x30) or $OBJECT_ID (0x40) is
    // placed ahead of $DATA (0x80) instead of after it.
    let new_type = u32::from_le_bytes([new_attr[0], new_attr[1], new_attr[2], new_attr[3]]);
    let attrs_offset = u16::from_le_bytes([record[0x14], record[0x15]]) as usize;
    let mut end_marker_pos: Option<usize> = None;
    let mut insert_pos: Option<usize> = None;
    let scan_end = bytes_used.min(record.len().saturating_sub(4));
    let mut cursor = attrs_offset;
    while cursor + 4 <= scan_end {
        let type_code = u32::from_le_bytes([
            record[cursor],
            record[cursor + 1],
            record[cursor + 2],
            record[cursor + 3],
        ]);
        if type_code == 0xFFFF_FFFF {
            end_marker_pos = Some(cursor);
            break;
        }
        if type_code == 0 {
            break; // hit zero padding before finding marker
        }
        if insert_pos.is_none() && type_code > new_type {
            insert_pos = Some(cursor);
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
    let insert_pos = insert_pos.unwrap_or(end_marker_pos);

    // Open a gap at insert_pos by shifting everything from there up to and
    // including the end marker (i.e. up to bytes_used) forward by new_len.
    record.copy_within(insert_pos..bytes_used, insert_pos + new_len);
    // Zero the gap (defensive; overwritten below by the attribute bytes).
    for byte in &mut record[insert_pos..insert_pos + new_len] {
        *byte = 0;
    }
    record[insert_pos..insert_pos + new_len].copy_from_slice(new_attr);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr_io::attr_off;

    const ATTRS_OFF: usize = 0x38;
    const ALLOC: usize = 4096;

    /// Build a minimal MFT record with one resident attribute of the given value.
    /// Returns (record, attr_offset).
    fn one_attr_record(value: &[u8]) -> (Vec<u8>, usize) {
        let header_size = 24usize; // resident attr header through value_offset fields
        let value_offset: u16 = header_size as u16;
        let attr_len = ((header_size + value.len()) + 7) & !7;
        let end_marker_pos = ATTRS_OFF + attr_len;
        // Round bytes_used up to the 8-byte boundary, matching the real record
        // layout (the end marker is followed by 0..7 zero-pad bytes). This
        // exercises insert_attribute_sorted's padded-marker scan.
        let bytes_used = align_up_8(end_marker_pos + 4);

        let mut rec = vec![0u8; ALLOC];
        // Record header fields
        rec[0x14..0x16].copy_from_slice(&(ATTRS_OFF as u16).to_le_bytes()); // attrs_offset
        rec[0x18..0x1C].copy_from_slice(&(bytes_used as u32).to_le_bytes()); // bytes_used
        rec[0x1C..0x20].copy_from_slice(&(ALLOC as u32).to_le_bytes()); // bytes_allocated

        let a = ATTRS_OFF;
        rec[a..a + 4].copy_from_slice(&0x80u32.to_le_bytes()); // type: $DATA
        rec[a + attr_off::LENGTH..a + attr_off::LENGTH + 4]
            .copy_from_slice(&(attr_len as u32).to_le_bytes());
        rec[a + attr_off::NON_RESIDENT] = 0;
        rec[a + attr_off::RESIDENT_VALUE_LENGTH..a + attr_off::RESIDENT_VALUE_LENGTH + 4]
            .copy_from_slice(&(value.len() as u32).to_le_bytes());
        rec[a + attr_off::RESIDENT_VALUE_OFFSET..a + attr_off::RESIDENT_VALUE_OFFSET + 2]
            .copy_from_slice(&value_offset.to_le_bytes());
        rec[a + header_size..a + header_size + value.len()].copy_from_slice(value);

        // End marker
        rec[end_marker_pos..end_marker_pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

        (rec, ATTRS_OFF)
    }

    // --- align_up_8 ---

    #[test]
    fn align_up_8_already_aligned() {
        assert_eq!(align_up_8(0), 0);
        assert_eq!(align_up_8(8), 8);
        assert_eq!(align_up_8(16), 16);
        assert_eq!(align_up_8(1024), 1024);
    }

    #[test]
    fn align_up_8_rounds_up() {
        assert_eq!(align_up_8(1), 8);
        assert_eq!(align_up_8(7), 8);
        assert_eq!(align_up_8(9), 16);
        assert_eq!(align_up_8(15), 16);
        assert_eq!(align_up_8(17), 24);
    }

    // --- allocate_attribute_id ---

    #[test]
    fn allocate_attribute_id_returns_current_and_increments() {
        let mut rec = vec![0u8; 64];
        rec[0x28..0x2A].copy_from_slice(&5u16.to_le_bytes());
        let id = allocate_attribute_id(&mut rec);
        assert_eq!(id, 5);
        let next = u16::from_le_bytes([rec[0x28], rec[0x29]]);
        assert_eq!(next, 6);
    }

    #[test]
    fn allocate_attribute_id_starts_at_zero() {
        let mut rec = vec![0u8; 64];
        let id = allocate_attribute_id(&mut rec);
        assert_eq!(id, 0);
        assert_eq!(u16::from_le_bytes([rec[0x28], rec[0x29]]), 1);
    }

    #[test]
    fn allocate_attribute_id_wraps_around() {
        let mut rec = vec![0u8; 64];
        rec[0x28..0x2A].copy_from_slice(&u16::MAX.to_le_bytes());
        let id = allocate_attribute_id(&mut rec);
        assert_eq!(id, u16::MAX);
        assert_eq!(u16::from_le_bytes([rec[0x28], rec[0x29]]), 0);
    }

    // --- resize_resident_value ---

    #[test]
    fn resize_same_size_succeeds_without_shifting() {
        let (mut rec, attr_off) = one_attr_record(b"hello");
        let bytes_used_before = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        resize_resident_value(&mut rec, attr_off, 5).unwrap();
        let bytes_used_after = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        assert_eq!(bytes_used_before, bytes_used_after);
    }

    #[test]
    fn resize_grow_updates_bytes_used() {
        let (mut rec, attr_off) = one_attr_record(b"hi");
        let bu_before = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        resize_resident_value(&mut rec, attr_off, 10).unwrap();
        let bu_after = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        assert!(bu_after > bu_before);
    }

    #[test]
    fn resize_shrink_updates_bytes_used() {
        let (mut rec, attr_off) = one_attr_record(&[0u8; 16]);
        let bu_before = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        resize_resident_value(&mut rec, attr_off, 4).unwrap();
        let bu_after = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        assert!(bu_after < bu_before);
    }

    #[test]
    fn resize_nonresident_fails() {
        let (mut rec, attr_off) = one_attr_record(b"x");
        rec[attr_off + attr_off::NON_RESIDENT] = 1; // mark as non-resident
        assert!(resize_resident_value(&mut rec, attr_off, 10).is_err());
    }

    #[test]
    fn resize_exceeds_capacity_fails() {
        let (mut rec, attr_off) = one_attr_record(b"x");
        // Try to grow to more than bytes_allocated allows
        assert!(resize_resident_value(&mut rec, attr_off, ALLOC as u32).is_err());
    }

    // --- set_resident_value ---

    #[test]
    fn set_resident_value_writes_bytes() {
        let (mut rec, attr_off) = one_attr_record(&[0u8; 8]);
        set_resident_value(&mut rec, attr_off, b"newdata!").unwrap();
        let val_off = u16::from_le_bytes([
            rec[attr_off + attr_off::RESIDENT_VALUE_OFFSET],
            rec[attr_off + attr_off::RESIDENT_VALUE_OFFSET + 1],
        ]) as usize;
        assert_eq!(
            &rec[attr_off + val_off..attr_off + val_off + 8],
            b"newdata!"
        );
    }

    // --- insert_attribute_sorted ---

    #[test]
    fn insert_attribute_sorted_adds_attribute() {
        let (mut rec, _) = one_attr_record(b"first");
        let bu_before = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);

        // Build a minimal new attribute (24-byte header + 8 bytes value = 32 bytes)
        let mut new_attr = vec![0u8; 32];
        new_attr[0..4].copy_from_slice(&0x10u32.to_le_bytes()); // type: $STANDARD_INFO
        new_attr[4..8].copy_from_slice(&32u32.to_le_bytes()); // length = 32

        insert_attribute_sorted(&mut rec, &new_attr).unwrap();
        let bu_after = u32::from_le_bytes([rec[0x18], rec[0x19], rec[0x1A], rec[0x1B]]);
        assert_eq!(bu_after, bu_before + 32);
    }

    #[test]
    fn insert_attribute_bad_alignment_fails() {
        let (mut rec, _) = one_attr_record(b"x");
        let bad_attr = vec![0u8; 7]; // not 8-aligned
        assert!(insert_attribute_sorted(&mut rec, &bad_attr).is_err());
    }
}
