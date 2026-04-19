//! Tests for `write::write_resident_contents` + round-trip with
//! create_file so newly-created files are immediately useful.

use fs_ntfs::write;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_write_res_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn read_data(img: &str, file_path: &str) -> Vec<u8> {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let mut v = a.value(&mut r).unwrap();
        v.seek(&mut r, std::io::SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0u8; a.value_length() as usize];
        let mut filled = 0;
        while filled < buf.len() {
            let n = v.read(&mut r, &mut buf[filled..]).unwrap();
            if n == 0 {
                break;
            }
            filled += n;
        }
        return buf;
    }
    panic!("no $DATA");
}

#[test]
fn create_then_write_resident_contents() {
    let img = working_copy("crw");
    write::create_file(Path::new(&img), "/Documents", "greet.txt").expect("create");
    let payload = b"Hello from a newly-created file.";
    let n = write::write_resident_contents(Path::new(&img), "/Documents/greet.txt", payload)
        .expect("write");
    assert_eq!(n, payload.len() as u64);

    let content = read_data(&img, "/Documents/greet.txt");
    assert_eq!(content, payload);
}

#[test]
fn write_resident_can_replace_existing_content() {
    // hello.txt already exists with 17 bytes of resident data. Replace
    // with a shorter payload.
    let img = working_copy("replace");
    let payload = b"REPLACED";
    write::write_resident_contents(Path::new(&img), "/Documents/readme.txt", payload)
        .expect("write");
    let content = read_data(&img, "/Documents/readme.txt");
    assert_eq!(content, payload);
}

#[test]
fn write_resident_can_expand_up_to_record_capacity() {
    let img = working_copy("expand");
    write::create_file(Path::new(&img), "/Documents", "big.txt").expect("create");
    let payload = vec![b'A'; 400]; // well under 1 KiB record but larger than a typical resident write
    write::write_resident_contents(Path::new(&img), "/Documents/big.txt", &payload).expect("write");
    assert_eq!(read_data(&img, "/Documents/big.txt"), payload);
}

#[test]
fn write_resident_rejects_nonresident_file() {
    // ntfs-large-file.img has a non-resident /big.bin. Resident write
    // should refuse.
    let img = "test-disks/_write_res_nonres.img".to_string();
    std::fs::copy("test-disks/ntfs-large-file.img", &img).unwrap();
    let err = write::write_resident_contents(Path::new(&img), "/big.bin", b"x").unwrap_err();
    assert!(
        err.contains("non-resident") || err.contains("resident"),
        "{err:?}"
    );
}

#[test]
fn upstream_mounts_after_write() {
    let img = working_copy("remount");
    write::create_file(Path::new(&img), "/Documents", "file.txt").expect("create");
    write::write_resident_contents(Path::new(&img), "/Documents/file.txt", b"survive me")
        .expect("write");

    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
