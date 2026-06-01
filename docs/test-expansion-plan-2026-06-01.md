# Test Expansion Plan — 2026-06-01

Goal: dramatically expand situational test coverage across every layer of the
codebase — unit isolation, field-level exhaustion, disk image round-trips, and
edge-case stress. The project is integration-heavy (77 test files, ~503 tests)
but thin on unit isolation for critical infrastructure modules and lacks
systematic field-level coverage for NTFS on-disk structures.

---

## Current State Summary

| Layer | Test Files | Test Count | Status |
|-------|-----------|-----------|--------|
| Unit (`#[cfg(test)]`) | ~10 modules | ~56 | Thin; missing for 8 modules |
| Integration (`tests/`) | 77 files | ~503 | Good breadth, poor field coverage |
| Disk image (42-scenario matrix) | 1 harness | 42 | Passing; chkdsk ceiling reached |
| Fuzz / corruption | 1 file | 7 | Minimal |

Modules with **zero unit tests**: `attr_io`, `block_io`, `ea_io`, `fs_core_bridge`,
`idx_block`, `index_io`, `mft_bitmap`, `upcase`.

---

## Phase 1 — Unit Test Isolation for Core Infrastructure

### 1.1 `attr_io.rs` — Attribute Iteration and Location

Every public function needs its own test operating on a hand-built in-memory MFT
record. No disk required.

Tests to add (inside `src/attr_io.rs` `#[cfg(test)]`):

- `attr_iter_empty_record` — record with only end-marker; iterator yields nothing
- `attr_iter_single_resident` — single `$STANDARD_INFORMATION`; correct type, offset, length
- `attr_iter_two_attrs` — `$STANDARD_INFORMATION` then `$FILE_NAME`; correct order
- `attr_iter_stops_at_end_marker` — end-marker at various 8-byte-aligned offsets
- `attr_iter_nonresident_attr` — non-resident attr; mapping-pairs pointer, lowest VCN, highest VCN all readable
- `attr_find_by_type_present` — locate `$DATA` (type 0x80) by type; returns correct offset
- `attr_find_by_type_absent` — type not present; returns None / correct error
- `attr_find_by_type_and_name_present` — named `$DATA` (ADS); matches by type+name
- `attr_find_by_type_and_name_wrong_name` — same type, different name; returns None
- `attr_find_nth_of_type` — record with two `$FILE_NAME` attrs; find second one
- `attr_location_resident_bounds` — resident attr content slice is within record bounds
- `attr_location_oob_header` — attr header that claims to extend past record end; safe error
- `attr_id_next_unused` — records with IDs 0,1,3 (gap at 2); next unused = 2 or 4 depending on policy
- `attr_id_wraparound` — all 65535 attr IDs used; returns error or wraps correctly

### 1.2 `attr_resize.rs` — Resident Attribute Resizing

Tests operating on a single 1024-byte MFT record buffer:

- `resize_grow_simple` — grow `$DATA` by 8 bytes; trailing attrs shift right; end-marker intact
- `resize_shrink_simple` — shrink by 8 bytes; attrs shift left; no garbage bytes exposed
- `resize_to_zero_content` — shrink content length to 0; value-length field = 0; attr header intact
- `resize_at_record_boundary` — grow causes exact fit to record end; succeeds
- `resize_overflow` — grow would exceed record end; returns error (no mutation)
- `resize_preserves_subsequent_attrs` — three attrs; resize middle one; first and third unchanged
- `resize_8byte_alignment` — new length not 8-byte aligned; rounded up; padding correct
- `resize_updates_attr_length_field` — after resize, attr.length field reflects new size
- `resize_end_marker_never_overwritten` — end-marker type (0xFFFFFFFF) always at correct position
- `resize_cascading_shift` — 5 attrs; shrink then grow middle one; net zero; all attrs intact
- `resize_attr_id_preserved` — attr_id of resized attr unchanged
- `resize_first_attr` — resize the first attr in the record; subsequent attrs shift correctly

### 1.3 `mft_bitmap.rs` — MFT Record Allocation

Tests using a freshly formatted in-memory bitmap buffer:

