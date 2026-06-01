//! Integration tests for `write::write_sparse_file` (the new sparse-write API).
//!
//! Proves two things per case: (1) content round-trips — holes read back as
//! zeros via the upstream `ntfs` crate (independent of our parsers); and
//! (2) sparseness is real — a hole consumes NO clusters, measured directly
//! against `$Bitmap` free-count before/after.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{bitmap, write};
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_sparsew_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create temp image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("SPW"), Some(0x5A_AB_5A_AB))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Read the unnamed `$DATA` of `path` via upstream (holes → zeros).
fn read_back(img: &str, path: &str) -> Vec<u8> {
    let f = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let mut cur = ntfs.root_directory(&mut reader).expect("root");
    for comp in path.trim_start_matches('/').split('/') {
        if comp.is_empty() {
            continue;
        }
        let idx = cur.directory_index(&mut reader).expect("idx");
        let mut finder = idx.finder();
        let e = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
            .expect("find result")
            .expect("find ok");
        cur = e.to_file(&ntfs, &mut reader).expect("to_file");
    }
    let mut attrs = cur.attributes();
    while let Some(item) = attrs.next(&mut reader) {
        let item = item.expect("item");
        let a = item.to_attribute().expect("attr");
        if a.ty().ok() != Some(NtfsAttributeType::Data) {
            continue;
        }
        if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
            continue;
        }
        let mut v = a.value(&mut reader).expect("value");
        let mut buf = vec![0u8; v.len() as usize];
        let mut off = 0usize;
        while off < buf.len() {
            let n = v.read(&mut reader, &mut buf[off..]).expect("read");
            if n == 0 {
                break;
            }
            off += n;
        }
        buf.truncate(off);
        return buf;
    }
    panic!("no unnamed $DATA at {path}");
}

/// Clusters currently allocated on the volume (total - free).
fn allocated_clusters(img: &str) -> u64 {
    let bm = bitmap::locate_bitmap(Path::new(img)).expect("locate $Bitmap");
    let total = bm.total_bits;
    let free = bitmap::count_free(Path::new(img), &bm).expect("count_free");
    total - free
}

// ---------------------------------------------------------------------------

#[test]
fn sparse_hole_in_middle_roundtrips_and_saves_a_cluster() {
    let img = fresh_vol("middle_hole");
    write::create_file(Path::new(&img), "/", "sp.bin").expect("create");

    // 3 clusters: data, hole, data.
    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 3 * cs];
    data[0..cs].fill(0xAA); // cluster 0
                            // cluster 1 left zero (hole)
    data[2 * cs..3 * cs].fill(0xBB); // cluster 2

    let before = allocated_clusters(&img);
    write::write_sparse_file(Path::new(&img), "/sp.bin", &data).expect("write_sparse");
    let after = allocated_clusters(&img);

    // Content round-trips (the hole reads back as zeros).
    assert_eq!(read_back(&img, "/sp.bin"), data, "sparse content must round-trip");
    // Only 2 of the 3 clusters were allocated — the hole cost nothing.
    assert_eq!(
        after - before,
        2,
        "expected 2 clusters allocated (hole saved 1); before={before} after={after}"
    );
}

#[test]
fn fully_sparse_file_allocates_nothing_and_reads_zeros() {
    let img = fresh_vol("fully");
    write::create_file(Path::new(&img), "/", "z.bin").expect("create");

    let cs = CLUSTER as usize;
    let data = vec![0u8; 8 * cs]; // 8 all-zero clusters

    let before = allocated_clusters(&img);
    write::write_sparse_file(Path::new(&img), "/z.bin", &data).expect("write_sparse");
    let after = allocated_clusters(&img);

    assert_eq!(read_back(&img, "/z.bin"), data, "fully-sparse reads all zeros");
    assert_eq!(after, before, "a fully-sparse file must allocate ZERO clusters");
}

#[test]
fn sparse_file_attribute_bit_is_set() {
    let img = fresh_vol("flag");
    write::create_file(Path::new(&img), "/", "f.bin").expect("create");
    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 2 * cs];
    data[0] = 1; // cluster 0 data, cluster 1 hole
    write::write_sparse_file(Path::new(&img), "/f.bin", &data).expect("write_sparse");

    // FILE_ATTRIBUTE_SPARSE_FILE (0x200) must be set in $STANDARD_INFORMATION.
    let si = write::read_si_full(Path::new(&img), "/f.bin").expect("read SI");
    // read_si_full exposes the raw file_attributes via the v1 common fields;
    // assert the sparse bit through the DOS-attributes surface.
    // (read_si_full returns the attributes in its struct.)
    assert_ne!(
        si.file_attributes & 0x0000_0200,
        0,
        "FILE_ATTRIBUTE_SPARSE_FILE must be set; attrs={:#x}",
        si.file_attributes
    );
}

#[test]
fn dense_data_with_no_holes_allocates_every_cluster() {
    // Control: a file with no zero clusters must allocate them all (no false
    // holes from the sparse planner).
    let img = fresh_vol("dense");
    write::create_file(Path::new(&img), "/", "d.bin").expect("create");
    let cs = CLUSTER as usize;
    let data = vec![0x7Fu8; 4 * cs]; // 4 fully non-zero clusters

    let before = allocated_clusters(&img);
    write::write_sparse_file(Path::new(&img), "/d.bin", &data).expect("write_sparse");
    let after = allocated_clusters(&img);

    assert_eq!(read_back(&img, "/d.bin"), data);
    assert_eq!(after - before, 4, "no holes → all 4 clusters allocated");
}
