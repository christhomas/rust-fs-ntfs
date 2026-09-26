//! INDX block read-modify-write (`$INDEX_ALLOCATION` contents).
//!
//! An INDX block is a multi-sector structure just like an MFT record,
//! with its own "INDX" magic and USA fixup. Block size is fixed per
//! directory by the `index_block_size` field in `$INDEX_ROOT`.
//!
//! References (no GPL code consulted): INDX record format and
//! $INDEX_ALLOCATION layout per Windows Internals 7th ed.
//! ch. "NTFS On-Disk Structure" and MS-FSCC.

use crate::attr_io::{self, AttrType};
use crate::block_io::{BlockIo, PathIo};
use crate::data_runs::{self, DataRun};
use crate::mft_io::{
    apply_fixup_on_read_magic, apply_fixup_on_write_magic, read_mft_record_io, BootParams,
};
use crate::mkfs::stream;

use std::path::Path;

/// The largest index block NTFS defines.
///
/// 64 KiB. The field is a `u32` and was checked only against zero.
const MAX_INDEX_BLOCK_SIZE: u64 = 65536;

/// Info required to locate + traverse `$INDEX_ALLOCATION` for a parent
/// directory.
pub struct IndexAllocation {
    pub params: BootParams,
    pub block_size: u64,
    /// Data runs of the `$INDEX_ALLOCATION:$I30` attribute.
    pub runs: Vec<DataRun>,
    /// Bytes of the $I30 named `$Bitmap` attribute (tracks which INDX
    /// blocks are in use). Bit `k` is 1 ⇒ block at VCN k is allocated.
    pub bitmap: Vec<u8>,
    /// Total bytes in `$INDEX_ALLOCATION:$I30`.
    pub data_length: u64,
}

impl IndexAllocation {
    /// Returns VCNs of allocated INDX blocks.
    pub fn allocated_block_vcns(&self) -> Vec<u64> {
        let mut out = Vec::new();
        let blocks_per_byte = 8;
        for (byte_idx, byte) in self.bitmap.iter().enumerate() {
            for bit in 0..8u32 {
                if byte & (1 << bit) != 0 {
                    let vcn = (byte_idx as u64 * blocks_per_byte as u64 + bit as u64)
                        * self.block_size
                        / self.params.cluster_size;
                    // Each INDX block starts at `block_index * block_size`,
                    // which in VCN units = block_index * (block_size / cluster_size).
                    // But since block_size can differ from cluster_size, we
                    // emit block _start_ VCN.
                    out.push(vcn);
                }
            }
        }
        out
    }
}

/// Load $INDEX_ALLOCATION + $Bitmap for a directory. Assumes the
/// parent is already known to have a non-resident index (flags & 0x1
/// on the `$INDEX_ROOT`'s INDEX_HEADER).
pub fn load_for_directory(
    image: &Path,
    parent_record_number: u64,
) -> Result<IndexAllocation, String> {
    let mut io = PathIo::open_ro(image)?;
    load_for_directory_io(&mut io, parent_record_number)
}

