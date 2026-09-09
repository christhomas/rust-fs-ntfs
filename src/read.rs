//! Native NTFS read layer (work in progress).
//!
//! This module is the start of replacing the upstream `ntfs` crate on the
//! production read path with our own primitives (`mft_io`, `attr_io`,
//! `data_runs`, `index_io`, `idx_block`). See
//! `docs/native-read-layer-plan.md` for the full plan; the crate stays as a
//! test-only oracle that these functions are cross-checked against.
//!
//! Phase 1: path resolution (`/a/b/c` → MFT record number), assembled from
//! the index primitives the write path already uses for lookup. No upstream
//! `ntfs` types appear here.

use crate::attr_io::{self, attr_off, AttrType};
use crate::block_io::BlockIo;
use crate::compression;
use crate::data_runs;
use crate::idx_block;
use crate::index_io::{self, IH_FLAG_HAS_SUBNODES};
use crate::mft_io::{read_mft_record_io, record_flags, MFT_FLAG_DIRECTORY};
use crate::upcase::UpcaseTable;

/// Attribute data-flags (header +0x0C): the value is transformed and
/// can't be returned as raw bytes by this reader yet. The names live in
/// `attr_io::attr_flags` now, beside the offset they are read from, so
/// that the reader and the three write guards share one definition.
use crate::attr_io::attr_flags::{
    COMPRESSED as ATTR_FLAG_COMPRESSED, ENCRYPTED as ATTR_FLAG_ENCRYPTED,
};

/// MFT record number of the root directory (`.`), fixed by the NTFS spec.
pub const ROOT_RECORD_NUMBER: u64 = 5;

/// Resolve an absolute path to its MFT record number, walking the directory
/// tree natively (no upstream `ntfs` crate). A leading `/` is optional;
/// `""`/`"/"` resolve to the root directory.
///
/// Each component is looked up in its parent directory's index (resident
/// `$INDEX_ROOT` then, if spilled, the `$INDEX_ALLOCATION` INDX blocks).
///
/// Lookup is **case-insensitive**, matching NTFS's default
/// `COLLATION_FILE_NAME`: the `$UpCase` table is loaded once (natively) and
/// names are compared upcase-folded (so `/Foo` finds an on-disk `foo`), the
/// same behaviour as the upstream oracle. `$UpCase` is **required** — if it
/// can't be loaded this returns `Err` rather than silently degrading to
/// case-sensitive matching (which would make `/foo` miss an on-disk `FOO`).
///
/// (Note: the *write* path's dedup still uses the shared exact-match
/// `index_io::find_index_entry` — making that collation-aware is the
/// write-affecting half of C5, handled when `write.rs`'s resolver is flipped.)
pub fn resolve_path<T: BlockIo + ?Sized>(io: &mut T, path: &str) -> Result<u64, String> {
    // Load $UpCase once for the whole walk. Required for correct collation —
    // propagate failure instead of silently matching case-sensitively.
    let upcase = UpcaseTable::load_io(io)
        .map_err(|e| format!("resolve_path: load $UpCase for collation: {e}"))?;
    let mut record_number = ROOT_RECORD_NUMBER;

    for component in path.split('/') {
        match component {
            "" | "." => continue, // empty (slashes) and "." stay put
            ".." => {
                // Parent directory, via the $FILE_NAME parent reference; the
                // root is its own parent.
                if record_number != ROOT_RECORD_NUMBER {
                    record_number = read_parent_record(io, record_number)?;
                }
                continue;
            }
            _ => {}
        }

        let (_, dir_bytes) = read_mft_record_io(io, record_number)?;
        if record_flags(&dir_bytes) & MFT_FLAG_DIRECTORY == 0 {
            return Err(format!(
                "resolve_path: '{component}' parent (record {record_number}) is not a directory"
            ));
        }

        record_number = lookup_in_directory(io, record_number, &dir_bytes, component, &upcase)?
            .ok_or_else(|| format!("resolve_path: '{component}' not found"))?;
    }

    Ok(record_number)
}

/// Case-insensitive (upcase-folded) match of `want` against a batch of index
/// entries; returns the first matching entry's target record.
fn match_name(
    entries: &[index_io::DirEntryRaw],
    want: &[u16],
    upcase: &UpcaseTable,
) -> Option<u64> {
    entries.iter().find_map(|e| {
        let entry_name: Vec<u16> = e.name.encode_utf16().collect();
        (upcase.cmp_names(&entry_name, want) == std::cmp::Ordering::Equal)
            .then_some(e.file_record_number)
    })
}

/// Look up a single name in one directory, upcase-collated. `dir_bytes` is the
/// directory's already-read (post-fixup) MFT record; `dir_record` is its number
/// (needed to load `$INDEX_ALLOCATION` if the index has spilled). Returns the
/// target record number, or `None` if absent.
///
/// Enumerates via the shared `index_io::collect_*` iterators and matches here
/// (so the collation lives in the read layer, not the write path's dedup), but
/// **short-circuits**: the resident `$INDEX_ROOT` is checked first, then INDX
/// blocks one at a time, returning on the first hit instead of reading every
/// block.
fn lookup_in_directory<T: BlockIo + ?Sized>(
    io: &mut T,
    dir_record: u64,
    dir_bytes: &[u8],
    name: &str,
    upcase: &UpcaseTable,
) -> Result<Option<u64>, String> {
    let want: Vec<u16> = name.encode_utf16().collect();

    // Resident $INDEX_ROOT first — return on hit.
    let mut root_entries = Vec::new();
    index_io::collect_index_root_entries(dir_bytes, &mut root_entries)?;
    if let Some(rec) = match_name(&root_entries, &want, upcase) {
        return Ok(Some(rec));
    }

    // Spilled into $INDEX_ALLOCATION? Scan blocks one at a time, returning on
    // the first match rather than collecting every block.
    let ir_flags = index_io::index_root_flags(dir_bytes)
        .ok_or_else(|| format!("directory record {dir_record} has no $INDEX_ROOT"))?;
    if ir_flags & IH_FLAG_HAS_SUBNODES != 0 {
        let ia = idx_block::load_for_directory_io(io, dir_record)?;
        let mut block_entries = Vec::new();
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block_io(io, &ia, vcn)?;
            block_entries.clear();
            index_io::collect_indx_block_entries(&block, &mut block_entries)?;
            if let Some(rec) = match_name(&block_entries, &want, upcase) {
                return Ok(Some(rec));
            }
        }
    }

    Ok(None)
}

/// Read an attribute's full value bytes natively (no upstream `ntfs` crate).
///
/// Handles resident values, non-resident values (walking the data runs via
/// [`data_runs`] and reading clusters through [`BlockIo`]), and sparse holes
/// (unmapped runs read as zeros). Bytes past the attribute's
/// `initialized_size` read as zero even when clusters are allocated, matching
/// NTFS semantics. The returned vector has length = the attribute's data size.
///
/// Compressed / encrypted attributes are refused for now (the value would be
/// transformed, not raw) — LZNT1 decompression wiring builds on this reader in
/// a later step.
pub fn read_attribute_value<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    attr_type: AttrType,
    name: Option<&str>,
) -> Result<Vec<u8>, String> {
    match locate_attribute(io, record_number, attr_type, name)? {
        Some((params, _holder, record, loc)) => {
            if attr_type == AttrType::Data && name.is_none() {
                refuse_wof_compressed(io, &params, &record, record_number)?;
            }
            read_value_from_record(io, &params, &record, &loc)
        }
        None => Err(format!(
            "read_attribute_value: attribute {attr_type:?} (name {name:?}) not found in record {record_number}"
        )),
    }
}

/// Refuse the unnamed `$DATA` of a WOF-compressed file.
///
/// Windows Overlay Filter compression -- `compact /exe`, and "Compact
/// OS" across a whole system partition -- leaves the file's unnamed
/// `$DATA` empty and sparse, puts the real bytes in a
/// `WofCompressedData` stream, and marks the file with an
/// `IO_REPARSE_TAG_WOF` `$REPARSE_POINT`. A plain `$DATA` read of such
/// a file succeeds and returns the right *number* of bytes, all zero:
/// a copy-out produces a zero-filled file of the correct length that
/// looks right until someone tries to use it.
///
/// The C ABI's `fs_ntfs_read` used to carry this check on its own, with
/// the reasoning written out beside it, and `facade::read_file` did
/// not -- so the two front doors of the same crate disagreed about
/// whether the file was readable, and the one that said yes was wrong.
/// It lives here now, on the path they share, so they cannot drift
/// again.
///
/// Only the content reads refuse. `read_stat` and `read_dir_entries` go
/// on working, because a WOF file is a regular file whose bytes this
/// crate cannot decode yet -- refusing to list it or to size it would
/// make a Windows system volume unusable rather than honest.
///
/// The record checked is the one holding `$DATA`. For the shape WOF
/// produces that is the base record, which is also where the
/// `$REPARSE_POINT` is: an empty sparse `$DATA` does not overflow into
/// an extension record.
fn refuse_wof_compressed<T: BlockIo + ?Sized>(
    io: &mut T,
    params: &crate::mft_io::BootParams,
    record: &[u8],
    record_number: u64,
) -> Result<(), String> {
    let Some(rp) = attr_io::find_attribute(record, AttrType::ReparsePoint, None) else {
        return Ok(());
    };
    let value = read_value_from_record(io, params, record, &rp)?;
    if value.len() >= 4
        && u32::from_le_bytes([value[0], value[1], value[2], value[3]])
            == crate::record_build::reparse_tag::WOF
    {
        return Err(format!(
            "record {record_number} is WOF-compressed (IO_REPARSE_TAG_WOF); \
             decompression not yet supported"
        ));
    }
    Ok(())
}

/// Locate where an attribute physically lives: the MFT record bytes (base or,
/// following `$ATTRIBUTE_LIST`, an extension record) plus its `AttrLocation`.
/// Returns `None` if the attribute is absent. Shared by `read_attribute_value`
/// and `read_stat` so both handle overflowed (`$ATTRIBUTE_LIST`) files.
///
/// **Limitation (fails loud, never truncates):** a single non-resident
/// attribute whose run list is split across *multiple* records (i.e.
/// `$ATTRIBUTE_LIST` carries entries for the same type+name with
/// `starting_vcn > 0`) is not yet stitched — this returns `Err` rather than
/// silently reading only the VCN-0 segment.
///
/// That guarantee needs `$ATTRIBUTE_LIST` to be consulted *first*, and it
/// used to be consulted second: the base record was searched, the
/// attribute was found there, and the function returned before the check
/// could run. A split run list is exactly the shape where the VCN-0
/// segment is in the base record, so the guard sat behind the only case
/// it was written for and a fragmented file read back its first segment
/// with `Ok`. When the list is present it is the authority — the base
/// record's copy of an attribute is one segment among several, and
/// finding it there says nothing about whether it is the whole thing.
#[allow(clippy::type_complexity)]
fn locate_attribute<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    attr_type: AttrType,
    name: Option<&str>,
) -> Result<
    Option<(
        crate::mft_io::BootParams,
        u64,
        Vec<u8>,
        attr_io::AttrLocation,
    )>,
    String,
