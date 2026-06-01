//! Rust-native ergonomic facade (§4.2).
//!
//! The C ABI in `lib.rs` is the primary entrypoint for FSKit / Swift
//! consumers — it deals in raw pointers, fixed-size buffers, and out-
//! parameters. That shape is awkward for direct Rust consumers.
//!
//! This module exposes idiomatic wrappers with `Result<_, Error>`
//! returns, `String` names, owned `Vec<u8>` payloads, and typed enums.
//! It's stateless: every operation opens the image fresh. The mount
//! call is validation-only, so `Filesystem::mount` doubles as "does
//! this look like a usable NTFS volume?".
//!
//! No performance-critical consumer should use this — it re-parses the
//! boot sector and MFT per call. FSKit and friends stay on the C ABI
//! via `fs_ntfs_mount` which keeps the parsed volume hot.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

// Upstream `ntfs` is still used by the not-yet-flipped volume_info/open_reader
// path; the per-path read methods (stat/read_dir/read_file) are native.
use ntfs::Ntfs;

use crate::attr_io::AttrType;
use crate::block_io::PathIo;
use crate::{fsck, read, write};

/// Generic facade error. Wraps the underlying string error from the
/// lower-level APIs.
#[derive(Debug, Clone)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Junction,
    Other,
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub file_record_number: u64,
    pub size: u64,
    pub atime_sec: i64,
    pub mtime_sec: i64,
    pub ctime_sec: i64,
    pub crtime_sec: i64,
    pub atime_nsec: u32,
    pub mtime_nsec: u32,
    pub ctime_nsec: u32,
    pub crtime_nsec: u32,
    pub mode: u16,
    pub link_count: u16,
    pub file_type: FileType,
    pub attributes: u32,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub file_record_number: u64,
    pub file_type: FileType,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub volume_name: String,
    pub cluster_size: u32,
    pub total_clusters: u64,
    pub ntfs_version_major: u16,
    pub ntfs_version_minor: u16,
    pub serial_number: u64,
    pub total_size: u64,
}

#[derive(Debug, Clone)]
pub struct VolumeStats {
    pub total_clusters: u64,
    pub free_clusters: u64,
    pub mft_total_records: u64,
    pub mft_free_records: u64,
    pub dirty: bool,
}

/// Stateless facade around a disk image. `mount` validates the image
/// once; all subsequent methods re-open it.
#[derive(Debug)]
pub struct Filesystem {
    image: PathBuf,
}

impl Filesystem {
    /// Open and validate the image. Returns an error if it isn't a
    /// parseable NTFS volume.
    pub fn mount(path: impl AsRef<Path>) -> Result<Self, Error> {
        let image = path.as_ref().to_path_buf();
        let f = File::open(&image).map_err(|e| Error(format!("open: {e}")))?;
        let mut reader = BufReader::new(f);
        Ntfs::new(&mut reader).map_err(|e| Error(format!("parse ntfs: {e}")))?;
        Ok(Self { image })
    }

    /// Open and validate the image, then apply `ntfs.sys`-style
    /// "upgrade on mount" (`$VOLUME_INFORMATION` 1.2 -> 3.1, clear
    /// `UPGRADE_ON_MOUNT`) best-effort. Use this when a mutation is
    /// imminent — CLI write commands, RW FFI flows, anything that
    /// will modify the volume. Read-only callers should use
    /// [`mount`](Self::mount).
    ///
    /// Upgrade failure is logged at `warn` but doesn't fail the
    /// mount; the volume is still usable in its pre-upgrade form.
    pub fn mount_rw(path: impl AsRef<Path>) -> Result<Self, Error> {
        let fs = Self::mount(path)?;
        match fs.upgrade_volume_version() {
            Ok(true) => log::info!(
                target: "fs_ntfs::facade",
                "upgraded $VOLUME_INFORMATION 1.2 -> 3.1 on {}",
                fs.image.display()
            ),
            Ok(false) => log::debug!(
                target: "fs_ntfs::facade",
                "no $VOLUME_INFORMATION upgrade needed on {}",
                fs.image.display()
            ),
            Err(e) => log::warn!(
                target: "fs_ntfs::facade",
                "$VOLUME_INFORMATION upgrade skipped on {}: {e}",
                fs.image.display()
            ),
        }
        Ok(fs)
    }

