//! Locate attributes within a clean (post-fixup) MFT record buffer.
//!
//! Stays in the Rust domain so every W1+ write path can work on a buffer
//! handed to it by `mft_io::update_mft_record` without going back
//! through upstream parsers. Upstream is used for path → record-number
//! resolution; once inside the RMW callback, we walk attributes here.
//!
//! References (no GPL code consulted): NTFS attribute-header layout
//! and FILE_RECORD_SEGMENT_HEADER per Windows Internals 7th ed.
//! ch. "NTFS On-Disk Structure" and MS-FSCC.

/// NTFS attribute type codes we care about. Values match upstream's
/// `NtfsAttributeType` and MS-FSCC.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrType {
    StandardInformation = 0x10,
    AttributeList = 0x20,
    FileName = 0x30,
    ObjectId = 0x40,
    SecurityDescriptor = 0x50,
    VolumeName = 0x60,
    VolumeInformation = 0x70,
    Data = 0x80,
    IndexRoot = 0x90,
    IndexAllocation = 0xA0,
    Bitmap = 0xB0,
    ReparsePoint = 0xC0,
    ExtendedAttributeInformation = 0xD0,
    ExtendedAttribute = 0xE0,
}

impl AttrType {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0x10 => Some(Self::StandardInformation),
            0x20 => Some(Self::AttributeList),
            0x30 => Some(Self::FileName),
            0x40 => Some(Self::ObjectId),
            0x50 => Some(Self::SecurityDescriptor),
            0x60 => Some(Self::VolumeName),
            0x70 => Some(Self::VolumeInformation),
            0x80 => Some(Self::Data),
            0x90 => Some(Self::IndexRoot),
            0xA0 => Some(Self::IndexAllocation),
            0xB0 => Some(Self::Bitmap),
            0xC0 => Some(Self::ReparsePoint),
            0xD0 => Some(Self::ExtendedAttributeInformation),
            0xE0 => Some(Self::ExtendedAttribute),
            _ => None,
        }
    }
}

/// Where an attribute lives within an MFT record. Offsets are relative
/// to the start of the record buffer passed to the walker.
#[derive(Debug, Clone, Copy)]
pub struct AttrLocation {
    pub type_code: u32,
    pub attr_offset: usize,
    pub attr_length: usize,
    pub is_resident: bool,
    pub name_length: u8,
    pub name_offset: u16,
    pub attribute_id: u16,
    /// For resident attributes: offset of the value from the attribute
    /// start. Absolute value offset within the record is
    /// `attr_offset + resident_value_offset`.
    pub resident_value_offset: Option<u16>,
    pub resident_value_length: Option<u32>,
    /// For non-resident attributes: size of the attribute's logical data.
    pub non_resident_value_length: Option<u64>,
    /// Offset of the mapping-pairs (data run list) from attribute start,
    /// for non-resident attributes.
    pub non_resident_mapping_pairs_offset: Option<u16>,
}

/// End-of-attributes sentinel per NTFS spec.
const END_MARKER: u32 = 0xFFFF_FFFF;

/// Iterate the attributes in `record` (post-fixup). Yields an
/// [`AttrLocation`] per attribute until the end marker or the record
/// `bytes_used` boundary (whichever comes first).
pub fn iter_attributes(record: &[u8]) -> impl Iterator<Item = AttrLocation> + '_ {
    AttrIter::new(record)
}

/// Find the first attribute of the given type. `name` is optional; if
/// `Some`, only matches attributes whose name (UTF-16 LE) equals the
/// provided string.
pub fn find_attribute(
    record: &[u8],
    type_code: AttrType,
    name: Option<&str>,
) -> Option<AttrLocation> {
    iter_attributes(record).find(|loc| {
        if loc.type_code != type_code as u32 {
            return false;
        }
        match name {
            None => loc.name_length == 0,
            Some(want) => attr_name_equals(record, loc, want),
        }
    })
}

/// Compare an attribute's name field (UTF-16 LE) to a Rust `&str`.
/// Returns `true` iff the decoded UTF-16 matches `want`.
pub fn attr_name_equals(record: &[u8], loc: &AttrLocation, want: &str) -> bool {
    if loc.name_length == 0 {
        return want.is_empty();
    }
    let name_bytes_start = loc.attr_offset + loc.name_offset as usize;
    let name_bytes_len = loc.name_length as usize * 2;
    if name_bytes_start + name_bytes_len > record.len() {
        return false;
    }
    let slice = &record[name_bytes_start..name_bytes_start + name_bytes_len];
    let u16s: Vec<u16> = slice
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    match String::from_utf16(&u16s) {
        Ok(decoded) => decoded == want,
        Err(_) => false,
    }
}

