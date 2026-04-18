//! Extended Attribute pack/unpack (NTFS `$EA` / `$EA_INFORMATION`).
//!
//! Each EA is a (name, value) pair with a flags byte. Names are ASCII,
//! values are arbitrary bytes. On disk they're stored in `$EA` as a
//! packed list of FILE_FULL_EA_INFORMATION entries, 4-byte aligned.
//!
//! Reference (no GPL code consulted):
//! * [Flatcap $EA](https://flatcap.github.io/linux-ntfs/ntfs/attributes/ea.html)
//! * MS-FSCC 2.4.15 FILE_FULL_EA_INFORMATION

use crate::attr_io::{self, AttrType};

pub const FLAG_NEED_EA: u8 = 0x80;

/// Decoded single EA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ea {
    pub flags: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// Align `n` up to the next 4-byte boundary (EA entries are 4-aligned
/// per spec).
fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Encode a size-calculated per-entry header + name/value size in bytes.
pub fn entry_encoded_size(name_len: usize, value_len: usize) -> usize {
    align4(8 + name_len + 1 + value_len)
}

/// Pack a list of EAs into a `$EA` blob.
pub fn encode(eas: &[Ea]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (i, ea) in eas.iter().enumerate() {
        if ea.name.len() > 254 {
            return Err(format!("ea name too long: {}", ea.name.len()));
        }
        if ea.value.len() > u16::MAX as usize {
            return Err(format!("ea value too large: {}", ea.value.len()));
        }
        let entry_len = entry_encoded_size(ea.name.len(), ea.value.len());
        let next_off = if i == eas.len() - 1 { 0 } else { entry_len };
        // Header (8 bytes).
        out.extend_from_slice(&(next_off as u32).to_le_bytes());
        out.push(ea.flags);
        out.push(ea.name.len() as u8);
        out.extend_from_slice(&(ea.value.len() as u16).to_le_bytes());
        // Name (null-terminated).
        out.extend_from_slice(&ea.name);
        out.push(0);
        // Value.
        out.extend_from_slice(&ea.value);
        // Pad to 4-byte boundary.
        while out.len() & 3 != 0 {
            out.push(0);
        }
    }
    Ok(out)
}

/// Decode a `$EA` blob into a list.
pub fn decode(bytes: &[u8]) -> Result<Vec<Ea>, String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor + 8 <= bytes.len() {
        let next_off = u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]) as usize;
        let flags = bytes[cursor + 4];
        let name_len = bytes[cursor + 5] as usize;
        let value_len = u16::from_le_bytes([bytes[cursor + 6], bytes[cursor + 7]]) as usize;
        let name_start = cursor + 8;
        let value_start = name_start + name_len + 1; // skip NUL
        let value_end = value_start + value_len;
        if value_end > bytes.len() {
            return Err(format!(
                "EA entry at {cursor} extends past buffer ({value_end} > {})",
                bytes.len()
            ));
        }
        out.push(Ea {
            flags,
            name: bytes[name_start..name_start + name_len].to_vec(),
            value: bytes[value_start..value_end].to_vec(),
        });
        if next_off == 0 {
            break;
        }
        if next_off < 8 || cursor + next_off > bytes.len() {
            return Err(format!(
                "EA entry at {cursor} has invalid next_offset {next_off}"
            ));
        }
        cursor += next_off;
    }
    Ok(out)
}

/// Count EAs with the NEED_EA flag set (bit 0x80). Used to populate
/// `$EA_INFORMATION.ea_count_need`.
pub fn count_need_ea(eas: &[Ea]) -> u32 {
    eas.iter().filter(|e| e.flags & FLAG_NEED_EA != 0).count() as u32
}

/// Build the `$EA_INFORMATION` value (8 bytes).
pub fn build_ea_information_value(packed_ea_length: u16, need_ea_count: u32) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v[0..2].copy_from_slice(&packed_ea_length.to_le_bytes()); // ea_length
    v[2..4].copy_from_slice(&packed_ea_length.to_le_bytes()); // ea_query_size (approximation)
    v[4..8].copy_from_slice(&need_ea_count.to_le_bytes());
    v
}

/// Read the current EAs + $EA_INFORMATION state from a record. Returns
/// `Ok(vec)` if `$EA` is absent (empty list) or present + resident; an
/// error if present but non-resident (MVP).
pub fn read_from_record(record: &[u8]) -> Result<Vec<Ea>, String> {
    let Some(ea) = attr_io::find_attribute(record, AttrType::ExtendedAttribute, None) else {
        return Ok(Vec::new());
    };
    if !ea.is_resident {
        return Err("$EA is non-resident (MVP only supports resident EAs)".to_string());
    }
    let val_off = ea.resident_value_offset.ok_or("no value_offset")? as usize;
    let val_len = ea.resident_value_length.ok_or("no value_length")? as usize;
    let start = ea.attr_offset + val_off;
    decode(&record[start..start + val_len])
}

/// Upsert a single EA in a decoded list (replaces if name matches
/// case-insensitively, as Windows requires; appends otherwise).
pub fn upsert(list: &mut Vec<Ea>, entry: Ea) {
    for e in list.iter_mut() {
        if e.name.eq_ignore_ascii_case(&entry.name) {
            *e = entry;
            return;
        }
    }
    list.push(entry);
}

/// Remove by name (case-insensitive). Returns true if an entry was removed.
pub fn remove_by_name(list: &mut Vec<Ea>, name: &[u8]) -> bool {
    let before = list.len();
    list.retain(|e| !e.name.eq_ignore_ascii_case(name));
    list.len() != before
}
