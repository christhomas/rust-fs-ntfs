//! `sectors_per_cluster` has two encodings, and the driver reads both.
//!
//! Values `0x01..=0x80` are a literal sector count — `0x80` is 128
//! sectors, 64 KiB at a 512-byte sector, and that is where the literal
//! range ends. Above `0x80` the byte is a signed binary exponent, which
//! is how Windows writes the 128 KiB to 2 MiB cluster sizes it has
//! formatted since Windows 10.
//!
//! Reading the exponent form as a literal does not fail. It yields a
//! cluster size that is wrong by a small, non-obvious factor and is not
//! even a power of two, and every offset the driver computes — the MFT's
//! own location, every VCN-to-LCN translation, `volume_bytes()` — is
//! that number times something off the disk. The reads land at
//! consistently wrong places and return whatever is there.
//!
//! `mkfs` cannot produce these volumes: `format_filesystem` caps
//! `cluster_size` at 65536, so its `sectors_per_cluster` never leaves
//! the literal range. These are volumes Windows formatted.

use fs_ntfs::mft_io::read_boot_params;
use std::path::Path;

/// A 512-byte boot sector with the geometry fields this test cares
/// about, written to its own file so the public path-based reader can
/// parse it.
fn boot_image(tag: &str, bytes_per_sector: u16, spc_raw: u8) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let path = format!("test-disks/_bootgeom_{tag}.img");
    let mut b = vec![0u8; 512];
    b[3..11].copy_from_slice(b"NTFS    ");
    b[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
    b[0x0D] = spc_raw;
    b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes()); // mft_lcn
    b[0x40] = (-10i8) as u8; // clusters_per_mft_record → 1024-byte records
    std::fs::write(&path, &b).expect("write boot image");
    path
}

fn cluster_size(tag: &str, bytes_per_sector: u16, spc_raw: u8) -> Result<u64, String> {
    let img = boot_image(tag, bytes_per_sector, spc_raw);
    read_boot_params(Path::new(&img)).map(|p| p.cluster_size)
}

#[test]
fn the_literal_range_is_unchanged() {
    for (spc, expected) in [(1u8, 512u64), (8, 4096), (0x40, 32768), (0x80, 65536)] {
        assert_eq!(
            cluster_size(&format!("lit{spc:02x}"), 512, spc),
            Ok(expected),
            "spc {spc:#04x} is a literal sector count"
        );
    }
    // A 4Kn drive with one sector per cluster.
    assert_eq!(cluster_size("lit4kn", 4096, 1), Ok(4096));
}

#[test]
fn the_exponent_form_gives_the_large_cluster_sizes() {
    // 0xF8 is -8: 2^8 = 256 sectors = 128 KiB. 0xF7 is -9: 512 sectors
    // = 256 KiB. 0xF4 is -12: 4096 sectors = 2 MiB, Windows' maximum.
    for (spc, expected) in [
        (0xF8u8, 128 * 1024u64),
        (0xF7, 256 * 1024),
        (0xF4, 2 * 1024 * 1024),
    ] {
        assert_eq!(
            cluster_size(&format!("exp{spc:02x}"), 512, spc),
            Ok(expected),
            "spc {spc:#04x} is a binary exponent, not a count of {}",
            spc as u64
        );
    }
}

#[test]
fn an_exponent_past_what_ntfs_defines_is_refused() {
    // 0x90 is -112. Read as a literal it is 144 sectors, giving a
    // 73728-byte "cluster"; read as an exponent it asks for 2^112.
    // Neither is a volume, so it has to be an error rather than a
    // number.
    let err = cluster_size("exp90", 512, 0x90).unwrap_err();
    assert!(
        err.contains("exponent"),
        "the refusal should say what it refused: {err}"
    );
}

#[test]
fn a_non_power_of_two_literal_is_refused() {
    // Every cluster size is a power of two, and the driver multiplies
    // this number into every offset it computes.
    let err = cluster_size("lit03", 512, 3).unwrap_err();
    assert!(err.contains("power of two"), "{err}");
    let err = cluster_size("lit00", 512, 0).unwrap_err();
    assert!(err.contains("power of two"), "{err}");
}
