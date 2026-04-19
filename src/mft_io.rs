//! Low-level MFT-record read-modify-write with Update Sequence Array (USA)
//! fixup. Primary primitive for any write that touches an MFT record.
//!
//! References (no GPL code consulted):
//! * [Flatcap Fixup](https://flatcap.github.io/linux-ntfs/ntfs/concepts/fixup.html)
//! * [Flatcap Boot Sector](https://flatcap.github.io/linux-ntfs/ntfs/files/boot.html)
//! * [Flatcap File Record](https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html)
//! * MS-FSCC
//!
//! **Why fixup matters.** NTFS stores multi-sector records (MFT records,
//! INDEX_ALLOCATION blocks) with the last 2 bytes of every 512-byte sector
//! replaced by an Update Sequence Number (USN). The original bytes live in
//! the Update Sequence Array (USA) in the record header. A torn write is
//! detected because the new USN hasn't propagated to every sector. Any
//! write to an MFT record must re-apply this encoding correctly or the
//! volume becomes unmountable.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

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
    let mut f = File::open(path).map_err(|e| format!("open boot: {e}"))?;
    let mut boot = [0u8; 512];
    f.read_exact(&mut boot)
        .map_err(|e| format!("read boot: {e}"))?;

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

/// In-memory MFT record header offsets (Flatcap).
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
    let params = read_boot_params(path)?;
    let offset = mft_record_offset(&params, record_number);
    let size = params.file_record_size as usize;
    let mut f = File::open(path).map_err(|e| format!("open ro: {e}"))?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek record {record_number}: {e}"))?;
    let mut buf = vec![0u8; size];
    f.read_exact(&mut buf)
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
    let (params, mut record) = read_mft_record(path, record_number)?;
    if record_flags(&record) & MFT_FLAG_IN_USE == 0 {
        return Err(format!(
            "refusing to write to MFT record {record_number}: IN_USE flag is clear"
        ));
    }

    mutate(&mut record)?;
    apply_fixup_on_write(&mut record, params.bytes_per_sector)?;

    let offset = mft_record_offset(&params, record_number);
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open rw: {e}"))?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek write record {record_number}: {e}"))?;
    f.write_all(&record)
        .map_err(|e| format!("write record {record_number}: {e}"))?;
    f.sync_all().map_err(|e| format!("fsync: {e}"))?;
    Ok(())
}
