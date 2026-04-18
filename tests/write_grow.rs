//! Tests for `write::grow_nonresident` (W2.5 grow).

use fs_ntfs::{bitmap, write};
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const LARGE_IMG: &str = "test-disks/ntfs-large-file.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_grow_{tag}.img");
    std::fs::copy(LARGE_IMG, &dst).expect("copy");
    dst
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
fn grow_adds_clusters_and_reports_new_size() {
    let img = working_copy("grow_basic");
    // Start by shrinking to 1 MiB so there's headroom to grow within the
    // volume (16 MiB total).
    write::truncate(Path::new(&img), "/big.bin", 1024 * 1024).unwrap();
    assert_eq!(value_length(&img, "/big.bin"), 1024 * 1024);

    let target = 3 * 1024 * 1024;
    let n = write::grow_nonresident(Path::new(&img), "/big.bin", target).expect("grow");
    assert_eq!(n, target);
    assert_eq!(value_length(&img, "/big.bin"), target);
}

#[test]
fn grow_new_bytes_read_as_zero() {
    // NTFS semantics: bytes past initialized_length read as zero.
    let img = working_copy("zero_tail");
    write::truncate(Path::new(&img), "/big.bin", 512 * 1024).unwrap();
    let target = 1024 * 1024;
    write::grow_nonresident(Path::new(&img), "/big.bin", target).expect("grow");

    // Read 64 bytes at the boundary: first 64 were there before (zero-pad
    // from fixture), last 64 are newly grown (must be zero).
    let boundary_bytes = read_range(&img, "/big.bin", 512 * 1024 - 32, 64);
    for (i, &b) in boundary_bytes.iter().enumerate() {
        assert_eq!(b, 0, "byte at offset {i} should be zero");
    }
    let new_tail_bytes = read_range(&img, "/big.bin", 1024 * 1024 - 64, 64);
    for (i, &b) in new_tail_bytes.iter().enumerate() {
        assert_eq!(b, 0, "new-tail byte at {i} should be zero");
    }
}

#[test]
fn grow_consumes_free_clusters_in_bitmap() {
    let img = working_copy("consumes_bitmap");
    write::truncate(Path::new(&img), "/big.bin", 256 * 1024).unwrap();

    let bm_before = bitmap::locate_bitmap(Path::new(&img)).unwrap();
    let free_before = count_free_clusters(&img, &bm_before);

    write::grow_nonresident(Path::new(&img), "/big.bin", 256 * 1024 + 8 * 4096).expect("grow");

    let bm_after = bitmap::locate_bitmap(Path::new(&img)).unwrap();
    let free_after = count_free_clusters(&img, &bm_after);

    assert!(
        free_after < free_before,
        "grow should consume free clusters; before={free_before} after={free_after}"
    );
    assert!(
        free_before - free_after >= 8,
        "should have allocated at least 8 clusters"
    );
}

fn count_free_clusters(img: &str, bm: &bitmap::BitmapLocation) -> u64 {
    // Sample: just count bits across the whole bitmap via is_allocated.
    // Slow but fine for a 4096-bit bitmap.
    let mut free = 0u64;
    for lcn in 0..bm.total_bits {
        if !bitmap::is_allocated(Path::new(img), bm, lcn).unwrap() {
            free += 1;
        }
    }
    free
}

#[test]
fn grow_rejects_shrink() {
    let img = working_copy("reject_shrink");
    let err = write::grow_nonresident(Path::new(&img), "/big.bin", 1000).unwrap_err();
    assert!(
        err.contains("not greater") || err.contains("grow"),
        "{err:?}"
    );
}

#[test]
fn upstream_mounts_and_reads_after_grow() {
    let img = working_copy("mount_after_grow");
    write::truncate(Path::new(&img), "/big.bin", 512 * 1024).unwrap();
    write::grow_nonresident(Path::new(&img), "/big.bin", 2 * 1024 * 1024).expect("grow");

    // Fresh upstream mount + read of the first byte should still be 'A'
    // (original marker preserved).
    let b = read_range(&img, "/big.bin", 0, 1);
    assert_eq!(b[0], b'A');
}
