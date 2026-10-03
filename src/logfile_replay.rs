//! Replaying `$LogFile`: redoing the transactions NTFS logged and had not
//! yet written home when the volume stopped (#137).
//!
//! [`plan`] reads the log and the blocks it names, and returns every write
//! the replay makes, in memory. It writes nothing. Anything it cannot do
//! completely and as Windows does it is an error, so the caller writes
//! either the whole replay or nothing at all: a partial redo leaves
//! metadata that matches no state the volume was ever in.
//!
//! # What is replayed, and how that was established
//!
//! The ARIES-style restart NTFS performs, as far as five volumes Windows
//! left mid-write need it (`test-disks/windows-interrupted-*`, three of
//! whose logs wrapped between the checkpoint and the capture):
//!
//! * **analysis** from the last checkpoint: the open attribute table and
//!   the dirty page table as the checkpoint dumped them, extended by every
//!   record after it, and the transactions still open at the end;
//! * **redo** from the oldest dirty page's LSN to the log's last record:
//!   a record is applied when its page is in the dirty page table at or
//!   before the record, and, for an MFT record or an index block, when the
//!   block's own LSN is older than the record's;
//! * **undo is not implemented.** A transaction still open at the end of
//!   the log with anything to undo is refused. No fixture has one: every
//!   transaction in each ends in a `ForgetTransaction` record.
//!
//! Every redo operation replayed here was checked against what Windows'
//! own restart produced from the same pre-image (the `.recovered` images):
//! after replay every MFT record, every index block and `$MFT`'s bitmap
//! are what Windows wrote, and `$Bitmap` differs only where Windows
//! allocated clusters after its restart. Operations neither log holds are
//! refused, not guessed at.
//!
//! The semantics that were not obvious, each settled by that comparison:
//!
//! * `UpdateResidentValue` grows the value when the write runs past its
//!   end; `CreateAttribute` advances the record's next attribute instance;
//!   `UpdateMappingPairs` resizes the attribute and recomputes its highest
//!   VCN from the new runs.
//! * Records whose header carries flag `0x4` may hold less redo data than
//!   their redo length (`UpdateResidentValue`, `ZeroEndOfFileRecord`): the
//!   rest is zeros. That is a resident file's data, which NTFS does not
//!   log; Windows recovers those bytes as zeros too.
//! * In an LFS 2.x log the newest pages are written to the tail-copy area
//!   before their home, so a page is read from whichever valid copy -- home
//!   or tail -- has the highest last LSN.
//! * The log wraps from its last page to the first page of its record
//!   area (the first page at its own home, after the tail-copy area), one
//!   sequence number up: a record may run across the end, and the record
//!   after the last page's last starts the new lap.
//! * `DeallocateFileRecordSegment` clears the in-use flag and increments
//!   the record's sequence number, skipping 0.
//! * `SetIndexEntryVcnAllocation` writes the child VCN into the last 8
//!   bytes of the entry it names inside an index block, as
//!   `SetIndexEntryVcnRoot` does in an index root
//!   (`test-disks/windows-interrupted-index-vcn`).
//! * `DeleteIndexEntryAllocation` moves the entries after it down and
//!   leaves the bytes past the block's new end as they were; the block's
//!   update sequence array, which records the end of every sector, shows
//!   it.
//! * A log may grow `$MFT` and fill the records it adds, so the replay's
//!   own `$MFT` -- not the one on disk before it -- says where they are.
//!
//! # Formats
//!
//! LFS restart and record pages, the NTFS client record header, and the
//! restart tables, from MS-FSCC and Windows Internals 7th ed. ("NTFS
//! Logging"), with field meanings confirmed on the captured logs. No GPL
//! implementation was consulted.

use std::collections::{BTreeMap, HashMap};

use crate::error::Error;
use crate::mft_io::{apply_fixup_on_read_magic, apply_fixup_on_write_magic, BootParams};

/// Log pages are protected with a 512-byte update sequence stride.
const LFS_STRIDE: u16 = 512;
/// Size of an LFS record header; the client data follows it.
const RECORD_HEADER: usize = 0x30;
/// LFS record types.
const LFS_CLIENT_RECORD: u32 = 1;
const LFS_CLIENT_RESTART: u32 = 2;
/// Record page flag: a record ends on this page.
const PAGE_RECORD_END: u32 = 1;
/// Record header flag on records whose redo data may be shorter than its
/// declared length, the rest being zeros.
const RECORD_REDO_ZEROS: u16 = 0x4;
/// A restart-table entry in use.
const ENTRY_ALLOCATED: u32 = 0xFFFF_FFFF;
/// Open attribute table entry size Windows 8 and later write, the only
/// one these tables were checked on.
const OPEN_ATTRIBUTE_ENTRY: usize = 0x28;

// NTFS log operations (redo and undo codes share one numbering).
const NOOP: u16 = 0x00;
const COMPENSATION: u16 = 0x01;
const INITIALIZE_FILE_RECORD: u16 = 0x02;
const DEALLOCATE_FILE_RECORD: u16 = 0x03;
const CREATE_ATTRIBUTE: u16 = 0x05;
const DELETE_ATTRIBUTE: u16 = 0x06;
const UPDATE_RESIDENT_VALUE: u16 = 0x07;
const UPDATE_NONRESIDENT_VALUE: u16 = 0x08;
const UPDATE_MAPPING_PAIRS: u16 = 0x09;
const SET_NEW_ATTRIBUTE_SIZES: u16 = 0x0B;
const ADD_INDEX_ENTRY_ROOT: u16 = 0x0C;
const DELETE_INDEX_ENTRY_ROOT: u16 = 0x0D;
const ADD_INDEX_ENTRY_ALLOCATION: u16 = 0x0E;
const DELETE_INDEX_ENTRY_ALLOCATION: u16 = 0x0F;
const WRITE_END_OF_INDEX_BUFFER: u16 = 0x10;
const SET_INDEX_ENTRY_VCN_ROOT: u16 = 0x11;
const SET_INDEX_ENTRY_VCN_ALLOCATION: u16 = 0x12;
const UPDATE_FILE_NAME_ROOT: u16 = 0x13;
const UPDATE_FILE_NAME_ALLOCATION: u16 = 0x14;
const SET_BITS_IN_NONRESIDENT_BITMAP: u16 = 0x15;
const CLEAR_BITS_IN_NONRESIDENT_BITMAP: u16 = 0x16;
const END_TOP_LEVEL_ACTION: u16 = 0x18;
const PREPARE_TRANSACTION: u16 = 0x19;
const COMMIT_TRANSACTION: u16 = 0x1A;
const FORGET_TRANSACTION: u16 = 0x1B;
const OPEN_NONRESIDENT_ATTRIBUTE: u16 = 0x1C;
const OPEN_ATTRIBUTE_TABLE_DUMP: u16 = 0x1D;
const TRANSACTION_TABLE_DUMP: u16 = 0x20;
const ZERO_END_OF_FILE_RECORD: u16 = 0x25;

