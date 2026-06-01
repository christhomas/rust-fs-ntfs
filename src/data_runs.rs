//! Encode / decode NTFS mapping-pair (data run) lists. A data run is a
//! `(length, lcn)` pair — a contiguous span of clusters. The list for a
//! non-resident attribute lives in the MFT record at the attribute's
//! `mapping_pairs_offset` and runs until the first 0x00 byte (or the
//! attribute's length).
//!
//! Reference (no GPL code consulted): NTFS data-run / mapping-pair
//! encoding per Windows Internals 7th ed. ch. "NTFS On-Disk Structure".
//!
//! Run-list encoding:
//!   byte 0     header = (lcn_bytes << 4) | length_bytes
//!                       length_bytes  in 1..=8
//!                       lcn_bytes     in 0..=8  (0 ⇒ sparse hole)
//!   bytes 1..  length (little-endian unsigned)
//!   next M     lcn delta (little-endian signed; first run is absolute)
//!   repeat…
//!   0x00       terminator

/// One decoded run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    /// First VCN covered by this run.
    pub starting_vcn: u64,
    /// Number of clusters in this run.
    pub length: u64,
    /// Absolute LCN of the first cluster. `None` ⇒ sparse hole (data
    /// reads as zero, no clusters allocated).
    pub lcn: Option<u64>,
}

/// Sign-extend a variable-width integer to a full `i64`.
///
/// NTFS LCN delta fields are little-endian signed integers packed into
/// 1–8 bytes. After reading `nbytes` bytes into `raw` (with the rest of
/// the `i64` zero), the value is only correct if the high bit of the
/// `nbytes`-wide field is 0 (positive). If that bit is 1, all upper bits
/// must be filled with 1s to produce the correct two's-complement negative.
///
/// Example: a 1-byte field `0xF6` represents −10, not 246. Without sign
/// extension the raw read gives `0x00000000000000F6` (= 246); after
/// extension it becomes `0xFFFFFFFFFFFFFFF6` (= −10).
fn sign_extend_i64(raw: i64, nbytes: usize) -> i64 {
    if nbytes >= 8 {
        // All 64 bits are already in use — no extension possible.
        return raw;
    }
    let sign_bit = 1i64 << (8 * nbytes - 1);
    if raw & sign_bit != 0 {
        // High bit of the field is set — fill every bit above it with 1s.
        // (sign_bit << 1) is the first bit above the field; subtracting 1
        // gives a mask of all bits AT or below the field; NOT flips it to
        // give all bits ABOVE the field.
        let upper_bits_mask = !((sign_bit << 1) - 1);
        raw | upper_bits_mask
    } else {
        raw
    }
}

/// Decode a mapping-pairs blob into an ordered list of runs. Stops at
/// the first 0x00 header byte or the end of `bytes`, whichever comes
/// first. Validates that LCN deltas don't produce negative absolute
/// LCNs.
pub fn decode_runs(bytes: &[u8]) -> Result<Vec<DataRun>, String> {
    let mut runs = Vec::new();
    let mut prev_lcn: i64 = 0;
    let mut starting_vcn: u64 = 0;
    let mut p = 0usize;

    while p < bytes.len() {
        let header = bytes[p];
        if header == 0 {
            return Ok(runs);
        }
        let length_bytes = (header & 0x0F) as usize;
        let lcn_bytes = ((header >> 4) & 0x0F) as usize;
        if length_bytes == 0 {
            return Err(format!("run at offset {p}: length-byte-count is zero"));
        }
        if length_bytes > 8 || lcn_bytes > 8 {
            return Err(format!("run at offset {p}: invalid header {header:#04x}"));
        }
        p += 1;
        if p + length_bytes + lcn_bytes > bytes.len() {
            return Err(format!(
                "run at offset {p}: extends past data ({length_bytes}+{lcn_bytes} needed, {} left)",
                bytes.len() - p
            ));
        }

        // length: unsigned little-endian, variable width 1..=8.
        let mut length = 0u64;
        for i in 0..length_bytes {
            length |= (bytes[p + i] as u64) << (8 * i);
        }
        p += length_bytes;
        if length == 0 {
            return Err(format!("run at offset {p}: zero-cluster length"));
        }

        let lcn = if lcn_bytes == 0 {
            None
        } else {
            // LCN field is a signed variable-width little-endian integer.
            // Assemble the raw bytes, then sign-extend to fill the full i64.
            let mut raw: i64 = 0;
            for i in 0..lcn_bytes {
                raw |= (bytes[p + i] as i64) << (8 * i);
            }
            let delta = sign_extend_i64(raw, lcn_bytes);
            p += lcn_bytes;
            let new_lcn = prev_lcn
                .checked_add(delta)
                .ok_or_else(|| format!("LCN delta overflow at offset {p}"))?;
            if new_lcn < 0 {
                return Err(format!("negative absolute LCN {new_lcn}"));
            }
            prev_lcn = new_lcn;
            Some(new_lcn as u64)
        };

        runs.push(DataRun {
            starting_vcn,
            length,
            lcn,
        });
        starting_vcn = starting_vcn.checked_add(length).ok_or("VCN overflow")?;
    }
    // Ran off the end without a 0x00 terminator — tolerate since the
    // attribute's `attr_length` can itself bound the list.
    Ok(runs)
}

