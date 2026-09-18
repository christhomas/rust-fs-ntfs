//! Tests for `write::grow_nonresident` (W2.5 grow).

mod common;

use fs_ntfs::{bitmap, write};
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const LARGE_IMG: &str = "test-disks/ntfs-large-file.img";

fn working_copy(tag: &str) -> String {
    let dst = common::temp_image_path(format!("grow_{tag}"));
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

// ---------------------------------------------------------------------------
// A failure after the allocation gives the clusters back (#146, #251), and
// an allocation the volume cannot address is refused before it is committed
// (#223).
// ---------------------------------------------------------------------------

use fs_ntfs::block_io::{BlockIo, PathIo};

/// Count free clusters through the driver's own counter, which is what a
/// consumer sees as `volume_stats().free_clusters`.
fn free_clusters(img: &str) -> u64 {
    let bm = bitmap::locate_bitmap(Path::new(img)).expect("locate_bitmap");
    bitmap::count_free(Path::new(img), &bm).expect("count_free")
}

/// Run `grow` against a `FailingIo` and report the error plus the
/// free-cluster count afterwards. `transient` chooses whether the device
/// takes writes again after the injected one.
fn grow_with_failure_at(
    img: &str,
    target: u64,
    n: usize,
    transient: bool,
) -> (Result<u64, String>, u64) {
    let rec = {
        let mut probe = PathIo::open_ro(Path::new(img)).expect("open_ro");
        fs_ntfs::read::resolve_path(&mut probe, "/big.bin").expect("resolve /big.bin")
    };
    let inner = PathIo::open_rw(Path::new(img)).unwrap();
    let mut io = if transient {
        common::FailingIo::failing_only(inner, n)
    } else {
        common::FailingIo::failing_from(inner, n)
    };
    let got = write::grow_nonresident_by_record_number_io(&mut io, rec, target);
    let _ = BlockIo::sync(&mut io);
    drop(io);
    (got, free_clusters(img))
}

/// How many writes a successful grow to `target` performs, measured on a
/// throwaway copy rather than assumed.
fn writes_in_a_successful_grow(target: u64) -> usize {
    let img = working_copy("rollback_probe");
    write::truncate(Path::new(&img), "/big.bin", 256 * 1024).unwrap();
    let rec = {
        let mut probe = PathIo::open_ro(Path::new(&img)).unwrap();
        fs_ntfs::read::resolve_path(&mut probe, "/big.bin").unwrap()
    };
    let mut io = common::FailingIo::recording(PathIo::open_rw(Path::new(&img)).unwrap());
    write::grow_nonresident_by_record_number_io(&mut io, rec, target).expect("the probe grow");
    io.writes_seen()
}

/// THE ALLOCATION IS GIVEN BACK WHEN THE COMMIT FAILS.
///
/// `grow` takes the clusters out of `$Bitmap` before it rewrites the MFT
/// record, and the record write was not a path anyone had treated as an
/// error path: it returned through `?`. Every error inside that closure
/// and every error `update_mft_record_io` raises itself — a `$DATA` that
/// moved under the re-read, a record whose IN_USE bit is clear, the
/// fixup, the write-back, the fsync — left `need_clusters` marked in use
/// with no record naming them. Permanently: nothing allocates them again
/// and no `unlink` frees them, so only `chkdsk /f` reclaims them, and a
/// driver retrying against a flaky device burns the free space every
/// time.
///
/// The fault is transient — one bad write, the device still there —
/// because that is the failure the rollback was written for and the only
/// one it can actually complete against. The sweep covers every write a
/// successful grow performs, so it does not depend on which of them is
/// the commit. See #146.
#[test]
fn a_grow_whose_commit_fails_leaves_no_clusters_allocated() {
    let target = 256 * 1024 + 8 * 4096;
    let total_writes = writes_in_a_successful_grow(target);
    assert!(
        total_writes >= 2,
        "a grow that allocates must write at least the bitmap and the record; \
         saw {total_writes}"
    );

    for n in 1..=total_writes {
        let img = working_copy(&format!("rollback_w{n}"));
        write::truncate(Path::new(&img), "/big.bin", 256 * 1024).unwrap();
        let before = free_clusters(&img);

        let (got, after) = grow_with_failure_at(&img, target, n, true);
        let err = got.expect_err(&format!(
            "write #{n} of {total_writes} was injected with an I/O failure, so the \
             grow cannot have succeeded"
        ));
        assert!(
            !err.contains("rollback incomplete"),
            "write #{n}: the fault was transient, so the rollback had a working \
             device to give the clusters back to: {err}"
        );
        assert_eq!(
            after,
            before,
            "write #{n} of {total_writes} failed and left {} clusters allocated that \
             no record names ({err})",
            before.saturating_sub(after)
        );
    }
}

/// A rollback that cannot run says so, instead of reporting only the
/// original error.
///
/// Every rollback site in this file's subject used to be spelled
/// `let _ = free_io(...)`, which is silent about exactly this: the
/// clusters stay marked in use and the caller hears only that the commit
/// failed. Against a device that has stopped taking writes the clusters
/// genuinely do leak — the honest outcome is an error that names them,
/// which is what a later `chkdsk` is being predicted by. See #146.
#[test]
fn a_rollback_that_fails_is_reported_rather_than_swallowed() {
    let target = 256 * 1024 + 8 * 4096;
    let total_writes = writes_in_a_successful_grow(target);
    // The last write of a successful grow is the record commit; failing
    // from there leaves the allocation made and the device gone.
    let img = working_copy("rollback_permanent");
    write::truncate(Path::new(&img), "/big.bin", 256 * 1024).unwrap();

    let (got, _) = grow_with_failure_at(&img, target, total_writes, false);
    let err = got.expect_err("the commit write was injected with a failure");
    assert!(
        err.contains("rollback incomplete") && err.contains("clusters leaked in $Bitmap"),
        "a rollback that could not run must name the clusters it could not give \
         back, got: {err}"
    );
}

/// The control: the same fixture, the same entry point, no injected
/// failure. Without it the assertion above is satisfied by a grow that
/// never allocates anything at all.
#[test]
fn a_grow_that_succeeds_still_consumes_its_clusters() {
    let img = working_copy("rollback_control");
    write::truncate(Path::new(&img), "/big.bin", 256 * 1024).unwrap();
    let before = free_clusters(&img);
    let target = 256 * 1024 + 8 * 4096;

    let rec = {
        let mut probe = PathIo::open_ro(Path::new(&img)).unwrap();
        fs_ntfs::read::resolve_path(&mut probe, "/big.bin").unwrap()
    };
    let mut io = common::FailingIo::recording(PathIo::open_rw(Path::new(&img)).unwrap());
    let n = write::grow_nonresident_by_record_number_io(&mut io, rec, target).expect("grow");
    drop(io);
    assert_eq!(n, target);
    let after = free_clusters(&img);
    assert!(
        before - after >= 8,
        "a successful grow of 8 clusters must consume them; before={before} after={after}"
    );
}
