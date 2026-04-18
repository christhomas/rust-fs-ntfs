//! Tests for create_file / mkdir / rename on parents that have
//! overflowed to `$INDEX_ALLOCATION` — specifically the root
//! directory, which mkntfs always lays out that way.

use fs_ntfs::write;
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_root_ops_{tag}.img");
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

#[test]
fn create_file_in_root() {
    let img = working_copy("create_file");
    let rec = write::create_file(Path::new(&img), "/", "root_child.txt").expect("create in /");
    assert!(rec > 0);
    let names = list_root(&img);
    assert!(names.iter().any(|n| n == "root_child.txt"), "{names:?}");
}

#[test]
fn mkdir_in_root() {
    let img = working_copy("mkdir");
    write::mkdir(Path::new(&img), "/", "newdir").expect("mkdir in /");
    let names = list_root(&img);
    assert!(names.iter().any(|n| n == "newdir"), "{names:?}");
}

#[test]
fn create_file_rejects_duplicate_in_root() {
    let img = working_copy("dup");
    write::create_file(Path::new(&img), "/", "dup.txt").expect("first");
    let err = write::create_file(Path::new(&img), "/", "dup.txt").unwrap_err();
    assert!(err.contains("already exists"), "{err:?}");
}

#[test]
fn create_in_root_preserves_existing_entries() {
    let img = working_copy("preserve");
    let before = list_root(&img);
    write::create_file(Path::new(&img), "/", "newbie.txt").expect("create");
    let after = list_root(&img);
    for n in &before {
        assert!(after.iter().any(|m| m == n), "lost {n}: {after:?}");
    }
    assert!(after.iter().any(|n| n == "newbie.txt"));
}

#[test]
fn create_then_unlink_in_root_leaves_no_trace() {
    let img = working_copy("create_unlink");
    write::create_file(Path::new(&img), "/", "ephemeral.txt").expect("create");
    assert!(list_root(&img).iter().any(|n| n == "ephemeral.txt"));
    write::unlink(Path::new(&img), "/ephemeral.txt").expect("unlink");
    assert!(!list_root(&img).iter().any(|n| n == "ephemeral.txt"));
}

#[test]
fn create_many_files_in_root() {
    // Stress-test the INDX-block insert — create 20 files, each should
    // go into the index without error. (Block splitting still TODO;
    // this catches accidentally-wrong sort-order or off-by-one in the
    // shift logic.)
    let img = working_copy("many");
    for i in 0..20 {
        let name = format!("f_{i:03}.txt");
        write::create_file(Path::new(&img), "/", &name).expect("create");
    }
    let names = list_root(&img);
    for i in 0..20 {
        let want = format!("f_{i:03}.txt");
        assert!(
            names.iter().any(|n| n == &want),
            "missing {want}; {names:?}"
        );
    }
}

#[test]
fn upstream_mounts_after_root_ops() {
    let img = working_copy("remount");
    write::create_file(Path::new(&img), "/", "r1.txt").unwrap();
    write::mkdir(Path::new(&img), "/", "r2").unwrap();
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
