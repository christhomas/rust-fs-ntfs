//! Low-level MFT-record read-modify-write with Update Sequence Array (USA)
//! fixup. Primary primitive for any write that touches an MFT record.
//!
//! # Concurrency contract
//!
//! [`update_mft_record`] is **not safe under concurrent writers** to
//! the same image. The function reads the record, applies USA fixup,
//! calls the mutator, re-applies fixup, and writes back — any external
//! write that lands between the read and the write tears the update
//! and silently corrupts the volume.
//!
//! Callers MUST arrange that nobody else writes to the image during
//! the call:
//!   - **Single-process**: fs-ntfs doesn't spawn threads internally,
//!     so a single mount-and-mutate call from one thread is safe.
//!   - **Multi-process / external writers** (Windows mounting the
//!     same volume, a second fs-ntfs caller, an upstream NTFS driver):
//!     UB. The image must be quiesced (unmounted everywhere else)
//!     before any mutation here.
//!
//! Advisory file locking is **deliberately not** added — it can't
//! prevent external concurrency anyway, so it would only catch
//! in-process races we don't produce.
//!
//! References (no GPL code consulted): Multi-sector update sequence
//! ("fixup"), boot sector / BPB layout, and FILE_RECORD_SEGMENT_HEADER
//! per Windows Internals 7th ed. ch. "NTFS On-Disk Structure" and
//! MS-FSCC.
//!
//! **Why fixup matters.** NTFS stores multi-sector records (MFT records,
//! INDEX_ALLOCATION blocks) with the last 2 bytes of every 512-byte sector
//! replaced by an Update Sequence Number (USN). The original bytes live in
//! the Update Sequence Array (USA) in the record header. A torn write is
//! detected because the new USN hasn't propagated to every sector. Any
//! write to an MFT record must re-apply this encoding correctly or the
//! volume becomes unmountable.

use std::path::Path;

use crate::block_io::{BlockIo, PathIo};

/// NTFS boot-sector fields we need for MFT addressing.
#[derive(Debug, Clone, Copy)]
pub struct BootParams {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u64,
    pub cluster_size: u64,
    pub mft_lcn: u64,
    pub file_record_size: u64,
}

/// Parse the 512-byte boot sector at offset 0 for the subset of fields we
/// need. Does not validate the NTFS magic ("NTFS    " at +3) or checksum
/// — upstream `Ntfs::new` already does that during read-side parsing.
pub fn read_boot_params(path: &Path) -> Result<BootParams, String> {
    let mut io = PathIo::open_ro(path)?;
    read_boot_params_io(&mut io)
}

/// Parse the boot sector via an arbitrary `BlockIo`. Used directly by
/// the handle-based mutator stack.
pub fn read_boot_params_io<T: BlockIo + ?Sized>(io: &mut T) -> Result<BootParams, String> {
    let mut boot = [0u8; 512];
    io.read_exact_at(0, &mut boot)?;
    parse_boot_params_from_bytes(&boot)
}

