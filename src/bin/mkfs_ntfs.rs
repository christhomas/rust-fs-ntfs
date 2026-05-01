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
  --create-size <SIZE>     DiskJockey extension (not in mkntfs): if device
                           doesn't exist, create it as a regular file of the
                           given size first. SIZE accepts K/M/G/T suffixes
                           (1024-based). Refuses to apply to existing block
                           devices — only valid for image files. Use when
                           scripting test pipelines so you don't have to chain
                           truncate + mkfs.ntfs. Without this flag the tool
                           follows mkntfs convention (file must pre-exist).
  -V, --version            Print version and exit.
  -h, --help               Print this help and exit.

Positional:
  device                   Path to a block device or pre-sized regular file.
                           The file/device MUST already exist at the target
                           size unless --create-size is given. Pre-create with
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
    /// Bytes from `--create-size <SIZE>`. Same semantics as the ext4
    /// binary's flag: when `Some(n)` and the device path doesn't
    /// exist, create it as a regular file of `n` bytes before
    /// formatting. Refuses on real block/char devices.
    create_size: Option<u64>,
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

    // --create-size handling. Mirror of the mkfs_ext4 binary's flag —
    // see that file for the doc'd contract. Three cases: existing
    // file (idempotent), block/char device (refuse), missing path
    // (create + size). Refusing on block devices is the safety net
    // against typo-like errors (`/dev/diskN` instead of an image
    // path).
    if let Some(n) = opts.create_size {
        match std::fs::metadata(device) {
            Ok(meta) => {
                let ft = meta.file_type();
                // Block/char-device check is Unix-only — Windows
                // doesn't expose /dev/diskN-style raw devices through
                // std::fs at all (block-level access goes via different
                // APIs there). On Windows the safety guard is just
                // "must be a regular file"; on Unix we additionally
                // refuse if the path is a real block/char device, so
                // a typo'd `--create-size 32M /dev/disk5` doesn't
                // sail through.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if ft.is_block_device() || ft.is_char_device() {
                        return Err(format!(
                            "--create-size refuses to apply to {device}: looks like a real block/char device, \
                             not a regular file. Did you mean to leave --create-size off?"
                        ));
                    }
                }
                if !ft.is_file() {
                    return Err(format!(
                        "--create-size: {device} exists but is not a regular file"
                    ));
                }
                if !opts.quiet {
                    eprintln!(
                        "mkfs.ntfs: --create-size: {device} already exists ({} bytes); leaving as-is",
                        meta.len()
                    );
                }
            }
            Err(_) => {
                let f = std::fs::File::create(device)
                    .map_err(|e| format!("--create-size: create {device}: {e}"))?;
                f.set_len(n)
                    .map_err(|e| format!("--create-size: set_len({n}) on {device}: {e}"))?;
                drop(f);
                if !opts.quiet {
                    eprintln!("mkfs.ntfs: --create-size: created {device} ({n} bytes)");
                }
            }
        }
    }

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
            "--create-size" => {
                let v = args.next().ok_or_else(|| {
                    "--create-size requires a SIZE argument (e.g. 256M)".to_string()
                })?;
                opts.create_size = Some(parse_size(&v)?);
            }
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

/// Parse a size like "64M" / "1G" / "1024K" / "33554432" into bytes.
/// 1024-based multipliers (K/M/G/T), case-insensitive, optional 'B'
/// suffix tolerated. Bare numbers are bytes. Same convention as
/// `truncate -s` and most disk-image tools. Mirror of the mkfs_ext4
/// binary's helper — kept duplicated rather than shared because the
/// crates ship as independent binaries with their own dep trees.
fn parse_size(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("--create-size: empty size argument".to_string());
    }
    let s = trimmed.strip_suffix(['B', 'b']).unwrap_or(trimmed);
    let (num, mult): (&str, u64) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('T' | 't') => (&s[..s.len() - 1], 1024 * 1024 * 1024 * 1024),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(format!("--create-size: unrecognised size suffix in {s:?}")),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("--create-size: not a valid number: {num:?}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("--create-size: {s} overflows u64"))
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