/// A debug-oriented description of a single attribute on disk —
/// suitable for human inspection and diagnostics. Used by
/// [`describe_attributes`] / `fs_ntfs_describe_attributes` to dump
/// what's in a file's MFT record for $Reparse byte-diff research
/// and $ATTRIBUTE_LIST debugging.
///
/// This struct is the *narrative* shape of an attribute (its name +
/// dimensions); it's NOT a parser intermediate. Code that needs to
/// operate on the bytes should use [`AttrLocation`] from
/// [`iter_attributes`] instead.
#[derive(Debug, Clone)]
pub struct AttrDescription {
    pub type_code: u32,
    /// The well-known name for this attribute type ("$STANDARD_INFORMATION"
    /// / "$FILE_NAME" / "$DATA" / ...) when recognised, or `"?(0xNN)"`
    /// for unknown types.
    pub type_name: String,
    /// Stream name decoded from UTF-16 LE (empty for unnamed
    /// attributes), or `Err(decode_message)` if the bytes don't form
    /// valid UTF-16. Returning the lossy decode lets the caller see
    /// what was there even when the name is malformed.
    pub name: String,
    pub attribute_id: u16,
    /// Byte offset of the attribute header within the MFT record.
    pub attr_offset: usize,
    pub attr_length: usize,
    pub is_resident: bool,
    /// Resident attributes: the value length declared in the
    /// resident-form header. Non-resident: the `data_length` from the
    /// non-resident header.
    pub value_length: u64,
}

/// Human-readable name for a small set of well-known NTFS attribute
/// type codes per MS-FSCC §2.4.
fn attr_type_name(type_code: u32) -> String {
    match type_code {
        0x10 => "$STANDARD_INFORMATION".to_string(),
        0x20 => "$ATTRIBUTE_LIST".to_string(),
        0x30 => "$FILE_NAME".to_string(),
        0x40 => "$OBJECT_ID".to_string(),
        0x50 => "$SECURITY_DESCRIPTOR".to_string(),
        0x60 => "$VOLUME_NAME".to_string(),
        0x70 => "$VOLUME_INFORMATION".to_string(),
        0x80 => "$DATA".to_string(),
        0x90 => "$INDEX_ROOT".to_string(),
        0xA0 => "$INDEX_ALLOCATION".to_string(),
        0xB0 => "$BITMAP".to_string(),
        0xC0 => "$REPARSE_POINT".to_string(),
        0xD0 => "$EA_INFORMATION".to_string(),
        0xE0 => "$EA".to_string(),
        0x100 => "$LOGGED_UTILITY_STREAM".to_string(),
        other => format!("?(0x{other:x})"),
    }
}

/// Decode an attribute's UTF-16 LE name field to a Rust `String`,
/// returning lossy decoding rather than failing so the caller still
/// sees what's there.
pub fn decode_attr_name(record: &[u8], loc: &AttrLocation) -> String {
    if loc.name_length == 0 {
        return String::new();
    }
    let start = loc.attr_offset + loc.name_offset as usize;
    let nbytes = loc.name_length as usize * 2;
    if start + nbytes > record.len() {
        return format!("<out-of-record: off={start}, len={nbytes}>");
    }
    let u16s: Vec<u16> = record[start..start + nbytes]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u16s)
}

/// Build a list of [`AttrDescription`]s for every attribute in a
/// raw MFT record buffer. Useful for matching what reference
/// volumes ship vs. what our mkfs emits when chasing chkdsk
/// disagreements (see `feature/s4-extend-reparse` — the
/// $Reparse byte-diff investigation needs exactly this view of
/// "what attributes does the reference's rec 26 actually carry?").
///
/// Does NOT follow `$ATTRIBUTE_LIST` extension records — the caller
/// must call `describe_attributes` again on each extension record's
/// bytes. Today no code in this crate emits extension records, so
/// this is forward-compatible: the function reports the extension
/// record's presence via its $ATTRIBUTE_LIST entry, the caller
/// chases the file_reference + reads that record explicitly.
pub fn describe_attributes(record: &[u8]) -> Vec<AttrDescription> {
    iter_attributes(record)
        .map(|loc| AttrDescription {
            type_code: loc.type_code,
            type_name: attr_type_name(loc.type_code),
            name: decode_attr_name(record, &loc),
            attribute_id: loc.attribute_id,
            attr_offset: loc.attr_offset,
            attr_length: loc.attr_length,
            is_resident: loc.is_resident,
            value_length: if loc.is_resident {
                loc.resident_value_length.unwrap_or(0) as u64
            } else {
                loc.non_resident_value_length.unwrap_or(0)
            },
        })
        .collect()
}

