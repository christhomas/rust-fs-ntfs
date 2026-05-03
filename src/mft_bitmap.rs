//! Track MFT record allocation via `$MFT`'s unnamed `$Bitmap` attribute.
//!
//! Separate from the volume-level `$Bitmap` (MFT record 6, tracks clusters).
//! `$MFT`'s own `$Bitmap` (a `$Bitmap` attribute on record 0) has one bit
//! per MFT record: 1 ⇒ record in use, 0 ⇒ free.
//!
//! On small volumes this bitmap is typically resident. On larger volumes
//! it's non-resident and stored in clusters via its own data-run list.
//! Both cases are handled here.
//!
//! References (no GPL code consulted): $Bitmap and $MFT layout per
//! Windows Internals 7th ed. ch. "NTFS On-Disk Structure" and MS-FSCC.

use crate::attr_io::{self, AttrType};
use crate::block_io::{BlockIo, PathIo};
use crate::data_runs::{self, DataRun};
use crate::mft_io::{read_mft_record_io, update_mft_record_io, BootParams};

use std::path::Path;

/// `$MFT` is always MFT record 0.
const MFT_RECORD_NUMBER: u64 = 0;

pub struct MftBitmap {
    pub params: BootParams,
    pub layout: MftBitmapLayout,
}

pub enum MftBitmapLayout {
    Resident {
        /// Byte offset within the MFT record where the resident data starts.
        data_offset_in_record: usize,
        /// Current resident bytes of the bitmap.
        bytes: Vec<u8>,
        /// Record-number ceiling (bitmap length in bits).
        total_bits: u64,
    },
    NonResident {
        runs: Vec<DataRun>,
        total_bits: u64,
    },
}

pub fn locate(image: &Path) -> Result<MftBitmap, String> {
    let mut io = PathIo::open_ro(image)?;
    locate_io(&mut io)
}

pub fn locate_io<T: BlockIo + ?Sized>(io: &mut T) -> Result<MftBitmap, String> {
    let (params, record) = read_mft_record_io(io, MFT_RECORD_NUMBER)?;

    // $MFT's unnamed $Bitmap (attribute type 0xB0, name "").
    let bm = attr_io::find_attribute(&record, AttrType::Bitmap, None)
        .ok_or_else(|| "$MFT has no unnamed $Bitmap".to_string())?;

    let layout = if bm.is_resident {
        let val_off = bm.resident_value_offset.ok_or("no value_offset")? as usize;
        let val_len = bm.resident_value_length.ok_or("no value_length")? as usize;
        let data_offset_in_record = bm.attr_offset + val_off;
        let bytes = record[data_offset_in_record..data_offset_in_record + val_len].to_vec();
        MftBitmapLayout::Resident {
            data_offset_in_record,
            bytes,
            total_bits: val_len as u64 * 8,
        }
    } else {
        let mpo = bm
            .non_resident_mapping_pairs_offset
            .ok_or("no mapping_pairs_offset")? as usize;
        let runs =
            data_runs::decode_runs(&record[bm.attr_offset + mpo..bm.attr_offset + bm.attr_length])?;
        let data_length = bm.non_resident_value_length.ok_or("no value_length")?;
        MftBitmapLayout::NonResident {
            runs,
            total_bits: data_length * 8,
        }
    };

    Ok(MftBitmap { params, layout })
}

/// Is MFT record `n` marked in-use in `$MFT:$Bitmap`?
pub fn is_allocated(image: &Path, bm: &MftBitmap, n: u64) -> Result<bool, String> {
    let mut io = PathIo::open_ro(image)?;
    is_allocated_io(&mut io, bm, n)
}

pub fn is_allocated_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &MftBitmap,
    n: u64,
) -> Result<bool, String> {
    let byte_idx = n / 8;
    let bit = (n % 8) as u8;
    let byte = read_bitmap_byte_io(io, bm, byte_idx)?;
    Ok((byte >> bit) & 1 != 0)
}

/// Find the first free MFT record number at or after `hint`. Returns
/// `None` if the bitmap is fully allocated. (Growing `$MFT` itself is
/// a separate concern — future W2.6 work.)
pub fn find_free_record(image: &Path, bm: &MftBitmap, hint: u64) -> Result<Option<u64>, String> {
    let mut io = PathIo::open_ro(image)?;
    find_free_record_io(&mut io, bm, hint)
}

pub fn find_free_record_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &MftBitmap,
    hint: u64,
) -> Result<Option<u64>, String> {
    let total = match &bm.layout {
        MftBitmapLayout::Resident { total_bits, .. } => *total_bits,
        MftBitmapLayout::NonResident { total_bits, .. } => *total_bits,
    };
    // Two passes: [hint..total), then [0..hint).
    for (begin, finish) in [(hint, total), (0, hint.min(total))] {
        let mut n = begin;
        while n < finish {
            let byte_idx = n / 8;
            let bit = (n % 8) as u8;
            let byte = read_bitmap_byte_io(io, bm, byte_idx)?;
            if (byte >> bit) & 1 == 0 {
                return Ok(Some(n));
            }
            n += 1;
        }
    }
    Ok(None)
}

