// Shared by both tiers, included textually rather than depended on.
//
// `tests/fuzz_decoders.rs` and `fuzz/src/lib.rs` both `include!` this
// file. A crate dependency would have been tidier, but the fuzz crate
// depends on `libfuzzer-sys`, which builds libFuzzer's C++ runtime, and
// making the gate depend on the fuzz crate would drag that into every
// pull request build on the stable toolchain.
//
// What matters is that the two tiers read a volume identically, so a
// reproducer from one reproduces in the other.

use fs_ntfs::block_io::BlockIo;

/// A volume held in memory, presented as block I/O.
pub struct Bytes(pub Vec<u8>);

impl BlockIo for Bytes {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(buf.len());
        if end > self.0.len() {
            // What a real device answers for a read past its end, so a
            // crafted volume cannot be told apart from a truncated one
            // by which error it provokes.
            return Err(format!(
                "read of {} bytes at {offset} runs past the end of a {}-byte volume",
                buf.len(),
                self.0.len()
            ));
        }
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }

    fn write_all_at(&mut self, _offset: u64, _buf: &[u8]) -> Result<(), String> {
        // Read-only: the fuzzing never writes, and a volume that
        // accepted writes would let a target mutate its own input
        // between cases and stop being deterministic.
        Err("this volume is read-only".to_string())
    }

    fn size(&self) -> u64 {
        self.0.len() as u64
    }
}

/// How many MFT records one walk will read.
///
/// A crafted boot sector can put the MFT anywhere and claim any size,
/// and reading all of it would make a case slow rather than failing it
/// -- which reads as a hang without being one.
pub const RECORD_BUDGET: u64 = 48;

/// Open a volume and read what a caller would: the boot geometry, the
/// first MFT records, their attributes, and the root directory.
///
/// Every result is discarded. A crafted volume is *supposed* to be
/// refused; what it may not do is panic, hang, or read somebody else's
/// memory.
pub fn walk(image: &[u8]) {
    let mut io = Bytes(image.to_vec());

    let Ok(params) = fs_ntfs::mft_io::read_boot_params_io(&mut io) else {
        return;
    };

    for record_number in 0..RECORD_BUDGET {
        let _ = fs_ntfs::mft_io::mft_record_offset(&params, record_number);
        let _ = fs_ntfs::read::read_stat(&mut io, record_number);
    }

    // Path resolution walks the root directory's index, which is the
    // structure a lookup actually touches.
    for path in ["/", "/$MFT", "/nowhere", "/a/b/c"] {
        let _ = fs_ntfs::read::resolve_path(&mut io, path);
    }
}