/// Resolve a virtual cluster number to an absolute LCN by walking the
/// decoded runs. Returns `None` if `vcn` is past the end of all runs or
/// falls inside a sparse hole.
pub fn vcn_to_lcn(runs: &[DataRun], vcn: u64) -> Option<u64> {
    for r in runs {
        if vcn < r.starting_vcn {
            continue;
        }
        if vcn < r.starting_vcn + r.length {
            return r.lcn.map(|base| base + (vcn - r.starting_vcn));
        }
    }
    None
}

/// Encode a sequence of runs into NTFS mapping-pairs bytes. Inverse of
/// [`decode_runs`]. Appends a `0x00` terminator.
///
/// Requires `runs` be VCN-contiguous starting at 0 (the usual shape for
/// a complete attribute value). A gap between runs is rejected — sparse
/// regions must be expressed as an explicit `DataRun` with `lcn = None`.
pub fn encode_runs(runs: &[DataRun]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut prev_lcn: i64 = 0;
    let mut expected_vcn: u64 = 0;
    for (i, r) in runs.iter().enumerate() {
        if r.starting_vcn != expected_vcn {
            return Err(format!(
                "run {i} starts at VCN {} but previous runs cover up to {}",
                r.starting_vcn, expected_vcn
            ));
        }
        if r.length == 0 {
            return Err(format!("run {i} has zero length"));
        }

        // NTFS encodes mapping-pair length as a *signed* value (so the
        // high bit must always be 0 to keep it non-negative). 0x8000
        // (= 32768) fits in 2 unsigned bytes but reads as -32768 when
        // sign-extended; use the signed-byte calculation so we emit
        // 3 bytes (00 00 80) instead of 2 (00 80). The bug surfaced as
        // Event ID 55 "MFT contains a corrupted file record" against
        // $BadClus on a 128 MiB / 4 KiB-cluster volume (smoke diag
        // write-smoke-20260502-175747).
        let length_signed = i64::try_from(r.length)
            .map_err(|_| format!("run length {} exceeds i63 range", r.length))?;
        let length_bytes = signed_bytes_needed(length_signed);
        let (lcn_bytes, lcn_field) = match r.lcn {
            None => (0usize, 0i64),
            Some(abs) => {
                let abs_i =
                    i64::try_from(abs).map_err(|_| format!("LCN {abs} exceeds i64 range"))?;
                let delta = abs_i.checked_sub(prev_lcn).ok_or("LCN delta overflow")?;
                let nb = signed_bytes_needed(delta);
                (nb, delta)
            }
        };

        out.push(((lcn_bytes as u8) << 4) | (length_bytes as u8));
        for i in 0..length_bytes {
            out.push((r.length >> (8 * i)) as u8);
        }
        if lcn_bytes > 0 {
            for i in 0..lcn_bytes {
                out.push((lcn_field >> (8 * i)) as u8);
            }
            prev_lcn += lcn_field; // lcn_field is delta; accumulate absolute
        }

        expected_vcn = expected_vcn.checked_add(r.length).ok_or("VCN overflow")?;
    }
    out.push(0x00);
    Ok(out)
}

