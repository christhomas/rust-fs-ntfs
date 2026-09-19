//! `rust-ntfs mkdir` — create a directory in an NTFS image.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs mkdir <image> <parent-dir> <basename>

Creates an empty directory `<basename>` under `<parent-dir>`.
";

pub fn run(args: Vec<String>) -> ExitCode {
    crate::cli::finish("mkdir", run_inner(args))
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
        .mkdir(parent, basename)
        .map_err(|e| format!("mkdir {parent}/{basename}: {e}"))?;
    println!("created dir rec={rec} {parent}/{basename}");
    Ok(())
}
