// Integration test: verify the ntfs crate works against a real NTFS image.
// Tests the Rust layer directly (not via C FFI, since staticlib can't be linked to tests).

use std::fs::File;
use std::io::{BufReader, Read, Seek};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace, NtfsStandardInformation};
use ntfs::{Ntfs, NtfsAttributeType, NtfsFile, NtfsReadSeek};

const TEST_IMAGE: &str = "/tmp/test_ntfs.img";

fn open_ntfs() -> (Ntfs, BufReader<File>) {
    let f = File::open(TEST_IMAGE).expect("open test image");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("parse NTFS");
    ntfs.read_upcase_table(&mut reader).expect("read upcase table");
    (ntfs, reader)
}

fn navigate<'n>(
    ntfs: &'n Ntfs,
    reader: &mut BufReader<File>,
    path: &str,
) -> NtfsFile<'n> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return ntfs.root_directory(reader).unwrap();
    }

    let mut current = ntfs.root_directory(reader).unwrap();
    for component in path.split('/') {
        if component.is_empty() {
            continue;
        }
        let index = current.directory_index(reader).unwrap();
        let mut finder = index.finder();
        let entry = NtfsFileNameIndex::find(&mut finder, ntfs, reader, component)
            .unwrap_or_else(|| panic!("not found: '{component}'"))
            .unwrap();
        current = entry.to_file(ntfs, reader).unwrap();
    }
    current
}

#[test]
fn test_volume_info() {
    let (ntfs, mut reader) = open_ntfs();

    println!("Cluster size: {}", ntfs.cluster_size());
    println!("Size: {}", ntfs.size());
    println!("Serial: {}", ntfs.serial_number());

    if let Some(Ok(name)) = ntfs.volume_name(&mut reader) {
        println!("Volume name: {}", name.name());
        assert_eq!(name.name().to_string_lossy(), "BasicNTFS");
    }

    let vol_info = ntfs.volume_info(&mut reader).unwrap();
    println!("NTFS version: {}.{}", vol_info.major_version(), vol_info.minor_version());
    assert!(vol_info.major_version() >= 3);
}

#[test]
fn test_root_directory() {
    let (ntfs, mut reader) = open_ntfs();

    let root = ntfs.root_directory(&mut reader).unwrap();
    let index = root.directory_index(&mut reader).unwrap();
    let mut iter = index.entries();

    let mut names = Vec::new();
    while let Some(entry) = iter.next(&mut reader) {
        let entry = entry.unwrap();
        let file_name = entry.key().unwrap().unwrap();

        // Skip DOS-only names
        if file_name.namespace() == NtfsFileNamespace::Dos {
            continue;
        }

        let name = file_name.name().to_string_lossy();
        let is_dir = file_name.is_directory();
        println!("  {} {}", if is_dir { "<DIR>" } else { "     " }, name);
        names.push(name);
    }

    assert!(
        names.iter().any(|n| n == "hello.txt"),
        "missing hello.txt, found: {:?}",
        names
    );
    assert!(
        names.iter().any(|n| n == "Documents"),
        "missing Documents, found: {:?}",
        names
    );
}

#[test]
fn test_stat_file() {
    let (ntfs, mut reader) = open_ntfs();

    let file = navigate(&ntfs, &mut reader, "/hello.txt");
    println!("hello.txt record number: {}", file.file_record_number());
    assert!(!file.is_directory());

    // Read StandardInformation
    let mut attributes = file.attributes();
    while let Some(item) = attributes.next(&mut reader) {
        let item = item.unwrap();
        let attr = item.to_attribute().unwrap();
        if attr.ty().unwrap() == NtfsAttributeType::StandardInformation {
            let std_info = attr
                .resident_structured_value::<NtfsStandardInformation>()
                .unwrap();
            println!("  Attributes: {}", std_info.file_attributes());
            println!(
                "  Creation time (NT): {}",
                std_info.creation_time().nt_timestamp()
            );
        }
        if attr.ty().unwrap() == NtfsAttributeType::Data {
            println!("  Data size: {}", attr.value_length());
            assert_eq!(attr.value_length(), 17); // "Hello from NTFS!\n"
        }
    }

    // Stat directory
    let dir = navigate(&ntfs, &mut reader, "/Documents");
    assert!(dir.is_directory());
}

#[test]
fn test_read_file_content() {
    let (ntfs, mut reader) = open_ntfs();

    let file = navigate(&ntfs, &mut reader, "/hello.txt");
    let data_item = file.data(&mut reader, "").unwrap().unwrap();
    let data_attr = data_item.to_attribute().unwrap();
    let mut data_value = data_attr.value(&mut reader).unwrap();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = data_value.read(&mut reader, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let content = String::from_utf8(buf).unwrap();
    println!("hello.txt content: {:?}", content);
    assert_eq!(content, "Hello from NTFS!\n");
}

#[test]
fn test_read_nested_file() {
    let (ntfs, mut reader) = open_ntfs();

    let file = navigate(&ntfs, &mut reader, "/Documents/readme.txt");
    let data_item = file.data(&mut reader, "").unwrap().unwrap();
    let data_attr = data_item.to_attribute().unwrap();
    let mut data_value = data_attr.value(&mut reader).unwrap();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = data_value.read(&mut reader, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let content = String::from_utf8(buf).unwrap();
    println!("Documents/readme.txt: {:?}", content);
    assert_eq!(content, "Test document content\n");
}

#[test]
fn test_subdirectory_listing() {
    let (ntfs, mut reader) = open_ntfs();

    let dir = navigate(&ntfs, &mut reader, "/Documents");
    let index = dir.directory_index(&mut reader).unwrap();
    let mut iter = index.entries();

    let mut names = Vec::new();
    while let Some(entry) = iter.next(&mut reader) {
        let entry = entry.unwrap();
        let file_name = entry.key().unwrap().unwrap();
        if file_name.namespace() == NtfsFileNamespace::Dos {
            continue;
        }
        let name = file_name.name().to_string_lossy();
        println!("  Documents/{}", name);
        names.push(name);
    }

    assert!(names.iter().any(|n| n == "notes.txt"));
    assert!(names.iter().any(|n| n == "readme.txt"));
}
