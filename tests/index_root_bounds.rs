//! The index ends where the attribute holding it ends.
//!
//! `$INDEX_ROOT`'s entries live inside one attribute's resident value.
//! `remove_index_entry` and the collated insert were taught that; the
//! three readers were not, and they walked to `ih_start + total_size`
//! bounded only by the MFT record. The comment on the writers' bound
//! names the reader as the source of the entries it has to defend
//! against.
//!
//! Two shapes, both starting from an `$INDEX_ROOT` that does not parse —
//! the ordinary result of an interrupted write to a directory record.
//!
//! * A `total_size` past the value makes `readdir` decode whatever
//!   follows the attribute in the same record as index entries. Each
//!   fabricated entry carries a 48-bit record number taken from those
//!   bytes, which the caller then opens.
//! * `index_root_has_real_entries` answered `Ok(false)` — "no real
//!   entries" — when it could not read the header, and `rmdir` deletes
//!   a directory on that answer. An unreadable index is the one input
//!   where reporting emptiness is destructive.

use fs_ntfs::attr_io::{self, AttrType};
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::index_io;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{mft_io, read, write};
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;

/// `$INDEX_ROOT` value layout: the INDEX_HEADER starts 16 bytes in.
const IR_INDEX_HEADER_OFFSET: usize = 16;
const IH_FIRST_ENTRY_OFFSET: usize = 0;
const IH_TOTAL_SIZE_OF_ENTRIES: usize = 4;
/// Index-entry header: file_reference(8) length(2) key_length(2) flags(2).
const IE_LENGTH: usize = 0x08;
const IE_KEY_LENGTH: usize = 0x0A;
const IE_FLAGS: usize = 0x0C;
const IE_FLAG_LAST: u16 = 0x02;
/// `$FILE_NAME` key: name_length is one byte 0x40 in, the UTF-16 name
/// starts at 0x42.
const FN_NAME_LENGTH_OFFSET: usize = 0x40;
const FN_NAMESPACE_OFFSET: usize = 0x41;
const FN_NAME_OFFSET: usize = 0x42;

fn fresh_volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_irbounds_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("IRBD"), Some(8))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

/// Where a directory record's `$INDEX_ROOT:$I30` value starts and ends.
fn index_root_value(record: &[u8]) -> (usize, usize) {
    let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some("$I30"))
        .expect("$INDEX_ROOT:$I30");
    assert!(ir.is_resident);
    let off = ir.attr_offset + ir.resident_value_offset.expect("value_offset") as usize;
    let len = ir.resident_value_length.expect("value_length") as usize;
    (off, off + len)
}

