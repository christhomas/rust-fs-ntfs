//! `rust-ntfs remove` — POSIX-style remove (dispatches by type).

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs remove <image> <path>

Removes `<path>`, dispatching by type: directories go through rmdir
(must be empty), regular files through unlink.
";

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs remove: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<(), String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args.len() != 2 {
        return Err(format!(
            "expected exactly 2 arguments, got {}\n\n{USAGE}",
            args.len()
        ));
    }
    let image = &args[0];
    let path = &args[1];
    let fs = Filesystem::mount_rw(image).map_err(|e| format!("mount {image}: {e}"))?;
    fs.remove(path).map_err(|e| format!("remove {path}: {e}"))?;
    println!("removed {path}");
    Ok(())
}
