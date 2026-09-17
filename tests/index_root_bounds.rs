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

mod common;

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
    let dst = common::temp_image_path(format!("irbounds_{tag}"));
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

// ---------------------------------------------------------------------------
// The splice point has to be an entry boundary, and the value has to hold
// both headers (#271); a name that does not parse is a finding, not an
// entry named "" (#244).
// ---------------------------------------------------------------------------

/// Where the index header's `first_entry_offset` lives in a directory
/// record, and its current value.
fn first_entry_offset_field(record: &[u8]) -> (usize, u32) {
    let (value_start, _) = index_root_value(record);
    let at = value_start + IR_INDEX_HEADER_OFFSET + IH_FIRST_ENTRY_OFFSET;
    (
        at,
        u32::from_le_bytes(record[at..at + 4].try_into().unwrap()),
    )
}

/// A directory record, read out of a fresh volume, holding one file.
///
/// Returned as bytes so each test can corrupt its own copy in memory and
/// hand it straight to the `pub` function under test. Both of #271's
/// functions take a buffer, which is exactly the surface a consumer of
/// this crate has, and the issue's reachability argument rests on that.
fn directory_record(tag: &str) -> Vec<u8> {
    let img = fresh_volume(tag);
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.mkdir("/", "dir").expect("mkdir");
    fs.create_file("/dir", "real.txt").expect("create real.txt");
    drop(fs);
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let dir_rec = read::resolve_path(&mut io, "/dir").expect("resolve /dir");
    let (_, record) = mft_io::read_mft_record_io(&mut io, dir_rec).expect("read dir record");
    record
}

/// An entry to splice in, so an insert that is refused is refused by the
/// bound under test rather than by its input.
fn an_entry_to_insert() -> Vec<u8> {
    index_io::build_file_name_index_entry(0x2A, 0x05, "new.txt", 0, false)
        .expect("build a $FILE_NAME index entry")
}

/// Every `first_entry_offset` that cannot begin an entry: inside the
/// 16-byte index header, and every misalignment of a plausible value.
///
/// 0x18 is the real one on a directory mkfs wrote, so the list below is
/// what a torn write or a hostile image can put there *instead* — the
/// walk started at `ih_start + this` and the new entry was spliced where
/// the walk stopped.
const NOT_ENTRY_BOUNDARIES: [u32; 6] = [0, 8, 15, 0x11, 0x17, 0x1F];

#[test]
fn an_insert_whose_first_entry_offset_is_not_an_entry_boundary_is_refused() {
    let pristine = directory_record("splice_insert");
    let entry = an_entry_to_insert();
    for bad in NOT_ENTRY_BOUNDARIES {
        let mut record = pristine.clone();
        let (at, real) = first_entry_offset_field(&record);
        assert_ne!(real, bad, "the fixture must not already hold {bad}");
        record[at..at + 4].copy_from_slice(&bad.to_le_bytes());
        let before = record.clone();

        let err = index_io::insert_entry_into_index_root_with_collation(
            &mut record,
            &entry,
            "new.txt",
            None,
        )
        .expect_err(&format!(
            "a first_entry_offset of {bad} is not the start of an entry; the insert \
             must refuse it rather than splice at whatever the walk reaches"
        ));
        assert!(
            err.contains("not an entry boundary") || err.contains("past the"),
            "expected the boundary refusal for {bad}, got: {err}"
        );
        assert_eq!(
            record, before,
            "a refused insert must not have written into the record (first_entry_offset {bad})"
        );
    }
}

#[test]
fn a_lookup_whose_first_entry_offset_is_not_an_entry_boundary_is_refused() {
    // The write paths run this as their collision check and act on its
    // answer, so a walk that starts mid-entry is not merely a bad read.
    let pristine = directory_record("splice_lookup");
    for bad in NOT_ENTRY_BOUNDARIES {
        let mut record = pristine.clone();
        let (at, _) = first_entry_offset_field(&record);
        record[at..at + 4].copy_from_slice(&bad.to_le_bytes());
        let got = index_io::find_index_entry(&record, "real.txt", None);
        assert!(
            got.is_err(),
            "find_index_entry walked from a first_entry_offset of {bad} and answered \
             {got:?} instead of refusing"
        );
    }
    // The untouched record still resolves, so the refusals above are
    // about the planted offset and not about the check refusing
    // everything.
    let record = pristine.clone();
    assert!(
        index_io::find_index_entry(&record, "real.txt", None)
            .expect("a well-formed index root")
            .is_some(),
        "real.txt is in this directory"
    );
}

