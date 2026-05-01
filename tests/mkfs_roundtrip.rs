//! Round-trip test: format an in-memory volume with `format_filesystem`,
//! then parse it back with the existing read path (upstream `Ntfs::new`)
//! and confirm the basic structure (boot sector, $Volume label, root
//! directory) is intact.

use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::Mutex;

use fs_ntfs::block_io::BlockIo;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{fs_ntfs_mkfs, FsNtfsBlockdevCfg};

use ntfs::indexes::NtfsFileNameIndex;
use ntfs::structured_values::{NtfsFileNamespace, NtfsVolumeName};
use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType};

const VOL_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB

/// Vec-backed in-memory blockdev. The Rust path passes `&mut dyn BlockIo`
/// directly; the C ABI test plumbs through via callbacks.
struct MemDev {
    buf: Vec<u8>,
}

impl MemDev {
    fn new(size: u64) -> Self {
        Self {
            buf: vec![0u8; size as usize],
        }
    }
}

impl BlockIo for MemDev {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let off = offset as usize;
        if off + buf.len() > self.buf.len() {
            return Err(format!(
                "read past end: offset={off} len={} size={}",
                buf.len(),
                self.buf.len()
            ));
        }
        buf.copy_from_slice(&self.buf[off..off + buf.len()]);
        Ok(())
    }
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        let off = offset as usize;
        if off + buf.len() > self.buf.len() {
            return Err(format!(
                "write past end: offset={off} len={} size={}",
                buf.len(),
                self.buf.len()
            ));
        }
        self.buf[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.buf.len() as u64
    }
}

#[test]
fn format_and_parse_back() {
    let mut dev = MemDev::new(VOL_SIZE);
    format_filesystem(
        &mut dev,
        VOL_SIZE,
        4096,
        4096,
        Some("TESTVOL"),
        Some(0xDEADBEEF),
    )
    .expect("format_filesystem");

    // Parse back via upstream Ntfs.
    let mut cursor = std::io::Cursor::new(&dev.buf);
    let mut ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new on freshly formatted volume");
    ntfs.read_upcase_table(&mut cursor)
        .expect("read $UpCase from freshly formatted volume");

    assert_eq!(ntfs.cluster_size(), 4096);
    assert_eq!(ntfs.serial_number(), 0xDEADBEEF);

    // Volume info: version 3.1, flags clear.
    let vi = ntfs
        .volume_info(&mut cursor)
        .expect("read $VOLUME_INFORMATION");
    assert_eq!(vi.major_version(), 3);
    assert_eq!(vi.minor_version(), 1);

    // Volume name.
    let vol_name_opt = ntfs.volume_name(&mut cursor);
    let name: NtfsVolumeName = vol_name_opt
        .expect("$Volume has $VOLUME_NAME")
        .expect("read $VOLUME_NAME");
    assert_eq!(name.name().to_string_lossy(), "TESTVOL");

    // Root directory exists, is a directory, and is empty (no children
    // beyond the synthesized "." / ".." that mount layers add).
    let root = ntfs.root_directory(&mut cursor).expect("root directory");
    let index = root
        .directory_index(&mut cursor)
        .expect("root directory_index");
    let mut iter = index.entries();
    let mut names = Vec::new();
    while let Some(entry) = iter.next(&mut cursor) {
        let entry = entry.expect("entry");
        let key = match entry.key() {
            Some(Ok(k)) => k,
            _ => continue,
        };
        if key.namespace() == NtfsFileNamespace::Dos {
            continue;
        }
        names.push(key.name().to_string_lossy());
    }
    assert!(
        names.is_empty(),
        "expected empty root directory, got: {names:?}"
    );

    // $UpCase should be readable as the file at record 10 with a
    // 128 KiB unnamed $DATA.
    let upcase_file = ntfs
        .file(&mut cursor, KnownNtfsFileRecordNumber::UpCase as u64)
        .expect("open $UpCase record");
    let mut found_data = false;
    let mut attrs = upcase_file.attributes();
    while let Some(item) = attrs.next(&mut cursor) {
        let item = item.expect("attr item");
        let a = item.to_attribute().expect("to_attribute");
        if a.ty().ok() == Some(NtfsAttributeType::Data) {
            assert_eq!(a.value_length(), 128 * 1024);
            found_data = true;
        }
    }
    assert!(found_data, "$UpCase $DATA missing");

    // Looking up a nonexistent name in the root index should not panic
    // (just return None / Err).
    let mut finder = index.finder();
    let result = NtfsFileNameIndex::find(&mut finder, &ntfs, &mut cursor, "nonexistent");
    assert!(result.is_none(), "should not find a nonexistent name");
}

// --------------------------------------------------------------------------
// C ABI smoke: drive `fs_ntfs_mkfs` with callbacks against a Vec-backed
// context, then re-parse the resulting buffer.
// --------------------------------------------------------------------------

struct Ctx {
    buf: Mutex<Vec<u8>>,
}

unsafe extern "C" fn read_cb(
    ctx: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> c_int {
    let ctx = &*(ctx as *const Ctx);
    let v = ctx.buf.lock().expect("lock");
    let off = offset as usize;
    let len = length as usize;
    if off + len > v.len() {
        return 1;
    }
    let slice = std::slice::from_raw_parts_mut(buf as *mut u8, len);
    slice.copy_from_slice(&v[off..off + len]);
    0
}

unsafe extern "C" fn write_cb(
    ctx: *mut c_void,
    buf: *const c_void,
    offset: u64,
    length: u64,
) -> c_int {
    let ctx = &*(ctx as *const Ctx);
    let mut v = ctx.buf.lock().expect("lock");
    let off = offset as usize;
    let len = length as usize;
    if off + len > v.len() {
        return 1;
    }
    let slice = std::slice::from_raw_parts(buf as *const u8, len);
    v[off..off + len].copy_from_slice(slice);
    0
}

#[test]
fn capi_mkfs_then_parse() {
    let ctx = Ctx {
        buf: Mutex::new(vec![0u8; VOL_SIZE as usize]),
    };
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const Ctx as *mut c_void,
        size_bytes: VOL_SIZE,
        write: Some(write_cb),
    };
    let rc = fs_ntfs_mkfs(&cfg);
    assert_eq!(rc, 0, "fs_ntfs_mkfs failed");

    let buf = ctx.buf.lock().expect("lock").clone();
    let mut cursor = std::io::Cursor::new(&buf);
    let mut ntfs = Ntfs::new(&mut cursor).expect("Ntfs::new");
    ntfs.read_upcase_table(&mut cursor).expect("upcase");
    assert_eq!(ntfs.cluster_size(), 4096);
}
