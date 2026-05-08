//! Bridge between ntfs's local [`BlockIo`] trait and the shared
//! [`fs_core::BlockDevice`] trait.
//!
//! Direction supported here: **inbound** — wrap any
//! `fs_core::BlockDevice` (a `Qcow2Reader`, a `SliceReader` produced by
//! `am-partitions`, an `FsCoreFromSomethingElse`) and present it as
//! ntfs's local `BlockIo`. This is the path that lets ntfs mount a
//! partition that lives inside a virtual disk image.
//!
//! The outbound direction (ntfs's `PathIo` / `CallbackBlockIo` exposed
//! as `fs_core::BlockDevice`) is intentionally not provided yet —
//! `PathIo`'s `&mut self` methods would require a wrapper struct + Mutex
//! and there's no concrete consumer asking for it.
//!
//! Strictly additive — does not touch the existing `BlockIo` trait or
//! its implementors. Removing `pub mod fs_core_bridge;` from `lib.rs`
//! reverts the entire change.

use crate::block_io::BlockIo;

/// Wraps any [`fs_core::BlockDevice`] and presents it as ntfs's local
/// [`BlockIo`]. ntfs's trait uses `&mut self` and `Result<(), String>`;
/// we satisfy both: the `&mut` is purely a signature concern (the
/// wrapper has no per-call mutable state), and `fs_core::Error` is
/// flattened to its `Display` string.
pub struct CoreDevice<T: fs_core::BlockDevice> {
    inner: T,
}

impl<T: fs_core::BlockDevice> CoreDevice<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Borrow the wrapped device.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Consume the wrapper and return the inner device.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: fs_core::BlockDevice> BlockIo for CoreDevice<T> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        fs_core::BlockRead::read_at(&self.inner, offset, buf).map_err(|e| e.to_string())
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), String> {
        fs_core::BlockDevice::write_at(&self.inner, offset, buf).map_err(|e| e.to_string())
    }

    fn size(&self) -> u64 {
        fs_core::BlockRead::size_bytes(&self.inner)
    }

    fn sync(&mut self) -> Result<(), String> {
        fs_core::BlockDevice::flush(&self.inner).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Trivial in-memory `fs_core::BlockDevice` for testing the inbound
    /// adapter without dragging in a real qcow2 fixture.
    struct InMemoryFsCore(Mutex<Vec<u8>>);

    impl fs_core::BlockRead for InMemoryFsCore {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let b = self.0.lock().unwrap();
            let start = offset as usize;
            let end = start + buf.len();
            if end > b.len() {
                return Err(fs_core::Error::ShortRead {
                    offset,
                    want: buf.len(),
                    got: b.len().saturating_sub(start),
                });
            }
            buf.copy_from_slice(&b[start..end]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.lock().unwrap().len() as u64
        }
    }

    impl fs_core::BlockDevice for InMemoryFsCore {
        fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
            let mut b = self.0.lock().unwrap();
            let start = offset as usize;
            let end = start + buf.len();
            b[start..end].copy_from_slice(buf);
            Ok(())
        }
        fn is_writable(&self) -> bool {
            true
        }
    }

    #[test]
    fn core_device_round_trip() {
        let mem = InMemoryFsCore(Mutex::new(vec![0u8; 4096]));
        let mut dev = CoreDevice::new(mem);
        assert_eq!(BlockIo::size(&dev), 4096);

        BlockIo::write_all_at(&mut dev, 100, &[0x11, 0x22, 0x33, 0x44]).unwrap();
        let mut buf = [0u8; 4];
        BlockIo::read_exact_at(&mut dev, 100, &mut buf).unwrap();
        assert_eq!(buf, [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn core_device_propagates_short_read_as_string() {
        let mem = InMemoryFsCore(Mutex::new(vec![0u8; 64]));
        let mut dev = CoreDevice::new(mem);
        let mut buf = [0u8; 16];
        let err = BlockIo::read_exact_at(&mut dev, 60, &mut buf).unwrap_err();
        assert!(err.contains("short read"), "got: {err}");
    }
}
