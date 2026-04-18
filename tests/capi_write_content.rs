//! C-ABI tests for `fs_ntfs_write_file`.

#![allow(unused_unsafe)]

use fs_ntfs::{fs_ntfs_last_error, fs_ntfs_write_file};
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::ffi::{CStr, CString};
use std::io::BufReader;

const LARGE_IMG: &str = "test-disks/ntfs-large-file.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_capi_write_content_{tag}.img");
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

fn read_range(img: &str, path: &str, off: u64, len: usize) -> Vec<u8> {
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
        let mut v = a.value(&mut r).unwrap();
        v.seek(&mut r, std::io::SeekFrom::Start(off)).unwrap();
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = v.read(&mut r, &mut buf[filled..]).unwrap();
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        return buf;
    }
    panic!("no $DATA");
}

#[test]
fn capi_write_file_happy_path() {
    let img = working_copy("happy");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/big.bin").unwrap();
    let payload = b"C_ABI_WRITE";
    let off = 4u64 * 1024 * 1024;

    let n = unsafe {
        fs_ntfs_write_file(
            img_c.as_ptr(),
            p_c.as_ptr(),
            off,
            payload.as_ptr() as *const std::ffi::c_void,
            payload.len() as u64,
        )
    };
    assert_eq!(n, payload.len() as i64, "last_error={}", last_error());

    let readback = read_range(&img, "/big.bin", off, payload.len());
    assert_eq!(readback, payload);
}

#[test]
fn capi_write_file_zero_length_is_noop() {
    let img_c = CString::new(LARGE_IMG).unwrap();
    let p_c = CString::new("/big.bin").unwrap();
    let n = unsafe { fs_ntfs_write_file(img_c.as_ptr(), p_c.as_ptr(), 0, std::ptr::null(), 0) };
    assert_eq!(n, 0);
}

#[test]
fn capi_write_file_rejects_null_buf_with_len() {
    let img_c = CString::new(LARGE_IMG).unwrap();
    let p_c = CString::new("/big.bin").unwrap();
    let n = unsafe { fs_ntfs_write_file(img_c.as_ptr(), p_c.as_ptr(), 0, std::ptr::null(), 10) };
    assert_eq!(n, -1);
    assert!(last_error().contains("null") || last_error().contains("buffer"));
}

#[test]
fn capi_write_file_rejects_past_eof() {
    let img = working_copy("past_eof");
    let img_c = CString::new(img.as_str()).unwrap();
    let p_c = CString::new("/big.bin").unwrap();
    let payload = b"XYZ";
    let n = unsafe {
        fs_ntfs_write_file(
            img_c.as_ptr(),
            p_c.as_ptr(),
            8 * 1024 * 1024 - 1,
            payload.as_ptr() as *const std::ffi::c_void,
            payload.len() as u64,
        )
    };
    assert_eq!(n, -1);
    assert!(last_error().contains("EOF"));
}