fn parse_boot_params_from_bytes(boot: &[u8; 512]) -> Result<BootParams, String> {
    let bytes_per_sector = u16::from_le_bytes([boot[0x0B], boot[0x0C]]);
    if bytes_per_sector == 0 || bytes_per_sector & (bytes_per_sector - 1) != 0 {
        return Err(format!(
            "bytes_per_sector {bytes_per_sector} not a power of two"
        ));
    }

    // sectors_per_cluster encoding: high bit set ⇒ 2^(256-val) (rare, for very
    // large clusters). Positive: literal value.
    let spc_raw = boot[0x0D];
    let sectors_per_cluster: u64 = if spc_raw < 0x80 {
        spc_raw as u64
    } else {
        1u64 << (256u32.saturating_sub(spc_raw as u32))
    };
    if sectors_per_cluster == 0 {
        return Err(format!(
            "sectors_per_cluster decoded to 0 (raw {spc_raw:#x})"
        ));
    }
    let cluster_size = bytes_per_sector as u64 * sectors_per_cluster;

    let mft_lcn = u64::from_le_bytes(boot[0x30..0x38].try_into().unwrap());

    // clusters_per_mft_record: positive ⇒ that many clusters; negative ⇒
    // 2^|val| bytes (common: -10 ⇒ 1024 byte records).
    let cpmr = boot[0x40] as i8;
    let file_record_size = if cpmr > 0 {
        (cpmr as u64) * cluster_size
    } else {
        1u64 << ((-(cpmr as i16)) as u32)
    };
    if !(512..=16384).contains(&file_record_size) {
        return Err(format!(
            "file_record_size {file_record_size} out of plausible range"
        ));
    }

    Ok(BootParams {
        bytes_per_sector,
        sectors_per_cluster,
        cluster_size,
        mft_lcn,
        file_record_size,
    })
}

/// Byte offset of MFT record `record_number` on disk.
pub fn mft_record_offset(params: &BootParams, record_number: u64) -> u64 {
    params.mft_lcn * params.cluster_size + record_number * params.file_record_size
}

/// In-memory MFT record header offsets (per Windows Internals 7th ed.).
const FILE_MAGIC: &[u8; 4] = b"FILE";
const OFF_USA_OFFSET: usize = 0x04;
const OFF_USA_COUNT: usize = 0x06;
const OFF_FLAGS: usize = 0x16;

/// Record flag: record is in use (allocated). Clear ⇒ record is free.
pub const MFT_FLAG_IN_USE: u16 = 0x0001;
/// Record flag: record represents a directory.
pub const MFT_FLAG_DIRECTORY: u16 = 0x0002;

/// Returns the record's `flags` field (u16 LE at +0x16). Expects a
/// post-fixup buffer.
pub fn record_flags(record: &[u8]) -> u16 {
    u16::from_le_bytes([record[OFF_FLAGS], record[OFF_FLAGS + 1]])
}

/// Apply the on-disk → in-memory fixup. Validates the FILE magic and
/// verifies every sector-end pair matches the USN; returns Err on mismatch
/// (indicates a torn write or corrupted record).
pub fn apply_fixup_on_read(record: &mut [u8], bytes_per_sector: u16) -> Result<(), String> {
    apply_fixup_on_read_magic(record, bytes_per_sector, FILE_MAGIC)
}

/// Variant of [`apply_fixup_on_read`] parameterized on the expected
/// 4-byte magic. Use this for INDX blocks (magic `b"INDX"`) and any
/// other multi-sector record with USA fixup.
pub fn apply_fixup_on_read_magic(
    record: &mut [u8],
    bytes_per_sector: u16,
    expected_magic: &[u8; 4],
) -> Result<(), String> {
    if &record[0..4] != expected_magic {
        return Err(format!(
            "magic mismatch: expected {:?}, got {:02x?}",
            std::str::from_utf8(expected_magic).unwrap_or("?"),
            &record[0..4]
        ));
    }
    let (usa_offset, usa_count) = read_usa_header(record)?;
    validate_usa_geometry(record.len(), bytes_per_sector, usa_offset, usa_count)?;

    let usn_bytes = [record[usa_offset], record[usa_offset + 1]];
    let sectors = usa_count - 1;
    for sector in 0..sectors {
        let sector_end = (sector + 1) * bytes_per_sector as usize;
        let check = sector_end - 2;
        if record[check..check + 2] != usn_bytes {
            return Err(format!(
                "USN mismatch at sector {sector} (offset {check:#x}): \
                 expected {usn_bytes:02x?}, found {:02x?}",
                &record[check..check + 2]
            ));
        }
        let saved = usa_offset + 2 + sector * 2;
        record[check] = record[saved];
        record[check + 1] = record[saved + 1];
    }
    Ok(())
}

