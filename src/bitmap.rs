//! Cluster allocator against the volume `$Bitmap` (MFT record 6).
//!
//! `$Bitmap`'s unnamed `$DATA` is a packed bit-per-cluster array: bit
//! `k` (within byte `k/8`, bit position `k%8`) is 1 if cluster `k` is
//! allocated, 0 if free. On volumes large enough for `$Bitmap` to be
//! non-resident (the usual case), the bitmap itself is stored in
//! clusters reached via a data-run list.
//!
//! This module reads + mutates that bitmap. It reuses the
//! `data_runs::decode_runs` walker to translate cluster-range VCNs to
//! on-disk byte offsets, then reads / writes the bits directly.
//!
//! References (no GPL code consulted):
//! * [Flatcap $Bitmap](https://flatcap.github.io/linux-ntfs/ntfs/files/bitmap.html)
//! * MS-FSCC
//!
//! **Scope.** Allocate / free contiguous cluster ranges, find the first
//! contiguous free run of `N` clusters. No best-fit or locality
//! heuristics — first-fit linear scan. Good enough for W2.

use crate::attr_io::{self, AttrType};
use crate::data_runs::{self, DataRun};
use crate::mft_io::{read_boot_params, read_mft_record, BootParams};

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const BITMAP_RECORD_NUMBER: u64 = 6;

/// Info needed to reach `$Bitmap`'s data on disk.
pub struct BitmapLocation {
    pub params: BootParams,
    pub runs: Vec<DataRun>,
    /// Total bitmap length in bits (= total clusters on the volume).
    pub total_bits: u64,
    /// Logical byte length of `$Bitmap`'s $DATA.
    pub value_length: u64,
}

pub fn locate_bitmap(image: &Path) -> Result<BitmapLocation, String> {
    let (params, record) = read_mft_record(image, BITMAP_RECORD_NUMBER)?;
    let loc = attr_io::find_attribute(&record, AttrType::Data, None)
        .ok_or_else(|| "$Bitmap has no unnamed $DATA".to_string())?;
    if loc.is_resident {
        return Err("resident $Bitmap unsupported (volume too small?)".to_string());
    }
    let mapping_offset = loc
        .non_resident_mapping_pairs_offset
        .ok_or("missing mapping_pairs_offset")? as usize;
    let mapping_start = loc.attr_offset + mapping_offset;
    let mapping_end = loc.attr_offset + loc.attr_length;
    if mapping_end > record.len() || mapping_start >= mapping_end {
        return Err("$Bitmap mapping_pairs out of record".to_string());
    }
    let runs = data_runs::decode_runs(&record[mapping_start..mapping_end])?;
    let value_length = loc.non_resident_value_length.ok_or("no value_length")?;
    let total_clusters = params.file_record_size; // just to silence unused warn; replaced below
    let _ = total_clusters;
    // total bits = value_length * 8 BUT real cluster count = value_length * 8
    // Both Windows and Flatcap define the count as bitsize = value_length * 8.
    Ok(BitmapLocation {
        params,
        runs,
        total_bits: value_length.saturating_mul(8),
        value_length,
    })
}

/// Read a contiguous bit range `[start..start+nbits)` from `$Bitmap`.
/// Returns as a `Vec<u8>` with bit `k` of the range at byte `k/8` bit `k%8`.
/// Used primarily for testing.
pub fn read_range(
    image: &Path,
    bm: &BitmapLocation,
    start: u64,
    nbits: u64,
) -> Result<Vec<u8>, String> {
    if start + nbits > bm.total_bits {
        return Err(format!(
            "range [{start}..{}] exceeds total_bits {}",
            start + nbits,
            bm.total_bits
        ));
    }
    let start_byte = start / 8;
    let end_byte = (start + nbits).div_ceil(8);
    let bytes = read_bitmap_bytes(image, bm, start_byte, end_byte - start_byte)?;
    Ok(bytes)
}

