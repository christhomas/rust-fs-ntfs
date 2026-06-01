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

/// Attribute data-flags (header +0x0C, low bits): the value is transformed
/// and can't be returned as raw bytes by this reader yet.
const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
const ATTR_FLAG_ENCRYPTED: u16 = 0x4000;

/// MFT record number of the root directory (`.`), fixed by the NTFS spec.
pub const ROOT_RECORD_NUMBER: u64 = 5;

/// Resolve an absolute path to its MFT record number, walking the directory
/// tree natively (no upstream `ntfs` crate). A leading `/` is optional;
/// `""`/`"/"` resolve to the root directory.
///
/// Each component is looked up in its parent directory's index: first the
/// resident `$INDEX_ROOT`, then — if the index has spilled — the allocated
/// `$INDEX_ALLOCATION` (INDX) blocks.
///
/// **Limitation — case-sensitive.** Lookups use the shared
/// `index_io::find_index_entry` / `find_entry_in_indx_block`, which match the
/// name with exact UTF-16 equality, so `/Foo` will not find an on-disk `foo`.
/// Real NTFS is case-insensitive by default (upcase-table collation). This is
/// the tracked C5 gap (`docs/missing-functionality.md`): the collation table
/// exists (`compare_names_ordinal`) but is not yet wired into the index
/// scanners. Wiring it is write-affecting (those scanners back create/rename
/// dedup too) and matrix-gated, so it's deferred to a dedicated change rather
/// than made case-insensitive only on the read side.
pub fn resolve_path<T: BlockIo + ?Sized>(io: &mut T, path: &str) -> Result<u64, String> {
    let mut record_number = ROOT_RECORD_NUMBER;

    for component in path.split('/') {
        if component.is_empty() {
            continue; // leading/trailing/duplicate slashes and the root itself
        }

        let (_, dir_bytes) = read_mft_record_io(io, record_number)?;
        if record_flags(&dir_bytes) & MFT_FLAG_DIRECTORY == 0 {
            return Err(format!(
                "resolve_path: '{component}' parent (record {record_number}) is not a directory"
            ));
        }

        record_number = lookup_in_directory(io, record_number, &dir_bytes, component)?
            .ok_or_else(|| format!("resolve_path: '{component}' not found"))?;
    }

    Ok(record_number)
}

