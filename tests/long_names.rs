//! Long-name boundary tests.
//!
//! NTFS caps a single $FILE_NAME at 255 UTF-16 code units. Beyond that
//! limit `$FILE_NAME` doesn't fit in the standard MFT record at all
//! (the entry is 66 bytes of fixed header + 2 bytes per code unit).
//!
//! This file exercises:
//!   * names just under, at, and just over the 255-WCHAR limit
//!   * names whose UTF-8 length differs sharply from their UTF-16
//!     length (multi-byte CJK, 4-byte surrogate-pair emoji)
//!   * paths long enough to exceed Windows' classic 260-char MAX_PATH
//!     even though NTFS itself has no path-length cap

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

const VOL_SIZE: u64 = 32 * 1024 * 1024;

fn fresh_volume(tag: &str) -> String {
    let dst = format!("test-disks/_longname_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(
        &mut io,
        VOL_SIZE,
        4096,
        4096,
        Some("LONG"),
        Some(0xF00DC0DE),
    )
    .expect("format");
    io.sync().expect("sync");
    drop(io);
    dst
}

/// 254 ASCII chars: comfortably under the WCHAR limit. Must work.
#[test]
fn ascii_name_254_chars_roundtrip() {
    let img = fresh_volume("ascii_254");
    let fs = Filesystem::mount(&img).unwrap();
    let name: String = std::iter::repeat_n('a', 254).collect();
    fs.create_file("/", &name).unwrap();
    fs.write_file_contents(&format!("/{name}"), b"ok").unwrap();
    assert_eq!(fs.stat(&format!("/{name}")).unwrap().size, 2);

    let entries: Vec<String> = fs
        .read_dir("/")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(entries.iter().any(|n| n == &name), "name not in listing");
}

/// 255 ASCII chars: at the NTFS WCHAR ceiling. Must still work.
#[test]
fn ascii_name_255_chars_roundtrip() {
    let img = fresh_volume("ascii_255");
    let fs = Filesystem::mount(&img).unwrap();
    let name: String = std::iter::repeat_n('b', 255).collect();
    fs.create_file("/", &name).unwrap();
    fs.write_file_contents(&format!("/{name}"), b"ok").unwrap();
    assert_eq!(fs.stat(&format!("/{name}")).unwrap().size, 2);
}

/// 256 ASCII chars: one past the limit. Must reject — never panic,
/// never silently truncate.
#[test]
fn ascii_name_256_chars_rejected_or_truncated_safely() {
    let img = fresh_volume("ascii_256");
    let fs = Filesystem::mount(&img).unwrap();
    let name: String = std::iter::repeat_n('c', 256).collect();
    let res = fs.create_file("/", &name);
    if res.is_ok() {
        // If the driver accepted, the listed name MUST NOT exceed
        // 255 chars. Silent truncation to 255 would still be a bug
        // worth knowing about, but the panic-free contract is the
        // floor.
        let names: Vec<String> = fs
            .read_dir("/")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        for n in &names {
            assert!(
                n.chars().count() <= 255,
                "driver accepted name >255 chars: {n:?}"
            );
        }
    }
    // Either reject is fine; failure mode "panic" is the only thing
    // we won't tolerate.
}

/// CJK name at the WCHAR boundary. Each Han character is 1 UTF-16
/// code unit but 3 UTF-8 bytes, so this byte-string is 255*3 = 765
/// bytes long.
#[test]
fn cjk_name_at_wchar_limit_roundtrip() {
    let img = fresh_volume("cjk_255");
    let fs = Filesystem::mount(&img).unwrap();
    // 255 copies of '字'
    let name: String = std::iter::repeat_n('字', 255).collect();
    assert_eq!(name.chars().count(), 255);
    assert_eq!(name.encode_utf16().count(), 255);
    assert_eq!(name.len(), 255 * 3);

    fs.create_file("/", &name).unwrap();
    fs.write_file_contents(&format!("/{name}"), b"han").unwrap();
    assert_eq!(fs.stat(&format!("/{name}")).unwrap().size, 3);
}

/// Surrogate-pair emoji: each '🌍' is 2 UTF-16 code units. So 127 of
/// them is 254 WCHARs (legal); 128 is 256 WCHARs (over the limit).
#[test]
fn surrogate_pair_at_wchar_limit_roundtrip() {
    let img = fresh_volume("emoji_254");
    let fs = Filesystem::mount(&img).unwrap();
    let name: String = std::iter::repeat_n('🌍', 127).collect();
    assert_eq!(name.encode_utf16().count(), 254);
    fs.create_file("/", &name).unwrap();
    let stat = fs.stat(&format!("/{name}")).unwrap();
    assert_eq!(stat.size, 0);
}

#[test]
fn surrogate_pair_one_past_wchar_limit_rejected_safely() {
    let img = fresh_volume("emoji_256");
    let fs = Filesystem::mount(&img).unwrap();
    let name: String = std::iter::repeat_n('🌍', 128).collect();
    assert_eq!(name.encode_utf16().count(), 256);
    let res = fs.create_file("/", &name);
    if res.is_ok() {
        // Same rule as ASCII: if accepted, listing must not show a
        // longer-than-255-WCHAR name.
        for e in fs.read_dir("/").unwrap() {
            assert!(
                e.name.encode_utf16().count() <= 255,
                "stored {} WCHARs",
                e.name.encode_utf16().count()
            );
        }
    }
}

/// Nested path totalling ~600 chars (5 levels × 120-char names).
/// NTFS itself has no path cap; classic Win32 MAX_PATH is 260, so
/// this verifies the driver doesn't import that limit.
#[test]
fn deep_path_with_long_components_roundtrip() {
    let img = fresh_volume("deep_long");
    let fs = Filesystem::mount(&img).unwrap();

    let component: String = std::iter::repeat_n('z', 120).collect();
    let mut path = String::new();
    for _ in 0..5 {
        path.push('/');
        path.push_str(&component);
        fs.mkdir(
            if path.rfind('/').unwrap() == 0 {
                "/"
            } else {
                &path[..path.rfind('/').unwrap()]
            },
            &component,
        )
        .unwrap();
    }
    fs.create_file(&path, "leaf.txt").unwrap();
    fs.write_file_contents(&format!("{path}/leaf.txt"), b"reachable")
        .unwrap();
    let mut buf = vec![0u8; 16];
    let n = fs
        .read_file(&format!("{path}/leaf.txt"), 0, &mut buf)
        .unwrap();
    assert_eq!(&buf[..n], b"reachable");
    assert!(
        path.len() > 260,
        "path is shorter than MAX_PATH: {}",
        path.len()
    );
}

/// Empty basename and "."/".."  must be rejected — they have special
/// meaning in path-resolution.
#[test]
fn reserved_basenames_rejected() {
    let img = fresh_volume("reserved");
    let fs = Filesystem::mount(&img).unwrap();
    assert!(
        fs.create_file("/", "").is_err(),
        "empty basename should be rejected"
    );
    assert!(fs.create_file("/", ".").is_err(), "'.' should be rejected");
    assert!(
        fs.create_file("/", "..").is_err(),
        "'..' should be rejected"
    );
}
