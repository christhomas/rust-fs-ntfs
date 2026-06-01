//! `$Secure:$SDS` data-stream serialization plus the SDH/SII hash.
//!
//! The `$SDS` stream is the canonical store of every distinct security
//! descriptor on the volume. Each entry consists of a 20-byte header
//! (hash + security_id + offset + size) immediately followed by the SD
//! payload (MS-DTYP §2.4.6 SECURITY_DESCRIPTOR_RELATIVE), zero-padded
//! to a 16-byte boundary. NTFS keeps an exact byte-for-byte mirror of
//! every entry at `entry_offset + 0x40000` so the cache survives a
//! torn-write at the primary copy.
//!
//! References:
//!   * MS-FSCC §2.4 ("$Secure" system file; SDH/SII view indexes).
//!   * MS-DTYP §2.4.6 (`SECURITY_DESCRIPTOR_RELATIVE`).
//!   * Windows Internals, 7th ed., ch. "NTFS On-Disk Structure".
//!
//! The hash function below is implementation-defined per MS-FSCC —
//! observed via byte-diff against Microsoft `Format-Volume`'s
//! `$Secure:$SDS` output (NTFS v3.1 reference). All 12 sampled entries
//! from a fresh reference matched a 32-bit rotate-3-and-add over the
//! SD bytes; that is the algorithm implemented here.

/// One SD entry destined for `$SDS`. `security_id` is the monotonic
/// ID (Microsoft's allocation starts at 0x100); the offset and full
/// entry size are derived in [`build_sds`].
#[derive(Clone, Debug)]
pub struct SdEntry<'a> {
    pub security_id: u32,
    pub sd: &'a [u8],
}

/// SDS entry header size (hash + security_id + offset + size).
pub const SDS_HEADER_LEN: u32 = 20;

// Field offsets within the 20-byte SDS entry header (all little-endian).
const SDS_HDR_HASH_OFF: usize = 0; // u32 SDH/SII hash
const SDS_HDR_SECURITY_ID_OFF: usize = 4; // u32 security identifier
const SDS_HDR_ENTRY_OFFSET_OFF: usize = 8; // u64 byte offset of this entry in $SDS
const SDS_HDR_ENTRY_SIZE_OFF: usize = 16; // u32 unpadded entry size (header + SD)
const SDS_HDR_SD_DATA_OFF: usize = 20; // SD payload starts immediately after the header

/// Round `n` up to the next multiple of 16. NTFS pads SDS entries to 16-byte
/// boundaries so successive entries remain naturally aligned in the $SDS stream.
fn align_to_16(n: usize) -> usize {
    (n + 15) & !15
}

/// NTFS mirrors every `$SDS` entry at `offset + MIRROR_GAP`. Per the
/// public Microsoft layout this is exactly 256 KiB.
pub const SDS_MIRROR_GAP: u64 = 0x40000;

/// SDH/SII hash of a security-descriptor blob.
///
/// MS-FSCC §2.4 declares the SDH hash algorithm implementation-
/// specific; this implementation was derived purely by observing
/// Microsoft `Format-Volume`'s `$Secure:$SDS` byte output and
/// brute-checking 32-bit hash candidates against the resulting
/// SD bytes (12 entries from a fresh NTFS v3.1 reference all matched).
pub fn sdh_hash(sd: &[u8]) -> u32 {
    let mut h: u32 = 0;
    let mut chunks = sd.chunks_exact(4);
    for c in &mut chunks {
        let w = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        h = h.rotate_left(3).wrapping_add(w);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut buf = [0u8; 4];
        buf[..rem.len()].copy_from_slice(rem);
        let w = u32::from_le_bytes(buf);
        h = h.rotate_left(3).wrapping_add(w);
    }
    h
}

/// Total in-stream length of a single SDS entry (header + SD), padded
/// to a 16-byte boundary. Microsoft pads with zero bytes; ntfs.sys
/// reads `sds_size` (unpadded) but the next entry begins at the
/// padded offset.
fn sds_entry_total_len(sd_len: usize) -> usize {
    align_to_16(SDS_HEADER_LEN as usize + sd_len)
}

