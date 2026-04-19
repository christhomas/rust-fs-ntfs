//! Tests for volume statistics (§4.3): free-cluster count,
//! MFT free-record count, dirty flag.

#![allow(unused_unsafe)]

use fs_ntfs::facade::Filesystem;
use fs_ntfs::{fs_ntfs_free_clusters, fs_ntfs_mft_free_records};
use std::ffi::CString;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

#[test]
fn facade_volume_stats_populates_fields() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    let s = fs.volume_stats().unwrap();
    assert!(s.total_clusters > 0);
    assert!(s.free_clusters > 0);
    assert!(s.free_clusters < s.total_clusters);
    assert!(s.mft_total_records > 0);
    assert!(s.mft_free_records > 0);
    assert!(s.mft_free_records <= s.mft_total_records);
}

#[test]
fn capi_free_clusters() {
    let p = CString::new(BASIC_IMG).unwrap();
    let n = unsafe { fs_ntfs_free_clusters(p.as_ptr()) };
    assert!(n > 0);
}

#[test]
fn capi_free_clusters_null_path() {
    let n = unsafe { fs_ntfs_free_clusters(std::ptr::null()) };
    assert_eq!(n, -1);
}

#[test]
fn capi_mft_free_records() {
    let p = CString::new(BASIC_IMG).unwrap();
    let n = unsafe { fs_ntfs_mft_free_records(p.as_ptr()) };
    assert!(n > 0);
}

#[test]
fn capi_mft_free_records_null_path() {
    let n = unsafe { fs_ntfs_mft_free_records(std::ptr::null()) };
    assert_eq!(n, -1);
}

#[test]
fn free_clusters_decreases_after_allocation() {
    // Copy the fixture and write a multi-cluster file. The free count
    // should drop by the number of clusters the file consumes.
    let dst = "test-disks/_volstats_alloc.img";
    std::fs::copy(BASIC_IMG, dst).unwrap();
    let fs = Filesystem::mount(dst).unwrap();
    let before = fs.volume_stats().unwrap().free_clusters;
    // Promote hello.txt with a payload that forces clustering.
    let payload = vec![0xAB; 16384]; // 16 KiB
    fs.write_file_contents("/hello.txt", &payload).unwrap();
    let after = fs.volume_stats().unwrap().free_clusters;
    assert!(after < before, "free clusters: before={before} after={after}");
}
