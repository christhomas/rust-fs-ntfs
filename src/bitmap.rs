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
//! References (no GPL code consulted): $Bitmap layout per Windows
//! Internals 7th ed. ch. "NTFS On-Disk Structure" and MS-FSCC.
//!
//! **Scope.** Allocate / free contiguous cluster ranges, find the first
//! contiguous free run of `N` clusters. No best-fit or locality
//! heuristics — first-fit linear scan. Good enough for W2.

use crate::attr_io::{self, AttrType};
use crate::block_io::{BlockIo, PathIo};
use crate::data_runs::{self, DataRun};
use crate::mft_io::{read_mft_record_io, BootParams};

use std::path::Path;

const BITMAP_RECORD_NUMBER: u64 = 6;

/// Returns true iff cluster bit `bit` (0–7) within `byte` is set (= allocated).
fn bit_is_set(byte: u8, bit: u8) -> bool {
    (byte >> bit) & 1 != 0
}

/// Set bit `bit` (0–7) within `bytes[idx]` to 1 (mark cluster allocated).
fn set_bit(bytes: &mut [u8], idx: usize, bit: u8) {
    bytes[idx] |= 1 << bit;
}

/// Clear bit `bit` (0–7) within `bytes[idx]` to 0 (mark cluster free).
fn clear_bit(bytes: &mut [u8], idx: usize, bit: u8) {
    bytes[idx] &= !(1u8 << bit);
}

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
    let mut io = PathIo::open_ro(image)?;
    locate_bitmap_io(&mut io)
}

pub fn locate_bitmap_io<T: BlockIo + ?Sized>(io: &mut T) -> Result<BitmapLocation, String> {
    let (params, record) = read_mft_record_io(io, BITMAP_RECORD_NUMBER)?;
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
    // total bits = value_length * 8; each bit covers one cluster.
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
    let mut io = PathIo::open_ro(image)?;
    read_range_io(&mut io, bm, start, nbits)
}

pub fn read_range_io<T: BlockIo + ?Sized>(
    io: &mut T,
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
    read_bitmap_bytes_io(io, bm, start_byte, end_byte - start_byte)
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
    let mut io = PathIo::open_ro(image)?;
    find_free_run_io(&mut io, bm, n_clusters, hint_lcn)
}

pub fn find_free_run_io<T: BlockIo + ?Sized>(
    io: &mut T,
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
            let bytes = read_bitmap_bytes_io(io, bm, first_byte, last_byte_exclusive - first_byte)?;
            let mut cursor = lcn;
            let mut byte_idx = 0usize;
            let mut bit_in_byte = bit_off_in_byte;
            while cursor < end_bit {
                let byte = bytes[byte_idx];
                let free = !bit_is_set(byte, bit_in_byte);
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
    let mut io = PathIo::open_rw(image)?;
    allocate_io(&mut io, bm, lcn, n)
}

pub fn allocate_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    lcn: u64,
    n: u64,
) -> Result<(), String> {
    mutate_bits_io(io, bm, lcn, n, true)
}

/// Flip the bits for `[lcn..lcn+n)` to 0 (free). Fails if any bit in
/// the range is already 0.
pub fn free(image: &Path, bm: &BitmapLocation, lcn: u64, n: u64) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    free_io(&mut io, bm, lcn, n)
}

pub fn free_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    lcn: u64,
    n: u64,
) -> Result<(), String> {
    mutate_bits_io(io, bm, lcn, n, false)
}

fn mutate_bits_io<T: BlockIo + ?Sized>(
    io: &mut T,
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
    let mut bytes = read_bitmap_bytes_io(io, bm, first_byte, end_byte_excl - first_byte)?;

    for i in 0..n {
        let bit = lcn + i - first_byte * 8;
        let byte_idx = (bit / 8) as usize;
        let bit_in_byte = (bit % 8) as u8;
        let cur = bit_is_set(bytes[byte_idx], bit_in_byte);
        if set && cur {
            return Err(format!("cluster {} already allocated", lcn + i));
        }
        if !set && !cur {
            return Err(format!("cluster {} already free", lcn + i));
        }
        if set {
            set_bit(&mut bytes, byte_idx, bit_in_byte);
        } else {
            clear_bit(&mut bytes, byte_idx, bit_in_byte);
        }
    }
    write_bitmap_bytes_io(io, bm, first_byte, &bytes)?;
    Ok(())
}

