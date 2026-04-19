//! Hard-link creation tests (§3.1).

use fs_ntfs::write;
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_link_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn hard_link_count(img: &str, path: &str) -> u16 {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    cur.hard_link_count()
}

fn read_data(img: &str, path: &str) -> Vec<u8> {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
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
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = v.read(&mut r, &mut chunk).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        return out;
    }
    panic!("no $DATA");
}

#[test]
fn link_increments_hard_link_count() {
    let img = working_copy("hlcount");
    // mkdir first so the new parent is a freshly created (overflow-free) dir.
    write::mkdir(Path::new(&img), "/", "linkdir").unwrap();
    assert_eq!(hard_link_count(&img, "/hello.txt"), 1);
    write::link(Path::new(&img), "/hello.txt", "/linkdir", "hello_link").unwrap();
    assert_eq!(hard_link_count(&img, "/hello.txt"), 2);
}

#[test]
fn link_new_path_reads_same_data() {
    let img = working_copy("samedata");
    write::mkdir(Path::new(&img), "/", "d").unwrap();
    let original = read_data(&img, "/hello.txt");
    write::link(Path::new(&img), "/hello.txt", "/d", "linked.txt").unwrap();
    let viad = read_data(&img, "/d/linked.txt");
    assert_eq!(original, viad);
}

#[test]
fn link_refuses_directory() {
    let img = working_copy("nodir");
    write::mkdir(Path::new(&img), "/", "a").unwrap();
    write::mkdir(Path::new(&img), "/", "b").unwrap();
    let err = write::link(Path::new(&img), "/a", "/b", "aliased").unwrap_err();
    assert!(err.contains("directory"), "err={err}");
}

#[test]
fn link_rejects_duplicate_name() {
    let img = working_copy("dup");
    write::mkdir(Path::new(&img), "/", "d").unwrap();
    write::link(Path::new(&img), "/hello.txt", "/d", "x").unwrap();
    let err = write::link(Path::new(&img), "/hello.txt", "/d", "x").unwrap_err();
    assert!(err.contains("already exists"), "err={err}");
}

#[test]
fn link_rejects_invalid_basename() {
    let img = working_copy("badname");
    write::mkdir(Path::new(&img), "/", "d").unwrap();
    assert!(write::link(Path::new(&img), "/hello.txt", "/d", "").is_err());
    assert!(write::link(Path::new(&img), "/hello.txt", "/d", ".").is_err());
    assert!(write::link(Path::new(&img), "/hello.txt", "/d", "a/b").is_err());
}
