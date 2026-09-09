//! A recycled MFT record does not restart its sequence number.
//!
//! The sequence at record header offset `0x10` is what makes a file
//! reference more than a record number: it is bumped every time a slot
//! is reused, so a reference carrying the old value stops matching.
//! Every record this driver created was stamped the literal `1`, and
//! `find_free_record_io` hands back the first clear bit from its hint
//! upward -- so create A, delete A, create B put both records in the
//! same slot with the same sequence, and a reference to A was
//! bit-identical to one to B.
//!
//! The precondition is asserted rather than assumed: each test checks
//! that the second create landed on the SAME record number, because
//! without that the sequence assertion would pass on a volume where no
//! slot was recycled at all -- a test that cannot observe the defect it
//! is written for.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{mft_io, write};
use std::path::Path;

const VOL_SIZE: u64 = 64 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_vol(tag: &str) -> String {
    let dst = format!("test-disks/_seq_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create temp image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        CLUSTER,
        CLUSTER,
        Some("SEQ"),
        Some(0x5E_51_00_00),
    )
    .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

fn sequence_of(img: &str, record: u64) -> u16 {
    let (_, bytes) = mft_io::read_mft_record(Path::new(img), record).expect("read record");
    mft_io::record_sequence(&bytes)
}

/// A FRESH SLOT STILL STARTS AT 1.
///
/// The acceptance half: nothing about a volume this driver formats and
/// then writes to for the first time changes. A slot with no `FILE`
/// magic has never held a record, so there is no history to continue.
#[test]
fn a_slot_that_never_held_a_record_starts_at_one() {
    let img = fresh_vol("virgin");
    let rec = write::create_file(Path::new(&img), "/", "first.txt").expect("create");
    assert_eq!(
        sequence_of(&img, rec),
        1,
        "record {rec} has never been used before, so its sequence starts at 1"
    );
}

/// THE DEFECT, ON THE CREATE PATH.
#[test]
fn a_recycled_slot_does_not_repeat_its_predecessors_sequence() {
    let img = fresh_vol("create");
    let first = write::create_file(Path::new(&img), "/", "a.txt").expect("create a");
    let before = sequence_of(&img, first);
    write::unlink(Path::new(&img), "/a.txt").expect("unlink a");

    let second = write::create_file(Path::new(&img), "/", "b.txt").expect("create b");
    assert_eq!(
        first, second,
        "the allocator returns the first free record, so b.txt must land on \
         the slot a.txt just freed -- without that this test proves nothing"
    );
    let after = sequence_of(&img, second);
    assert_ne!(
        before, after,
        "a reference to a.txt would still match b.txt: same record {first}, \
         same sequence {before}"
    );
    assert_eq!(
        after,
        before + 1,
        "the slot's history continues from {before}, it does not restart"
    );
}

/// THE SAME DEFECT ON THE MKDIR PATH, which builds its record in a
/// second place and carried its own copy of the literal.
#[test]
fn a_recycled_slot_taken_by_a_directory_also_advances() {
    let img = fresh_vol("mkdir");
    let first = write::create_file(Path::new(&img), "/", "a.txt").expect("create a");
    let before = sequence_of(&img, first);
    write::unlink(Path::new(&img), "/a.txt").expect("unlink a");

    let second = write::mkdir(Path::new(&img), "/", "d").expect("mkdir d");
    assert_eq!(
        first, second,
        "the directory must land on the slot a.txt just freed"
    );
    assert_eq!(
        sequence_of(&img, second),
        before + 1,
        "mkdir continues the slot's history too"
    );
}

/// THE REFERENCE THE PARENT STORES CARRIES THE NEW SEQUENCE.
///
/// Stamping the record is only half of it: the file reference written
/// into the parent's index is built from the same value, so if the two
/// ever disagree the entry points at a record it does not match, and
/// the driver would refuse its own file the moment the read side starts
/// checking.
#[test]
fn the_index_entry_carries_the_sequence_the_record_was_stamped_with() {
    let img = fresh_vol("index");
    let first = write::create_file(Path::new(&img), "/", "a.txt").expect("create a");
    write::unlink(Path::new(&img), "/a.txt").expect("unlink a");
    let second = write::create_file(Path::new(&img), "/", "b.txt").expect("create b");
    assert_eq!(first, second, "b.txt must land on the freed slot");

    let stamped = sequence_of(&img, second);
    let (_, root) = mft_io::read_mft_record(Path::new(&img), 5).expect("read root record");
    let entry = fs_ntfs::index_io::find_index_entry(&root, "b.txt", None)
        .expect("index search")
        .expect("b.txt is in the root index");
    assert_eq!(
        entry.file_record_number, second,
        "the entry names the record the create returned"
    );
    assert_eq!(
        entry.sequence, stamped,
        "the reference in the index carries the sequence the record was stamped with"
    );
}