/// Operations on an MFT record (`$MFT`'s `$DATA`).
const ON_FILE_RECORD: &[u16] = &[
    INITIALIZE_FILE_RECORD,
    DEALLOCATE_FILE_RECORD,
    CREATE_ATTRIBUTE,
    DELETE_ATTRIBUTE,
    UPDATE_RESIDENT_VALUE,
    UPDATE_MAPPING_PAIRS,
    SET_NEW_ATTRIBUTE_SIZES,
    ADD_INDEX_ENTRY_ROOT,
    DELETE_INDEX_ENTRY_ROOT,
    SET_INDEX_ENTRY_VCN_ROOT,
    UPDATE_FILE_NAME_ROOT,
    ZERO_END_OF_FILE_RECORD,
];
/// Operations on an index block (`$INDEX_ALLOCATION`).
const ON_INDEX_BLOCK: &[u16] = &[
    ADD_INDEX_ENTRY_ALLOCATION,
    DELETE_INDEX_ENTRY_ALLOCATION,
    WRITE_END_OF_INDEX_BUFFER,
    SET_INDEX_ENTRY_VCN_ALLOCATION,
    UPDATE_FILE_NAME_ALLOCATION,
];

fn op_name(op: u16) -> &'static str {
    const NAMES: [&str; 38] = [
        "Noop",
        "CompensationLogRecord",
        "InitializeFileRecordSegment",
        "DeallocateFileRecordSegment",
        "WriteEndOfFileRecordSegment",
        "CreateAttribute",
        "DeleteAttribute",
        "UpdateResidentValue",
        "UpdateNonresidentValue",
        "UpdateMappingPairs",
        "DeleteDirtyClusters",
        "SetNewAttributeSizes",
        "AddIndexEntryRoot",
        "DeleteIndexEntryRoot",
        "AddIndexEntryAllocation",
        "DeleteIndexEntryAllocation",
        "WriteEndOfIndexBuffer",
        "SetIndexEntryVcnRoot",
        "SetIndexEntryVcnAllocation",
        "UpdateFileNameRoot",
        "UpdateFileNameAllocation",
        "SetBitsInNonresidentBitMap",
        "ClearBitsInNonresidentBitMap",
        "HotFix",
        "EndTopLevelAction",
        "PrepareTransaction",
        "CommitTransaction",
        "ForgetTransaction",
        "OpenNonresidentAttribute",
        "OpenAttributeTableDump",
        "AttributeNamesDump",
        "DirtyPageTableDump",
        "TransactionTableDump",
        "UpdateRecordDataRoot",
        "UpdateRecordDataAllocation",
        "UpdateRelativeDataInIndex",
        "UpdateRelativeDataInIndex2",
        "ZeroEndOfFileRecord",
    ];
    NAMES.get(op as usize).copied().unwrap_or("unknown")
}

fn refuse(why: impl std::fmt::Display) -> Error {
    Error::io(format!(
        "$LogFile cannot be replayed in full ({why}), so nothing was replayed \
         (rust-fs-ntfs#137)"
    ))
}

fn u16_at(b: &[u8], off: usize) -> Result<u16, Error> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| refuse(format!("a field at {off:#x} is past its structure")))
}
fn u32_at(b: &[u8], off: usize) -> Result<u32, Error> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| refuse(format!("a field at {off:#x} is past its structure")))
}
fn u64_at(b: &[u8], off: usize) -> Result<u64, Error> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| refuse(format!("a field at {off:#x} is past its structure")))
}
fn put(b: &mut [u8], off: usize, data: &[u8]) -> Result<(), Error> {
    let end = off
        .checked_add(data.len())
        .filter(|&e| e <= b.len())
        .ok_or_else(|| {
            refuse(format!(
                "a write of {} bytes at {off:#x} runs past a {}-byte block",
                data.len(),
                b.len()
            ))
        })?;
    b[off..end].copy_from_slice(data);
    Ok(())
}
fn put_u32(b: &mut [u8], off: usize, v: u32) -> Result<(), Error> {
    put(b, off, &v.to_le_bytes())
}
fn put_u64(b: &mut [u8], off: usize, v: u64) -> Result<(), Error> {
    put(b, off, &v.to_le_bytes())
}

/// Shift `b[at..used]` up by `data.len()` and write `data` at `at`.
fn insert(b: &mut [u8], at: usize, used: usize, data: &[u8]) -> Result<(), Error> {
    if at > used || used + data.len() > b.len() {
        return Err(refuse(format!(
            "an insert of {} bytes at {at:#x} does not fit a block using {used:#x} of {:#x}",
            data.len(),
            b.len()
        )));
    }
    b.copy_within(at..used, at + data.len());
    b[at..at + data.len()].copy_from_slice(data);
    Ok(())
}

/// Remove `n` bytes at `at` from `b[..used]`, shifting the rest down.
fn remove(b: &mut [u8], at: usize, n: usize, used: usize) -> Result<(), Error> {
    if at.checked_add(n).is_none_or(|e| e > used) || used > b.len() {
        return Err(refuse(format!(
            "a removal of {n} bytes at {at:#x} runs past the {used:#x} bytes in use"
        )));
    }
    b.copy_within(at + n..used, at);
    b[used - n..used].fill(0);
    Ok(())
}