/// Serialise one entry into `out` at `offset`, returning the unpadded
/// `sds_size` value (header + SD, what the entry header reports).
fn write_one_entry(out: &mut Vec<u8>, offset: u64, entry: &SdEntry<'_>) -> u32 {
    let hash = sdh_hash(entry.sd);
    let unpadded = SDS_HEADER_LEN as usize + entry.sd.len();
    let total = sds_entry_total_len(entry.sd.len());

    let needed = offset as usize + total;
    if out.len() < needed {
        out.resize(needed, 0);
    }

    let at = offset as usize;
    out[at + SDS_HDR_HASH_OFF..at + SDS_HDR_HASH_OFF + 4].copy_from_slice(&hash.to_le_bytes());
    out[at + SDS_HDR_SECURITY_ID_OFF..at + SDS_HDR_SECURITY_ID_OFF + 4]
        .copy_from_slice(&entry.security_id.to_le_bytes());
    out[at + SDS_HDR_ENTRY_OFFSET_OFF..at + SDS_HDR_ENTRY_OFFSET_OFF + 8]
        .copy_from_slice(&offset.to_le_bytes());
    out[at + SDS_HDR_ENTRY_SIZE_OFF..at + SDS_HDR_ENTRY_SIZE_OFF + 4]
        .copy_from_slice(&(unpadded as u32).to_le_bytes());
    out[at + SDS_HDR_SD_DATA_OFF..at + SDS_HDR_SD_DATA_OFF + entry.sd.len()]
        .copy_from_slice(entry.sd);
    // Bytes at + unpadded .. at + total stay zero (alignment pad).

    unpadded as u32
}