/// Apply the in-memory → on-disk fixup. Bumps the USN by one, saves the
/// current sector-end bytes into the USA, and overwrites the sector-ends
/// with the new USN. Call after mutating the record and immediately
/// before writing back.
pub fn apply_fixup_on_write(record: &mut [u8], bytes_per_sector: u16) -> Result<(), String> {
    apply_fixup_on_write_magic(record, bytes_per_sector, FILE_MAGIC)
}

/// Variant of [`apply_fixup_on_write`] parameterized on the expected
/// 4-byte magic (use `b"INDX"` for `$INDEX_ALLOCATION` blocks).
pub fn apply_fixup_on_write_magic(
    record: &mut [u8],
    bytes_per_sector: u16,
    expected_magic: &[u8; 4],
) -> Result<(), String> {
    if &record[0..4] != expected_magic {
        return Err(format!(
            "magic mismatch: expected {:?}, got {:02x?}",
            std::str::from_utf8(expected_magic).unwrap_or("?"),
            &record[0..4]
        ));
    }
    let (usa_offset, usa_count) = read_usa_header(record)?;
    validate_usa_geometry(record.len(), bytes_per_sector, usa_offset, usa_count)?;

    // Bump USN, skipping 0 (some NTFS drivers treat 0 as "uninitialized").
    let old_usn = u16::from_le_bytes([record[usa_offset], record[usa_offset + 1]]);
    let new_usn = match old_usn.wrapping_add(1) {
        0 => 1,
        n => n,
    };
    record[usa_offset..usa_offset + 2].copy_from_slice(&new_usn.to_le_bytes());

    let sectors = usa_count - 1;
    for sector in 0..sectors {
        let sector_end = (sector + 1) * bytes_per_sector as usize;
        let replace = sector_end - 2;
        let saved = usa_offset + 2 + sector * 2;
        record[saved] = record[replace];
        record[saved + 1] = record[replace + 1];
        record[replace..replace + 2].copy_from_slice(&new_usn.to_le_bytes());
    }
    Ok(())
}

fn read_usa_header(record: &[u8]) -> Result<(usize, usize), String> {
    if record.len() < 8 {
        return Err("record too small to contain USA header".to_string());
    }
    let usa_offset =
        u16::from_le_bytes([record[OFF_USA_OFFSET], record[OFF_USA_OFFSET + 1]]) as usize;
    let usa_count = u16::from_le_bytes([record[OFF_USA_COUNT], record[OFF_USA_COUNT + 1]]) as usize;
    if usa_count == 0 {
        return Err("USA count is zero (record has no fixup array)".to_string());
    }
    Ok((usa_offset, usa_count))
}

fn validate_usa_geometry(
    record_len: usize,
    bytes_per_sector: u16,
    usa_offset: usize,
    usa_count: usize,
) -> Result<(), String> {
    let bps = bytes_per_sector as usize;
    let sectors_expected = record_len / bps;
    // USA needs usa_count slots of 2 bytes each starting at usa_offset.
    let usa_end = usa_offset
        .checked_add(usa_count.checked_mul(2).ok_or("usa_count overflow")?)
        .ok_or("usa bounds overflow")?;
    if usa_end > record_len {
        return Err(format!(
            "USA [{:#x}..{:#x}] extends past record end {:#x}",
            usa_offset, usa_end, record_len
        ));
    }
    if usa_count - 1 != sectors_expected {
        return Err(format!(
            "USA count {usa_count} inconsistent with record size {record_len} \
             / bytes_per_sector {bytes_per_sector} (expected {} slots)",
            sectors_expected + 1
        ));
    }
    Ok(())
}

/// Read an MFT record, apply fixup, and return the clean bytes.
pub fn read_mft_record(path: &Path, record_number: u64) -> Result<(BootParams, Vec<u8>), String> {
    let mut io = PathIo::open_ro(path)?;
    read_mft_record_io(&mut io, record_number)
}