> {
    let (params, record) = read_mft_record_io(io, record_number)?;

    // $ATTRIBUTE_LIST first. It is the authority on where this file's
    // attributes live, and when it is present the base record's copy of
    // one is a segment rather than necessarily the whole thing.
    if let Some(al_loc) = attr_io::find_attribute(&record, AttrType::AttributeList, None) {
        let al_value = read_value_from_record(io, &params, &record, &al_loc)?;
        let entries = parse_attribute_list(&al_value)?;
        let matching: Vec<&AttrListEntry> = entries
            .iter()
            .filter(|e| e.type_code == attr_type as u32 && e.name.as_deref() == name)
            .collect();

        if !matching.is_empty() {
            // Refuse a run list split across records rather than
            // returning the VCN-0 segment and calling it the value.
            // Stitching the segments is the real answer; a refusal is
            // the half of it that must not wait, because the other
            // behaviour is a short read reported as a complete one.
            if matching.iter().any(|e| e.starting_vcn != 0) {
                return Err(format!(
                    "locate_attribute: {attr_type:?} (name {name:?}) in record {record_number} is \
                     split across {} records ($ATTRIBUTE_LIST multi-extent stitching not yet \
                     supported)",
                    matching.len()
                ));
            }
            let entry = matching
                .iter()
                .find(|e| e.starting_vcn == 0)
                .ok_or_else(|| {
                    format!(
                    "locate_attribute: $ATTRIBUTE_LIST lists {attr_type:?} (name {name:?}) for \
                     record {record_number} with no VCN-0 segment"
                )
                })?;
            if entry.record_number != record_number {
                let (ext_params, ext) = read_mft_record_io(io, entry.record_number)?;
                let loc = attr_io::find_attribute(&ext, attr_type, name).ok_or_else(|| {
                    format!(
                        "locate_attribute: $ATTRIBUTE_LIST points {attr_type:?} (name {name:?}) \
                         at record {} but it's not there",
                        entry.record_number
                    )
                })?;
                return Ok(Some((ext_params, entry.record_number, ext, loc)));
            }
            // The single segment is in the base record: fall through.
        }
        // A list that does not mention this attribute at all says
        // nothing about it; the base record still might.
    }

    if let Some(loc) = attr_io::find_attribute(&record, attr_type, name) {
        return Ok(Some((params, record_number, record, loc)));
    }

    Ok(None)
}

/// Read one attribute's value from the record + location that holds it
/// (resident copy, or non-resident runs with sparse-hole zero-fill honouring
/// `initialized_size`). Refuses compressed/encrypted values (the bytes would
/// be transformed, not raw). `record` must be the record containing `loc`.
fn read_value_from_record<T: BlockIo + ?Sized>(
    io: &mut T,
    params: &crate::mft_io::BootParams,
    record: &[u8],
    loc: &attr_io::AttrLocation,
) -> Result<Vec<u8>, String> {
    if loc.is_resident {
        let vo = loc.attr_offset
            + loc
                .resident_value_offset
                .ok_or("resident attr has no value offset")? as usize;
        let vl = loc
            .resident_value_length
            .ok_or("resident attr has no value length")? as usize;
        // Bounds-check before slicing: corrupt on-disk offset/length must
        // produce an Err, not a panic.
        let end = vo
            .checked_add(vl)
            .filter(|&e| e <= record.len())
            .ok_or_else(|| {
                format!(
                    "resident value [{vo}..{vo}+{vl}] out of bounds (record {} bytes)",
                    record.len()
                )
            })?;
        return Ok(record[vo..end].to_vec());
    }

    let flags = u16::from_le_bytes([
        record[loc.attr_offset + attr_off::FLAGS],
        record[loc.attr_offset + attr_off::FLAGS + 1],
    ]);
    if flags & ATTR_FLAG_ENCRYPTED != 0 {
        return Err("read_attribute_value: encrypted attribute ($EFS) unsupported".to_string());
    }
    if flags & ATTR_FLAG_COMPRESSED != 0 {
        return read_compressed_nonresident(io, params, record, loc);
    }

    let data_size = bounded_value_length(params, &loc.non_resident_value_length, "non-resident")?;
    let init_size = u64::from_le_bytes(
        record[loc.attr_offset + attr_off::NONRES_INITIALIZED_LENGTH
            ..loc.attr_offset + attr_off::NONRES_INITIALIZED_LENGTH + 8]
            .try_into()
            .map_err(|_| "short record reading initialized_size")?,
    ) as usize;
    let mpo = loc
        .non_resident_mapping_pairs_offset
        .ok_or("non-resident attr has no mapping-pairs offset")? as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])?;

    let cluster_size = params.cluster_size as usize;
    // Zero-initialised: holes and the [initialized_size, data_size) tail are
    // both zero, so we only have to fill in allocated, initialised clusters.
    let mut out = vec![0u8; data_size];
    let readable = data_size.min(init_size);
    let cluster_count = data_size.div_ceil(cluster_size);
    for vcn in 0..cluster_count as u64 {
        let file_off = vcn as usize * cluster_size;
        if file_off >= readable {
            break; // rest is uninitialised → stays zero
        }
        if let Some(lcn) = data_runs::vcn_to_lcn(&runs, vcn) {
            // The LCN came off a mapping-pair list; `decode_runs` proved
            // only that it is not negative. See `mft_io::cluster_span`.
            let at =
                crate::mft_io::cluster_span(params, lcn, 0, 0, params.cluster_size, io.size())?;
            let mut cluster = vec![0u8; cluster_size];
            io.read_exact_at(at, &mut cluster)?;
            let copy_len = (file_off + cluster_size).min(readable) - file_off;
            out[file_off..file_off + copy_len].copy_from_slice(&cluster[..copy_len]);
        }
        // else: sparse hole → leave zeros.
    }

    Ok(out)
}

/// Read up to `len` bytes of an attribute's value starting at byte `offset`,
/// **without materialising the whole value**. An uncompressed non-resident
/// attribute reads only the clusters overlapping the window — so a small read
/// of a huge file doesn't allocate gigabytes. Resident values (tiny) and
/// compressed values (decompress as a unit) fall back to a full read + slice.
/// Follows `$ATTRIBUTE_LIST`. Returns fewer than `len` bytes only at end of
/// value. `offset` stays `u64` throughout (no 32-bit truncation).
pub fn read_attribute_range<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    attr_type: AttrType,
    name: Option<&str>,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, String> {
    let (params, _holder, record, loc) = locate_attribute(io, record_number, attr_type, name)?
        .ok_or_else(|| {
            format!("read_attribute_range: attribute {attr_type:?} (name {name:?}) not found in record {record_number}")
        })?;
    if attr_type == AttrType::Data && name.is_none() {
        refuse_wof_compressed(io, &params, &record, record_number)?;
    }

    // Uncompressed, unencrypted, non-resident → true ranged read.
    if !loc.is_resident {
        let flags = u16::from_le_bytes([
            record[loc.attr_offset + attr_off::FLAGS],
            record[loc.attr_offset + attr_off::FLAGS + 1],
        ]);
        if flags & (ATTR_FLAG_COMPRESSED | ATTR_FLAG_ENCRYPTED) == 0 {
            return read_nonresident_range(io, &params, &record, &loc, offset, len);
        }
    }

    // Resident or compressed: read the whole value, then slice the window.
    let full = read_value_from_record(io, &params, &record, &loc)?;
    let full_len = full.len() as u64;
    let start = offset.min(full_len) as usize;
    let end = offset.saturating_add(len as u64).min(full_len) as usize;
    Ok(full[start..end].to_vec())
}

/// Ranged read of an uncompressed non-resident attribute: reads only the
/// clusters overlapping `[offset, offset+len)`, zero-filling sparse holes and
/// the `[initialized_size, data_size)` tail.
fn read_nonresident_range<T: BlockIo + ?Sized>(
    io: &mut T,
    params: &crate::mft_io::BootParams,
    record: &[u8],
    loc: &attr_io::AttrLocation,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, String> {
    let data_size = loc
        .non_resident_value_length
        .ok_or("non-resident attr has no data size")?;
    if len == 0 || offset >= data_size {
        return Ok(Vec::new());
    }
    let init_size = u64::from_le_bytes(
        record[loc.attr_offset + attr_off::NONRES_INITIALIZED_LENGTH
            ..loc.attr_offset + attr_off::NONRES_INITIALIZED_LENGTH + 8]
            .try_into()
            .map_err(|_| "short record reading initialized_size")?,
    );
    let mpo = loc
        .non_resident_mapping_pairs_offset
        .ok_or("non-resident attr has no mapping-pairs offset")? as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])?;

    let cs = params.cluster_size; // u64
    let end = offset.saturating_add(len as u64).min(data_size); // exclusive byte
    let readable = data_size.min(init_size); // bytes past this read as zero
    let mut out = vec![0u8; (end - offset) as usize];

    for vcn in (offset / cs)..=((end - 1) / cs) {
        let cluster_byte = vcn * cs;
        let win_start = offset.max(cluster_byte);
        let win_end = end.min(cluster_byte + cs).min(readable);
        if win_end <= win_start {
            continue; // beyond initialized_size or empty → stays zero
        }
        if let Some(lcn) = data_runs::vcn_to_lcn(&runs, vcn) {
            // Checked and bounded by the volume; see
            // `mft_io::cluster_span`.
            let at = crate::mft_io::cluster_span(params, lcn, 0, 0, cs, io.size())?;
            let mut cluster = vec![0u8; cs as usize];
            io.read_exact_at(at, &mut cluster)?;
            let in_cluster = (win_start - cluster_byte) as usize;
            let n = (win_end - win_start) as usize;
            let out_off = (win_start - offset) as usize;
            out[out_off..out_off + n].copy_from_slice(&cluster[in_cluster..in_cluster + n]);
        }
        // else: sparse hole → leave zeros.
    }

    Ok(out)
}

