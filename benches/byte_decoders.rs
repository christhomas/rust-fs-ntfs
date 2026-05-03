//! Criterion benches for the three byte-decoders that fuzz already
//! covers (data_runs, ea_io, attr_io). Pairs with the fuzz harness
//! in `fuzz/`: fuzzing finds correctness bugs, criterion finds
//! perf regressions when one of these gets refactored.
//!
//! Run with `cargo bench --bench byte_decoders`. Per-target HTML
//! reports land at `target/criterion/<group>/<id>/report/index.html`.
//!
//! All inputs are constructed in-memory — no real NTFS image reads,
//! no I/O. Each bench targets the realistic shape its function
//! handles in production: a populated MFT record for `iter_attributes`,
//! a single multi-run mapping-pair list for `decode_runs`, and a
//! short EA stream for `ea_io::decode`.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};

use fs_ntfs::attr_io;
use fs_ntfs::data_runs;
use fs_ntfs::ea_io;

fn bench_decode_runs(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_runs");

    // Single-run mapping-pair list — the common case for short
    // resident extensions and small contiguous attributes.
    let single = vec![0x21, 0x18, 0x34, 0x00];
    group.bench_function("single_run", |b| {
        b.iter(|| data_runs::decode_runs(black_box(&single)).unwrap())
    });

    // Eight-run zigzag — deltas alternate sign so the LCN delta
    // re-base loop runs on every step. Stresses the inner state
    // machine more than a single big run does.
    let mut zigzag = Vec::new();
    let mut sign = 1i64;
    let mut lcn: i64 = 0x100;
    for _ in 0..8 {
        zigzag.push(0x21);
        zigzag.push(0x10);
        let delta = sign * 0x40;
        let bytes = (delta as i16).to_le_bytes();
        zigzag.extend_from_slice(&bytes);
        lcn += delta;
        let _ = lcn;
        sign = -sign;
    }
    zigzag.push(0x00);
    group.bench_function("eight_run_zigzag", |b| {
        b.iter(|| data_runs::decode_runs(black_box(&zigzag)).unwrap())
    });

    // Sparse-then-data: a hole followed by 4 KiB of contiguous data.
    // Exercises the lcn_bytes=0 branch.
    let sparse = vec![
        0x01, 0x10, // length=0x10 clusters, no LCN (sparse)
        0x21, 0x10, 0x00, 0x10, // length=0x10 at LCN 0x1000
        0x00,
    ];
    group.bench_function("sparse_then_data", |b| {
        b.iter(|| data_runs::decode_runs(black_box(&sparse)).unwrap())
    });

    group.finish();
}

fn bench_decode_eas(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_eas");

    // Single small EA — Windows shell-extension shape (e.g. a
    // 4-byte name + 8-byte value).
    let mut single = Vec::new();
    push_ea(&mut single, b"AUTHOR", b"chris");
    group.bench_function("single_small", |b| {
        b.iter_batched(
            || single.clone(),
            |bytes| ea_io::decode(black_box(&bytes)).unwrap(),
            BatchSize::SmallInput,
        )
    });

    // 16 short EAs back-to-back — alignment padding repeats.
    let mut sixteen = Vec::new();
    for i in 0..16 {
        let name = format!("EA{i:04}");
        push_ea(&mut sixteen, name.as_bytes(), b"value-bytes");
    }
    group.bench_function("sixteen_short", |b| {
        b.iter(|| ea_io::decode(black_box(&sixteen)).unwrap())
    });

    group.finish();
}

fn bench_iter_attributes(c: &mut Criterion) {
    let mut group = c.benchmark_group("iter_attributes");

    // Hand-built minimal record: FILE header + USA + STD_INFO +
    // FILE_NAME + DATA + 0xFFFFFFFF terminator. iter_attributes
    // doesn't apply fixup; it walks an already-clean buffer.
    let record = synth_minimal_record();
    group.bench_function("minimal_three_attr", |b| {
        b.iter(|| {
            for loc in attr_io::iter_attributes(black_box(&record)) {
                black_box(loc);
            }
        })
    });

    group.finish();
}

// EA wire format: u32 next_offset, u8 flags, u8 name_len, u16 value_len,
// name (NUL-terminated), value, padding to 4-byte alignment.
fn push_ea(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    let header_len = 8 + name.len() + 1 + value.len();
    let padded = (header_len + 3) & !3;
    let next_offset = padded as u32;
    out.extend_from_slice(&next_offset.to_le_bytes());
    out.push(0); // flags
    out.push(name.len() as u8);
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.push(0); // name terminator
    out.extend_from_slice(value);
    while out.len() < padded + (out.len() - header_len) {
        out.push(0);
    }
}

fn synth_minimal_record() -> Vec<u8> {
    // 1 KiB record buffer. Header is 56 bytes, attrs start at 56.
    // We don't emit valid attributes — iter_attributes skips on
    // unknown type / oversized length, so we use the SHORTCUT of a
    // single 0xFFFFFFFF terminator at offset 56 to end the iter.
    // This benchmarks the header-walk overhead, not real attribute
    // decoding (the per-attr-type cost lives elsewhere).
    let mut buf = vec![0u8; 1024];
    buf[0..4].copy_from_slice(b"FILE");
    buf[20..22].copy_from_slice(&56u16.to_le_bytes()); // first attr offset
                                                       // Emit: $STANDARD_INFORMATION (type=0x10), len=72.
    buf[56..60].copy_from_slice(&0x10u32.to_le_bytes());
    buf[60..64].copy_from_slice(&72u32.to_le_bytes());
    // Emit: $FILE_NAME (type=0x30), len=104.
    let off = 56 + 72;
    buf[off..off + 4].copy_from_slice(&0x30u32.to_le_bytes());
    buf[off + 4..off + 8].copy_from_slice(&104u32.to_le_bytes());
    // Emit: $DATA (type=0x80), len=24.
    let off = off + 104;
    buf[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes());
    buf[off + 4..off + 8].copy_from_slice(&24u32.to_le_bytes());
    // Terminator.
    let off = off + 24;
    buf[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    buf
}

criterion_group!(
    benches,
    bench_decode_runs,
    bench_decode_eas,
    bench_iter_attributes
);
criterion_main!(benches);
