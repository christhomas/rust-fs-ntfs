# Human-code report — 2026-08-28

> **This is analysis only. No code was changed.** No files were edited, no branches
> were created, nothing was committed. The only file this pass added is the one you
> are reading. Phase 2 (the dev-loop that would actually apply fixes) was deliberately
> not run — the intent is that you read this first and choose what, if anything, moves.

---

## Scope

**Analysed:** the crate's own Rust source.

| Area | Files | Lines |
|---|---:|---:|
| `src/*.rs` — library | 22 | 24,977 |
| `src/bin/rust_ntfs/*.rs` — the `rust-ntfs` CLI | 13 | 1,075 |
| `tests/*.rs` + `tests/common/mod.rs` | 93 | 17,108 |
| **Total** | **128** | **43,160** |

**Excluded entirely, and read at no point:**

- **`vendor/`** — contains a submodule (`vendor/fs-test-harness`) that is currently
  modified as part of another agent's work in progress. Reading it would have meant
  reporting on a half-finished change set that is not this crate's code.
- **`docs/testing/`** — two untracked files there (`01-strategy-and-the-contract.html`,
  `_style-comparison.md`) are likewise someone else's in-flight work.

Both were left untouched, along with the repository's existing stash. The working
tree still holds exactly the three uncommitted entries it held before this pass, plus
this report.

Also out of scope by nature: `benches/`, `examples/`, `fuzz/`, `scripts/`, `.githooks/`.

**Counts:** 103 items found · **0 fixed** · 103 deferred pending your decision.

| Severity | Count | Meaning |
|---|---:|---|
| **High** | 56 | Confusion that could hide a bug, or already has |
| **Medium** | 44 | Slows comprehension materially |
| **Low** | 3 | Cosmetic (representative sample; see *Rolled up*) |

Because this is a **read-write** driver, findings were weighted by blast radius: a
readability problem in code that mutates a volume counts for more than the same
problem in code that only parses one. **47 of the 56 High findings sit in a write,
format, or allocator path**, and one of them (G1) has already produced a real defect —
a readability problem, a stale cache behind an innocent-looking accessor, that silently
defeats fifteen rollback sites.

**Static analysis baseline:** `cargo clippy --all-targets --locked` exits **0**, and
`.githooks/pre-commit.d/rust-clippy.sh` blocks any commit that produces a warning. So
clippy is not merely clean, it is *enforced* clean — which means **every finding below
is something clippy structurally cannot see**. That is the value of this pass and also
its limit: nothing here is a lint you forgot to run.

**Relationship to `docs/code-quality-review-2026-08-25.md`:** that review (three days
ago, 8 findings) measured the *shapes* — file sizes, 46 functions over 60 lines, 43
functions with 5+ parameters, 112 unnamed offsets. This pass goes after the *instances*
inside those shapes, and specifically after **divergence**: places where the same logic
was copied and one copy then drifted. It also covers two areas that review excluded —
the CLI binary and `tests/`. Where the two overlap it is noted.

---

## Findings

Ordered by area. Within each area, High before Medium before Low. Every row carries
its own test-coverage note, because that determines what can safely be touched first.

### A. The CLI — `src/bin/rust_ntfs/` (12 verbs, 1,075 lines)

Twelve verbs, twelve modules. The theme here is exactly the one you predicted:
argument handling, error reporting and exit codes have drifted apart between verbs,
and there is almost no test coverage to notice when they do.

**Coverage first, because it explains everything else in this section:** of 92
integration test files, **exactly one** (`tests/mkfs_bin_smoke.rs`) spawns the binary
at all, and it exercises only `format`'s happy path. The entire CLI contains **4 unit
tests**, all in `sparse.rs`, all covering `gen_pattern`. Eleven of the twelve verbs
have zero end-to-end coverage; no test anywhere asserts an exit code, a usage error,
or `--help` behaviour. Nothing in CI would catch any of the drift below.

| # | Location | Category | Finding | Sev | Covered |
|---|---|---|---|---|---|
| **A1** | `main.rs:21` vs all 12 verb modules | Comment that lies | The header documents *"Exit codes: 0 success, 1 failure, 2 usage error."* Only the top-level dispatcher honours 2. Verified empirically: `rust-ntfs ls --bogus` → **1**, `rust-ntfs touch a b` → **1**. Ten of twelve verbs collapse usage errors into the generic failure code, so a script cannot distinguish "you typed it wrong" from "the volume is corrupt". | High | none |
| **A2** | `write.rs:97-111` | Silent divergence, write path | The USAGE text says the data-source flags are *"exactly one required"*. Nothing enforces it — the three `if let Some(...)` returns establish a silent priority order. Verified: `write img /a.bin --content hi --bytes 4096` prints `wrote 2 bytes`, **exit 0**. The user asked for 4096 bytes and got 2, and the tool reported success. | High | none |
| **A3** | `ls.rs:50`, `format.rs:220` vs the other ten | Three parsing paradigms | `ls` and `format` reject unknown flags (`other if other.starts_with('-')`). The other ten only count positionals, so a flag is silently reinterpreted as a path. Verified: `rust-ntfs rm img.img -x` → `unlink -x: resolve_path: '-x' not found` — it tried to unlink a file *named* `-x`. On a volume that happens to contain such a name, a typo'd flag is a delete. A third paradigm again in `write.rs:60` / `sparse.rs:69` (`Vec::remove(0)` draining). | High | none |
| **A4** | `main.rs:6-15`, `38-59`, `77` | Stale doc / undiscoverable verb | `sparse` is dispatched at line 77 but appears in neither the `HELP` constant nor the module doc — verified: `rust-ntfs --help` never mentions it. The module doc also lists ten verbs and opens with *"the four operations"*, for a binary that takes twelve. | Medium | none |
| **A5** | `write.rs:105-110` vs `sparse.rs:96-113` | Same flag, three divergences | `--pattern` differs between two sibling verbs in three ways: default (`zeros` vs `sparse`); accepted set (`ones` valid in `write`, rejected in `sparse` — verified); and **`incrementing` produces different bytes** — `write` emits `i & 0xFF` (contains zeros), `sparse` emits `(i & 0xFF) \| 1` (never zero). `sparse`'s own comment calls `incrementing` "a control pattern"; it is not controlling against the same data `write` produces. | Medium | `gen_pattern` only |
| **A6** | all 12 modules | Duplication (12× and 8×) | The `pub fn run` wrapper — `match run_inner(args) { Ok(())=>SUCCESS, Err(msg)=>{eprintln!("rust-ntfs <verb>: {msg}"); FAILURE} }` — is transcribed **12 times**, differing only in the verb string. The `-h`-scan + `args.len() != N` preamble is transcribed **8 times**. Both far past the 3-instance threshold; A1 and A3 are what that duplication is currently hiding. | Medium | none |
| **A7** | `format.rs:176-179` | Divergent control flow | `-h` inside `parse_args` calls `std::process::exit(0)` directly. The other eleven return `Ok(())` and let `run` map it to an `ExitCode`. The hard exit bypasses the crate's own exit-code path and makes `format`'s help unreachable from an in-process test. | Medium | none |
| **A8** | `format.rs:34`, `145`, `211` | Dead option | `-F/--force` is documented, parsed and stored, then never read. The discard `let _ = (opts.force, opts.quick);` is parked *inside the dry-run branch*, so on a real format the field is simply unused. | Medium | none |
| **A9** | `format.rs:78-79`, `25`, `27` | Magic number, 3 places | The `4096` cluster and MFT-record defaults are hardcoded in `run_inner` and separately written into the USAGE text. `mkfs.rs` then re-hardcodes `4096` six more times (see D6). | Medium | none |
| **A10** | `format.rs:28` vs `:260` | Doc/code mismatch | USAGE says the serial is *"16 hex chars"*; `parse_hex_u64` accepts 1..=16 with an optional `0x`. | Low | none |
| **A11** | `ls.rs:77-81` and `86-90`; `touch.rs:40` | Duplication / cosmetic bug | The five-line `if dir == "/" { format!("/{}", n) } else { format!("{}/{}", dir, n) }` join appears twice in one 40-line function. `touch`/`mkdir` do not do it at all, so `rust-ntfs touch img / a.bin` prints `created file rec=24 //a.bin`. | Low | none |
| **A12** | `rm.rs:38`, `rmdir.rs:38`, `remove.rs:38` | Indistinguishable output | Three verbs print the byte-identical success line `removed {path}`. In a log there is no way to tell which ran. | Low | none |
| **A13** | `write.rs:104` | Unchecked cast | `let n = n as usize;` then `vec![0u8; n]`. `--bytes 999999999999` allocates rather than erroring. | Medium | none |

