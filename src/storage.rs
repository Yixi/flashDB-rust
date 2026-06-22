//! Storage backends and the [`Storage`] abstraction.
//!
//! FlashDB treats its backing medium as a single flat address space split into
//! equal-sized sectors. Programming may only clear bits (`1 -> 0`); erasing a
//! sector restores it to all-ones. The database engine only ever programs bit
//! patterns that are subsets of what is already there, so a backend may simply
//! overwrite bytes on `write` and fill with `0xFF` on `erase`.

use crate::error::{FdbError, Result};
use alloc::vec;
use alloc::vec::Vec;

/// A flat, sector-addressable storage medium.
///
/// All addresses are byte offsets from the start of the database region. This
/// is the Rust equivalent of FlashDB's `_fdb_flash_read/write/erase` layer.
pub trait Storage {
    /// Read `buf.len()` bytes starting at `addr` into `buf`.
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<()>;

    /// Program `buf` at `addr`. When `sync` is true the write must be flushed
    /// to the underlying medium before returning.
    fn write(&mut self, addr: u32, buf: &[u8], sync: bool) -> Result<()>;

    /// Erase `size` bytes starting at `addr`, restoring them to `0xFF`. `addr`
    /// and `size` are always sector-aligned.
    fn erase(&mut self, addr: u32, size: u32) -> Result<()>;
}

impl<S: Storage + ?Sized> Storage for &mut S {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<()> {
        (**self).read(addr, buf)
    }
    fn write(&mut self, addr: u32, buf: &[u8], sync: bool) -> Result<()> {
        (**self).write(addr, buf, sync)
    }
    fn erase(&mut self, addr: u32, size: u32) -> Result<()> {
        (**self).erase(addr, size)
    }
}

/// An in-memory storage backend backed by a single `Vec<u8>`.
///
/// Useful for tests and for bringing the database up on a new platform before a
/// real flash driver exists. The whole capacity starts out erased (`0xFF`).
#[derive(Clone)]
pub struct RamStorage {
    mem: Vec<u8>,
}

impl RamStorage {
    /// Create a backend with `capacity` bytes, all erased to `0xFF`.
    pub fn new(capacity: u32) -> Self {
        RamStorage {
            mem: vec![0xFF; capacity as usize],
        }
    }

    /// Borrow the raw backing bytes (handy for assertions in tests).
    pub fn as_bytes(&self) -> &[u8] {
        &self.mem
    }
}

impl Storage for RamStorage {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<()> {
        let start = addr as usize;
        let end = start.checked_add(buf.len()).ok_or(FdbError::ReadErr)?;
        if end > self.mem.len() {
            return Err(FdbError::ReadErr);
        }
        buf.copy_from_slice(&self.mem[start..end]);
        Ok(())
    }

    fn write(&mut self, addr: u32, buf: &[u8], _sync: bool) -> Result<()> {
        let start = addr as usize;
        let end = start.checked_add(buf.len()).ok_or(FdbError::WriteErr)?;
        if end > self.mem.len() {
            return Err(FdbError::WriteErr);
        }
        self.mem[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn erase(&mut self, addr: u32, size: u32) -> Result<()> {
        let start = addr as usize;
        let end = start.checked_add(size as usize).ok_or(FdbError::EraseErr)?;
        if end > self.mem.len() {
            return Err(FdbError::EraseErr);
        }
        for b in self.mem[start..end].iter_mut() {
            *b = 0xFF;
        }
        Ok(())
    }
}

#[cfg(feature = "std")]
pub use file::FileStorage;

#[cfg(feature = "std")]
mod file {
    use super::Storage;
    use crate::def::align_down;
    use crate::error::{FdbError, Result};
    use std::collections::HashMap;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;

    /// Maximum number of characters of the database name used in file names,
    /// matching `DB_NAME_MAX` in `fdb_file.c`.
    const DB_NAME_MAX: usize = 8;

    /// A file-backed storage backend mirroring FlashDB's `fdb_file.c`.
    ///
    /// Every sector is stored in its own file named `<name>.fdb.<index>` inside
    /// `dir`, where `index` is the sector number. The files persist across
    /// `Drop`, so re-opening a database (a "reboot") sees the previous state.
    pub struct FileStorage {
        dir: PathBuf,
        name: String,
        sec_size: u32,
        cache: HashMap<u32, File>,
    }

    impl FileStorage {
        /// Open (creating the directory if needed) a file-backed database called
        /// `name` under `dir`, with the given sector size.
        pub fn new(
            dir: impl Into<PathBuf>,
            name: impl Into<String>,
            sec_size: u32,
        ) -> Result<Self> {
            let dir = dir.into();
            std::fs::create_dir_all(&dir).map_err(|_| FdbError::InitFailed)?;
            Ok(FileStorage {
                dir,
                name: name.into(),
                sec_size,
                cache: HashMap::new(),
            })
        }

        fn sector_index(&self, addr: u32) -> u32 {
            align_down(addr, self.sec_size) / self.sec_size
        }

        fn file_path(&self, index: u32) -> PathBuf {
            let truncated: String = self.name.chars().take(DB_NAME_MAX).collect();
            self.dir.join(format!("{truncated}.fdb.{index}"))
        }

        /// Open an existing sector file for read/write (no create), caching the
        /// handle. Returns `None` when the file does not exist yet.
        fn open_rw(&mut self, index: u32) -> Option<&mut File> {
            if !self.cache.contains_key(&index) {
                let path = self.file_path(index);
                let file = OpenOptions::new().read(true).write(true).open(path).ok()?;
                self.cache.insert(index, file);
            }
            self.cache.get_mut(&index)
        }
    }

    impl Storage for FileStorage {
        fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<()> {
            let index = self.sector_index(addr);
            let off = (addr % self.sec_size) as u64;
            let file = self.open_rw(index).ok_or(FdbError::ReadErr)?;
            file.seek(SeekFrom::Start(off)).map_err(|_| FdbError::ReadErr)?;
            file.read_exact(buf).map_err(|_| FdbError::ReadErr)
        }

        fn write(&mut self, addr: u32, buf: &[u8], sync: bool) -> Result<()> {
            let index = self.sector_index(addr);
            let off = (addr % self.sec_size) as u64;
            let file = self.open_rw(index).ok_or(FdbError::WriteErr)?;
            file.seek(SeekFrom::Start(off)).map_err(|_| FdbError::WriteErr)?;
            file.write_all(buf).map_err(|_| FdbError::WriteErr)?;
            if sync {
                file.sync_all().map_err(|_| FdbError::WriteErr)?;
            }
            Ok(())
        }

        fn erase(&mut self, addr: u32, size: u32) -> Result<()> {
            let index = self.sector_index(addr);
            // Drop any cached handle and recreate the file truncated.
            self.cache.remove(&index);
            let path = self.file_path(index);
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .map_err(|_| FdbError::EraseErr)?;
            const CHUNK: usize = 256;
            let ones = [0xFFu8; CHUNK];
            let mut remaining = size as usize;
            while remaining > 0 {
                let n = remaining.min(CHUNK);
                file.write_all(&ones[..n]).map_err(|_| FdbError::EraseErr)?;
                remaining -= n;
            }
            file.sync_all().map_err(|_| FdbError::EraseErr)?;
            self.cache.insert(index, file);
            Ok(())
        }
    }
}
