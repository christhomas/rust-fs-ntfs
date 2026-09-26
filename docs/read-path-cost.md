# What a read costs

Measured by `tests/read_path_cost.rs`, which counts **calls to the
device** rather than wall time. Wall time on a laptop with a warm page
cache says more about the laptop than the driver; call counts are
deterministic — the same image walked the same way makes the same calls
every time — so they can be compared across months and asserted on.

Wall time is printed beside them because it is what a user feels. It is
not what anything is judged by.

## 2026-09-23 — mounted metadata cache

Issue #131 puts `am-fs-core` 0.2.11's `CachingDevice` at the NTFS mount
boundary. The focused regression formats a 32 MiB volume, mounts it through a
`CountingDevice`, and stats `/hello.txt` twice through the same C ABI handle.
The count is taken below the cache, so it records requests that reached the
backing device rather than cache lookups:

| operation | before cache | after cache |
|---|---:|---:|
| first stat after mount | 44 reads | 36 reads |
| identical repeated stat | 44 reads | 0 reads |

The before figure is the red regression run; the after figure is written by a
green run to `tmp/logs/metadata-cache-cost.txt`. The first stat also benefits
from blocks warmed during mount. The repeated operation eliminates all 44
device reads, which is the result this cache exists to preserve.

### Placement and geometry

The cache belongs to each mounted NTFS handle, not to `PathIo` and not to a
process-global device boundary. A per-operation `PathIo` cache would die before
the next walk, while a global cache would impose one block geometry on drivers
with different metadata layouts. All three NTFS mount sources (path, callbacks,
and fs-core devices) now feed one cache held for the handle's lifetime.

The cache uses 4 KiB blocks and 1024 entries: at most 4 MiB per mount. Four KiB
matches the common NTFS cluster and index-block size while covering four common
1 KiB MFT records per fetch. Writable mounts use `CachingDevice::new`, so every
write goes through the same layer and invalidates overlapping cached blocks;
read-only mounts use `CachingDevice::read_only`.

This is read-path behavior only. It changes neither the bytes written to an
NTFS volume nor their ordering, so the Windows `chkdsk` matrix is not an
applicable gate under this repository's write-side matrix rule.

## 2026-09-19 — measured on the named fixture

Fixture: `test-disks/ntfs-large-file.img`, named in the test rather than
chosen by size. Its two files, `big.bin` (8 MiB) and `small.txt`; NTFS's
own metafiles are excluded, because `$MFT` and `$Secure` are not what a
read costs.

From CI run 35460761620, `tmp/logs/read-cost.txt`:

| shape | reads | bytes | opens | reads/item |
|---|---:|---:|---:|---:|
| **one handle** | | | | |
| walk — list every directory | 5 | 7.2 KB | 0 | 5.0 |
| stat — resolve every file by path | 82 | 283 KB | 0 | 41.0 |
| read — read every file | 342 | 1.33 MB | 0 | 171.0 |
| **fresh open per call** | | | | |
| walk | 39 | 140 KB | 1 | 39.0 |
| stat | 82 | 283 KB | 2 | 41.0 |
| read | 342 | 1.33 MB | 2 | 171.0 |

**The finding holds, and is now measured on files rather than metafiles.**
`stat` and `read` are identical across the two halves: 82 reads either
way, 342 either way. Holding a handle saves the `open` and nothing else,
because the driver keeps no state between calls.

The walk is the one row where re-opening costs something (5 against 39),
and that is the `open` plus re-reading the boot sector and `$MFT`'s own
record to get back to the root.

`tests/read_path_cost.rs` asserts a ceiling of 60 / 125 / 520 reads —
about 1.5x these numbers, which catches a change in kind without tripping
on a path that grows by a read or two.

## 2026-09-06 — the first measurement (superseded; see the note below)

Fixture: `test-disks/_csize_c64k.img`, described here as "the largest
image the fixture script builds". It is not: the script's largest is
`ntfs-large-file.img` at 64 MiB, and `_csize_c64k.img` is a 512 MiB
leftover of `tests/cluster_size_matrix.rs`. The test picked whichever
`.img` happened to be biggest, so which volume these numbers describe
depended on what had been run before (#226).

**The two walk rows below are artefacts and should not be compared.**
The two passes measured different directory sets — the shared walk
listed `/`, the per-call walk did not — and divided by different
quantities: every path found (15) against directories listed (1). That,
and not the driver, is the "0.3 versus 8.0 reads per item" (#229).

The stat and read rows are sound: both passes did the same work over the
same files, which is why they agree across the two halves of the table.

Both faults are fixed in `tests/read_path_cost.rs`, which now names its
fixture and counts the same thing in both passes. THESE NUMBERS PREDATE
THAT and have not been retaken; the next run on `ntfs-large-file.img`
replaces this section.

15 paths, 14 of them files.

Each shape is measured twice: once through **one handle held across
every call**, and once with a **fresh open per call**, which is what
`facade::Filesystem` actually does — its `stat`, `read_dir` and
`read_file` each begin with `PathIo::open_ro(&self.image)`.

| shape | reads | bytes | opens | reads/item |
|---|---:|---:|---:|---:|
| **one handle** | | | | |
| walk — list every directory | 4 | 9.2 KB | 0 | 0.3 |
| stat — resolve every file by path | 86 | 1.97 MB | 0 | 6.1 |
| read — read every file | 144 | 4.00 MB | 0 | 10.3 |
| **fresh open per call** | | | | |
| walk | 8 | 145 KB | 1 | 8.0 |
| stat | 86 | 1.97 MB | 14 | 6.1 |
| read | 144 | 4.00 MB | 14 | 10.3 |

## What the numbers say

**Holding the handle saves nothing but the `open`.** 86 reads either
way for `stat`; 144 either way for `read`. Byte for byte identical.
That is the finding: this driver keeps **no state at all** between
calls, so there is nothing for a held handle to preserve. Every
resolution re-reads the boot sector, re-reads `$MFT`'s own record,
re-walks the MFT to the target, and re-reads each index it passes
through. The per-call `PathIo::open_ro` looks wasteful and is the least
of it.

**A path resolution costs 6.1 reads and 140 KB.** Fourteen `stat`s move
1.97 megabytes. The reads average 23 KB each, because resolving a name
means loading whole index allocations rather than the entry wanted.
Every one of those bytes was already read by the `stat` before it, for
the same directories, on the same volume.

**`walk` looks cheap because it is one listing.** Four reads for the
whole tree, against 8 for a single directory in the per-call pass. Not
a real difference in the work — the shared-handle walk descends by
record number from a root it already resolved, while the per-call pass
resolves each directory by path first. The comparison worth making is
`stat`, where both passes do identical work.

## What this means for a cache

`am-fs-core` 0.2.8's `CachingDevice` would serve this traffic well:
the same index blocks and the same MFT records, over and over, at
offsets that repeat exactly. It cannot be wired in as things stand,
for a structural reason rather than a missing line of code:

**there is no mount to hang it on.** `facade::Filesystem` holds a
`PathBuf`, not a device. Every operation constructs its own `PathIo`
and drops it, so a cache built inside one would be born empty and die
before the next call. The `BlockIo` trait takes `&mut self`, which also
rules out sharing one behind an `Arc` the way the other drivers do.

So the ordering is: give the facade a device it holds, then cache it.
That is filed as #131, and this measurement is what says how much it is
worth — 86 reads and 2 MB for fourteen `stat`s, nearly all of it
repeat.

## How to take the measurement again

```sh
cargo test --release --test read_path_cost -- --nocapture
```

It skips without fixtures. Build them with
`test-disks/build-ntfs-feature-images.sh`.
