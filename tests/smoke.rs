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
