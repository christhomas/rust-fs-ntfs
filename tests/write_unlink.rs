//! Tests for `write::unlink`.

use fs_ntfs::{bitmap, mft_bitmap, write};
use ntfs::Ntfs;
use std::io::BufReader;
use std::path::Path;

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";
const LARGE_IMG: &str = "test-disks/ntfs-large-file.img";

fn working_copy(base: &str, tag: &str) -> String {
    let dst = format!("test-disks/_unlink_{tag}.img");
    std::fs::copy(base, &dst).expect("copy");
    dst
}

fn list_root(img: &str) -> Vec<String> {
    use ntfs::structured_values::NtfsFileNamespace;
    let f = std::fs::File::open(img).unwrap();
    let mut r = BufReader::new(f);
    let mut ntfs = Ntfs::new(&mut r).unwrap();
    ntfs.read_upcase_table(&mut r).unwrap();
    let root = ntfs.root_directory(&mut r).unwrap();
    let idx = root.directory_index(&mut r).unwrap();
    let mut it = idx.entries();
    let mut names = Vec::new();
    while let Some(entry) = it.next(&mut r) {
        let entry = entry.unwrap();
        if let Some(Ok(name)) = entry.key() {
            if name.namespace() == NtfsFileNamespace::Dos {
                continue;
            }
            names.push(name.name().to_string_lossy());
        }
    }
    names
}

fn list_dir(img: &str, path: &str) -> Vec<String> {
    use ntfs::structured_values::NtfsFileNamespace;
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
    let idx = cur.directory_index(&mut r).unwrap();
    let mut it = idx.entries();
    let mut names = Vec::new();
    while let Some(entry) = it.next(&mut r) {
        let entry = entry.unwrap();
        if let Some(Ok(name)) = entry.key() {
            if name.namespace() == NtfsFileNamespace::Dos {
                continue;
            }
            names.push(name.name().to_string_lossy());
        }
    }
    names
}

fn record_number_of(img: &str, path: &str) -> u64 {
    fs_ntfs::write::resolve_path_to_record_number(Path::new(img), path).unwrap()
}

#[test]
fn unlink_removes_from_subdir_index() {
    let img = working_copy(BASIC_IMG, "subdir_index");
    assert!(list_dir(&img, "/Documents")
        .iter()
        .any(|n| n == "readme.txt"));

    write::unlink(Path::new(&img), "/Documents/readme.txt").expect("unlink");

    let names = list_dir(&img, "/Documents");
    assert!(
        !names.iter().any(|n| n == "readme.txt"),
        "still present: {names:?}"
    );
    // notes.txt must still be there.
    assert!(names.iter().any(|n| n == "notes.txt"));
}

#[test]
fn unlink_removes_from_root_index_allocation() {
    let img = working_copy(BASIC_IMG, "root_ia");
    write::unlink(Path::new(&img), "/hello.txt").expect("unlink");

    let names = list_root(&img);
    assert!(
        !names.iter().any(|n| n == "hello.txt"),
        "still present: {names:?}"
    );
    // Documents dir must still be there.
    assert!(names.iter().any(|n| n == "Documents"));
}

#[test]
fn unlink_frees_mft_record_bit() {
    let img = working_copy(BASIC_IMG, "mft_free");
    let rec = record_number_of(&img, "/Documents/readme.txt");

    let mbm_before = mft_bitmap::locate(Path::new(&img)).unwrap();
    assert!(mft_bitmap::is_allocated(Path::new(&img), &mbm_before, rec).unwrap());

    write::unlink(Path::new(&img), "/Documents/readme.txt").expect("unlink");

    let mbm_after = mft_bitmap::locate(Path::new(&img)).unwrap();
    assert!(
        !mft_bitmap::is_allocated(Path::new(&img), &mbm_after, rec).unwrap(),
        "MFT record {rec} should be free after unlink"
    );
}

#[test]
fn unlink_frees_non_resident_data_clusters() {
    let img = working_copy(LARGE_IMG, "large_clusters");

    // Pick some LCNs from /big.bin's run list to sample after unlink.
    let sample_lcns: Vec<u64> = {
        use fs_ntfs::{attr_io, data_runs, mft_io};
        let rn = record_number_of(&img, "/big.bin");
        let (_p, rec) = mft_io::read_mft_record(Path::new(&img), rn).unwrap();
        let loc = attr_io::find_attribute(&rec, attr_io::AttrType::Data, None).unwrap();
        let mo = loc.non_resident_mapping_pairs_offset.unwrap() as usize;
        let runs =
            data_runs::decode_runs(&rec[loc.attr_offset + mo..loc.attr_offset + loc.attr_length])
                .unwrap();
        let mut out = Vec::new();
        for r in &runs {
            if let Some(lcn) = r.lcn {
                for i in 0..r.length.min(4) {
                    out.push(lcn + i);
                }
            }
        }
        out
    };
    assert!(
        !sample_lcns.is_empty(),
        "no LCNs sampled — /big.bin should have data runs"
    );

    let bm_before = bitmap::locate_bitmap(Path::new(&img)).unwrap();
    for lcn in &sample_lcns {
        assert!(bitmap::is_allocated(Path::new(&img), &bm_before, *lcn).unwrap());
    }

    write::unlink(Path::new(&img), "/big.bin").expect("unlink");

    let bm_after = bitmap::locate_bitmap(Path::new(&img)).unwrap();
    for lcn in &sample_lcns {
        assert!(
            !bitmap::is_allocated(Path::new(&img), &bm_after, *lcn).unwrap(),
            "cluster {lcn} should be free after unlink"
        );
    }
}

#[test]
fn unlink_refuses_directory() {
    let img = working_copy(BASIC_IMG, "refuse_dir");
    let err = write::unlink(Path::new(&img), "/Documents").unwrap_err();
    assert!(err.contains("director"), "{err:?}");
}

#[test]
fn unlink_rejects_missing_source() {
    let img = working_copy(BASIC_IMG, "missing");
    let err = write::unlink(Path::new(&img), "/nonexistent.xx").unwrap_err();
    assert!(
        err.contains("not found") || err.contains("nonexistent"),
        "{err:?}"
    );
}

#[test]
fn upstream_mounts_after_unlink() {
    let img = working_copy(BASIC_IMG, "mount_after");
    write::unlink(Path::new(&img), "/hello.txt").expect("unlink");

    let f = std::fs::File::open(&img).unwrap();
    let mut r = BufReader::new(f);
    let ntfs = Ntfs::new(&mut r).expect("parse");
    let vi = ntfs.volume_info(&mut r).expect("volume_info");
    assert!(vi.major_version() >= 3);
}
