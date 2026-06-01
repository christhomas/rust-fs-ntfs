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

use std::path::Path;

use crate::attr_io::AttrType;
use crate::block_io::{BlockIo, PathIo};

/// MFT record number of `$UpCase` (fixed by the NTFS spec).
const UPCASE_RECORD_NUMBER: u64 = 10;

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
    /// Load `$UpCase` from the volume (record 10's unnamed `$DATA`) using the
    /// native read layer — no upstream `ntfs` crate.
    pub fn load(image: &Path) -> Result<Self, String> {
        let mut io = PathIo::open_ro(image)?;
        Self::load_io(&mut io)
    }

    /// Load `$UpCase` over a [`BlockIo`]. The mutator stack uses this when
    /// building a sorted index entry over a callback-mounted volume.
    ///
    /// Reads `$UpCase`'s unnamed `$DATA` by its fixed record number, so this
    /// needs no name collation (and thus no upcase table) — safe even though
    /// collation itself depends on this table.
    pub fn load_io<T: BlockIo + ?Sized>(io: &mut T) -> Result<Self, String> {
        let bytes =
            crate::read::read_attribute_value(io, UPCASE_RECORD_NUMBER, AttrType::Data, None)
                .map_err(|e| format!("read $UpCase: {e}"))?;
        if bytes.len() < UPCASE_BYTES {
            return Err(format!(
                "$UpCase value length {} < expected {UPCASE_BYTES}",
                bytes.len()
            ));
        }
        let table = bytes[..UPCASE_BYTES]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(Self { table })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn canonical_table() -> UpcaseTable {
        let table: Vec<u16> = CANONICAL_UPCASE
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        UpcaseTable { table }
    }

    // --- generate_upcase_table ---

    #[test]
    fn generate_upcase_table_is_128_kib() {
        assert_eq!(generate_upcase_table().len(), 65536 * 2);
    }

    #[test]
    fn generate_upcase_table_is_deterministic() {
        assert_eq!(generate_upcase_table(), generate_upcase_table());
    }

    #[test]
    fn generate_upcase_table_uppercase_ascii_identity() {
        let t = generate_upcase_table();
        // 'A' at index 0x41 → upcase[0x41] should be 'A' (0x41)
        let entry = u16::from_le_bytes([t[0x41 * 2], t[0x41 * 2 + 1]]);
        assert_eq!(entry, 0x41);
    }

    #[test]
    fn generate_upcase_table_lowercase_maps_to_uppercase() {
        let t = generate_upcase_table();
        // 'a' (0x61) → 'A' (0x41)
        let a = u16::from_le_bytes([t[0x61 * 2], t[0x61 * 2 + 1]]);
        assert_eq!(a, 0x41);
        // 'z' (0x7a) → 'Z' (0x5a)
        let z = u16::from_le_bytes([t[0x7a * 2], t[0x7a * 2 + 1]]);
        assert_eq!(z, 0x5a);
    }

    // --- UpcaseTable::upcase ---

    #[test]
    fn upcase_uppercase_ascii_unchanged() {
        let t = canonical_table();
        assert_eq!(t.upcase(b'A' as u16), b'A' as u16);
        assert_eq!(t.upcase(b'Z' as u16), b'Z' as u16);
    }

    #[test]
    fn upcase_lowercase_ascii_converts() {
        let t = canonical_table();
        assert_eq!(t.upcase(b'a' as u16), b'A' as u16);
        assert_eq!(t.upcase(b'z' as u16), b'Z' as u16);
        assert_eq!(t.upcase(b'm' as u16), b'M' as u16);
    }

    #[test]
    fn upcase_digits_and_punctuation_unchanged() {
        let t = canonical_table();
        assert_eq!(t.upcase(b'0' as u16), b'0' as u16);
        assert_eq!(t.upcase(b'.' as u16), b'.' as u16);
        assert_eq!(t.upcase(b'_' as u16), b'_' as u16);
    }

    // --- UpcaseTable::cmp_names ---

    #[test]
    fn cmp_names_equal_ascii() {
        let t = canonical_table();
        assert_eq!(
            t.cmp_names(&utf16("foo"), &utf16("foo")),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn cmp_names_case_insensitive() {
        let t = canonical_table();
        assert_eq!(
            t.cmp_names(&utf16("FOO"), &utf16("foo")),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            t.cmp_names(&utf16("Hello"), &utf16("HELLO")),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn cmp_names_ordering() {
        let t = canonical_table();
        assert_eq!(
            t.cmp_names(&utf16("abc"), &utf16("abd")),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            t.cmp_names(&utf16("b"), &utf16("a")),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn cmp_names_prefix_shorter_is_less() {
        let t = canonical_table();
        assert_eq!(
            t.cmp_names(&utf16("ab"), &utf16("abc")),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            t.cmp_names(&utf16("abc"), &utf16("ab")),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn cmp_names_empty_slices() {
        let t = canonical_table();
        assert_eq!(t.cmp_names(&[], &[]), std::cmp::Ordering::Equal);
        assert_eq!(t.cmp_names(&[], &utf16("a")), std::cmp::Ordering::Less);
        assert_eq!(t.cmp_names(&utf16("a"), &[]), std::cmp::Ordering::Greater);
    }

    // --- load_io (via in-memory formatted filesystem) ----------------------

    struct MemDev {
        buf: Vec<u8>,
    }

    impl crate::block_io::BlockIo for MemDev {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            let off = offset as usize;
            buf.copy_from_slice(&self.buf[off..off + buf.len()]);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
            let off = offset as usize;
            self.buf[off..off + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn size(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    fn formatted_dev() -> MemDev {
        const SIZE: u64 = 64 * 1024 * 1024;
        let mut dev = MemDev {
            buf: vec![0u8; SIZE as usize],
        };
        crate::mkfs::format_filesystem(
            &mut dev as &mut dyn crate::block_io::BlockIo,
            SIZE,
            4096,
            4096,
            None,
            None,
        )
        .expect("format_filesystem");
        dev
    }

    #[test]
    fn load_io_succeeds_on_formatted_volume() {
        let mut dev = formatted_dev();
        let table = UpcaseTable::load_io(&mut dev).unwrap();
        // Spot-check: 'a' → 'A' and 'Z' → 'Z'.
        assert_eq!(table.upcase(b'a' as u16), b'A' as u16);
        assert_eq!(table.upcase(b'Z' as u16), b'Z' as u16);
    }

    #[test]
    fn load_io_table_matches_embedded_canonical_spot_check() {
        let mut dev = formatted_dev();
        let table = UpcaseTable::load_io(&mut dev).unwrap();
        let expected = generate_upcase_table();
        // Spot-check a spread of code points rather than all 65536
        // (full loop is slow under coverage instrumentation).
        for &i in &[0u16, 0x61, 0x7a, 0xE9, 0x100, 0x3B1, 0x4000, 0x7FFF, 0xFFFF] {
            let exp = u16::from_le_bytes([expected[i as usize * 2], expected[i as usize * 2 + 1]]);
            assert_eq!(table.upcase(i), exp, "mismatch at code point {i:#06x}");
        }
    }

    #[test]
    fn load_io_cmp_names_case_insensitive_roundtrip() {
        let mut dev = formatted_dev();
        let table = UpcaseTable::load_io(&mut dev).unwrap();
        assert_eq!(
            table.cmp_names(&utf16("Hello"), &utf16("HELLO")),
            std::cmp::Ordering::Equal
        );
    }
}
