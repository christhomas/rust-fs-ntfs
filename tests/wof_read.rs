//! A WOF-compressed file must not read back as zeros.
//!
//! Windows Overlay Filter compression — `compact /exe`, and "Compact
//! OS" across a whole system partition — leaves the file's unnamed
//! `$DATA` empty and sparse, puts the real bytes in a
//! `WofCompressedData` stream, and marks the file with an
//! `IO_REPARSE_TAG_WOF` `$REPARSE_POINT`. A plain `$DATA` read of such
//! a file succeeds and returns the right *number* of bytes, all zero.
//!
//! The C ABI's `fs_ntfs_read` detected the tag and failed loudly, with
//! the reasoning written out beside it. `facade::read_file` — the
//! crate's Rust-facing API — had no such check, so the two front doors
//! of the same crate disagreed about whether the file was readable, and
//! the one that said yes was wrong.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::{FileType, Filesystem};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::record_build::reparse_tag;
use fs_ntfs::{read, write};
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;
const SIZE: u64 = 8192;

fn fresh_volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_wof_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("WOFC"), Some(9))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// The shape WOF leaves behind: a file whose unnamed `$DATA` is the
/// right logical size and holds nothing, whose real bytes are in a
/// `WofCompressedData` stream, and which carries the WOF reparse tag.
fn wof_file(tag: &str) -> String {
    let img = fresh_volume(tag);
    let p = Path::new(&img);
    write::create_file(p, "/", "wof.bin").expect("create");
    // Promote $DATA to non-resident, then take the content back out so
    // the stream is the right length and entirely uninitialised --
    // which is what reads as zeros.
    write::write_file_contents(p, "/wof.bin", &vec![b'x'; CLUSTER as usize]).expect("promote");
    write::truncate(p, "/wof.bin", 0).expect("truncate to 0");
    write::grow_nonresident(p, "/wof.bin", SIZE).expect("grow");
    // The real bytes, where WOF keeps them.
    write::write_named_stream(p, "/wof.bin", "WofCompressedData", &vec![0xC0u8; 256])
        .expect("write WofCompressedData");
    write::write_reparse_point(p, "/wof.bin", reparse_tag::WOF, &[0x01, 0x00, 0x00, 0x00])
        .expect("set the WOF reparse tag");
    img
}

#[test]
fn the_rust_api_refuses_a_wof_file_rather_than_returning_zeros() {
    let img = wof_file("facade");
    let fs = Filesystem::mount(&img).expect("mount");
    let mut buf = vec![0u8; SIZE as usize];
    match fs.read_file("/wof.bin", 0, &mut buf) {
        Err(_) => {}
        Ok(n) => panic!(
            "read_file returned Ok({n}) for a WOF-compressed file; {} of those \
             bytes are zero and none of them are the file's contents",
            buf[..n].iter().filter(|&&b| b == 0).count()
        ),
    }
}

#[test]
fn the_whole_value_read_refuses_it_too() {
    let img = wof_file("value");
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let rec = read::resolve_path(&mut io, "/wof.bin").expect("resolve");
    let got = read::read_attribute_value(&mut io, rec, fs_ntfs::attr_io::AttrType::Data, None);
    assert!(
        got.is_err(),
        "read_attribute_value returned {:?} bytes for a WOF-compressed file",
        got.map(|v| v.len())
    );
}

#[test]
fn stat_and_readdir_still_work_on_a_wof_file() {
    // Refusing the CONTENT must not make the file invisible: a
    // directory listing and a stat of a Windows system volume would
    // stop working, and the file is a regular file whose bytes this
    // crate cannot decode yet -- not a broken one.
    let img = wof_file("stat");
    let fs = Filesystem::mount(&img).expect("mount");
    let names: Vec<String> = fs
        .read_dir("/")
        .expect("read_dir")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.iter().any(|n| n == "wof.bin"), "{names:?}");
    let st = fs.stat("/wof.bin").expect("stat a WOF file");
    assert_eq!(st.size, SIZE, "the logical size is still readable");
    assert_eq!(
        st.file_type,
        FileType::Regular,
        "a WOF file is a regular file whose bytes this crate cannot decode yet"
    );
}

#[test]
fn an_ordinary_file_still_reads() {
    let img = fresh_volume("plain");
    let p = Path::new(&img);
    write::create_file(p, "/", "plain.bin").expect("create");
    write::write_file_contents(p, "/plain.bin", &vec![b'z'; 4096]).expect("write");
    let fs = Filesystem::mount(&img).expect("mount");
    let mut buf = vec![0u8; 4096];
    let n = fs.read_file("/plain.bin", 0, &mut buf).expect("read");
    assert_eq!(n, 4096);
    assert!(buf.iter().all(|&b| b == b'z'));
}

#[test]
fn a_symlink_still_reads_its_target() {
    // A different reparse tag must not be caught by the WOF refusal.
    let img = fresh_volume("symlink");
    let p = Path::new(&img);
    write::create_symlink(p, "/", "link", "/target.txt", false).expect("create_symlink");
    let fs = Filesystem::mount(&img).expect("mount");
    let st = fs.stat("/link").expect("stat the symlink");
    assert_eq!(st.file_type, FileType::Symlink);
}
