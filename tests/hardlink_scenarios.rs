//! Phase 3.6 — hard-link real-world scenarios.
//!
//! Complements the existing `write_link.rs` (which uses a prebuilt fixture
//! and covers create/reject paths). These tests format fresh in-memory
//! volumes and exercise the harder semantics: link-count decrement on
//! unlink, content sharing across links, cross-directory links, multi-link
//! trees, and last-name removal — each verified by reading back through the
//! upstream `ntfs` crate (independent of our own parsers).

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use ntfs::indexes::NtfsFileNameIndex;
use ntfs::{Ntfs, NtfsAttributeType, NtfsReadSeek};
use std::io::BufReader;
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_hl_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create temp image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("HL"),
        Some(0x4C_4E_4B_53),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Navigate to `path`; returns its NtfsFile-derived view via a closure so we
/// can read either hard_link_count or $DATA without re-walking.
fn with_file<R>(
    img: &str,
    path: &str,
    f: impl FnOnce(&ntfs::NtfsFile, &mut BufReader<std::fs::File>, &Ntfs) -> R,
) -> R {
    let file = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(file);
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
    f(&cur, &mut reader, &ntfs)
}

fn hard_link_count(img: &str, path: &str) -> u16 {
    with_file(img, path, |file, _r, _n| file.hard_link_count())
}

fn read_data(img: &str, path: &str) -> Vec<u8> {
    with_file(img, path, |file, reader, _n| {
        let mut attrs = file.attributes();
        while let Some(item) = attrs.next(reader) {
            let item = item.expect("item");
            let a = item.to_attribute().expect("attr");
            if a.ty().ok() != Some(NtfsAttributeType::Data) {
                continue;
            }
            if !a.name().map(|n| n.is_empty()).unwrap_or(true) {
                continue;
            }
            let mut v = a.value(reader).expect("value");
            let mut buf = vec![0u8; v.len() as usize];
            let mut off = 0usize;
            while off < buf.len() {
                let n = v.read(reader, &mut buf[off..]).expect("read");
                if n == 0 {
                    break;
                }
                off += n;
            }
            buf.truncate(off);
            return buf;
        }
        Vec::new()
    })
}

fn exists(img: &str, dir_path: &str, name: &str) -> bool {
    let file = std::fs::File::open(img).expect("open");
    let mut reader = BufReader::new(file);
    let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut reader).expect("upcase");
    let mut cur = ntfs.root_directory(&mut reader).expect("root");
    for comp in dir_path.trim_start_matches('/').split('/') {
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
    let idx = cur.directory_index(&mut reader).expect("idx");
    let mut finder = idx.finder();
    NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, name).is_some()
}

// ---------------------------------------------------------------------------

#[test]
fn link_then_unlink_one_keeps_other_name_and_data() {
    let img = fresh_vol("dec");
    write::create_file(Path::new(&img), "/", "a.txt").expect("create");
    write::write_file_contents(Path::new(&img), "/a.txt", b"shared payload").expect("write");
    write::link(Path::new(&img), "/a.txt", "/", "b.txt").expect("link");

    assert_eq!(
        hard_link_count(&img, "/a.txt"),
        2,
        "count must be 2 after link"
    );

    // Unlink the original name; the second name + data must survive.
    write::unlink(Path::new(&img), "/a.txt").expect("unlink a");
    assert!(!exists(&img, "/", "a.txt"), "a.txt must be gone");
    assert!(exists(&img, "/", "b.txt"), "b.txt must remain");
    assert_eq!(
        read_data(&img, "/b.txt"),
        b"shared payload",
        "data intact via b.txt"
    );
}

// unlink of one of several hard-linked names drops that $FILE_NAME and
// decrements the FILE record header's hard_link_count (NTFS requires the
// count to equal the number of $FILE_NAME attributes, else chkdsk flags
// it). Storage is freed only when the last link goes away.
#[test]
fn unlink_decrements_hard_link_count() {
    let img = fresh_vol("dec_count");
    write::create_file(Path::new(&img), "/", "a.txt").expect("create");
    write::link(Path::new(&img), "/a.txt", "/", "b.txt").expect("link");
    assert_eq!(hard_link_count(&img, "/a.txt"), 2);
    write::unlink(Path::new(&img), "/a.txt").expect("unlink");
    assert_eq!(
        hard_link_count(&img, "/b.txt"),
        1,
        "count must drop to 1 after unlink"
    );
}