    /// Absolute path of the backing image.
    pub fn image_path(&self) -> &Path {
        &self.image
    }

    fn open_reader(&self) -> Result<(Ntfs, BufReader<File>), Error> {
        let f = File::open(&self.image).map_err(|e| Error(format!("open: {e}")))?;
        let mut reader = BufReader::new(f);
        let mut ntfs = Ntfs::new(&mut reader).map_err(|e| Error(format!("parse: {e}")))?;
        ntfs.read_upcase_table(&mut reader)
            .map_err(|e| Error(format!("upcase: {e}")))?;
        Ok((ntfs, reader))
    }

    /// Is the volume's DIRTY flag set in `$Volume`?
    pub fn is_dirty(&self) -> Result<bool, Error> {
        fsck::is_dirty(&self.image).map_err(Error)
    }

    pub fn clear_dirty(&self) -> Result<bool, Error> {
        fsck::clear_dirty(&self.image).map_err(Error)
    }

    /// Mimic `ntfs.sys`'s "upgrade on mount" transition: rewrite
    /// `$VOLUME_INFORMATION` from `major=1, minor=2` (the fresh-format
    /// state Microsoft `format.com` and our `mkfs` produce) to
    /// `major=3, minor=1` and clear the `UPGRADE_ON_MOUNT` flag.
    ///
    /// Returns `Ok(true)` if the upgrade fired, `Ok(false)` if the
    /// volume didn't need upgrading (already 3.1, different version,
    /// or flag clear). Idempotent; safe to call before every RW open.
    pub fn upgrade_volume_version(&self) -> Result<bool, Error> {
        fsck::upgrade_volume_version(&self.image).map_err(Error)
    }

    /// Rich stats: free clusters, MFT free records, dirty flag.
    /// Two full bitmap scans — not cheap.
    pub fn volume_stats(&self) -> Result<VolumeStats, Error> {
        let bm = crate::bitmap::locate_bitmap(&self.image).map_err(Error)?;
        let free_clusters = crate::bitmap::count_free(&self.image, &bm).map_err(Error)?;
        let mft_bm = crate::mft_bitmap::locate(&self.image).map_err(Error)?;
        let mft_total_records = match &mft_bm.layout {
            crate::mft_bitmap::MftBitmapLayout::Resident { total_bits, .. } => *total_bits,
            crate::mft_bitmap::MftBitmapLayout::NonResident { total_bits, .. } => *total_bits,
        };
        let mft_free_records =
            crate::mft_bitmap::count_free(&self.image, &mft_bm).map_err(Error)?;
        let dirty = fsck::is_dirty(&self.image).map_err(Error)?;
        Ok(VolumeStats {
            total_clusters: bm.total_bits,
            free_clusters,
            mft_total_records,
            mft_free_records,
            dirty,
        })
    }

    pub fn volume_info(&self) -> Result<VolumeInfo, Error> {
        let (ntfs, mut reader) = self.open_reader()?;
        // Read the real $VOLUME_INFORMATION bytes off disk. A
        // fresh-format volume reads back as 1.2 with UPGRADE_ON_MOUNT
        // set; ntfs.sys rewrites it to 3.1 on first RW mount.
        // Hardcoding 3.1 lied about that state.
        let vi = ntfs
            .volume_info(&mut reader)
            .map_err(|e| Error(format!("read $VOLUME_INFORMATION: {e}")))?;
        // We don't resolve $Volume's name here (that path is in lib.rs
        // and a proper extraction is follow-up work). Provide everything
        // the boot sector gives us.
        Ok(VolumeInfo {
            volume_name: String::new(),
            cluster_size: ntfs.cluster_size(),
            total_clusters: ntfs.size() / ntfs.cluster_size() as u64,
            ntfs_version_major: vi.major_version() as u16,
            ntfs_version_minor: vi.minor_version() as u16,
            serial_number: ntfs.serial_number(),
            total_size: ntfs.size(),
        })
    }