/// [`remove`], leaving the `n` bytes before `used` as they were.
fn shift_down(b: &mut [u8], at: usize, n: usize, used: usize) -> Result<(), Error> {
    if at.checked_add(n).is_none_or(|e| e > used) || used > b.len() {
        return Err(refuse(format!(
            "a removal of {n} bytes at {at:#x} runs past the {used:#x} bytes in use"
        )));
    }
    b.copy_within(at + n..used, at);
    Ok(())
}

/// The log, read whole, with every valid copy of every record page.
struct Log<'a> {
    bytes: &'a [u8],
    log_page: usize,
    seq_bits: u32,
    data_offset: usize,
    /// The first page of the record area: where the log continues when it
    /// wraps past its end, its sequence number one higher.
    first_page: usize,
    /// Home page offset -> (last LSN, whether this copy sits at that
    /// offset, page with fixups undone), one per valid copy: the page
    /// itself and any tail copy of it.
    copies: HashMap<usize, Vec<(u64, bool, Vec<u8>)>>,
    /// The LSN of the last record the log holds.
    newest: u64,
}

impl Log<'_> {
    fn offset(&self, lsn: u64) -> usize {
        ((lsn << self.seq_bits) >> (self.seq_bits - 3)) as usize
    }
    fn lsn(&self, wrap: u64, offset: usize) -> u64 {
        (wrap << (64 - self.seq_bits)) | (offset as u64 >> 3)
    }
    fn wrap(&self, lsn: u64) -> u64 {
        lsn >> (64 - self.seq_bits)
    }

    /// The newest valid copy of the page at `home` that already holds `lsn`.
    ///
    /// The newest copy wins; between copies equally new, the one at its
    /// home. Two tail copies equally new and different are refused.
    fn page(&self, home: usize, lsn: u64) -> Result<&[u8], Error> {
        let candidates: Vec<_> = self
            .copies
            .get(&home)
            .map(|c| c.iter().filter(|(last, _, _)| *last >= lsn).collect())
            .unwrap_or_default();
        let best = candidates
            .iter()
            .max_by_key(|(last, at_home, _)| (*last, *at_home))
            .ok_or_else(|| {
                refuse(format!(
                    "no valid copy of the log page at {home:#x} holds LSN {lsn:#x}"
                ))
            })?;
        if !best.1
            && candidates
                .iter()
                .any(|(last, at_home, page)| *last == best.0 && !at_home && *page != best.2)
        {
            return Err(refuse(format!(
                "two different tail copies of the log page at {home:#x} are equally new"
            )));
        }
        Ok(best.2.as_slice())
    }

    /// The record at `lsn`: its header and its client data, and the LSN
    /// of the record after it.
    fn record(&self, lsn: u64) -> Result<([u8; RECORD_HEADER], Vec<u8>, u64), Error> {
        let at = self.offset(lsn);
        let lps = self.log_page;
        let (mut home, within) = (at - at % lps, at % lps);
        let page = self.page(home, lsn)?;
        if u64_at(page, within)? != lsn || within + RECORD_HEADER > lps {
            return Err(refuse(format!(
                "the record at LSN {lsn:#x} is not where it says"
            )));
        }
        let mut header = [0u8; RECORD_HEADER];
        header.copy_from_slice(&page[within..within + RECORD_HEADER]);
        let len = u32_at(&header, 0x18)? as usize;
        if len > self.bytes.len() {
            return Err(refuse(format!(
                "the record at LSN {lsn:#x} claims {len} bytes"
            )));
        }
        let mut data =
            page[within + RECORD_HEADER..(within + RECORD_HEADER + len).min(lps)].to_vec();
        let mut end = within + RECORD_HEADER + data.len();
        // Whether the log wrapped between this record's start and the next.
        let mut wrapped = false;
        while data.len() < len {
            home += lps;
            if home + lps > self.bytes.len() {
                // The record continues on the record area's first page.
                home = self.first_page;
                wrapped = true;
            }
            let page = self.page(home, lsn)?;
            let take = (len - data.len()).min(lps - self.data_offset);
            data.extend_from_slice(&page[self.data_offset..self.data_offset + take]);
            end = self.data_offset + take;
        }
        // The next record starts 8-aligned after this one, on the next
        // page when fewer than a record header's bytes remain on this one.
        let mut next = home + end.div_ceil(8) * 8;
        if lps - next % lps < RECORD_HEADER {
            next = next - next % lps + lps;
        }
        if next >= self.bytes.len() {
            // Past the last page: the next record starts the record area's
            // first page.
            next = self.first_page;
            wrapped = true;
        }
        if next.is_multiple_of(lps) {
            next += self.data_offset;
        }
        Ok((
            header,
            data,
            self.lsn(self.wrap(lsn) + u64::from(wrapped), next),
        ))
    }
}

/// The parts of a restart area replay needs.
struct Restart {
    current_lsn: u64,
    seq_bits: u32,
    log_page: usize,
    data_offset: usize,
    file_size: u64,
    major: i16,
    /// The in-use client's restart LSN: its last checkpoint.
    checkpoint: Option<u64>,
}

fn restart(bytes: &[u8], at: usize, sps: usize) -> Result<Restart, Error> {
    let mut page = bytes
        .get(at..at + sps)
        .ok_or_else(|| refuse(format!("no restart page at {at:#x}")))?
        .to_vec();
    apply_fixup_on_read_magic(&mut page, LFS_STRIDE, b"RSTR")
        .map_err(|e| refuse(format!("restart page at {at:#x}: {e}")))?;
    let ra = u16_at(&page, 0x18)? as usize;
    let clients = u16_at(&page, ra + 0x08)?;
    let in_use = u16_at(&page, ra + 0x0C)?;
    let checkpoint = if in_use == 0xFFFF {
        None
    } else {
        if in_use >= clients {
            return Err(refuse(format!("in-use client {in_use} of {clients}")));
        }
        let client = ra + u16_at(&page, ra + 0x16)? as usize + in_use as usize * 0xA0;
        Some(u64_at(&page, client + 0x08)?)
    };
    Ok(Restart {
        current_lsn: u64_at(&page, ra)?,
        seq_bits: u32_at(&page, ra + 0x10)?,
        log_page: u32_at(&page, 0x14)? as usize,
        data_offset: u16_at(&page, ra + 0x26)? as usize,
        file_size: u64_at(&page, ra + 0x18)?,
        major: u16_at(&page, 0x1C)? as i16,
        checkpoint,
    })
}

