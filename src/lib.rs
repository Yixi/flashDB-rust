//! # FlashDB
//!
//! A faithful, pure-Rust rewrite of [armink/FlashDB](https://github.com/armink/FlashDB),
//! an ultra-lightweight embedded database designed for flash / file storage.
//!
//! The crate provides two databases that share a common flash abstraction:
//!
//! * [`Kvdb`] — a log-structured key/value store with wear levelling, power-loss
//!   safe updates, CRC32 integrity checks and garbage collection.
//! * [`Tsdb`] — an append-only time-series database with optional ring-buffer
//!   rollover and fast time-range queries.
//!
//! Both talk to storage through the [`Storage`] trait. The crate ships a
//! [`RamStorage`] backend (always available) and, with the default `std`
//! feature, a [`FileStorage`] backend that mirrors FlashDB's file mode.
//!
//! The on-storage layout matches upstream FlashDB built with write
//! granularity 1 (the NOR-flash / file-mode configuration).

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod crc32;
mod db;
mod def;
mod error;
mod flash;
mod kvdb;
mod status;
mod storage;

pub use crc32::calc_crc32;
pub use db::DefaultKv;
pub use def::{TslStatus, KV_NAME_MAX};
pub use error::{FdbError, Result};
pub use kvdb::{Kv, KvIterator, Kvdb};
pub use storage::{RamStorage, Storage};

#[cfg(feature = "std")]
pub use storage::FileStorage;