- `alloc_first_record` — empty bitmap; allocate returns record 0
- `alloc_sequential` — allocate N times; returns 0,1,2,...N-1
- `free_and_realloc` — alloc 3, free 1, alloc again; returns freed slot (hint-based)
- `free_unallocated` — free a record that was never allocated; error or no-op
- `alloc_hint_respected` — hint=10; next alloc starts scanning from 10; avoids low records if free
- `alloc_near_full` — bitmap with one free bit; alloc succeeds; bitmap now full
- `alloc_full_bitmap` — completely full bitmap; alloc returns error
- `bitmap_extend_on_demand` — bitmap at capacity; extension adds new page; alloc succeeds
- `count_free_records` — known pattern; count_free matches expected
- `alloc_preserves_system_records` — records 0–15 reserved; allocator never returns them
- `range_alloc_contiguous` — allocate a contiguous run of N records; all returned
- `range_alloc_fragmented` — no run of N exists; returns fragmented list or error per policy

### 1.4 `idx_block.rs` — INDEX_ALLOCATION Block Management

Tests on a raw 4096-byte INDEX_ALLOCATION block buffer:

- `parse_valid_index_block` — well-formed INDX signature; all fields parse correctly
- `parse_invalid_signature` — signature != "INDX"; returns error
- `parse_usa_applied_correctly` — USA fixup applied; sector end-words restored
- `vcn_to_offset_simple` — VCN 0 maps to block offset 0; VCN 1 maps to block size
- `vcn_to_offset_cluster_aligned` — with 4096-byte cluster; offsets cluster-aligned
- `index_entries_iterate` — block with 3 entries; iterator yields all 3 in order
- `index_entries_empty_block` — no entries; iterator yields nothing
- `insert_entry` — insert new entry; binary-search position correct; existing entries intact
- `insert_entry_full_block` — block at capacity; insert returns BlockFull error
- `remove_entry_present` — remove existing entry; block compacted; remaining entries intact
- `remove_entry_absent` — remove non-existent key; returns error / no mutation
- `leaf_vs_node_flag` — NODE flag set means child VCN pointer present; parsed correctly
- `child_vcn_pointer` — node entry's child VCN read correctly from end of entry

### 1.5 `index_io.rs` — Index Entry Location, Insertion, Removal

Tests using a formatted in-memory volume (mkfs + operate on root $I30):

- `find_entry_present` — create file "foo"; find "foo" in index; returns correct MFT ref
- `find_entry_absent` — look up "bar" which doesn't exist; returns NotFound
- `find_case_insensitive` — NTFS names are case-folded via upcase; "FOO" finds "foo"
- `insert_into_root_simple` — insert one entry into empty $INDEX_ROOT; root node updated
- `insert_maintains_sorted_order` — insert "zoo", "apple", "mango"; order = apple, mango, zoo
- `insert_duplicate_name` — insert name that already exists; returns AlreadyExists
- `remove_from_root` — insert then remove; root node empty again
- `remove_last_entry` — remove the only entry; root node collapses correctly
- `name_comparison_ordinal_ascii` — "a" < "b" ordinal; "A" < "a" ordinal (uppercase < lowercase)
- `name_comparison_ordinal_unicode` — code-point ordering for U+0400 range
- `name_comparison_length_tiebreak` — "abc" < "abcd" because shorter wins on prefix tie
- `root_to_allocation_overflow` — fill root past threshold; allocation block created; all entries findable
- `allocation_block_inserted_entries_findable` — entries in allocation block found by find_entry
- `remove_from_allocation_block` — remove an entry that lives in an allocation block

### 1.6 `block_io.rs` — Block Device Abstraction

Tests for PathIo and CallbackBlockIo independently:

- `path_io_read_within_bounds` — write known bytes to temp file; PathIo reads correct slice
- `path_io_read_oob` — read beyond file end; returns error or truncated data per policy
- `path_io_write_and_read_back` — write 512 bytes at offset 512; read back; identical
- `path_io_sector_alignment` — reads at non-sector-aligned offsets; behaviour documented and tested
- `callback_io_read_dispatches` — CallbackBlockIo calls read callback with correct args
- `callback_io_write_dispatches` — write callback receives correct offset, length, data
- `callback_io_error_propagation` — callback returns error code; BlockIo returns same error
- `callback_io_null_write_callback` — write callback not provided; write returns ReadOnly error

---

## Phase 2 — Field-Level Exhaustion for On-Disk Structures