/// A block of metadata the replay has read, and changed, in memory.
struct Block {
    /// `FILE` or `INDX`.
    magic: &'static [u8; 4],
    /// With fixups undone, when it read as a block of its kind.
    bytes: Vec<u8>,
    /// Whether it read as one (a torn or never-written block does not).
    valid: bool,
    changed: bool,
    /// The MFT record number, for a `FILE` block.
    record: Option<u64>,
}

/// Reads `buf.len()` bytes of the volume at a byte offset.
pub type ReadVolume<'a> = dyn FnMut(u64, &mut [u8]) -> Result<(), Error> + 'a;

/// Every write a full replay makes, none of them made yet.
#[derive(Debug, Default)]
pub struct Plan {
    /// Device byte offset -> bytes to write there (fixups applied).
    pub writes: BTreeMap<u64, Vec<u8>>,
    /// Every MFT record the replay writes, by number, and the device byte
    /// offset the log put it at. The caller checks each against where this
    /// volume's `$MFT` has that record before writing anything: a log that
    /// is not this volume's must not be replayed onto it.
    pub mft_records: Vec<(u64, u64)>,
    /// Records read from the oldest dirty page to the log's end.
    pub records: u64,
    /// Redo operations applied.
    pub applied: u64,
    /// The checkpoint the analysis started from, and the last LSN.
    pub checkpoint: u64,
    pub last_lsn: u64,
}

