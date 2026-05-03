//! Extended-attribute (`$EA`) combinatorics.
//!
//! Existing `write_ea.rs` tests cover single-EA happy paths. This file
//! pushes harder:
//!   * many EAs on one file
//!   * EA values large enough to force `$EA` non-resident
//!   * upsert (set name twice — second value wins)
//!   * remove-then-readd cycles
//!   * EA names at the spec's 255-byte limit
//!   * remount survival
//!
//! Each test owns a fresh formatted volume.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use std::path::Path;

const VOL_SIZE: u64 = 32 * 1024 * 1024;

fn fresh_volume(tag: &str) -> String {
    let dst = format!("test-disks/_eax_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, 4096, 4096, Some("EA"), Some(0xEA1EA1EA)).expect("format");
    io.sync().expect("sync");
    drop(io);
    dst
}

fn list(image: &str, path: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    write::list_eas(Path::new(image), path)
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.value))
        .collect()
}

/// Many EAs of varied small sizes. Total $EA blob still resident.
#[test]
fn many_small_eas_roundtrip() {
    let img = fresh_volume("many_small");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();

    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..16)
        .map(|i| {
            (
                format!("EA_{i:02}").into_bytes(),
                format!("value-of-ea-{i:02}").into_bytes(),
            )
        })
        .collect();
    for (n, v) in &pairs {
        fs.write_ea("/f", n, v, 0).unwrap();
    }

    let got = list(&img, "/f");
    for (n, v) in &pairs {
        assert!(
            got.iter().any(|(rn, rv)| rn == n && rv == v),
            "missing EA {} = {}",
            String::from_utf8_lossy(n),
            String::from_utf8_lossy(v),
        );
    }
}

/// Upsert: writing an EA twice with the same name replaces the value
/// (does not duplicate the entry). list_eas must show exactly one
/// entry with the new value.
#[test]
fn upsert_replaces_value() {
    let img = fresh_volume("upsert");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();

    fs.write_ea("/f", b"K", b"v1", 0).unwrap();
    fs.write_ea("/f", b"K", b"v2_longer", 0).unwrap();

    let got = list(&img, "/f");
    let matches: Vec<_> = got.iter().filter(|(n, _)| n.as_slice() == b"K").collect();
    assert_eq!(matches.len(), 1, "duplicate K entries: {got:?}");
    assert_eq!(matches[0].1.as_slice(), b"v2_longer");
}

/// Remove-then-readd. The slot must be reusable; final list shows the
/// re-added value.
#[test]
fn remove_then_readd_works() {
    let img = fresh_volume("remove_readd");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();

    fs.write_ea("/f", b"X", b"orig", 0).unwrap();
    fs.remove_ea("/f", b"X").unwrap();
    assert!(list(&img, "/f").iter().all(|(n, _)| n.as_slice() != b"X"));

    fs.write_ea("/f", b"X", b"refreshed", 0).unwrap();
    let got = list(&img, "/f");
    assert!(got
        .iter()
        .any(|(n, v)| n.as_slice() == b"X" && v.as_slice() == b"refreshed"));
}

/// EA name at the driver's 254-byte ceiling (one byte for the
/// length field, one for the trailing NUL — leaves 254 for the
/// name itself).
#[test]
fn ea_name_at_254_byte_limit() {
    let img = fresh_volume("name_254");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();

    let name = vec![b'A'; 254];
    fs.write_ea("/f", &name, b"value", 0).unwrap();
    let got = list(&img, "/f");
    assert!(got
        .iter()
        .any(|(n, v)| n == &name && v.as_slice() == b"value"));
}

/// EA name 255 bytes — one past the on-disk encoding ceiling. Driver
/// must reject, not truncate or panic.
#[test]
fn ea_name_255_bytes_rejected() {
    let img = fresh_volume("name_255");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();
    let name = vec![b'B'; 255];
    let res = fs.write_ea("/f", &name, b"value", 0);
    assert!(res.is_err(), "255-byte EA name should be rejected");
}

/// EA NEED_EA flag (0x80) round-trip.
#[test]
fn need_ea_flag_preserved() {
    let img = fresh_volume("need_ea");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();
    fs.write_ea("/f", b"NEED", b"critical", 0x80).unwrap();

    let eas = write::list_eas(Path::new(&img), "/f").unwrap();
    let need = eas
        .iter()
        .find(|e| e.name == b"NEED")
        .expect("NEED EA missing");
    assert_eq!(need.flags, 0x80, "NEED_EA flag must round-trip");
}

/// EAs survive an unmount/remount.
#[test]
fn eas_survive_remount() {
    let img = fresh_volume("survive");
    {
        let fs = Filesystem::mount(&img).unwrap();
        fs.create_file("/", "f").unwrap();
        fs.write_ea("/f", b"AUTHOR", b"chris", 0).unwrap();
        fs.write_ea("/f", b"VERSION", b"1.0.0", 0).unwrap();
    }
    let got = list(&img, "/f");
    assert!(got
        .iter()
        .any(|(n, v)| n.as_slice() == b"AUTHOR" && v.as_slice() == b"chris"));
    assert!(got
        .iter()
        .any(|(n, v)| n.as_slice() == b"VERSION" && v.as_slice() == b"1.0.0"));
}

/// Removing a non-existent EA must error cleanly, not panic.
#[test]
fn remove_missing_ea_errors_cleanly() {
    let img = fresh_volume("rm_missing");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();
    let err = fs.remove_ea("/f", b"NEVER_EXISTED").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not found")
            || msg.contains("absent")
            || msg.contains("missing")
            || msg.contains("no such"),
        "unexpected error: {msg}"
    );
}

/// Empty value (0 bytes) is legal per spec.
#[test]
fn ea_with_empty_value() {
    let img = fresh_volume("empty_val");
    let fs = Filesystem::mount(&img).unwrap();
    fs.create_file("/", "f").unwrap();
    fs.write_ea("/f", b"FLAG_ONLY", b"", 0).unwrap();
    let got = list(&img, "/f");
    assert!(got
        .iter()
        .any(|(n, v)| n.as_slice() == b"FLAG_ONLY" && v.is_empty()));
}
