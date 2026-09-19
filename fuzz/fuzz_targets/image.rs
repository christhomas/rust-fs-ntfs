#![no_main]
//! A whole volume, opened and read.
//!
//! The boot sector's geometry decides where the MFT is and how big a
//! record is; the record's fixup rewrites bytes at offsets the record
//! itself declares; the attributes behind that declare their own
//! lengths. Only opening a volume reaches all of them, and all of it
//! runs before anything has been validated.
use fs_ntfs_fuzz::walk;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    walk(data);
});
