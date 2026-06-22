//! KVDB test suite, ported from `tests/fdb_kvdb_tc.c`.
//!
//! Uses the file storage backend (file mode), like upstream. The "reboot"
//! helper re-opens the database on the same directory to verify that on-disk
//! state — in particular the oldest-sector address after garbage collection —
//! is recovered correctly.

use flashdb::{FileStorage, Kvdb};
use std::path::{Path, PathBuf};

const SECTOR_SIZE: u32 = 4096;

// TEST_KV_VALUE_LEN, computed exactly as the upstream macro does for
// FDB_WRITE_GRAN == 1 (raw, unpadded header sizes), giving 1005.
const SEC_HDR_RAW: u32 = 1 + 1 + 4 + 4 + 4; // store + dirty + magic + combined + reserved
const KV_HDR_RAW: u32 = 1 + 4 + 4 + 4 + 1 + 4; // status + magic + len + crc32 + name_len + value_len
const NAME_ALIGNED: u32 = 3;
const USABLE: u32 = SECTOR_SIZE - SEC_HDR_RAW;
const BASE: u32 = KV_HDR_RAW + NAME_ALIGNED;
const VALUE_LEN: usize = ((USABLE - 3 * BASE + 3) / 4) as usize;

struct TestKv {
    name: &'static str,
    label: &'static str,
    value_len: usize,
    sector: u32,
    is_changed: bool,
}

fn make_value(label: &str, value_len: usize) -> Vec<u8> {
    let mut v = vec![0u8; value_len];
    let lb = label.as_bytes();
    v[..lb.len()].copy_from_slice(lb);
    v
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flashdb_kv_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open(dir: &Path, sectors: u32) -> Kvdb<FileStorage> {
    let max = SECTOR_SIZE * sectors;
    let storage = FileStorage::new(dir, "test_kv", SECTOR_SIZE).unwrap();
    Kvdb::new(storage, SECTOR_SIZE, max, None).unwrap()
}

fn reboot(db: Kvdb<FileStorage>, dir: &Path, sectors: u32) -> Kvdb<FileStorage> {
    drop(db);
    open(dir, sectors)
}

fn save_kvs(db: &mut Kvdb<FileStorage>, kvs: &[TestKv]) {
    for kv in kvs {
        if kv.is_changed {
            let val = make_value(kv.label, kv.value_len);
            assert_eq!(db.set(kv.name.as_bytes(), &val), Ok(()));
        }
    }
}

fn check_kvs(db: &mut Kvdb<FileStorage>, kvs: &[TestKv]) {
    let saved = db.iter_collect();
    for kv in kvs {
        let found = saved
            .iter()
            .find(|s| s.name() == kv.name.as_bytes())
            .unwrap_or_else(|| panic!("KV {} not found", kv.name));
        let mut buf = vec![0u8; found.value_len()];
        db.read_value(found, &mut buf);
        let prefix = match buf.iter().position(|&b| b == 0) {
            Some(p) => &buf[..p],
            None => &buf[..],
        };
        assert_eq!(
            prefix,
            kv.label.as_bytes(),
            "value mismatch for {}",
            kv.name
        );
        let sec_idx = (found.addr() / SECTOR_SIZE) * SECTOR_SIZE / SECTOR_SIZE;
        assert_eq!(sec_idx, kv.sector, "sector mismatch for {}", kv.name);
    }
}

fn test_fdb_by_kvs(db: &mut Kvdb<FileStorage>, kvs: &[TestKv]) {
    save_kvs(db, kvs);
    check_kvs(db, kvs);
}

#[test]
fn kv_blob_lifecycle() {
    let dir = fresh_dir("blob_lifecycle");
    let mut db = open(&dir, 4);
    assert_eq!(db.oldest_addr(), 0);

    // create
    let tick: u32 = 0x1234_5678;
    db.set(b"kv_blob_test", &tick.to_le_bytes()).unwrap();
    let mut buf = [0u8; 4];
    let n = db.get(b"kv_blob_test", &mut buf).unwrap();
    assert_eq!(n, 4);
    assert_eq!(u32::from_le_bytes(buf), tick);

    // change
    let tick2: u32 = 0x9abc_def0;
    db.set(b"kv_blob_test", &tick2.to_le_bytes()).unwrap();
    let v = db.get_vec(b"kv_blob_test").unwrap();
    assert_eq!(u32::from_le_bytes(v.try_into().unwrap()), tick2);

    // delete
    db.del(b"kv_blob_test").unwrap();
    assert!(db.get(b"kv_blob_test", &mut buf).is_none());
}

#[test]
fn kv_string_lifecycle() {
    let dir = fresh_dir("string_lifecycle");
    let mut db = open(&dir, 4);

    db.set_str(b"kv_test", "12345").unwrap();
    assert_eq!(db.get_str(b"kv_test").as_deref(), Some("12345"));

    db.set_str(b"kv_test", "67890").unwrap();
    assert_eq!(db.get_str(b"kv_test").as_deref(), Some("67890"));

    db.del(b"kv_test").unwrap();
    assert!(db.get_str(b"kv_test").is_none());

    // oldest address stays at sector 0 across reboot
    assert_eq!(db.oldest_addr(), 0);
    let db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), 0);
}

