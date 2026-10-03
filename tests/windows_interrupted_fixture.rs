//! NTFS volumes Windows was writing to when they were captured, and what
//! Windows recovered from them: the replay oracle #137 needs (#366).
//!
//! HOW THEY WERE MADE. `test-disks/capture-interrupted-logfile.ps1`, run by
//! `.github/workflows/logfile-oracle.yml` on `windows-latest` (run
//! 36668256057, 2026-09-30), formatted a fresh fixed VHD NTFS (4 KiB
//! clusters, label ORACLE366) and ran a workload on it that created,
//! renamed and deleted files. While it ran, VSS shadow copies of the VHD's
//! host volume were taken: point-in-time images of every block the inner
//! volume had written, and nothing it still held in memory -- what a power
//! cut at that instant leaves. Nothing was synthesised; no byte here was
//! written by this crate.
//!
//! * `windows-interrupted-{1,6}.img.gz`: the NTFS partition of snapshots 1
//!   (after 244 workload operations) and 6 (after 1,982), cut from the
//!   shadow's VHD at byte 65536. Never attached again after capture.
//! * `windows-interrupted-{1,6}.recovered.img.gz`: a separate copy of each,
//!   which Windows attached -- running its `$LogFile` restart pass -- and
//!   detached again.
//!   Windows' read-only chkdsk found no problems on either afterwards, and
//!   `fsutil dirty query` called both not dirty.
//! * `windows-interrupted-{1,6}.recovered.manifest`: every workload file on
//!   the recovered copy, as Windows listed it: path, size, and Windows' own
//!   `Get-FileHash` SHA-256.
//!
//! * `windows-interrupted-wrap-{span,boundary,mft-grows}*`: the same three
//!   files for three later captures whose `$LogFile` wrapped between the
//!   last checkpoint and the snapshot. See [`WRAPPED_LOG_END`].
//!
//! * `windows-interrupted-index-vcn*`: the same three files for a capture
//!   whose log holds `SetIndexEntryVcnAllocation` redo. See
//!   [`INDEX_VCN_LOG_END`].
//!
//! Every partition's SHA-256 is in [`PARTITIONS`] and checked on unpacking.
//!
//! WHAT WINDOWS' REPLAY CHANGED, read by ntfs-3g without replaying: on
//! snapshot 1 ntfs-3g cannot mount the volume at all ("Failed to open
//! $Secure"), and all 206 files exist only after recovery; on snapshot 6,
//! 195 names ntfs-3g sees are gone after recovery and 393 appear. So these
//! logs hold redo work, and the manifests are what a replay must produce.
//!
//! What is checked: this crate knows the pre-images' logs hold work,
//! though neither has its dirty flag set (#376); it reads what Windows
//! recovered exactly as Windows does; and `fsck`, and every read-write
//! mount, replay each pre-image to the metadata Windows' own restart
//! reached, or refuse with nothing written a log they cannot replay in
//! full (#137).

mod common;

use fs_ntfs::facade::{FileType, Filesystem};
use fs_ntfs::fsck;
use fs_ntfs::{
    fs_ntfs_mount, fs_ntfs_mount_rw_with_fs_core_device, fs_ntfs_mount_with_callbacks,
    fs_ntfs_mount_with_fs_core_device, fs_ntfs_umount, FsNtfsBlockdevCfg,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_int, c_void};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// The SHA-256 of each decompressed partition, as captured.
const PARTITIONS: [(&str, &str); 12] = [
    (
        "windows-interrupted-1",
        "7236ffe64f5532b6f5cd976c1fa81c66e6be6cd31cfb8c22bc0bb588e8fad54f",
    ),
    (
        "windows-interrupted-1.recovered",
        "ba2efd60cf3de25f5e87e1adf11581826ccb3d3cfbb6c0fd5ac015bf8c308d2b",
    ),
    (
        "windows-interrupted-6",
        "5899daffd9e67bff8446c24647a4712092611cb00e20818ee771c20d63374238",
    ),
    (
        "windows-interrupted-6.recovered",
        "b6f8928649ec6239ac3e6fb40646c9bcd7128dca033a2da14d761a2f90fdc797",
    ),
    (
        "windows-interrupted-wrap-span",
        "589c38c13024a1404379c62441cb5f3d5fe6fb8f2432ab71ae8f682bcc98c600",
    ),
    (
        "windows-interrupted-wrap-span.recovered",
        "d9a60e5fe56a03951e9fb2b739155f73bd45767da5f3eb016028486bd1952d41",
    ),
    (
        "windows-interrupted-wrap-boundary",
        "c787d580c09d56dccd74a0de0ee05fbb2124cabfbe6dac56039b2eb870403480",
    ),
    (
        "windows-interrupted-wrap-boundary.recovered",
        "e5c6bf187a7241a9094562ec8d8ac5a514e72e8b1a80240e8c3d99e532336676",
    ),
    (
        "windows-interrupted-wrap-mft-grows",
        "141da94cd9d750f78a62396b1c776592aa7cd493d7ad3f6a0635643e14e183a2",
    ),
    (
        "windows-interrupted-wrap-mft-grows.recovered",
        "4159123653da5b98eb16774db10b01626c2106a8ba54e5511ac99b3de0047a59",
    ),
    (
        "windows-interrupted-index-vcn",
        "4aca41ada0fe8b14cb582dd3f5e5f8b99e77cf526168a0e74bb50c3989c99262",
    ),
    (
        "windows-interrupted-index-vcn.recovered",
        "16e5e62ae6719c45eef010a3d3d5823fec1a1ab2e7f252b32dd125263cd77063",
    ),
];

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn unpack(name: &str) -> String {
    let path = common::temp_image_path(name.replace('.', "_"));
    let out = File::create(&path).expect("create image copy");
    let status = Command::new("gzip")
        .args(["-dc", &format!("test-disks/{name}.img.gz")])
        .stdout(Stdio::from(out))
        .status()
        .expect("run gzip");
    assert!(status.success(), "decompress test-disks/{name}.img.gz");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        133_103_616,
        "{name} is the 133,103,616-byte partition"
    );
    let want = PARTITIONS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("{name} has no recorded SHA-256"))
        .1;
    assert_eq!(
        hex_sha256(&std::fs::read(&path).unwrap()),
        want,
        "{name} is not the partition that was captured"
    );
    path
}

