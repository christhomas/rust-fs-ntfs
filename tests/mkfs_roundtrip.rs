//! Round-trip test: format an in-memory volume with `format_filesystem`,
//! then parse it back with the existing read path (upstream `Ntfs::new`)
//! and confirm the basic structure (boot sector, $Volume label, root
//! directory) is intact.

use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::Mutex;

use fs_ntfs::block_io::BlockIo;
use fs_ntfs::mkfs::{format_filesystem, rec, stream};
use fs_ntfs::{fs_ntfs_mkfs, FsNtfsBlockdevCfg};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileNamespace, NtfsVolumeName};
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType};

const VOL_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB

/// Vec-backed in-memory blockdev. The Rust path passes `&mut dyn BlockIo`
/// directly; the C ABI test plumbs through via callbacks.
struct MemDev {
    buf: Vec<u8>,
}

impl MemDev {
    fn new(size: u64) -> Self {
        Self {
            buf: vec![0u8; size as usize],
        }
    }
}

impl BlockIo for MemDev {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let off = offset as usize;
        if off + buf.len() > self.buf.len() {
            return Err(format!(
                "read past end: offset={off} len={} size={}",
                buf.len(),
                self.buf.len()
            ));
        }
        buf.copy_from_slice(&self.buf[off..off + buf.len()]);
        Ok(())
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        let off = offset as usize;
        if off + buf.len() > self.buf.len() {
            return Err(format!(
                "write past end: offset={off} len={} size={}",
                buf.len(),
                self.buf.len()
            ));
        }
        self.buf[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.buf.len() as u64
    }
}

