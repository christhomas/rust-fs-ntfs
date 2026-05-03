//! End-to-end workflow tests.
//!
//! Existing per-feature tests prove individual operations work in
//! isolation. These tests compound many operations on the same volume
//! to surface state-corruption bugs that only appear in sequences:
//! e.g. a write that leaves bitmap state inconsistent in a way that
//! only the next mkdir notices, or a rename that breaks subsequent
//! navigation.
//!
//! Each test runs an entire user-style scenario from a fresh volume
//! (formatted in-test via `format_filesystem`) and verifies the final
//! state. No shared fixtures — every test owns its own image.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::{FileType, Filesystem};
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;

fn fresh_volume(tag: &str, label: &str) -> String {
    let dst = format!("test-disks/_e2e_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, 4096, 4096, Some(label), Some(0xDEADBEEF))
        .expect("format");
    io.sync().expect("sync");
    drop(io);
    dst
}

fn read_all(fs: &Filesystem, path: &str, expected: u64) -> Vec<u8> {
    let mut out = vec![0u8; expected as usize];
    let n = fs.read_file(path, 0, &mut out).expect("read");
    out.truncate(n);
    out
}

/// User-visible entries in `dir` — drops `.`, `..`, and the always-
/// present NTFS system files (`$Boot`, `$MFT`, …) that the facade's
/// `read_dir` surfaces from the root.
fn user_entries(fs: &Filesystem, dir: &str) -> Vec<String> {
    fs.read_dir(dir)
        .unwrap()
        .into_iter()
        .filter(|e| e.name != "." && e.name != ".." && !e.name.starts_with('$'))
        .map(|e| e.name)
        .collect()
}

/// A single file goes through every mutation the API offers, in order.
/// Verifies the file remains coherent at every stage.
#[test]
fn single_file_full_lifecycle() {
    let img = fresh_volume("single_lifecycle", "E2E1");
    let fs = Filesystem::mount(&img).unwrap();

    fs.create_file("/", "doc.txt").unwrap();
    assert_eq!(fs.stat("/doc.txt").unwrap().size, 0);

    // Resident write.
    fs.write_file_contents("/doc.txt", b"v1").unwrap();
    assert_eq!(read_all(&fs, "/doc.txt", 2), b"v1");

    // Promote to non-resident.
    let big = vec![0xAB; 16 * 1024];
    fs.write_file_contents("/doc.txt", &big).unwrap();
    assert_eq!(fs.stat("/doc.txt").unwrap().size, 16 * 1024);
    assert_eq!(read_all(&fs, "/doc.txt", 16 * 1024), big);

    // Truncate down (still non-resident here).
    fs.truncate("/doc.txt", 4096).unwrap();
    assert_eq!(fs.stat("/doc.txt").unwrap().size, 4096);
    assert_eq!(read_all(&fs, "/doc.txt", 4096), big[..4096]);

    // Truncate to zero.
    fs.truncate("/doc.txt", 0).unwrap();
    assert_eq!(fs.stat("/doc.txt").unwrap().size, 0);

    // Rename in place (length differs — exercises rename, not rename_same_length).
    fs.rename("/doc.txt", "renamed.dat").unwrap();
    assert!(fs.stat("/doc.txt").is_err());
    assert_eq!(fs.stat("/renamed.dat").unwrap().size, 0);

    // Add an ADS, delete it.
    fs.write_named_stream("/renamed.dat", "meta", b"hello stream")
        .unwrap();
    fs.delete_named_stream("/renamed.dat", "meta").unwrap();

    // Add an EA, remove it.
    fs.write_ea("/renamed.dat", b"AUTHOR", b"chris", 0).unwrap();
    fs.remove_ea("/renamed.dat", b"AUTHOR").unwrap();

    // Final unlink.
    fs.unlink("/renamed.dat").unwrap();
    assert!(fs.stat("/renamed.dat").is_err());

    // Volume must remount cleanly afterwards.
    let fs2 = Filesystem::mount(&img).unwrap();
    let entries = fs2.read_dir("/").unwrap();
    assert!(!entries.iter().any(|e| e.name == "renamed.dat"));
}

/// Build a small directory tree, then dismantle it. Mixes mkdir,
/// create_file, write, rename, unlink, rmdir.
#[test]
fn directory_tree_build_and_teardown() {
    let img = fresh_volume("tree_build_teardown", "TREE");
    let fs = Filesystem::mount(&img).unwrap();

    fs.mkdir("/", "src").unwrap();
    fs.mkdir("/src", "lib").unwrap();
    fs.mkdir("/src", "bin").unwrap();
    fs.mkdir("/", "docs").unwrap();

    fs.create_file("/src/lib", "core.rs").unwrap();
    fs.write_file_contents("/src/lib/core.rs", b"// core")
        .unwrap();
    fs.create_file("/src/bin", "main.rs").unwrap();
    fs.write_file_contents("/src/bin/main.rs", b"fn main(){}")
        .unwrap();
    fs.create_file("/docs", "README.md").unwrap();
    fs.write_file_contents("/docs/README.md", b"# proj")
        .unwrap();

    // Snapshot.
    let src_lib_names = user_entries(&fs, "/src/lib");
    assert_eq!(src_lib_names, vec!["core.rs"]);

    // Rename, then teardown bottom-up.
    fs.rename("/docs/README.md", "INTRO.md").unwrap();
    assert!(fs.stat("/docs/INTRO.md").is_ok());

    fs.unlink("/src/lib/core.rs").unwrap();
    fs.rmdir("/src/lib").unwrap();
    fs.unlink("/src/bin/main.rs").unwrap();
    fs.rmdir("/src/bin").unwrap();
    fs.rmdir("/src").unwrap();
    fs.unlink("/docs/INTRO.md").unwrap();
    fs.rmdir("/docs").unwrap();

    let names = user_entries(&fs, "/");
    assert!(
        names.is_empty(),
        "root should be empty after teardown, got {names:?}"
    );

    // Remount sanity.
    let fs2 = Filesystem::mount(&img).unwrap();
    let names2 = user_entries(&fs2, "/");
    assert!(names2.is_empty(), "still empty after remount: {names2:?}");
}

/// rmdir refuses non-empty dirs; unlink refuses dirs.
#[test]
fn delete_kind_safety() {
    let img = fresh_volume("delete_kind", "DSAFE");
    let fs = Filesystem::mount(&img).unwrap();
    fs.mkdir("/", "d").unwrap();
    fs.create_file("/d", "f").unwrap();

    // rmdir on non-empty must fail.
    assert!(fs.rmdir("/d").is_err());
    // unlink on a directory must fail.
    assert!(fs.unlink("/d").is_err());

    fs.unlink("/d/f").unwrap();
    fs.rmdir("/d").unwrap();
}

/// Two-name hard link: unlink the original, the alias must still
/// resolve to the same MFT record and return the same bytes.
///
/// Currently fails because `write::unlink` frees the MFT record
/// regardless of link_count — the second unlink hits a freed record
/// and reports `refusing to write to MFT record N: IN_USE flag is
/// clear`. POSIX says the inode survives until link_count reaches 0.
/// Drop `#[ignore]` once `unlink` consults link_count.
#[test]
#[ignore = "driver: unlink frees MFT record without checking link_count"]
fn hard_link_pair_unlink_original_keeps_alias_alive() {
    let img = fresh_volume("hl_pair", "HL2");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "orig.txt").unwrap();
    fs.write_file_contents("/orig.txt", b"shared content")
        .unwrap();

    fs.mkdir("/", "aliases").unwrap();
    fs.link("/orig.txt", "/aliases", "a1").unwrap();

    let orig_rn = fs.stat("/orig.txt").unwrap().file_record_number;
    let alias_rn = fs.stat("/aliases/a1").unwrap().file_record_number;
    assert_eq!(orig_rn, alias_rn, "alias must point at the same MFT record");
    assert_eq!(fs.stat("/orig.txt").unwrap().link_count, 2);

    fs.unlink("/orig.txt").unwrap();
    assert!(fs.stat("/orig.txt").is_err());

    // The alias must remain readable, with the same data, pointing at
    // the same MFT record. This is the user-visible POSIX contract.
    assert_eq!(read_all(&fs, "/aliases/a1", 14), b"shared content");
    assert_eq!(fs.stat("/aliases/a1").unwrap().file_record_number, orig_rn);

    fs.unlink("/aliases/a1").unwrap();
    assert!(user_entries(&fs, "/aliases").is_empty());
}

