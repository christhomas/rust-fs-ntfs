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
    /// How many clusters `$MFT` occupies, so a free can be refused
    /// before it hands them out. Zero when `$MFT`'s own length could
    /// not be read -- treated as fail-closed in `covers_the_volumes_own`,
    /// not as "nothing to protect."
    pub mft_clusters: u64,
    /// CLUSTER ranges `[start_lcn, end_lcn)` of every OTHER system
    /// metafile's own storage -- `$MFTMirr`, `$LogFile`, `$AttrDef`,
    /// `$Bitmap` itself, `$Secure`, `$UpCase` -- so
    /// `covers_the_volumes_own` can refuse a free against any of them,
    /// not only `$MFT`. Converted once, at locate time, from the BYTE
    /// ranges `crate::read::other_protected_metafile_ranges_io` reports
    /// -- that function is shared with `fsck`, which works in bytes.
    pub other_protected: Vec<(u64, u64)>,
}

pub fn locate_bitmap(image: &Path) -> Result<BitmapLocation, String> {
    let mut io = PathIo::open_ro(image)?;
    locate_bitmap_io(&mut io)
}

impl BitmapLocation {
    /// Whether `[lcn, lcn + n)` covers clusters the volume needs.
    ///
    /// `unlink` and `truncate` push every run of every non-resident
    /// attribute of a file straight into [`free_io`], bounded only by
    /// `$Bitmap`'s own size. A file record whose `$DATA` runs overlap
    /// `$MFT` therefore made `unlink` mark live system clusters free,
    /// and the next allocation handed them out.
    ///
    /// `$MFT` is where everything else is, and its length is read once
    /// when the bitmap is located. The `lcn == 0` case below is a floor
    /// for the boot region, NOT its coverage: `$Boot`'s $DATA is 8192
    /// bytes, which is two clusters at 4096 and sixteen at 512, and
    /// measured before record 7 joined the protected list fifteen of
    /// those sixteen were freeable on a third-party volume. `$Boot`'s
    /// real extent is in `other_protected` with the rest.
    pub fn covers_the_volumes_own(&self, lcn: u64, n: u64) -> bool {
        let end = lcn.saturating_add(n);
        if lcn == 0 {
            return true;
        }
        // `$MFT`'s protection does NOT depend on this scalar any more.
        // `mft_clusters` comes from `nonresident_contiguous_disk_range`,
        // which refuses a fragmented attribute -- and a fragmented
        // `$MFT` is ordinary, six runs on the `ntfs` crate's own
        // testdata. So `$MFT` is in `other_protected` below, by its
        // real runs, and this scalar is only a fast path for the
        // contiguous case.
        //
        // IT MUST NOT SHORT-CIRCUIT TO `true` WHEN ZERO. It did, for
        // one revision of rust-fs-ntfs#157, on the reasoning that an
        // unverifiable `$MFT` should widen what is refused. Measured
        // against a volume this crate did not format, that refused
        // 4095 of 4095 clusters and broke `rm` on an ordinary file.
        // Zero here means "not known from this scalar", which is a
        // statement about the scalar, not about the volume.
        if self.mft_clusters != 0 {
            let mft_end = self.params.mft_lcn.saturating_add(self.mft_clusters);
            if lcn < mft_end && self.params.mft_lcn < end {
                return true;
            }
        }
        self.other_protected
            .iter()
            .any(|&(start_lcn, end_lcn)| lcn < end_lcn && start_lcn < end)
    }
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
    // A bit covers a cluster, and the volume has only so many.
    //
    // The declared count is $Bitmap's own data_length times eight, off
    // the disk, with nothing tying it to the boot sector. NTFS sets the
    // padding bits past the last real cluster so a search cannot walk
    // off the end -- but a truncated image, a wrong length field, or an
    // image from another tool leaves them clear, and then
    // `find_free_run_io` returns an LCN the volume does not have. The
    // $MFT:$Bitmap sibling has clamped its own count by what the volume
    // could hold since the same bug was found there; this is the
    // equivalent.
    let declared_bits = value_length.saturating_mul(8);
    // Rounded UP, not down. `volume_bytes()` is `total_sectors x
    // bytes_per_sector`, and NTFS's total_sectors is one sector short of
    // the device -- the last sector holds the backup boot sector -- so
    // the final cluster is only partly inside it. Dividing down would
    // make that cluster unallocatable on every volume, which is a
    // capacity bug in exchange for nothing: the ceiling only has to be
    // tight enough to reject a bitmap that claims several times the
    // volume, which is the failure being guarded.
    let cluster_capacity = params.volume_bytes().div_ceil(params.cluster_size.max(1));
    // A volume that does not say how big it is cannot judge the bitmap,
    // and answering zero to every question would be worse than
    // answering what the bitmap said.
    let total_bits = if cluster_capacity == 0 {
        declared_bits
    } else {
        declared_bits.min(cluster_capacity)
    };
    // $MFT's own extent, so `free_io` can refuse to hand it out. Read
    // once here rather than on every free. `mft_clusters == 0` is the
    // "could not be determined" sentinel `covers_the_volumes_own`
    // treats as fail-closed. `other_protected` below covers the other
    // five metafiles' own equivalent failure.
    //
    // BOUNDED BY THE VOLUME, like every run in `other_protected` is.
    // This scalar is `$MFT`'s declared value length in clusters, and a
    // damaged record 0 can declare almost the whole volume -- the
    // helper only requires the extent to be on the DEVICE. Unbounded,
    // `mft_end` below then refuses everything from `mft_lcn` upward,
    // which is the outage direction rust-fs-ntfs#157 was returned for.
    // Measured: 4 + 64 <= 16384 on this crate's own formatted volume,
    // so the bound costs a healthy volume nothing.
    let mft_clusters = crate::read::nonresident_contiguous_disk_range(io, 0, AttrType::Data, None)
        .map(|(_, len)| len.div_ceil(params.cluster_size.max(1)))
        .ok()
        .filter(|&clusters| params.mft_lcn.saturating_add(clusters) <= cluster_capacity)
        .unwrap_or(0);

    // The other system metafiles' own storage, converted from the
    // BYTE ranges the shared helper reports to the CLUSTER ranges
    // `covers_the_volumes_own` compares against. See rust-fs-ntfs#157.
    let cluster_size = params.cluster_size.max(1);
    let other_protected: Vec<(u64, u64)> =
        crate::read::other_protected_metafile_ranges_io(io, None)
            .into_iter()
            .map(|(start, end)| (start / cluster_size, end.div_ceil(cluster_size)))
            .collect();

