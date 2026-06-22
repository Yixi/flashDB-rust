# FlashDB-rust — project notes

A faithful pure-Rust rewrite of [armink/FlashDB](https://github.com/armink/FlashDB),
an embedded KV + time-series database for flash/file storage.

## Layout

- `src/def.rs` — constants, status enums, geometry helpers (`fdb_def.h` / `fdb_low_lvl.h`)
- `src/crc32.rs` — `fdb_calc_crc32`
- `src/status.rs` — status-table bit encode/decode (`_fdb_set_status` / `_fdb_get_status`)
- `src/flash.rs` — `write_status` / `read_status` / `continue_ff_addr` / `write_align`
- `src/storage.rs` — `Storage` trait, `RamStorage`, `FileStorage` (std, sector-per-file)
- `src/db.rs` — geometry validation, `DefaultKv`
- `src/kvdb.rs` — KVDB engine (`fdb_kvdb.c`)
- `src/tsdb.rs` — TSDB engine (`fdb_tsdb.c`)
- `tests/` — ported upstream suites (`kvdb.rs`, `tsdb.rs`), `recovery.rs`, `smoke.rs`
- `examples/` — ported samples

## Targeted configuration (important)

The port deliberately matches one upstream build configuration:

- **`FDB_WRITE_GRAN == 1`** (NOR flash / file mode). All header sizes/offsets are
  hardcoded for this layout (with the same C struct natural-alignment padding):
  KVDB sector hdr = 16 B, KV hdr = 24 B; TSDB sector hdr = 32 B, log idx = 16 B.
- **32-bit timestamps** (`FdbTime = i32`).
- **Variable-size TSDB blobs** (no `FDB_TSDB_FIXED_BLOB_SIZE`).
- **KV caches disabled** (`FDB_KV_USING_CACHE` off) — a pure speed optimisation
  upstream; on-storage behaviour is identical, which is why the port omits it.

The on-storage format is **byte-compatible** with C FlashDB in this config
(verified bidirectionally).

## Conventions

- No `unsafe` (`#![forbid(unsafe_code)]`). `no_std + alloc` capable; `std` feature
  gates `FileStorage`.
- When changing engine code, re-run `cargo test` (the GC oldest-address and
  TSDB time-boundary assertions are the sensitive ones) and `cargo clippy
  --all-targets -- -D warnings`.
- All numbers little-endian; addresses are `u32` offsets into the flat DB space.

## Build / test

```
cargo test                         # full suite
cargo clippy --all-targets         # lint
cargo build --no-default-features  # no_std
cargo run --example kvdb_basic     # samples
```