fn signed_bytes_needed(n: i64) -> usize {
    // Smallest N (1..=8) such that -2^(8N-1) <= n < 2^(8N-1).
    if n == 0 {
        return 1;
    }
    for n_bytes in 1usize..=8 {
        let half_range = 1i64 << (8 * n_bytes - 1);
        if n >= -half_range && n < half_range {
            return n_bytes;
        }
    }
    8
}

/// True if any VCN in `[vcn_start, vcn_start+n)` lies in a sparse hole
/// or past the end of the run list.
pub fn range_has_hole_or_past_end(runs: &[DataRun], vcn_start: u64, n_clusters: u64) -> bool {
    let end = vcn_start + n_clusters;
    let mut covered_to = vcn_start;
    for r in runs {
        if r.starting_vcn >= end {
            break;
        }
        let overlap_start = r.starting_vcn.max(vcn_start);
        let overlap_end = (r.starting_vcn + r.length).min(end);
        if overlap_end <= overlap_start {
            continue;
        }
        if r.lcn.is_none() {
            return true;
        }
        if overlap_start > covered_to {
            // gap between runs ⇒ hole (should not happen in well-formed
            // NTFS but be defensive).
            return true;
        }
        covered_to = overlap_end;
    }
    covered_to < end
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sign_extend_i64 ---------------------------------------------------

    #[test]
    fn sign_extend_positive_byte_unchanged() {
        // 0x64 = 100; high bit clear → no extension needed.
        assert_eq!(sign_extend_i64(0x64, 1), 100);
    }

    #[test]
    fn sign_extend_negative_byte_fills_upper_bits() {
        // 0xF6 = 246 unsigned; high bit set → sign-extend → -10.
        assert_eq!(sign_extend_i64(0xF6, 1), -10);
    }

    #[test]
    fn sign_extend_two_byte_negative() {
        // 0xFF9C = 65436 unsigned; should extend to -100.
        assert_eq!(sign_extend_i64(0xFF9C, 2), -100);
    }

    #[test]
    fn sign_extend_full_eight_bytes_is_identity() {
        // 8-byte values already fill i64 — extension is a no-op.
        assert_eq!(sign_extend_i64(-42i64, 8), -42);
        assert_eq!(sign_extend_i64(42, 8), 42);
    }

    // --- decode_runs: happy paths ------------------------------------------

    #[test]
    fn decode_empty_terminator_returns_no_runs() {
        assert_eq!(decode_runs(&[0x00]).unwrap(), Vec::<DataRun>::new());
    }

    #[test]
    fn decode_single_run_one_cluster_at_lcn_0() {
        // header 0x11 = 1 lcn byte, 1 length byte. length=1, lcn=0.
        let bytes = [0x11, 0x01, 0x00, 0x00];
        let runs = decode_runs(&bytes).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0],
            DataRun {
                starting_vcn: 0,
                length: 1,
                lcn: Some(0),
            }
        );
    }

    #[test]
    fn decode_two_runs_with_positive_lcn_delta() {
        // run0: length=2, lcn=5; run1: length=3, lcn=5+10=15.
        let bytes = [0x11, 0x02, 0x05, 0x11, 0x03, 0x0A, 0x00];
        let runs = decode_runs(&bytes).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].starting_vcn, 0);
        assert_eq!(runs[0].length, 2);
        assert_eq!(runs[0].lcn, Some(5));
        assert_eq!(runs[1].starting_vcn, 2);
        assert_eq!(runs[1].length, 3);
        assert_eq!(runs[1].lcn, Some(15));
    }

    #[test]
    fn decode_sparse_run_lcn_is_none() {
        // header 0x01 = 0 lcn bytes, 1 length byte ⇒ sparse hole.
        let bytes = [0x01, 0x05, 0x00];
        let runs = decode_runs(&bytes).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].length, 5);
        assert_eq!(runs[0].lcn, None);
    }

    #[test]
    fn decode_negative_lcn_delta_sign_extends() {
        // run0: length=2, lcn=100 (1 byte 0x64).
        // run1: length=2, lcn=100 + (-10) = 90 (1 signed byte 0xF6).
        let bytes = [0x11, 0x02, 0x64, 0x11, 0x02, 0xF6, 0x00];
        let runs = decode_runs(&bytes).unwrap();
        assert_eq!(runs[1].lcn, Some(90));
    }

    // --- decode_runs: errors -----------------------------------------------

    #[test]
    fn decode_rejects_zero_length_bytes_header() {
        // header 0x10 = 1 lcn byte, 0 length bytes.
        let err = decode_runs(&[0x10, 0x00]).unwrap_err();
        assert!(err.contains("length-byte-count is zero"), "{err}");
    }

    #[test]
    fn decode_rejects_zero_cluster_length() {
        // header 0x11, length-byte = 0 ⇒ run of zero clusters.
        let err = decode_runs(&[0x11, 0x00, 0x05]).unwrap_err();
        assert!(err.contains("zero-cluster length"), "{err}");
    }

    #[test]
    fn decode_rejects_header_extending_past_data() {
        // header says we need 2+2 bytes but only 1 follows.
        let err = decode_runs(&[0x22, 0xFF]).unwrap_err();
        assert!(err.contains("extends past data"), "{err}");
    }

    #[test]
    fn decode_rejects_run_landing_at_negative_absolute_lcn() {
        // First run sets prev_lcn=10. Second run delta = -100 ⇒ -90.
        let bytes = [0x11, 0x01, 0x0A, 0x11, 0x01, 0x9C, 0x00];
        let err = decode_runs(&bytes).unwrap_err();
        assert!(err.contains("negative absolute LCN"), "{err}");
    }

    // --- encode_runs: happy paths + round-trip -----------------------------

    #[test]
    fn encode_decode_round_trip_single_run() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 7,
            lcn: Some(42),
        }];
        let encoded = encode_runs(&runs).unwrap();
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded, runs);
    }

    #[test]
    fn encode_decode_round_trip_multi_run_with_sparse() {
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 4,
                lcn: Some(100),
            },
            DataRun {
                starting_vcn: 4,
                length: 8,
                lcn: None,
            },
            DataRun {
                starting_vcn: 12,
                length: 2,
                lcn: Some(120),
            },
        ];
        let encoded = encode_runs(&runs).unwrap();
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded, runs);
    }

    #[test]
    fn encode_rejects_vcn_gap_between_runs() {
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 2,
                lcn: Some(10),
            },
            DataRun {
                starting_vcn: 5,
                length: 1,
                lcn: Some(20),
            },
        ];
        let err = encode_runs(&runs).unwrap_err();
        assert!(err.contains("starts at VCN"), "{err}");
    }

    #[test]
    fn encode_rejects_zero_length_run() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 0,
            lcn: Some(0),
        }];
        let err = encode_runs(&runs).unwrap_err();
        assert!(err.contains("zero length"), "{err}");
    }

    // Regression: length 0x8000 (= 32768) must encode with 3 bytes, not
    // 2 — sign-extension would otherwise read it back as -32768. See
    // the "Event ID 55 against $BadClus" history in encode_runs's
    // doc-comment. This pins the fix.
    #[test]
    fn encode_length_32768_uses_three_bytes_not_two() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 0x8000,
            lcn: Some(0),
        }];
        let encoded = encode_runs(&runs).unwrap();
        // header byte + 3 length bytes (sign-extended) + 1 lcn byte + 0x00.
        // header low nibble = 3 (length bytes), high nibble = 1 (lcn byte).
        assert_eq!(encoded[0], 0x13, "header was {:#04x}", encoded[0]);
        // Round-trip must give back exactly 0x8000.
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded[0].length, 0x8000);
    }

    // --- vcn_to_lcn --------------------------------------------------------

    #[test]
    fn vcn_to_lcn_inside_first_run() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 10,
            lcn: Some(100),
        }];
        assert_eq!(vcn_to_lcn(&runs, 0), Some(100));
        assert_eq!(vcn_to_lcn(&runs, 5), Some(105));
        assert_eq!(vcn_to_lcn(&runs, 9), Some(109));
    }

    #[test]
    fn vcn_to_lcn_in_sparse_hole_is_none() {
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 2,
                lcn: Some(100),
            },
            DataRun {
                starting_vcn: 2,
                length: 5,
                lcn: None,
            },
        ];
        assert_eq!(vcn_to_lcn(&runs, 1), Some(101));
        assert_eq!(vcn_to_lcn(&runs, 3), None);
        // Past end of all runs.
        assert_eq!(vcn_to_lcn(&runs, 100), None);
    }

    // --- range_has_hole_or_past_end ----------------------------------------

    #[test]
    fn range_fully_inside_allocated_run_has_no_hole() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 10,
            lcn: Some(100),
        }];
        assert!(!range_has_hole_or_past_end(&runs, 2, 5));
    }

    #[test]
    fn range_straddling_sparse_run_has_hole() {
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 4,
                lcn: Some(100),
            },
            DataRun {
                starting_vcn: 4,
                length: 2,
                lcn: None,
            },
        ];
        assert!(range_has_hole_or_past_end(&runs, 3, 2));
    }

    #[test]
    fn range_past_end_of_runs_is_hole() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 4,
            lcn: Some(100),
        }];
        assert!(range_has_hole_or_past_end(&runs, 2, 5));
    }

    // --- additional encode/decode edge cases (Phase 2.4) -------------------

    #[test]
    fn encode_negative_lcn_delta_round_trips() {
        // run0 at LCN 100, run1 at LCN 90 (delta = -10, signed 1 byte).
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 2,
                lcn: Some(100),
            },
            DataRun {
                starting_vcn: 2,
                length: 3,
                lcn: Some(90),
            },
        ];
        let encoded = encode_runs(&runs).unwrap();
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded, runs);
    }

    #[test]
    fn decode_no_terminator_tolerates_buffer_end() {
        // A single run with no 0x00 terminator — tolerated by spec since
        // `attr_length` can bound the list. The decoder must not crash.
        let bytes = [0x11u8, 0x01, 0x05]; // header + length=1 + lcn=5, no 0x00
        let runs = decode_runs(&bytes).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].lcn, Some(5));
    }

    #[test]
    fn encode_large_lcn_requires_five_byte_offset() {
        // LCN = 2^32 + 1. The offset field must use 5 bytes to represent it.
        let lcn: u64 = (1u64 << 32) + 1;
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 1,
            lcn: Some(lcn),
        }];
        let encoded = encode_runs(&runs).unwrap();
        // Decode the header byte to check lcn_bytes.
        let lcn_bytes = (encoded[0] >> 4) as usize;
        assert!(
            lcn_bytes >= 5,
            "LCN > 2^32 needs at least 5 bytes; got {lcn_bytes}"
        );
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded[0].lcn, Some(lcn));
    }

    #[test]
    fn encode_large_run_length_requires_five_bytes() {
        // Run length = 2^32 + 1. Must not be truncated to 32 bits.
        let big_len: u64 = (1u64 << 32) + 1;
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: big_len,
            lcn: Some(0),
        }];
        let encoded = encode_runs(&runs).unwrap();
        let length_bytes = (encoded[0] & 0x0F) as usize;
        assert!(
            length_bytes >= 5,
            "length > 2^32 needs at least 5 bytes; got {length_bytes}"
        );
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded[0].length, big_len);
    }

    #[test]
    fn vcn_to_lcn_past_end_of_all_runs_returns_none() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 5,
            lcn: Some(100),
        }];
        // VCN 5 is past the end (run covers [0..5)).
        assert_eq!(vcn_to_lcn(&runs, 5), None);
        assert_eq!(vcn_to_lcn(&runs, 100), None);
    }

    #[test]
    fn vcn_to_lcn_exactly_at_run_end_is_none() {
        // Run covers VCNs [2..7). VCN 7 is not covered.
        let runs = vec![DataRun {
            starting_vcn: 2,
            length: 5,
            lcn: Some(50),
        }];
        assert_eq!(vcn_to_lcn(&runs, 6), Some(54)); // last VCN in run
        assert_eq!(vcn_to_lcn(&runs, 7), None); // one past end
    }

    #[test]
    fn vcn_to_lcn_in_second_of_two_runs() {
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 4,
                lcn: Some(10),
            },
            DataRun {
                starting_vcn: 4,
                length: 6,
                lcn: Some(20),
            },
        ];
        assert_eq!(vcn_to_lcn(&runs, 3), Some(13)); // last in first run
        assert_eq!(vcn_to_lcn(&runs, 4), Some(20)); // first in second run
        assert_eq!(vcn_to_lcn(&runs, 9), Some(25)); // last in second run
    }

    #[test]
    fn range_has_hole_empty_run_list_is_always_hole() {
        // No runs at all — every range is "past end".
        assert!(range_has_hole_or_past_end(&[], 0, 1));
        assert!(range_has_hole_or_past_end(&[], 0, 100));
    }

    #[test]
    fn range_has_hole_exact_coverage_has_no_hole() {
        // Range exactly matches one run — no hole.
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 8,
            lcn: Some(50),
        }];
        assert!(!range_has_hole_or_past_end(&runs, 0, 8));
    }

    #[test]
    fn encode_roundtrip_all_single_byte_lengths_1_to_7() {
        // Run lengths 1..=7 each fit in 1 byte. Verify round-trip for all.
        for length in 1u64..=7 {
            let runs = vec![DataRun {
                starting_vcn: 0,
                length,
                lcn: Some(1),
            }];
            let encoded = encode_runs(&runs).unwrap();
            let decoded = decode_runs(&encoded).unwrap();
            assert_eq!(decoded[0].length, length, "length={length}");
        }
    }

    #[test]
    fn encode_roundtrip_boundary_lengths() {
        // Boundary values that force 2-byte vs 3-byte length encoding.
        for length in [127u64, 128, 255, 256, 32767, 32768] {
            let runs = vec![DataRun {
                starting_vcn: 0,
                length,
                lcn: Some(1),
            }];
            let encoded = encode_runs(&runs).unwrap();
            let decoded = decode_runs(&encoded).unwrap();
            assert_eq!(decoded[0].length, length, "boundary length={length}");
        }
    }

    #[test]
    fn encode_five_contiguous_runs_roundtrip() {
        let runs: Vec<DataRun> = (0..5)
            .map(|i| DataRun {
                starting_vcn: i * 10,
                length: 10,
                lcn: Some(100 + i * 15),
            })
            .collect();
        let encoded = encode_runs(&runs).unwrap();
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded, runs);
    }

    #[test]
    fn encode_alternating_sparse_and_real_runs() {
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 3,
                lcn: Some(50),
            },
            DataRun {
                starting_vcn: 3,
                length: 2,
                lcn: None,
            }, // sparse
            DataRun {
                starting_vcn: 5,
                length: 4,
                lcn: Some(80),
            },
            DataRun {
                starting_vcn: 9,
                length: 1,
                lcn: None,
            }, // sparse
            DataRun {
                starting_vcn: 10,
                length: 2,
                lcn: Some(200),
            },
        ];
        let encoded = encode_runs(&runs).unwrap();
        let decoded = decode_runs(&encoded).unwrap();
        assert_eq!(decoded, runs);
    }
}
