//! Debug probe: dump the on-disk attribute layout of a file's MFT record.
//!
//! Reconstruction note: the original `examples/probe_dbg.rs` was an
//! uncommitted scratch file that was lost before it could be captured.
//! This is a fresh, functionally-equivalent debug probe built on the
//! crate's public API — not a byte-for-byte recovery of the original.
//!
//! Usage:
//!   # Probe a path inside an existing image:
//!   cargo run --example probe_dbg -- <image.img> </path/to/file>
//!
//!   # No args: format a throwaway image, populate it, and probe it.
//!   cargo run --example probe_dbg
//!
//! It prints, for every attribute in the target's MFT record, the type
//! code + well-known name, attribute id, byte offset/length within the
//! record, residency, and value length — the view you want when chasing
//! "what attributes does this record actually carry?" during debugging.

use std::path::{Path, PathBuf};

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;

fn dump(image: &Path, file_path: &str) {
    match write::read_attributes(image, file_path) {
        Ok(descs) => {
            println!(
                "{} attribute(s) in MFT record for {file_path}:",
                descs.len()
            );
            println!(
                "  {:<24} {:>4} {:>8} {:>8} {:>9} {:>12}  name",
                "type", "id", "offset", "length", "resident", "value_len"
            );
            for d in &descs {
                println!(
                    "  {:<24} {:>4} {:>#8x} {:>8} {:>9} {:>12}  {}",
                    format!("{} (0x{:x})", d.type_name, d.type_code),
                    d.attribute_id,
                    d.attr_offset,
                    d.attr_length,
                    d.is_resident,
                    d.value_length,
                    d.name,
                );
            }
        }
        Err(e) => eprintln!("probe failed for {file_path}: {e}"),
    }
}

/// Format a throwaway image under the temp dir, create one file, and
/// return its path so the no-args path can demonstrate the probe.
fn make_demo_image() -> PathBuf {
    const SIZE: u64 = 16 * 1024 * 1024;
    let path = std::env::temp_dir().join("fs_ntfs_probe_dbg_demo.img");

    {
        let f = std::fs::File::create(&path).expect("create demo image");
        f.set_len(SIZE).expect("set_len");
    }
    {
        let mut io = PathIo::open_rw(&path).expect("open_rw demo image");
        format_filesystem(
            &mut io as &mut dyn BlockIo,
            SIZE,
            4096,
            4096,
            Some("PROBE"),
            Some(0x5052_4F42),
        )
        .expect("format demo image");
    }
    write::create_file(&path, "/", "probe.txt").expect("create probe.txt");
    path
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [image, file_path] => dump(Path::new(image), file_path),
        [] => {
            let img = make_demo_image();
            println!("(no args) formatted demo image at {}", img.display());
            dump(&img, "/probe.txt");
            let _ = std::fs::remove_file(&img);
        }
        _ => {
            eprintln!("usage: probe_dbg [<image.img> </path/to/file>]");
            std::process::exit(2);
        }
    }
}