/// Windows' manifest: path -> (size, SHA-256).
fn manifest(k: &str) -> BTreeMap<String, (u64, String)> {
    let path = format!("test-disks/windows-interrupted-{k}.recovered.manifest");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), 3, "{path}: {l:?}");
            (f[0].to_string(), (f[1].parse().unwrap(), f[2].to_string()))
        })
        .collect()
}

/// Every file under the workload's `dNNN` directories, as this crate reads
/// it: path -> (size read, SHA-256 of the bytes).
fn walk(img: &str) -> BTreeMap<String, (u64, String)> {
    let fs = Filesystem::mount(img).unwrap_or_else(|e| panic!("mount {img}: {e:?}"));
    let mut out = BTreeMap::new();
    for dir in fs.read_dir("/").expect("read /") {
        let workload = dir.name.len() == 4
            && dir.name.starts_with('d')
            && dir.name[1..].bytes().all(|b| b.is_ascii_digit());
        if !workload || dir.file_type != FileType::Directory {
            continue;
        }
        for entry in fs.read_dir(&format!("/{}", dir.name)).expect("read dir") {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let path = format!("{}/{}", dir.name, entry.name);
            let mut buf = vec![0u8; 64 * 1024];
            let n = fs
                .read_file(&format!("/{path}"), 0, &mut buf)
                .unwrap_or_else(|e| panic!("read /{path}: {e:?}"));
            let hash = hex_sha256(&buf[..n]);
            out.insert(path, (n as u64, hash));
        }
    }
    out
}

fn ntfs3g(args: &[&str]) -> (bool, String) {
    let out = Command::new(args[0])
        .args(&args[1..])
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "{} could not be run ({e}): install ntfs-3g (`apt-get install ntfs-3g`)",
                args[0]
            )
        });
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_logs_windows_left_mid_write_are_read_as_holding_work() {
    for k in ["1", "6"] {
        let img = unpack(&format!("windows-interrupted-{k}"));
        let mut io = fs_ntfs::block_io::PathIo::open_ro(std::path::Path::new(&img)).unwrap();
        let state = fsck::logfile_state_io(&mut io).expect("read $LogFile");
        assert!(state.needs_replay(), "snapshot {k}: {state:?}");

        // The independent reading: ntfs-3g refuses both read-write. On
        // snapshot 6 it reads the log and calls the volume unclean; on
        // snapshot 1 it gets no further than `$Secure`, which Windows had
        // logged but not yet written home.
        let (ok, err) = ntfs3g(&["ntfs-3g.probe", "--readwrite", &img]);
        let why = if k == "1" {
            "Failed to open $Secure"
        } else {
            "unclean"
        };
        assert!(
            !ok && err.contains(why),
            "snapshot {k}: ntfs-3g.probe succeeded={ok}, said {err}"
        );
    }
}

/// The last LSN each pre-image's `$LogFile` holds, read by an
/// independent reader written for #137 (not this crate's): the newest
/// `last_end_lsn` of any record page whose update sequence checks out,
/// tail copies included.
const LOG_END: [(&str, u64); 2] = [("1", 0x11_d767), ("6", 0x2b_48b3)];

/// Volumes whose `$LogFile` wrapped -- the writer passed its last page and
/// went on at the first page of its record area, one sequence number up --
/// between the last checkpoint and the capture, so a replay must follow it
/// across the end (#137). Made by `.github/workflows/logfile-oracle.yml`
/// with the same capture, and recovered by Windows the same way, as the
/// two snapshots above; each last LSN read by the same independent reader:
///
/// * `wrap-span` (run 37077649836, one writer for 240 s, snapshot 2): a
///   record starts on the log's last page and ends on the record area's
///   first.
/// * `wrap-boundary` (same run, snapshot 5): a record ends at the last
///   page, and the next starts the new lap.
/// * `wrap-mft-grows` (run 37077643885, eight writers for 60 s, snapshot
///   3): a record spans the end, and the log also grows `$MFT` and fills
///   the records it adds, so the records it names are where `$MFT` has
///   them only once the replay is written.
const WRAPPED_LOG_END: [(&str, u64); 3] = [
    ("wrap-span", 0x21_4840),
    ("wrap-boundary", 0x50_c32d),
    ("wrap-mft-grows", 0x52_13e3),
];

