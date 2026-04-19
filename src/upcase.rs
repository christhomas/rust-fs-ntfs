//! NTFS `$UpCase` table loader + NTFS-style UTF-16 collation.
//!
//! `$UpCase` lives at MFT record 10 and contains a simple 128 KiB array
//! of 65536 `u16` values: `upcase[c]` is the uppercase form of BMP
//! code point `c`. NTFS uses this table for case-insensitive compare
//! in B+ tree indexes (COLLATION_FILE_NAME).
//!
//! Historically `index_io` used an ASCII-only case-folder for
//! insertion sort. That was wrong for non-ASCII filenames — our sort
//! order and Windows' upcase order diverged, so Windows' binary search
//! missed entries we inserted. This module restores correct collation.
//!
//! References (no GPL code consulted):
//! * [Flatcap $UpCase](https://flatcap.github.io/linux-ntfs/ntfs/files/upcase.html)
//! * MS-FSCC (collation rules)

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType, NtfsReadSeek};

const UPCASE_LEN: usize = 65536;

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
        let ntfs = Ntfs::new(&mut reader).map_err(|e| format!("parse ntfs: {e}"))?;
        let upcase_file = ntfs
            .file(&mut reader, KnownNtfsFileRecordNumber::UpCase as u64)
            .map_err(|e| format!("open $UpCase: {e}"))?;

        let mut attrs = upcase_file.attributes();
        while let Some(item) = attrs.next(&mut reader) {
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
                .value(&mut reader)
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
                    .read(&mut reader, &mut bytes[filled..])
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
