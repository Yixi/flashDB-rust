//! Error codes, mirroring `fdb_err_t` from `fdb_def.h`.

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, FdbError>;

/// Error codes returned by FlashDB operations.
///
/// The discriminants match the original `fdb_err_t` enum so that numeric error
/// codes stay compatible with upstream FlashDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FdbError {
    /// Erase operation failed.
    EraseErr = 1,
    /// Read operation failed.
    ReadErr = 2,
    /// Write operation failed.
    WriteErr = 3,
    /// The requested partition / storage was not found.
    PartNotFound = 4,
    /// The KV name is invalid (e.g. too long) or the KV was not found.
    KvNameErr = 5,
    /// A KV with this name already exists.
    KvNameExist = 6,
    /// The database is full and the value could not be saved.
    SavedFull = 7,
    /// Database initialization failed.
    InitFailed = 8,
}

impl core::fmt::Display for FdbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            FdbError::EraseErr => "flash erase error",
            FdbError::ReadErr => "flash read error",
            FdbError::WriteErr => "flash write error",
            FdbError::PartNotFound => "partition not found",
            FdbError::KvNameErr => "KV name error or not found",
            FdbError::KvNameExist => "KV name already exists",
            FdbError::SavedFull => "database is full",
            FdbError::InitFailed => "database initialization failed",
        };
        f.write_str(s)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for FdbError {}
