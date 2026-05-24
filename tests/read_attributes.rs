//! Tests for `read_attributes` — the debug-oriented attribute-listing
//! helper added alongside the $Reparse byte-diff investigation.

use fs_ntfs::write::{create_file, read_attributes};

const BASIC_IMG: &str = "test-disks/ntfs-basic.img";

fn working_copy(tag: &str) -> String {
    let dst = format!("test-disks/_attrlist_{tag}.img");
    std::fs::copy(BASIC_IMG, &dst).expect("copy");
    dst
}

#[test]
fn enumerates_fixture_file_attrs() {
    // /hello.txt in ntfs-basic.img: $STANDARD_INFORMATION + $FILE_NAME +
    // $DATA at minimum (it's a regular non-empty file from the qemu
    // fixture pipeline; depending on the pipeline it may also carry
    // $OBJECT_ID).
    let attrs = read_attributes(std::path::Path::new(BASIC_IMG), "/hello.txt").unwrap();
    let types: Vec<u32> = attrs.iter().map(|a| a.type_code).collect();
    assert!(types.contains(&0x10), "$STANDARD_INFORMATION present");
    assert!(types.contains(&0x30), "$FILE_NAME present");
    assert!(types.contains(&0x80), "$DATA present");
}

#[test]
fn type_names_decoded() {
    let attrs = read_attributes(std::path::Path::new(BASIC_IMG), "/hello.txt").unwrap();
    for a in &attrs {
        // Every known attribute type should have a name starting with '$'.
        assert!(
            a.type_name.starts_with('$') || a.type_name.starts_with('?'),
            "type 0x{:x} got name {:?}",
            a.type_code,
            a.type_name
        );
    }
}

#[test]
fn unnamed_data_has_empty_name() {
    let attrs = read_attributes(std::path::Path::new(BASIC_IMG), "/hello.txt").unwrap();
    let data = attrs
        .iter()
        .find(|a| a.type_code == 0x80)
        .expect("$DATA present");
    assert_eq!(data.name, "", "default unnamed $DATA stream has empty name");
}

#[test]
fn runtime_file_includes_runtime_only_attrs() {
    // After create_file the new MFT record has at least
    // $STANDARD_INFORMATION + $FILE_NAME + $DATA. The runtime path
    // does NOT add $OBJECT_ID or $SECURITY_DESCRIPTOR, so they should
    // be absent.
    let img = working_copy("runtime_only");
    create_file(std::path::Path::new(&img), "/", "runtime.bin").unwrap();
    let attrs = read_attributes(std::path::Path::new(&img), "/runtime.bin").unwrap();
    let types: Vec<u32> = attrs.iter().map(|a| a.type_code).collect();
    assert!(types.contains(&0x10));
    assert!(types.contains(&0x30));
    assert!(types.contains(&0x80));
    assert!(!types.contains(&0x40), "no $OBJECT_ID on a fresh file");
}

#[test]
fn descriptions_carry_offsets_and_lengths() {
    let attrs = read_attributes(std::path::Path::new(BASIC_IMG), "/hello.txt").unwrap();
    // First attribute always starts at the record's attrs_offset
    // (varies by record size; for 4096-byte records it's 0x48). All
    // subsequent attr_offsets should be strictly increasing.
    let mut prev = 0;
    for a in &attrs {
        assert!(
            a.attr_offset > prev,
            "attr_offsets must be strictly increasing"
        );
        assert!(a.attr_length > 0, "attr_length must be > 0");
        prev = a.attr_offset;
    }
}

#[test]
fn missing_file_errors() {
    let err = read_attributes(std::path::Path::new(BASIC_IMG), "/no_such.txt").unwrap_err();
    assert!(!err.is_empty(), "expected non-empty error string");
}
