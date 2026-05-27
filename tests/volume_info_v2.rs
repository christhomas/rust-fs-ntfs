//! Tests for `fs_ntfs_get_volume_info_v2` — the extended volume-info
//! reader added per future-features.md §7 (audit follow-up).

#![allow(unused_unsafe)]

use fs_ntfs::{
    fs_ntfs_get_volume_info_v2, fs_ntfs_mount, fs_ntfs_umount, FsNtfsVolumeInfo, FsNtfsVolumeInfoV2,
};
use std::ffi::CString;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn mount(img: &str) -> *mut fs_ntfs::FsNtfsHandle {
    let c = CString::new(img).unwrap();
    unsafe { fs_ntfs_mount(c.as_ptr()) }
}

fn zero_v2() -> FsNtfsVolumeInfoV2 {
    // Safe: FsNtfsVolumeInfoV2 is repr(C) with no Drop and only
    // primitive fields.
    unsafe { std::mem::zeroed() }
}

#[test]
fn v2_populates_basic_fields() {
    let h = mount(BASIC_IMG);
    assert!(!h.is_null());
    let mut v2 = zero_v2();
    let rc = unsafe { fs_ntfs_get_volume_info_v2(h, &mut v2) };
    assert_eq!(rc, 0);

    assert!(v2.cluster_size > 0 && v2.cluster_size.is_power_of_two());
    assert!(v2.total_clusters > 0);
    assert!(v2.total_size > 0);
    assert!(v2.ntfs_version_major > 0);
    // Serial number could be any value; just confirm it isn't an
    // uninitialized 0xFFFFFFFF_FFFFFFFF or similar suspicious pattern.
    assert_ne!(v2.serial_number, 0xFFFFFFFFFFFFFFFF);

    unsafe { fs_ntfs_umount(h) };
}

#[test]
fn v2_reports_mft_record_size_and_sector_size() {
    let h = mount(BASIC_IMG);
    let mut v2 = zero_v2();
    let rc = unsafe { fs_ntfs_get_volume_info_v2(h, &mut v2) };
    assert_eq!(rc, 0);
    // Typical NTFS volumes ship 1024- or 4096-byte MFT records and
    // 512- or 4096-byte sectors. Whatever the fixture uses, both
    // must be powers of two > 0.
    assert!(v2.mft_record_size > 0 && v2.mft_record_size.is_power_of_two());
    assert!(v2.bytes_per_sector > 0 && v2.bytes_per_sector.is_power_of_two());
    unsafe { fs_ntfs_umount(h) };
}

#[test]
fn v2_dirty_bit_consistent_with_flags_bit_0() {
    let h = mount(BASIC_IMG);
    let mut v2 = zero_v2();
    let rc = unsafe { fs_ntfs_get_volume_info_v2(h, &mut v2) };
    assert_eq!(rc, 0);
    let bit0 = (v2.volume_flags & 0x0001) != 0;
    assert_eq!(v2.is_dirty != 0, bit0);
    unsafe { fs_ntfs_umount(h) };
}

#[test]
fn v2_rejects_null_handle() {
    let mut v2 = zero_v2();
    let rc = unsafe { fs_ntfs_get_volume_info_v2(std::ptr::null_mut(), &mut v2) };
    assert_eq!(rc, -1);
}

#[test]
fn v2_rejects_null_info() {
    let h = mount(BASIC_IMG);
    let rc = unsafe { fs_ntfs_get_volume_info_v2(h, std::ptr::null_mut()) };
    assert_eq!(rc, -1);
    unsafe { fs_ntfs_umount(h) };
}

// -- Compile-time sanity: prove every v1 field starts at the same
// offset in v2 as it does in v1. If this test compiles + passes,
// the "v1 fields land at identical offsets" guarantee holds for
// the build it was compiled against.
#[test]
fn v1_fields_at_same_offsets_in_v2() {
    use std::mem::offset_of;
    assert_eq!(
        offset_of!(FsNtfsVolumeInfoV2, volume_name),
        offset_of!(FsNtfsVolumeInfo, volume_name)
    );
    assert_eq!(
        offset_of!(FsNtfsVolumeInfoV2, cluster_size),
        offset_of!(FsNtfsVolumeInfo, cluster_size)
    );
    assert_eq!(
        offset_of!(FsNtfsVolumeInfoV2, total_clusters),
        offset_of!(FsNtfsVolumeInfo, total_clusters)
    );
    assert_eq!(
        offset_of!(FsNtfsVolumeInfoV2, ntfs_version_major),
        offset_of!(FsNtfsVolumeInfo, ntfs_version_major)
    );
    assert_eq!(
        offset_of!(FsNtfsVolumeInfoV2, serial_number),
        offset_of!(FsNtfsVolumeInfo, serial_number)
    );
    assert_eq!(
        offset_of!(FsNtfsVolumeInfoV2, total_size),
        offset_of!(FsNtfsVolumeInfo, total_size)
    );
}