### B. `src/write.rs` — the primary mutation path (4,803 lines, 49 `*_io` entry points)

| # | Location | Category | Finding | Sev | Covered |
|---|---|---|---|---|---|
| **B1** | `write.rs:1-8` | Comment that lies, top of file | The module header — the first thing any reader of the crate's largest write path sees — is wrong on both of its load-bearing claims. It says *"No attribute resize, no cluster allocation, no new files"*; the file contains `create_file_io`, `mkdir_io`, `promote_*_to_nonresident_io`, `bitmap::allocate_io`, `attr_resize::replace_attribute`. It says *"Path resolution uses upstream `ntfs`"*; per `Cargo.toml` that crate is a **dev-dependency** and the only `ntfs::` references in the file are inside `#[cfg(test)]` at line 3858+. It also points at "status.md Phase W1" for scope. | High | n/a |
| **B2** | `write.rs:970` vs `1093`, `1292`, `2242`, `3188` | **Duplication that has already drifted** | Basename validation is copy-pasted five times. Four reject `""`, `"."`, `".."` and `/`. The fifth — `rename_same_length_io:970` — checks only `contains('/') \|\| is_empty()`, and emits a different message. `".."` is two UTF-16 units, so renaming `"ab"` → `".."` satisfies its same-length rule and passes. That function is public through `facade::rename_same_length` (`facade.rs:347`) **and** the C ABI (`fs_ntfs_rename_same_length`, `lib.rs:2348`). No guard downstream catches it. | High | `write_rename.rs`, `capi_rename.rs` — neither tests `.`/`..` |
| **B3** | `write.rs:2695-2858` | Hand-rolled rollback, 10× | `write_sparse_file_io` allocates clusters and must free them on every error exit. `free_all(io, &allocated);` is therefore written out by hand **ten times** in one 164-line function. It is currently correct — but correctness rests on a human remembering it at each of ten exits, and the eleventh exit anyone adds is a silent `$Bitmap` leak. The idiom wants to be expressed once. | High | `sparse_write.rs`, `sparse.rs` (happy paths; no error-injection test) |
| **B4** | `write.rs:3182-3282` | Missing rollback, same file | `rename_replace_io` commits step 1 (swap the parent's `$INDEX_ROOT` entry, line 3252) and *then* runs step 2 (rewrite the file's `$FILE_NAME`, line 3266). If step 2 fails, step 1 stands — a torn rename, with no rollback and no comment acknowledging it. The opposite discipline to B3, 400 lines away. Because neither is a named, shared idiom, the divergence is invisible. | High | `capi_rename_overwrite.rs`, `write_rename_varlen.rs` (success paths) |
| **B5** | `write.rs:1130`, `1323`, `2286`, `2288`, `3236`, `3237`, `3582` | Magic offset, 7× | `u16::from_le_bytes([rec[0x10], rec[0x11]])` — the MFT record sequence number — open-coded seven times. The comment explaining what `0x10` is appears **once**, at line 1129. Worse, the same file defines `SI_MFT_MODIFICATION: usize = 0x10` at line 39, so a reader meeting `0x10` in this file has to work out which `0x10` it is. | Medium | indirectly, via create/link/rename tests |
| **B6** | `write.rs:1007-1009` | Inverted control flow | The "renaming to the same name is a no-op" case is expressed as the `else` arm of the collision check (`} else { return Ok(()); }`) rather than an early return at the top, so the reader meets the exit condition after the work. | Medium | yes |
| **B7** | `write.rs:1176-1186` | Rollback idiom #3 | `create_file_io`'s rollback (clear `IN_USE`, free the MFT bitmap bit) is spelled out inline, a third distinct shape alongside B3 and B4. | Medium | `write_create.rs` |
| **B8** | throughout | Mechanical duplication | 49 `*_io` generic functions, each shadowed by a 3-line `PathIo::open_rw` convenience wrapper. Individually idiomatic; collectively it doubles the public surface a reader must scan to find the one that does the work. | Medium | yes |

### C. `src/lib.rs` — the C ABI (3,753 lines, 69 entry points)

This is where FSKit and Swift meet the driver, so an inconsistency here reaches the
app. 42 of the 69 entry points contain an `unsafe` block; the whole file has **6**
`// Safety:` comments.

