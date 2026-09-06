//! What a read costs, in calls to the device.
//!
//! # Why this is a test and not a benchmark
//!
//! The number that matters is not wall time. Wall time on a laptop with
//! a warm page cache says more about the laptop than the driver: run it
//! twice and the second is faster for reasons this repository does not
//! control. **Calls to the device** are deterministic — the same image
//! walked the same way makes the same calls every time — so they can be
//! asserted on, and a change that makes the driver ask for more is a
//! regression a test can catch rather than a number somebody has to
//! remember.
//!
//! Wall time is printed beside them, because it is what a user feels,
//! and ignored by the assertions.
//!
//! # What is measured
//!
//! Three shapes, each twice:
//!
//! - **walk** — every directory in the tree, listed.
//! - **stat** — every file resolved by path from the root.
//! - **read** — every file's contents.
//!
//! Once through a **shared** `BlockIo`, which is one open handle held
//! across every operation, and once the way [`fs_ntfs::facade`] actually
//! works — a **fresh** `PathIo` opened per call. The facade's `stat`,
//! `read_dir` and `read_file` each begin with
//! `PathIo::open_ro(&self.image)`, so nothing survives from one call to
//! the next: no boot sector, no MFT record, no index block.
//!
//! The gap between the two columns is what a mount that held its handle
//! would save before any caching is considered at all.
//!
//! Fixtures are built by `test-disks/build-ntfs-feature-images.sh`, so
//! this skips on a fresh clone.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::read;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Counts what reaches the device, whatever sits above it.
///
/// A decorator rather than a field on `PathIo`, so the thing being
/// measured is the unmodified code path.
struct CountingIo<T: BlockIo> {
    inner: T,
    reads: u64,
    bytes: u64,
}

impl<T: BlockIo> CountingIo<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            reads: 0,
            bytes: 0,
        }
    }
    fn reset(&mut self) {
        self.reads = 0;
        self.bytes = 0;
    }
}

impl<T: BlockIo> BlockIo for CountingIo<T> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        // COUNTED BEFORE THE CALL, so a read that fails is still a read
        // that was made: a driver asking for something that is not there
        // has cost the device the same seek.
        self.reads += 1;
        self.bytes += buf.len() as u64;
        self.inner.read_exact_at(offset, buf)
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        self.inner.write_all_at(offset, buf)
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

/// What one shape of read cost.
struct Cost {
    reads: u64,
    bytes: u64,
    micros: u128,
    /// How much work was actually done, so a number that fell because
    /// the driver did less is not read as a number that fell because
    /// the driver got better.
    items: usize,
    /// How many times the image file was opened. Zero is the shared
    /// handle; one per operation is what the facade does.
    opens: u64,
}

fn fixture() -> Option<PathBuf> {
    let disks = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-disks");
    let Ok(entries) = std::fs::read_dir(&disks) else {
        return None;
    };
    // The largest image available: the cost of a walk is the point, and
    // the bigger fixtures have the deeper trees.
    let mut images: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("img"))
        .collect();
    images.sort_by_key(|p| std::cmp::Reverse(p.metadata().map(|m| m.len()).unwrap_or(0)));
    images.into_iter().next()
}

fn report(what: &str, c: &Cost) {
    let per = if c.items == 0 {
        0.0
    } else {
        c.reads as f64 / c.items as f64
    };
    eprintln!(
        "{what:<12} {:>6} reads  {:>9} bytes  {:>4} opens  {:>8} µs  over {:>4} items  \
         ({per:.1} reads/item)",
        c.reads, c.bytes, c.opens, c.micros, c.items
    );
}

/// Every path in the tree, bounded so a large fixture cannot make this
/// run for minutes.
fn walk_paths<T: BlockIo>(io: &mut T, at: &str, record: u64, depth: u32, out: &mut Vec<PathEntry>) {
    if depth == 0 || out.len() > 400 {
        return;
    }
    let Ok(entries) = read::read_dir_entries(io, record) else {
        return;
    };
    for e in entries {
        if out.len() > 400 {
            return;
        }
        if e.name == "." || e.name == ".." {
            continue;
        }
        let child = if at == "/" {
            format!("/{}", e.name)
        } else {
            format!("{at}/{}", e.name)
        };
        out.push(PathEntry {
            path: child.clone(),
            record: e.record_number,
            is_dir: e.is_dir,
        });
        if e.is_dir {
            walk_paths(io, &child, e.record_number, depth - 1, out);
        }
    }
}

struct PathEntry {
    path: String,
    /// The record the listing named. Kept beside the path because the
    /// walk descends by record and the later shapes resolve by path,
    /// and comparing the two is how a listing that names records a
    /// lookup cannot reach would be caught.
    #[allow(dead_code)]
    record: u64,
    is_dir: bool,
}

/// What one pass measured.
struct Pass {
    walk: Cost,
    stat: Cost,
    /// Reported rather than asserted on: the data a file holds has to
    /// come off the device however good the metadata path gets, so this
    /// is the line that should NOT move much.
    #[allow(dead_code)]
    read: Cost,
    paths: Vec<PathEntry>,
}

