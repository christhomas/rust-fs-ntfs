//! Every verb the CLI ships, end to end: exit code, stdout, stderr.
//!
//! Of the crate's integration tests exactly one spawned the binary
//! before this file, and it drove `format`'s happy path only. Eleven of
//! the twelve verbs had no end-to-end coverage and nothing anywhere
//! asserted an exit code, a usage error or `--help` (#187). The audit
//! calls this the highest-leverage file it could add, for a reason worth
//! restating: the CLI's documented behaviour cannot be corrected safely
//! while nothing checks what it currently does.
//!
//! WHAT THIS FILE IS FOR is the contract a script sees: status, and what
//! comes back on each stream. It is deliberately not a filesystem test --
//! the volume's contents are checked by the suites that own them.

mod common;

use std::path::Path;
use std::process::{Command, Output};

const EXE: &str = env!("CARGO_BIN_EXE_rust-ntfs");
const VOL: u64 = 16 * 1024 * 1024;

fn run(args: &[&str]) -> Output {
    Command::new(EXE)
        .args(args)
        .output()
        .expect("spawn rust-ntfs")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("the process exited normally")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A formatted image with one file and one directory in it.
fn volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let img = common::temp_image_path(format!("cli_{tag}"));
    let f = std::fs::File::create(&img).expect("create image");
    f.set_len(VOL).expect("set_len");
    drop(f);
    assert_eq!(code(&run(&["format", "-L", "CLI", &img])), 0, "format");
    assert_eq!(code(&run(&["touch", &img, "/", "f.txt"])), 0, "touch");
    assert_eq!(code(&run(&["mkdir", &img, "/", "d"])), 0, "mkdir");
    img
}

/// EVERY VERB ANSWERS `--help` ON STDOUT WITH STATUS 0. A `--help` that
/// exits non-zero, or prints to stderr, breaks `cmd --help | less` and
/// every wrapper that checks the status.
#[test]
fn every_verb_documents_itself_on_stdout() {
    for verb in [
        "format",
        "ls",
        "touch",
        "mkdir",
        "write",
        "rm",
        "rmdir",
        "link",
        "rename",
        "remove",
        "set-dirty",
        "sparse",
    ] {
        let out = run(&[verb, "--help"]);
        assert_eq!(code(&out), 0, "`{verb} --help` exit status");
        assert!(
            !stdout(&out).is_empty(),
            "`{verb} --help` printed nothing to stdout"
        );
        assert!(
            stdout(&out).contains(verb) || stdout(&out).to_lowercase().contains("usage"),
            "`{verb} --help` does not look like usage text: {}",
            stdout(&out)
        );
    }
}

/// The two global flags, and the two shapes of "you have not given me a
/// command": both are usage errors, and a usage error is status 2.
#[test]
fn the_global_flags_and_the_usage_errors() {
    let version = run(&["--version"]);
    assert_eq!(code(&version), 0);
    assert!(
        stdout(&version).starts_with("rust-ntfs "),
        "--version prints the program and its version: {}",
        stdout(&version)
    );

    let help = run(&["--help"]);
    assert_eq!(code(&help), 0);
    assert!(stdout(&help).contains("Subcommands:"));

    let bare = run(&[]);
    assert_eq!(code(&bare), 2, "no subcommand is a usage error");
    let unknown = run(&["definitely-not-a-verb"]);
    assert_eq!(code(&unknown), 2, "an unknown subcommand is a usage error");
    assert!(
        !stderr(&unknown).is_empty(),
        "an unknown subcommand says so on stderr"
    );
}

/// THE WORKING VERBS, each on a real volume, each checked by status.
#[test]
fn the_verbs_that_change_a_volume_report_success() {
    let img = volume("happy");

    assert_eq!(code(&run(&["write", &img, "/f.txt", "--content", "hi"])), 0);
    assert_eq!(code(&run(&["link", &img, "/f.txt", "/", "alias.txt"])), 0);
    assert_eq!(code(&run(&["rename", &img, "/alias.txt", "alias2.txt"])), 0);
    assert_eq!(code(&run(&["rm", &img, "/alias2.txt"])), 0);
    assert_eq!(code(&run(&["rmdir", &img, "/d"])), 0);
    assert_eq!(code(&run(&["set-dirty", &img])), 0);

    let listing = run(&["ls", &img]);
    assert_eq!(code(&listing), 0);
    assert!(
        stdout(&listing).contains("f.txt"),
        "ls prints the volume's files on stdout: {}",
        stdout(&listing)
    );
}

