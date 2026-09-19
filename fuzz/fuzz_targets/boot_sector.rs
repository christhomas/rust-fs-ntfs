#![no_main]
//! The boot sector.
//!
//! `clusters_per_mft_record` is a SIGNED byte: positive means clusters,
//! negative means a power-of-two byte count. Bytes-per-sector times
//! sectors-per-cluster is the unit every run list is measured in, and
//! the MFT's location is a cluster number multiplied by it.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut io = fs_ntfs_fuzz::Bytes(data.to_vec());
    let _ = fs_ntfs::mft_io::read_boot_params_io(&mut io);
});
