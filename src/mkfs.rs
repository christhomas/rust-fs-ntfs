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
//! References (no GPL code consulted): NTFS boot sector / BPB and
//! $MFT layout per Windows Internals 7th ed. ch. "NTFS On-Disk
//! Structure"; MS-FSCC for system files and attribute definitions.

use crate::block_io::BlockIo;
use crate::data_runs::{encode_runs, DataRun};
use crate::mft_io::apply_fixup_on_write;
use crate::record_build::{
    align8, build_nonresident_attribute, build_nonresident_data_attribute, nt_time_now, FA_ARCHIVE,
    FA_HIDDEN, FA_NTFS_DIRECTORY, FA_NTFS_VIEW_INDEX, FA_SYSTEM,
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
const COLLATION_FILE_NAME: u32 = 0x01;
// $SDH (Security Descriptor Hash) view-index — keyed by
// (hash, security_id). MS-FSCC §2.4 ("Indexable Types") and the
// publicly published NTFS layout list this collation as
// `COLLATION_NTOFS_SECURITY_HASH = 0x12`.
const COLLATION_NTOFS_SECURITY_HASH: u32 = 0x12;
// $SII (Security ID Index) view-index — keyed by security_id.
// `COLLATION_NTOFS_ULONG = 0x10` per the same source. Used by
const COLLATION_NTOFS_ULONG: u32 = 0x10;
// $ObjId:$O and $Reparse:$R view-index — keyed by OBJECT_ID (16-byte
// GUID-like structure) and reparse-tag+MFT-ref respectively.
// `COLLATION_NTOFS_ULONGS = 0x13` per MS-FSCC §2.4.
const COLLATION_NTOFS_ULONGS: u32 = 0x13;
// $Quota:$O view-index — keyed by Security Identifier (SID).
// `COLLATION_NTOFS_SID = 0x11` per MS-FSCC §2.4.
const COLLATION_NTOFS_SID: u32 = 0x11;

// ---------------------------------------------------------------------------
// `SD_SYSFILE_RW`: canonical security descriptor stored in $Secure:$SDS
// (security_id = 0x100).  All system records reference this entry via
// their $STD_INFO.SecurityId field; no per-file $SECURITY_DESCRIPTOR
// attribute is emitted in NTFS v3.x mode (S3.1 upgrade, 2026-05-24).
//
// DACL access mask 0x0012009F = FILE_GENERIC_READ | FILE_GENERIC_WRITE
// | FILE_GENERIC_EXECUTE.  SECURITY_DESCRIPTOR_RELATIVE per MS-DTYP
// §2.4.6: Revision=1, Control=0x8004 (SE_DACL_PRESENT|SE_SELF_RELATIVE),
// Owner/Group = BUILTIN\Administrators (S-1-5-32-544), no SACL.
const SD_SYSFILE_RW: &[u8] = &[
    0x01, 0x00, 0x04, 0x80, 0x48, 0x00, 0x00, 0x00, 0x58, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x02, 0x00, 0x34, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00,
    0x9f, 0x01, 0x12, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x12, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x18, 0x00, 0x9f, 0x01, 0x12, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
    0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
    0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
    0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00,
];

// ---------------------------------------------------------------------------
// $LogFile canonical content — first 12 KiB.
//
// Base bytes captured from a Microsoft `format.com /FS:NTFS` reference run on
// a 256 MiB / 4 KiB-cluster volume (diag
// `test-diagnostics/run-20260502-154836/mac-format-label-empty/
// reference-logfile.bin`).
//
// The restart page pair was then patched so that page 0 (cur_lsn=0x104408) is
// the authoritative copy and page 1 is a stale backup (cur_lsn=0x100000).
// Without this patch, NTFS would select page 1 as authoritative (it had a
// higher cur_lsn=0x10634B) and look for log records in the range
// [0x10443C, 0x10634B] — a range not covered by the embedded pages — which
// triggered a full recovery pass on every first mount, causing chkdsk /scan
// to exit 13 during the initialization window.  With the patch, NTFS selects
// page 0 and its active range [0x100000, 0x104408] is fully covered by page 2
// (RCRD, lsn=0x104408), so no recovery is needed and the first online scan
// exits 0.
//
// Layout of the 12 KiB:
//   * page 0 (offset 0x0000)  — authoritative RSTR (cur_lsn=0x104408,
//                               oldest_lsn=0x100000, flags=0x0000 clean)
//   * page 1 (offset 0x1000)  — stale backup RSTR (cur_lsn=0x100000;
//                               ntfs.sys ignores it as older)
//   * page 2 (offset 0x2000)  — single RCRD record page (lsn=0x104408;
//                               covers page 0's active range)
//
// Bytes 0x3000..end are all-0xFF; written by the 0xFF sweep below.
//
// References: MS-FSCC (system files / log structure), Windows Internals
// 7th ed. ch. "NTFS Logging" (RSTR / RCRD page taxonomy). No GPL'd
// NTFS reimplementations consulted.
//
const LOGFILE_CANONICAL: &[u8] = include_bytes!("logfile-canonical-12k.bin");

/// `$FILE_NAME` namespace values (MS-FSCC §2.4.4).
///
/// * `WIN32_DOS` (3) is used on every name we currently ship: root,
///   the canonical 0..10 system files, and `$Extend` itself. All
///   these names fit DOS 8.3.
/// * `POSIX` (0) is the value any future `$Extend` descendant must
///   pass to `write_file_name` / `build_file_name_stream` — Iter L
///   2026-05-22 byte truth (clean Windows-format reference) showed
///   every `$Extend` child uses POSIX namespace, and shipping an
///   11-char name like `$RmMetadata` with `WIN32_DOS` makes chkdsk
///   Stage 2 reject it ("An invalid filename X (11) was found in
///   directory B"). Defined here (despite no current call site) so
///   the rule documented above is self-enforcing at the call site
///   the day we re-introduce those records.
#[allow(dead_code)] // Reserved for future $Extend descendants; see docstring above.
const NAMESPACE_POSIX: u8 = 0;
const NAMESPACE_WIN32_DOS: u8 = 3;

/// MFT record numbers + canonical names for the system metafiles we
/// populate. Pair the constants with [`rec::name`] so callers never
/// re-type the magic strings (`"$Secure"` vs `"$Sercure"` would
/// otherwise compile fine and produce a broken volume).
pub mod rec {
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
    // Records 12..15 are reserved (no $FILE_NAME, not in root $I30).
    // Microsoft format.com leaves these as in-use 304-byte placeholder
    // records — see build_reserved_placeholder + the loop in
    // format_filesystem.
    //
    // Slots 16..18 are $Extend's children. scan-f-scan experiments
    // (2026-05-24) proved that chkdsk /scan exits 13 unless $ObjId,
    // $Reparse, and $RmMetadata are present under $Extend at format
    // time; chkdsk /f creates them if absent, and scan2 then exits 0.
    // The earlier Iter L warning about TxF Event 136 was based on a
    // broken S4-v1 that omitted VIEW_INDEX (0x0008) from $Reparse.
    // With correct flags all three ship safely.
    pub const OBJID: u32 = 16; // $Extend\$ObjId   VIEW_INDEX
    pub const REPARSE: u32 = 17; // $Extend\$Reparse VIEW_INDEX
    pub const QUOTA: u32 = 18; // $Extend\$Quota   VIEW_INDEX (empty $O/$Q indexes)
                               // $RmMetadata intentionally omitted: chkdsk /scan accepts $ObjId+$Reparse
                               // without $RmMetadata (scan-f-scan 2026-05-24). Including an empty
                               // $RmMetadata causes "corrupt basic file structure" because chkdsk
                               // expects its $I30 to contain $Repair/$TxfLog/$Txf children; shipping
                               // those chains is out of scope.

    /// Canonical NTFS name for each system MFT record (root → "."
    /// per filesystem convention; everything else is `$Name`). This
    /// is the single source of truth — every call site that needs
    /// `"$Secure"` or `"$MFT"` must go through this so a typo cannot
    /// produce a broken-but-compiling volume.
    ///
    /// Returns `Some(&'static str)` for known system MFT records
    /// (0..=11 minus the reserved 5/12..15 placeholders that have
    /// no `$FILE_NAME`) and `None` otherwise.
    ///
    /// # Note on `cluster_size`
    ///
    /// Under NTFS 1.2 (pre-upgrade), rec 9 was named `$Quota` on
    /// small-cluster volumes and chkdsk enforced that convention. With
    /// NTFS 3.1 (our current format), the kernel performs the 1.2→3.1
    /// upgrade (renaming `$Quota`→`$Secure`, among other changes) via
    /// `UPGRADE_ON_MOUNT` when a 1.2 volume is first mounted. Since we
    /// write NTFS 3.1 directly, rec 9 is `$Secure` at every cluster
    /// size. The `cluster_size` parameter is retained for call-site
    /// symmetry (all callers already pass it) but is currently unused.
    pub fn name(rec_num: u32, _cluster_size: u32) -> Option<&'static str> {
        Some(match rec_num {
            MFT => "$MFT",
            MFTMIRR => "$MFTMirr",
            LOGFILE => "$LogFile",
            VOLUME => "$Volume",
            ATTRDEF => "$AttrDef",
            ROOT => ".",
            BITMAP => "$Bitmap",
            BOOT => "$Boot",
            BADCLUS => "$BadClus",
            SECURE => "$Secure",
            UPCASE => "$UpCase",
            EXTEND => "$Extend",
            OBJID => "$ObjId",
            REPARSE => "$Reparse",
            QUOTA => "$Quota",
            _ => return None,
        })
    }
}

/// Named NTFS attribute stream identifiers. These are the **named**
/// `$DATA` / `$INDEX_ROOT` / `$INDEX_ALLOCATION` / `$BITMAP` streams
/// we emit at format time AND read back at runtime. Centralising
/// them here makes the spellings review-able in one place and
/// prevents typos at call sites (both write-path `mkfs` and
/// read-path `index_io` / `idx_block` go through this module).
pub mod stream {
    /// `$BadClus:$Bad` — non-resident sparse $DATA carrying the
    /// volume's bad-cluster bitmap. Empty at fresh format.
    pub const BAD: &str = "$Bad";

    /// `$Secure:$SDS` — non-resident $DATA carrying every security
    /// descriptor on the volume (mirrored at +0x40000).
    pub const SDS: &str = "$SDS";

    /// `$Secure:$SDH` — INDEX_ROOT (+ optional INDEX_ALLOCATION) for
    /// the security-descriptor-by-hash view index.
    pub const SDH: &str = "$SDH";