/// A volume whose `$LogFile` holds `SetIndexEntryVcnAllocation` redo
/// records -- an entry inside an index block pointed at another child
/// block -- which replay refused before it performed them (#137). Made by
/// the same capture, and recovered by Windows the same way, as the
/// snapshots above (run 37077649836, one writer for 240 s, snapshot 14:
/// refused at LSN `0xd26ed7`); its last LSN read by the same independent
/// reader.
const INDEX_VCN_LOG_END: [(&str, u64); 1] = [("index-vcn", 0xd3_21e9)];

/// Where, in each pre-image, a `$LogFile` record page the replay needs
/// sits: inside the walk from the oldest dirty page to the log's end, and
/// in no tail copy.
const NEEDED_LOG_PAGE: [(&str, u64); 2] = [("1", 0x8_0000), ("6", 0x10_0000)];

#[test]
fn fsck_refuses_a_log_it_cannot_read_to_the_end_and_writes_nothing() {
    // A torn record page in the middle of the work: the log cannot be
    // replayed in full, so nothing may be replayed at all. A partial redo
    // would leave metadata that matches no state the volume was ever in.
    for (k, page) in NEEDED_LOG_PAGE {
        let img = unpack(&format!("windows-interrupted-{k}"));
        let logfile_lcn = 10_318u64; // $LogFile's one run, in both images
        let at = logfile_lcn * 4096 + page + 510; // the first sector's USN copy
        let mut bytes = std::fs::read(&img).unwrap();
        bytes[at as usize] ^= 0xFF;
        std::fs::write(&img, &bytes).unwrap();

        let err = fsck::fsck(&img).expect_err("fsck must not act on a log it cannot read");
        assert!(err.contains("$LogFile"), "snapshot {k}: {err}");
        assert!(
            std::fs::read(&img).unwrap() == bytes,
            "snapshot {k}: a refused fsck wrote to the volume"
        );
    }
}

#[test]
fn fsck_replays_a_volume_windows_left_mid_write_to_what_windows_recovered() {
    // Neither volume's dirty flag is set: Windows 8 and later record an
    // unclean shutdown in the log alone. Windows' own restart pass turned
    // each pre-image into the `.recovered` image; fsck must reach the same
    // metadata from the same log (#137).
    for (k, log_end) in LOG_END {
        let img = unpack(&format!("windows-interrupted-{k}"));
        fsck::fsck(&img).unwrap_or_else(|e| panic!("snapshot {k}: fsck: {e}"));

        // What Windows lists after recovery, file for file.
        let want = manifest(k);
        let got = walk(&img);
        assert!(
            got == want,
            "snapshot {k}: {} files after replay, {} in Windows' manifest",
            got.len(),
            want.len()
        );

        // The log no longer holds work, and the volume opens for writing.
        let mut io = fs_ntfs::block_io::PathIo::open_ro(std::path::Path::new(&img)).unwrap();
        let state = fsck::logfile_state_io(&mut io).expect("read $LogFile");
        assert!(
            !state.needs_replay(),
            "snapshot {k} after replay: {state:?}"
        );
        drop(io);
        Filesystem::mount_rw(&img).unwrap_or_else(|e| panic!("snapshot {k}: mount_rw: {e}"));

        // A third reader: ntfs-3g refused both pre-images read-write (one
        // could not even open $Secure); it opens the replayed volumes.
        let (ok, err) = ntfs3g(&["ntfs-3g.probe", "--readwrite", &img]);
        assert!(
            ok,
            "snapshot {k}: ntfs-3g.probe --readwrite after replay: {err}"
        );

        let pre = Image::read(&unpack(&format!("windows-interrupted-{k}")));
        let win = Image::read(&unpack(&format!("windows-interrupted-{k}.recovered")));
        let ours = Image::read(&img);
        oracle::same_metadata_as_windows(k, &pre, &ours, &win, log_end);
    }
}

#[test]
fn a_log_that_wrapped_is_replayed_across_its_end_to_what_windows_recovered() {
    // The writer reached the log's last page and went on at the first page
    // of its record area between the checkpoint and the capture. Windows'
    // restart follows it there; so must fsck, and so must a read-write
    // mount, or the newest committed work is lost (#137).
    for (k, log_end) in WRAPPED_LOG_END {
        fsck_and_mount_rw_replay_to_what_windows_recovered(k, log_end);
    }
}

#[test]
fn an_index_entry_vcn_set_in_an_index_block_is_replayed_to_what_windows_recovered() {
    // SetIndexEntryVcnAllocation points an entry inside an index block at
    // another child block. Windows' restart redoes it; a replay that
    // refuses it sends the volume back to Windows, and one that skips it
    // leaves a directory whose index reaches the wrong block (#137).
    for (k, log_end) in INDEX_VCN_LOG_END {
        fsck_and_mount_rw_replay_to_what_windows_recovered(k, log_end);
    }
}

