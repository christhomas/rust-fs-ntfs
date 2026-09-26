//! Directory index boundary tests, including routed `$INDEX_ALLOCATION` leaves.
//!
//! These format a fresh volume at runtime (no prebuilt fixture) and stress
//! the directory-index insert path that lives in `index_io`.
//!
//! The create path promotes a full resident `$INDEX_ROOT` into allocated
//! leaves. This file covers subsequent routed-leaf splits too.
//!
//! These tests guarantee:
//!   * every inserted entry is independently findable (via the upstream
//!     `ntfs` parser, not our own read path),
//!   * entries collate in NTFS upcase order,
//!   * hitting a capacity limit fails gracefully (clear error, no panic) and
//!     leaves the directory readable and consistent.

mod common;

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_vol(tag: &str) -> String {
    let dst = common::temp_image_path(format!("ld_{tag}"));
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("LDTEST"),
        Some(0xD1_4EC7),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Independently enumerate a subdirectory's `$FILE_NAME` entries via the
/// upstream `ntfs` crate. Returns names in index order (i.e. how the
/// directory's B-tree stores them). Skips the DOS-namespace duplicates so
/// each file appears once. `dir` must be a single component directly under
/// the root (sufficient for these tests).
fn list_subdir(img: &str, dir: &str) -> Vec<String> {
    let f = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("ntfs");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let root = ntfs.root_directory(&mut reader).expect("root");
    let root_idx = root.directory_index(&mut reader).expect("root idx");
    let mut finder = root_idx.finder();
    let entry = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, dir)
        .expect("dir present")
        .expect("dir find ok");
    let dir_file = entry.to_file(&ntfs, &mut reader).expect("to_file");

    let index = dir_file.directory_index(&mut reader).expect("dir idx");
    let mut iter = index.entries();
    let mut names = Vec::new();
    while let Some(e) = iter.next(&mut reader) {
        let e = e.expect("entry");
        let key = e.key().expect("key").expect("key ok");
        // Each file may carry a Win32 + DOS name; keep Win32/POSIX, drop the
        // pure-DOS 8.3 duplicate so counts match what we created.
        use ntfs::structured_values::NtfsFileNamespace;
        if key.namespace() == NtfsFileNamespace::Dos {
            continue;
        }
        names.push(key.name().to_string_lossy());
    }
    names
}

/// Create files named `prefix{NNNN}` in `dir` until `create_file` errors,
/// returning (count_created, the_error_string).
fn fill_until_full(img: &str, dir: &str, prefix: &str) -> (usize, String) {
    let dir_path = format!("/{dir}");
    let mut created = 0usize;
    for i in 0..1000 {
        let name = format!("{prefix}{i:04}.txt");
        match write::create_file(Path::new(img), &dir_path, &name) {
            Ok(_) => created += 1,
            Err(e) => return (created, e),
        }
    }
    (created, String::new())
}

#[test]
fn subdir_fills_gracefully_at_capacity() {
    let img = fresh_vol("ceiling");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");
    let (created, err) = fill_until_full(&img, "d", "f_");

    // Index growth now proceeds past the old one-leaf ceiling. A later
    // capacity limit (often the fixed-size MFT) must be a clean refusal.
    assert!(
        (80..1000).contains(&created),
        "expected growth past two leaves before a capacity limit, got {created}"
    );
    assert!(
        err.contains("exceeds record capacity")
            || err.contains("no room")
            || err.contains("capacity")
            || err.contains("MFT has no free records"),
        "ceiling failure must be a graceful capacity error, got: {err:?}"
    );
}

#[test]
fn all_entries_below_ceiling_are_findable() {
    let img = fresh_vol("findable");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");
    // 20 is comfortably under the subdir ceiling.
    for i in 0..20 {
        let name = format!("f_{i:04}.txt");
        write::create_file(Path::new(&img), "/d", &name).expect("create");
    }
    let names = list_subdir(&img, "d");
    for i in 0..20 {
        let want = format!("f_{i:04}.txt");
        assert!(
            names.iter().any(|n| n == &want),
            "missing {want}; got {names:?}"
        );
    }
    assert_eq!(
        names.len(),
        20,
        "exactly 20 entries expected; got {names:?}"
    );
}

#[test]
fn entries_collate_in_upcase_order() {
    let img = fresh_vol("collate");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");
    // Insert in deliberately non-sorted, mixed-case order.
    for name in [
        "zebra.txt",
        "Apple.txt",
        "mango.txt",
        "BANANA.txt",
        "cherry.txt",
    ] {
        write::create_file(Path::new(&img), "/d", name).expect("create");
    }
    let names = list_subdir(&img, "d");
    // NTFS COLLATION_FILENAME is case-insensitive (upcase fold). Verify the
    // index returns them in that order.
    let mut expected = names.clone();
    expected.sort_by_key(|n| n.to_uppercase());
    assert_eq!(
        names, expected,
        "entries must be stored in upcase collation order"
    );
    assert_eq!(names.len(), 5);
}

