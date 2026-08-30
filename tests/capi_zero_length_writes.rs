//! What `len == 0` means to each of the two C-ABI write entry points.
//!
//! `fs_ntfs_write_file` and `fs_ntfs_write_file_contents` sit next to
//! each other, take almost the same arguments, and both open with
//! `if len == 0`. They mean **opposite things** by it:
//!
//! - a ranged write of nothing is nothing, and does not even check the
//!   path;
//! - writing nothing as a file's whole contents **empties it**.
//!
//! Each is right for its own operation, and neither is inferable from
//! the signature. The report filed the pair as High for that reason: a
//! caller cannot be right about both.
//!
//! These tests pin both behaviours, so a later change that quietly makes
//! them agree — in either direction — has to be a deliberate one.
//!
//! The image is formatted here rather than taken from `test-disks/`,
//! which needs `mkntfs` and therefore only exists on CI.

#![allow(unused_unsafe)]

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{fs_ntfs_write_file, fs_ntfs_write_file_contents};
use std::ffi::{c_void, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A freshly formatted image, removed when the test ends.
struct TmpImage(PathBuf);

impl TmpImage {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "fs_ntfs_zerolen_{tag}_{}_{n}.img",
            std::process::id()
        ));

        // 16 MiB: mkfs puts $MFTMirr at the halfway point and the
        // primary metadata region has to end before it.
        const SIZE: u64 = 16 * 1024 * 1024;
        std::fs::File::create(&path).expect("create image");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open image")
            .set_len(SIZE)
            .expect("size image");
        {
            let mut io = PathIo::open_rw(&path).expect("open_rw");
            format_filesystem(
                &mut io as &mut dyn BlockIo,
                SIZE,
                4096,
                4096,
                Some("ZEROLEN"),
                Some(0x0BAD_F00D),
            )
            .expect("format");
        }
        TmpImage(path)
    }

    fn c_path(&self) -> CString {
        CString::new(self.0.to_str().unwrap()).unwrap()
    }
}

impl Drop for TmpImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn file_size(img: &TmpImage, path: &str) -> u64 {
    let mut io = PathIo::open_ro(&img.0).expect("open_ro");
    let rec = fs_ntfs::read::resolve_path(&mut io, path).expect("resolve");
    fs_ntfs::read::read_stat(&mut io, rec).expect("stat").size
}

/// Writing nothing as a file's whole contents empties it.
///
/// The same thing `open(O_TRUNC)` with no following write does — which
/// is the right reading of "write this as the entire contents".
#[test]
fn write_file_contents_with_zero_length_empties_the_file() {
    let img = TmpImage::new("contents_empties");
    let image = img.c_path();
    let path = CString::new("/victim.txt").unwrap();

    fs_ntfs::write::create_file(&img.0, "/", "victim.txt").expect("create");
    let payload = b"something to lose";
    let n = fs_ntfs_write_file_contents(
        image.as_ptr(),
        path.as_ptr(),
        payload.as_ptr() as *const c_void,
        payload.len() as u64,
    );
    assert_eq!(n, payload.len() as i64, "setup write");
    assert_eq!(file_size(&img, "/victim.txt"), payload.len() as u64);

    // buf is not read when len is 0, so null is allowed.
    let n = fs_ntfs_write_file_contents(image.as_ptr(), path.as_ptr(), std::ptr::null(), 0);
    assert_eq!(n, 0, "a zero-length whole-file write reports zero bytes");
    assert_eq!(
        file_size(&img, "/victim.txt"),
        0,
        "and it emptied the file, which is what 'the entire contents' means"
    );
}

/// A ranged write of nothing changes nothing.
#[test]
fn write_file_with_zero_length_leaves_the_file_alone() {
    let img = TmpImage::new("ranged_noop");
    let image = img.c_path();
    let path = CString::new("/keeper.txt").unwrap();

    fs_ntfs::write::create_file(&img.0, "/", "keeper.txt").expect("create");
    let payload = b"something to keep";
    let n = fs_ntfs_write_file_contents(
        image.as_ptr(),
        path.as_ptr(),
        payload.as_ptr() as *const c_void,
        payload.len() as u64,
    );
    assert_eq!(n, payload.len() as i64, "setup write");

    let n = fs_ntfs_write_file(image.as_ptr(), path.as_ptr(), 0, std::ptr::null(), 0);
    assert_eq!(n, 0, "a zero-length ranged write reports zero bytes");
    assert_eq!(
        file_size(&img, "/keeper.txt"),
        payload.len() as u64,
        "and it left the contents where they were — the opposite of \
         fs_ntfs_write_file_contents with the same length"
    );
}

/// The ranged write does not even check the path.
///
/// It returns 0 for a file that does not exist, because
/// `write::write_at` short-circuits on empty data before resolving
/// anything — so a caller chunking a buffer pays nothing for an empty
/// tail. Documented rather than changed: it is the library's policy at
/// three levels, not a shortcut in the wrapper.
#[test]
fn a_zero_length_ranged_write_succeeds_for_a_path_that_does_not_exist() {
    let img = TmpImage::new("ranged_nopath");
    let image = img.c_path();
    let missing = CString::new("/no-such-file.txt").unwrap();

    let n = fs_ntfs_write_file(image.as_ptr(), missing.as_ptr(), 0, std::ptr::null(), 0);
    assert_eq!(n, 0, "zero length returns 0 without resolving the path");

    // One byte to the same path does fail, which is the contrast that
    // makes the zero-length case worth documenting.
    let one = [0xAAu8];
    let n = fs_ntfs_write_file(
        image.as_ptr(),
        missing.as_ptr(),
        0,
        one.as_ptr() as *const c_void,
        1,
    );
    assert_eq!(n, -1, "a non-empty write to the same path fails");
}