// -- byte-level bitmap I/O -------------------------------------------------

fn read_bitmap_bytes_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    start_byte: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len as usize];
    let mut cursor_in_out = 0usize;
    let mut file_offset = start_byte;
    let end = start_byte + len;
    let cluster_size = bm.params.cluster_size;

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
        io.read_exact_at(disk_offset, &mut out[cursor_in_out..cursor_in_out + chunk])
            .map_err(|e| format!("read bitmap: {e}"))?;

        cursor_in_out += chunk;
        file_offset += chunk as u64;
    }
    Ok(out)
}

fn write_bitmap_bytes_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    start_byte: u64,
    data: &[u8],
) -> Result<(), String> {
    let cluster_size = bm.params.cluster_size;
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
        io.write_all_at(disk_offset, &data[cursor..cursor + chunk])
            .map_err(|e| format!("write bitmap: {e}"))?;

        cursor += chunk;
        file_offset += chunk as u64;
    }
    io.sync()?;
    Ok(())
}

/// Count free clusters in `$Bitmap`. Scans the whole bitmap once.
pub fn count_free(image: &Path, bm: &BitmapLocation) -> Result<u64, String> {
    let mut io = PathIo::open_ro(image)?;
    count_free_io(&mut io, bm)
}

pub fn count_free_io<T: BlockIo + ?Sized>(io: &mut T, bm: &BitmapLocation) -> Result<u64, String> {
    let total_bytes = bm.value_length;
    let bytes = read_bitmap_bytes_io(io, bm, 0, total_bytes)?;
    let set: u64 = bytes.iter().map(|b| b.count_ones() as u64).sum();
    // Bits past total_bits (if any, due to padding) are required to be
    // zero by the spec; count_ones is safe to subtract from total.
    Ok(bm.total_bits.saturating_sub(set))
}

/// Is cluster `lcn` marked allocated?
pub fn is_allocated(image: &Path, bm: &BitmapLocation, lcn: u64) -> Result<bool, String> {
    let mut io = PathIo::open_ro(image)?;
    is_allocated_io(&mut io, bm, lcn)
}

