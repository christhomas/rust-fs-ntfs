//! Shared test helpers for fs-ntfs integration tests.
//!
//! Tests open generated fixtures under `test-disks/` raw (via the `ntfs`
//! crate the way fs_ntfs uses it internally). No NTFS driver / FUSE / kernel
//! driver is involved on the test side.

// Test binaries compile this module once each and only use a subset of
// helpers, so per-binary dead-code warnings are expected and meaningless.
#![allow(dead_code)]

use std::cell::RefCell;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::NtfsFileNamespace;
use ntfs::{Ntfs, NtfsFile, NtfsReadSeek};

pub type Reader = BufReader<File>;

/// Paths of generated images owned by the current test worker.
///
/// The registry's destructor runs when the worker exits, including after a
/// panicking test. Keeping the guard here rather than in each fixture builder
/// also lets builders continue returning `String` without dropping the guard
/// before their caller uses the image.
struct TempImages(RefCell<Vec<PathBuf>>);

impl Drop for TempImages {
    fn drop(&mut self) {
        for path in self.0.get_mut().drain(..).rev() {
            let _ = std::fs::remove_file(path);
        }
    }
}

thread_local! {
    static TEMP_IMAGES: TempImages = const { TempImages(RefCell::new(Vec::new())) };
}

/// Allocate a collision-safe, panic-cleaned image path under `test-disks/`.
pub fn temp_image_path(stem: impl AsRef<str>) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let stem = stem.as_ref();
    assert!(!stem.is_empty(), "temporary image stem must not be empty");
    assert!(
        stem.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
        "temporary image stem contains a path separator or unsupported byte: {stem:?}"
    );

    std::fs::create_dir_all("test-disks").expect("create test-disks directory");
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(format!(
        "test-disks/_{stem}_{}_{}.img",
        std::process::id(),
        serial
    ));
    TEMP_IMAGES.with(|images| images.0.borrow_mut().push(path.clone()));
    path.into_os_string()
        .into_string()
        .expect("temporary image path is UTF-8")
}

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
