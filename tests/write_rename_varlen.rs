//! Tests for variable-length `write::rename`.

use fs_ntfs::write;
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_rename_v_{tag}.img");
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

fn read_file(img: &str, path: &str) -> Vec<u8> {
    use ntfs::{NtfsAttributeType, NtfsReadSeek};
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
fn rename_shorter() {
    let img = working_copy("shorter");
    // readme.txt (10) → hi (2).
    write::rename(Path::new(&img), "/Documents/readme.txt", "hi").expect("rename");
    let names = list_dir(&img, "/Documents");
    assert!(names.iter().any(|n| n == "hi"), "{names:?}");
    assert!(!names.iter().any(|n| n == "readme.txt"), "{names:?}");
    assert_eq!(read_file(&img, "/Documents/hi"), b"Test document content\n");
}

#[test]
fn rename_longer() {
    let img = working_copy("longer");
    // readme.txt (10) → really-long-name.txt (19).
    write::rename(
        Path::new(&img),
        "/Documents/readme.txt",
        "really-long-name.txt",
    )
    .expect("rename");
    let names = list_dir(&img, "/Documents");
    assert!(
        names.iter().any(|n| n == "really-long-name.txt"),
        "{names:?}"
    );
}

#[test]
fn rename_variable_then_same_delegation() {
    // Exercise the same-length delegation path to rename_same_length.
    let img = working_copy("same_len_delegate");
    write::rename(Path::new(&img), "/Documents/readme.txt", "README.TXT").expect("rename"); // 10 == 10
    assert_eq!(
        read_file(&img, "/Documents/README.TXT"),
        b"Test document content\n"
    );
}

#[test]
fn rename_rejects_existing_target() {
    let img = working_copy("dup");
    let err = write::rename(Path::new(&img), "/Documents/readme.txt", "notes.txt").unwrap_err();
    assert!(err.contains("already exists"), "{err:?}");
}

#[test]
fn rename_rejects_invalid_basename() {
    let img = working_copy("invalid");
    for bad in [".", "..", "", "a/b"] {
        let err = write::rename(Path::new(&img), "/Documents/readme.txt", bad).unwrap_err();
        assert!(err.contains("invalid basename"), "bad={bad} → {err:?}");
    }
}

#[test]
fn rename_noop_on_same_name() {
    let img = working_copy("noop");
    write::rename(Path::new(&img), "/Documents/readme.txt", "readme.txt").expect("noop");
    assert_eq!(
        read_file(&img, "/Documents/readme.txt"),
        b"Test document content\n"
    );
}

#[test]
fn upstream_mounts_after_varlen_rename() {
    let img = working_copy("remount");
    write::rename(Path::new(&img), "/Documents/readme.txt", "r").expect("rename");
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
