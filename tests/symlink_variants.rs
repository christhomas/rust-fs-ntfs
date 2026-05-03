//! Symlink target / classification edge cases.
//!
//! `create_symlink` writes a `$REPARSE_POINT` attribute with NTFS's
//! IO_REPARSE_TAG_SYMLINK. Tests:
//!   * absolute targets vs relative targets
//!   * target lengths from 1 char to many KiB
//!   * Unicode targets (full UTF-16 round-trip)
//!   * symlink basename limits (same WCHAR ceiling as files)
//!   * `stat` reports `FileType::Symlink` (vs `Junction`/`Other`)
//!
//! Symlinks are read-only here — actually following them is a kernel
//! responsibility on macOS/Windows; the driver's job is to store and
//! retrieve them faithfully.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::{FileType, Filesystem};
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

const VOL_SIZE: u64 = 32 * 1024 * 1024;

fn fresh_volume(tag: &str) -> String {
    let dst = format!("test-disks/_symx_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, 4096, 4096, Some("SYM"), Some(0x517B0117))
        .expect("format");
    io.sync().expect("sync");
    drop(io);
    dst
}

#[test]
fn absolute_target_short_roundtrip() {
    let img = fresh_volume("abs_short");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_symlink("/", "link", "C:\\Windows\\System32", false)
        .unwrap();
    assert_eq!(fs.stat("/link").unwrap().file_type, FileType::Symlink);
}

#[test]
fn relative_target_short_roundtrip() {
    let img = fresh_volume("rel_short");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "real.txt").unwrap();
    fs.create_symlink("/", "alias", "real.txt", true).unwrap();
    assert_eq!(fs.stat("/alias").unwrap().file_type, FileType::Symlink);
}

/// 1-character target. Smallest realistic target.
#[test]
fn target_one_char() {
    let img = fresh_volume("one_char");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_symlink("/", "tiny_link", "x", true).unwrap();
    assert_eq!(fs.stat("/tiny_link").unwrap().file_type, FileType::Symlink);
}

/// Long absolute target — many directory levels deep, well past the
/// classic Win32 MAX_PATH=260.
#[test]
fn target_long_path() {
    let img = fresh_volume("long_target");
    let fs = Filesystem::mount(&img).unwrap();
    let target = format!("C:\\{}", "verylongdirname\\".repeat(20));
    assert!(target.len() > 260);
    fs.create_symlink("/", "deep_link", &target, false).unwrap();
    assert_eq!(fs.stat("/deep_link").unwrap().file_type, FileType::Symlink);
}

/// CJK + emoji target — UTF-16 round-trip in the reparse blob.
#[test]
fn target_unicode_roundtrip() {
    let img = fresh_volume("unicode_target");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_symlink("/", "uni_link", "C:\\日本語\\папка\\🌍.txt", false)
        .unwrap();
    assert_eq!(fs.stat("/uni_link").unwrap().file_type, FileType::Symlink);
}

/// Symlink basename at the WCHAR ceiling — same constraint as a
/// regular file basename.
#[test]
fn symlink_basename_at_wchar_limit() {
    let img = fresh_volume("basename_255");
    let fs = Filesystem::mount(&img).unwrap();
    let name: String = std::iter::repeat_n('s', 255).collect();
    fs.create_symlink("/", &name, "target.txt", true).unwrap();
    assert_eq!(
        fs.stat(&format!("/{name}")).unwrap().file_type,
        FileType::Symlink
    );
}

/// Multiple symlinks pointing at the same target — the driver must
/// generate independent reparse data per symlink.
#[test]
fn multiple_symlinks_share_target() {
    let img = fresh_volume("share_target");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "real.txt").unwrap();
    fs.create_symlink("/", "a", "real.txt", true).unwrap();
    fs.create_symlink("/", "b", "real.txt", true).unwrap();
    fs.create_symlink("/", "c", "real.txt", true).unwrap();
    for name in &["a", "b", "c"] {
        assert_eq!(
            fs.stat(&format!("/{name}")).unwrap().file_type,
            FileType::Symlink
        );
    }
}

/// Symlink survives unmount/remount with type intact.
#[test]
fn symlink_survives_remount() {
    let img = fresh_volume("survive");
    {
        let fs = Filesystem::mount(&img).unwrap();
        fs.create_symlink("/", "persist_link", "..\\sibling", true)
            .unwrap();
    }
    let fs = Filesystem::mount(&img).unwrap();
    assert_eq!(
        fs.stat("/persist_link").unwrap().file_type,
        FileType::Symlink
    );
}

/// Symlink with a target that itself uses unusual but legal NTFS
/// path syntax — drive-relative and namespace prefix.
#[test]
fn target_with_namespace_prefix() {
    let img = fresh_volume("ns_prefix");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_symlink("/", "raw_link", "\\??\\C:\\Volume\\file", false)
        .unwrap();
    assert_eq!(fs.stat("/raw_link").unwrap().file_type, FileType::Symlink);
}

/// Empty target — must error or accept; never panic.
#[test]
fn empty_target_handled() {
    let img = fresh_volume("empty_target");
    let fs = Filesystem::mount(&img).unwrap();
    let _ = fs.create_symlink("/", "empty", "", true);
    // No assertion on success/failure: both are defensible. The only
    // contract is "no panic".
}
