//! `rust-ntfs format` — build a fresh NTFS volume.
//!
//! Wraps `fs_ntfs::mkfs::format_filesystem`, which the DiskJockey FSKit
//! extension's `startFormat` also calls — so "format an SD card from
//! the GUI" and "format a disk image from this CLI" exercise the
//! exact same code path.
//!
//! Convention: the device/file MUST already exist at the target size,
//! same as every other mkfs.* tool. Use `truncate -s 256M out.img`
//! (Linux/macOS) or `fsutil file createnew out.img 268435456`
//! (Windows). The `--create-size <SIZE>` flag collapses that to a
//! single command for image-file workflows.

use fs_ntfs::block_io::PathIo;
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rust-ntfs format [options] <device>

Options:
  -L, --label <label>      Volume label (max 32 UTF-16 code units after encode).
  -c, --cluster-size <n>   Cluster size in bytes. Power of 2, 512..=65536.
                           Default: 4096.
  --mft-record-size <n>    MFT record size in bytes. Power of 2, 512..=16384.
                           Default: 4096.
  --serial <hex>           NTFS volume serial number (16 hex chars). Default:
                           random.
  -Q, --quick              Quick format. Accepted; the on-disk layout we
                           write is always quick-format-equivalent (no
                           full-volume zero pass).
  -f, --fast               Fast format alias for -Q.
  -F, --force              Format even if device looks in use. (Accepted.)
  -n                       Dry-run: parse args + open device but do not write.
  -q, --quiet              Suppress non-error output.
  --create-size <SIZE>     If device doesn't exist, create it as a regular
                           file of the given size first. SIZE accepts K/M/G/T
                           suffixes (1024-based). Refuses to apply to existing
                           block devices — only valid for image files.
  -h, --help               Print this help and exit.

Positional:
  device                   Path to a block device or pre-sized regular file.
";

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
    create_size: Option<u64>,
    device: Option<String>,
}

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs format: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<(), String> {
    let opts = parse_args(args)?;
    let device = opts
        .device
        .as_deref()
        .ok_or_else(|| format!("missing positional <device> argument\n\n{USAGE}"))?;

    let cluster_size = opts.cluster_size.unwrap_or(4096);
    let mft_record_size = opts.mft_record_size.unwrap_or(4096);

    if let Some(n) = opts.create_size {
        match std::fs::metadata(device) {
            Ok(meta) => {
                let ft = meta.file_type();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if ft.is_block_device() || ft.is_char_device() {
                        return Err(format!(
                            "--create-size refuses to apply to {device}: looks like a real block/char device, \
                             not a regular file."
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
                        "rust-ntfs format: --create-size: {device} already exists ({} bytes); leaving as-is",
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
                    eprintln!("rust-ntfs format: --create-size: created {device} ({n} bytes)");
                }
            }
        }
    }

    let mut dev =
        PathIo::open_rw(Path::new(device)).map_err(|e| format!("open {device} read-write: {e}"))?;

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
            "rust-ntfs format: formatting {device} ({size} bytes, cluster_size={cluster_size}, mft_record_size={mft_record_size}{}{})",
            if opts.quick { ", quick" } else { "" },
            if opts.dry_run { ", dry-run" } else { "" }
        );
    }

    if opts.dry_run {
        if !opts.quiet {
            eprintln!("rust-ntfs format: dry-run — no writes performed");
        }
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
        eprintln!("rust-ntfs format: {device} formatted successfully");
    }
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-L" | "--label" => {
                opts.label = Some(
                    iter.next()
                        .ok_or_else(|| format!("{arg} requires a label argument"))?,
                );
            }
            "-c" | "--cluster-size" => {
                let v = iter
                    .next()
                    .ok_or_else(|| format!("{arg} requires a cluster size argument"))?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("{arg}: not a valid number: {v}"))?;
                opts.cluster_size = Some(n);
            }
            "--mft-record-size" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--mft-record-size requires a value".to_string())?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("--mft-record-size: not a valid number: {v}"))?;
                opts.mft_record_size = Some(n);
            }
            "--serial" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--serial requires a hex value".to_string())?;
                opts.serial = Some(parse_hex_u64(&v)?);
            }
            "-Q" | "--quick" | "-f" | "--fast" => opts.quick = true,
            "-F" | "--force" => opts.force = true,
            "-n" => opts.dry_run = true,
            "-q" | "--quiet" => opts.quiet = true,
            "--create-size" => {
                let v = iter.next().ok_or_else(|| {
                    "--create-size requires a SIZE argument (e.g. 256M)".to_string()
                })?;
                opts.create_size = Some(parse_size(&v)?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}\n\n{USAGE}"));
            }
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
