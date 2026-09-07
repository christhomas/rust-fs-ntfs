//! A `$DATA` run list split across MFT records must not read short.
//!
//! When a file's attributes outgrow its base record, NTFS moves some of
//! them into extension records and puts an `$ATTRIBUTE_LIST` in the base
//! record saying where everything went. A heavily fragmented file gets
//! its `$DATA` mapping pairs split that way: the segment covering VCN 0
//! stays in the base record and the continuations go elsewhere.
//!
//! `locate_attribute` documents that it refuses that shape rather than
//! returning a truncated value. This builds one and checks.
//!
//! The fixture is assembled by hand because nothing in this crate writes
//! an `$ATTRIBUTE_LIST` — only Windows does — so no self-generated
//! volume can reach the path. A real 8 KiB file is split down the
//! middle: the base record keeps VCN 0 and a genuine extension record,
//! allocated in `$MFT:$Bitmap` and pointing back at the base through its
//! `base_file_record_reference`, holds VCN 1.

use fs_ntfs::attr_io::{self, attr_off, AttrType};
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::data_runs::{self, DataRun};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{attr_resize, mft_bitmap, mft_io, read, record_build};
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;
/// Two clusters: one segment each side of the split.
const FILE_SIZE: usize = 8192;
const CONTENT: u8 = 0x5A;

// MFT record header fields this test writes by hand. Same layout the
// crate's own `record_build` uses; it keeps its copies private.
const REC_OFF_USA_OFFSET: usize = 0x04;
const REC_OFF_USA_COUNT: usize = 0x06;
const REC_OFF_SEQ: usize = 0x10;
const REC_OFF_LINK_COUNT: usize = 0x12;
const REC_OFF_ATTRS_OFFSET: usize = 0x14;
const REC_OFF_FLAGS: usize = 0x16;
const REC_OFF_BYTES_USED: usize = 0x18;
const REC_OFF_BYTES_ALLOCATED: usize = 0x1C;
const REC_OFF_BASE_FILE_REF: usize = 0x20;
const REC_OFF_NEXT_ATTR_ID: usize = 0x28;
const REC_OFF_MFT_REC_NUM: usize = 0x2C;
const USA_OFFSET: usize = 0x30;

/// One `$ATTRIBUTE_LIST` entry, unnamed: the 0x1A-byte fixed part
/// rounded up to the 8-byte alignment the entries are walked with.
const AL_ENTRY_LEN: usize = 0x20;

fn attribute_list_entry(
    type_code: u32,
    starting_vcn: u64,
    holder_ref: u64,
    attr_id: u16,
) -> Vec<u8> {
    let mut e = vec![0u8; AL_ENTRY_LEN];
    e[0..4].copy_from_slice(&type_code.to_le_bytes());
    e[4..6].copy_from_slice(&(AL_ENTRY_LEN as u16).to_le_bytes());
    e[6] = 0; // name_length
    e[7] = 0x1A; // name_offset (nothing there; the entry is unnamed)
    e[8..16].copy_from_slice(&starting_vcn.to_le_bytes());
    e[16..24].copy_from_slice(&holder_ref.to_le_bytes());
    e[24..26].copy_from_slice(&attr_id.to_le_bytes());
    e
}

/// A resident attribute blob with an arbitrary type code and value.
/// `record_build` has builders for the specific resident attributes the
/// driver writes; `$ATTRIBUTE_LIST` is not one of them, because the
/// driver never writes one.
fn resident_attribute(type_code: u32, attr_id: u16, value: &[u8]) -> Vec<u8> {
    const HEADER: usize = 0x18;
    let attr_length = record_build::align8(HEADER + value.len());
    let mut a = vec![0u8; attr_length];
    a[0..4].copy_from_slice(&type_code.to_le_bytes());
    a[4..8].copy_from_slice(&(attr_length as u32).to_le_bytes());
    a[8] = 0; // resident
    a[9] = 0; // unnamed
    a[10..12].copy_from_slice(&(HEADER as u16).to_le_bytes()); // name_offset
    a[12..14].copy_from_slice(&0u16.to_le_bytes()); // flags
    a[14..16].copy_from_slice(&attr_id.to_le_bytes());
    a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes()); // value_length
    a[20..22].copy_from_slice(&(HEADER as u16).to_le_bytes()); // value_offset
    a[HEADER..HEADER + value.len()].copy_from_slice(value);
    a
}