/// `fsck` and `Filesystem::mount_rw` must each bring the pre-image `k` to
/// the files, MFT records, index blocks and bitmaps Windows' own restart
/// produced from it.
fn fsck_and_mount_rw_replay_to_what_windows_recovered(k: &str, log_end: u64) {
    let pre_img = unpack(&format!("windows-interrupted-{k}"));
    let mut io = fs_ntfs::block_io::PathIo::open_ro(std::path::Path::new(&pre_img)).unwrap();
    let state = fsck::logfile_state_io(&mut io).expect("read $LogFile");
    assert!(state.needs_replay(), "{k}: {state:?}");
    drop(io);
    let pre = Image::read(&pre_img);
    let win = Image::read(&unpack(&format!("windows-interrupted-{k}.recovered")));
    let want = manifest(k);
    assert!(want.len() > 500, "{k}: the manifest lost its lines");

    let replay: [(&str, Mount); 2] = [
        ("fsck", |img| {
            fsck::fsck(img).map(|_| ()).map_err(|e| e.to_string())
        }),
        ("Filesystem::mount_rw", RW_MOUNTS[0].1),
    ];
    for (how, run) in replay {
        let img = unpack(&format!("windows-interrupted-{k}"));
        run(&img).unwrap_or_else(|e| panic!("{k}: {how}: {e}"));
        let got = walk(&img);
        let missing: Vec<_> = want
            .keys()
            .filter(|p| !got.contains_key(*p))
            .take(5)
            .collect();
        let differ: Vec<_> = want
            .iter()
            .filter(|(p, v)| got.get(*p).is_some_and(|g| g != *v))
            .take(5)
            .collect();
        assert!(
            got == want,
            "{k}: {how}: {} files after replay, {} in Windows' manifest; missing \
             {missing:?}, differing {differ:?}",
            got.len(),
            want.len()
        );
        let mut io = fs_ntfs::block_io::PathIo::open_ro(std::path::Path::new(&img)).unwrap();
        let state = fsck::logfile_state_io(&mut io).expect("read $LogFile");
        assert!(!state.needs_replay(), "{k}: {how} left {state:?}");
        drop(io);
        oracle::same_metadata_as_windows(k, &pre, &Image::read(&img), &win, log_end);
        std::fs::remove_file(&img).unwrap();
    }
}

/// Every way this crate opens a volume for writing, each over an image
/// path: `Ok` once it mounted (and unmounted again), or the reason it
/// refused.
type Mount = fn(&str) -> Result<(), String>;

const RW_MOUNTS: [(&str, Mount); 4] = [
    ("Filesystem::mount_rw", |img| {
        Filesystem::mount_rw(img).map(|_| ()).map_err(|e| e.0)
    }),
    ("fs_ntfs_mount", |img| {
        let c_path = CString::new(img).unwrap();
        let h = fs_ntfs_mount(c_path.as_ptr());
        if h.is_null() {
            return Err(last_error());
        }
        fs_ntfs_umount(h);
        Ok(())
    }),
    ("fs_ntfs_mount_with_callbacks", |img| {
        callback_mount(img, true)
    }),
    ("fs_ntfs_mount_rw_with_fs_core_device", |img| {
        let c_path = CString::new(img).unwrap();
        let dev = unsafe { fs_core::ffi::fs_core_file_open(c_path.as_ptr(), true) };
        assert!(!dev.is_null(), "open fs-core device on {img}");
        let h = fs_ntfs_mount_rw_with_fs_core_device(dev);
        let why = last_error();
        if !h.is_null() {
            fs_ntfs_umount(h);
        }
        unsafe { fs_core::ffi::fs_core_device_close(dev) };
        if h.is_null() {
            Err(why)
        } else {
            Ok(())
        }
    }),
];

struct FileCtx(Mutex<File>);

unsafe extern "C" fn read_cb(ctx: *mut c_void, buf: *mut c_void, at: u64, len: u64) -> c_int {
    let ctx = unsafe { &*(ctx as *const FileCtx) };
    let mut f = ctx.0.lock().unwrap();
    let buf = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
    c_int::from(f.seek(SeekFrom::Start(at)).is_err() || f.read_exact(buf).is_err())
}

unsafe extern "C" fn write_cb(ctx: *mut c_void, buf: *const c_void, at: u64, len: u64) -> c_int {
    let ctx = unsafe { &*(ctx as *const FileCtx) };
    let mut f = ctx.0.lock().unwrap();
    let buf = unsafe { std::slice::from_raw_parts(buf as *const u8, len as usize) };
    c_int::from(f.seek(SeekFrom::Start(at)).is_err() || f.write_all(buf).is_err())
}

/// Mount `img` through host callbacks, with a write callback or without.
fn callback_mount(img: &str, writable: bool) -> Result<(), String> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(img)
        .unwrap();
    let size = f.metadata().unwrap().len();
    let ctx = FileCtx(Mutex::new(f));
    let cfg = FsNtfsBlockdevCfg {
        read: read_cb,
        context: &ctx as *const FileCtx as *mut c_void,
        size_bytes: size,
        write: if writable { Some(write_cb) } else { None },
    };
    let h = fs_ntfs_mount_with_callbacks(&cfg);
    if h.is_null() {
        return Err(last_error());
    }
    fs_ntfs_umount(h);
    Ok(())
}