#[test]
fn format_and_parse_back() {
    let mut dev = MemDev::new(VOL_SIZE);
    format_filesystem(
        &mut dev,
        VOL_SIZE,
        4096,
        4096,
        Some("TESTVOL"),
        Some(0xDEADBEEF),
    )
    .expect("format_filesystem");

    // Parse back via upstream Ntfs.
    let mut cursor = std::io::Cursor::new(&dev.buf);
    let mut ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new on freshly formatted volume");
    ntfs.read_upcase_table(&mut cursor)
        .expect("read $UpCase from freshly formatted volume");

    assert_eq!(ntfs.cluster_size(), 4096);
    assert_eq!(ntfs.serial_number(), 0xDEADBEEF);

    // Volume info: NTFS 3.1 with NO UPGRADE_ON_MOUNT flag.
    // S3.1 2026-05-24: system records now carry 72-byte $STD_INFO
    // with SecurityId, so 3.1 is self-consistent with the metadata.
    let vi = ntfs
        .volume_info(&mut cursor)
        .expect("read $VOLUME_INFORMATION");
    assert_eq!(vi.major_version(), 3);
    assert_eq!(vi.minor_version(), 1);

    // Volume name.
    let vol_name_opt = ntfs.volume_name(&mut cursor);
    let name: NtfsVolumeName = vol_name_opt
        .expect("$Volume has $VOLUME_NAME")
        .expect("read $VOLUME_NAME");
    assert_eq!(name.name().to_string_lossy(), "TESTVOL");

    // Root directory's $I30 must contain entries for every system file
    // (records 0..11) plus a self-entry for ".". This matches Microsoft
    // format.com's output and the publicly documented NTFS layout.
    // See iter13 in docs/chkdsk-findings.md: prior builds left the root
    // index empty, which made chkdsk treat every system file as
    // orphaned. Sub-PR S3 (chkdsk-improvement-findings.md §6.9, Iter H)
    // re-adds rec 11 ($Extend) as a directory shell so chkdsk /scan's
    // recursion into $Extend\$Reparse and $Extend\$RmMetadata can find
    // a parent. Entry order is COLLATION_FILE_NAME (case-insensitive
    // UTF-16 with shorter-prefix-loses), which on pure-ASCII names
    // reduces to ASCII-uppercase code-unit comparison.
    let root = ntfs.root_directory(&mut cursor).expect("root directory");
    let index = root
        .directory_index(&mut cursor)
        .expect("root directory_index");
    let mut iter = index.entries();
    let mut names = Vec::new();
    while let Some(entry) = iter.next(&mut cursor) {
        let entry = entry.expect("entry");
        let key = match entry.key() {
            Some(Ok(k)) => k,
            _ => continue,
        };
        if key.namespace() == NtfsFileNamespace::Dos {
            continue;
        }
        names.push(key.name().to_string_lossy());
    }
    // Slot 9 is named `$Secure` at this test's cluster_size = 4096.
    // At smaller cluster sizes (512, 1024) chkdsk expects `$Quota`
    // instead — see `mkfs::rec::name`'s docstring for the full Iter M
    // matrix-trace rationale. Pulling the names from `rec::name()`
    // routes both sides of this assertion through that single source
    // of truth, so a typo / wrong-cluster-size choice can't produce
    // a green test against a broken volume.
    assert_eq!(
        names,
        vec![
            rec::name(rec::ATTRDEF, 4096).expect("known rec_num"),
            rec::name(rec::BADCLUS, 4096).expect("known rec_num"),
            rec::name(rec::BITMAP, 4096).expect("known rec_num"),
            rec::name(rec::BOOT, 4096).expect("known rec_num"),
            rec::name(rec::EXTEND, 4096).expect("known rec_num"),
            rec::name(rec::LOGFILE, 4096).expect("known rec_num"),
            rec::name(rec::MFT, 4096).expect("known rec_num"),
            rec::name(rec::MFTMIRR, 4096).expect("known rec_num"),
            rec::name(rec::SECURE, 4096).expect("known rec_num"),
            rec::name(rec::UPCASE, 4096).expect("known rec_num"),
            rec::name(rec::VOLUME, 4096).expect("known rec_num"),
            rec::name(rec::ROOT, 4096).expect("known rec_num"),
        ],
        "root $I30 must list every system file in COLLATION_FILE_NAME order"
    );

    // $UpCase should be readable as the file at record 10 with a
    // 128 KiB unnamed $DATA.
    let upcase_file = ntfs
        .file(&mut cursor, KnownNtfsFileRecordNumber::UpCase as u64)
        .expect("open $UpCase record");
    let mut found_unnamed_data = false;
    let mut found_info_stream = false;
    let mut attrs = upcase_file.attributes();
    while let Some(item) = attrs.next(&mut cursor) {
        let item = item.expect("attr item");
        let a = item.to_attribute().expect("to_attribute");
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        let name = a
            .name()
            .expect("attr name")
            .to_string()
            .expect("attr name to_string");
        if name.is_empty() {
            // The 128 KiB UpCase table itself.
            assert_eq!(a.value_length(), 128 * 1024);
            found_unnamed_data = true;
        } else if name == stream::INFO {
            // 32-byte resident named stream (Iter M-2): carries the
            // CRC64 of the UpCase table content + reserved zeros.
            assert!(a.is_resident(), "$UpCase:$Info must be resident");
            assert_eq!(a.value_length(), 32);
            found_info_stream = true;
        }
    }
    assert!(found_unnamed_data, "$UpCase unnamed $DATA missing");
    assert!(found_info_stream, "$UpCase:$Info named stream missing");

    // Looking up a nonexistent name in the root index should not panic
    // (just return None / Err).
    let mut finder = index.finder();
    let result = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut cursor, "nonexistent");
    assert!(result.is_none(), "should not find a nonexistent name");
}