/// Non-resident attribute header: compression-unit exponent (u16 at +0x22).
/// The unit is `2^exp` clusters (4 ⇒ 16 clusters, the LZNT1 default).
/// The largest compression unit NTFS defines, as its base-2 log.
///
/// NTFS compresses in units of sixteen clusters, so the field is 4 on
/// every volume anyone has. It is a u16, and it was checked only
/// against zero.
const MAX_COMPRESSION_UNIT: u32 = 4;

/// A non-resident attribute's declared length, once it is known to be
/// something the volume could hold.
///
/// `data_length` is a `u64` off the disk and it is the size of the
/// buffer this reader allocates before reading a byte. Nothing in the
/// crate compared an attribute length to the volume's size, so
/// `$UpCase` -- which `resolve_path` loads before any lookup, so on the
/// first operation of any mount -- with `data_length = 0x0000_FFFF_FFFF_FFFF`
/// on a 10 MB image gave `memory allocation of 281474976710655 bytes
/// failed`. That is an abort, not a catchable error: the FFI guard
/// never sees it.
///
/// A non-resident value lives in clusters, and a volume has only so
/// many.
fn bounded_value_length(
    params: &crate::mft_io::BootParams,
    declared: &Option<u64>,
    what: &str,
) -> Result<usize, String> {
    let declared = declared.ok_or(format!("{what} attr has no data size"))?;
    let volume = params.volume_bytes();
    if declared > volume {
        return Err(format!(
            "{what} attribute says its value is {declared} bytes, on a volume of {volume}"
        ));
    }
    Ok(declared as usize)
}

const NONRES_COMPRESSION_UNIT: usize = 0x22;

/// Read a compressed non-resident attribute, decompressing each compression
/// unit. A unit is `2^exp` clusters; NTFS stores it one of three ways:
/// * all clusters allocated  ⇒ stored uncompressed (raw), copy verbatim;
/// * some leading clusters + a trailing hole ⇒ LZNT1-compressed, decompress
///   the leading (allocated) clusters into the unit's bytes;
/// * no clusters (whole-unit hole) ⇒ all zeros.
///
/// The returned vector has length = the attribute's logical data size.
fn read_compressed_nonresident<T: BlockIo + ?Sized>(
    io: &mut T,
    params: &crate::mft_io::BootParams,
    record: &[u8],
    loc: &attr_io::AttrLocation,
) -> Result<Vec<u8>, String> {
    let data_size = bounded_value_length(params, &loc.non_resident_value_length, "compressed")?;
    let cu_exp = u16::from_le_bytes([
        record[loc.attr_offset + NONRES_COMPRESSION_UNIT],
        record[loc.attr_offset + NONRES_COMPRESSION_UNIT + 1],
    ]) as u32;
    // NTFS compresses in units of 16 clusters, which is
    // `compression_unit = 4`; the field is the base-2 log of the
    // cluster count and the format defines no larger unit. It was
    // rejected only at zero, so 62 shifted `unit_clusters` to a value
    // whose product with the cluster size wrapped to 0 -- and the loop
    // below advances by that product, so a release build hung.
    if cu_exp == 0 || cu_exp > MAX_COMPRESSION_UNIT {
        return Err(format!(
            "compression_unit is {cu_exp}, where {MAX_COMPRESSION_UNIT} is the largest \
             unit NTFS compresses in"
        ));
    }
    let unit_clusters = 1usize << cu_exp;
    let mpo = loc
        .non_resident_mapping_pairs_offset
        .ok_or("compressed attr has no mapping-pairs offset")? as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])?;

    let cluster_size = params.cluster_size as usize;
    let unit_size = unit_clusters * cluster_size;
    let mut out = vec![0u8; data_size];

    let mut unit_first_vcn = 0usize;
    while unit_first_vcn * cluster_size < data_size {
        let unit_off = unit_first_vcn * cluster_size;
        let unit_out_len = unit_size.min(data_size - unit_off);

        // Collect this unit's allocated clusters (in VCN order — the leading,
        // real ones for a compressed unit). A compression unit is always
        // `unit_clusters` wide on disk: a compressed unit stores its data in
        // the leading clusters and leaves the rest a hole (so allocated <
        // unit_clusters), while an uncompressed unit allocates all of them.
        // Classify by `unit_clusters`, NOT by how many VCNs fall inside
        // data_size — the final partial unit still compresses into one
        // allocated cluster and must be decompressed, not copied raw.
        let mut allocated_lcns = Vec::new();
        for k in 0..unit_clusters {
            if let Some(lcn) = data_runs::vcn_to_lcn(&runs, (unit_first_vcn + k) as u64) {
                allocated_lcns.push(lcn);
            }
        }

        if allocated_lcns.is_empty() {
            // Whole-unit hole → zeros (already zero-initialised).
        } else {
            let mut raw = Vec::with_capacity(allocated_lcns.len() * cluster_size);
            for lcn in &allocated_lcns {
                // Checked and bounded by the volume; see
                // `mft_io::cluster_span`.
                let at = crate::mft_io::cluster_span(
                    params,
                    *lcn,
                    0,
                    0,
                    params.cluster_size,
                    io.size(),
                )?;
                let mut cluster = vec![0u8; cluster_size];
                io.read_exact_at(at, &mut cluster)?;
                raw.extend_from_slice(&cluster);
            }
            let plain = if allocated_lcns.len() == unit_clusters {
                // All clusters allocated ⇒ stored uncompressed; raw is content.
                raw
            } else {
                // Fewer than a full unit ⇒ LZNT1-compressed; decompress.
                compression::decompress_unit(&raw, unit_out_len)?
            };
            let copy = unit_out_len.min(plain.len());
            out[unit_off..unit_off + copy].copy_from_slice(&plain[..copy]);
        }

        unit_first_vcn += unit_clusters;
    }

    Ok(out)
}

/// `$STANDARD_INFORMATION` value-field offsets (NTFS 1.x and 3.x agree on
/// the first 0x24 bytes that we read here).
const SI_CREATION: usize = 0x00;
const SI_MODIFICATION: usize = 0x08;
const SI_MFT_MODIFICATION: usize = 0x10;
const SI_ACCESS: usize = 0x18;
const SI_FILE_ATTRIBUTES: usize = 0x20;

/// Seconds between the NTFS epoch (1601-01-01) and the Unix epoch (1970-01-01).
const NT_UNIX_EPOCH_DIFF_SECS: i64 = 11_644_473_600;

/// Convert an NTFS FILETIME (100-ns intervals since 1601-01-01 UTC) to whole
/// Unix seconds. Pure function.
pub fn nt_to_unix(nt: u64) -> i64 {
    (nt / 10_000_000) as i64 - NT_UNIX_EPOCH_DIFF_SECS
}

/// File metadata read natively from one MFT record (no upstream `ntfs`
/// crate). Timestamps are raw NTFS FILETIMEs; use [`nt_to_unix`] to convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// Logical size of the unnamed `$DATA` (0 if the file has none, e.g. a dir).
    pub size: u64,
    pub is_dir: bool,
    /// `$STANDARD_INFORMATION.file_attributes`.
    pub file_attributes: u32,
    /// Hard-link count from the FILE record header (+0x12).
    pub link_count: u16,
    pub created_nt: u64,
    pub modified_nt: u64,
    pub mft_modified_nt: u64,
    pub accessed_nt: u64,
}

/// Read a record's metadata: directory flag, `$STANDARD_INFORMATION`
/// timestamps + attributes, and the unnamed `$DATA` size.
pub fn read_stat<T: BlockIo + ?Sized>(io: &mut T, record_number: u64) -> Result<Stat, String> {
    let (_, record) = read_mft_record_io(io, record_number)?;
    let is_dir = record_flags(&record) & MFT_FLAG_DIRECTORY != 0;
    let link_count = u16::from_le_bytes([record[0x12], record[0x13]]);

    let si = attr_io::find_attribute(&record, AttrType::StandardInformation, None)
        .ok_or("read_stat: $STANDARD_INFORMATION not found")?;
    if !si.is_resident {
        return Err(
            "read_stat: $STANDARD_INFORMATION is non-resident (impossible per spec)".into(),
        );
    }
    let v = si.attr_offset
        + si.resident_value_offset
            .ok_or("read_stat: $STANDARD_INFORMATION has no value offset")? as usize;
    let u64_at = |off: usize| -> Result<u64, String> {
        record
            .get(off..off + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
            .ok_or_else(|| "read_stat: $STANDARD_INFORMATION truncated".to_string())
    };
    let created_nt = u64_at(v + SI_CREATION)?;
    let modified_nt = u64_at(v + SI_MODIFICATION)?;
    let mft_modified_nt = u64_at(v + SI_MFT_MODIFICATION)?;
    let accessed_nt = u64_at(v + SI_ACCESS)?;
    let file_attributes = u32::from_le_bytes(
        record
            .get(v + SI_FILE_ATTRIBUTES..v + SI_FILE_ATTRIBUTES + 4)
            .ok_or("read_stat: file_attributes truncated")?
            .try_into()
            .unwrap(),
    );

    // Size of the unnamed $DATA. Follow $ATTRIBUTE_LIST so files whose $DATA
    // overflowed into an extension record report the real size, not 0.
    let size = match locate_attribute(io, record_number, AttrType::Data, None)? {
        Some((_, _, _, d)) if d.is_resident => d.resident_value_length.unwrap_or(0) as u64,
        Some((_, _, _, d)) => d.non_resident_value_length.unwrap_or(0),
        None => 0,
    };

    Ok(Stat {
        size,
        is_dir,
        file_attributes,
        link_count,
        created_nt,
        modified_nt,
        mft_modified_nt,
        accessed_nt,
    })
}

/// Read a record's parent-directory record number from its `$FILE_NAME`
/// (`parent_directory_reference` — the leading 8 bytes of the value, low 48
/// bits). Follows `$ATTRIBUTE_LIST`. For the root directory the parent
/// reference points at itself (record 5).
pub fn read_parent_record<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
) -> Result<u64, String> {
    let fname = read_attribute_value(io, record_number, AttrType::FileName, None)?;
    if fname.len() < 8 {
        return Err(format!(
            "read_parent_record: $FILE_NAME of record {record_number} too short"
        ));
    }
    let parent_ref = u64::from_le_bytes(fname[0..8].try_into().unwrap());
    Ok(parent_ref & 0x0000_FFFF_FFFF_FFFF)
}