/// Find the first contiguous run of `n_clusters` free clusters starting
/// at or after `hint_lcn`. Returns the LCN of the first cluster of the
/// run, or `None` if the volume doesn't have `n_clusters` contiguous
/// free clusters.
pub fn find_free_run(
    image: &Path,
    bm: &BitmapLocation,
    n_clusters: u64,
    hint_lcn: u64,
) -> Result<Option<u64>, String> {
    if n_clusters == 0 {
        return Err("n_clusters = 0".to_string());
    }
    // Simple linear scan starting at hint, wrapping around. Read bytes
    // in chunks so we don't ever hold the whole bitmap in memory.
    const CHUNK: u64 = 64 * 1024;
    let total = bm.total_bits;

    let mut scan_start = hint_lcn.min(total);

    // Two passes: [hint .. end), then [0 .. hint).
    for (begin, finish) in [(scan_start, total), (0, scan_start.min(total))] {
        scan_start = begin; // silence unused-assignment warning
        let _ = scan_start;
        let mut run_start: Option<u64> = None;
        let mut run_len: u64 = 0;
        let mut lcn = begin;
        while lcn < finish {
            let chunk_bits = CHUNK.min(finish - lcn);
            let first_byte = lcn / 8;
            let bit_off_in_byte = (lcn % 8) as u8;
            let end_bit = lcn + chunk_bits;
            let last_byte_exclusive = end_bit.div_ceil(8);
            let bytes = read_bitmap_bytes(image, bm, first_byte, last_byte_exclusive - first_byte)?;
            let mut cursor = lcn;
            let mut byte_idx = 0usize;
            let mut bit_in_byte = bit_off_in_byte;
            while cursor < end_bit {
                let byte = bytes[byte_idx];
                let free = (byte >> bit_in_byte) & 1 == 0;
                if free {
                    if run_start.is_none() {
                        run_start = Some(cursor);
                        run_len = 0;
                    }
                    run_len += 1;
                    if run_len >= n_clusters {
                        return Ok(run_start);
                    }
                } else {
                    run_start = None;
                    run_len = 0;
                }
                cursor += 1;
                bit_in_byte += 1;
                if bit_in_byte == 8 {
                    bit_in_byte = 0;
                    byte_idx += 1;
                }
            }
            lcn = end_bit;
        }
    }
    Ok(None)
}

/// Flip the bits for `[lcn..lcn+n)` to 1 (allocated). Fails if any bit
/// in the range is already 1.
pub fn allocate(image: &Path, bm: &BitmapLocation, lcn: u64, n: u64) -> Result<(), String> {
    mutate_bits(image, bm, lcn, n, true)
}

/// Flip the bits for `[lcn..lcn+n)` to 0 (free). Fails if any bit in
/// the range is already 0.
pub fn free(image: &Path, bm: &BitmapLocation, lcn: u64, n: u64) -> Result<(), String> {
    mutate_bits(image, bm, lcn, n, false)
}

fn mutate_bits(
    image: &Path,
    bm: &BitmapLocation,
    lcn: u64,
    n: u64,
    set: bool,
) -> Result<(), String> {
    if n == 0 {
        return Ok(());
    }
    if lcn + n > bm.total_bits {
        return Err(format!(
            "range [{lcn}..{}] exceeds total_bits {}",
            lcn + n,
            bm.total_bits
        ));
    }
    let first_byte = lcn / 8;
    let end_byte_excl = (lcn + n).div_ceil(8);
    let mut bytes = read_bitmap_bytes(image, bm, first_byte, end_byte_excl - first_byte)?;

    for i in 0..n {
        let bit = lcn + i - first_byte * 8;
        let byte_idx = (bit / 8) as usize;
        let bit_in_byte = (bit % 8) as u8;
        let cur = (bytes[byte_idx] >> bit_in_byte) & 1 != 0;
        if set && cur {
            return Err(format!("cluster {} already allocated", lcn + i));
        }
        if !set && !cur {
            return Err(format!("cluster {} already free", lcn + i));
        }
        if set {
            bytes[byte_idx] |= 1 << bit_in_byte;
        } else {
            bytes[byte_idx] &= !(1 << bit_in_byte);
        }
    }
    write_bitmap_bytes(image, bm, first_byte, &bytes)?;
    Ok(())
}

