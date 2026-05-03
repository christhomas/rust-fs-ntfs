//! `rust-ntfs write` — write data to an existing file's `$DATA` stream.

use fs_ntfs::facade::Filesystem;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs write <image> <path> [data-source]

Writes bytes into the unnamed $DATA stream of an existing file.

Data-source flags (exactly one required):
  --content STR     Literal UTF-8 text (no trailing newline).
  --bytes N         Generate N bytes of data using --pattern (default zeros).
  --from FILE       Read bytes from FILE on the host filesystem.
  --pattern PAT     One of: zeros, ones, incrementing. Default: zeros.

Examples:
  rust-ntfs write vol.img /tiny.txt --content 'hello world'
  rust-ntfs write vol.img /docs/notes.bin --bytes 4096 --pattern incrementing
";

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs write: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<(), String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args.len() < 2 {
        return Err(format!("expected <image> <path> [...]\n\n{USAGE}"));
    }
    let mut iter = args.into_iter();
    let image = iter.next().unwrap();
    let path = iter.next().unwrap();
    let mut rest: Vec<String> = iter.collect();

    let data = parse_data_source(&mut rest)?;

    let fs = Filesystem::mount(&image).map_err(|e| format!("mount {image}: {e}"))?;
    let new_size = fs
        .write_file_contents(&path, &data)
        .map_err(|e| format!("write {path}: {e}"))?;
    println!("wrote {} bytes to {path} (new size {new_size})", data.len());
    Ok(())
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