    Ok(BitmapLocation {
        params,
        runs,
        total_bits,
        value_length,
        mft_clusters,
        other_protected,
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
    // Not the volume's own clusters, however a file record describes
    // its runs. See `BitmapLocation::covers_the_volumes_own`.
    if bm.covers_the_volumes_own(lcn, n) {
        return Err(format!(
            "clusters [{lcn}, +{n}) hold the volume's own structures and are not a \
             file's to free"
        ));
    }
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
    // Checked: `lcn` and `n` reach here from a file's own run list,
    // where `decode_runs` permits an LCN up to 2^63 and a length up to
    // 2^64. `lcn + n` wrapping to a small number made this guard pass,
    // and `end_byte_excl - first_byte` then underflowed into a huge
    // allocation below -- a capacity-overflow panic, or an abort for
    // values that merely do not fit.
    let last = lcn
        .checked_add(n)
        .filter(|end| *end <= bm.total_bits)
        .ok_or_else(|| {
            format!(
                "range [{lcn}, +{n}) is not inside the {} bits $Bitmap describes",
                bm.total_bits
            )
        })?;
    let first_byte = lcn / 8;
    let end_byte_excl = last.div_ceil(8);
    let mut bytes = read_bitmap_bytes_io(io, bm, first_byte, end_byte_excl - first_byte)?;
    // The pre-image, kept so a write that fails partway can be undone.
    // Every caller of the write half has already read these bytes in
    // order to change them, so this costs a copy rather than a read.
    let before = bytes.clone();

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
    write_bitmap_bytes_io(io, bm, first_byte, &bytes, &before)?;
    Ok(())
}

// -- byte-level bitmap I/O -------------------------------------------------

/// One contiguous piece of a `$Bitmap` byte range, and where it lives.
///
/// A `$Bitmap` may be fragmented, so a byte range that is contiguous in
/// the file is not contiguous on disk: it becomes one chunk per data
/// run it crosses.
struct MappedChunk {
    /// Byte offset on the device.
    disk_offset: u64,
    /// Offset of this chunk within the caller's buffer.
    cursor: usize,
    /// Length in bytes.
    len: usize,
}

/// Resolve `[start_byte, start_byte + len)` of `$Bitmap` into the disk
/// chunks it occupies.
///
/// # Why this is a plan rather than a loop that does the work
///
/// The read and write halves each used to walk the runs themselves —
/// two functions that were line-for-line identical apart from
/// `read_exact_at` against `write_all_at` and the buffer direction. Two
/// copies of a mapping walk on a mutation path is how one of them ends
/// up wrong.
///
/// Resolving the whole range **before** any I/O also buys the write
/// half something the loop could not give it: an unmapped or sparse
/// VCN halfway along is discovered before the first byte is written,
/// rather than after. `mutate_bits_io` validates every bit before
/// touching its buffer; this is the same discipline one layer down.
///
/// # Errors
///
/// A VCN the runs do not cover, or one in a sparse run. `$Bitmap` is
/// never sparse in practice — a hole would mean clusters whose
/// allocation state is unrecorded — so both are corruption rather than
/// an unsupported layout.
fn map_bitmap_range(
    bm: &BitmapLocation,
    start_byte: u64,
    len: u64,
) -> Result<Vec<MappedChunk>, String> {
    let cluster_size = bm.params.cluster_size;
    let end = start_byte + len;
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut file_offset = start_byte;

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
            .ok_or_else(|| format!("VCN {vcn} is in a sparse $Bitmap run"))?;
        // Checked, and required to make progress. Both halves of the
        // sum are run-list fields off the disk, and in release -- where
        // this crate ships with `overflow-checks` off -- the product
        // wrapped. Wrapping to exactly `file_offset` makes `chunk`
        // zero, and then the cursor never advances and `out` grows
        // until the process dies.
        let run_end_offset = run
            .starting_vcn
            .checked_add(run.length)
            .and_then(|vcns| vcns.checked_mul(cluster_size))
            .ok_or_else(|| {
                format!(
                    "$Bitmap run at VCN {} ends past the address space",
                    run.starting_vcn
                )
            })?;
        let chunk = run_end_offset
            .checked_sub(file_offset)
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| {
                format!(
                    "$Bitmap run at VCN {} ends at or before {file_offset}",
                    run.starting_vcn
                )
            })?
            .min(end - file_offset) as usize;

        out.push(MappedChunk {
            // Checked and bounded by the volume; see
            // `mft_io::cluster_span`. Every $Bitmap read AND write goes
            // through here, and `mutate_bits_io` is a read-modify-write
            // -- so a run pointing at the boot sector or an MFT record
            // turned the first create or unlink into a targeted bit
            // flip anywhere on the device.
            disk_offset: crate::mft_io::cluster_span(
                &bm.params,
                lcn,
                vcn - run.starting_vcn,
                off_in_cluster,
                chunk as u64,
                u64::MAX,
            )?,
            cursor,
            len: chunk,
        });
        cursor += chunk;
        file_offset += chunk as u64;
    }
    Ok(out)
}

fn read_bitmap_bytes_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    start_byte: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len as usize];
    for c in map_bitmap_range(bm, start_byte, len)? {
        io.read_exact_at(c.disk_offset, &mut out[c.cursor..c.cursor + c.len])
            .map_err(|e| format!("read bitmap: {e}"))?;
    }
    Ok(out)
}