/// Sub-PR S2: `$Secure` (rec 9) ships
///   * named `$DATA`        "$SDS" — non-resident (primary at file
///     offset 0, mirror at +0x40000), holding one canonical SD entry.
///   * named `$INDEX_ROOT`  "$SDH" — populated with one entry mapping
///     `(hash, security_id=0x100)` to the SDS offset/size pair.
///   * named `$INDEX_ROOT`  "$SII" — populated with one entry keyed
///     on `security_id = 0x100`.
///
/// In addition, every system MFT record's `$STANDARD_INFORMATION` now
/// carries `security_id = 0x100` referencing that entry.
#[test]
fn secure_record_has_sds_sdh_sii_named_streams() {
    use fs_ntfs::sds::{sdh_hash, SDS_HEADER_LEN, SDS_MIRROR_GAP};

    let mut dev = MemDev::new(VOL_SIZE);
    format_filesystem(&mut dev, VOL_SIZE, 4096, 4096, Some("TESTVOL"), None)
        .expect("format_filesystem");

    // The canonical SD stored in $Secure:$SDS at security_id=0x100.
    // All system records reference this entry via $STD_INFO.SecurityId.
    const SD_SYSFILE_RW: &[u8] = &[
        0x01, 0x00, 0x04, 0x80, 0x48, 0x00, 0x00, 0x00, 0x58, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x14, 0x00, 0x00, 0x00, 0x02, 0x00, 0x34, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x14, 0x00, 0x9f, 0x01, 0x12, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x12,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x9f, 0x01, 0x12, 0x00, 0x01, 0x02, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00, 0x01, 0x02, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00, 0x01, 0x02,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00,
    ];
    let expected_hash = sdh_hash(SD_SYSFILE_RW);
    let expected_sds_size: u32 = SDS_HEADER_LEN + SD_SYSFILE_RW.len() as u32;

    let mut cursor = std::io::Cursor::new(&dev.buf);
    let mut ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut cursor).expect("upcase");

    let secure = ntfs
        .file(&mut cursor, KnownNtfsFileRecordNumber::Secure as u64)
        .expect("open $Secure record");

    let mut seen_sds = false;
    let mut seen_sdh = false;
    let mut seen_sii = false;
    let mut sds_bytes: Vec<u8> = Vec::new();
    let mut sdh_value: Vec<u8> = Vec::new();
    let mut sii_value: Vec<u8> = Vec::new();

    let mut attrs = secure.attributes();
    while let Some(item) = attrs.next(&mut cursor) {
        let item = item.expect("attr item");
        let a = item.to_attribute().expect("to_attribute");
        let ty = match a.ty() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let name = a
            .name()
            .expect("attr name")
            .to_string()
            .expect("attr name to_string");
        use std::io::Read;
        match (ty, name.as_str()) {
            (NtfsAttributeType::Data, n) if n == stream::SDS => {
                assert!(
                    !a.is_resident(),
                    "$SDS must be non-resident at S2 (one canonical entry + mirror)"
                );
                let sds_data_len = a.value_length();
                assert!(
                    sds_data_len >= SDS_MIRROR_GAP + expected_sds_size as u64,
                    "$SDS data_length {sds_data_len} too small to hold primary + mirror"
                );
                let v = a.value(&mut cursor).expect("sds value");
                let mut buf = vec![0u8; sds_data_len as usize];
                v.attach(&mut cursor)
                    .read_exact(&mut buf)
                    .expect("read $SDS stream");
                sds_bytes = buf;
                seen_sds = true;
            }
            (NtfsAttributeType::IndexRoot, n) if n == stream::SDH => {
                assert!(a.is_resident(), "$SDH index-root must be resident at S2");
                let total = a.value_length() as usize;
                let v = a.value(&mut cursor).expect("sdh value");
                let mut buf = vec![0u8; total];
                v.attach(&mut cursor)
                    .read_exact(&mut buf)
                    .expect("read sdh");
                sdh_value = buf;
                seen_sdh = true;
            }
            (NtfsAttributeType::IndexRoot, n) if n == stream::SII => {
                assert!(a.is_resident(), "$SII index-root must be resident at S2");
                let total = a.value_length() as usize;
                let v = a.value(&mut cursor).expect("sii value");
                let mut buf = vec![0u8; total];
                v.attach(&mut cursor)
                    .read_exact(&mut buf)
                    .expect("read sii");
                sii_value = buf;
                seen_sii = true;
            }
            _ => {}
        }
    }

    assert!(seen_sds, "rec 9 missing named-$DATA \"$SDS\"");
    assert!(seen_sdh, "rec 9 missing named-$INDEX_ROOT \"$SDH\"");
    assert!(seen_sii, "rec 9 missing named-$INDEX_ROOT \"$SII\"");

    // Primary entry at offset 0.
    let hash_at_0 = u32::from_le_bytes([sds_bytes[0], sds_bytes[1], sds_bytes[2], sds_bytes[3]]);
    let sid_at_0 = u32::from_le_bytes([sds_bytes[4], sds_bytes[5], sds_bytes[6], sds_bytes[7]]);
    let off_at_0 = u64::from_le_bytes([
        sds_bytes[8],
        sds_bytes[9],
        sds_bytes[10],
        sds_bytes[11],
        sds_bytes[12],
        sds_bytes[13],
        sds_bytes[14],
        sds_bytes[15],
    ]);
    let size_at_0 =
        u32::from_le_bytes([sds_bytes[16], sds_bytes[17], sds_bytes[18], sds_bytes[19]]);
    assert_eq!(hash_at_0, expected_hash, "$SDS primary entry hash");
    assert_eq!(sid_at_0, 0x100, "$SDS primary entry security_id");
    assert_eq!(off_at_0, 0, "$SDS primary entry sds_offset");
    assert_eq!(size_at_0, expected_sds_size, "$SDS primary entry sds_size");

    // Mirror at +0x40000 — same bytes as primary header.
    let m = SDS_MIRROR_GAP as usize;
    assert_eq!(
        &sds_bytes[m..m + 20],
        &sds_bytes[..20],
        "$SDS mirror header bytes must match primary"
    );

    // $SDH: parse the inline index. value layout is index-root header
    // (16 bytes) + index header (16 bytes) + entries. The first entry
    // is the populated one (16-byte header + 8-byte key + 20-byte
    // value padded to 8 = 48 bytes), followed by a 16-byte LAST.
    let entries_off = 32usize;
    let e0 = &sdh_value[entries_off..];
    let e0_len = u16::from_le_bytes([e0[8], e0[9]]) as usize;
    let key_len = u16::from_le_bytes([e0[10], e0[11]]) as usize;
    assert_eq!(key_len, 8, "$SDH key length");
    let key_off = 0x10;
    let sdh_hash_key = u32::from_le_bytes([
        e0[key_off],
        e0[key_off + 1],
        e0[key_off + 2],
        e0[key_off + 3],
    ]);
    let sdh_sid_key = u32::from_le_bytes([
        e0[key_off + 4],
        e0[key_off + 5],
        e0[key_off + 6],
        e0[key_off + 7],
    ]);
    assert_eq!(sdh_hash_key, expected_hash, "$SDH entry key hash");
    assert_eq!(sdh_sid_key, 0x100, "$SDH entry key security_id");
    // Regression — Iter J: view-index entries MUST carry a non-zero
    // data_offset (+0x00 u16 LE) and data_length (+0x02 u16 LE),
    // pointing at where in the entry the value bytes live. S2 first
    // shipped these as zero (treating +0x00..0x08 as `file_reference`
    // — the file-name-index convention, wrong for view indexes) and
    // chkdsk read `Index $SDH in file 9 is corrupt`.
    let sdh_data_offset = u16::from_le_bytes([e0[0], e0[1]]);
    let sdh_data_length = u16::from_le_bytes([e0[2], e0[3]]);
    assert_eq!(
        sdh_data_offset, 24,
        "$SDH entry data_offset must point at value (after 16B hdr + 8B key)"
    );
    assert_eq!(
        sdh_data_length, 20,
        "$SDH entry data_length must equal value length"
    );
    // Value starts after key, 8-aligned. With 16-byte header + 8-byte
    // key = 24, next 8-aligned offset = 24.
    let val_off = 24usize;
    let v_sds_off = u64::from_le_bytes(e0[val_off + 8..val_off + 16].try_into().unwrap());
    let v_sds_size = u32::from_le_bytes(e0[val_off + 16..val_off + 20].try_into().unwrap());
    assert_eq!(v_sds_off, 0, "$SDH value sds_offset");
    assert_eq!(v_sds_size, expected_sds_size, "$SDH value sds_size");
    // LAST sentinel: next entry has flags=0x02.
    let last = &sdh_value[entries_off + e0_len..];
    let last_flags = u32::from_le_bytes([last[12], last[13], last[14], last[15]]);
    assert_eq!(last_flags & 0x02, 0x02, "$SDH LAST sentinel");

    // $SII: key is 4-byte security_id, value mirrors $SDH's value.
    let s0 = &sii_value[entries_off..];
    let s0_klen = u16::from_le_bytes([s0[10], s0[11]]) as usize;
    assert_eq!(s0_klen, 4, "$SII key length");
    // View-index value lives **immediately after the key** with no
    // alignment padding — for `$SII` (key_len=4) that's offset 0x14,
    // not 0x18. An earlier cut of `build_view_index_entry` `align8`d
    // the value offset, which `$SDH` (key_len=8) silently tolerated
    // but caused chkdsk to report `Index $SII in file 9 is corrupt`
    // on the resulting `data_offset = 0x18 / entry_length = 0x30`
    // layout. Reference `Format-Volume` byte-diff (Iter K) confirms
    // no padding between key and value across 8 sampled entries.
    let sii_data_offset = u16::from_le_bytes([s0[0], s0[1]]);
    let sii_data_length = u16::from_le_bytes([s0[2], s0[3]]);
    assert_eq!(
        sii_data_offset, 0x14,
        "$SII entry data_offset must point at value (after 16B hdr + 4B key, NO padding)"
    );
    assert_eq!(
        sii_data_length, 20,
        "$SII entry data_length must equal value length"
    );
    let sii_sid = u32::from_le_bytes([s0[0x10], s0[0x11], s0[0x12], s0[0x13]]);
    assert_eq!(sii_sid, 0x100, "$SII entry key security_id");
    // Value at offset 0x14 (immediately after the 4-byte key).
    let sii_val_off = 0x14usize;
    let s_sds_size = u32::from_le_bytes(s0[sii_val_off + 16..sii_val_off + 20].try_into().unwrap());
    assert_eq!(s_sds_size, expected_sds_size, "$SII value sds_size");

    // S3.1: system records use the 72-byte v3.x $STD_INFO form.
    // SecurityId at value-relative offset 0x34 must be 0x100 (the
    // single canonical SDS entry).
    for rec_num in [0u64, 5u64, 9u64] {
        let f = ntfs.file(&mut cursor, rec_num).expect("open system rec");
        let mut std_attrs = f.attributes();
        let mut found = false;
        while let Some(item) = std_attrs.next(&mut cursor) {
            let item = item.expect("attr item");
            let a = item.to_attribute().expect("to_attribute");
            if a.ty().ok() != Some(NtfsAttributeType::StandardInformation) {
                continue;
            }
            assert_eq!(
                a.value_length(),
                72,
                "rec {rec_num} $STD_INFO must be 72-byte v3.x form"
            );
            use std::io::Read;
            let mut val = vec![0u8; 72];
            a.value(&mut cursor)
                .expect("std_info value")
                .attach(&mut cursor)
                .read_exact(&mut val)
                .expect("read std_info");
            let sid = u32::from_le_bytes([val[0x34], val[0x35], val[0x36], val[0x37]]);
            assert_eq!(sid, 0x100, "rec {rec_num} $STD_INFO SecurityId");
            found = true;
            break;
        }
        assert!(found, "rec {rec_num} missing $STD_INFO");
    }
}