| # | Location | Category | Finding | Sev | Covered |
|---|---|---|---|---|---|
| **C1** | `lib.rs:2410-2412` vs `2021-2026` | **Same guard, opposite effect** | Two adjacent write entry points both open with `if len == 0`. `fs_ntfs_write_file` returns **0 (success) without opening the image or checking the path exists**. `fs_ntfs_write_file_contents` calls through with `&[]`, i.e. **truncates the file to zero**. Verified by reading both. A caller cannot reason about `len == 0` from the ABI. | High | separate happy-path tests; nothing compares them |
| **C2** | `lib.rs:89-138` | Fragile heuristic, no typed errors | `infer_errno_from_message` derives the C errno by substring-matching English prose. Measured by scoring every production error string in `write.rs` against the function's own rules: **85 of 107 (79%) fall through to EIO**, and only **one** string in the whole file reaches ENOSPC. Confirmed misroutes include all three spellings of `"no contiguous free run of N clusters"` (should be ENOSPC), five `"no entry / no index entry / no matching index entry for …"` (should be ENOENT — the rule matches `"not found"`, not `"no entry"`), `"new_name must be a basename"` (EINVAL), `"cannot rename root"` (EPERM), and both `"refusing to hardlink directory"` / `"refusing resident $DATA"` — which miss the `"refuse"` probe because the word is *"refusing"*. In the other direction, on-disk **corruption** messages (`compression.rs:61`, `ea_io.rs:97`, `data_runs.rs:81`) all contain `"invalid"` and reach the caller as **EINVAL** — "you passed a bad argument". A full disk reports as an I/O error. | High | 16 tests (`lib.rs:3253`, `3426`) — but every one asserts on a hand-written string, not one any module emits, so they pass while 79% of real messages misroute |
| **C3** | `lib.rs:911`, `953`, `1075`, `1179`, `1245`, `1304` | Silent failure | Six entry points return `-1`/null on a null argument **without calling `set_error`**, leaving `fs_ntfs_last_error`/`last_errno` holding whatever the previous, unrelated call left. `fs_ntfs_readlink:1385` — identical guard — does set it. A caller that checks the return then reads the errno gets a stale one. | High | no |
| **C4** | `lib.rs:1659`, `1774`, `1906`, `2031`, `2091`, `2417`, `2671` | Asymmetric safety guard | `fs_ntfs_read_file:1313` rejects `length > isize::MAX` with a six-line comment explaining the `from_raw_parts` UB contract. **Seven mutating entry points** build slices the same way with no such guard. The read path documents a hazard the write paths ignore. | High | no |
| **C5** | `lib.rs:1736`, `1964`, `1843`, `2258-2266`, `1052-1056` | Five siblings, five contracts | "Caller's buffer is too small" is signalled as `-2` (twice), `2` (once), and **silently** twice — `read_object_id_extended` returns 16 having dropped the Birth GUIDs; `read_volume_label` truncates and returns the truncated count. Two of the five lose data without telling anyone. | High | partial |
| **C6** | `lib.rs:862-882` | Contract by comment only | `handle_to_ro_io`'s doc says *"The returned `HandleIo` must not be written to"*, but the returned value has a working `write_all_at`, and in the `Callbacks` case (line 873) it carries the real `write_fn`. Nothing in the type prevents the write. | High | no |
| **C7** | `lib.rs:1513-1518` | Unsound signature | `fn cstr_to_path<'a>(path: *const c_char) -> Option<&'a str>` invents an unbounded lifetime from a raw pointer, is not an `unsafe fn`, and has no safety doc — while being reached by roughly 50 entry points. It is also filed under the section header `// Recovery / fsck — write operations`. | High | n/a |
| **C8** | `lib.rs:2927-2985` vs `block_io.rs:153-206` | Duplicated I/O backend | `CallbackIo` is a near-verbatim copy of `block_io::CallbackBlockIo` — identical `read_exact_at`/`write_all_at` bodies, identical error strings. The one difference (`write_fn` non-`Option`) is what forces the `write_stub` at `lib.rs:2998`, an `unsafe extern "C" fn` declared inside a function body that returns `5 /* EIO */`. `block_io` already models this correctly. | High | partial |
| **C9** | `lib.rs:220-230` + 13 sites + 8 sites; `190-212` + 18 sites | Duplication, adoption stalled | Three co-existing idioms for "C string → `&str` or bail": the `cstr_or_return!` macro (37 entry points), a hand-written `let-else` (13), and a raw `CStr::from_ptr` match (8). The macro exists to kill exactly this and got to ~54%. Likewise `err_int`/`err_i64`/`err_ptr` exist "to collapse those into one call" and 18 entry points still spell it out; `err_ptr` has **one** caller. | Medium | yes |
| **C10** | `lib.rs:813`, `826`, `838`, `517` | Four ways to fail the same way | Read-only rejection produces three differently-worded strings, and `fs_ntfs_mount:517` hardcodes `writable: true` regardless, so a genuinely read-only image instead fails later inside `open_rw` with a raw OS error. All four take different routes through C2. | Medium | partial |
| **C11** | `lib.rs:96-104`, `3003` | Scoped constants | The nine errno constants are declared *inside* `infer_errno_from_message`, so `write_stub` re-hardcodes `5 /* EIO */`. `ENOTEMPTY = 66` carries the comment *"macOS; Linux is 39; both non-zero, good enough"* on a public ABI. | Medium | partial |
| **C12** | `lib.rs:600-603`, `1370`, `1172` vs `422`, `31-39` | Stale docs | `fs_ntfs_mount_with_fs_core_device` documents an error string (`"handle has no recorded mount source"`) that `handle_to_rw_io` returns before ever reaching. Section header `// Symlink / reparse point reading (stub for now)` sits above 90 lines of complete MS-FSCC decoding. `dir_open`'s doc tells callers to check `dir_skipped`; the field's own doc says it is always 0. Crate-level `#![allow(clippy::too_many_arguments)]` and `needless_range_loop` are justified by constructs that are not in this file. | Medium | n/a |
| **C13** | `lib.rs:2131`, `2155`, `2201-2205`, `2251-2257`, `2266`, `2871-2875` | Magic number, 15+ sites | The GUID length `16` as a bare `copy_nonoverlapping` count across five object-id entry points, with no named constant and no caller-supplied length in the ABI. | Medium | partial |
| **C14** | `lib.rs:1145-1158`, `1271`, `1153`/`1427` | Magic number, duplicated | File-type codes (`1`, `2`, `7`, `8`) written as bare literals in two places, each with a comment naming the C enum it should reference. Reparse tags `0xA000_000C`/`0xA000_0003` appear in both `stat` and `readlink` with no shared definition. `make_dirent(0, 0, b"")` at 1233 passes file type `0`, which is not a defined value. | Medium | partial |

### D. `src/mkfs.rs` — the format path (2,857 lines)

The most concentrated write path in the crate, and the one carrying the most
hard-won knowledge — chkdsk event IDs, procmon traces, dated debugging notes. That
is exactly why the stale comments here rate High: on a format path, a stale comment
is indistinguishable from a spec claim, and the next reader will trust it.

