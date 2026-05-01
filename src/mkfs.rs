//! Minimum-viable NTFS formatter. Builds a v3.1-flagged volume that
//! mounts cleanly under this crate's existing read path.
//!
//! Layout written for a typical 4 KiB-cluster volume:
//!   LCN 0           — boot sector (512 bytes; rest of cluster zeroed)
//!   LCN 4..         — $MFT (32 clusters / 128 KiB initial)
//!   LCN 36..        — $LogFile (16 clusters / 64 KiB; filled 0xFF)
//!   LCN 52..        — $Bitmap (sized to volume)
//!   LCN 53..        — $UpCase (32 clusters / 128 KiB)
//!   LCN cluster_count/2 — $MFTMirr (single cluster)
//!   LCN cluster_count-1 — backup boot sector
//!
//! References (no GPL code consulted):
//! * [Flatcap Boot Sector](https://flatcap.github.io/linux-ntfs/ntfs/files/boot.html)
//! * [Flatcap $MFT layout](https://flatcap.github.io/linux-ntfs/ntfs/files/mft.html)
//! * MS-FSCC (system files + attributes)

use crate::block_io::BlockIo;
use crate::data_runs::{encode_runs, DataRun};
use crate::mft_io::apply_fixup_on_write;
use crate::record_build::{
    align8, build_nonresident_attribute, build_nonresident_data_attribute, nt_time_now,
};
use crate::upcase;

const FILE_MAGIC: &[u8; 4] = b"FILE";
const NTFS_OEM: &[u8; 8] = b"NTFS    ";

const REC_OFF_USA_OFFSET: usize = 0x04;
const REC_OFF_USA_COUNT: usize = 0x06;
const REC_OFF_LSN: usize = 0x08;
const REC_OFF_SEQ: usize = 0x10;
const REC_OFF_LINK_COUNT: usize = 0x12;
const REC_OFF_ATTRS_OFFSET: usize = 0x14;
const REC_OFF_FLAGS: usize = 0x16;
const REC_OFF_BYTES_USED: usize = 0x18;
const REC_OFF_BYTES_ALLOCATED: usize = 0x1C;
const REC_OFF_BASE_FILE_REF: usize = 0x20;
const REC_OFF_NEXT_ATTR_ID: usize = 0x28;
const REC_OFF_MFT_REC_NUM: usize = 0x2C;
const USA_OFFSET: usize = 0x30;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_VOLUME_NAME: u32 = 0x60;
const ATTR_VOLUME_INFORMATION: u32 = 0x70;
const ATTR_DATA: u32 = 0x80;
const ATTR_INDEX_ROOT: u32 = 0x90;
const ATTR_END_MARKER: u32 = 0xFFFF_FFFF;

/// Win32 + DOS namespace value for $FILE_NAME.
const NAMESPACE_WIN32_DOS: u8 = 3;

/// MFT record numbers we populate (must match NTFS reservations).
mod rec {
    pub const MFT: u32 = 0;
    pub const MFTMIRR: u32 = 1;
    pub const LOGFILE: u32 = 2;
    pub const VOLUME: u32 = 3;
    pub const ATTRDEF: u32 = 4;
    pub const ROOT: u32 = 5;
    pub const BITMAP: u32 = 6;
    pub const BOOT: u32 = 7;
    pub const BADCLUS: u32 = 8;
    pub const SECURE: u32 = 9;
    pub const UPCASE: u32 = 10;
    pub const EXTEND: u32 = 11;
}