    pub fn stat(&self, path: &str) -> Result<Attr, Error> {
        let mut io = PathIo::open_ro(&self.image).map_err(Error)?;
        let rec = read::resolve_path(&mut io, path).map_err(Error)?;
        let st = read::read_stat(&mut io, rec).map_err(Error)?;
        let (crtime_sec, crtime_nsec) = nt_parts(st.created_nt);
        let (mtime_sec, mtime_nsec) = nt_parts(st.modified_nt);
        let (atime_sec, atime_nsec) = nt_parts(st.accessed_nt);
        let (ctime_sec, ctime_nsec) = nt_parts(st.mft_modified_nt);
        let mut attr = Attr {
            file_record_number: rec,
            size: st.size,
            atime_sec,
            mtime_sec,
            ctime_sec,
            crtime_sec,
            atime_nsec,
            mtime_nsec,
            ctime_nsec,
            crtime_nsec,
            mode: if st.is_dir { 0o40755 } else { 0o100644 },
            link_count: st.link_count,
            file_type: if st.is_dir {
                FileType::Directory
            } else {
                FileType::Regular
            },
            attributes: st.file_attributes,
        };
        // Reparse tag (symlink / junction), if the file has a $REPARSE_POINT.
        if let Ok(rp) = read::read_attribute_value(&mut io, rec, AttrType::ReparsePoint, None) {
            if rp.len() >= 4 {
                let tag = u32::from_le_bytes([rp[0], rp[1], rp[2], rp[3]]);
                attr.file_type = match tag {
                    0xA000_000C => FileType::Symlink,
                    0xA000_0003 => FileType::Junction,
                    _ => attr.file_type,
                };
                if attr.file_type == FileType::Symlink {
                    attr.mode = 0o120777;
                }
            }
        }
        Ok(attr)
    }

    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, Error> {
        let mut io = PathIo::open_ro(&self.image).map_err(Error)?;
        let current_rn = read::resolve_path(&mut io, path).map_err(Error)?;
        let parent_rn = if current_rn == read::ROOT_RECORD_NUMBER {
            current_rn
        } else {
            read::read_parent_record(&mut io, current_rn).unwrap_or(current_rn)
        };
        let mut out = vec![
            DirEntry {
                file_record_number: current_rn,
                file_type: FileType::Directory,
                name: ".".into(),
            },
            DirEntry {
                file_record_number: parent_rn,
                file_type: FileType::Directory,
                name: "..".into(),
            },
        ];
        // read_dir_entries already merges $INDEX_ROOT + INDX blocks, skips the
        // DOS 8.3 shadow names, and sets is_dir from each entry's $FILE_NAME bit.
        for e in read::read_dir_entries(&mut io, current_rn).map_err(Error)? {
            out.push(DirEntry {
                file_record_number: e.record_number,
                file_type: if e.is_dir {
                    FileType::Directory
                } else {
                    FileType::Regular
                },
                name: e.name,
            });
        }
        Ok(out)
    }