/// Plan the replay of `log` -- the whole of `$LogFile`'s `$DATA` -- over
/// the volume `read` reads, whose geometry is `params`. Writes nothing.
pub fn plan(log: &[u8], params: &BootParams, read: &mut ReadVolume<'_>) -> Result<Plan, Error> {
    // ---- the restart area ------------------------------------------------
    let sps = u32_at(log, 0x10)? as usize;
    if !(512..=65536).contains(&sps) || !sps.is_power_of_two() {
        return Err(refuse(format!("a system page size of {sps}")));
    }
    let ra = match (restart(log, 0, sps), restart(log, sps, sps)) {
        (Ok(a), Ok(b)) => {
            if b.current_lsn > a.current_lsn {
                b
            } else {
                a
            }
        }
        (Ok(a), Err(_)) | (Err(_), Ok(a)) => a,
        (Err(a), Err(_)) => return Err(a),
    };
    if ra.major != 2 {
        return Err(refuse(format!(
            "LFS version {}, and replay has been checked only on the 2.x logs Windows 8 \
             and later write",
            ra.major
        )));
    }
    let checkpoint = ra
        .checkpoint
        .ok_or_else(|| refuse("no client has the log open"))?;
    let lps = ra.log_page;
    if !(512..=65536).contains(&lps)
        || !lps.is_power_of_two()
        || !(3..64).contains(&ra.seq_bits)
        || ra.data_offset < 0x28
        || ra.data_offset >= lps
        || ra.file_size != log.len() as u64
    {
        return Err(refuse(format!(
            "log geometry: page {lps}, data offset {:#x}, {} sequence bits, file size {} of {}",
            ra.data_offset,
            ra.seq_bits,
            ra.file_size,
            log.len()
        )));
    }

    // ---- every valid copy of every record page ---------------------------
    let mut l = Log {
        bytes: log,
        log_page: lps,
        seq_bits: ra.seq_bits,
        data_offset: ra.data_offset,
        first_page: 0,
        copies: HashMap::new(),
        newest: 0,
    };
    // A page names its home through its last LSN. Only a page in the tail
    // area -- before the first page that sits at its own home -- is a copy
    // of another page: in the record area, a page no record starts on
    // carries the LSN of the record spanning into it, which names the page
    // before it.
    let mut pages = Vec::new();
    let mut at = 2 * sps;
    while at + lps <= log.len() {
        let mut page = log[at..at + lps].to_vec();
        if &page[..4] == b"RCRD"
            && apply_fixup_on_read_magic(&mut page, LFS_STRIDE, b"RCRD").is_ok()
        {
            let last = u64_at(&page, 0x08)?;
            if u32_at(&page, 0x10)? & PAGE_RECORD_END != 0 {
                l.newest = l.newest.max(u64_at(&page, 0x20)?);
            }
            let home = l.offset(last);
            pages.push((at, home - home % lps, last, page));
        }
        at += lps;
    }
    let record_area = pages
        .iter()
        .filter(|(at, home, _, _)| at == home)
        .map(|(at, _, _, _)| *at)
        .min()
        .unwrap_or(log.len());
    l.first_page = record_area;
    for (at, home, last, page) in pages {
        if home != at && at < record_area {
            l.copies
                .entry(home)
                .or_default()
                .push((last, false, page.clone()));
        }
        l.copies.entry(at).or_default().push((last, true, page));
    }

    // ---- the checkpoint and the tables it dumped -------------------------
    let (header, body, _) = l.record(checkpoint)?;
    if u32_at(&header, 0x20)? != LFS_CLIENT_RESTART || body.len() < 0x40 {
        return Err(refuse(format!(
            "the checkpoint at LSN {checkpoint:#x} is not one"
        )));
    }
    if u32_at(&body, 0x00)? != 1 {
        return Err(refuse(format!(
            "an NTFS restart area of version {}, and replay has been checked only on 1",
            u32_at(&body, 0x00)?
        )));
    }
    let start = u64_at(&body, 0x08)?;
    let table = |lsn: u64, op: u16| -> Result<Option<Vec<u8>>, Error> {
        if lsn == 0 {
            return Ok(None);
        }
        let (_, data, _) = l.record(lsn)?;
        if u16_at(&data, 0x00)? != op {
            return Err(refuse(format!(
                "the table at LSN {lsn:#x} is not a {}",
                op_name(op)
            )));
        }
        let (off, len) = (u16_at(&data, 0x04)? as usize, u16_at(&data, 0x06)? as usize);
        data.get(off..off + len)
            .map(|t| Some(t.to_vec()))
            .ok_or_else(|| refuse(format!("the table at LSN {lsn:#x} is truncated")))
    };
    let entries = |t: &[u8]| -> Result<Vec<(usize, Vec<u8>)>, Error> {
        let (size, count) = (u16_at(t, 0)? as usize, u16_at(t, 2)? as usize);
        let mut out = Vec::new();
        for i in 0..count {
            let at = 0x18 + i * size;
            let e = t
                .get(at..at + size)
                .ok_or_else(|| refuse("a restart table entry past its table"))?;
            if u32_at(e, 0)? == ENTRY_ALLOCATED {
                out.push((at, e.to_vec()));
            }
        }
        Ok(out)
    };
    // Target attribute (an offset into this table) -> (type, MFT record).
    let mut open: HashMap<u16, (u32, u64)> = HashMap::new();
    if let Some(t) = table(u64_at(&body, 0x10)?, OPEN_ATTRIBUTE_TABLE_DUMP)? {
        if u16_at(&t, 0)? as usize != OPEN_ATTRIBUTE_ENTRY {
            return Err(refuse(format!(
                "open attribute entries of {} bytes, and replay has been checked only on {}",
                u16_at(&t, 0)?,
                OPEN_ATTRIBUTE_ENTRY
            )));
        }
        for (at, e) in entries(&t)? {
            open.insert(
                at as u16,
                (u32_at(&e, 0x08)?, u64_at(&e, 0x10)? & 0xFFFF_FFFF_FFFF),
            );
        }
    }
    // (target attribute, VCN) -> (oldest LSN, LCN).
    let mut dirty: HashMap<(u16, u64), (u64, Option<u64>)> = HashMap::new();
    if let Some(t) = table(u64_at(&body, 0x20)?, 0x1F)? {
        for (_, e) in entries(&t)? {
            if u32_at(&e, 0x0C)? != 1 {
                return Err(refuse("a dirty page spanning more than one cluster"));
            }
            dirty.insert(
                (u32_at(&e, 0x04)? as u16, u64_at(&e, 0x10)?),
                (u64_at(&e, 0x18)?, Some(u64_at(&e, 0x20)?)),
            );
        }
    }
    if let Some(t) = table(u64_at(&body, 0x28)?, TRANSACTION_TABLE_DUMP)? {
        if !entries(&t)?.is_empty() {
            return Err(refuse(
                "the checkpoint lists transactions still open, which would need undo",
            ));
        }
    }

    // ---- every record from the oldest dirty page to the end --------------
    let redo_start = dirty
        .values()
        .map(|d| d.0)
        .min()
        .unwrap_or(start)
        .min(start);
    let mut records = Vec::new();
    let mut lsn = redo_start;
    loop {
        let (header, data, next) = l.record(lsn)?;
        records.push((lsn, header, data));
        if lsn >= l.newest {
            break;
        }
        if next <= lsn {
            return Err(refuse(format!(
                "the record after LSN {lsn:#x} goes backwards"
            )));
        }
        lsn = next;
    }
    if lsn != l.newest {
        return Err(refuse(format!(
            "the records stop at LSN {lsn:#x}, not at the last one, {:#x}",
            l.newest
        )));
    }

    // ---- analysis, from the checkpoint's start ---------------------------
    let mut open_transactions: BTreeMap<u32, bool> = BTreeMap::new();
    for (lsn, header, data) in &records {
        if *lsn < start || u32_at(header, 0x20)? != LFS_CLIENT_RECORD {
            continue;
        }
        let (redo, undo) = (u16_at(data, 0)?, u16_at(data, 2)?);
        let transaction = u32_at(header, 0x24)?;
        if redo == FORGET_TRANSACTION {
            open_transactions.remove(&transaction);
        } else {
            *open_transactions.entry(transaction).or_default() |=
                !matches!(undo, NOOP | COMPENSATION);
        }
        if redo == OPEN_NONRESIDENT_ATTRIBUTE {
            let e = redo_data(header, data)?;
            if e.len() < OPEN_ATTRIBUTE_ENTRY {
                return Err(refuse(format!(
                    "a short open attribute entry at LSN {lsn:#x}"
                )));
            }
            open.insert(
                u16_at(data, 0x0C)?,
                (u32_at(&e, 0x08)?, u64_at(&e, 0x10)? & 0xFFFF_FFFF_FFFF),
            );
        } else if u16_at(data, 0x0E)? > 0 {
            dirty
                .entry((u16_at(data, 0x0C)?, u64_at(data, 0x18)?))
                .or_insert((*lsn, None));
        }
    }
    if let Some((t, _)) = open_transactions.iter().find(|(_, undo)| **undo) {
        return Err(refuse(format!(
            "transaction {t} did not finish before the log stopped, and undoing it is not \
             implemented"
        )));
    }

    // ---- redo ------------------------------------------------------------
    let cluster = params.cluster_size as usize;
    let bps = params.bytes_per_sector;
    let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
    let mut raw: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut applied = 0u64;
    for (lsn, header, data) in &records {
        let lsn = *lsn;
        if u32_at(header, 0x20)? != LFS_CLIENT_RECORD {
            continue;
        }
        let op = u16_at(data, 0)?;
        if matches!(
            op,
            NOOP | COMPENSATION
                | END_TOP_LEVEL_ACTION
                | PREPARE_TRANSACTION
                | COMMIT_TRANSACTION
                | FORGET_TRANSACTION
                | OPEN_NONRESIDENT_ATTRIBUTE
        ) || (OPEN_ATTRIBUTE_TABLE_DUMP..=TRANSACTION_TABLE_DUMP).contains(&op)
        {
            continue;
        }
        let supported = ON_FILE_RECORD.contains(&op)
            || ON_INDEX_BLOCK.contains(&op)
            || matches!(
                op,
                UPDATE_NONRESIDENT_VALUE
                    | SET_BITS_IN_NONRESIDENT_BITMAP
                    | CLEAR_BITS_IN_NONRESIDENT_BITMAP
            );
        if !supported {
            return Err(refuse(format!(
                "LSN {lsn:#x} holds a {} ({op:#x}) redo, which replay does not perform yet",
                op_name(op)
            )));
        }
        let target = u16_at(data, 0x0C)?;
        let vcn = u64_at(data, 0x18)?;
        let Some(&(oldest, dirty_lcn)) = dirty.get(&(target, vcn)) else {
            continue;
        };
        if lsn < oldest {
            continue;
        }
        if u16_at(data, 0x0E)? != 1 {
            return Err(refuse(format!(
                "LSN {lsn:#x} names {} clusters for one page, and replay has been checked \
                 only on one",
                u16_at(data, 0x0E)?
            )));
        }
        let lcn = u64_at(data, 0x20)?;
        if dirty_lcn.is_some_and(|d| d != lcn) {
            return Err(refuse(format!(
                "LSN {lsn:#x} names LCN {lcn:#x} for a page the checkpoint has at another"
            )));
        }
        let &(attr_type, file) = open.get(&target).ok_or_else(|| {
            refuse(format!(
                "LSN {lsn:#x} names attribute {target:#x}, which is not open"
            ))
        })?;
        let in_cluster = u16_at(data, 0x14)? as usize * 512;
        let (record_off, attr_off) = (u16_at(data, 0x10)? as usize, u16_at(data, 0x12)? as usize);
        let redo = redo_data(header, data)?;
        let base = lcn
            .checked_mul(params.cluster_size)
            .and_then(|b| b.checked_add(in_cluster as u64))
            .ok_or_else(|| refuse(format!("LSN {lsn:#x} names LCN {lcn:#x}")))?;

        if op == SET_BITS_IN_NONRESIDENT_BITMAP
            || op == CLEAR_BITS_IN_NONRESIDENT_BITMAP
            || (op == UPDATE_NONRESIDENT_VALUE && attr_type != 0xA0)
        {
            if (attr_type, file) == (0x80, 0) || attr_type == 0xA0 {
                return Err(refuse(format!(
                    "LSN {lsn:#x} writes raw bytes into an MFT record or index block"
                )));
            }
            let page = match raw.entry(lcn) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => {
                    let mut buf = vec![0u8; cluster];
                    read(lcn * params.cluster_size, &mut buf)?;
                    e.insert(buf)
                }
            };
            let page = &mut page[in_cluster.min(cluster)..];
            if op == UPDATE_NONRESIDENT_VALUE {
                put(page, record_off + attr_off, &redo)?;
            } else {
                let (first, count) = (u32_at(&redo, 0)? as usize, u32_at(&redo, 4)? as usize);
                if (first + count).div_ceil(8) > page.len() {
                    return Err(refuse(format!(
                        "LSN {lsn:#x} sets bits past its page ({first}+{count})"
                    )));
                }
                for bit in first..first + count {
                    if op == SET_BITS_IN_NONRESIDENT_BITMAP {
                        page[bit / 8] |= 1 << (bit % 8);
                    } else {
                        page[bit / 8] &= !(1 << (bit % 8));
                    }
                }
            }
            applied += 1;
            continue;
        }

        // An MFT record or an index block: read once, LSN-checked.
        let (magic, size): (&'static [u8; 4], usize) = if ON_FILE_RECORD.contains(&op) {
            if (attr_type, file) != (0x80, 0) {
                return Err(refuse(format!(
                    "LSN {lsn:#x}'s {} names attribute {attr_type:#x} of record {file}, not \
                     $MFT's data",
                    op_name(op)
                )));
            }
            (b"FILE", params.file_record_size as usize)
        } else {
            if attr_type != 0xA0 {
                return Err(refuse(format!(
                    "LSN {lsn:#x}'s {} names attribute {attr_type:#x}, not an index allocation",
                    op_name(op)
                )));
            }
            (b"INDX", params.index_block_size as usize)
        };
        if u16_at(data, 0x16)? as usize * 512 != size || in_cluster + size > cluster {
            return Err(refuse(format!(
                "LSN {lsn:#x} names a {}-byte block at {in_cluster:#x} in a {cluster}-byte \
                 cluster, where this volume's blocks are {size} bytes",
                u16_at(data, 0x16)? as usize * 512
            )));
        }
        if raw.contains_key(&lcn) {
            return Err(refuse(format!(
                "LCN {lcn:#x} is written both as raw bytes and as {}",
                std::str::from_utf8(magic).unwrap_or("?")
            )));
        }
        let block = match blocks.entry(base) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                let mut bytes = vec![0u8; size];
                read(base, &mut bytes)?;
                let mut fixed = bytes.clone();
                let valid = apply_fixup_on_read_magic(&mut fixed, bps, magic).is_ok();
                let record = (magic == b"FILE").then(|| {
                    (vcn * params.cluster_size + in_cluster as u64) / params.file_record_size
                });
                e.insert(Block {
                    magic,
                    bytes: if valid { fixed } else { bytes },
                    valid,
                    changed: false,
                    record,
                })
            }
        };
        let initialises = (op == INITIALIZE_FILE_RECORD && redo.get(..4) == Some(b"FILE"))
            || (op == UPDATE_NONRESIDENT_VALUE
                && record_off + attr_off == 0
                && redo.get(..4) == Some(b"INDX"));
        if block.valid {
            if u64_at(&block.bytes, 0x08)? >= lsn {
                continue;
            }
        } else if !initialises {
            return Err(refuse(format!(
                "the {} block at byte {base:#x} does not read as one (torn, or never written), \
                 and LSN {lsn:#x}'s {} does not rewrite it whole",
                std::str::from_utf8(magic).unwrap_or("?"),
                op_name(op)
            )));
        }
        if magic == b"FILE" {
            redo_file_record(&mut block.bytes, op, record_off, attr_off, &redo)
        } else {
            redo_index_block(&mut block.bytes, op, record_off + attr_off, &redo)
        }
        .map_err(|e| e.context(format!("LSN {lsn:#x}, {}", op_name(op))))?;
        put_u64(&mut block.bytes, 0x08, lsn)?;
        block.valid = true;
        block.changed = true;
        applied += 1;
    }

    // ---- the writes ------------------------------------------------------
    let mut plan = Plan {
        records: records.len() as u64,
        applied,
        checkpoint,
        last_lsn: l.newest,
        ..Plan::default()
    };
    for (at, mut block) in blocks {
        if !block.changed {
            continue;
        }
        apply_fixup_on_write_magic(&mut block.bytes, bps, block.magic)
            .map_err(|e| refuse(format!("the block at byte {at:#x}: {e}")))?;
        if let Some(n) = block.record {
            plan.mft_records.push((n, at));
        }
        plan.writes.insert(at, block.bytes);
    }
    for (lcn, bytes) in raw {
        plan.writes.insert(lcn * params.cluster_size, bytes);
    }
    Ok(plan)
}

