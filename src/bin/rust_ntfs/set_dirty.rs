//! `rust-ntfs set-dirty` — mark an NTFS volume as dirty.
//!
//! Inverse of fsck's `clear_dirty`. Intended for test scenarios that
//! need to exercise dirty-volume code paths in Windows (ntfs.sys's
//! mount checks, chkdsk's "needs full scan" branch).

use fs_ntfs::fsck;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs set-dirty <image>

Sets the VOLUME_IS_DIRTY (0x0001) flag in `$Volume`'s
`$VOLUME_INFORMATION` attribute. Idempotent: a volume that is already
dirty is left as-is and the command exits 0.
";

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs set-dirty: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<(), String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args.len() != 1 {
        return Err(format!(
            "expected exactly 1 argument <image>, got {}\n\n{USAGE}",
            args.len()
        ));
    }
    let image = &args[0];
    let changed = fsck::set_dirty(image).map_err(|e| format!("{image}: {e}"))?;
    if changed {
        println!("set VOLUME_IS_DIRTY on {image}");
    } else {
        println!("{image} was already dirty; no change");
    }
    Ok(())
}
