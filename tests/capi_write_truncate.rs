//! C-ABI tests for `fs_ntfs_truncate`.

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_last_error, fs_ntfs_truncate};
use ntfs::{Ntfs, NtfsAttributeType};
use std::ffi::{CStr, CString};
use std::io::BufReader;

const LARGE_IMG: &str = "test-disks/ntfs-large-file.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_trunc_{tag}.img");
    std::fs::copy(LARGE_IMG, &dst).expect("copy");
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

fn value_length(img: &str, path: &str) -> u64 {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, comp)
            .unwrap()
            .unwrap();
        cur = e.to_file(&ntfs, &mut r).unwrap();
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        return a.value_length();
    }
    panic!("no $DATA");
}

#[test]
fn capi_truncate_shrinks() {
    let img = working_copy("shrink");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/big.bin").unwrap();

    let n = unsafe { fs_ntfs_truncate(img_c.as_ptr(), p_c.as_ptr(), 3 * 1024 * 1024) };
    assert_eq!(n, (3 * 1024 * 1024) as i64, "last_error={}", last_error());
    assert_eq!(value_length(&img, "/big.bin"), 3 * 1024 * 1024);
}

#[test]
fn capi_truncate_rejects_growth() {
    let img = working_copy("grow");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/big.bin").unwrap();
    let n = unsafe { fs_ntfs_truncate(img_c.as_ptr(), p_c.as_ptr(), 9 * 1024 * 1024) };
    assert_eq!(n, -1);
    assert!(last_error().contains("grow"));
}

#[test]
fn capi_truncate_to_zero() {
    let img = working_copy("zero");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/big.bin").unwrap();
    let n = unsafe { fs_ntfs_truncate(img_c.as_ptr(), p_c.as_ptr(), 0) };
    assert_eq!(n, 0);
    assert_eq!(value_length(&img, "/big.bin"), 0);
}

#[test]
fn capi_truncate_null_path_rejected() {
    let img_c = CString::new(LARGE_IMG).unwrap();
    let n = unsafe { fs_ntfs_truncate(img_c.as_ptr(), std::ptr::null(), 0) };
    assert_eq!(n, -1);
}
