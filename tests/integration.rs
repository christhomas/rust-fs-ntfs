//! Basic end-to-end: mount ntfs-basic.img, list root, stat + read files.
//! Matches fixture content produced by test-disks/_vm-builder.sh::build_basic.

mod common;

use ntfs::structured_values::NtfsStandardInformation;
use ntfs::NtfsAttributeType;

const IMG: &str = "test-disks/ntfs-basic.img";

#[test]
fn volume_info() {
    let (ntfs, mut reader) = common::open(IMG);

    println!(
        "cluster_size={} size={} serial={}",
        ntfs.cluster_size(),
        ntfs.size(),
        ntfs.serial_number()
    );

    let name = ntfs
        .volume_name(&mut reader)
        .expect("volume name present")
        .expect("volume name read");
    assert_eq!(name.name().to_string_lossy(), "BasicNTFS");

    let vol_info = ntfs.volume_info(&mut reader).expect("volume info");
    assert!(vol_info.major_version() >= 3);
}

#[test]
fn root_directory_listing() {
    let (ntfs, mut reader) = common::open(IMG);
    let names = common::list_names(&ntfs, &mut reader, "/");
    println!("root entries: {names:?}");
    assert!(
        names.iter().any(|n| n == "hello.txt"),
        "missing hello.txt: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "Documents"),
        "missing Documents: {names:?}"
    );
}

#[test]
fn stat_file_and_dir() {
    let (ntfs, mut reader) = common::open(IMG);

    let file = common::navigate(&ntfs, &mut reader, "/hello.txt");
    assert!(!file.is_directory());

    let mut attrs = file.attributes();
    let mut saw_data = false;
    let mut saw_std_info = false;
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("attr item");
        let attr = item.to_attribute().expect("to_attribute");
        match attr.ty().expect("attr ty") {
            NtfsAttributeType::StandardInformation => {
                let si = attr
                    .resident_structured_value::<NtfsStandardInformation>()
                    .expect("std info");
                assert!(si.creation_time().nt_timestamp() > 0);
                saw_std_info = true;
            }
            NtfsAttributeType::Data => {
                if attr.name().map(|n| n.is_empty()).unwrap_or(true) {
                    assert_eq!(attr.value_length(), 17, "hello.txt is 17 bytes");
                    saw_data = true;
                }
            }
            _ => {}
        }
    }
    assert!(saw_data && saw_std_info);

    let dir = common::navigate(&ntfs, &mut reader, "/Documents");
    assert!(dir.is_directory());
}

#[test]
fn read_root_file() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/hello.txt");
    assert_eq!(content, b"Hello from NTFS!\n");
}

#[test]
fn read_nested_file() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/Documents/readme.txt");
    assert_eq!(content, b"Test document content\n");
}

#[test]
fn subdirectory_listing() {
    let (ntfs, mut reader) = common::open(IMG);
    let names = common::list_names(&ntfs, &mut reader, "/Documents");
    assert!(names.iter().any(|n| n == "readme.txt"), "{names:?}");
    assert!(names.iter().any(|n| n == "notes.txt"), "{names:?}");
}
