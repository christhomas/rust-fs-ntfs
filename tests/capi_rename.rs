//! C-ABI tests for `fs_ntfs_rename` and `fs_ntfs_rename_same_length`.

#![allow(unused_unsafe)]

use fs_ntfs::{
    fs_ntfs_create_file, fs_ntfs_last_error, fs_ntfs_mkdir, fs_ntfs_rename,
    fs_ntfs_rename_same_length,
};
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::Ntfs;
use std::ffi::{CStr, CString};
use std::io::BufReader;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_rename_{tag}.img");
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
fn capi_rename_same_length_happy() {
    let img = working_copy("sl_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let root_c = CString::new("/").unwrap();
    let old_c = CString::new("abcd.txt").unwrap();
    unsafe { fs_ntfs_create_file(img_c.as_ptr(), root_c.as_ptr(), old_c.as_ptr()) };
    let old_path_c = CString::new("/abcd.txt").unwrap();
    let new_c = CString::new("zyxw.txt").unwrap();

    let rc =
        unsafe { fs_ntfs_rename_same_length(img_c.as_ptr(), old_path_c.as_ptr(), new_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    assert!(!file_exists(&img, "/", "abcd.txt"));
    assert!(file_exists(&img, "/", "zyxw.txt"));
}

#[test]
fn capi_rename_same_length_rejects_length_change() {
    let img = working_copy("sl_wronglen");
    let img_c = CString::new(img.as_str()).unwrap();
    let root_c = CString::new("/").unwrap();
    let old_c = CString::new("abc.txt").unwrap();
    unsafe { fs_ntfs_create_file(img_c.as_ptr(), root_c.as_ptr(), old_c.as_ptr()) };
    let old_path_c = CString::new("/abc.txt").unwrap();
    let new_c = CString::new("longer_name.txt").unwrap();

    let rc =
        unsafe { fs_ntfs_rename_same_length(img_c.as_ptr(), old_path_c.as_ptr(), new_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}

#[test]
fn capi_rename_variable_length_happy() {
    // Parent must be a freshly-created subdir; root of ntfs-basic
    // has $INDEX_ALLOCATION overflow which variable-length rename
    // doesn't yet support.
    let img = working_copy("vl_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let root_c = CString::new("/").unwrap();
    let dir_c = CString::new("rnd").unwrap();
    unsafe { fs_ntfs_mkdir(img_c.as_ptr(), root_c.as_ptr(), dir_c.as_ptr()) };
    let subdir_c = CString::new("/rnd").unwrap();
    let old_c = CString::new("short.txt").unwrap();
    unsafe { fs_ntfs_create_file(img_c.as_ptr(), subdir_c.as_ptr(), old_c.as_ptr()) };
    let old_path_c = CString::new("/rnd/short.txt").unwrap();
    let new_c = CString::new("a_much_longer_name.txt").unwrap();

    let rc = unsafe { fs_ntfs_rename(img_c.as_ptr(), old_path_c.as_ptr(), new_c.as_ptr()) };
    assert_eq!(rc, 0, "err={}", last_error());
    assert!(!file_exists(&img, "/rnd", "short.txt"));
    assert!(file_exists(&img, "/rnd", "a_much_longer_name.txt"));
}

#[test]
fn capi_rename_rejects_null_path() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let new_c = CString::new("x").unwrap();
    let rc = unsafe { fs_ntfs_rename(img_c.as_ptr(), std::ptr::null(), new_c.as_ptr()) };
    assert_eq!(rc, -1);
    assert!(last_error().contains("old_path"));
}

#[test]
fn capi_rename_same_length_rejects_null_name() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let old_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe {
        fs_ntfs_rename_same_length(img_c.as_ptr(), old_c.as_ptr(), std::ptr::null())
    };
    assert_eq!(rc, -1);
    assert!(last_error().contains("new_name"));
}
