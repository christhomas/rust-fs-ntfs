//! Sparse-file write planning.
//!
//! NTFS stores a sparse file by representing all-zero, cluster-aligned
//! regions as *holes* — runs in the mapping-pairs list with no LCN
//! (`DataRun.lcn == None`) and therefore no allocated clusters. A read of a
//! hole returns zeros. The file's `data_size` still counts the holes, but
//! `allocated_size` counts only the real (non-hole) clusters.
//!
//! This module is the pure, disk-free core: given a byte buffer, it decides
//! WHICH cluster-aligned regions become holes vs. allocated data, and
//! assembles the final [`DataRun`] list once the I/O layer has allocated
//! LCNs for the data regions. The byte-level encoding of holes is already
//! handled by [`crate::data_runs::encode_runs`] (it emits `lcn_bytes == 0`
//! for `lcn == None`).
//!
//! References (no GPL code consulted): NTFS sparse-file layout and the
//! `FILE_ATTRIBUTE_SPARSE_FILE` / attribute SPARSE flag per Windows
//! Internals 7th ed. ch. "NTFS On-Disk Structure" and MS-FSCC §2.4.

use crate::data_runs::DataRun;

/// Attribute-header data-flag (`+0x0C`, u16 LE) marking a non-resident
/// attribute as sparse. Per MS-FSCC: 0x0001 = compressed, 0x4000 = encrypted,
/// 0x8000 = sparse.
pub const ATTR_FLAG_SPARSE: u16 = 0x8000;

/// `$STANDARD_INFORMATION` / `$FILE_NAME` file-attribute bit marking the file
/// as sparse (MS-FSCC §2.6 `FILE_ATTRIBUTE_SPARSE_FILE`).
pub const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0000_0200;

/// One cluster-aligned segment of a file being written sparsely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SparseSegment {
    /// An all-zero region stored as a hole — no clusters allocated, reads
    /// back as zeros.
    Hole { start_vcn: u64, clusters: u64 },
    /// A non-zero region — clusters must be allocated and these bytes
    /// written. `byte_start..byte_start+byte_len` slices the source buffer
    /// (the final segment may be a partial cluster, hence a separate length).
    Data {
        start_vcn: u64,
        clusters: u64,
        byte_start: usize,
        byte_len: usize,
    },
}

/// True if every byte in `chunk` is zero (a cluster eligible to be a hole).
fn is_zero(chunk: &[u8]) -> bool {
    chunk.iter().all(|&b| b == 0)
}

/// Number of clusters needed to hold `len` bytes.
fn clusters_for(len: usize, cluster_size: u64) -> u64 {
    (len as u64).div_ceil(cluster_size)
}

/// Classify `data` into cluster-aligned hole / data segments. Consecutive
/// clusters of the same kind are coalesced into a single segment.
///
/// A trailing partial cluster is treated as a normal cluster: it becomes a
/// hole only if its (partial) bytes are all zero. Empty input yields no
/// segments.
pub fn plan_sparse_segments(data: &[u8], cluster_size: u64) -> Vec<SparseSegment> {
    assert!(cluster_size > 0, "cluster_size must be > 0");
    let total_clusters = clusters_for(data.len(), cluster_size);
    let cs = cluster_size as usize;

    let mut segments: Vec<SparseSegment> = Vec::new();
    let mut vcn = 0u64;
    while vcn < total_clusters {
        let byte_start = vcn as usize * cs;
        let byte_end = ((vcn as usize + 1) * cs).min(data.len());
        let hole = is_zero(&data[byte_start..byte_end]);

        // Extend the run while the next cluster is the same kind.
        let mut run_clusters = 1u64;
        while vcn + run_clusters < total_clusters {
            let nb_start = (vcn + run_clusters) as usize * cs;
            let nb_end = (((vcn + run_clusters) as usize + 1) * cs).min(data.len());
            if is_zero(&data[nb_start..nb_end]) != hole {
                break;
            }
            run_clusters += 1;
        }

        if hole {
            segments.push(SparseSegment::Hole {
                start_vcn: vcn,
                clusters: run_clusters,
            });
        } else {
            let run_byte_start = byte_start;
            let run_byte_end = (((vcn + run_clusters) as usize) * cs).min(data.len());
            segments.push(SparseSegment::Data {
                start_vcn: vcn,
                clusters: run_clusters,
                byte_start: run_byte_start,
                byte_len: run_byte_end - run_byte_start,
            });
        }
        vcn += run_clusters;
    }
    segments
}

