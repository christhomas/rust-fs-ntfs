//! The stable-toolchain half of the fuzzing setup: replay the corpus,
//! then mutate it, and refuse if a decoder panics, hangs, or if the
//! suite quietly stopped doing any work.
//!
//! # Why there are two halves
//!
//! `fuzz/` holds `cargo-fuzz` targets. Those are the explorer: they run
//! for as long as you give them and find inputs nobody thought of. They
//! cannot be a required check, because how long they ran decides what
//! they found, and a fresh discovery would fail whichever unrelated
//! pull request happened to be open.
//!
//! This suite is the gate. Deterministic, on the stable toolchain, in
//! every pull request, reading the same `fuzz/corpus/` the explorer
//! does. Anything the explorer finds is committed there and replayed
//! here from then on.
//!
//! # What was here before
//!
//! Three `cargo-fuzz` targets, added on 2026-05-03. No workflow ran
//! them, and `fuzz/Cargo.toml` named the dependency `fs-ntfs` where the
//! package is `am-fs-ntfs` -- so they had never compiled either. A fuzz
//! harness that cannot build is indistinguishable from one that builds
//! and finds nothing, which is the failure this arrangement exists to
//! refuse.
//!
//! # Why the corpus is whole volumes
//!
//! NTFS is the most structurally complex format in the constellation
//! and several of its parsers run before anything has been validated:
//! the boot sector's geometry decides where the MFT is and how big a
//! record is, the fixup rewrites bytes at offsets the record itself
//! declares, and the index blocks a lookup walks declare their own
//! entry lengths.
//!
//! So the corpus holds two volumes `mkntfs` wrote, at 4 KiB and 512
//! byte clusters -- the cluster size is the unit every run list is
//! measured in, so a smaller one makes the lists longer and the numbers
//! smaller, which is where an off-by-one lives.
//!
//! # Four targets share one corpus
//!
//! `decode_runs`, `decode_eas`, `iter_attributes` and
//! `decompress_unit` take a *fragment* of an MFT record rather than a
//! structure of their own -- a data-run list and an EA list are
//! attribute values, and picking one out means parsing the record the
//! fuzzer is about to mutate. They read the `mft_record` corpus rather
//! than each keeping an identical copy of it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Where this suite looks for the corpus.
fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus")
}

// The in-memory device and the bounded walk, shared verbatim with the
// explorer. See fuzz/shared/walk.rs for why they are included rather
// than depended on.
include!("../fuzz/shared/walk.rs");

/// Distinct starting points for the mutation stream. Fixed, so a
/// failure reproduces from the message alone.
const SEEDS: u64 = 6;

/// Below this, the suite is not doing its job.
const CASE_FLOOR: usize = 6_000;

/// Long enough that a loaded machine is never the reason, short enough
/// that a genuine hang is reported rather than left to the job timeout.
const DEADLINE: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------- targets

struct Target {
    corpus: &'static str,
    name: &'static str,
    /// Mutated cases per (seed, starting point) pair.
    ///
    /// Per target rather than one constant, because a case costs what
    /// its seed costs to copy and to run. A 64 KiB region table is
    /// cheap; a 16 MB image opened and read is not, and giving both the
    /// same budget would mean either a slow gate or a shallow one.
    cases: usize,
    run: fn(&[u8]),
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            corpus: "image",
            name: "image",
            cases: 32,
            run: walk,
        },
        Target {
            corpus: "boot_sector",
            name: "boot_sector",
            cases: 256,
            run: |b| {
                let mut io = Bytes(b.to_vec());
                let _ = fs_ntfs::mft_io::read_boot_params_io(&mut io);
            },
        },
        Target {
            corpus: "index_block",
            name: "index_block",
            cases: 256,
            run: |b| {
                let mut out = Vec::new();
                let _ = fs_ntfs::index_io::collect_indx_block_entries(b, &mut out);
            },
        },
        Target {
            corpus: "mft_record",
            name: "iter_attributes",
            cases: 256,
            run: |b| {
                for _ in fs_ntfs::attr_io::iter_attributes(b) {}
                let _ = fs_ntfs::attr_io::describe_attributes(b);
                let _ = fs_ntfs::mft_io::record_flags(b);
            },
        },
        Target {
            corpus: "mft_record",
            name: "decode_runs",
            cases: 256,
            run: |b| {
                let _ = fs_ntfs::data_runs::decode_runs(b);
            },
        },
        Target {
            corpus: "mft_record",
            name: "decode_eas",
            cases: 256,
            run: |b| {
                let _ = fs_ntfs::ea_io::decode(b);
            },
        },
        Target {
            corpus: "mft_record",
            name: "decompress_unit",
            cases: 128,
            run: |b| {
                for max_len in [0usize, 4096, 65_536] {
                    let _ = fs_ntfs::compression::decompress_unit(b, max_len);
                }
            },
        },
        // THE FIXUP APPLIER, which rewrites bytes at offsets the record
        // itself declares and runs before anything about that record
        // has been validated. The explorer found a panic here on its
        // first run -- `record[0..4]` on a two-byte buffer -- so the
        // sector size is part of the case rather than fixed: the stride
        // count derives from it and a hostile boot sector chooses it.
        Target {
            // ITS OWN CORPUS, because one of the seeds is three bytes
            // long and `every_committed_record_has_attributes_to_walk`
            // rightly asserts that everything in `mft_record` is a
            // record. The three bytes are the explorer's crash input,
            // kept as the reproducer the way this file's header says
            // findings should be.
            corpus: "apply_fixup",
            name: "apply_fixup",
            cases: 128,
            run: |b| {
                for bytes_per_sector in [512u16, 1024, 4096] {
                    let mut record = b.to_vec();
                    let _ = fs_ntfs::mft_io::apply_fixup_on_read(&mut record, bytes_per_sector);
                }
            },
        },
    ]
}