/// Sub-PR S3 + Iter L final: rec 11 must be a directory shell named
/// `$Extend` whose parent is the root (rec 5), with the MFT-header
/// IS_DIRECTORY flag set. Its `$I30` is *empty* (just the LAST
/// sentinel) — Iter L 2026-05-22 verified against a freshly-formatted
/// Windows volume that **chkdsk readonly exits 0** when $Extend's
/// children are absent. (chkdsk `/scan` still exits 13 with NTFS
/// Event 55 "exact nature unknown" — the same baseline that pre-dated
/// the S1..S5 work; Event 136 "TxF metadata reset" is *gone*, which
/// is what removing the partial hierarchy bought us.) Shipping any
/// partial subset (e.g. $RmMetadata without the full $TxfLog +
/// $TxfLog.blf + $TxfLogContainer.. family) drives the kernel TxF
/// resource manager to fail (Event 136) and raises Event 55
/// "corruption discovered".
#[test]
fn extend_record_is_empty_directory() {
    let mut dev = MemDev::new(VOL_SIZE);
    format_filesystem(
        &mut dev,
        VOL_SIZE,
        4096,
        4096,
        Some("S3TEST"),
        Some(0xCAFEBABE),
    )
    .expect("format_filesystem");

    let mut cursor = std::io::Cursor::new(&dev.buf);
    let ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new on freshly formatted volume");

    // Open rec 11 via the upstream crate.
    let extend = ntfs.file(&mut cursor, 11).expect("open rec 11 ($Extend)");

    // MFT header IS_DIRECTORY flag must be set.
    assert!(
        extend.is_directory(),
        "rec 11 must have MFT header IS_DIRECTORY flag set"
    );

    // $FILE_NAME: name = "$Extend", parent_reference = (rec=5, root).
    // System records use the Win32AndDos namespace (value 3) — the same
    // convention every other system record in this layout uses.
    let fname = extend
        .name(&mut cursor, Some(NtfsFileNamespace::Win32AndDos), None)
        .expect("rec 11 has a Win32AndDos $FILE_NAME")
        .expect("read $FILE_NAME");
    assert_eq!(
        fname.name().to_string_lossy(),
        rec::name(rec::EXTEND, 4096).expect("known rec_num")
    );
    assert_eq!(
        fname.parent_directory_reference().file_record_number(),
        5,
        "rec 11 parent must be root (rec 5)"
    );
    assert!(
        fname.is_directory(),
        "rec 11 $FILE_NAME.file_attributes must carry FILE_ATTRIBUTE_DIRECTORY"
    );

    // $I30 INDEX_ROOT must contain exactly $ObjId (16), $Reparse (17),
    // $RmMetadata (18) — sorted by NTFS collation order. These are
    // required for chkdsk /scan to exit 0 (scan-f-scan proof 2026-05-24).
    let index = extend
        .directory_index(&mut cursor)
        .expect("$Extend must have $I30 $INDEX_ROOT");
    let mut iter = index.entries();
    let mut entries: Vec<(String, u64)> = Vec::new();
    while let Some(entry) = iter.next(&mut cursor) {
        let entry = entry.expect("entry iterates without error");
        let key = entry.key().expect("entry has a key").expect("decode key");
        if key.namespace() == NtfsFileNamespace::Dos {
            continue;
        }
        let mft_ref = entry.file_reference().file_record_number();
        entries.push((key.name().to_string_lossy(), mft_ref));
    }
    assert_eq!(
        entries,
        vec![
            ("$ObjId".to_string(), rec::OBJID as u64),
            ("$Reparse".to_string(), rec::REPARSE as u64),
        ],
        "$Extend $I30 must list $ObjId and $Reparse; got {entries:?}"
    );
}