pub fn load_for_directory_io<T: BlockIo + ?Sized>(
    io: &mut T,
    parent_record_number: u64,
) -> Result<IndexAllocation, String> {
    let (params, record) = read_mft_record_io(io, parent_record_number)?;

    // Get block_size from $INDEX_ROOT:$I30.
    let ir = attr_io::find_attribute(&record, AttrType::IndexRoot, Some(stream::I30))
        .ok_or_else(|| "$INDEX_ROOT:$I30 not found on parent".to_string())?;
    let ir_val_off = ir.resident_value_offset.ok_or("no value_offset")? as usize;
    let ir_val_len = ir.resident_value_length.ok_or("no value_length")? as usize;
    // The block size sits at 0x08..0x0C of the $INDEX_ROOT value, so
    // there has to be that much value to read it from.
    if ir_val_len < 0x10 {
        return Err(format!(
            "$INDEX_ROOT:$I30 value is {ir_val_len} bytes, too short to hold an index header"
        ));
    }
    let ir_data_start = ir.attr_offset + ir_val_off;
    let block_size = u32::from_le_bytes([
        record[ir_data_start + 0x08],
        record[ir_data_start + 0x09],
        record[ir_data_start + 0x0A],
        record[ir_data_start + 0x0B],
    ]) as u64;
    // An index block is read whole into a buffer sized from this, and
    // it was checked only against zero: 0xFFFFFFFF is a 4 GiB
    // allocation per block, and anything below four bytes panics on
    // `&buf[0..4]` before the magic can be checked. NTFS index blocks
    // are between one sector and 64 KiB, in powers of two.
    if !(u64::from(params.bytes_per_sector)..=MAX_INDEX_BLOCK_SIZE).contains(&block_size)
        || !block_size.is_power_of_two()
    {
        return Err(format!(
            "$INDEX_ROOT:$I30 says its blocks are {block_size} bytes, which is not an \
             index block size"
        ));
    }

    // Get $INDEX_ALLOCATION:$I30 data runs.
    let ia = attr_io::find_attribute(&record, AttrType::IndexAllocation, Some(stream::I30))
        .ok_or_else(|| "$INDEX_ALLOCATION:$I30 not found".to_string())?;
    if ia.is_resident {
        return Err("$INDEX_ALLOCATION unexpectedly resident".to_string());
    }
    let mpo = ia
        .non_resident_mapping_pairs_offset
        .ok_or("no mapping_pairs_offset")? as usize;
    let runs =
        data_runs::decode_runs(&record[ia.attr_offset + mpo..ia.attr_offset + ia.attr_length])?;
    let data_length = ia.non_resident_value_length.ok_or("no value_length")?;

    // Get $Bitmap:$I30.
    let bm_attr = attr_io::find_attribute(&record, AttrType::Bitmap, Some(stream::I30))
        .ok_or_else(|| "$Bitmap:$I30 not found".to_string())?;
    // A DIRECTORY BIG ENOUGH PUSHES ITS OWN BITMAP OUT OF THE RECORD, and
    // this used to refuse it: "non-resident $Bitmap:$I30 unsupported in
    // this MVP". `load_for_directory_io` is the single door to
    // `$INDEX_ALLOCATION` for reading AND writing, and all of its call
    // sites propagate, so such a directory could not be listed, looked up
    // in, created in, renamed in or removed from -- the whole directory,
    // not the entry that overflowed (#174).
    //
    // One bit per index block, so the crossover is a directory with more
    // blocks than the record has spare bytes for: a few thousand entries
    // at 4 KiB blocks. Reading it is the same non-resident read every
    // other attribute gets.
    let bitmap = if bm_attr.is_resident {
        let off = bm_attr.resident_value_offset.ok_or("no value_offset")? as usize;
        let len = bm_attr.resident_value_length.ok_or("no value_length")? as usize;
        record[bm_attr.attr_offset + off..bm_attr.attr_offset + off + len].to_vec()
    } else {
        crate::read::read_attribute_value(
            io,
            parent_record_number,
            AttrType::Bitmap,
            Some(stream::I30),
        )
        .map_err(|e| format!("reading a non-resident $Bitmap:$I30: {e}"))?
    };

    Ok(IndexAllocation {
        params,
        block_size,
        runs,
        bitmap,
        data_length,
    })
}