#[test]
fn every_read_write_mount_replays_a_volume_windows_left_mid_write_to_what_windows_recovered() {
    // Windows replays `$LogFile` when it mounts a volume, and so must a
    // read-write mount here (#137): refusing it sends the user to a
    // separate repair step for the most common state a disk arrives in,
    // and writing without it lands this crate's changes on metadata whose
    // committed updates are still in the log. Each entry point must reach
    // the metadata Windows' own restart reached from the same pre-image.
    for (k, log_end) in LOG_END {
        let pre_img = unpack(&format!("windows-interrupted-{k}"));
        let pre = Image::read(&pre_img);
        let win = Image::read(&unpack(&format!("windows-interrupted-{k}.recovered")));
        let want = manifest(k);
        for (name, rw_mount) in RW_MOUNTS {
            let img = unpack(&format!("windows-interrupted-{k}"));
            rw_mount(&img).unwrap_or_else(|e| panic!("snapshot {k}: {name}: {e}"));

            let got = walk(&img);
            assert!(
                got == want,
                "snapshot {k}: {name}: {} files after the mount, {} in Windows' manifest",
                got.len(),
                want.len()
            );
            let mut io = fs_ntfs::block_io::PathIo::open_ro(std::path::Path::new(&img)).unwrap();
            let state = fsck::logfile_state_io(&mut io).expect("read $LogFile");
            assert!(
                !state.needs_replay(),
                "snapshot {k}: {name} left the log holding work: {state:?}"
            );
            drop(io);
            oracle::same_metadata_as_windows(k, &pre, &Image::read(&img), &win, log_end);
            std::fs::remove_file(&img).unwrap();
        }
    }
}

#[test]
fn a_read_only_mount_of_a_volume_windows_left_mid_write_replays_nothing() {
    // A read-only mount reads the metadata as it is on disk, a little
    // behind the log, and writes nothing -- what every other driver here
    // does with a volume it is not allowed to change.
    for k in ["1", "6"] {
        let img = unpack(&format!("windows-interrupted-{k}"));
        let before = std::fs::read(&img).unwrap();

        Filesystem::mount(&img).expect("a read-only mount is allowed");
        callback_mount(&img, false)
            .unwrap_or_else(|e| panic!("snapshot {k}: read-only callback mount: {e}"));
        let c_path = CString::new(img.as_str()).unwrap();
        let dev = unsafe { fs_core::ffi::fs_core_file_open(c_path.as_ptr(), true) };
        assert!(!dev.is_null(), "snapshot {k}: open fs-core device");
        let ro = fs_ntfs_mount_with_fs_core_device(dev);
        assert!(!ro.is_null(), "snapshot {k}: read-only: {}", last_error());
        fs_ntfs_umount(ro);
        unsafe { fs_core::ffi::fs_core_device_close(dev) };

        assert!(
            std::fs::read(&img).unwrap() == before,
            "snapshot {k}: a read-only mount wrote to the volume"
        );
    }
}

#[test]
fn a_read_write_mount_refuses_a_log_it_cannot_replay_in_full_and_writes_nothing() {
    // A torn record page in the middle of the work: the replay cannot be
    // done whole, so none of it may be done, and the mount is refused as
    // it was before replay existed. The volume is left exactly as found.
    for (k, page) in NEEDED_LOG_PAGE {
        for (name, rw_mount) in RW_MOUNTS {
            let img = unpack(&format!("windows-interrupted-{k}"));
            let logfile_lcn = 10_318u64; // $LogFile's one run, in both images
            let at = logfile_lcn * 4096 + page + 510; // the first sector's USN copy
            let mut bytes = std::fs::read(&img).unwrap();
            bytes[at as usize] ^= 0xFF;
            std::fs::write(&img, &bytes).unwrap();

            let err = rw_mount(&img).expect_err("a mount over a log it cannot replay");
            assert!(
                err.contains("$LogFile") && err.contains("read-write mount refused"),
                "snapshot {k}: {name}: {err}"
            );
            assert!(
                std::fs::read(&img).unwrap() == bytes,
                "snapshot {k}: {name}: a refused mount wrote to the volume"
            );
            std::fs::remove_file(&img).unwrap();
        }
    }
}

#[test]
fn a_dirty_volume_is_refused_before_its_log_is_replayed() {
    // The dirty flag asks for a check of the whole volume, which a replay
    // is not. A read-write mount refuses it as before, and does not replay
    // first: nothing is written to a volume the mount then declines.
    for k in ["1", "6"] {
        for (name, rw_mount) in RW_MOUNTS {
            let img = unpack(&format!("windows-interrupted-{k}"));
            fsck::set_dirty(&img).expect("mark dirty");
            let before = std::fs::read(&img).unwrap();

            let err = rw_mount(&img).expect_err("a read-write mount of a dirty volume");
            assert!(err.contains("dirty"), "snapshot {k}: {name}: {err}");
            assert!(
                std::fs::read(&img).unwrap() == before,
                "snapshot {k}: {name}: a refused mount wrote to the volume"
            );
            std::fs::remove_file(&img).unwrap();
        }
    }
}