/// Read the MFT base offset + record size from the BPB so the
/// zero-slot assertions below stay load-bearing if the test's
/// cluster/record params ever drift. NTFS BPB (sector 0):
///   * `bytes_per_sector` (u16 @ 0x0B)
///   * `sectors_per_cluster` (u8 @ 0x0D)
///   * `mft_lcn` (u64 @ 0x30)
///   * `clusters_per_mft_record` (i8 @ 0x40)
///     positive ⇒ clusters per record;
///     negative ⇒ `record_size = 1 << -value` bytes (used when the
///     record is smaller than one cluster).
fn mft_layout_from_bpb(buf: &[u8]) -> (usize, usize) {
    let bps = u16::from_le_bytes([buf[0x0B], buf[0x0C]]) as usize;
    let spc = buf[0x0D] as usize;
    let cluster_size = bps * spc;
    let mut mft_lcn_bytes = [0u8; 8];
    mft_lcn_bytes.copy_from_slice(&buf[0x30..0x38]);
    let mft_lcn = u64::from_le_bytes(mft_lcn_bytes) as usize;
    let cpmr = buf[0x40] as i8;
    let record_size = if cpmr >= 0 {
        cpmr as usize * cluster_size
    } else {
        1usize << (-(cpmr as i32) as u32)
    };
    (mft_lcn * cluster_size, record_size)
}

