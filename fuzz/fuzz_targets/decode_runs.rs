#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // decode_runs walks NTFS mapping-pair lists. The error path is
    // expected for malformed input; we want libfuzzer to detect
    // panics, OOB reads, and infinite loops — not Result::Err.
    let _ = fs_ntfs::data_runs::decode_runs(data);
});
