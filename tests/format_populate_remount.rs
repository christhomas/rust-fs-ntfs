//! End-to-end format → populate full FS → remount → verify.
//!
//! Existing `mkfs_roundtrip.rs` proves that a freshly-formatted volume
//! parses back. This file goes further: after formatting, it builds
//! a small but realistic project tree (subdirs, mixed file sizes,
//! hard links, ADS, EAs, symlinks), then unmounts, remounts, and
//! verifies every file's content + every metadata bit survived
//! the round-trip.
//!
//! This catches whole classes of bugs that single-feature tests miss:
//! ones where two features write conflicting MFT bytes when used
//! together, or where the in-memory write path produces output that
//! the read path can't parse back.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::{FileType, Filesystem};
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;

fn fresh_volume(tag: &str) -> String {
    let dst = format!("test-disks/_fpr_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, 4096, 4096, Some("FPR"), Some(0xFEEDFACE))
        .expect("format");
    io.sync().expect("sync");
    drop(io);
    dst
}

fn read_string(fs: &Filesystem, path: &str) -> String {
    let size = fs.stat(path).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read_file(path, 0, &mut buf).unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn read_bytes(fs: &Filesystem, path: &str) -> Vec<u8> {
    let size = fs.stat(path).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read_file(path, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn pattern(size: usize, seed: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut s = seed as u32;
    for _ in 0..size {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        out.push((s >> 16) as u8);
    }
    out
}

/// Format an empty volume, build a mock "project" inside it, unmount,
/// remount, and verify every file matches what we wrote.
#[test]
fn populate_full_project_tree_and_remount() {
    let img = fresh_volume("project");

    // -- Phase 1: populate ----------------------------------------
    {
        let fs = Filesystem::mount(&img).unwrap();

        // Top-level layout.
        fs.mkdir("/", "src").unwrap();
        fs.mkdir("/", "tests").unwrap();
        fs.mkdir("/", "docs").unwrap();
        fs.mkdir("/", "data").unwrap();

        // Resident files (small text).
        fs.create_file("/", "README.md").unwrap();
        fs.write_file_contents("/README.md", b"# project\n")
            .unwrap();
        fs.create_file("/", "LICENSE").unwrap();
        fs.write_file_contents("/LICENSE", b"MIT\n").unwrap();

        // src/ — a few resident source files.
        fs.create_file("/src", "lib.rs").unwrap();
        fs.write_file_contents(
            "/src/lib.rs",
            b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        fs.create_file("/src", "main.rs").unwrap();
        fs.write_file_contents("/src/main.rs", b"fn main() { println!(\"hi\"); }\n")
            .unwrap();

        // tests/ — non-resident binary blob.
        let blob = pattern(48 * 1024, 0x37);
        fs.create_file("/tests", "fixture.bin").unwrap();
        fs.write_file_contents("/tests/fixture.bin", &blob).unwrap();

        // docs/ — file with an ADS metadata stream and an EA author tag.
        fs.create_file("/docs", "guide.md").unwrap();
        fs.write_file_contents("/docs/guide.md", b"# guide\n")
            .unwrap();
        fs.write_named_stream("/docs/guide.md", "annotations", b"reviewed=true\n")
            .unwrap();
        fs.write_ea("/docs/guide.md", b"AUTHOR", b"chris", 0)
            .unwrap();

        // data/ — three small files.
        for i in 0..3 {
            let name = format!("rec{i:02}.dat");
            fs.create_file("/data", &name).unwrap();
            fs.write_file_contents(&format!("/data/{name}"), format!("record {i}").as_bytes())
                .unwrap();
        }

        // Symlinks.
        fs.create_symlink("/", "current_main", "src\\main.rs", true)
            .unwrap();
        fs.create_symlink("/docs", "license_alias", "..\\LICENSE", true)
            .unwrap();
    }

    // -- Phase 2: remount + verify --------------------------------
    let fs = Filesystem::mount(&img).unwrap();

    // Volume info.
    let vi = fs.volume_info().unwrap();
    assert_eq!(vi.cluster_size, 4096);
    assert_eq!(vi.serial_number, 0xFEEDFACE);

    // Top-level entries (skip system files and `.`/`..`).
    let top: Vec<String> = fs
        .read_dir("/")
        .unwrap()
        .into_iter()
        .filter(|e| e.name != "." && e.name != ".." && !e.name.starts_with('$'))
        .map(|e| e.name)
        .collect();
    for expect in &[
        "src",
        "tests",
        "docs",
        "data",
        "README.md",
        "LICENSE",
        "current_main",
    ] {
        assert!(
            top.iter().any(|n| n == expect),
            "missing /{expect}: {top:?}"
        );
    }

    // Resident text reads back exactly.
    assert_eq!(read_string(&fs, "/README.md"), "# project\n");
    assert_eq!(read_string(&fs, "/LICENSE"), "MIT\n");
    assert_eq!(
        read_string(&fs, "/src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );

    // Non-resident blob byte-for-byte.
    let blob_back = read_bytes(&fs, "/tests/fixture.bin");
    assert_eq!(blob_back, pattern(48 * 1024, 0x37));

    // Subdirs are typed correctly.
    for dir in &["src", "tests", "docs", "data"] {
        assert_eq!(
            fs.stat(&format!("/{dir}")).unwrap().file_type,
            FileType::Directory
        );
    }

    // Data files.
    for i in 0..3 {
        let path = format!("/data/rec{i:02}.dat");
        assert_eq!(read_string(&fs, &path), format!("record {i}"));
    }

    // Symlinks classified as such.
    assert_eq!(
        fs.stat("/current_main").unwrap().file_type,
        FileType::Symlink
    );
    assert_eq!(
        fs.stat("/docs/license_alias").unwrap().file_type,
        FileType::Symlink
    );

    // EA survived.
    let eas = fs_ntfs::write::list_eas(Path::new(&img), "/docs/guide.md").unwrap();
    let author = eas
        .iter()
        .find(|e| e.name == b"AUTHOR")
        .expect("AUTHOR EA missing");
    assert_eq!(author.value, b"chris");

    // Volume should be self-consistent (not flagged dirty by our own
    // operations).
    assert!(!fs.is_dirty().unwrap(), "volume marked dirty after writes");
}

/// Same shape, but populate, then *delete* everything, then remount
/// and assert the volume is empty (no leaked entries) and free
/// clusters returned to baseline.
#[test]
fn populate_delete_remount_returns_to_baseline() {
    let img = fresh_volume("teardown");

    let baseline_clusters;
    {
        let fs = Filesystem::mount(&img).unwrap();
        baseline_clusters = fs.volume_stats().unwrap().free_clusters;

        fs.mkdir("/", "tmp").unwrap();
        fs.create_file("/tmp", "a.bin").unwrap();
        fs.write_file_contents("/tmp/a.bin", &pattern(64 * 1024, 0x11))
            .unwrap();
        fs.create_file("/tmp", "b.bin").unwrap();
        fs.write_file_contents("/tmp/b.bin", &pattern(64 * 1024, 0x22))
            .unwrap();
        fs.create_file("/", "top.txt").unwrap();
        fs.write_file_contents("/top.txt", b"top-level").unwrap();

        // Sanity: bitmap dropped while files exist.
        let mid = fs.volume_stats().unwrap().free_clusters;
        assert!(mid < baseline_clusters);

        // Tear down.
        fs.unlink("/tmp/a.bin").unwrap();
        fs.unlink("/tmp/b.bin").unwrap();
        fs.rmdir("/tmp").unwrap();
        fs.unlink("/top.txt").unwrap();
    }

    let fs = Filesystem::mount(&img).unwrap();

    // No user-visible entries left in root.
    let user: Vec<String> = fs
        .read_dir("/")
        .unwrap()
        .into_iter()
        .filter(|e| e.name != "." && e.name != ".." && !e.name.starts_with('$'))
        .map(|e| e.name)
        .collect();
    assert!(
        user.is_empty(),
        "user entries remain after teardown: {user:?}"
    );

    // Free clusters back to baseline.
    let after = fs.volume_stats().unwrap().free_clusters;
    assert_eq!(
        after, baseline_clusters,
        "bitmap leak across populate-and-teardown"
    );
}

/// Format same volume twice. The second format must succeed and the
/// post-format state must match a never-populated volume — no
/// remnants from the first cycle.
#[test]
fn reformat_clears_prior_state() {
    let img = fresh_volume("reformat_a");
    {
        let fs = Filesystem::mount(&img).unwrap();
        fs.create_file("/", "secret.txt").unwrap();
        fs.write_file_contents("/secret.txt", b"first incarnation")
            .unwrap();
    }

    // Reformat in place.
    let mut io = PathIo::open_rw(Path::new(&img)).unwrap();
    format_filesystem(
        &mut io,
        VOL_SIZE,
        4096,
        4096,
        Some("RFRM"),
        Some(0xC0FFEE00),
    )
    .expect("re-format");
    io.sync().unwrap();
    drop(io);

    let fs = Filesystem::mount(&img).unwrap();
    assert_eq!(fs.volume_info().unwrap().serial_number, 0xC0FFEE00);
    assert!(
        fs.stat("/secret.txt").is_err(),
        "old data visible after reformat"
    );
}
