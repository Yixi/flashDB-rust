//! Key-value database, a port of `fdb_kvdb.c`.
//!
//! This implements the granularity-1 layout without the optional in-RAM caches
//! (`FDB_KV_USING_CACHE` undefined). The caches are a pure speed optimisation in
//! upstream FlashDB; disabling them yields identical on-storage behaviour and a
//! much simpler, borrow-checker-friendly implementation.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::crc32::calc_crc32;
use crate::db::{validate_geometry, DefaultKv};
use crate::def::{
    align_down, wg_align, KvStatus, SectorDirtyStatus, SectorStoreStatus, KV_NAME_MAX,
    KV_STATUS_NUM, SECTOR_DIRTY_STATUS_NUM, SECTOR_STORE_STATUS_NUM, STR_KV_VALUE_MAX_SIZE,
};
use crate::error::{FdbError, Result};
use crate::flash::{continue_ff_addr, read_status, write_align, write_status};
use crate::status::{get_status, set_status};
use crate::storage::Storage;

/// magic word (`F`, `D`, `B`, `1`)
const SECTOR_MAGIC_WORD: u32 = 0x3042_4446;
/// magic word (`K`, `V`, `0`, `0`)
const KV_MAGIC_WORD: u32 = 0x3030_564B;

const SECTOR_HDR_DATA_SIZE: u32 = 16;
const SECTOR_DIRTY_OFFSET: u32 = 1;
const SECTOR_MAGIC_OFFSET: u32 = 4;

const KV_HDR_DATA_SIZE: u32 = 24;
const KV_MAGIC_OFFSET: u32 = 4;

const SECTOR_NOT_COMBINED: u32 = 0xFFFF_FFFF;
const SECTOR_COMBINED: u32 = 0x0000_0000;
const FAILED_ADDR: u32 = 0xFFFF_FFFF;

/// `FDB_SEC_REMAIN_THRESHOLD` = KV_HDR_DATA_SIZE + FDB_KV_NAME_MAX.
const SEC_REMAIN_THRESHOLD: i64 = KV_HDR_DATA_SIZE as i64 + KV_NAME_MAX as i64;
/// `FDB_GC_EMPTY_SEC_THRESHOLD`.
const GC_EMPTY_SEC_THRESHOLD: usize = 1;

#[inline]
fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// A key-value node read from storage. Mirrors `struct fdb_kv`.
#[derive(Clone)]
pub struct Kv {
    pub(crate) status: KvStatus,
    pub(crate) crc_is_ok: bool,
    pub(crate) name_len: u8,
    pub(crate) len: u32,
    pub(crate) value_len: u32,
    pub(crate) name: [u8; KV_NAME_MAX],
    pub(crate) addr_start: u32,
    pub(crate) addr_value: u32,
}

impl Kv {
    fn blank() -> Self {
        Kv {
            status: KvStatus::Unused,
            crc_is_ok: false,
            name_len: 0,
            len: 0,
            value_len: 0,
            name: [0u8; KV_NAME_MAX],
            addr_start: FAILED_ADDR,
            addr_value: FAILED_ADDR,
        }
    }

    /// The KV name as bytes.
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// The stored value length in bytes.
    pub fn value_len(&self) -> usize {
        self.value_len as usize
    }

    /// The start address of the KV node within the database.
    pub fn addr(&self) -> u32 {
        self.addr_start
    }
}

/// Sector descriptor, mirrors `struct kvdb_sec_info`.
#[derive(Clone, Copy)]
struct KvSecInfo {
    check_ok: bool,
    store: SectorStoreStatus,
    dirty: SectorDirtyStatus,
    addr: u32,
    combined: u32,
    remain: i64,
    empty_kv: u32,
}

impl KvSecInfo {
    fn blank() -> Self {
        KvSecInfo {
            check_ok: false,
            store: SectorStoreStatus::Unused,
            dirty: SectorDirtyStatus::Unused,
            addr: 0,
            combined: SECTOR_NOT_COMBINED,
            remain: 0,
            empty_kv: FAILED_ADDR,
        }
    }
}

/// Iterator state for [`Kvdb::iterate`], mirrors `struct fdb_kv_iterator`.
pub struct KvIterator {
    curr_kv: Kv,
    /// Number of KVs iterated so far.
    pub iterated_cnt: u32,
    /// Total node storage iterated.
    pub iterated_obj_bytes: usize,
    /// Total value bytes iterated.
    pub iterated_value_bytes: usize,
    sector_addr: u32,
    traversed_len: u32,
}

impl KvIterator {
    /// The KV at the current iterator position (valid after [`Kvdb::iterate`]
    /// returns `true`).
    pub fn current(&self) -> &Kv {
        &self.curr_kv
    }
}