/// Offsets within an attribute header. Named constants so the arithmetic
/// in this module doesn't depend on magic numbers elsewhere.
pub mod attr_off {
    pub const TYPE: usize = 0x00;
    pub const LENGTH: usize = 0x04;
    pub const NON_RESIDENT: usize = 0x08;
    pub const NAME_LENGTH: usize = 0x09;
    pub const NAME_OFFSET: usize = 0x0A;
    pub const FLAGS: usize = 0x0C;
    pub const ATTRIBUTE_ID: usize = 0x0E;
    // resident:
    pub const RESIDENT_VALUE_LENGTH: usize = 0x10;
    pub const RESIDENT_VALUE_OFFSET: usize = 0x14;
    // non-resident:
    pub const NONRES_FIRST_VCN: usize = 0x10;
    pub const NONRES_LAST_VCN: usize = 0x18;
    pub const NONRES_MAPPING_PAIRS_OFFSET: usize = 0x20;
    pub const NONRES_ALLOCATED_LENGTH: usize = 0x28;
    pub const NONRES_DATA_LENGTH: usize = 0x30;
    pub const NONRES_INITIALIZED_LENGTH: usize = 0x38;
}

// File-record header offsets we need.
const REC_OFF_ATTRS_OFFSET: usize = 0x14;
const REC_OFF_BYTES_USED: usize = 0x18;

/// Read a little-endian `u16` from `buf[offset..offset+2]`.
/// Returns `None` if the slice is too short.
pub(crate) fn read_u16_le(buf: &[u8], offset: usize) -> Option<u16> {
    buf.get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
}

