//! C-ABI tests for `.` and `..` path-component handling (§1.6 fix).

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_mount, fs_ntfs_read_file, fs_ntfs_stat, fs_ntfs_umount};
use std::ffi::CString;
use std::os::raw::c_void;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

#[repr(C)]
#[derive(Default)]
struct FsNtfsAttr {
    file_record_number: u64,
    size: u64,
    atime: u32,
    mtime: u32,
    ctime: u32,
    crtime: u32,
    mode: u16,
    link_count: u16,
    file_type: u32,
    attributes: u32,
}

fn mount() -> *mut std::ffi::c_void {
    let c = CString::new(BASIC_IMG).unwrap();
    let fs = unsafe { fs_ntfs_mount(c.as_ptr()) } as *mut std::ffi::c_void;
    assert!(!fs.is_null(), "mount failed");
    fs
}

fn stat(fs: *mut std::ffi::c_void, path: &str) -> Option<FsNtfsAttr> {
    let mut attr = FsNtfsAttr::default();
    let c = CString::new(path).unwrap();
    let rc = unsafe {
        fs_ntfs_stat(
            fs as *mut _,
            c.as_ptr(),
            &mut attr as *mut FsNtfsAttr as *mut _,
        )
    };
    if rc == 0 {
        Some(attr)
    } else {
        None
    }
}

fn read_file_bytes(fs: *mut std::ffi::c_void, path: &str) -> Vec<u8> {
    let attr = stat(fs, path).expect("stat");
    let mut buf = vec![0u8; attr.size as usize];
    let c = CString::new(path).unwrap();
    let n = unsafe {
        fs_ntfs_read_file(
            fs as *mut _,
            c.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            0,
            attr.size,
        )
    };
    assert!(n >= 0, "read_file");
    buf.truncate(n as usize);
    buf
}

#[test]
fn dot_component_is_noop() {
    let fs = mount();
    let a = stat(fs, "/hello.txt").expect("hello.txt exists");
    let b = stat(fs, "/./hello.txt").expect("/./hello.txt should resolve");
    assert_eq!(a.file_record_number, b.file_record_number);
    unsafe { fs_ntfs_umount(fs as *mut _) };
}

#[test]
fn dotdot_walks_to_parent() {
    let fs = mount();
    // /Documents/../hello.txt == /hello.txt
    let direct = stat(fs, "/hello.txt").expect("hello.txt");
    let via_dotdot = stat(fs, "/Documents/../hello.txt").expect("dotdot path");
    assert_eq!(direct.file_record_number, via_dotdot.file_record_number);
    unsafe { fs_ntfs_umount(fs as *mut _) };
}

#[test]
fn multiple_dotdots_still_resolve() {
    let fs = mount();
    // /Documents/readme.txt contents via ./ and ../ patterns.
    let content_a = read_file_bytes(fs, "/Documents/readme.txt");
    let content_b = read_file_bytes(fs, "/Documents/./../Documents/readme.txt");
    assert_eq!(content_a, content_b);
    unsafe { fs_ntfs_umount(fs as *mut _) };
}

#[test]
fn dotdot_from_root_stays_at_root() {
    let fs = mount();
    // /../hello.txt should still resolve (.. at root is a no-op).
    let a = stat(fs, "/hello.txt").expect("direct");
    let b = stat(fs, "/../hello.txt").expect("dotdot-from-root");
    assert_eq!(a.file_record_number, b.file_record_number);
    unsafe { fs_ntfs_umount(fs as *mut _) };
}

#[test]
fn trailing_dot_on_directory_path() {
    let fs = mount();
    let a = stat(fs, "/Documents").expect("direct");
    let b = stat(fs, "/Documents/.").expect("trailing dot");
    assert_eq!(a.file_record_number, b.file_record_number);
    assert_eq!(b.file_type, 2); // FS_NTFS_FT_DIR
    unsafe { fs_ntfs_umount(fs as *mut _) };
}
