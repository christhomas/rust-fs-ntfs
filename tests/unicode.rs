//! unicode fixture: filenames covering three UTF-16 ranges — BMP Latin,
//! CJK, and astral (emoji via surrogate pair).

mod common;

const IMG: &str = "test-disks/ntfs-unicode.img";

#[test]
fn lists_unicode_names() {
    let (ntfs, mut reader) = common::open(IMG);
    let names = common::list_names(&ntfs, &mut reader, "/");
    println!("unicode root: {names:?}");

    assert!(
        names.iter().any(|n| n == "grüße.txt"),
        "missing umlaut name: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "日本語.txt"),
        "missing CJK name: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "hello-🌍.txt"),
        "missing emoji (surrogate pair) name: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "папка"),
        "missing cyrillic dir: {names:?}"
    );
}

#[test]
fn navigate_by_unicode_name() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/日本語.txt");
    assert_eq!(content, b"japanese\n");
}

#[test]
fn navigate_into_unicode_dir() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/папка/file.txt");
    assert_eq!(content, b"cyrillic dir\n");
}

#[test]
fn emoji_surrogate_pair_roundtrip() {
    let (ntfs, mut reader) = common::open(IMG);
    let content = common::read_file_all(&ntfs, &mut reader, "/hello-🌍.txt");
    assert_eq!(content, b"emoji\n");
}
