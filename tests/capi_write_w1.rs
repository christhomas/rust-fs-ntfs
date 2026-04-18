//! C-ABI tests for W1 in-place write functions (set_times, chattr).

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_chattr, fs_ntfs_last_error, fs_ntfs_set_times};
use ntfs::structured_values::NtfsStandardInformation;
use ntfs::{Ntfs, NtfsAttributeType};
use std::ffi::{CStr, CString};
use std::io::BufReader;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_w1_{tag}.img");
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

fn read_si(img: &str, path: &str) -> NtfsStandardInformation {
    let f = std::fs::File::open(img).unwrap();
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).unwrap();
    ntfs.read_upcase_table(&mut reader).unwrap();
    let mut cur = ntfs.root_directory(&mut reader).unwrap();
    for comp in path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut reader).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut reader).unwrap();
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() == Some(NtfsAttributeType::StandardInformation) {
            return a
                .resident_structured_value::<NtfsStandardInformation>()
                .unwrap();
        }
    }
    panic!("no SI");
}

#[test]
fn capi_set_times_sets_all_four() {
    let img = working_copy("times_all");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let creation = 130_500_000_000_000_000i64;
    let modification = 130_600_000_000_000_000i64;
    let ctime = 130_700_000_000_000_000i64;
    let atime = 130_800_000_000_000_000i64;

    let rc = unsafe {
        fs_ntfs_set_times(
            img_c.as_ptr(),
            p_c.as_ptr(),
            &creation,
            &modification,
            &ctime,
            &atime,
        )
    };
    assert_eq!(rc, 0, "last_error={}", last_error());

    let si = read_si(&img, "/hello.txt");
    assert_eq!(si.creation_time().nt_timestamp(), creation as u64);
    assert_eq!(si.modification_time().nt_timestamp(), modification as u64);
    assert_eq!(
        si.mft_record_modification_time().nt_timestamp(),
        ctime as u64
    );
    assert_eq!(si.access_time().nt_timestamp(), atime as u64);
}

#[test]
fn capi_set_times_skips_null_fields() {
    let img = working_copy("times_partial");
    let before = read_si(&img, "/hello.txt");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let new_mtime = 131_000_000_000_000_000i64;

    let rc = unsafe {
        fs_ntfs_set_times(
            img_c.as_ptr(),
            p_c.as_ptr(),
            std::ptr::null(),
            &new_mtime,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0);

    let after = read_si(&img, "/hello.txt");
    assert_eq!(after.modification_time().nt_timestamp(), new_mtime as u64);
    assert_eq!(
        after.creation_time().nt_timestamp(),
        before.creation_time().nt_timestamp()
    );
    assert_eq!(
        after.access_time().nt_timestamp(),
        before.access_time().nt_timestamp()
    );
}

#[test]
fn capi_set_times_rejects_null_path() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let rc = unsafe {
        fs_ntfs_set_times(
            img_c.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, -1);
    assert!(!last_error().is_empty());
}

#[test]
fn capi_chattr_adds_readonly() {
    let img = working_copy("chattr_add_ro");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe { fs_ntfs_chattr(img_c.as_ptr(), p_c.as_ptr(), 0x01, 0) };
    assert_eq!(rc, 0, "last_error={}", last_error());
    let si = read_si(&img, "/hello.txt");
    assert_ne!(si.file_attributes().bits() & 0x01, 0);
}

#[test]
fn capi_chattr_removes_bit() {
    let img = working_copy("chattr_remove");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    // Add ARCHIVE, then remove it.
    unsafe { fs_ntfs_chattr(img_c.as_ptr(), p_c.as_ptr(), 0x20, 0) };
    assert_ne!(
        read_si(&img, "/hello.txt").file_attributes().bits() & 0x20,
        0
    );
    unsafe { fs_ntfs_chattr(img_c.as_ptr(), p_c.as_ptr(), 0, 0x20) };
    assert_eq!(
        read_si(&img, "/hello.txt").file_attributes().bits() & 0x20,
        0
    );
}

#[test]
fn capi_chattr_rejects_overlap() {
    let img_c = CString::new(BASIC_IMG).unwrap();
    let p_c = CString::new("/hello.txt").unwrap();
    let rc = unsafe { fs_ntfs_chattr(img_c.as_ptr(), p_c.as_ptr(), 0x01, 0x01) };
    assert_eq!(rc, -1);
    let err = last_error();
    assert!(err.contains("overlap"), "{err:?}");
}
