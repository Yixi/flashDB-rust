//! Shared constants and on-storage status enumerations.
//!
//! These mirror the definitions in `fdb_def.h` / `fdb_low_lvl.h` for the
//! write-granularity-1 (NOR flash / file mode) configuration that this crate
//! targets.

/// Flash write granularity in bits. This crate targets the granularity-1
/// configuration (NOR flash and file mode), where every byte can be programmed
/// individually and there is no alignment padding inside status fields.
pub(crate) const WRITE_GRAN: usize = 1;

/// The value of an erased flash byte.
pub(crate) const BYTE_ERASED: u8 = 0xFF;

/// The value of a fully-written flash byte.
pub(crate) const BYTE_WRITTEN: u8 = 0x00;

/// A 32-bit "unused"/erased word (all bits set, since [`BYTE_ERASED`] is 0xFF).
pub(crate) const DATA_UNUSED: u32 = 0xFFFF_FFFF;

/// Maximum length of a KV name (`FDB_KV_NAME_MAX`).
pub const KV_NAME_MAX: usize = 64;

/// Default maximum string length for [`crate::Kvdb::get_str`]
/// (`FDB_STR_KV_VALUE_MAX_SIZE`).
pub(crate) const STR_KV_VALUE_MAX_SIZE: usize = 128;

/// Return the status-table size in bytes for `status_number` states.
///
/// Port of `FDB_STATUS_TABLE_SIZE` for `FDB_WRITE_GRAN == 1`.
#[inline]
pub(crate) const fn status_table_size(status_number: usize) -> usize {
    (status_number * WRITE_GRAN + 7) / 8
}

/// Align `size` up to the write granularity. Identity for granularity 1.
#[inline]
pub(crate) const fn wg_align(size: u32) -> u32 {
    // FDB_WG_ALIGN(size) with align unit (WRITE_GRAN + 7)/8 == 1
    size
}

/// Align `addr` down to `align` (`FDB_ALIGN_DOWN`).
#[inline]
pub(crate) const fn align_down(size: u32, align: u32) -> u32 {
    (size / align) * align
}

// ---------------------------------------------------------------------------
// Status enumerations
// ---------------------------------------------------------------------------

/// Timestamp type, `fdb_time_t` (32-bit configuration).
pub type FdbTime = i32;

/// Number of KV node states (`FDB_KV_STATUS_NUM`).
pub(crate) const KV_STATUS_NUM: usize = 6;
/// Number of TSL node states (`FDB_TSL_STATUS_NUM`).
pub(crate) const TSL_STATUS_NUM: usize = 6;
/// Number of sector store states (`FDB_SECTOR_STORE_STATUS_NUM`).
pub(crate) const SECTOR_STORE_STATUS_NUM: usize = 4;
/// Number of sector dirty states (`FDB_SECTOR_DIRTY_STATUS_NUM`).
pub(crate) const SECTOR_DIRTY_STATUS_NUM: usize = 4;

/// KV node status, `fdb_kv_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum KvStatus {
    Unused = 0,
    PreWrite = 1,
    Write = 2,
    PreDelete = 3,
    Deleted = 4,
    ErrHdr = 5,
}

impl KvStatus {
    pub(crate) fn from_index(i: usize) -> Self {
        match i {
            0 => KvStatus::Unused,
            1 => KvStatus::PreWrite,
            2 => KvStatus::Write,
            3 => KvStatus::PreDelete,
            4 => KvStatus::Deleted,
            _ => KvStatus::ErrHdr,
        }
    }
}

/// Time-series log node status, `fdb_tsl_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum TslStatus {
    /// Slot is erased / unused.
    Unused = 0,
    /// The log index has been reserved but the write is not yet complete.
    PreWrite = 1,
    /// The log has been written and is valid.
    Write = 2,
    /// User-defined status #1.
    UserStatus1 = 3,
    /// The log has been (logically) deleted.
    Deleted = 4,
    /// User-defined status #2.
    UserStatus2 = 5,
}

impl TslStatus {
    pub(crate) fn from_index(i: usize) -> Self {
        match i {
            0 => TslStatus::Unused,
            1 => TslStatus::PreWrite,
            2 => TslStatus::Write,
            3 => TslStatus::UserStatus1,
            4 => TslStatus::Deleted,
            _ => TslStatus::UserStatus2,
        }
    }
    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

/// Flash sector store status, `fdb_sector_store_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum SectorStoreStatus {
    Unused = 0,
    Empty = 1,
    Using = 2,
    Full = 3,
}

impl SectorStoreStatus {
    pub(crate) fn from_index(i: usize) -> Self {
        match i {
            1 => SectorStoreStatus::Empty,
            2 => SectorStoreStatus::Using,
            3 => SectorStoreStatus::Full,
            _ => SectorStoreStatus::Unused,
        }
    }
}

/// Flash sector dirty status, `fdb_sector_dirty_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum SectorDirtyStatus {
    Unused = 0,
    False = 1,
    True = 2,
    Gc = 3,
}

impl SectorDirtyStatus {
    pub(crate) fn from_index(i: usize) -> Self {
        match i {
            1 => SectorDirtyStatus::False,
            2 => SectorDirtyStatus::True,
            3 => SectorDirtyStatus::Gc,
            _ => SectorDirtyStatus::Unused,
        }
    }
}
