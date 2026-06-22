//! Time-series database, a port of `fdb_tsdb.c`.
//!
//! Granularity-1 layout with 32-bit timestamps and variable-size blobs (the
//! `FDB_TSDB_FIXED_BLOB_SIZE` feature is not used). TSL = time series log; the
//! database stores many TSLs. Each sector keeps an array of fixed-size index
//! records growing down from the header while the variable-size blob data grows
//! up from the sector bottom.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::db::validate_geometry;
use crate::def::{
    wg_align, FdbTime, SectorStoreStatus, TslStatus, DATA_UNUSED, SECTOR_STORE_STATUS_NUM,
    TSL_STATUS_NUM,
};
use crate::error::{FdbError, Result};
use crate::flash::{write_align, write_status};
use crate::status::get_status;
use crate::storage::Storage;

/// magic word (`T`, `S`, `L`, `0`)
const SECTOR_MAGIC_WORD: u32 = 0x304C_5354;

const SECTOR_HDR_DATA_SIZE: u32 = 32;
const SECTOR_MAGIC_OFFSET: u32 = 1;
const SECTOR_START_TIME_OFFSET: u32 = 5;
const SECTOR_END0_TIME_OFFSET: u32 = 9;
const SECTOR_END0_IDX_OFFSET: u32 = 13;
const SECTOR_END0_STATUS_OFFSET: u32 = 17;
const SECTOR_END1_TIME_OFFSET: u32 = 18;
const SECTOR_END1_IDX_OFFSET: u32 = 22;
const SECTOR_END1_STATUS_OFFSET: u32 = 26;

const LOG_IDX_DATA_SIZE: u32 = 16;
const LOG_IDX_TS_OFFSET: u32 = 4;

const FAILED_ADDR: u32 = 0xFFFF_FFFF;

#[inline]
fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn rd_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn align_up(x: i64, a: i64) -> i64 {
    if x <= 0 {
        0
    } else {
        ((x + a - 1) / a) * a
    }
}

/// A time-series log node. Mirrors `struct fdb_tsl`.
#[derive(Clone)]
pub struct Tsl {
    pub(crate) status: TslStatus,
    pub(crate) time: FdbTime,
    pub(crate) log_len: u32,
    pub(crate) addr_index: u32,
    pub(crate) addr_log: u32,
}

impl Tsl {
    fn blank() -> Self {
        Tsl {
            status: TslStatus::Unused,
            time: 0,
            log_len: 0,
            addr_index: FAILED_ADDR,
            addr_log: FAILED_ADDR,
        }
    }

    /// The TSL timestamp.
    pub fn time(&self) -> FdbTime {
        self.time
    }

    /// The stored blob length in bytes.
    pub fn log_len(&self) -> usize {
        self.log_len as usize
    }

    /// The TSL status.
    pub fn status(&self) -> TslStatus {
        self.status
    }

    /// The address of this TSL's blob data within the database.
    pub fn log_addr(&self) -> u32 {
        self.addr_log
    }

    /// The address of this TSL's index record within the database.
    pub fn index_addr(&self) -> u32 {
        self.addr_index
    }
}

/// Sector descriptor, mirrors `struct tsdb_sec_info`.
#[derive(Clone, Copy)]
struct TsdbSecInfo {
    check_ok: bool,
    status: SectorStoreStatus,
    addr: u32,
    start_time: FdbTime,
    end_time: FdbTime,
    end_idx: u32,
    end_info_stat: [TslStatus; 2],
    remain: i64,
    empty_idx: u32,
    empty_data: u32,
}

impl TsdbSecInfo {
    fn blank() -> Self {
        TsdbSecInfo {
            check_ok: false,
            status: SectorStoreStatus::Unused,
            addr: 0,
            start_time: 0,
            end_time: 0,
            end_idx: 0,
            end_info_stat: [TslStatus::Unused; 2],
            remain: 0,
            empty_idx: 0,
            empty_data: 0,
        }
    }
}