/// The redo data of a client record, with any zeros the log left out.
fn redo_data(header: &[u8], data: &[u8]) -> Result<Vec<u8>, Error> {
    let (off, len) = (u16_at(data, 0x04)? as usize, u16_at(data, 0x06)? as usize);
    let have = data.get(off..).map_or(&[][..], |d| &d[..d.len().min(len)]);
    if have.len() < len {
        let op = u16_at(data, 0)?;
        let zeros = u16_at(header, 0x28)? & RECORD_REDO_ZEROS != 0
            && matches!(op, UPDATE_RESIDENT_VALUE | ZERO_END_OF_FILE_RECORD);
        if !zeros {
            return Err(refuse(format!(
                "a {} record holds {} of its {len} bytes of redo data",
                op_name(op),
                have.len()
            )));
        }
    }
    let mut out = have.to_vec();
    out.resize(len, 0);
    Ok(out)
}

/// Redo one operation on an MFT record (fixups undone). `a` is the
/// attribute's offset in the record, `off` the offset within it.
fn redo_file_record(
    rec: &mut [u8],
    op: u16,
    a: usize,
    off: usize,
    data: &[u8],
) -> Result<(), Error> {
    let used = u32_at(rec, 0x18)? as usize;
    if used > rec.len() && op != INITIALIZE_FILE_RECORD {
        return Err(refuse(format!("a record claiming {used:#x} bytes in use")));
    }
    let set_used = |rec: &mut [u8], v: usize| put_u32(rec, 0x18, v as u32);
    // Resize the attribute at `a` from its length to `new_len`, moving
    // everything after it.
    let resize = |rec: &mut [u8], used: usize, new_len: usize| -> Result<usize, Error> {
        let len = u32_at(rec, a + 4)? as usize;
        if new_len > len {
            insert(rec, a + len, used, &vec![0u8; new_len - len])?;
        } else if new_len < len {
            remove(rec, a + new_len, len - new_len, used)?;
        }
        put_u32(rec, a + 4, new_len as u32)?;
        Ok(used + new_len - len)
    };
    match op {
        INITIALIZE_FILE_RECORD => {
            rec.fill(0);
            put(rec, 0, data)?;
        }
        DEALLOCATE_FILE_RECORD => {
            // Out of use, and a new sequence number, so a reference to the
            // file that was here no longer matches it; 0 is skipped.
            let flags = u16_at(rec, 0x16)?;
            put(rec, 0x16, &(flags & !1).to_le_bytes())?;
            let sequence = match u16_at(rec, 0x10)?.wrapping_add(1) {
                0 => 1,
                s => s,
            };
            put(rec, 0x10, &sequence.to_le_bytes())?;
        }
        CREATE_ATTRIBUTE => {
            insert(rec, a, used, data)?;
            set_used(rec, used + data.len())?;
            let instance = u16_at(rec, a + 0x0E)?;
            if instance >= u16_at(rec, 0x28)? {
                put(rec, 0x28, &(instance + 1).to_le_bytes())?;
            }
        }
        DELETE_ATTRIBUTE => {
            let len = u32_at(rec, a + 4)? as usize;
            remove(rec, a, len, used)?;
            set_used(rec, used - len)?;
        }
        UPDATE_RESIDENT_VALUE => {
            let (vo, vl) = (
                u16_at(rec, a + 0x14)? as usize,
                u32_at(rec, a + 0x10)? as usize,
            );
            let end = off + data.len();
            if end > vo + vl {
                let used = resize(rec, used, end.div_ceil(8) * 8)?;
                put_u32(rec, a + 0x10, (end - vo) as u32)?;
                set_used(rec, used)?;
            }
            put(rec, a + off, data)?;
        }
        UPDATE_MAPPING_PAIRS => {
            let used = resize(rec, used, (off + data.len()).div_ceil(8) * 8)?;
            set_used(rec, used)?;
            put(rec, a + off, data)?;
            // The highest VCN follows from the runs.
            let end = a + u32_at(rec, a + 4)? as usize;
            let mut at = a + u16_at(rec, a + 0x20)? as usize;
            let mut clusters = 0u64;
            while at < end && rec[at] != 0 {
                let (ln, on) = ((rec[at] & 15) as usize, (rec[at] >> 4) as usize);
                let bytes = rec
                    .get(at + 1..at + 1 + ln)
                    .ok_or_else(|| refuse("mapping pairs past their attribute"))?;
                clusters += bytes
                    .iter()
                    .rev()
                    .fold(0u64, |v, b| (v << 8) | u64::from(*b));
                at += 1 + ln + on;
            }
            let lowest = u64_at(rec, a + 0x10)?;
            put_u64(rec, a + 0x18, (lowest + clusters).wrapping_sub(1))?;
        }
        SET_NEW_ATTRIBUTE_SIZES => {
            if data.len() > 24 && (u16_at(rec, a + 0x20)? as usize) < 0x48 {
                return Err(refuse(
                    "a total-allocated size for an attribute without one",
                ));
            }
            put_u64(rec, a + 0x28, u64_at(data, 0)?)?;
            put_u64(rec, a + 0x38, u64_at(data, 8)?)?;
            put_u64(rec, a + 0x30, u64_at(data, 16)?)?;
            if data.len() > 24 {
                put_u64(rec, a + 0x40, u64_at(data, 24)?)?;
            }
        }
        ADD_INDEX_ENTRY_ROOT | DELETE_INDEX_ENTRY_ROOT => {
            let header = a + u16_at(rec, a + 0x14)? as usize + 0x10;
            let delta = if op == ADD_INDEX_ENTRY_ROOT {
                insert(rec, a + off, used, data)?;
                data.len() as i64
            } else {
                let len = u16_at(rec, a + off + 8)? as usize;
                remove(rec, a + off, len, used)?;
                -(len as i64)
            };
            for field in [a + 4, a + 0x10, header + 4, header + 8] {
                let v = u32_at(rec, field)? as i64 + delta;
                put_u32(rec, field, v as u32)?;
            }
            set_used(rec, (used as i64 + delta) as usize)?;
        }
        SET_INDEX_ENTRY_VCN_ROOT => {
            let entry_len = u16_at(rec, a + off + 8)? as usize;
            put(rec, a + off + entry_len - 8, data)?;
        }
        UPDATE_FILE_NAME_ROOT => put(rec, a + off + 0x18, data)?,
        ZERO_END_OF_FILE_RECORD => put(rec, a + off, data)?,
        _ => unreachable!("only file-record operations reach here"),
    }
    Ok(())
}

