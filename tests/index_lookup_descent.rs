//! A name lookup follows the `$I30` B+tree instead of sweeping every live
//! `$INDEX_ALLOCATION` block.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::{idx_block, read};
use std::collections::HashSet;
use std::path::Path;

const IMG: &str = "test-disks/ntfs-manyfiles.img";

struct IndexReadCounter<T> {
    inner: T,
    index_offsets: HashSet<u64>,
    block_size: usize,
    index_reads: usize,
}

impl<T: BlockIo> BlockIo for IndexReadCounter<T> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        if buf.len() == self.block_size && self.index_offsets.contains(&offset) {
            self.index_reads += 1;
        }
        self.inner.read_exact_at(offset, buf)
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        self.inner.write_all_at(offset, buf)
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }
}

#[test]
fn lookup_in_a_large_directory_descends_to_one_leaf() {
    let image = Path::new(IMG);
    assert!(
        image.is_file(),
        "missing {IMG}; run test-disks/build-ntfs-feature-images.sh"
    );

    let mut setup = PathIo::open_ro(image).expect("open fixture");
    let bigdir = read::resolve_path(&mut setup, "/bigdir").expect("resolve /bigdir");
    let allocation =
        idx_block::load_for_directory_io(&mut setup, bigdir).expect("load bigdir $I30");
    let vcns = allocation.allocated_block_vcns();
    assert!(
        vcns.len() >= 8,
        "the regression needs a large index; fixture has only {} blocks",
        vcns.len()
    );

    let device_size = setup.size();
    let index_offsets = vcns
        .iter()
        .map(|&vcn| idx_block::vcn_to_disk_offset(&allocation, vcn, device_size))
        .collect::<Result<HashSet<_>, _>>()
        .expect("map bigdir INDX blocks");

    for (name, present) in [
        ("file_1.txt", true),
        ("file_256.txt", true),
        ("file_400.txt", true),
        ("file_512.txt", true),
        ("file_999.txt", false),
    ] {
        let mut probe = IndexReadCounter {
            inner: PathIo::open_ro(image).expect("reopen fixture"),
            index_offsets: index_offsets.clone(),
            block_size: allocation.block_size as usize,
            index_reads: 0,
        };
        let result = read::resolve_path(&mut probe, &format!("/bigdir/{name}"));
        assert_eq!(
            result.is_ok(),
            present,
            "unexpected lookup result for {name}"
        );
        assert!(
            probe.index_reads <= 2,
            "lookup for {name} needs at most its interior block and one leaf, not {} of {} allocated blocks",
            probe.index_reads,
            vcns.len()
        );
    }
}
