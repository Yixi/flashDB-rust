//! Shared database geometry validation and default-KV definitions.

use crate::error::{FdbError, Result};

/// Validate the sector / database geometry, mirroring the checks in
/// `_fdb_init_ex`.
///
/// * sector size and database size must be non-zero,
/// * sector size must be a power of two,
/// * database size must be a whole number of sectors,
/// * the database must contain at least two sectors.
pub(crate) fn validate_geometry(sec_size: u32, max_size: u32) -> Result<()> {
    if sec_size == 0 || max_size == 0 {
        return Err(FdbError::InitFailed);
    }
    if sec_size & (sec_size - 1) != 0 {
        return Err(FdbError::InitFailed);
    }
    if max_size % sec_size != 0 {
        return Err(FdbError::InitFailed);
    }
    if max_size / sec_size < 2 {
        return Err(FdbError::InitFailed);
    }
    Ok(())
}

/// A default key/value pair used to seed a freshly formatted KVDB.
///
/// The value is stored verbatim with length `value.len()`.
#[derive(Debug, Clone, Copy)]
pub struct DefaultKv<'a> {
    /// KV name.
    pub key: &'a [u8],
    /// KV value bytes.
    pub value: &'a [u8],
}

impl<'a> DefaultKv<'a> {
    /// Create a default KV from a name and value.
    pub const fn new(key: &'a [u8], value: &'a [u8]) -> Self {
        DefaultKv { key, value }
    }
}
