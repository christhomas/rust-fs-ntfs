//! Tests for reparse-tag dispatch in fill_attr + proper fs_ntfs_readlink
//! (§1.2 + §3.6).

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_mount, fs_ntfs_readlink, fs_ntfs_stat, fs_ntfs_umount, write};
use std::ffi::CString;
use std::os::raw::c_char;
use std::path::Path;

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

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_readlink_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

fn stat_via_mount(img: &str, path: &str) -> Option<FsNtfsAttr> {
    let img_c = CString::new(img).unwrap();
    let fs = unsafe { fs_ntfs_mount(img_c.as_ptr()) };
    if fs.is_null() {
        return None;
    }
    let mut attr = FsNtfsAttr::default();
    let p_c = CString::new(path).unwrap();
    let rc = unsafe { fs_ntfs_stat(fs, p_c.as_ptr(), &mut attr as *mut FsNtfsAttr as *mut _) };
    unsafe { fs_ntfs_umount(fs) };
    if rc == 0 {
        Some(attr)
    } else {
        None
    }
}

fn readlink_via_mount(img: &str, path: &str) -> Result<String, String> {
    let img_c = CString::new(img).unwrap();
    let fs = unsafe { fs_ntfs_mount(img_c.as_ptr()) };
    if fs.is_null() {
        return Err("mount failed".to_string());
    }
    let p_c = CString::new(path).unwrap();
    let mut buf = vec![0u8; 512];
    let rc =
        unsafe { fs_ntfs_readlink(fs, p_c.as_ptr(), buf.as_mut_ptr() as *mut c_char, buf.len()) };
    unsafe { fs_ntfs_umount(fs) };
    if rc < 0 {
        return Err("readlink returned -1".to_string());
    }
    let n = rc as usize;
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

#[test]
fn symlink_target_readback() {
    let img = working_copy("symlink");
    let target = r"\??\C:\Windows\System32";
    write::create_symlink(Path::new(&img), "/Documents", "shortcut", target, false)
        .expect("create_symlink");

    let got = readlink_via_mount(&img, "/Documents/shortcut").expect("readlink");
    // NT-prefix is stripped for POSIX callers.
    assert_eq!(got, "C:\\Windows\\System32");
}

#[test]
fn readlink_fails_on_non_reparse() {
    let img = working_copy("non_reparse");
    let err = readlink_via_mount(&img, "/hello.txt").unwrap_err();
    assert!(err.contains("readlink returned -1") || err.contains("not a reparse"));
}

#[test]
fn symlink_stat_reports_ft_symlink() {
    let img = working_copy("symlink_stat");
    write::create_symlink(Path::new(&img), "/Documents", "link", r"\??\D:\x", false)
        .expect("create_symlink");
    let a = stat_via_mount(&img, "/Documents/link").expect("stat");
    assert_eq!(a.file_type, 7, "symlink → FS_NTFS_FT_SYMLINK");
}

#[test]
fn non_symlink_reparse_doesnt_trigger_symlink_type() {
    // Write a reparse point with a tag we don't specifically handle
    // (LX_SYMLINK 0xA000001D). stat should NOT report FT_SYMLINK.
    let img = working_copy("non_symlink_reparse");
    write::write_reparse_point(
        Path::new(&img),
        "/Documents/readme.txt",
        0xA000_001D,
        b"\x08\x00\x00\x00dummy lxsymlink payload",
    )
    .expect("write_reparse_point");

    let a = stat_via_mount(&img, "/Documents/readme.txt").expect("stat");
    // Should fall back to regular file (was a regular file before).
    assert_eq!(a.file_type, 1, "non-symlink reparse shouldn't mis-type");
}

#[test]
fn mount_point_reparse_reports_junction() {
    // MOUNT_POINT tag 0xA0000003.
    let img = working_copy("mount_point");
    // On a regular file, the directory flag stays 0 — but mount points
    // are always on directories. For a simple test, write the tag on
    // a directory. /Documents is one.
    write::write_reparse_point(
        Path::new(&img),
        "/Documents",
        0xA000_0003,
        // Minimal MountPointReparseBuffer: substitute offset/length + print offset/length + empty PathBuffer.
        &[0u8; 8],
    )
    .expect("write_reparse_point");
    let a = stat_via_mount(&img, "/Documents").expect("stat");
    assert_eq!(a.file_type, 8, "mount point → FS_NTFS_FT_JUNCTION");
}

#[test]
fn readlink_buffer_too_small_errors() {
    let img = working_copy("small_buf");
    write::create_symlink(
        Path::new(&img),
        "/Documents",
        "lnk",
        r"\??\a_long_enough_target_path",
        false,
    )
    .expect("create_symlink");

    let img_c = CString::new(img.as_str()).unwrap();
    let fs = unsafe { fs_ntfs_mount(img_c.as_ptr()) };
    assert!(!fs.is_null());
    let p_c = CString::new("/Documents/lnk").unwrap();
    let mut tiny = vec![0u8; 4];
    let rc = unsafe {
        fs_ntfs_readlink(
            fs,
            p_c.as_ptr(),
            tiny.as_mut_ptr() as *mut c_char,
            tiny.len(),
        )
    };
    unsafe { fs_ntfs_umount(fs) };
    assert_eq!(rc, -1, "tiny buffer should fail");
}
