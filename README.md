# FlashDB-rust

A faithful, pure-Rust rewrite of [armink/FlashDB](https://github.com/armink/FlashDB) —
an ultra-lightweight embedded database oriented to flash / file storage. It provides:

- **KVDB** — a log-structured key-value database with wear-levelling, power-loss
  safe writes, CRC32 integrity checking and garbage collection.
- **TSDB** — an append-only time-series database with optional ring-buffer
  rollover, fast time-range queries and per-record status.

The on-storage data structures, status-bit encoding, garbage-collection
algorithm and recovery logic mirror the original C implementation
(write granularity = 1, the NOR-flash / file-mode layout), so the behaviour is
identical to upstream FlashDB.

## Storage backends

The database talks to storage through the [`Storage`] trait. Two backends ship
with the crate:

- [`FileStorage`] (requires the default `std` feature) — mirrors FlashDB's
  `fdb_file.c`: every sector is mapped to a `name.fdb.N` file inside a directory.
- [`RamStorage`] — an in-memory flat address space, ideal for tests and for
  bring-up on a new platform.

You can implement [`Storage`] yourself to target real flash hardware.

## Quick start

```rust
use flashdb::{Kvdb, RamStorage};

// 4 sectors of 4 KiB each.
let storage = RamStorage::new(4096 * 4);
let mut db = Kvdb::new(storage, 4096, 4096 * 4, None).unwrap();

db.set(b"boot_count", b"42").unwrap();
let mut buf = [0u8; 16];
let n = db.get(b"boot_count", &mut buf).unwrap();
assert_eq!(&buf[..n], b"42");
```

See the [`examples`](examples) directory for the KV and TSDB samples ported from
upstream, and the [`tests`](tests) directory for the ported FlashDB test suites.

## Compatibility & verification

The on-storage format is byte-compatible with upstream FlashDB built for the
NOR-flash / file mode (write granularity 1). This was verified both ways:

- the original C FlashDB writes a KVDB + TSDB in file mode, and this crate reads
  every key/log back correctly;
- this crate writes a KVDB + TSDB, and the original C FlashDB reads them back
  correctly.

In addition, both upstream test suites (`fdb_kvdb_tc.c`, `fdb_tsdb_tc.c`) are
ported as integration tests — including the garbage-collection tests with their
exact oldest-sector assertions and the exhaustive time-range boundary queries —
and all pass. Run them with `cargo test`.

### Configuration notes

This port targets the most common upstream configuration:

- write granularity 1 (NOR flash / file mode),
- 32-bit timestamps,
- variable-size TSDB blobs,
- KV caches disabled (a pure speed optimisation upstream; behaviour is identical).

## License

Apache-2.0, matching upstream FlashDB.