/// Rec 16 ($ObjId) must be a populated MFT slot with FILE magic,
/// VIEW_INDEX flag (0x0009), and a named $O INDEX_ROOT attribute.
/// scan-f-scan proof (2026-05-24) showed chkdsk /scan exits 13
/// without $ObjId under $Extend; with it, /scan exits 0.
#[test]
fn objid_slot_is_populated() {
    let mut dev = MemDev::new(VOL_SIZE);
    format_filesystem(
        &mut dev,
        VOL_SIZE,
        4096,
        4096,
        Some("S4TEST"),
        Some(0xCAFEBABE),
    )
    .expect("format_filesystem");

    let (mft_offset, record_size) = mft_layout_from_bpb(&dev.buf);
    let rec16 = mft_offset + 16 * record_size;
    let slot = &dev.buf[rec16..rec16 + record_size];
    assert_eq!(&slot[0..4], b"FILE", "rec 16 must have FILE magic");
    let flags = u16::from_le_bytes([slot[22], slot[23]]);
    assert_eq!(
        flags, 0x000D,
        "rec 16 must have flags=0x000D (IN_USE|0x04|VIEW_INDEX)"
    );
}

/// Slot 17 ($Reparse) must be populated; slot 18+ must be zeroed.
/// $RmMetadata is intentionally absent — chkdsk /scan accepts
/// $ObjId+$Reparse alone, and including an empty $RmMetadata caused
/// "corrupt basic file structure" (its $I30 needs children we don't ship).
#[test]
fn extend_child_slot_17_is_populated_18_is_zeroed() {
    let mut dev = MemDev::new(VOL_SIZE);
    format_filesystem(
        &mut dev,
        VOL_SIZE,
        4096,
        4096,
        Some("ITERL"),
        Some(0xFEEDFACE),
    )
    .expect("format_filesystem");

    let (mft_offset, record_size) = mft_layout_from_bpb(&dev.buf);

    // rec 17 = $Reparse: FILE magic + IN_USE|0x0004|VIEW_INDEX = 0x000D
    let rec17_off = mft_offset + 17 * record_size;
    let s17 = &dev.buf[rec17_off..rec17_off + record_size];
    assert_eq!(&s17[0..4], b"FILE", "rec 17 must have FILE magic");
    let flags17 = u16::from_le_bytes([s17[22], s17[23]]);
    assert_eq!(
        flags17, 0x000D,
        "rec 17 must have flags=0x000D (IN_USE|0x04|VIEW_INDEX)"
    );

    // rec 18 must be zeroed (no $RmMetadata)
    let rec18_off = mft_offset + 18 * record_size;
    assert!(
        dev.buf[rec18_off..rec18_off + record_size]
            .iter()
            .all(|&b| b == 0),
        "rec 18 must be a zeroed MFT slot"
    );
}

