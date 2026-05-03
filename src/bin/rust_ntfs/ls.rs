//! `rust-ntfs ls` — recursive read-only enumerate of an NTFS image.
//!
//! Used by the multi-agent test matrix's `mac:enumerate` operation:
//! after a format / write, this walks the volume and prints a sorted
//! line-per-file listing the harness can `diff` against
//! `Get-ChildItem` output from the Windows side.

use fs_ntfs::facade::{FileType, Filesystem};
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs ls [options] <image>

Options:
  -p, --path <p>   Subtree root to enumerate (default: /).
  -t, --type       Print 'd' or 'f' before each name.
  -h, --help       Print this help and exit.

Output: one path per line, sorted, depth-first. Subdirectories
descended in collation order. Skips '.' and '..'.
";

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs ls: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<(), String> {
    let mut image: Option<String> = None;
    let mut start_path = "/".to_string();
    let mut show_type = false;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "-p" | "--path" => {
                start_path = iter
                    .next()
                    .ok_or_else(|| format!("{a} requires a path argument"))?;
            }
            "-t" | "--type" => show_type = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}\n\n{USAGE}"));
            }
            _ => {
                if image.is_some() {
                    return Err("only one image argument allowed".to_string());
                }
                image = Some(a);
            }
        }
    }
    let image = image.ok_or_else(|| format!("missing <image> argument\n\n{USAGE}"))?;

    let fs = Filesystem::mount(&image).map_err(|e| format!("mount {image}: {e}"))?;

    let mut stack: Vec<String> = vec![start_path];
    while let Some(dir) = stack.pop() {
        let entries = fs
            .read_dir(&dir)
            .map_err(|e| format!("read_dir {dir}: {e}"))?;
        let mut names: Vec<_> = entries
            .into_iter()
            .filter(|e| e.name != "." && e.name != "..")
            .collect();
        names.sort_by(|a, b| a.name.cmp(&b.name));
        for e in names.iter().rev() {
            if e.file_type == FileType::Directory {
                let child = if dir == "/" {
                    format!("/{}", e.name)
                } else {
                    format!("{}/{}", dir, e.name)
                };
                stack.push(child);
            }
        }
        for e in &names {
            let full = if dir == "/" {
                format!("/{}", e.name)
            } else {
                format!("{}/{}", dir, e.name)
            };
            if show_type {
                let t = match e.file_type {
                    FileType::Directory => 'd',
                    FileType::Regular => 'f',
                    FileType::Symlink => 'l',
                    FileType::Junction => 'j',
                    FileType::Other => '?',
                };
                println!("{t} {full}");
            } else {
                println!("{full}");
            }
        }
    }
    Ok(())
}