Every NTFS on-disk structure should have tests that write every known field value
and read it back, verifying the round-trip at the byte level.

### 2.1 `$STANDARD_INFORMATION` — All Fields

Create a file, set each field via the write API, remount, read back via stat/getattr:

- `si_created_time_roundtrip` — set crtime to epoch, max FILETIME, mid-range value
- `si_modified_time_roundtrip` — set mtime to all three sentinel values
- `si_mft_modified_time_roundtrip` — set ctime (MFT change) independently
- `si_accessed_time_roundtrip` — set atime independently
- `si_all_times_independent` — set all 4 times to 4 different values; all read back correctly
- `si_dos_attributes_archive` — set FILE_ATTRIBUTE_ARCHIVE; read back; bit set
- `si_dos_attributes_readonly` — set FILE_ATTRIBUTE_READONLY
- `si_dos_attributes_hidden` — set FILE_ATTRIBUTE_HIDDEN
- `si_dos_attributes_system` — set FILE_ATTRIBUTE_SYSTEM
- `si_dos_attributes_combinations` — all 4 combined; read back; all bits intact
- `si_max_versions_field` — write max_versions=0,1,0xFFFFFFFF; read back
- `si_version_field` — write version=0,1,0xFFFFFFFF; read back
- `si_class_id_field` — write class_id sentinel values; read back
- `si_owner_id_field` — write owner_id = 0, 256, 0xFFFFFFFF; read back
- `si_security_id_field` — write security_id; read back (must be valid entry in $Secure)
- `si_quota_charged_field` — write quota_charged = 0, max; read back
- `si_usn_field` — write usn = 0, large value; read back

### 2.2 `$FILE_NAME` — All Fields

> **Known limitations (not bugs):** `$FILE_NAME.allocated_size`, `data_size`, and
> `file_attributes` are snapshot values written at link-creation / rename time and
> maintained loosely — Windows does not keep them in sync on every write. chkdsk does
> not flag stale values here as corruption. See `docs/spec/sections/04-indexes-directories.md`
> §88-91 and coverage.md row 109. Tests covering these fields are `#[ignore]`d with a
> reference to this section until a flush/close path is implemented.