/// Total clusters that must be allocated for these segments (sum of `Data`
/// segment cluster counts; holes cost nothing).
pub fn allocated_clusters(segments: &[SparseSegment]) -> u64 {
    segments
        .iter()
        .map(|s| match s {
            SparseSegment::Data { clusters, .. } => *clusters,
            SparseSegment::Hole { .. } => 0,
        })
        .sum()
}

/// Assemble the final VCN-contiguous [`DataRun`] list from planned segments,
/// pulling an LCN for each `Data` segment from `data_lcns` in order. Holes
/// become `lcn = None` runs.
///
/// `data_lcns` must have exactly one entry per `Data` segment (the starting
/// LCN of the contiguous run the I/O layer allocated for it).
pub fn build_runs(segments: &[SparseSegment], data_lcns: &[u64]) -> Result<Vec<DataRun>, String> {
    let mut runs = Vec::with_capacity(segments.len());
    let mut lcn_iter = data_lcns.iter();
    for seg in segments {
        match seg {
            SparseSegment::Hole {
                start_vcn,
                clusters,
            } => runs.push(DataRun {
                starting_vcn: *start_vcn,
                length: *clusters,
                lcn: None,
            }),
            SparseSegment::Data {
                start_vcn,
                clusters,
                ..
            } => {
                let lcn = *lcn_iter
                    .next()
                    .ok_or("build_runs: fewer LCNs than Data segments")?;
                runs.push(DataRun {
                    starting_vcn: *start_vcn,
                    length: *clusters,
                    lcn: Some(lcn),
                });
            }
        }
    }
    if lcn_iter.next().is_some() {
        return Err("build_runs: more LCNs than Data segments".to_string());
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS: u64 = 4096;

    #[test]
    fn empty_data_has_no_segments() {
        assert!(plan_sparse_segments(&[], CS).is_empty());
    }

    #[test]
    fn all_zero_single_cluster_is_one_hole() {
        let segs = plan_sparse_segments(&vec![0u8; CS as usize], CS);
        assert_eq!(segs, vec![SparseSegment::Hole { start_vcn: 0, clusters: 1 }]);
        assert_eq!(allocated_clusters(&segs), 0);
    }

    #[test]
    fn all_nonzero_single_cluster_is_one_data() {
        let segs = plan_sparse_segments(&vec![0xABu8; CS as usize], CS);
        assert_eq!(
            segs,
            vec![SparseSegment::Data {
                start_vcn: 0,
                clusters: 1,
                byte_start: 0,
                byte_len: CS as usize
            }]
        );
        assert_eq!(allocated_clusters(&segs), 1);
    }

    #[test]
    fn data_hole_data_pattern() {
        // cluster 0: data, cluster 1: hole, cluster 2: data
        let mut data = vec![0u8; 3 * CS as usize];
        data[0] = 1; // cluster 0 non-zero
        data[2 * CS as usize] = 1; // cluster 2 non-zero
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(
            segs,
            vec![
                SparseSegment::Data { start_vcn: 0, clusters: 1, byte_start: 0, byte_len: CS as usize },
                SparseSegment::Hole { start_vcn: 1, clusters: 1 },
                SparseSegment::Data {
                    start_vcn: 2,
                    clusters: 1,
                    byte_start: 2 * CS as usize,
                    byte_len: CS as usize
                },
            ]
        );
        assert_eq!(allocated_clusters(&segs), 2);
    }

    #[test]
    fn consecutive_holes_coalesce() {
        // 3 zero clusters then 1 data cluster.
        let mut data = vec![0u8; 4 * CS as usize];
        data[3 * CS as usize] = 0xFF;
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(
            segs,
            vec![
                SparseSegment::Hole { start_vcn: 0, clusters: 3 },
                SparseSegment::Data {
                    start_vcn: 3,
                    clusters: 1,
                    byte_start: 3 * CS as usize,
                    byte_len: CS as usize
                },
            ]
        );
    }

    #[test]
    fn trailing_partial_cluster_nonzero_is_data() {
        // 1 full zero cluster + a partial cluster with a non-zero byte.
        let mut data = vec![0u8; CS as usize + 10];
        data[CS as usize + 5] = 1;
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(
            segs,
            vec![
                SparseSegment::Hole { start_vcn: 0, clusters: 1 },
                SparseSegment::Data {
                    start_vcn: 1,
                    clusters: 1,
                    byte_start: CS as usize,
                    byte_len: 10
                },
            ]
        );
    }

    #[test]
    fn trailing_partial_cluster_zero_is_hole() {
        // 1 data cluster + a partial all-zero cluster.
        let mut data = vec![0u8; CS as usize + 10];
        data[0] = 1;
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(
            segs,
            vec![
                SparseSegment::Data { start_vcn: 0, clusters: 1, byte_start: 0, byte_len: CS as usize },
                SparseSegment::Hole { start_vcn: 1, clusters: 1 },
            ]
        );
    }

    #[test]
    fn build_runs_assigns_lcns_to_data_segments_only() {
        let mut data = vec![0u8; 3 * CS as usize];
        data[0] = 1;
        data[2 * CS as usize] = 1;
        let segs = plan_sparse_segments(&data, CS);
        // Two Data segments → two LCNs.
        let runs = build_runs(&segs, &[100, 200]).unwrap();
        assert_eq!(
            runs,
            vec![
                DataRun { starting_vcn: 0, length: 1, lcn: Some(100) },
                DataRun { starting_vcn: 1, length: 1, lcn: None },
                DataRun { starting_vcn: 2, length: 1, lcn: Some(200) },
            ]
        );
    }

    #[test]
    fn build_runs_roundtrips_through_encode_decode() {
        // The assembled runs must survive the real mapping-pairs codec.
        let mut data = vec![0u8; 5 * CS as usize];
        data[0] = 1; // cluster 0 data
        data[4 * CS as usize] = 1; // cluster 4 data; 1..4 hole
        let segs = plan_sparse_segments(&data, CS);
        let runs = build_runs(&segs, &[10, 20]).unwrap();
        let encoded = crate::data_runs::encode_runs(&runs).unwrap();
        let decoded = crate::data_runs::decode_runs(&encoded).unwrap();
        assert_eq!(decoded, runs, "sparse runs must round-trip the codec");
    }

    #[test]
    fn build_runs_rejects_lcn_count_mismatch() {
        let mut data = vec![0u8; CS as usize];
        data[0] = 1; // one Data segment
        let segs = plan_sparse_segments(&data, CS);
        assert!(build_runs(&segs, &[]).is_err(), "too few LCNs");
        assert!(build_runs(&segs, &[1, 2]).is_err(), "too many LCNs");
    }

    #[test]
    fn fully_sparse_large_file_is_single_hole() {
        // 256 all-zero clusters → one coalesced hole, zero allocation.
        let segs = plan_sparse_segments(&vec![0u8; 256 * CS as usize], CS);
        assert_eq!(segs, vec![SparseSegment::Hole { start_vcn: 0, clusters: 256 }]);
        assert_eq!(allocated_clusters(&segs), 0);
    }

    // --- additional edge cases -------------------------------------------

    #[test]
    fn single_nonzero_byte_makes_exactly_one_data_cluster() {
        // 16 clusters, a lone non-zero byte in cluster 7 → holes 0..7, data 7,
        // holes 8..16. Only one cluster allocated.
        let mut data = vec![0u8; 16 * CS as usize];
        data[7 * CS as usize + 3] = 0x01;
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(
            segs,
            vec![
                SparseSegment::Hole { start_vcn: 0, clusters: 7 },
                SparseSegment::Data {
                    start_vcn: 7,
                    clusters: 1,
                    byte_start: 7 * CS as usize,
                    byte_len: CS as usize
                },
                SparseSegment::Hole { start_vcn: 8, clusters: 8 },
            ]
        );
        assert_eq!(allocated_clusters(&segs), 1);
    }

    #[test]
    fn hole_at_start_and_end() {
        // hole, data, hole.
        let mut data = vec![0u8; 3 * CS as usize];
        data[CS as usize] = 9; // cluster 1 only
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(
            segs,
            vec![
                SparseSegment::Hole { start_vcn: 0, clusters: 1 },
                SparseSegment::Data {
                    start_vcn: 1,
                    clusters: 1,
                    byte_start: CS as usize,
                    byte_len: CS as usize
                },
                SparseSegment::Hole { start_vcn: 2, clusters: 1 },
            ]
        );
    }

    #[test]
    fn alternating_clusters_produce_a_segment_each() {
        // data,hole,data,hole over 4 clusters → 4 segments.
        let mut data = vec![0u8; 4 * CS as usize];
        data[0] = 1;
        data[2 * CS as usize] = 1;
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(segs.len(), 4);
        assert_eq!(allocated_clusters(&segs), 2);
    }

    #[test]
    fn smaller_cluster_size_512_classifies_per_512() {
        // With a 512-byte cluster, a non-zero byte only in the 2nd 512 block
        // makes cluster 0 a hole and cluster 1 data.
        let cs = 512u64;
        let mut data = vec![0u8; 2 * cs as usize];
        data[cs as usize] = 1;
        let segs = plan_sparse_segments(&data, cs);
        assert_eq!(
            segs,
            vec![
                SparseSegment::Hole { start_vcn: 0, clusters: 1 },
                SparseSegment::Data {
                    start_vcn: 1,
                    clusters: 1,
                    byte_start: cs as usize,
                    byte_len: cs as usize
                },
            ]
        );
    }

    #[test]
    fn sub_cluster_data_is_one_partial_data_segment() {
        // Fewer bytes than a cluster, non-zero → one Data segment, partial len.
        let data = vec![0xFFu8; 10];
        let segs = plan_sparse_segments(&data, CS);
        assert_eq!(
            segs,
            vec![SparseSegment::Data {
                start_vcn: 0,
                clusters: 1,
                byte_start: 0,
                byte_len: 10
            }]
        );
        assert_eq!(allocated_clusters(&segs), 1);
    }

    #[test]
    fn build_runs_all_holes_needs_no_lcns() {
        let segs = plan_sparse_segments(&vec![0u8; 4 * CS as usize], CS);
        let runs = build_runs(&segs, &[]).unwrap();
        assert_eq!(runs, vec![DataRun { starting_vcn: 0, length: 4, lcn: None }]);
    }

    #[test]
    fn build_runs_preserves_vcn_contiguity_for_encoder() {
        // The encoder requires VCN-contiguous runs starting at 0; assert the
        // assembled runs satisfy that for a hole-data-hole-data shape.
        let mut data = vec![0u8; 6 * CS as usize];
        data[2 * CS as usize] = 1; // cluster 2
        data[5 * CS as usize] = 1; // cluster 5
        let segs = plan_sparse_segments(&data, CS);
        let runs = build_runs(&segs, &[50, 60]).unwrap();
        // VCN coverage must be gap-free 0..6.
        let mut expected_vcn = 0u64;
        for r in &runs {
            assert_eq!(r.starting_vcn, expected_vcn);
            expected_vcn += r.length;
        }
        assert_eq!(expected_vcn, 6);
        // And it must actually encode (encoder rejects VCN gaps).
        assert!(crate::data_runs::encode_runs(&runs).is_ok());
    }

    #[test]
    #[should_panic(expected = "cluster_size must be > 0")]
    fn zero_cluster_size_panics() {
        let _ = plan_sparse_segments(&[1, 2, 3], 0);
    }
}
