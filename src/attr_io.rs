//! Locate attributes within a clean (post-fixup) MFT record buffer.
//!
//! Stays in the Rust domain so every W1+ write path can work on a buffer
//! handed to it by [`mft_io::update_mft_record`] without going back
//! through upstream parsers. Upstream is used for path → record-number
//! resolution; once inside the RMW callback, we walk attributes here.
//!
//! References (no GPL code consulted): [Flatcap Attribute Header](https://flatcap.github.io/linux-ntfs/ntfs/concepts/attribute_header.html),
//! [Flatcap File Record](https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html),
//! MS-FSCC.

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
        if length == 0 || length % 8 != 0 || self.cursor + length > self.bytes_used {
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