/// Same as [`read_mft_record`] but takes any `BlockIo`. The mutator stack
/// uses this directly so it can share a single open file (or callback
/// pair) across multiple record reads.
pub fn read_mft_record_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
) -> Result<(BootParams, Vec<u8>), String> {
    let params = read_boot_params_io(io)?;
    let offset = mft_record_offset(&params, record_number);
    let size = params.file_record_size as usize;
    let mut buf = vec![0u8; size];
    io.read_exact_at(offset, &mut buf)
        .map_err(|e| format!("read record {record_number}: {e}"))?;
    apply_fixup_on_read(&mut buf, params.bytes_per_sector)?;
    Ok((params, buf))
}

/// Read-modify-write an MFT record. The `mutate` closure receives the
/// post-fixup clean bytes and mutates them in place. The fixup is
/// re-applied (USN bumped) before writing back, and the whole record is
/// `fsync`'d.
///
/// Refuses to operate on a record whose `in use` flag is clear — writing
/// to a free record is almost certainly a bug and could corrupt
/// subsequent allocations.
pub fn update_mft_record<F>(path: &Path, record_number: u64, mutate: F) -> Result<(), String>
where
    F: FnOnce(&mut [u8]) -> Result<(), String>,
{
    let mut io = PathIo::open_rw(path)?;
    update_mft_record_io(&mut io, record_number, mutate)
}

