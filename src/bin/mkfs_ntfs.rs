//! mkfs.ntfs — standalone CLI for creating fresh NTFS filesystems.
//!
//! CLI-compatible subset of ntfs-3g's `mkntfs`. Same flag names and the
//! same positional `device` argument so existing scripts and CI
//! pipelines work against this binary unchanged. Note: ntfs-3g's mkntfs
//! is GPL — we are NOT a derived work; this is an independent
//! implementation built on top of our pure-Rust crate. Drop-in-compatible
//! at the CLI surface, separate at the source level.
//!
//! Cross-platform: pure Rust, std-only I/O. Builds and runs identically
//! on Linux, macOS, Windows. The same `format_filesystem()` entry point
//! is also called by the DiskJockey FSKit extension's `startFormat`, so
//! "format an SD card from the GUI" and "format a disk image from this
//! CLI" exercise the exact same code path.
//!
//! Convention follows mkntfs: the device/file MUST already exist at the
//! target size. Use `truncate -s 256M out.img` (Linux/macOS) or
//! `fsutil file createnew out.img 268435456` (Windows) to pre-create an
//! image, then `mkfs.ntfs out.img` formats it.
//!
//! Exit codes: 0 success, 1 failure.

use fs_ntfs::block_io::PathIo;
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: mkfs.ntfs [options] device

Options:
  -L, --label <label>      Volume label (max 32 UTF-16 code units after encode).
  -c, --cluster-size <n>   Cluster size in bytes. Power of 2, 512..=65536.
                           Default: 4096.
  --mft-record-size <n>    MFT record size in bytes. Power of 2, 512..=16384.
                           Default: 4096.
  --serial <hex>           NTFS volume serial number (16 hex chars). Default:
                           random.
  -Q, --quick              Quick format. (Accepted; we always do a quick
                           format — the on-disk layout we write is the
                           equivalent of mkntfs --quick.)
  -f, --fast               Fast format alias for -Q.
  -F, --force              Format even if device looks in use. (Accepted; we
                           do not currently inspect for active mounts.)
  -n                       Dry-run: parse args + open device but do not write.
  -q, --quiet              Suppress non-error output.
  -V, --version            Print version and exit.
  -h, --help               Print this help and exit.

Positional:
  device                   Path to a block device or pre-sized regular file.
                           The file/device MUST already exist at the target
                           size. Pre-create with
                             truncate -s 256M out.img    (Linux/macOS)
                             fsutil file createnew out.img 268435456 (Windows)

