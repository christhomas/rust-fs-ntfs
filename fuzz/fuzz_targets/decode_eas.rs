#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // ea_io::decode walks a packed list of FILE_FULL_EA_INFORMATION
    // entries. Every malformed shape is acceptable as an Err; we're
    // hunting panics, OOB reads on the variable-length name + value
    // fields, and integer overflows on the 4-byte aligned offsets.
    let _ = fs_ntfs::ea_io::decode(data);
});