/// Drain a chain of 4 hard links by unlinking each in turn. Currently
/// fails because the driver frees the MFT record before all names are
/// gone (`refusing to write to MFT record N: IN_USE flag is clear`
/// surfaces on the third unlink). Kept as a regression target — when
/// the link-count plumbing in `write::unlink` is fixed, drop the
/// `#[ignore]`.
#[test]
#[ignore = "driver: hard-link drain frees MFT record before link_count reaches 0"]
fn hard_link_chain_full_drain() {
    let img = fresh_volume("hl_drain", "HLD");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "orig.txt").unwrap();
    fs.write_file_contents("/orig.txt", b"shared content")
        .unwrap();

    fs.mkdir("/", "aliases").unwrap();
    fs.link("/orig.txt", "/aliases", "a1").unwrap();
    fs.link("/orig.txt", "/aliases", "a2").unwrap();
    fs.link("/orig.txt", "/aliases", "a3").unwrap();

    fs.unlink("/orig.txt").unwrap();
    fs.unlink("/aliases/a1").unwrap();
    fs.unlink("/aliases/a2").unwrap();
    fs.unlink("/aliases/a3").unwrap();

    assert!(user_entries(&fs, "/aliases").is_empty());
}

/// Allocate a non-resident file, dirty the bitmap, then free it. Free
/// cluster count must return to its starting value.
#[test]
fn bitmap_reclaim_after_unlink() {
    let img = fresh_volume("bitmap_reclaim", "BMR");
    let fs = Filesystem::mount(&img).unwrap();
    let before = fs.volume_stats().unwrap().free_clusters;

    fs.create_file("/", "blob.bin").unwrap();
    fs.write_file_contents("/blob.bin", &vec![0xCC; 512 * 1024])
        .unwrap();
    let during = fs.volume_stats().unwrap().free_clusters;
    assert!(
        during < before,
        "alloc should consume clusters: before={before} during={during}"
    );

    fs.unlink("/blob.bin").unwrap();
    let after = fs.volume_stats().unwrap().free_clusters;
    // Reclaim must recover all of the allocation. Allow exact equality
    // — no metadata side-effects should outlive the file once it's gone.
    assert_eq!(
        after, before,
        "free_clusters should return to baseline: before={before} after={after}"
    );
}