#[test]
fn directory_stays_consistent_after_ceiling_rejection() {
    let img = fresh_vol("afterfull");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");
    let (created, _err) = fill_until_full(&img, "d", "f_");
    assert!(created > 0);

    // One more create must fail (we're at the ceiling)...
    let over = write::create_file(Path::new(&img), "/d", "one_more_over_ceiling.txt");
    assert!(over.is_err(), "create past ceiling must fail");

    // ...and the directory must still enumerate exactly the entries that
    // were successfully created — the failed insert left no corruption.
    let names = list_subdir(&img, "d");
    assert_eq!(
        names.len(),
        created,
        "failed insert must not change the entry set; have {} want {created}",
        names.len()
    );
    assert!(!names.iter().any(|n| n == "one_more_over_ceiling.txt"));
}

#[test]
fn upstream_mounts_after_filling_subdir() {
    let img = fresh_vol("remount");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");
    for i in 0..15 {
        write::create_file(Path::new(&img), "/d", &format!("f_{i:04}.txt")).expect("create");
    }
    // Independent parser must accept the volume and list all 15.
    let names = list_subdir(&img, "d");
    assert_eq!(names.len(), 15);
}

#[test]
fn an_overflowed_directory_can_gain_and_lose_a_name() {
    let img = fresh_vol("overflow_mutation");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");

    for i in 0..50 {
        write::create_file(Path::new(&img), "/d", &format!("f_{i:04}.txt"))
            .unwrap_or_else(|e| panic!("create {i} across the root-to-allocation transition: {e}"));
    }
    write::unlink(Path::new(&img), "/d/f_0007.txt").expect("unlink from allocation leaf");

    let names = list_subdir(&img, "d");
    assert_eq!(names.len(), 49);
    assert!(!names.iter().any(|name| name == "f_0007.txt"));
    assert!(names.iter().any(|name| name == "f_0049.txt"));
}

#[test]
fn routed_leaf_splits_again_after_the_first_two_leaf_tree() {
    let img = fresh_vol("routed_leaf_split");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");

    // Sorted inserts fill the right-hand leaf after the first split. The
    // next split must add another separator to the resident parent index.
    for i in 0..80 {
        let name = format!("f_{i:04}.txt");
        write::create_file(Path::new(&img), "/d", &name)
            .unwrap_or_else(|e| panic!("create {name} in routed leaf: {e}"));
    }

    let names = list_subdir(&img, "d");
    assert_eq!(names.len(), 80, "independent parser lost entries");
    for i in 0..80 {
        assert!(names.contains(&format!("f_{i:04}.txt")), "missing {i}");
    }
}

#[test]
fn left_routed_leaf_split_updates_the_existing_parent_child() {
    let img = fresh_vol("left_routed_leaf_split");
    write::mkdir(Path::new(&img), "/", "d").expect("mkdir");

    // Descending inserts fill the left leaf. Its old parent entry must be
    // retargeted to the new right leaf when a separator is inserted before it.
    for i in (0..80).rev() {
        let name = format!("f_{i:04}.txt");
        write::create_file(Path::new(&img), "/d", &name)
            .unwrap_or_else(|e| panic!("create {name} in left leaf: {e}"));
    }

    let names = list_subdir(&img, "d");
    assert_eq!(names.len(), 80, "independent parser lost entries");
    for i in 0..80 {
        assert!(names.contains(&format!("f_{i:04}.txt")), "missing {i}");
    }
}

/// Find `name` directly in the ROOT directory's index via the upstream
/// `ntfs` parser.
fn found_in_root(img: &str, name: &str) -> bool {
    let f = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("ntfs");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let root = ntfs.root_directory(&mut reader).expect("root");
    let idx = root.directory_index(&mut reader).expect("root idx");
    let mut finder = idx.finder();
    // `find` returns Option<Result<..>>: None = absent, Some(Ok) = present,
    // Some(Err) = present-but-corrupt. Only Some(Ok) counts as a clean find;
    // treating Some(Err) as "found" would mask the silent-loss this guards.
    matches!(
        NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, name),
        Some(Ok(_))
    )
}

/// The ROOT directory has its own resident `$INDEX_ROOT` ceiling, distinct
/// from (and smaller than) a fresh subdirectory's — the other tests here only
/// fill subdirectories. Fill the root directly and confirm the same
/// guarantees hold: a graceful stop at the ceiling (clear error, no panic)
/// and every created entry still independently findable (no silent loss).
#[test]
fn root_dir_fills_gracefully_at_capacity() {
    let img = fresh_vol("root_ceiling");
    let mut created = 0usize;
    let mut err = String::new();
    for i in 0..1000 {
        let name = format!("r_{i:04}.txt");
        match write::create_file(Path::new(&img), "/", &name) {
            Ok(_) => created += 1,
            Err(e) => {
                err = e;
                break;
            }
        }
    }
    assert!(created >= 1, "must create at least one root entry");
    // A fixed-size MFT can run out before the index does. Both capacity
    // refusals must preserve the names already committed.
    assert!(
        err.contains("exceeds record capacity")
            || err.contains("no room")
            || err.contains("capacity")
            || err.contains("MFT has no free records"),
        "root ceiling failure must be a graceful capacity error, got: {err:?}"
    );
    // Spot-check first / middle / last created entries remain findable in the
    // root after the ceiling rejection.
    assert!(
        found_in_root(&img, "r_0000.txt"),
        "first root entry findable"
    );
    let mid = created / 2;
    assert!(
        found_in_root(&img, &format!("r_{mid:04}.txt")),
        "middle root entry findable"
    );
    let last = created - 1;
    assert!(
        found_in_root(&img, &format!("r_{last:04}.txt")),
        "last root entry findable"
    );
}