| # | Location | Category | Finding | Sev | Covered |
|---|---|---|---|---|---|
| **D1** | `mkfs.rs:271-1411` | God function | `format_filesystem` is 1,141 lines — nine comment-marked phases, thirteen anonymous `{ }` blocks, and **57 function-scope locals**, 27 of which are layout values created around line 301-385 and still read at line 1407. Those 27 are the `Layout` struct that wants to exist. Phase `// 0. Zero out critical regions` (390-393) is a numbered heading with **no code under it**. *(This is the lead finding of the 2026-08-25 review too.)* | High | 24 integration files call it end-to-end; no in-file test |
| **D2** | `mkfs.rs:1-11` | Stale doc, every line | The module header's layout diagram claims `$LogFile` is 16 clusters / **64 KiB** at LCN 36; the code sets `LOGFILE_TARGET_BYTES = 0x3B_0000` = **3.78 MiB** (line 332). `$MFT` is not 32 fixed clusters — it is `(mft_record_size * 64).div_ceil(cluster_size)` (311). The stated LCNs for `$Bitmap` and `$UpCase` follow from the wrong `$LogFile` size. The diagram also omits `$AttrDef`, the MFT-internal bitmap cluster, and the two `$SDS` clusters. | High | n/a |
| **D3** | `mkfs.rs:938-955` vs `188-197` | Two comments contradicting each other about one field | The rec-9 comment says `$Secure` is a *"minimal resident stub… empty placeholders"* with `security_id` *"set to 0"*. The code 30 lines below builds a populated non-resident `$SDS` (1018-1027) plus populated `$SDH`/`$SII`, with `security_id = 0x100`. The block comment then asserts modern `format.com` names slot 9 `$Quota`; `rec::name`'s own doc asserts the exact opposite (*"Since we write NTFS 3.1 directly, rec 9 is `$Secure` at every cluster size"*). Both cite chkdsk evidence. | High | end-to-end only |
| **D4** | `mkfs.rs:1006-1015` | Comment wrong by 0x1C, write path | The comment states each `$Secure` entry is *"92 bytes padded to 96"* and the mirror *"ends at 0x40060"*. `SD_SYSFILE_RW` is 104 bytes, so the entry is 124 padded to 128 and the code correctly computes **0x4007C**. The code is right and the comment is wrong — a reader "fixing" the code to match would corrupt `$Secure`. | High | end-to-end only |
| **D5** | `mkfs.rs:309`, `477`, `849` | One value, three names | The `$Boot` size `8192` and its `.div_ceil(cluster_size)` are computed three separate times as `boot_clusters_for_layout`, `boot_clusters_bitmap`, `boot_clusters`. The comment at 343-347 records that precisely this kind of split **already caused an LCN collision that clobbered the `$MFT` bitmap**. The lesson was written down; the constant was not. | High | end-to-end only |
| **D6** | `mkfs.rs:1051`, `1066`, `1157`, `1210`, `1331`, `1484` | Magic number, 6 sites | `4096` as the index-block size, six independent literals. `build_populated_index_root_attr`'s own doc (2281-2283) warns that chkdsk reports `Corrupt master file table` if the boot sector's value and `$INDEX_ROOT`'s disagree — and nothing ties the six together. | High | end-to-end only |
| **D7** | `mkfs.rs:349` vs `2542-2561`, `762-766` | Guard compiled out in release | `attrdef_clusters` is sized from a hardcoded `16` entries; `build_attrdef_table` derives its size from the array. The only thing connecting them is a **`debug_assert!`**, which vanishes in the release build that actually formats disks. Add an entry to the table and the allocation silently under-sizes. | High | `build_attrdef_table` has 4 unit tests; the linkage has none |
| **D8** | `mkfs.rs:26-50` vs `record_build.rs:31-79`; `1785` vs `17`; `1789` vs `909`; `1858` vs `1010` | Whole-block duplication across files | 15 constants are byte-identical in both files. `encode_file_reference` is defined twice — and `mkfs.rs` already imports eight other symbols from `record_build`. `write_standard_information` and `write_file_name` also exist in both, **divergently**: mkfs's takes `security_id`, record_build's hardcodes SecurityId 0 — which the comment at 1803-1809 says makes chkdsk exit 13. | High | partial |
| **D9** | `mkfs.rs` §3 (7×, 12×, 3×, 4×) | Duplication with drift | The "single-run non-resident `$DATA`" block appears **7×**; the `sys_entries.push(...)` 5-tuple **12×**; the 0x42-byte `$FILE_NAME` layout **3×** — and those three have already drifted (`write_file_name` applies `FA_NTFS_VIEW_INDEX`, `build_file_name_stream` has no such parameter, though the doc at 2181-2184 says they must produce identical output). The `sequence = max(1, rec_num)` rule appears 4× and **line 1177 omits the `max(1, …)` guard**. | High | end-to-end only |
| **D10** | `mkfs.rs:1858`, `1789`, `1521`, `1552` | Parameter soup | `write_file_name` takes **12 parameters including three consecutive unlabelled `bool`s** — the call site at 1666 reads `…, is_dir, true, is_view_index, …`. `write_standard_information` takes 8 with three adjacent bools. `build_system_record` takes 8, called as `…, false, 0, 0, &[data_attr]`. Any transposition compiles. *(Related to M6 in the prior review.)* | High | end-to-end only |
| **D11** | `mkfs.rs:382-383` | Name means the opposite | `last_used_lcn` is assigned `sds_mirror_lcn + 1` — one **past** the last used LCN — and is then used in `>=` comparisons at 383 where that off-by-one decides whether the layout is rejected. The same line also fuses two unrelated layout invariants into one condition with one shared error message, so a failure cannot be attributed. | High | end-to-end only |
| **D12** | `mkfs.rs:1851`, `1854` | Mixed radix, adjacent lines | `rec[v + 32..v + 36]` followed by `rec[v + 0x34..v + 0x38]` — decimal then hex, for adjacent fields of the same buffer. Read together it looks like a 4-byte gap; it is 16. | High | end-to-end only |
| **D13** | `mkfs.rs:1467-1476`, `1484-1489`, `2062-2066`, `2336-2340` | One concept, three encodings | "Clusters per index block" is computed three different ways in one file: a signed-log2 branch, `-(x.trailing_zeros() as i8)`, and `index_block_size / 512`. The comment at 2321-2335 explains why they differ; nothing in the code links them. `cpib_raw` is not a raw form of `cpib` — it is a different quantity (size in bytes). | High | end-to-end only |
| **D14** | `mkfs.rs:548`, `1346` | Anonymous tuple | `sys_entries: Vec<(u32, &'static str, bool, u64, u64)>` — five positional fields whose meaning exists only in a comment on line 547, destructured 800 lines later. | Medium | end-to-end only |
| **D15** | `mkfs.rs:2381`, `570-578`; `1682-1687` vs `1760-1764` | Overlapping / asymmetric guards | `make_mft_internal_bitmap` **silently drops** out-of-range record numbers rather than erroring, while its caller already range-checks — two guards, neither reports. Separately, `build_system_record_with_parent` checks `cursor + attr.len() + 8 > rs` before writing; `build_reserved_placeholder` performs the same write with no check. | Medium | 4 unit tests assert the silent-drop as intended |
| **D16** | `mkfs.rs:198`, 26 call sites | Dead parameter | `rec::name(rec_num, _cluster_size)` documents its second parameter as unused and keeps it "for call-site symmetry" — making 26 call sites two lines longer each. | Medium | yes |

**In-file coverage:** `mod tests` (2597-2857) has 23 tests covering **6 of the file's
25 functions**, all leaf helpers — and two of those six actually test `upcase` and
`data_runs`, not mkfs. `format_filesystem`, `build_boot_sector`, `build_system_record`,
`write_standard_information`, `write_file_name`, and both `build_populated_*index_root_attr`
have no in-file test. The whole-volume output *is* well guarded — 24 integration files
call `format_filesystem` — but nothing guards a refactor of an individual byte-layout
helper, which is exactly what fixing D8/D9 would touch.

### E. Index and record assembly — `index_io.rs`, `record_build.rs`, `attr_io.rs`, `attr_resize.rs`, `idx_block.rs`

These mutate the B-tree that makes files findable. A wrong byte here is a volume
chkdsk rejects.