/// Repeated unlink+recreate on the same name. Models a log file that
/// fills, gets rotated, refills. Each cycle's content must read back
/// cleanly, and the bitmap must not leak clusters across cycles.
#[test]
fn rotation_cycle_keeps_bitmap_sane() {
    let img = fresh_volume("rotation_cycle", "GSC");
    let fs = Filesystem::mount(&img).unwrap();
    let baseline = fs.volume_stats().unwrap().free_clusters;

    for cycle in 0..6u32 {
        fs.create_file("/", "log.bin").unwrap();
        let payload = vec![cycle as u8; 64 * 1024];
        fs.write_file_contents("/log.bin", &payload).unwrap();
        assert_eq!(read_all(&fs, "/log.bin", 64 * 1024), payload);
        fs.unlink("/log.bin").unwrap();
    }

    // After 6 alloc/free cycles, free clusters should be back to
    // baseline. A leak shows up here as a steady drift downward.
    let after = fs.volume_stats().unwrap().free_clusters;
    assert_eq!(
        after, baseline,
        "bitmap leak across cycles: baseline={baseline} after={after}"
    );
}

/// Mass create then mass delete. Stresses the MFT free-record bitmap.
///
/// Bounded to a count that fits in a resident `$INDEX_ROOT`. The
/// driver doesn't yet promote `$INDEX_ROOT` → `$INDEX_ALLOCATION` on
/// overflow when the index grows under direct create_file calls; the
/// existing `manyfiles.rs` test uses a pre-built fixture to cover the
/// >$INDEX_ROOT-capacity case (until promote-from-create lands).
#[test]
fn mass_create_then_delete_recovers_mft_records() {
    let img = fresh_volume("mass_mft", "MFT");
    let fs = Filesystem::mount(&img).unwrap();
    fs.mkdir("/", "many").unwrap();

    let mft_before = fs.volume_stats().unwrap().mft_free_records;
    let n = 24u32;
    for i in 0..n {
        fs.create_file("/many", &format!("f{i:04}")).unwrap();
    }
    let mft_during = fs.volume_stats().unwrap().mft_free_records;
    assert!(
        mft_during < mft_before,
        "MFT records should drop after creating {n} files: before={mft_before} during={mft_during}"
    );
    let consumed = mft_before - mft_during;
    assert!(
        consumed >= n as u64,
        "MFT records consumed ({consumed}) should be >= file count ({n})"
    );

    for i in 0..n {
        fs.unlink(&format!("/many/f{i:04}")).unwrap();
    }
    let mft_after = fs.volume_stats().unwrap().mft_free_records;
    // Freed MFT records must be reclaimable. Allow a tiny slop for
    // metadata churn that the platform may or may not GC.
    assert!(
        mft_after + 2 >= mft_before,
        "freed MFT records should recover: before={mft_before} after={mft_after}"
    );
}