/// Translate a VCN (relative to the start of `$INDEX_ALLOCATION`) to
/// the on-disk byte offset of a whole index block that starts there.
/// This helper returns an offset only when the entire block occupies one
/// run. Callers handling fragmented blocks must use piecewise block I/O.
///
/// THE WHOLE BLOCK HAS TO BE IN ONE RUN.
///
/// The run lookup below proves only that the block's FIRST cluster is
/// mapped, and the transfer the callers then make is `block_size`
/// bytes. An index block is 4096 bytes, so on a 512-byte-cluster volume
/// it is eight clusters, on a 1 KiB volume four, on a 2 KiB volume two
/// -- and `$INDEX_ALLOCATION` fragments as a directory grows, so a
/// block landing across a run boundary is a normal outcome rather than
/// a corrupt one.
///
/// Without this check the tail of the block is read from, and written
/// to, whichever clusters happen to follow the run's last one, which
/// belong to some other file. On read the borrowed sector tails usually
/// fail the update-sequence check, so a healthy directory simply stops
/// listing; that is the good case. `update_indx_block_io` writes
/// `block_size` bytes at the same offset, so a directory edit -- a
/// create, a delete -- silently overwrites unrelated file contents.
///
/// Reading and writing a straddling block in per-run pieces is the
/// complete answer. Refusing is the correct and safe half, and it is
/// what the read path already does by accident.
///
/// `device_bytes` is the size of the device the transfer will land on;
/// this used to pass `u64::MAX`, which left `cluster_span` bounding
/// against the volume alone -- the weaker of the two limits it applies.
pub fn vcn_to_disk_offset(
    ia: &IndexAllocation,
    vcn: u64,
    device_bytes: u64,
) -> Result<u64, String> {
    check_declared_block_extent(ia, vcn)?;
    let run = ia
        .runs
        .iter()
        .find(|r| {
            r.starting_vcn
                .checked_add(r.length)
                .is_some_and(|end| vcn >= r.starting_vcn && vcn < end)
        })
        .ok_or_else(|| format!("VCN {vcn} not mapped in $INDEX_ALLOCATION"))?;
    let lcn = run.lcn.ok_or_else(|| format!("VCN {vcn} in sparse run"))?;

    let block_clusters = ia.block_size.div_ceil(ia.params.cluster_size.max(1));
    let run_end_vcn = run.starting_vcn.checked_add(run.length).ok_or_else(|| {
        format!(
            "$INDEX_ALLOCATION run at VCN {} has no end",
            run.starting_vcn
        )
    })?;
    let block_end_vcn = vcn
        .checked_add(block_clusters)
        .ok_or_else(|| format!("an index block at VCN {vcn} has no end"))?;
    if block_end_vcn > run_end_vcn {
        return Err(format!(
            "index block at VCN {vcn} spans {block_clusters} clusters, past the end of \
             its run at VCN {run_end_vcn}; a block that straddles a run boundary is not \
             read or written as one transfer"
        ));
    }

    // Checked and bounded by the volume and the device, for the whole
    // index block.
    crate::mft_io::cluster_span(
        &ia.params,
        lcn,
        vcn - run.starting_vcn,
        0,
        ia.block_size,
        device_bytes,
    )
}

#[derive(Clone, Copy, Debug)]
struct MappedChunk {
    disk_offset: u64,
    cursor: usize,
    len: usize,
}

fn check_declared_block_extent(ia: &IndexAllocation, vcn: u64) -> Result<(), String> {
    let end = vcn
        .checked_mul(ia.params.cluster_size)
        .and_then(|start| start.checked_add(ia.block_size))
        .ok_or_else(|| format!("index block at VCN {vcn} has no byte end"))?;
    if end > ia.data_length {
        return Err(format!(
            "index block at VCN {vcn} ends at byte {end}, past $INDEX_ALLOCATION length {}",
            ia.data_length
        ));
    }
    Ok(())
}

/// Map one logical INDX block into transfers that never cross a data-run
/// boundary. The complete mapping is validated before the caller performs
/// any I/O, so a missing or sparse tail cannot leave a write half-finished.
fn map_indx_block(
    ia: &IndexAllocation,
    vcn: u64,
    device_bytes: u64,
) -> Result<Vec<MappedChunk>, String> {
    check_declared_block_extent(ia, vcn)?;
    let cluster_size = ia.params.cluster_size;
    if cluster_size == 0 {
        return Err("$INDEX_ALLOCATION has a zero-byte cluster size".to_string());
    }

    let block_len = usize::try_from(ia.block_size)
        .map_err(|_| format!("index block size {} does not fit in memory", ia.block_size))?;
    if block_len == 0 {
        return Err("index block size is zero".to_string());
    }
    let mut chunks = Vec::new();
    let mut cursor = 0usize;

    while cursor < block_len {
        let logical_cluster = vcn
            .checked_add(cursor as u64 / cluster_size)
            .ok_or_else(|| format!("an index block at VCN {vcn} has no end"))?;
        let byte_in_cluster = cursor as u64 % cluster_size;
        let run = ia
            .runs
            .iter()
            .find(|r| {
                r.starting_vcn
                    .checked_add(r.length)
                    .is_some_and(|end| logical_cluster >= r.starting_vcn && logical_cluster < end)
            })
            .ok_or_else(|| format!("VCN {logical_cluster} not mapped in $INDEX_ALLOCATION"))?;
        let lcn = run
            .lcn
            .ok_or_else(|| format!("VCN {logical_cluster} in sparse run"))?;
        let run_end_vcn = run.starting_vcn.checked_add(run.length).ok_or_else(|| {
            format!(
                "$INDEX_ALLOCATION run at VCN {} has no end",
                run.starting_vcn
            )
        })?;
        let bytes_in_run = run_end_vcn
            .checked_sub(logical_cluster)
            .and_then(|clusters| clusters.checked_mul(cluster_size))
            .and_then(|bytes| bytes.checked_sub(byte_in_cluster))
            .ok_or_else(|| {
                format!(
                    "$INDEX_ALLOCATION run at VCN {} cannot cover block byte {cursor}",
                    run.starting_vcn
                )
            })?;
        let len = usize::try_from(bytes_in_run.min((block_len - cursor) as u64))
            .map_err(|_| "index block transfer does not fit in memory".to_string())?;
        if len == 0 {
            return Err(format!(
                "$INDEX_ALLOCATION run at VCN {} contributes no bytes to the block",
                run.starting_vcn
            ));
        }
        let disk_offset = crate::mft_io::cluster_span(
            &ia.params,
            lcn,
            logical_cluster - run.starting_vcn,
            byte_in_cluster,
            len as u64,
            device_bytes,
        )?;
        chunks.push(MappedChunk {
            disk_offset,
            cursor,
            len,
        });
        cursor += len;
    }

    Ok(chunks)
}