/// Format an NTFS volume in place over a [`BlockIo`].
pub fn format_filesystem(
    dev: &mut dyn BlockIo,
    size_bytes: u64,
    cluster_size: u32,
    mft_record_size: u32,
    label: Option<&str>,
    serial: Option<u64>,
) -> Result<(), String> {
    if !cluster_size.is_power_of_two() || !(512..=65536).contains(&cluster_size) {
        return Err(format!("invalid cluster_size {cluster_size}"));
    }
    if !mft_record_size.is_power_of_two() || !(512..=16384).contains(&mft_record_size) {
        return Err(format!("invalid mft_record_size {mft_record_size}"));
    }
    let bytes_per_sector: u16 = 512;
    if (cluster_size as u64) < bytes_per_sector as u64 {
        return Err("cluster_size < bytes_per_sector".to_string());
    }
    let sectors_per_cluster = cluster_size / bytes_per_sector as u32;
    let cluster_count = size_bytes / cluster_size as u64;
    if cluster_count < 1024 {
        return Err(format!("volume too small: {cluster_count} clusters"));
    }
    if dev.size() < size_bytes {
        return Err(format!(
            "device size {} < requested format size {size_bytes}",
            dev.size()
        ));
    }

    // Layout planning ------------------------------------------------------
    let mft_lcn: u64 = 4;
    let mft_clusters: u64 = (mft_record_size as u64 * 64)
        .div_ceil(cluster_size as u64)
        .max(1);
    let mft_records_capacity: u64 = mft_clusters * cluster_size as u64 / mft_record_size as u64;
    if mft_records_capacity < 24 {
        return Err("MFT initial allocation too small".to_string());
    }

    let logfile_lcn = mft_lcn + mft_clusters;
    let logfile_clusters: u64 = (64 * 1024u64).div_ceil(cluster_size as u64);

    let bitmap_lcn = logfile_lcn + logfile_clusters;
    let bitmap_bytes: u64 = cluster_count.div_ceil(8);
    let bitmap_clusters: u64 = bitmap_bytes.div_ceil(cluster_size as u64);

    let upcase_lcn = bitmap_lcn + bitmap_clusters;
    let upcase_bytes: u64 = 128 * 1024;
    let upcase_clusters: u64 = upcase_bytes.div_ceil(cluster_size as u64);

    let mftmirr_lcn = cluster_count / 2;
    // Mirror records 0..3 (4 records). Round up in case record_size > cluster_size.
    let mftmirr_clusters: u64 = (4 * mft_record_size as u64).div_ceil(cluster_size as u64);

    let backup_boot_lcn = cluster_count - 1;

    let last_used_lcn = upcase_lcn + upcase_clusters;
    if last_used_lcn >= mftmirr_lcn || mftmirr_lcn + mftmirr_clusters >= backup_boot_lcn {
        return Err("volume too small for chosen layout".to_string());
    }

    let serial = serial.unwrap_or_else(generate_serial);
    let now = nt_time_now();

    // 0. Zero out critical regions ---------------------------------------
    // We only zero what we'll touch (not the entire device — too slow for a
    // 1 GiB+ volume in tests). The MFT region, bitmap region, MFTMirr,
    // upcase region, and backup boot all get explicit writes below.

    // 1. Boot sector + backup --------------------------------------------
    let boot = build_boot_sector(
        bytes_per_sector,
        sectors_per_cluster as u8,
        cluster_count,
        mft_lcn,
        mftmirr_lcn,
        cluster_size,
        mft_record_size,
        serial,
    )?;
    dev.write_all_at(0, &boot)?;
    dev.write_all_at(backup_boot_lcn * cluster_size as u64, &boot)?;

    // 2. $LogFile — fill with 0xFF (RSTR-less; chkdsk reinits on mount).
    let log_size_bytes = logfile_clusters * cluster_size as u64;
    write_filled(dev, logfile_lcn * cluster_size as u64, log_size_bytes, 0xFF)?;

    // 3. $UpCase data -----------------------------------------------------
    let upcase_data = upcase::generate_upcase_table();
    let upcase_value_bytes = upcase_data.len() as u64; // 128 KiB exact
    dev.write_all_at(upcase_lcn * cluster_size as u64, &upcase_data)?;
    // Zero remainder of last upcase cluster if any.
    let pad = upcase_clusters * cluster_size as u64 - upcase_value_bytes;
    if pad > 0 {
        write_filled(
            dev,
            upcase_lcn * cluster_size as u64 + upcase_value_bytes,
            pad,
            0,
        )?;
    }

    // 4. $Bitmap data -----------------------------------------------------
    let mut bitmap = vec![0u8; (bitmap_clusters * cluster_size as u64) as usize];
    // Mark every cluster we've placed on disk.
    let mut allocate = |start: u64, count: u64| -> Result<(), String> {
        for c in start..start + count {
            if c >= cluster_count {
                return Err(format!(
                    "tried to allocate cluster {c} past volume end {cluster_count}"
                ));
            }
            let byte = (c / 8) as usize;
            let bit = (c % 8) as u8;
            bitmap[byte] |= 1u8 << bit;
        }
        Ok(())
    };
    allocate(0, 1)?; // boot
    allocate(mft_lcn, mft_clusters)?;
    allocate(logfile_lcn, logfile_clusters)?;
    allocate(bitmap_lcn, bitmap_clusters)?;
    allocate(upcase_lcn, upcase_clusters)?;
    allocate(mftmirr_lcn, mftmirr_clusters)?;
    allocate(backup_boot_lcn, 1)?;
    // Trailing bits past `cluster_count` (within the final byte) must be
    // set so they are never picked by the allocator.
    if !cluster_count.is_multiple_of(8) {
        let last_byte = (cluster_count / 8) as usize;
        for bit in (cluster_count % 8)..8 {
            bitmap[last_byte] |= 1u8 << bit;
        }
    }
    dev.write_all_at(bitmap_lcn * cluster_size as u64, &bitmap)?;

    // 5. MFT records ------------------------------------------------------
    let rs = mft_record_size as usize;
    let bps = bytes_per_sector;

    let mft_record_layout = MftLayout {
        record_size: rs,
        bytes_per_sector: bps,
        nt_time: now,
    };

    let mut mft_buf = vec![0u8; (mft_clusters * cluster_size as u64) as usize];

    // record 0: $MFT
    {
        let mft_data_runs = vec![DataRun {
            starting_vcn: 0,
            length: mft_clusters,
            lcn: Some(mft_lcn),
        }];
        let mp = encode_runs(&mft_data_runs)?;
        let data_attr = build_nonresident_data_attribute(
            3,
            mft_clusters * cluster_size as u64,
            mft_clusters * cluster_size as u64,
            mft_clusters * cluster_size as u64,
            (mft_clusters as i64) - 1,
            &mp,
        )?;
        let bitmap_value_size = mft_records_capacity.div_ceil(8) as usize;
        let mft_bitmap_value = make_mft_internal_bitmap(
            bitmap_value_size,
            &[
                rec::MFT,
                rec::MFTMIRR,
                rec::LOGFILE,
                rec::VOLUME,
                rec::ATTRDEF,
                rec::ROOT,
                rec::BITMAP,
                rec::BOOT,
                rec::BADCLUS,
                rec::SECURE,
                rec::UPCASE,
                rec::EXTEND,
            ],
        );
        let bitmap_attr = build_resident_unnamed(0xB0, 4, &mft_bitmap_value);
        // $MFT $FILE_NAME tracks the MFT's $DATA size — the bytes
        // backing the MFT as a file. mft_clusters * cluster_size is
        // exactly what build_nonresident_data_attribute uses above.
        let mft_data_size = mft_clusters * cluster_size as u64;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::MFT,
            "$MFT",
            false,
            mft_data_size,
            mft_data_size,
            &[data_attr, bitmap_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::MFT, rec_bytes)?;
    }

    // record 1: $MFTMirr  — non-resident $DATA pointing at mftmirr_lcn.
    {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: mftmirr_clusters,
            lcn: Some(mftmirr_lcn),
        }];
        let mp = encode_runs(&runs)?;
        let len_bytes = mftmirr_clusters * cluster_size as u64;
        let data_attr = build_nonresident_data_attribute(
            3,
            len_bytes,
            len_bytes,
            len_bytes,
            (mftmirr_clusters as i64) - 1,
            &mp,
        )?;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::MFTMIRR,
            "$MFTMirr",
            false,
            len_bytes,
            len_bytes,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::MFTMIRR, rec_bytes)?;
    }

    // record 2: $LogFile
    {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: logfile_clusters,
            lcn: Some(logfile_lcn),
        }];
        let mp = encode_runs(&runs)?;
        let len_bytes = logfile_clusters * cluster_size as u64;
        let data_attr = build_nonresident_data_attribute(
            3,
            len_bytes,
            len_bytes,
            len_bytes,
            (logfile_clusters as i64) - 1,
            &mp,
        )?;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::LOGFILE,
            "$LogFile",
            false,
            len_bytes,
            len_bytes,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::LOGFILE, rec_bytes)?;
    }

    // record 3: $Volume
    {
        let label = label.unwrap_or("");
        let label_utf16: Vec<u16> = label.encode_utf16().collect();
        let mut volume_name_value = Vec::with_capacity(label_utf16.len() * 2);
        for c in &label_utf16 {
            volume_name_value.extend_from_slice(&c.to_le_bytes());
        }
        let volume_name_attr = build_resident_unnamed(ATTR_VOLUME_NAME, 3, &volume_name_value);

        // $VOLUME_INFORMATION value: reserved(8) + major(1) + minor(1) + flags(2)
        let mut vi = vec![0u8; 12];
        vi[8] = 3;
        vi[9] = 1;
        // flags = 0 (clean)
        vi[10..12].copy_from_slice(&0u16.to_le_bytes());
        let volume_info_attr = build_resident_unnamed(ATTR_VOLUME_INFORMATION, 4, &vi);

        // Empty $DATA at attr_id=5 to satisfy callers that look one up.
        let attrs = vec![build_resident_unnamed(ATTR_DATA, 5, &[])];
        let mut combined = vec![volume_name_attr, volume_info_attr];
        combined.extend(attrs);
        // $Volume's $DATA is empty (resident, zero bytes), so $FILE_NAME
        // sizes are 0/0.
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::VOLUME,
            "$Volume",
            false,
            0,
            0,
            &combined,
        )?;
        place_record(&mut mft_buf, rs, rec::VOLUME, rec_bytes)?;
    }

    // record 4: $AttrDef (canonical 2560-byte table)
    {
        let attrdef_blob = build_attrdef_table();
        // Always non-resident at 2560 bytes (well over the resident
        // ceiling for our 1024/4096 record sizes).
        let attrdef_clusters = (attrdef_blob.len() as u64).div_ceil(cluster_size as u64);
        // Allocate clusters at the tail of the early-allocation region.
        let attrdef_lcn = upcase_lcn + upcase_clusters;
        // Mark allocated in our in-memory bitmap and rewrite.
        for c in attrdef_lcn..attrdef_lcn + attrdef_clusters {
            let byte = (c / 8) as usize;
            let bit = (c % 8) as u8;
            bitmap[byte] |= 1u8 << bit;
        }
        // Write attrdef bytes (zero-pad to cluster boundary).
        let mut padded = attrdef_blob.clone();
        let pad_to = (attrdef_clusters * cluster_size as u64) as usize;
        padded.resize(pad_to, 0);
        dev.write_all_at(attrdef_lcn * cluster_size as u64, &padded)?;
        // Re-write bitmap to capture the late allocation.
        dev.write_all_at(bitmap_lcn * cluster_size as u64, &bitmap)?;

        let runs = vec![DataRun {
            starting_vcn: 0,
            length: attrdef_clusters,
            lcn: Some(attrdef_lcn),
        }];
        let mp = encode_runs(&runs)?;
        let data_attr = build_nonresident_data_attribute(
            3,
            attrdef_blob.len() as u64,
            attrdef_clusters * cluster_size as u64,
            attrdef_blob.len() as u64,
            (attrdef_clusters as i64) - 1,
            &mp,
        )?;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::ATTRDEF,
            "$AttrDef",
            false,
            attrdef_clusters * cluster_size as u64,
            attrdef_blob.len() as u64,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::ATTRDEF, rec_bytes)?;
    }

    // record 5: root directory "."
    {
        let index_block_size: u32 = 4096;
        let index_root = build_empty_index_root_attr(3, index_block_size, bytes_per_sector);
        // Root directory has no $DATA (only an $INDEX_ROOT), so $FILE_NAME
        // sizes are 0/0 — matches Microsoft's reference (rec 5).
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::ROOT,
            ".",
            true,
            0,
            0,
            &[index_root],
        )?;
        place_record(&mut mft_buf, rs, rec::ROOT, rec_bytes)?;
    }

    // record 6: $Bitmap (non-resident $DATA over bitmap_lcn..)
    {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: bitmap_clusters,
            lcn: Some(bitmap_lcn),
        }];
        let mp = encode_runs(&runs)?;
        let value_len = bitmap_bytes;
        let data_attr = build_nonresident_data_attribute(
            3,
            value_len,
            bitmap_clusters * cluster_size as u64,
            value_len,
            (bitmap_clusters as i64) - 1,
            &mp,
        )?;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::BITMAP,
            "$Bitmap",
            false,
            bitmap_clusters * cluster_size as u64,
            value_len,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::BITMAP, rec_bytes)?;
    }

    // record 7: $Boot — non-resident, single-cluster run at LCN 0,
    // value_length = 8192 (boot file is conventionally 8 KiB).
    {
        let boot_value_len: u64 = 8192;
        let boot_clusters: u64 = boot_value_len.div_ceil(cluster_size as u64);
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: boot_clusters,
            lcn: Some(0),
        }];
        let mp = encode_runs(&runs)?;
        let data_attr = build_nonresident_data_attribute(
            3,
            boot_value_len,
            boot_clusters * cluster_size as u64,
            boot_value_len,
            (boot_clusters as i64) - 1,
            &mp,
        )?;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::BOOT,
            "$Boot",
            false,
            boot_clusters * cluster_size as u64,
            boot_value_len,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::BOOT, rec_bytes)?;
    }

    // record 8: $BadClus — empty unnamed $DATA + named "$Bad" sparse
    // covering the whole volume (no clusters allocated; just
    // bookkeeping).
    {
        let empty_data = build_resident_unnamed(ATTR_DATA, 3, &[]);

        // Named $Bad: sparse run covering all clusters (lcn=None).
        let bad_runs = vec![DataRun {
            starting_vcn: 0,
            length: cluster_count,
            lcn: None,
        }];
        let bad_mp = encode_runs(&bad_runs)?;
        let bad_attr = build_nonresident_attribute(
            ATTR_DATA,
            Some("$Bad"),
            4,
            cluster_count * cluster_size as u64,
            cluster_count * cluster_size as u64,
            0,
            (cluster_count as i64) - 1,
            &bad_mp,
        )?;
        // $BadClus's unnamed $DATA is empty (resident, zero bytes); the
        // sparse named $Bad attribute is what carries the cluster
        // bookkeeping. $FILE_NAME tracks the unnamed $DATA, which is
        // empty — sizes are 0/0. (Microsoft's reference matches this.)
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::BADCLUS,
            "$BadClus",
            false,
            0,
            0,
            &[empty_data, bad_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::BADCLUS, rec_bytes)?;
    }

    // record 9: $Secure — minimal resident stub. Real NTFS has $SDS /
    // $SDH / $SII; for v1 we ship empty placeholders. chkdsk treats
    // an empty $Secure as "no security descriptor cache" and tolerates
    // it — the per-file SD pointer in $STANDARD_INFORMATION is what
    // governs ACL semantics, and we set it to 0 (default DACL).
    {
        let empty_data = build_resident_unnamed(ATTR_DATA, 3, &[]);
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::SECURE,
            "$Secure",
            false,
            0,
            0,
            &[empty_data],
        )?;
        place_record(&mut mft_buf, rs, rec::SECURE, rec_bytes)?;
    }

    // record 10: $UpCase
    {
        let runs = vec![DataRun {
            starting_vcn: 0,
            length: upcase_clusters,
            lcn: Some(upcase_lcn),
        }];
        let mp = encode_runs(&runs)?;
        let data_attr = build_nonresident_data_attribute(
            3,
            upcase_value_bytes,
            upcase_clusters * cluster_size as u64,
            upcase_value_bytes,
            (upcase_clusters as i64) - 1,
            &mp,
        )?;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::UPCASE,
            "$UpCase",
            false,
            upcase_clusters * cluster_size as u64,
            upcase_value_bytes,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::UPCASE, rec_bytes)?;
    }

    // record 11: $Extend (empty directory)
    {
        let index_block_size: u32 = 4096;
        let index_root = build_empty_index_root_attr(3, index_block_size, bytes_per_sector);
        // $Extend has no $DATA (only $INDEX_ROOT), 0/0 like root dir.
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::EXTEND,
            "$Extend",
            true,
            0,
            0,
            &[index_root],
        )?;
        place_record(&mut mft_buf, rs, rec::EXTEND, rec_bytes)?;
    }

    // 12..15 reserved — leave free in $MFT:$Bitmap (handled above) and
    // the record bytes zero. NTFS treats records with no FILE magic and
    // IN_USE clear as available.

    // 6. Write $MFT to disk + mirror first 4 records ----------------------
    dev.write_all_at(mft_lcn * cluster_size as u64, &mft_buf)?;

    let mirror_size = (4 * rs).min(mft_buf.len());
    let mut mirror = vec![0u8; (mftmirr_clusters * cluster_size as u64) as usize];
    mirror[..mirror_size].copy_from_slice(&mft_buf[..mirror_size]);
    dev.write_all_at(mftmirr_lcn * cluster_size as u64, &mirror)?;

    dev.sync()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Boot sector
