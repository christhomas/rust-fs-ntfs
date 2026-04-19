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
use std::io::{BufReader, SeekFrom};
use std::path::{Path, PathBuf};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileNamespace, NtfsStandardInformation};
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType, NtfsReadSeek};

use crate::{fsck, write};

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
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub crtime: u32,
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

    pub fn volume_info(&self) -> Result<VolumeInfo, Error> {
        let (ntfs, _reader) = self.open_reader()?;
        // We don't resolve $Volume's name here (that path is in lib.rs
        // and a proper extraction is follow-up work). Provide everything
        // the boot sector gives us.
        Ok(VolumeInfo {
            volume_name: String::new(),
            cluster_size: ntfs.cluster_size(),
            total_clusters: ntfs.size() / ntfs.cluster_size() as u64,
            ntfs_version_major: 3,
            ntfs_version_minor: 1,
            serial_number: ntfs.serial_number(),
            total_size: ntfs.size(),
        })
    }

    pub fn stat(&self, path: &str) -> Result<Attr, Error> {
        let (ntfs, mut reader) = self.open_reader()?;
        let file = navigate(&ntfs, &mut reader, path)?;
        let mut attr = Attr {
            file_record_number: file.file_record_number(),
            size: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            crtime: 0,
            mode: if file.is_directory() { 0o40755 } else { 0o100644 },
            link_count: file.hard_link_count(),
            file_type: if file.is_directory() {
                FileType::Directory
            } else {
                FileType::Regular
            },
            attributes: 0,
        };
        let mut attributes = file.attributes();
        let mut reparse_tag: Option<u32> = None;
        while let Some(item) = attributes.next(&mut reader) {
            let Ok(item) = item else { continue };
            let Ok(a) = item.to_attribute() else { continue };
            match a.ty() {
                Ok(NtfsAttributeType::StandardInformation) => {
                    if let Ok(si) = a.resident_structured_value::<NtfsStandardInformation>() {
                        attr.crtime = ntfs_time_to_unix(si.creation_time());
                        attr.mtime = ntfs_time_to_unix(si.modification_time());
                        attr.atime = ntfs_time_to_unix(si.access_time());
                        attr.ctime = ntfs_time_to_unix(si.mft_record_modification_time());
                        attr.attributes = si.file_attributes().bits();
                    }
                }
                Ok(NtfsAttributeType::Data) => {
                    if a.name().map(|n| n.is_empty()).unwrap_or(true) {
                        attr.size = a.value_length();
                    }
                }
                Ok(NtfsAttributeType::ReparsePoint) => {
                    if let Ok(mut v) = a.value(&mut reader) {
                        let mut tag = [0u8; 4];
                        if v.read(&mut reader, &mut tag).is_ok() {
                            reparse_tag = Some(u32::from_le_bytes(tag));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(tag) = reparse_tag {
            attr.file_type = match tag {
                0xA000_000C => FileType::Symlink,
                0xA000_0003 => FileType::Junction,
                _ => attr.file_type,
            };
            if attr.file_type == FileType::Symlink {
                attr.mode = 0o120777;
            }
        }
        Ok(attr)
    }

    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, Error> {
        let (ntfs, mut reader) = self.open_reader()?;
        let dir = navigate(&ntfs, &mut reader, path)?;
        let current_rn = dir.file_record_number();
        let parent_rn = if current_rn == KnownNtfsFileRecordNumber::RootDirectory as u64 {
            current_rn
        } else {
            parent_record_of(&dir, &mut reader).unwrap_or(current_rn)
        };
        let mut out = Vec::new();
        out.push(DirEntry {
            file_record_number: current_rn,
            file_type: FileType::Directory,
            name: ".".into(),
        });
        out.push(DirEntry {
            file_record_number: parent_rn,
            file_type: FileType::Directory,
            name: "..".into(),
        });
        let index = dir
            .directory_index(&mut reader)
            .map_err(|e| Error(format!("directory_index: {e}")))?;
        let mut iter = index.entries();
        while let Some(entry) = iter.next(&mut reader) {
            let Ok(entry) = entry else { continue };
            let Some(Ok(file_name)) = entry.key() else {
                continue;
            };
            if file_name.namespace() == NtfsFileNamespace::Dos {
                continue;
            }
            out.push(DirEntry {
                file_record_number: entry.file_reference().file_record_number(),
                file_type: if file_name.is_directory() {
                    FileType::Directory
                } else {
                    FileType::Regular
                },
                name: file_name.name().to_string_lossy(),
            });
        }
        Ok(out)
    }

    /// Read `buf.len()` bytes from the unnamed `$DATA` stream starting at
    /// `offset`. Returns the number of bytes actually read (may be less
    /// than `buf.len()` if EOF is hit).
    pub fn read_file(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let (ntfs, mut reader) = self.open_reader()?;
        let file = navigate(&ntfs, &mut reader, path)?;
        let data_item = file
            .data(&mut reader, "")
            .ok_or_else(|| Error("no $DATA".into()))?
            .map_err(|e| Error(format!("$DATA: {e}")))?;
        let data_attr = data_item
            .to_attribute()
            .map_err(|e| Error(format!("to_attribute: {e}")))?;
        let mut value = data_attr
            .value(&mut reader)
            .map_err(|e| Error(format!("value: {e}")))?;
        value
            .seek(&mut reader, SeekFrom::Start(offset))
            .map_err(|e| Error(format!("seek: {e}")))?;
        let mut filled = 0;
        while filled < buf.len() {
            let n = value
                .read(&mut reader, &mut buf[filled..])
                .map_err(|e| Error(format!("read: {e}")))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(filled)
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

    pub fn write_ea(
        &self,
        path: &str,
        name: &[u8],
        value: &[u8],
        flags: u8,
    ) -> Result<(), Error> {
        write::write_ea(&self.image, path, name, value, flags).map_err(Error)
    }

    pub fn remove_ea(&self, path: &str, name: &[u8]) -> Result<(), Error> {
        write::remove_ea(&self.image, path, name).map_err(Error)
    }

    pub fn write_reparse_point(
        &self,
        path: &str,
        tag: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        write::write_reparse_point(&self.image, path, tag, data).map_err(Error)
    }

    pub fn remove_reparse_point(&self, path: &str) -> Result<(), Error> {
        write::remove_reparse_point(&self.image, path).map_err(Error)
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

fn ntfs_time_to_unix(t: ntfs::NtfsTime) -> u32 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let secs = t.nt_timestamp() / 10_000_000;
    secs.saturating_sub(EPOCH_DIFF) as u32
}

fn parent_record_of(
    file: &ntfs::NtfsFile,
    reader: &mut BufReader<File>,
) -> Result<u64, String> {
    let mut attrs = file.attributes();
    while let Some(item) = attrs.next(reader) {
        let item = item.map_err(|e| format!("attr iter: {e}"))?;
        let a = item.to_attribute().map_err(|e| format!("to_attr: {e}"))?;
        if a.ty().ok() != Some(NtfsAttributeType::FileName) {
            continue;
        }
        if let Ok(fname) =
            a.structured_value::<_, ntfs::structured_values::NtfsFileName>(reader)
        {
            return Ok(fname.parent_directory_reference().file_record_number());
        }
    }
    Err("no $FILE_NAME".into())
}

fn navigate<'n>(
    ntfs: &'n Ntfs,
    reader: &mut BufReader<File>,
    path: &str,
) -> Result<ntfs::NtfsFile<'n>, Error> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return ntfs
            .root_directory(reader)
            .map_err(|e| Error(format!("root: {e}")));
    }
    let mut current = ntfs
        .root_directory(reader)
        .map_err(|e| Error(format!("root: {e}")))?;
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            if current.file_record_number() == KnownNtfsFileRecordNumber::RootDirectory as u64 {
                continue;
            }
            let prn = parent_record_of(&current, reader).map_err(Error)?;
            current = ntfs
                .file(reader, prn)
                .map_err(|e| Error(format!("open parent: {e}")))?;
            continue;
        }
        let idx = current
            .directory_index(reader)
            .map_err(|e| Error(format!("index '{comp}': {e}")))?;
        let mut finder = idx.finder();
        let entry = NtfsFileNameIndex::find(&mut finder, ntfs, reader, comp)
            .ok_or_else(|| Error(format!("not found: '{comp}'")))?
            .map_err(|e| Error(format!("find '{comp}': {e}")))?;
        current = entry
            .to_file(ntfs, reader)
            .map_err(|e| Error(format!("to_file '{comp}': {e}")))?;
    }
    Ok(current)
}
