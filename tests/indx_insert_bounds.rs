//! The INDX insert's three header fields come off the disk and bound a
//! `copy_within`. They have to be checked before they are used.
//!
//! `insert_entry_into_index_root` was hardened against exactly this and
//! says so in a comment: a `first_entry_offset` past `total_size` produced
//! a reversed range, and one inside the value but not at an entry boundary
//! spliced the new entry into the middle of an existing one — "and THAT
//! record was written back". The INDX insert does the same job on the
//! other block kind and trusted all three values.

use fs_ntfs::index_io;

const INDX_INDEX_HEADER_OFFSET: usize = 0x18;
const IH_FIRST_ENTRY_OFFSET: usize = 0x00;
const IH_TOTAL_SIZE_OF_ENTRIES: usize = 0x04;
const IH_ALLOCATED_SIZE_OF_ENTRIES: usize = 0x08;

const IE_LENGTH: usize = 0x08;
const IE_KEY_LENGTH: usize = 0x0A;
const IE_FLAGS: usize = 0x0C;
const IE_FLAG_LAST: u16 = 0x02;

const BLOCK_LEN: usize = 4096;

fn w32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// A leaf INDX block whose header fields are whatever the caller says.
/// `sentinel_at` places the end-of-entries entry, in bytes from the index
/// header.
fn block_with(
    first_entry_rel: u32,
    total_size: u32,
    allocated_size: u32,
    sentinel_at: Option<usize>,
) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_LEN];
    block[0..4].copy_from_slice(b"INDX");
    let ih = INDX_INDEX_HEADER_OFFSET;
    w32(&mut block, ih + IH_FIRST_ENTRY_OFFSET, first_entry_rel);
    w32(&mut block, ih + IH_TOTAL_SIZE_OF_ENTRIES, total_size);
    w32(
        &mut block,
        ih + IH_ALLOCATED_SIZE_OF_ENTRIES,
        allocated_size,
    );
    if let Some(rel) = sentinel_at {
        let last = ih + rel;
        block[last + IE_LENGTH..last + IE_LENGTH + 2].copy_from_slice(&0x10u16.to_le_bytes());
        block[last + IE_KEY_LENGTH..last + IE_KEY_LENGTH + 2].copy_from_slice(&0u16.to_le_bytes());
        block[last + IE_FLAGS..last + IE_FLAGS + 2].copy_from_slice(&IE_FLAG_LAST.to_le_bytes());
    }
    block
}

fn entry(parent_reference: u64) -> Vec<u8> {
    index_io::build_file_name_index_entry(
        0x0001_0000_0000_002A,
        parent_reference,
        "notes.txt",
        0x01DA_0000_0000_0000,
        false,
    )
    .expect("build entry")
}

/// `first_entry_offset` past `total_size` leaves the sorted-position walk
/// unrun and `insertion_point` greater than `end`, so the shift is asked
/// for a range that runs backwards.
#[test]
fn a_first_entry_offset_past_the_entries_is_refused_rather_than_reversing_the_shift() {
    let mut block = block_with(
        0x40,
        0x20,
        (BLOCK_LEN - INDX_INDEX_HEADER_OFFSET) as u32,
        None,
    );
    let result = index_io::insert_entry_into_indx_block(&mut block, &entry(5), "notes.txt");
    assert!(
        result.is_err(),
        "a first entry offset past the entries must be refused; got {result:?}"
    );
}

/// The only check the function had compared two disk-supplied values to
/// each other. Neither was tied to the block, so a `total_size` far past
/// the end of a 4096-byte block passed it, and the shift read from
/// outside the buffer.
#[test]
fn a_total_size_past_the_block_is_refused_rather_than_shifting_from_outside_it() {
    // The sentinel is what makes this reach the shift: without it the walk
    // meets a zero-length entry and the malformed-entry check catches the
    // block for the wrong reason.
    let mut block = block_with(0x10, 0x10_0000, 0xFFFF_FFFF, Some(0x10));
    let result = index_io::insert_entry_into_indx_block(&mut block, &entry(5), "notes.txt");
    assert!(
        result.is_err(),
        "a total size past the block must be refused; got {result:?}"
    );
}

/// The quietest of the three, and the one that gets written back to the
/// disk: a `first_entry_offset` that is inside the entries but not on an
/// entry boundary starts the walk mid-entry, and the new entry is laid
/// across the bytes of the one already there.
///
/// The parent reference is chosen so that the two bytes the walk reads as
/// an entry's flags — at `cursor + 0x0C`, which from this cursor lands on
/// the first entry's `parent_reference` — spell the LAST sentinel. The
/// walk therefore stops immediately and splices right there.
#[test]
fn an_unaligned_first_entry_offset_cannot_splice_an_entry_across_another() {
    let existing = entry(0x0001_0000_0000_0002);
    let ih = INDX_INDEX_HEADER_OFFSET;
    let total = 0x10 + existing.len() + 0x10;
    let mut block = block_with(
        0x14, // inside the entries, and not on an 8-byte boundary
        total as u32,
        (BLOCK_LEN - ih) as u32,
        Some(0x10 + existing.len()),
    );
    block[ih + 0x10..ih + 0x10 + existing.len()].copy_from_slice(&existing);
    let before = block.clone();

    let result = index_io::insert_entry_into_indx_block(&mut block, &entry(5), "later.txt");

    assert!(
        result.is_err(),
        "an insert starting mid-entry must be refused; got {result:?}"
    );
    assert_eq!(
        block, before,
        "a refused insert must leave the block exactly as it found it"
    );
}

/// The control: a well-formed block must still take an entry, so none of
/// the above can be satisfied by refusing everything.
#[test]
fn a_well_formed_block_still_accepts_an_entry() {
    let mut block = block_with(
        0x10,
        0x20,
        (BLOCK_LEN - INDX_INDEX_HEADER_OFFSET) as u32,
        Some(0x10),
    );
    index_io::insert_entry_into_indx_block(&mut block, &entry(5), "notes.txt")
        .expect("a well-formed leaf block accepts an entry");
    let found = index_io::find_entry_in_indx_block(&block, "notes.txt", None).expect("search");
    assert!(found.is_some(), "the entry that was inserted must be found");
}
