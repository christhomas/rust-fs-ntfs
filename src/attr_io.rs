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

struct AttrIter<'a> {
    record: &'a [u8],
    cursor: usize,
    bytes_used: usize,
}

impl<'a> AttrIter<'a> {
    fn new(record: &'a [u8]) -> Self {
        let attrs_offset = u16::from_le_bytes([
            record[REC_OFF_ATTRS_OFFSET],
            record[REC_OFF_ATTRS_OFFSET + 1],
        ]) as usize;
        let bytes_used = u32::from_le_bytes([
            record[REC_OFF_BYTES_USED],
            record[REC_OFF_BYTES_USED + 1],
            record[REC_OFF_BYTES_USED + 2],
            record[REC_OFF_BYTES_USED + 3],
        ]) as usize;
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
        let type_code = u32::from_le_bytes([
            self.record[self.cursor + attr_off::TYPE],
            self.record[self.cursor + attr_off::TYPE + 1],
            self.record[self.cursor + attr_off::TYPE + 2],
            self.record[self.cursor + attr_off::TYPE + 3],
        ]);
        if type_code == END_MARKER {
            return None;
        }
        // An attribute header is at least 16 bytes; bail if we don't
        // have that.
        if self.cursor + 16 > self.bytes_used {
            return None;
        }
        let length = u32::from_le_bytes([
            self.record[self.cursor + attr_off::LENGTH],
            self.record[self.cursor + attr_off::LENGTH + 1],
            self.record[self.cursor + attr_off::LENGTH + 2],
            self.record[self.cursor + attr_off::LENGTH + 3],
        ]) as usize;
        // length must be a multiple of 8, >0, and fit within bytes_used.
        if length == 0 || !length.is_multiple_of(8) || self.cursor + length > self.bytes_used {
            return None;
        }

        let non_resident = self.record[self.cursor + attr_off::NON_RESIDENT] != 0;
        let name_length = self.record[self.cursor + attr_off::NAME_LENGTH];
        let name_offset = u16::from_le_bytes([
            self.record[self.cursor + attr_off::NAME_OFFSET],
            self.record[self.cursor + attr_off::NAME_OFFSET + 1],
        ]);
        let attribute_id = u16::from_le_bytes([
            self.record[self.cursor + attr_off::ATTRIBUTE_ID],
            self.record[self.cursor + attr_off::ATTRIBUTE_ID + 1],
        ]);

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

        if !non_resident {
            loc.resident_value_length = Some(u32::from_le_bytes([
                self.record[self.cursor + attr_off::RESIDENT_VALUE_LENGTH],
                self.record[self.cursor + attr_off::RESIDENT_VALUE_LENGTH + 1],
                self.record[self.cursor + attr_off::RESIDENT_VALUE_LENGTH + 2],
                self.record[self.cursor + attr_off::RESIDENT_VALUE_LENGTH + 3],
            ]));
            loc.resident_value_offset = Some(u16::from_le_bytes([
                self.record[self.cursor + attr_off::RESIDENT_VALUE_OFFSET],
                self.record[self.cursor + attr_off::RESIDENT_VALUE_OFFSET + 1],
            ]));
        } else {
            loc.non_resident_value_length = Some(u64::from_le_bytes(
                self.record[self.cursor + attr_off::NONRES_DATA_LENGTH
                    ..self.cursor + attr_off::NONRES_DATA_LENGTH + 8]
                    .try_into()
                    .unwrap(),
            ));
            loc.non_resident_mapping_pairs_offset = Some(u16::from_le_bytes([
                self.record[self.cursor + attr_off::NONRES_MAPPING_PAIRS_OFFSET],
                self.record[self.cursor + attr_off::NONRES_MAPPING_PAIRS_OFFSET + 1],
            ]));
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
}