/// Count free MFT record slots in `$MFT:$Bitmap`.
pub fn count_free(image: &Path, bm: &MftBitmap) -> Result<u64, String> {
    let mut io = PathIo::open_ro(image)?;
    count_free_io(&mut io, bm)
}

pub fn count_free_io<T: BlockIo + ?Sized>(io: &mut T, bm: &MftBitmap) -> Result<u64, String> {
    match &bm.layout {
        MftBitmapLayout::Resident {
            bytes, total_bits, ..
        } => {
            let set: u64 = bytes.iter().map(|b| b.count_ones() as u64).sum();
            Ok(total_bits.saturating_sub(set))
        }
        MftBitmapLayout::NonResident { total_bits, .. } => {
            let total_bytes = total_bits.div_ceil(8);
            let mut set: u64 = 0;
            for i in 0..total_bytes {
                set += read_bitmap_byte_io(io, bm, i)?.count_ones() as u64;
            }
            Ok(total_bits.saturating_sub(set))
        }
    }
}

/// Mark MFT record `n` as allocated (set bit = 1).
pub fn allocate(image: &Path, bm: &MftBitmap, n: u64) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    allocate_io(&mut io, bm, n)
}

pub fn allocate_io<T: BlockIo + ?Sized>(io: &mut T, bm: &MftBitmap, n: u64) -> Result<(), String> {
    mutate_bit_io(io, bm, n, true)
}

/// Mark MFT record `n` as free (set bit = 0).
pub fn free(image: &Path, bm: &MftBitmap, n: u64) -> Result<(), String> {
    let mut io = PathIo::open_rw(image)?;
    free_io(&mut io, bm, n)
}

pub fn free_io<T: BlockIo + ?Sized>(io: &mut T, bm: &MftBitmap, n: u64) -> Result<(), String> {
    mutate_bit_io(io, bm, n, false)
}

fn mutate_bit_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &MftBitmap,
    n: u64,
    set: bool,
) -> Result<(), String> {
    let byte_idx = n / 8;
    let bit = (n % 8) as u8;
    let mut byte = read_bitmap_byte_io(io, bm, byte_idx)?;
    let cur = (byte >> bit) & 1 != 0;
    if set && cur {
        return Err(format!("MFT record {n} already allocated"));
    }
    if !set && !cur {
        return Err(format!("MFT record {n} already free"));
    }
    if set {
        byte |= 1 << bit;
    } else {
        byte &= !(1 << bit);
    }
    write_bitmap_byte_io(io, bm, byte_idx, byte)
}

fn read_bitmap_byte_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &MftBitmap,
    byte_idx: u64,
) -> Result<u8, String> {
    match &bm.layout {
        MftBitmapLayout::Resident { bytes, .. } => {
            let i = byte_idx as usize;
            if i >= bytes.len() {
                return Err(format!(
                    "byte_idx {i} past resident bitmap length {}",
                    bytes.len()
                ));
            }
            Ok(bytes[i])
        }
        MftBitmapLayout::NonResident { runs, .. } => {
            let (_, disk_offset) = disk_offset_for_byte(bm, runs, byte_idx)?;
            let mut b = [0u8; 1];
            io.read_exact_at(disk_offset, &mut b)
                .map_err(|e| format!("read mftbm: {e}"))?;
            Ok(b[0])
        }
    }
}

fn write_bitmap_byte_io<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &MftBitmap,
    byte_idx: u64,
    v: u8,
) -> Result<(), String> {
    match &bm.layout {
        MftBitmapLayout::Resident {
            data_offset_in_record,
            ..
        } => {
            // Resident — patch the bitmap inside $MFT's own record.
            let dor = *data_offset_in_record;
            update_mft_record_io(io, MFT_RECORD_NUMBER, |record| {
                let i = dor + byte_idx as usize;
                if i >= record.len() {
                    return Err(format!("byte_idx {byte_idx} past record end"));
                }
                record[i] = v;
                Ok(())
            })
        }
        MftBitmapLayout::NonResident { runs, .. } => {
            let (_, disk_offset) = disk_offset_for_byte(bm, runs, byte_idx)?;
            io.write_all_at(disk_offset, &[v])
                .map_err(|e| format!("write mftbm: {e}"))?;
            io.sync()
        }
    }
}

fn disk_offset_for_byte(
    bm: &MftBitmap,
    runs: &[DataRun],
    byte_idx: u64,
) -> Result<(DataRun, u64), String> {
    let vcn = byte_idx / bm.params.cluster_size;
    let off_in_cluster = byte_idx % bm.params.cluster_size;
    let run = runs
        .iter()
        .find(|r| vcn >= r.starting_vcn && vcn < r.starting_vcn + r.length)
        .copied()
        .ok_or_else(|| format!("byte_idx {byte_idx} (VCN {vcn}) not mapped in $MFT:$Bitmap"))?;
    let lcn = run.lcn.ok_or("sparse $MFT bitmap run")?;
    let disk = (lcn + (vcn - run.starting_vcn)) * bm.params.cluster_size + off_in_cluster;
    Ok((run, disk))
}
