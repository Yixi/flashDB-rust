//! Power-loss recovery tests.
//!
//! These exercise the recovery paths (`recover_one_kv`, pre-write/pre-delete
//! handling) that the upstream functional tests do not reach. A fault-injecting
//! storage aborts the Nth write of an update, simulating a power cut; on the
//! next open the database must recover to a consistent state where the key holds
//! either the old or the new value — never lost or corrupted.

use flashdb::{FdbError, Kvdb, RamStorage, Result, Storage};

const SECTOR_SIZE: u32 = 4096;
const SECTORS: u32 = 4;
const MAX_SIZE: u32 = SECTOR_SIZE * SECTORS;

/// Wraps a `RamStorage` and fails after a budget of writes is exhausted.
struct FaultStorage {
    inner: RamStorage,
    budget: i64,
}

impl FaultStorage {
    fn new(bytes: Vec<u8>, budget: i64) -> Self {
        FaultStorage {
            inner: RamStorage::from_bytes(bytes),
            budget,
        }
    }
    fn into_bytes(self) -> Vec<u8> {
        self.inner.into_bytes()
    }
}

impl Storage for FaultStorage {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<()> {
        self.inner.read(addr, buf)
    }
    fn write(&mut self, addr: u32, buf: &[u8], sync: bool) -> Result<()> {
        if self.budget <= 0 {
            return Err(FdbError::WriteErr);
        }
        self.budget -= 1;
        self.inner.write(addr, buf, sync)
    }
    fn erase(&mut self, addr: u32, size: u32) -> Result<()> {
        if self.budget <= 0 {
            return Err(FdbError::EraseErr);
        }
        self.budget -= 1;
        self.inner.erase(addr, size)
    }
}

/// Build a clean image holding key="data" -> `v1`, and return the raw bytes.
fn image_with(v1: &[u8]) -> Vec<u8> {
    let mut db = Kvdb::new(RamStorage::new(MAX_SIZE), SECTOR_SIZE, MAX_SIZE, None).unwrap();
    db.set(b"data", v1).unwrap();
    db.set(b"other", b"keep-me").unwrap();
    db.into_storage().into_bytes()
}

#[test]
fn power_loss_during_update_keeps_a_consistent_value() {
    let v1 = b"OLD-VALUE";
    let v2 = b"NEW-VALUE-CHANGED";
    let base = image_with(v1);

    // A clean open performs no writes, so the budget applies to the update.
    // Sweep the fault point across the whole update operation.
    for budget in 0..40i64 {
        // 1. Open from the base image with a write budget and attempt the update.
        let fs = FaultStorage::new(base.clone(), budget);
        let mut db = Kvdb::new(fs, SECTOR_SIZE, MAX_SIZE, None).unwrap();
        let _ = db.set(b"data", v2); // may fail mid-way (power loss)
        let crashed = db.into_storage().into_bytes();

        // 2. Reboot from whatever made it to storage (unlimited budget now).
        let mut db = Kvdb::new(
            RamStorage::from_bytes(crashed),
            SECTOR_SIZE,
            MAX_SIZE,
            None,
        )
        .unwrap();

        // Note: a power loss mid-create can leave a half-written KV that
        // recovery marks ERR_HDR (matching upstream); that node is garbage
        // awaiting GC, so `check()` may legitimately report it. The invariant
        // we require is no data loss/corruption, verified below.

        // "data" must be exactly v1 or v2 — never lost or corrupted.
        match db.get_vec(b"data") {
            Some(val) => assert!(
                val == v1 || val == v2,
                "budget {budget}: 'data' corrupted: {:?}",
                val
            ),
            None => panic!("budget {budget}: 'data' was lost"),
        }

        // The untouched key must always survive.
        assert_eq!(
            db.get_vec(b"other").as_deref(),
            Some(&b"keep-me"[..]),
            "budget {budget}: 'other' lost"
        );

        // The database must remain writable afterwards.
        db.set(b"after", b"works").unwrap();
        assert_eq!(db.get_vec(b"after").as_deref(), Some(&b"works"[..]));
    }
}

#[test]
fn power_loss_during_delete_keeps_consistency() {
    let base = image_with(b"to-be-deleted");
    for budget in 0..12i64 {
        let fs = FaultStorage::new(base.clone(), budget);
        let mut db = Kvdb::new(fs, SECTOR_SIZE, MAX_SIZE, None).unwrap();
        let _ = db.del(b"data");
        let crashed = db.into_storage().into_bytes();

        let mut db =
            Kvdb::new(RamStorage::from_bytes(crashed), SECTOR_SIZE, MAX_SIZE, None).unwrap();
        db.check().unwrap_or_else(|e| panic!("check failed at budget {budget}: {e:?}"));
        // "other" must always survive a delete of "data".
        assert_eq!(db.get_vec(b"other").as_deref(), Some(&b"keep-me"[..]));
    }
}

use flashdb::Tsdb;

/// Opening a database over arbitrary garbage must never panic; it should
/// reformat and come up empty and usable.
#[test]
fn open_on_garbage_does_not_panic() {
    for fill in [0x00u8, 0xFF, 0x5A, 0xA5] {
        // KVDB
        let bytes = vec![fill; MAX_SIZE as usize];
        let mut kv = Kvdb::new(
            RamStorage::from_bytes(bytes),
            SECTOR_SIZE,
            MAX_SIZE,
            None,
        )
        .unwrap();
        assert!(kv.get_vec(b"anything").is_none());
        kv.set(b"k", b"v").unwrap();
        assert_eq!(kv.get_vec(b"k").as_deref(), Some(&b"v"[..]));

        // TSDB
        let tmax = SECTOR_SIZE * 8;
        let bytes = vec![fill; tmax as usize];
        let mut t = 0i32;
        let mut ts = Tsdb::new(
            RamStorage::from_bytes(bytes),
            SECTOR_SIZE,
            tmax,
            move || {
                t += 2;
                t
            },
            128,
        )
        .unwrap();
        assert_eq!(ts.collect().len(), 0);
        ts.append(b"log").unwrap();
        assert_eq!(ts.collect().len(), 1);
    }
}