| # | Location | Category | Finding | Sev | Covered |
|---|---|---|---|---|---|
| **E1** | crate-wide | **No constants module** | There is no `src/constants.rs`. NTFS on-disk constants are declared per-file in 14 of 22 modules, and the same values are re-declared independently. Verified duplicates: `REC_OFF_BYTES_USED = 0x18` in **five** places (`record_build.rs:41`, `mkfs.rs:36`, `attr_io.rs:260`, `attr_resize.rs:17`, `index_io.rs:881`); `REC_OFF_BYTES_ALLOCATED` in four; the 48-bit file-reference mask in **eight**; `b"FILE"` in three; 8-byte alignment in **six** spellings (`align8`, `align_up_8`, four inline `(n+7)&!7`); the five `SI_*` offsets byte-identical in `read.rs:496-500` **and** `write.rs:37-41`; `VOLUME_RECORD_NUMBER = 3` and `VOLUME_IS_DIRTY = 0x0001` in both `read.rs:602/610` and `fsck.rs:47/56`. Sharpest case: `write.rs:567-570` privately re-declares `NONRES_ALLOCATED_LENGTH`/`DATA_LENGTH`/`INITIALIZED_LENGTH`/`LAST_VCN` — all four of which `attr_io::attr_off` already exports publicly, **from a module `write.rs` already imports on line 10**. Two shared groups do exist and work well (`attr_io::attr_off`, `idx_block`'s `pub const`s) — that is the pattern the rest should follow. Root cause of D8, D6, A9, B5 and much of §4 above. | High | n/a |
| **E2** | `index_io.rs:202-204`, `689-691` vs `21-22` | Shadowed constants | Two functions do `use crate::idx_block::{IH_FIRST_ENTRY_OFFSET, IH_TOTAL_SIZE_OF_ENTRIES, …};` **inside the function body**, shadowing `index_io`'s own module-level constants of the same names. Verified: the values agree today (`0`/`4` vs `0x00`/`0x04`) — but two functions in this file silently read different constants from the other four, and nothing enforces that they stay equal. | High | partial |
| **E3** | `index_io.rs:702-707` vs `:31` | Magic number where the name exists | The INDX room check that gates **every** index insert reads `allocated_size` as a bare `block[ih_start + 8 .. + 11]`, while `IH_ALLOCATED_SIZE_OF_ENTRIES: usize = 8` is defined at line 31 **of the same file** — with a doc citing the diagnostic run that made it necessary. Verified. | High | not covered — no test exercises this path |
| **E4** | `index_io.rs:98-145`, `229-272`, `306-354`, `598-621`, `720-741` | Duplication with **differing bounds checks** | The index-entry walk appears five times. Only `collect_entries` (322-332) clamps name reads to the entry (`entry_end`); the two insert-path copies (612, 733) slice `name_start .. name_start + name_len*2` with **no bound at all**. Five copies, three levels of strictness. | High | partial — the INDX-block copies are untested |
| **E5** | `index_io.rs:76-147` vs `222-274` | The shared helper that isn't shared | `find_index_entry`'s inner walk is a line-for-line copy of `scan_entries_for_name` — the helper whose own doc (220-221) calls it *"Shared scanner"*. `find_index_entry` never calls it. | High | yes (for the copy) |
| **E6** | `record_build.rs:350`, `481`, `504`, `548`, `619`, `713`, `920`, `965`, `1037` | Duplication, **proven to drift** | The 24-byte resident attribute header is written out byte-by-byte **nine times**, six of which could call the existing `build_resident_unnamed_attribute`. They have already diverged: only line 983 sets `indexed_flag = 1`, and its own comment says *"this builder predates that fix and was missing it"*. That is this finding's evidence, in the codebase's own words. | High | only via the individual builders; no test pins `indexed_flag` outside `$FILE_NAME` |
| **E7** | `record_build.rs:856-865`, `813-821`, `375-384`, `1010-1019`, `752-759` | Transposable parameters | `build_nonresident_attribute` takes 8 parameters including **three adjacent `u64`s** — `data_length, allocated_length, initialized_length`. The sparse variant takes 7 with **four** adjacent `u64`s. Swapping any two compiles cleanly and writes wrong sizes into a non-resident `$DATA` header. `build_record_inner`/`build_directory_record` take 8 each with two adjacent `u16`s (`sequence`, `bytes_per_sector`). | High | partial |
| **E8** | `attr_resize.rs:54-59` | Comment says the opposite of the code | `// Same size. Just write the new length field (already the same).` — the *attribute* length is unchanged, but `new_value_length` can differ, which is the entire reason the three lines below execute. The parenthetical contradicts them. | High | yes |
| **E9** | `index_io.rs:629-641` | Two offset bases across a mutation | `insertion_in_value = insertion_point - attr_val_start_old` → resize → re-find the attribute → `insertion_point = attr_val_start + insertion_in_value`, with a comment (634-636) admitting the recompute is a no-op *"but compute defensively anyway"* — in the highest-risk stretch of the insert path. Either the invariant holds and the code should say so, or it does not and the comment is wrong. | High | partial |
| **E10** | `record_build.rs:1024-1025` | Panic in an assembly path | `.expect("write_file_name: valid UTF-16 name must build cleanly")` — a panic mid-buffer-build in a write driver, on a condition the caller already screened at line 389. | High | no |
| **E11** | `record_build.rs:375` vs `238` | Name promises a factoring that doesn't exist | `build_record_inner` is **not** the shared inner of `build_directory_record`; the two are near-identical bodies with the record-finalisation logic transcribed twice. | High | partial |
| **E12** | `index_io.rs:567-666`, `414-479`; `attr_resize.rs:205-302` | God functions in mutation paths | `insert_entry_into_index_root_with_collation` does seven things in ~100 lines, including the pre-resize/post-resize offset dance of E9, and is not separable for testing. `remove_index_entry` handles both container kinds, tail shift, zero-fill, two header patches and a resident resize in one body. `insert_attribute_sorted` runs one loop that simultaneously searches for the insert position *and* the end marker, with two `Option` accumulators and three `break`s. | High | partial |
| **E13** | `index_io.rs:555-666` vs `673-755`; `attr_resize.rs:75-110` vs `169-194` | Same algorithm, two containers | The two `insert_entry_into_*_with_collation` functions are one algorithm duplicated rather than parameterised by the `BlockKind` enum that already exists at 481-486; the root version patches `allocated_size` and the INDX version deliberately does not, a distinction discoverable only in a comment. Likewise `resize_resident_value` and `replace_attribute` transcribe the same grow/shrink logic — and diverge on whether the grow branch zeroes. | High | `replace_attribute` has **no unit test at all** |
| **E14** | `attr_resize.rs` (21 sites) | Hand-rolled decodes | 21 manual `u32::from_le_bytes([...])` four-element decodes, in a file that already imports from `attr_io` — which has `read_u32_le` with bounds checking. Each unchecked index panics where the helper would return `None`. | Medium | partial |
| **E15** | `index_io.rs:48` | Stale suppression | `#[allow(dead_code)]` on `FN_NAMESPACE_OFFSET`, which **is** used at lines 326 and 534. It tells the reader the namespace field is unhandled. | Medium | n/a |
| **E16** | `attr_io.rs:16-31`, `34-52`, `164-183` | One table, three copies | The 14 attribute type codes are written out three times — enum discriminants, a hand-written `from_u32`, and a hand-written `attr_type_name`. The third knows about `0x100`; the other two do not. | Medium | partial |
| **E17** | `index_io.rs:786-814`; `idx_block.rs:200-201`; `record_build.rs:336` | Unreached code | `compare_names_ordinal` carries 19 lines of doc saying it is *"not yet wired into"* anything and the feature bit is *"not yet pinned"*. `INDX_USA_OFFSET_FIELD`/`INDX_USA_COUNT_FIELD` are `pub` and referenced nowhere in the workspace. `write_empty_index_root`'s `bytes_per_sector` parameter exists only to be discarded. | Medium | n/a |

### F. `tests/` — 92 files, 671 integration tests, 17,108 lines

The single largest pool of literal duplication in the repository, and — because a
shared helper module already exists and is barely used — the cheapest to collapse.

| # | Location | Category | Finding | Sev | Covered |
|---|---|---|---|---|---|
| **F1** | 45 files under `tests/` | Duplication, 45× | `fn working_copy(tag: &str) -> String` is defined **forty-five times**, each a four-line body identical but for a filename prefix string and which fixture constant it copies. Verified: 45 distinct definitions, 45 distinct prefixes, one shape. | High | n/a |
| **F2** | 22 files under `tests/` | Duplication **with meaningful drift**, 22× | The "make a fresh formatted volume" helper exists 22 times under **two names** — `fresh_vol` (13) and `fresh_volume` (9). They are not interchangeable: only `capi_rename_overwrite.rs` uniquifies the filename with an `AtomicU32` and returns an RAII `ImgGuard` that cleans up; the other 21 return a bare `String` with a fixed path and never delete it. Only `corruption_resistance.rs` calls `create_dir_all("test-disks")`, with the comment *"don't assume test-disks/ already exists (clean checkout)"* — so **21 of 22 fail on a clean checkout**, and the one that learned this never propagated the fix. They also split between `4096, 4096` literals and a `CLUSTER` const, and between `io.sync()` and `<PathIo as BlockIo>::sync(&mut io)`. | High | n/a |
| **F3** | 19 files under `tests/` | Duplication, 19× | `fn last_error() -> String` is **byte-identical** in 19 files. | Medium | n/a |
| **F4** | `tests/common/mod.rs` | Unused shared module | The module that F1-F3 should collapse into already exists (93 lines, `open`/`navigate`/`list_names`/`read_file_all`) — and **only 10 of 92** test files declare `mod common`. The infrastructure is built; adoption stopped. | Medium | n/a |
| **F5** | all `working_copy`/`fresh_vol` sites | Fixture hygiene | Images are written to a fixed `test-disks/_<prefix>_<tag>.img` and never removed, so they accumulate across runs; identical tags in different binaries can collide. `capi_rename_overwrite.rs` is the only file that solved this. | Medium | n/a |
| **F6** | `tests/` | Coverage gap | Only `tests/mkfs_bin_smoke.rs` executes the `rust-ntfs` binary, and only for `format`. See A13 — this is why section A exists. | High | n/a |

### G. Allocators, fixup and volume flags — `bitmap.rs`, `mft_bitmap.rs`, `mft_io.rs`, `fsck.rs`, `read.rs`, `data_runs.rs`, `block_io.rs`, and the small parsers

`bitmap.rs` and `mft_bitmap.rs` are the cluster and MFT-record allocators — the two
places where "I changed a byte" becomes "I changed what the volume believes it owns".
**G1 is the most important finding in this report**, and it is a readability problem
that has already become a real defect.

| # | Location | Category | Finding | Sev | Covered |
|---|---|---|---|---|---|
| **G1** | `mft_bitmap.rs:200-216` vs `:227-256`, `:190-192` | **One API, two storage backends — and rollback silently fails** | `read_bitmap_byte_io`'s `Resident` arm returns `bytes[i]` from **the snapshot captured at `locate_io` time**. `write_bitmap_byte_io`'s `Resident` arm writes through `update_mft_record_io` to **disk**. `bm: &MftBitmap` is immutable, so the snapshot is never refreshed — a write is invisible to the next read. Verified by reading both arms. The consequence, in `write.rs:1152-1160`: `allocate_io` sets the disk bit (cache still 0); if the record write then fails, the paired `free_io` → `mutate_bit_io` reads the **stale cache**, sees `!set && !cur`, and returns `Err("MFT record {n} already free")` — which `write.rs` discards with `let _ =`. **The bit stays set on disk and the MFT record is leaked.** Six rollback sites in `write.rs` (1157, 1161, 1184, 1347, 1351, 1372) are defeated the same way. Currently masked only because our own `mkfs` always emits the *non-resident* layout (see G10) — but the resident path is public API and any volume that has one hits it. | **High** | **none** — `mft_bitmap.rs` has 11 Resident tests and **all are read-only**; there is no mutate→read-back test, which is exactly what hides this |
| **G2** | `write.rs:1157`, `1161`, `1184`, `1347`, `1351`, `1372`, `2626`, `2633`, `2638`, `2674`, `2719`, `2921`, `2928`, `2933`, `2973` | Hand-written undo, 15× | Neither `bitmap.rs` nor `mft_bitmap.rs` offers a scoped guard, RAII type, or `allocate_then(…)` combinator, so every consumer re-derives the same cleanup — **fifteen times**, and every single one discards the cleanup's own error with `let _ =`. B3's ten-fold `free_all` is the same disease in one function; this is the crate-wide form, and G1 is what it costs. | **High** | happy paths only |
| **G3** | `bitmap.rs:147-149` | Dead code that misrepresents control flow | ```for (begin, finish) in [(scan_start, total), (0, scan_start.min(total))] { scan_start = begin; // silence unused-assignment warning; let _ = scan_start;``` — verified verbatim. The array is evaluated **before** the loop, so the second pass's bound uses the pre-loop value. The code is correct; the two statements have no effect and actively teach the reader that the second bound is mutated by the first pass. In the volume allocator's wrap-around scan. | **High** | `bitmap.rs` has 31 tests incl. wrap-around |
| **G4** | `bitmap.rs:283-296`, `:320-331`, `mft_bitmap.rs:265-271` | Reimplemented helper, 3× | Three hand-rolled VCN→run→disk-offset walks, all reimplementing `data_runs::vcn_to_lcn` (`data_runs.rs:137-147`). Two of the three are the cluster allocator's own read and write halves. | **High** | partial |
| **G5** | `bitmap.rs:268-304` vs `:306-340` | Twins, and the write twin has no rollback | The two functions are line-for-line identical except `read_exact_at`→`write_all_at`, the buffer direction, and a trailing `io.sync()` — ~35 duplicated lines on a mutation path. Worse: the write half issues **one `write_all_at` per data run**, so if run 2 fails after run 1 succeeded, the first chunk is already on disk, `io.sync()` never runs, and `Err` is returned with the on-disk `$Bitmap` in a state nobody tracks. `mutate_bits_io` validates every bit *before* touching its buffer, so the all-or-nothing intent is explicit — the write half breaks it. | **High** | **no test uses a fragmented (multi-run) `$Bitmap`** — precisely the case this concerns |
| **G6** | `bitmap.rs:130-190` | God function, 6 levels deep | `find_free_run_io` — the cluster allocator's search — runs 61 lines at **six nesting levels** with four interleaved cursors (`lcn`, `cursor`, `byte_idx`, `bit_in_byte`), the two-pass wrap-around, and G3's dead assignment. | **High** | yes |
| **G7** | `mft_io.rs:92-94` vs `:85-88` | Comment that lies, contradicting itself | `/// Does not validate the NTFS magic … — upstream `Ntfs::new` already does that during read-side parsing.` The `ntfs` crate is a **dev-dependency** (`Cargo.toml:43-48`, *"No production code path links it anymore"*). Nothing validates it on the read path but `read.rs:646`. And five lines above, `mft_io.rs:85-88` states the correct thing. Two doc comments in one file give opposite answers about who checks the boot magic. | **High** | n/a |
| **G8** | `fsck.rs:363`, `:370-371` | Bare version literals on a write path | `major == 1 && minor == 2 && current_flags & VOLUME_UPGRADE_ON_MOUNT != 0`, then `new_bytes[0] = 3; // major` / `new_bytes[1] = 1; // minor`. Four unnamed NTFS version numbers deciding and performing an on-disk upgrade, disambiguated only by trailing comments. The condition also mixes an unparenthesised `&` inside `&&`. | **High** | 3 tests cover the transition |
| **G9** | `fsck.rs:349` vs `read.rs:606-607` | Offset derived by arithmetic instead of named | `read.rs` names both fields (`VI_MAJOR = 8`, `VI_FLAGS = 10`). `fsck.rs` re-declares only the second, as `VOLUME_FLAGS_OFFSET: u64 = 10`, then recovers the first with `let major_disk_offset = flag_disk_offset - 2;`. That `- 2` is `VI_FLAGS - VI_MAJOR` open-coded — and it is the offset `upgrade_volume_version_io` writes four bytes to. (Also: `VOLUME_RECORD_NUMBER = 3` and `VOLUME_IS_DIRTY = 0x0001` are declared in both files, and `read::VOLUME_IS_DIRTY` is already `pub` and already consumed by `lib.rs:981` — so one on-disk bit is read through two constants.) | **High** | 3 tests |
| **G10** | `mft_bitmap.rs:29-42`, `:7` | A branch our formatter can never produce | The doc says *"On small volumes this bitmap is typically resident"*, but `mkfs.rs:579-596` **always** emits a non-resident `$MFT:$Bitmap`. So the entire Resident layout is unexercised by anything this crate produces — every Resident test hand-constructs the struct and only reads. That is what let G1 survive. Untested-and-wrong is worse than absent. | **High** | 11 read-only tests |
| **G11** | `bitmap.rs:224-230`, `mft_bitmap.rs:177-182` | Boolean-blind parameter on an allocator | `mutate_bits_io(io, bm, lcn, n, set: bool)` — 5 parameters — is called as `mutate_bits_io(io, bm, lcn, n, true)` (`bitmap.rs:205`). On an allocator, `true` = allocate is not self-evident at the call site. | **High** | yes |
| **G12** | `fsck.rs:87-133` vs `block_io.rs:76-134`; `fsck.rs:139-188` vs `block_io.rs:216-261` | Whole types duplicated across modules | `PathIo` is defined **twice**, same name, same fields, byte-identical `read_exact_at`/`write_all_at`/`size`/`sync`, differing only in constructors. `IoReader` likewise duplicates `IoReadSeek` — **including their test suites** (`fsck.rs:790-857` ≡ `block_io.rs:306-398`, 7 tests each). | Medium | duplicated tests |
| **G13** | `bitmap.rs:234`, `:104` | Unchecked add in the overflow guard | `if lcn + n > bm.total_bits` — the guard that exists to prevent an overrun wraps in release and bypasses itself. `mft_io.rs:310-312` and `data_runs.rs:113,127,203` both use `checked_add` for exactly this. The allocator is the one place that doesn't. | Medium | no |
| **G14** | `mft_bitmap.rs:88-97` vs `bitmap.rs:368-373` | Asymmetric contracts | `bitmap.rs`'s `is_allocated` rejects out-of-range with a clear error; the MFT sibling has **no range check at all** and relies on the layout erroring downstream. | Medium | partial |
| **G15** | `mft_io.rs:370-390` | Partial commit, undocumented | `update_mft_record_io` does `write_all_at(...)?` then `sync()?`. If the write lands and the sync fails, the record is on disk with an already-bumped USN and the caller sees only `Err`. The module's concurrency contract (4-29) is thorough about *external* writers and silent about this window. | Medium | yes (happy path) |
| **G16** | `mft_bitmap.rs:227-256` vs `bitmap.rs` | Three sync policies, none documented | Resident goes through `update_mft_record_io` (which fsyncs); NonResident does `write_all_at` + `sync` per byte; `bitmap.rs` syncs once per *range*. One conceptual operation, three durability regimes. | Medium | partial |
| **G17** | `read.rs:303-309`, `:395-403` vs `:172-176` | Silent fallback contradicting a documented contract | A truncated or corrupt run list is indistinguishable from a legitimate sparse hole — `vcn_to_lcn` returns `None` and the reader leaves zeros. `locate_attribute`'s doc 130 lines above advertises *"**Limitation (fails loud, never truncates)**"*. | Medium | partial |
| **G18** | `data_runs.rs:209-221` | Defensive code that is itself broken | In `signed_bytes_needed`, at `n_bytes == 8`, `half_range = 1i64 << 63 = i64::MIN`, so `-half_range` overflows — debug panic, release wrap. In the mapping-pairs **encoder**. Unreachable in practice (needs \|n\| ≥ 2⁵⁵ clusters) and no test reaches it. | Medium | no |
| **G19** | `data_runs.rs:85`, `:98`, `:114` | Diagnostics that misreport | The cursor is advanced *before* the error strings are formatted, so every `"run at offset {p}"` is off by 1..=9 bytes — actively misleading when debugging a corrupt run list, which is the only time anyone reads them. | Medium | no |
| **G20** | `read.rs:1-11`, `fsck.rs:2-4`, `:6-16`, `:439-441`, `:287-296`, `block_io.rs:33-38`, `:16-23` | Stale module and section docs | `read.rs` still calls itself *"work in progress… Phase 1: path resolution"*. `fsck.rs` says *"The two operations we support"* for a module exporting six, and claims *"Everything in this module breaks the otherwise read-only invariant of the crate"* — for a crate with `write.rs` and `mkfs.rs`. Its `// Internals — NTFS parsing via an IoReader` header sits above two functions that go through `read::` natively. A 9-line rustdoc about a progress-callback type that doesn't exist is glued to the front of `is_dirty_io`'s published docs. `block_io.rs` frames `IoReadSeek` as production API (zero non-test uses) and carries a 0.2.0 migration changelog in its module doc. | Medium | n/a |
| **G21** | `fsck.rs:617` | Test name contradicts its assertions | `fn fresh_mkfs_volume_is_1_2_with_modified_by_chkdsk_flag()` asserts `major_version() == 3` and `minor_version() == 1`. Its own doc comment says 3.1 too. A pre-upgrade fossil left in the name. | Medium | n/a |
| **G22** | `fsck.rs:79-82` | Phantom distinction | `trait FsckIo: BlockIo {}` — zero methods, blanket impl, kept *"so the fsck routines keep their `T: FsckIo` bounds unchanged"*. Every fsck signature therefore advertises a distinction from `BlockIo` that does not exist. | Medium | n/a |
| **G23** | `fsck.rs:337-355` | Guarantee compiled out of release | The 10-line doc explains that the single 4-byte version+flags write is torn-write-free *only* if the bytes sit inside one 512-byte sector, and says the assert *"makes the requirement explicit rather than hoping a future layout change doesn't silently break the guarantee."* It is a `debug_assert!`. In release it evaporates and the guarantee is exactly the hope it was written to replace. Same shape as D7. | Medium | debug builds only |
| **G24** | `read.rs:873-923` | The last raw-literal parser | `parse_attribute_list` reads every field with bare `0x1A`, `+4`, `+5`, `+6`, `+7`, `8..16`, `16..24`, `+24`, `+25` — the only on-disk structure in the crate with **zero** named offsets. | Medium | 1 test |
| **G25** | `read.rs:178-232` | Unnamed tuple under a suppression | `locate_attribute` returns a bare `(BootParams, u64, Vec<u8>, AttrLocation)` behind `#[allow(clippy::type_complexity)]`; callers destructure it four different ways and the `u64` is never named anywhere in the type. | Medium | yes |
| **G26** | 10 files (`MemDev`), 7 files (format fixture) | Test-double duplication | A `MemDev` + `impl BlockIo` in-memory device is written out **ten times** across `read.rs`, `fsck.rs`, `mft_io.rs`, `bitmap.rs` (×2), `mft_bitmap.rs`, `block_io.rs`, `upcase.rs`, `idx_block.rs`, `write.rs` — **with four different bounds-check behaviours**. The `format_filesystem` fixture helper appears seven more times under three names (`fresh_vol`, `fresh_dev`, `formatted_dev`). The in-crate counterpart to F1/F2. | Medium | n/a |
| **G27** | `ea_io.rs:123-125` | Doc promises data it doesn't return | `/// Read the current EAs + $EA_INFORMATION state from a record.` on `fn read_from_record(record: &[u8]) -> Result<Vec<Ea>, String>` — no `$EA_INFORMATION` is read or returned. | Medium | 27 tests on the pure functions |

**Coverage gaps that matter here:** `mft_bitmap.rs` has **zero** Resident mutate→read-back
tests (hides G1); `bitmap.rs` has no fragmented multi-run `$Bitmap` test (the exact case
G5 concerns); `read_compressed_nonresident` (72 lines) has no end-to-end test; and
`fs_core_bridge.rs`'s `is_writable() == false` guard is untested. `data_runs.rs` (36
tests, including a named regression pinning the Event-55/`$BadClus` fix) and `mft_io.rs`
(34) are the best-covered modules in the crate and should be the model.