// ---------------------------------------------------------------- corpus

fn seeds(corpus: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_root().join(corpus);
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading the corpus directory {}: {e}", dir.display()))
        .map(|entry| {
            let path = entry.expect("corpus directory entry").path();
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("reading the seed {}: {e}", path.display()));
            let name = path
                .file_name()
                .expect("seed file name")
                .to_string_lossy()
                .into_owned();
            (name, bytes)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------- mutation

/// xorshift64*. Small, deterministic, and not a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One mutation of a real structure or a real image, preserving length.
///
/// Length is preserved because a device answers a read past its end
/// with `ShortRead` before any of this code is reached -- a hostile
/// image controls what is in a block, not how many bytes the device
/// hands back.
///
/// The `header` bias exists because an image is mostly file data: a
/// uniformly random offset in a 48 KiB image lands in somebody's text
/// file nine times out of ten, where nothing parses it. Half the
/// mutations are aimed at the first two blocks, which is where the
/// superblock, the inode table and the directory blocks are.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }
    let metadata_end = out.len().min(8192);
    let region = if rng.next() & 1 == 0 {
        metadata_end
    } else {
        out.len()
    };

    match rng.below(5) {
        0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(region);
                out[at] ^= 1u8 << rng.below(8);
            }
        }
        1 => {
            let at = rng.below(region);
            let len = 1 + rng.below(16.min(out.len() - at));
            let fill = if rng.next() & 1 == 0 { 0x00 } else { 0xff };
            out[at..at + len].fill(fill);
        }
        2 => {
            let width = [2usize, 4, 8][rng.below(3)];
            if out.len() >= width {
                let at = rng.below(region.saturating_sub(width) + 1) & !(width - 1);
                if at + width <= out.len() {
                    let value: u64 = match rng.below(4) {
                        0 => 0,
                        1 => 1,
                        2 => u64::MAX,
                        _ => rng.next(),
                    };
                    // Little-endian: every multi-byte field in partition table is.
                    out[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
                }
            }
        }
        3 => {
            if out.len() >= 8 {
                let a = rng.below(region / 4) * 4;
                let b = rng.below(region / 4) * 4;
                if a + 4 <= out.len() && b + 4 <= out.len() {
                    for i in 0..4 {
                        out.swap(a + i, b + i);
                    }
                }
            }
        }
        _ => {
            if out.len() >= 4 {
                let at = rng.below(region / 4) * 4;
                if at + 4 <= out.len() {
                    let word = u32::from_le_bytes(out[at..at + 4].try_into().expect("4 bytes"));
                    let delta = [1i64, -1, 2, -2, 255, -255][rng.below(6)];
                    let changed = (i64::from(word).wrapping_add(delta)) as u32;
                    out[at..at + 4].copy_from_slice(&changed.to_le_bytes());
                }
            }
        }
    }
    out
}

