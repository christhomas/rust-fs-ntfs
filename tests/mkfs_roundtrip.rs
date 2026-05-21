//! Round-trip test: format an in-memory volume with `format_filesystem`,
//! then parse it back with the existing read path (upstream `Ntfs::new`)
//! and confirm the basic structure (boot sector, $Volume label, root
//! directory) is intact.

use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::Mutex;

use fs_ntfs::block_io::BlockIo;
use fs_ntfs::mkfs::format_filesystem;
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

    // Volume info: NTFS 1.2 with UPGRADE_ON_MOUNT flag set — matches
    // what Microsoft `format.com` stamps on a fresh format. ntfs.sys
    // rewrites this to 3.1 on first mount via UPGRADE_ON_MOUNT; mkfs
    // intentionally produces the pre-upgrade state. See mkfs.rs's
    // $VOLUME_INFORMATION block.
    let vi = ntfs
        .volume_info(&mut cursor)
        .expect("read $VOLUME_INFORMATION");
    assert_eq!(vi.major_version(), 1);
    assert_eq!(vi.minor_version(), 2);

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
    // Slot 9 is named `$Quota` — that's the NTFS 3.x convention
    // Microsoft `format.com` uses ($Secure lives under \$Extend on the
    // volume). The legacy NTFS 1.x name was `$Secure` at slot 9;
    // chkdsk explicitly repairs that name at non-4K cluster sizes. See
    // mkfs.rs's record-9 builder.
    assert_eq!(
        names,
        vec![
            "$AttrDef", "$BadClus", "$Bitmap", "$Boot", "$Extend", "$LogFile", "$MFT", "$MFTMirr",
            "$Quota", "$UpCase", "$Volume", ".",
        ],
        "root $I30 must list every system file in COLLATION_FILE_NAME order"
    );

    // $UpCase should be readable as the file at record 10 with a
    // 128 KiB unnamed $DATA.
    let upcase_file = ntfs
        .file(&mut cursor, KnownNtfsFileRecordNumber::UpCase as u64)
        .expect("open $UpCase record");
    let mut found_data = false;
    let mut attrs = upcase_file.attributes();
    while let Some(item) = attrs.next(&mut cursor) {
        let item = item.expect("attr item");
        let a = item.to_attribute().expect("to_attribute");
        if a.ty().ok() == Some(NtfsAttributeType::Data) {
            assert_eq!(a.value_length(), 128 * 1024);
            found_data = true;
        }
    }
    assert!(found_data, "$UpCase $DATA missing");

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

    // The canonical SD shipped at security_id=0x100 — same blob the
    // mkfs path applies inline to every system record's
    // $SECURITY_DESCRIPTOR attribute.
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
            (NtfsAttributeType::Data, "$SDS") => {
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
            (NtfsAttributeType::IndexRoot, "$SDH") => {
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
            (NtfsAttributeType::IndexRoot, "$SII") => {
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
    // Same view-index data_offset/data_length invariant as $SDH above.
    let sii_data_offset = u16::from_le_bytes([s0[0], s0[1]]);
    let sii_data_length = u16::from_le_bytes([s0[2], s0[3]]);
    assert_eq!(
        sii_data_offset, 24,
        "$SII entry data_offset must point at value (after 16B hdr + 4B key + 4B align)"
    );
    assert_eq!(
        sii_data_length, 20,
        "$SII entry data_length must equal value length"
    );
    let sii_sid = u32::from_le_bytes([s0[0x10], s0[0x11], s0[0x12], s0[0x13]]);
    assert_eq!(sii_sid, 0x100, "$SII entry key security_id");
    // Value at 8-aligned offset after key. Header 16 + key 4 = 20 → 24.
    let sii_val_off = 24usize;
    let s_sds_size = u32::from_le_bytes(s0[sii_val_off + 16..sii_val_off + 20].try_into().unwrap());
    assert_eq!(s_sds_size, expected_sds_size, "$SII value sds_size");

    // Spot-check system records' `$STANDARD_INFORMATION` payload size.
    // Per MS-FSCC §2.4.2 SecurityId lives at value-relative offset
    // 0x34 in the 72-byte v3.x form; the 48-byte v1.x form does not
    // have a SecurityId field (its tail is MaxVersions / VersionNumber
    // / ClassId). System records currently use the 48-byte form (§2.3
    // in chkdsk-improvement-findings.md), so S2 cannot make them
    // reference the SDS entry — the $SDS/$SDH/$SII machinery exists
    // but no STD_INFO points to it. If a future iteration switches
    // system records to the 72-byte form, this test will need to
    // assert SecurityId == 0x100 at offset 0x34.
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
                48,
                "rec {rec_num} $STD_INFO must be 48-byte v1.x form (no SecurityId field)"
            );
            found = true;
            break;
        }
        assert!(found, "rec {rec_num} missing $STD_INFO");
    }
}