/// Build the full `$SDS` data-stream bytes for the given entries.
///
/// Microsoft maintains a duplicate copy of every entry at
/// `offset + 0x40000` (256 KiB) inside the same data stream — the
/// "mirror" copy. The returned buffer covers from offset 0 through
/// the end of the last mirrored entry, so reading it as a normal data
/// stream yields both copies in their canonical positions.
///
/// For S2 we ship a single entry but the API takes a slice so S3+ can
/// extend without touching this function.
pub fn build_sds(entries: &[SdEntry<'_>]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut cursor: u64 = 0;

    let mut written: Vec<(u64, usize)> = Vec::with_capacity(entries.len());

    for entry in entries {
        let total = sds_entry_total_len(entry.sd.len());
        let _ = write_one_entry(&mut out, cursor, entry);
        written.push((cursor, total));
        cursor += total as u64;
    }

    // Mirror every entry at offset+0x40000. Each mirror entry carries
    // the SAME sds_offset value as its primary (Microsoft's reference
    // does NOT bump the header offset for the mirror copy — the mirror
    // is bit-identical to the primary's bytes).
    for (off, total) in &written {
        let mirror_off = off + SDS_MIRROR_GAP;
        let needed = mirror_off as usize + *total;
        if out.len() < needed {
            out.resize(needed, 0);
        }
        // Copy primary's bytes verbatim. We can't slice-overlap so
        // pull into a temp.
        let src: Vec<u8> = out[*off as usize..*off as usize + *total].to_vec();
        out[mirror_off as usize..mirror_off as usize + *total].copy_from_slice(&src);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode an ASCII hex string into bytes.
    fn hex_decode(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        assert!(bytes.len().is_multiple_of(2), "odd hex length");
        let mut out = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks_exact(2) {
            let hi = hex_nibble(chunk[0]);
            let lo = hex_nibble(chunk[1]);
            out.push((hi << 4) | lo);
        }
        out
    }

    fn hex_nibble(b: u8) -> u8 {
        match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => panic!("bad hex digit {b}"),
        }
    }

    // --- align_to_16 ----------------------------------------------------------

    #[test]
    fn align_to_16_already_aligned_is_unchanged() {
        assert_eq!(align_to_16(0), 0);
        assert_eq!(align_to_16(16), 16);
        assert_eq!(align_to_16(32), 32);
    }

    #[test]
    fn align_to_16_rounds_up_to_next_multiple() {
        assert_eq!(align_to_16(1), 16);
        assert_eq!(align_to_16(15), 16);
        assert_eq!(align_to_16(17), 32);
    }

    // --- sds_entry_total_len --------------------------------------------------

    #[test]
    fn sds_entry_total_len_pads_to_16_boundary() {
        // header=20 + sd=0 = 20 → pad to 32
        assert_eq!(sds_entry_total_len(0), 32);
        // header=20 + sd=12 = 32 → already aligned
        assert_eq!(sds_entry_total_len(12), 32);
        // header=20 + sd=52 = 72 → pad to 80
        assert_eq!(sds_entry_total_len(52), 80);
    }

    #[test]
    fn sdh_hash_matches_reference() {
        // External anchor: SD blob + expected hash captured from a
        // fresh Microsoft `Format-Volume` output (NTFS v3.1
        // reference). 100 bytes, security_id=0x100. This is the
        // canonical system-metafile SD on a fresh-formatted volume
        // (matches `SD_SYSFILE_RW` modulo the leading length /
        // revision bytes).
        let sd_100 = hex_decode("01000480480000005400000000000000140000000200340002000000000014008900120001010000000000051200000000001800890012000102000000000005200000002002000001010000000000051400000001020000000000052000000020020000");
        assert_eq!(sdh_hash(&sd_100), 0x32fee6cb);
    }

    #[test]
    fn sdh_hash_stable_round_trip() {
        // Regression guard: hashing a known multi-byte payload twice
        // must yield the same value, and the algorithm's defining
        // formula (h = rotl(h, 3) + w_le) must be re-derivable from
        // the bytes. If a future commit accidentally swaps endianness
        // or changes the rotation, this test catches it without
        // needing an external dump.
        let bytes: Vec<u8> = (0..40u8).collect();
        let h1 = sdh_hash(&bytes);
        let h2 = sdh_hash(&bytes);
        assert_eq!(h1, h2, "hash must be deterministic");

        // Hand-compute on a 4-byte input.
        let single = [0x01u8, 0x02, 0x03, 0x04];
        // rotl(0, 3) = 0; 0 + 0x04030201_LE = 0x04030201
        assert_eq!(sdh_hash(&single), 0x04030201);

        // Two words: w1 = 0x04030201_LE, h = rotl(0, 3) + w1 = 0x04030201.
        // rotl(0x04030201, 3) = 0x20181008.  w2 = 0x08070605_LE.
        // h2 = 0x20181008 + 0x08070605 = 0x281f160d.
        let two = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(sdh_hash(&two), 0x281f160d);
    }

    #[test]
    fn build_sds_single_entry_layout() {
        // Use a small 72-byte SD-shaped payload. Bytes are arbitrary;
        // this asserts only the framing (header values, padding, mirror).
        let sd: Vec<u8> = (0..72u8).collect();
        let entries = [SdEntry {
            security_id: 0x100,
            sd: &sd,
        }];
        let bytes = build_sds(&entries);

        // Primary copy at offset 0: header + SD = 92 bytes; padded to 96.
        assert_eq!(bytes.len(), SDS_MIRROR_GAP as usize + 96);

        let hash = sdh_hash(&sd);
        assert_eq!(&bytes[0..4], &hash.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&bytes[8..16], &0u64.to_le_bytes());
        assert_eq!(&bytes[16..20], &92u32.to_le_bytes());
        assert_eq!(&bytes[20..92], &sd[..]);
        // Pad bytes 92..96 zero.
        assert_eq!(&bytes[92..96], &[0u8; 4]);

        // Mirror copy: identical 96 bytes at +0x40000.
        let m = SDS_MIRROR_GAP as usize;
        assert_eq!(&bytes[m..m + 96], &bytes[0..96]);
    }

    // --- sdh_hash: remainder byte paths -----------------------------------

    #[test]
    fn sdh_hash_empty_input_returns_zero() {
        assert_eq!(sdh_hash(&[]), 0);
    }

    #[test]
    fn sdh_hash_single_byte_remainder() {
        // 1 byte, no full 4-byte chunk: buf=[0x42,0,0,0], w=0x42, h=0+0x42=0x42.
        assert_eq!(sdh_hash(&[0x42]), 0x42);
    }

    #[test]
    fn sdh_hash_two_byte_remainder() {
        // [0x01, 0x02]: buf=[0x01,0x02,0,0], w=0x0201, h=0+0x0201=0x0201.
        assert_eq!(sdh_hash(&[0x01, 0x02]), 0x0201);
    }

    #[test]
    fn sdh_hash_three_byte_remainder() {
        // [0x01, 0x02, 0x03]: buf=[0x01,0x02,0x03,0], w=0x00030201.
        assert_eq!(sdh_hash(&[0x01, 0x02, 0x03]), 0x00030201);
    }

    #[test]
    fn sdh_hash_five_bytes_full_chunk_plus_one_remainder() {
        // Bytes 0-3: full chunk → h = rotl(0, 3) + w0 = w0.
        // Byte 4: remainder buf=[b4, 0, 0, 0].
        let bytes = [0x01u8, 0x02, 0x03, 0x04, 0x05];
        let w0: u32 = u32::from_le_bytes([0x01, 0x02, 0x03, 0x04]);
        let h1 = 0u32.rotate_left(3).wrapping_add(w0);
        let w1 = u32::from_le_bytes([0x05, 0, 0, 0]);
        let expected = h1.rotate_left(3).wrapping_add(w1);
        assert_eq!(sdh_hash(&bytes), expected);
    }

    // --- sdh_hash: determinism and stability --------------------------------

    #[test]
    fn sdh_hash_deterministic_for_same_input() {
        let bytes: Vec<u8> = (0..100u8).collect();
        assert_eq!(sdh_hash(&bytes), sdh_hash(&bytes));
    }

    // --- build_sds: edge cases ---------------------------------------------

    #[test]
    fn build_sds_empty_entries_returns_empty_buffer() {
        let bytes = build_sds(&[]);
        assert!(bytes.is_empty());
    }

    #[test]
    fn build_sds_two_entries_sequential_primary_layout() {
        let sd0: Vec<u8> = vec![0xAAu8; 12]; // entry0: 20+12=32 = aligned
        let sd1: Vec<u8> = vec![0xBBu8; 8]; // entry1: 20+8=28 → 32
        let entries = [
            SdEntry {
                security_id: 0x100,
                sd: &sd0,
            },
            SdEntry {
                security_id: 0x101,
                sd: &sd1,
            },
        ];
        let bytes = build_sds(&entries);

        // Entry 0 at offset 0: 32 bytes.
        let hash0 = sdh_hash(&sd0);
        assert_eq!(&bytes[0..4], &hash0.to_le_bytes(), "entry0 hash");
        assert_eq!(&bytes[4..8], &0x100u32.to_le_bytes(), "entry0 security_id");
        assert_eq!(&bytes[8..16], &0u64.to_le_bytes(), "entry0 self-offset");
        assert_eq!(&bytes[16..20], &32u32.to_le_bytes(), "entry0 unpadded size");

        // Entry 1 at offset 32: 32 bytes.
        let hash1 = sdh_hash(&sd1);
        assert_eq!(&bytes[32..36], &hash1.to_le_bytes(), "entry1 hash");
        assert_eq!(
            &bytes[36..40],
            &0x101u32.to_le_bytes(),
            "entry1 security_id"
        );
        assert_eq!(&bytes[40..48], &32u64.to_le_bytes(), "entry1 self-offset");
        assert_eq!(&bytes[48..52], &28u32.to_le_bytes(), "entry1 unpadded size");
    }

    #[test]
    fn build_sds_two_entries_both_mirrors_present() {
        let sd: Vec<u8> = vec![0xCCu8; 12]; // 32-byte padded entry
        let entries = [
            SdEntry {
                security_id: 0x100,
                sd: &sd,
            },
            SdEntry {
                security_id: 0x101,
                sd: &sd,
            },
        ];
        let bytes = build_sds(&entries);
        let m = SDS_MIRROR_GAP as usize;
        // Mirror of entry 0 at +0x40000.
        assert_eq!(&bytes[m..m + 32], &bytes[0..32], "entry0 mirror");
        // Mirror of entry 1 at 32 + 0x40000.
        assert_eq!(&bytes[m + 32..m + 64], &bytes[32..64], "entry1 mirror");
    }

    #[test]
    fn build_sds_entry_offset_field_reflects_primary_position() {
        // The mirror copy must report the same sds_offset as the primary.
        let sd: Vec<u8> = vec![0x11u8; 12];
        let entries = [
            SdEntry {
                security_id: 0x100,
                sd: &sd,
            },
            SdEntry {
                security_id: 0x101,
                sd: &sd,
            },
        ];
        let bytes = build_sds(&entries);
        let m = SDS_MIRROR_GAP as usize;
        // Primary entry1 sds_offset = 32.
        let primary_off = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
        // Mirror entry1 sds_offset must also = 32 (NOT 32 + 0x40000).
        let mirror_off = u64::from_le_bytes(bytes[m + 40..m + 48].try_into().unwrap());
        assert_eq!(primary_off, 32);
        assert_eq!(
            mirror_off, 32,
            "mirror sds_offset must equal primary position"
        );
    }
}
