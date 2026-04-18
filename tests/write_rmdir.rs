//! Tests for `write::rmdir`.

use fs_ntfs::write;
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_rmdir_{tag}.img");
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
fn rmdir_removes_empty_directory() {
    let img = working_copy("basic");
    write::mkdir(Path::new(&img), "/Documents", "todel").expect("mkdir");
    assert!(list_dir(&img, "/Documents").iter().any(|n| n == "todel"));

    write::rmdir(Path::new(&img), "/Documents/todel").expect("rmdir");
    assert!(!list_dir(&img, "/Documents").iter().any(|n| n == "todel"));
}

#[test]
fn rmdir_refuses_nonempty() {
    let img = working_copy("nonempty");
    write::mkdir(Path::new(&img), "/Documents", "full").expect("mkdir");
    write::create_file(Path::new(&img), "/Documents/full", "inside.txt").expect("create");

    let err = write::rmdir(Path::new(&img), "/Documents/full").unwrap_err();
    assert!(err.contains("not empty"), "{err:?}");
    // Entry must still be there.
    assert!(list_dir(&img, "/Documents").iter().any(|n| n == "full"));
}

#[test]
fn rmdir_refuses_regular_file() {
    let img = working_copy("refuse_file");
    let err = write::rmdir(Path::new(&img), "/Documents/readme.txt").unwrap_err();
    assert!(err.contains("not a directory"), "{err:?}");
}

#[test]
fn rmdir_then_remount() {
    let img = working_copy("remount");
    write::mkdir(Path::new(&img), "/Documents", "short").expect("mkdir");
    write::rmdir(Path::new(&img), "/Documents/short").expect("rmdir");

    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}

#[test]
fn mkdir_rmdir_roundtrip_doesnt_leak_records() {
    // Create + delete the same dir 5 times; the MFT bitmap should end
    // up in the same state it started.
    use fs_ntfs::mft_bitmap;
    let img = working_copy("roundtrip");
    let mbm_before = mft_bitmap::locate(Path::new(&img)).unwrap();
    let free_count_before = count_free(&img, &mbm_before);

    for _ in 0..5 {
        write::mkdir(Path::new(&img), "/Documents", "churn").expect("mkdir");
        write::rmdir(Path::new(&img), "/Documents/churn").expect("rmdir");
    }

    let mbm_after = mft_bitmap::locate(Path::new(&img)).unwrap();
    let free_count_after = count_free(&img, &mbm_after);
    assert_eq!(free_count_before, free_count_after, "MFT records leaked");
}

fn count_free(img: &str, bm: &fs_ntfs::mft_bitmap::MftBitmap) -> u64 {
    let total = match &bm.layout {
        fs_ntfs::mft_bitmap::MftBitmapLayout::Resident { total_bits, .. } => *total_bits,
        fs_ntfs::mft_bitmap::MftBitmapLayout::NonResident { total_bits, .. } => *total_bits,
    };
    let mut free = 0u64;
    for n in 0..total {
        if !fs_ntfs::mft_bitmap::is_allocated(Path::new(img), bm, n).unwrap() {
            free += 1;
        }
    }
    free
}