    /// `$Secure:$SII` — INDEX_ROOT (+ optional INDEX_ALLOCATION) for
    /// the security-descriptor-by-id view index.
    pub const SII: &str = "$SII";

    /// `$UpCase:$Info` — 32-byte resident $DATA carrying the CRC64
    /// of the $UpCase table (Iter M-2 byte truth).
    pub const INFO: &str = "$Info";

    /// `$I30` — every directory's file-name index. Used as both the
    /// INDEX_ROOT attribute name and the INDEX_ALLOCATION /
    /// BITMAP / etc. stream name when the directory grows.
    pub const I30: &str = "$I30";

    /// `$ObjId:$O` / `$Quota:$O` — object-id and quota view indexes.
    pub const O: &str = "$O";

    /// `$Reparse:$R` — reparse-point view index.
    pub const R: &str = "$R";

    /// `$Quota:$Q` — quota table view index (keyed by owner_id).
    pub const Q: &str = "$Q";

    /// UTF-16-LE encoding of a stream name. NTFS attribute headers
    /// store names as raw UTF-16 code units (no NUL); this is the
    /// one place that conversion happens, so a typo upstream can't
    /// produce a malformed-but-compiling name buffer.
    pub fn utf16(name: &str) -> Vec<u16> {
        name.encode_utf16().collect()
    }
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
    // mft_lcn must sit past the cluster region $Boot's $DATA claims
    // (8 KiB conventional). At 4 KiB cluster that's 2 clusters and our
    // historic mft_lcn=4 leaves a 2-cluster gap; at 1 KiB cluster the
    // 8 KiB boot spans 8 clusters and a hardcoded 4 puts MFT *inside*
    // $Boot's $DATA mapping — chkdsk reports `Corrupt master file
    // table. CHKDSK aborted` (matrix run-20260503-024058 cluster-1k
    // and cluster-512). Cap to max(4, boot_clusters).
    let boot_clusters_for_layout: u64 = 8192u64.div_ceil(cluster_size as u64);
    let mft_lcn: u64 = boot_clusters_for_layout.max(4);
    let mft_clusters: u64 = (mft_record_size as u64 * 64)
        .div_ceil(cluster_size as u64)
        .max(1);
    let mft_records_capacity: u64 = mft_clusters * cluster_size as u64 / mft_record_size as u64;
    if mft_records_capacity < 24 {
        return Err("MFT initial allocation too small".to_string());
    }

    let logfile_lcn = mft_lcn + mft_clusters;
    // $LogFile sized at 0x3B0000 (3866624 bytes / ~3.78 MiB). The
    // value MUST match the `file_size` field encoded inside the
    // baked LOGFILE_CANONICAL restart pages — ntfs.sys reads
    // restart-area.file_size at mount and chkdsk compares it to
    // the on-disk allocated length. With our previous 1 MiB sizing,
    // chkdsk reported "CHKDSK is adjusting the size of the log file"
    // because the restart area declared 3866624 but the file was
    // 1048576. Aligning the two clears the warning.
    //
    // 0x3B0000 is what Microsoft's format.com produced for our
    // 256 MiB / 4 KiB-cluster reference; for cluster sizes other
    // than 4 KiB the value rounds up to the nearest cluster.
    const LOGFILE_TARGET_BYTES: u64 = 0x3B_0000;
    let logfile_clusters: u64 = LOGFILE_TARGET_BYTES.div_ceil(cluster_size as u64);

    let bitmap_lcn = logfile_lcn + logfile_clusters;
    let bitmap_bytes: u64 = cluster_count.div_ceil(8);
    let bitmap_clusters: u64 = bitmap_bytes.div_ceil(cluster_size as u64);

    let upcase_lcn = bitmap_lcn + bitmap_clusters;
    let upcase_bytes: u64 = 128 * 1024;
    let upcase_clusters: u64 = upcase_bytes.div_ceil(cluster_size as u64);

    // $AttrDef placement was historically computed in the rec 4 block
    // (`attrdef_lcn = upcase_lcn + upcase_clusters`). Moving it into
    // layout planning so $MFT's bitmap can be placed immediately
    // after — otherwise both end up at the same LCN and $AttrDef's
    // write clobbers the $MFT bitmap cluster.
    let attrdef_lcn = upcase_lcn + upcase_clusters;
    let attrdef_clusters: u64 = (16 * 160u64).div_ceil(cluster_size as u64).max(1);

    // $MFT's own $BITMAP attribute is non-resident on Microsoft's
    // reference output (per-record byte-diff: ref ships an attribute
    // header pointing at a single cluster of bitmap data; our previous
    // code shipped a 32-byte resident attribute, leaving the on-disk
    // record 40 bytes shorter than the reference). After fixing
    // $LogFile, ntfs.sys advances past the log read but then trips
    // Event ID 55 ("MFT contains a corrupted file record") while
    // parsing $MFT's record itself. Diag write-smoke-20260502-161433.
    //
    // Allocate one cluster immediately after $AttrDef to hold the
    // bitmap bytes (only mft_records_capacity/8 bytes used; rest of
    // the cluster is zero-padded — same shape as Microsoft's output).
    let mft_bitmap_lcn = attrdef_lcn + attrdef_clusters;
    let mft_bitmap_clusters: u64 = 1;

    // $Secure:$SDS data — two real clusters (primary entries + mirror at
    // file offset 0x40000). Placed immediately after $MFT's $BITMAP
    // cluster. The 256 KiB gap between primary and mirror lives in a
    // sparse data run, so only 2 clusters are actually consumed on
    // disk regardless of cluster size — matches the Microsoft
    // reference's $SDS layout (primary + mirror, sparse middle).
    let sds_primary_lcn = mft_bitmap_lcn + mft_bitmap_clusters;
    let sds_mirror_lcn = sds_primary_lcn + 1;
    let sds_real_clusters: u64 = 2;

    let mftmirr_lcn = cluster_count / 2;
    // Mirror records 0..3 (4 records). Round up in case record_size > cluster_size.
    let mftmirr_clusters: u64 = (4 * mft_record_size as u64).div_ceil(cluster_size as u64);

    let backup_boot_lcn = cluster_count - 1;

    let last_used_lcn = sds_mirror_lcn + 1;
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
    // Backup boot lives at the LAST 512-byte sector of the volume —
    // byte offset (cluster_count * cluster_size) - bytes_per_sector
    // — matching what publicly documented NTFS layout descriptions
    // say ntfs.sys probes via BPB.NumberSectors. Was previously
    // written at start-of-last-cluster; that was 7 sectors too early
    // for the 4 KiB / 512 default and triggered Event ID 55
    // ("corruption discovered, exact nature unknown") on
    // mac-format-tiny-32mib (diag iter-20260502-054124). Moving it to
    // the last sector cleared Event 55 on tiny (iter-20260502-061249).
    let volume_bytes = cluster_count * cluster_size as u64;
    let backup_boot_byte_offset = volume_bytes - bytes_per_sector as u64;
    dev.write_all_at(backup_boot_byte_offset, &boot)?;

    // 2. $LogFile — write 12 KiB of canonical RSTR + RCRD pages
    // captured byte-for-byte from a Microsoft `format.com` reference
    // run (diag run-20260502-154836). Pages 0/1 are paired log-restart
    // pages (RSTR magic, USA-protected, declaring "log clean / no
    // replay"); page 2 is a single sentinel RCRD record. The remaining
    // 1 MiB - 12 KiB is filled with 0xFF, matching what `format.com`
    // produces on a fresh volume.
    //
    // ntfs.sys reads the restart pages at mount, picks the one with
    // the higher current_lsn, validates the client array, and decides
    // whether the volume needs replay. Without these pages the read
    // fails internally and surfaces as Event ID 55 ("corruption
    // discovered, exact nature unknown") plus ERROR_NO_SYSTEM_RESOURCES
    // on every subsequent write.
    let log_size_bytes = logfile_clusters * cluster_size as u64;
    let logfile_off = logfile_lcn * cluster_size as u64;
    dev.write_all_at(logfile_off, LOGFILE_CANONICAL)?;
    let pad_off = logfile_off + LOGFILE_CANONICAL.len() as u64;
    let pad_len = log_size_bytes - LOGFILE_CANONICAL.len() as u64;
    write_filled(dev, pad_off, pad_len, 0xFF)?;

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
    // $Boot's MFT record claims a 2-cluster $DATA (covering 8192
    // bytes of boot region per Microsoft convention) at LCN 0..1.
    // Both clusters must be marked allocated so chkdsk doesn't
    // report `Found 0x1 clusters allocated to file "$Boot" at
    // offset "0x1" marked as free` (smoke diag run-20260502-174506).
    let boot_clusters_bitmap: u64 = 8192u64.div_ceil(cluster_size as u64);
    allocate(0, boot_clusters_bitmap)?;
    allocate(mft_lcn, mft_clusters)?;
    allocate(logfile_lcn, logfile_clusters)?;
    allocate(bitmap_lcn, bitmap_clusters)?;
    allocate(upcase_lcn, upcase_clusters)?;
    allocate(attrdef_lcn, attrdef_clusters)?;
    allocate(mft_bitmap_lcn, mft_bitmap_clusters)?;
    allocate(sds_primary_lcn, sds_real_clusters)?;
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

    // 4b. $Secure:$SDS data ----------------------------------------------
    //
    // Sub-PR S2: one canonical SD entry (`security_id = 0x100`, hash
    // computed via `sds::sdh_hash`) at file offset 0, mirrored
    // byte-identical at file offset 0x40000 (256 KiB) per the public
    // NTFS layout. Two real clusters back the stream — primary at
    // `sds_primary_lcn`, mirror at `sds_mirror_lcn` — with a sparse
    // run in the middle representing the 256 KiB gap.
    let canonical_sd_entry = crate::sds::SdEntry {
        security_id: 0x100,
        sd: SD_SYSFILE_RW,
    };
    let sds_stream = crate::sds::build_sds(&[canonical_sd_entry]);
    let sds_primary_off = sds_primary_lcn * cluster_size as u64;
    let primary_bytes_in_cluster = (cluster_size as usize).min(sds_stream.len());
    let mut primary_cluster = vec![0u8; cluster_size as usize];
    primary_cluster[..primary_bytes_in_cluster]
        .copy_from_slice(&sds_stream[..primary_bytes_in_cluster]);
    dev.write_all_at(sds_primary_off, &primary_cluster)?;

