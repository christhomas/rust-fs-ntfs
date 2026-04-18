//! Tests for same-length file rename (W3 MVP).

use fs_ntfs::write;
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_rename_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn list_root(img: &str) -> Vec<String> {
    use ntfs::structured_values::NtfsFileNamespace;
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let root = ntfs.root_directory(&mut r).unwrap();
    let idx = root.directory_index(&mut r).unwrap();
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

fn read_file_contents(img: &str, path: &str) -> Vec<u8> {
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
fn rename_in_root_currently_fails_with_index_allocation_msg() {
    // Root dir on mkntfs-produced images has $INDEX_ALLOCATION spillover
    // even when small — the MVP walker only handles resident $INDEX_ROOT.
    // Document the limitation via a negative test; once the index_io
    // walker learns $INDEX_ALLOCATION (W3 follow-up), flip this to a
    // positive assertion.
    let img = working_copy("root_allocation_case");
    let err = write::rename_same_length(Path::new(&img), "/hello.txt", "world.txt").unwrap_err();
    assert!(
        err.contains("no entry") || err.contains("index"),
        "expected index-allocation limitation message; got {err:?}"
    );
}

#[test]
fn rename_same_length_in_subdir() {
    let img = working_copy("subdir");
    // /Documents/readme.txt → /Documents/README.TXT (10 chars → 10 chars)
    write::rename_same_length(Path::new(&img), "/Documents/readme.txt", "README.TXT")
        .expect("rename");

    // Confirm by reading via the new path.
    let content = read_file_contents(&img, "/Documents/README.TXT");
    assert_eq!(content, b"Test document content\n");
}

#[test]
fn rename_rejects_different_length() {
    let img = working_copy("diff_len");
    // /Documents/readme.txt is 10 chars; try renaming to 6 chars.
    let err =
        write::rename_same_length(Path::new(&img), "/Documents/readme.txt", "r.txt").unwrap_err();
    assert!(err.contains("same-length"), "{err:?}");
}

#[test]
fn rename_rejects_basename_with_slash() {
    let err = write::rename_same_length(
        Path::new(BASIC_IMG),
        "/Documents/readme.txt",
        "new/path.txt",
    )
    .unwrap_err();
    assert!(err.contains("basename"), "{err:?}");
}

#[test]
fn rename_rejects_missing_source() {
    let err = write::rename_same_length(
        Path::new(BASIC_IMG),
        "/Documents/nonexistent.xx",
        "newnameeeeee",
    )
    .unwrap_err();
    assert!(
        err.contains("not found") || err.contains("nonexistent"),
        "{err:?}"
    );
}

#[test]
fn rename_preserves_other_files_in_same_dir() {
    let img = working_copy("preserve");
    // readme.txt → NOTES4.TXT (10 → 10 chars).
    write::rename_same_length(Path::new(&img), "/Documents/readme.txt", "NOTES4.TXT")
        .expect("rename");
    // notes.txt must still be there.
    let content = read_file_contents(&img, "/Documents/notes.txt");
    assert_eq!(content, b"Some notes here.\n");
    // And the renamed file reads correctly.
    let renamed = read_file_contents(&img, "/Documents/NOTES4.TXT");
    assert_eq!(renamed, b"Test document content\n");
}

#[test]
fn upstream_mounts_after_rename() {
    let img = working_copy("remount");
    write::rename_same_length(Path::new(&img), "/Documents/readme.txt", "NOTES4.TXT")
        .expect("rename");

    // Fresh mount still works.
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
