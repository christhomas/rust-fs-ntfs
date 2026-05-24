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
    let ir_data_start = ir.attr_offset + ir_val_off;
    let block_size = u32::from_le_bytes([
        record[ir_data_start + 0x08],
        record[ir_data_start + 0x09],
        record[ir_data_start + 0x0A],
        record[ir_data_start + 0x0B],
    ]) as u64;
    if block_size == 0 {
        return Err("INDEX_ROOT block_size is zero".to_string());
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
    let bitmap = if bm_attr.is_resident {
        let off = bm_attr.resident_value_offset.ok_or("no value_offset")? as usize;
        let len = bm_attr.resident_value_length.ok_or("no value_length")? as usize;
        record[bm_attr.attr_offset + off..bm_attr.attr_offset + off + len].to_vec()
    } else {
        return Err("non-resident $Bitmap:$I30 unsupported in this MVP".to_string());
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
/// the on-disk byte offset.
pub fn vcn_to_disk_offset(ia: &IndexAllocation, vcn: u64) -> Result<u64, String> {
    let run = ia
        .runs
        .iter()
        .find(|r| vcn >= r.starting_vcn && vcn < r.starting_vcn + r.length)
        .ok_or_else(|| format!("VCN {vcn} not mapped in $INDEX_ALLOCATION"))?;
    let lcn = run.lcn.ok_or_else(|| format!("VCN {vcn} in sparse run"))?;
    Ok((lcn + (vcn - run.starting_vcn)) * ia.params.cluster_size)
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
    let disk_offset = vcn_to_disk_offset(ia, vcn)?;
    let mut buf = vec![0u8; ia.block_size as usize];
    io.read_exact_at(disk_offset, &mut buf)
        .map_err(|e| format!("read indx: {e}"))?;
    if &buf[0..4] != b"INDX" {
        return Err(format!(
            "block at VCN {vcn} (disk {disk_offset:#x}) is not an INDX record: {:02x?}",
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
    let mut block = read_indx_block_io(io, ia, vcn)?;
    mutate(&mut block)?;
    apply_fixup_on_write_magic(&mut block, ia.params.bytes_per_sector, b"INDX")?;

    let disk_offset = vcn_to_disk_offset(ia, vcn)?;
    io.write_all_at(disk_offset, &block)
        .map_err(|e| format!("write indx: {e}"))?;
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
        let ia = make_ia(4096, 4096, runs, vec![], 0);
        // VCN 0 → LCN 10 → byte offset 10 * 4096.
        assert_eq!(vcn_to_disk_offset(&ia, 0).unwrap(), 10 * 4096);
        // VCN 3 → LCN 13 → byte offset 13 * 4096.
        assert_eq!(vcn_to_disk_offset(&ia, 3).unwrap(), 13 * 4096);
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
        let ia = make_ia(4096, 4096, runs, vec![], 0);
        // VCN 2 maps to LCN 20 + (2-2) = 20.
        assert_eq!(vcn_to_disk_offset(&ia, 2).unwrap(), 20 * 4096);
        // VCN 4 → LCN 22.
        assert_eq!(vcn_to_disk_offset(&ia, 4).unwrap(), 22 * 4096);
    }

    #[test]
    fn vcn_to_disk_offset_in_sparse_run_errors() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 4,
            lcn: None,
        }];
        let ia = make_ia(4096, 4096, runs, vec![], 0);
        let err = vcn_to_disk_offset(&ia, 1).unwrap_err();
        assert!(err.contains("sparse"), "{err}");
    }

    #[test]
    fn vcn_to_disk_offset_past_end_errors() {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: 4,
            lcn: Some(10),
        }];
        let ia = make_ia(4096, 4096, runs, vec![], 0);
        let err = vcn_to_disk_offset(&ia, 99).unwrap_err();
        assert!(err.contains("not mapped"), "{err}");
    }
}
