//! Cross-checks for the native read layer (`fs_ntfs::read`) against the
//! upstream `ntfs` crate, using **Windows-authored** fixtures that our own
//! write path cannot produce (compressed `$DATA`, `$ATTRIBUTE_LIST` overflow).
//!
//! Fixtures live in `test-disks/` and are intentionally not committed (large,
//! Windows-generated). Each test skips with a notice if its fixture is absent,
//! so a fresh checkout still runs green — the fixtures are produced on the
//! Windows VM (see the native-read-layer plan).

use fs_ntfs::attr_io::AttrType;
use fs_ntfs::block_io::PathIo;
use fs_ntfs::read::{parse_attribute_list, read_attribute_value, resolve_path};
use ntfs::{Ntfs, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const ATTRLIST_IMG: &str = "test-disks/ntfs-attrlist.img";
const COMPRESSED_IMG: &str = "test-disks/ntfs-compressed.img";

/// Open a fixture read-only, or return `None` (with a skip notice) if absent.
fn open_fixture(path: &str) -> Option<PathIo> {
    if !Path::new(path).exists() {
        eprintln!("SKIP: fixture {path} not present (generate on the Windows VM)");
        return None;
    }
    Some(PathIo::open_ro(Path::new(path)).expect("open_ro fixture"))
}

/// Upstream oracle: read named `$DATA` stream `stream` of `path`.
fn upstream_read_named_data(img: &str, path: &str, stream: &str) -> Vec<u8> {
    let file = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(file);
    let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let mut cur = ntfs.root_directory(&mut reader).expect("root");
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        let index = cur.directory_index(&mut reader).expect("dir index");
        let mut finder = index.finder();
        let entry = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .expect("entry present")
            .expect("entry ok");
        cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
    }
    let item = cur
        .data(&mut reader, stream)
        .unwrap_or_else(|| panic!("no $DATA:{stream}"))
        .expect("data item");
    let attr = item.to_attribute().expect("attr");
    let mut value = attr.value(&mut reader).expect("value");
    let mut out = vec![0u8; value.len() as usize];
    let mut filled = 0usize;
    while filled < out.len() {
        let n = value.read(&mut reader, &mut out[filled..]).expect("read");
        if n == 0 {
            break;
        }
        filled += n;
    }
    out.truncate(filled);
    out
}

#[test]
fn attribute_list_ads_in_extension_record_matches_upstream() {
    let Some(mut io) = open_fixture(ATTRLIST_IMG) else {
        return;
    };
    let rec = resolve_path(&mut io, "/many.bin").expect("resolve /many.bin");

    // Parse the file's $ATTRIBUTE_LIST and find a *named* $DATA stream whose
    // instance lives in an extension record (record != base) — i.e. one that
    // genuinely overflowed and can only be read by following the list.
    let al = read_attribute_value(&mut io, rec, AttrType::AttributeList, None)
        .expect("read $ATTRIBUTE_LIST value");
    let entries = parse_attribute_list(&al).expect("parse $ATTRIBUTE_LIST");
    let overflow = entries
        .iter()
        .find(|e| {
            e.type_code == AttrType::Data as u32 && e.name.is_some() && e.record_number != rec
        })
        .expect("a named $DATA stream living in an extension record");
    let stream = overflow.name.clone().unwrap();
    eprintln!(
        "testing ADS '{stream}' held in extension record {} (base {rec})",
        overflow.record_number
    );

    // Native read must follow $ATTRIBUTE_LIST into that extension record.
    let native = read_attribute_value(&mut io, rec, AttrType::Data, Some(&stream))
        .expect("native read of overflowed ADS");
    let oracle = upstream_read_named_data(ATTRLIST_IMG, "/many.bin", &stream);

    assert!(!native.is_empty(), "stream should have content");
    assert_eq!(
        native, oracle,
        "native vs upstream mismatch for overflowed ADS '{stream}'"
    );
}

#[test]
fn many_named_streams_all_match_upstream() {
    let Some(mut io) = open_fixture(ATTRLIST_IMG) else {
        return;
    };
    let rec = resolve_path(&mut io, "/many.bin").expect("resolve");
    let al = read_attribute_value(&mut io, rec, AttrType::AttributeList, None).expect("attrlist");
    let entries = parse_attribute_list(&al).expect("parse");

    // Every named $DATA stream (base or extension, starting_vcn 0) must read
    // back byte-identical to upstream.
    let mut checked = 0usize;
    for e in entries
        .iter()
        .filter(|e| e.type_code == AttrType::Data as u32 && e.name.is_some() && e.starting_vcn == 0)
    {
        let stream = e.name.as_ref().unwrap();
        let native = read_attribute_value(&mut io, rec, AttrType::Data, Some(stream))
            .unwrap_or_else(|err| panic!("native read of '{stream}': {err}"));
        let oracle = upstream_read_named_data(ATTRLIST_IMG, "/many.bin", stream);
        assert_eq!(native, oracle, "mismatch for stream '{stream}'");
        checked += 1;
    }
    assert!(checked > 0, "expected at least one named stream");
    eprintln!("cross-checked {checked} named $DATA streams against upstream");
}

#[test]
fn compressed_data_decompresses_to_known_content() {
    let Some(mut io) = open_fixture(COMPRESSED_IMG) else {
        return;
    };
    let rec = resolve_path(&mut io, "/comp.txt").expect("resolve /comp.txt");

    // \comp.txt is a Windows-compressed file (LZNT1) of exactly 200000 bytes
    // of the repeating ASCII pattern "ABC" (byte[i] = "ABC"[i % 3]). Upstream
    // ntfs 0.4 cannot decompress, so the *known original* is the oracle.
    let expected: Vec<u8> = (0..200_000usize).map(|i| b"ABC"[i % 3]).collect();

    let native = read_attribute_value(&mut io, rec, AttrType::Data, None)
        .expect("native read of compressed $DATA");

    assert_eq!(native.len(), expected.len(), "decompressed length");
    assert_eq!(native, expected, "decompressed content mismatch");
}