/// Look up a single name in one directory. `dir_bytes` is the directory's
/// already-read (post-fixup) MFT record; `dir_record` is its number (needed
/// to load `$INDEX_ALLOCATION` if the index has spilled). Returns the target
/// record number, or `None` if the name is absent.
fn lookup_in_directory<T: BlockIo + ?Sized>(
    io: &mut T,
    dir_record: u64,
    dir_bytes: &[u8],
    name: &str,
) -> Result<Option<u64>, String> {
    // Resident $INDEX_ROOT first.
    if let Some(entry) = index_io::find_index_entry(dir_bytes, name)? {
        return Ok(Some(entry.file_record_number));
    }

    // Spilled into $INDEX_ALLOCATION? Scan the allocated INDX blocks.
    let ir_flags = index_io::index_root_flags(dir_bytes)
        .ok_or_else(|| format!("directory record {dir_record} has no $INDEX_ROOT"))?;
    if ir_flags & IH_FLAG_HAS_SUBNODES != 0 {
        let ia = idx_block::load_for_directory_io(io, dir_record)?;
        for vcn in ia.allocated_block_vcns() {
            let block = idx_block::read_indx_block_io(io, &ia, vcn)?;
            if let Some(entry) = index_io::find_entry_in_indx_block(&block, name)? {
                return Ok(Some(entry.file_record_number));
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
        Some((params, record, loc)) => read_value_from_record(io, &params, &record, &loc),
        None => Err(format!(
            "read_attribute_value: attribute {attr_type:?} (name {name:?}) not found in record {record_number}"
        )),
    }
}

/// Locate where an attribute physically lives: the MFT record bytes (base or,
/// following `$ATTRIBUTE_LIST`, an extension record) plus its `AttrLocation`.
/// Returns `None` if the attribute is absent. Shared by `read_attribute_value`
/// and `read_stat` so both handle overflowed (`$ATTRIBUTE_LIST`) files.
///
/// **Limitation (fails loud, never truncates):** a single non-resident
/// attribute whose run list is split across *multiple* extension records
/// (i.e. `$ATTRIBUTE_LIST` carries entries for the same type+name with
/// `starting_vcn > 0`) is not yet stitched — this returns `Err` rather than
/// silently reading only the VCN-0 segment.
#[allow(clippy::type_complexity)]
fn locate_attribute<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    attr_type: AttrType,
    name: Option<&str>,
) -> Result<Option<(crate::mft_io::BootParams, Vec<u8>, attr_io::AttrLocation)>, String> {
    let (params, record) = read_mft_record_io(io, record_number)?;

    // Common case: the attribute lives in the base record.
    if let Some(loc) = attr_io::find_attribute(&record, attr_type, name) {
        return Ok(Some((params, record, loc)));
    }

    // Overflowed: follow $ATTRIBUTE_LIST to the extension record that holds it.
    if let Some(al_loc) = attr_io::find_attribute(&record, AttrType::AttributeList, None) {
        let al_value = read_value_from_record(io, &params, &record, &al_loc)?;
        let entries = parse_attribute_list(&al_value)?;
        let mut matching = entries
            .iter()
            .filter(|e| e.type_code == attr_type as u32 && e.name.as_deref() == name);

        if let Some(entry) = matching.clone().find(|e| e.starting_vcn == 0) {
            // Refuse multi-extent attributes (run list split across records)
            // rather than returning a truncated value (principle: fail fast).
            if matching.any(|e| e.starting_vcn != 0) {
                return Err(format!(
                    "locate_attribute: {attr_type:?} (name {name:?}) in record {record_number} is \
                     split across multiple extension records ($ATTRIBUTE_LIST multi-extent stitching \
                     not yet supported)"
                ));
            }
            if entry.record_number != record_number {
                let (ext_params, ext) = read_mft_record_io(io, entry.record_number)?;
                let loc = attr_io::find_attribute(&ext, attr_type, name).ok_or_else(|| {
                    format!(
                        "locate_attribute: $ATTRIBUTE_LIST points {attr_type:?} (name {name:?}) \
                         at record {} but it's not there",
                        entry.record_number
                    )
                })?;
                return Ok(Some((ext_params, ext, loc)));
            }
        }
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

    let data_size = loc
        .non_resident_value_length
        .ok_or("non-resident attr has no data size")? as usize;
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
            let mut cluster = vec![0u8; cluster_size];
            io.read_exact_at(lcn * cluster_size as u64, &mut cluster)?;
            let copy_len = (file_off + cluster_size).min(readable) - file_off;
            out[file_off..file_off + copy_len].copy_from_slice(&cluster[..copy_len]);
        }
        // else: sparse hole → leave zeros.
    }

    Ok(out)
}

/// Non-resident attribute header: compression-unit exponent (u16 at +0x22).
/// The unit is `2^exp` clusters (4 ⇒ 16 clusters, the LZNT1 default).
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
    let data_size = loc
        .non_resident_value_length
        .ok_or("compressed attr has no data size")? as usize;
    let cu_exp = u16::from_le_bytes([
        record[loc.attr_offset + NONRES_COMPRESSION_UNIT],
        record[loc.attr_offset + NONRES_COMPRESSION_UNIT + 1],
    ]) as u32;
    if cu_exp == 0 {
        return Err("compressed flag set but compression_unit is 0".to_string());
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
                let mut cluster = vec![0u8; cluster_size];
                io.read_exact_at(lcn * cluster_size as u64, &mut cluster)?;
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
        Some((_, _, d)) if d.is_resident => d.resident_value_length.unwrap_or(0) as u64,
        Some((_, _, d)) => d.non_resident_value_length.unwrap_or(0),
        None => 0,
    };

    Ok(Stat {
        size,
        is_dir,
        file_attributes,
        created_nt,
        modified_nt,
        mft_modified_nt,
        accessed_nt,
    })
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
}
