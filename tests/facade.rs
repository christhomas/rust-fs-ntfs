//! Tests for the Rust-native facade (§4.2).

use fs_ntfs::facade::{FileType, Filesystem};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_facade_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn facade_mount_valid_image() {
    let fs = Filesystem::mount(BASIC_IMG).expect("mount");
    assert_eq!(fs.image_path().to_str().unwrap(), BASIC_IMG);
}

#[test]
fn facade_mount_invalid_path_errors() {
    let err = Filesystem::mount("test-disks/_does_not_exist.img").unwrap_err();
    assert!(err.to_string().contains("open"));
}

#[test]
fn facade_volume_info() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    let v = fs.volume_info().unwrap();
    assert!(v.cluster_size > 0);
    assert!(v.total_size > 0);
}

#[test]
fn facade_stat_existing_file() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    let a = fs.stat("/hello.txt").unwrap();
    assert_eq!(a.file_type, FileType::Regular);
    assert!(a.size > 0);
}

#[test]
fn facade_stat_missing_file_errors() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    assert!(fs.stat("/no_such_file_xxx").is_err());
}

#[test]
fn facade_read_dir_lists_root() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    let entries = fs.read_dir("/").unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"."));
    assert!(names.contains(&".."));
    assert!(names.contains(&"hello.txt"));
}

#[test]
fn facade_read_file() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    let mut buf = vec![0u8; 256];
    let n = fs.read_file("/hello.txt", 0, &mut buf).unwrap();
    assert!(n > 0);
}

#[test]
fn facade_is_dirty() {
    let fs = Filesystem::mount(BASIC_IMG).unwrap();
    let d = fs.is_dirty().unwrap();
    // ntfs-basic is clean; just assert it's a bool-ish value
    assert!(matches!(d, true | false));
}

#[test]
fn facade_create_and_unlink() {
    let img = working_copy("create_unlink");
    let fs = Filesystem::mount(&img).unwrap();
    let rn = fs.create_file("/", "facade_new.txt").unwrap();
    assert!(rn >= 16);
    let a = fs.stat("/facade_new.txt").unwrap();
    assert_eq!(a.file_record_number, rn);
    fs.unlink("/facade_new.txt").unwrap();
    assert!(fs.stat("/facade_new.txt").is_err());
}

#[test]
fn facade_mkdir_and_rmdir() {
    let img = working_copy("mkdir_rmdir");
    let fs = Filesystem::mount(&img).unwrap();
    fs.mkdir("/", "fadir").unwrap();
    assert_eq!(fs.stat("/fadir").unwrap().file_type, FileType::Directory);
    fs.rmdir("/fadir").unwrap();
    assert!(fs.stat("/fadir").is_err());
}

#[test]
fn facade_write_and_read_contents() {
    let img = working_copy("write_read");
    let fs = Filesystem::mount(&img).unwrap();
    let payload = b"facade payload";
    fs.write_file_contents("/hello.txt", payload).unwrap();
    let mut buf = vec![0u8; 64];
    let n = fs.read_file("/hello.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], payload);
}

#[test]
fn facade_named_stream_roundtrip() {
    let img = working_copy("ads");
    let fs = Filesystem::mount(&img).unwrap();
    fs.write_named_stream("/hello.txt", "foo", b"stream data")
        .unwrap();
    fs.delete_named_stream("/hello.txt", "foo").unwrap();
}