/// Read a little-endian `u32` from `buf[offset..offset+4]`.
/// Returns `None` if the slice is too short.
pub(crate) fn read_u32_le(buf: &[u8], offset: usize) -> Option<u32> {
    buf.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

/// Read a little-endian `u64` from `buf[offset..offset+8]`.
/// Returns `None` if the slice is too short.
pub(crate) fn read_u64_le(buf: &[u8], offset: usize) -> Option<u64> {
    buf.get(offset..offset + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
}

struct AttrIter<'a> {
    record: &'a [u8],
    cursor: usize,
    bytes_used: usize,
}

impl<'a> AttrIter<'a> {
    fn new(record: &'a [u8]) -> Self {
        let attrs_offset = read_u16_le(record, REC_OFF_ATTRS_OFFSET).unwrap_or(0) as usize;
        let bytes_used = read_u32_le(record, REC_OFF_BYTES_USED).unwrap_or(0) as usize;
        Self {
            record,
            cursor: attrs_offset,
            bytes_used: bytes_used.min(record.len()),
        }
    }
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = AttrLocation;

    fn next(&mut self) -> Option<Self::Item> {
        // End-of-attributes sentinel is a u32 0xFFFFFFFF. If cursor + 4
        // would run past bytes_used, stop.
        if self.cursor + 4 > self.bytes_used {
            return None;
        }
        let type_code = read_u32_le(self.record, self.cursor + attr_off::TYPE)?;
        if type_code == END_MARKER {
            return None;
        }
        // An attribute header is at least 16 bytes; bail if we don't
        // have that.
        if self.cursor + 16 > self.bytes_used {
            return None;
        }
        let length = read_u32_le(self.record, self.cursor + attr_off::LENGTH)? as usize;
        // length must be a multiple of 8, >0, and fit within bytes_used.
        if length == 0 || !length.is_multiple_of(8) || self.cursor + length > self.bytes_used {
            return None;
        }

        let non_resident = self.record[self.cursor + attr_off::NON_RESIDENT] != 0;

        // THE HEADER HAS TO BE INSIDE THE ATTRIBUTE.
        //
        // A resident header is 0x18 bytes and a non-resident one 0x40,
        // and the fields read below sit at fixed offsets inside them.
        // `length` is checked above; the header size was not, so an
        // attribute of `length = 8` was yielded and then had its
        // `initialized_size` read from bytes 0x38..0x40 -- past the end
        // of the record when the attribute sat near the end of it.
        let header = if non_resident { 0x40 } else { 0x18 };
        if length < header {
            return None;
        }

        let name_length = self.record[self.cursor + attr_off::NAME_LENGTH];
        let name_offset = read_u16_le(self.record, self.cursor + attr_off::NAME_OFFSET)?;
        let attribute_id = read_u16_le(self.record, self.cursor + attr_off::ATTRIBUTE_ID)?;

        // A name, where there is one, is inside the attribute too.
        if name_length != 0 {
            let name_end = (name_offset as usize).checked_add(usize::from(name_length) * 2)?;
            if name_end > length {
                return None;
            }
        }

        let mut loc = AttrLocation {
            type_code,
            attr_offset: self.cursor,
            attr_length: length,
            is_resident: !non_resident,
            name_length,
            name_offset,
            attribute_id,
            resident_value_offset: None,
            resident_value_length: None,
            non_resident_value_length: None,
            non_resident_mapping_pairs_offset: None,
        };

        // THE VALUE HAS TO BE INSIDE THE ATTRIBUTE TOO.
        //
        // These three fields were yielded exactly as the disk gave
        // them, and around thirty call sites then used them as slice
        // bounds, allocation sizes and disk write offsets. Two of those
        // sites check them and the rest assume the iterator did.
        //
        //   - `resident_value_offset` + `resident_value_length` is
        //     where a resident value sits. Unchecked, `$VOLUME_INFORMATION`
        //     with a value_offset of 0xFFF0 and a plausible length made
        //     `set_dirty` write two bytes into a neighbouring MFT record.
        //   - `mapping_pairs_offset` is where a non-resident
        //     attribute's run list starts, and `read.rs` slices
        //     `record[attr_offset + mpo .. attr_offset + attr_length]`.
        //     One byte changed in a normal $DATA header -- mpo 0xFFFF --
        //     gave "range start index 65591 out of range for slice of
        //     length 1024".
        //
        // An attribute whose own header does not describe something
        // inside itself is not an attribute, and the iterator stops
        // where it stops for a bad `length`.
        if !non_resident {
            let value_length =
                read_u32_le(self.record, self.cursor + attr_off::RESIDENT_VALUE_LENGTH)? as usize;
            let value_offset =
                read_u16_le(self.record, self.cursor + attr_off::RESIDENT_VALUE_OFFSET)? as usize;
            if value_offset < header || value_offset.checked_add(value_length)? > length {
                return None;
            }
            loc.resident_value_length = Some(value_length as u32);
            loc.resident_value_offset = Some(value_offset as u16);
        } else {
            let mapping_pairs_offset = read_u16_le(
                self.record,
                self.cursor + attr_off::NONRES_MAPPING_PAIRS_OFFSET,
            )? as usize;
            if mapping_pairs_offset < header || mapping_pairs_offset > length {
                return None;
            }
            loc.non_resident_value_length = Some(read_u64_le(
                self.record,
                self.cursor + attr_off::NONRES_DATA_LENGTH,
            )?);
            loc.non_resident_mapping_pairs_offset = Some(mapping_pairs_offset as u16);
        }

        self.cursor += length;
        Some(loc)
    }
}

/// Byte offset within the record of the first byte of a resident value.
/// Returns `None` if the attribute is non-resident.
pub fn resident_value_start(loc: &AttrLocation) -> Option<usize> {
    loc.resident_value_offset
        .map(|off| loc.attr_offset + off as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builder for a synthetic MFT record with manually-placed attributes.
    /// Sets attrs_offset, bytes_used, and the 0xFFFFFFFF terminator. Does
    /// NOT include FILE magic or USA fixup — those live in `mft_io`; this
    /// is purely a fixture for the attribute walker.
    struct RecordBuilder {
        rec: Vec<u8>,
        cursor: usize,
    }
    impl RecordBuilder {
        fn new(size: usize, attrs_offset: u16) -> Self {
            let mut rec = vec![0u8; size];
            rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
                .copy_from_slice(&attrs_offset.to_le_bytes());
            Self {
                rec,
                cursor: attrs_offset as usize,
            }
        }
        /// Append a minimal resident attribute. Returns self for chaining.
        fn push_resident(
            mut self,
            type_code: u32,
            attr_id: u16,
            value: &[u8],
            name_utf16: &[u16],
        ) -> Self {
            let header_size = 24usize; // up through value_offset
            let name_size = name_utf16.len() * 2;
            let value_offset = header_size + name_size;
            let total_unaligned = value_offset + value.len();
            let total = (total_unaligned + 7) & !7; // 8-byte align
            let start = self.cursor;
            // Header
            self.rec[start..start + 4].copy_from_slice(&type_code.to_le_bytes());
            self.rec[start + attr_off::LENGTH..start + attr_off::LENGTH + 4]
                .copy_from_slice(&(total as u32).to_le_bytes());
            self.rec[start + attr_off::NON_RESIDENT] = 0;
            self.rec[start + attr_off::NAME_LENGTH] = name_utf16.len() as u8;
            self.rec[start + attr_off::NAME_OFFSET..start + attr_off::NAME_OFFSET + 2]
                .copy_from_slice(&(header_size as u16).to_le_bytes());
            self.rec[start + attr_off::ATTRIBUTE_ID..start + attr_off::ATTRIBUTE_ID + 2]
                .copy_from_slice(&attr_id.to_le_bytes());
            self.rec[start + attr_off::RESIDENT_VALUE_LENGTH
                ..start + attr_off::RESIDENT_VALUE_LENGTH + 4]
                .copy_from_slice(&(value.len() as u32).to_le_bytes());
            self.rec[start + attr_off::RESIDENT_VALUE_OFFSET
                ..start + attr_off::RESIDENT_VALUE_OFFSET + 2]
                .copy_from_slice(&(value_offset as u16).to_le_bytes());
            // Name bytes
            for (i, &codeunit) in name_utf16.iter().enumerate() {
                self.rec[start + header_size + i * 2..start + header_size + i * 2 + 2]
                    .copy_from_slice(&codeunit.to_le_bytes());
            }
            // Value bytes
            self.rec[start + value_offset..start + value_offset + value.len()]
                .copy_from_slice(value);
            self.cursor = start + total;
            self
        }
        /// Append a minimal non-resident attribute with the given data
        /// length and mapping_pairs blob.
        fn push_nonresident(
            mut self,
            type_code: u32,
            data_length: u64,
            mapping_pairs: &[u8],
        ) -> Self {
            let header_size = 64usize; // non-resident header is 64 bytes
            let mp_offset = header_size;
            let total_unaligned = mp_offset + mapping_pairs.len();
            let total = (total_unaligned + 7) & !7;
            let start = self.cursor;
            self.rec[start..start + 4].copy_from_slice(&type_code.to_le_bytes());
            self.rec[start + attr_off::LENGTH..start + attr_off::LENGTH + 4]
                .copy_from_slice(&(total as u32).to_le_bytes());
            self.rec[start + attr_off::NON_RESIDENT] = 1;
            self.rec[start + attr_off::NAME_LENGTH] = 0;
            self.rec[start + attr_off::NAME_OFFSET..start + attr_off::NAME_OFFSET + 2]
                .copy_from_slice(&0u16.to_le_bytes());
            self.rec
                [start + attr_off::NONRES_DATA_LENGTH..start + attr_off::NONRES_DATA_LENGTH + 8]
                .copy_from_slice(&data_length.to_le_bytes());
            self.rec[start + attr_off::NONRES_MAPPING_PAIRS_OFFSET
                ..start + attr_off::NONRES_MAPPING_PAIRS_OFFSET + 2]
                .copy_from_slice(&(mp_offset as u16).to_le_bytes());
            self.rec[start + mp_offset..start + mp_offset + mapping_pairs.len()]
                .copy_from_slice(mapping_pairs);
            self.cursor = start + total;
            self
        }
        fn finish(mut self) -> Vec<u8> {
            // 0xFFFFFFFF end marker.
            self.rec[self.cursor..self.cursor + 4].copy_from_slice(&END_MARKER.to_le_bytes());
            // bytes_used = cursor + 4 (the end marker).
            let bu = (self.cursor + 4) as u32;
            self.rec[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&bu.to_le_bytes());
            self.rec
        }
    }

    // --- AttrType::from_u32 ------------------------------------------------

    #[test]
    fn attr_type_from_u32_known_values() {
        assert_eq!(
            AttrType::from_u32(0x10),
            Some(AttrType::StandardInformation)
        );
        assert_eq!(AttrType::from_u32(0x30), Some(AttrType::FileName));
        assert_eq!(AttrType::from_u32(0x80), Some(AttrType::Data));
        assert_eq!(AttrType::from_u32(0xE0), Some(AttrType::ExtendedAttribute));
        assert_eq!(AttrType::from_u32(0xFF), None);
        assert_eq!(AttrType::from_u32(0), None);
    }

    // --- iter_attributes ---------------------------------------------------

    /// The iterator's three yielded offsets -- where a resident value
    /// sits, how long it is, and where a non-resident attribute's run
    /// list starts -- came off the disk untouched, and around thirty
    /// call sites used them as slice bounds, allocation sizes and disk
    /// write offsets. Two of those sites check them; the rest assume
    /// the iterator did.
    ///
    /// An attribute whose own header does not describe something inside
    /// itself is not an attribute, and the iterator stops where it
    /// stops for a `length` that is not a multiple of eight.
    #[test]
    fn an_attribute_whose_value_is_not_inside_it_is_not_yielded() {
        // The control: a well-formed record with one of each.
        let good = RecordBuilder::new(1024, 0x38)
            .push_resident(0x10, 1, b"standard information", &[])
            .push_nonresident(0x80, 4096, &[0x21, 0x01, 0x00])
            .finish();
        assert_eq!(iter_attributes(&good).count(), 2, "the control record");

        // `mapping_pairs_offset` past the attribute's own length. One
        // byte changed in a normal $DATA header; before this it gave
        // "range start index 65591 out of range for slice of length
        // 1024" out of `read.rs`.
        let mut hostile = good.clone();
        let data_at = iter_attributes(&good)
            .find(|a| a.type_code == 0x80)
            .expect("the $DATA attribute")
            .attr_offset;
        hostile[data_at + attr_off::NONRES_MAPPING_PAIRS_OFFSET
            ..data_at + attr_off::NONRES_MAPPING_PAIRS_OFFSET + 2]
            .copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(
            iter_attributes(&hostile).count(),
            1,
            "the $DATA attribute names a run list outside itself and was yielded anyway"
        );

        // A resident value that starts inside the attribute and ends
        // outside it: the shape that made `set_dirty` write into a
        // neighbouring MFT record.
        let si_at = iter_attributes(&good)
            .find(|a| a.type_code == 0x10)
            .expect("the $STANDARD_INFORMATION attribute")
            .attr_offset;
        let mut hostile = good.clone();
        hostile
            [si_at + attr_off::RESIDENT_VALUE_LENGTH..si_at + attr_off::RESIDENT_VALUE_LENGTH + 4]
            .copy_from_slice(&0xFFFFu32.to_le_bytes());
        assert!(
            !iter_attributes(&hostile).any(|a| a.type_code == 0x10),
            "a resident value 65535 bytes long was yielded from an attribute of 48"
        );

        // And one that starts outside it entirely.
        let mut hostile = good.clone();
        hostile
            [si_at + attr_off::RESIDENT_VALUE_OFFSET..si_at + attr_off::RESIDENT_VALUE_OFFSET + 2]
            .copy_from_slice(&0xFFF0u16.to_le_bytes());
        assert!(!iter_attributes(&hostile).any(|a| a.type_code == 0x10));
    }

    /// The fields at fixed offsets inside an attribute header are only
    /// there when the attribute is long enough to hold the header. A
    /// non-resident one of `length = 8` had `initialized_size` read
    /// from bytes 0x38..0x40 of it.
    #[test]
    fn an_attribute_shorter_than_its_own_header_is_not_yielded() {
        let good = RecordBuilder::new(1024, 0x38)
            .push_nonresident(0x80, 4096, &[0x21, 0x01, 0x00])
            .finish();
        let at = iter_attributes(&good)
            .next()
            .expect("one attribute")
            .attr_offset;

        let mut hostile = good.clone();
        hostile[at + attr_off::LENGTH..at + attr_off::LENGTH + 4]
            .copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(iter_attributes(&hostile).count(), 0);

        // A resident header is 0x18, so 0x10 is short for one too.
        let mut hostile = good.clone();
        hostile[at + attr_off::NON_RESIDENT] = 0;
        hostile[at + attr_off::LENGTH..at + attr_off::LENGTH + 4]
            .copy_from_slice(&16u32.to_le_bytes());
        assert_eq!(iter_attributes(&hostile).count(), 0);
    }

    #[test]
    fn iter_empty_record_yields_no_attributes() {
        let rec = RecordBuilder::new(1024, 0x38).finish();
        let attrs: Vec<_> = iter_attributes(&rec).collect();
        assert!(attrs.is_empty());
    }

    #[test]
    fn iter_record_with_one_resident_attribute() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::StandardInformation as u32, 0, b"hello", &[])
            .finish();
        let attrs: Vec<_> = iter_attributes(&rec).collect();
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].type_code, 0x10);
        assert!(attrs[0].is_resident);
        assert_eq!(attrs[0].resident_value_length, Some(5));
        let val_start = attrs[0].attr_offset + attrs[0].resident_value_offset.unwrap() as usize;
        assert_eq!(&rec[val_start..val_start + 5], b"hello");
    }

    #[test]
    fn iter_record_with_multiple_attributes() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::StandardInformation as u32, 0, &[0; 48], &[])
            .push_resident(AttrType::FileName as u32, 1, &[0; 16], &[])
            .push_resident(AttrType::Data as u32, 2, b"\x01\x02\x03", &[])
            .finish();
        let attrs: Vec<_> = iter_attributes(&rec).collect();
        assert_eq!(attrs.len(), 3);
        let codes: Vec<u32> = attrs.iter().map(|a| a.type_code).collect();
        assert_eq!(codes, vec![0x10, 0x30, 0x80]);
    }

    #[test]
    fn iter_stops_at_end_marker() {
        // After a manual 0xFFFFFFFF, no further iteration even if more
        // bytes follow.
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::StandardInformation as u32, 0, b"x", &[])
            .finish();
        // Mess up bytes past the end marker — iter_attributes must not
        // touch them.
        let mut rec = rec;
        let bu = u32::from_le_bytes([
            rec[REC_OFF_BYTES_USED],
            rec[REC_OFF_BYTES_USED + 1],
            rec[REC_OFF_BYTES_USED + 2],
            rec[REC_OFF_BYTES_USED + 3],
        ]) as usize;
        rec[bu..bu + 4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let attrs: Vec<_> = iter_attributes(&rec).collect();
        assert_eq!(attrs.len(), 1);
    }

    #[test]
    fn iter_rejects_attr_length_not_multiple_of_8() {
        // Build a record then corrupt one attribute's length to 5.
        let mut rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::StandardInformation as u32, 0, b"x", &[])
            .finish();
        let start = 0x38;
        rec[start + attr_off::LENGTH..start + attr_off::LENGTH + 4]
            .copy_from_slice(&5u32.to_le_bytes());
        let attrs: Vec<_> = iter_attributes(&rec).collect();
        assert_eq!(attrs.len(), 0, "iterator must stop on malformed length");
    }

    // --- find_attribute ----------------------------------------------------

    #[test]
    fn find_attribute_by_type_returns_first_match() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::StandardInformation as u32, 0, &[0; 48], &[])
            .push_resident(AttrType::FileName as u32, 1, &[0; 16], &[])
            .finish();
        let si = find_attribute(&rec, AttrType::StandardInformation, None).unwrap();
        assert_eq!(si.type_code, 0x10);
        let fname = find_attribute(&rec, AttrType::FileName, None).unwrap();
        assert_eq!(fname.type_code, 0x30);
    }

    #[test]
    fn find_attribute_returns_none_when_absent() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::StandardInformation as u32, 0, b"x", &[])
            .finish();
        assert!(find_attribute(&rec, AttrType::Data, None).is_none());
    }

    #[test]
    fn find_attribute_by_name_matches_utf16_name() {
        // Build an attribute with name "$I30" (4 UTF-16 code units).
        let name = [0x0024, 0x0049, 0x0033, 0x0030u16]; // "$I30"
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::IndexRoot as u32, 0, b"data", &name)
            .finish();
        let found = find_attribute(&rec, AttrType::IndexRoot, Some("$I30")).unwrap();
        assert_eq!(found.name_length, 4);
        assert!(find_attribute(&rec, AttrType::IndexRoot, Some("$WRONG")).is_none());
    }

    // --- non-resident iteration -------------------------------------------

    #[test]
    fn iter_returns_nonresident_data_length_and_mapping_pairs_offset() {
        // mapping pairs: one run of length=2, lcn=5.
        let mp = [0x11, 0x02, 0x05, 0x00];
        let rec = RecordBuilder::new(1024, 0x38)
            .push_nonresident(AttrType::Data as u32, 8192, &mp)
            .finish();
        let attrs: Vec<_> = iter_attributes(&rec).collect();
        assert_eq!(attrs.len(), 1);
        assert!(!attrs[0].is_resident);
        assert_eq!(attrs[0].non_resident_value_length, Some(8192));
        assert_eq!(attrs[0].non_resident_mapping_pairs_offset, Some(64));
    }

    // --- attr_name_equals --------------------------------------------------

    #[test]
    fn attr_name_equals_empty_name_only_matches_empty_string() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::Data as u32, 0, b"x", &[])
            .finish();
        let loc = iter_attributes(&rec).next().unwrap();
        assert!(attr_name_equals(&rec, &loc, ""));
        assert!(!attr_name_equals(&rec, &loc, "anything"));
    }

    #[test]
    fn attr_name_equals_named_attribute() {
        let name_utf16: Vec<u16> = "$I30".encode_utf16().collect();
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::IndexRoot as u32, 0, b"data", &name_utf16)
            .finish();
        let loc = iter_attributes(&rec).next().unwrap();
        assert!(attr_name_equals(&rec, &loc, "$I30"));
        assert!(!attr_name_equals(&rec, &loc, ""));
        assert!(!attr_name_equals(&rec, &loc, "$I31"));
    }

    // --- read_u16/32/64_le -------------------------------------------------

    #[test]
    fn read_u16_le_basic() {
        let buf = [0x34u8, 0x12, 0x00];
        assert_eq!(read_u16_le(&buf, 0), Some(0x1234));
        assert_eq!(read_u16_le(&buf, 1), Some(0x0012)); // bytes [0x12, 0x00] → 0x0012
        assert_eq!(read_u16_le(&buf, 2), None); // only 1 byte left
    }

    #[test]
    fn read_u16_le_bounds() {
        let buf = [0xABu8, 0xCD];
        assert_eq!(read_u16_le(&buf, 0), Some(0xCDAB));
        assert_eq!(read_u16_le(&buf, 1), None); // only 1 byte left
        assert_eq!(read_u16_le(&buf, 2), None);
    }

    #[test]
    fn read_u32_le_basic() {
        let buf = [0x78u8, 0x56, 0x34, 0x12];
        assert_eq!(read_u32_le(&buf, 0), Some(0x1234_5678));
    }

    #[test]
    fn read_u32_le_bounds() {
        let buf = [0u8; 3];
        assert_eq!(read_u32_le(&buf, 0), None);
        let buf4 = [1u8, 0, 0, 0];
        assert_eq!(read_u32_le(&buf4, 0), Some(1));
        assert_eq!(read_u32_le(&buf4, 1), None);
    }

    #[test]
    fn read_u64_le_basic() {
        let buf: [u8; 8] = [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
        assert_eq!(read_u64_le(&buf, 0), Some(0x0102_0304_0506_0708));
    }

    #[test]
    fn read_u64_le_bounds() {
        let buf = [0u8; 7];
        assert_eq!(read_u64_le(&buf, 0), None);
        let buf8 = [0xFFu8; 8];
        assert_eq!(read_u64_le(&buf8, 0), Some(u64::MAX));
        assert_eq!(read_u64_le(&buf8, 1), None);
    }

    // --- decode_attr_name ---------------------------------------------------

    #[test]
    fn decode_attr_name_unnamed_returns_empty() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::Data as u32, 0, b"x", &[])
            .finish();
        let loc = iter_attributes(&rec).next().unwrap();
        assert_eq!(decode_attr_name(&rec, &loc), "");
    }

    #[test]
    fn decode_attr_name_named_attribute() {
        let name_utf16: Vec<u16> = "Zone.Identifier".encode_utf16().collect();
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::Data as u32, 0, b"x", &name_utf16)
            .finish();
        let loc = iter_attributes(&rec).next().unwrap();
        assert_eq!(decode_attr_name(&rec, &loc), "Zone.Identifier");
    }

    // --- describe_attributes ------------------------------------------------

    #[test]
    fn describe_attributes_returns_one_per_attr() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::StandardInformation as u32, 0, &[0u8; 48], &[])
            .push_resident(AttrType::FileName as u32, 1, &[0u8; 32], &[])
            .finish();
        let descs = describe_attributes(&rec);
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].type_code, 0x10);
        assert_eq!(descs[0].type_name, "$STANDARD_INFORMATION");
        assert_eq!(descs[1].type_code, 0x30);
        assert_eq!(descs[1].type_name, "$FILE_NAME");
    }

    #[test]
    fn describe_attributes_empty_record_returns_empty() {
        let rec = RecordBuilder::new(1024, 0x38).finish();
        assert!(describe_attributes(&rec).is_empty());
    }

    #[test]
    fn describe_attributes_resident_value_length() {
        let rec = RecordBuilder::new(1024, 0x38)
            .push_resident(AttrType::Data as u32, 0, b"hello", &[])
            .finish();
        let descs = describe_attributes(&rec);
        assert_eq!(descs[0].value_length, 5);
        assert!(descs[0].is_resident);
    }

    #[test]
    fn describe_attributes_nonresident_value_length() {
        let mp = [0x11u8, 0x04, 0x05, 0x00]; // one run, 4 clusters from lcn 5
        let rec = RecordBuilder::new(1024, 0x38)
            .push_nonresident(AttrType::Data as u32, 16384, &mp)
            .finish();
        let descs = describe_attributes(&rec);
        assert_eq!(descs[0].value_length, 16384);
        assert!(!descs[0].is_resident);
    }
}
