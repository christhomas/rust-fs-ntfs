//! Track MFT record allocation via `$MFT`'s unnamed `$Bitmap` attribute.
//!
//! Separate from the volume-level `$Bitmap` (MFT record 6, tracks clusters).
//! `$MFT`'s own `$Bitmap` (a `$Bitmap` attribute on record 0) has one bit
//! per MFT record: 1 ⇒ record in use, 0 ⇒ free.
//!
//! On small volumes this bitmap is typically resident. On larger volumes
//! it's non-resident and stored in clusters via its own data-run list.
//!
//! Both layouts are handled here, and **both read and write the same
//! storage**. The resident form is read back out of `$MFT`'s own record,
//! not from a copy taken at [`locate_io`] time: a copy goes stale the
//! instant a bit is written, and the callers of this module allocate a
//! record and then, on a later failure, free the bit they have just set.
//! A read that could not see the preceding write turned every one of
//! those rollbacks into a no-op and leaked the MFT record.
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
        /// Byte offset within `$MFT`'s own record where the resident data
        /// starts. Every read and every write goes through here — the
        /// bitmap's bytes are deliberately *not* cached in this struct.
        data_offset_in_record: usize,
        /// Length of the resident value, in bytes.
        value_length: usize,
        /// Record-number ceiling (bitmap length in bits).
        total_bits: u64,
    },
    NonResident {
        runs: Vec<DataRun>,
        total_bits: u64,
    },
}

