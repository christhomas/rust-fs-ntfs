//! deep fixture: 20-level nested path — exercises repeated directory-index
//! walks from root down to a leaf file.

mod common;

const IMG: &str = "test-disks/ntfs-deep.img";

#[test]
fn read_deeply_buried_file() {
    let (ntfs, mut reader) = common::open(IMG);

    let mut path = String::new();
    for i in 1..=20 {
        path.push_str(&format!("/level{i}"));
    }
    path.push_str("/buried.txt");

    let content = common::read_file_all(&ntfs, &mut reader, &path);
    assert_eq!(content, b"deep file content\n");
}

#[test]
fn surface_file_still_reads() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/surface.txt");
    assert_eq!(content, b"surface\n");
}