/// `BlockIo`-based equivalent of [`update_mft_record`]. Shares one
/// underlying open file / callback pair across the read and the write.
pub fn update_mft_record_io<T, F>(io: &mut T, record_number: u64, mutate: F) -> Result<(), String>
where
    T: BlockIo + ?Sized,
    F: FnOnce(&mut [u8]) -> Result<(), String>,
{
    let (params, mut record) = read_mft_record_io(io, record_number)?;
    if record_flags(&record) & MFT_FLAG_IN_USE == 0 {
        return Err(format!(
            "refusing to write to MFT record {record_number}: IN_USE flag is clear"
        ));
    }

    mutate(&mut record)?;
    apply_fixup_on_write(&mut record, params.bytes_per_sector)?;

    let offset = mft_record_offset(&params, record_number);
    io.write_all_at(offset, &record)
        .map_err(|e| format!("write record {record_number}: {e}"))?;
    io.sync()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_io::BlockIo;

    /// In-memory `BlockIo` for tests. Tracks size so size()/read past-end
    /// behave like a real file.
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

    /// Synthesize a 512-byte NTFS boot sector with the four fields we
    /// parse, plus the "NTFS    " magic so we don't trip the magic check
    /// upstream (we don't check it here, but real boot sectors have it).
    fn synth_boot(
        bytes_per_sector: u16,
        sectors_per_cluster: u8,
        mft_lcn: u64,
        clusters_per_mft_record: i8,
    ) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
        b[0x0D] = sectors_per_cluster;
        b[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
        b[0x40] = clusters_per_mft_record as u8;
        b
    }

    // --- boot params parsing -----------------------------------------------

    #[test]
    fn parse_boot_typical_512_8_4_neg10() {
        // 512 bps, 8 spc → 4 KiB cluster, MFT at LCN 4, cpmr=-10 → 1024 records.
        let boot = synth_boot(512, 8, 4, -10);
        let bp = parse_boot_params_from_bytes(&boot).unwrap();
        assert_eq!(bp.bytes_per_sector, 512);
        assert_eq!(bp.sectors_per_cluster, 8);
        assert_eq!(bp.cluster_size, 4096);
        assert_eq!(bp.mft_lcn, 4);
        assert_eq!(bp.file_record_size, 1024);
    }

    #[test]
    fn parse_boot_positive_cpmr_means_cluster_count() {
        // 4096 bps, 1 spc → 4 KiB cluster, cpmr=+1 → record = 1*cluster = 4096.
        let boot = synth_boot(4096, 1, 0, 1);
        let bp = parse_boot_params_from_bytes(&boot).unwrap();
        assert_eq!(bp.file_record_size, 4096);
    }

    #[test]
    fn parse_boot_negative_cpmr_means_power_of_two_bytes() {
        // cpmr=-12 → record = 2^12 = 4096 bytes.
        let boot = synth_boot(512, 8, 4, -12);
        let bp = parse_boot_params_from_bytes(&boot).unwrap();
        assert_eq!(bp.file_record_size, 4096);
    }

    #[test]
    fn parse_boot_rejects_non_power_of_two_bytes_per_sector() {
        let boot = synth_boot(513, 1, 0, -10);
        let err = parse_boot_params_from_bytes(&boot).unwrap_err();
        assert!(err.contains("not a power of two"), "{err}");
    }

    #[test]
    fn parse_boot_rejects_record_size_out_of_plausible_range() {
        // cpmr=-20 → 2^20 = 1 MiB record, refused as implausible.
        let boot = synth_boot(512, 1, 0, -20);
        let err = parse_boot_params_from_bytes(&boot).unwrap_err();
        assert!(err.contains("out of plausible range"), "{err}");
    }

    #[test]
    fn read_boot_params_via_block_io() {
        let mut dev = MemDev::new(4096);
        let boot = synth_boot(512, 8, 4, -10);
        dev.write_all_at(0, &boot).unwrap();
        let bp = read_boot_params_io(&mut dev).unwrap();
        assert_eq!(bp.cluster_size, 4096);
        assert_eq!(bp.file_record_size, 1024);
    }

    // --- mft_record_offset (pure arithmetic) -------------------------------

    #[test]
    fn mft_record_offset_record_0_is_mft_lcn_times_cluster_size() {
        let p = BootParams {
            bytes_per_sector: 512,
            sectors_per_cluster: 8,
            cluster_size: 4096,
            mft_lcn: 4,
            file_record_size: 1024,
        };
        assert_eq!(mft_record_offset(&p, 0), 4 * 4096);
        assert_eq!(mft_record_offset(&p, 7), 4 * 4096 + 7 * 1024);
    }

    // --- record_flags ------------------------------------------------------

    #[test]
    fn record_flags_reads_u16_le_at_offset_0x16() {
        let mut rec = vec![0u8; 64];
        rec[0x16] = 0x03; // IN_USE + DIRECTORY
        rec[0x17] = 0x00;
        let flags = record_flags(&rec);
        assert!(flags & MFT_FLAG_IN_USE != 0);
        assert!(flags & MFT_FLAG_DIRECTORY != 0);
    }

    // --- USA fixup ---------------------------------------------------------

    /// Synthesize a minimal FILE-magic'd 1024-byte record with 2 sectors
    /// of 512 bytes each. Sets up USA header at 0x2A with count=3 (one
    /// USN slot + 2 sector slots). Saved USA bytes are 0x00, sector-end
    /// bytes are set to the USN (0x0001).
    fn synth_fixup_record(usn: u16) -> Vec<u8> {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        // USA offset = 0x2A, count = 3 (USN + 2 sectors).
        rec[OFF_USA_OFFSET..OFF_USA_OFFSET + 2].copy_from_slice(&0x002Au16.to_le_bytes());
        rec[OFF_USA_COUNT..OFF_USA_COUNT + 2].copy_from_slice(&0x0003u16.to_le_bytes());
        // USN at 0x2A.
        rec[0x2A..0x2C].copy_from_slice(&usn.to_le_bytes());
        // saved sector-end bytes at 0x2C, 0x2E. Use 0xAA 0xAA / 0xBB 0xBB
        // so the round-trip is non-trivially detectable.
        rec[0x2C] = 0xAA;
        rec[0x2D] = 0xAA;
        rec[0x2E] = 0xBB;
        rec[0x2F] = 0xBB;
        // Sector-end pairs replaced with USN to match a freshly-written
        // record (on-disk form).
        rec[0x1FE..0x200].copy_from_slice(&usn.to_le_bytes());
        rec[0x3FE..0x400].copy_from_slice(&usn.to_le_bytes());
        rec
    }

    #[test]
    fn fixup_on_read_restores_saved_sector_end_bytes() {
        let mut rec = synth_fixup_record(0x0001);
        apply_fixup_on_read(&mut rec, 512).unwrap();
        // Sector ends restored from USA.
        assert_eq!(&rec[0x1FE..0x200], &[0xAA, 0xAA]);
        assert_eq!(&rec[0x3FE..0x400], &[0xBB, 0xBB]);
    }

    #[test]
    fn fixup_write_then_read_round_trips_record_bytes() {
        // Start from a clean (post-read) record: sector-ends hold the
        // "real" data bytes.
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[OFF_USA_OFFSET..OFF_USA_OFFSET + 2].copy_from_slice(&0x002Au16.to_le_bytes());
        rec[OFF_USA_COUNT..OFF_USA_COUNT + 2].copy_from_slice(&0x0003u16.to_le_bytes());
        rec[0x2A..0x2C].copy_from_slice(&0x0000u16.to_le_bytes());
        // Put recognisable bytes at the sector ends.
        rec[0x1FE] = 0x12;
        rec[0x1FF] = 0x34;
        rec[0x3FE] = 0x56;
        rec[0x3FF] = 0x78;
        let snapshot = rec.clone();

        apply_fixup_on_write(&mut rec, 512).unwrap();
        // USN bumped from 0 → 1, sector ends overwritten with USN.
        assert_eq!(rec[0x1FE], 0x01);
        assert_eq!(rec[0x1FF], 0x00);

        apply_fixup_on_read(&mut rec, 512).unwrap();
        // Original sector-end bytes restored.
        assert_eq!(rec[0x1FE..0x200], snapshot[0x1FE..0x200]);
        assert_eq!(rec[0x3FE..0x400], snapshot[0x3FE..0x400]);
    }

    #[test]
    fn fixup_write_skips_usn_value_zero() {
        // Pre-set USN to 0xFFFF so bump wraps to 0; must skip to 1.
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[OFF_USA_OFFSET..OFF_USA_OFFSET + 2].copy_from_slice(&0x002Au16.to_le_bytes());
        rec[OFF_USA_COUNT..OFF_USA_COUNT + 2].copy_from_slice(&0x0003u16.to_le_bytes());
        rec[0x2A..0x2C].copy_from_slice(&0xFFFFu16.to_le_bytes());

        apply_fixup_on_write(&mut rec, 512).unwrap();
        let new_usn = u16::from_le_bytes([rec[0x2A], rec[0x2B]]);
        assert_eq!(new_usn, 1, "USN must skip 0 on wraparound");
    }

    #[test]
    fn fixup_on_read_rejects_usn_mismatch() {
        let mut rec = synth_fixup_record(0x0001);
        // Corrupt one sector end so it doesn't match USN.
        rec[0x1FF] = 0x99;
        let err = apply_fixup_on_read(&mut rec, 512).unwrap_err();
        assert!(err.contains("USN mismatch"), "{err}");
    }

    #[test]
    fn fixup_rejects_wrong_magic() {
        let mut rec = synth_fixup_record(0x0001);
        rec[0..4].copy_from_slice(b"XXXX");
        let err = apply_fixup_on_read(&mut rec, 512).unwrap_err();
        assert!(err.contains("magic mismatch"), "{err}");
    }

    #[test]
    fn fixup_works_on_indx_magic() {
        let mut rec = synth_fixup_record(0x0001);
        rec[0..4].copy_from_slice(b"INDX");
        apply_fixup_on_read_magic(&mut rec, 512, b"INDX").unwrap();
    }
}
