//! `rust-ntfs rmdir` — remove an empty directory from an NTFS image.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs rmdir <image> <path>

Removes an empty directory at `<path>`. Refuses to remove non-empty
directories.
";

pub fn run(args: Vec<String>) -> ExitCode {
    crate::cli::finish("rmdir", run_inner(args))
}

fn run_inner(args: Vec<String>) -> Result<(), crate::cli::CliError> {
    if crate::cli::asked_for_help(&args, USAGE) {
        return Ok(());
    }
    let args = crate::cli::positionals(&args, 2, USAGE)?;
    let image = &args[0];
    let path = &args[1];
    let fs = Filesystem::mount_rw(image).map_err(|e| format!("mount {image}: {e}"))?;
    fs.rmdir(path).map_err(|e| format!("rmdir {path}: {e}"))?;
    println!("removed {path}");
    Ok(())
}
