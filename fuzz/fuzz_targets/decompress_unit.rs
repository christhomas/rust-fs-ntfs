#![no_main]
//! LZNT1 decompression of one compression unit.
//!
//! The output length is the caller's, but the stream declares its own
//! run lengths and back-reference distances -- the shape of defect that
//! has produced CVEs in every LZ-family decoder ever written.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for max_len in [0usize, 4096, 65_536] {
        let _ = fs_ntfs::compression::decompress_unit(data, max_len);
    }
});