    /// Read `buf.len()` bytes from the unnamed `$DATA` stream starting at
    /// `offset`. Returns the number of bytes actually read (may be less
    /// than `buf.len()` if EOF is hit).
    pub fn read_file(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let mut io = PathIo::open_ro(&self.image).map_err(Error)?;
        let rec = read::resolve_path(&mut io, path).map_err(Error)?;
        // Ranged native read of the unnamed $DATA (resident / non-resident /
        // sparse / LZNT1) — reads only the clusters overlapping the window, so
        // a small read of a huge file doesn't materialise the whole file.
        let data =
            read::read_attribute_range(&mut io, rec, AttrType::Data, None, offset, buf.len())
                .map_err(Error)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    // ---------- mutations ----------

    pub fn create_file(&self, parent: &str, basename: &str) -> Result<u64, Error> {
        write::create_file(&self.image, parent, basename).map_err(Error)
    }

    pub fn mkdir(&self, parent: &str, basename: &str) -> Result<u64, Error> {
        write::mkdir(&self.image, parent, basename).map_err(Error)
    }

    pub fn unlink(&self, path: &str) -> Result<(), Error> {
        write::unlink(&self.image, path).map_err(Error)
    }

    pub fn rmdir(&self, path: &str) -> Result<(), Error> {
        write::rmdir(&self.image, path).map_err(Error)
    }

    /// POSIX-style remove: dispatches to `rmdir` for directories and
    /// `unlink` for regular files.
    pub fn remove(&self, path: &str) -> Result<(), Error> {
        write::remove(&self.image, path).map_err(Error)
    }

    pub fn rename(&self, old_path: &str, new_basename: &str) -> Result<(), Error> {
        write::rename(&self.image, old_path, new_basename).map_err(Error)
    }

    pub fn rename_same_length(&self, old_path: &str, new_name: &str) -> Result<(), Error> {
        write::rename_same_length(&self.image, old_path, new_name).map_err(Error)
    }

    pub fn truncate(&self, path: &str, new_size: u64) -> Result<u64, Error> {
        write::truncate(&self.image, path, new_size).map_err(Error)
    }

    pub fn grow(&self, path: &str, new_size: u64) -> Result<u64, Error> {
        write::grow_nonresident(&self.image, path, new_size).map_err(Error)
    }

    pub fn write_file_contents(&self, path: &str, data: &[u8]) -> Result<u64, Error> {
        write::write_file_contents(&self.image, path, data).map_err(Error)
    }

    pub fn write_resident_contents(&self, path: &str, data: &[u8]) -> Result<u64, Error> {
        write::write_resident_contents(&self.image, path, data).map_err(Error)
    }

    pub fn write_named_stream(
        &self,
        path: &str,
        stream_name: &str,
        data: &[u8],
    ) -> Result<(), Error> {
        write::write_named_stream(&self.image, path, stream_name, data).map_err(Error)
    }

    pub fn delete_named_stream(&self, path: &str, stream_name: &str) -> Result<(), Error> {
        write::delete_named_stream(&self.image, path, stream_name).map_err(Error)
    }

    pub fn write_ea(&self, path: &str, name: &[u8], value: &[u8], flags: u8) -> Result<(), Error> {
        write::write_ea(&self.image, path, name, value, flags).map_err(Error)
    }

    pub fn remove_ea(&self, path: &str, name: &[u8]) -> Result<(), Error> {
        write::remove_ea(&self.image, path, name).map_err(Error)
    }

    pub fn write_reparse_point(&self, path: &str, tag: u32, data: &[u8]) -> Result<(), Error> {
        write::write_reparse_point(&self.image, path, tag, data).map_err(Error)
    }

    pub fn remove_reparse_point(&self, path: &str) -> Result<(), Error> {
        write::remove_reparse_point(&self.image, path).map_err(Error)
    }

    /// Read the file's 16-byte `$OBJECT_ID` (GUID). Returns `Ok(None)`
    /// if the file has no object ID.
    pub fn object_id(&self, path: &str) -> Result<Option<[u8; 16]>, Error> {
        write::read_object_id(&self.image, path).map_err(Error)
    }

    pub fn link(
        &self,
        existing_path: &str,
        new_parent: &str,
        new_basename: &str,
    ) -> Result<(), Error> {
        write::link(&self.image, existing_path, new_parent, new_basename).map_err(Error)
    }

    pub fn create_symlink(
        &self,
        parent: &str,
        basename: &str,
        target: &str,
        relative: bool,
    ) -> Result<u64, Error> {
        write::create_symlink(&self.image, parent, basename, target, relative).map_err(Error)
    }
}

/// Split an NTFS FILETIME (100-ns since 1601) into (Unix seconds, nanoseconds).
fn nt_parts(nt: u64) -> (i64, u32) {
    (read::nt_to_unix(nt), ((nt % 10_000_000) * 100) as u32)
}
