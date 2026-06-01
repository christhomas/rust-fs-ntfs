//! Phase 6.1 — error-path coverage for the write API.
//!
//! Verifies that invalid operations FAIL (return `Err`) rather than silently
//! corrupting the volume: duplicate creates, missing parents, bad names,
//! over-long names, rmdir on non-empty/non-dir, unlink of missing/dir, etc.
//! Self-generating volumes; each op is checked on a fresh format.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_err_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create temp image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("ERR"),
        Some(0xE2_2D_00_00),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

fn p(img: &str) -> &Path {
    Path::new(img)
}

// ---------------------------------------------------------------------------
// create_file
// ---------------------------------------------------------------------------

#[test]
fn create_file_duplicate_name_errors() {
    let img = fresh_vol("dup_create");
    write::create_file(p(&img), "/", "dup.txt").expect("first create ok");
    let err = write::create_file(p(&img), "/", "dup.txt");
    assert!(err.is_err(), "creating an existing name must fail: {err:?}");
}

#[test]
fn create_file_in_missing_parent_errors() {
    let img = fresh_vol("missing_parent");
    let err = write::create_file(p(&img), "/no_such_dir", "x.txt");
    assert!(
        err.is_err(),
        "create under a missing parent must fail: {err:?}"
    );
}

#[test]
fn create_file_empty_basename_errors() {
    let img = fresh_vol("empty_name");
    assert!(
        write::create_file(p(&img), "/", "").is_err(),
        "empty name must fail"
    );
}

#[test]
fn create_file_dot_and_dotdot_error() {
    let img = fresh_vol("dots");
    assert!(
        write::create_file(p(&img), "/", ".").is_err(),
        "'.' must fail"
    );
    assert!(
        write::create_file(p(&img), "/", "..").is_err(),
        "'..' must fail"
    );
}

#[test]
fn create_file_basename_with_slash_errors() {
    let img = fresh_vol("slash");
    assert!(
        write::create_file(p(&img), "/", "a/b").is_err(),
        "basename containing '/' must fail"
    );
}

#[test]
fn create_file_name_too_long_errors() {
    let img = fresh_vol("toolong");
    let name = "z".repeat(256); // NTFS max is 255 UTF-16 code units
    assert!(
        write::create_file(p(&img), "/", &name).is_err(),
        "256-char name must fail (max 255)"
    );
}

// ---------------------------------------------------------------------------
// mkdir
// ---------------------------------------------------------------------------

#[test]
fn mkdir_duplicate_name_errors() {
    let img = fresh_vol("dup_mkdir");
    write::mkdir(p(&img), "/", "d").expect("first mkdir ok");
    assert!(
        write::mkdir(p(&img), "/", "d").is_err(),
        "duplicate mkdir must fail"
    );
}

#[test]
fn mkdir_in_missing_parent_errors() {
    let img = fresh_vol("mkdir_missing");
    assert!(
        write::mkdir(p(&img), "/nope", "child").is_err(),
        "mkdir under a missing parent must fail"
    );
}

#[test]
fn mkdir_over_existing_file_errors() {
    let img = fresh_vol("mkdir_over_file");
    write::create_file(p(&img), "/", "f").expect("create file");
    assert!(
        write::mkdir(p(&img), "/", "f").is_err(),
        "mkdir over an existing file name must fail"
    );
}

// ---------------------------------------------------------------------------
// rmdir
// ---------------------------------------------------------------------------

#[test]
fn rmdir_missing_errors() {
    let img = fresh_vol("rmdir_missing");
    assert!(
        write::rmdir(p(&img), "/ghost").is_err(),
        "rmdir of missing dir must fail"
    );
}

#[test]
fn rmdir_nonempty_errors() {
    let img = fresh_vol("rmdir_nonempty");
    write::mkdir(p(&img), "/", "full").expect("mkdir");
    write::create_file(p(&img), "/full", "inside.txt").expect("create inside");
    assert!(
        write::rmdir(p(&img), "/full").is_err(),
        "rmdir of a non-empty dir must fail"
    );
}

