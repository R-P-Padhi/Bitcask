# Bitcask Lite

Bitcask Lite is a small, append-only key-value store written in Rust, with no external dependencies. It keeps an in-memory index of keys and writes records to numbered log segments. Old records are reclaimed through compaction.

**Rust 1.89+ required.** Older Rust versions are unsupported and will not build this crate: the minimum version is enforced in `Cargo.toml`, and the standard-library file locking API requires 1.89. Linux, macOS, and WSL are supported; native Windows is not.

## Run

```sh
cargo test --all-targets
cargo run --example basic
cargo run --example rotation
cargo run --release --example bench
```

## Use

```rust
use bitcask_lite::Bitcask;

fn main() -> std::io::Result<()> {
    let mut db = Bitcask::open("app.db")?;
    db.put(b"hello", b"world")?;
    assert_eq!(db.get(b"hello")?, Some(b"world".to_vec()));
    db.delete(b"hello")?;
    db.compact()?;
    Ok(())
}
```

**As a dependency:** add `bitcask-lite = { path = "../bitcask-lite" }` under `[dependencies]`; adjust the path. This project is not published to crates.io.

## API

| Call | Result |
| --- | --- |
| `open(path)` | Open/create; sync each mutation; parent directory must exist |
| `open_with_durability(path, Durability::Manual)` | Explicit sync mode |
| `open_with_options(path, Options)` | Configure segment size, compaction, and durability |
| `put(key, value)` | Insert/overwrite; `io::Result<()>` |
| `put_batch(&[(key, value)])` | Group puts; one final sync in default durability mode; not a transaction |
| `get(key)` | `&self` positional read; parallel reads allowed |
| `delete(key)` | `io::Result<bool>`; false when absent |
| `sync()` | Synchronize pending writes |
| `compact()` | Blocking full rewrite of live records |
| `start_compaction()` | Prepare sealed-segment compaction on a worker thread |
| `poll_compaction()` / `finish_compaction()` | Publish completed work without waiting / wait and publish |
| `stats()` | Constant-time live-key, segment, total/live/stale byte counters |
| `len()` / `is_empty()` | Last successfully updated index state |

By default, the active segment rotates at 8 MiB. Rotation starts background compaction after eight segments when at least a third of the log bytes are obsolete. `Options` changes these thresholds; `compact_after_segments: 0` disables automatic compaction. A completed background rewrite is published on a later write, `poll_compaction()`, or `finish_compaction()`. A record larger than the segment limit is allowed in an empty segment.

```rust
use bitcask_lite::{Bitcask, Options};

fn main() -> std::io::Result<()> {
    let options = Options {
        max_segment_bytes: 1024 * 1024,
        compact_after_segments: 4,
        ..Options::default()
    };
    let mut db = Bitcask::open_with_options("data.db", options)?;
    db.put(b"key", b"value")?;
    println!("{:?}", db.stats());
    Ok(())
}
```

## Design

- **Write:** encode and checksum → rotate if needed → append to active segment → optional sync → update RAM index. Overwrites reuse the indexed key allocation.
- **Read:** hash lookup → direct segment lookup → positional read → verify record → return value without shifting its buffer.
- **Open:** load the manifest → replay listed segments in order → latest record wins; tombstones remove keys.
- **Compact:** sort live record locations by disk order and rewrite sequentially. Automatic compaction prepares sealed segments on a worker while new writes continue; publication is atomic via the manifest.
- **Cost:** expected O(1) key/segment lookups; reads/writes O(record bytes) plus filesystem costs; reopen O(total log bytes). RAM holds live keys/locations. A 16-entry LRU cache bounds retained sealed-file handles; cold reads may reopen a file.

## Recovery

- **Incomplete final record:** trim the active segment to its last complete record on open. An incomplete sealed segment is an error.
- **Complete invalid record:** return `InvalidData`; leave the log unchanged, including corruption at EOF.
- **Write/sync/compaction I/O failure:** return an error; disable further operations on that handle; drop and reopen. A failed operation may appear after reopening.
- **Invalid input:** return `InvalidInput`; handle stays usable.
- **Lock conflict:** return `WouldBlock`; one database owner at a time; OS releases the lock when its handle/process exits.
- **Interrupted compaction:** `MANIFEST` is authoritative. Unlisted segments are ignored, and abandoned background temporary files are removed on the next open.
- **Existing v0.1 databases:** a directory with `data.log` and no manifest is automatically migrated without changing the BCK1 records.

## Durability

**Default:** successful individual mutations call `sync_all()` before returning. **Batch:** `put_batch()` appends a group and synchronizes once before reporting success; the batch is not atomic if an operation fails. **Manual:** call `sync()` at your chosen boundary; unsynced writes can be lost. Drop does not sync. Guarantees depend on filesystem/storage honoring sync and atomic rename; physical power-loss behavior has not been tested.

## Limits

**Keys:** 1–65,536 bytes. **Values:** 0–16,777,216 bytes. Binary supported. **Concurrency:** one writer owns the database; immutable reads can run concurrently through `Arc<Bitcask>`, or use `Arc<RwLock<Bitcask>>` for reads alongside exclusive writes. Background compaction only rewrites sealed segments. **Scope:** full replay, no transactions/TTL/networking. Keep database files in a trusted directory; never edit or remove `LOCK` while open.

**Error handling:** the storage engine uses no `unsafe` code or unchecked `unwrap()` calls. I/O errors are returned to the caller, and record/index allocations use fallible reservation.

## Tests / development

The 28 integration tests cover persistence, overwrite/delete, binary values, partial writes, CRC corruption, locking, rotation, migration, malformed manifests, background compaction with concurrent updates, parallel readers, group writes, file cleanup, and storage counters. A reference-map test checks 1,000 operations. Recovery tests simulate truncated files, not a physical power cut.

`cargo run --release --example bench` prints write, random-read, and full-compaction timings using 20,000 records; results depend on the machine and filesystem. It is a small reproducible smoke benchmark, not a standardized throughput claim.

For 22 core performance measurements plus additional recovery, compaction, concurrency, durability, and write-amplification tests, see [PERFORMANCE.md](PERFORMANCE.md) and [the 103-metric dataset](PERFORMANCE_DATA.csv). Run `cargo run --release --example perf` on Linux to repeat the measurements.

For development, run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets`.

GitHub Actions checks the project on Linux and macOS with Rust 1.89 and stable. Bug reports should include the OS, Rust version, and a minimal example that reproduces the issue.

The project is licensed under MIT. See [LICENSE](LICENSE). More details: [changelog](CHANGELOG.md), [disk format](FORMAT.md), [file sync and locking](https://doc.rust-lang.org/std/fs/struct.File.html), and [rename semantics](https://doc.rust-lang.org/std/fs/fn.rename.html).