/// A directory holding one real file, whose `$INDEX_ROOT` then claims
/// its entries run well past its own value, with a plausible-looking
/// index entry planted immediately after it. That is what a resident
/// `$DATA` or a `$FILE_NAME` sitting after the attribute looks like to a
/// walk that is not bounded by the value.
fn directory_with_a_planted_entry(tag: &str) -> (String, u64) {
    let img = fresh_volume(tag);
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.mkdir("/", "dir").expect("mkdir");
    fs.create_file("/dir", "real.txt").expect("create real.txt");
    drop(fs);

    let mut io = PathIo::open_rw(Path::new(&img)).expect("open_rw");
    let dir_rec = read::resolve_path(&mut io, "/dir").expect("resolve /dir");
    mft_io::update_mft_record_io(&mut io, dir_rec, |record| {
        let (value_start, value_end) = index_root_value(record);
        let ih = value_start + IR_INDEX_HEADER_OFFSET;
        let first_entry_rel = u32::from_le_bytes(
            record[ih + IH_FIRST_ENTRY_OFFSET..ih + IH_FIRST_ENTRY_OFFSET + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let real_total = u32::from_le_bytes(
            record[ih + IH_TOTAL_SIZE_OF_ENTRIES..ih + IH_TOTAL_SIZE_OF_ENTRIES + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        assert!(first_entry_rel > 0 && real_total > first_entry_rel);

        // Clear the LAST bit on the trailing sentinel. A walk bounded
        // only by the record stops there today, so leaving it set would
        // hide the defect rather than test it -- and a sentinel whose
        // flags word did not survive a torn write is the same corruption
        // that produces the overlong total_size.
        let mut cursor = ih + first_entry_rel;
        let scan_end = ih + real_total;
        while cursor + 0x10 <= scan_end {
            let len =
                u16::from_le_bytes([record[cursor + IE_LENGTH], record[cursor + IE_LENGTH + 1]])
                    as usize;
            let flags =
                u16::from_le_bytes([record[cursor + IE_FLAGS], record[cursor + IE_FLAGS + 1]]);
            if flags & IE_FLAG_LAST != 0 {
                record[cursor + IE_FLAGS..cursor + IE_FLAGS + 2]
                    .copy_from_slice(&(flags & !IE_FLAG_LAST).to_le_bytes());
                break;
            }
            if len == 0 {
                break;
            }
            cursor += len;
        }

        // Plant an entry just past the value: 0x60 bytes, a $FILE_NAME
        // key naming "ghost", pointing at MFT record 0x1234.
        let planted = value_end;
        let entry_len = 0x60usize;
        assert!(planted + entry_len <= record.len());
        for b in &mut record[planted..planted + entry_len] {
            *b = 0;
        }
        record[planted..planted + 8].copy_from_slice(&0x1234u64.to_le_bytes());
        record[planted + IE_LENGTH..planted + IE_LENGTH + 2]
            .copy_from_slice(&(entry_len as u16).to_le_bytes());
        record[planted + IE_KEY_LENGTH..planted + IE_KEY_LENGTH + 2]
            .copy_from_slice(&0x4Cu16.to_le_bytes());
        record[planted + IE_FLAGS..planted + IE_FLAGS + 2].copy_from_slice(&0u16.to_le_bytes());
        let key = planted + 0x10;
        record[key + FN_NAME_LENGTH_OFFSET] = 5;
        record[key + FN_NAMESPACE_OFFSET] = 1;
        for (i, c) in "ghost".encode_utf16().enumerate() {
            record[key + FN_NAME_OFFSET + i * 2..key + FN_NAME_OFFSET + i * 2 + 2]
                .copy_from_slice(&c.to_le_bytes());
        }

        // And say the entries run past the value into it.
        let overlong = (planted + entry_len - ih) as u32;
        record[ih + IH_TOTAL_SIZE_OF_ENTRIES..ih + IH_TOTAL_SIZE_OF_ENTRIES + 4]
            .copy_from_slice(&overlong.to_le_bytes());
        Ok(())
    })
    .expect("plant the entry");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    (img, dir_rec)
}

#[test]
fn readdir_does_not_decode_bytes_past_the_index_root_value() {
    let (img, _) = directory_with_a_planted_entry("readdir");
    let fs = Filesystem::mount(&img).expect("mount");
    match fs.read_dir("/dir") {
        Err(_) => {}
        Ok(entries) => {
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert!(
                !names.contains(&"ghost"),
                "read_dir returned an entry decoded from bytes outside the \
                 $INDEX_ROOT value: {names:?}"
            );
        }
    }
}

#[test]
fn a_lookup_does_not_match_a_name_past_the_index_root_value() {
    let (img, dir_rec) = directory_with_a_planted_entry("lookup");
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let (_, record) = mft_io::read_mft_record_io(&mut io, dir_rec).expect("read dir record");
    match index_io::find_index_entry(&record, "ghost", None) {
        Err(_) => {}
        Ok(hit) => assert!(
            hit.is_none(),
            "find_index_entry matched a name outside the $INDEX_ROOT value, \
             pointing at record {}",
            hit.unwrap().file_record_number
        ),
    }
    // The real entry is still found.
    let real = index_io::find_index_entry(&record, "real.txt", None)
        .expect("the real entry is inside the value")
        .expect("real.txt is there");
    assert!(real.file_record_number > 0);
}

#[test]
fn rmdir_refuses_a_directory_whose_index_it_cannot_read() {
    let img = fresh_volume("rmdir");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.mkdir("/", "full").expect("mkdir");
    fs.create_file("/full", "child.txt").expect("create child");
    drop(fs);

    let mut io = PathIo::open_rw(Path::new(&img)).expect("open_rw");
    let dir_rec = read::resolve_path(&mut io, "/full").expect("resolve /full");
    let child_rec = read::resolve_path(&mut io, "/full/child.txt").expect("resolve child");
    // Push first_entry_offset past the end of the record. The header is
    // now unreadable; the directory still has a child.
    mft_io::update_mft_record_io(&mut io, dir_rec, |record| {
        let (value_start, _) = index_root_value(record);
        let ih = value_start + IR_INDEX_HEADER_OFFSET;
        record[ih + IH_FIRST_ENTRY_OFFSET..ih + IH_FIRST_ENTRY_OFFSET + 4]
            .copy_from_slice(&0x000F_0000u32.to_le_bytes());
        Ok(())
    })
    .expect("break the index header");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);

    let removed = write::rmdir(Path::new(&img), "/full");
    assert!(
        removed.is_err(),
        "rmdir deleted a directory whose index it could not read; its child's \
         MFT record is now orphaned"
    );

    // And both records are still in use.
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let (_, child) = mft_io::read_mft_record_io(&mut io, child_rec).expect("read child");
    assert!(
        mft_io::record_flags(&child) & mft_io::MFT_FLAG_IN_USE != 0,
        "the child record should still be in use"
    );
    let (_, dir) = mft_io::read_mft_record_io(&mut io, dir_rec).expect("read dir");
    assert!(
        mft_io::record_flags(&dir) & mft_io::MFT_FLAG_IN_USE != 0,
        "the directory should not have been freed"
    );
}

#[test]
fn an_empty_directory_can_still_be_removed() {
    // The emptiness check got stricter; it must not have got so strict
    // that an ordinary empty directory is refused.
    let img = fresh_volume("empty");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.mkdir("/", "empty").expect("mkdir");
    drop(fs);
    write::rmdir(Path::new(&img), "/empty").expect("rmdir on an empty directory");

    // And a non-empty one is still refused for the ordinary reason.
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.mkdir("/", "busy").expect("mkdir");
    fs.create_file("/busy", "c.txt").expect("create");
    drop(fs);
    let err = write::rmdir(Path::new(&img), "/busy").expect_err("busy dir");
    assert!(err.contains("not empty"), "{err}");
}
