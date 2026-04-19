//! C-ABI tests for `fs_ntfs_create_file` and `fs_ntfs_mkdir`.
//!
//! Drives the FFI boundary: CString marshalling, null-pointer rejection,
//! return-code semantics, and `fs_ntfs_last_error` propagation.

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_create_file, fs_ntfs_last_error, fs_ntfs_mkdir};
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::Ntfs;
use std::ffi::{CStr, CString};
use std::io::BufReader;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_create_{tag}.img");
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
fn capi_create_file_happy_path() {
    let img = working_copy("file_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("capi_new.txt").unwrap();

    let rn = unsafe { fs_ntfs_create_file(img_c.as_ptr(), parent_c.as_ptr(), name_c.as_ptr()) };
    assert!(
        rn >= 16,
        "expected record num >=16, got {rn}, err={}",
        last_error()
    );
    assert!(file_exists(&img, "/", "capi_new.txt"));
}

#[test]
fn capi_create_file_rejects_null_image() {
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("x.txt").unwrap();
    let rn = unsafe { fs_ntfs_create_file(std::ptr::null(), parent_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rn, -1);
    assert!(last_error().contains("image"));
}

#[test]
fn capi_create_file_rejects_null_basename() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let parent_c = CString::new("/").unwrap();
    let rn = unsafe { fs_ntfs_create_file(img_c.as_ptr(), parent_c.as_ptr(), std::ptr::null()) };
    assert_eq!(rn, -1);
    assert!(last_error().contains("basename"));
}

#[test]
fn capi_create_file_duplicate_name_fails() {
    let img = working_copy("file_dup");
    let img_c = CString::new(img.as_str()).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("dupfile.txt").unwrap();
    let rn1 = unsafe { fs_ntfs_create_file(img_c.as_ptr(), parent_c.as_ptr(), name_c.as_ptr()) };
    assert!(rn1 >= 16);
    let rn2 = unsafe { fs_ntfs_create_file(img_c.as_ptr(), parent_c.as_ptr(), name_c.as_ptr()) };
    assert_eq!(rn2, -1);
    assert!(!last_error().is_empty());
}

#[test]
fn capi_mkdir_happy_path() {
    let img = working_copy("mkdir_happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let parent_c = CString::new("/").unwrap();
    let name_c = CString::new("newdir").unwrap();

    let rn = unsafe { fs_ntfs_mkdir(img_c.as_ptr(), parent_c.as_ptr(), name_c.as_ptr()) };
    assert!(rn >= 16, "err={}", last_error());
    assert!(file_exists(&img, "/", "newdir"));
}

#[test]
fn capi_mkdir_rejects_null_parent() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let name_c = CString::new("x").unwrap();
    let rn = unsafe { fs_ntfs_mkdir(img_c.as_ptr(), std::ptr::null(), name_c.as_ptr()) };
    assert_eq!(rn, -1);
    assert!(last_error().contains("parent_path"));
}

#[test]
fn capi_mkdir_nested_works() {
    let img = working_copy("mkdir_nested");
    let img_c = CString::new(img.as_str()).unwrap();
    let a_c = CString::new("a").unwrap();
    let b_c = CString::new("b").unwrap();
    let root_c = CString::new("/").unwrap();
    let parent_c = CString::new("/a").unwrap();

    let ra = unsafe { fs_ntfs_mkdir(img_c.as_ptr(), root_c.as_ptr(), a_c.as_ptr()) };
    assert!(ra >= 16, "err={}", last_error());
    let rb = unsafe { fs_ntfs_mkdir(img_c.as_ptr(), parent_c.as_ptr(), b_c.as_ptr()) };
    assert!(rb >= 16, "err={}", last_error());
    assert!(file_exists(&img, "/a", "b"));
}