/// `$Volume` MFT record number (fixed by the NTFS spec).
const VOLUME_RECORD_NUMBER: u64 = 3;

/// `$VOLUME_INFORMATION` value layout: reserved(8) + major(1) + minor(1) +
/// flags(2). (MS-FSCC / Windows Internals 7th ed.)
const VI_MAJOR: usize = 8;
const VI_FLAGS: usize = 10;

/// `VOLUME_IS_DIRTY` bit in `$VOLUME_INFORMATION.flags`.
pub const VOLUME_IS_DIRTY: u16 = 0x0001;

/// Volume metadata read natively (no upstream `ntfs` crate): boot-sector
/// geometry + serial, `$VOLUME_INFORMATION` version/flags, and the
/// `$VOLUME_NAME` label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub bytes_per_sector: u16,
    pub cluster_size: u32,
    pub file_record_size: u32,
    /// Volume size in bytes (`total_sectors × bytes_per_sector`, matching what
    /// the upstream parser reported via `Ntfs::size` — one sector short of the
    /// raw device).
    pub total_size: u64,
    pub total_clusters: u64,
    pub serial_number: u64,
    pub version_major: u8,
    pub version_minor: u8,
    pub flags: u16,
    /// `$VOLUME_NAME` label, or empty if the volume has none.
    pub label: String,
}

/// Read a volume's metadata natively: boot-sector geometry + serial number,
/// `$VOLUME_INFORMATION` (version + flags) and `$VOLUME_NAME` (label) from the
/// `$Volume` record (number 3). Replaces the upstream `Ntfs` parse used by the
/// volume-info / volume-stats entry points.
pub fn read_volume_info<T: BlockIo + ?Sized>(io: &mut T) -> Result<VolumeInfo, String> {
    // One boot-sector read; `BootParams` now carries serial + total_sectors +
    // oem_id alongside the geometry, so no second read is needed.
    let params = crate::mft_io::read_boot_params_io(io)?;

    // Validate the NTFS OEM signature here (the mount/volume-info path). The
    // shared boot parser deliberately doesn't, so geometry-only callers aren't
    // affected; but this entry point replaced `Ntfs::new`, which used to reject
    // non-NTFS (FAT/exFAT/raw) images at +0x03 before any structural parse.
    if &params.oem_id != crate::mft_io::NTFS_OEM_ID {
        return Err(format!(
            "not an NTFS volume: OEM id {:?} != {:?}",
            params.oem_id,
            crate::mft_io::NTFS_OEM_ID
        ));
    }

    // Volume size: total_sectors × bytes_per_sector. Matches what the upstream
    // parser reported via `Ntfs::size` (and what the C-ABI consumers have
    // always seen) — one sector short of the raw device, since NTFS reserves
    // the final sector for the backup boot sector. Fall back to the device
    // length only if a malformed boot sector reports zero total_sectors.
    let total_size = if params.total_sectors > 0 {
        params.total_sectors * params.bytes_per_sector as u64
    } else {
        io.size()
    };
    let total_clusters = total_size.checked_div(params.cluster_size).unwrap_or(0);

    // $VOLUME_INFORMATION (record 3, attr 0x70).
    let vi = read_attribute_value(io, VOLUME_RECORD_NUMBER, AttrType::VolumeInformation, None)?;
    if vi.len() < VI_FLAGS + 2 {
        return Err(format!("$VOLUME_INFORMATION too short: {} bytes", vi.len()));
    }
    let version_major = vi[VI_MAJOR];
    let version_minor = vi[VI_MAJOR + 1];
    let flags = u16::from_le_bytes([vi[VI_FLAGS], vi[VI_FLAGS + 1]]);

    // $VOLUME_NAME (record 3, attr 0x60) — optional UTF-16LE label. Locate it
    // first so a genuinely-absent attribute yields an empty label, while a real
    // I/O error (bad read / fixup mismatch on a corrupt $Volume extension) is
    // surfaced rather than masked as "no label".
    let label = match locate_attribute(io, VOLUME_RECORD_NUMBER, AttrType::VolumeName, None)? {
        Some((p, _holder, record, loc)) => {
            let bytes = read_value_from_record(io, &p, &record, &loc)?;
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        None => String::new(),
    };

    Ok(VolumeInfo {
        bytes_per_sector: params.bytes_per_sector,
        cluster_size: params.cluster_size as u32,
        file_record_size: params.file_record_size as u32,
        total_size,
        total_clusters,
        serial_number: params.serial_number,
        version_major,
        version_minor,
        flags,
        label,
    })
}

/// On-disk byte offset of a *resident* attribute's value (its first data
/// byte). For callers that patch a fixed field in place — e.g. fsck writing
/// the `$VOLUME_INFORMATION` dirty flag — add the field's offset within the
/// value to the result. Errors if the attribute is absent or non-resident.
///
/// Safe to write directly: a resident value sits inside the record body, away
/// from the USA fixup bytes NTFS stores at each sector's tail.
/// `min_value_len` guards a corrupt/truncated value: the returned offset is
/// only valid when the attribute's resident value is at least that many bytes,
/// so a caller patching a field at a fixed offset can't run past the value
/// into adjacent record bytes. Errors if the attribute is absent,
/// non-resident, or shorter than `min_value_len`.
pub fn resident_value_disk_offset<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    attr_type: AttrType,
    name: Option<&str>,
    min_value_len: usize,
) -> Result<u64, String> {
    let (params, holder, _record, loc) = locate_attribute(io, record_number, attr_type, name)?
        .ok_or_else(|| {
            format!("resident_value_disk_offset: {attr_type:?} not found in record {record_number}")
        })?;
    if !loc.is_resident {
        return Err(format!(
            "resident_value_disk_offset: {attr_type:?} in record {record_number} is non-resident"
        ));
    }
    let value_len = loc.resident_value_length.unwrap_or(0) as usize;
    if value_len < min_value_len {
        return Err(format!(
            "resident_value_disk_offset: {attr_type:?} value is {value_len} bytes, need >= {min_value_len}"
        ));
    }
    let value_offset = loc
        .resident_value_offset
        .ok_or("resident attribute has no value offset")? as u64;
    // Use `holder` (the record actually containing the attribute, which may be
    // an `$ATTRIBUTE_LIST` extension record), not the base `record_number`.
    Ok(crate::mft_io::mft_record_offset(&params, holder) + loc.attr_offset as u64 + value_offset)
}

/// On-disk byte offset and logical length of a *non-resident* attribute that
/// occupies a **single contiguous allocated extent** — e.g. `$LogFile`'s
/// `$DATA`, which fsck overwrites with `0xFF` as one flat `[offset, offset +
/// length)` range. Refuses anything that isn't exactly one allocated run long
/// enough to cover the logical length (a fragmented or sparse layout would
/// make the flat-range write clobber unrelated clusters between runs), as well
/// as absent / resident attributes.
pub fn nonresident_contiguous_disk_range<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    attr_type: AttrType,
    name: Option<&str>,
) -> Result<(u64, u64), String> {
    let (params, _holder, record, loc) =
        locate_attribute(io, record_number, attr_type, name)?.ok_or_else(|| {
            format!(
                "nonresident_contiguous_disk_range: {attr_type:?} not found in record {record_number}"
            )
        })?;
    if loc.is_resident {
        return Err(format!(
            "nonresident_contiguous_disk_range: {attr_type:?} in record {record_number} is resident"
        ));
    }
    let length = loc
        .non_resident_value_length
        .ok_or("non-resident attribute has no data length")?;
    let mpo = loc
        .non_resident_mapping_pairs_offset
        .ok_or("non-resident attribute has no mapping-pairs offset")? as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])?;

    // Exactly one allocated run, covering the whole logical length. Callers
    // treat the result as a single flat range, so refuse fragmented / sparse
    // layouts rather than risk writing into clusters between runs.
    if runs.len() != 1 {
        return Err(format!(
            "nonresident_contiguous_disk_range: {attr_type:?} in record {record_number} is not a \
             single extent ({} runs); refusing flat-range access",
            runs.len()
        ));
    }
    let run = &runs[0];
    let lcn = run
        .lcn
        .ok_or("non-resident attribute's only run is a sparse hole")?;
    // THE RANGE HAS TO BE ON THE DEVICE.
    //
    // The only guard here used to compare two fields of the same
    // attribute against each other, and its multiply wrapped. Both come
    // off the disk, and callers treat the answer as a flat range to
    // write over: `fsck::reset_logfile_io` fills it with 0xFF. A
    // `$LogFile` naming a run of 2^30 clusters at an lcn of the
    // caller's choosing directed a terabyte of 0xFF anywhere on the
    // device -- over the MFT on a raw disk, or inflating a sparse image
    // until the host volume filled.
    let extent_bytes = run
        .length
        .checked_mul(params.cluster_size)
        .ok_or_else(|| format!("{attr_type:?} extent length overflows"))?;
    if extent_bytes < length {
        return Err(format!(
            "nonresident_contiguous_disk_range: {attr_type:?} extent ({extent_bytes} bytes) shorter \
             than value length ({length})"
        ));
    }
    let start = lcn
        .checked_mul(params.cluster_size)
        .ok_or_else(|| format!("{attr_type:?} extent starts past the address space"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| format!("{attr_type:?} extent ends past the address space"))?;
    let device = io.size();
    if end > device {
        return Err(format!(
            "nonresident_contiguous_disk_range: {attr_type:?} in record {record_number} spans \
             [{start}, {end}) on a device of {device} bytes"
        ));
    }
    Ok((start, length))
}

