#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // iter_attributes walks an NTFS MFT record buffer (post-fixup),
    // chasing variable-length attribute headers via attribute_length
    // until a 0xFFFFFFFF terminator. The fuzz target drains the
    // iterator so any panic mid-walk surfaces.
    for _ in fs_ntfs::attr_io::iter_attributes(data) {
        // Consume; iterator yields AttrLocation values.
    }
});
