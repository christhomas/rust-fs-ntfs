//! Phase 2 field-exhaustion tests for the `$FILE_NAME` attribute.
//!
//! Verifies that specific on-disk fields (data_size, allocated_size,
//! file_attributes, namespace, name) carry the expected value after write
//! operations. Tests format a fresh temp-file volume, populate it via our
//! write API, then read back via the upstream `ntfs` crate (independent of
//! our own parsers) using raw attribute value bytes.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

/// Format a fresh NTFS volume into a temp image file and return its path.
fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_fex_fn_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create temp image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("FEXTEST"),
        Some(0xFEEF_FACE),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Parsed subset of a `$FILE_NAME` attribute value's fields.
#[derive(Debug)]
struct FnFields {
    allocated_size: u64,
    data_size: u64,
    file_attributes: u32,
    namespace: u8,
    name: String,
}

/// Navigate to `file_path` and return all `$FILE_NAME` attribute field tuples.
/// Reads raw attribute value bytes (layout per MS-FSCC §2.4.4) to avoid
/// the `NtfsStructuredValueFromResidentAttributeValue` trait bound on `NtfsFileName`.
fn read_fn_fields(img: &str, file_path: &str) -> Vec<FnFields> {
    let f = std::fs::File::open(img).expect("open image");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut reader).expect("upcase");

    let mut cur = ntfs.root_directory(&mut reader).expect("root dir");
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut reader).expect("dir_index");
        let mut finder = idx.finder();
        let entry = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .expect("find result")
            .expect("find ok");
        cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
    }

    let mut out = Vec::new();
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let a = item.to_attribute().expect("to_attribute");
        if a.ty().ok() != Some(NtfsAttributeType::FileName) {
            continue;
        }
        // Read the raw resident value bytes.
        let mut val = a.value(&mut reader).expect("value");
        let mut bytes = vec![0u8; val.len() as usize];
        let mut off = 0usize;
        while off < bytes.len() {
            let n = val.read(&mut reader, &mut bytes[off..]).expect("read attr");
            if n == 0 {
                break;
            }
            off += n;
        }

        // $FILE_NAME value layout (MS-FSCC §2.4.4):
        //   +0x00 parent_ref    (u64)
        //   +0x08 creation      (u64)
        //   +0x10 modification  (u64)
        //   +0x18 mft_mod       (u64)
        //   +0x20 access        (u64)
        //   +0x28 allocated_sz  (u64)
        //   +0x30 data_sz       (u64)
        //   +0x38 file_attrs    (u32)
        //   +0x3C ea/reparse    (u32)
        //   +0x40 name_len      (u8, UTF-16 code units)
        //   +0x41 namespace     (u8)
        //   +0x42+ name         (UTF-16 LE)
        if bytes.len() < 0x42 {
            continue;
        }
        let allocated_size = u64::from_le_bytes(bytes[0x28..0x30].try_into().unwrap());
        let data_size = u64::from_le_bytes(bytes[0x30..0x38].try_into().unwrap());
        let file_attributes = u32::from_le_bytes(bytes[0x38..0x3C].try_into().unwrap());
        let name_len = bytes[0x40] as usize; // UTF-16 code units
        let namespace = bytes[0x41];
        let name_bytes = &bytes[0x42..0x42 + name_len * 2];
        let u16s: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&u16s).to_string();

        out.push(FnFields {
            allocated_size,
            data_size,
            file_attributes,
            namespace,
            name,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// data_size field
// ---------------------------------------------------------------------------

// DIVERGENCE (not a confirmed bug): write_file_contents() leaves
// $FILE_NAME.data_size stale after a write. The authoritative size lives
// in the $DATA attribute; $FILE_NAME carries a *denormalised* copy that
// NTFS updates lazily — per write.rs:52, Windows itself only refreshes
// the $FILE_NAME duplicates on rename/create, not on every write. This
// test asserts EAGER sync, which may NOT match Windows. It stays
// #[ignore] until the Windows VM + chkdsk confirms whether a stale
// $FILE_NAME.data_size is actually flagged. Do NOT "fix" the write path
// to satisfy this test without that verification. (test-expansion-plan §2.2)
#[ignore = "pending Windows-matrix verification: $FILE_NAME.data_size lazy vs eager update — see comment"]
#[test]
fn data_size_matches_written_byte_count() {
    let img = fresh_vol("datasize");
    write::create_file(Path::new(&img), "/", "data.bin").expect("create");
    let payload: Vec<u8> = (0u8..200).collect();
    write::write_file_contents(Path::new(&img), "/data.bin", &payload).expect("write");

    let entries = read_fn_fields(&img, "/data.bin");
    assert!(!entries.is_empty(), "must have at least one $FILE_NAME");
    assert_eq!(
        entries[0].data_size, 200,
        "data_size must equal written byte count"
    );
}

#[test]
fn data_size_zero_for_empty_file() {
    let img = fresh_vol("datasize_zero");
    write::create_file(Path::new(&img), "/", "empty.bin").expect("create");

    let entries = read_fn_fields(&img, "/empty.bin");
    assert!(!entries.is_empty());
    assert_eq!(
        entries[0].data_size, 0,
        "data_size must be 0 for empty file"
    );
}

// ---------------------------------------------------------------------------
// allocated_size field
// ---------------------------------------------------------------------------

// DIVERGENCE (not a confirmed bug): write_file_contents() leaves
// $FILE_NAME.allocated_size / data_size stale after a write — same
// lazy-vs-eager $FILE_NAME question as data_size_matches_written_byte_count
// above. Windows updates these denormalised copies lazily (write.rs:52).
// #[ignore] until the Windows VM + chkdsk confirms eager sync is required.
#[ignore = "pending Windows-matrix verification: $FILE_NAME size fields lazy vs eager update"]
#[test]
fn allocated_size_is_cluster_multiple_for_nonresident_file() {
    let img = fresh_vol("allocsize_nonres");
    write::create_file(Path::new(&img), "/", "big.bin").expect("create");
    // 6000 bytes forces non-resident at 4096-byte cluster size.
    let payload = vec![0xABu8; 6000];
    write::write_file_contents(Path::new(&img), "/big.bin", &payload).expect("write");

    let entries = read_fn_fields(&img, "/big.bin");
    assert!(!entries.is_empty());
    let alloc = entries[0].allocated_size;
    let data = entries[0].data_size;
    assert_eq!(data, 6000, "data_size must be 6000");
    assert!(
        alloc >= data,
        "allocated_size {alloc} must be >= data_size {data}"
    );
    assert_eq!(
        alloc % CLUSTER as u64,
        0,
        "allocated_size {alloc} must be a cluster multiple"
    );
}

#[test]
fn allocated_size_zero_for_resident_file() {
    let img = fresh_vol("allocsize_res");
    write::create_file(Path::new(&img), "/", "tiny.txt").expect("create");
    write::write_file_contents(Path::new(&img), "/tiny.txt", b"hi").expect("write");

    let entries = read_fn_fields(&img, "/tiny.txt");
    assert!(!entries.is_empty());
    // Resident files: content in MFT record — no clusters allocated.
    assert_eq!(
        entries[0].allocated_size, 0,
        "resident file must have allocated_size = 0 (got {})",
        entries[0].allocated_size
    );
}

// ---------------------------------------------------------------------------
// file_attributes field
// ---------------------------------------------------------------------------

#[test]
fn file_attribute_archive_set_on_new_file() {
    let img = fresh_vol("attr_archive");
    write::create_file(Path::new(&img), "/", "newfile.txt").expect("create");

    let entries = read_fn_fields(&img, "/newfile.txt");
    assert!(!entries.is_empty());
    let fa = entries[0].file_attributes;
    // FILE_ATTRIBUTE_ARCHIVE = 0x20
    assert_ne!(
        fa & 0x20,
        0,
        "ARCHIVE bit must be set on a new file; fa={fa:#x}"
    );
}

#[test]
fn directory_flag_set_in_file_name_for_dirs() {
    let img = fresh_vol("attr_dir");
    write::mkdir(Path::new(&img), "/", "mydir").expect("mkdir");

    let entries = read_fn_fields(&img, "/mydir");
    assert!(!entries.is_empty(), "directory must have $FILE_NAME");
    let fa = entries[0].file_attributes;
    // FA_NTFS_DIRECTORY = 0x10000000 (per record_build.rs and MS-FSCC §2.6).
    // This is the NTFS on-disk flag for "file has an index" — set for all dirs.
    assert_ne!(
        fa & 0x1000_0000,
        0,
        "FA_NTFS_DIRECTORY (0x10000000) must be set in $FILE_NAME for a directory; fa={fa:#x}"
    );
}

// DIVERGENCE (not a confirmed bug): set_file_attributes() updates
// $STANDARD_INFORMATION (the authoritative copy) but leaves the
// denormalised $FILE_NAME.file_attributes copy stale. Per write.rs:52
// Windows refreshes $FILE_NAME duplicates lazily (rename/create), so a
// stale copy here may match Windows. #[ignore] until the Windows VM +
// chkdsk confirms whether eager propagation is required. (plan §2.2)
#[ignore = "pending Windows-matrix verification: $FILE_NAME.file_attributes lazy vs eager update"]
#[test]
fn readonly_attribute_reflected_in_file_name() {
    let img = fresh_vol("attr_ro");
    write::create_file(Path::new(&img), "/", "ro.txt").expect("create");
    write::set_file_attributes(
        Path::new(&img),
        "/ro.txt",
        write::FileAttributesChange {
            add: write::file_attr::READONLY,
            remove: 0,
        },
    )
    .expect("set_attrs");

    let entries = read_fn_fields(&img, "/ro.txt");
    assert!(!entries.is_empty());
    let fa = entries[0].file_attributes;
    // FILE_ATTRIBUTE_READONLY = 0x01
    assert_ne!(fa & 0x01, 0, "READONLY must be in $FILE_NAME; fa={fa:#x}");
}

// ---------------------------------------------------------------------------
// namespace field
// ---------------------------------------------------------------------------

#[test]
fn short_8_3_name_uses_win32_and_dos_namespace() {
    let img = fresh_vol("ns_w32dos");
    write::create_file(Path::new(&img), "/", "hi.txt").expect("create");

    let entries = read_fn_fields(&img, "/hi.txt");
    assert!(!entries.is_empty());
    // WIN32_AND_DOS namespace = 3
    let has_w32dos = entries.iter().any(|e| e.namespace == 3);
    assert!(
        has_w32dos,
        "8.3-compatible name must use WIN32_AND_DOS namespace; got {entries:?}"
    );
}

#[test]
fn long_name_does_not_use_win32_and_dos_namespace() {
    let img = fresh_vol("ns_long");
    // "longfilename.txt" is > 8.3 so must not get WIN32_AND_DOS.
    write::create_file(Path::new(&img), "/", "longfilename.txt").expect("create");

    let entries = read_fn_fields(&img, "/longfilename.txt");
    assert!(!entries.is_empty());
    let all_ns: Vec<u8> = entries.iter().map(|e| e.namespace).collect();
    assert!(
        !all_ns.iter().all(|&ns| ns == 3),
        "long name must NOT use WIN32_AND_DOS-only namespace; got {all_ns:?}"
    );
    // Must have at least one WIN32 (1) or POSIX (0) namespace.
    assert!(
        all_ns.iter().any(|&ns| ns == 0 || ns == 1),
        "long name must use WIN32 or POSIX namespace; got {all_ns:?}"
    );
}

// ---------------------------------------------------------------------------
// name length bounds
// ---------------------------------------------------------------------------

#[test]
fn single_char_filename_name_field_correct() {
    let img = fresh_vol("name_1char");
    write::create_file(Path::new(&img), "/", "x").expect("create 1-char file");

    let entries = read_fn_fields(&img, "/x");
    assert!(!entries.is_empty());
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"x"),
        "at least one $FILE_NAME must carry name 'x'; got {names:?}"
    );
}

#[test]
fn max_filename_255_chars_roundtrips() {
    let img = fresh_vol("name_255");
    let long_name: String = "a".repeat(255);
    write::create_file(Path::new(&img), "/", &long_name).expect("create 255-char file");

    let entries = read_fn_fields(&img, &format!("/{long_name}"));
    assert!(
        !entries.is_empty(),
        "255-char filename must have $FILE_NAME entries"
    );
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.iter().any(|&n| n == long_name),
        "at least one $FILE_NAME must carry the 255-char name"
    );
}