/// Record numbers of every metafile whose on-disk storage must never be
/// handed out by `$Bitmap`'s allocator, or overwritten by `fsck`'s
/// `$LogFile` reset -- beyond `$MFT` (record 0), which every caller
/// already locates by other means, because it needs `$MFT`'s length for
/// its own purposes anyway.
///
/// See rust-fs-ntfs#157: `BitmapLocation::covers_the_volumes_own` and
/// `fsck`'s `forbidden_fill_ranges` each protected only the boot sector
/// and `$MFT`, so a file record whose run list happened to overlap
/// `$MFTMirr`, `$Bitmap`'s own storage, `$LogFile`, `$AttrDef`,
/// `$Secure` or `$UpCase` could free or overwrite live volume metadata
/// with no refusal at all.
pub const OTHER_PROTECTED_METAFILE_RECORDS: [(u64, Option<&str>, &str); 8] = [
    // $MFT itself. It belongs in this list rather than being handled
    // separately by `mft_clusters`, because that scalar is derived from
    // `nonresident_contiguous_disk_range`, which REFUSES a fragmented
    // attribute -- and a fragmented `$MFT` is ordinary. The image
    // shipped as the `ntfs` crate's own `testdata/testfs1` has SIX runs
    // at format time. See rust-fs-ntfs#157.
    (0, None, "$MFT"),
    (1, None, "$MFTMirr"),
    (2, None, "$LogFile"),
    (4, None, "$AttrDef"),
    (6, None, "$Bitmap"),
    // $Boot. The `lcn == 0` special case in `covers_the_volumes_own`
    // guards ONE cluster, and `$Boot`'s $DATA is 8192 bytes -- two
    // clusters at 4096, sixteen at 512. Measured before this entry
    // existed: on a 4096-byte-cluster volume LCN 1 was unprotected and
    // `free_io` on it succeeded; on a 512-byte-cluster volume fifteen
    // of the sixteen boot-region clusters were freeable.
    (7, None, "$Boot"),
    // NOT the unnamed stream. `$Secure` keeps its security descriptors
    // in the NAMED `$SDS` stream; its unnamed `$DATA` does not exist on
    // a third-party-formatted volume at all, and is a small resident
    // stub on one this crate formats itself. Asking for the unnamed
    // stream therefore protected `$Secure` on NO volume, while also
    // being the lookup whose failure triggered a whole-volume refusal.
    // Measured on testfs1: unnamed $DATA "not found", `$SDS` at
    // (289792, 262396).
    (9, Some("$SDS"), "$Secure:$SDS"),
    (10, None, "$UpCase"),
];

/// The byte range one decoded run protects, or `None` if the run
/// cannot describe real storage on this volume.
///
/// AN IMPOSSIBLE RUN IS EVIDENCE OF DAMAGE, NOT SOMETHING TO PROTECT.
/// That distinction is what keeps a damaged record from turning these
/// guards into an outage. A run's length comes off the same record the
/// guards exist to defend against -- rust-fs-ntfs#157's own example of
/// how one gets damaged is mapping pairs partially overwritten by an
/// interrupted write -- and until this function existed nothing bounded
/// it. Measured on a 64 MiB volume, 4096-byte clusters, 16384 clusters,
/// from mapping pairs `[0x14, 0x00, 0x00, 0x00, 0x40, 0x01, 0x00]`
/// written into one metafile record:
///
///     decode_runs -> DataRun { length: 1073741824, lcn: Some(1) }
///     -> byte range (4096, 4398046515200) -> clusters (1, 1073741825)
///     -> 16384 of 16384 clusters refused, ordinary cluster 5000 among
///        them, `free_io` refused for every file on the volume
///
/// That is the volume-wide refusal an earlier revision of this fix was
/// returned for, reached from one damaged record rather than from a
/// code path. NOTHING OVERFLOWS ANYWHERE IN IT: `1073741824 * 4096`
/// fits in a `u64` with room to spare, so swapping a `saturating_add`
/// for a `checked_add` changes nothing measurable here. The length
/// itself is the problem.
///
/// Two bounds, because either alone is evadable:
///
/// 1. **The run may not be longer than the attribute says it is.**
///    `declared_clusters` is the attribute's own non-resident value
///    length in clusters, a different field of the same record from the
///    mapping pairs. Measured across every protected metafile of two
///    volumes -- one this crate formatted, one from a third-party
///    formatter -- the largest single run of each was never longer than
///    its own declared length:
///
///        mkfs 64 MiB:  $MFT 64/64, $MFTMirr 4/4, $LogFile 944/944,
///                      $AttrDef 1/1, $Bitmap 1/1, $Boot 2/2,
///                      $Secure:$SDS 1/65, $UpCase 32/32
///        third party:  $MFT 512/1162, $MFTMirr 8/8, $LogFile 512/512,
///                      $AttrDef 5/5, $Bitmap 1/1, $Boot 16/16,
///                      $Secure:$SDS 513/513, $UpCase 256/256
///
///    (biggest run / declared. `$MFT`'s runs there SUM to 1174 against
///    a declared 1162 -- allocation legitimately exceeds data length --
///    so this bounds each run, not their total.)
///
/// 2. **The run must fit inside the volume.** `[lcn, lcn + length)`
///    within the cluster capacity, and the byte range intersected with
///    `[0, volume_bytes())`.
///
/// Bound 2 alone is not enough, and this is the measurement that says
/// so: a damaged length of `capacity - 1` clusters at LCN 1 FITS, so it
/// survives a volume-only bound and refuses 16383 of 16384 clusters --
/// revision one's outage, one arithmetic step away. Bound 1 refuses it
/// against `$MFTMirr`'s declared 4 clusters. Bound 1 alone is not
/// enough either, since a run of a plausible length can still start
/// beyond the volume.
///
/// A run that fails either bound is DISCARDED, not clamped: clamping an
/// absurd length to the volume's end protects almost every cluster,
/// which is the same outage wearing a bound. Discarding costs that one
/// run's protection and leaves the record's other runs intact.
///
/// The residual, stated rather than implied: value length and mapping
/// pairs are separate fields, so a record damaged in BOTH -- an
/// inflated value length AND a matching absurd run -- can still widen
/// what is refused, up to the volume. No guard reading only that record
/// can do better; it would need an independent witness of the truth.
fn run_protected_range(
    lcn: u64,
    length: u64,
    declared_clusters: u64,
    params: &crate::mft_io::BootParams,
) -> Option<(u64, u64)> {
    let volume_bytes = params.volume_bytes();
    if volume_bytes == 0 || length == 0 {
        return None;
    }
    // Bound 1: the attribute's own declared size. `max(1)` because a
    // declared length of zero with a real run is itself odd, and one
    // cluster is the smallest thing worth protecting.
    if length > declared_clusters.max(1) {
        return None;
    }
    let cluster_size = params.cluster_size.max(1);
    let capacity = volume_bytes.div_ceil(cluster_size);
    // Bound 2: the run fits inside the volume.
    if lcn.saturating_add(length) > capacity {
        return None;
    }
    let start = lcn.checked_mul(cluster_size)?;
    let end = length
        .checked_mul(cluster_size)
        .and_then(|len| start.checked_add(len))?
        .min(volume_bytes);
    if end <= start {
        return None;
    }
    Some((start, end))
}

/// Every physical extent of record `record_number`'s unnamed `$DATA`,
/// as byte ranges `[start, end)`, in run order. Sparse holes (`lcn ==
/// None`) are skipped -- they hold no clusters to protect.
///
/// Unlike [`nonresident_contiguous_disk_range`], this does NOT refuse a
/// fragmented attribute. Several of the files this backs -- `$Bitmap`
/// itself, notably -- are legitimately laid out in more than one run on
/// an ordinary volume (see `bitmap::make_fragmented_bm` in this crate's
/// own tests), and refusing them here would make an unremarkable volume
/// look too dangerous to ever free a cluster on. The single-extent
/// restriction exists for callers that write a flat byte range over the
/// result; this one only reports where the attribute's clusters already
/// are.
fn nonresident_disk_ranges_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    name: Option<&str>,
) -> Result<Vec<(u64, u64)>, String> {
    let (params, _holder, record, loc) = locate_attribute(io, record_number, AttrType::Data, name)?
        .ok_or_else(|| format!("record {record_number}: no $DATA named {name:?}"))?;
    if loc.is_resident {
        // NOT A FAILURE. A resident attribute's bytes live inside the
        // MFT record itself, not in clusters `$Bitmap` tracks -- there
        // is nothing separate for a corrupted run list to be aimed at,
        // so there is nothing to add here. `$AttrDef`, `$Secure` and
        // `$UpCase` in particular are ordinarily small enough to be
        // resident on a freshly formatted or otherwise modest volume;
        // treating that as "could not determine, protect everything"
        // made `fsck` refuse its own `$LogFile` reset on an entirely
        // unremarkable volume the first time this was measured against
        // a real one, which is the class of false positive this
        // function exists to avoid causing.
        return Ok(Vec::new());
    }
    let mpo = loc
        .non_resident_mapping_pairs_offset
        .ok_or("non-resident attribute has no mapping-pairs offset")? as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])?;
    // The attribute's own declared size, which `run_protected_range`
    // bounds each run against. A different field of the same record
    // from the mapping pairs, which is what makes it a cross-check.
    let declared_clusters = loc
        .non_resident_value_length
        .unwrap_or(0)
        .div_ceil(params.cluster_size.max(1));
    Ok(runs
        .iter()
        .filter_map(|r| {
            // A DISCARDED RUN IS NOT AN ERROR. The rest of this
            // record's runs may be perfectly good, and one impossible
            // run must not cost the whole record its protection --
            // still less turn into a volume-wide refusal.
            run_protected_range(r.lcn?, r.length, declared_clusters, &params)
        })
        .collect())
}