### Root causes (cross-cutting)

Two crate-wide shapes that a large fraction of the findings above are downstream of.
Neither is a local fix, and neither should be attempted casually — but knowing they
are there explains why the same smell keeps reappearing in unrelated modules.

| # | Location | Category | Finding | Sev |
|---|---|---|---|---|
| **R1** | crate-wide | Stringly-typed errors | There is **no error enum anywhere in `src/`** (verified: zero `enum *Error`, no `thiserror`). Every module returns `Result<_, String>` — 167 distinct error-message literals crate-wide, 107 of them in `write.rs`. This is the **root cause of C2**: the C ABI's only route to an errno is to grep English prose, which is why 79% of write errors reach FSKit as EIO. Every other finding in this report is local; this one is the shape of the crate. | High |
| **R2** | crate-wide | No constants home | See E1. Also the root of D6, D8, A9, B5, G9 and the 112 unnamed offsets the prior review counted. Two working examples already exist in-tree — `attr_io::attr_off` and `mkfs::rec` — and neither is used by the modules that redeclare their values. | Medium |

### Rolled up

Roughly 60 further Low-severity items were found and are **not** listed individually,
because listing them would bury the rest: WHAT-restating field-label comments
(~60 in `record_build.rs`, ~12 in `build_boot_sector`, and `read.rs` repeating the
parenthetical *"(no upstream `ntfs` crate)"* on six separate doc comments),
single-letter names (`b`, `v`, `k`, `e`, `ih`, `mp`, `rs`, `bps`, `vo`, `vl`, `cs`,
`si`, `vi`) carried across long stretches of offset arithmetic, redundant
`if !value.is_empty()` guards around `copy_from_slice`, unreachable `unwrap_or`s,
return values discarded at every call site (`mft_bitmap.rs:258`, `sds.rs:85`), internal
milestone references that mean nothing to an outside reader (*"Good enough for W2"*,
*"future W2.6 work"*, *"Phase 2.4"*), an error string ending in a question mark
(`bitmap.rs:290`), and rustfmt having mangled several trailing-comment columns into
ragged continuations. Three representative examples are listed as A10-A12. If any of
the areas above is refactored these fall out with it; on their own they are not worth
a commit.

