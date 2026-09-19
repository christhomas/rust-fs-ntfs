//! `rust-ntfs touch` — create an empty file in an NTFS image.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs touch <image> <parent-dir> <basename>

Creates an empty file `<basename>` under `<parent-dir>`.
";

pub fn run(args: Vec<String>) -> ExitCode {
    crate::cli::finish("touch", run_inner(args))
}

fn run_inner(args: Vec<String>) -> Result<(), crate::cli::CliError> {
    if crate::cli::asked_for_help(&args, USAGE) {
        return Ok(());
    }
    let args = crate::cli::positionals(&args, 3, USAGE)?;
    let image = &args[0];
    let parent = &args[1];
    let basename = &args[2];
    let fs = Filesystem::mount_rw(image).map_err(|e| format!("mount {image}: {e}"))?;
    let rec = fs
        .create_file(parent, basename)
        .map_err(|e| format!("create {parent}/{basename}: {e}"))?;
    println!("created file rec={rec} {parent}/{basename}");
    Ok(())
}
