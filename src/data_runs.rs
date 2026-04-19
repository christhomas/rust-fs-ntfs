//! Encode / decode NTFS mapping-pair (data run) lists. A data run is a
//! `(length, lcn)` pair — a contiguous span of clusters. The list for a
//! non-resident attribute lives in the MFT record at the attribute's
//! `mapping_pairs_offset` and runs until the first 0x00 byte (or the
//! attribute's length).
//!
//! Reference (no GPL code consulted):
//! [Flatcap Data Runs](https://flatcap.github.io/linux-ntfs/ntfs/concepts/data_runs.html).
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
            // Signed delta, little-endian, variable width. Sign-extend.
            let mut delta: i64 = 0;
            for i in 0..lcn_bytes {
                delta |= (bytes[p + i] as i64) << (8 * i);
            }
            let sign_bit_pos = 8 * lcn_bytes - 1;
            if delta & (1i64 << sign_bit_pos) != 0 {
                let mask = !((1i64 << (sign_bit_pos + 1)).wrapping_sub(1));
                delta |= mask;
            }
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

        let length_bytes = unsigned_bytes_needed(r.length);
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

fn unsigned_bytes_needed(n: u64) -> usize {
    if n == 0 {
        return 1;
    }
    let bits = 64 - n.leading_zeros() as usize;
    bits.div_ceil(8)
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
