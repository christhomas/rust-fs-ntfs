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

// ---------------------------------------------------------------------------
// Fault injection
// ---------------------------------------------------------------------------

/// A `BlockIo` that passes everything through until a chosen write, and
/// fails that one and every write after it.
///
/// WHY THIS EXISTS. A rollback path can only be tested by reaching it,
/// and the errors these paths exist for — a device that stops taking
/// writes half way through an operation — cannot be produced by any
/// input to the driver. Without an I/O fault the "undo the allocation"
/// branches are unreachable code that a regression can delete with the
/// whole suite green, which is exactly what happened to the three
/// promotion paths in #251 and to the grow commit in #146.
///
/// It counts *writes*, not bytes or offsets, because that is the
/// coordinate a test can state without depending on the driver's
/// internal write order beyond its own arithmetic: "let the data land
/// and fail the record update" is `fail_write_number(n)` with `n`
/// measured once, asserted in the test, and visible in the failure
/// message when the order changes.
///
/// `size()` and `sync()` are honest pass-throughs. A `sync` that
/// silently succeeded after a failed write would model a device this
/// crate's ordering discipline assumes does not exist.
pub struct FailingIo<T> {
    inner: T,
    writes_seen: usize,
    fail_from: usize,
    fail_until: usize,
    /// Every write offset the wrapper has passed through, in order, so a
    /// test can say which write it chose and report it when the choice
    /// stops being the right one.
    pub write_log: Vec<u64>,
}

impl<T> FailingIo<T> {
    /// Fail the `n`th write (1-based) and every write after it.
    ///
    /// `usize::MAX` records the write log without ever failing, which is
    /// how a test measures the number to pass here.
    pub fn failing_from(inner: T, n: usize) -> Self {
        Self {
            inner,
            writes_seen: 0,
            fail_from: n,
            fail_until: usize::MAX,
            write_log: Vec::new(),
        }
    }

    /// Fail the `n`th write (1-based) and **only** that one.
    ///
    /// THE TWO FAULTS ARE DIFFERENT QUESTIONS, AND A ROLLBACK TEST NEEDS
    /// THIS ONE. A rollback is itself a write, so against
    /// `failing_from(n)` it cannot succeed either — the right answer
    /// there is an error that *says* the clusters leaked, not a volume
    /// that recovered. A transient fault (one bad write, the device
    /// still there) is what lets the rollback run, and it is the fault
    /// the rollback paths were written for: an I/O error while a file is
    /// being extended, not a disk that has gone away.
    pub fn failing_only(inner: T, n: usize) -> Self {
        Self {
            inner,
            writes_seen: 0,
            fail_from: n,
            fail_until: n,
            write_log: Vec::new(),
        }
    }

    /// Pass every write through, recording each one.
    pub fn recording(inner: T) -> Self {
        Self::failing_from(inner, usize::MAX)
    }

    pub fn writes_seen(&self) -> usize {
        self.writes_seen
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: fs_ntfs::block_io::BlockIo> fs_ntfs::block_io::BlockIo for FailingIo<T> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        self.inner.read_exact_at(offset, buf)
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        self.writes_seen += 1;
        self.write_log.push(offset);
        if self.writes_seen >= self.fail_from && self.writes_seen <= self.fail_until {
            return Err(format!(
                "injected I/O failure on write #{} at offset {offset} ({} bytes)",
                self.writes_seen,
                buf.len()
            ));
        }
        self.inner.write_all_at(offset, buf)
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sync(&mut self) -> Result<(), String> {
        // Only while the fault is live. A transient fault that has passed
        // must let the rollback's own sync through, or "the rollback
        // worked" is untestable.
        if self.writes_seen >= self.fail_from && self.writes_seen <= self.fail_until {
            return Err("injected I/O failure: sync during a failed write".to_string());
        }
        self.inner.sync()
    }
}