fn read_raw_indx_block_io<T: BlockIo + ?Sized>(
    io: &mut T,
    ia: &IndexAllocation,
    vcn: u64,
) -> Result<(Vec<u8>, Vec<MappedChunk>), String> {
    let chunks = map_indx_block(ia, vcn, io.size())?;
    let mut block = vec![0u8; ia.block_size as usize];
    for chunk in &chunks {
        io.read_exact_at(
            chunk.disk_offset,
            &mut block[chunk.cursor..chunk.cursor + chunk.len],
        )
        .map_err(|e| format!("read indx: {e}"))?;
    }
    Ok((block, chunks))
}

/// Read an INDX block at the given VCN, applying fixup. Returns the
/// clean block bytes. The caller must know `block_size` from the
/// `IndexAllocation` handle.
pub fn read_indx_block(image: &Path, ia: &IndexAllocation, vcn: u64) -> Result<Vec<u8>, String> {
    let mut io = PathIo::open_ro(image)?;
    read_indx_block_io(&mut io, ia, vcn)
}

pub fn read_indx_block_io<T: BlockIo + ?Sized>(
    io: &mut T,
    ia: &IndexAllocation,
    vcn: u64,
) -> Result<Vec<u8>, String> {
    let (mut buf, chunks) = read_raw_indx_block_io(io, ia, vcn)?;
    if &buf[0..4] != b"INDX" {
        return Err(format!(
            "block at VCN {vcn} (disk {:#x}) is not an INDX record: {:02x?}",
            chunks[0].disk_offset,
            &buf[0..4]
        ));
    }
    apply_fixup_on_read_magic(&mut buf, ia.params.bytes_per_sector, b"INDX")?;
    Ok(buf)
}

/// Read-modify-write an INDX block. `mutate` sees the clean (post-fixup)
/// bytes. Fixup is re-applied before write, and the whole block is
/// fsync'd.
pub fn update_indx_block<F>(
    image: &Path,
    ia: &IndexAllocation,
    vcn: u64,
    mutate: F,
) -> Result<(), String>
where
    F: FnOnce(&mut [u8]) -> Result<(), String>,
{
    let mut io = PathIo::open_rw(image)?;
    update_indx_block_io(&mut io, ia, vcn, mutate)
}

