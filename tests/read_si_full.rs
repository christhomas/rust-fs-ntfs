//! Tests for `write::read_si_full` — exposes every field of a file's
//! `$STANDARD_INFORMATION` (both the common 48-byte header and the
//! optional 24-byte NTFS 3.x trailer).

use fs_ntfs::write::{create_file, read_si_full, set_security_id, set_times, FileTimes};
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> (String, &'static str) {
    let dst = format!("test-disks/_rsf_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    create_file(Path::new(&dst), "/", "runtime.bin").expect("runtime create_file");
    (dst, "/runtime.bin")
}

#[test]
fn runtime_file_has_v3_form_with_default_values() {
    let (img, path) = working_copy("runtime");
    let si = read_si_full(Path::new(&img), path).unwrap();
    // Fresh runtime-created files use the 72-byte v3.x form.
    let v3 = si.v3.expect("runtime file should have v3 trailer");
    // Defaults for fresh files: security_id = 0 (inherited DACL),
    // owner_id = 0, quota = 0, usn = 0.
    assert_eq!(v3.security_id, 0);
    assert_eq!(v3.owner_id, 0);
    assert_eq!(v3.quota, 0);
    assert_eq!(v3.usn, 0);
    // Sanity: all 4 timestamps should be non-zero (set during create_file).
    assert!(si.creation_time > 0);
    assert!(si.modification_time > 0);
    assert!(si.access_time > 0);
    assert!(si.mft_modification_time > 0);
}

#[test]
fn legacy_v1x_file_has_no_v3_trailer() {
    // The fixture's /hello.txt is the 48-byte v1.x form.
    let si = read_si_full(Path::new(BASIC_IMG), "/hello.txt").unwrap();
    assert!(
        si.v3.is_none(),
        "expected None for 48-byte SI, got {:?}",
        si.v3
    );
    // The 48-byte common fields still decode normally.
    assert!(si.creation_time > 0);
    assert!(si.modification_time > 0);
}

#[test]
fn security_id_visible_via_full_read() {
    let (img, path) = working_copy("secid_visible");
    // Use set_security_id to point at the canonical mkfs system-files
    // DACL slot (0x100); read_si_full must surface the new value.
    set_security_id(Path::new(&img), path, 0x100).unwrap();
    let si = read_si_full(Path::new(&img), path).unwrap();
    let v3 = si.v3.unwrap();
    assert_eq!(v3.security_id, 0x100);
}

#[test]
fn timestamps_reflect_set_times_writes() {
    let (img, path) = working_copy("times");
    // 132514304000000000 = 2020-01-01 00:00:00 UTC in NT 100ns ticks.
    let t0 = 132_514_304_000_000_000u64;
    set_times(
        Path::new(&img),
        path,
        FileTimes {
            creation: Some(t0),
            modification: Some(t0 + 10),
            mft_record_modification: Some(t0 + 20),
            access: Some(t0 + 30),
        },
    )
    .unwrap();
    let si = read_si_full(Path::new(&img), path).unwrap();
    assert_eq!(si.creation_time, t0);
    assert_eq!(si.modification_time, t0 + 10);
    assert_eq!(si.mft_modification_time, t0 + 20);
    assert_eq!(si.access_time, t0 + 30);
}

#[test]
fn missing_file_returns_err() {
    let err = read_si_full(Path::new(BASIC_IMG), "/no_such_file.bin").unwrap_err();
    assert!(!err.is_empty(), "expected non-empty error string");
}
