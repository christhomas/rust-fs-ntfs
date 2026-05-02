//! write_ntfs — Mac-side write helper for the multi-agent test matrix.
//!
//! Wraps `fs_ntfs::facade::Filesystem`'s mutation API into a tiny CLI so
//! orchestrators can populate a freshly-formatted image without going
//! through the Windows VM. Required by scenarios in
//! `tests/matrix/work-list.json` that contain a `mac:write` operation.
//!
//! Subcommands:
//!   create   <image> <parent> <basename>          create empty file
//!   mkdir    <image> <parent> <basename>          create directory
//!   write    <image> <path> [--content STR | --bytes N | --from FILE | --pattern incrementing|zeros|ones]
//!                                                 write data to the file
//!
//! Exit codes: 0 success, 1 failure.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: write_ntfs <subcommand> <image> [args...]

Subcommands:
  create  <image> <parent-dir> <basename>            Create an empty file.
  mkdir   <image> <parent-dir> <basename>            Create a directory.
  write   <image> <path> [data-source]               Write bytes into the unnamed
                                                     $DATA stream of an existing file.

Data-source flags (write subcommand):
  --content STR     Literal UTF-8 text (no trailing newline).
  --bytes N         Generate N bytes of data using --pattern (default zeros).
  --from FILE       Read bytes from FILE on the host filesystem.
  --pattern PAT     One of: zeros, ones, incrementing. Default: zeros.

Examples:
  write_ntfs create  vol.img / tiny.txt
  write_ntfs write   vol.img /tiny.txt --content 'hello world'
  write_ntfs mkdir   vol.img / docs
  write_ntfs create  vol.img /docs notes.bin
  write_ntfs write   vol.img /docs/notes.bin --bytes 4096 --pattern incrementing
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("write_ntfs: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let subcmd = args.remove(0);
    if args.is_empty() {
        return Err(format!("{subcmd}: missing <image> argument\n\n{USAGE}"));
    }
    let image = args.remove(0);
    let fs = Filesystem::mount(&image).map_err(|e| format!("mount {image}: {e}"))?;

    match subcmd.as_str() {
        "create" => {
            let parent = next_pos(&mut args, "create", "parent-dir")?;
            let basename = next_pos(&mut args, "create", "basename")?;
            let rec = fs
                .create_file(&parent, &basename)
                .map_err(|e| format!("create {parent}/{basename}: {e}"))?;
            println!("created file rec={rec} {parent}/{basename}");
        }
        "mkdir" => {
            let parent = next_pos(&mut args, "mkdir", "parent-dir")?;
            let basename = next_pos(&mut args, "mkdir", "basename")?;
            let rec = fs
                .mkdir(&parent, &basename)
                .map_err(|e| format!("mkdir {parent}/{basename}: {e}"))?;
            println!("created dir rec={rec} {parent}/{basename}");
        }
        "write" => {
            let path = next_pos(&mut args, "write", "path")?;
            let data = parse_data_source(&mut args)?;
            let new_size = fs
                .write_file_contents(&path, &data)
                .map_err(|e| format!("write {path}: {e}"))?;
            println!("wrote {} bytes to {path} (new size {new_size})", data.len());
        }
        other => {
            return Err(format!("unknown subcommand: {other}\n\n{USAGE}"));
        }
    }
    Ok(())
}

fn next_pos(args: &mut Vec<String>, sub: &str, name: &str) -> Result<String, String> {
    if args.is_empty() {
        return Err(format!("{sub}: missing <{name}> argument"));
    }
    Ok(args.remove(0))
}

fn parse_data_source(args: &mut Vec<String>) -> Result<Vec<u8>, String> {
    let mut content: Option<String> = None;
    let mut bytes: Option<u64> = None;
    let mut from_file: Option<String> = None;
    let mut pattern = String::from("zeros");
    while !args.is_empty() {
        let flag = args.remove(0);
        match flag.as_str() {
            "--content" => {
                content = Some(
                    args.first()
                        .ok_or_else(|| "--content requires a string".to_string())?
                        .clone(),
                );
                args.remove(0);
            }
            "--bytes" => {
                let v = args
                    .first()
                    .ok_or_else(|| "--bytes requires a number".to_string())?
                    .clone();
                args.remove(0);
                bytes = Some(v.parse().map_err(|_| format!("--bytes: bad number {v}"))?);
            }
            "--from" => {
                from_file = Some(
                    args.first()
                        .ok_or_else(|| "--from requires a path".to_string())?
                        .clone(),
                );
                args.remove(0);
            }
            "--pattern" => {
                pattern = args
                    .first()
                    .ok_or_else(|| "--pattern requires a value".to_string())?
                    .clone();
                args.remove(0);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if let Some(s) = content {
        return Ok(s.into_bytes());
    }
    if let Some(p) = from_file {
        return std::fs::read(&p).map_err(|e| format!("--from {p}: {e}"));
    }
    if let Some(n) = bytes {
        let n = n as usize;
        return Ok(match pattern.as_str() {
            "zeros" => vec![0u8; n],
            "ones" => vec![0xFFu8; n],
            "incrementing" => (0..n).map(|i| (i & 0xFF) as u8).collect(),
            other => return Err(format!("unknown --pattern: {other}")),
        });
    }
    Err("write: provide one of --content / --bytes / --from".to_string())
}
