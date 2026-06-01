// Recovered original: extracted verbatim from the session transcript
// (77f00d3f, the `cat > examples/probe_dbg.rs` heredoc that preceded the
// accidental rm). This is the true original scratch probe — it checks
// whether a freshly-written resident file's MFT record reports
// bytes_used > bytes_allocated (the record-overflow question it was
// chasing). Agent 1's general-purpose rebuild is preserved in git
// commit f51112a if the dumper form is wanted instead.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::mft_io::read_mft_record;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::write;
use std::path::Path;

fn main() {
    let dst = "test-disks/_probe_dbg.img";
    let f = std::fs::File::create(dst).unwrap();
    f.set_len(64 * 1024 * 1024).unwrap();
    drop(f);
    let mut io = PathIo::open_rw(Path::new(dst)).unwrap();
    format_filesystem(&mut io, 64 * 1024 * 1024, 4096, 4096, Some("DBG"), Some(1)).unwrap();
    <PathIo as BlockIo>::sync(&mut io).unwrap();
    drop(io);

    let rec = write::create_file(Path::new(dst), "/", "p.bin").unwrap();
    let payload = vec![0x5Au8; 1024];
    let wr = write::write_resident_contents(Path::new(dst), "/p.bin", &payload);
    println!("record_number={rec} write_result={:?}", wr.map(|_| "OK"));

    // Read raw record and dump header fields.
    let (params, record) = read_mft_record(Path::new(dst), rec).unwrap();
    let buf_len = record.len();
    let bytes_used = u32::from_le_bytes([record[0x18], record[0x19], record[0x1A], record[0x1B]]);
    let bytes_alloc = u32::from_le_bytes([record[0x1C], record[0x1D], record[0x1E], record[0x1F]]);
    println!("file_record_size(param)={} buffer_len={buf_len} bytes_used={bytes_used} bytes_allocated={bytes_alloc}",
        params.file_record_size);
    println!(
        "OVERFLOW? bytes_used({bytes_used}) > bytes_allocated({bytes_alloc}): {}",
        bytes_used > bytes_alloc
    );
}
