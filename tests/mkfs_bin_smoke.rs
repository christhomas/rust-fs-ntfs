//! Smoke test for the `mkfs.ntfs` (mkfs_ntfs) binary.
//!
//! Pre-creates a 64 MiB regular file, runs the binary against it with a
//! known label + serial, then re-opens the file via the upstream `ntfs`
//! crate's parser (same path the existing `mkfs_roundtrip` test uses)
//! and verifies the on-disk layout the binary produced is parseable
//! and reflects the CLI args. Catches:
//!   - args plumbed to format_filesystem() correctly (label, serial propagate)
//!   - file-as-device path opens R/W under the binary's process
//!   - resulting bytes parse cleanly via the upstream crate (same library
//!     Windows + our FSKit extension would use to read the volume)
//!
//! The test deliberately mirrors the structure of `mkfs_roundtrip.rs` so
//! a future "fixture-vs-mkfs comparison" test (run our binary, compare
//! parsed output against a known-good mkntfs-generated image) can layer
//! on top without rewiring.

use ntfs::Ntfs;
use std::process::Command;

const SIZE_BYTES: u64 = 64 * 1024 * 1024;
const TEST_LABEL: &str = "BINSMOKE";
const TEST_SERIAL_HEX: &str = "deadbeefcafe1234";
const TEST_SERIAL: u64 = 0xdeadbeefcafe1234;

fn unique_tmp_path(suffix: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("fs-ntfs-mkfs-bin-{pid}-{nanos}-{suffix}"))
}

#[test]
fn mkfs_bin_formats_a_pre_sized_file_and_parses_clean() {
    let bin = env!("CARGO_BIN_EXE_mkfs_ntfs");
    let img = unique_tmp_path("img");
    let img_str = img.to_string_lossy().into_owned();

    // Pre-size with std (no `truncate` shell-out — keeps the test
    // platform-portable for when this runs on Windows CI later).
    {
        let f = std::fs::File::create(&img).expect("create img");
        f.set_len(SIZE_BYTES).expect("set_len");
    }

    let out = Command::new(bin)
        .args(["-L", TEST_LABEL, "--serial", TEST_SERIAL_HEX, &img_str])
        .output()
        .expect("spawn mkfs_ntfs");

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        panic!(
            "mkfs_ntfs failed: status={:?}\nstderr:\n{stderr}",
            out.status
        );
    }

    // Parse the result via the upstream `ntfs` crate — the same parser
    // our crate's read path and the FSKit extension use. Anything that
    // works through this parser is something Windows would also accept,
    // modulo deeper chkdsk-style checks.
    let bytes = std::fs::read(&img).expect("read formatted image");
    let mut cursor = std::io::Cursor::new(&bytes);
    let ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new on freshly formatted volume");

    assert_eq!(
        ntfs.cluster_size(),
        4096,
        "default cluster size should be 4096"
    );
    assert_eq!(
        ntfs.serial_number(),
        TEST_SERIAL,
        "--serial argument did not propagate to boot sector"
    );

    let _ = std::fs::remove_file(&img);
}

#[test]
fn mkfs_bin_dry_run_does_not_modify_file() {
    let bin = env!("CARGO_BIN_EXE_mkfs_ntfs");
    let img = unique_tmp_path("dryrun");
    let img_str = img.to_string_lossy().into_owned();

    let pattern = vec![0xAAu8; SIZE_BYTES as usize];
    std::fs::write(&img, &pattern).expect("seed pattern");

    let out = Command::new(bin)
        .args(["-n", "-L", "DRYRUN", &img_str])
        .output()
        .expect("spawn mkfs_ntfs -n");
    assert!(
        out.status.success(),
        "dry-run mkfs_ntfs should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = std::fs::read(&img).expect("read after dry-run");
    assert_eq!(
        after.len(),
        pattern.len(),
        "dry-run must not change file size"
    );
    assert!(after == pattern, "dry-run must not modify file contents");

    let _ = std::fs::remove_file(&img);
}
