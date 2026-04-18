//! Tests for `write::mkdir` (W3 MVP — parent must have resident-only
//! `$INDEX_ROOT`).

use fs_ntfs::write;
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_mkdir_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn list_dir(img: &str, path: &str) -> Vec<String> {
    use ntfs::structured_values::NtfsFileNamespace;
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
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    let idx = cur.directory_index(&mut r).unwrap();
    let mut it = idx.entries();
    let mut names = Vec::new();
    while let Some(entry) = it.next(&mut r) {
        let entry = entry.unwrap();
        if let Some(Ok(name)) = entry.key() {
            if name.namespace() == NtfsFileNamespace::Dos {
                continue;
            }
            names.push(name.name().to_string_lossy());
        }
    }
    names
}

#[test]
fn mkdir_creates_empty_directory() {
    let img = working_copy("basic");
    let rec = write::mkdir(Path::new(&img), "/Documents", "subdir").expect("mkdir");
    assert!(rec > 0);
    let names = list_dir(&img, "/Documents");
    assert!(names.iter().any(|n| n == "subdir"), "{names:?}");
}

#[test]
fn mkdir_new_directory_is_empty_and_listable() {
    let img = working_copy("listable");
    write::mkdir(Path::new(&img), "/Documents", "emptydir").expect("mkdir");
    let names = list_dir(&img, "/Documents/emptydir");
    assert!(names.is_empty(), "{names:?}");
}

#[test]
fn mkdir_rejects_duplicate() {
    let img = working_copy("dup");
    write::mkdir(Path::new(&img), "/Documents", "twice").expect("first");
    let err = write::mkdir(Path::new(&img), "/Documents", "twice").unwrap_err();
    assert!(err.contains("already exists"), "{err:?}");
}

#[test]
fn mkdir_then_create_file_inside() {
    // Full round-trip: make a new dir, then create a file inside it,
    // and read both back.
    let img = working_copy("nested");
    write::mkdir(Path::new(&img), "/Documents", "newdir").expect("mkdir");
    write::create_file(Path::new(&img), "/Documents/newdir", "inside.txt").expect("create");

    let names = list_dir(&img, "/Documents/newdir");
    assert!(names.iter().any(|n| n == "inside.txt"), "{names:?}");
}

#[test]
fn upstream_mounts_after_mkdir() {
    let img = working_copy("remount");
    write::mkdir(Path::new(&img), "/Documents", "mounttest").expect("mkdir");
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
