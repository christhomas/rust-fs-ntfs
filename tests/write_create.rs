//! Tests for `write::create_file` (W3 MVP — parent must have a
//! resident-only `$INDEX_ROOT`).

use fs_ntfs::write;
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_create_{tag}.img");
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
fn create_file_in_subdirectory() {
    let img = working_copy("basic");
    let rec = write::create_file(Path::new(&img), "/Documents", "new.txt").expect("create");
    assert!(rec > 0);
    let names = list_dir(&img, "/Documents");
    assert!(
        names.iter().any(|n| n == "new.txt"),
        "missing new.txt: {names:?}"
    );
}

#[test]
fn create_file_preserves_existing_entries() {
    let img = working_copy("preserve");
    let before = list_dir(&img, "/Documents");
    write::create_file(Path::new(&img), "/Documents", "extra.txt").expect("create");
    let after = list_dir(&img, "/Documents");
    for n in &before {
        assert!(after.iter().any(|m| m == n), "lost {n}: {after:?}");
    }
    assert!(after.iter().any(|n| n == "extra.txt"));
    assert_eq!(after.len(), before.len() + 1);
}

#[test]
fn create_file_rejects_duplicate() {
    let img = working_copy("dup");
    write::create_file(Path::new(&img), "/Documents", "dup.txt").expect("first");
    let err = write::create_file(Path::new(&img), "/Documents", "dup.txt").unwrap_err();
    assert!(err.contains("already exists"), "{err:?}");
}

#[test]
fn create_file_rejects_invalid_basename() {
    let img = working_copy("invalid");
    for bad in [".", "..", "", "nested/name"] {
        let err = write::create_file(Path::new(&img), "/Documents", bad).unwrap_err();
        assert!(err.contains("invalid basename"), "{bad} → {err:?}");
    }
}

#[test]
fn create_file_rejects_nondirectory_parent() {
    let img = working_copy("nondir");
    let err = write::create_file(Path::new(&img), "/Documents/readme.txt", "x.txt").unwrap_err();
    assert!(
        err.contains("not a directory") || err.contains("directory"),
        "{err:?}"
    );
}

#[test]
fn create_file_rejects_root_in_this_mvp() {
    let img = working_copy("root_refused");
    let err = write::create_file(Path::new(&img), "/", "new.txt").unwrap_err();
    assert!(err.contains("overflow") || err.contains("MVP"), "{err:?}");
}

#[test]
fn upstream_reads_newly_created_empty_file() {
    let img = working_copy("e2e");
    write::create_file(Path::new(&img), "/Documents", "hi.txt").expect("create");

    // Upstream should find it and report 0-byte $DATA.
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let docs = ntfs.root_directory(&mut r).unwrap();
    let idx = docs.directory_index(&mut r).unwrap();
    let mut finder = idx.finder();
    let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, "Documents")
        .unwrap()
        .unwrap();
    let docs_file = e.to_file(&ntfs, &mut r).unwrap();
    let idx2 = docs_file.directory_index(&mut r).unwrap();
    let mut finder2 = idx2.finder();
    let e2 = ntfs::indexes::NtfsFileNameIndex::find(&mut finder2, &ntfs, &mut r, "hi.txt")
        .expect("hi.txt should be findable")
        .expect("index entry parseable");
    let file = e2.to_file(&ntfs, &mut r).expect("to_file");

    let mut attrs = file.attributes();
    let mut saw_data = false;
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() == Some(ntfs::NtfsAttributeType::Data) {
            assert_eq!(a.value_length(), 0);
            saw_data = true;
        }
    }
    assert!(saw_data, "new file has no $DATA");
}
