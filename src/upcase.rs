//! NTFS `$UpCase` table loader + NTFS-style UTF-16 collation.
//!
//! `$UpCase` lives at MFT record 10 and contains a simple 128 KiB array
//! of 65536 `u16` values: `upcase[c]` is the uppercase form of BMP
//! code point `c`. NTFS uses this table for case-insensitive compare
//! in B+ tree indexes (COLLATION_FILE_NAME).
//!
//! The on-disk table must match Microsoft's canonical NT 3.x mapping
//! byte-for-byte. Earlier versions of this module synthesised the
//! table from `char::to_uppercase()` (modern Unicode), which differed
//! from Microsoft's table at 327 BMP code points (e.g. U+00B5
//! MICRO SIGN — modern Unicode uppercases to U+039C GREEK CAPITAL
//! LETTER MU; NTFS preserves it as U+00B5). chkdsk reports
//! `Read-only chkdsk found bad on-disk uppercase table — using
//! system table` when our table doesn't match its own — and uses its
//! built-in table for the rest of the run, which causes filename
//! collation to disagree with what we wrote on disk.
//!
//! The canonical 128 KiB is captured byte-for-byte from a Microsoft
//! `format.com /FS:NTFS` reference volume — see
//! `src/upcase-canonical.bin` (SHA256
//! 41c26bc7a12bdaeb26025c93118697c7e3ef81ee048b00fe5cce2a472e0e0742)
//! and the iter16 entry in `docs/chkdsk-findings.md` for the
//! extraction recipe.
//!
//! No GPL/LGPL code or NTFS-on-Linux table was consulted — the bytes
//! are Microsoft's own output, captured via `fsutil file queryextents`
//! + raw volume read on a clean format.com-formatted VHDX.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType, NtfsReadSeek};

use crate::block_io::{BlockIo, IoReadSeek};

const UPCASE_LEN: usize = 65536;
const UPCASE_BYTES: usize = UPCASE_LEN * 2; // 131072

/// Microsoft-canonical NT 3.x `$UpCase` table (128 KiB, 65536 LE u16).
const CANONICAL_UPCASE: &[u8; UPCASE_BYTES] = include_bytes!("upcase-canonical.bin");

/// Return the on-disk `$UpCase` table that `mkfs_ntfs` writes into
/// MFT record 10's `$DATA`. Microsoft's canonical NT 3.x table,
/// captured verbatim from `format.com` reference output and embedded
/// at compile time. chkdsk verifies the table's bytes against its
/// built-in copy; mismatches surface as
/// `Read-only chkdsk found bad on-disk uppercase table - using
/// system table`.
pub fn generate_upcase_table() -> Vec<u8> {
    CANONICAL_UPCASE.to_vec()
}

pub struct UpcaseTable {
    table: Vec<u16>,
}

impl UpcaseTable {
    /// Load `$UpCase` from the volume. Reads through upstream's
    /// non-resident `$DATA` walker so we don't reinvent the run-list
    /// traversal.
    pub fn load(image: &Path) -> Result<Self, String> {
        let f = File::open(image).map_err(|e| format!("open image: {e}"))?;
        let mut reader = BufReader::new(f);
        Self::load_from_reader(&mut reader)
    }

    /// Load `$UpCase` over a [`BlockIo`]. The mutator stack uses this
    /// when building a sorted index entry over a callback-mounted volume.
    pub fn load_io<T: BlockIo + ?Sized>(io: &mut T) -> Result<Self, String> {
        let mut reader = IoReadSeek::new(io);
        Self::load_from_reader(&mut reader)
    }

    fn load_from_reader<R: std::io::Read + std::io::Seek>(reader: &mut R) -> Result<Self, String> {
        let ntfs = Ntfs::new(reader).map_err(|e| format!("parse ntfs: {e}"))?;
        let upcase_file = ntfs
            .file(reader, KnownNtfsFileRecordNumber::UpCase as u64)
            .map_err(|e| format!("open $UpCase: {e}"))?;

        let mut attrs = upcase_file.attributes();
        while let Some(item) = attrs.next(reader) {
            let item = item.map_err(|e| format!("$UpCase attr iter: {e}"))?;
            let attribute = item
                .to_attribute()
                .map_err(|e| format!("$UpCase to_attr: {e}"))?;
            if attribute.ty().ok() != Some(NtfsAttributeType::Data) {
                continue;
            }
            if !attribute.name().map(|n| n.is_empty()).unwrap_or(true) {
                continue;
            }
            let mut value = attribute
                .value(reader)
                .map_err(|e| format!("$UpCase value: {e}"))?;
            let total = attribute.value_length() as usize;
            if total < UPCASE_LEN * 2 {
                return Err(format!(
                    "$UpCase value length {total} < expected {}",
                    UPCASE_LEN * 2
                ));
            }
            let mut bytes = vec![0u8; UPCASE_LEN * 2];
            let mut filled = 0;
            while filled < bytes.len() {
                let n = value
                    .read(reader, &mut bytes[filled..])
                    .map_err(|e| format!("$UpCase read: {e}"))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled < bytes.len() {
                return Err(format!(
                    "$UpCase short read: got {filled}, expected {}",
                    bytes.len()
                ));
            }
            let mut table = Vec::with_capacity(UPCASE_LEN);
            for chunk in bytes.chunks_exact(2) {
                table.push(u16::from_le_bytes([chunk[0], chunk[1]]));
            }
            return Ok(Self { table });
        }
        Err("$UpCase has no unnamed $DATA".to_string())
    }

    /// Upcase a single UTF-16 code unit. Non-BMP units (surrogates)
    /// are left alone — NTFS only collates within BMP per
    /// COLLATION_FILE_NAME.
    pub fn upcase(&self, c: u16) -> u16 {
        self.table.get(c as usize).copied().unwrap_or(c)
    }

    /// Compare two UTF-16 name slices per NTFS COLLATION_FILE_NAME
    /// (case-insensitive upcase-table fold, then code-unit-wise
    /// comparison, shorter-prefix-loses on tie).
    pub fn cmp_names(&self, a: &[u16], b: &[u16]) -> std::cmp::Ordering {
        for (ac, bc) in a
            .iter()
            .copied()
            .map(|c| self.upcase(c))
            .zip(b.iter().copied().map(|c| self.upcase(c)))
        {
            match ac.cmp(&bc) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }
        a.len().cmp(&b.len())
    }
}
