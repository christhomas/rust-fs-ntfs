//! Shared test helpers for fs-ntfs integration tests.
//!
//! Tests open generated fixtures under `test-disks/` raw (via the `ntfs`
//! crate the way fs_ntfs uses it internally). No ntfs-3g / FUSE / kernel
//! driver is involved on the test side.

// Test binaries compile this module once each and only use a subset of
// helpers, so per-binary dead-code warnings are expected and meaningless.
#![allow(dead_code)]

use std::fs::File;
use std::io::BufReader;

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::NtfsFileNamespace;
use ntfs::{Ntfs, NtfsFile, NtfsReadSeek};

pub type Reader = BufReader<File>;

pub fn open(path: &str) -> (Ntfs, Reader) {
    let f = File::open(path).unwrap_or_else(|e| {
        panic!("open {path}: {e} (did you run test-disks/build-ntfs-feature-images.sh?)")
    });
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("parse NTFS");
    ntfs.read_upcase_table(&mut reader)
        .expect("read upcase table");
    (ntfs, reader)
}

pub fn navigate<'n>(ntfs: &'n Ntfs, reader: &mut Reader, path: &str) -> NtfsFile<'n> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return ntfs.root_directory(reader).expect("root directory");
    }
    let mut current = ntfs.root_directory(reader).expect("root directory");
    for component in path.split('/') {
        if component.is_empty() {
            continue;
        }
        let index = current
            .directory_index(reader)
            .unwrap_or_else(|e| panic!("directory_index for '{component}': {e}"));
        let mut finder = index.finder();
        let entry = NtfsFileNameIndex::find(&mut finder, ntfs, reader, component)
            .unwrap_or_else(|| panic!("not found: '{component}'"))
            .unwrap_or_else(|e| panic!("find '{component}': {e}"));
        current = entry
            .to_file(ntfs, reader)
            .unwrap_or_else(|e| panic!("to_file '{component}': {e}"));
    }
    current
}

pub fn list_names(ntfs: &Ntfs, reader: &mut Reader, path: &str) -> Vec<String> {
    let dir = navigate(ntfs, reader, path);
    let index = dir.directory_index(reader).expect("directory_index");
    let mut iter = index.entries();
    let mut names = Vec::new();
    while let Some(entry) = iter.next(reader) {
        let entry = entry.expect("entry");
        let file_name = match entry.key() {
            Some(Ok(n)) => n,
            _ => continue,
        };
        // Skip DOS-only names (auto-generated 8.3 duplicates).
        if file_name.namespace() == NtfsFileNamespace::Dos {
            continue;
        }
        names.push(file_name.name().to_string_lossy());
    }
    names
}

pub fn read_file_all(ntfs: &Ntfs, reader: &mut Reader, path: &str) -> Vec<u8> {
    let file = navigate(ntfs, reader, path);
    let data_item = file
        .data(reader, "")
        .expect("no $DATA attribute")
        .expect("data attribute error");
    let data_attr = data_item.to_attribute().expect("to_attribute");
    let mut data_value = data_attr.value(reader).expect("attribute value");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = data_value.read(reader, &mut chunk).expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    buf
}
