//! `rust-ntfs remove` — POSIX-style remove (dispatches by type).

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs remove <image> <path>

Removes `<path>`, dispatching by type: directories go through rmdir
(must be empty), regular files through unlink.
";

pub fn run(args: Vec<String>) -> ExitCode {
    crate::cli::finish("remove", run_inner(args))
}

fn run_inner(args: Vec<String>) -> Result<(), crate::cli::CliError> {
    if crate::cli::asked_for_help(&args, USAGE) {
        return Ok(());
    }
    let args = crate::cli::positionals(&args, 2, USAGE)?;
    let image = &args[0];
    let path = &args[1];
    let fs = Filesystem::mount_rw(image).map_err(|e| format!("mount {image}: {e}"))?;
    fs.remove(path).map_err(|e| format!("remove {path}: {e}"))?;
    println!("removed {path}");
    Ok(())
}