Worth noting in the other direction: `data_runs.rs:53` carries a four-line WHY comment
on a genuinely dense bit-mask expression that explains exactly why it is written that
way. That is the house standard the rest of the crate should be measured against, and
several modules already meet it.

---

## What to fix first

The ordering below prefers, per the triage rule, changes where coverage already
exists — with two deliberate exceptions at the top, both of which are "write the
missing test first, because the code cannot be touched safely without it".

**0. G1 — the MFT-bitmap stale cache. Before anything else.**
This is the one finding that is not merely a readability risk: it is a live defect
that fifteen rollback sites depend on and none of them detect. The order is
(a) add a Resident mutate→read-back test to `mft_bitmap.rs` — it will fail;
(b) make the resident read go to the same place the resident write goes, or make the
layout carry a mutable cache that the write updates;
(c) then stop `write.rs` discarding rollback errors with `let _ =`.
It is currently masked because our own `mkfs` only emits the non-resident layout
(G10), so there is no urgency in the sense of "users are hitting it today" — but that
masking is accidental, the resident path is public API, and the fix is small while the
knowledge is fresh. Doing this first also makes G2's shared-guard refactor safe.

**1. Write the CLI's missing tests before touching the CLI (F6).**
Everything in section A is uncovered, so nothing in section A can be fixed safely
today. One test file that spawns the binary and asserts exit codes and stderr for all
twelve verbs turns eleven untestable findings into eleven mechanical ones. This is
the highest-leverage single file in the plan and it adds no risk, because it changes
no production code.