// -- byte-level bitmap I/O -------------------------------------------------

fn read_bitmap_bytes(
    image: &Path,
    bm: &BitmapLocation,
    start_byte: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len as usize];
    let mut cursor_in_out = 0usize;
    let mut file_offset = start_byte;
    let end = start_byte + len;
    let cluster_size = bm.params.cluster_size;
    let mut f = std::fs::File::open(image).map_err(|e| format!("open ro: {e}"))?;

    while file_offset < end {
        let vcn = file_offset / cluster_size;
        let off_in_cluster = file_offset % cluster_size;
        let run = bm
            .runs
            .iter()
            .find(|r| vcn >= r.starting_vcn && vcn < r.starting_vcn + r.length)
            .ok_or_else(|| format!("VCN {vcn} not mapped in $Bitmap"))?;
        let lcn = run
            .lcn
            .ok_or_else(|| format!("VCN {vcn} is in sparse $Bitmap run?"))?;
        let run_end_vcn = run.starting_vcn + run.length;
        let run_end_offset = run_end_vcn * cluster_size;
        let max_this_run = run_end_offset - file_offset;
        let chunk = max_this_run.min(end - file_offset) as usize;

        let disk_offset = (lcn + (vcn - run.starting_vcn)) * cluster_size + off_in_cluster;
        f.seek(SeekFrom::Start(disk_offset))
            .map_err(|e| format!("seek bitmap: {e}"))?;
        f.read_exact(&mut out[cursor_in_out..cursor_in_out + chunk])
            .map_err(|e| format!("read bitmap: {e}"))?;

        cursor_in_out += chunk;
        file_offset += chunk as u64;
    }
    Ok(out)
}

fn write_bitmap_bytes(
    image: &Path,
    bm: &BitmapLocation,
    start_byte: u64,
    data: &[u8],
) -> Result<(), String> {
    let cluster_size = bm.params.cluster_size;
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .map_err(|e| format!("open rw: {e}"))?;
    let mut cursor = 0usize;
    let mut file_offset = start_byte;
    let end = start_byte + data.len() as u64;

    while file_offset < end {
        let vcn = file_offset / cluster_size;
        let off_in_cluster = file_offset % cluster_size;
        let run = bm
            .runs
            .iter()
            .find(|r| vcn >= r.starting_vcn && vcn < r.starting_vcn + r.length)
            .ok_or_else(|| format!("VCN {vcn} not mapped"))?;
        let lcn = run.lcn.ok_or_else(|| format!("VCN {vcn} in sparse run"))?;
        let run_end_vcn = run.starting_vcn + run.length;
        let run_end_offset = run_end_vcn * cluster_size;
        let max_this_run = run_end_offset - file_offset;
        let chunk = max_this_run.min(end - file_offset) as usize;

        let disk_offset = (lcn + (vcn - run.starting_vcn)) * cluster_size + off_in_cluster;
        f.seek(SeekFrom::Start(disk_offset))
            .map_err(|e| format!("seek write: {e}"))?;
        f.write_all(&data[cursor..cursor + chunk])
            .map_err(|e| format!("write bitmap: {e}"))?;

        cursor += chunk;
        file_offset += chunk as u64;
    }
    f.sync_all().map_err(|e| format!("fsync: {e}"))?;
    Ok(())
}

/// Is cluster `lcn` marked allocated?
pub fn is_allocated(image: &Path, bm: &BitmapLocation, lcn: u64) -> Result<bool, String> {
    if lcn >= bm.total_bits {
        return Err(format!(
            "LCN {lcn} out of range (total_bits {})",
            bm.total_bits
        ));
    }
    let byte_idx = lcn / 8;
    let bit = (lcn % 8) as u8;
    let bytes = read_bitmap_bytes(image, bm, byte_idx, 1)?;
    Ok((bytes[0] >> bit) & 1 != 0)
}

#[allow(dead_code)]
fn _touch_unused_imports() {
    // Keep `read_boot_params` import alive for future helpers that may
    // want to re-parse params without going through locate_bitmap.
    let _ = read_boot_params;
}
