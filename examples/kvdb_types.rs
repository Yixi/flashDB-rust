//! String and blob KV samples, ported from `samples/kvdb_type_string_sample.c`
//! and `samples/kvdb_type_blob_sample.c`.
//!
//! ```text
//! cargo run --example kvdb_types
//! ```

use flashdb::{Kvdb, RamStorage};

fn main() {
    let mut kvdb = Kvdb::new(RamStorage::new(4096 * 4), 4096, 4096 * 4, None).unwrap();

    println!("==================== kvdb_type_string_sample ====================");
    // CREATE / GET / CHANGE / DELETE a string KV.
    kvdb.set_str(b"temp", "36.5").unwrap();
    println!("create the 'temp' string KV, value is: {:?}", kvdb.get_str(b"temp"));

    kvdb.set_str(b"temp", "38.1").unwrap();
    println!("change the 'temp' string KV, value is: {:?}", kvdb.get_str(b"temp"));

    kvdb.del(b"temp").unwrap();
    println!("delete the 'temp' string KV, get now: {:?}", kvdb.get_str(b"temp"));

    println!("==================== kvdb_type_blob_sample ======================");
    // CREATE / GET / CHANGE / DELETE a blob KV (a little-endian i32 here).
    let temp: i32 = 36;
    kvdb.set(b"temp", &temp.to_le_bytes()).unwrap();
    println!("create the 'temp' blob KV, value is: {}", read_i32(&mut kvdb, b"temp"));

    let temp: i32 = 38;
    kvdb.set(b"temp", &temp.to_le_bytes()).unwrap();
    println!("change the 'temp' blob KV, value is: {}", read_i32(&mut kvdb, b"temp"));

    kvdb.del(b"temp").unwrap();
    println!("delete the 'temp' blob KV, present: {}", kvdb.get_obj(b"temp").is_some());

    println!("=================================================================");
}

fn read_i32(kvdb: &mut Kvdb<RamStorage>, key: &[u8]) -> i32 {
    let mut buf = [0u8; 4];
    kvdb.get(key, &mut buf);
    i32::from_le_bytes(buf)
}