fn last_error() -> String {
    let p = fs_ntfs::fs_ntfs_last_error();
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn what_windows_recovered_reads_here_as_windows_lists_it() {
    let wrapped = WRAPPED_LOG_END.map(|(k, _)| k);
    let index_vcn = INDEX_VCN_LOG_END.map(|(k, _)| k);
    for k in ["1", "6"].into_iter().chain(wrapped).chain(index_vcn) {
        let img = unpack(&format!("windows-interrupted-{k}.recovered"));
        let want = manifest(k);
        assert!(
            want.len() > 200,
            "snapshot {k}: the manifest lost its lines"
        );
        let got = walk(&img);
        let missing: Vec<_> = want
            .keys()
            .filter(|p| !got.contains_key(*p))
            .take(5)
            .collect();
        let extra: Vec<_> = got
            .keys()
            .filter(|p| !want.contains_key(*p))
            .take(5)
            .collect();
        let differ: Vec<_> = want
            .iter()
            .filter(|(p, v)| got.get(*p).is_some_and(|g| g != *v))
            .take(5)
            .collect();
        assert!(
            missing.is_empty() && extra.is_empty() && differ.is_empty(),
            "snapshot {k}: {} files here, {} in Windows' manifest; missing {missing:?}, \
             extra {extra:?}, differing {differ:?}",
            got.len(),
            want.len()
        );
        // And the recovered copy's log is one this crate calls clean.
        let mut io = fs_ntfs::block_io::PathIo::open_ro(std::path::Path::new(&img)).unwrap();
        let state = fsck::logfile_state_io(&mut io).expect("read $LogFile");
        assert!(!state.needs_replay(), "snapshot {k} recovered: {state:?}");
    }
}

/// The oracle is only worth committing if the replay did something: on
/// the pre-images ntfs-3g, which does not replay, sees a different set of
/// files from Windows after recovery -- or cannot mount the volume at all.
#[test]
fn windows_replay_changed_what_the_volumes_hold() {
    let (mounted, err) = ntfs3g(&["ntfsls", "-f", &unpack("windows-interrupted-1")]);
    assert!(
        !mounted,
        "snapshot 1 should not mount without a replay; ntfs-3g said {err}"
    );

    let img = unpack("windows-interrupted-6");
    let want: std::collections::BTreeSet<String> = manifest("6").into_keys().collect();
    let mut seen = std::collections::BTreeSet::new();
    for dir in 0..64 {
        let out = Command::new("ntfsls")
            .args(["-f", "-p", &format!("/d{dir:03}"), &img])
            .output()
            .expect("run ntfsls");
        for name in String::from_utf8_lossy(&out.stdout).lines() {
            if name.ends_with(".bin") || name.ends_with(".renamed") {
                seen.insert(format!("d{dir:03}/{name}"));
            }
        }
    }
    assert!(!seen.is_empty(), "ntfs-3g listed nothing on snapshot 6");
    let gone = seen.difference(&want).count();
    let appeared = want.difference(&seen).count();
    assert!(
        gone > 0 && appeared > 0,
        "Windows' replay should both remove and add names on snapshot 6 \
         (removed {gone}, added {appeared})"
    );
}

/// How much each fixture's comparison must have to compare, so a
/// comparison that has lost its subject fails rather than passing on
/// nothing: MFT records Windows' replay changed, index blocks compared,
/// and `$Bitmap` bits the replay set as Windows did. Measured, 2026-10-03:
///
/// | fixture        | records | blocks | bits  |
/// |----------------|---------|--------|-------|
/// | 1              | 247     | 3      | 2,990 |
/// | 6              | 206     | 67     | 2,607 |
/// | wrap-span      | 210     | 67     | 954   |
/// | wrap-boundary  | 147     | 131    | 439   |
/// | wrap-mft-grows | 516     | 212    | 1,493 |
/// | index-vcn      | 134     | 418    | <= 99 |
///
/// `index-vcn`'s records and blocks were measured from the pre-image and
/// Windows' recovered copy alone; its bits are at most the 99 `$Bitmap`
/// differs in between them, before discounting clusters Windows allocated
/// after its restart.
fn floors(k: &str) -> (usize, usize, usize) {
    match k {
        "1" | "6" => (150, 3, 1000),
        "wrap-span" => (150, 50, 800),
        "wrap-boundary" => (100, 100, 350),
        "wrap-mft-grows" => (400, 150, 1200),
        "index-vcn" => (100, 300, 40),
        other => panic!("no floors measured for fixture {other}"),
    }
}

/// A raw reading of an NTFS image, written for this comparison from
/// MS-FSCC and nothing in this crate: the boot sector, `$MFT`'s runs, and
/// update-sequence fixups.
struct Image {
    bytes: Vec<u8>,
    cluster: u64,
    record: u64,
    mft_runs: Vec<(u64, u64)>,
}

fn le(b: &[u8], at: usize, n: usize) -> u64 {
    let mut v = 0u64;
    for i in (0..n).rev() {
        v = (v << 8) | u64::from(b[at + i]);
    }
    v
}

/// Mapping pairs of the attribute at `a` in `rec`: (lcn, clusters) runs.
fn runs_of(rec: &[u8], a: usize) -> Vec<(u64, u64)> {
    let mut at = a + le(rec, a + 0x20, 2) as usize;
    let end = a + le(rec, a + 4, 4) as usize;
    let mut lcn = 0i64;
    let mut out = Vec::new();
    while at < end && rec[at] != 0 {
        let (ln, on) = ((rec[at] & 15) as usize, (rec[at] >> 4) as usize);
        let len = le(rec, at + 1, ln);
        if on > 0 {
            let raw = le(rec, at + 1 + ln, on);
            let shift = 64 - 8 * on as u32;
            lcn += ((raw << shift) as i64) >> shift;
            out.push((lcn as u64, len));
        }
        at += 1 + ln + on;
    }
    out
}

/// Attributes of a record (fixups undone): (offset, type, non-resident).
fn attrs(rec: &[u8]) -> Vec<(usize, u32, bool)> {
    let mut at = le(rec, 0x14, 2) as usize;
    let mut out = Vec::new();
    while at + 8 <= rec.len() {
        let t = le(rec, at, 4) as u32;
        let len = le(rec, at + 4, 4) as usize;
        if t == 0xFFFF_FFFF || len == 0 || at + len > rec.len() {
            break;
        }
        out.push((at, t, rec[at + 8] != 0));
        at += len;
    }
    out
}

impl Image {
    fn read(path: &str) -> Image {
        let bytes = std::fs::read(path).unwrap();
        let cluster = le(&bytes, 0x0B, 2) * le(&bytes, 0x0D, 1);
        let c = bytes[0x40] as i8;
        let record = if c > 0 { c as u64 * cluster } else { 1 << -c };
        let mut img = Image {
            bytes,
            cluster,
            record,
            mft_runs: Vec::new(),
        };
        let mft_lcn = le(&img.bytes, 0x30, 8);
        let at = (mft_lcn * cluster) as usize;
        let rec0 = fixed(&img.bytes[at..at + record as usize], b"FILE").unwrap();
        let data = attrs(&rec0)
            .into_iter()
            .find(|&(_, t, nr)| t == 0x80 && nr)
            .unwrap();
        img.mft_runs = runs_of(&rec0, data.0);
        img
    }

    fn cluster_bytes(&self, lcn: u64, n: u64) -> &[u8] {
        &self.bytes[(lcn * self.cluster) as usize..((lcn + n) * self.cluster) as usize]
    }

    fn records(&self) -> u64 {
        self.mft_runs.iter().map(|r| r.1).sum::<u64>() * self.cluster / self.record
    }

    /// Record `n` as stored, before fixups.
    fn raw_record(&self, n: u64) -> &[u8] {
        self.raw_record_if_mapped(n)
            .unwrap_or_else(|| panic!("record {n} is past $MFT"))
    }

    /// Record `n` as stored, or `None` past this image's `$MFT`: a record
    /// the log adds after growing `$MFT` has no place before the replay.
    fn raw_record_if_mapped(&self, n: u64) -> Option<&[u8]> {
        let mut off = n * self.record;
        for &(lcn, len) in &self.mft_runs {
            if off < len * self.cluster {
                let at = (lcn * self.cluster + off) as usize;
                return Some(&self.bytes[at..at + self.record as usize]);
            }
            off -= len * self.cluster;
        }
        None
    }

    fn record(&self, n: u64) -> Option<Vec<u8>> {
        fixed(self.raw_record(n), b"FILE")
    }

    /// The unnamed stream of type `t` of record `n`, non-resident.
    fn stream(&self, n: u64, t: u32) -> Vec<u8> {
        let rec = self.record(n).unwrap();
        let (a, _, _) = attrs(&rec)
            .into_iter()
            .find(|&(_, ty, nr)| ty == t && nr)
            .unwrap();
        let size = le(&rec, a + 0x30, 8) as usize;
        let mut out = Vec::new();
        for (lcn, len) in runs_of(&rec, a) {
            out.extend_from_slice(self.cluster_bytes(lcn, len));
        }
        out.truncate(size);
        out
    }
}

/// Fixups undone, or `None` for a block that is not `magic` or is torn.
fn fixed(raw: &[u8], magic: &[u8; 4]) -> Option<Vec<u8>> {
    if &raw[..4] != magic {
        return None;
    }
    let mut b = raw.to_vec();
    let (uo, uc) = (le(&b, 4, 2) as usize, le(&b, 6, 2) as usize);
    for i in 1..uc {
        let end = i * 512 - 2;
        if b[end..end + 2] != b[uo..uo + 2] {
            return None;
        }
        b[end] = b[uo + 2 * i];
        b[end + 1] = b[uo + 2 * i + 1];
    }
    Some(b)
}

/// A FILE record or INDX block as content: fixups undone, the LSN and
/// the update sequence number (which every write moves) blanked, and only
/// the bytes in use.
fn content(raw: &[u8], magic: &[u8; 4]) -> Option<Vec<u8>> {
    let mut b = fixed(raw, magic)?;
    b[8..16].fill(0);
    let uo = le(&b, 4, 2) as usize;
    b[uo..uo + 2].fill(0);
    let used = if magic == b"FILE" {
        le(&b, 0x18, 4) as usize
    } else {
        0x18 + le(&b, 0x1C, 4) as usize
    };
    b.truncate(used.min(raw.len()));
    Some(b)
}

mod oracle {
    use super::*;
    use std::collections::BTreeSet;

    /// What Windows wrote AFTER its restart pass, which no replay of this
    /// log can produce: every record whose LSN is past the log's last
    /// record (new log records, written once the volume was mounted), and
    /// the transactional-NTFS metadata under `$Extend\$RmMetadata`, which
    /// Windows' resource manager rewrites at mount whether or not there
    /// was anything to replay.
    fn windows_after_recovery(win: &Image, log_end: u64) -> BTreeSet<u64> {
        let mut out = BTreeSet::new();
        let mut parent = std::collections::BTreeMap::new();
        for n in 0..win.records() {
            let Some(rec) = win.record(n) else { continue };
            if le(&rec, 8, 8) > log_end {
                out.insert(n);
            }
            for (a, t, nr) in attrs(&rec) {
                if t == 0x30 && !nr {
                    let v = a + le(&rec, a + 0x14, 2) as usize;
                    let name_len = rec[v + 0x40] as usize;
                    let name: Vec<u16> = (0..name_len)
                        .map(|i| le(&rec, v + 0x42 + 2 * i, 2) as u16)
                        .collect();
                    parent.insert(n, (le(&rec, v, 6), String::from_utf16_lossy(&name)));
                }
            }
        }
        let mut rm: BTreeSet<u64> = parent
            .iter()
            .filter(|(_, (_, name))| name == "$RmMetadata")
            .map(|(n, _)| *n)
            .collect();
        assert!(!rm.is_empty(), "no $RmMetadata in Windows' image");
        loop {
            let more: Vec<u64> = parent
                .iter()
                .filter(|(n, (p, _))| rm.contains(p) && !rm.contains(n))
                .map(|(n, _)| *n)
                .collect();
            if more.is_empty() {
                break;
            }
            rm.extend(more);
        }
        out.extend(rm);
        out
    }

    pub fn same_metadata_as_windows(k: &str, pre: &Image, ours: &Image, win: &Image, log_end: u64) {
        let (min_records, min_blocks, min_bits) = super::floors(k);
        let after = windows_after_recovery(win, log_end);

        // Every MFT record, as content.
        let mut replayed = 0;
        let mut differ = Vec::new();
        for n in 0..win.records() {
            if after.contains(&n) {
                continue;
            }
            let w = content(win.raw_record(n), b"FILE");
            if pre
                .raw_record_if_mapped(n)
                .and_then(|r| content(r, b"FILE"))
                != w
            {
                replayed += 1;
            }
            if content(ours.raw_record(n), b"FILE") != w {
                differ.push(n);
            }
        }
        assert!(
            differ.is_empty(),
            "snapshot {k}: {} MFT records differ from what Windows recovered: {:?}",
            differ.len(),
            &differ[..differ.len().min(20)]
        );
        assert!(
            replayed > min_records,
            "snapshot {k}: Windows' replay changed only {replayed} records; the comparison \
             has lost its subject"
        );

        // Every index block of every directory Windows did not touch after.
        let mut blocks = 0;
        for n in 0..win.records() {
            if after.contains(&n) {
                continue;
            }
            let Some(rec) = win.record(n) else { continue };
            for (a, t, nr) in attrs(&rec) {
                if t != 0xA0 || !nr {
                    continue;
                }
                for (lcn, len) in runs_of(&rec, a) {
                    for c in lcn..lcn + len {
                        let w = content(win.cluster_bytes(c, 1), b"INDX");
                        if w.is_none() {
                            continue;
                        }
                        blocks += 1;
                        assert!(
                            content(ours.cluster_bytes(c, 1), b"INDX") == w,
                            "snapshot {k}: record {n}'s index block at LCN {c} differs from \
                             what Windows recovered"
                        );
                    }
                }
            }
        }
        assert!(
            blocks >= min_blocks,
            "snapshot {k}: only {blocks} index blocks compared"
        );

        // $MFT's own bitmap, exactly.
        assert!(
            ours.stream(0, 0xB0) == win.stream(0, 0xB0),
            "snapshot {k}: $MFT's bitmap differs from what Windows recovered"
        );

        // $Bitmap: any cluster that differs is one Windows allocated, after
        // recovery, to a record it wrote after recovery.
        let (o, w, p) = (
            ours.stream(6, 0x80),
            win.stream(6, 0x80),
            pre.stream(6, 0x80),
        );
        let mut owned_after = BTreeSet::new();
        for &n in &after {
            let Some(rec) = win.record(n) else { continue };
            for (a, _, nr) in attrs(&rec) {
                if nr {
                    for (lcn, len) in runs_of(&rec, a) {
                        owned_after.extend(lcn..lcn + len);
                    }
                }
            }
        }
        let mut changed = 0;
        for c in 0..(w.len() as u64 * 8) {
            let bit = |b: &[u8]| (b[(c / 8) as usize] >> (c % 8)) & 1;
            if bit(&p) != bit(&w) && bit(&o) == bit(&w) {
                changed += 1;
            }
            if bit(&o) != bit(&w) {
                assert!(
                    owned_after.contains(&c),
                    "snapshot {k}: cluster {c} is {} in $Bitmap after replay, {} after Windows' \
                     recovery, and Windows did not allocate it afterwards",
                    bit(&o),
                    bit(&w)
                );
            }
        }
        assert!(
            changed > min_bits,
            "snapshot {k}: replay matched only {changed} $Bitmap bits Windows changed"
        );
    }
}