/// Write `data` over `$Bitmap` at `start_byte`, all or nothing.
///
/// `previous` is what those bytes held before — the caller has it,
/// because every caller read them in order to change them.
///
/// # Why a partial write is not acceptable here
///
/// A fragmented `$Bitmap` takes one `write_all_at` per data run. If the
/// second fails after the first succeeded, the first chunk is already
/// on disk, `sync` never runs, and `Err` comes back with the bitmap in
/// a state nobody tracks — a bitmap that disagrees with the records it
/// describes, which is exactly what `chkdsk` exists to find.
///
/// So a failure rewrites the chunks that did land, from `previous`.
///
/// # Errors
///
/// The mapping errors from [`map_bitmap_range`], raised before any byte
/// is written; or a write failure, in which case the error also says
/// whether the rollback succeeded — if it did not, the bitmap really is
/// inconsistent and no further write here can be trusted to fix it.
fn write_bitmap_bytes_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    start_byte: u64,
    data: &[u8],
    previous: &[u8],
) -> Result<(), String> {
    debug_assert_eq!(
        data.len(),
        previous.len(),
        "the pre-image must cover the same range as the new bytes"
    );
    let chunks = map_bitmap_range(bm, start_byte, data.len() as u64)?;

    for (i, c) in chunks.iter().enumerate() {
        if let Err(e) = io.write_all_at(c.disk_offset, &data[c.cursor..c.cursor + c.len]) {
            let failure = format!("write bitmap: {e}");
            // Put back the chunks that did land.
            let mut rollback_failed = None;
            for done in &chunks[..i] {
                if let Err(re) = io.write_all_at(
                    done.disk_offset,
                    &previous[done.cursor..done.cursor + done.len],
                ) {
                    rollback_failed = Some(re);
                    break;
                }
            }
            let _ = io.sync();
            return Err(match rollback_failed {
                None => failure,
                Some(re) => format!(
                    "{failure}; and rolling the earlier chunks back failed too ({re}): \
                     $Bitmap now records an allocation state that does not match the \
                     records it describes — run chkdsk"
                ),
            });
        }
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
    // IN CHUNKS. `value_length` is $Bitmap's declared, unvalidated
    // `data_length`, and this read it whole into one buffer: 2^40 asks
    // for a terabyte, which `handle_alloc_error` answers by aborting --
    // past the FFI guard, taking the host process with it.
    // `find_free_run_io` already streams the same bitmap 64 KiB at a
    // time; this did not.
    const CHUNK: u64 = 64 * 1024;
    let total_bytes = bm.value_length.min(bm.total_bits.div_ceil(8));
    let mut set: u64 = 0;
    let mut at = 0u64;
    while at < total_bytes {
        let n = CHUNK.min(total_bytes - at);
        let bytes = read_bitmap_bytes_io(io, bm, at, n)?;
        set += bytes.iter().map(|b| b.count_ones() as u64).sum::<u64>();
        at += n;
    }
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

    pub(super) struct MemDev {
        buf: Vec<u8>,
    }
    impl MemDev {
        pub(super) fn new(size: usize) -> Self {
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
                // Placed far outside every cluster range these
                // allocate/free tests exercise (0..n_bytes*8, always
                // well under 1_000_000 here). $MFT IS modelled -- a
                // NON-zero `mft_clusters` -- because zero now means
                // "could not be determined," which fails closed and
                // would refuse every free in this file. A disjoint
                // placeholder keeps that fail-closed path untested here
                // (see `a_files_runs_may_not_free_the_volumes_own_clusters`
                // for where it IS tested) while leaving these ordinary
                // allocate/free tests exercising exactly the clusters
                // they always did.
                mft_lcn: 1_000_000,
                file_record_size: 1024,
                // A real boot sector always says how big the volume is,
                // and cluster_span judges every transfer against it. 512 MiB
                // at 512-byte sectors is larger than anything these tests
                // address, so the bound is present without being the thing
                // under test.
                total_sectors: 1 << 20,
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
            mft_clusters: 1,
            // The other five system metafiles are not modelled here
            // either; see the same reasoning above.
            other_protected: Vec::new(),
        }
    }

    /// A `$Bitmap` split across two non-adjacent runs.
    ///
    /// **No test used one before**, which is the whole reason G5
    /// survived: every mapped-chunk loop in this file walks runs, and a
    /// single-run bitmap exercises exactly one iteration of it.
    ///
    /// Run 0 holds VCN 0 at LCN 1; run 1 holds VCN 1 at LCN 5. So a
    /// write spanning the cluster boundary becomes two `write_all_at`
    /// calls at far-apart disk offsets — which is what makes a partial
    /// failure observable.
    fn make_fragmented_bm(cluster_size: u64, n_bytes: u64) -> BitmapLocation {
        let mut bm = make_bm(cluster_size, n_bytes);
        bm.runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 1,
                lcn: Some(1),
            },
            DataRun {
                starting_vcn: 1,
                length: 1,
                lcn: Some(5),
            },
        ];
        bm
    }

    /// A device that refuses writes landing at or past `deny_from`.
    struct FailsWritesFrom {
        inner: MemDev,
        deny_from: u64,
    }

    impl BlockIo for FailsWritesFrom {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            self.inner.read_exact_at(offset, buf)
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            if offset >= self.deny_from {
                return Err("injected write failure".to_string());
            }
            self.inner.write_all_at(offset, buf)
        }
        fn size(&self) -> u64 {
            self.inner.size()
        }
    }

    // --- a bitmap write is all or nothing ---------------------------------

    /// A write that fails on the second run must not leave the first
    /// run's bytes changed.
    ///
    /// The write half issues one `write_all_at` per data run. Before
    /// this, a failure on run 2 left run 1 already on disk, `io.sync()`
    /// never ran, and `Err` came back with `$Bitmap` in a state nobody
    /// tracked — while `mutate_bits_io` validates every bit before
    /// touching its buffer, so the all-or-nothing intent was explicit
    /// and the write half broke it.
    #[test]
    fn a_bitmap_write_that_fails_partway_leaves_the_bitmap_as_it_was() {
        let cluster = 4096u64;
        // 8192 bytes of bitmap ⇒ the write spans VCN 0 and VCN 1.
        let bm = make_fragmented_bm(cluster, 2 * cluster);
        let mut inner = MemDev::new((cluster * 8) as usize);
        // Every cluster allocated, so clearing the range is legal —
        // `mutate_bits_io` refuses to free a bit that is already free,
        // and that check runs before any write.
        for b in 0..cluster as usize {
            inner.buf[cluster as usize + b] = 0xFF; // run 0, at LCN 1
        }
        inner.buf[(5 * cluster) as usize] = 0xFF; // run 1, at LCN 5
        let before_run0: Vec<u8> = inner.buf[cluster as usize..(2 * cluster) as usize].to_vec();

        // Deny run 1's disk region (LCN 5) but allow run 0's (LCN 1).
        let mut dev = FailsWritesFrom {
            inner,
            deny_from: 5 * cluster,
        };

        // Free a range that starts in run 0 and ends in run 1, so the
        // write is split across both.
        let first_bit = 0u64;
        let n = cluster * 8 + 8; // past the end of run 0's byte range
        let err = mutate_bits_io(&mut dev, &bm, first_bit, n, false)
            .expect_err("the write must fail when its second run does");
        assert!(
            err.contains("injected write failure"),
            "the failure should name its cause: {err}"
        );

        assert_eq!(
            &dev.inner.buf[cluster as usize..(2 * cluster) as usize],
            &before_run0[..],
            "run 0's bytes must be back as they were — a failed bitmap write \
             that leaves half its change on disk is a bitmap that disagrees \
             with the records it describes"
        );
    }

    /// The write half refuses an unmapped range without writing.
    ///
    /// Tested against `write_bitmap_bytes_io` directly, because through
    /// `mutate_bits_io` it is unreachable: the read half walks the same
    /// runs first and fails there. Removing the write half's own check
    /// therefore breaks nothing end-to-end — which is precisely why it
    /// needs a test of its own rather than an assumption that the outer
    /// path covers it.
    ///
    /// It is defence in depth, and it is cheap: the plan is built
    /// before the first `write_all_at`, so a caller that ever writes
    /// without reading first still cannot leave half a change behind.
    #[test]
    fn the_write_half_refuses_an_unmapped_range_without_writing() {
        let cluster = 4096u64;
        let mut bm = make_fragmented_bm(cluster, 2 * cluster);
        bm.runs.truncate(1); // VCN 1 is now unmapped

        let mut dev = MemDev::new((cluster * 8) as usize);
        for b in 0..cluster as usize {
            dev.buf[cluster as usize + b] = 0xAA;
        }
        let before: Vec<u8> = dev.buf[cluster as usize..(2 * cluster) as usize].to_vec();

        let data = vec![0xFFu8; (cluster + 8) as usize];
        let previous = vec![0xAAu8; (cluster + 8) as usize];
        let err = write_bitmap_bytes_io(&mut dev, &bm, 0, &data, &previous)
            .expect_err("an unmapped VCN must be refused");
        assert!(err.contains("not mapped"), "got: {err}");
        assert_eq!(
            &dev.buf[cluster as usize..(2 * cluster) as usize],
            &before[..],
            "the mappable chunk must not have been written before the \
             unmappable one was discovered"
        );
    }

    /// And end to end, the read half refuses first — which is where the
    /// guarantee actually comes from today.
    #[test]
    fn an_unmapped_run_is_refused_before_any_byte_is_written() {
        let cluster = 4096u64;
        let mut bm = make_fragmented_bm(cluster, 2 * cluster);
        // Drop run 1: VCN 1 is now unmapped.
        bm.runs.truncate(1);

        let mut dev = MemDev::new((cluster * 8) as usize);
        for b in 0..cluster as usize {
            dev.buf[cluster as usize + b] = 0xFF;
        }
        let before: Vec<u8> = dev.buf[cluster as usize..(2 * cluster) as usize].to_vec();

        let err = mutate_bits_io(&mut dev, &bm, 0, cluster * 8 + 8, false)
            .expect_err("an unmapped VCN must be refused");
        assert!(err.contains("not mapped"), "got: {err}");
        assert_eq!(
            &dev.buf[cluster as usize..(2 * cluster) as usize],
            &before[..],
            "nothing may be written when part of the range cannot be mapped"
        );
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

    /// Cluster 0 is $Boot, so a file never frees it and `free_io`
    /// refuses to -- the round trip starts at cluster 1.
    #[test]
    fn allocate_io_then_free_io_full_roundtrip() {
        let mut dev = MemDev::new(8192);
        let bm = make_bm(4096, 4);
        allocate_io(&mut dev, &bm, 1, 31).unwrap();
        assert_eq!(count_free_io(&mut dev, &bm).unwrap(), 1);
        free_io(&mut dev, &bm, 1, 31).unwrap();
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
        assert!(err.contains("not inside"), "{err}");
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

    /// `$Boot` IS MORE THAN ONE CLUSTER. The `lcn == 0` special case
    /// guards exactly one, and `$Boot`'s $DATA is 8192 bytes -- two
    /// clusters at 4096, sixteen at 512.
    ///
    /// Measured before `$Boot` joined the protected list: on this
    /// fixture LCN 1 answered false and `free_io` on it SUCCEEDED; on a
    /// 512-byte-cluster volume fifteen of the sixteen boot-region
    /// clusters were freeable.
    #[test]
    fn the_whole_boot_region_is_protected_not_just_its_first_cluster() {
        let mut dev = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut dev).unwrap();
        let bm = locate_bitmap_io(&mut dev).unwrap();

        // $Boot's $DATA is 8192 bytes; at this fixture's 4096-byte
        // clusters that is clusters 0 and 1.
        let boot_clusters = 8192u64.div_ceil(params.cluster_size.max(1));
        assert!(
            boot_clusters > 1,
            "precondition: this fixture's clusters must be smaller than $Boot, or the \
             test cannot distinguish the lcn==0 case from real $Boot coverage"
        );
        for lcn in 0..boot_clusters {
            assert!(
                bm.covers_the_volumes_own(lcn, 1),
                "boot-region cluster {lcn} of {boot_clusters} must be refused"
            );
        }
        assert!(
            free_io(&mut dev, &bm, 1, 1).is_err(),
            "and the refusal must reach free_io, which is the call unlink makes -- this \
             succeeded before $Boot was protected"
        );
    }

    /// Overwrite one metafile record's mapping pairs on `dev` and
    /// report how many clusters the guard then refuses.
    ///
    /// `record_number` is written in place, fixups reapplied, so the
    /// damage is exactly what an interrupted write to a mapping-pairs
    /// list leaves behind -- the damage class rust-fs-ntfs#157 names.
    fn refused_after_damaging_pairs(record_number: u64, pairs: &[u8]) -> (u64, u64, bool) {
        let mut dev = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut dev).unwrap();
        let capacity = params.volume_bytes().div_ceil(params.cluster_size.max(1));
        let record_size = params.file_record_size;
        let at_off = params.mft_lcn * params.cluster_size + record_size * record_number;
        let mut record = vec![0u8; record_size as usize];
        dev.read_exact_at(at_off, &mut record).unwrap();
        crate::mft_io::apply_fixup_on_read(&mut record, params.bytes_per_sector).unwrap();
        let loc = crate::attr_io::find_attribute(&record, AttrType::Data, None)
            .expect("record has an unnamed $DATA");
        let mpo = loc.non_resident_mapping_pairs_offset.expect("non-resident") as usize;
        let at = loc.attr_offset + mpo;
        assert!(
            at + pairs.len() <= loc.attr_offset + loc.attr_length,
            "precondition: the replacement pairs must fit inside the attribute, or the \
             record is malformed in a way this test did not intend"
        );
        record[at..at + pairs.len()].copy_from_slice(pairs);
        crate::mft_io::apply_fixup_on_write(&mut record, params.bytes_per_sector).unwrap();
        dev.write_all_at(at_off, &record).unwrap();

        let bm = locate_bitmap_io(&mut dev).unwrap();
        let refused = (0..capacity)
            .filter(|&lcn| bm.covers_the_volumes_own(lcn, 1))
            .count() as u64;
        // An ordinary file's cluster, well clear of every metafile on
        // this fixture. Whether the GUARD stops a free here is the
        // property that matters to a user: `unlink` and `truncate`
        // both route every run through `free_io`.
        let guard_stops_ordinary = guard_refuses_free(&mut dev, &bm, 5000);
        (refused, capacity, guard_stops_ordinary)
    }

    /// Whether `free_io` refuses `lcn` BECAUSE OF THE GUARD.
    ///
    /// `free_io` has two refusals and they are not the same evidence:
    /// the guard's, and "cluster N already free" for a cluster no file
    /// owns. An unallocated cluster on a freshly formatted volume gives
    /// the second, so `is_ok()` cannot tell "the guard let it through"
    /// from "there was nothing to free" -- measured on this fixture,
    /// every ordinary cluster answers `Err("cluster N already free")`.
    fn guard_refuses_free<T: BlockIo + ?Sized>(io: &mut T, bm: &BitmapLocation, lcn: u64) -> bool {
        match free_io(io, bm, lcn, 1) {
            Err(e) => e.contains("the volume's own structures"),
            Ok(()) => false,
        }
    }

    /// A RUN LENGTH COMES OFF THE SAME DAMAGED RECORD THESE GUARDS
    /// DEFEND AGAINST, AND NOTHING USED TO BOUND IT. One absurd length
    /// in any one metafile record turned the guard into a volume-wide
    /// refusal -- `unlink`, `truncate` and `fsck`'s `$LogFile` reset
    /// all stop working, which is revision one's outage reached from a
    /// damaged record instead of a code bug.
    ///
    /// Two lengths, because the two bounds catch different ones and
    /// EITHER BOUND ALONE IS EVADABLE:
    ///
    /// - 2^30 clusters, from mapping pairs
    ///   `[0x14, 0x00, 0x00, 0x00, 0x40, 0x01, 0x00]`. `decode_runs`
    ///   yields `length: 1073741824, lcn: Some(1)`, byte range
    ///   `(4096, 4398046515200)`, clusters `(1, 1073741825)`; measured
    ///   16384 of 16384 refused. NOTHING OVERFLOWS: `1073741824 *
    ///   4096` fits in a `u64` with room to spare, so swapping a
    ///   `saturating_add` for a `checked_add` changes nothing here.
    /// - `capacity - 1` clusters at LCN 1, which FITS INSIDE THE
    ///   VOLUME and therefore survives a volume-only bound, refusing
    ///   16383 of 16384. It is caught only by the second bound: the
    ///   attribute's own declared length, 4 clusters for `$MFTMirr`.
    #[test]
    fn a_damaged_run_length_does_not_turn_the_guard_into_a_volume_wide_refusal() {
        let mut clean = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut clean).unwrap();
        let capacity = params.volume_bytes().div_ceil(params.cluster_size.max(1));
        let healthy = locate_bitmap_io(&mut clean).unwrap();
        let baseline = (0..capacity)
            .filter(|&lcn| healthy.covers_the_volumes_own(lcn, 1))
            .count() as u64;
        assert!(
            baseline > 0 && baseline * 4 < capacity,
            "control: a healthy volume refuses its metadata and little else, got \
             {baseline} of {capacity}"
        );

        // $MFTMirr (record 1), whose declared $DATA is 4 clusters here.
        let absurd = [0x14u8, 0x00, 0x00, 0x00, 0x40, 0x01, 0x00, 0x00];
        let fits_but_huge = {
            let len = capacity - 1;
            [
                0x13u8,
                (len & 0xff) as u8,
                ((len >> 8) & 0xff) as u8,
                ((len >> 16) & 0xff) as u8,
                0x01,
                0x00,
                0x00,
                0x00,
            ]
        };

        for (label, pairs) in [
            ("2^30 clusters", &absurd),
            ("capacity-1 clusters", &fits_but_huge),
        ] {
            let (refused, cap, guard_stops_ordinary) = refused_after_damaging_pairs(1, pairs);
            assert!(
                refused <= baseline,
                "{label}: {refused} of {cap} clusters refused against a healthy volume's \
                 {baseline}. A damaged run must cost that record its own protection, not \
                 widen the refusal -- that is the outage this fix was returned for twice."
            );
            assert!(
                !guard_stops_ordinary,
                "{label}: the guard refused an ordinary file's cluster 5000 through \
                 free_io. That breaks `rm` on every file, which is worse than the defect \
                 being guarded."
            );
        }
    }

    /// A RUN THAT STARTS INSIDE THE VOLUME AND ENDS PAST IT IS
    /// DISCARDED, NOT CLAMPED -- and this is the one arm that the
    /// declared-length bound cannot catch, because the length here is
    /// exactly what the attribute declares.
    ///
    /// `$MFTMirr`'s declared $DATA is 4 clusters. Point its run at
    /// `capacity - 2` and it still claims 4: plausible against its own
    /// record, impossible against the volume. Clamping it to the
    /// volume's end would protect the last two clusters -- ordinary
    /// file storage, refused on the strength of a damaged pointer --
    /// so the run is dropped instead. The cost either way is bounded;
    /// the direction is chosen so a damaged record does not take
    /// somebody's file with it.
    #[test]
    fn a_run_running_off_the_end_of_the_volume_is_discarded_not_clamped() {
        let mut clean = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut clean).unwrap();
        let capacity = params.volume_bytes().div_ceil(params.cluster_size.max(1));
        let healthy = locate_bitmap_io(&mut clean).unwrap();
        for lcn in [capacity - 2, capacity - 1] {
            assert!(
                !healthy.covers_the_volumes_own(lcn, 1),
                "control: cluster {lcn} at the end of a healthy volume is ordinary storage"
            );
        }

        // header 0x31: one length byte, three offset bytes. Length 4 --
        // exactly $MFTMirr's declared size -- at LCN capacity-2.
        let lcn = capacity - 2;
        let pairs = [
            0x31u8,
            0x04,
            (lcn & 0xff) as u8,
            ((lcn >> 8) & 0xff) as u8,
            ((lcn >> 16) & 0xff) as u8,
            0x00,
            0x00,
            0x00,
        ];
        let (refused, cap, guard_stops_ordinary) = refused_after_damaging_pairs(1, &pairs);
        assert!(
            !guard_stops_ordinary,
            "{refused} of {cap}: cluster 5000 must stay a file's"
        );

        let mut dev = formatted_dev();
        {
            let p2 = crate::mft_io::read_boot_params_io(&mut dev).unwrap();
            let record_size = p2.file_record_size;
            let off = p2.mft_lcn * p2.cluster_size + record_size;
            let mut record = vec![0u8; record_size as usize];
            dev.read_exact_at(off, &mut record).unwrap();
            crate::mft_io::apply_fixup_on_read(&mut record, p2.bytes_per_sector).unwrap();
            let loc = crate::attr_io::find_attribute(&record, AttrType::Data, None).unwrap();
            let at = loc.attr_offset + loc.non_resident_mapping_pairs_offset.unwrap() as usize;
            record[at..at + pairs.len()].copy_from_slice(&pairs);
            crate::mft_io::apply_fixup_on_write(&mut record, p2.bytes_per_sector).unwrap();
            dev.write_all_at(off, &record).unwrap();
        }
        let bm = locate_bitmap_io(&mut dev).unwrap();
        for lcn in [capacity - 2, capacity - 1] {
            assert!(
                !bm.covers_the_volumes_own(lcn, 1),
                "cluster {lcn} must not be refused: the only thing claiming it is a run \
                 that runs off the end of the volume, which is damage. Clamping that run \
                 to the volume's end is what protects it, and that is the failure \
                 direction this fix has been returned for twice. other_protected={:?}",
                bm.other_protected
            );
        }
    }

    /// A GENUINELY FRAGMENTED `$MFT`, described as two runs in record 0
    /// itself -- the property rust-fs-ntfs#246 was filed for, which the
    /// mirror and floor tiers could not cover.
    ///
    /// `nonresident_contiguous_disk_range` refuses a fragmented
    /// attribute, so `mft_clusters` is 0 here (asserted, so this test
    /// fails rather than silently stops exercising the path), and only
    /// the multi-run helper can report where `$MFT` lives. The
    /// assertion is that BOTH runs appear SEPARATELY: the mirror tier
    /// beneath this one holds the format-time single-run copy and would
    /// answer one range covering the same clusters, so asserting only
    /// that `$MFT`'s clusters are refused would pass with the multi-run
    /// helper gone.
    #[test]
    fn a_two_run_mft_is_protected_by_its_own_record_not_only_by_the_mirror() {
        let mut dev = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut dev).unwrap();
        let healthy = locate_bitmap_io(&mut dev).unwrap();
        assert_eq!(
            healthy.mft_clusters, 64,
            "control: a contiguous $MFT is 64 clusters on this fixture"
        );

        // Re-encode record 0's own run list as TWO runs covering the
        // same 64 clusters: 32 at LCN 4, then 32 at LCN 36. The
        // physical layout does not move -- only the encoding
        // fragments -- so the volume stays readable and the difference
        // measured is the guard's, not the fixture's.
        let record_size = params.file_record_size;
        let mft_off = params.mft_lcn * params.cluster_size;
        let mut record = vec![0u8; record_size as usize];
        dev.read_exact_at(mft_off, &mut record).unwrap();
        crate::mft_io::apply_fixup_on_read(&mut record, params.bytes_per_sector).unwrap();
        let loc = crate::attr_io::find_attribute(&record, AttrType::Data, None)
            .expect("$MFT has an unnamed $DATA");
        let mpo = loc.non_resident_mapping_pairs_offset.expect("non-resident") as usize;
        let at = loc.attr_offset + mpo;
        // header 0x11 = one length byte, one offset byte; the second
        // run's offset is a DELTA from the first's LCN.
        let pairs = [0x11u8, 0x20, 0x04, 0x11, 0x20, 0x20, 0x00, 0x00];
        assert!(
            at + pairs.len() <= loc.attr_offset + loc.attr_length,
            "precondition: two runs must fit in $MFT's mapping-pairs space"
        );
        record[at..at + pairs.len()].copy_from_slice(&pairs);
        crate::mft_io::apply_fixup_on_write(&mut record, params.bytes_per_sector).unwrap();
        dev.write_all_at(mft_off, &record).unwrap();

        let bm = locate_bitmap_io(&mut dev).unwrap();
        assert_eq!(
            bm.mft_clusters, 0,
            "precondition: the contiguous-only helper must refuse a two-run $MFT, or this \
             test is no longer exercising the multi-run path it is named for"
        );
        assert!(
            bm.other_protected.contains(&(4, 36)) && bm.other_protected.contains(&(36, 68)),
            "both of record 0's runs must be protected SEPARATELY, got {:?}. One range \
             (4, 68) means the answer came from the mirror's single-run copy, not from \
             record 0's own fragmented list.",
            bm.other_protected
        );
        for lcn in 4..68 {
            assert!(
                bm.covers_the_volumes_own(lcn, 1),
                "$MFT's cluster {lcn} must be refused"
            );
        }
        assert!(
            guard_refuses_free(&mut dev, &bm, 4),
            "and the refusal must reach free_io, the call unlink makes"
        );
        assert!(
            !guard_refuses_free(&mut dev, &bm, 5000),
            "while the guard leaves an ordinary file's cluster alone"
        );
    }

    /// $MFT IS NOT BEST-EFFORT. Blanking record 0 used to take `$MFT`
    /// out of the protected set entirely -- measured before the tiered
    /// fallback existed: `(4, 68)` vanished, `covers_the_volumes_own`
    /// answered false for `mft_lcn`, and `free_io` on `$MFT`'s own
    /// first cluster SUCCEEDED. That is this issue's opening sentence
    /// restored, so it fails closed, bounded, via `$MFTMirr`.
    #[test]
    fn a_damaged_mft_record_still_protects_the_mft_via_the_mirror() {
        let mut dev = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut dev).unwrap();
        let healthy = locate_bitmap_io(&mut dev).unwrap();
        assert!(
            healthy.covers_the_volumes_own(params.mft_lcn, 1),
            "control: $MFT is protected on an undamaged volume"
        );

        // Blank record 0 only. $MFTMirr still holds its copy.
        let record_size = params.file_record_size;
        let mft_start = params.mft_lcn * params.cluster_size;
        dev.write_all_at(mft_start, &vec![0u8; record_size as usize])
            .unwrap();

        let bm = locate_bitmap_io(&mut dev).unwrap();
        assert_eq!(
            bm.mft_clusters, 0,
            "precondition: record 0 no longer decodes, so the scalar is unset -- if this \
             ever stops holding, this test is no longer exercising the fallback"
        );
        // THE MIRROR SPECIFICALLY, not merely "something protected it".
        // The bounded floor (tier 3) also refuses `mft_lcn`, so
        // asserting that alone passes whichever tier fired -- measured:
        // deleting the mirror tier left this test green. `$MFT`'s real
        // extent here is clusters 4..68, and only the mirror can
        // recover the full 64 clusters; the floor covers the sixteen
        // reserved records, which is 4..8.
        assert!(
            bm.other_protected
                .contains(&(params.mft_lcn, params.mft_lcn + 64)),
            "the mirror must recover $MFT's FULL extent ({}..{}), not just the bounded \
             floor -- got {:?}. Asserting only that mft_lcn is refused cannot tell the \
             mirror tier from the floor beneath it.",
            params.mft_lcn,
            params.mft_lcn + 64,
            bm.other_protected
        );
        assert!(
            bm.covers_the_volumes_own(params.mft_lcn + 63, 1),
            "$MFT's LAST cluster must be refused too, which the floor alone does not reach"
        );
        assert!(
            free_io(&mut dev, &bm, params.mft_lcn, 1).is_err(),
            "and the refusal must reach free_io, which is the call unlink makes"
        );
    }

    /// The bottom tier, and the reason it is a FLOOR rather than the
    /// volume: blank record 0 AND `$MFTMirr`, so neither can speak.
    /// `$MFT`'s declared start must still be refused, and an ordinary
    /// cluster must still be free.
    #[test]
    fn with_both_the_mft_record_and_its_mirror_damaged_the_floor_is_bounded() {
        let mut dev = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut dev).unwrap();
        let mirror_ranges = crate::read::other_protected_metafile_ranges_io(&mut dev, None);
        assert!(
            !mirror_ranges.is_empty(),
            "control: something is locatable to begin with"
        );

        let record_size = params.file_record_size;
        let mft_start = params.mft_lcn * params.cluster_size;
        // Record 0 and record 1 ($MFTMirr) both blanked.
        dev.write_all_at(mft_start, &vec![0u8; record_size as usize])
            .unwrap();
        dev.write_all_at(mft_start + record_size, &vec![0u8; record_size as usize])
            .unwrap();

        let bm = locate_bitmap_io(&mut dev).unwrap();
        let cluster_capacity = params.volume_bytes().div_ceil(params.cluster_size.max(1));

        assert!(
            bm.covers_the_volumes_own(params.mft_lcn, 1),
            "$MFT's declared start must still be refused from the bounded floor"
        );
        assert!(
            !bm.covers_the_volumes_own(cluster_capacity - 1, 1),
            "BOUNDED, not volume-wide: the last cluster is not $MFT's, and refusing it is \
             the outage this fix exists to avoid re-creating"
        );
        let refused = (0..cluster_capacity)
            .filter(|&lcn| bm.covers_the_volumes_own(lcn, 1))
            .count() as u64;
        assert!(
            refused * 4 < cluster_capacity,
            "{refused} of {cluster_capacity} refused on this 64 MiB fixture -- a floor that \
             swallows the volume is the whole-volume fallback under a new name"
        );
    }

    /// `$Secure` on the NAMED `$SDS` stream, pinned by the ranges it
    /// actually contributes rather than left to a third-party probe.
    ///
    /// Its unnamed `$DATA` is a resident stub on a volume this crate
    /// formats, so the unnamed spelling contributes nothing at all --
    /// which is why `$Secure` was protected on no volume before this
    /// fix, `main` included. These two ranges are what the `$SDS`
    /// route adds, and reverting the spelling removes them.
    #[test]
    fn secures_sds_clusters_are_protected() {
        let mut dev = formatted_dev();
        let bm = locate_bitmap_io(&mut dev).unwrap();
        for expected in [(1047u64, 1048u64), (1048, 1049)] {
            assert!(
                bm.other_protected.contains(&expected),
                "$Secure's $SDS range {expected:?} must be in the protected set; got {:?}. \
                 Its absence is the unnamed-$DATA spelling, which contributes nothing here.",
                bm.other_protected
            );
        }
        assert!(
            bm.covers_the_volumes_own(1047, 2),
            "and those clusters must be refused by the guard itself"
        );
    }

    /// A METAFILE WHOSE LOOKUP GENUINELY FAILS -- the case no test in
    /// this crate could reach, and the reason a guard that refused
    /// 4095 of 4095 clusters on a real volume passed 626 tests.
    ///
    /// Every other fixture here is `mkfs` output, and on `mkfs` output
    /// nothing fails: `$MFT` is contiguous, and `$Secure`'s unnamed
    /// `$DATA` is a small resident stub, which the resident
    /// short-circuit reports as `Ok(no ranges)` rather than `Err`. So
    /// the failure path -- the one that used to manufacture a
    /// whole-volume range and refuse the entire volume -- was never
    /// executed by the suite at all. Reintroducing that fallback and
    /// re-running every test was measured: 626 passed.
    ///
    /// This damages one metafile record on purpose, which is the only
    /// way to reach it without shipping a third-party image into the
    /// repository.
    #[test]
    fn a_metafile_whose_lookup_fails_costs_only_its_own_protection() {
        let mut dev = formatted_dev();
        let params = crate::mft_io::read_boot_params_io(&mut dev).unwrap();

        // Blank $AttrDef's whole MFT record (record 4), so locating its
        // $DATA fails outright rather than returning "resident, nothing
        // to add".
        let record_size = params.file_record_size;
        let mft_start = params.mft_lcn * params.cluster_size;
        let blank = vec![0u8; record_size as usize];
        dev.write_all_at(mft_start + 4 * record_size, &blank)
            .unwrap();

        let bm = locate_bitmap_io(&mut dev).unwrap();
        let cluster_capacity = params.volume_bytes().div_ceil(params.cluster_size.max(1));

        assert!(
            !bm.other_protected
                .iter()
                .any(|&(start, end)| start == 0 && end >= cluster_capacity),
            "one unlocatable metafile must not produce a whole-volume range. Got {:?}",
            bm.other_protected
        );
        assert!(
            !bm.other_protected.is_empty(),
            "the metafiles that ARE locatable must still be protected. Got {:?}",
            bm.other_protected
        );
        assert!(
            !bm.covers_the_volumes_own(cluster_capacity - 1, 1),
            "an ordinary cluster must stay free when one metafile could not be located -- \
             refusing it is what broke `rm` on every cluster of a real volume"
        );
        // AND THE VOLUME IS STILL USABLE. This is the assertion that
        // catches the defect directly: count how much of the volume is
        // refused. The revision this replaces refused 4095 of 4095
        // clusters on a real volume; a correct guard refuses only what
        // the metafiles occupy, which on THIS 64 MiB fixture is about
        // 6%.
        //
        // The quarter-of-the-volume bound is a fact about this fixture,
        // NOT a general property -- the same correct guard refuses
        // 60.3% of a 2 MiB volume, where $LogFile, $UpCase and $SDS
        // genuinely are most of it. Sound here because this test only
        // ever runs against `formatted_dev()`; do not lift the bound
        // into a test on a smaller volume.
        //
        // Not a mid-volume spot check: `mkfs` places $MFTMirr at
        // `cluster_count / 2` (mkfs.rs), so the middle of the volume
        // IS legitimately protected -- an earlier version of this test
        // asserted otherwise and failed, correctly.
        let refused = (0..cluster_capacity)
            .filter(|&lcn| bm.covers_the_volumes_own(lcn, 1))
            .count() as u64;
        assert!(
            refused * 4 < cluster_capacity,
            "{refused} of {cluster_capacity} clusters refused -- a guard that refuses most of \
             the volume has stopped being a guard and started being an outage. The metafiles \
             on a 64 MiB volume do not occupy a quarter of it."
        );
    }

    /// THE WIRING, not just the mechanism. Every test of
    /// `covers_the_volumes_own` and `other_protected`'s effect
    /// constructs a `BitmapLocation` by hand -- `located_with` sets
    /// `other_protected` directly. None of them would notice
    /// `locate_bitmap_io` itself failing to populate that field from a
    /// real volume: a mutation that made it always return `Vec::new()`
    /// survives every one of those tests, because they never call
    /// `locate_bitmap_io` at all.
    #[test]
    fn locate_bitmap_io_actually_populates_other_protected_from_a_real_volume() {
        let mut dev = formatted_dev();
        let bm = locate_bitmap_io(&mut dev).unwrap();
        let volume_bytes = bm.params.volume_bytes();
        let cluster_size = bm.params.cluster_size.max(1);
        let cluster_capacity = volume_bytes.div_ceil(cluster_size);

        // NON-EMPTINESS IS NOT ENOUGH, and asserting only that is how
        // the first revision of this shipped a guard that refused every
        // cluster on a third-party volume: the whole-volume fallback it
        // produced on any lookup failure was itself a non-empty list of
        // exactly one range, so `!is_empty()` passed on the failure it
        // was supposed to detect. These assertions fail on that shape.
        assert!(
            !bm.other_protected.is_empty(),
            "a formatted volume has locatable system metafiles to report"
        );
        assert!(
            !bm.other_protected
                .iter()
                .any(|&(start, end)| start == 0 && end >= cluster_capacity),
            "no single range may span the whole volume ({cluster_capacity} clusters): that is \
             the shape a failed lookup used to manufacture, and it refuses every cluster on \
             the volume. Got {:?}",
            bm.other_protected
        );
        assert!(
            bm.other_protected.len() > 1,
            "several metafiles are located separately, so several ranges are expected -- one \
             range is the signature of the old whole-volume fallback. Got {:?}",
            bm.other_protected
        );
        // And an ordinary data cluster past all the metadata must stay
        // free, or `unlink` cannot free anything.
        assert!(
            !bm.covers_the_volumes_own(cluster_capacity - 1, 1),
            "the last cluster on the volume is not any metafile's, and must not be refused"
        );

        for &(start, end) in &bm.other_protected {
            assert!(start < end, "empty or inverted range ({start}, {end})");
            assert!(
                end <= cluster_capacity,
                "range ({start}, {end}) exceeds the volume's {cluster_capacity} clusters"
            );
        }
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

#[cfg(test)]
mod range_bound_tests {
    use super::tests::MemDev;
    use super::*;
    use crate::mft_io::BootParams;

    fn bm(total_bits: u64) -> BitmapLocation {
        BitmapLocation {
            params: BootParams {
                bytes_per_sector: 512,
                sectors_per_cluster: 8,
                cluster_size: 4096,
                mft_lcn: 4,
                file_record_size: 1024,
                total_sectors: 1 << 20,
                serial_number: 0,
                oem_id: *b"NTFS    ",
            },
            runs: Vec::new(),
            total_bits,
            value_length: total_bits / 8,
            // This fixture's own test calls `mutate_bits_io` directly,
            // which never consults `covers_the_volumes_own` -- but
            // `mft_clusters: 0` now means "protect everything," not
            // "not modelled," so a non-zero placeholder keeps that true
            // for any test added here later too.
            mft_clusters: 1,
            other_protected: Vec::new(),
        }
    }

    /// `lcn` and `n` reach `mutate_bits_io` from a file's own run list,
    /// where `decode_runs` permits an LCN up to 2^63 and a length up to
    /// 2^64. `lcn + n` wrapping to a small number made the range guard
    /// pass, and `end_byte_excl - first_byte` then underflowed into an
    /// allocation of nearly 2^61 bytes -- a capacity-overflow panic, or
    /// an abort for values that merely do not fit.
    #[test]
    fn a_range_whose_end_wraps_is_refused_by_the_guard_that_bounds_it() {
        let mut dev = MemDev::new(8192);
        let bm = bm(32);

        // The pair the wrap needs: 2^63 + 2^63 is zero, which is
        // "inside" any bitmap.
        let why = mutate_bits_io(&mut dev, &bm, 1 << 63, 1 << 63, true).unwrap_err();
        assert!(why.contains("not inside"), "{why}");

        // And the ordinary out-of-range case still reads the same way.
        let why = mutate_bits_io(&mut dev, &bm, 30, 4, true).unwrap_err();
        assert!(why.contains("not inside"), "{why}");
    }
}

#[cfg(test)]
mod volume_own_tests {
    use super::*;
    use crate::mft_io::BootParams;

    fn located(mft_lcn: u64, mft_clusters: u64) -> BitmapLocation {
        located_with(mft_lcn, mft_clusters, Vec::new())
    }

    fn located_with(
        mft_lcn: u64,
        mft_clusters: u64,
        other_protected: Vec<(u64, u64)>,
    ) -> BitmapLocation {
        BitmapLocation {
            params: BootParams {
                bytes_per_sector: 512,
                sectors_per_cluster: 8,
                cluster_size: 4096,
                mft_lcn,
                file_record_size: 1024,
                total_sectors: 1 << 20,
                serial_number: 0,
                oem_id: *b"NTFS    ",
            },
            runs: Vec::new(),
            total_bits: 1 << 16,
            value_length: 1 << 13,
            mft_clusters,
            other_protected,
        }
    }

    /// `unlink` and `truncate` push every run of every non-resident
    /// attribute of a file straight into `free_io`, bounded only by
    /// `$Bitmap`'s own size. A file record whose `$DATA` runs overlap
    /// `$MFT` therefore made `unlink` mark live system clusters free,
    /// and the next allocation handed them out.
    #[test]
    fn a_files_runs_may_not_free_the_volumes_own_clusters() {
        let bm = located(64, 32); // $MFT at clusters 64..96

        // The boot sector is cluster 0.
        assert!(bm.covers_the_volumes_own(0, 1));
        assert!(bm.covers_the_volumes_own(0, 4096));

        // $MFT, from any direction.
        assert!(bm.covers_the_volumes_own(64, 1), "its first cluster");
        assert!(bm.covers_the_volumes_own(95, 1), "its last");
        assert!(bm.covers_the_volumes_own(60, 8), "a run reaching into it");
        assert!(bm.covers_the_volumes_own(90, 16), "a run leaving it");
        assert!(bm.covers_the_volumes_own(1, 1000), "a run swallowing it");

        // An ordinary file's clusters.
        assert!(!bm.covers_the_volumes_own(1, 63), "up to $MFT");
        assert!(!bm.covers_the_volumes_own(96, 100), "past $MFT");
    }

    /// THIS ASSERTION WAS WRONG, and rust-fs-ntfs#157 is that it was
    /// wrong: this exact case -- `located(64, 0)`, "$MFT's own extent
    /// could not be determined" -- used to assert
    /// `!unknown.covers_the_volumes_own(64, 32)`, declaring clusters
    /// 64..96 (where `$MFT` actually lives on this fixture) UNPROTECTED
    /// precisely because nothing could be verified about them. That is
    /// backwards: not knowing whether a cluster is the volume's own is
    /// a reason to refuse freeing it, not a reason to allow it. A
    /// fragmented `$MFT` reaches this path in production --
    /// `nonresident_contiguous_disk_range` refuses any `$MFT` that is
    /// not a single extent, and `locate_bitmap_io` turns that `Err`
    /// into `mft_clusters = 0` via `.unwrap_or(0)` -- so this was not a
    /// hypothetical: an ordinary volume that has grown over time and
    /// fragmented its own `$MFT` had NOTHING protected but the boot
    /// sector, confirmed by execution against this crate's own
    /// `make_bm` fixture before this fix landed.
    ///
    /// The old expectation is not preserved alongside a new one. A
    /// passing test asserting the vulnerable behaviour, left beside a
    /// new test asserting its opposite, is a suite nobody can read as
    /// intentional; whichever a future change happens to break first is
    /// the one that looks like the regression.
    #[test]
    fn a_zero_mft_clusters_scalar_does_not_blanket_refuse_the_volume() {
        // $MFT's real runs, as `locate_bitmap_io` now supplies them --
        // `other_protected` carries them, so protection no longer
        // depends on the scalar at all.
        let unknown = located_with(64, 0, vec![(64, 96)]);

        // The boot sector, always.
        assert!(unknown.covers_the_volumes_own(0, 1));
        // $MFT is STILL protected -- by its runs, not by the scalar.
        assert!(
            unknown.covers_the_volumes_own(64, 32),
            "$MFT's own clusters must be refused even with mft_clusters == 0, because \
             other_protected carries its real runs"
        );
        // AND an unrelated cluster is NOT refused. This is the assertion
        // that matters, and it has now been wrong in two different
        // directions -- see this test's history in the commit message.
        assert!(
            !unknown.covers_the_volumes_own(5000, 1),
            "a zero mft_clusters scalar must NOT blanket-refuse the volume: measured on the \
             `ntfs` crate's own testdata/testfs1 (a $MFT with six runs, so the single-extent \
             helper feeding this scalar returns Err and it is 0), blanket-refusing declared \
             4095 of 4095 clusters the volume's own and broke `rm` on an ordinary file"
        );
    }

    /// The other five system metafiles this issue names -- `$MFTMirr`,
    /// `$LogFile`, `$AttrDef`, `$Bitmap` itself, `$Secure`, `$UpCase` --
    /// are refused exactly like `$MFT` is, via `other_protected`, not
    /// via a second hardcoded pair. `located_with` supplies the cluster
    /// range as `locate_bitmap_io` would, post byte-to-cluster
    /// conversion.
    #[test]
    fn a_files_runs_may_not_free_any_other_system_metafiles_clusters_either() {
        // $MFT at 64..96 (as above), $MFTMirr at 200..204.
        let bm = located_with(64, 32, vec![(200, 204)]);

        assert!(
            bm.covers_the_volumes_own(200, 1),
            "$MFTMirr's first cluster"
        );
        assert!(bm.covers_the_volumes_own(203, 1), "$MFTMirr's last cluster");
        assert!(
            bm.covers_the_volumes_own(198, 4),
            "a run reaching into $MFTMirr"
        );
        assert!(bm.covers_the_volumes_own(202, 10), "a run leaving $MFTMirr");

        // Still an ordinary, unrelated file's clusters.
        assert!(
            !bm.covers_the_volumes_own(100, 50),
            "between $MFT and $MFTMirr"
        );
        assert!(!bm.covers_the_volumes_own(204, 100), "past $MFTMirr");

        // $MFT protection is unaffected by $MFTMirr being tracked too.
        assert!(bm.covers_the_volumes_own(64, 1));
    }

    /// `other_protected` carrying a single whole-volume range -- the
    /// shape `read::other_protected_metafile_ranges_io` returns when
    /// ANY of the five records it locates could not be determined --
    /// refuses everything past the boot sector, with no special case
    /// needed in `covers_the_volumes_own` to recognise it as such: the
    /// ordinary overlap check already does the job. Exercises the
    /// SAME fail-closed property as
    /// `an_undetermined_mft_extent_fails_closed_rather_than_open`,
    /// through the OTHER of the two mechanisms this fix adds.
    #[test]
    fn one_unlocatable_metafile_does_not_cost_the_whole_volume() {
        // $MFT and $MFTMirr located; another metafile simply absent.
        let bm = located_with(64, 32, vec![(64, 96), (200, 204)]);

        assert!(bm.covers_the_volumes_own(0, 1), "the boot sector");
        assert!(bm.covers_the_volumes_own(64, 1), "$MFT, from its runs");
        assert!(bm.covers_the_volumes_own(200, 1), "$MFTMirr, from its runs");
        assert!(
            !bm.covers_the_volumes_own(1000, 1),
            "a cluster no located metafile claims must stay free -- the guard protects what \
             it can find, and a metafile it could not find costs only that metafile's own \
             protection, not the volume's usability"
        );
    }
}
