//! `rust-ntfs rename` — rename a file or directory within its parent.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs rename <image> <old_path> <new_basename>

Renames the entry at `<old_path>` to `<new_basename>` within the same
parent directory. Fails if `<new_basename>` already exists.
";

pub fn run(args: Vec<String>) -> ExitCode {
    crate::cli::finish("rename", run_inner(args))
}

fn run_inner(args: Vec<String>) -> Result<(), crate::cli::CliError> {
    if crate::cli::asked_for_help(&args, USAGE) {
        return Ok(());
    }
    let args = crate::cli::positionals(&args, 3, USAGE)?;
    let image = &args[0];
    let old_path = &args[1];
    let new_basename = &args[2];
    let fs = Filesystem::mount_rw(image).map_err(|e| format!("mount {image}: {e}"))?;
    fs.rename(old_path, new_basename)
        .map_err(|e| format!("rename {old_path} -> {new_basename}: {e}"))?;
    println!("renamed {old_path} -> {new_basename}");
    Ok(())
}
