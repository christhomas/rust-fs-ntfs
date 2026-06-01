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
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("SPW"),
        Some(0x5A_AB_5A_AB),
    )
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
    assert_eq!(
        read_back(&img, "/sp.bin"),
        data,
        "sparse content must round-trip"
    );
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

    assert_eq!(
        read_back(&img, "/z.bin"),
        data,
        "fully-sparse reads all zeros"
    );
    assert_eq!(
        after, before,
        "a fully-sparse file must allocate ZERO clusters"
    );
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

// ---------------------------------------------------------------------------
// More hole-placement scenarios.
// ---------------------------------------------------------------------------

#[test]
fn hole_at_start_roundtrips_and_saves_clusters() {
    let img = fresh_vol("hole_start");
    write::create_file(Path::new(&img), "/", "hs.bin").expect("create");
    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 4 * cs];
    data[3 * cs..4 * cs].fill(0xCC); // only the last cluster has data
    let before = allocated_clusters(&img);
    write::write_sparse_file(Path::new(&img), "/hs.bin", &data).expect("write_sparse");
    let after = allocated_clusters(&img);
    assert_eq!(
        read_back(&img, "/hs.bin"),
        data,
        "leading holes read as zeros"
    );
    assert_eq!(after - before, 1, "3 leading holes cost nothing");
}

#[test]
fn hole_at_end_roundtrips_and_saves_clusters() {
    let img = fresh_vol("hole_end");
    write::create_file(Path::new(&img), "/", "he.bin").expect("create");
    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 5 * cs];
    data[0..cs].fill(0xDD); // first cluster data; trailing 4 are holes
    let before = allocated_clusters(&img);
    write::write_sparse_file(Path::new(&img), "/he.bin", &data).expect("write_sparse");
    let after = allocated_clusters(&img);
    assert_eq!(
        read_back(&img, "/he.bin"),
        data,
        "trailing holes read as zeros"
    );
    assert_eq!(after - before, 1, "4 trailing holes cost nothing");
}

#[test]
fn multiple_holes_interleaved() {
    let img = fresh_vol("multi_hole");
    write::create_file(Path::new(&img), "/", "mh.bin").expect("create");
    let cs = CLUSTER as usize;
    // data, hole, data, hole, data  (clusters 0,2,4 have data)
    let mut data = vec![0u8; 5 * cs];
    for c in [0usize, 2, 4] {
        data[c * cs..(c + 1) * cs].fill(0xEE);
    }
    let before = allocated_clusters(&img);
    write::write_sparse_file(Path::new(&img), "/mh.bin", &data).expect("write_sparse");
    let after = allocated_clusters(&img);
    assert_eq!(
        read_back(&img, "/mh.bin"),
        data,
        "interleaved holes round-trip"
    );
    assert_eq!(after - before, 3, "only the 3 data clusters allocated");
}

#[test]
fn large_scattered_sparse_file() {
    let img = fresh_vol("scattered");
    write::create_file(Path::new(&img), "/", "big.bin").expect("create");
    let cs = CLUSTER as usize;
    // 64 clusters, data in only 5 of them.
    let mut data = vec![0u8; 64 * cs];
    let data_clusters = [1usize, 8, 17, 40, 63];
    for &c in &data_clusters {
        data[c * cs..(c + 1) * cs].fill((c as u8) | 1);
    }
    let before = allocated_clusters(&img);
    write::write_sparse_file(Path::new(&img), "/big.bin", &data).expect("write_sparse");
    let after = allocated_clusters(&img);
    assert_eq!(
        read_back(&img, "/big.bin"),
        data,
        "64-cluster scattered sparse round-trips"
    );
    assert_eq!(
        after - before,
        data_clusters.len() as u64,
        "only the {} data clusters allocated of 64",
        data_clusters.len()
    );
}

#[test]
fn sparse_write_does_not_disturb_a_sibling_file() {
    let img = fresh_vol("isolation");
    write::create_file(Path::new(&img), "/", "keep.txt").expect("create keep");
    write::write_file_contents(Path::new(&img), "/keep.txt", b"untouched").expect("write keep");
    write::create_file(Path::new(&img), "/", "sp.bin").expect("create sparse");
    let cs = CLUSTER as usize;
    let mut data = vec![0u8; 3 * cs];
    data[0] = 1;
    write::write_sparse_file(Path::new(&img), "/sp.bin", &data).expect("write_sparse");
    // Sibling must be intact.
    assert_eq!(
        read_back(&img, "/keep.txt"),
        b"untouched",
        "sibling file untouched"
    );
}

// ---------------------------------------------------------------------------
// Error paths.
// ---------------------------------------------------------------------------

#[test]
fn write_sparse_to_missing_file_errors() {
    let img = fresh_vol("missing");
    assert!(
        write::write_sparse_file(Path::new(&img), "/ghost.bin", &[0u8; 4096]).is_err(),
        "sparse write to a non-existent file must fail"
    );
}

#[test]
fn write_sparse_rejects_already_nonresident_data() {
    // MVP precondition: current $DATA must be resident. Promote first, then a
    // sparse write must be refused (not silently corrupt).
    let img = fresh_vol("already_nonres");
    write::create_file(Path::new(&img), "/", "nr.bin").expect("create");
    let cs = CLUSTER as usize;
    write::promote_resident_data_to_nonresident(Path::new(&img), "/nr.bin", &vec![1u8; cs])
        .expect("promote");
    let err = write::write_sparse_file(Path::new(&img), "/nr.bin", &vec![0u8; 2 * cs]);
    assert!(
        err.is_err(),
        "sparse write on non-resident $DATA must be refused: {err:?}"
    );
}