impl MftBitmap {
    /// Record-number ceiling: how many bits this bitmap describes.
    pub fn total_bits(&self) -> u64 {
        match &self.layout {
            MftBitmapLayout::Resident { total_bits, .. } => *total_bits,
            MftBitmapLayout::NonResident { total_bits, .. } => *total_bits,
        }
    }
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
        if data_offset_in_record + val_len > record.len() {
            return Err(format!(
                "resident $MFT:$Bitmap [{data_offset_in_record}..\
                 {}] extends past record end {}",
                data_offset_in_record + val_len,
                record.len()
            ));
        }
        MftBitmapLayout::Resident {
            data_offset_in_record,
            value_length: val_len,
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
    let total = bm.total_bits();
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
    // One loop for both layouts: `read_bitmap_byte_io` is the only reader
    // of either, so this cannot drift away from what the allocator sees.
    let total_bits = bm.total_bits();
    let mut set: u64 = 0;
    for i in 0..total_bits.div_ceil(8) {
        set += read_bitmap_byte_io(io, bm, i)?.count_ones() as u64;
    }
    Ok(total_bits.saturating_sub(set))
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
        MftBitmapLayout::Resident {
            data_offset_in_record,
            value_length,
            ..
        } => {
            let i = byte_idx as usize;
            if i >= *value_length {
                return Err(format!(
                    "byte_idx {i} past resident bitmap length {value_length}"
                ));
            }
            // Read through to $MFT's own record — the same record
            // `write_bitmap_byte_io` writes to. Reading a snapshot taken at
            // `locate_io` time instead would not see our own writes.
            let (_, record) = read_mft_record_io(io, MFT_RECORD_NUMBER)?;
            record
                .get(data_offset_in_record + i)
                .copied()
                .ok_or_else(|| format!("byte_idx {byte_idx} past record end"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mft_io::BootParams;

    fn params(cluster_size: u64) -> BootParams {
        BootParams {
            bytes_per_sector: 512,
            sectors_per_cluster: cluster_size / 512,
            cluster_size,
            mft_lcn: 0,
            file_record_size: 1024,
            total_sectors: 0,
            serial_number: 0,
            oem_id: *b"NTFS    ",
        }
    }

    fn run(starting_vcn: u64, length: u64, lcn: u64) -> DataRun {
        DataRun {
            starting_vcn,
            length,
            lcn: Some(lcn),
        }
    }

    fn sparse_run(starting_vcn: u64, length: u64) -> DataRun {
        DataRun {
            starting_vcn,
            length,
            lcn: None,
        }
    }

    fn bm(cluster_size: u64) -> MftBitmap {
        MftBitmap {
            params: params(cluster_size),
            layout: MftBitmapLayout::Resident {
                data_offset_in_record: 0,
                value_length: 0,
                total_bits: 0,
            },
        }
    }

    // --- disk_offset_for_byte ---

    #[test]
    fn disk_offset_first_byte_in_first_run() {
        // cluster_size=512, byte 0 → VCN 0, lcn=10 → disk = 10*512 + 0 = 5120
        let b = bm(512);
        let runs = vec![run(0, 4, 10)];
        let (_, disk) = disk_offset_for_byte(&b, &runs, 0).unwrap();
        assert_eq!(disk, 10 * 512);
    }

    #[test]
    fn disk_offset_byte_within_cluster() {
        // cluster_size=512, byte 7 → VCN 0, off_in_cluster=7, disk = 10*512 + 7
        let b = bm(512);
        let runs = vec![run(0, 4, 10)];
        let (_, disk) = disk_offset_for_byte(&b, &runs, 7).unwrap();
        assert_eq!(disk, 10 * 512 + 7);
    }

    #[test]
    fn disk_offset_second_cluster() {
        // byte 512 → VCN 1 (cluster_size=512), lcn=10+1=11, disk = 11*512 + 0
        let b = bm(512);
        let runs = vec![run(0, 4, 10)];
        let (_, disk) = disk_offset_for_byte(&b, &runs, 512).unwrap();
        assert_eq!(disk, 11 * 512);
    }

    #[test]
    fn disk_offset_second_run() {
        // Two runs: VCN 0-3 → lcn 10, VCN 4-7 → lcn 20
        // byte 4*512 → VCN 4, in second run, lcn=20, disk = 20*512
        let b = bm(512);
        let runs = vec![run(0, 4, 10), run(4, 4, 20)];
        let (_, disk) = disk_offset_for_byte(&b, &runs, 4 * 512).unwrap();
        assert_eq!(disk, 20 * 512);
    }

    #[test]
    fn disk_offset_unmapped_byte_errors() {
        let b = bm(512);
        let runs = vec![run(0, 4, 10)]; // only covers VCNs 0-3
        assert!(disk_offset_for_byte(&b, &runs, 4 * 512).is_err());
    }

    #[test]
    fn disk_offset_sparse_run_errors() {
        let b = bm(512);
        let runs = vec![sparse_run(0, 4)];
        assert!(disk_offset_for_byte(&b, &runs, 0).is_err());
    }

    // -------------------------------------------------------------------------
    // In-memory BlockIo for public-API tests.
    // -------------------------------------------------------------------------

    use crate::block_io::BlockIo;
    use crate::data_runs::DataRun;

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
                return Err(format!("read past end: off={off} len={}", buf.len()));
            }
            buf.copy_from_slice(&self.buf[off..off + buf.len()]);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            let off = offset as usize;
            if off + buf.len() > self.buf.len() {
                return Err(format!("write past end: off={off} len={}", buf.len()));
            }
            self.buf[off..off + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn size(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    /// A formatted volume whose `$MFT:$Bitmap` has been rewritten as a
    /// **resident** attribute, with `bitmap_bytes` as its value (one bit
    /// per MFT record, LSB first).
    ///
    /// `mkfs` only ever emits the non-resident form, so the resident
    /// layout is unreachable from anything this crate produces and has to
    /// be built by hand to be exercised at all. Everything below goes
    /// through the real `locate_io`, so the layout under test is the one
    /// a foreign volume would hand us.
    fn resident_bitmap_volume(bitmap_bytes: &[u8]) -> MemDev {
        const SIZE: u64 = 64 * 1024 * 1024;
        let mut dev = MemDev::new(SIZE as usize);
        crate::mkfs::format_filesystem(&mut dev as &mut dyn BlockIo, SIZE, 4096, 4096, None, None)
            .expect("format_filesystem");

        update_mft_record_io(&mut dev, MFT_RECORD_NUMBER, |record| {
            let old = attr_io::find_attribute(record, AttrType::Bitmap, None)
                .ok_or_else(|| "$MFT has no unnamed $Bitmap".to_string())?;
            let resident = crate::record_build::build_resident_unnamed_attribute(
                AttrType::Bitmap as u32,
                old.attribute_id,
                bitmap_bytes,
            )?;
            crate::attr_resize::replace_attribute(record, old.attr_offset, &resident)
        })
        .expect("rewrite $MFT:$Bitmap as resident");

        dev
    }

    /// Records 0..24 in use (the system files `mkfs` writes), 24..64 free.
    const SYSTEM_RECORDS_IN_USE: [u8; 8] = [0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00];

    /// A volume whose `$MFT:$Bitmap` is resident with value `bytes`, plus
    /// the `MftBitmap` `locate_io` derives from it. Both halves are needed:
    /// the resident layout addresses `$MFT`'s record, so the reads and the
    /// writes below are real I/O against `dev`.
    fn resident_bm(bytes: &[u8]) -> (MemDev, MftBitmap) {
        let mut dev = resident_bitmap_volume(bytes);
        let bm_val = locate_io(&mut dev).expect("locate resident $MFT:$Bitmap");
        (dev, bm_val)
    }

    /// Non-resident MftBitmap whose bitmap data is at disk byte 0
    /// (LCN 0, cluster_size = 512 for easy byte arithmetic).
    fn nonresident_bm(n_bitmap_bytes: u64) -> (MemDev, MftBitmap) {
        let cluster_size = 512u64;
        let dev = MemDev::new((cluster_size * 4) as usize);
        let bm_val = MftBitmap {
            params: params(cluster_size),
            layout: MftBitmapLayout::NonResident {
                runs: vec![DataRun {
                    starting_vcn: 0,
                    length: 4,
                    lcn: Some(0),
                }],
                total_bits: n_bitmap_bytes * 8,
            },
        };
        (dev, bm_val)
    }

    // -------------------------------------------------------------------------
    // is_allocated_io — Resident layout.
    // -------------------------------------------------------------------------

    #[test]
    fn is_allocated_resident_set_bit_returns_true() {
        let (mut dev, bm_val) = resident_bm(&[0b0010_0000]); // bit 5 set = record 5 allocated
        assert!(is_allocated_io(&mut dev, &bm_val, 5).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 4).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 6).unwrap());
    }

    #[test]
    fn is_allocated_resident_all_bits_set() {
        let (mut dev, bm_val) = resident_bm(&[0xFF]);
        for n in 0..8u64 {
            assert!(is_allocated_io(&mut dev, &bm_val, n).unwrap(), "record {n}");
        }
    }

    #[test]
    fn is_allocated_resident_bit_in_second_byte() {
        // byte[1] bit 2 = record 10
        let (mut dev, bm_val) = resident_bm(&[0x00, 0b0000_0100]);
        assert!(!is_allocated_io(&mut dev, &bm_val, 8).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 9).unwrap());
        assert!(is_allocated_io(&mut dev, &bm_val, 10).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 11).unwrap());
    }

    // -------------------------------------------------------------------------
    // count_free_io — Resident layout.
    // -------------------------------------------------------------------------

    #[test]
    fn count_free_resident_all_free() {
        let (mut dev, bm_val) = resident_bm(&[0x00, 0x00]); // 16 bits, all free
        assert_eq!(count_free_io(&mut dev, &bm_val).unwrap(), 16);
    }

    #[test]
    fn count_free_resident_all_allocated() {
        let (mut dev, bm_val) = resident_bm(&[0xFF, 0xFF]);
        assert_eq!(count_free_io(&mut dev, &bm_val).unwrap(), 0);
    }

    #[test]
    fn count_free_resident_known_mixed_pattern() {
        // 0b1100_1100 → 4 set; 0b1010_1010 → 4 set; 8 free of 16.
        let (mut dev, bm_val) = resident_bm(&[0b1100_1100, 0b1010_1010]);
        assert_eq!(count_free_io(&mut dev, &bm_val).unwrap(), 8);
    }

    // -------------------------------------------------------------------------
    // find_free_record_io — Resident layout.
    // -------------------------------------------------------------------------

    #[test]
    fn find_free_resident_all_free_returns_hint() {
        let (mut dev, bm_val) = resident_bm(&[0x00, 0x00]);
        assert_eq!(find_free_record_io(&mut dev, &bm_val, 0).unwrap(), Some(0));
        assert_eq!(find_free_record_io(&mut dev, &bm_val, 5).unwrap(), Some(5));
    }

    #[test]
    fn find_free_resident_skips_allocated_bits() {
        // bits 0..4 set (0b0000_1111), bit 4 free
        let (mut dev, bm_val) = resident_bm(&[0b0000_1111]);
        assert_eq!(find_free_record_io(&mut dev, &bm_val, 0).unwrap(), Some(4));
    }

    #[test]
    fn find_free_resident_wraps_around() {
        // bits 4..8 set, bits 0..4 free; hint=4 → wraps → returns 0
        let (mut dev, bm_val) = resident_bm(&[0b1111_0000]);
        assert_eq!(find_free_record_io(&mut dev, &bm_val, 4).unwrap(), Some(0));
    }

    #[test]
    fn find_free_resident_all_allocated_returns_none() {
        let (mut dev, bm_val) = resident_bm(&[0xFF, 0xFF]);
        assert_eq!(find_free_record_io(&mut dev, &bm_val, 0).unwrap(), None);
    }

    #[test]
    fn find_free_resident_single_free_bit() {
        // Only bit 3 is free
        let (mut dev, bm_val) = resident_bm(&[0b1111_0111]);
        assert_eq!(find_free_record_io(&mut dev, &bm_val, 0).unwrap(), Some(3));
    }

    // -------------------------------------------------------------------------
    // allocate_io / free_io — NonResident layout (real byte I/O through MemDev).
    // -------------------------------------------------------------------------

    #[test]
    fn allocate_nonresident_sets_bit_readable_via_is_allocated() {
        let (mut dev, bm_val) = nonresident_bm(4);
        allocate_io(&mut dev, &bm_val, 5).unwrap();
        assert!(is_allocated_io(&mut dev, &bm_val, 5).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 4).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 6).unwrap());
    }

    #[test]
    fn free_nonresident_clears_previously_allocated_bit() {
        let (mut dev, bm_val) = nonresident_bm(4);
        allocate_io(&mut dev, &bm_val, 7).unwrap();
        free_io(&mut dev, &bm_val, 7).unwrap();
        assert!(!is_allocated_io(&mut dev, &bm_val, 7).unwrap());
    }

    #[test]
    fn allocate_nonresident_rejects_already_allocated() {
        let (mut dev, bm_val) = nonresident_bm(4);
        allocate_io(&mut dev, &bm_val, 3).unwrap();
        let err = allocate_io(&mut dev, &bm_val, 3).unwrap_err();
        assert!(err.contains("already allocated"), "{err}");
    }

    #[test]
    fn free_nonresident_rejects_already_free() {
        let (mut dev, bm_val) = nonresident_bm(4);
        let err = free_io(&mut dev, &bm_val, 3).unwrap_err();
        assert!(err.contains("already free"), "{err}");
    }

    #[test]
    fn count_free_nonresident_decreases_after_allocations() {
        let (mut dev, bm_val) = nonresident_bm(4); // 32 bits total
        allocate_io(&mut dev, &bm_val, 0).unwrap();
        allocate_io(&mut dev, &bm_val, 1).unwrap();
        allocate_io(&mut dev, &bm_val, 5).unwrap();
        assert_eq!(count_free_io(&mut dev, &bm_val).unwrap(), 29);
    }

    #[test]
    fn allocate_free_roundtrip_across_byte_boundary() {
        let (mut dev, bm_val) = nonresident_bm(4);
        // Allocate records spanning byte 0 (bits 0-7) and byte 1 (bits 8-15).
        allocate_io(&mut dev, &bm_val, 7).unwrap(); // byte 0, bit 7
        allocate_io(&mut dev, &bm_val, 8).unwrap(); // byte 1, bit 0
        assert!(is_allocated_io(&mut dev, &bm_val, 7).unwrap());
        assert!(is_allocated_io(&mut dev, &bm_val, 8).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 6).unwrap());
        assert!(!is_allocated_io(&mut dev, &bm_val, 9).unwrap());
        free_io(&mut dev, &bm_val, 7).unwrap();
        assert!(!is_allocated_io(&mut dev, &bm_val, 7).unwrap());
        assert!(is_allocated_io(&mut dev, &bm_val, 8).unwrap());
    }

    // -------------------------------------------------------------------------
    // allocate_io / free_io — Resident layout, on a real formatted volume.
    // -------------------------------------------------------------------------

    /// The allocator contract every rollback site in `write.rs` depends on:
    /// after `allocate_io` succeeds, the paired `free_io` must succeed.
    #[test]
    fn resident_free_after_allocate_succeeds() {
        let (mut dev, bm_val) = resident_bm(&SYSTEM_RECORDS_IN_USE);
        assert!(
            matches!(bm_val.layout, MftBitmapLayout::Resident { .. }),
            "fixture must produce the Resident layout"
        );

        let rec = find_free_record_io(&mut dev, &bm_val, 24)
            .unwrap()
            .expect("volume must have a free MFT record");
        allocate_io(&mut dev, &bm_val, rec).unwrap();
        free_io(&mut dev, &bm_val, rec)
            .expect("free after allocate must succeed — every rollback site depends on it");
        assert!(
            !is_allocated_io(&mut dev, &bm_val, rec).unwrap(),
            "the freed bit must be clear on disk"
        );
    }

    /// The asymmetry itself: a resident write must be visible to the next
    /// resident read.
    #[test]
    fn resident_allocate_is_visible_to_the_next_read() {
        let (mut dev, bm_val) = resident_bm(&SYSTEM_RECORDS_IN_USE);

        allocate_io(&mut dev, &bm_val, 40).unwrap();
        assert!(
            is_allocated_io(&mut dev, &bm_val, 40).unwrap(),
            "allocate_io wrote the bit; is_allocated_io must see it"
        );
        assert_eq!(
            find_free_record_io(&mut dev, &bm_val, 40).unwrap(),
            Some(41),
            "the allocated record must no longer be handed out"
        );
    }

    /// `count_free_io` reads the same bitmap and must move with it.
    #[test]
    fn resident_count_free_reflects_allocation() {
        let (mut dev, bm_val) = resident_bm(&SYSTEM_RECORDS_IN_USE);

        let before = count_free_io(&mut dev, &bm_val).unwrap();
        assert_eq!(before, 64 - 24);
        allocate_io(&mut dev, &bm_val, 30).unwrap();
        assert_eq!(count_free_io(&mut dev, &bm_val).unwrap(), before - 1);
        free_io(&mut dev, &bm_val, 30).unwrap();
        assert_eq!(count_free_io(&mut dev, &bm_val).unwrap(), before);
    }

    /// A resident bitmap read must still refuse a byte past its value.
    #[test]
    fn resident_read_past_bitmap_length_errors() {
        let (mut dev, bm_val) = resident_bm(&SYSTEM_RECORDS_IN_USE);
        let err = read_bitmap_byte_io(&mut dev, &bm_val, 8).unwrap_err();
        assert!(err.contains("past resident bitmap length"), "{err}");
    }
}