/// Byte ranges `[start, end)` that must never be freed or overwritten:
/// the on-disk storage of every metafile in
/// [`OTHER_PROTECTED_METAFILE_RECORDS`].
///
/// `$MFT`'s own extent, with a bounded fallback when record 0 will not
/// decode.
///
/// `$MFT` is the one metafile whose absence from the protected set
/// restores this issue's opening sentence: `unlink` freeing the
/// clusters that hold every file record on the volume. Measured on a
/// formatted volume with record 0 blanked, before this function
/// existed: `(4, 68)` vanished from the protected set,
/// `covers_the_volumes_own(mft_lcn, 1)` answered false, and
/// `free_io` on `$MFT`'s own first cluster SUCCEEDED.
///
/// So this one fails closed, unlike the best-effort treatment the other
/// metafiles get -- and BOUNDED, never to the whole volume, which is
/// the distinction that makes it safe. The whole-volume fallback an
/// earlier revision used refused 4095 of 4095 clusters on an ordinary
/// third-party volume and broke `rm` on every file. That was caused by
/// two lookup bugs, both since fixed: `$MFT` located by a
/// single-extent-only helper that refuses an ordinary fragmented
/// `$MFT`, and `$Secure` asked for an unnamed `$DATA` that does not
/// exist. With those fixed, all seven records resolve on both a
/// volume this crate formats and a third-party one -- twelve ranges on
/// the latter, eight on the former -- so the tiers below fire on
/// neither healthy volume.
///
/// Three tiers, narrowest first:
///
/// 1. record 0's own run list, via the multi-run helper;
/// 2. `$MFTMirr`'s copy of record 0. The mirror holds records 0..3 and
///    is independently locatable, so a `$MFT` whose own record is
///    damaged can still say where it lives. This is the case `chkdsk`
///    repairs from, and the reason `$MFTMirr` exists;
/// 3. a bounded region from `params.mft_lcn` -- enough to hold the
///    sixteen reserved system records. A FLOOR, not a claim about
///    `$MFT`'s true length: a larger `$MFT` has its tail unprotected
///    in this already-degraded case, which is strictly better than the
///    nothing that was protected before and strictly better than the
///    volume-wide refusal that broke ordinary use.
fn mft_ranges_io<T: BlockIo + ?Sized>(
    io: &mut T,
    params: &crate::mft_io::BootParams,
) -> Vec<(u64, u64)> {
    // Tier 1: record 0 itself.
    if let Ok(runs) = nonresident_disk_ranges_io(io, 0, None) {
        if !runs.is_empty() {
            return runs;
        }
    }

    // Tier 2: the mirror's copy of record 0.
    if let Ok(mirror) = nonresident_disk_ranges_io(io, 1, None) {
        if let Some(&(mirror_start, _)) = mirror.first() {
            let size = params.file_record_size.max(1) as usize;
            let mut record = vec![0u8; size];
            if io.read_exact_at(mirror_start, &mut record).is_ok()
                && crate::mft_io::apply_fixup_on_read(&mut record, params.bytes_per_sector).is_ok()
            {
                if let Some(loc) = attr_io::find_attribute(&record, AttrType::Data, None) {
                    if !loc.is_resident {
                        if let Some(mpo) = loc.non_resident_mapping_pairs_offset {
                            let start = loc.attr_offset + mpo as usize;
                            let end = loc.attr_offset + loc.attr_length;
                            if start < end && end <= record.len() {
                                if let Ok(runs) = data_runs::decode_runs(&record[start..end]) {
                                    let declared = loc
                                        .non_resident_value_length
                                        .unwrap_or(0)
                                        .div_ceil(params.cluster_size.max(1));
                                    let ranges: Vec<(u64, u64)> = runs
                                        .iter()
                                        .filter_map(|r| {
                                            run_protected_range(r.lcn?, r.length, declared, params)
                                        })
                                        .collect();
                                    if !ranges.is_empty() {
                                        return ranges;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Tier 3: a bounded floor at $MFT's declared start.
    const RESERVED_RECORDS: u64 = 16;
    let start = params.mft_lcn.saturating_mul(params.cluster_size);
    let floor = params
        .file_record_size
        .max(1)
        .saturating_mul(RESERVED_RECORDS);
    vec![(start, start.saturating_add(floor))]
}

/// BEST EFFORT PER METAFILE, and deliberately NOT fail-closed to the
/// whole volume. A record that cannot be located contributes no ranges
/// and the rest are still protected.
///
/// The first version of this did fail closed -- any lookup failure
/// returned one range spanning the whole volume, on the reasoning that
/// protecting too much costs a refused operation while protecting too
/// little corrupts the volume. Measured against a volume this crate did
/// not format (the `ntfs` crate's own `testdata/testfs1`, which this
/// crate reads correctly), that reasoning was inverted by the numbers:
/// `$Secure`'s unnamed `$DATA` does not exist there, the fallback fired,
/// and `rm` on an ordinary file was refused with 4095 of 4095 clusters
/// declared the volume's own. The defect this function closes needs a
/// MALFORMED run list to fire and hurts almost nobody; a false positive
/// here breaks every `unlink` on every volume, every time. The
/// asymmetry runs the other way from how it first reads.
///
/// So a metafile whose storage cannot be located is simply not added.
/// That is still strictly more than the boot sector and `$MFT` this
/// crate protected before, which is what rust-fs-ntfs#157 is about.
///
/// `exclude_record` leaves one record out -- for a caller about to
/// legitimately overwrite that record's own storage, so its own target
/// does not "overlap" the very list checking it.
pub fn other_protected_metafile_ranges_io<T: BlockIo + ?Sized>(
    io: &mut T,
    exclude_record: Option<u64>,
) -> Vec<(u64, u64)> {
    let mut ranges = Vec::with_capacity(OTHER_PROTECTED_METAFILE_RECORDS.len());
    // `$MFT` is not best-effort: see `mft_ranges_io`. Its absence from
    // the set is the defect this whole guard exists to close, so it
    // fails closed -- bounded, never volume-wide.
    if exclude_record != Some(0) {
        if let Ok(params) = crate::mft_io::read_boot_params_io(io) {
            ranges.extend(mft_ranges_io(io, &params));
        }
    }
    for &(record_number, name, _label) in &OTHER_PROTECTED_METAFILE_RECORDS {
        if record_number == 0 {
            continue; // handled above, with its own fallback tiers
        }
        // A caller about to overwrite ITS OWN metafile's storage on
        // purpose -- `fsck`'s `$LogFile` reset is the one case here --
        // excludes that record, or its own target always "overlaps"
        // the list it is being checked against.
        if exclude_record == Some(record_number) {
            continue;
        }
        if let Ok(runs) = nonresident_disk_ranges_io(io, record_number, name) {
            ranges.extend(runs);
        }
    }
    ranges
}

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub record_number: u64,
    pub is_dir: bool,
}

/// Enumerate a directory's entries natively (no upstream `ntfs` crate),
/// merging the resident `$INDEX_ROOT` with any spilled `$INDEX_ALLOCATION`
/// (INDX) blocks. DOS-namespace-only entries (the 8.3 shadow names) are
/// skipped, matching the canonical Win32 listing; `is_dir` is read from each
/// target record's flags. Order follows index/B-tree order, which is not a
/// global sort across blocks — callers that need sorted output should sort.
pub fn read_dir_entries<T: BlockIo + ?Sized>(
    io: &mut T,
    dir_record: u64,
) -> Result<Vec<DirEntry>, String> {
    let (_, dir_bytes) = read_mft_record_io(io, dir_record)?;
    if record_flags(&dir_bytes) & MFT_FLAG_DIRECTORY == 0 {
        return Err(format!(
            "read_dir_entries: record {dir_record} is not a directory"
        ));
    }

    let mut raw = Vec::new();
    index_io::collect_index_root_entries(&dir_bytes, &mut raw)?;
    if index_io::index_root_flags(&dir_bytes).is_some_and(|f| f & IH_FLAG_HAS_SUBNODES != 0) {
        let ia = idx_block::load_for_directory_io(io, dir_record)?;
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block_io(io, &ia, vcn)?;
            index_io::collect_indx_block_entries(&block, &mut raw)?;
        }
    }

    /// `$FILE_NAME.file_attributes` directory bit — how NTFS marks a directory
    /// in an index entry (matches upstream `is_directory()`).
    const FN_IS_DIRECTORY: u32 = 0x1000_0000;
    let out = raw
        .into_iter()
        .filter(|e| e.namespace != 2) // skip DOS 8.3 shadow names
        .map(|e| DirEntry {
            name: e.name,
            record_number: e.file_record_number,
            is_dir: e.file_attributes & FN_IS_DIRECTORY != 0,
        })
        .collect();
    Ok(out)
}

/// One decoded `$ATTRIBUTE_LIST` entry: which MFT record holds an instance of
/// an attribute when a file's attributes overflow its base record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrListEntry {
    pub type_code: u32,
    /// Attribute name (e.g. an ADS stream name), or `None` for unnamed.
    pub name: Option<String>,
    /// First VCN this instance covers (for an attribute split across records).
    pub starting_vcn: u64,
    /// MFT record number holding this attribute instance (low 48 bits of the
    /// entry's base_file_reference).
    pub record_number: u64,
    pub attribute_id: u16,
}

/// Parse an `$ATTRIBUTE_LIST` attribute value into its entries. Each entry is:
/// type(4) length(2) name_len(1) name_off(1) starting_vcn(8)
/// base_file_reference(8) attribute_id(2) name(name_len × UTF-16). Entries are
/// walked by their `length` field until the value is exhausted.
pub fn parse_attribute_list(value: &[u8]) -> Result<Vec<AttrListEntry>, String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor + 0x1A <= value.len() {
        let type_code = u32::from_le_bytes(value[cursor..cursor + 4].try_into().unwrap());
        let length = u16::from_le_bytes([value[cursor + 4], value[cursor + 5]]) as usize;
        if length == 0 {
            break;
        }
        if cursor + length > value.len() {
            return Err(format!(
                "$ATTRIBUTE_LIST entry at {cursor} (len {length}) overruns the value"
            ));
        }
        let name_length = value[cursor + 6] as usize;
        let name_offset = value[cursor + 7] as usize;
        let starting_vcn = u64::from_le_bytes(value[cursor + 8..cursor + 16].try_into().unwrap());
        let base_ref = u64::from_le_bytes(value[cursor + 16..cursor + 24].try_into().unwrap());
        let attribute_id = u16::from_le_bytes([value[cursor + 24], value[cursor + 25]]);

        let name = if name_length == 0 {
            None
        } else {
            let ns = cursor + name_offset;
            if ns + name_length * 2 > value.len() {
                return Err(format!(
                    "$ATTRIBUTE_LIST entry at {cursor} name overruns the value"
                ));
            }
            Some(
                char::decode_utf16(
                    value[ns..ns + name_length * 2]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                )
                .map(|r| r.unwrap_or('\u{FFFD}'))
                .collect(),
            )
        };

        out.push(AttrListEntry {
            type_code,
            name,
            starting_vcn,
            record_number: base_ref & 0x0000_FFFF_FFFF_FFFF,
            attribute_id,
        });
        cursor += length;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_io::{BlockIo, IoReadSeek};
    use crate::mkfs::format_filesystem;
    use crate::write;
    use ntfs::indexes::NtfsFileNameIndex;
    use ntfs::structured_values::{NtfsFileNamespace, NtfsStandardInformation};
    use ntfs::{Ntfs, NtfsReadSeek};

    /// In-memory volume so the cross-check has no fixture dependency.
    struct MemDev {
        buf: Vec<u8>,
    }
    impl BlockIo for MemDev {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            let off = offset as usize;
            buf.copy_from_slice(&self.buf[off..off + buf.len()]);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            let off = offset as usize;
            self.buf[off..off + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn size(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    fn fresh_vol() -> MemDev {
        const SIZE: u64 = 32 * 1024 * 1024;
        let mut dev = MemDev {
            buf: vec![0u8; SIZE as usize],
        };
        format_filesystem(
            &mut dev as &mut dyn BlockIo,
            SIZE,
            4096,
            4096,
            Some("NREAD"),
            Some(0xABCD_1234),
        )
        .expect("format");
        dev
    }

    /// The oracle: resolve the same path through the upstream `ntfs` crate.
    fn upstream_resolve(dev: &mut MemDev, path: &str) -> u64 {
        let mut reader = IoReadSeek::new(dev);
        let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
        ntfs.read_upcase_table(&mut reader).expect("upcase");
        let mut cur = ntfs.root_directory(&mut reader).expect("root");
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let index = cur.directory_index(&mut reader).expect("dir index");
            let mut finder = index.finder();
            let entry = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
                .expect("entry present")
                .expect("entry ok");
            cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
        }
        cur.file_record_number()
    }

    #[test]
    fn root_resolves_to_record_5() {
        let mut dev = fresh_vol();
        assert_eq!(resolve_path(&mut dev, "/").unwrap(), ROOT_RECORD_NUMBER);
        assert_eq!(resolve_path(&mut dev, "").unwrap(), ROOT_RECORD_NUMBER);
    }

    #[test]
    fn native_matches_upstream_for_files_and_dirs() {
        let mut dev = fresh_vol();
        write::mkdir_io(&mut dev, "/", "sub").expect("mkdir");
        write::create_file_io(&mut dev, "/", "top.txt").expect("create top");
        write::create_file_io(&mut dev, "/sub", "inner.bin").expect("create inner");

        for path in ["/top.txt", "/sub", "/sub/inner.bin"] {
            let native = resolve_path(&mut dev, path).expect("native resolve");
            let oracle = upstream_resolve(&mut dev, path);
            assert_eq!(
                native, oracle,
                "native vs upstream record number disagree for {path}"
            );
        }
    }

    #[test]
    fn case_insensitive_lookup_matches_upstream() {
        let mut dev = fresh_vol();
        // Store with mixed case, then look it up with different casings —
        // native (upcase-collated) must find it and agree with the upstream
        // oracle (which is case-insensitive by default).
        write::create_file_io(&mut dev, "/", "MixedCase.txt").expect("create");
        write::mkdir_io(&mut dev, "/", "SubDir").expect("mkdir");
        write::create_file_io(&mut dev, "/SubDir", "Inner.BIN").expect("create inner");

        for q in [
            "/MixedCase.txt",
            "/mixedcase.txt",
            "/MIXEDCASE.TXT",
            "/subdir/inner.bin",
            "/SUBDIR/Inner.BIN",
        ] {
            let native = resolve_path(&mut dev, q).expect("native resolve");
            let oracle = upstream_resolve(&mut dev, q);
            assert_eq!(native, oracle, "case-insensitive resolve mismatch for {q}");
        }
    }

    #[test]
    fn dot_and_dotdot_components_resolve() {
        let mut dev = fresh_vol();
        write::mkdir_io(&mut dev, "/", "sub").expect("mkdir");
        write::create_file_io(&mut dev, "/sub", "inner.bin").expect("create");
        let inner = resolve_path(&mut dev, "/sub/inner.bin").expect("resolve");

        // "." stays put; ".." goes to the parent; root is its own parent.
        assert_eq!(resolve_path(&mut dev, "/./sub/./inner.bin").unwrap(), inner);
        assert_eq!(
            resolve_path(&mut dev, "/sub/../sub/inner.bin").unwrap(),
            inner
        );
        assert_eq!(
            resolve_path(&mut dev, "/sub/..").unwrap(),
            ROOT_RECORD_NUMBER
        );
        assert_eq!(resolve_path(&mut dev, "/..").unwrap(), ROOT_RECORD_NUMBER);
        // read_parent_record agrees.
        let sub = resolve_path(&mut dev, "/sub").unwrap();
        assert_eq!(
            read_parent_record(&mut dev, sub).unwrap(),
            ROOT_RECORD_NUMBER
        );
        assert_eq!(read_parent_record(&mut dev, inner).unwrap(), sub);
    }

    #[test]
    fn missing_path_errors() {
        let mut dev = fresh_vol();
        assert!(resolve_path(&mut dev, "/nope.txt").is_err());
        write::create_file_io(&mut dev, "/", "f").expect("create");
        // A file is not a directory: can't descend through it.
        assert!(resolve_path(&mut dev, "/f/child").is_err());
    }

    /// Oracle: read the unnamed `$DATA` of `path` through the upstream crate.
    fn upstream_read_data(dev: &mut MemDev, path: &str) -> Vec<u8> {
        let mut reader = IoReadSeek::new(dev);
        let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
        ntfs.read_upcase_table(&mut reader).expect("upcase");
        let mut cur = ntfs.root_directory(&mut reader).expect("root");
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let index = cur.directory_index(&mut reader).expect("dir index");
            let mut finder = index.finder();
            let entry = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
                .expect("entry present")
                .expect("entry ok");
            cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
        }
        let data_item = cur
            .data(&mut reader, "")
            .expect("has $DATA")
            .expect("data item");
        let data = data_item.to_attribute().expect("attr");
        let mut value = data.value(&mut reader).expect("value");
        let mut out = vec![0u8; value.len() as usize];
        let mut filled = 0usize;
        while filled < out.len() {
            let n = value.read(&mut reader, &mut out[filled..]).expect("read");
            if n == 0 {
                break;
            }
            filled += n;
        }
        out.truncate(filled);
        out
    }

    /// Native read of the unnamed `$DATA` of `path` via resolve_path +
    /// read_attribute_value (the code under test).
    fn native_read_data(dev: &mut MemDev, path: &str) -> Vec<u8> {
        let rec = resolve_path(dev, path).expect("resolve");
        read_attribute_value(dev, rec, AttrType::Data, None).expect("read value")
    }

    #[test]
    fn resident_data_matches_upstream() {
        let mut dev = fresh_vol();
        write::create_file_io(&mut dev, "/", "r.txt").expect("create");
        write::write_file_contents_io(&mut dev, "/r.txt", b"hello resident world").expect("write");
        let native = native_read_data(&mut dev, "/r.txt");
        assert_eq!(native, b"hello resident world");
        assert_eq!(native, upstream_read_data(&mut dev, "/r.txt"));
    }

    #[test]
    fn nonresident_sparse_data_matches_upstream() {
        // 3 clusters: data | hole (all-zero) | data. write_sparse_file makes
        // the middle cluster a hole, exercising non-resident run reading +
        // hole zero-fill in read_attribute_value.
        let cs = 4096usize;
        let mut data = vec![0u8; cs * 3];
        for (i, b) in data[..cs].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        for (i, b) in data[cs * 2..].iter_mut().enumerate() {
            *b = (i % 241 + 5) as u8;
        }
        // middle cluster stays all-zero → a hole.

        let mut dev = fresh_vol();
        write::create_file_io(&mut dev, "/", "sparse.bin").expect("create");
        write::write_sparse_file_io(&mut dev, "/sparse.bin", &data).expect("sparse write");

        let native = native_read_data(&mut dev, "/sparse.bin");
        assert_eq!(native.len(), data.len(), "length matches data_size");
        assert_eq!(
            native, data,
            "native read reconstructs data incl. hole=zeros"
        );
        assert_eq!(
            native,
            upstream_read_data(&mut dev, "/sparse.bin"),
            "native vs upstream byte mismatch on sparse file"
        );
    }

    #[test]
    fn ranged_read_returns_correct_window_without_full_read() {
        let mut dev = fresh_vol();
        // Non-resident file (> a cluster) with a deterministic pattern.
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        write::create_file_io(&mut dev, "/", "big.bin").expect("create");
        write::write_file_contents_io(&mut dev, "/big.bin", &data).expect("write");
        let rec = resolve_path(&mut dev, "/big.bin").unwrap();

        // Windows: at 0, crossing a cluster boundary, near EOF, and past EOF.
        for (off, len) in [
            (0u64, 300usize),
            (4090, 20),
            (4096, 4096),
            (19_990, 50),
            (25_000, 10),
        ] {
            let got = read_attribute_range(&mut dev, rec, AttrType::Data, None, off, len).unwrap();
            let s = (off as usize).min(data.len());
            let e = (off as usize + len).min(data.len());
            assert_eq!(got, &data[s..e], "window off={off} len={len}");
        }
        // Full range equals the whole-value read.
        let whole = read_attribute_value(&mut dev, rec, AttrType::Data, None).unwrap();
        let ranged_all =
            read_attribute_range(&mut dev, rec, AttrType::Data, None, 0, data.len()).unwrap();
        assert_eq!(ranged_all, whole);
    }

    #[test]
    fn missing_attribute_errors() {
        let mut dev = fresh_vol();
        write::create_file_io(&mut dev, "/", "x").expect("create");
        let rec = resolve_path(&mut dev, "/x").unwrap();
        // No $INDEX_ROOT on a regular file.
        assert!(read_attribute_value(&mut dev, rec, AttrType::IndexRoot, None).is_err());
    }

    #[test]
    fn nt_to_unix_known_values() {
        // NTFS epoch (1601-01-01) maps to -11_644_473_600 Unix seconds.
        assert_eq!(nt_to_unix(0), -11_644_473_600);
        // Unix epoch (1970-01-01) is 116_444_736_000_000_000 in NTFS 100ns.
        assert_eq!(nt_to_unix(116_444_736_000_000_000), 0);
        // One second past the Unix epoch.
        assert_eq!(nt_to_unix(116_444_736_010_000_000), 1);
    }

    /// Oracle: read a record's SI timestamps/attributes + size via upstream.
    fn upstream_stat(dev: &mut MemDev, path: &str) -> (u32, [u64; 4], u64, bool) {
        let mut reader = IoReadSeek::new(dev);
        let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
        ntfs.read_upcase_table(&mut reader).expect("upcase");
        let mut cur = ntfs.root_directory(&mut reader).expect("root");
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let index = cur.directory_index(&mut reader).expect("dir index");
            let mut finder = index.finder();
            let entry = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
                .expect("entry present")
                .expect("entry ok");
            cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
        }
        let is_dir = cur.is_directory();
        let size = match cur.data(&mut reader, "") {
            Some(Ok(item)) => item.to_attribute().map(|a| a.value_length()).unwrap_or(0),
            _ => 0,
        };
        let si: NtfsStandardInformation = cur.info().expect("$STANDARD_INFORMATION");
        let times = [
            si.creation_time().nt_timestamp(),
            si.modification_time().nt_timestamp(),
            si.mft_record_modification_time().nt_timestamp(),
            si.access_time().nt_timestamp(),
        ];
        (si.file_attributes().bits(), times, size, is_dir)
    }

    #[test]
    fn stat_matches_upstream_file_and_dir() {
        let mut dev = fresh_vol();
        write::create_file_io(&mut dev, "/", "f.txt").expect("create file");
        write::write_file_contents_io(&mut dev, "/f.txt", b"twelve bytes").expect("write");
        write::mkdir_io(&mut dev, "/", "d").expect("mkdir");

        for (path, want_dir) in [("/f.txt", false), ("/d", true)] {
            let rec = resolve_path(&mut dev, path).expect("resolve");
            let st = read_stat(&mut dev, rec).expect("stat");
            let (u_attrs, u_times, u_size, u_is_dir) = upstream_stat(&mut dev, path);

            assert_eq!(st.is_dir, want_dir, "is_dir for {path}");
            assert_eq!(st.is_dir, u_is_dir, "is_dir vs upstream for {path}");
            assert_eq!(
                st.file_attributes, u_attrs,
                "file_attributes vs upstream for {path}"
            );
            assert_eq!(st.size, u_size, "size vs upstream for {path}");
            assert_eq!(
                [
                    st.created_nt,
                    st.modified_nt,
                    st.mft_modified_nt,
                    st.accessed_nt
                ],
                u_times,
                "timestamps vs upstream for {path}"
            );
        }
    }

    /// Oracle: list a directory's entries through the upstream crate, as a
    /// sorted set of (name, record, is_dir).
    fn upstream_list(dev: &mut MemDev, dir_path: &str) -> Vec<(String, u64, bool)> {
        let mut reader = IoReadSeek::new(dev);
        let mut ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
        ntfs.read_upcase_table(&mut reader).expect("upcase");
        let mut cur = ntfs.root_directory(&mut reader).expect("root");
        for comp in dir_path.split('/').filter(|c| !c.is_empty()) {
            let index = cur.directory_index(&mut reader).expect("dir index");
            let mut finder = index.finder();
            let entry = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut reader, comp)
                .expect("entry present")
                .expect("entry ok");
            cur = entry.to_file(&ntfs, &mut reader).expect("to_file");
        }
        let index = cur.directory_index(&mut reader).expect("directory_index");
        let mut iter = index.entries();
        let mut out = Vec::new();
        while let Some(entry) = iter.next(&mut reader) {
            let Ok(entry) = entry else { continue };
            let Some(Ok(file_name)) = entry.key() else {
                continue;
            };
            if file_name.namespace() == NtfsFileNamespace::Dos {
                continue;
            }
            out.push((
                file_name.name().to_string_lossy(),
                entry.file_reference().file_record_number(),
                file_name.is_directory(),
            ));
        }
        out.sort();
        out
    }

    fn native_list(dev: &mut MemDev, dir_path: &str) -> Vec<(String, u64, bool)> {
        let rec = resolve_path(dev, dir_path).expect("resolve dir");
        let mut v: Vec<(String, u64, bool)> = read_dir_entries(dev, rec)
            .expect("read_dir_entries")
            .into_iter()
            .map(|e| (e.name, e.record_number, e.is_dir))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn dir_listing_matches_upstream() {
        let mut dev = fresh_vol();
        write::mkdir_io(&mut dev, "/", "d").expect("mkdir d");
        write::create_file_io(&mut dev, "/d", "alpha.txt").expect("a");
        write::create_file_io(&mut dev, "/d", "beta.bin").expect("b");
        write::mkdir_io(&mut dev, "/d", "child").expect("child");
        write::create_file_io(&mut dev, "/d", "zeta").expect("z");

        // Subdirectory listing.
        let native = native_list(&mut dev, "/d");
        assert_eq!(
            native,
            upstream_list(&mut dev, "/d"),
            "subdir listing mismatch"
        );
        // Our own entries are all present + typed.
        assert!(native.contains(&(
            "child".to_string(),
            { resolve_path(&mut dev, "/d/child").unwrap() },
            true
        )));
        assert!(native
            .iter()
            .any(|(n, _, is_dir)| n == "alpha.txt" && !is_dir));

        // Root listing (includes the $-prefixed system files) must match too —
        // including is_dir, which (like upstream) we read from each entry's
        // $FILE_NAME directory bit, not the target record's flags.
        assert_eq!(
            native_list(&mut dev, "/"),
            upstream_list(&mut dev, "/"),
            "root listing mismatch"
        );
    }

    #[test]
    fn read_dir_on_a_file_errors() {
        let mut dev = fresh_vol();
        write::create_file_io(&mut dev, "/", "f").expect("create");
        let rec = resolve_path(&mut dev, "/f").unwrap();
        assert!(read_dir_entries(&mut dev, rec).is_err());
    }

    /// Build one synthetic $ATTRIBUTE_LIST entry (name at the standard 0x1A
    /// offset, 8-byte aligned length, sequence 1 in the base reference).
    fn al_entry(type_code: u32, name: Option<&str>, vcn: u64, rec: u64, id: u16) -> Vec<u8> {
        let name_u16: Vec<u16> = name.map(|n| n.encode_utf16().collect()).unwrap_or_default();
        let name_offset = 0x1Ausize;
        let raw = name_offset + name_u16.len() * 2;
        let length = raw.div_ceil(8) * 8;
        let mut e = vec![0u8; length];
        e[0..4].copy_from_slice(&type_code.to_le_bytes());
        e[4..6].copy_from_slice(&(length as u16).to_le_bytes());
        e[6] = name_u16.len() as u8;
        e[7] = name_offset as u8;
        e[8..16].copy_from_slice(&vcn.to_le_bytes());
        e[16..24].copy_from_slice(&(rec | (1u64 << 48)).to_le_bytes()); // base_file_reference
        e[24..26].copy_from_slice(&id.to_le_bytes());
        for (i, u) in name_u16.iter().enumerate() {
            e[name_offset + i * 2..name_offset + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        e
    }

    #[test]
    fn parse_attribute_list_decodes_entries() {
        let mut value = Vec::new();
        value.extend(al_entry(0x10, None, 0, 5, 0)); // $STANDARD_INFORMATION in base rec 5
        value.extend(al_entry(0x80, None, 0, 5, 1)); // unnamed $DATA in base rec 5
        value.extend(al_entry(0x80, Some("s001"), 0, 42, 7)); // named $DATA in ext rec 42

        let entries = parse_attribute_list(&value).expect("parse");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].type_code, 0x10);
        assert_eq!(entries[0].name, None);
        assert_eq!(entries[0].record_number, 5);
        assert_eq!(entries[2].type_code, 0x80);
        assert_eq!(entries[2].name.as_deref(), Some("s001"));
        assert_eq!(entries[2].record_number, 42, "seq bits masked off");
        assert_eq!(entries[2].attribute_id, 7);
        assert_eq!(entries[2].starting_vcn, 0);
    }

    #[test]
    fn parse_attribute_list_rejects_overrun_and_stops_on_zero() {
        // Zero-length entry terminates the walk.
        let mut v = al_entry(0x80, None, 0, 5, 0);
        v.extend_from_slice(&[0u8; 8]); // zero header → stop
        assert_eq!(parse_attribute_list(&v).unwrap().len(), 1);

        // A length that runs past the buffer errors.
        let mut bad = al_entry(0x80, None, 0, 5, 0);
        bad[4..6].copy_from_slice(&0x0FFFu16.to_le_bytes()); // absurd length
        assert!(parse_attribute_list(&bad).is_err());
    }

    // --- read_volume_info: cross-check vs format params + upstream oracle ----

    #[test]
    fn volume_info_matches_format_params() {
        let mut dev = fresh_vol();
        let vi = read_volume_info(&mut dev).expect("read_volume_info");
        // fresh_vol formats 32 MiB, 4 KiB clusters (512 B sectors), label
        // "NREAD", serial 0xABCD1234.
        assert_eq!(vi.cluster_size, 4096);
        assert_eq!(vi.bytes_per_sector, 512);
        assert_eq!(vi.serial_number, 0xABCD_1234);
        assert_eq!(vi.label, "NREAD");
        // total_size = total_sectors * bps, just under the 32 MiB device
        // (NTFS reserves the final sector); total_clusters derives from it.
        assert!(
            vi.total_size > 0 && vi.total_size <= 32 * 1024 * 1024,
            "total_size={}",
            vi.total_size
        );
        assert_eq!(vi.total_clusters, vi.total_size / 4096);
        // $VOLUME_INFORMATION version present (fresh mkfs stamps a real version).
        assert!(vi.version_major >= 1, "version_major={}", vi.version_major);
    }

    #[test]
    fn volume_info_matches_upstream_oracle() {
        let mut dev = fresh_vol();
        let vi = read_volume_info(&mut dev).expect("read_volume_info");
        let mut reader = IoReadSeek::new(&mut dev);
        let ntfs = Ntfs::new(&mut reader).expect("Ntfs::new");
        assert_eq!(vi.cluster_size as u64, ntfs.cluster_size() as u64);
        assert_eq!(vi.serial_number, ntfs.serial_number());
        assert_eq!(vi.total_size, ntfs.size());
        let ovi = ntfs.volume_info(&mut reader).expect("upstream volume_info");
        assert_eq!(vi.version_major, ovi.major_version());
        assert_eq!(vi.version_minor, ovi.minor_version());
    }

    #[test]
    fn volume_info_label_empty_when_no_volume_name() {
        // A volume with the $VOLUME_NAME omitted reads back an empty label
        // rather than erroring.
        let mut dev = MemDev {
            buf: vec![0u8; 32 * 1024 * 1024],
        };
        format_filesystem(
            &mut dev as &mut dyn BlockIo,
            32 * 1024 * 1024,
            4096,
            4096,
            None, // no label
            Some(0x1111_2222),
        )
        .expect("format");
        let vi = read_volume_info(&mut dev).expect("read_volume_info");
        assert_eq!(vi.label, "");
        assert_eq!(vi.serial_number, 0x1111_2222);
    }

    #[test]
    fn volume_info_rejects_non_ntfs_oem() {
        // A volume whose OEM ID isn't "NTFS    " (e.g. a FAT/exFAT/raw image)
        // is rejected up front — read_volume_info replaced Ntfs::new on the
        // mount path, which used to reject these at +0x03.
        let mut dev = fresh_vol();
        dev.buf[3..11].copy_from_slice(b"MSDOS5.0");
        let err = read_volume_info(&mut dev).expect_err("should reject non-NTFS OEM");
        assert!(err.contains("not an NTFS volume"), "err={err}");
    }
}