#[test]
fn kv_gc() {
    let dir = fresh_dir("gc");
    let mut db = open(&dir, 4);
    db.set_default().unwrap();

    // prepare1: add 4 KVs (kv0..kv2 in sector0, kv3 in sector1)
    let phase1 = [
        TestKv { name: "kv0", label: "0", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv1", label: "1", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv2", label: "2", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv3", label: "3", value_len: VALUE_LEN, sector: 1, is_changed: true },
    ];
    test_fdb_by_kvs(&mut db, &phase1);
    assert_eq!(db.oldest_addr(), 0);
    let mut db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), 0);

    // prepare2: change kv0 and kv3
    let phase2 = [
        TestKv { name: "kv1", label: "1", value_len: VALUE_LEN, sector: 0, is_changed: false },
        TestKv { name: "kv2", label: "2", value_len: VALUE_LEN, sector: 0, is_changed: false },
        TestKv { name: "kv0", label: "00", value_len: VALUE_LEN, sector: 1, is_changed: true },
        TestKv { name: "kv3", label: "33", value_len: VALUE_LEN, sector: 1, is_changed: true },
    ];
    test_fdb_by_kvs(&mut db, &phase2);
    assert_eq!(db.oldest_addr(), 0);
    let mut db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), 0);

    // change kv0,kv1,kv2,kv3 -> trigger GC; oldest advances to sector 1
    let phase3 = [
        TestKv { name: "kv0", label: "000", value_len: VALUE_LEN, sector: 2, is_changed: true },
        TestKv { name: "kv1", label: "111", value_len: VALUE_LEN, sector: 2, is_changed: true },
        TestKv { name: "kv2", label: "222", value_len: VALUE_LEN, sector: 2, is_changed: true },
        TestKv { name: "kv3", label: "333", value_len: VALUE_LEN, sector: 3, is_changed: true },
    ];
    test_fdb_by_kvs(&mut db, &phase3);
    assert_eq!(db.oldest_addr(), SECTOR_SIZE);
    let mut db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), SECTOR_SIZE);

    // change again -> GC; oldest advances to sector 2
    let phase4 = [
        TestKv { name: "kv0", label: "0000", value_len: VALUE_LEN, sector: 3, is_changed: true },
        TestKv { name: "kv1", label: "1111", value_len: VALUE_LEN, sector: 3, is_changed: true },
        TestKv { name: "kv2", label: "2222", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv3", label: "3333", value_len: VALUE_LEN, sector: 0, is_changed: true },
    ];
    test_fdb_by_kvs(&mut db, &phase4);
    assert_eq!(db.oldest_addr(), SECTOR_SIZE * 2);
    let db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), SECTOR_SIZE * 2);
    drop(db);
}

