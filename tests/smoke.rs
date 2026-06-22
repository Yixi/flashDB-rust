use flashdb::{Kvdb, RamStorage};

#[test]
fn kv_basic_set_get_del() {
    let storage = RamStorage::new(4096 * 4);
    let mut db = Kvdb::new(storage, 4096, 4096 * 4, None).unwrap();

    assert_eq!(db.oldest_addr(), 0);

    db.set(b"hello", b"world").unwrap();
    let mut buf = [0u8; 32];
    let n = db.get(b"hello", &mut buf).unwrap();
    assert_eq!(&buf[..n], b"world");

    db.set(b"hello", b"there!").unwrap();
    let v = db.get_vec(b"hello").unwrap();
    assert_eq!(v, b"there!");

    db.set(b"k2", b"v2").unwrap();
    db.del(b"hello").unwrap();
    assert!(db.get(b"hello", &mut buf).is_none());
    assert_eq!(db.get_vec(b"k2").unwrap(), b"v2");

    // iterate should yield exactly k2
    let kvs = db.iter_collect();
    assert_eq!(kvs.len(), 1);
    assert_eq!(kvs[0].name(), b"k2");
}

#[test]
fn kv_many_triggers_gc() {
    let storage = RamStorage::new(4096 * 4);
    let mut db = Kvdb::new(storage, 4096, 4096 * 4, None).unwrap();
    // Repeatedly rewrite a handful of keys to force dirty sectors + GC.
    for round in 0..200u32 {
        for k in 0..6u32 {
            let key = alloc_key(k);
            let val = round.to_le_bytes();
            db.set(&key, &val).unwrap();
        }
    }
    for k in 0..6u32 {
        let key = alloc_key(k);
        let v = db.get_vec(&key).unwrap();
        assert_eq!(v, 199u32.to_le_bytes());
    }
    db.check().unwrap();
}

fn alloc_key(k: u32) -> Vec<u8> {
    format!("key{k}").into_bytes()
}

use flashdb::{Tsdb, TslStatus};

#[test]
fn tsdb_basic_append_iter_query() {
    let storage = RamStorage::new(4096 * 16);
    let mut counter = 0i32;
    let mut db = Tsdb::new(
        storage,
        4096,
        4096 * 16,
        move || {
            counter += 2;
            counter
        },
        128,
    )
    .unwrap();

    for i in 1..=256i32 {
        let t = i * 2;
        let s = t.to_string();
        db.append(s.as_bytes()).unwrap();
    }

    let mut count = 0;
    db.iter(|tsl, data| {
        let s = core::str::from_utf8(data).unwrap();
        assert_eq!(tsl.time(), s.parse::<i32>().unwrap());
        count += 1;
        false
    });
    assert_eq!(count, 256);

    assert_eq!(db.query_count(0, 256 * 2, TslStatus::Write), 256);

    // reverse iteration yields times in descending order
    let rev = db.collect_by_time(256 * 2, 0);
    assert_eq!(rev.len(), 256);
    assert_eq!(rev[0].time(), 512);
    assert_eq!(rev[255].time(), 2);
}