Unsupported flags from ntfs-3g's mkntfs (-C compression, -I disable index,
-z mft-zone-multiplier, etc.) are accepted with a warning if they take an
argument we can ignore safely, and rejected as errors otherwise. The full
feature set will land incrementally as the underlying crate grows.
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("mkfs.ntfs: {msg}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default)]
struct Opts {
    label: Option<String>,
    cluster_size: Option<u32>,
    mft_record_size: Option<u32>,
    serial: Option<u64>,
    force: bool,
    quick: bool,
    dry_run: bool,
    quiet: bool,
    device: Option<String>,
}

fn run() -> Result<(), String> {
    let opts = parse_args()?;
    let device = opts
        .device
        .as_deref()
        .ok_or_else(|| format!("missing positional <device> argument\n\n{USAGE}"))?;

    let cluster_size = opts.cluster_size.unwrap_or(4096);
    let mft_record_size = opts.mft_record_size.unwrap_or(4096);

    // PathIo::open_rw fails fast on missing path / no write permission.
    // We don't separately stat — PathIo caches the size at open time.
    let mut dev =
        PathIo::open_rw(Path::new(device)).map_err(|e| format!("open {device} read-write: {e}"))?;

    // Read size via the BlockIo trait so we go through the same surface
    // format_filesystem() will use; rules out "open succeeded but
    // size-reporting disagrees" surprises.
    let size = {
        use fs_ntfs::block_io::BlockIo;
        dev.size()
    };
    if size == 0 {
        return Err(format!(
            "device {device} reports size 0 — pre-create with truncate / fsutil first"
        ));
    }

    if !opts.quiet {
        eprintln!(
            "mkfs.ntfs: formatting {device} ({size} bytes, cluster_size={cluster_size}, mft_record_size={mft_record_size}{}{})",
            if opts.quick { ", quick" } else { "" },
            if opts.dry_run { ", dry-run" } else { "" }
        );
    }

    if opts.dry_run {
        if !opts.quiet {
            eprintln!("mkfs.ntfs: dry-run — no writes performed");
        }
        // Suppress unused warnings for fields we accept-but-don't-use yet.
        let _ = (opts.force, opts.quick);
        return Ok(());
    }

    format_filesystem(
        &mut dev,
        size,
        cluster_size,
        mft_record_size,
        opts.label.as_deref(),
        opts.serial,
    )
    .map_err(|e| format!("format failed: {e}"))?;

    {
        use fs_ntfs::block_io::BlockIo;
        dev.sync().map_err(|e| format!("fsync failed: {e}"))?;
    }

    if !opts.quiet {
        eprintln!("mkfs.ntfs: {device} formatted successfully");
    }
    Ok(())
}

/// Hand-rolled CLI parser. Same reasoning as the ext4 binary — pulling
/// in clap doubles the binary size for ten flags.
fn parse_args() -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("mkfs.ntfs (fs-ntfs) {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-L" | "--label" => {
                let v = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a label argument"))?;
                opts.label = Some(v);
            }
            "-c" | "--cluster-size" => {
                let v = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a cluster size argument"))?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("{arg}: not a valid number: {v}"))?;
                opts.cluster_size = Some(n);
            }
            "--mft-record-size" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--mft-record-size requires a value".to_string())?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("--mft-record-size: not a valid number: {v}"))?;
                opts.mft_record_size = Some(n);
            }
            "--serial" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--serial requires a hex value".to_string())?;
                opts.serial = Some(parse_hex_u64(&v)?);
            }
            "-Q" | "--quick" | "-f" | "--fast" => opts.quick = true,
            "-F" | "--force" => opts.force = true,
            "-n" => opts.dry_run = true,
            "-q" | "--quiet" => opts.quiet = true,
            // Accepted-but-ignored mkntfs flags, each takes one argument.
            // Warn so users know the value didn't take effect, but don't
            // fail — keeps existing scripts portable.
            "-C"
            | "--compress"
            | "-I"
            | "--no-indexing"
            | "-z"
            | "--mft-zone-multiplier"
            | "-T"
            | "--zero-time" => {
                // Some of these are flag-only in mkntfs (no arg), but the
                // safe behaviour is to accept and warn either way. We do
                // not consume a follow-on arg here because misclassifying
                // the next positional as an arg-of-this-flag would lose
                // the device path silently.
                if !opts.quiet {
                    eprintln!("mkfs.ntfs: warning: {arg} not yet honored, ignoring");
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}\n\n{USAGE}"));
            }
            // First non-flag positional is the device path. Reject
            // duplicates because mkfs.ntfs only formats one target per
            // invocation.
            _ => {
                if opts.device.is_some() {
                    return Err(format!(
                        "extra positional argument: {arg} (only one device may be given)"
                    ));
                }
                opts.device = Some(arg);
            }
        }
    }

    Ok(opts)
}

fn parse_hex_u64(s: &str) -> Result<u64, String> {
    let cleaned = s.trim_start_matches("0x").trim_start_matches("0X");
    if cleaned.is_empty() || cleaned.len() > 16 {
        return Err(format!(
            "serial must be 1..=16 hex chars (with optional 0x prefix), got {} chars",
            cleaned.len()
        ));
    }
    u64::from_str_radix(cleaned, 16).map_err(|_| format!("serial has non-hex character: {s}"))
}