pub fn update_indx_block_io<T, F>(
    io: &mut T,
    ia: &IndexAllocation,
    vcn: u64,
    mutate: F,
) -> Result<(), String>
where
    T: BlockIo + ?Sized,
    F: FnOnce(&mut [u8]) -> Result<(), String>,
{
    let (previous, chunks) = read_raw_indx_block_io(io, ia, vcn)?;
    let mut block = previous.clone();
    apply_fixup_on_read_magic(&mut block, ia.params.bytes_per_sector, b"INDX")?;
    mutate(&mut block)?;
    apply_fixup_on_write_magic(&mut block, ia.params.bytes_per_sector, b"INDX")?;

    for (i, chunk) in chunks.iter().enumerate() {
        if let Err(e) = io.write_all_at(
            chunk.disk_offset,
            &block[chunk.cursor..chunk.cursor + chunk.len],
        ) {
            let failure = format!("write indx: {e}");
            let mut rollback_failure = None;
            // The failing write may itself have written a prefix, so restore it
            // along with every earlier chunk that definitely landed.
            for touched in &chunks[..=i] {
                if let Err(rollback) = io.write_all_at(
                    touched.disk_offset,
                    &previous[touched.cursor..touched.cursor + touched.len],
                ) {
                    if rollback_failure.is_none() {
                        rollback_failure = Some(rollback);
                    }
                }
            }
            let sync_failure = io.sync().err();
            return Err(match (rollback_failure, sync_failure) {
                (None, None) => failure,
                (Some(rollback), _) => format!(
                    "{failure}; and rolling the INDX block back failed too ({rollback}) — run chkdsk"
                ),
                (None, Some(sync)) => format!(
                    "{failure}; the INDX rollback was written but syncing it failed ({sync}) — run chkdsk"
                ),
            });
        }
    }
    io.sync()?;
    Ok(())
}

/// INDX block header offsets.
pub const INDX_USA_OFFSET_FIELD: usize = 0x04;
pub const INDX_USA_COUNT_FIELD: usize = 0x06;
/// INDEX_HEADER starts here within an INDX block.
pub const INDX_INDEX_HEADER_OFFSET: usize = 0x18;

