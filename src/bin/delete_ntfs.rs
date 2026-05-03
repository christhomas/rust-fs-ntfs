//! delete_ntfs — Mac-side delete helper for the multi-agent test matrix.
//!
//! Wraps `fs_ntfs::facade::Filesystem::unlink` and `rmdir` so
//! orchestrators can exercise the `mac:delete` operation without going
//! through the Windows VM. Required by scenarios in
//! `test-matrix.json` that contain a `mac:delete` step.
//!
//! Subcommands:
//!   unlink <image> <path>      remove a regular file
//!   rmdir  <image> <path>      remove an empty directory
//!
//! Exit codes: 0 success, 1 failure.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: delete_ntfs <subcommand> <image> <path>

Subcommands:
  unlink <image> <path>      Remove a regular file.
  rmdir  <image> <path>      Remove an empty directory.

Examples:
  delete_ntfs unlink vol.img /tiny.txt
  delete_ntfs rmdir  vol.img /docs
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("delete_ntfs: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let subcmd = args.remove(0);
    if args.len() < 2 {
        return Err(format!("{subcmd}: requires <image> <path>\n\n{USAGE}"));
    }
    let image = args.remove(0);
    let path = args.remove(0);
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount {image}: {e}"))?;
    match subcmd.as_str() {
        "unlink" => fs
            .unlink(&path)
            .map_err(|e| format!("unlink {path}: {e}"))?,
        "rmdir" => fs.rmdir(&path).map_err(|e| format!("rmdir {path}: {e}"))?,
        other => return Err(format!("unknown subcommand: {other}\n\n{USAGE}")),
    }
    println!("removed {path}");
    Ok(())
}
