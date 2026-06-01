//! Extended Attribute pack/unpack (NTFS `$EA` / `$EA_INFORMATION`).
//!
//! Each EA is a (name, value) pair with a flags byte. Names are ASCII,
//! values are arbitrary bytes. On disk they're stored in `$EA` as a
//! packed list of FILE_FULL_EA_INFORMATION entries, 4-byte aligned.
//!
//! References (no GPL code consulted): MS-FSCC 2.4.15
//! FILE_FULL_EA_INFORMATION; $EA / $EA_INFORMATION attribute layout
//! per Windows Internals 7th ed. ch. "NTFS On-Disk Structure".

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

#[cfg(test)]
mod tests {
    use super::*;

    fn ea(name: &[u8], value: &[u8]) -> Ea {
        Ea {
            flags: 0,
            name: name.to_vec(),
            value: value.to_vec(),
        }
    }

    fn ea_need(name: &[u8], value: &[u8]) -> Ea {
        Ea {
            flags: FLAG_NEED_EA,
            name: name.to_vec(),
            value: value.to_vec(),
        }
    }

    // --- align4 ---

    #[test]
    fn align4_already_aligned() {
        assert_eq!(align4(0), 0);
        assert_eq!(align4(4), 4);
        assert_eq!(align4(8), 8);
        assert_eq!(align4(128), 128);
    }

    #[test]
    fn align4_rounds_up() {
        assert_eq!(align4(1), 4);
        assert_eq!(align4(2), 4);
        assert_eq!(align4(3), 4);
        assert_eq!(align4(5), 8);
        assert_eq!(align4(7), 8);
        assert_eq!(align4(9), 12);
    }

    // --- entry_encoded_size ---

    #[test]
    fn entry_encoded_size_basic() {
        // header(8) + name_len + NUL + value_len, aligned to 4
        assert_eq!(entry_encoded_size(0, 0), 12); // 9 → 12
        assert_eq!(entry_encoded_size(3, 0), 12); // 12 → 12
        assert_eq!(entry_encoded_size(4, 0), 16); // 13 → 16
        assert_eq!(entry_encoded_size(3, 3), 16); // 15 → 16
        assert_eq!(entry_encoded_size(4, 4), 20); // 8+4+1+4=17 → 20
    }

    #[test]
    fn entry_encoded_size_is_multiple_of_4() {
        for nl in 0..=10usize {
            for vl in 0..=10usize {
                assert_eq!(entry_encoded_size(nl, vl) % 4, 0);
            }
        }
    }

    // --- encode / decode roundtrip ---

