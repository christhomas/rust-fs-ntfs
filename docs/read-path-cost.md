# What a read costs

Measured by `tests/read_path_cost.rs`, which counts **calls to the
device** rather than wall time. Wall time on a laptop with a warm page
cache says more about the laptop than the driver; call counts are
deterministic — the same image walked the same way makes the same calls
every time — so they can be compared across months and asserted on.

Wall time is printed beside them because it is what a user feels. It is
not what anything is judged by.

## 2026-09-06 — the first measurement

Fixture: `test-disks/_csize_c64k.img`, the largest image the fixture
script builds. 15 paths, 14 of them files.

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
