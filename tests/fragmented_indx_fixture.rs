//! A real 512-byte-cluster NTFS directory whose first 4 KiB INDX block
//! crosses a noncontiguous $INDEX_ALLOCATION run boundary.

mod common;

use fs_ntfs::facade::Filesystem;
use fs_ntfs::idx_block::load_for_directory;
use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

fn fixture_copy() -> String {
    let path = common::temp_image_path("fragmented_indx");
    let output = File::create(&path).expect("create image copy");
    let status = Command::new("gzip")
        .args(["-dc", "test-disks/ntfs-fragmented-indx.img.gz"])
        .stdout(Stdio::from(output))
        .status()
        .expect("run gzip");
    assert!(status.success(), "decompress fragmented INDX fixture");
    path
}

#[test]
fn fragmented_indx_block_is_read_and_can_be_updated() {
    let image = fixture_copy();
    let path = Path::new(&image);
    let allocation = load_for_directory(path, 64).expect("load /fragdir index allocation");
    assert_eq!(allocation.params.cluster_size, 512);
    assert_eq!(allocation.block_size, 4096);
    assert_eq!(allocation.runs.len(), 3);
    assert_eq!(allocation.runs[0].starting_vcn, 0);
    assert_eq!(allocation.runs[0].length, 4);
    assert_eq!(allocation.runs[1].starting_vcn, 4);
    assert_eq!(allocation.runs[1].length, 4);
    assert_ne!(
        allocation.runs[0].lcn.unwrap() + 4,
        allocation.runs[1].lcn.unwrap(),
        "the first INDX block must physically straddle nonadjacent runs"
    );

    let fs = Filesystem::mount(&image).expect("mount fixture");
    let names: BTreeSet<_> = fs
        .read_dir("/fragdir")
        .expect("enumerate fragmented directory")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    for number in 1..=80 {
        assert!(names.contains(&format!("file_{number:02}.txt")));
    }

    fs.create_file("/fragdir", "fixture_new.txt")
        .expect("insert into fragmented directory");
    let after: BTreeSet<_> = fs
        .read_dir("/fragdir")
        .expect("enumerate after insert")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(after.contains("fixture_new.txt"));
    for number in 1..=80 {
        assert!(after.contains(&format!("file_{number:02}.txt")));
    }
}