    #[test]
    fn encode_decode_roundtrip_single() {
        let eas = vec![ea(b"FOO", b"bar")];
        let bytes = encode(&eas).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded, eas);
    }

    #[test]
    fn encode_decode_roundtrip_multiple() {
        let eas = vec![
            ea_need(b"KEY1", b"VAL1"),
            ea(b"KEY2", b"V2"),
            ea(b"K3", &[0xDE, 0xAD, 0xBE, 0xEF]),
        ];
        let bytes = encode(&eas).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded, eas);
    }

    #[test]
    fn encode_empty_list_produces_empty_bytes() {
        let bytes = encode(&[]).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn decode_empty_bytes_produces_empty_list() {
        let decoded = decode(&[]).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn encode_output_4_byte_aligned_length() {
        for nl in 0..=8usize {
            for vl in 0..=8usize {
                let eas = vec![ea(&vec![b'A'; nl], &vec![0u8; vl])];
                let bytes = encode(&eas).unwrap();
                assert_eq!(bytes.len() % 4, 0, "nl={nl} vl={vl}");
            }
        }
    }

    #[test]
    fn encode_last_entry_has_zero_next_offset() {
        let eas = vec![ea(b"A", b"1"), ea(b"B", b"2")];
        let bytes = encode(&eas).unwrap();
        // first entry's next_off = entry_encoded_size(1, 1) = align4(8+1+1+1)=12
        let first_len = entry_encoded_size(1, 1);
        // second entry's next_off (at bytes[first_len..first_len+4]) must be 0
        let last_next_off = u32::from_le_bytes(bytes[first_len..first_len + 4].try_into().unwrap());
        assert_eq!(last_next_off, 0);
    }

    #[test]
    fn encode_error_on_name_too_long() {
        let name = vec![b'X'; 255];
        assert!(encode(&[Ea {
            flags: 0,
            name,
            value: vec![]
        }])
        .is_err());
    }

    #[test]
    fn decode_error_on_truncated_buffer() {
        let eas = vec![ea(b"KEY", b"val")];
        let bytes = encode(&eas).unwrap();
        // encode pads to 4-byte boundary; truncate into the actual value data
        // (remove enough bytes to cut into name/value, not just trailing padding)
        let result = decode(&bytes[..8]); // header only, no name/value
        assert!(result.is_err());
    }

    // --- count_need_ea ---

    #[test]
    fn count_need_ea_none_set() {
        let eas = vec![ea(b"A", b"1"), ea(b"B", b"2")];
        assert_eq!(count_need_ea(&eas), 0);
    }

    #[test]
    fn count_need_ea_all_set() {
        let eas = vec![ea_need(b"A", b"1"), ea_need(b"B", b"2")];
        assert_eq!(count_need_ea(&eas), 2);
    }

    #[test]
    fn count_need_ea_mixed() {
        let eas = vec![ea(b"A", b"1"), ea_need(b"B", b"2"), ea(b"C", b"3")];
        assert_eq!(count_need_ea(&eas), 1);
    }

    #[test]
    fn count_need_ea_empty() {
        assert_eq!(count_need_ea(&[]), 0);
    }

    // --- build_ea_information_value ---

    #[test]
    fn build_ea_information_value_is_8_bytes() {
        assert_eq!(build_ea_information_value(0, 0).len(), 8);
        assert_eq!(build_ea_information_value(1000, 3).len(), 8);
    }

    #[test]
    fn build_ea_information_value_layout() {
        let val = build_ea_information_value(1234, 5);
        assert_eq!(u16::from_le_bytes([val[0], val[1]]), 1234); // ea_length
        assert_eq!(u16::from_le_bytes([val[2], val[3]]), 1234); // ea_query_size
        assert_eq!(u32::from_le_bytes([val[4], val[5], val[6], val[7]]), 5); // need_count
    }

    #[test]
    fn build_ea_information_value_zeros() {
        assert_eq!(build_ea_information_value(0, 0), vec![0u8; 8]);
    }

    // --- upsert ---

    #[test]
    fn upsert_appends_when_name_is_new() {
        let mut list = vec![ea(b"A", b"1")];
        upsert(&mut list, ea(b"B", b"2"));
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].name, b"B");
        assert_eq!(list[1].value, b"2");
    }

    #[test]
    fn upsert_replaces_existing_exact_case() {
        let mut list = vec![ea(b"KEY", b"old")];
        upsert(&mut list, ea(b"KEY", b"new"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].value, b"new");
    }

    #[test]
    fn upsert_replaces_existing_case_insensitive() {
        let mut list = vec![ea(b"KEY", b"old")];
        upsert(&mut list, ea(b"key", b"new"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].value, b"new");
    }

    #[test]
    fn upsert_into_empty_list() {
        let mut list = vec![];
        upsert(&mut list, ea(b"X", b"Y"));
        assert_eq!(list.len(), 1);
    }

    // --- remove_by_name ---

    #[test]
    fn remove_by_name_found_returns_true() {
        let mut list = vec![ea(b"A", b"1"), ea(b"B", b"2")];
        assert!(remove_by_name(&mut list, b"A"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, b"B");
    }

    #[test]
    fn remove_by_name_not_found_returns_false() {
        let mut list = vec![ea(b"A", b"1")];
        assert!(!remove_by_name(&mut list, b"Z"));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn remove_by_name_case_insensitive() {
        let mut list = vec![ea(b"KEY", b"val")];
        assert!(remove_by_name(&mut list, b"key"));
        assert!(list.is_empty());
    }

    #[test]
    fn remove_by_name_from_empty_list() {
        let mut list: Vec<Ea> = vec![];
        assert!(!remove_by_name(&mut list, b"X"));
    }
}
