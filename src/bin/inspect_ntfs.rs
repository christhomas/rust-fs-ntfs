//! inspect-ntfs — small CLI for the matrix test pipeline's mac:enumerate
//! and mac:write/delete legs. Wraps `Filesystem::{mount, read_dir, stat,
//! create_file, mkdir, unlink, rmdir}` with a stable text output format
//! the runner can grep.
//!
//! Cross-platform: pure Rust, std-only I/O.
//!
//! Exit codes: 0 success, 1 failure.

use fs_ntfs::facade::{FileType, Filesystem};
use std::process::ExitCode;

const USAGE: &str = "\
Usage:
  inspect-ntfs enumerate <image> [<path>]
  inspect-ntfs stat      <image> <path>
  inspect-ntfs cat       <image> <path>
  inspect-ntfs touch     <image> <path>
  inspect-ntfs mkdir     <image> <path>
  inspect-ntfs rm        <image> <path>
  inspect-ntfs rmdir     <image> <path>

Output format for `enumerate`:
  per-entry line: '<type>\\t<rec_num>\\t<name>'
                  type ∈ {dir, file, link, junction}
  trailer line:   'count=<N>'  (excludes . and ..)
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let rc = match cmd.as_str() {
        "enumerate" => cmd_enumerate(args),
        "stat" => cmd_stat(args),
        "cat" => cmd_cat(args),
        "touch" => cmd_touch(args),
        "mkdir" => cmd_mkdir(args),
        "rm" => cmd_rm(args),
        "rmdir" => cmd_rmdir(args),
        "-h" | "--help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("inspect-ntfs: unknown subcommand '{cmd}'\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match rc {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("inspect-ntfs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn next_arg<I: Iterator<Item = String>>(it: &mut I, name: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("missing <{name}>"))
}

fn cmd_enumerate<I: Iterator<Item = String>>(mut args: I) -> Result<(), String> {
    let image = next_arg(&mut args, "image")?;
    let path = args.next().unwrap_or_else(|| "/".to_string());
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount: {e}"))?;
    let entries = fs.read_dir(&path).map_err(|e| format!("read_dir: {e}"))?;
    let mut count = 0usize;
    for e in entries {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let kind = match e.file_type {
            FileType::Directory => "dir",
            FileType::Regular => "file",
            FileType::Symlink => "link",
            FileType::Junction => "junction",
            _ => "other",
        };
        println!("{}\t{}\t{}", kind, e.file_record_number, e.name);
        count += 1;
    }
    println!("count={count}");
    Ok(())
}

fn cmd_stat<I: Iterator<Item = String>>(mut args: I) -> Result<(), String> {
    let image = next_arg(&mut args, "image")?;
    let path = next_arg(&mut args, "path")?;
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount: {e}"))?;
    let a = fs.stat(&path).map_err(|e| format!("stat: {e}"))?;
    println!("rec_num={}", a.file_record_number);
    println!("size={}", a.size);
    println!("mode={:o}", a.mode);
    println!("link_count={}", a.link_count);
    println!(
        "type={}",
        match a.file_type {
            FileType::Directory => "dir",
            FileType::Regular => "file",
            FileType::Symlink => "link",
            FileType::Junction => "junction",
            _ => "other",
        }
    );
    Ok(())
}

fn cmd_cat<I: Iterator<Item = String>>(mut args: I) -> Result<(), String> {
    let image = next_arg(&mut args, "image")?;
    let path = next_arg(&mut args, "path")?;
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount: {e}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut offset = 0u64;
    let mut total = 0u64;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    use std::io::Write;
    loop {
        let n = fs
            .read_file(&path, offset, &mut buf)
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        handle
            .write_all(&buf[..n])
            .map_err(|e| format!("stdout: {e}"))?;
        offset += n as u64;
        total += n as u64;
    }
    eprintln!("bytes={total}");
    Ok(())
}

fn cmd_touch<I: Iterator<Item = String>>(mut args: I) -> Result<(), String> {
    let image = next_arg(&mut args, "image")?;
    let path = next_arg(&mut args, "path")?;
    let (parent, base) = split_path(&path)?;
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount: {e}"))?;
    let rec = fs
        .create_file(parent, base)
        .map_err(|e| format!("create: {e}"))?;
    println!("rec_num={rec}");
    Ok(())
}

fn cmd_mkdir<I: Iterator<Item = String>>(mut args: I) -> Result<(), String> {
    let image = next_arg(&mut args, "image")?;
    let path = next_arg(&mut args, "path")?;
    let (parent, base) = split_path(&path)?;
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount: {e}"))?;
    let rec = fs.mkdir(parent, base).map_err(|e| format!("mkdir: {e}"))?;
    println!("rec_num={rec}");
    Ok(())
}

fn cmd_rm<I: Iterator<Item = String>>(mut args: I) -> Result<(), String> {
    let image = next_arg(&mut args, "image")?;
    let path = next_arg(&mut args, "path")?;
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount: {e}"))?;
    fs.unlink(&path).map_err(|e| format!("unlink: {e}"))?;
    Ok(())
}

fn cmd_rmdir<I: Iterator<Item = String>>(mut args: I) -> Result<(), String> {
    let image = next_arg(&mut args, "image")?;
    let path = next_arg(&mut args, "path")?;
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount: {e}"))?;
    fs.rmdir(&path).map_err(|e| format!("rmdir: {e}"))?;
    Ok(())
}

/// Split `/foo/bar.txt` into (`/foo`, `bar.txt`); root path is `/`.
fn split_path(path: &str) -> Result<(&str, &str), String> {
    let (parent, base) = path
        .rsplit_once('/')
        .ok_or_else(|| format!("path '{path}' must contain at least one '/'"))?;
    let parent = if parent.is_empty() { "/" } else { parent };
    if base.is_empty() {
        return Err(format!("path '{path}' has empty basename"));
    }
    Ok((parent, base))
}
