//! `rust-ntfs link` — create a hard link to an existing file.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs link <image> <existing_path> <new_parent> <new_basename>

Creates a new hard link `<new_parent>/<new_basename>` pointing at the same
file as `<existing_path>`. Refuses to hard-link directories.
";

pub fn run(args: Vec<String>) -> ExitCode {
    crate::cli::finish("link", run_inner(args))
}

fn run_inner(args: Vec<String>) -> Result<(), crate::cli::CliError> {
    if crate::cli::asked_for_help(&args, USAGE) {
        return Ok(());
    }
    let args = crate::cli::positionals(&args, 4, USAGE)?;
    let image = &args[0];
    let existing_path = &args[1];
    let new_parent = &args[2];
    let new_basename = &args[3];
    let fs = Filesystem::mount_rw(image).map_err(|e| format!("mount {image}: {e}"))?;
    fs.link(existing_path, new_parent, new_basename)
        .map_err(|e| format!("link {existing_path} -> {new_parent}/{new_basename}: {e}"))?;
    println!("linked {new_parent}/{new_basename} -> {existing_path}");
    Ok(())
}