- `fn_parent_mft_ref_roundtrip` — verify parent MFT ref matches directory after link
- `fn_allocated_size_roundtrip` — non-resident file; allocated_size = cluster multiple *(#[ignore]: known gap above)*
- `fn_data_size_roundtrip` — file of known size; data_size field matches *(#[ignore]: known gap above)*
- `fn_flags_archive` — FILE_ATTRIBUTE_ARCHIVE set on creation
- `fn_flags_directory` — directory file; IS_DIRECTORY bit set (FA_NTFS_DIRECTORY = 0x10000000)
- `fn_flags_reparse_point` — symlink; IS_REPARSE_POINT bit set
- `fn_namespace_posix` — POSIX namespace (0x00) — case-sensitive
- `fn_namespace_win32` — Win32 namespace (0x01)
- `fn_namespace_dos` — DOS namespace (0x02) — 8.3 name
- `fn_namespace_win32_and_dos` — combined namespace (0x03)
- `fn_filename_length_single_char` — 1-char name; length=1; UCS-2 encoded correctly
- `fn_filename_length_max` — 255-char name (NTFS max); all chars round-trip
- `fn_ea_size_field_nonzero` — file with EAs; EA_SIZE in $FILE_NAME populated
- `fn_reparse_tag_field` — symlink; reparse_tag in $FILE_NAME = IO_REPARSE_TAG_SYMLINK

### 2.3 `$DATA` Attribute — Resident and Non-Resident Forms

- `data_resident_zero_bytes` — empty resident $DATA; length=0; no content bytes
- `data_resident_1_byte` — single byte round-trip
- `data_resident_at_threshold_boundary` — exactly at resident threshold; stays resident
- `data_resident_one_over_threshold` — one byte over; promoted to non-resident; data intact
- `data_nonresident_lowest_vcn` — lowest_vcn = 0 for normal file
- `data_nonresident_highest_vcn` — highest_vcn = (file_size - 1) / cluster_size
- `data_nonresident_allocated_size` — always a cluster multiple
- `data_nonresident_initialized_size` — equals data_size for non-sparse files
- `data_nonresident_mapping_pairs` — encode/decode round-trip for each mapping-pair type
- `data_named_stream_resident` — ADS "foo:bar" stays resident below threshold
- `data_named_stream_nonresident` — ADS grows past threshold; promoted correctly
- `data_compression_unit_zero` — non-compressed file; compression_unit = 0

### 2.4 Data Runs (Mapping Pairs) — Exhaustive Encoding

These feed directly into `data_runs.rs` unit tests (expand existing 11 tests):

- `encode_single_run_small_offset` — lcn fits in 1 byte; length fits in 1 byte
- `encode_single_run_large_offset` — lcn requires 8 bytes; encoded correctly
- `encode_single_run_zero_length` — length = 0; handled as sentinel or error
- `encode_multiple_runs_sequential` — 3 runs; each LCN is delta from previous
- `encode_multiple_runs_backwards` — negative LCN delta (fragmented file); signed encoding
- `encode_sparse_run` — LCN = 0 (sparse hole); offset bytes = 0; length bytes nonzero
- `decode_single_run` — decode back; LCN and length match
- `decode_multiple_runs` — decode 5 runs; all match original
- `decode_terminator` — terminator byte (0x00); iterator stops
- `decode_oob_header` — header claims more bytes than buffer; returns error
- `roundtrip_all_length_byte_counts` — length from 1 to 8 bytes; all encode+decode correctly
- `roundtrip_all_offset_byte_counts` — offset from 0 to 8 bytes; all encode+decode correctly
- `roundtrip_large_volume` — LCN > 2^32; requires 5-byte encoding
- `roundtrip_huge_run_length` — run length > 2^32 clusters; requires 5-byte encoding

### 2.5 `$INDEX_ROOT` and `$INDEX_ALLOCATION` — All Fields

- `index_root_attr_type_filename` — attr_type = $FILE_NAME (0x30)
- `index_root_collation_rule_filename` — collation = COLLATION_FILE_NAME (0x01)
- `index_root_index_block_size` — matches cluster size or 4096 (whichever larger)
- `index_root_clusters_per_index_block` — matches block_size / cluster_size
- `index_node_flags_leaf` — leaf flag (0x00) — no child VCN pointers
- `index_node_flags_node` — node flag (0x01) — has child VCN pointers
- `index_entry_flags_last` — last-entry flag (0x02) set on sentinel entry
- `index_entry_mft_ref` — MFT reference in entry matches target file's inode
- `index_entry_key_length` — key_length matches sizeof($FILE_NAME) for that entry
- `index_entry_data_offset` — data_offset points past key to index data area

### 2.6 Extended Attributes — All Fields and Boundary Values

- `ea_roundtrip_min_name` — EA name = 1 char
- `ea_roundtrip_max_name` — EA name = 255 chars (max per spec)
- `ea_roundtrip_zero_value` — value length = 0
- `ea_roundtrip_1_byte_value` — value = single byte
- `ea_roundtrip_65535_byte_value` — value = 65535 bytes (max per spec)
- `ea_roundtrip_flag_need_ea` — NEED_EA flag (0x80) set; read back
- `ea_roundtrip_multiple_attrs` — 3 EAs on same file; all round-trip
- `ea_roundtrip_ea_only_file` — file with EAs but no $DATA stream
- `ea_remove_and_readd` — remove one EA, add different one; others unchanged
- `ea_name_case_sensitivity` — EA names are case-sensitive in NTFS; "FOO" != "foo"
- `ea_padding_alignment` — each EA entry is 4-byte aligned; verify pad bytes are zero
- `ea_total_size_field` — $EA_INFORMATION total_size field matches sum of entries

### 2.7 Reparse Points — All Known Tags

- `reparse_symlink_absolute` — absolute symlink (\\?\\C:\\foo); print-name and sub-name both correct
- `reparse_symlink_relative` — relative symlink (../sibling); RELATIVE flag set
- `reparse_symlink_relative_deep` — ../../grandparent/sibling
- `reparse_mount_point` — mount point reparse tag (0xA0000003); buffer structure correct
- `reparse_lx_symlink_tag` — Linux WSL symlink tag (0xA000001D); raw buffer preserved
- `reparse_wof_tag` — Windows Overlay Filter tag (0x80000017); raw buffer preserved  
- `reparse_appexeclink_tag` — (0x8000001B); raw buffer preserved
- `reparse_unknown_tag` — arbitrary unknown tag; written and read back unchanged
- `reparse_max_buffer_size` — 16KB reparse buffer (spec maximum); stored + retrieved
- `reparse_zero_buffer` — zero-length reparse buffer; tag only
- `reparse_flag_in_file_name` — $FILE_NAME IS_REPARSE_POINT bit set when reparse attr present

### 2.8 Object IDs — All Variants

- `object_id_16_byte_form` — basic GUID only; 16 bytes; round-trip
- `object_id_64_byte_form` — full extended form: GUID + birth-volume + birth-object + domain; all 4 round-trip
- `object_id_64_byte_zero_birth_guids` — extended form with birth GUIDs = all-zero; valid
- `object_id_64_byte_mixed` — some GUIDs zero, some nonzero; all correct after round-trip
- `object_id_random_guid` — random GUID bytes; no validation constraints; stored verbatim
- `object_id_overwrite` — set object ID, set different one; new value survives
- `object_id_remove` — set then remove; object ID attr gone; stat shows no object ID

---

## Phase 3 — Disk Image Tests for Real-World Scenarios

Each test here formats a fresh NTFS volume, populates data, unmounts, and either
re-mounts to verify or runs through the 42-scenario Windows chkdsk matrix.

### 3.1 Cluster Size Coverage (expand existing matrix)

Current: 512B, 1KB, 4KB, 64KB cluster sizes.
Add: 2KB, 8KB, 16KB, 32KB cluster sizes.

For each new cluster size:
- Basic file create/read/write/delete
- MFT record overflow (resident → non-resident threshold)
- Directory with >500 entries (forces $I30 allocation)
- Large file (>1 cluster per MFT record requires data runs)

### 3.2 Volume Size Coverage

- `tiny_volume_1mb` — 1 MB volume; minimal MFT; format + basic ops
- `small_volume_32mb` — current standard test size
- `medium_volume_256mb` — format; fill 75% with files; verify bitmap consistency
- `large_volume_2gb` — format; write 1 GB file; non-resident data runs span many clusters
- `max_32bit_volume_4gb` — LCN values require 32-bit encoding; data runs encode correctly

### 3.3 MFT Fragmentation and Pressure

- `mft_fills_first_zone` — create enough files to exhaust initial MFT zone; MFT extension allocated
- `mft_extension_files_accessible` — files created after MFT extension are accessible
- `mft_delete_and_reuse` — create 1000 files, delete 500 odd-indexed, create 500 new; all 1000 accessible
- `mft_near_full` — fill volume to 99% of MFT capacity; operations near boundary
- `mft_record_number_gaps` — freed MFT records reused; inode numbers recycled correctly

### 3.4 Index Overflow (Large Directories)

- `dir_100_entries` — 100 files; all findable; $I30 uses index allocation
- `dir_1000_entries` — 1000 files; B-tree deep enough to require multi-level index
- `dir_10000_entries` — 10K files; deep B-tree; all findable; random access O(log n)
- `dir_entries_unicode_sort_order` — mixed ASCII/unicode names; sorted by upcase table
- `dir_entries_delete_rebalance` — delete 50% of entries from 1000-entry dir; tree rebalances
- `dir_rename_within_dir` — rename file in large dir; old key removed, new key inserted

### 3.5 Sparse File Semantics

- `sparse_write_then_read_hole` — write sparse file; read hole bytes = 0x00
- `sparse_allocated_size_vs_data_size` — allocated_size < data_size for sparse file
- `sparse_initialized_size_tracking` — initialized_size field tracks written region boundary
- `sparse_punch_hole` — write dense file, punch hole in middle, read back
- `sparse_large_hole` — hole > 2^32 bytes; correct LCN=0 run in mapping pairs
- `sparse_chkdsk_clean` — sparse file passes Windows chkdsk

### 3.6 Hard Link Scenarios

- `hardlink_two_names` — create hardlink; both names in $I30; same inode
- `hardlink_link_count_increments` — nlink goes 1→2 after link creation
- `hardlink_link_count_decrements` — nlink goes 2→1 after one name removed
- `hardlink_delete_last_name` — nlink reaches 0; MFT record freed; data cluster freed
- `hardlink_cross_directory` — hard link source and target in different directories
- `hardlink_max_count` — NTFS allows 1023 hard links; test boundary
- `hardlink_rename_one_name` — rename one of two names; other name unchanged

### 3.7 Alternate Data Streams — Comprehensive

- `ads_create_and_read` — create "file:stream"; read back exact bytes
- `ads_multiple_streams_on_file` — 5 named streams on one file; all readable
- `ads_grow_past_threshold` — stream resident → non-resident; data intact
- `ads_shrink_below_threshold` — non-resident stream shrinks; NOT pulled back resident (by design)
- `ads_delete_stream` — delete one ADS; default stream intact; other ADS intact
- `ads_stream_zero_length` — empty named stream; exists; zero bytes readable
- `ads_stream_max_name_length` — 255-char stream name; creates and reads back
- `ads_rename_stream` — rename an ADS (if implemented; test expected failure if not)
- `ads_chkdsk_clean` — file with 3 ADS passes Windows chkdsk

### 3.8 Security Descriptor Coverage

- `security_default_sd_on_creation` — every new file gets security_id pointing to valid $Secure entry
- `security_id_consistent` — all files created in one session share same default security_id
- `security_sd_readable` — read raw SD bytes via extended attr or ioctl; valid SD structure
- `security_inheritable_ace` — parent dir with inheritable ACE; child inherits (if implemented)
- `security_chkdsk_clean` — volume with files passes chkdsk $Secure validation

### 3.9 Volume Metadata Integrity

- `volume_label_roundtrip` — set volume label; remount; label reads back correctly
- `volume_label_max_length` — 32-char label (NTFS max); round-trip
- `volume_label_unicode` — label with non-ASCII chars; round-trip
- `volume_flags_dirty_bit` — set dirty; remount; dirty bit present; clear; remount; clean
- `volume_serial_number` — format preserves serial number; readable after remount
- `volume_ntfs_version` — $Volume attr reports NTFS 3.1 after mkfs
- `volume_info_chkdsk_clean` — volume passes chkdsk $Volume validation

### 3.10 Corruption Resistance (expand `corruption_fuzz.rs`)

- `corrupt_mft_record_signature` — flip "FILE" to "GILE"; read of that file returns error
- `corrupt_mft_record_usa` — corrupt update sequence array end-word; fixup detects mismatch
- `corrupt_attr_type_field` — set attr type to 0xDEADBEEF; iterator skips or errors
- `corrupt_attr_length_zero` — attr length = 0; iterator does not infinite-loop
- `corrupt_attr_length_oob` — attr length extends past record end; error, no OOB read
- `corrupt_data_run_terminator_missing` — data runs with no terminator byte; parse stops at buffer end
- `corrupt_index_block_signature` — flip "INDX" to "XDNI"; parse error
- `corrupt_index_block_usa` — corrupt INDX usa end-word; error detected
- `corrupt_cluster_bitmap_partial` — mark allocated cluster as free in bitmap; write detects double-alloc
- `corrupt_boot_sector_bpb` — flip cluster_size to non-power-of-two; mount rejects volume
- `corrupt_mft_mirror_mismatch` — $MFTMirr differs from $MFT record 0; mount handles gracefully

---

## Phase 4 — Performance and Boundary Tests

### 4.1 Throughput Benchmarks (criterion)

Add `benches/` directory with criterion benchmarks:

- `bench_sequential_write` — write 64 MB sequentially; measure MB/s
- `bench_sequential_read` — read 64 MB file; measure MB/s
- `bench_random_4k_write` — 4KB writes at random offsets; measure IOPS
- `bench_directory_lookup` — lookup in 10K-entry dir; measure latency
- `bench_file_create_delete` — create+delete 1000 files; measure ops/sec
- `bench_data_run_encode_decode` — encode/decode 10000 mapping-pair arrays; measure ns/op

### 4.2 Boundary and Overflow Tests

- `file_size_zero` — zero-byte file; stat shows size 0; read returns empty
- `file_size_1_byte` — single byte; resident; read back correct
- `file_size_exactly_resident_threshold` — at boundary; stays resident
- `file_size_resident_threshold_plus_1` — promoted; data intact
- `file_size_exactly_1_cluster` — non-resident; 1 cluster allocated; no over-allocation
- `file_size_max_u32` — 4 GB file; 32-bit boundary
- `file_size_max_u63` — near max NTFS file size (2^63 - 1 bytes); fields don't overflow
- `filename_length_1` — 1-char filename
- `filename_length_255` — 255-char filename (NTFS max)
- `filename_all_legal_special_chars` — spaces, dots, brackets, unicode in name

---

## Phase 5 — Code Quality and Readability (human-code pass)

Run `human-code` skill on each module below; apply fixes with `dev-loop` test gate.

Priority order (most impact on future contributors):

1. `index_io.rs` — dense B-tree manipulation; magic numbers (0x02, 0x01 flags); god function `insert_entry_into_index_root`
2. `attr_resize.rs` — byte-level shifting arithmetic; alignment magic numbers
3. `write.rs` — large function bodies handling multiple W1 operations
4. `mft_io.rs` — USA fixup math; power-of-two validation
5. `data_runs.rs` — bit-twiddling for length/offset byte count encoding
6. `record_build.rs` — repeated attribute assembly patterns; candidates for helper extraction
7. `bitmap.rs` — bitwise scan loops
8. `mkfs.rs` — large formatter function; constants scattered inline

Readability criteria per module:
- No magic numbers: extract named constants for all NTFS type codes, flag bits, offsets
- No god functions over ~60 lines: split into named helpers with clear single responsibilities
- No duplication: identical 3+ line patterns extracted to shared helpers
- Dense arithmetic: one expression per line with named intermediates
- Invariants that surprise future readers: one-line comment (why, not what)

---

## Phase 6 — Integration Test Hardening

### 6.1 Errno / Error Code Coverage

Expand `errno_companion.rs`:

- Every public API surface that can fail; every distinct error code documented
- Positive case + negative case per error: verify correct errno returned
- ENOENT on missing file
- EEXIST on create of existing
- ENOTDIR on file treated as dir
- EISDIR on dir treated as file
- ENOTEMPTY on rmdir of non-empty dir
- EACCES on write to read-only volume
- ENAMETOOLONG on 256+ char filename
- ENOSPC on full volume

### 6.2 Remount Consistency

Every write operation followed by:
1. Unmount
2. Remount
3. Verify state persists correctly

Currently most write tests operate on a mounted volume and verify in-memory state.
Adding explicit remount after each write op catches flush bugs.

Tests to add (suffix `_persists_after_remount`):
- `write_file_content_persists_after_remount`
- `write_timestamps_persist_after_remount`
- `write_attrs_persist_after_remount`
- `write_ea_persists_after_remount`
- `write_reparse_persists_after_remount`
- `write_object_id_persists_after_remount`
- `write_hardlink_persists_after_remount`
- `write_ads_persists_after_remount`
- `mkdir_persists_after_remount`
- `rmdir_persists_after_remount`
- `rename_persists_after_remount`
- `unlink_persists_after_remount`

### 6.3 C API Surface Completeness

Verify every C-exported function has at least one:
- Success-path test
- Error-path test (invalid handle, null pointer, bad args)
- Memory-safety test (no double-free, no use-after-free, correct buffer sizing)

---

## Execution Order

| Phase | Effort | Impact | Dependencies |
|-------|--------|--------|-------------|
| 1 — Unit isolation | High | Very High | None |
| 2 — Field exhaustion | High | High | Phase 1 helpers |
| 3.1–3.3 Disk images core | High | Very High | None |
| 3.4–3.10 Disk images extended | High | High | 3.1–3.3 passing |
| 4 — Performance | Medium | Medium | Phase 3 complete |
| 5 — Human code | Medium | High (long-term) | Stable test suite |
| 6 — Integration hardening | Medium | High | Phase 2+3 |

Recommended starting point: **Phase 1 (unit isolation)** — these tests require no
disk images, run in milliseconds, and immediately reveal bugs in the infrastructure
code that all other tests depend on.

---

## Estimated Test Count Growth

| Phase | New Tests | Total After |
|-------|-----------|-------------|
| Baseline | — | ~559 |
| Phase 1 | ~90 unit tests | ~649 |
| Phase 2 | ~120 field tests | ~769 |
| Phase 3 | ~80 disk image tests | ~849 |
| Phase 4 | ~30 boundary + 6 benchmarks | ~885 |
| Phase 6 | ~40 integration tests | ~925 |
| **Total** | **~366 new tests** | **~925** |
