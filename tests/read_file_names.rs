//! Tests for `read_file_names` — list all $FILE_NAME attributes on a
//! file's MFT record. Complements `read_attributes` from the
//! diagnostic-helper family.

use fs_ntfs::write::{create_file, read_file_names};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_rfnames_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn fixture_file_has_at_least_one_filename() {
    let names = read_file_names(std::path::Path::new(BASIC_IMG), "/hello.txt").unwrap();
    assert!(!names.is_empty(), "every file must have ≥ 1 $FILE_NAME");
    // hello.txt fits DOS 8.3, so the fixture likely emits a single
    // WIN32_AND_DOS (namespace=3) FILE_NAME — but the qemu pipeline
    // is opaque enough that we only assert the name is reachable.
    let has_hello = names.iter().any(|n| n.name == "hello.txt");
    assert!(
        has_hello,
        "expected /hello.txt's name to appear; got {names:?}"
    );
}

#[test]
fn runtime_short_name_uses_win32_and_dos_namespace() {
    let img = working_copy("runtime_short");
    create_file(std::path::Path::new(&img), "/", "short.txt").unwrap();
    let names = read_file_names(std::path::Path::new(&img), "/short.txt").unwrap();
    // "short.txt" (5+3) fits DOS 8.3 → our runtime uses
    // WIN32_AND_DOS (3).
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].name, "short.txt");
    assert_eq!(
        names[0].namespace, 3,
        "DOS-8.3-fit names should use WIN32_AND_DOS"
    );
}

#[test]
fn runtime_long_name_uses_posix_namespace() {
    let img = working_copy("runtime_long");
    let long_name = "persistent_archive_with_long_filename.txt";
    create_file(std::path::Path::new(&img), "/", long_name).unwrap();
    let names = read_file_names(std::path::Path::new(&img), &format!("/{long_name}")).unwrap();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].name, long_name);
    assert_eq!(
        names[0].namespace, 0,
        "names exceeding DOS 8.3 should use POSIX (Iter N fix)"
    );
}

#[test]
fn parent_reference_points_at_root() {
    let img = working_copy("runtime_parent");
    create_file(std::path::Path::new(&img), "/", "child.bin").unwrap();
    let names = read_file_names(std::path::Path::new(&img), "/child.bin").unwrap();
    let parent_rec = names[0].parent_reference & 0xFFFF_FFFF_FFFF;
    // Root directory's MFT record number is 5.
    assert_eq!(parent_rec, 5, "expected parent_ref → rec 5 (root)");
}

#[test]
fn missing_file_errors() {
    let err = read_file_names(std::path::Path::new(BASIC_IMG), "/no_such.txt").unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn directory_file_attributes_carry_directory_bit() {
    use fs_ntfs::write::mkdir;
    let img = working_copy("runtime_dir_attrs");
    mkdir(std::path::Path::new(&img), "/", "subdir").unwrap();
    let names = read_file_names(std::path::Path::new(&img), "/subdir").unwrap();
    assert!(!names.is_empty());
    // FILE_ATTRIBUTE_DIRECTORY = 0x10000000 per MS-FSCC §2.6.
    assert_ne!(
        names[0].file_attributes & 0x10000000,
        0,
        "directory $FILE_NAME.file_attributes must carry the DIRECTORY bit"
    );
}