    let mirror_off_in_stream = crate::sds::SDS_MIRROR_GAP as usize;
    let mut mirror_cluster = vec![0u8; cluster_size as usize];
    let mirror_payload_len = sds_stream
        .len()
        .saturating_sub(mirror_off_in_stream)
        .min(cluster_size as usize);
    mirror_cluster[..mirror_payload_len].copy_from_slice(
        &sds_stream[mirror_off_in_stream..mirror_off_in_stream + mirror_payload_len],
    );
    dev.write_all_at(sds_mirror_lcn * cluster_size as u64, &mirror_cluster)?;

    // 5. MFT records ------------------------------------------------------
    let rs = mft_record_size as usize;
    let bps = bytes_per_sector;

    let mft_record_layout = MftLayout {
        record_size: rs,
        bytes_per_sector: bps,
        nt_time: now,
    };

    let mut mft_buf = vec![0u8; (mft_clusters * cluster_size as u64) as usize];

    // Collected during rec 0..11 building so root's $I30 can carry an
    // INDEX_ENTRY per system file. Microsoft's reference root $I30
    // contains all 12 system files (incl. `.` itself); chkdsk Stage 2
    // reports them as orphaned ("Detected orphaned file $X (N), should
    // be recovered into directory file 5") when the index is empty.
    // Per-record byte-diff in iter13 (rust-fs-ntfs-diag iter-20260502-024032).
    // Tuple: (rec_num, name, is_dir, data_alloc, data_real).
    let mut sys_entries: Vec<(u32, &'static str, bool, u64, u64)> = Vec::with_capacity(12);

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
        // Slots 0..10: canonical system files; 11: $Extend; 12..15:
        // reserved placeholders; 16..18: $Extend children ($ObjId,
        // $Reparse, $Quota). Slots 19+ left free.
        let mut allocated: Vec<u32> = (0u32..=10u32).collect();
        for slot in 11..mft_records_capacity.min(16) as u32 {
            allocated.push(slot);
        }
        for slot in [rec::OBJID, rec::REPARSE, rec::QUOTA] {
            if (slot as u64) < mft_records_capacity {
                allocated.push(slot);
            }
        }
        let mft_bitmap_value = make_mft_internal_bitmap(bitmap_value_size, &allocated);
        // Reference's $MFT $BITMAP is non-resident: 1 cluster allocated,
        // first N bytes carry the bitmap, rest is zero-padded. Match
        // that shape — see the mft_bitmap_lcn comment up top for why.
        let mft_bitmap_cluster_off = mft_bitmap_lcn * cluster_size as u64;
        let mut bitmap_cluster = vec![0u8; cluster_size as usize];
        bitmap_cluster[..bitmap_value_size].copy_from_slice(&mft_bitmap_value);
        dev.write_all_at(mft_bitmap_cluster_off, &bitmap_cluster)?;
        let bitmap_runs = vec![DataRun {
            starting_vcn: 0,
            length: mft_bitmap_clusters,
            lcn: Some(mft_bitmap_lcn),
        }];
        let bitmap_mp = encode_runs(&bitmap_runs)?;
        let bitmap_attr = build_nonresident_attribute(
            0xB0,
            None,
            4,
            bitmap_value_size as u64,
            mft_bitmap_clusters * cluster_size as u64,
            bitmap_value_size as u64,
            (mft_bitmap_clusters as i64) - 1,
            &bitmap_mp,
        )?;
        // $MFT $FILE_NAME tracks the MFT's $DATA size — the bytes
        // backing the MFT as a file. mft_clusters * cluster_size is
        // exactly what build_nonresident_data_attribute uses above.
        let mft_data_size = mft_clusters * cluster_size as u64;
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::MFT,
            rec::name(rec::MFT, cluster_size).expect("known rec_num"),
            false,
            mft_data_size,
            mft_data_size,
            &[data_attr, bitmap_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::MFT, rec_bytes)?;
        sys_entries.push((
            rec::MFT,
            rec::name(rec::MFT, cluster_size).expect("known rec_num"),
            false,
            mft_data_size,
            mft_data_size,
        ));
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
            rec::name(rec::MFTMIRR, cluster_size).expect("known rec_num"),
            false,
            len_bytes,
            len_bytes,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::MFTMIRR, rec_bytes)?;
        sys_entries.push((
            rec::MFTMIRR,
            rec::name(rec::MFTMIRR, cluster_size).expect("known rec_num"),
            false,
            len_bytes,
            len_bytes,
        ));
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
            rec::name(rec::LOGFILE, cluster_size).expect("known rec_num"),
            false,
            len_bytes,
            len_bytes,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::LOGFILE, rec_bytes)?;
        sys_entries.push((
            rec::LOGFILE,
            rec::name(rec::LOGFILE, cluster_size).expect("known rec_num"),
            false,
            len_bytes,
            len_bytes,
        ));
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

        // $VOLUME_INFORMATION: reserved(8) + major(1) + minor(1) + flags(2)
        //
        // version 3.1, flags 0x0080 (MODIFIED_BY_CHKDSK).
        //
        // 3.1 matches Windows fresh-format output (S3.1 upgrade
        // 2026-05-24: all system records now carry the v3.x 72-byte
        // $STD_INFO with SecurityId, so 3.1 is self-consistent).
        // Earlier iterations used 1.2 to avoid a "v3.1 + v1.x STD_INFO"
        // mismatch that ntfs.sys flagged as Critical Event 55; that
        // mismatch is now resolved.
        //
        // 0x0080 (MODIFIED_BY_CHKDSK) prevents ntfs.sys from queuing
        // a proactive scan on every mount (Iter M-3 trace confirmed
        // 0x0000 causes chkdsk /scan exit 13 even with clean metadata).
        // 0x0004 (UPGRADE_ON_MOUNT) is intentionally absent: on a
        // /scan snapshot mount the upgrade can't complete (read-only
        // snapshot), causing a Critical Event 55 (Iter M-2 trace).
        let mut vi = vec![0u8; 12];
        vi[8] = 3;
        vi[9] = 1;
        vi[10..12].copy_from_slice(&0x0080u16.to_le_bytes());
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
            rec::name(rec::VOLUME, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
            &combined,
        )?;
        place_record(&mut mft_buf, rs, rec::VOLUME, rec_bytes)?;
        sys_entries.push((
            rec::VOLUME,
            rec::name(rec::VOLUME, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
        ));
    }

    // record 4: $AttrDef (canonical NTFS 3.1, 2560 bytes / 16 entries)
    {
        let attrdef_blob = build_attrdef_table();
        // Sanity: the layout-planning attrdef_clusters must be at
        // least as big as the actual blob; if build_attrdef_table is
        // ever extended past 1 cluster, planning needs to know.
        debug_assert!(
            attrdef_blob.len() as u64 <= attrdef_clusters * cluster_size as u64,
            "attrdef_clusters too small for blob of {} bytes",
            attrdef_blob.len()
        );
        // Write attrdef bytes (zero-pad to cluster boundary).
        let mut padded = attrdef_blob.clone();
        let pad_to = (attrdef_clusters * cluster_size as u64) as usize;
        padded.resize(pad_to, 0);
        dev.write_all_at(attrdef_lcn * cluster_size as u64, &padded)?;

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
            rec::name(rec::ATTRDEF, cluster_size).expect("known rec_num"),
            false,
            attrdef_clusters * cluster_size as u64,
            attrdef_blob.len() as u64,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::ATTRDEF, rec_bytes)?;
        sys_entries.push((
            rec::ATTRDEF,
            rec::name(rec::ATTRDEF, cluster_size).expect("known rec_num"),
            false,
            attrdef_clusters * cluster_size as u64,
            attrdef_blob.len() as u64,
        ));
    }

    // record 5: root directory "." — built last so $I30 can include
    // INDEX_ENTRY for every system file (rec 0..11). See block at end of
    // this function.

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
            rec::name(rec::BITMAP, cluster_size).expect("known rec_num"),
            false,
            bitmap_clusters * cluster_size as u64,
            value_len,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::BITMAP, rec_bytes)?;
        sys_entries.push((
            rec::BITMAP,
            rec::name(rec::BITMAP, cluster_size).expect("known rec_num"),
            false,
            bitmap_clusters * cluster_size as u64,
            value_len,
        ));
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
            rec::name(rec::BOOT, cluster_size).expect("known rec_num"),
            false,
            boot_clusters * cluster_size as u64,
            boot_value_len,
            &[data_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::BOOT, rec_bytes)?;
        sys_entries.push((
            rec::BOOT,
            rec::name(rec::BOOT, cluster_size).expect("known rec_num"),
            false,
            boot_clusters * cluster_size as u64,
            boot_value_len,
        ));
    }

    // record 8: $BadClus — empty unnamed $DATA + named "$Bad" sparse
    // covering the whole volume (no clusters allocated; just
    // bookkeeping).
    {
        let empty_data = build_resident_unnamed(ATTR_DATA, 3, &[]);

        // Named $Bad: sparse run covering the *data* portion of the
        // volume — all clusters EXCEPT the last (which holds the
        // backup boot sector and is excluded by NTFS convention).
        // Microsoft `format.com` uses length = cluster_count - 1
        // (matrix run-20260503-072644 reference: 94191 clusters in
        // bitmap, $Bad covers 94191; ours pre-fix covered 65536 on a
        // 65535-cluster volume, off by one). The mismatch caused
        // chkdsk /scan to flag the volume needing offline /F.
        let bad_clusters = cluster_count - 1;
        let bad_runs = vec![DataRun {
            starting_vcn: 0,
            length: bad_clusters,
            lcn: None,
        }];
        let bad_mp = encode_runs(&bad_runs)?;
        let bad_attr = build_nonresident_attribute(
            ATTR_DATA,
            Some(stream::BAD),
            4,
            bad_clusters * cluster_size as u64,
            bad_clusters * cluster_size as u64,
            0,
            (bad_clusters as i64) - 1,
            &bad_mp,
        )?;
        // $BadClus's unnamed $DATA is empty (resident, zero bytes); the
        // sparse named $Bad attribute is what carries the cluster
        // bookkeeping. $FILE_NAME tracks the unnamed $DATA, which is
        // empty — sizes are 0/0. (Microsoft's reference matches this.)
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::BADCLUS,
            rec::name(rec::BADCLUS, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
            &[empty_data, bad_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::BADCLUS, rec_bytes)?;
        sys_entries.push((
            rec::BADCLUS,
            rec::name(rec::BADCLUS, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
        ));
    }

    // record 9: $Secure — minimal resident stub. Real NTFS has $SDS /
    // $SDH / $SII; for v1 we ship empty placeholders. chkdsk treats
    // an empty $Secure as "no security descriptor cache" and tolerates
    // it — the per-file SD pointer in $STANDARD_INFORMATION is what
    // governs ACL semantics, and we set it to 0 (default DACL).
    {
        // Microsoft modern format.com names slot 9 `$Quota` (NTFS 3.x
        // convention: $Quota is the slot-9 system file; $Secure lives
        // under \$Extend on the volume). chkdsk validates this name
        // explicitly at non-4K cluster sizes and reports
        // `Deleting invalid system file name $Secure (9) in directory 5.
        //  Repairing invalid system file name $Quota (9)`
        // when the slot carries the legacy NTFS 1.x name (matrix
        // run-20260503-024058 cluster-8k / cluster-64k).
        //
        // The IS_VIEW_INDEX flag applies to both $Quota and $Secure
        // — both host named view indexes in modern NTFS — so keep
        // the flag-setting branch in build_system_record unchanged.
        //
        // Sub-PR S1 (see docs/implementation-plan-secure-and-extend.md
        // §"Sub-PR S1") adds the three named streams chkdsk's Iter H
        // Procmon trace shows it opens on `$Secure`:
        //   * named `$DATA`        "$SDS" — empty (S2 populates).
        //   * named `$INDEX_ROOT`  "$SDH" — empty (header + END
        //     sentinel only).
        //   * named `$INDEX_ROOT`  "$SII" — empty (header + END
        //     sentinel only).
        // chkdsk's open of `\Device\…\$Secure:$SDS` failed with
        // STATUS_OBJECT_PATH_NOT_FOUND on the previous (empty-stub)
        // layout; S1 makes those three opens succeed. Index-block
        // size is 4 KiB (the same value the populated `$I30` uses).
        let empty_data = build_resident_unnamed(ATTR_DATA, 3, &[]);
        let sdh_name = stream::utf16(stream::SDH);
        let sii_name = stream::utf16(stream::SII);

        // Sub-PR S2: $SDS becomes non-resident with one canonical SD
        // entry at offset 0 and its mirror at file offset 0x40000.
        // Two real clusters back the stream — primary at
        // sds_primary_lcn, mirror at sds_mirror_lcn — separated by a
        // sparse run that covers the 256 KiB gap exactly. The
        // file-logical layout is:
        //   VCN 0           — primary cluster (LCN = sds_primary_lcn)
        //   VCN 1..gap_vcn  — sparse hole
        //   VCN gap_vcn     — mirror cluster (LCN = sds_mirror_lcn)
        //
        // gap_vcn = 0x40000 / cluster_size, so the mirror cluster
        // starts exactly at file offset 0x40000 regardless of
        // cluster size.
        let gap_vcn: u64 = crate::sds::SDS_MIRROR_GAP / cluster_size as u64;
        debug_assert!(gap_vcn >= 1, "256 KiB mirror gap must span >= 1 cluster");
        let sds_runs = vec![
            DataRun {
                starting_vcn: 0,
                length: 1,
                lcn: Some(sds_primary_lcn),
            },
            DataRun {
                starting_vcn: 1,
                length: gap_vcn - 1,
                lcn: None,
            },
            DataRun {
                starting_vcn: gap_vcn,
                length: 1,
                lcn: Some(sds_mirror_lcn),
            },
        ];
        let sds_mp = encode_runs(&sds_runs)?;
        // data_length: bytes from offset 0 through end of mirror
        // entry. With a single 72-byte SD payload, each entry
        // (header+SD) is 92 bytes padded to 96. Mirror starts at
        // 0x40000; mirror entry occupies 0x40000..0x40060 (data
        // bytes) — and we round to the 16-byte boundary, ending at
        // 0x40060. allocated_length is the on-stream allocated size
        // ((gap_vcn + 1) clusters * cluster_size).
        let sds_data_len = crate::sds::SDS_MIRROR_GAP
            + (crate::sds::SDS_HEADER_LEN as u64)
            + SD_SYSFILE_RW.len() as u64;
        let sds_alloc_len = (gap_vcn + 1) * cluster_size as u64;
        let sds_last_vcn = gap_vcn as i64;
        let sds_data = build_nonresident_attribute(
            ATTR_DATA,
            Some(stream::SDS),
            4,
            sds_data_len,
            sds_alloc_len,
            sds_data_len,
            sds_last_vcn,
            &sds_mp,
        )?;

        // $SDH: one entry. Key (8 bytes) = (hash, security_id);
        // value (20 bytes) = (hash, security_id, sds_offset, sds_size).
        let sds_size_u32 = crate::sds::SDS_HEADER_LEN + SD_SYSFILE_RW.len() as u32;
        let sd_hash = crate::sds::sdh_hash(SD_SYSFILE_RW);
        let security_id: u32 = 0x100;
        let sds_offset: u64 = 0;

        let mut sdh_key = [0u8; 8];
        sdh_key[0..4].copy_from_slice(&sd_hash.to_le_bytes());
        sdh_key[4..8].copy_from_slice(&security_id.to_le_bytes());
        let mut sdh_value = [0u8; 20];
        sdh_value[0..4].copy_from_slice(&sd_hash.to_le_bytes());
        sdh_value[4..8].copy_from_slice(&security_id.to_le_bytes());
        sdh_value[8..16].copy_from_slice(&sds_offset.to_le_bytes());
        sdh_value[16..20].copy_from_slice(&sds_size_u32.to_le_bytes());
        let mut sdh_entries = build_view_index_entry(&sdh_key, &sdh_value);
        sdh_entries.extend_from_slice(&build_view_index_last_entry());
        let sdh_index_root = build_populated_named_index_root_attr(
            5,
            &sdh_name,
            0,
            COLLATION_NTOFS_SECURITY_HASH,
            4096,
            cluster_size,
            &sdh_entries,
        );

        // $SII: one entry. Key (4 bytes) = security_id;
        // value (20 bytes) = same as $SDH's value.
        let sii_key = security_id.to_le_bytes();
        let mut sii_entries = build_view_index_entry(&sii_key, &sdh_value);
        sii_entries.extend_from_slice(&build_view_index_last_entry());
        let sii_index_root = build_populated_named_index_root_attr(
            6,
            &sii_name,
            0,
            COLLATION_NTOFS_ULONG,
            4096,
            cluster_size,
            &sii_entries,
        );

        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::SECURE,
            rec::name(rec::SECURE, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
            &[empty_data, sds_data, sdh_index_root, sii_index_root],
        )?;
        place_record(&mut mft_buf, rs, rec::SECURE, rec_bytes)?;
        sys_entries.push((
            rec::SECURE,
            rec::name(rec::SECURE, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
        ));
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

        // Iter M-2 2026-05-22: $UpCase:$Info named resident $DATA
        // stream (32 bytes). A freshly-formatted Windows volume
        // (`windows-fresh-256m.bin`, chkdsk exit 0 confirmed) carries
        // this stream on rec 10:
        //   +0x00 u32  total length of this stream = 0x20
        //   +0x04 u32  reserved (0)
        //   +0x08 u64  CRC64 of the $UpCase table content
        //   +0x10 u8[16] reserved (zero)
        // Our $UpCase content is byte-identical to Windows' canonical
        // 128 KiB table (verified md5 match Iter M-2), so the same hash
        // (0xDADC_7E77_6B1B_690C, little-endian on disk) applies. The
        // absence of `$Info` is one of the structural divergences
        // ntfs.sys's /scan-only checks flag as "exact nature unknown"
        // — Event 55 raised at every mount.
        let mut info_value = vec![0u8; 32];
        info_value[0..4].copy_from_slice(&0x20u32.to_le_bytes());
        info_value[8..16].copy_from_slice(&0xDADC_7E77_6B1B_690Cu64.to_le_bytes());
        let info_name = stream::utf16(stream::INFO);
        let info_attr = build_resident_named(ATTR_DATA, 4, &info_name, &info_value);

        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::UPCASE,
            rec::name(rec::UPCASE, cluster_size).expect("known rec_num"),
            false,
            upcase_clusters * cluster_size as u64,
            upcase_value_bytes,
            &[data_attr, info_attr],
        )?;
        place_record(&mut mft_buf, rs, rec::UPCASE, rec_bytes)?;
        sys_entries.push((
            rec::UPCASE,
            rec::name(rec::UPCASE, cluster_size).expect("known rec_num"),
            false,
            upcase_clusters * cluster_size as u64,
            upcase_value_bytes,
        ));
    }

    // record 11: $Extend — directory whose `$I30` enumerates three
    // $Extend children: $ObjId (slot 16), $Reparse (slot 17), $Quota
    // (slot 18). scan-f-scan experiments (2026-05-24) proved that
    // chkdsk /scan exits 13 unless all three are present under $Extend
    // at format time. $RmMetadata is intentionally omitted (shipping it
    // incompletely causes TxF resource manager failures; see rec module
    // comment).
    //
    // $Extend: $STD_INFO + $FILE_NAME + $I30 with entries for the
    // three children built immediately below.
    {
        let index_block_size: u32 = 4096;
        let extend_ref = encode_file_reference(rec::EXTEND as u64, rec::EXTEND as u16);
        let mut extend_children: Vec<(u32, &str)> = vec![
            (
                rec::OBJID,
                rec::name(rec::OBJID, cluster_size).expect("known"),
            ),
            (
                rec::REPARSE,
                rec::name(rec::REPARSE, cluster_size).expect("known"),
            ),
            (
                rec::QUOTA,
                rec::name(rec::QUOTA, cluster_size).expect("known"),
            ),
        ];
        extend_children.sort_by(|a, b| collate_file_name(a.1, b.1));

        let mut entries_blob: Vec<u8> = Vec::new();
        for &(child_rec, child_name) in &extend_children {
            let child_seq: u16 = child_rec as u16;
            let child_ref = encode_file_reference(child_rec as u64, child_seq);
            let stream = build_skeleton_fn_stream(extend_ref, child_name)?;
            entries_blob.extend_from_slice(&build_index_entry(child_ref, &stream, false));
        }
        entries_blob.extend_from_slice(&build_index_entry(0, &[], true));
        let index_root =
            build_populated_index_root_attr(3, index_block_size, cluster_size, &entries_blob);
        let rec_bytes = build_system_record(
            &mft_record_layout,
            rec::EXTEND,
            rec::name(rec::EXTEND, cluster_size).expect("known rec_num"),
            true,
            0,
            0,
            &[index_root],
        )?;
        place_record(&mut mft_buf, rs, rec::EXTEND, rec_bytes)?;
        sys_entries.push((
            rec::EXTEND,
            rec::name(rec::EXTEND, cluster_size).expect("known rec_num"),
            true,
            0,
            0,
        ));
    }

    // $Extend children — $ObjId (VIEW_INDEX), $Reparse (VIEW_INDEX),
    // $RmMetadata (DIRECTORY). Slot ordering matches chkdsk /f output
    // (2026-05-24 scan-f-scan experiments). These three are required for
    // chkdsk /scan to exit 0; without them /scan exits 13 and queues
    // offline repairs.
    {
        let index_block_size: u32 = 4096;
        let last_entry = build_view_index_last_entry();

        // $ObjId: empty $O view index (COLLATION_NTOFS_ULONGS = 0x13)
        let o_name = stream::utf16(stream::O);
        let o_index_root = build_populated_named_index_root_attr(
            3,
            &o_name,
            0,
            COLLATION_NTOFS_ULONGS,
            index_block_size,
            cluster_size,
            &last_entry,
        );
        let objid_rec = build_system_record_with_parent(
            &mft_record_layout,
            rec::OBJID,
            rec::EXTEND,
            rec::name(rec::OBJID, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
            &[o_index_root],
        )?;
        place_record(&mut mft_buf, rs, rec::OBJID, objid_rec)?;

        // $Reparse: empty $R view index (COLLATION_NTOFS_ULONGS = 0x13).
        // A resident-only $INDEX_ROOT is sufficient; chkdsk accepts this
        // layout when the MFT record flags are 0x000D (IN_USE|0x04|VIEW_INDEX).
        let r_name = stream::utf16(stream::R);
        let r_index_root = build_populated_named_index_root_attr(
            3,
            &r_name,
            0,
            COLLATION_NTOFS_ULONGS,
            index_block_size,
            cluster_size,
            &last_entry,
        );
        let reparse_rec = build_system_record_with_parent(
            &mft_record_layout,
            rec::REPARSE,
            rec::EXTEND,
            rec::name(rec::REPARSE, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
            &[r_index_root],
        )?;
        place_record(&mut mft_buf, rs, rec::REPARSE, reparse_rec)?;

        // $Quota: $O (SID→owner_id) and $Q (owner_id→quota_info) view indexes.
        // Shipping $Quota preemptively prevents the Windows NTFS driver from
        // creating it on first mount, which for sub-4K cluster sizes results in
        // an incomplete record (missing $O/$Q initialization) that chkdsk flags.
        //
        // $Q default record (OwnerID=1) strategy:
        //   cluster_size >= 4096 (cpib=1): Windows creates OwnerID=1 correctly on
        //     first mount. Emitting it ourselves triggers a 180-second quota-rebuild
        //     scan on every subsequent disk-online cycle (Windows sees $O empty and
        //     rebuilds OwnerID→SID mappings by walking the entire MFT). Leave $Q
        //     empty so Windows initialises it cleanly on first mount.
        //   cluster_size < 4096 (cpib>1): Windows NTFS driver fails to create the
        //     default quota record for sub-4K cluster volumes (observed empirically:
        //     chkdsk /scan exits 1 with "default quota record missing in file 18"
        //     unless OwnerID=1 is present at format time). Emit it here.
        let q_name = stream::utf16(stream::Q);
        let q_entries: Vec<u8> = if cluster_size < 4096 {
            // Sub-4K: pre-populate OwnerID=1 so chkdsk /scan does not report
            // "default quota record missing in file 18."
            // QUOTA_CONTROL v2, FLAG_ID_ALLOCATED=1, zero usage, -1 thresholds.
            let default_quota_key = 1u32.to_le_bytes(); // OwnerID = 1
            let mut e = build_view_index_entry(&default_quota_key, &build_default_quota_control());
            e.extend_from_slice(&last_entry);
            e
        } else {
            // 4K+: leave $Q empty; Windows populates OwnerID=1 on first mount
            // without triggering the MFT-wide quota rebuild that an empty $O causes.
            last_entry.to_vec()
        };
        let q_index_root = build_populated_named_index_root_attr(
            3,
            &q_name,
            0,
            COLLATION_NTOFS_ULONG,
            index_block_size,
            cluster_size,
            &q_entries,
        );
        let o_quota_name = stream::utf16(stream::O);
        let o_quota_index_root = build_populated_named_index_root_attr(
            4,
            &o_quota_name,
            0,
            COLLATION_NTOFS_SID,
            index_block_size,
            cluster_size,
            &last_entry,
        );
        let quota_rec = build_system_record_with_parent(
            &mft_record_layout,
            rec::QUOTA,
            rec::EXTEND,
            rec::name(rec::QUOTA, cluster_size).expect("known rec_num"),
            false,
            0,
            0,
            &[o_quota_index_root, q_index_root],
        )?;
        place_record(&mut mft_buf, rs, rec::QUOTA, quota_rec)?;
    }

    // record 5: root directory "." — built AFTER rec 0..4 and rec 6..11
    // so we have every system file's name + $DATA size to emit as
    // INDEX_ENTRYs in `$I30`. Microsoft's reference root contains
    // entries for all 12 system files (incl. `.` itself); chkdsk Stage 2
    // walks `$I30` and reports every record absent from it as orphaned.
    // Per-record byte-diff in iter13 (rust-fs-ntfs-diag iter-20260502-024032)
    // confirmed reference $I30 = 0x468 bytes (12 entries + LAST sentinel)
    // vs ours = 0x30 bytes (just the LAST sentinel).
    {
        let index_block_size: u32 = 4096;
        sys_entries.push((
            rec::ROOT,
            rec::name(rec::ROOT, cluster_size).expect("known rec_num"),
            true,
            0,
            0,
        ));
        sys_entries.sort_by(|a, b| collate_file_name(a.1, b.1));

        // Every system $FILE_NAME's parent is the root directory at
        // (rec=5, seq=5). Same convention used by `build_system_record`.
        let parent_ref = encode_file_reference(rec::ROOT as u64, 5);

        let mut entries_blob: Vec<u8> = Vec::new();
        for &(rec_num, name, is_dir, alloc, real) in &sys_entries {
            // sequence_number = max(1, rec_num) per iter11 byte-diff.
            let seq: u16 = if rec_num == 0 { 1 } else { rec_num as u16 };
            // Microsoft's reference root $I30 ships skeleton streams
            // (parent_ref + name only; timestamps + sizes + fa zeroed)
            // for every system entry EXCEPT $MFT, which keeps its
            // populated values. Byte-corroboration:
            // run-20260503-011545/mac-format-label-empty/reference-mft-16recs.bin
            // — entries 0..4,6..10 carry zeros at offsets 0x08..0x40
            // of the stream; entry 5 ($MFT) keeps the populated form.
            // Diverging from this triggers Event ID 55 "$I30:$INDEX_ROOT
            // corrupt" on rec 5 (chkdsk /scan exit 13).
            let stream = if rec_num == rec::MFT {
                build_file_name_stream(
                    parent_ref,
                    mft_record_layout.nt_time,
                    name,
                    is_dir,
                    true,
                    alloc,
                    real,
                    NAMESPACE_WIN32_DOS,
                )?
            } else {
                build_skeleton_fn_stream(parent_ref, name)?
            };
            let entry =
                build_index_entry(encode_file_reference(rec_num as u64, seq), &stream, false);
            entries_blob.extend_from_slice(&entry);
        }
        // LAST sentinel terminates the inline index.
        entries_blob.extend_from_slice(&build_index_entry(0, &[], true));

        let index_root =
            build_populated_index_root_attr(3, index_block_size, cluster_size, &entries_blob);
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

    // 12..15 reserved-slot placeholders. Slot 11 is now $Extend (Sub-PR
    // S3); slots 12..15 remain as 304-byte placeholders matching
    // Microsoft format.com's first-64-MFT-records reference layout.
    for slot in 12..mft_records_capacity.min(16) as u32 {
        let rec_bytes = build_reserved_placeholder(&mft_record_layout, slot)?;
        place_record(&mut mft_buf, rs, slot, rec_bytes)?;
    }

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

    // BPB NumberSectors at offset 0x28: count of *data* sectors in the
    // volume, NOT counting the trailing backup-boot sector. Microsoft
    // format.com always writes (volume_sectors - 1). Corroborated in
    // iter13 (agent-840e-2026-05-02): a 96 MiB reference volume showed
    // NumberSectors=0x0002FEFF for 196352 partition sectors, i.e. N-1.
    // We previously wrote N (the full sector count); at >= 256 MiB the
    // off-by-one was tolerated by chkdsk + ntfs.sys, but at 32 MiB it
    // pushes the kernel's expected backup-boot location past EOV and
    // ntfs.sys refuses to mount the volume (Get-Volume reports
    // FileSystemType=Unknown, Size=0).
    let volume_sectors: u64 = cluster_count * (cluster_size as u64) / bytes_per_sector as u64;
    let number_sectors: u64 = volume_sectors - 1;
    b[0x28..0x30].copy_from_slice(&number_sectors.to_le_bytes());
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

    // Bootstrap area: minimal halt loop (CLI; JMP $-1) so a stray
    // BIOS boot from this volume doesn't run garbage. The volume is
    // mounted, never executed; ntfs.sys / chkdsk don't validate
    // bootstrap code.
    b[0x54] = 0xFA; // CLI
    b[0x55] = 0xEB; // JMP $-1
    b[0x56] = 0xFE;
    // 0x57..0x1FE stay zero.
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
    // Default parent for slots 0..11 is the root directory (rec 5,
    // seq 5). Records nested under another system directory use the
    // `_with_parent` variant. None ship at format time today; future
    // `$Extend` descendants would be the typical caller.
    build_system_record_with_parent(
        layout,
        record_number,
        rec::ROOT,
        name,
        is_dir,
        fn_data_alloc,
        fn_data_real,
        extra_attrs,
    )
}

fn build_system_record_with_parent(
    layout: &MftLayout,
    record_number: u32,
    parent_record: u32,
    name: &str,
    is_dir: bool,
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
    // MFT record header flags (publicly published NTFS layout):
    //   0x0001 MFT_RECORD_IN_USE
    //   0x0002 MFT_RECORD_IS_DIRECTORY (set when the record hosts an
    //          $I30 ($FILE_NAME) index — i.e. a normal directory)
    //   0x0004 (reserved / "is 4")
    //   0x0008 MFT_RECORD_IS_VIEW_INDEX (set when the record hosts a
    //          named view index — anything indexing something other
    //          than $FILE_NAME, e.g. $Secure's $SDH/$SII).
    //
    // chkdsk has hardcoded knowledge that record 9 is `$Secure` and
    // demands the VIEW_INDEX bit on its MFT header even when the
    // on-disk view-index attributes are absent (our v1 stub). CI
    // iter11 (chkdsk readonly diag dir 20260502-014556) reported
    // `Flags for file record segment 9 are incorrect` against an
    // otherwise-clean Stage 1; rec 9 was the only segment named.
    //
    // The Microsoft format.com reference can't corroborate this via
    // byte-diff — its rec 9 is `$Quota`, not `$Secure`, so the
    // comparable flag field is uninformative. Fix is keyed on the
    // public spec rather than a flag-byte diff.
    let is_view_index = matches!(
        record_number,
        rec::SECURE | rec::REPARSE | rec::OBJID | rec::QUOTA
    );
    // VIEW_INDEX records need both 0x0004 (MFT_RECORD_HAS_VIEW_INDEX, indicating
    // a named non-$I30 index root is present) and 0x0008 (MFT_RECORD_IS_VIEW_INDEX).
    // Post-/f byte-diff (2026-05-24): slots 16/17 show 0x000D after /f fixes them.
    let flags: u16 =
        0x0001 | if is_dir { 0x0002 } else { 0x0000 } | if is_view_index { 0x000C } else { 0x0000 };
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

    // Parent file-reference. For slots 0..11 the parent is the root
    // directory (rec 5, seq 5) — `.` parents to itself per NTFS
    // convention. Sub-PR S4 introduces nested system records (rec 16
    // `$Reparse` parented to rec 11 `$Extend`) where the parent's
    // sequence number must match the parent's own record header
    // (`max(1, rec_num)` per the rec_seq encoding above), otherwise
    // chkdsk reports "Incorrect information was detected in file
    // record segment N".
    let parent_seq: u16 = if parent_record == 0 {
        1
    } else {
        parent_record as u16
    };
    let parent_ref = encode_file_reference(parent_record as u64, parent_seq);

    // Every system record references the single canonical SD entry
    // shipped in `$Secure:$SDS` (security_id = 0x100).
    cursor = write_standard_information(
        &mut rec,
        cursor,
        0,
        layout.nt_time,
        is_dir,
        true,
        is_view_index,
        0x100,
    );
    cursor = write_file_name(
        &mut rec,
        cursor,
        1,
        parent_ref,
        name,
        layout.nt_time,
        is_dir,
        true,
        is_view_index,
        fn_data_alloc,
        fn_data_real,
        NAMESPACE_WIN32_DOS,
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

/// Build a FILE-magic placeholder for one of the reserved MFT slots
/// (11..15). `$STD_INFO + empty $DATA`, no `$FILE_NAME`. ntfs.sys
/// validates this layout at mount; raw-zero slots cause Event ID 55.
fn build_reserved_placeholder(layout: &MftLayout, record_number: u32) -> Result<Vec<u8>, String> {
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
    let attrs_offset = align8(USA_OFFSET + 2 + sectors * 2);
    rec[REC_OFF_LSN..REC_OFF_LSN + 8].copy_from_slice(&0u64.to_le_bytes());
    rec[REC_OFF_SEQ..REC_OFF_SEQ + 2].copy_from_slice(&(record_number as u16).to_le_bytes());
    // link_count = 0: these placeholders have no $FILE_NAME and are
    // therefore not linked from any directory. Microsoft's reference
    // sets link_count=0 here; ours previously set 1, which `chkdsk /F`
    // detected as inconsistent (post-/F dump shows link_count cleared
    // to 0 on slots 12-15) and which `chkdsk /scan` reports via
    // exit 13 ("found problems that must be fixed offline").
    // Diag: run-20260503-075833 reference rec 11-15 link_count=0
    // vs our pre-fix link_count=1.
    rec[REC_OFF_LINK_COUNT..REC_OFF_LINK_COUNT + 2].copy_from_slice(&0u16.to_le_bytes());
    rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
        .copy_from_slice(&(attrs_offset as u16).to_le_bytes());
    rec[REC_OFF_FLAGS..REC_OFF_FLAGS + 2].copy_from_slice(&0x0001u16.to_le_bytes()); // IN_USE
    rec[REC_OFF_BYTES_ALLOCATED..REC_OFF_BYTES_ALLOCATED + 4]
        .copy_from_slice(&(rs as u32).to_le_bytes());
    rec[REC_OFF_BASE_FILE_REF..REC_OFF_BASE_FILE_REF + 8].copy_from_slice(&0u64.to_le_bytes());
    rec[REC_OFF_NEXT_ATTR_ID..REC_OFF_NEXT_ATTR_ID + 2].copy_from_slice(&2u16.to_le_bytes());
    rec[REC_OFF_MFT_REC_NUM..REC_OFF_MFT_REC_NUM + 4].copy_from_slice(&record_number.to_le_bytes());
    rec[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());

    let mut cursor = attrs_offset;
    cursor = write_standard_information(
        &mut rec,
        cursor,
        0,
        layout.nt_time,
        false,
        true,
        false,
        0x100,
    );

    let data_attr = build_resident_unnamed(ATTR_DATA, 1, &[]);
    rec[cursor..cursor + data_attr.len()].copy_from_slice(&data_attr);
    cursor += data_attr.len();

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
    is_view_index: bool,
    security_id: u32,
) -> usize {
    // 72-byte NTFS 3.x $STANDARD_INFORMATION (MS-FSCC §2.4.2):
    //   4 timestamps (32) + DOSAttributes (4) + MaxVersions (4)
    //   + VersionNumber (4) + ClassId (4) + OwnerId (4) + SecurityId (4)
    //   + QuotaCharged (8) + USN (8).
    // All records use this form (S3.1 upgrade, 2026-05-24). Prior
    // iterations used the 48-byte v1.x form on system records to match
    // format.com's raw byte layout; ETW trace analysis revealed chkdsk
    // /scan validates SecurityId→$SDS linkage and exits 13 when
    // SecurityId=0 (the implicit value of the absent field in v1.x).
    // chkdsk /f confirms the correct fix: it upgrades every record to
    // the 72-byte form with SecurityId=0x100 and removes per-file $SD.
    let value_size = 72usize;
    let header_size = 24usize;
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
    // file_attributes: system records get FA_HIDDEN|FA_SYSTEM only, no
    // FA_ARCHIVE. Microsoft `format.com`'s reference per-record byte-diff
    // (agent-8934-2026-05-02 iter19, diag iter-20260502-072713) shows
    // file_attributes = FA_HIDDEN|FA_SYSTEM (0x06) on every system record;
    // ours emitted 0x26 (HIDDEN|SYSTEM|ARCHIVE) before this fix.
    // Regular files keep FA_ARCHIVE.
    let mut fa: u32 = if is_system {
        FA_HIDDEN | FA_SYSTEM // matches Microsoft format.com reference output
    } else {
        let mut f = FA_ARCHIVE;
        if is_dir {
            f |= FA_NTFS_DIRECTORY;
        }
        f
    };
    // VIEW_INDEX records carry an internal 0x20000000 bit in file_attributes
    // (both $STD_INFO and $FILE_NAME). Post-/f byte-diff (2026-05-24) shows
    // this bit present on $ObjId and $Reparse after chkdsk repairs them.
    if is_view_index {
        fa |= FA_NTFS_VIEW_INDEX;
    }
    rec[v + 32..v + 36].copy_from_slice(&fa.to_le_bytes());
    // SecurityId at value+0x34 (MS-FSCC §2.4.2). OwnerId (+0x30),
    // QuotaCharged (+0x38), USN (+0x40) stay zero (fresh-format defaults).
    rec[v + 0x34..v + 0x38].copy_from_slice(&security_id.to_le_bytes());
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
    is_view_index: bool,
    data_alloc: u64,
    data_real: u64,
    namespace: u8,
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
    // file_attributes: FA_HIDDEN|FA_SYSTEM for system records, FA_ARCHIVE
    // for regular files, plus FA_NTFS_DIRECTORY for any directory (including
    // the root, which is both system and a directory).
    //
    // Byte-corroboration (run-20260503-011545/mac-format-label-empty,
    // rec 5 in-record $FILE_NAME):
    //   reference bytes 0xE0..0xE3: 06 00 00 10  (= FA_NTFS_DIRECTORY|FA_HIDDEN|FA_SYSTEM)
    //   ours pre-fix:                06 00 00 00  (= FA_HIDDEN|FA_SYSTEM only)
    //
    // Without FA_NTFS_DIRECTORY on the root's in-record FN, ntfs.sys
    // reports `$I30:$INDEX_ROOT corrupt` against rec 5 (Event ID 55
    // with file reference 0x5000000000005). chkdsk readonly tolerates
    // it but /scan queues offline repair (exit 13).
    let mut fa: u32 = if is_system {
        FA_HIDDEN | FA_SYSTEM
    } else {
        FA_ARCHIVE
    };
    if is_dir {
        fa |= FA_NTFS_DIRECTORY;
    }
    if is_view_index {
        fa |= FA_NTFS_VIEW_INDEX;
    }
    rec[v + 56..v + 60].copy_from_slice(&fa.to_le_bytes());
    rec[v + 60..v + 64].copy_from_slice(&0u32.to_le_bytes());
    rec[v + 64] = utf16.len() as u8;
    rec[v + 65] = namespace;
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

/// Build a resident attribute with a name (e.g. a named `$DATA` stream
/// or a named view-index `$INDEX_ROOT`). `name_utf16` is the stream
/// name as UTF-16 code units (without a NUL terminator).
///
/// Header layout (MS-FSCC §2.4.2.1 / NtfsResidentAttributeHeader):
///   0x00 ty                  u32
///   0x04 length              u32
///   0x08 is_non_resident     u8   (0 for resident)
///   0x09 name_length         u8   (in UTF-16 code units, not bytes)
///   0x0A name_offset         u16
///   0x0C flags               u16  (0)
///   0x0E instance            u16  (attr_id)
///   0x10 value_length        u32
///   0x14 value_offset        u16
///   0x16 indexed_flag        u8   (0)
///   0x17 reserved            u8   (0)
///   [name @ name_offset, name_length * 2 bytes]
///   [value @ value_offset, value_length bytes]
///
/// All offsets are from the start of this attribute. The name is
/// placed immediately after the 24-byte header and the value is
/// placed after the name, both 8-aligned. The total attribute length
/// is 8-aligned.
fn build_resident_named(attr_type: u32, attr_id: u16, name_utf16: &[u16], value: &[u8]) -> Vec<u8> {
    let header_size = 24usize;
    let name_offset = header_size;
    let name_bytes = name_utf16.len() * 2;
    let value_offset = align8(name_offset + name_bytes);
    let attr_length = align8(value_offset + value.len());
    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&attr_type.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0;
    buf[9] = name_utf16.len() as u8;
    buf[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;
    for (i, c) in name_utf16.iter().enumerate() {
        let off = name_offset + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    if !value.is_empty() {
        buf[value_offset..value_offset + value.len()].copy_from_slice(value);
    }
    buf
}

/// Build a populated named `$INDEX_ROOT` view-index attribute. The
/// caller supplies a pre-built `entries_blob` whose final entry is
/// the LAST sentinel; this function wraps it in the standard
/// INDEX_ROOT framing (attribute header + index-root header + index
/// header + entries). Used at S2 for `$Secure:$SDH` and `$Secure:$SII`
/// which each carry exactly one data entry plus a LAST sentinel.
fn build_populated_named_index_root_attr(
    attr_id: u16,
    name_utf16: &[u16],
    indexed_attr_type: u32,
    collation: u32,
    index_block_size: u32,
    cluster_size: u32,
    entries_blob: &[u8],
) -> Vec<u8> {
    let header_size = 16usize + 8usize; // generic + resident
    let name_offset = header_size;
    let name_bytes = name_utf16.len() * 2;
    let value_offset = align8(name_offset + name_bytes);
    let ir_value_size = 16 + 16 + entries_blob.len(); // ir_hdr + idx_hdr + entries
    let attr_length = align8(value_offset + ir_value_size);

    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&ATTR_INDEX_ROOT.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0;
    buf[9] = name_utf16.len() as u8;
    buf[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(ir_value_size as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;

    for (i, c) in name_utf16.iter().enumerate() {
        let off = name_offset + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }

    let v = value_offset;
    buf[v..v + 4].copy_from_slice(&indexed_attr_type.to_le_bytes());
    buf[v + 4..v + 8].copy_from_slice(&collation.to_le_bytes());
    buf[v + 8..v + 12].copy_from_slice(&index_block_size.to_le_bytes());
    let cpib_byte: u8 = if cluster_size <= index_block_size {
        (index_block_size / cluster_size) as u8
    } else {
        (index_block_size / 512) as u8
    };
    buf[v + 12] = cpib_byte;

    let ih = v + 16;
    buf[ih..ih + 4].copy_from_slice(&16u32.to_le_bytes()); // first_entry_offset
    let used = 16u32 + entries_blob.len() as u32;
    buf[ih + 4..ih + 8].copy_from_slice(&used.to_le_bytes()); // total_size_of_entries
    buf[ih + 8..ih + 12].copy_from_slice(&used.to_le_bytes()); // allocated_size_of_entries
                                                               // flags byte at ih + 12 stays 0 (SMALL_INDEX / no children).

    let entries_at = ih + 16;
    buf[entries_at..entries_at + entries_blob.len()].copy_from_slice(entries_blob);

    buf
}

/// Build a single INDEX_ENTRY for a view-index (`$SDH` / `$SII` style).
///
/// INDEX_ENTRY's first 8 bytes are a union (per MS-FSCC §2.4.9 +
/// Windows Internals 7e ch. "NTFS On-Disk Structure"):
///
/// * **File-name indexes** (`$I30`) put a `FILE_REFERENCE` (u64)
///   there — the MFT ref the entry points at. The KEY embedded in
///   the entry body IS the data (`$FILE_NAME` stream), so no
///   separate data-offset/length is needed.
/// * **View indexes** (`$SDH`, `$SII`, `$O`, `$Q`, ...) put
///   `data_offset` (u16 LE) at `+0x00` and `data_length` (u16 LE)
///   at `+0x02`, with `+0x04..0x08` reserved. The value lives at
///   `entry_start + data_offset` and is **separate** from the key.
///
/// S2's first cut wrote zeros into `+0x00..0x08`, treating the union
/// as `file_reference = 0`. That's the file-name-index convention,
/// not the view-index one. chkdsk's view-index parser (per
/// Iter I trace: `Index $SDH in file 9 is corrupt`) reads
/// `data_offset = 0` from that, finds no value, and flags the entry.
///
/// Full layout this builder produces:
///
/// ```text
///   +0x00 data_offset   u16   = `value_off` (== key data ends, aligned to 8)
///   +0x02 data_length   u16   = `value.len()`
///   +0x04 reserved      u32   = 0
///   +0x08 entry_length  u16   = total bytes incl. padding
///   +0x0A key_length    u16
///   +0x0C flags         u32   = 0 (normal) or 0x02 (LAST / INDEX_ENTRY_END)
///   +0x10 key           key_length bytes
///   +pad  value         value.len() bytes (at data_offset)
/// ```
///
/// Returns the entry bytes (no extra padding outside `entry_length`).
///
/// **Iter K finding (chkdsk byte-diff against `Format-Volume`)**: the
/// value lives **immediately after the key**, with no alignment padding
/// in between. Only `entry_length` is rounded to 8 bytes. An earlier
/// cut of this builder `align8`'d `value_off` too — for 8-byte keys
/// that was a no-op (`$SDH`), but for 4-byte keys (`$SII`) it inserted
/// 4 zero bytes between key and value, producing `data_offset = 0x18,
/// entry_length = 0x30` instead of the reference's `0x14 / 0x28`.
/// chkdsk's view-index parser flagged `Index $SII in file 9 is
/// corrupt` on the resulting bytes. Removing the `value_off`
/// alignment matches the reference byte-for-byte across 8 sampled
/// entries from a fresh NTFS v3.1 `Format-Volume` dump.
fn build_view_index_entry(key: &[u8], value: &[u8]) -> Vec<u8> {
    let header = 0x10usize;
    let key_len = key.len();
    let value_off = header + key_len;
    let entry_len = align8(value_off + value.len());
    let mut buf = vec![0u8; entry_len];
    // View-index union: data_offset + data_length, NOT file_reference.
    // u16 narrowing must fail loudly if a future caller's
    // `value_off` or `value.len()` ever exceeds 65535 — those fields
    // are u16-sized on disk per MS-FSCC §2.4.9, so silent truncation
    // would produce a wrong on-disk layout. Current callers ($SDH/$SII
    // with 4-/8-byte keys and 20-byte values) stay well under, but
    // future view indexes ($O / $Q / etc.) could push higher.
    let data_offset_u16 = u16::try_from(value_off).expect("view-index data_offset overflows u16");
    let data_length_u16 = u16::try_from(value.len()).expect("view-index data_length overflows u16");
    buf[0..2].copy_from_slice(&data_offset_u16.to_le_bytes());
    buf[2..4].copy_from_slice(&data_length_u16.to_le_bytes());
    // buf[4..8] reserved (already zero).
    buf[8..10].copy_from_slice(&(entry_len as u16).to_le_bytes());
    buf[10..12].copy_from_slice(&(key_len as u16).to_le_bytes());
    // flags (u32) — only low 2 bits defined (0x01 HAS_SUBNODE,
    // 0x02 LAST). Zero for a normal entry.
    buf[12..16].copy_from_slice(&0u32.to_le_bytes());
    buf[header..header + key_len].copy_from_slice(key);
    buf[value_off..value_off + value.len()].copy_from_slice(value);
    buf
}

/// 48-byte on-disk QUOTA_CONTROL for the $Q default record (OwnerID=1).
/// Version=2, FLAG_ID_ALLOCATED=1, zero usage, unlimited warning/hard
/// limits (-1), zero exceeded-time. Extracted from a Windows-formatted
/// cluster-4K image (post-mount, USA fixup applied).
fn build_default_quota_control() -> [u8; 48] {
    let mut buf = [0u8; 48];
    buf[0..4].copy_from_slice(&2u32.to_le_bytes()); // Version
    buf[4..8].copy_from_slice(&1u32.to_le_bytes()); // Flags = FLAG_ID_ALLOCATED
                                                    // bytes_used[8..16]  = 0  (already)
                                                    // change_time[16..24] = 0 (already)
    buf[24..32].copy_from_slice(&(-1i64).to_le_bytes()); // WarningLimit
    buf[32..40].copy_from_slice(&(-1i64).to_le_bytes()); // HardLimit
                                                         // exceeded_time[40..48] = 0 (already)
    buf
}

/// LAST sentinel for a view-index (no key, no value, flags=0x02).
fn build_view_index_last_entry() -> Vec<u8> {
    let mut buf = vec![0u8; 0x10];
    buf[8..10].copy_from_slice(&0x0010u16.to_le_bytes());
    buf[10..12].copy_from_slice(&0u16.to_le_bytes());
    buf[12..16].copy_from_slice(&0x00000002u32.to_le_bytes());
    buf
}

/// Compose just the value bytes of a `$FILE_NAME` attribute (the
/// attribute *stream*, without the attribute header). Same byte layout
/// `write_file_name` produces in-record; reused so root's `$I30`
/// `INDEX_ENTRY`s carry byte-identical streams to the in-record `$FILE_NAME`s.
fn build_file_name_stream(
    parent_reference: u64,
    nt_time: u64,
    name: &str,
    is_dir: bool,
    is_system: bool,
    data_alloc: u64,
    data_real: u64,
    namespace: u8,
) -> Result<Vec<u8>, String> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    if utf16.is_empty() || utf16.len() > 255 {
        return Err(format!("invalid name length {}", utf16.len()));
    }
    let key_fixed = 0x42usize;
    let mut buf = vec![0u8; key_fixed + utf16.len() * 2];
    buf[0..8].copy_from_slice(&parent_reference.to_le_bytes());
    buf[8..16].copy_from_slice(&nt_time.to_le_bytes());
    buf[16..24].copy_from_slice(&nt_time.to_le_bytes());
    buf[24..32].copy_from_slice(&nt_time.to_le_bytes());
    buf[32..40].copy_from_slice(&nt_time.to_le_bytes());
    buf[40..48].copy_from_slice(&data_alloc.to_le_bytes());
    buf[48..56].copy_from_slice(&data_real.to_le_bytes());
    let mut fa: u32 = if is_system {
        FA_HIDDEN | FA_SYSTEM
    } else {
        FA_ARCHIVE
    };
    if is_dir {
        fa |= FA_NTFS_DIRECTORY;
    }
    buf[56..60].copy_from_slice(&fa.to_le_bytes());
    buf[60..64].copy_from_slice(&0u32.to_le_bytes());
    buf[64] = utf16.len() as u8;
    buf[65] = namespace;
    for (i, c) in utf16.iter().enumerate() {
        let off = 66 + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    Ok(buf)
}

/// "Skeleton" `$FILE_NAME` stream for a system metafile entry in the
/// root `$I30`. Microsoft `format.com` zeros every numeric field
/// (timestamps, alloc/real sizes, file_attributes, ea/reparse) on
/// the embedded FN of system entries, keeping only `parent_reference`
/// and the name. The in-record FN on the system file itself stays
/// fully populated; the divergence is specific to the index entry.
///
/// Byte-corroboration: run-20260503-011545/mac-format-label-empty,
/// reference-mft-16recs.bin rec 5 entries 0..4,6..10. See
/// `docs/spec/sections/04-indexes-directories.md#i30-system-skeleton`.
fn build_skeleton_fn_stream(parent_reference: u64, name: &str) -> Result<Vec<u8>, String> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    if utf16.is_empty() || utf16.len() > 255 {
        return Err(format!("invalid name length {}", utf16.len()));
    }
    let key_fixed = 0x42usize;
    let mut buf = vec![0u8; key_fixed + utf16.len() * 2];
    buf[0..8].copy_from_slice(&parent_reference.to_le_bytes());
    // Bytes 0x08..0x40 left zero — that's the whole point.
    buf[64] = utf16.len() as u8;
    buf[65] = NAMESPACE_WIN32_DOS;
    for (i, c) in utf16.iter().enumerate() {
        let off = 66 + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }
    Ok(buf)
}

/// Build a single `INDEX_ENTRY` for an `$I30` index. Header is 16 bytes;
/// stream follows; entry padded to 8. `is_last=true` produces the LAST
/// sentinel (zero-length stream, flags=0x02).
fn build_index_entry(file_reference: u64, stream: &[u8], is_last: bool) -> Vec<u8> {
    let header = 0x10usize;
    let entry_len = align8(header + stream.len());
    let mut buf = vec![0u8; entry_len];
    buf[0..8].copy_from_slice(&file_reference.to_le_bytes());
    buf[8..10].copy_from_slice(&(entry_len as u16).to_le_bytes());
    buf[10..12].copy_from_slice(&(stream.len() as u16).to_le_bytes());
    let flags: u32 = if is_last { 0x02 } else { 0x00 };
    buf[12..16].copy_from_slice(&flags.to_le_bytes());
    if !stream.is_empty() {
        buf[16..16 + stream.len()].copy_from_slice(stream);
    }
    buf
}

/// Build a populated `$INDEX_ROOT` `$I30` attribute. `entries_blob` must
/// already contain pre-sorted `INDEX_ENTRY`s terminated by a LAST sentinel.
///
/// `cluster_size` is required to encode the `clusters_per_index_block`
/// byte at value-offset 0x0C. Microsoft's INDEX_ROOT carries a *signed*
/// power-of-2 representation of `index_block_size / cluster_size`:
/// when `index_block_size >= cluster_size` it's the positive quotient;
/// when `cluster_size > index_block_size` it's `-(log2(index_block_size))`.
/// chkdsk validates this against the boot-sector cpib at 0x44 and
/// reports `Corrupt master file table` on any mismatch (matrix run
/// `mac-format-cluster-512` / `cluster-1k`, run-20260503-011545).
fn build_populated_index_root_attr(
    attr_id: u16,
    index_block_size: u32,
    cluster_size: u32,
    entries_blob: &[u8],
) -> Vec<u8> {
    let common_header = 16usize;
    let resident_fields = 8usize;
    let header_size = common_header + resident_fields;
    let name_offset = header_size;
    let name_bytes = 8usize;
    let value_offset = align8(name_offset + name_bytes);
    let ir_value_size = 16 + 16 + entries_blob.len();
    let attr_length = align8(value_offset + ir_value_size);

    let mut buf = vec![0u8; attr_length];
    buf[0..4].copy_from_slice(&ATTR_INDEX_ROOT.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    buf[8] = 0;
    buf[9] = 4;
    buf[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&0u16.to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(ir_value_size as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    buf[22] = 0;
    buf[23] = 0;

    for (i, c) in stream::utf16(stream::I30).iter().enumerate() {
        let off = name_offset + i * 2;
        buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }

    let v = value_offset;
    buf[v..v + 4].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes());
    buf[v + 4..v + 8].copy_from_slice(&COLLATION_FILE_NAME.to_le_bytes());
    buf[v + 8..v + 12].copy_from_slice(&index_block_size.to_le_bytes());
    // clusters_per_index_block byte at INDEX_ROOT body offset 0x0C.
    //
    // Microsoft's encoding (corroborated against `format.com` reference
    // dumps for 512 / 1024 / 4096 / 8192 cluster sizes in matrix run
    // run-20260503-030521):
    //   cluster_size <= index_block_size:
    //       cpib = index_block_size / cluster_size  (e.g. 8/4/1)
    //   cluster_size  >  index_block_size:
    //       cpib = index_block_size / 512           (sectors-per-block)
    //
    // This is DIFFERENT from boot.cpib at 0x44, which uses signed-
    // negative-log2 in the second branch (e.g. cluster=8192 →
    // boot.cpib = -log2(4096) = -12 = 0xF4 vs INDEX_ROOT.cpib = 8).
    // chkdsk Stage 2 reports `Error detected in index $I30 for file 5`
    // when these byte forms diverge from reference.
    let cpib_byte: u8 = if cluster_size <= index_block_size {
        (index_block_size / cluster_size) as u8
    } else {
        (index_block_size / 512) as u8
    };
    buf[v + 12] = cpib_byte;

    let ih = v + 16;
    buf[ih..ih + 4].copy_from_slice(&16u32.to_le_bytes());
    let used = 16u32 + entries_blob.len() as u32;
    buf[ih + 4..ih + 8].copy_from_slice(&used.to_le_bytes());
    buf[ih + 8..ih + 12].copy_from_slice(&used.to_le_bytes());

    let entries_at = ih + 16;
    buf[entries_at..entries_at + entries_blob.len()].copy_from_slice(entries_blob);

    buf
}

/// COLLATION_FILE_NAME ordering. Our system file names are ASCII with
/// `$` prefix (and `.` for root), so simple ASCII upcase + UTF-16-LE
/// bytewise comparison reproduces Microsoft's reference order exactly.
fn collate_file_name(a: &str, b: &str) -> std::cmp::Ordering {
    let ua: Vec<u16> = a.encode_utf16().map(ascii_upcase16).collect();
    let ub: Vec<u16> = b.encode_utf16().map(ascii_upcase16).collect();
    ua.cmp(&ub)
}

fn ascii_upcase16(c: u16) -> u16 {
    if (0x61..=0x7A).contains(&c) {
        c - 0x20
    } else {
        c
    }
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

/// Build the canonical NTFS 3.1 $AttrDef table. 15 active entries ×
/// 160 bytes + 1 zero-terminator entry = 2560 bytes total. Format per
///   MS-FSCC and Windows Internals 7th ed.: 64-byte UTF-16 name +
///   u32 type + u32 display_rule + u32 collation + u32 flags +
///   u64 min_size + u64 max_size.
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
    // Entry contents match the NTFS 3.1 $AttrDef that chkdsk /scan
    // accepts without error. Key differences from format.com's NTFS 1.x
    // legacy table:
    //   - $STANDARD_INFORMATION max = 72 (not 48) — NTFS 3.x extended form
    //   - 0x40 entry name is "$OBJECT_ID" (not "$VOLUME_VERSION"), max=256
    //   - 0xC0 entry name is "$REPARSE_POINT" (not "$SYMBOLIC_LINK"), max=16384
    // $LOGGED_UTILITY_STREAM (0x100) is included — its absence caused
    // "Errors found in Attribute Definition Table" on sub-4K cluster
    // volumes where ntfs.sys does not update the legacy 1.x table.
    // chkdsk /scan validates every on-disk attribute against this table,
    // so the entry must be present and correctly sized.
    let entries = [
        Entry {
            name: "$STANDARD_INFORMATION",
            type_code: 0x10,
            collation: 0,
            flags: RESIDENT,
            min_size: 48,
            max_size: 72, // NTFS 3.x extended form (quota owner, security id, USN)
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
            collation: 0,
            flags: RESIDENT | INDEXED,
            min_size: 68,
            max_size: 578,
        },
        Entry {
            name: "$OBJECT_ID", // NTFS 3.x name (was "$VOLUME_VERSION" in 1.x)
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
            flags: NONRES, // ref has 0x80, not 0
            min_size: 0,
            max_size: NEG1,
        },
        Entry {
            name: "$REPARSE_POINT", // NTFS 3.x name (was "$SYMBOLIC_LINK" in 1.x)
            type_code: 0xC0,
            collation: 0,
            flags: NONRES,
            min_size: 0,
            max_size: 16384, // 0x4000 per post-mount reference image
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
            name: "$LOGGED_UTILITY_STREAM",
            type_code: 0x100,
            collation: 0,
            flags: NONRES,
            min_size: 0,
            max_size: 65536,
        },
    ];
    // 15 active entries + one 160-byte all-zeros terminator = 2560 bytes.
    let mut out = Vec::with_capacity(160 * (entries.len() + 1));
    for e in &entries {
        let mut buf = [0u8; 160];
        let name_utf16: Vec<u16> = e.name.encode_utf16().collect();
        for (i, c) in name_utf16.iter().enumerate().take(64) {
            let off = i * 2;
            buf[off..off + 2].copy_from_slice(&c.to_le_bytes());
        }
        buf[0x80..0x84].copy_from_slice(&e.type_code.to_le_bytes());
        buf[0x84..0x88].copy_from_slice(&0u32.to_le_bytes());
        buf[0x88..0x8C].copy_from_slice(&e.collation.to_le_bytes());
        buf[0x8C..0x90].copy_from_slice(&e.flags.to_le_bytes());
        buf[0x90..0x98].copy_from_slice(&e.min_size.to_le_bytes());
        buf[0x98..0xA0].copy_from_slice(&e.max_size.to_le_bytes());
        out.extend_from_slice(&buf);
    }
    // Trailing 160-byte zero entry — present in reference (entry 14).
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
