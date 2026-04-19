//! Bit-flipping fuzz tests (§6.3).
//!
//! Mutates random bytes in a real NTFS image and checks that the
//! driver reacts with clean errors rather than panicking. The
//! contract is: no panic on any input, however malformed.
//!
//! Not a property-based harness — deterministic seeds so CI produces
//! the same runs each time. For unbounded fuzzing, see the §5.2
//! `cargo-fuzz` targets.

use fs_ntfs::facade::Filesystem;
use std::panic::{catch_unwind, AssertUnwindSafe};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.state
    }
}

fn corrupt_copy(tag: &str, seed: u64, flips: usize) -> String {
    let dst = format!("test-disks/_corrupt_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    let mut bytes = std::fs::read(&dst).unwrap();
    let mut rng = Lcg::new(seed);
    // Leave the boot sector (first 512 bytes) mostly intact so the
    // initial parse usually succeeds; we want corruption in MFT-
    // reachable regions more often than not, but also some boot-sector
    // fuzz is interesting. Mix both.
    for _ in 0..flips {
        let pos = (rng.next() % bytes.len() as u64) as usize;
        let bit = (rng.next() % 8) as u8;
        bytes[pos] ^= 1 << bit;
    }
    std::fs::write(&dst, &bytes).unwrap();
    dst
}

fn assert_no_panic(img: &str) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        match Filesystem::mount(img) {
            Ok(fs) => {
                // Try a few read ops that shouldn't panic even on bad data.
                let _ = fs.volume_info();
                let _ = fs.is_dirty();
                let _ = fs.stat("/hello.txt");
                let _ = fs.read_dir("/");
                let mut buf = [0u8; 64];
                let _ = fs.read_file("/hello.txt", 0, &mut buf);
            }
            Err(_) => {
                // Mount failure is a fine outcome — just no panic.
            }
        }
    }));
    assert!(r.is_ok(), "driver panicked on corrupted image {img}");
}

#[test]
fn single_bit_flip_boot_region_does_not_panic() {
    // Flip 1 bit somewhere in the first 2KiB.
    let img = corrupt_copy("boot_1flip", 0x1111_2222_3333_4444, 1);
    // Override: truncate the seeded random position to fall in [0..2048)
    // by using a fresh copy + manual flip.
    let mut bytes = std::fs::read(&img).unwrap();
    bytes[1024] ^= 0x01;
    std::fs::write(&img, &bytes).unwrap();
    assert_no_panic(&img);
}

#[test]
fn random_flips_5_do_not_panic() {
    let img = corrupt_copy("r5", 0xDEAD_BEEF_CAFE_0005, 5);
    assert_no_panic(&img);
}

#[test]
fn random_flips_20_do_not_panic() {
    let img = corrupt_copy("r20", 0xDEAD_BEEF_CAFE_0020, 20);
    assert_no_panic(&img);
}

#[test]
fn random_flips_100_do_not_panic() {
    let img = corrupt_copy("r100", 0xDEAD_BEEF_CAFE_0100, 100);
    assert_no_panic(&img);
}

#[test]
fn random_flips_500_do_not_panic() {
    let img = corrupt_copy("r500", 0xDEAD_BEEF_CAFE_0500, 500);
    assert_no_panic(&img);
}

#[test]
fn wiped_first_sector_does_not_panic() {
    // Deliberately destroy the boot sector entirely.
    let dst = "test-disks/_corrupt_wiped_boot.img";
    std::fs::copy(BASIC_IMG, dst).unwrap();
    let mut bytes = std::fs::read(dst).unwrap();
    for byte in bytes.iter_mut().take(512) {
        *byte = 0;
    }
    std::fs::write(dst, &bytes).unwrap();
    assert_no_panic(dst);
}

#[test]
fn truncated_image_does_not_panic() {
    let dst = "test-disks/_corrupt_truncated.img";
    std::fs::copy(BASIC_IMG, dst).unwrap();
    let bytes = std::fs::read(dst).unwrap();
    // Truncate to 64 KiB.
    std::fs::write(dst, &bytes[..65536]).unwrap();
    assert_no_panic(dst);
}
