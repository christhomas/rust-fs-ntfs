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

use crate::block_io::BlockIo;
use crate::idx_block;
use crate::index_io::{self, IH_FLAG_HAS_SUBNODES};
use crate::mft_io::{read_mft_record_io, record_flags, MFT_FLAG_DIRECTORY};

/// MFT record number of the root directory (`.`), fixed by the NTFS spec.
pub const ROOT_RECORD_NUMBER: u64 = 5;

/// Resolve an absolute path to its MFT record number, walking the directory
/// tree natively (no upstream `ntfs` crate). A leading `/` is optional;
/// `""`/`"/"` resolve to the root directory.
///
/// Each component is looked up in its parent directory's index: first the
/// resident `$INDEX_ROOT`, then — if the index has spilled — the allocated
/// `$INDEX_ALLOCATION` (INDX) blocks. Lookup is collation-aware via the
/// shared index scanners.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_io::{BlockIo, IoReadSeek};
    use crate::mkfs::format_filesystem;
    use crate::write;
    use ntfs::indexes::NtfsFileNameIndex;
    use ntfs::Ntfs;

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
}
