#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // LZNT1 DECODES ATTACKER-DECLARED LENGTHS. Every chunk header in a
    // compressed unit says how long the chunk is and how far back its
    // back-references reach, and all of it comes off the disk (#296).
    // This is also the one decoder whose only other coverage is a happy
    // path against a Windows-built fixture nothing can rebuild (#279).
    //
    // The cap is what a caller would pass: a compression unit is 16
    // clusters, 64 KiB at the usual geometry.
    let _ = fs_ntfs::compression::decompress_unit(data, 64 * 1024);
});