/// A VERB THAT CANNOT DO WHAT IT WAS ASKED FAILS, and says why on
/// stderr rather than on stdout. Anything that exits 0 here is a script
/// silently carrying on from a failure.
#[test]
fn the_verbs_fail_when_the_target_is_wrong() {
    let img = volume("sad");

    for (what, args) in [
        ("a file that is not there", vec!["rm", &img, "/nope.txt"]),
        (
            "a directory that is not there",
            vec!["rmdir", &img, "/nope"],
        ),
        ("rm on a directory", vec!["rm", &img, "/d"]),
        ("rmdir on a file", vec!["rmdir", &img, "/f.txt"]),
        (
            "a write to a file that is not there",
            vec!["write", &img, "/nope.txt", "--content", "x"],
        ),
        (
            "a link from a file that is not there",
            vec!["link", &img, "/nope.txt", "/", "l.txt"],
        ),
        ("ls of an image that is not there", vec!["ls", "/nope.img"]),
    ] {
        let out = run(&args);
        assert_ne!(code(&out), 0, "{what}: expected a non-zero exit");
        assert!(
            !stderr(&out).is_empty(),
            "{what}: a failure must say why on stderr"
        );
    }
}

/// `remove` dispatches by type where `rm` and `rmdir` refuse the other's.
#[test]
fn remove_takes_either_a_file_or_a_directory() {
    let img = volume("remove");
    assert_eq!(code(&run(&["remove", &img, "/f.txt"])), 0, "remove a file");
    assert_eq!(code(&run(&["remove", &img, "/d"])), 0, "remove a directory");
    assert_ne!(
        code(&run(&["remove", &img, "/f.txt"])),
        0,
        "removing it twice fails the second time"
    );
}

/// THE THREE EXIT CODES ARE THREE DIFFERENT THINGS, which is what the
/// documented contract promises and what a script needs: 2 for "you
/// typed it wrong", 1 for "the volume said no", 0 for done. Every verb
/// used to map its own usage error to 1 as well (#186).
#[test]
fn a_usage_error_and_a_failure_have_different_exit_codes() {
    let img = volume("codes");
    assert_eq!(
        code(&run(&["rm", &img, "-x"])),
        2,
        "an unknown flag is a usage error"
    );
    assert_eq!(
        code(&run(&["rm", &img])),
        2,
        "the wrong number of arguments is a usage error"
    );
    assert_eq!(
        code(&run(&["rm", &img, "/nope.txt"])),
        1,
        "a file that is not there is a failure, not a usage error"
    );
    assert_eq!(code(&run(&["rm", &img, "/f.txt"])), 0);
}

/// AN UNKNOWN FLAG IS NOT A FILENAME. Ten verbs counted positionals
/// only, so `rust-ntfs rm vol.img -x` tried to unlink a file NAMED `-x`
/// -- and on a volume that has one, a mistyped flag is a delete (#186).
#[test]
fn a_mistyped_flag_is_refused_rather_than_taken_as_a_path() {
    let img = volume("flagpath");
    // The file this would delete if the flag were read as a path.
    // `--` ends the options, which is how a legal dash-leading NTFS
    // name stays reachable from the tool that writes the volume.
    assert_eq!(
        code(&run(&["touch", &img, "/", "--", "-x"])),
        0,
        "create a file named `-x`, after `--`"
    );

    for args in [
        vec!["rm", &img, "-x"],
        vec!["rmdir", &img, "-x"],
        vec!["remove", &img, "-x"],
    ] {
        let out = run(&args);
        assert_eq!(code(&out), 2, "{args:?} must be a usage error");
        assert!(
            stderr(&out).contains("unknown flag"),
            "{args:?} says which flag it did not know: {}",
            stderr(&out)
        );
    }

    // It can still be removed, by saying so explicitly.
    assert_eq!(
        code(&run(&["rm", &img, "--", "/-x"])),
        0,
        "`--` reaches the file the bare flag could not"
    );

    // And nothing else went missing.
    let listing = run(&["ls", &img]);
    assert!(
        stdout(&listing).contains("f.txt"),
        "the ordinary file is untouched: {}",
        stdout(&listing)
    );
}

/// Every verb the binary dispatches is in its help. `sparse` writes to a
/// volume and was undiscoverable from the tool itself (#186).
#[test]
fn the_help_lists_every_verb_that_exists() {
    let help = stdout(&run(&["--help"]));
    for verb in [
        "format",
        "ls",
        "touch",
        "mkdir",
        "write",
        "sparse",
        "rm",
        "rmdir",
        "link",
        "rename",
        "remove",
        "set-dirty",
    ] {
        assert!(
            help.contains(verb),
            "`--help` does not mention `{verb}`:\n{help}"
        );
    }
}

/// A missing required argument is a usage error, not a crash and not a
/// silent success.
#[test]
fn a_missing_argument_is_a_usage_error() {
    let img = volume("args");
    for args in [
        vec!["touch", &img],
        vec!["mkdir", &img],
        vec!["write", &img],
        vec!["link", &img],
        vec!["rename", &img],
    ] {
        let out = run(&args);
        assert_ne!(
            code(&out),
            0,
            "{args:?}: a missing argument must not succeed"
        );
        assert!(
            !stderr(&out).is_empty(),
            "{args:?}: it must say what was missing"
        );
    }
    assert!(Path::new(&img).exists(), "none of that removed the image");
}
