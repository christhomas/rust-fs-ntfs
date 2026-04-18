//! Round-trip tests for `write::set_file_attributes`.

use fs_ntfs::write::{self, file_attr, FileAttributesChange};
use ntfs::structured_values::NtfsStandardInformation;
use ntfs::{Ntfs, NtfsAttributeType};
use std::io::BufReader;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_write_attrs_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn read_file_attributes(img: &str, file_path: &str) -> u32 {
    let f = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("parse");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let mut cur = ntfs.root_directory(&mut reader).expect("root");
    for comp in file_path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut reader).expect("idx");
        let mut finder = idx.finder();
        let entry = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .expect("some")
            .expect("ok");
        cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::StandardInformation) {
            continue;
        }
        let si = a
            .resident_structured_value::<NtfsStandardInformation>()
            .expect("si");
        return si.file_attributes().bits();
    }
    panic!("no SI");
}

#[test]
fn add_readonly_bit() {
    let img = working_copy("add_ro");
    let before = read_file_attributes(&img, "/hello.txt");
    assert_eq!(before & file_attr::READONLY, 0, "must start non-readonly");

    write::set_file_attributes(
        std::path::Path::new(&img),
        "/hello.txt",
        FileAttributesChange {
            add: file_attr::READONLY,
            remove: 0,
        },
    )
    .expect("set");

    let after = read_file_attributes(&img, "/hello.txt");
    assert_ne!(after & file_attr::READONLY, 0, "READONLY should be set");
    assert_eq!(
        (after & !file_attr::READONLY),
        before,
        "only READONLY should have changed; before={before:#x} after={after:#x}"
    );
}

#[test]
fn remove_a_previously_set_bit() {
    let img = working_copy("remove_bit");
    // First add HIDDEN.
    write::set_file_attributes(
        std::path::Path::new(&img),
        "/hello.txt",
        FileAttributesChange {
            add: file_attr::HIDDEN,
            remove: 0,
        },
    )
    .expect("add");
    assert_ne!(
        read_file_attributes(&img, "/hello.txt") & file_attr::HIDDEN,
        0
    );

    // Then remove it.
    write::set_file_attributes(
        std::path::Path::new(&img),
        "/hello.txt",
        FileAttributesChange {
            add: 0,
            remove: file_attr::HIDDEN,
        },
    )
    .expect("remove");
    assert_eq!(
        read_file_attributes(&img, "/hello.txt") & file_attr::HIDDEN,
        0
    );
}

#[test]
fn multiple_bits_in_single_call() {
    let img = working_copy("multi");
    let before = read_file_attributes(&img, "/hello.txt");
    write::set_file_attributes(
        std::path::Path::new(&img),
        "/hello.txt",
        FileAttributesChange {
            add: file_attr::READONLY | file_attr::HIDDEN | file_attr::SYSTEM,
            remove: 0,
        },
    )
    .expect("set");
    let after = read_file_attributes(&img, "/hello.txt");
    assert_ne!(after & file_attr::READONLY, 0);
    assert_ne!(after & file_attr::HIDDEN, 0);
    assert_ne!(after & file_attr::SYSTEM, 0);
    // Any bits outside the three we set must be unchanged.
    let untouched_mask = !(file_attr::READONLY | file_attr::HIDDEN | file_attr::SYSTEM);
    assert_eq!(before & untouched_mask, after & untouched_mask);
}

#[test]
fn overlap_in_add_and_remove_is_rejected() {
    let img = working_copy("overlap");
    let err = write::set_file_attributes(
        std::path::Path::new(&img),
        "/hello.txt",
        FileAttributesChange {
            add: file_attr::READONLY,
            remove: file_attr::READONLY,
        },
    )
    .unwrap_err();
    assert!(err.contains("overlap"), "{err:?}");
}

#[test]
fn attributes_survive_remount() {
    let img = working_copy("survive");
    write::set_file_attributes(
        std::path::Path::new(&img),
        "/hello.txt",
        FileAttributesChange {
            add: file_attr::READONLY | file_attr::ARCHIVE,
            remove: 0,
        },
    )
    .expect("set");
    // Fresh open — re-parse from scratch.
    let after = read_file_attributes(&img, "/hello.txt");
    assert_ne!(after & file_attr::READONLY, 0);
    assert_ne!(after & file_attr::ARCHIVE, 0);
}

#[test]
fn attribute_write_is_isolated_to_target_file() {
    // Touching hello.txt attributes must not affect Documents/readme.txt.
    let img = working_copy("isolated");
    let before_other = read_file_attributes(&img, "/Documents/readme.txt");
    write::set_file_attributes(
        std::path::Path::new(&img),
        "/hello.txt",
        FileAttributesChange {
            add: file_attr::READONLY,
            remove: 0,
        },
    )
    .expect("set");
    let after_other = read_file_attributes(&img, "/Documents/readme.txt");
    assert_eq!(
        before_other, after_other,
        "unrelated file's attrs changed: before={before_other:#x} after={after_other:#x}"
    );
}
