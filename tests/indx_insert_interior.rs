//! An index entry may only be spliced into a LEAF node.
//!
//! Every entry in an interior node carries an 8-byte child VCN in its
//! tail, and `build_file_name_index_entry` emits a leaf entry with none.
//! Splice one into an interior node and `ntfs.sys` reads the child VCN of
//! the new entry from the last eight bytes of its own UTF-16 filename,
//! then descends to whatever VCN those bytes happen to spell.
//!
//! The `$INDEX_ROOT` insert refuses this, and `remove_index_entry` refuses
//! it for both block kinds. The INDX insert was the one member of the trio
//! that never read the node header's flags byte.

use fs_ntfs::index_io;

const INDX_INDEX_HEADER_OFFSET: usize = 0x18;
const IH_FIRST_ENTRY_OFFSET: usize = 0x00;
const IH_TOTAL_SIZE_OF_ENTRIES: usize = 0x04;
const IH_ALLOCATED_SIZE_OF_ENTRIES: usize = 0x08;
const IH_FLAGS_OFFSET: usize = 0x0C;

const IE_LENGTH: usize = 0x08;
const IE_KEY_LENGTH: usize = 0x0A;
const IE_FLAGS: usize = 0x0C;

const IE_FLAG_HAS_SUBNODE: u16 = 0x01;
const IE_FLAG_LAST: u16 = 0x02;

const BLOCK_LEN: usize = 4096;
const FIRST_ENTRY_REL: usize = 0x10;

/// An INDX block holding nothing but its end-of-entries sentinel.
///
/// `has_subnodes` picks which kind of node it is, and the sentinel is
/// shaped to match: an interior node's entries are eight bytes longer,
/// because each ends in the VCN of the child holding every key below it.
fn indx_block(has_subnodes: bool) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_LEN];
    block[0..4].copy_from_slice(b"INDX");

    let ih = INDX_INDEX_HEADER_OFFSET;
    let last_len: usize = if has_subnodes { 0x18 } else { 0x10 };
    let total_size = FIRST_ENTRY_REL + last_len;

    let w32 = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    w32(
        &mut block,
        ih + IH_FIRST_ENTRY_OFFSET,
        FIRST_ENTRY_REL as u32,
    );
    w32(&mut block, ih + IH_TOTAL_SIZE_OF_ENTRIES, total_size as u32);
    w32(
        &mut block,
        ih + IH_ALLOCATED_SIZE_OF_ENTRIES,
        (BLOCK_LEN - ih) as u32,
    );
    block[ih + IH_FLAGS_OFFSET] = u8::from(has_subnodes);

    // The end-of-entries sentinel.
    let last = ih + FIRST_ENTRY_REL;
    block[last + IE_LENGTH..last + IE_LENGTH + 2].copy_from_slice(&(last_len as u16).to_le_bytes());
    block[last + IE_KEY_LENGTH..last + IE_KEY_LENGTH + 2].copy_from_slice(&0u16.to_le_bytes());
    let flags = if has_subnodes {
        IE_FLAG_LAST | IE_FLAG_HAS_SUBNODE
    } else {
        IE_FLAG_LAST
    };
    block[last + IE_FLAGS..last + IE_FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
    block
}

fn entry() -> Vec<u8> {
    index_io::build_file_name_index_entry(
        0x0001_0000_0000_002A,
        0x0001_0000_0000_0005,
        "notes.txt",
        0x01DA_0000_0000_0000,
        false,
    )
    .expect("build entry")
}

#[test]
fn inserting_into_an_interior_indx_block_is_refused() {
    let mut block = indx_block(true);
    let before = block.clone();

    let result = index_io::insert_entry_into_indx_block(&mut block, &entry(), "notes.txt");

    assert!(
        result.is_err(),
        "a leaf entry must not be spliced into an interior node; insert returned {result:?}"
    );
    assert_eq!(
        block, before,
        "a refused insert must leave the block exactly as it found it"
    );
}

#[test]
fn inserting_into_a_leaf_indx_block_still_works() {
    // The control. Without it, the fix could be "refuse every INDX
    // insert", which would pass the test above and break every create in
    // a directory that has overflowed its $INDEX_ROOT.
    let mut block = indx_block(false);

    index_io::insert_entry_into_indx_block(&mut block, &entry(), "notes.txt")
        .expect("a leaf node accepts a leaf entry");

    let found = index_io::find_entry_in_indx_block(&block, "notes.txt", None)
        .expect("search the block after the insert");
    assert!(found.is_some(), "the entry that was inserted must be found");
}