#[test]
fn kv_gc2() {
    let dir = fresh_dir("gc2");
    let mut db = open(&dir, 4);
    db.set_default().unwrap();

    let phase1 = [
        TestKv { name: "kv0", label: "0", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv1", label: "1", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv2", label: "2", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv3", label: "3", value_len: VALUE_LEN, sector: 1, is_changed: true },
    ];
    test_fdb_by_kvs(&mut db, &phase1);
    assert_eq!(db.oldest_addr(), 0);
    let mut db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), 0);

    let phase2 = [
        TestKv { name: "kv1", label: "1", value_len: VALUE_LEN, sector: 0, is_changed: false },
        TestKv { name: "kv2", label: "2", value_len: VALUE_LEN, sector: 0, is_changed: false },
        TestKv { name: "kv0", label: "00", value_len: VALUE_LEN, sector: 1, is_changed: true },
        TestKv { name: "kv3", label: "33", value_len: VALUE_LEN, sector: 1, is_changed: true },
    ];
    test_fdb_by_kvs(&mut db, &phase2);
    assert_eq!(db.oldest_addr(), 0);
    let mut db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), 0);

    // prepare3: add big kv4 (3x value) in sector 2
    let phase3 = [
        TestKv { name: "kv1", label: "1", value_len: VALUE_LEN, sector: 0, is_changed: false },
        TestKv { name: "kv2", label: "2", value_len: VALUE_LEN, sector: 0, is_changed: false },
        TestKv { name: "kv0", label: "00", value_len: VALUE_LEN, sector: 1, is_changed: false },
        TestKv { name: "kv3", label: "33", value_len: VALUE_LEN, sector: 1, is_changed: false },
        TestKv { name: "kv4", label: "4", value_len: VALUE_LEN * 3, sector: 2, is_changed: true },
    ];
    test_fdb_by_kvs(&mut db, &phase3);
    assert_eq!(db.oldest_addr(), 0);
    let mut db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), 0);

    // add kv5 (2x value) -> trigger GC; oldest advances to sector 2
    let phase4 = [
        TestKv { name: "kv3", label: "33", value_len: VALUE_LEN, sector: 0, is_changed: false },
        TestKv { name: "kv5", label: "5", value_len: VALUE_LEN * 2, sector: 0, is_changed: true },
        TestKv { name: "kv4", label: "4", value_len: VALUE_LEN * 3, sector: 2, is_changed: false },
        TestKv { name: "kv1", label: "1", value_len: VALUE_LEN, sector: 3, is_changed: false },
        TestKv { name: "kv2", label: "2", value_len: VALUE_LEN, sector: 3, is_changed: false },
        TestKv { name: "kv0", label: "00", value_len: VALUE_LEN, sector: 3, is_changed: false },
    ];
    test_fdb_by_kvs(&mut db, &phase4);
    assert_eq!(db.oldest_addr(), SECTOR_SIZE * 2);
    let db = reboot(db, &dir, 4);
    assert_eq!(db.oldest_addr(), SECTOR_SIZE * 2);
    drop(db);
}

#[test]
fn kv_scale_up() {
    let dir = fresh_dir("scale_up");
    let mut db = open(&dir, 4);
    db.set_default().unwrap();

    let old = [
        TestKv { name: "kv0", label: "0", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv1", label: "1", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv2", label: "2", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv3", label: "3", value_len: VALUE_LEN, sector: 1, is_changed: true },
    ];
    save_kvs(&mut db, &old);

    // reboot, scale up from 4 to 8 sectors
    let mut db = reboot(db, &dir, 8);

    // old data still present
    check_kvs(&mut db, &old);

    let new = [
        TestKv { name: "kv4", label: "4", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv5", label: "5", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv6", label: "6", value_len: VALUE_LEN, sector: 0, is_changed: true },
        TestKv { name: "kv7", label: "7", value_len: VALUE_LEN, sector: 0, is_changed: true },
    ];
    // write the new set three times to spread it across sectors
    save_kvs(&mut db, &new);
    save_kvs(&mut db, &new);
    save_kvs(&mut db, &new);

    // both old and new data must be readable (values, not exact sectors)
    for kv in old.iter().chain(new.iter()) {
        let v = db.get_vec(kv.name.as_bytes()).unwrap();
        let prefix = match v.iter().position(|&b| b == 0) {
            Some(p) => &v[..p],
            None => &v[..],
        };
        assert_eq!(prefix, kv.label.as_bytes(), "value mismatch for {}", kv.name);
    }
    db.check().unwrap();
}
