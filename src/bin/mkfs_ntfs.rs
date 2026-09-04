//! `mkfs.ntfs` — format a device or image as NTFS.
//!
//! The same formatter as `rust-ntfs format`, under the name the
//! filesystem tooling convention uses. `am-fs-ext4` ships `mkfs.ext4`
//! and `am-fs-erofs` ships `mkfs.erofs`; NTFS only offered the
//! subcommand, so a caller reaching for the obvious name found
//! nothing.
//!
//! SHARES THE IMPLEMENTATION RATHER THAN COPYING IT. `#[path]` pulls in
//! the very file the subcommand uses, so the two cannot drift: one
//! argument parser, one set of defaults, one set of error messages. A
//! second implementation of a formatter is exactly the kind of copy
//! that ends up quietly weaker than the original.
//!
//! The target name is `mkfs_ntfs` because cargo will not accept a dot
//! in one; the release job renames it to `mkfs.ntfs` when it packages,
//! so what you install has the conventional name.

use std::process::ExitCode;

#[path = "rust_ntfs/format.rs"]
mod format;

fn main() -> ExitCode {
    // Skip argv[0]: the shared parser expects the arguments that follow
    // the subcommand, and here there is no subcommand to skip past.
    let args: Vec<String> = std::env::args().skip(1).collect();
    format::run(args, "mkfs.ntfs")
}
