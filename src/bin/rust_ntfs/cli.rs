//! The wrapper every verb shares: `--help`, the usage-error exit code,
//! and the rule that an unrecognised flag is never a path.
//!
//! WHY THIS EXISTS AS ONE FILE. `main.rs` documents "0 success, 1
//! failure, 2 usage error", and only the top-level dispatcher ever
//! returned 2: each of the twelve verb modules carried its own copy of
//! the same wrapper and mapped every error, including its own usage
//! error, to `FAILURE`. So a script could not tell "you typed it wrong"
//! from "the volume is corrupt", which is the whole purpose of a
//! separate usage code (#186).
//!
//! AND AN UNKNOWN FLAG WAS A PATH. Ten of the twelve verbs only counted
//! positionals, so `rust-ntfs rm vol.img -x` tried to unlink a file
//! NAMED `-x`. On a volume that has one, a mistyped flag is a delete.
//! The same shape covered `rmdir`, `remove`, `touch`, `mkdir`, `link`
//! and `rename` -- the destructive half of the verb set.

use std::process::ExitCode;

/// What went wrong, which decides the exit code.
pub enum CliError {
    /// The command was malformed: wrong arity, an unknown flag, a bad
    /// value. Exit code 2, so a caller can tell it from a failure.
    Usage(String),
    /// The command was understood and could not be carried out. Exit 1.
    Failed(String),
}

impl From<String> for CliError {
    /// Anything a verb's body returns is a failure unless it says
    /// otherwise -- the volume, not the command line.
    fn from(msg: String) -> Self {
        CliError::Failed(msg)
    }
}

/// Exit code 2, as `main.rs` documents.
const USAGE_EXIT: u8 = 2;

/// Run one verb's body and turn its result into an exit code.
pub fn finish(verb: &str, result: Result<(), CliError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(msg)) => {
            eprintln!("rust-ntfs {verb}: {msg}");
            ExitCode::from(USAGE_EXIT)
        }
        Err(CliError::Failed(msg)) => {
            eprintln!("rust-ntfs {verb}: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// `true` when the caller asked for help, having printed it.
pub fn asked_for_help(args: &[String], usage: &str) -> bool {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{usage}");
        return true;
    }
    false
}

/// The positional arguments, refusing anything that looks like a flag.
///
/// A verb with no flags of its own has no reason to accept one, and
/// every reason not to: taking `-x` as a path is how a mistyped flag
/// becomes a deleted file.
/// `--` ENDS THE OPTIONS, as it does everywhere else. Refusing
/// dash-leading arguments would otherwise make a legal NTFS name
/// unreachable: `-x` is a perfectly good filename, and the tool that
/// writes the volume should be able to write it. `rust-ntfs touch img /
/// -- -x` creates it; `rust-ntfs rm img -x` is still a usage error.
pub fn positionals(args: &[String], expected: usize, usage: &str) -> Result<Vec<String>, CliError> {
    let (scan, rest): (&[String], &[String]) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[]),
    };
    if let Some(flag) = scan.iter().find(|a| a.starts_with('-') && a.len() > 1) {
        return Err(CliError::Usage(format!(
            "unknown flag {flag:?}; this verb takes {expected} argument(s) and no flags. \
             If that is a filename, put it after `--`.\n\n{usage}"
        )));
    }
    let mut positional: Vec<String> = scan.to_vec();
    positional.extend_from_slice(rest);
    if positional.len() != expected {
        return Err(CliError::Usage(format!(
            "expected exactly {expected} arguments, got {}\n\n{usage}",
            positional.len()
        )));
    }
    Ok(positional)
}