#[test]
fn content_visible_through_all_links() {
    let img = fresh_vol("shared");
    write::create_file(Path::new(&img), "/", "orig.bin").expect("create");
    write::write_file_contents(Path::new(&img), "/orig.bin", b"v1").expect("write v1");
    write::link(Path::new(&img), "/orig.bin", "/", "alias.bin").expect("link");

    // Both names see the same content.
    assert_eq!(read_data(&img, "/orig.bin"), b"v1");
    assert_eq!(read_data(&img, "/alias.bin"), b"v1");

    // Rewrite through one name; the other reflects it (same inode).
    write::write_file_contents(Path::new(&img), "/orig.bin", b"v2-longer").expect("write v2");
    assert_eq!(read_data(&img, "/orig.bin"), b"v2-longer");
    assert_eq!(
        read_data(&img, "/alias.bin"),
        b"v2-longer",
        "alias sees rewrite (shared inode)"
    );
}

#[test]
fn cross_directory_hard_link() {
    let img = fresh_vol("crossdir");
    write::mkdir(Path::new(&img), "/", "sub").expect("mkdir");
    write::create_file(Path::new(&img), "/", "top.txt").expect("create");
    write::write_file_contents(Path::new(&img), "/top.txt", b"crossdir data").expect("write");

    // Link the root file into the subdirectory under a new name.
    write::link(Path::new(&img), "/top.txt", "/sub", "linked.txt").expect("cross-dir link");

    assert_eq!(hard_link_count(&img, "/top.txt"), 2, "count 2 across dirs");
    assert!(exists(&img, "/sub", "linked.txt"), "link present in subdir");
    assert_eq!(
        read_data(&img, "/sub/linked.txt"),
        b"crossdir data",
        "data via subdir link"
    );
}

#[test]
fn three_links_unlink_middle_leaves_others() {
    let img = fresh_vol("three");
    write::create_file(Path::new(&img), "/", "n1").expect("create");
    write::write_file_contents(Path::new(&img), "/n1", b"triple").expect("write");
    write::link(Path::new(&img), "/n1", "/", "n2").expect("link n2");
    write::link(Path::new(&img), "/n1", "/", "n3").expect("link n3");
    assert_eq!(hard_link_count(&img, "/n1"), 3, "three names => count 3");

    write::unlink(Path::new(&img), "/n2").expect("unlink n2");
    assert!(!exists(&img, "/", "n2"), "n2 gone");
    assert!(
        exists(&img, "/", "n1") && exists(&img, "/", "n3"),
        "n1 + n3 remain"
    );
    assert_eq!(
        hard_link_count(&img, "/n1"),
        2,
        "count drops 3 -> 2 after unlinking the middle name"
    );
    assert_eq!(read_data(&img, "/n3"), b"triple", "data intact");
}

#[test]
fn cross_directory_unlink_decrements_count() {
    let img = fresh_vol("crossdec");
    write::mkdir(Path::new(&img), "/", "sub").expect("mkdir");
    write::create_file(Path::new(&img), "/", "top.txt").expect("create");
    write::write_file_contents(Path::new(&img), "/top.txt", b"xdir").expect("write");
    write::link(Path::new(&img), "/top.txt", "/sub", "linked.txt").expect("link");
    assert_eq!(hard_link_count(&img, "/top.txt"), 2);

    // Unlink the link living in the subdirectory; the root name + data
    // survive and the count drops to 1.
    write::unlink(Path::new(&img), "/sub/linked.txt").expect("unlink subdir link");
    assert!(!exists(&img, "/sub", "linked.txt"), "subdir link gone");
    assert!(exists(&img, "/", "top.txt"), "root name remains");
    assert_eq!(
        hard_link_count(&img, "/top.txt"),
        1,
        "count drops to 1 after cross-dir unlink"
    );
    assert_eq!(read_data(&img, "/top.txt"), b"xdir", "data intact");
}

#[test]
fn delete_last_name_removes_file() {
    let img = fresh_vol("last");
    write::create_file(Path::new(&img), "/", "solo.txt").expect("create");
    write::write_file_contents(Path::new(&img), "/solo.txt", b"bye").expect("write");
    write::unlink(Path::new(&img), "/solo.txt").expect("unlink");
    assert!(
        !exists(&img, "/", "solo.txt"),
        "file must be gone after last unlink"
    );
}

#[test]
fn links_persist_across_remount() {
    let img = fresh_vol("remount");
    write::create_file(Path::new(&img), "/", "p.txt").expect("create");
    write::write_file_contents(Path::new(&img), "/p.txt", b"persist").expect("write");
    write::link(Path::new(&img), "/p.txt", "/", "q.txt").expect("link");
    // All handles dropped between each call (each opens its own PathIo),
    // so this already crosses unmount/remount boundaries.
    assert_eq!(hard_link_count(&img, "/p.txt"), 2);
    assert_eq!(read_data(&img, "/q.txt"), b"persist");
    assert_eq!(read_data(&img, "/p.txt"), b"persist");
}