#[test]
fn an_index_root_value_too_short_for_both_headers_is_refused_by_both_mutators() {
    // `IR_INDEX_HEADER_OFFSET` is 16 and the INDEX_HEADER that starts
    // there is another 16, so 16..=31 used to pass and then read
    // `first_entry_offset` and `total_size` out of the next attribute.
    let pristine = directory_record("short_value");
    let entry = an_entry_to_insert();
    let located = index_io::find_index_entry(&pristine, "real.txt", None)
        .expect("a well-formed index root")
        .expect("real.txt is there");

    for short in [16u16, 20, 24, 31] {
        let mut record = pristine.clone();
        let ir = attr_io::find_attribute(&record, AttrType::IndexRoot, Some("$I30"))
            .expect("$INDEX_ROOT:$I30");
        let len_at = ir.attr_offset + 0x10; // resident value_length
        record[len_at..len_at + 4].copy_from_slice(&u32::from(short).to_le_bytes());
        let before = record.clone();

        let insert_err = index_io::insert_entry_into_index_root_with_collation(
            &mut record.clone(),
            &entry,
            "new.txt",
            None,
        )
        .expect_err(&format!(
            "a {short}-byte value cannot hold both headers; the insert must refuse it"
        ));
        assert!(
            insert_err.contains("too short"),
            "expected the short-value refusal for {short}, got: {insert_err}"
        );

        let remove_err =
            index_io::remove_index_entry(&mut record, &located, index_io::BlockKind::IndexRoot)
                .expect_err(&format!(
                    "a {short}-byte value cannot hold both headers; the removal must \
                     refuse it"
                ));
        assert!(
            remove_err.contains("too short"),
            "expected the short-value refusal for {short}, got: {remove_err}"
        );
        assert_eq!(
            record, before,
            "a refused removal must not have shifted bytes (value_length {short})"
        );
    }
}

#[test]
fn a_name_that_runs_past_its_entry_is_a_finding_not_an_empty_name() {
    // `entry_name(...).unwrap_or_default()` turned a name field that
    // overruns its own entry into `[]`, and then compared that against
    // the wanted name as if the entry were legitimately unnamed — so a
    // lookup for "" would have matched it, and the corruption was never
    // reported. See #244.
    let mut record = directory_record("bad_name");
    let located = index_io::find_index_entry(&record, "real.txt", None)
        .expect("a well-formed index root")
        .expect("real.txt is there");

    // Say the name is 255 UTF-16 units long: 510 bytes of name in an
    // entry that is nowhere near that big.
    let key = located.record_offset + 0x10;
    record[key + FN_NAME_LENGTH_OFFSET] = 255;

    let err = index_io::find_index_entry(&record, "real.txt", None)
        .expect_err("a name that runs past its entry must be reported");
    assert!(
        err.contains("runs past"),
        "expected the name-overrun refusal, got: {err}"
    );
    // And the fabricated empty name is not matchable either.
    assert!(
        index_io::find_index_entry(&record, "", None).is_err(),
        "a lookup for the empty name must not match an entry whose name failed to parse"
    );
}

/// The short-value half of #271, with the outcome rather than the
/// message: a value too short to hold the index header made the mutators
/// read `first_entry_offset` and `total_size` from *past* the value, and
/// with those bytes saying "no entries, starting at zero" the insert
/// spliced a new entry outside the value it was told it had.
///
/// The declared `value_length` is cut to 16 while the attribute itself is
/// left its real size, which is what a torn write to the attribute header
/// produces, and the two header fields at `ih_start` — now outside the
/// declared value — are set so the old code's own bounds tests pass.
#[test]
fn an_insert_into_a_value_too_short_for_the_header_stays_out_of_the_record() {
    let mut record = directory_record("short_value_outcome");
    let (value_start, _) = index_root_value(&record);
    let ih = value_start + IR_INDEX_HEADER_OFFSET;

    // "First entry at 0, zero bytes of entries" — past the 16-byte value
    // the attribute header is about to claim.
    record[ih + IH_FIRST_ENTRY_OFFSET..ih + IH_FIRST_ENTRY_OFFSET + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    record[ih + IH_TOTAL_SIZE_OF_ENTRIES..ih + IH_TOTAL_SIZE_OF_ENTRIES + 4]
        .copy_from_slice(&0u32.to_le_bytes());

    let ir = attr_io::find_attribute(&record, AttrType::IndexRoot, Some("$I30"))
        .expect("$INDEX_ROOT:$I30");
    let len_at = ir.attr_offset + 0x10; // resident value_length
    record[len_at..len_at + 4].copy_from_slice(&16u32.to_le_bytes());

    let before = record.clone();
    let entry = an_entry_to_insert();
    let err =
        index_io::insert_entry_into_index_root_with_collation(&mut record, &entry, "new.txt", None)
            .expect_err(
                "a 16-byte value cannot hold the index header, so there is nowhere inside it \
         to splice an entry; the insert must refuse rather than write past the value",
            );
    assert!(
        err.contains("too short"),
        "expected the short-value refusal, got: {err}"
    );
    assert_eq!(
        record, before,
        "a refused insert must not have written into the record"
    );
}