/// One handle, opened once and held. What a mount would do.
fn measure_shared(img: &Path) -> Pass {
    let mut io = CountingIo::new(PathIo::open_ro(img).expect("open the fixture"));

    let root = read::resolve_path(&mut io, "/").expect("resolve the root");
    io.reset();

    let mut paths = Vec::new();
    let start = Instant::now();
    walk_paths(&mut io, "/", root, 8, &mut paths);
    let walk = Cost {
        reads: io.reads,
        bytes: io.bytes,
        micros: start.elapsed().as_micros(),
        items: paths.len(),
        opens: 0,
    };
    report("walk", &walk);

    let files: Vec<&PathEntry> = paths.iter().filter(|p| !p.is_dir).collect();

    io.reset();
    let start = Instant::now();
    for f in &files {
        let _ = read::resolve_path(&mut io, &f.path);
    }
    let stat = Cost {
        reads: io.reads,
        bytes: io.bytes,
        micros: start.elapsed().as_micros(),
        items: files.len(),
        opens: 0,
    };
    report("stat", &stat);

    io.reset();
    let start = Instant::now();
    for f in &files {
        if let Ok(record) = read::resolve_path(&mut io, &f.path) {
            let _ = read::read_attribute_range(
                &mut io,
                record,
                fs_ntfs::attr_io::AttrType::Data,
                None,
                0,
                1 << 20,
            );
        }
    }
    let read_cost = Cost {
        reads: io.reads,
        bytes: io.bytes,
        micros: start.elapsed().as_micros(),
        items: files.len(),
        opens: 0,
    };
    report("read", &read_cost);

    Pass {
        walk,
        stat,
        read: read_cost,
        paths,
    }
}

/// A fresh open per operation, which is what `facade::Filesystem` does:
/// its `stat`, `read_dir` and `read_file` each begin with
/// `PathIo::open_ro(&self.image)`.
fn measure_per_call(img: &Path, paths: &[PathEntry]) -> Pass {
    let dirs: Vec<&PathEntry> = paths.iter().filter(|p| p.is_dir).collect();
    let files: Vec<&PathEntry> = paths.iter().filter(|p| !p.is_dir).collect();

    let mut reads = 0u64;
    let mut bytes = 0u64;
    let mut opens = 0u64;
    let start = Instant::now();
    for d in &dirs {
        let mut io = CountingIo::new(PathIo::open_ro(img).expect("open the fixture"));
        opens += 1;
        if let Ok(record) = read::resolve_path(&mut io, &d.path) {
            let _ = read::read_dir_entries(&mut io, record);
        }
        reads += io.reads;
        bytes += io.bytes;
    }
    let walk = Cost {
        reads,
        bytes,
        micros: start.elapsed().as_micros(),
        items: dirs.len(),
        opens,
    };
    report("walk", &walk);

    let mut reads = 0u64;
    let mut bytes = 0u64;
    let mut opens = 0u64;
    let start = Instant::now();
    for f in &files {
        let mut io = CountingIo::new(PathIo::open_ro(img).expect("open the fixture"));
        opens += 1;
        let _ = read::resolve_path(&mut io, &f.path);
        reads += io.reads;
        bytes += io.bytes;
    }
    let stat = Cost {
        reads,
        bytes,
        micros: start.elapsed().as_micros(),
        items: files.len(),
        opens,
    };
    report("stat", &stat);

    let mut reads = 0u64;
    let mut bytes = 0u64;
    let mut opens = 0u64;
    let start = Instant::now();
    for f in &files {
        let mut io = CountingIo::new(PathIo::open_ro(img).expect("open the fixture"));
        opens += 1;
        if let Ok(record) = read::resolve_path(&mut io, &f.path) {
            let _ = read::read_attribute_range(
                &mut io,
                record,
                fs_ntfs::attr_io::AttrType::Data,
                None,
                0,
                1 << 20,
            );
        }
        reads += io.reads;
        bytes += io.bytes;
    }
    let read_cost = Cost {
        reads,
        bytes,
        micros: start.elapsed().as_micros(),
        items: files.len(),
        opens,
    };
    report("read", &read_cost);

    Pass {
        walk,
        stat,
        read: read_cost,
        paths: Vec::new(),
    }
}

/// The measurement itself. Prints the numbers and asserts only that the
/// work was done and that the per-call pass really does re-open — the
/// figures are recorded in `docs/read-path-cost.md` and compared by hand
/// when something changes, because a threshold baked in here would
/// either be so loose it catches nothing or so tight it fails on a
/// fixture rebuild.
#[test]
fn what_a_read_costs_in_calls_to_the_device() {
    let Some(img) = fixture() else {
        eprintln!("no fixture to measure — run test-disks/build-ntfs-feature-images.sh");
        return;
    };
    eprintln!("measuring {}", img.display());

    eprintln!("--- one handle, held across every call ---");
    let shared = measure_shared(&img);
    eprintln!("--- a fresh open per call, which is what the facade does ---");
    let percall = measure_per_call(&img, &shared.paths);

    assert!(
        shared.walk.items > 0,
        "the fixture had nothing to walk — the measurement is of nothing"
    );
    assert!(
        shared.walk.reads > 0,
        "no calls reached the device, so the counter is not wired to the io"
    );
    assert!(
        percall.stat.opens as usize == percall.stat.items,
        "the per-call pass must open the image once per operation, or it is \
         not measuring what the facade does"
    );
    // Re-opening cannot make the driver ask for LESS, and if it ever
    // appears to, the two passes are not doing the same work.
    assert!(
        percall.stat.reads >= shared.stat.reads,
        "re-opening per call asked the device for fewer reads than holding \
         one handle, which cannot be right"
    );
}
