//! manyfiles fixture: 512 files in /bigdir/ — forces the $INDEX_ALLOCATION
//! B+ tree path (past the point a single resident $INDEX_ROOT can hold).

mod common;

const IMG: &str = "test-disks/ntfs-manyfiles.img";

#[test]
fn lists_all_512_files() {
    let (ntfs, mut reader) = common::open(IMG);
    let names = common::list_names(&ntfs, &mut reader, "/bigdir");

    assert_eq!(
        names.len(),
        512,
        "expected 512 entries, got {}",
        names.len()
    );

    for i in 1..=512u32 {
        let want = format!("file_{i}.txt");
        assert!(names.iter().any(|n| n == &want), "missing {want}");
    }
}

#[test]
fn reads_arbitrary_file() {
    let (ntfs, mut reader) = common::open(IMG);
    // Pick one from deep in the range — covers the non-root nodes of the
    // index tree, not just early resident entries.
    let content = common::read_file_all(&ntfs, &mut reader, "/bigdir/file_400.txt");
    assert_eq!(content, b"content of file 400\n");
}

#[test]
fn root_control_file_intact() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/small.txt");
    assert_eq!(content, b"control\n");
}