pub fn is_allocated_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    lcn: u64,
) -> Result<bool, String> {
    if lcn >= bm.total_bits {
        return Err(format!(
            "LCN {lcn} out of range (total_bits {})",
            bm.total_bits
        ));
    }
    let byte_idx = lcn / 8;
    let bit = (lcn % 8) as u8;
    let bytes = read_bitmap_bytes_io(io, bm, byte_idx, 1)?;
    Ok(bit_is_set(bytes[0], bit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_io::BlockIo;

    struct MemDev {
        buf: Vec<u8>,
    }
    impl MemDev {
        fn new(size: usize) -> Self {
            Self {
                buf: vec![0u8; size],
            }
        }
    }
    impl BlockIo for MemDev {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            let off = offset as usize;
            if off + buf.len() > self.buf.len() {
                return Err("read past end".into());
            }
            buf.copy_from_slice(&self.buf[off..off + buf.len()]);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            let off = offset as usize;
            if off + buf.len() > self.buf.len() {
                return Err("write past end".into());
            }
            self.buf[off..off + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn size(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    /// Build a `BitmapLocation` pointing to a single contiguous run
    /// starting at LCN 1 (= file offset = cluster_size). Bitmap covers
    /// `n_bytes` of $Bitmap data ⇒ `n_bytes*8` clusters total.
    fn make_bm(cluster_size: u64, n_bytes: u64) -> BitmapLocation {
        BitmapLocation {
            params: BootParams {
                bytes_per_sector: 512,
                sectors_per_cluster: cluster_size / 512,
                cluster_size,
                mft_lcn: 0,
                file_record_size: 1024,
                total_sectors: 0,
                serial_number: 0,
                oem_id: *b"NTFS    ",
            },
            // One run: bitmap lives at LCN 1, one cluster's worth.
            runs: vec![DataRun {
                starting_vcn: 0,
                length: 1,
                lcn: Some(1),
            }],
            total_bits: n_bytes * 8,
            value_length: n_bytes,
        }
    }

    // --- is_allocated_io ---------------------------------------------------

    #[test]
    fn is_allocated_io_reads_set_bit_as_true() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 4 bytes ⇒ 32 clusters total.
                                   // Set bit 5 of byte 0 (cluster 5) in the bitmap (stored at offset 4096).
        dev.buf[4096] = 0b0010_0000;
        assert!(is_allocated_io(&mut dev, &bm, 5).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm, 4).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm, 6).unwrap());
    }

    #[test]
    fn is_allocated_io_out_of_range_errors() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        let err = is_allocated_io(&mut dev, &bm, 32).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    // --- allocate_io / free_io --------------------------------------------

    #[test]
    fn allocate_io_sets_bits_in_range() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        allocate_io(&mut dev, &bm, 3, 5).unwrap();
        for lcn in 3..8 {
            assert!(is_allocated_io(&mut dev, &bm, lcn).unwrap(), "lcn {lcn}");
        }
        // Neighbours untouched.
        assert!(!is_allocated_io(&mut dev, &bm, 2).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm, 8).unwrap());
    }

    #[test]
    fn allocate_io_rejects_already_allocated_cluster() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        allocate_io(&mut dev, &bm, 5, 1).unwrap();
        let err = allocate_io(&mut dev, &bm, 5, 1).unwrap_err();
        assert!(err.contains("already allocated"), "{err}");
    }

    #[test]
    fn free_io_clears_bits_in_range() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        allocate_io(&mut dev, &bm, 0, 16).unwrap();
        free_io(&mut dev, &bm, 4, 8).unwrap();
        for lcn in 0..4 {
            assert!(is_allocated_io(&mut dev, &bm, lcn).unwrap());
        }
        for lcn in 4..12 {
            assert!(!is_allocated_io(&mut dev, &bm, lcn).unwrap());
        }
        for lcn in 12..16 {
            assert!(is_allocated_io(&mut dev, &bm, lcn).unwrap());
        }
    }

    #[test]
    fn free_io_rejects_already_free_cluster() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        let err = free_io(&mut dev, &bm, 5, 1).unwrap_err();
        assert!(err.contains("already free"), "{err}");
    }

    // --- count_free_io -----------------------------------------------------

    #[test]
    fn count_free_io_reports_zeros_minus_ones() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        // 32 total bits. Allocate 11.
        allocate_io(&mut dev, &bm, 0, 11).unwrap();
        assert_eq!(count_free_io(&mut dev, &bm).unwrap(), 32 - 11);
    }

    // --- find_free_run_io --------------------------------------------------

    #[test]
    fn find_free_run_io_picks_first_fit_starting_at_hint() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        // Allocate clusters 0..10, leaving 10..32 free.
        allocate_io(&mut dev, &bm, 0, 10).unwrap();
        // Looking for 4 contiguous starting from hint=0 → must land at 10.
        let lcn = find_free_run_io(&mut dev, &bm, 4, 0).unwrap();
        assert_eq!(lcn, Some(10));
    }

    #[test]
    fn find_free_run_io_returns_none_if_not_enough_contiguous_free() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        // Allocate every other cluster — no 2-contiguous run available.
        for lcn in (0..32).step_by(2) {
            allocate_io(&mut dev, &bm, lcn, 1).unwrap();
        }
        let res = find_free_run_io(&mut dev, &bm, 2, 0).unwrap();
        assert_eq!(res, None);
    }

    // --- bit helpers ----------------------------------------------------------

    #[test]
    fn bit_is_set_reads_individual_bits() {
        assert!(bit_is_set(0b0000_0001, 0));
        assert!(bit_is_set(0b1000_0000, 7));
        assert!(!bit_is_set(0b1111_1110, 0));
        assert!(!bit_is_set(0b0111_1111, 7));
    }

    #[test]
    fn set_bit_sets_only_target_bit() {
        let mut bytes = [0u8; 2];
        set_bit(&mut bytes, 0, 3);
        assert_eq!(bytes[0], 0b0000_1000);
        assert_eq!(bytes[1], 0);
    }

    #[test]
    fn clear_bit_clears_only_target_bit() {
        let mut bytes = [0xFFu8; 2];
        clear_bit(&mut bytes, 0, 3);
        assert_eq!(bytes[0], 0b1111_0111);
        assert_eq!(bytes[1], 0xFF);
    }

    #[test]
    fn find_free_run_io_wraps_around_to_below_hint() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        // Free clusters only in [0..3); past 3 everything allocated.
        allocate_io(&mut dev, &bm, 3, 29).unwrap();
        // Hint at end of bitmap; should wrap and find free run at 0.
        let lcn = find_free_run_io(&mut dev, &bm, 2, 25).unwrap();
        assert_eq!(lcn, Some(0));
    }

    // --- additional edge cases -------------------------------------------

    #[test]
    fn allocate_io_crossing_byte_boundary() {
        // Allocate a run that spans byte 0 and byte 1 of the bitmap.
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 32 clusters
        allocate_io(&mut dev, &bm, 6, 4).unwrap(); // clusters 6,7,8,9
                                                   // Byte 0 should have bits 6 and 7 set (0b1100_0000).
        let byte0 = dev.buf[4096];
        let byte1 = dev.buf[4097];
        assert_eq!(byte0, 0b1100_0000, "bits 6-7 of byte 0");
        assert_eq!(byte1, 0b0000_0011, "bits 0-1 of byte 1 (clusters 8-9)");
    }

    #[test]
    fn allocate_io_then_free_io_full_roundtrip() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        allocate_io(&mut dev, &bm, 0, 32).unwrap();
        assert_eq!(count_free_io(&mut dev, &bm).unwrap(), 0);
        free_io(&mut dev, &bm, 0, 32).unwrap();
        assert_eq!(count_free_io(&mut dev, &bm).unwrap(), 32);
    }

    #[test]
    fn find_free_run_zero_clusters_returns_error() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        let err = find_free_run_io(&mut dev, &bm, 0, 0).unwrap_err();
        assert!(err.contains("0"), "zero-cluster request is invalid: {err}");
    }

    #[test]
    fn find_free_run_hint_beyond_total_clamps_correctly() {
        // hint_lcn > total_bits: the function should clamp and still find a run.
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 32 clusters
        let lcn = find_free_run_io(&mut dev, &bm, 1, 999).unwrap();
        assert!(
            lcn.is_some(),
            "clamped hint should still find a free cluster"
        );
    }

    #[test]
    fn find_free_run_all_allocated_returns_none() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 32 clusters
        allocate_io(&mut dev, &bm, 0, 32).unwrap();
        assert_eq!(find_free_run_io(&mut dev, &bm, 1, 0).unwrap(), None);
    }

    #[test]
    fn find_free_run_exactly_one_cluster_free_at_end() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 32 clusters
                                   // Allocate all except the last cluster.
        allocate_io(&mut dev, &bm, 0, 31).unwrap();
        let lcn = find_free_run_io(&mut dev, &bm, 1, 0).unwrap();
        assert_eq!(lcn, Some(31));
    }

    #[test]
    fn allocate_io_out_of_range_returns_error() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 32 clusters
        let err = allocate_io(&mut dev, &bm, 30, 4).unwrap_err(); // 30+4=34 > 32
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn range_has_hole_returns_false_for_allocated_range() {
        use crate::data_runs::DataRun;
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 10,
            lcn: Some(100),
        }];
        assert!(!crate::data_runs::range_has_hole_or_past_end(&runs, 0, 10));
        assert!(!crate::data_runs::range_has_hole_or_past_end(&runs, 3, 5));
    }

    #[test]
    fn bit_helpers_roundtrip_all_bit_positions() {
        for bit in 0..8u8 {
            let mut bytes = [0u8; 1];
            set_bit(&mut bytes, 0, bit);
            assert!(bit_is_set(bytes[0], bit), "bit {bit} should be set");
            clear_bit(&mut bytes, 0, bit);
            assert!(!bit_is_set(bytes[0], bit), "bit {bit} should be clear");
        }
    }

    // --- read_range_io --------------------------------------------------------

    #[test]
    fn read_range_io_returns_correct_byte_containing_queried_bits() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        // Allocate clusters 3 and 5 → bitmap byte 0 = 0b0010_1000.
        allocate_io(&mut dev, &bm, 3, 1).unwrap();
        allocate_io(&mut dev, &bm, 5, 1).unwrap();
        // Read 8 bits from start.
        let bytes = read_range_io(&mut dev, &bm, 0, 8).unwrap();
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes[0], 0b0010_1000);
    }

    #[test]
    fn read_range_io_spanning_two_bytes() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 32 clusters
                                   // Set bit 7 (end of byte 0) and bit 8 (start of byte 1).
        allocate_io(&mut dev, &bm, 7, 1).unwrap();
        allocate_io(&mut dev, &bm, 8, 1).unwrap();
        // Read bits 4..12 (spans byte 0 bits 4-7 + byte 1 bits 0-3).
        let bytes = read_range_io(&mut dev, &bm, 4, 8).unwrap();
        // start_byte = 4/8 = 0, end_byte = div_ceil(12, 8) = 2
        // So reads bytes [0..2] = 2 bytes.
        assert_eq!(bytes.len(), 2);
        // Byte 0 bit 7 set → 0b1000_0000; byte 1 bit 0 set → 0b0000_0001.
        assert_eq!(bytes[0], 0b1000_0000, "byte 0");
        assert_eq!(bytes[1], 0b0000_0001, "byte 1");
    }

    #[test]
    fn read_range_io_out_of_bounds_errors() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4); // 32 clusters total
        let err = read_range_io(&mut dev, &bm, 30, 5).unwrap_err();
        assert!(err.contains("exceeds total_bits"), "{err}");
    }

    #[test]
    fn read_range_io_zero_bits_returns_empty() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        let bytes = read_range_io(&mut dev, &bm, 0, 0).unwrap();
        assert!(bytes.is_empty());
    }

    // --- locate_bitmap_io on a real formatted volume -------------------------

    struct FmtDev(Vec<u8>);
    impl BlockIo for FmtDev {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            let off = offset as usize;
            buf.copy_from_slice(&self.0[off..off + buf.len()]);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            let off = offset as usize;
            self.0[off..off + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn formatted_dev() -> FmtDev {
        const SIZE: u64 = 64 * 1024 * 1024;
        let mut dev = FmtDev(vec![0u8; SIZE as usize]);
        crate::mkfs::format_filesystem(
            &mut dev as &mut dyn BlockIo,
            SIZE,
            4096,
            4096,
            Some("BITMAPTEST"),
            Some(0xABCD),
        )
        .expect("format_filesystem");
        dev
    }

    #[test]
    fn locate_bitmap_io_on_formatted_volume_succeeds() {
        let mut dev = formatted_dev();
        let bm = locate_bitmap_io(&mut dev).unwrap();
        assert!(bm.total_bits > 0);
        assert!(!bm.runs.is_empty());
    }

    #[test]
    fn locate_bitmap_io_cluster_size_matches_format_params() {
        let mut dev = formatted_dev();
        let bm = locate_bitmap_io(&mut dev).unwrap();
        assert_eq!(bm.params.cluster_size, 4096);
    }

    #[test]
    fn locate_bitmap_io_total_bits_covers_volume() {
        let mut dev = formatted_dev();
        let bm = locate_bitmap_io(&mut dev).unwrap();
        // 64 MiB volume / 4 KiB clusters = 16384 clusters; bitmap has one bit per cluster
        assert!(bm.total_bits >= 16384);
    }

    #[test]
    fn count_free_io_on_formatted_volume_is_positive() {
        let mut dev = formatted_dev();
        let bm = locate_bitmap_io(&mut dev).unwrap();
        let free = count_free_io(&mut dev, &bm).unwrap();
        assert!(free > 0, "a fresh format must have free clusters");
    }

    #[test]
    fn is_allocated_on_formatted_volume_cluster_zero_is_allocated() {
        let mut dev = formatted_dev();
        let bm = locate_bitmap_io(&mut dev).unwrap();
        // Cluster 0 holds the boot sector — always allocated.
        assert!(is_allocated_io(&mut dev, &bm, 0).unwrap());
    }
}