/// The case in flight, readable even if the lock was poisoned by the
/// panic we are trying to describe.
fn describe(current: &Arc<Mutex<String>>) -> String {
    match current.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn every_target_has_a_corpus() {
    for target in targets() {
        assert!(
            !seeds(target.corpus).is_empty(),
            "the target {} reads fuzz/corpus/{}, which holds no seeds -- a target with an \
             empty corpus runs no cases and would pass in silence. Rebuild it with \
             scripts/make-fuzz-corpus.sh",
            target.name,
            target.corpus,
        );
    }
}

/// The corpus is an oracle, not just fuel: every committed volume is one
/// `mkntfs` wrote, so this crate must read its geometry and find the
/// MFT where the boot sector says it is.
///
/// A seed that stopped reading would otherwise go on being mutated and
/// go on not failing, because a mutation of an unreadable volume is
/// also unreadable.
#[test]
fn every_committed_volume_reads_its_geometry_and_finds_its_mft() {
    let images = seeds("image");
    assert_eq!(
        images.len(),
        2,
        "the image corpus holds {} volumes, not the two the script builds",
        images.len()
    );

    for (name, bytes) in images {
        let mut io = Bytes(bytes);
        let params = fs_ntfs::mft_io::read_boot_params_io(&mut io)
            .unwrap_or_else(|e| panic!("{name}: a volume mkntfs wrote would not read: {e}"));

        // Record 0 is $MFT itself. If the arithmetic that locates it is
        // wrong, this reads somewhere else entirely and the signature
        // says so -- which is a much clearer failure than a later one
        // about an attribute that makes no sense.
        let at = fs_ntfs::mft_io::mft_record_offset(&params, 0);
        let mut record = vec![0u8; 1024];
        io.read_exact_at(at, &mut record)
            .unwrap_or_else(|e| panic!("{name}: reading MFT record 0 at {at}: {e}"));
        assert_eq!(
            &record[..4],
            b"FILE",
            "{name}: MFT record 0 does not start with FILE, so the boot geometry was misread"
        );
    }
}

/// Every committed MFT record is one `mkntfs` wrote, so walking its
/// attributes must find some.
///
/// Without this, a corpus of records that had quietly become
/// unparseable would still mutate and still find nothing.
#[test]
fn every_committed_record_has_attributes_to_walk() {
    let records = seeds("mft_record");
    assert!(
        records.len() >= 8,
        "only {} MFT records; the corpus has shrunk",
        records.len()
    );

    let mut with_attributes = 0;
    for (name, bytes) in records {
        assert_eq!(&bytes[..4], b"FILE", "{name}: not an MFT record");
        if fs_ntfs::attr_io::iter_attributes(&bytes).next().is_some() {
            with_attributes += 1;
        }
    }
    assert!(
        with_attributes >= 8,
        "only {with_attributes} of the committed records had an attribute to walk"
    );
}

#[test]
fn deterministic_mutations_of_real_structures_are_survived() {
    let cases = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(String::from("(not started)")));
    let (done_tx, done_rx) = mpsc::channel();

    let hook_current = Arc::clone(&current);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("\nfuzz gate: panicked at {}", describe(&hook_current));
        previous_hook(info);
    }));

    let worker_cases = Arc::clone(&cases);
    let worker_current = Arc::clone(&current);
    let worker = std::thread::spawn(move || {
        for target in targets() {
            for (seed_name, bytes) in seeds(target.corpus) {
                for start in 0..SEEDS {
                    let mut rng = Rng::new(start);
                    for case in 0..target.cases {
                        *worker_current.lock().expect("progress lock") =
                            format!("{} / {seed_name} / seed {start} / case {case}", target.name);
                        let mutated = mutate(&bytes, &mut rng);
                        (target.run)(&mutated);
                        worker_cases.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });

    // A timeout means the worker is still running: a hang. A disconnect
    // means it panicked, and the panic is what is worth reporting.
    match done_rx.recv_timeout(DEADLINE) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Written to the process's stderr rather than through
            // `eprintln!`, which the harness captures into a buffer it
            // only prints when a test finishes -- and exiting here means
            // it never finishes.
            let _ = writeln!(
                std::io::stderr(),
                "\nhung: no progress for {:?} at {}\n\
                 A decoder did not return. An attribute whose declared length is zero, \
                 or an index entry that points back at its own block, looks exactly like \
                 this.",
                DEADLINE,
                describe(&current),
            );
            let _ = std::io::stderr().flush();
            std::process::exit(1);
        }
    }

    let outcome = worker.join();
    let _ = std::panic::take_hook();
    if outcome.is_err() {
        panic!("a decoder panicked at {}", describe(&current));
    }

    let total = cases.load(Ordering::Relaxed);
    assert!(
        total >= CASE_FLOOR,
        "only {total} mutated cases ran, below the floor of {CASE_FLOOR} -- the target \
         list or the corpus has collapsed, and a suite that runs nothing passes quickly",
    );
    eprintln!("{total} mutated cases");
}

#[test]
fn the_gate_covers_every_explorer_target() {
    // The two tiers drift apart the moment somebody adds a cargo-fuzz
    // target and forgets that nothing gates it on the stable toolchain.
    let manifest =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/Cargo.toml"))
            .expect("reading fuzz/Cargo.toml");

    let explorer: Vec<String> = manifest
        .lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .skip(1) // the package name is the first `name =` in the file
        .collect();

    assert!(
        !explorer.is_empty(),
        "fuzz/Cargo.toml declares no [[bin]] targets",
    );

    let gated: Vec<&str> = targets().iter().map(|t| t.name).collect();
    for name in &explorer {
        assert!(
            gated.contains(&name.as_str()),
            "fuzz/fuzz_targets/{name}.rs has no counterpart in this suite, so nothing \
             replays its corpus on the stable toolchain and anything it finds would only \
             stay fixed for as long as somebody keeps running the fuzzer by hand",
        );
    }
}