/// Offset of the `first_entry` field inside the INDEX_HEADER.
pub const IH_FIRST_ENTRY_OFFSET: usize = 0x00;
pub const IH_TOTAL_SIZE_OF_ENTRIES: usize = 0x04;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ia(
        block_size: u64,
        cluster_size: u64,
        runs: Vec<DataRun>,
        bitmap: Vec<u8>,
        data_length: u64,
    ) -> IndexAllocation {
        IndexAllocation {
            params: BootParams {
                bytes_per_sector: 512,
                sectors_per_cluster: cluster_size / 512,
                cluster_size,
                mft_lcn: 4,
                file_record_size: 1024,
                // A real boot sector always says how big the volume is,
                // and cluster_span judges every transfer against it. 512 MiB
                // at 512-byte sectors is larger than anything these tests
                // address, so the bound is present without being the thing
                // under test.
                total_sectors: 1 << 20,
                serial_number: 0,
                index_block_size: 4096,
                oem_id: *b"NTFS    ",
            },
            block_size,
            runs,
            bitmap,
            data_length,
        }
    }

    // --- allocated_block_vcns ---------------------------------------------

    #[test]
    fn allocated_block_vcns_empty_bitmap_yields_empty() {
        let ia = make_ia(4096, 4096, vec![], vec![0u8; 4], 0);
        assert!(ia.allocated_block_vcns().is_empty());
    }

    #[test]
    fn allocated_block_vcns_decodes_set_bits_in_order() {
        // 4 KiB block size, 4 KiB cluster size → block_size/cluster_size = 1,
        // so block index = VCN.  bitmap byte 0 = 0b0000_0101 ⇒ blocks 0, 2.
        let ia = make_ia(4096, 4096, vec![], vec![0b0000_0101u8], 0);
        let vcns = ia.allocated_block_vcns();
        assert_eq!(vcns, vec![0, 2]);
    }

    #[test]
    fn allocated_block_vcns_spans_multiple_bytes() {
        // Bitmap byte 0 = 0b1000_0000 (bit 7), byte 1 = 0b0000_0010 (bit 1
        // of byte 1 ⇒ block index 9). VCN = block_index for 1:1 block:cluster.
        let ia = make_ia(4096, 4096, vec![], vec![0b1000_0000u8, 0b0000_0010u8], 0);
        let vcns = ia.allocated_block_vcns();
        assert_eq!(vcns, vec![7, 9]);
    }

    // --- vcn_to_disk_offset ------------------------------------------------

    #[test]
    fn vcn_to_disk_offset_inside_first_run_uses_cluster_size_arithmetic() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 4,
            lcn: Some(10),
        }];
        let ia = make_ia(4096, 4096, runs, vec![], 4 * 4096);
        // VCN 0 → LCN 10 → byte offset 10 * 4096.
        assert_eq!(vcn_to_disk_offset(&ia, 0, u64::MAX).unwrap(), 10 * 4096);
        // VCN 3 → LCN 13 → byte offset 13 * 4096.
        assert_eq!(vcn_to_disk_offset(&ia, 3, u64::MAX).unwrap(), 13 * 4096);
    }

    #[test]
    fn vcn_to_disk_offset_in_second_run() {
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 2,
                lcn: Some(10),
            },
            DataRun {
                starting_vcn: 2,
                length: 3,
                lcn: Some(20),
            },
        ];
        let ia = make_ia(4096, 4096, runs, vec![], 5 * 4096);
        // VCN 2 maps to LCN 20 + (2-2) = 20.
        assert_eq!(vcn_to_disk_offset(&ia, 2, u64::MAX).unwrap(), 20 * 4096);
        // VCN 4 → LCN 22.
        assert_eq!(vcn_to_disk_offset(&ia, 4, u64::MAX).unwrap(), 22 * 4096);
    }

    #[test]
    fn vcn_to_disk_offset_in_sparse_run_errors() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 4,
            lcn: None,
        }];
        let ia = make_ia(4096, 4096, runs, vec![], 4 * 4096);
        let err = vcn_to_disk_offset(&ia, 1, u64::MAX).unwrap_err();
        assert!(err.contains("sparse"), "{err}");
    }

    #[test]
    fn vcn_to_disk_offset_past_end_errors() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 4,
            lcn: Some(10),
        }];
        let ia = make_ia(4096, 4096, runs, vec![], 4 * 4096);
        let err = vcn_to_disk_offset(&ia, 99, u64::MAX).unwrap_err();
        assert!(err.contains("VCN 99"), "{err}");
        assert!(err.contains("past $INDEX_ALLOCATION length 16384"), "{err}");
    }

    // --- additional edge cases -------------------------------------------

    #[test]
    fn allocated_block_vcns_block_size_double_cluster_size() {
        // block_size = 8192, cluster_size = 4096 → VCN per block = 2.
        // Bit 0 (block 0) → VCN 0; bit 1 (block 1) → VCN 2.
        let ia = make_ia(8192, 4096, vec![], vec![0b0000_0011u8], 0);
        let vcns = ia.allocated_block_vcns();
        assert_eq!(vcns, vec![0, 2]);
    }

    #[test]
    fn allocated_block_vcns_all_bits_set_in_one_byte() {
        // 8 blocks all allocated; VCN-per-block = 1 (equal sizes).
        let ia = make_ia(4096, 4096, vec![], vec![0xFF], 0);
        let vcns = ia.allocated_block_vcns();
        assert_eq!(vcns, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn allocated_block_vcns_high_bit_of_last_byte() {
        // Bitmap = [0x00, 0x80]: bit 7 of byte 1 = block 15 → VCN 15.
        let ia = make_ia(4096, 4096, vec![], vec![0x00u8, 0x80u8], 0);
        let vcns = ia.allocated_block_vcns();
        assert_eq!(vcns, vec![15]);
    }

    #[test]
    fn vcn_to_disk_offset_small_cluster_size() {
        // cluster_size=512: VCN 0 at LCN 100 → disk = 100*512.
        // The run is 16 clusters so that the two VCNs addressed below
        // each have a whole 8-cluster block behind them; this test is
        // about the arithmetic, and the straddle check has its own.
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 16,
            lcn: Some(100),
        }];
        let ia = make_ia(4096, 512, runs, vec![], 16 * 512);
        assert_eq!(vcn_to_disk_offset(&ia, 0, u64::MAX).unwrap(), 100 * 512);
        assert_eq!(vcn_to_disk_offset(&ia, 1, u64::MAX).unwrap(), 101 * 512);
    }

    #[test]
    fn a_block_that_straddles_a_run_boundary_is_refused() {
        // 4 KiB blocks on a 512-byte-cluster volume: a block is eight
        // clusters. The first run holds four of them, so the block at
        // VCN 0 runs off its end and the tail belongs to whatever
        // follows LCN 103 -- not to LCN 500, where this attribute's
        // next four clusters actually are.
        let runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 4,
                lcn: Some(100),
            },
            DataRun {
                starting_vcn: 4,
                length: 4,
                lcn: Some(500),
            },
        ];
        let ia = make_ia(4096, 512, runs, vec![0x01], 4096);
        let err = vcn_to_disk_offset(&ia, 0, u64::MAX).unwrap_err();
        assert!(err.contains("straddles"), "{err}");
    }

    #[test]
    fn a_block_that_exactly_fills_its_run_is_allowed() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 8,
            lcn: Some(100),
        }];
        let ia = make_ia(4096, 512, runs, vec![0x01], 4096);
        assert_eq!(vcn_to_disk_offset(&ia, 0, u64::MAX).unwrap(), 100 * 512);
    }

    #[test]
    fn a_mapped_block_past_the_declared_allocation_length_is_refused() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 8,
            lcn: Some(100),
        }];
        // Eight clusters are physically mapped, but only four belong to
        // the declared value. The remaining clusters cannot be read or
        // written as part of an INDX block.
        let ia = make_ia(4096, 512, runs, vec![0x01], 2048);
        let mut dev = MemDev(vec![0; 110 * 512]);
        assert!(map_indx_block(&ia, 0, dev.size())
            .unwrap_err()
            .contains("length"));
        assert!(vcn_to_disk_offset(&ia, 0, dev.size())
            .unwrap_err()
            .contains("length"));
        assert!(read_indx_block_io(&mut dev, &ia, 0)
            .unwrap_err()
            .contains("length"));
    }

    #[test]
    fn vcn_to_disk_offset_at_run_boundary_is_exact() {
        // Run covers VCNs 0..4. VCN 3 (last) is inside; VCN 4 (first of next) errors.
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 4,
            lcn: Some(10),
        }];
        let ia = make_ia(4096, 4096, runs, vec![], 4 * 4096);
        assert!(vcn_to_disk_offset(&ia, 3, u64::MAX).is_ok());
        assert!(vcn_to_disk_offset(&ia, 4, u64::MAX).is_err());
    }

    // --- read_indx_block_io / update_indx_block_io -------------------------

    struct MemDev(Vec<u8>);

    impl BlockIo for MemDev {
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

    /// Build a 4096-byte INDX block with a valid USA that passes fixup.
    /// bytes_per_sector=512 → 8 sectors → usa_count=9, usa_offset=0x28.
    fn valid_indx_block() -> Vec<u8> {
        const BLOCK: usize = 4096;
        const BPS: usize = 512;
        const SECTORS: usize = BLOCK / BPS;
        const USA_OFFSET: usize = 0x28;
        const USN: [u8; 2] = [0x01, 0x00];

        let mut block = vec![0u8; BLOCK];
        block[0..4].copy_from_slice(b"INDX");
        block[0x04..0x06].copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
        block[0x06..0x08].copy_from_slice(&((SECTORS + 1) as u16).to_le_bytes());
        block[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&USN);
        // Each sector's last 2 bytes must equal USN for fixup to accept.
        for s in 0..SECTORS {
            let tail = (s + 1) * BPS - 2;
            block[tail..tail + 2].copy_from_slice(&USN);
        }
        block
    }

    fn ia_with_run(lcn: u64, cluster_size: u64) -> IndexAllocation {
        make_ia(
            4096,
            cluster_size,
            vec![DataRun {
                starting_vcn: 0,
                length: 1,
                lcn: Some(lcn),
            }],
            vec![0x01],
            4096,
        )
    }

    #[test]
    fn read_indx_block_io_returns_clean_block() {
        let block = valid_indx_block();
        // Place block at cluster 0 (disk offset 0).
        let mut dev = MemDev(block.clone());
        let ia = ia_with_run(0, 4096);
        let result = read_indx_block_io(&mut dev, &ia, 0).unwrap();
        assert_eq!(&result[0..4], b"INDX");
        assert_eq!(result.len(), 4096);
    }

    #[test]
    fn read_indx_block_io_bad_magic_fails() {
        let mut block = valid_indx_block();
        block[0] = 0xFF; // corrupt magic
        let mut dev = MemDev(block);
        let ia = ia_with_run(0, 4096);
        assert!(read_indx_block_io(&mut dev, &ia, 0).is_err());
    }

    #[test]
    fn read_indx_block_io_usn_mismatch_fails() {
        let mut block = valid_indx_block();
        // Corrupt the USN at the first sector tail.
        block[510] = 0xFF;
        block[511] = 0xFF;
        let mut dev = MemDev(block);
        let ia = ia_with_run(0, 4096);
        assert!(read_indx_block_io(&mut dev, &ia, 0).is_err());
    }

    #[test]
    fn update_indx_block_io_mutates_block() {
        let block = valid_indx_block();
        let mut dev = MemDev(block);
        let ia = ia_with_run(0, 4096);
        // Write a marker byte inside the INDX data area (past the header).
        update_indx_block_io(&mut dev, &ia, 0, |blk| {
            blk[0x40] = 0xAB;
            Ok(())
        })
        .unwrap();
        // Read back and verify the byte survived the write-fixup round-trip.
        let readback = read_indx_block_io(&mut dev, &ia, 0).unwrap();
        assert_eq!(readback[0x40], 0xAB);
    }

    fn fragmented_ia() -> IndexAllocation {
        make_ia(
            4096,
            512,
            vec![
                DataRun {
                    starting_vcn: 0,
                    length: 4,
                    lcn: Some(100),
                },
                DataRun {
                    starting_vcn: 4,
                    length: 4,
                    lcn: Some(500),
                },
            ],
            vec![0x01],
            4096,
        )
    }

    fn fragmented_storage() -> Vec<u8> {
        let cluster = 512usize;
        let mut storage = vec![0x4Eu8; 510 * cluster];
        let block = valid_indx_block();
        storage[100 * cluster..104 * cluster].copy_from_slice(&block[..4 * cluster]);
        storage[500 * cluster..504 * cluster].copy_from_slice(&block[4 * cluster..]);
        storage
    }

    #[test]
    fn a_4k_block_split_across_two_512_byte_cluster_runs_round_trips() {
        let cluster = 512usize;
        let storage = fragmented_storage();
        let neighbour_after_first = storage[104 * cluster..108 * cluster].to_vec();
        let neighbour_after_second = storage[504 * cluster..508 * cluster].to_vec();
        let mut dev = MemDev(storage);
        let ia = fragmented_ia();

        let read = read_indx_block_io(&mut dev, &ia, 0).unwrap();
        assert_eq!(&read[0..4], b"INDX");
        update_indx_block_io(&mut dev, &ia, 0, |block| {
            block[0x40] = 0xAB;
            block[3000] = 0xCD;
            Ok(())
        })
        .unwrap();
        let readback = read_indx_block_io(&mut dev, &ia, 0).unwrap();
        assert_eq!(readback[0x40], 0xAB);
        assert_eq!(readback[3000], 0xCD);
        assert_eq!(&dev.0[104 * cluster..108 * cluster], &neighbour_after_first);
        assert_eq!(
            &dev.0[504 * cluster..508 * cluster],
            &neighbour_after_second
        );
    }

    struct FailSecondWrite {
        inner: MemDev,
        writes: usize,
    }

    impl BlockIo for FailSecondWrite {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            self.inner.read_exact_at(offset, buf)
        }

        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            self.writes += 1;
            if self.writes == 2 {
                // A failed write is allowed to have changed a prefix. The
                // rollback must therefore include this chunk, not just the
                // first chunk that completed.
                let partial = 17usize.min(buf.len());
                self.inner.write_all_at(offset, &buf[..partial])?;
                return Err("injected second-piece failure".to_string());
            }
            self.inner.write_all_at(offset, buf)
        }

        fn size(&self) -> u64 {
            self.inner.size()
        }
    }

    #[test]
    fn a_partial_piecewise_write_is_rolled_back_to_the_raw_preimage() {
        let storage = fragmented_storage();
        let before = storage.clone();
        let mut dev = FailSecondWrite {
            inner: MemDev(storage),
            writes: 0,
        };

        let err = update_indx_block_io(&mut dev, &fragmented_ia(), 0, |block| {
            block[0x40] = 0xAB;
            block[3000] = 0xCD;
            Ok(())
        })
        .unwrap_err();

        assert!(err.contains("injected second-piece failure"), "{err}");
        assert_eq!(dev.inner.0, before);
    }

    #[test]
    fn update_indx_block_io_block_at_nonzero_lcn() {
        let cluster_size = 4096u64;
        let lcn = 5u64;
        let disk_offset = lcn * cluster_size;
        let block = valid_indx_block();
        // Allocate device large enough to hold the block at its disk position.
        let mut storage = vec![0u8; (disk_offset as usize) + 4096];
        storage[disk_offset as usize..disk_offset as usize + 4096].copy_from_slice(&block);
        let mut dev = MemDev(storage);
        let ia = ia_with_run(lcn, cluster_size);
        let result = read_indx_block_io(&mut dev, &ia, 0).unwrap();
        assert_eq!(&result[0..4], b"INDX");
    }
}
