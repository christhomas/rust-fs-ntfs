#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // THE FIXUP APPLIER REWRITES BYTES AT OFFSETS THE RECORD DECLARES:
    // the update-sequence array's own offset and count come out of the
    // record being fixed up, and it writes a two-byte tail into every
    // sector-sized stride (#296). A record is read before anything
    // about it has been validated, so this runs on the rawest bytes
    // the driver ever touches.
    //
    // The sector size is part of the input rather than fixed, because
    // the stride count is derived from it and a hostile boot sector
    // chooses it.
    if data.len() < 3 {
        return;
    }
    let bytes_per_sector = match data[0] % 4 {
        0 => 512u16,
        1 => 1024,
        2 => 2048,
        _ => 4096,
    };
    let mut record = data[1..].to_vec();
    let _ = fs_ntfs::mft_io::apply_fixup_on_read(&mut record, bytes_per_sector);
});
