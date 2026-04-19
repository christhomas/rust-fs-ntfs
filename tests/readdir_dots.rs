//! Tests for synthesized "." / ".." in directory listings (§1.7).

#![allow(unused_unsafe)]

use fs_ntfs::{
    fs_ntfs_dir_close, fs_ntfs_dir_next, fs_ntfs_dir_open, fs_ntfs_mount, fs_ntfs_umount,
};
use std::ffi::CString;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

#[repr(C)]
struct FsNtfsDirent {
    file_record_number: u64,
    file_type: u8,
    name_len: u16,
    name: [u8; 256],
}

fn collect_names(fs: *mut std::ffi::c_void, path: &str) -> Vec<String> {
    let c = CString::new(path).unwrap();
    let iter = unsafe { fs_ntfs_dir_open(fs as *mut _, c.as_ptr()) };
    assert!(!iter.is_null(), "dir_open");
    let mut names = Vec::new();
    loop {
        let ent = unsafe { fs_ntfs_dir_next(iter) };
        if ent.is_null() {
            break;
        }
        let d = unsafe { &*(ent as *const FsNtfsDirent) };
        let n = d.name_len as usize;
        let s = std::str::from_utf8(&d.name[..n]).unwrap().to_string();
        names.push(s);
    }
    unsafe { fs_ntfs_dir_close(iter) };
    names
}

fn mount() -> *mut std::ffi::c_void {
    let c = CString::new(BASIC_IMG).unwrap();
    unsafe { fs_ntfs_mount(c.as_ptr()) as *mut _ }
}

#[test]
fn root_dir_lists_dot_and_dotdot_first() {
    let fs = mount();
    let names = collect_names(fs, "/");
    assert_eq!(names[0], ".", "first entry must be .; got {names:?}");
    assert_eq!(names[1], "..", "second entry must be ..; got {names:?}");
    // And real entries follow.
    assert!(names.iter().any(|n| n == "hello.txt"), "{names:?}");
    unsafe { fs_ntfs_umount(fs as *mut _) };
}

#[test]
fn subdir_lists_dot_and_dotdot() {
    let fs = mount();
    let names = collect_names(fs, "/Documents");
    assert_eq!(names[0], ".");
    assert_eq!(names[1], "..");
    assert!(names.iter().any(|n| n == "readme.txt"));
    unsafe { fs_ntfs_umount(fs as *mut _) };
}

#[test]
fn dot_in_root_points_at_root_record() {
    let fs = mount();
    let c = CString::new("/").unwrap();
    let iter = unsafe { fs_ntfs_dir_open(fs as *mut _, c.as_ptr()) };
    let ent = unsafe { fs_ntfs_dir_next(iter) };
    let d = unsafe { &*(ent as *const FsNtfsDirent) };
    let name = std::str::from_utf8(&d.name[..d.name_len as usize]).unwrap();
    assert_eq!(name, ".");
    // Root directory's record number is 5.
    assert_eq!(d.file_record_number, 5);
    unsafe {
        fs_ntfs_dir_close(iter);
        fs_ntfs_umount(fs as *mut _);
    }
}

#[test]
fn dotdot_in_subdir_points_at_parent() {
    let fs = mount();
    let c = CString::new("/Documents").unwrap();
    let iter = unsafe { fs_ntfs_dir_open(fs as *mut _, c.as_ptr()) };
    // Skip past "."
    unsafe { fs_ntfs_dir_next(iter) };
    // ".."
    let ent = unsafe { fs_ntfs_dir_next(iter) };
    let d = unsafe { &*(ent as *const FsNtfsDirent) };
    let name = std::str::from_utf8(&d.name[..d.name_len as usize]).unwrap();
    assert_eq!(name, "..");
    assert_eq!(d.file_record_number, 5); // root
    unsafe {
        fs_ntfs_dir_close(iter);
        fs_ntfs_umount(fs as *mut _);
    }
}

#[test]
fn dotdot_in_root_points_at_root() {
    let fs = mount();
    let c = CString::new("/").unwrap();
    let iter = unsafe { fs_ntfs_dir_open(fs as *mut _, c.as_ptr()) };
    unsafe { fs_ntfs_dir_next(iter) }; // skip .
    let ent = unsafe { fs_ntfs_dir_next(iter) };
    let d = unsafe { &*(ent as *const FsNtfsDirent) };
    let name = std::str::from_utf8(&d.name[..d.name_len as usize]).unwrap();
    assert_eq!(name, "..");
    // At root, .. also points at root (POSIX convention).
    assert_eq!(d.file_record_number, 5);
    unsafe {
        fs_ntfs_dir_close(iter);
        fs_ntfs_umount(fs as *mut _);
    }
}