/// Sub-PR S3 + S4: rec 11 must be a directory shell named `$Extend`
/// whose parent is the root (rec 5), with the MFT-header IS_DIRECTORY
/// flag set. After S4 the `$I30` contains exactly one entry —
/// `$Reparse` (pointing at rec 16) — plus the LAST sentinel.
#[test]
fn extend_record_is_directory_with_reparse() {
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
    assert_eq!(fname.name().to_string_lossy(), "$Extend");
    assert_eq!(
        fname.parent_directory_reference().file_record_number(),
        5,
        "rec 11 parent must be root (rec 5)"
    );
    assert!(
        fname.is_directory(),
        "rec 11 $FILE_NAME.file_attributes must carry FILE_ATTRIBUTE_DIRECTORY"
    );

    // $I30 INDEX_ROOT exists (directory_index() succeeds) and lists
    // exactly one real entry: `$Reparse` → rec 16. The LAST sentinel
    // is consumed by the upstream iterator and not surfaced.
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
        vec![("$Reparse".to_string(), 16)],
        "$Extend $I30 must list exactly $Reparse → rec 16 after S4"
    );
}

/// Sub-PR S4: rec 16 must be a file named `$Reparse` parented to rec
/// 11 (`$Extend`), carrying an empty named `$INDEX_ROOT` "$R"
/// (view-index). chkdsk's Iter H Procmon trace shows /scan opens
/// `$Extend\$Reparse:$R:$INDEX_ALLOCATION` and currently gets
/// STATUS_OBJECT_PATH_NOT_FOUND; S4 makes that open resolve to a
/// parseable empty view-index.
#[test]
fn reparse_record_is_empty_r_view_index() {
    use std::io::Read;

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

    let mut cursor = std::io::Cursor::new(&dev.buf);
    let ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new on freshly formatted volume");

    let reparse = ntfs.file(&mut cursor, 16).expect("open rec 16 ($Reparse)");

    // $FILE_NAME: name = "$Reparse", parent = (rec=11, $Extend), namespace
    // = Win32AndDos (3). This is the Win32 + DOS double-namespace single
    // entry every system record uses; system names are pure ASCII so a
    // separate DOS short name isn't required.
    let fname = reparse
        .name(&mut cursor, Some(NtfsFileNamespace::Win32AndDos), None)
        .expect("rec 16 has a Win32AndDos $FILE_NAME")
        .expect("read $FILE_NAME");
    assert_eq!(fname.name().to_string_lossy(), "$Reparse");
    assert_eq!(
        fname.parent_directory_reference().file_record_number(),
        11,
        "rec 16 parent must be $Extend (rec 11)"
    );
    assert!(
        !fname.is_directory(),
        "$Reparse is a file (view-index host), not a directory"
    );

    // The MFT-header IS_DIRECTORY flag must NOT be set (the record
    // hosts a view-index, not a $FILE_NAME index).
    assert!(
        !reparse.is_directory(),
        "rec 16 must not have MFT header IS_DIRECTORY flag set"
    );

    // Walk the attributes: must find exactly one named `$INDEX_ROOT
    // "$R"`, resident, with a zero-entry payload (just the index-root
    // header + index header + LAST sentinel — 16+16+16 = 48 bytes).
    let mut found_r_index_root = false;
    let mut seen_data_stream = false;
    let mut attrs = reparse.attributes();
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
        match (ty, name.as_str()) {
            (NtfsAttributeType::IndexRoot, "$R") => {
                assert!(
                    a.is_resident(),
                    "$R $INDEX_ROOT must be resident at S4 (empty view-index)"
                );
                let total = a.value_length() as usize;
                let v = a.value(&mut cursor).expect("$R value");
                let mut buf = vec![0u8; total];
                v.attach(&mut cursor).read_exact(&mut buf).expect("read $R");
                // INDEX_ROOT body: indexed_attr_type(4) + collation(4)
                // + index_block_size(4) + cpib(1) + 3 pad + index_header(16)
                // + entries. Empty index = exactly one LAST sentinel
                // (16 bytes). Total = 16 + 16 + 16 = 48.
                assert_eq!(buf.len(), 48, "$R INDEX_ROOT empty form = 48 bytes");
                let indexed_attr_type = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                assert_eq!(
                    indexed_attr_type, 0,
                    "$R indexed_attr_type=0 (view-index, no per-key $FILE_NAME)"
                );
                let collation = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                assert_eq!(
                    collation, 0x13,
                    "$R collation = COLLATION_NTOFS_ULONGS (0x13)"
                );
                let index_block_size = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
                assert_eq!(index_block_size, 4096, "$R index_block_size = 4096");
                // LAST sentinel at offset 32: entry_len = 0x10, flags = 0x02.
                let last_entry_len = u16::from_le_bytes([buf[32 + 8], buf[32 + 9]]);
                assert_eq!(last_entry_len, 0x10, "$R LAST entry length");
                let last_flags =
                    u32::from_le_bytes([buf[32 + 12], buf[32 + 13], buf[32 + 14], buf[32 + 15]]);
                assert_eq!(last_flags & 0x02, 0x02, "$R LAST sentinel flags");
                found_r_index_root = true;
            }
            (NtfsAttributeType::Data, _) => {
                // S4 explicitly defaults to no separate $DATA stream
                // for $Reparse — the Iter H trace shows chkdsk opens
                // `$Extend\$Reparse:$R:$INDEX_ALLOCATION`, not
                // `$Extend\$Reparse:$R` itself.
                seen_data_stream = true;
            }
            _ => {}
        }
    }
    assert!(
        found_r_index_root,
        "rec 16 missing named $INDEX_ROOT \"$R\""
    );
    assert!(
        !seen_data_stream,
        "rec 16 must not carry any $DATA stream at S4"
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
