//! `rust-ntfs rmdir` — remove an empty directory from an NTFS image.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs rmdir <image> <path>

Removes an empty directory at `<path>`. Refuses to remove non-empty
directories.
";

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs rmdir: {msg}");
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
    let fs = Filesystem::mount(image).map_err(|e| format!("mount {image}: {e}"))?;
    fs.rmdir(path).map_err(|e| format!("rmdir {path}: {e}"))?;
    println!("removed {path}");
    Ok(())
}