// ---------------------------------------------------------------------------

fn build_boot_sector(
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    cluster_count: u64,
    mft_lcn: u64,
    mftmirr_lcn: u64,
    cluster_size: u32,
    mft_record_size: u32,
    serial: u64,
) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; 512];
    // Jump instruction (3 bytes), per spec: EB 52 90.
    b[0] = 0xEB;
    b[1] = 0x52;
    b[2] = 0x90;
    // OEM: "NTFS    "
    b[3..11].copy_from_slice(NTFS_OEM);
    // BPB
    b[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
    b[0x0D] = sectors_per_cluster;
    // bytes 0x0E..0x10: reserved sectors = 0
    // 0x10..0x13: number of FATs (3 bytes) = 0
    // 0x13..0x15: root entries = 0
    // 0x15..0x17: total small sectors = 0
    b[0x15] = 0xF8; // media descriptor (fixed disk)
                    // 0x16..0x18: sectors per FAT = 0
    b[0x18..0x1A].copy_from_slice(&63u16.to_le_bytes()); // sectors per track (cosmetic)
    b[0x1A..0x1C].copy_from_slice(&255u16.to_le_bytes()); // heads
                                                          // 0x1C..0x20: hidden sectors = 0
                                                          // 0x20..0x24: total large sectors (FAT) = 0
    b[0x24..0x28].copy_from_slice(&0x00800080u32.to_le_bytes()); // signature byte for NTFS

    // Total sectors (volume size in sectors). Includes the very last
    // sector which contains the backup boot.
    let total_sectors: u64 = cluster_count * (cluster_size as u64) / bytes_per_sector as u64;
    b[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    b[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
    b[0x38..0x40].copy_from_slice(&mftmirr_lcn.to_le_bytes());

    // clusters_per_mft_record encoding: positive when record >= cluster,
    // negative log2 when record < cluster.
    let cpmr: i8 = if (mft_record_size as u64) >= cluster_size as u64 {
        let n = mft_record_size as u64 / cluster_size as u64;
        if n > i8::MAX as u64 {
            return Err(format!("clusters_per_mft_record {n} exceeds i8 range"));
        }
        n as i8
    } else {
        let log2 = (mft_record_size.trailing_zeros()) as i8;
        -log2
    };
    b[0x40] = cpmr as u8;
    b[0x41] = 0;
    b[0x42] = 0;
    b[0x43] = 0;

    // clusters_per_index_block (signed, same encoding). 4096-byte index
    // blocks; on a 4096-cluster volume that's 1.
    let cpib_raw: u64 = 4096;
    let cpib: i8 = if cpib_raw >= cluster_size as u64 {
        (cpib_raw / cluster_size as u64) as i8
    } else {
        -(cpib_raw.trailing_zeros() as i8)
    };
    b[0x44] = cpib as u8;
    b[0x45] = 0;
    b[0x46] = 0;
    b[0x47] = 0;

    b[0x48..0x50].copy_from_slice(&serial.to_le_bytes());
    // 0x50..0x54: checksum = 0 (not validated by major drivers).

    // Bootstrap area + boot signature.
    b[0x54] = 0xFA; // CLI
    b[0x55] = 0xEB; // JMP $-1
    b[0x56] = 0xFE;
    // The rest of 0x57..0x1FE stays 0.
    b[0x1FE] = 0x55;
    b[0x1FF] = 0xAA;
    Ok(b)
}

// ---------------------------------------------------------------------------
// MFT record builder (system files)
// ---------------------------------------------------------------------------

struct MftLayout {
    record_size: usize,
    bytes_per_sector: u16,
    nt_time: u64,
}

fn build_system_record(
    layout: &MftLayout,
    record_number: u32,
    name: &str,
    is_dir: bool,
    // $FILE_NAME tracks the underlying $DATA's allocated + real sizes.
    // Pass 0 for both when the record has no $DATA (directories,
    // $Volume's empty $DATA). Microsoft's format.com populates these
    // fields with the canonical $DATA sizes; with them at 0, chkdsk
    // reports 'Attribute record (30, "") is corrupt'. Corroborated
    // against format.com reference in CI iter8 (mft-rec*-diff.txt).
    fn_data_alloc: u64,
    fn_data_real: u64,
    extra_attrs: &[Vec<u8>],
) -> Result<Vec<u8>, String> {
    let rs = layout.record_size;
    let bps = layout.bytes_per_sector;
    if rs < 512 || !rs.is_multiple_of(bps as usize) {
        return Err(format!("invalid record_size {rs}"));
    }

    let mut rec = vec![0u8; rs];
    rec[0..4].copy_from_slice(FILE_MAGIC);
    rec[REC_OFF_USA_OFFSET..REC_OFF_USA_OFFSET + 2]
        .copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
    let sectors = rs / bps as usize;
    rec[REC_OFF_USA_COUNT..REC_OFF_USA_COUNT + 2]
        .copy_from_slice(&((sectors + 1) as u16).to_le_bytes());
    // USA starts at 0x30 and consumes 2 bytes per slot (1 USN + N
    // saves). Round up to 8 for the first attribute. For 1024-byte /
    // 512-bps records this lands at 0x38 (matching record_build.rs).
    // For 4096-byte / 512-bps records this lands at 0x48.
    let attrs_offset = align8(USA_OFFSET + 2 + sectors * 2);
    rec[REC_OFF_LSN..REC_OFF_LSN + 8].copy_from_slice(&0u64.to_le_bytes());
    // Microsoft's format.com sets sequence_number = max(1, rec_number)
    // for system records (CI iter10 byte-diff: rec 5 has seq=5, rec 11
    // has seq=11, etc.). Our prior `seq=1` constant created a mismatch
    // against parent_reference's (rec=5, seq=5) that pointed at the
    // root: the children claimed parent (5,5) but the root itself had
    // seq=1, so chkdsk reported "Incorrect information was detected in
    // file record segment N" on every system record EXCEPT 0 and 1
    // (whose own seq=1 happened to match the constant).
    let rec_seq: u16 = if record_number == 0 {
        1
    } else {
        record_number as u16
    };
    rec[REC_OFF_SEQ..REC_OFF_SEQ + 2].copy_from_slice(&rec_seq.to_le_bytes());
    rec[REC_OFF_LINK_COUNT..REC_OFF_LINK_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());
    rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
        .copy_from_slice(&(attrs_offset as u16).to_le_bytes());
    let flags: u16 = 0x0001 | if is_dir { 0x0002 } else { 0x0000 };
    rec[REC_OFF_FLAGS..REC_OFF_FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
    rec[REC_OFF_BYTES_ALLOCATED..REC_OFF_BYTES_ALLOCATED + 4]
        .copy_from_slice(&(rs as u32).to_le_bytes());
    rec[REC_OFF_BASE_FILE_REF..REC_OFF_BASE_FILE_REF + 8].copy_from_slice(&0u64.to_le_bytes());
    // next_attr_id: leave room for a few; pick high enough for any
    // future addition without colliding.
    rec[REC_OFF_NEXT_ATTR_ID..REC_OFF_NEXT_ATTR_ID + 2].copy_from_slice(&16u16.to_le_bytes());
    rec[REC_OFF_MFT_REC_NUM..REC_OFF_MFT_REC_NUM + 4].copy_from_slice(&record_number.to_le_bytes());
    rec[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());

    let mut cursor = attrs_offset;

    // Records 0..11 always live as children of the root directory. Use
    // sequence=ROOT (5) for parent reference per NTFS convention. The
    // root itself parents to itself (`.` is its own parent in NTFS),
    // so the same encoding applies regardless of which record this is.
    let parent_ref = encode_file_reference(rec::ROOT as u64, 5);

    cursor = write_standard_information(&mut rec, cursor, 0, layout.nt_time, is_dir, true);
    cursor = write_file_name(
        &mut rec,
        cursor,
        1,
        parent_ref,
        name,
        layout.nt_time,
        is_dir,
        true,
        fn_data_alloc,
        fn_data_real,
    )?;

    for attr in extra_attrs {
        if cursor + attr.len() + 8 > rs {
            return Err(format!(
                "system record {record_number} too small: need {} more bytes for attr",
                attr.len()
            ));
        }
        rec[cursor..cursor + attr.len()].copy_from_slice(attr);
        cursor += attr.len();
    }

    // The attribute end marker is the type 0xFFFFFFFF + a 4-byte length
    // field of 0, totalling 8 bytes — not 4. The buffer is zero-init,
    // so we only need to write the type, but bytes_used MUST include
    // the trailing 4-byte length=0. Microsoft format.com's reference
    // shows bytes_used = end_marker_offset + 8 across every system
    // record (CI iter9 byte-diff: rec0 ref=0x210 vs ours=0x17C, etc.).
    // chkdsk reports "First free byte offset corrected" when this is
    // off by 4.
    rec[cursor..cursor + 4].copy_from_slice(&ATTR_END_MARKER.to_le_bytes());
    cursor += 8;
    rec[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&(cursor as u32).to_le_bytes());

    apply_fixup_on_write(&mut rec, bps)?;
    Ok(rec)
}

fn place_record(
    mft_buf: &mut [u8],
    record_size: usize,
    record_number: u32,
    rec: Vec<u8>,
) -> Result<(), String> {
    let off = (record_number as usize) * record_size;
    if off + record_size > mft_buf.len() {
        return Err(format!("record {record_number} past MFT buffer"));
    }
    mft_buf[off..off + record_size].copy_from_slice(&rec);
    Ok(())
}

fn encode_file_reference(record_number: u64, sequence: u16) -> u64 {
    (record_number & 0x0000_FFFF_FFFF_FFFF) | ((sequence as u64) << 48)
}

fn write_standard_information(
    rec: &mut [u8],
    at: usize,
    attr_id: u16,
    nt_time: u64,
    is_dir: bool,
    is_system: bool,
) -> usize {
    let header_size = 24usize;
    let value_size = 72usize;
    let attr_length = align8(header_size + value_size);
    rec[at..at + 4].copy_from_slice(&ATTR_STANDARD_INFORMATION.to_le_bytes());
    rec[at + 4..at + 8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    rec[at + 8] = 0;
    rec[at + 9] = 0;
    rec[at + 10..at + 12].copy_from_slice(&(header_size as u16).to_le_bytes());
    rec[at + 12..at + 14].copy_from_slice(&0u16.to_le_bytes());
    rec[at + 14..at + 16].copy_from_slice(&attr_id.to_le_bytes());
    rec[at + 16..at + 20].copy_from_slice(&(value_size as u32).to_le_bytes());
    rec[at + 20..at + 22].copy_from_slice(&(header_size as u16).to_le_bytes());
    rec[at + 22] = 0;
    rec[at + 23] = 0;

    let v = at + header_size;
    rec[v..v + 8].copy_from_slice(&nt_time.to_le_bytes());
    rec[v + 8..v + 16].copy_from_slice(&nt_time.to_le_bytes());
    rec[v + 16..v + 24].copy_from_slice(&nt_time.to_le_bytes());
    rec[v + 24..v + 32].copy_from_slice(&nt_time.to_le_bytes());
    let mut fa: u32 = 0x20; // ARCHIVE
    if is_dir {
        fa |= 0x10000000;
    }
    if is_system {
        fa |= 0x06; // HIDDEN | SYSTEM
    }
    rec[v + 32..v + 36].copy_from_slice(&fa.to_le_bytes());
    at + attr_length
}

fn write_file_name(
    rec: &mut [u8],
    at: usize,
    attr_id: u16,
    parent_reference: u64,
    name: &str,
    nt_time: u64,
    is_dir: bool,
    is_system: bool,
    data_alloc: u64,
    data_real: u64,
) -> Result<usize, String> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    if utf16.is_empty() || utf16.len() > 255 {
        return Err(format!("invalid name length {}", utf16.len()));
    }
    let header_size = 24usize;
    let key_fixed = 0x42usize;
    let value_size = key_fixed + utf16.len() * 2;
    let attr_length = align8(header_size + value_size);
    if at + attr_length > rec.len() {
        return Err("$FILE_NAME doesn't fit".to_string());
    }
    rec[at..at + 4].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes());
    rec[at + 4..at + 8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    rec[at + 8] = 0;
    rec[at + 9] = 0;
    rec[at + 10..at + 12].copy_from_slice(&(header_size as u16).to_le_bytes());
    rec[at + 12..at + 14].copy_from_slice(&0u16.to_le_bytes());
    rec[at + 14..at + 16].copy_from_slice(&attr_id.to_le_bytes());
    rec[at + 16..at + 20].copy_from_slice(&(value_size as u32).to_le_bytes());
    rec[at + 20..at + 22].copy_from_slice(&(header_size as u16).to_le_bytes());
    // indexed_flag = 1: corroborated against Microsoft format.com output
    // in CI iter8 — every $FILE_NAME (system and otherwise) had this byte
    // set to 1, every one of ours had it 0. chkdsk reports
    // 'Attribute record (30, "") is corrupt' when this differs.
    rec[at + 22] = 1;
    rec[at + 23] = 0;

    let v = at + header_size;
    rec[v..v + 8].copy_from_slice(&parent_reference.to_le_bytes());
    rec[v + 8..v + 16].copy_from_slice(&nt_time.to_le_bytes());
    rec[v + 16..v + 24].copy_from_slice(&nt_time.to_le_bytes());
    rec[v + 24..v + 32].copy_from_slice(&nt_time.to_le_bytes());
    rec[v + 32..v + 40].copy_from_slice(&nt_time.to_le_bytes());
    rec[v + 40..v + 48].copy_from_slice(&data_alloc.to_le_bytes());
    rec[v + 48..v + 56].copy_from_slice(&data_real.to_le_bytes());
    let mut fa: u32 = 0x20;
    if is_dir {
        fa |= 0x10000000;
    }
    if is_system {
        fa |= 0x06;
    }
    rec[v + 56..v + 60].copy_from_slice(&fa.to_le_bytes());
    rec[v + 60..v + 64].copy_from_slice(&0u32.to_le_bytes());
    rec[v + 64] = utf16.len() as u8;
    rec[v + 65] = NAMESPACE_WIN32_DOS;
    for (i, c) in utf16.iter().enumerate() {
        let off = v + 66 + i * 2;
        rec[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    Ok(at + attr_length)
}

// ---------------------------------------------------------------------------
// Resident attribute helpers
// ---------------------------------------------------------------------------

fn build_resident_unnamed(attr_type: u32, attr_id: u16, value: &[u8]) -> Vec<u8> {
    let header_size = 24usize;
    let attr_length = align8(header_size + value.len());
    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&attr_type.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0;
    buf[9] = 0;
    buf[10..12].copy_from_slice(&(header_size as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(header_size as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;
    if !value.is_empty() {
        buf[header_size..header_size + value.len()].copy_from_slice(value);
    }
    buf
}

fn build_empty_index_root_attr(
    attr_id: u16,
    index_block_size: u32,
    _bytes_per_sector: u16,
) -> Vec<u8> {
    // Attribute layout: 16 (common) + 8 (resident fields) + name "$I30"
    // (8 bytes UTF-16 LE) + value padded to 8.
    let common_header = 16usize;
    let resident_fields = 8usize;
    let header_size = common_header + resident_fields;
    let name_offset = header_size;
    let name_bytes = 8usize;
    let value_offset = align8(name_offset + name_bytes);
    let ir_value_size = 16 + 16 + 16; // IR_HEADER + INDEX_HEADER + LAST sentinel
    let attr_length = align8(value_offset + ir_value_size);

    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&ATTR_INDEX_ROOT.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0;
    buf[9] = 4; // name_length: "$I30" = 4 chars
    buf[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(ir_value_size as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;

    // Name "$I30"
    let i30: [u16; 4] = ['$' as u16, 'I' as u16, '3' as u16, '0' as u16];
    for (i, c) in i30.iter().enumerate() {
        let off = name_offset + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }

    // INDEX_ROOT header (16 bytes)
    let v = value_offset;
    buf[v..v + 4].copy_from_slice(&0x30u32.to_le_bytes()); // attribute_type = $FILE_NAME
    buf[v + 4..v + 8].copy_from_slice(&1u32.to_le_bytes()); // collation = COLLATION_FILE_NAME
    buf[v + 8..v + 12].copy_from_slice(&index_block_size.to_le_bytes());
    buf[v + 12] = 1;
    // 3 bytes padding remain zero.

    // INDEX_HEADER (16 bytes)
    let ih = v + 16;
    buf[ih..ih + 4].copy_from_slice(&16u32.to_le_bytes());
    buf[ih + 4..ih + 8].copy_from_slice(&32u32.to_le_bytes());
    buf[ih + 8..ih + 12].copy_from_slice(&32u32.to_le_bytes());
    // flags = 0 (no $INDEX_ALLOCATION)

    // LAST sentinel
    let le = ih + 16;
    buf[le..le + 8].copy_from_slice(&0u64.to_le_bytes());
    buf[le + 8..le + 10].copy_from_slice(&16u16.to_le_bytes());
    buf[le + 10..le + 12].copy_from_slice(&0u16.to_le_bytes());
    buf[le + 12..le + 14].copy_from_slice(&0x0002u16.to_le_bytes());
    buf[le + 14..le + 16].copy_from_slice(&0u16.to_le_bytes());

    buf
}

// ---------------------------------------------------------------------------
// $MFT-internal bitmap (records-in-use bitmap stored in $MFT:$Bitmap).
// ---------------------------------------------------------------------------

fn make_mft_internal_bitmap(size_bytes: usize, used_records: &[u32]) -> Vec<u8> {
    let mut b = vec![0u8; size_bytes.max(8)];
    for &rn in used_records {
        let byte = (rn / 8) as usize;
        let bit = (rn % 8) as u8;
        if byte < b.len() {
            b[byte] |= 1u8 << bit;
        }
    }
    b
}

// ---------------------------------------------------------------------------
// $AttrDef table (canonical NTFS 3.1)
// ---------------------------------------------------------------------------

/// Build the canonical NTFS 3.1 $AttrDef table. 20 entries × 160 bytes
/// + 1 zero-terminator entry = 2560 bytes total. Format per Flatcap
///   /MS-FSCC: 64-byte UTF-16 name + u32 type + u32 display_rule + u32
///   collation + u32 flags + u64 min_size + u64 max_size.
fn build_attrdef_table() -> Vec<u8> {
    struct Entry {
        name: &'static str,
        type_code: u32,
        collation: u32,
        flags: u32,
        min_size: u64,
        max_size: i64, // -1 ⇒ 0xFFFF_FFFF_FFFF_FFFF
    }
    const RESIDENT: u32 = 0x40;
    const NONRES: u32 = 0x80;
    const INDEXED: u32 = 0x02;
    const NEG1: i64 = -1;
    let entries = [
        Entry {
            name: "$STANDARD_INFORMATION",
            type_code: 0x10,
            collation: 0,
            flags: RESIDENT,
            min_size: 48,
            max_size: 72,
        },
        Entry {
            name: "$ATTRIBUTE_LIST",
            type_code: 0x20,
            collation: 0,
            flags: NONRES,
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$FILE_NAME",
            type_code: 0x30,
            collation: 1,
            flags: RESIDENT | INDEXED,
            min_size: 68,
            max_size: 578,
        },
        Entry {
            name: "$OBJECT_ID",
            type_code: 0x40,
            collation: 0,
            flags: RESIDENT,
            min_size: 0,
            max_size: 256,
        },
        Entry {
            name: "$SECURITY_DESCRIPTOR",
            type_code: 0x50,
            collation: 0,
            flags: NONRES,
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$VOLUME_NAME",
            type_code: 0x60,
            collation: 0,
            flags: RESIDENT,
            min_size: 2,
            max_size: 256,
        },
        Entry {
            name: "$VOLUME_INFORMATION",
            type_code: 0x70,
            collation: 0,
            flags: RESIDENT,
            min_size: 12,
            max_size: 12,
        },
        Entry {
            name: "$DATA",
            type_code: 0x80,
            collation: 0,
            flags: 0,
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$INDEX_ROOT",
            type_code: 0x90,
            collation: 0,
            flags: RESIDENT,
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$INDEX_ALLOCATION",
            type_code: 0xA0,
            collation: 0,
            flags: NONRES,
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$BITMAP",
            type_code: 0xB0,
            collation: 0,
            flags: 0,
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$REPARSE_POINT",
            type_code: 0xC0,
            collation: 0,
            flags: 0,
            min_size: 0,
            max_size: 16384,
        },
        Entry {
            name: "$EA_INFORMATION",
            type_code: 0xD0,
            collation: 0,
            flags: RESIDENT,
            min_size: 8,
            max_size: 8,
        },
        Entry {
            name: "$EA",
            type_code: 0xE0,
            collation: 0,
            flags: 0,
            min_size: 0,
            max_size: 65536,
        },
        Entry {
            name: "$PROPERTY_SET",
            type_code: 0xF0,
            collation: 0,
            flags: 0,
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$LOGGED_UTILITY_STREAM",
            type_code: 0x100,
            collation: 0,
            flags: 0,
            min_size: 0,
            max_size: 65536,
        },
    ];
    let mut out = Vec::with_capacity(160 * entries.len());
    for e in &entries {
        let mut buf = [0u8; 160];
        let name_utf16: Vec<u16> = e.name.encode_utf16().collect();
        for (i, c) in name_utf16.iter().enumerate().take(64) {
            let off = i * 2;
            buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
        }
        // 0x80: type
        buf[0x80..0x84].copy_from_slice(&e.type_code.to_le_bytes());
        // 0x84: display rule
        buf[0x84..0x88].copy_from_slice(&0u32.to_le_bytes());
        // 0x88: collation
        buf[0x88..0x8C].copy_from_slice(&e.collation.to_le_bytes());
        // 0x8C: flags
        buf[0x8C..0x90].copy_from_slice(&e.flags.to_le_bytes());
        // 0x90: min_size (i64)
        buf[0x90..0x98].copy_from_slice(&e.min_size.to_le_bytes());
        // 0x98: max_size (i64; -1 ⇒ all-ones)
        buf[0x98..0xA0].copy_from_slice(&e.max_size.to_le_bytes());
        out.extend_from_slice(&buf);
    }
    // Trailing zero entry (some tools key on it; harmless).
    out.extend(std::iter::repeat_n(0u8, 160));
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_filled(dev: &mut dyn BlockIo, offset: u64, len: u64, fill: u8) -> Result<(), String> {
    const CHUNK: usize = 64 * 1024;
    let buf = vec![fill; CHUNK];
    let mut off = offset;
    let mut remain = len;
    while remain > 0 {
        let n = remain.min(CHUNK as u64) as usize;
        dev.write_all_at(off, &buf[..n])?;
        off += n as u64;
        remain -= n as u64;
    }
    Ok(())
}

/// Generate a 64-bit volume serial. Uses time + a hash of available
/// entropy. No `getrandom` dep — we'd otherwise pull in a transitive
/// crate just for this single value.
fn generate_serial() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x0123_4567_89AB_CDEF);
    // Mix in a stack-address bit to vary across runs even if the
    // monotonic clock is coarse.
    let stackish = &now as *const _ as usize as u64;
    now.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ stackish.rotate_left(17)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_runs::{decode_runs, DataRun};

    #[test]
    fn upcase_table_size() {
        let t = upcase::generate_upcase_table();
        assert_eq!(t.len(), 128 * 1024);
        // 'a' (0x61) → 'A' (0x41)
        let off = 0x61 * 2;
        assert_eq!(u16::from_le_bytes([t[off], t[off + 1]]), 0x41);
        // 'A' (0x41) → 'A' (0x41)
        let off2 = 0x41 * 2;
        assert_eq!(u16::from_le_bytes([t[off2], t[off2 + 1]]), 0x41);
    }

    #[test]
    fn run_encode_decode_roundtrip() {
        let cases: Vec<Vec<DataRun>> = vec![
            vec![DataRun {
                starting_vcn: 0,
                length: 32,
                lcn: Some(4),
            }],
            vec![
                DataRun {
                    starting_vcn: 0,
                    length: 8,
                    lcn: Some(100),
                },
                DataRun {
                    starting_vcn: 8,
                    length: 16,
                    lcn: Some(200),
                },
                DataRun {
                    starting_vcn: 24,
                    length: 4,
                    lcn: Some(150),
                },
            ],
            vec![
                DataRun {
                    starting_vcn: 0,
                    length: 8,
                    lcn: None,
                },
                DataRun {
                    starting_vcn: 8,
                    length: 8,
                    lcn: Some(1024),
                },
            ],
        ];
        for (i, runs) in cases.iter().enumerate() {
            let bytes = encode_runs(runs).unwrap_or_else(|e| panic!("case {i}: {e}"));
            let back = decode_runs(&bytes).unwrap_or_else(|e| panic!("case {i}: {e}"));
            assert_eq!(*runs, back, "case {i}");
        }
    }
}