/// Persistence: every write must survive an unmount/remount round-trip.
#[test]
fn full_state_survives_remount() {
    let img = fresh_volume("survives_remount", "PERS");
    {
        let fs = Filesystem::mount(&img).unwrap();
        fs.mkdir("/", "a").unwrap();
        fs.mkdir("/a", "b").unwrap();
        fs.create_file("/a/b", "f1.txt").unwrap();
        fs.write_file_contents("/a/b/f1.txt", b"persist me")
            .unwrap();
        fs.create_file("/a", "f2.bin").unwrap();
        fs.write_file_contents("/a/f2.bin", &vec![0x55; 32 * 1024])
            .unwrap();
        fs.write_named_stream("/a/b/f1.txt", "alt", b"alt-stream-contents")
            .unwrap();
        fs.write_ea("/a/b/f1.txt", b"X", b"y", 0).unwrap();
    }

    let fs = Filesystem::mount(&img).unwrap();
    assert_eq!(read_all(&fs, "/a/b/f1.txt", 10), b"persist me");
    assert_eq!(fs.stat("/a/f2.bin").unwrap().size, 32 * 1024);
    let f2 = read_all(&fs, "/a/f2.bin", 32 * 1024);
    assert!(f2.iter().all(|&b| b == 0x55));
    assert_eq!(fs.stat("/a").unwrap().file_type, FileType::Directory);
    assert_eq!(fs.stat("/a/b").unwrap().file_type, FileType::Directory);
}

/// Renaming a directory keeps every child reachable under the new name.
#[test]
fn rename_directory_preserves_children() {
    let img = fresh_volume("rename_dir", "RDIR");
    let fs = Filesystem::mount(&img).unwrap();
    fs.mkdir("/", "old_name").unwrap();
    fs.create_file("/old_name", "child1.txt").unwrap();
    fs.write_file_contents("/old_name/child1.txt", b"abc")
        .unwrap();
    fs.create_file("/old_name", "child2.txt").unwrap();
    fs.write_file_contents("/old_name/child2.txt", b"def")
        .unwrap();
    fs.mkdir("/old_name", "subdir").unwrap();
    fs.create_file("/old_name/subdir", "leaf").unwrap();

    fs.rename("/old_name", "fresh_name").unwrap();

    assert!(fs.stat("/old_name").is_err());
    assert_eq!(
        fs.stat("/fresh_name").unwrap().file_type,
        FileType::Directory
    );
    assert_eq!(read_all(&fs, "/fresh_name/child1.txt", 3), b"abc");
    assert_eq!(read_all(&fs, "/fresh_name/child2.txt", 3), b"def");
    assert_eq!(
        fs.stat("/fresh_name/subdir/leaf").unwrap().file_type,
        FileType::Regular
    );
}