#[inline]
fn get_next_tsl_addr(sector: &TsdbSecInfo, pre_tsl: &Tsl) -> u32 {
    if sector.status == SectorStoreStatus::Empty {
        return FAILED_ADDR;
    }
    if pre_tsl.addr_index + LOG_IDX_DATA_SIZE <= sector.end_idx {
        pre_tsl.addr_index + LOG_IDX_DATA_SIZE
    } else {
        FAILED_ADDR
    }
}

#[inline]
fn get_last_tsl_addr(sector: &TsdbSecInfo, pre_tsl: &Tsl) -> u32 {
    if sector.status == SectorStoreStatus::Empty {
        return FAILED_ADDR;
    }
    if pre_tsl.addr_index >= sector.addr + SECTOR_HDR_DATA_SIZE + LOG_IDX_DATA_SIZE {
        pre_tsl.addr_index - LOG_IDX_DATA_SIZE
    } else {
        FAILED_ADDR
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Fwd,
    Rev,
}

/// A time-series database backed by a [`Storage`].
pub struct Tsdb<S: Storage> {
    storage: S,
    sec_size: u32,
    max_size: u32,
    oldest_addr: u32,
    init_ok: bool,
    not_formatable: bool,
    get_time: Box<dyn FnMut() -> FdbTime>,
    max_len: usize,
    rollover: bool,
    last_time: FdbTime,
    cur_sec: TsdbSecInfo,
}

impl<S: Storage> Tsdb<S> {
    /// Open (initialising / recovering) a time-series database on `storage`.
    ///
    /// `get_time` returns the current timestamp for appended logs, `max_len` is
    /// the maximum blob length per record.
    pub fn new(
        storage: S,
        sec_size: u32,
        max_size: u32,
        get_time: impl FnMut() -> FdbTime + 'static,
        max_len: usize,
    ) -> Result<Self> {
        Self::with_options(storage, sec_size, max_size, get_time, max_len, false)
    }

    /// Like [`Tsdb::new`] with a non-formattable option: initialization fails on
    /// integrity errors instead of reformatting.
    pub fn with_options(
        storage: S,
        sec_size: u32,
        max_size: u32,
        get_time: impl FnMut() -> FdbTime + 'static,
        max_len: usize,
        not_formatable: bool,
    ) -> Result<Self> {
        validate_geometry(sec_size, max_size)?;
        if max_len as u32 >= sec_size {
            return Err(FdbError::InitFailed);
        }
        let mut cur_sec = TsdbSecInfo::blank();
        cur_sec.addr = DATA_UNUSED;
        let mut db = Tsdb {
            storage,
            sec_size,
            max_size,
            oldest_addr: DATA_UNUSED,
            init_ok: false,
            not_formatable,
            get_time: Box::new(get_time),
            max_len,
            rollover: true,
            last_time: 0,
            cur_sec,
        };
        db.init()?;
        db.init_ok = true;
        Ok(db)
    }

    /// Consume the database and return the underlying storage.
    pub fn into_storage(self) -> S {
        self.storage
    }

    /// Borrow the underlying storage.
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// The oldest sector start address.
    pub fn oldest_addr(&self) -> u32 {
        self.oldest_addr
    }

    /// The timestamp of the most recently appended log.
    pub fn last_time(&self) -> FdbTime {
        self.last_time
    }

    /// Whether ring-buffer rollover is enabled.
    pub fn rollover(&self) -> bool {
        self.rollover
    }

    /// Enable or disable ring-buffer rollover (`FDB_TSDB_CTRL_SET_ROLLOVER`).
    pub fn set_rollover(&mut self, rollover: bool) {
        self.rollover = rollover;
    }

    // ----------------------------------------------------------------- reads

    /// Port of `read_tsl`.
    fn read_tsl(&mut self, tsl: &mut Tsl) {
        let mut idx = [0u8; LOG_IDX_DATA_SIZE as usize];
        let _ = self.storage.read(tsl.addr_index, &mut idx);
        tsl.status = TslStatus::from_index(get_status(&idx[0..1], TSL_STATUS_NUM));
        if tsl.status == TslStatus::PreWrite || tsl.status == TslStatus::Unused {
            tsl.log_len = self.max_len as u32;
            tsl.addr_log = DATA_UNUSED;
            tsl.time = 0;
        } else {
            tsl.time = rd_i32(&idx, LOG_IDX_TS_OFFSET as usize);
            tsl.log_len = rd_u32(&idx, 8);
            tsl.addr_log = rd_u32(&idx, 12);
        }
    }

    /// Port of `read_sector_info`.
    fn read_sector_info(
        &mut self,
        addr: u32,
        sector: &mut TsdbSecInfo,
        traversal: bool,
    ) -> Result<()> {
        let mut hdr = [0u8; SECTOR_HDR_DATA_SIZE as usize];
        let _ = self.storage.read(addr, &mut hdr);

        sector.addr = addr;
        let magic = rd_u32(&hdr, SECTOR_MAGIC_OFFSET as usize);
        if magic != SECTOR_MAGIC_WORD {
            sector.check_ok = false;
            return Err(FdbError::InitFailed);
        }
        sector.check_ok = true;
        sector.status = SectorStoreStatus::from_index(get_status(&hdr[0..1], SECTOR_STORE_STATUS_NUM));
        sector.start_time = rd_i32(&hdr, SECTOR_START_TIME_OFFSET as usize);
        sector.end_info_stat[0] = TslStatus::from_index(get_status(
            &hdr[SECTOR_END0_STATUS_OFFSET as usize..SECTOR_END0_STATUS_OFFSET as usize + 1],
            TSL_STATUS_NUM,
        ));
        sector.end_info_stat[1] = TslStatus::from_index(get_status(
            &hdr[SECTOR_END1_STATUS_OFFSET as usize..SECTOR_END1_STATUS_OFFSET as usize + 1],
            TSL_STATUS_NUM,
        ));
        if sector.end_info_stat[0] == TslStatus::Write {
            sector.end_time = rd_i32(&hdr, SECTOR_END0_TIME_OFFSET as usize);
            sector.end_idx = rd_u32(&hdr, SECTOR_END0_IDX_OFFSET as usize);
        } else if sector.end_info_stat[1] == TslStatus::Write {
            sector.end_time = rd_i32(&hdr, SECTOR_END1_TIME_OFFSET as usize);
            sector.end_idx = rd_u32(&hdr, SECTOR_END1_IDX_OFFSET as usize);
        }

        sector.empty_idx = sector.addr + SECTOR_HDR_DATA_SIZE;
        sector.empty_data = sector.addr + self.sec_size;
        sector.remain = (sector.empty_data - sector.empty_idx) as i64;

        let mut result = Ok(());
        if sector.status == SectorStoreStatus::Using && traversal {
            let mut tsl = Tsl::blank();
            tsl.addr_index = sector.empty_idx;
            loop {
                self.read_tsl(&mut tsl);
                if tsl.status == TslStatus::Unused {
                    break;
                }
                if tsl.status != TslStatus::PreWrite {
                    sector.end_time = tsl.time;
                }
                sector.end_idx = tsl.addr_index;
                sector.empty_idx += LOG_IDX_DATA_SIZE;
                // saturating: a corrupt over-large log_len must not panic in
                // debug builds; the `remain` guard below catches the error.
                sector.empty_data = sector.empty_data.saturating_sub(wg_align(tsl.log_len));
                tsl.addr_index += LOG_IDX_DATA_SIZE;
                let cost = LOG_IDX_DATA_SIZE as i64 + wg_align(tsl.log_len) as i64;
                if sector.remain > cost {
                    sector.remain -= cost;
                } else {
                    sector.remain = 0;
                    result = Err(FdbError::ReadErr);
                    break;
                }
            }
        }
        result
    }

    /// Port of `get_next_sector_addr`.
    fn get_next_sector_addr(&self, pre_sec: &TsdbSecInfo, traversed_len: u32) -> u32 {
        if traversed_len + self.sec_size <= self.max_size {
            if pre_sec.addr + self.sec_size < self.max_size {
                pre_sec.addr + self.sec_size
            } else {
                0
            }
        } else {
            FAILED_ADDR
        }
    }

    /// Port of `get_last_sector_addr`.
    fn get_last_sector_addr(&self, pre_sec: &TsdbSecInfo, traversed_len: u32) -> u32 {
        if traversed_len + self.sec_size <= self.max_size {
            if pre_sec.addr >= self.sec_size {
                pre_sec.addr - self.sec_size
            } else {
                self.max_size - self.sec_size
            }
        } else {
            FAILED_ADDR
        }
    }

    // ----------------------------------------------------------------- writes

    /// Port of `format_sector`.
    fn format_sector(&mut self, addr: u32) -> Result<()> {
        debug_assert!(addr % self.sec_size == 0);
        self.storage.erase(addr, self.sec_size)?;
        write_status(
            &mut self.storage,
            addr,
            SECTOR_STORE_STATUS_NUM,
            SectorStoreStatus::Empty as usize,
            true,
        )?;
        self.storage
            .write(addr + SECTOR_MAGIC_OFFSET, &SECTOR_MAGIC_WORD.to_le_bytes(), true)?;
        Ok(())
    }

    /// Port of `write_tsl`.
    fn write_tsl(&mut self, blob: &[u8], time: FdbTime) -> Result<()> {
        let idx_addr = self.cur_sec.empty_idx;
        let log_addr = self.cur_sec.empty_data - wg_align(blob.len() as u32);
        let mut idx = [0xFFu8; LOG_IDX_DATA_SIZE as usize];
        idx[4..8].copy_from_slice(&time.to_le_bytes());
        idx[8..12].copy_from_slice(&(blob.len() as u32).to_le_bytes());
        idx[12..16].copy_from_slice(&log_addr.to_le_bytes());

        write_status(
            &mut self.storage,
            idx_addr,
            TSL_STATUS_NUM,
            TslStatus::PreWrite as usize,
            false,
        )?;
        self.storage
            .write(idx_addr + LOG_IDX_TS_OFFSET, &idx[LOG_IDX_TS_OFFSET as usize..], false)?;
        write_align(&mut self.storage, log_addr, blob)?;
        write_status(
            &mut self.storage,
            idx_addr,
            TSL_STATUS_NUM,
            TslStatus::Write as usize,
            true,
        )?;
        Ok(())
    }

    /// Port of `update_sec_status`, operating on `self.cur_sec` (which aliases
    /// `sector` in the C code).
    fn update_sec_status(&mut self, blob_size: u32, cur_time: FdbTime) -> Result<()> {
        let cost = LOG_IDX_DATA_SIZE as i64 + wg_align(blob_size) as i64;
        if self.cur_sec.status == SectorStoreStatus::Using && self.cur_sec.remain < cost {
            let end_index_temp = self.cur_sec.empty_idx - LOG_IDX_DATA_SIZE;
            let cur_sec_addr = self.cur_sec.addr;
            let last_time = self.last_time;

            if self.cur_sec.end_info_stat[0] == TslStatus::Unused {
                write_status(
                    &mut self.storage,
                    cur_sec_addr + SECTOR_END0_STATUS_OFFSET,
                    TSL_STATUS_NUM,
                    TslStatus::PreWrite as usize,
                    false,
                )?;
                self.storage.write(
                    cur_sec_addr + SECTOR_END0_TIME_OFFSET,
                    &last_time.to_le_bytes(),
                    false,
                )?;
                self.storage.write(
                    cur_sec_addr + SECTOR_END0_IDX_OFFSET,
                    &end_index_temp.to_le_bytes(),
                    false,
                )?;
                write_status(
                    &mut self.storage,
                    cur_sec_addr + SECTOR_END0_STATUS_OFFSET,
                    TSL_STATUS_NUM,
                    TslStatus::Write as usize,
                    true,
                )?;
            } else if self.cur_sec.end_info_stat[1] == TslStatus::Unused {
                write_status(
                    &mut self.storage,
                    cur_sec_addr + SECTOR_END1_STATUS_OFFSET,
                    TSL_STATUS_NUM,
                    TslStatus::PreWrite as usize,
                    false,
                )?;
                self.storage.write(
                    cur_sec_addr + SECTOR_END1_TIME_OFFSET,
                    &last_time.to_le_bytes(),
                    false,
                )?;
                self.storage.write(
                    cur_sec_addr + SECTOR_END1_IDX_OFFSET,
                    &end_index_temp.to_le_bytes(),
                    false,
                )?;
                write_status(
                    &mut self.storage,
                    cur_sec_addr + SECTOR_END1_STATUS_OFFSET,
                    TSL_STATUS_NUM,
                    TslStatus::Write as usize,
                    true,
                )?;
            }

            write_status(
                &mut self.storage,
                cur_sec_addr,
                SECTOR_STORE_STATUS_NUM,
                SectorStoreStatus::Full as usize,
                true,
            )?;
            self.cur_sec.status = SectorStoreStatus::Full;

            let new_sec_addr = if cur_sec_addr + self.sec_size < self.max_size {
                cur_sec_addr + self.sec_size
            } else if self.rollover {
                0
            } else {
                return Err(FdbError::SavedFull);
            };

            let mut cs = TsdbSecInfo::blank();
            let _ = self.read_sector_info(new_sec_addr, &mut cs, false);
            self.cur_sec = cs;
            if self.cur_sec.status != SectorStoreStatus::Empty {
                if new_sec_addr + self.sec_size < self.max_size {
                    self.oldest_addr = new_sec_addr + self.sec_size;
                } else {
                    self.oldest_addr = 0;
                }
                self.format_sector(new_sec_addr)?;
                let mut cs2 = TsdbSecInfo::blank();
                let _ = self.read_sector_info(new_sec_addr, &mut cs2, false);
                self.cur_sec = cs2;
            }
        } else if self.cur_sec.status == SectorStoreStatus::Full {
            return Err(FdbError::SavedFull);
        }

        if self.cur_sec.status == SectorStoreStatus::Empty {
            self.cur_sec.status = SectorStoreStatus::Using;
            self.cur_sec.start_time = cur_time;
            let addr = self.cur_sec.addr;
            write_status(
                &mut self.storage,
                addr,
                SECTOR_STORE_STATUS_NUM,
                SectorStoreStatus::Using as usize,
                true,
            )?;
            self.storage
                .write(addr + SECTOR_START_TIME_OFFSET, &cur_time.to_le_bytes(), true)?;
        }
        Ok(())
    }

    /// Port of `tsl_append`.
    fn tsl_append(&mut self, blob: &[u8], timestamp: Option<FdbTime>) -> Result<()> {
        let cur_time = match timestamp {
            Some(t) => t,
            None => (self.get_time)(),
        };
        if blob.len() > self.max_len {
            return Err(FdbError::WriteErr);
        }
        if cur_time <= self.last_time {
            return Err(FdbError::WriteErr);
        }
        self.update_sec_status(blob.len() as u32, cur_time)?;
        self.write_tsl(blob, cur_time)?;

        self.cur_sec.end_idx = self.cur_sec.empty_idx;
        self.cur_sec.end_time = cur_time;
        self.cur_sec.empty_idx += LOG_IDX_DATA_SIZE;
        self.cur_sec.empty_data -= wg_align(blob.len() as u32);
        self.cur_sec.remain -= LOG_IDX_DATA_SIZE as i64 + wg_align(blob.len() as u32) as i64;
        self.last_time = cur_time;
        Ok(())
    }

    /// Append a new log with the current timestamp (`fdb_tsl_append`).
    pub fn append(&mut self, blob: &[u8]) -> Result<()> {
        if !self.init_ok {
            return Err(FdbError::InitFailed);
        }
        self.tsl_append(blob, None)
    }

    /// Append a new log with an explicit timestamp (`fdb_tsl_append_with_ts`).
    pub fn append_with_ts(&mut self, blob: &[u8], timestamp: FdbTime) -> Result<()> {
        if !self.init_ok {
            return Err(FdbError::InitFailed);
        }
        self.tsl_append(blob, Some(timestamp))
    }

    /// Set a TSL's status (`fdb_tsl_set_status`).
    pub fn set_status(&mut self, tsl: &Tsl, status: TslStatus) -> Result<()> {
        write_status(
            &mut self.storage,
            tsl.addr_index,
            TSL_STATUS_NUM,
            status.index(),
            true,
        )
    }

    /// Read a TSL's blob data into `buf`, returning the number of bytes read.
    pub fn read_log(&mut self, tsl: &Tsl, buf: &mut [u8]) -> usize {
        let read_len = core::cmp::min(buf.len(), tsl.log_len as usize);
        if self.storage.read(tsl.addr_log, &mut buf[..read_len]).is_err() {
            return 0;
        }
        read_len
    }

    // ------------------------------------------------------------ iteration

    /// Collect all TSLs in storage order (`fdb_tsl_iter`).
    pub fn collect(&mut self) -> Vec<Tsl> {
        let mut out = Vec::new();
        if !self.init_ok {
            return out;
        }
        let mut sec_addr = self.oldest_addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut sector = TsdbSecInfo::blank();
            if self.read_sector_info(sec_addr, &mut sector, false).is_ok()
                && (sector.status == SectorStoreStatus::Using
                    || sector.status == SectorStoreStatus::Full)
            {
                if sector.status == SectorStoreStatus::Using {
                    sector = self.cur_sec;
                }
                let mut tsl = Tsl::blank();
                tsl.addr_index = sector.addr + SECTOR_HDR_DATA_SIZE;
                loop {
                    self.read_tsl(&mut tsl);
                    out.push(tsl.clone());
                    let next = get_next_tsl_addr(&sector, &tsl);
                    if next == FAILED_ADDR {
                        break;
                    }
                    tsl.addr_index = next;
                }
            }
            let next = self.get_next_sector_addr(&sector, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        out
    }

    /// Iterate every TSL forward, reading each blob (`fdb_tsl_iter`). The
    /// callback receives the TSL and its data; return `true` to stop early.
    pub fn iter<F: FnMut(&Tsl, &[u8]) -> bool>(&mut self, mut f: F) {
        let tsls = self.collect();
        for tsl in &tsls {
            let mut buf = vec![0u8; tsl.log_len as usize];
            let n = self.read_log(tsl, &mut buf);
            buf.truncate(n);
            if f(tsl, &buf) {
                break;
            }
        }
    }

    /// Iterate every TSL in reverse storage order (`fdb_tsl_iter_reverse`).
    pub fn iter_reverse<F: FnMut(&Tsl, &[u8]) -> bool>(&mut self, mut f: F) {
        let tsls = self.collect_reverse();
        for tsl in &tsls {
            let mut buf = vec![0u8; tsl.log_len as usize];
            let n = self.read_log(tsl, &mut buf);
            buf.truncate(n);
            if f(tsl, &buf) {
                break;
            }
        }
    }

    fn collect_reverse(&mut self) -> Vec<Tsl> {
        let mut out = Vec::new();
        if !self.init_ok {
            return out;
        }
        let mut sec_addr = self.cur_sec.addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut sector = TsdbSecInfo::blank();
            if self.read_sector_info(sec_addr, &mut sector, false).is_ok() {
                if sector.status == SectorStoreStatus::Using
                    || sector.status == SectorStoreStatus::Full
                {
                    if sector.status == SectorStoreStatus::Using {
                        sector = self.cur_sec;
                    }
                    let mut tsl = Tsl::blank();
                    tsl.addr_index = sector.end_idx;
                    loop {
                        self.read_tsl(&mut tsl);
                        out.push(tsl.clone());
                        let next = get_last_tsl_addr(&sector, &tsl);
                        if next == FAILED_ADDR {
                            break;
                        }
                        tsl.addr_index = next;
                    }
                } else if sector.status == SectorStoreStatus::Empty
                    || sector.status == SectorStoreStatus::Unused
                {
                    break;
                }
            }
            let next = self.get_last_sector_addr(&sector, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        out
    }

    /// Port of `search_start_tsl_addr`.
    fn search_start_tsl_addr(&mut self, start_in: u32, end_in: u32, from: FdbTime, to: FdbTime) -> u32 {
        let mut start = start_in as i64;
        let mut end = end_in as i64;
        let mut tsl = Tsl::blank();
        loop {
            let mid = start + align_up((end - start) / 2, LOG_IDX_DATA_SIZE as i64);
            tsl.addr_index = mid as u32;
            self.read_tsl(&mut tsl);
            if tsl.time < from {
                start = tsl.addr_index as i64 + LOG_IDX_DATA_SIZE as i64;
            } else if tsl.time > from {
                end = tsl.addr_index as i64 - LOG_IDX_DATA_SIZE as i64;
            } else {
                return tsl.addr_index;
            }
            if start > end {
                if from > to {
                    tsl.addr_index = start as u32;
                    self.read_tsl(&mut tsl);
                    if tsl.time > from {
                        start -= LOG_IDX_DATA_SIZE as i64;
                    }
                }
                break;
            }
        }
        start as u32
    }

    /// Collect TSLs whose timestamp falls in `[from, to]` (or `[to, from]` when
    /// `from > to`, iterating in reverse), mirroring `fdb_tsl_iter_by_time`.
    pub fn collect_by_time(&mut self, from: FdbTime, to: FdbTime) -> Vec<Tsl> {
        let mut out = Vec::new();
        if !self.init_ok {
            return out;
        }
        let dir = if from <= to { Dir::Fwd } else { Dir::Rev };
        let start_addr = match dir {
            Dir::Fwd => self.oldest_addr,
            Dir::Rev => self.cur_sec.addr,
        };
        let mut found_start_tsl = false;
        let mut sec_addr = start_addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut sector = TsdbSecInfo::blank();
            if self.read_sector_info(sec_addr, &mut sector, false).is_ok() {
                if sector.status == SectorStoreStatus::Using
                    || sector.status == SectorStoreStatus::Full
                {
                    if sector.status == SectorStoreStatus::Using {
                        sector = self.cur_sec;
                    }
                    let cond = found_start_tsl
                        || match dir {
                            Dir::Fwd => {
                                (sec_addr == start_addr && from <= sector.start_time)
                                    || from <= sector.end_time
                            }
                            Dir::Rev => {
                                (sec_addr == start_addr && from >= sector.end_time)
                                    || from >= sector.start_time
                            }
                        };
                    if cond {
                        found_start_tsl = true;
                        let start = sector.addr + SECTOR_HDR_DATA_SIZE;
                        let end = sector.end_idx;
                        let mut tsl = Tsl::blank();
                        tsl.addr_index = self.search_start_tsl_addr(start, end, from, to);
                        let mut exit = false;
                        loop {
                            self.read_tsl(&mut tsl);
                            if tsl.status != TslStatus::Unused {
                                let in_range = match dir {
                                    Dir::Fwd => tsl.time >= from && tsl.time <= to,
                                    Dir::Rev => tsl.time <= from && tsl.time >= to,
                                };
                                if in_range {
                                    out.push(tsl.clone());
                                } else {
                                    exit = true;
                                    break;
                                }
                            }
                            let next = match dir {
                                Dir::Fwd => get_next_tsl_addr(&sector, &tsl),
                                Dir::Rev => get_last_tsl_addr(&sector, &tsl),
                            };
                            if next == FAILED_ADDR {
                                break;
                            }
                            tsl.addr_index = next;
                        }
                        if exit {
                            break;
                        }
                    }
                } else if sector.status == SectorStoreStatus::Empty {
                    break;
                }
            }
            let next = match dir {
                Dir::Fwd => self.get_next_sector_addr(&sector, traversed_len),
                Dir::Rev => self.get_last_sector_addr(&sector, traversed_len),
            };
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        out
    }

    /// Iterate TSLs by time range, reading each blob (`fdb_tsl_iter_by_time`).
    pub fn iter_by_time<F: FnMut(&Tsl, &[u8]) -> bool>(
        &mut self,
        from: FdbTime,
        to: FdbTime,
        mut f: F,
    ) {
        let tsls = self.collect_by_time(from, to);
        for tsl in &tsls {
            let mut buf = vec![0u8; tsl.log_len as usize];
            let n = self.read_log(tsl, &mut buf);
            buf.truncate(n);
            if f(tsl, &buf) {
                break;
            }
        }
    }

    /// Count TSLs in `[from, to]` with the given `status` (`fdb_tsl_query_count`).
    pub fn query_count(&mut self, from: FdbTime, to: FdbTime, status: TslStatus) -> usize {
        if !self.init_ok {
            return 0;
        }
        self.collect_by_time(from, to)
            .iter()
            .filter(|t| t.status == status)
            .count()
    }

    /// Maximum number of blobs the database can hold (`fdb_tsl_max_blob_count`).
    pub fn max_blob_count(&self) -> usize {
        let max_blob_len = self.max_len as u32;
        let sec_size = self.sec_size - SECTOR_HDR_DATA_SIZE;
        let blob_size = LOG_IDX_DATA_SIZE + wg_align(max_blob_len);
        let n_sec = self.max_size / self.sec_size;
        (n_sec as usize) * (sec_size as usize / blob_size as usize)
    }

    // ----------------------------------------------------------------- clean / init

    /// Erase all data (`fdb_tsl_clean`). DANGEROUS and irreversible.
    pub fn clean(&mut self) {
        let mut sec_addr = 0u32;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut s = TsdbSecInfo::blank();
            let _ = self.read_sector_info(sec_addr, &mut s, false);
            let _ = self.format_sector(s.addr);
            let next = self.get_next_sector_addr(&s, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        self.oldest_addr = 0;
        self.last_time = 0;
        let mut cs = TsdbSecInfo::blank();
        let _ = self.read_sector_info(0, &mut cs, false);
        self.cur_sec = cs;
    }

    /// Port of `tsl_format_all`.
    fn tsl_format_all(&mut self) {
        self.clean();
    }

    /// Port of `fdb_tsdb_init`.
    fn init(&mut self) -> Result<()> {
        let mut check_failed = false;
        let mut empty_num = 0usize;
        let mut empty_addr = 0u32;

        let mut sec_addr = 0u32;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut sector = TsdbSecInfo::blank();
            let _ = self.read_sector_info(sec_addr, &mut sector, false);
            let _ = self.read_sector_info(sec_addr, &mut sector, true);

            if !sector.check_ok {
                check_failed = true;
                break;
            } else if sector.status == SectorStoreStatus::Using {
                if self.cur_sec.addr == DATA_UNUSED {
                    self.cur_sec = sector;
                } else {
                    check_failed = true;
                    break;
                }
            } else if sector.status == SectorStoreStatus::Empty {
                empty_num += 1;
                empty_addr = sector.addr;
                if empty_num == 1 && self.cur_sec.addr == DATA_UNUSED {
                    self.cur_sec = sector;
                }
            }

            let next = self.get_next_sector_addr(&sector, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }

        if check_failed {
            if self.not_formatable {
                return Err(FdbError::ReadErr);
            }
            self.tsl_format_all();
        } else {
            let latest_addr;
            if empty_num > 0 {
                latest_addr = empty_addr;
            } else if self.rollover {
                latest_addr = self.cur_sec.addr;
            } else {
                latest_addr = self.max_size - self.sec_size;
                self.cur_sec.addr = latest_addr;
            }
            if latest_addr + self.sec_size >= self.max_size {
                self.oldest_addr = 0;
            } else {
                self.oldest_addr = latest_addr + self.sec_size;
            }
        }

        let cur_addr = self.cur_sec.addr;
        let mut cs = TsdbSecInfo::blank();
        let _ = self.read_sector_info(cur_addr, &mut cs, true);
        self.cur_sec = cs;

        if self.cur_sec.status == SectorStoreStatus::Using {
            self.last_time = self.cur_sec.end_time;
        } else if self.cur_sec.status == SectorStoreStatus::Empty
            && self.oldest_addr != self.cur_sec.addr
        {
            let addr = if self.cur_sec.addr == 0 {
                self.max_size - self.sec_size
            } else {
                self.cur_sec.addr - self.sec_size
            };
            let mut sec = TsdbSecInfo::blank();
            let _ = self.read_sector_info(addr, &mut sec, false);
            self.last_time = sec.end_time;
        }
        Ok(())
    }
}
