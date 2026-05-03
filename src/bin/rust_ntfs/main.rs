//! rust-ntfs — unified NTFS CLI (format, list, write, delete).
//!
//! Subcommand dispatcher for the four operations the matrix runner and
//! one-off NTFS work both need:
//!
//!   format  Build a fresh NTFS volume on a pre-sized image / device.
//!   ls      Read-only recursive directory walk.
//!   touch   Create an empty file.
//!   mkdir   Create a directory.
//!   write   Write bytes to an existing file's unnamed `$DATA`.
//!   rm      Remove a regular file.
//!   rmdir   Remove an empty directory.
//!
//! Single binary so distribution (GitHub releases, Homebrew taps) ships
//! one artefact rather than four. Each subcommand lives in its own
//! module so the per-command help / argv parsing stays small.
//!
//! Exit codes: 0 success, 1 failure, 2 usage error.

mod format;
mod ls;
mod mkdir;
mod rm;
mod rmdir;
mod touch;
mod write;

use std::process::ExitCode;

const HELP: &str = "\
Usage: rust-ntfs <subcommand> [options]

Subcommands:
  format  Format a pre-sized device or image as NTFS.
  ls      Recursively list entries in an NTFS image.
  touch   Create an empty file.
  mkdir   Create a directory.
  write   Write bytes to an existing file's $DATA stream.
  rm      Remove a regular file.
  rmdir   Remove an empty directory.

Run `rust-ntfs <subcommand> --help` for per-subcommand options.

Global flags:
  -V, --version   Print version and exit.
  -h, --help      Print this help and exit.
";

fn main() -> ExitCode {
    let mut args = std::env::args();
    args.next(); // argv[0]
    let Some(subcmd) = args.next() else {
        print!("{HELP}");
        return ExitCode::from(2);
    };
    let rest: Vec<String> = args.collect();
    match subcmd.as_str() {
        "format" => format::run(rest),
        "ls" => ls::run(rest),
        "touch" => touch::run(rest),
        "mkdir" => mkdir::run(rest),
        "write" => write::run(rest),
        "rm" => rm::run(rest),
        "rmdir" => rmdir::run(rest),
        "-h" | "--help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        "-V" | "--version" => {
            println!("rust-ntfs {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("rust-ntfs: unknown subcommand: {other}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}
