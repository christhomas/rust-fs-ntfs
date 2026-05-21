//! `rust-ntfs mkdir` — create a directory in an NTFS image.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs mkdir <image> <parent-dir> <basename>

Creates an empty directory `<basename>` under `<parent-dir>`.
";

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs mkdir: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<(), String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args.len() != 3 {
        return Err(format!(
            "expected exactly 3 arguments, got {}\n\n{USAGE}",
            args.len()
        ));
    }
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