#[test]
fn rmdir_on_a_file_errors() {
    let img = fresh_vol("rmdir_file");
    write::create_file(p(&img), "/", "notadir.txt").expect("create");
    assert!(
        write::rmdir(p(&img), "/notadir.txt").is_err(),
        "rmdir on a regular file must fail"
    );
}

// ---------------------------------------------------------------------------
// unlink
// ---------------------------------------------------------------------------

#[test]
fn unlink_missing_errors() {
    let img = fresh_vol("unlink_missing");
    assert!(
        write::unlink(p(&img), "/nope.txt").is_err(),
        "unlink of missing file must fail"
    );
}

// ---------------------------------------------------------------------------
// write_file_contents
// ---------------------------------------------------------------------------

#[test]
fn write_contents_to_missing_file_errors() {
    let img = fresh_vol("write_missing");
    assert!(
        write::write_file_contents(p(&img), "/ghost.bin", b"data").is_err(),
        "writing to a non-existent file must fail"
    );
}

// ---------------------------------------------------------------------------
// rename
// ---------------------------------------------------------------------------

#[test]
fn rename_missing_source_errors() {
    let img = fresh_vol("rename_missing");
    assert!(
        write::rename(p(&img), "/ghost.txt", "new.txt").is_err(),
        "rename of a missing source must fail"
    );
}

// Renaming onto an already-existing name must error: NTFS requires unique
// names per directory, so this would otherwise risk two $I30 entries with
// the same key (a corruption chkdsk flags) or a silent clobber.
#[test]
fn rename_onto_existing_name_errors() {
    let img = fresh_vol("rename_dup");
    write::create_file(p(&img), "/", "src.txt").expect("create src");
    write::create_file(p(&img), "/", "dst.txt").expect("create dst");
    assert!(
        write::rename(p(&img), "/src.txt", "dst.txt").is_err(),
        "rename onto an existing name must fail"
    );
}

#[test]
fn rename_different_length_onto_existing_name_errors() {
    // Different-length destination drives the variable-length rename path
    // (distinct from the same-length path covered above).
    let img = fresh_vol("rename_dup_vlen");
    write::create_file(p(&img), "/", "s.txt").expect("create src");
    write::create_file(p(&img), "/", "longer-dst.txt").expect("create dst");
    assert!(
        write::rename(p(&img), "/s.txt", "longer-dst.txt").is_err(),
        "variable-length rename onto an existing name must fail"
    );
}

#[test]
fn rename_to_same_name_is_noop() {
    let img = fresh_vol("rename_noop");
    write::create_file(p(&img), "/", "keep.txt").expect("create");
    write::rename(p(&img), "/keep.txt", "keep.txt").expect("identity rename is a no-op");
    write::rename_same_length(p(&img), "/keep.txt", "keep.txt")
        .expect("identity same-length rename is a no-op");
}

// ---------------------------------------------------------------------------
// remove (POSIX-style dispatch by type)
// ---------------------------------------------------------------------------

#[test]
fn remove_dispatches_file_to_unlink() {
    let img = fresh_vol("remove_file");
    write::create_file(p(&img), "/", "f.txt").expect("create");
    write::remove(p(&img), "/f.txt").expect("remove file");
    assert!(
        write::remove(p(&img), "/f.txt").is_err(),
        "second remove of gone file must fail"
    );
}

#[test]
fn remove_dispatches_empty_dir_to_rmdir() {
    let img = fresh_vol("remove_dir");
    write::mkdir(p(&img), "/", "d").expect("mkdir");
    write::remove(p(&img), "/d").expect("remove empty dir");
}

#[test]
fn remove_nonempty_dir_errors() {
    let img = fresh_vol("remove_nonempty");
    write::mkdir(p(&img), "/", "d").expect("mkdir");
    write::create_file(p(&img), "/d", "inside.txt").expect("create inside");
    assert!(
        write::remove(p(&img), "/d").is_err(),
        "remove of a non-empty dir must fail"
    );
}
