//! Integration tests for the `$Bitmap` cluster allocator.

use fs_ntfs::bitmap::{self, BitmapLocation};
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_bitmap_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn locate_bitmap_on_basic_fixture() {
    let bm = bitmap::locate_bitmap(Path::new(BASIC_IMG)).expect("locate");
    assert!(bm.value_length > 0);
    assert!(bm.total_bits > 0);
    // basic.img is 16 MiB with 4 KiB clusters → 4096 clusters total.
    // bitmap has 4096 bits = 512 bytes (rounded up to cluster size).
    // total_bits may be padded to full bitmap byte count; just check sanity.
    assert!(bm.total_bits >= 4096);
}

#[test]
fn cluster_0_is_always_allocated() {
    // LCN 0 is the boot sector; must be marked allocated.
    let bm = bitmap::locate_bitmap(Path::new(BASIC_IMG)).expect("locate");
    assert!(bitmap::is_allocated(Path::new(BASIC_IMG), &bm, 0).unwrap());
}

#[test]
fn mft_cluster_is_allocated() {
    // The cluster at bm.params.mft_lcn must be allocated.
    let bm = bitmap::locate_bitmap(Path::new(BASIC_IMG)).expect("locate");
    let mft_lcn = bm.params.mft_lcn;
    assert!(bitmap::is_allocated(Path::new(BASIC_IMG), &bm, mft_lcn).unwrap());
}

#[test]
fn find_free_run_returns_some_free_cluster() {
    let bm = bitmap::locate_bitmap(Path::new(BASIC_IMG)).expect("locate");
    let free = bitmap::find_free_run(Path::new(BASIC_IMG), &bm, 1, 0)
        .expect("find_free_run")
        .expect("at least one free cluster on a 16 MiB volume");
    // The found LCN must actually be free.
    assert!(!bitmap::is_allocated(Path::new(BASIC_IMG), &bm, free).unwrap());
}

#[test]
fn allocate_then_free_roundtrip() {
    let img = working_copy("roundtrip");
    let bm = bitmap::locate_bitmap(Path::new(&img)).expect("locate");
    let lcn = bitmap::find_free_run(Path::new(&img), &bm, 4, 0)
        .unwrap()
        .expect("at least 4 free contiguous clusters");

    bitmap::allocate(Path::new(&img), &bm, lcn, 4).expect("allocate");
    for i in 0..4 {
        assert!(
            bitmap::is_allocated(Path::new(&img), &bm, lcn + i).unwrap(),
            "cluster {} should be allocated",
            lcn + i
        );
    }

    bitmap::free(Path::new(&img), &bm, lcn, 4).expect("free");
    for i in 0..4 {
        assert!(
            !bitmap::is_allocated(Path::new(&img), &bm, lcn + i).unwrap(),
            "cluster {} should be free again",
            lcn + i
        );
    }
}

#[test]
fn allocate_rejects_already_allocated() {
    let img = working_copy("reject_double_alloc");
    let bm = bitmap::locate_bitmap(Path::new(&img)).expect("locate");
    // LCN 0 is always allocated.
    let err = bitmap::allocate(Path::new(&img), &bm, 0, 1).unwrap_err();
    assert!(err.contains("already allocated"), "{err:?}");
}

#[test]
fn free_rejects_already_free() {
    let img = working_copy("reject_double_free");
    let bm = bitmap::locate_bitmap(Path::new(&img)).expect("locate");
    let free_lcn = bitmap::find_free_run(Path::new(&img), &bm, 1, 0)
        .unwrap()
        .unwrap();
    let err = bitmap::free(Path::new(&img), &bm, free_lcn, 1).unwrap_err();
    assert!(err.contains("already free"), "{err:?}");
}

#[test]
fn allocate_contiguous_rejects_overflow() {
    let img = working_copy("overflow");
    let bm = bitmap::locate_bitmap(Path::new(&img)).expect("locate");
    let last = bm.total_bits - 1;
    // Asking for 5 clusters starting at the last valid LCN overflows.
    let err = bitmap::allocate(Path::new(&img), &bm, last, 5).unwrap_err();
    assert!(err.contains("exceeds"), "{err:?}");
}

#[test]
fn find_free_run_respects_length() {
    let img = working_copy("find_len");
    let bm = bitmap::locate_bitmap(Path::new(&img)).expect("locate");
    let lcn = bitmap::find_free_run(Path::new(&img), &bm, 8, 0)
        .unwrap()
        .expect("8 contiguous free");
    // Every cluster in the run must be free at search time.
    for i in 0..8 {
        assert!(!bitmap::is_allocated(Path::new(&img), &bm, lcn + i).unwrap());
    }
}

#[test]
fn bitmap_survives_mutation_roundtrip() {
    // Allocate, then re-locate the bitmap freshly — should see our bit
    // change. This is the "does the write survive a close/reopen" check.
    let img = working_copy("survive");
    let lcn = {
        let bm = bitmap::locate_bitmap(Path::new(&img)).expect("locate");
        let lcn = bitmap::find_free_run(Path::new(&img), &bm, 1, 0)
            .unwrap()
            .unwrap();
        bitmap::allocate(Path::new(&img), &bm, lcn, 1).unwrap();
        lcn
    };
    // New locate_bitmap reads state fresh from disk.
    let bm2 = bitmap::locate_bitmap(Path::new(&img)).expect("re-locate");
    assert!(bitmap::is_allocated(Path::new(&img), &bm2, lcn).unwrap());
}

fn _unused(_x: BitmapLocation) {}