/// Redo one operation on an index block (fixups undone), at byte `pos`.
fn redo_index_block(b: &mut [u8], op: u16, pos: usize, data: &[u8]) -> Result<(), Error> {
    const HEADER: usize = 0x18;
    if op == UPDATE_NONRESIDENT_VALUE {
        return put(b, pos, data);
    }
    let total = u32_at(b, HEADER + 4)? as usize;
    let used = HEADER + total;
    match op {
        ADD_INDEX_ENTRY_ALLOCATION => {
            insert(b, pos, used, data)?;
            put_u32(b, HEADER + 4, (total + data.len()) as u32)?;
        }
        DELETE_INDEX_ENTRY_ALLOCATION => {
            let len = u16_at(b, pos + 8)? as usize;
            // Windows moves the entries after it down and leaves the bytes
            // past the new end as they were, and the block's update
            // sequence array records them, so they are not zeroed here.
            shift_down(b, pos, len, used)?;
            put_u32(b, HEADER + 4, (total - len) as u32)?;
        }
        WRITE_END_OF_INDEX_BUFFER => {
            put(b, pos, data)?;
            put_u32(b, HEADER + 4, (pos + data.len() - HEADER) as u32)?;
        }
        SET_INDEX_ENTRY_VCN_ALLOCATION => {
            // The child VCN is the entry's last 8 bytes, as in the root.
            let entry_len = u16_at(b, pos + 8)? as usize;
            if entry_len < 0x18 || data.len() != 8 {
                return Err(refuse(format!(
                    "a child VCN of {} bytes for a {entry_len}-byte index entry",
                    data.len()
                )));
            }
            put(b, pos + entry_len - 8, data)?;
        }
        UPDATE_FILE_NAME_ALLOCATION => put(b, pos + 0x18, data)?,
        _ => unreachable!("only index-block operations reach here"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(flags: u16, op: u16, redo: &[u8], declared: u16) -> ([u8; RECORD_HEADER], Vec<u8>) {
        let mut header = [0u8; RECORD_HEADER];
        header[0x28..0x2A].copy_from_slice(&flags.to_le_bytes());
        let mut data = vec![0u8; 0x28];
        data[0..2].copy_from_slice(&op.to_le_bytes());
        data[4..6].copy_from_slice(&0x28u16.to_le_bytes());
        data[6..8].copy_from_slice(&declared.to_le_bytes());
        data.extend_from_slice(redo);
        (header, data)
    }

    #[test]
    fn left_out_redo_bytes_are_zeros_only_where_the_log_says_so() {
        // Windows' logs leave a resident file's data out of the record and
        // flag it 0x4; the value is recovered as zeros, as Windows does.
        let (h, d) = record(0x6, UPDATE_RESIDENT_VALUE, &[], 700);
        assert_eq!(redo_data(&h, &d).unwrap(), vec![0u8; 700]);
        // Without the flag, a short record is a damaged one.
        let (h, d) = record(0x2, UPDATE_RESIDENT_VALUE, &[1, 2], 700);
        assert!(redo_data(&h, &d).is_err());
        // And only for the operations it was seen on.
        let (h, d) = record(0x6, CREATE_ATTRIBUTE, &[1, 2], 700);
        assert!(redo_data(&h, &d).is_err());
        // A whole record is taken as it is.
        let (h, d) = record(0x0, CREATE_ATTRIBUTE, &[1, 2, 3], 3);
        assert_eq!(redo_data(&h, &d).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn a_change_that_does_not_fit_its_block_is_refused_not_clamped() {
        let mut b = vec![0u8; 16];
        assert!(insert(&mut b, 4, 12, &[9; 8]).is_err());
        assert!(remove(&mut b, 10, 8, 12).is_err());
        assert!(put(&mut b, 12, &[1; 8]).is_err());
        insert(&mut b, 4, 8, &[9; 4]).unwrap();
        assert_eq!(&b[..12], &[0, 0, 0, 0, 9, 9, 9, 9, 0, 0, 0, 0]);
        remove(&mut b, 4, 4, 12).unwrap();
        assert_eq!(b, vec![0u8; 16]);
    }

    #[test]
    fn a_log_that_is_not_lfs_is_refused() {
        let params = BootParams {
            bytes_per_sector: 512,
            sectors_per_cluster: 8,
            cluster_size: 4096,
            mft_lcn: 4,
            file_record_size: 1024,
            total_sectors: 0,
            serial_number: 0,
            index_block_size: 4096,
            oem_id: *b"NTFS    ",
        };
        let mut read = |_: u64, _: &mut [u8]| -> Result<(), Error> {
            panic!("nothing on the volume is read for a log that does not parse")
        };
        let err = plan(&vec![0u8; 65536], &params, &mut read).unwrap_err();
        assert!(err.contains("$LogFile"), "{err}");
    }
}
