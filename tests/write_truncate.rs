//! Integration tests for `write::truncate` (W2.5 shrink).

use fs_ntfs::bitmap;
use fs_ntfs::write;
use ntfs::{Ntfs, NtfsAttributeType};
use std::io::BufReader;
use std::path::Path;

const LARGE_IMG: &str = "test-disks/ntfs-large-file.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_truncate_{tag}.img");
    std::fs::copy(LARGE_IMG, &dst).expect("copy");
    dst
}

/// Read the unnamed $DATA value_length via upstream.
fn value_length(img: &str, file_path: &str) -> u64 {
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let mut cur = ntfs.root_directory(&mut r).unwrap();
    for comp in file_path.trim_start_matches('/').split('/') {
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
fn shrink_to_half_size() {
    let img = working_copy("half");
    assert_eq!(value_length(&img, "/big.bin"), 8 * 1024 * 1024);

    let n = write::truncate(Path::new(&img), "/big.bin", 4 * 1024 * 1024).expect("truncate");
    assert_eq!(n, 4 * 1024 * 1024);
    assert_eq!(value_length(&img, "/big.bin"), 4 * 1024 * 1024);
}

#[test]
fn shrink_to_zero() {
    let img = working_copy("zero");
    let n = write::truncate(Path::new(&img), "/big.bin", 0).expect("truncate");
    assert_eq!(n, 0);
    assert_eq!(value_length(&img, "/big.bin"), 0);
}

#[test]
fn shrink_frees_clusters_in_bitmap() {
    let img = working_copy("free_clusters");

    // Collect the LCNs that back the second half of /big.bin (they'll be
    // freed after truncate).
    let lcns_freed: Vec<u64> = {
        let f = std::fs::File::open(&img).unwrap();
        let mut r = BufReader::new(f);
        let mut ntfs = Ntfs::new(&mut r).unwrap();
        ntfs.read_upcase_table(&mut r).unwrap();
        let root = ntfs.root_directory(&mut r).unwrap();
        let idx = root.directory_index(&mut r).unwrap();
        let mut finder = idx.finder();
        let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, "big.bin")
            .unwrap()
            .unwrap();
        let file = e.to_file(&ntfs, &mut r).unwrap();
        let mut attrs = file.attributes();
        let mut found = Vec::new();
        while let Some(item) = attrs.next(&mut r) {
            let item = item.unwrap();
            let a = item.to_attribute().unwrap();
            if a.ty().ok() != Some(NtfsAttributeType::Data) {
                continue;
            }
            if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
                continue;
            }
            // Re-parse mapping_pairs ourselves through the module
            use fs_ntfs::attr_io;
            use fs_ntfs::data_runs;
            use fs_ntfs::mft_io;
            let rn = file.file_record_number();
            let (_p, rec) = mft_io::read_mft_record(Path::new(&img), rn).unwrap();
            let loc = attr_io::find_attribute(&rec, attr_io::AttrType::Data, None).unwrap();
            let mo = loc.non_resident_mapping_pairs_offset.unwrap() as usize;
            let runs = data_runs::decode_runs(
                &rec[loc.attr_offset + mo..loc.attr_offset + loc.attr_length],
            )
            .unwrap();
            // Second half: VCN >= 4 MiB / 4 KiB = 1024.
            let half_vcn = 4 * 1024 * 1024 / 4096;
            for r in &runs {
                if r.starting_vcn + r.length <= half_vcn {
                    continue;
                }
                if let Some(lcn) = r.lcn {
                    let first_vcn_in_second_half = r.starting_vcn.max(half_vcn);
                    let count = r.starting_vcn + r.length - first_vcn_in_second_half;
                    let first_lcn = lcn + (first_vcn_in_second_half - r.starting_vcn);
                    for i in 0..count {
                        found.push(first_lcn + i);
                    }
                }
            }
            break;
        }
        found
    };

    assert!(!lcns_freed.is_empty());

    let bm = bitmap::locate_bitmap(Path::new(&img)).unwrap();
    // Pre-truncate: all those clusters are allocated.
    for lcn in &lcns_freed[..lcns_freed.len().min(64)] {
        assert!(
            bitmap::is_allocated(Path::new(&img), &bm, *lcn).unwrap(),
            "cluster {lcn} should be allocated before truncate"
        );
    }

    write::truncate(Path::new(&img), "/big.bin", 4 * 1024 * 1024).unwrap();

    // Post-truncate: sample of those clusters must now be free.
    let bm2 = bitmap::locate_bitmap(Path::new(&img)).unwrap();
    for lcn in &lcns_freed[..lcns_freed.len().min(64)] {
        assert!(
            !bitmap::is_allocated(Path::new(&img), &bm2, *lcn).unwrap(),
            "cluster {lcn} should be free after truncate"
        );
    }
}

#[test]
fn shrink_rejects_growth() {
    let img = working_copy("reject_grow");
    let err = write::truncate(Path::new(&img), "/big.bin", 9 * 1024 * 1024).unwrap_err();
    assert!(err.contains("grow"), "{err:?}");
}

#[test]
fn no_op_shrink_to_same_size() {
    let img = working_copy("same");
    let n = write::truncate(Path::new(&img), "/big.bin", 8 * 1024 * 1024).expect("truncate");
    assert_eq!(n, 8 * 1024 * 1024);
    assert_eq!(value_length(&img, "/big.bin"), 8 * 1024 * 1024);
}

#[test]
fn shrink_partial_cluster_clamps_correctly() {
    // Shrink to 6.5 MiB; allocated size must be ceil(6.5 MiB / 4 KiB) * 4 KiB = 6.5 MiB + a
    // bit of rounding up. Value length should be exactly 6.5 MiB.
    let img = working_copy("partial");
    let target = 6 * 1024 * 1024 + 512 * 1024;
    let n = write::truncate(Path::new(&img), "/big.bin", target).expect("truncate");
    assert_eq!(n, target);
    assert_eq!(value_length(&img, "/big.bin"), target);
}

#[test]
fn upstream_mounts_and_reads_after_shrink() {
    // Read the first MB of data and verify the marker at 0 is preserved.
    let img = working_copy("read_after");
    write::truncate(Path::new(&img), "/big.bin", 2 * 1024 * 1024).expect("truncate");

    use ntfs::NtfsReadSeek;
    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let root = ntfs.root_directory(&mut r).unwrap();
    let idx = root.directory_index(&mut r).unwrap();
    let mut finder = idx.finder();
    let e = ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &ntfs, &mut r, "big.bin")
        .unwrap()
        .unwrap();
    let file = e.to_file(&ntfs, &mut r).unwrap();
    let mut attrs = file.attributes();
    while let Some(item) = attrs.next(&mut r) {
        let item = item.unwrap();
        let a = item.to_attribute().unwrap();
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        assert_eq!(a.value_length(), 2 * 1024 * 1024);
        let mut v = a.value(&mut r).unwrap();
        v.seek(&mut r, std::io::SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 1];
        v.read(&mut r, &mut buf).unwrap();
        assert_eq!(buf[0], b'A'); // marker at 0 preserved
        break;
    }
}