**2. A2, then A1, then A3 — in that order.**
A2 is the only CLI finding where the tool silently does something other than what was
asked, on a write path, and reports success. A1 is a documented contract the code does
not honour. A3 is the one that can delete the wrong thing. All three become one-line
changes once step 1 exists.

**3. B2 — the rename basename validator.**
This is the single most important finding in the report: five copies of one check,
one of which is weaker than the other four, and that one is the copy reachable from
both the Rust facade and the C ABI. It is a two-line fix plus a test, and existing
`write_rename.rs` / `capi_rename.rs` coverage means the fix is verifiable immediately.
Extracting the check into one named function is what stops it recurring.

**4. B1, D2, D3, D4 — the four lying comments on write paths.**
No behaviour changes, no test risk, and they are actively misleading anyone reading
the format and mutation paths right now. D4 is the sharpest: a reader who "corrects"
the code to match its comment corrupts `$Secure`. These are the cheapest High items
in the report.

**5. F1-F4 — collapse the test helpers into `tests/common/`.**
45 + 22 + 19 = 86 duplicated definitions into three shared ones, in a module that
already exists. The compiler proves this refactor. It also fixes the clean-checkout
failure (F2) for all 21 files at once, and gives every future test the RAII cleanup
that only one file currently has.

**6. E1/R2 — create `src/constants.rs` and move the duplicated on-disk offsets into it.**
Start with the ones that are demonstrably declared more than once (`REC_OFF_*`, the
`IH_*` pair, the 48-bit mask, `b"FILE"`, the align-8 helpers). This is the enabling
change for D6, D8, B5, E3 and C13 — none of which are worth doing individually first,
and all of which are near-trivial afterwards.

**7. G2, G3, G4, G5 — the allocator cluster.**
G3 is a two-line deletion. G4 collapses three hand-rolled walks onto the
`data_runs::vcn_to_lcn` that already exists. G5 folds the read/write byte-range twins
into one direction-parameterised walker — and while doing it, gives the write half the
all-or-nothing behaviour `mutate_bits_io` already assumes it has. G2 (one shared
allocator guard replacing fifteen hand-written undos) should come last of the four,
after step 0 has made rollback actually work, and it subsumes B3.

**8. E2, E3, E5, E6 — the index-mutation duplicates that have already drifted or shadow.**
Do these only after step 6, and one at a time under `dev-loop`. E13 flags that
`replace_attribute` has **no unit test at all**, so that one needs a test written
first, same as step 1.

**9. G7, G20, G21 and the remaining stale docs; G12's duplicated `PathIo`/`IoReader`.**
Zero behaviour change. G12 in particular deletes two whole duplicated types *and*
seven duplicated tests, so it makes the crate smaller.

**10. Defer D1 and R1.**
`format_filesystem` (D1) is a real god function, but the prior review already
recommended it and it is a large, high-risk change guarded only by end-to-end tests —
it should be its own piece of work with its own plan, starting with the pure
`plan_layout` extraction (lines 301-385), which has no I/O and would let the 27
long-lived locals become a struct. R1 (typed errors) is an architecture change, not a
readability fix; but it is worth noting that C2's real remedy is R1, and that until
then the errno the app sees is a guess.

---

## Test results

No tests were run to completion and no code changed, so there is no before/after.
The baseline snapshot as it stands:

| Measure | Value |
|---|---|
| `#[test]` in `src/*.rs` (unit) | 576 |
| `#[test]` in `src/bin/rust_ntfs/*.rs` | **4** (all `gen_pattern`, all in `sparse.rs`) |
| `#[test]` in `tests/*.rs` (integration) | 671 |
| **Total** | **1,251** |
| Integration test files | 92 |
| …that use `tests/common/` | 10 |
| …that execute the `rust-ntfs` binary | **1** (`format` happy path only) |
| `cargo clippy --all-targets --locked` | **exit 0**, no warnings |
| clippy enforcement | blocking pre-commit hook (`.githooks/pre-commit.d/rust-clippy.sh`) |

The headline number is the second row against the third. 1,251 tests, and four of them
touch a twelve-verb CLI — which is the mechanical reason the divergence in section A
was able to accumulate.

Four specific coverage holes are load-bearing for the findings above, in the sense that
closing them would have surfaced the finding on its own:

| Hole | What it hides |
|---|---|
| `mft_bitmap.rs` — 11 Resident tests, **all read-only**, no mutate→read-back | **G1**, the stale-cache defect |
| `bitmap.rs` — no fragmented (multi-run) `$Bitmap` fixture | **G5**, the partial multi-chunk write |
| `attr_resize.rs` — `replace_attribute` has **no unit test at all** | **E13**, the duplicated grow/shrink logic |
| `index_io.rs` — nothing exercises `insert_entry_into_indx_block*`, `find_entry_in_indx_block`, `BlockKind::IndexAllocation`, the `allocated_size` invariant, or the upcase collation branch | **E3**, **E4** |

By contrast `data_runs.rs` (36 tests, including a named regression pinning the
Event-55/`$BadClus` fix) and `mft_io.rs` (34) are the best-covered modules in the crate
and are the model the allocators should be brought up to.

---

## Working-tree state

Unchanged apart from this file. Still present and untouched:

- `vendor/fs-test-harness` — modified submodule, another agent's work
- `docs/testing/01-strategy-and-the-contract.html` — untracked, someone else's
- `docs/testing/_style-comparison.md` — untracked, someone else's
- `stash@{0}` — untouched

No `git stash`, `git checkout`, `git add`, `git commit` or branch operation was run at
any point.
