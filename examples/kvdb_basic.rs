//! Basic KVDB sample, ported from `samples/kvdb_basic_sample.c`.
//!
//! Reads, increments and stores a persistent `boot_count`. Run it repeatedly
//! (it uses a file-backed database under the system temp dir) to watch the
//! count grow across "reboots".
//!
//! ```text
//! cargo run --example kvdb_basic
//! ```

use flashdb::{FileStorage, Kvdb};

fn main() {
    let dir = std::env::temp_dir().join("flashdb_example_kvdb");
    std::fs::create_dir_all(&dir).unwrap();

    let sec_size = 4096;
    let max_size = 4096 * 4;
    let storage = FileStorage::new(&dir, "env", sec_size).unwrap();
    let mut kvdb = Kvdb::new(storage, sec_size, max_size, None).unwrap();

    println!("==================== kvdb_basic_sample ====================");

    // GET the KV value.
    let mut boot_count: i32 = 0;
    let mut buf = [0u8; 4];
    match kvdb.get(b"boot_count", &mut buf) {
        Some(4) => {
            boot_count = i32::from_le_bytes(buf);
            println!("get the 'boot_count' value is {boot_count}");
        }
        _ => println!("get the 'boot_count' failed (first run)"),
    }

    // CHANGE the KV value.
    boot_count += 1;
    kvdb.set(b"boot_count", &boot_count.to_le_bytes()).unwrap();
    println!("set the 'boot_count' value to {boot_count}");

    println!("===========================================================");
}