/// A key-value database backed by a [`Storage`].
pub struct Kvdb<S: Storage> {
    storage: S,
    sec_size: u32,
    max_size: u32,
    oldest_addr: u32,
    init_ok: bool,
    not_formatable: bool,
    gc_request: bool,
    in_recovery_check: bool,
    last_is_complete_del: bool,
    default_kvs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl<S: Storage> Kvdb<S> {
    /// Open (initialising / recovering) a KV database on `storage`.
    ///
    /// `sec_size` is the sector size (a power of two) and `max_size` the total
    /// database size (a whole number of at least two sectors). `default_kvs` are
    /// written when the database is first formatted.
    pub fn new(
        storage: S,
        sec_size: u32,
        max_size: u32,
        default_kvs: Option<&[DefaultKv]>,
    ) -> Result<Self> {
        Self::with_options(storage, sec_size, max_size, default_kvs, false)
    }

    /// Like [`Kvdb::new`] but allows requesting non-formattable mode: when the
    /// stored data fails integrity checks, initialization fails instead of
    /// reformatting.
    pub fn with_options(
        storage: S,
        sec_size: u32,
        max_size: u32,
        default_kvs: Option<&[DefaultKv]>,
        not_formatable: bool,
    ) -> Result<Self> {
        validate_geometry(sec_size, max_size)?;
        let defaults = default_kvs
            .map(|d| {
                d.iter()
                    .map(|kv| (kv.key.to_vec(), kv.value.to_vec()))
                    .collect()
            })
            .unwrap_or_default();
        let mut db = Kvdb {
            storage,
            sec_size,
            max_size,
            oldest_addr: 0,
            init_ok: false,
            not_formatable,
            gc_request: false,
            in_recovery_check: false,
            last_is_complete_del: false,
            default_kvs: defaults,
        };
        db.find_oldest_addr();
        db.kv_load()?;
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

    /// The oldest sector start address (the GC "tail").
    pub fn oldest_addr(&self) -> u32 {
        self.oldest_addr
    }

    fn sector_num(&self) -> usize {
        (self.max_size / self.sec_size) as usize
    }

    // ----------------------------------------------------------------- reads

    /// Port of `read_kv`: read and validate the KV header at `kv.addr_start`.
    fn read_kv(&mut self, kv: &mut Kv) -> Result<()> {
        let mut hdr = [0u8; KV_HDR_DATA_SIZE as usize];
        let _ = self.storage.read(kv.addr_start, &mut hdr);
        kv.status = KvStatus::from_index(get_status(&hdr[0..1], KV_STATUS_NUM));
        let hdr_len = rd_u32(&hdr, 8);
        kv.len = hdr_len;
        let hdr_crc32 = rd_u32(&hdr, 12);
        let hdr_name_len = hdr[16];
        let hdr_value_len = rd_u32(&hdr, 20);

        if kv.len == u32::MAX || kv.len > self.max_size || kv.len < KV_HDR_DATA_SIZE {
            kv.len = KV_HDR_DATA_SIZE;
            if kv.status != KvStatus::ErrHdr {
                kv.status = KvStatus::ErrHdr;
                let _ = write_status(
                    &mut self.storage,
                    kv.addr_start,
                    KV_STATUS_NUM,
                    KvStatus::ErrHdr as usize,
                    true,
                );
            }
            kv.crc_is_ok = false;
            return Err(FdbError::ReadErr);
        }

        // CRC32 over name_len(4) + value_len(4) + value data, matching upstream
        // (the 4-byte reads include the 0xFF header padding).
        let mut calc = calc_crc32(0, &hdr[16..20]);
        calc = calc_crc32(calc, &hdr[20..24]);
        let crc_data_len = kv.len - KV_HDR_DATA_SIZE;
        let mut buf = [0u8; 32];
        let mut len = 0u32;
        while len < crc_data_len {
            let size = core::cmp::min(32, (crc_data_len - len) as usize);
            let _ = self
                .storage
                .read(kv.addr_start + KV_HDR_DATA_SIZE + len, &mut buf[..size]);
            calc = calc_crc32(calc, &buf[..size]);
            len += size as u32;
        }

        if calc != hdr_crc32 {
            let name_len = core::cmp::min(hdr_name_len as usize, KV_NAME_MAX);
            kv.crc_is_ok = false;
            let kv_name_addr = kv.addr_start + KV_HDR_DATA_SIZE;
            let _ = self.storage.read(kv_name_addr, &mut kv.name[..name_len]);
            kv.name_len = hdr_name_len;
            return Err(FdbError::ReadErr);
        }

        kv.crc_is_ok = true;
        let kv_name_addr = kv.addr_start + KV_HDR_DATA_SIZE;
        let read_n = core::cmp::min(hdr_name_len as usize, KV_NAME_MAX);
        let _ = self.storage.read(kv_name_addr, &mut kv.name[..read_n]);
        kv.addr_value = kv_name_addr + wg_align(hdr_name_len as u32);
        kv.value_len = hdr_value_len;
        kv.name_len = hdr_name_len;
        let mut nl = hdr_name_len as usize;
        if nl >= KV_NAME_MAX {
            nl = KV_NAME_MAX - 1;
        }
        kv.name[nl] = 0;
        Ok(())
    }

    /// Port of `find_next_kv_addr` (no cache): scan for the KV magic word.
    fn find_next_kv_addr(&mut self, mut start: u32, end: u32) -> u32 {
        let mut buf = [0u8; 32];
        let start_bak = start;
        while start < end && start + 32 < end {
            if self.storage.read(start, &mut buf).is_err() {
                return FAILED_ADDR;
            }
            let mut i = 0usize;
            while i < 32 - 4 && start + i as u32 <= end {
                let magic =
                    u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
                if magic == KV_MAGIC_WORD
                    && (start + i as u32).wrapping_sub(KV_MAGIC_OFFSET) >= start_bak
                {
                    return start + i as u32 - KV_MAGIC_OFFSET;
                }
                i += 1;
            }
            start += 32 - 4;
        }
        FAILED_ADDR
    }

    /// Port of `get_next_kv_addr`.
    fn get_next_kv_addr(&mut self, sector: &KvSecInfo, pre_kv: &Kv) -> u32 {
        if sector.store == SectorStoreStatus::Empty {
            return FAILED_ADDR;
        }
        if pre_kv.addr_start == FAILED_ADDR {
            return sector.addr + SECTOR_HDR_DATA_SIZE;
        }
        if pre_kv.addr_start <= sector.addr + self.sec_size {
            let mut addr = if pre_kv.crc_is_ok {
                pre_kv.addr_start + pre_kv.len
            } else {
                pre_kv.addr_start + wg_align(1)
            };
            addr = self.find_next_kv_addr(addr, sector.addr + self.sec_size - SECTOR_HDR_DATA_SIZE);
            if addr == FAILED_ADDR || addr > sector.addr + self.sec_size || pre_kv.len == 0 {
                return FAILED_ADDR;
            }
            addr
        } else {
            FAILED_ADDR
        }
    }

    /// Port of `read_sector_info`.
    fn read_sector_info(
        &mut self,
        addr: u32,
        sector: &mut KvSecInfo,
        traversal: bool,
    ) -> Result<()> {
        debug_assert!(addr % self.sec_size == 0);
        let mut hdr = [0u8; SECTOR_HDR_DATA_SIZE as usize];
        let _ = self.storage.read(addr, &mut hdr);

        sector.store = SectorStoreStatus::Unused;
        sector.dirty = SectorDirtyStatus::Unused;
        sector.addr = addr;
        let magic = rd_u32(&hdr, SECTOR_MAGIC_OFFSET as usize);
        let combined = rd_u32(&hdr, 8);
        if magic != SECTOR_MAGIC_WORD
            || (combined != SECTOR_NOT_COMBINED && combined != SECTOR_COMBINED)
        {
            sector.check_ok = false;
            sector.combined = SECTOR_NOT_COMBINED;
            sector.empty_kv = FAILED_ADDR;
            sector.remain = 0;
            return Err(FdbError::InitFailed);
        }
        sector.check_ok = true;
        sector.combined = combined;
        sector.store = SectorStoreStatus::from_index(get_status(&hdr[0..1], SECTOR_STORE_STATUS_NUM));
        sector.dirty =
            SectorDirtyStatus::from_index(get_status(&hdr[1..2], SECTOR_DIRTY_STATUS_NUM));

        if !traversal {
            sector.empty_kv = FAILED_ADDR;
            sector.remain = 0;
            return Ok(());
        }

        let mut result = Ok(());
        sector.remain = 0;
        sector.empty_kv = sector.addr + SECTOR_HDR_DATA_SIZE;
        if sector.store == SectorStoreStatus::Empty {
            sector.remain = (self.sec_size - SECTOR_HDR_DATA_SIZE) as i64;
        } else if sector.store == SectorStoreStatus::Using {
            sector.remain = (self.sec_size - SECTOR_HDR_DATA_SIZE) as i64;
            let mut kv_obj = Kv::blank();
            kv_obj.addr_start = sector.addr + SECTOR_HDR_DATA_SIZE;
            loop {
                let _ = self.read_kv(&mut kv_obj);
                if !kv_obj.crc_is_ok
                    && kv_obj.status != KvStatus::PreWrite
                    && kv_obj.status != KvStatus::ErrHdr
                {
                    sector.remain = 0;
                    result = Err(FdbError::ReadErr);
                    break;
                }
                sector.empty_kv += kv_obj.len;
                sector.remain -= kv_obj.len as i64;
                let next = self.get_next_kv_addr(sector, &kv_obj);
                if next == FAILED_ADDR {
                    break;
                }
                kv_obj.addr_start = next;
            }
            let ff_addr =
                continue_ff_addr(&mut self.storage, sector.empty_kv, sector.addr + self.sec_size);
            if sector.empty_kv != ff_addr {
                sector.empty_kv = ff_addr;
                sector.remain = (self.sec_size - (ff_addr - sector.addr)) as i64;
            }
        }
        result
    }

    /// Port of `get_next_sector_addr`.
    fn get_next_sector_addr(&self, pre_sec: &KvSecInfo, traversed_len: u32) -> u32 {
        let cur_block_size = if pre_sec.combined == SECTOR_NOT_COMBINED {
            self.sec_size
        } else {
            pre_sec.combined * self.sec_size
        };
        if traversed_len + cur_block_size <= self.max_size {
            if pre_sec.addr + cur_block_size < self.max_size {
                pre_sec.addr + cur_block_size
            } else {
                0
            }
        } else {
            FAILED_ADDR
        }
    }
}

impl<S: Storage> Kvdb<S> {
    // --------------------------------------------------------------- writes

    /// Port of `format_sector` (granularity-1 path).
    fn format_sector(&mut self, addr: u32, combined_value: u32) -> Result<()> {
        debug_assert!(addr % self.sec_size == 0);
        self.storage.erase(addr, self.sec_size)?;
        let mut hdr = [0xFFu8; 16];
        set_status(&mut hdr[0..1], SECTOR_STORE_STATUS_NUM, SectorStoreStatus::Empty as usize);
        set_status(&mut hdr[1..2], SECTOR_DIRTY_STATUS_NUM, SectorDirtyStatus::False as usize);
        hdr[SECTOR_MAGIC_OFFSET as usize..SECTOR_MAGIC_OFFSET as usize + 4]
            .copy_from_slice(&SECTOR_MAGIC_WORD.to_le_bytes());
        hdr[8..12].copy_from_slice(&combined_value.to_le_bytes());
        hdr[12..16].copy_from_slice(&crate::def::DATA_UNUSED.to_le_bytes());
        self.storage.write(addr, &hdr, true)?;
        Ok(())
    }

    /// Port of `update_sec_status`; returns whether the sector became full.
    fn update_sec_status(&mut self, sector: &mut KvSecInfo, new_kv_len: u32) -> Result<bool> {
        let mut is_full = false;
        if sector.store == SectorStoreStatus::Empty {
            write_status(
                &mut self.storage,
                sector.addr,
                SECTOR_STORE_STATUS_NUM,
                SectorStoreStatus::Using as usize,
                true,
            )?;
        } else if sector.store == SectorStoreStatus::Using
            && (sector.remain < SEC_REMAIN_THRESHOLD
                || sector.remain - (new_kv_len as i64) < SEC_REMAIN_THRESHOLD)
        {
            write_status(
                &mut self.storage,
                sector.addr,
                SECTOR_STORE_STATUS_NUM,
                SectorStoreStatus::Full as usize,
                true,
            )?;
            is_full = true;
        }
        Ok(is_full)
    }

    /// Port of `write_kv_hdr`.
    fn write_kv_hdr(&mut self, addr: u32, hdr: &[u8; 24]) -> Result<()> {
        write_status(
            &mut self.storage,
            addr,
            KV_STATUS_NUM,
            KvStatus::PreWrite as usize,
            false,
        )?;
        self.storage
            .write(addr + KV_MAGIC_OFFSET, &hdr[KV_MAGIC_OFFSET as usize..], false)?;
        Ok(())
    }

    /// Count empty and using sectors (`sector_statistics_cb`).
    fn count_sectors(&mut self) -> (usize, usize) {
        let mut empty = 0usize;
        let mut using = 0usize;
        let mut sec_addr = self.oldest_addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut s = KvSecInfo::blank();
            let _ = self.read_sector_info(sec_addr, &mut s, false);
            if s.check_ok && s.store == SectorStoreStatus::Empty {
                empty += 1;
            } else if s.check_ok && s.store == SectorStoreStatus::Using {
                using += 1;
            }
            let next = self.get_next_sector_addr(&s, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        (empty, using)
    }

    /// Scan sectors of a given store status for one with room (`alloc_kv_cb`).
    fn alloc_scan(&mut self, want: SectorStoreStatus, kv_size: u32) -> Option<KvSecInfo> {
        let mut sec_addr = self.oldest_addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut s = KvSecInfo::blank();
            let _ = self.read_sector_info(sec_addr, &mut s, false);
            if s.store == want {
                let _ = self.read_sector_info(sec_addr, &mut s, true);
                if s.check_ok
                    && s.remain > kv_size as i64 + SEC_REMAIN_THRESHOLD
                    && (s.dirty == SectorDirtyStatus::False
                        || (s.dirty == SectorDirtyStatus::True && !self.gc_request))
                {
                    return Some(s);
                }
            }
            let next = self.get_next_sector_addr(&s, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        None
    }

    /// Port of `alloc_kv`.
    fn alloc_kv(&mut self, sector: &mut KvSecInfo, kv_size: u32) -> u32 {
        let (empty_count, using_count) = self.count_sectors();
        let mut empty_kv = FAILED_ADDR;
        if using_count > 0 {
            if let Some(s) = self.alloc_scan(SectorStoreStatus::Using, kv_size) {
                *sector = s;
                empty_kv = s.empty_kv;
            }
        }
        if empty_count > 0 && empty_kv == FAILED_ADDR {
            if empty_count > GC_EMPTY_SEC_THRESHOLD || self.gc_request {
                if let Some(s) = self.alloc_scan(SectorStoreStatus::Empty, kv_size) {
                    *sector = s;
                    empty_kv = s.empty_kv;
                }
            } else {
                self.gc_request = true;
            }
        }
        empty_kv
    }

    /// Port of `new_kv`: allocate, GC-then-retry once if needed.
    fn new_kv(&mut self, sector: &mut KvSecInfo, kv_size: u32) -> u32 {
        let mut already_gc = false;
        loop {
            let empty_kv = self.alloc_kv(sector, kv_size);
            if empty_kv != FAILED_ADDR {
                return empty_kv;
            }
            if self.gc_request && !already_gc {
                self.gc_collect_by_free_size(kv_size);
                already_gc = true;
                continue;
            } else if already_gc {
                self.gc_request = false;
            }
            return FAILED_ADDR;
        }
    }

    fn new_kv_ex(&mut self, sector: &mut KvSecInfo, key_len: u32, buf_len: u32) -> u32 {
        let kv_len = KV_HDR_DATA_SIZE + wg_align(key_len) + wg_align(buf_len);
        self.new_kv(sector, kv_len)
    }

    /// Port of `create_kv_blob`.
    fn create_kv_blob(&mut self, sector: &mut KvSecInfo, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > KV_NAME_MAX {
            return Err(FdbError::KvNameErr);
        }
        let name_len = key.len() as u32;
        let value_len = value.len() as u32;
        let kv_len = KV_HDR_DATA_SIZE + wg_align(name_len) + wg_align(value_len);
        if kv_len > self.sec_size - SECTOR_HDR_DATA_SIZE {
            return Err(FdbError::SavedFull);
        }
        let mut kv_addr = sector.empty_kv;
        if kv_addr == FAILED_ADDR {
            kv_addr = self.new_kv(sector, kv_len);
        }
        if kv_addr == FAILED_ADDR {
            return Err(FdbError::SavedFull);
        }
        let is_full = self.update_sec_status(sector, kv_len)?;

        let mut hdr = [0xFFu8; 24];
        hdr[4..8].copy_from_slice(&KV_MAGIC_WORD.to_le_bytes());
        hdr[8..12].copy_from_slice(&kv_len.to_le_bytes());
        hdr[16] = name_len as u8;
        hdr[20..24].copy_from_slice(&value_len.to_le_bytes());
        let mut crc = calc_crc32(0, &hdr[16..20]);
        crc = calc_crc32(crc, &hdr[20..24]);
        crc = calc_crc32(crc, key);
        crc = calc_crc32(crc, value);
        hdr[12..16].copy_from_slice(&crc.to_le_bytes());

        self.write_kv_hdr(kv_addr, &hdr)?;
        write_align(&mut self.storage, kv_addr + KV_HDR_DATA_SIZE, key)?;
        write_align(
            &mut self.storage,
            kv_addr + KV_HDR_DATA_SIZE + wg_align(name_len),
            value,
        )?;
        write_status(
            &mut self.storage,
            kv_addr,
            KV_STATUS_NUM,
            KvStatus::Write as usize,
            true,
        )?;
        if is_full {
            self.gc_request = true;
        }
        Ok(())
    }

    /// Port of `del_kv`.
    fn del_kv(&mut self, key: Option<&[u8]>, old_kv: Option<&Kv>, complete_del: bool) -> Result<()> {
        let mut local = Kv::blank();
        let addr_start = match old_kv {
            Some(k) => k.addr_start,
            None => {
                if self.find_kv(key.unwrap(), &mut local) {
                    local.addr_start
                } else {
                    return Err(FdbError::KvNameErr);
                }
            }
        };

        if !complete_del {
            write_status(
                &mut self.storage,
                addr_start,
                KV_STATUS_NUM,
                KvStatus::PreDelete as usize,
                false,
            )?;
            self.last_is_complete_del = true;
        } else {
            write_status(
                &mut self.storage,
                addr_start,
                KV_STATUS_NUM,
                KvStatus::Deleted as usize,
                true,
            )?;
            self.last_is_complete_del = false;
        }

        let dirty_status_addr = align_down(addr_start, self.sec_size) + SECTOR_DIRTY_OFFSET;
        if read_status(&mut self.storage, dirty_status_addr, SECTOR_DIRTY_STATUS_NUM)
            == SectorDirtyStatus::False as usize
        {
            write_status(
                &mut self.storage,
                dirty_status_addr,
                SECTOR_DIRTY_STATUS_NUM,
                SectorDirtyStatus::True as usize,
                true,
            )?;
        }
        Ok(())
    }

    /// Port of `move_kv`.
    fn move_kv(&mut self, kv: &Kv) -> Result<()> {
        if kv.status == KvStatus::Write {
            let _ = self.del_kv(None, Some(kv), false);
        }
        let mut sector = KvSecInfo::blank();
        let kv_addr = self.alloc_kv(&mut sector, kv.len);
        if kv_addr == FAILED_ADDR {
            return Err(FdbError::SavedFull);
        }
        if self.in_recovery_check && kv.status == KvStatus::PreDelete {
            let nl = kv.name_len as usize;
            let mut kv_bak = Kv::blank();
            if self.find_kv_no_cache(&kv.name[..nl], &mut kv_bak) {
                let _ = self.del_kv(None, Some(kv), true);
                return Ok(());
            }
        }
        let result = self.move_kv_body(kv, &mut sector, kv_addr);
        let _ = self.del_kv(None, Some(kv), true);
        result
    }

    fn move_kv_body(&mut self, kv: &Kv, sector: &mut KvSecInfo, kv_addr: u32) -> Result<()> {
        let _ = self.update_sec_status(sector, kv.len)?;
        write_status(
            &mut self.storage,
            kv_addr,
            KV_STATUS_NUM,
            KvStatus::PreWrite as usize,
            false,
        )?;
        let kv_len = kv.len - KV_MAGIC_OFFSET;
        let mut buf = [0u8; 32];
        let mut len = 0u32;
        while len < kv_len {
            let size = core::cmp::min(32, (kv_len - len) as usize);
            let _ = self
                .storage
                .read(kv.addr_start + KV_MAGIC_OFFSET + len, &mut buf[..size]);
            self.storage
                .write(kv_addr + KV_MAGIC_OFFSET + len, &buf[..size], true)?;
            len += size as u32;
        }
        write_status(
            &mut self.storage,
            kv_addr,
            KV_STATUS_NUM,
            KvStatus::Write as usize,
            true,
        )?;
        Ok(())
    }

    // -------------------------------------------------------------- garbage collection

    /// Port of `do_gc` for a single sector. Returns true to stop the GC walk.
    fn do_gc_one(
        &mut self,
        sector: &mut KvSecInfo,
        setting_free_size: u32,
        last_gc_sec_addr: &mut u32,
    ) -> bool {
        if !(sector.check_ok
            && (sector.dirty == SectorDirtyStatus::True || sector.dirty == SectorDirtyStatus::Gc))
        {
            return false;
        }
        let _ = write_status(
            &mut self.storage,
            sector.addr + SECTOR_DIRTY_OFFSET,
            SECTOR_DIRTY_STATUS_NUM,
            SectorDirtyStatus::Gc as usize,
            true,
        );
        let mut kv = Kv::blank();
        kv.addr_start = sector.addr + SECTOR_HDR_DATA_SIZE;
        loop {
            let _ = self.read_kv(&mut kv);
            if kv.crc_is_ok && (kv.status == KvStatus::Write || kv.status == KvStatus::PreDelete) {
                let _ = self.move_kv(&kv);
            }
            let next = self.get_next_kv_addr(sector, &kv);
            if next == FAILED_ADDR {
                break;
            }
            kv.addr_start = next;
        }
        let _ = self.format_sector(sector.addr, SECTOR_NOT_COMBINED);
        let prev = *last_gc_sec_addr;
        *last_gc_sec_addr = sector.addr;
        self.oldest_addr = self.get_next_sector_addr(sector, 0);
        let mut last_gc_sector = KvSecInfo::blank();
        if self.read_sector_info(prev, &mut last_gc_sector, true).is_ok()
            && last_gc_sector.remain > setting_free_size as i64
        {
            return true;
        }
        false
    }

    /// Port of `gc_collect_by_free_size`.
    fn gc_collect_by_free_size(&mut self, free_size: u32) {
        // Count empty sectors and remember the last empty sector address.
        let mut empty_sec_num = 0usize;
        let mut empty_sec_addr = 0u32;
        {
            let mut sec_addr = self.oldest_addr;
            let mut traversed_len = 0u32;
            loop {
                traversed_len += self.sec_size;
                let mut s = KvSecInfo::blank();
                let _ = self.read_sector_info(sec_addr, &mut s, false);
                if s.store == SectorStoreStatus::Empty && s.check_ok {
                    empty_sec_num += 1;
                    empty_sec_addr = s.addr;
                }
                let next = self.get_next_sector_addr(&s, traversed_len);
                if next == FAILED_ADDR {
                    break;
                }
                sec_addr = next;
            }
        }

        if empty_sec_num <= GC_EMPTY_SEC_THRESHOLD {
            let mut last_gc_sec_addr = empty_sec_addr;
            let mut sec_addr = self.oldest_addr;
            let mut traversed_len = 0u32;
            loop {
                traversed_len += self.sec_size;
                let mut s = KvSecInfo::blank();
                let _ = self.read_sector_info(sec_addr, &mut s, false);
                if self.do_gc_one(&mut s, free_size, &mut last_gc_sec_addr) {
                    break;
                }
                let next = self.get_next_sector_addr(&s, traversed_len);
                if next == FAILED_ADDR {
                    break;
                }
                sec_addr = next;
            }
        }
        self.gc_request = false;
    }

    fn gc_collect(&mut self) {
        self.gc_collect_by_free_size(self.max_size);
    }

    // ----------------------------------------------------------------- find

    fn find_kv(&mut self, key: &[u8], kv: &mut Kv) -> bool {
        self.find_kv_no_cache(key, kv)
    }

    /// Port of `find_kv_no_cache`.
    fn find_kv_no_cache(&mut self, key: &[u8], kv: &mut Kv) -> bool {
        let mut sec_addr = self.oldest_addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut sector = KvSecInfo::blank();
            if self.read_sector_info(sec_addr, &mut sector, false).is_ok()
                && (sector.store == SectorStoreStatus::Using
                    || sector.store == SectorStoreStatus::Full)
            {
                kv.addr_start = sector.addr + SECTOR_HDR_DATA_SIZE;
                loop {
                    let _ = self.read_kv(kv);
                    if kv.crc_is_ok
                        && kv.status == KvStatus::Write
                        && kv.name_len as usize == key.len()
                        && &kv.name[..kv.name_len as usize] == key
                    {
                        return true;
                    }
                    let next = self.get_next_kv_addr(&sector, kv);
                    if next == FAILED_ADDR {
                        break;
                    }
                    kv.addr_start = next;
                }
            }
            let next = self.get_next_sector_addr(&sector, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        false
    }

    // ----------------------------------------------------------------- init / load

    /// Port of the `check_oldest_addr_cb` walk in `fdb_kvdb_init`.
    fn find_oldest_addr(&mut self) {
        self.oldest_addr = 0;
        let mut sector_oldest_addr = 0u32;
        let mut last_status = SectorStoreStatus::Unused;
        let mut sec_addr = 0u32;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut s = KvSecInfo::blank();
            let _ = self.read_sector_info(sec_addr, &mut s, false);
            if last_status == SectorStoreStatus::Empty
                && (s.store == SectorStoreStatus::Full || s.store == SectorStoreStatus::Using)
            {
                sector_oldest_addr = s.addr;
            }
            last_status = s.store;
            let next = self.get_next_sector_addr(&s, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        self.oldest_addr = sector_oldest_addr;
    }

    /// Port of `_fdb_kv_load`.
    fn kv_load(&mut self) -> Result<()> {
        self.in_recovery_check = true;
        // Check sector headers, formatting bad ones (unless not_formatable).
        let mut check_failed_count = 0usize;
        {
            let mut sec_addr = self.oldest_addr;
            let mut traversed_len = 0u32;
            loop {
                traversed_len += self.sec_size;
                let mut s = KvSecInfo::blank();
                let _ = self.read_sector_info(sec_addr, &mut s, false);
                if !s.check_ok {
                    check_failed_count += 1;
                    if self.not_formatable {
                        break;
                    }
                    let _ = self.format_sector(s.addr, SECTOR_NOT_COMBINED);
                }
                let next = self.get_next_sector_addr(&s, traversed_len);
                if next == FAILED_ADDR {
                    break;
                }
                sec_addr = next;
            }
        }
        if self.not_formatable && check_failed_count > 0 {
            return Err(FdbError::ReadErr);
        }
        if check_failed_count == self.sector_num() {
            let _ = self.set_default();
        }
        // Resume an interrupted GC.
        {
            let mut sec_addr = self.oldest_addr;
            let mut traversed_len = 0u32;
            loop {
                traversed_len += self.sec_size;
                let mut s = KvSecInfo::blank();
                let _ = self.read_sector_info(sec_addr, &mut s, false);
                if s.check_ok && s.dirty == SectorDirtyStatus::Gc {
                    self.gc_request = true;
                    self.gc_collect();
                }
                let next = self.get_next_sector_addr(&s, traversed_len);
                if next == FAILED_ADDR {
                    break;
                }
                sec_addr = next;
            }
        }
        // Recover any half-written / pre-deleted KVs, GC-and-retry if needed.
        loop {
            self.recover_kvs_pass();
            if self.gc_request {
                self.gc_collect();
                continue;
            }
            break;
        }
        self.in_recovery_check = false;
        Ok(())
    }

    fn recover_kvs_pass(&mut self) {
        let mut sec_addr = self.oldest_addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut s = KvSecInfo::blank();
            if self.read_sector_info(sec_addr, &mut s, false).is_ok()
                && (s.store == SectorStoreStatus::Using || s.store == SectorStoreStatus::Full)
            {
                let mut kv = Kv::blank();
                kv.addr_start = s.addr + SECTOR_HDR_DATA_SIZE;
                loop {
                    let _ = self.read_kv(&mut kv);
                    if self.recover_one_kv(&kv) {
                        return;
                    }
                    let next = self.get_next_kv_addr(&s, &kv);
                    if next == FAILED_ADDR {
                        break;
                    }
                    kv.addr_start = next;
                }
            }
            let next = self.get_next_sector_addr(&s, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
    }

    /// Port of `check_and_recovery_kv_cb`. Returns true to stop the pass.
    fn recover_one_kv(&mut self, kv: &Kv) -> bool {
        if kv.crc_is_ok && kv.status == KvStatus::PreDelete {
            self.move_kv(kv).is_err()
        } else if kv.status == KvStatus::PreWrite {
            let _ = write_status(
                &mut self.storage,
                kv.addr_start,
                KV_STATUS_NUM,
                KvStatus::ErrHdr as usize,
                true,
            );
            true
        } else {
            false
        }
    }

    // ----------------------------------------------------------------- public API

    /// Set (create or replace) a KV with a blob value.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if !self.init_ok {
            return Err(FdbError::InitFailed);
        }
        self.set_kv(key, value)
    }

    /// Set a KV with a string value.
    pub fn set_str(&mut self, key: &[u8], value: &str) -> Result<()> {
        self.set(key, value.as_bytes())
    }

    /// Port of `set_kv` (non-delete branch).
    fn set_kv(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut cur_sector = KvSecInfo::blank();
        if self.new_kv_ex(&mut cur_sector, key.len() as u32, value.len() as u32) == FAILED_ADDR {
            return Err(FdbError::SavedFull);
        }
        let mut cur_kv = Kv::blank();
        let kv_is_found = self.find_kv(key, &mut cur_kv);
        let mut result = Ok(());
        if kv_is_found {
            result = self.del_kv(Some(key), Some(&cur_kv), false);
        }
        if result.is_ok() {
            result = self.create_kv_blob(&mut cur_sector, key, value);
        }
        if kv_is_found && result.is_ok() {
            result = self.del_kv(Some(key), Some(&cur_kv), true);
        }
        if self.gc_request {
            let fs = KV_HDR_DATA_SIZE + wg_align(key.len() as u32) + wg_align(value.len() as u32);
            self.gc_collect_by_free_size(fs);
        }
        result
    }

    /// Delete a KV by name.
    pub fn del(&mut self, key: &[u8]) -> Result<()> {
        if !self.init_ok {
            return Err(FdbError::InitFailed);
        }
        self.del_kv(Some(key), None, true)
    }

    /// Read a KV value into `buf`, returning the number of bytes copied
    /// (`min(buf.len(), value_len)`), or `None` if the KV does not exist.
    pub fn get(&mut self, key: &[u8], buf: &mut [u8]) -> Option<usize> {
        if !self.init_ok {
            return None;
        }
        let mut kv = Kv::blank();
        if self.find_kv(key, &mut kv) {
            let read_len = core::cmp::min(buf.len(), kv.value_len as usize);
            let _ = self.storage.read(kv.addr_value, &mut buf[..read_len]);
            Some(read_len)
        } else {
            None
        }
    }

    /// Read the full value of a KV into a freshly allocated `Vec`.
    pub fn get_vec(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        if !self.init_ok {
            return None;
        }
        let mut kv = Kv::blank();
        if self.find_kv(key, &mut kv) {
            let mut v = vec![0u8; kv.value_len as usize];
            let _ = self.storage.read(kv.addr_value, &mut v);
            Some(v)
        } else {
            None
        }
    }

    /// Look up a KV and return its metadata object (`fdb_kv_get_obj`).
    pub fn get_obj(&mut self, key: &[u8]) -> Option<Kv> {
        if !self.init_ok {
            return None;
        }
        let mut kv = Kv::blank();
        if self.find_kv(key, &mut kv) {
            Some(kv)
        } else {
            None
        }
    }

    /// Read a KV object's value into `buf` (`fdb_blob_read` of `fdb_kv_to_blob`).
    pub fn read_value(&mut self, kv: &Kv, buf: &mut [u8]) -> usize {
        let read_len = core::cmp::min(buf.len(), kv.value_len as usize);
        if self.storage.read(kv.addr_value, &mut buf[..read_len]).is_err() {
            return 0;
        }
        read_len
    }

    /// Get a string KV (`fdb_kv_get`): returns the value only if it is a
    /// printable string that fits in the legacy buffer.
    pub fn get_str(&mut self, key: &[u8]) -> Option<String> {
        if !self.init_ok {
            return None;
        }
        let mut kv = Kv::blank();
        if !self.find_kv(key, &mut kv) {
            return None;
        }
        let read = core::cmp::min(kv.value_len as usize, STR_KV_VALUE_MAX_SIZE);
        let mut buf = [0u8; STR_KV_VALUE_MAX_SIZE];
        let _ = self.storage.read(kv.addr_value, &mut buf[..read]);
        if is_str(&buf[..read]) {
            Some(String::from_utf8_lossy(&buf[..read]).into_owned())
        } else {
            None
        }
    }

    /// Reformat the whole database and write the default KV set
    /// (`fdb_kv_set_default`).
    pub fn set_default(&mut self) -> Result<()> {
        let mut addr = 0u32;
        while addr < self.max_size {
            self.format_sector(addr, SECTOR_NOT_COMBINED)?;
            addr += self.sec_size;
        }
        let defaults = core::mem::take(&mut self.default_kvs);
        for (k, v) in &defaults {
            let mut sector = KvSecInfo::blank();
            sector.empty_kv = FAILED_ADDR;
            let _ = self.create_kv_blob(&mut sector, k, v);
        }
        self.default_kvs = defaults;
        self.oldest_addr = 0;
        Ok(())
    }

    /// Integrity check (`fdb_kvdb_check`).
    pub fn check(&mut self) -> Result<()> {
        if !self.init_ok {
            return Err(FdbError::InitFailed);
        }
        let mut sec_addr = self.oldest_addr;
        let mut traversed_len = 0u32;
        loop {
            traversed_len += self.sec_size;
            let mut sector = KvSecInfo::blank();
            if self.read_sector_info(sec_addr, &mut sector, false).is_err() {
                return Err(FdbError::InitFailed);
            }
            if sector.store == SectorStoreStatus::Using || sector.store == SectorStoreStatus::Full {
                let mut kv = Kv::blank();
                kv.addr_start = sector.addr + SECTOR_HDR_DATA_SIZE;
                loop {
                    let rr = self.read_kv(&mut kv);
                    let next = self.get_next_kv_addr(&sector, &kv);
                    rr?;
                    if next == FAILED_ADDR {
                        break;
                    }
                    kv.addr_start = next;
                }
            }
            let next = self.get_next_sector_addr(&sector, traversed_len);
            if next == FAILED_ADDR {
                break;
            }
            sec_addr = next;
        }
        Ok(())
    }

    /// Create an iterator over all live KVs (`fdb_kv_iterator_init`).
    pub fn iterator(&self) -> KvIterator {
        let mut kv = Kv::blank();
        kv.addr_start = 0;
        KvIterator {
            curr_kv: kv,
            iterated_cnt: 0,
            iterated_obj_bytes: 0,
            iterated_value_bytes: 0,
            sector_addr: self.oldest_addr,
            traversed_len: 0,
        }
    }

    /// Advance the iterator (`fdb_kv_iterate`). Returns true while a KV is
    /// available at [`KvIterator::current`].
    pub fn iterate(&mut self, it: &mut KvIterator) -> bool {
        let mut sector = KvSecInfo::blank();
        loop {
            if self.read_sector_info(it.sector_addr, &mut sector, false).is_ok()
                && (sector.store == SectorStoreStatus::Using
                    || sector.store == SectorStoreStatus::Full)
            {
                if it.curr_kv.addr_start == 0 {
                    it.curr_kv.addr_start = sector.addr + SECTOR_HDR_DATA_SIZE;
                } else {
                    let next = self.get_next_kv_addr(&sector, &it.curr_kv);
                    if next == FAILED_ADDR {
                        it.curr_kv.addr_start = 0;
                        it.traversed_len += self.sec_size;
                        let nsec = self.get_next_sector_addr(&sector, it.traversed_len);
                        if nsec == FAILED_ADDR {
                            return false;
                        }
                        it.sector_addr = nsec;
                        continue;
                    }
                    it.curr_kv.addr_start = next;
                }
                loop {
                    let _ = self.read_kv(&mut it.curr_kv);
                    if it.curr_kv.status == KvStatus::Write && it.curr_kv.crc_is_ok {
                        it.iterated_cnt += 1;
                        it.iterated_obj_bytes += it.curr_kv.len as usize;
                        it.iterated_value_bytes += it.curr_kv.value_len as usize;
                        return true;
                    }
                    let next = self.get_next_kv_addr(&sector, &it.curr_kv);
                    if next == FAILED_ADDR {
                        break;
                    }
                    it.curr_kv.addr_start = next;
                }
            }
            it.curr_kv.addr_start = 0;
            it.traversed_len += self.sec_size;
            let nsec = self.get_next_sector_addr(&sector, it.traversed_len);
            if nsec == FAILED_ADDR {
                return false;
            }
            it.sector_addr = nsec;
        }
    }

    /// Collect all live KVs into a `Vec` (convenience wrapper over the iterator).
    pub fn iter_collect(&mut self) -> Vec<Kv> {
        let mut out = Vec::new();
        let mut it = self.iterator();
        while self.iterate(&mut it) {
            out.push(it.current().clone());
        }
        out
    }

    /// Render all live string KVs as `name=value` lines (`fdb_kv_print`).
    pub fn dump(&mut self) -> String {
        let kvs = self.iter_collect();
        let mut out = String::new();
        for kv in &kvs {
            let name = String::from_utf8_lossy(kv.name());
            let mut val = vec![0u8; kv.value_len as usize];
            let _ = self.storage.read(kv.addr_value, &mut val);
            out.push_str(&name);
            out.push('=');
            if is_str(&val) {
                out.push_str(&String::from_utf8_lossy(&val));
            } else {
                out.push_str(&alloc::format!("blob @0x{:08X} {} bytes", kv.addr_value, kv.value_len));
            }
            out.push('\n');
        }
        out
    }

    /// Print all live KVs to stdout.
    #[cfg(feature = "std")]
    pub fn print(&mut self) {
        let s = self.dump();
        std::print!("{s}");
    }
}

/// Port of `fdb_is_str`: true when every byte is printable ASCII.
fn is_str(value: &[u8]) -> bool {
    value
        .iter()
        .all(|&ch| (ch.wrapping_sub(b' ') as u32) < (127u32 - b' ' as u32))
}
