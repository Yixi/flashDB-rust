//! TSDB sample, ported from `samples/tsdb_sample.c`.
//!
//! Appends environment-status records, queries them (all and by time range),
//! then changes their status.
//!
//! ```text
//! cargo run --example tsdb_basic
//! ```

use flashdb::{RamStorage, Tsdb, TslStatus};
use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone, Copy)]
struct EnvStatus {
    temp: i32,
    humi: i32,
}

impl EnvStatus {
    fn to_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&self.temp.to_le_bytes());
        b[4..8].copy_from_slice(&self.humi.to_le_bytes());
        b
    }
    fn from_bytes(b: &[u8]) -> Self {
        EnvStatus {
            temp: i32::from_le_bytes(b[0..4].try_into().unwrap()),
            humi: i32::from_le_bytes(b[4..8].try_into().unwrap()),
        }
    }
}

fn main() {
    // A simple monotonic clock standing in for a real RTC.
    let clock = Rc::new(Cell::new(1_588_636_800i32)); // 2020-05-05 00:00:00 UTC
    let c = clock.clone();
    let get_time = move || {
        c.set(c.get() + 1);
        c.get()
    };

    let mut tsdb = Tsdb::new(RamStorage::new(4096 * 16), 4096, 4096 * 16, get_time, 128).unwrap();

    println!("==================== tsdb_sample ====================");

    // APPEND new TSLs.
    for status in [EnvStatus { temp: 36, humi: 85 }, EnvStatus { temp: 38, humi: 90 }] {
        tsdb.append(&status.to_bytes()).unwrap();
        println!("append status temp ({}) humi ({})", status.temp, status.humi);
    }

    // QUERY all TSLs by iterator.
    tsdb.iter(|tsl, data| {
        let s = EnvStatus::from_bytes(data);
        println!("[query_cb] time: {}, temp: {}, humi: {}", tsl.time(), s.temp, s.humi);
        false
    });

    // QUERY by time range and count.
    let from = 0;
    let to = i32::MAX;
    tsdb.iter_by_time(from, to, |tsl, data| {
        let s = EnvStatus::from_bytes(data);
        println!("[query_by_time_cb] time: {}, temp: {}, humi: {}", tsl.time(), s.temp, s.humi);
        false
    });
    let count = tsdb.query_count(from, to, TslStatus::Write);
    println!("query count is: {count}");

    // SET the TSL status (collect first, then mutate).
    let tsls = tsdb.collect_by_time(from, to);
    for tsl in &tsls {
        println!("set the TSL (time {}) status to USER_STATUS1", tsl.time());
        tsdb.set_status(tsl, TslStatus::UserStatus1).unwrap();
    }

    println!("===========================================================");
}