/// A whole extension MFT record holding one non-resident `$DATA`
/// segment that starts at `first_vcn`.
fn extension_record(
    params: &mft_io::BootParams,
    record_number: u64,
    base_ref: u64,
    first_vcn: u64,
    lcn: u64,
    clusters: u64,
) -> Vec<u8> {
    let size = params.file_record_size as usize;
    let sectors = size / params.bytes_per_sector as usize;
    let mut rec = vec![0u8; size];
    rec[0..4].copy_from_slice(b"FILE");
    rec[REC_OFF_USA_OFFSET..REC_OFF_USA_OFFSET + 2]
        .copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
    rec[REC_OFF_USA_COUNT..REC_OFF_USA_COUNT + 2]
        .copy_from_slice(&((sectors + 1) as u16).to_le_bytes());
    rec[REC_OFF_SEQ..REC_OFF_SEQ + 2].copy_from_slice(&1u16.to_le_bytes());
    // An extension record is not a file: it has no name and no links.
    rec[REC_OFF_LINK_COUNT..REC_OFF_LINK_COUNT + 2].copy_from_slice(&0u16.to_le_bytes());
    let attrs_offset = record_build::align8(USA_OFFSET + 2 + sectors * 2);
    rec[REC_OFF_ATTRS_OFFSET..REC_OFF_ATTRS_OFFSET + 2]
        .copy_from_slice(&(attrs_offset as u16).to_le_bytes());
    rec[REC_OFF_FLAGS..REC_OFF_FLAGS + 2].copy_from_slice(&mft_io::MFT_FLAG_IN_USE.to_le_bytes());
    rec[REC_OFF_BYTES_ALLOCATED..REC_OFF_BYTES_ALLOCATED + 4]
        .copy_from_slice(&(size as u32).to_le_bytes());
    rec[REC_OFF_BASE_FILE_REF..REC_OFF_BASE_FILE_REF + 8].copy_from_slice(&base_ref.to_le_bytes());
    rec[REC_OFF_NEXT_ATTR_ID..REC_OFF_NEXT_ATTR_ID + 2].copy_from_slice(&1u16.to_le_bytes());
    rec[REC_OFF_MFT_REC_NUM..REC_OFF_MFT_REC_NUM + 4]
        .copy_from_slice(&(record_number as u32).to_le_bytes());
    rec[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());

    // A segment's mapping pairs are relative to the segment: the LCN
    // delta chain restarts at zero, so encoding it as a VCN-0 run and
    // then setting first_vcn is what the on-disk form is.
    let pairs = data_runs::encode_runs(&[DataRun {
        starting_vcn: 0,
        length: clusters,
        lcn: Some(lcn),
    }])
    .expect("encode the extension segment's runs");
    // Only the VCN-0 segment carries the value's lengths; the
    // continuations leave them zero.
    let mut data = record_build::build_nonresident_data_attribute(
        0,
        0,
        0,
        0,
        (first_vcn + clusters - 1) as i64,
        &pairs,
    )
    .expect("build the extension segment's $DATA");
    data[attr_off::NONRES_FIRST_VCN..attr_off::NONRES_FIRST_VCN + 8]
        .copy_from_slice(&first_vcn.to_le_bytes());

    rec[attrs_offset..attrs_offset + data.len()].copy_from_slice(&data);
    let marker = attrs_offset + data.len();
    rec[marker..marker + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let bytes_used = record_build::align8(marker + 8);
    rec[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4]
        .copy_from_slice(&(bytes_used as u32).to_le_bytes());
    rec
}

/// An 8 KiB file whose `$DATA` really is split: VCN 0 in the base
/// record, VCN 1 in an extension record, and an `$ATTRIBUTE_LIST` in the
/// base record that says so. Returns the image path and the base record
/// number.
fn volume_with_a_split_file(tag: &str) -> (String, u64) {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_alsplit_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("ALST"), Some(2))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);

    // A real file with real content, and a second file whose record we
    // borrow: creating it puts the record inside the MFT's allocated
    // extent, and unlinking it takes the name back out of the index.
    let fs = Filesystem::mount_rw(&dst).expect("mount_rw");
    fs.create_file("/", "split.bin").expect("create split.bin");
    fs.write_file_contents("/split.bin", &vec![CONTENT; FILE_SIZE])
        .expect("write split.bin");
    fs.create_file("/", "donor.tmp").expect("create donor.tmp");
    let ext_rec = {
        let mut probe = PathIo::open_ro(Path::new(&dst)).expect("open_ro");
        read::resolve_path(&mut probe, "/donor.tmp").expect("resolve donor")
    };
    fs.unlink("/donor.tmp").expect("unlink donor.tmp");
    drop(fs);

    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    let base_rec = read::resolve_path(&mut io, "/split.bin").expect("resolve split.bin");
    let (params, record) = mft_io::read_mft_record_io(&mut io, base_rec).expect("read base record");
    let base_seq = u16::from_le_bytes([record[REC_OFF_SEQ], record[REC_OFF_SEQ + 1]]);
    let base_ref = record_build::encode_file_reference(base_rec, base_seq);

    let loc = attr_io::find_attribute(&record, AttrType::Data, None).expect("$DATA");
    let mpo = loc.non_resident_mapping_pairs_offset.expect("mpo") as usize;
    let runs =
        data_runs::decode_runs(&record[loc.attr_offset + mpo..loc.attr_offset + loc.attr_length])
            .expect("decode base runs");
    assert_eq!(runs.len(), 1, "expected split.bin to be one extent");
    assert_eq!(runs[0].length, 2, "expected split.bin to own two clusters");
    let lcn = runs[0].lcn.expect("split.bin's run is allocated");
    let data_attr_id = u16::from_le_bytes([
        record[loc.attr_offset + attr_off::ATTRIBUTE_ID],
        record[loc.attr_offset + attr_off::ATTRIBUTE_ID + 1],
    ]);
    let si_id = attr_io::find_attribute(&record, AttrType::StandardInformation, None)
        .map(|l| {
            u16::from_le_bytes([
                record[l.attr_offset + attr_off::ATTRIBUTE_ID],
                record[l.attr_offset + attr_off::ATTRIBUTE_ID + 1],
            ])
        })
        .expect("$STANDARD_INFORMATION");
    let fn_id = attr_io::find_attribute(&record, AttrType::FileName, None)
        .map(|l| {
            u16::from_le_bytes([
                record[l.attr_offset + attr_off::ATTRIBUTE_ID],
                record[l.attr_offset + attr_off::ATTRIBUTE_ID + 1],
            ])
        })
        .expect("$FILE_NAME");

    // The extension record: VCN 1, the file's second cluster.
    let ext = extension_record(&params, ext_rec, base_ref, 1, lcn + 1, 1);
    let mut on_disk = ext.clone();
    mft_io::apply_fixup_on_write(&mut on_disk, params.bytes_per_sector).expect("fixup");
    io.write_all_at(mft_io::mft_record_offset(&params, ext_rec), &on_disk)
        .expect("write extension record");
    let bm = mft_bitmap::locate_io(&mut io).expect("locate $MFT:$Bitmap");
    mft_bitmap::allocate_io(&mut io, &bm, ext_rec).expect("allocate the extension record");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");

    let ext_ref = record_build::encode_file_reference(ext_rec, 1);
    mft_io::update_mft_record_io(&mut io, base_rec, |record| {
        // Shrink the base segment to VCN 0 only, keeping the value's
        // declared lengths — the file is still 8192 bytes, it is just
        // described in two places now.
        let loc = attr_io::find_attribute(record, AttrType::Data, None).expect("$DATA");
        let pairs = data_runs::encode_runs(&[DataRun {
            starting_vcn: 0,
            length: 1,
            lcn: Some(lcn),
        }])?;
        let shrunk = record_build::build_nonresident_data_attribute(
            data_attr_id,
            FILE_SIZE as u64,
            FILE_SIZE as u64,
            FILE_SIZE as u64,
            0,
            &pairs,
        )?;
        attr_resize::replace_attribute(record, loc.attr_offset, &shrunk)?;

        let mut value = Vec::new();
        value.extend_from_slice(&attribute_list_entry(0x10, 0, base_ref, si_id));
        value.extend_from_slice(&attribute_list_entry(0x30, 0, base_ref, fn_id));
        value.extend_from_slice(&attribute_list_entry(0x80, 0, base_ref, data_attr_id));
        value.extend_from_slice(&attribute_list_entry(0x80, 1, ext_ref, 0));
        let al = resident_attribute(AttrType::AttributeList as u32, 3, &value);
        attr_resize::insert_attribute_sorted(record, &al)
    })
    .expect("rewrite the base record");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");

    // The fixture has to be the thing it claims to be, or the test below
    // proves nothing: the extension record parses, and its $DATA really
    // does start at VCN 1.
    let (_, ext_read) = mft_io::read_mft_record_io(&mut io, ext_rec).expect("read ext record");
    let ext_data = attr_io::find_attribute(&ext_read, AttrType::Data, None)
        .expect("the extension record holds a $DATA");
    let first_vcn = u64::from_le_bytes(
        ext_read[ext_data.attr_offset + attr_off::NONRES_FIRST_VCN
            ..ext_data.attr_offset + attr_off::NONRES_FIRST_VCN + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(first_vcn, 1, "the extension segment must start at VCN 1");
    let (_, base_read) = mft_io::read_mft_record_io(&mut io, base_rec).expect("re-read base");
    let al_loc = attr_io::find_attribute(&base_read, AttrType::AttributeList, None)
        .expect("the base record holds an $ATTRIBUTE_LIST");
    let al_value = read::read_attribute_value(&mut io, base_rec, AttrType::AttributeList, None)
        .expect("read the $ATTRIBUTE_LIST value");
    assert!(al_loc.is_resident);
    let entries = read::parse_attribute_list(&al_value).expect("parse the list");
    let data_segments: Vec<_> = entries.iter().filter(|e| e.type_code == 0x80).collect();
    assert_eq!(
        data_segments.len(),
        2,
        "the list must name both $DATA segments, got {entries:?}"
    );
    drop(io);
    (dst, base_rec)
}

#[test]
fn a_split_run_list_is_refused_rather_than_read_short() {
    let (img, base_rec) = volume_with_a_split_file("read");
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");

    match read::read_attribute_value(&mut io, base_rec, AttrType::Data, None) {
        Err(e) => assert!(
            e.contains("split across"),
            "the refusal should say why: {e}"
        ),
        Ok(bytes) => {
            let missing = bytes.iter().filter(|&&b| b != CONTENT).count();
            panic!(
                "read_attribute_value returned Ok with {} bytes, of which {missing} are not \
                 the {CONTENT:#04x} that was written -- the VCN-1 segment lives in an \
                 extension record and was silently dropped",
                bytes.len()
            );
        }
    }
}

#[test]
fn a_split_run_list_is_refused_by_the_ranged_read_too() {
    let (img, base_rec) = volume_with_a_split_file("range");
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");

    let tail = read::read_attribute_range(&mut io, base_rec, AttrType::Data, None, 4096, 4096);
    match tail {
        Err(e) => assert!(
            e.contains("split across"),
            "the refusal should say why: {e}"
        ),
        Ok(bytes) => panic!(
            "read_attribute_range returned Ok with {} bytes for a window that lies \
             entirely in the segment held by an extension record: {:02x?}...",
            bytes.len(),
            &bytes[..8.min(bytes.len())]
        ),
    }
}

#[test]
fn stat_still_reads_a_split_file_through_the_base_record() {
    // $STANDARD_INFORMATION and $FILE_NAME are listed too, and each has
    // exactly one segment. Consulting the list first must not turn an
    // ordinary attribute lookup on a file that HAS a list into a
    // failure.
    let (img, base_rec) = volume_with_a_split_file("stat");
    let mut io = PathIo::open_ro(Path::new(&img)).expect("open_ro");
    let si = read::read_attribute_value(&mut io, base_rec, AttrType::StandardInformation, None)
        .expect("$STANDARD_INFORMATION is listed once and lives in the base record");
    assert!(
        si.len() >= 48,
        "a $STANDARD_INFORMATION value is at least 48 bytes, got {}",
        si.len()
    );
    let fname = read::read_attribute_value(&mut io, base_rec, AttrType::FileName, None)
        .expect("$FILE_NAME is listed once and lives in the base record");
    assert!(!fname.is_empty());
}
