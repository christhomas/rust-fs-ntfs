//! C-ABI tests for `fs_ntfs_unlink` and `fs_ntfs_rmdir`.

#![allow(unused_unsafe)]

use fs_ntfs::{
    fs_ntfs_create_file, fs_ntfs_last_error, fs_ntfs_mkdir, fs_ntfs_rmdir, fs_ntfs_unlink,
};
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::Ntfs;
use std::ffi::{CStr, CString};
use std::io::BufReader;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_remove_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn last_error() -> String {
    unsafe {
        let p = fs_ntfs_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn file_exists(img: &str, parent: &str, name: &str) -> bool {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in parent.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    let idx = cur.directory_index(&mut r).unwrap();
    let mut finder = idx.finder();
    NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, name).is_some()
}

#[test]
fn capi_unlink_happy_path() {
    let img = working_copy("unlink_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("to_remove.txt").unwrap();
    unsafe { fs_ntfs_create_file(img_c.as_ptr(), parent_c.as_ptr(), name_c.as_ptr()) };
    assert!(file_exists(&img, "/", "to_remove.txt"));

    let path_c = CString::new("/to_remove.txt").unwrap();
    let rc = unsafe { fs_ntfs_unlink(img_c.as_ptr(), path_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    assert!(!file_exists(&img, "/", "to_remove.txt"));
}

#[test]
fn capi_unlink_missing_file_fails() {
    let img = working_copy("unlink_missing");
    let img_c = CString::new(img.as_str()).unwrap();
    let path_c = CString::new("/does_not_exist.txt").unwrap();
    let rc = unsafe { fs_ntfs_unlink(img_c.as_ptr(), path_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}

#[test]
fn capi_unlink_rejects_null_path() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let rc = unsafe { fs_ntfs_unlink(img_c.as_ptr(), std::ptr::null()) };
    assert_eq!(rc, -1);
    assert!(last_error().contains("path"));
}

#[test]
fn capi_rmdir_happy_path() {
    let img = working_copy("rmdir_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("empty_dir").unwrap();
    unsafe { fs_ntfs_mkdir(img_c.as_ptr(), parent_c.as_ptr(), name_c.as_ptr()) };
    assert!(file_exists(&img, "/", "empty_dir"));

    let path_c = CString::new("/empty_dir").unwrap();
    let rc = unsafe { fs_ntfs_rmdir(img_c.as_ptr(), path_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    assert!(!file_exists(&img, "/", "empty_dir"));
}

#[test]
fn capi_rmdir_nonempty_fails() {
    let img = working_copy("rmdir_nonempty");
    let img_c = CString::new(img.as_str()).unwrap();
    let root_c = CString::new("/").unwrap();
    let d_c = CString::new("nd").unwrap();
    unsafe { fs_ntfs_mkdir(img_c.as_ptr(), root_c.as_ptr(), d_c.as_ptr()) };
    let dp_c = CString::new("/nd").unwrap();
    let child_c = CString::new("file.txt").unwrap();
    unsafe { fs_ntfs_create_file(img_c.as_ptr(), dp_c.as_ptr(), child_c.as_ptr()) };

    let rc = unsafe { fs_ntfs_rmdir(img_c.as_ptr(), dp_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}

#[test]
fn capi_rmdir_rejects_null_image() {
    let p_c = CString::new("/x").unwrap();
    let rc = unsafe { fs_ntfs_rmdir(std::ptr::null(), p_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(last_error().contains("image"));
}