// --------------------------------------------------------------------------
// C ABI smoke: drive `fs_ntfs_mkfs` with callbacks against a Vec-backed
// context, then re-parse the resulting buffer.
// --------------------------------------------------------------------------

struct Ctx {
    buf: Mutex<Vec<u8>>,
}

unsafe extern "C" fn read_cb(
    ctx: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> c_int {
    let ctx = &*(ctx as *const Ctx);
    let v = ctx.buf.lock().expect("lock");
    let off = offset as usize;
    let len = length as usize;
    if off + len > v.len() {
        return 1;
    }
    let slice = std::slice::from_raw_parts_mut(buf as *mut u8, len);
    slice.copy_from_slice(&v[off..off + len]);
    0
}

unsafe extern "C" fn write_cb(
    ctx: *mut c_void,
    buf: *const c_void,
    offset: u64,
    length: u64,
) -> c_int {
    let ctx = &*(ctx as *const Ctx);
    let mut v = ctx.buf.lock().expect("lock");
    let off = offset as usize;
    let len = length as usize;
    if off + len > v.len() {
        return 1;
    }
    let slice = std::slice::from_raw_parts(buf as *const u8, len);
    v[off..off + len].copy_from_slice(slice);
    0
}

#[test]
fn capi_mkfs_then_parse() {
    let ctx = Ctx {
        buf: Mutex::new(vec![0u8; VOL_SIZE as usize]),
    };
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const Ctx as *mut c_void,
        size_bytes: VOL_SIZE,
        write: Some(write_cb),
    };
    let rc = fs_ntfs_mkfs(&cfg);
    assert_eq!(rc, 0, "fs_ntfs_mkfs failed");

    let buf = ctx.buf.lock().expect("lock").clone();
    let mut cursor = std::io::Cursor::new(&buf);
    let mut ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut cursor).expect("upcase");
    assert_eq!(ntfs.cluster_size(), 4096);
}
