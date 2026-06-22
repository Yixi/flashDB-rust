//! Low-level flash helpers shared by KVDB and TSDB.
//!
//! Ports of the status / scan helpers from `fdb_utils.c` (`_fdb_write_status`,
//! `_fdb_read_status`, `_fdb_continue_ff_addr`, `_fdb_flash_write_align`),
//! specialised to write granularity 1.

use crate::def::{status_table_size, wg_align, BYTE_ERASED, BYTE_WRITTEN};
use crate::error::Result;
use crate::status::{get_status, set_status};
use crate::storage::Storage;

/// Scratch buffer large enough for every status table this crate uses (all are
/// 1 byte at granularity 1).
const STATUS_SCRATCH: usize = 4;

/// Program a new status value into the status table at `addr`.
///
/// Port of `_fdb_write_status`. The first state ("unused", index 0) is the
/// erased pattern, so nothing is written for it.
pub(crate) fn write_status<S: Storage>(
    s: &mut S,
    addr: u32,
    status_num: usize,
    status_index: usize,
    sync: bool,
) -> Result<()> {
    debug_assert!(status_index < status_num);
    let mut table = [0xFFu8; STATUS_SCRATCH];
    match set_status(&mut table, status_num, status_index) {
        None => Ok(()),
        Some(byte_index) => s.write(addr + byte_index as u32, &table[byte_index..byte_index + 1], sync),
    }
}

/// Read and decode the status value stored at `addr`.
///
/// Port of `_fdb_read_status`.
pub(crate) fn read_status<S: Storage>(s: &mut S, addr: u32, total_num: usize) -> usize {
    let size = status_table_size(total_num);
    let mut table = [0xFFu8; STATUS_SCRATCH];
    // Upstream ignores read failures here; the buffer simply stays erased.
    let _ = s.read(addr, &mut table[..size]);
    get_status(&table[..size], total_num)
}

/// Find the address from which the flash is continuously erased (`0xFF`) up to
/// `end`. Port of `_fdb_continue_ff_addr`.
pub(crate) fn continue_ff_addr<S: Storage>(s: &mut S, mut start: u32, end: u32) -> u32 {
    let mut buf = [0u8; 32];
    let mut last_data = BYTE_WRITTEN;
    let mut addr = start;

    while start < end {
        let read_size = if start + buf.len() as u32 <= end {
            buf.len()
        } else {
            (end - start) as usize
        };
        let _ = s.read(start, &mut buf[..read_size]);
        for (i, &b) in buf[..read_size].iter().enumerate() {
            if last_data != BYTE_ERASED && b == BYTE_ERASED {
                addr = start + i as u32;
            }
            last_data = b;
        }
        start = start.wrapping_add(buf.len() as u32);
    }

    if last_data == BYTE_ERASED {
        wg_align(addr)
    } else {
        end
    }
}

/// Write `buf` honouring write-granularity alignment. At granularity 1 this is a
/// plain (unsynced) program. Port of `_fdb_flash_write_align`.
pub(crate) fn write_align<S: Storage>(s: &mut S, addr: u32, buf: &[u8]) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    s.write(addr, buf, false)
}
