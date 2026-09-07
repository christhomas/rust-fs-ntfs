//! Bytes written past `initialized_length` have to become readable.
//!
//! NTFS keeps three lengths on a non-resident attribute: how much space
//! it owns, how big the file is, and how much of it has ever been
//! written. Everything from `initialized_length` to `data_length` reads
//! as zeros no matter what is on the clusters — this driver honours that
//! (`read.rs` clamps every read to `min(data_size, init_size)`), and so
//! does Windows.
//!
//! Growing a file deliberately leaves `initialized_length` alone, so the
//! new tail reads back as zeros. That is right. What was missing is the
//! other half: the write that ends the uninitialised region has to move
//! the field, or the data it just wrote is unreachable through any
//! reader that obeys the field — which is all of them.
//!
//! The gap case matters as much. Raising `initialized_length` over a
//! region nothing ever wrote publishes whatever the previously-freed
//! clusters held as file content, which is the same bug pointing the
//! other way; these tests write a recognisable pattern into the free
//! space first so that failure would be visible rather than a lucky
//! screen of zeros.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{attr_io, mft_io, read, write};
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;
const FIRST: u8 = b'A';
const SECOND: u8 = b'B';
/// Painted over the volume's free space before the file is grown into
/// it, so a stale-cluster leak reads as this rather than as zeros.
const STALE: u8 = 0xDE;

fn fresh_volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_initlen_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("INIT"), Some(3))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Paint the back half of the volume, which is free space a grow will
/// hand out, with a byte that is not zero and not any file's content.
fn paint_free_space(img: &str) {
    let mut io = PathIo::open_rw(Path::new(img)).expect("open_rw");
    let from = VOL_SIZE / 2;
    let paint = vec![STALE; (VOL_SIZE - from) as usize];
    io.write_all_at(from, &paint).expect("paint free space");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
}

fn initialized_length(img: &str, path: &str) -> u64 {
    let mut io = PathIo::open_ro(Path::new(img)).expect("open_ro");
    let rec = read::resolve_path(&mut io, path).expect("resolve");
    let (_, record) = mft_io::read_mft_record_io(&mut io, rec).expect("read record");
    let loc = attr_io::find_attribute(&record, attr_io::AttrType::Data, None).expect("$DATA");
    assert!(
        !loc.is_resident,
        "{path} must be non-resident for this test"
    );
    u64::from_le_bytes(
        record[loc.attr_offset + attr_io::attr_off::NONRES_INITIALIZED_LENGTH
            ..loc.attr_offset + attr_io::attr_off::NONRES_INITIALIZED_LENGTH + 8]
            .try_into()
            .unwrap(),
    )
}

fn read_window(img: &str, path: &str, offset: u64, len: usize) -> Vec<u8> {
    let fs = Filesystem::mount(img).expect("mount");
    let mut buf = vec![0u8; len];
    let n = fs.read_file(path, offset, &mut buf).expect("read_file");
    buf.truncate(n);
    buf
}

/// A 4 KiB file grown to 8 KiB, so that [4096, 8192) is allocated but
/// uninitialised.
fn grown_file(tag: &str) -> String {
    let img = fresh_volume(tag);
    paint_free_space(&img);
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.create_file("/", "grown.bin").expect("create");
    fs.write_file_contents("/grown.bin", &vec![FIRST; 4096])
        .expect("write initial contents");
    fs.grow("/grown.bin", 8192).expect("grow");
    drop(fs);
    assert_eq!(
        initialized_length(&img, "/grown.bin"),
        4096,
        "grow must leave initialized_length where it was -- that half is correct \
         and this test depends on it"
    );
    img
}

#[test]
fn the_grown_tail_reads_as_zeros_until_something_writes_it() {
    // The half that already worked, restated here so a change that
    // raises initialized_length too eagerly fails loudly: the tail is
    // allocated out of free space that was painted 0xDE, and it must
    // still read as zeros.
    let img = grown_file("zeros");
    let tail = read_window(&img, "/grown.bin", 4096, 4096);
    assert_eq!(tail.len(), 4096);
    assert!(
        tail.iter().all(|&b| b == 0),
        "the uninitialised tail must read as zeros, got {:02x?}...",
        &tail[..8]
    );
}

#[test]
fn a_write_into_the_grown_tail_reads_back() {
    let img = grown_file("write_tail");
    let n = write::write_at(Path::new(&img), "/grown.bin", 4096, &[SECOND; 4096])
        .expect("write into the grown tail");
    assert_eq!(n, 4096, "the write reported {n} bytes");

    let tail = read_window(&img, "/grown.bin", 4096, 4096);
    let wrong = tail.iter().filter(|&&b| b != SECOND).count();
    assert_eq!(
        wrong,
        0,
        "{wrong} of {} bytes came back as something other than {SECOND:#04x}; \
         initialized_length is {} and the data is on the clusters but unreachable",
        tail.len(),
        initialized_length(&img, "/grown.bin"),
    );
    assert_eq!(
        initialized_length(&img, "/grown.bin"),
        8192,
        "the write ended at 8192, so initialized_length has to say so"
    );

    // The first half is untouched.
    let head = read_window(&img, "/grown.bin", 0, 4096);
    assert!(
        head.iter().all(|&b| b == FIRST),
        "the head must be unchanged"
    );
}

#[test]
fn a_write_that_leaves_a_gap_does_not_publish_the_old_cluster_contents() {
    // grown.bin is 8192 bytes with initialized_length 4096. Write only
    // the last 512 bytes: [4096, 7680) was never written by anyone and
    // sits on clusters that were painted 0xDE before the grow.
    let img = grown_file("gap");
    let before = read_window(&img, "/grown.bin", 4096, 4096);
    assert!(before.iter().all(|&b| b == 0));

    let wrote = write::write_at(Path::new(&img), "/grown.bin", 7680, &[SECOND; 512]);
    match wrote {
        Err(_) => {
            // Refusing the write is a legitimate answer to the gap, as
            // long as it does not raise initialized_length on the way
            // out.
            assert_eq!(
                initialized_length(&img, "/grown.bin"),
                4096,
                "a refused write must not have moved initialized_length"
            );
        }
        Ok(n) => {
            assert_eq!(n, 512);
            let all = read_window(&img, "/grown.bin", 4096, 4096);
            assert_eq!(all.len(), 4096);
            let stale = all.iter().filter(|&&b| b == STALE).count();
            assert_eq!(
                stale, 0,
                "{stale} bytes of the gap read back as {STALE:#04x} -- raising \
                 initialized_length over a region nothing wrote published the \
                 previous contents of those clusters as file data"
            );
            assert!(
                all[..3584].iter().all(|&b| b == 0),
                "the gap must read as zeros, got {:02x?}...",
                &all[..8]
            );
            assert!(
                all[3584..].iter().all(|&b| b == SECOND),
                "the written window must read back"
            );
        }
    }
}
