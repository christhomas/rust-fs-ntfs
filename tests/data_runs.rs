//! Unit tests for the mapping-pair decoder.

use fs_ntfs::data_runs::{decode_runs, range_has_hole_or_past_end, vcn_to_lcn, DataRun};

#[test]
fn decode_empty_terminator() {
    assert!(decode_runs(&[0x00]).unwrap().is_empty());
}

#[test]
fn decode_single_run_contiguous() {
    // header = 0x21 ⇒ length 1 byte (=5), LCN 2 bytes (absolute 0x0040 = 64).
    // run: length=5 clusters, LCN=64.
    let bytes = [0x21, 0x05, 0x40, 0x00, 0x00];
    let runs = decode_runs(&bytes).unwrap();
    assert_eq!(
        runs,
        vec![DataRun {
            starting_vcn: 0,
            length: 5,
            lcn: Some(0x40)
        }]
    );
}

#[test]
fn decode_two_runs_with_positive_delta() {
    // Run 1: length=3 @ LCN 10.  Run 2: length=2 @ LCN 10+7=17.
    // Headers both 0x11 (1-byte length, 1-byte LCN).
    let bytes = [0x11, 0x03, 0x0A, 0x11, 0x02, 0x07, 0x00];
    let runs = decode_runs(&bytes).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].length, 3);
    assert_eq!(runs[0].lcn, Some(10));
    assert_eq!(runs[1].starting_vcn, 3);
    assert_eq!(runs[1].length, 2);
    assert_eq!(runs[1].lcn, Some(17));
}

#[test]
fn decode_run_with_negative_delta() {
    // Run 1: length=4 @ LCN 100. Run 2: length=2 @ delta -10 ⇒ LCN 90.
    // 0xFF...F6 is -10 in 1-byte two's complement = 0xF6.
    let bytes = [0x11, 0x04, 0x64, 0x11, 0x02, 0xF6, 0x00];
    let runs = decode_runs(&bytes).unwrap();
    assert_eq!(runs[1].lcn, Some(90));
}

#[test]
fn decode_sparse_run() {
    // header 0x01 ⇒ length 1 byte, LCN 0 bytes (hole).
    let bytes = [0x01, 0x05, 0x00];
    let runs = decode_runs(&bytes).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].length, 5);
    assert_eq!(runs[0].lcn, None);
}

#[test]
fn decode_mixed_sparse_and_dense() {
    // Run 1 dense: 2 clusters @ LCN 20
    // Run 2 sparse: 3 clusters (hole)
    // Run 3 dense: 1 cluster @ LCN 25 (delta from prev dense = +5)
    let bytes = [
        0x11, 0x02, 0x14, // run 1
        0x01, 0x03, // run 2 (hole)
        0x11, 0x01, 0x05, // run 3
        0x00,
    ];
    let runs = decode_runs(&bytes).unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].lcn, Some(20));
    assert_eq!(runs[1].lcn, None);
    assert_eq!(runs[1].length, 3);
    assert_eq!(runs[2].lcn, Some(25));
    assert_eq!(runs[2].starting_vcn, 5);
}

#[test]
fn vcn_to_lcn_walks_multiple_runs() {
    let runs = vec![
        DataRun {
            starting_vcn: 0,
            length: 3,
            lcn: Some(100),
        },
        DataRun {
            starting_vcn: 3,
            length: 2,
            lcn: Some(200),
        },
    ];
    assert_eq!(vcn_to_lcn(&runs, 0), Some(100));
    assert_eq!(vcn_to_lcn(&runs, 2), Some(102));
    assert_eq!(vcn_to_lcn(&runs, 3), Some(200));
    assert_eq!(vcn_to_lcn(&runs, 4), Some(201));
    assert_eq!(vcn_to_lcn(&runs, 5), None); // past end
}

#[test]
fn vcn_to_lcn_returns_none_for_hole() {
    let runs = vec![
        DataRun {
            starting_vcn: 0,
            length: 2,
            lcn: Some(50),
        },
        DataRun {
            starting_vcn: 2,
            length: 2,
            lcn: None, // hole
        },
    ];
    assert_eq!(vcn_to_lcn(&runs, 1), Some(51));
    assert_eq!(vcn_to_lcn(&runs, 2), None);
    assert_eq!(vcn_to_lcn(&runs, 3), None);
}

#[test]
fn range_has_hole_detects_sparse() {
    let runs = vec![
        DataRun {
            starting_vcn: 0,
            length: 2,
            lcn: Some(50),
        },
        DataRun {
            starting_vcn: 2,
            length: 2,
            lcn: None,
        },
        DataRun {
            starting_vcn: 4,
            length: 2,
            lcn: Some(60),
        },
    ];
    assert!(!range_has_hole_or_past_end(&runs, 0, 2));
    assert!(range_has_hole_or_past_end(&runs, 0, 3)); // spans into hole
    assert!(!range_has_hole_or_past_end(&runs, 4, 2)); // fully dense
    assert!(range_has_hole_or_past_end(&runs, 4, 3)); // past end
    assert!(range_has_hole_or_past_end(&runs, 2, 2)); // fully in hole
}

#[test]
fn decode_rejects_zero_length_bytes() {
    // header 0x10: 0-byte length, 1-byte LCN — invalid (length must be ≥1).
    let bytes = [0x10, 0x00, 0x00];
    assert!(decode_runs(&bytes).is_err());
}

#[test]
fn decode_rejects_truncated_run() {
    // header says 2-byte length + 2-byte LCN but we only have 1 more byte.
    let bytes = [0x22, 0xFF];
    assert!(decode_runs(&bytes).is_err());
}
