//! Status-table bit encoding, ported from `_fdb_set_status` / `_fdb_get_status`
//! in `fdb_utils.c` for write granularity 1.
//!
//! A status table is a small bitmap stored at the start of a sector / KV / TSL
//! header. Each successive state clears one more high bit, which works on NOR
//! flash because programming can only flip `1` bits to `0`. State 0 ("unused")
//! is the all-ones erased value and is never written.

use crate::def::status_table_size;

/// Fill `table` with the bit pattern representing `status_index` and return the
/// index of the single byte that differs from the fully-erased pattern, or
/// `None` when `status_index == 0` (no programming needed).
///
/// Port of `_fdb_set_status` (granularity 1).
pub(crate) fn set_status(table: &mut [u8], status_num: usize, status_index: usize) -> Option<usize> {
    let size = status_table_size(status_num);
    for b in table[..size].iter_mut() {
        *b = 0xFF;
    }
    if status_index > 0 {
        let byte_index = (status_index - 1) / 8;
        table[byte_index] &= 0x00ff >> (status_index % 8);
        Some(byte_index)
    } else {
        None
    }
}

/// Decode the current state from a status `table`.
///
/// Port of `_fdb_get_status` (granularity 1): it finds the first `0` bit
/// scanning from the most-significant state downwards.
pub(crate) fn get_status(table: &[u8], status_num: usize) -> usize {
    // Mirrors the C post-decrement loop exactly.
    let bak = status_num - 1; // --status_num
    let mut sn = bak;
    let mut i = 0usize;
    loop {
        if sn == 0 {
            break;
        }
        sn -= 1;
        if table[sn / 8] & (0x80 >> (sn % 8)) == 0x00 {
            break;
        }
        i += 1;
    }
    bak - i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{KV_STATUS_NUM, SECTOR_STORE_STATUS_NUM};

    #[test]
    fn set_then_get_roundtrip_kv() {
        for idx in 0..KV_STATUS_NUM {
            let mut t = [0u8; 1];
            set_status(&mut t, KV_STATUS_NUM, idx);
            assert_eq!(get_status(&t, KV_STATUS_NUM), idx, "kv status {idx}");
        }
    }

    #[test]
    fn known_bit_patterns() {
        let mut t = [0u8; 1];
        // index 0 -> no write, table stays erased (0xFF), decodes to 0.
        assert_eq!(set_status(&mut t, KV_STATUS_NUM, 0), None);
        assert_eq!(t[0], 0xFF);
        // PRE_WRITE (1) -> 0x7F, WRITE (2) -> 0x3F.
        set_status(&mut t, KV_STATUS_NUM, 1);
        assert_eq!(t[0], 0x7F);
        set_status(&mut t, KV_STATUS_NUM, 2);
        assert_eq!(t[0], 0x3F);
    }

    #[test]
    fn sector_store_states() {
        for idx in 0..SECTOR_STORE_STATUS_NUM {
            let mut t = [0u8; 1];
            set_status(&mut t, SECTOR_STORE_STATUS_NUM, idx);
            assert_eq!(get_status(&t, SECTOR_STORE_STATUS_NUM), idx);
        }
    }
}
