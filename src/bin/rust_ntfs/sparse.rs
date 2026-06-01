//! `rust-ntfs sparse` — write an existing file's `$DATA` stream as a SPARSE
//! stream: cluster-aligned all-zero regions become holes (no clusters
//! allocated), only non-zero regions consume space.
//!
//! The file must already exist (use `touch` first) and still have resident
//! `$DATA` (the MVP precondition of `write::write_sparse_file`).

use fs_ntfs::write::write_sparse_file;
use std::path::Path;
use std::process::ExitCode;

/// 4 KiB block period for the `sparse` pattern — matches the matrix's
/// `alloc_unit_size` (4096) so alternating blocks land on cluster
/// boundaries and become real holes.
const SPARSE_BLOCK: usize = 4096;

const USAGE: &str = "\
Usage: rust-ntfs sparse <image> <path> --bytes N [--pattern PAT]

Writes N bytes into the unnamed $DATA stream of an existing file as a
SPARSE stream. Cluster-aligned all-zero regions are stored as holes.

  --bytes N         Logical size to write.
  --pattern PAT     One of:
                      zeros        all holes (fully sparse).
                      sparse       alternating 4 KiB data / hole blocks.
                      incrementing dense (no holes); a control pattern.

Examples:
  rust-ntfs touch  vol.img / big.bin
  rust-ntfs sparse vol.img /big.bin --bytes 65536 --pattern zeros
  rust-ntfs sparse vol.img /big.bin --bytes 65536 --pattern sparse
";

pub fn run(args: Vec<String>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rust-ntfs sparse: {msg}");
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

    let data = parse_sparse_source(&mut rest)?;

    write_sparse_file(Path::new(&image), &path, &data)
        .map_err(|e| format!("sparse write {path}: {e}"))?;
    println!("wrote {} bytes to {path} as a sparse stream", data.len());
    Ok(())
}

fn parse_sparse_source(args: &mut Vec<String>) -> Result<Vec<u8>, String> {
    let mut bytes: Option<u64> = None;
    let mut pattern = String::from("sparse");
    while !args.is_empty() {
        let flag = args.remove(0);
        match flag.as_str() {
            "--bytes" => {
                let v = args
                    .first()
                    .ok_or_else(|| "--bytes requires a number".to_string())?
                    .clone();
                args.remove(0);
                bytes = Some(v.parse().map_err(|_| format!("--bytes: bad number {v}"))?);
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
    let n = bytes.ok_or("sparse: --bytes is required")? as usize;
    gen_pattern(&pattern, n)
}

/// Generate `n` bytes for the named sparse pattern.
fn gen_pattern(pattern: &str, n: usize) -> Result<Vec<u8>, String> {
    match pattern {
        // Fully sparse: every cluster is a hole.
        "zeros" => Ok(vec![0u8; n]),
        // Alternating 4 KiB data / hole blocks → a mixed sparse file.
        "sparse" => Ok((0..n)
            .map(|i| {
                if (i / SPARSE_BLOCK).is_multiple_of(2) {
                    // Non-zero so the block is a real (allocated) data cluster.
                    ((i & 0xFF) as u8) | 1
                } else {
                    0
                }
            })
            .collect()),
        // Dense control: no zero clusters.
        "incrementing" => Ok((0..n).map(|i| ((i & 0xFF) as u8) | 1).collect()),
        other => Err(format!("unknown --pattern: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeros_pattern_is_all_zero() {
        assert!(gen_pattern("zeros", 8192).unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn sparse_pattern_alternates_4k_blocks() {
        let d = gen_pattern("sparse", 3 * SPARSE_BLOCK).unwrap();
        // block 0: non-zero, block 1: all-zero, block 2: non-zero.
        assert!(d[0..SPARSE_BLOCK].iter().any(|&b| b != 0), "block 0 has data");
        assert!(d[SPARSE_BLOCK..2 * SPARSE_BLOCK].iter().all(|&b| b == 0), "block 1 is a hole");
        assert!(d[2 * SPARSE_BLOCK..3 * SPARSE_BLOCK].iter().any(|&b| b != 0), "block 2 has data");
    }

    #[test]
    fn incrementing_pattern_has_no_zero_bytes() {
        // Every byte non-zero → no false holes (dense control).
        assert!(gen_pattern("incrementing", SPARSE_BLOCK).unwrap().iter().all(|&b| b != 0));
    }

    #[test]
    fn unknown_pattern_errors() {
        assert!(gen_pattern("bogus", 16).is_err());
    }
}
