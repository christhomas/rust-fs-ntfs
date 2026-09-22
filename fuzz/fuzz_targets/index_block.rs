#![no_main]
//! An INDX block: the structure a directory lookup walks, with entry
//! lengths and a stream length that are both read out of the block.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut out = Vec::new();
    let _ = fs_ntfs::index_io::collect_indx_block_entries(data, &mut out);
});
