# Changelog

## 0.3.0 — Read, write and compaction performance

**Requires Rust 1.89 or newer; older compiler versions are unsupported.** Rust 2021 edition; standard library only; no third-party crates, `unsafe`, or unchecked `unwrap` in the engine. Linux, macOS and WSL; no native Windows implementation.

### Seven improvements

1. **Direct segment index:** `HashMap<segment ID, position>` replaces repeated linear segment searches; index updates and segment rotation maintain the mapping.
2. **Sequential compaction:** live locations are sorted by physical segment order and offset before rewriting, reducing random reads; records retain CRC validation.
3. **Background compaction:** a worker prepares sealed-segment replacements while the owner continues writing. `start_compaction`, `poll_compaction` and `finish_compaction` control publication. The manifest lists replacement segments *before* later writes, preserving overwrite and delete ordering even when numeric segment IDs are no longer sorted.
4. **Grouped sync:** `put_batch` validates every entry first and performs one final sync in default durability mode. It is not an atomic transaction.
5. **Concurrent reads:** `get(&self)` uses positional file reads and supports parallel immutable access without changing a shared file cursor; writing still requires exclusive mutable access.
6. **Bounded file handles:** a 16-entry LRU keeps frequently read sealed segments open; old entries close on eviction. Active and temporary handles are managed separately.
7. **Reduced copying/allocation:** indexed overwrites reuse stored key allocations; direct reads allocate only the result value and compare key bytes in fixed-size stack chunks. Segment and byte statistics are cached and updated in O(1).

**Automatic compaction:** rotation starts background preparation only after the configured segment-count threshold and a stale-byte ratio of at least one-third. Unpublished `.compact-*.tmp` files are removed on reopen. The BCK1 record format is unchanged; BCKM2 manifest IDs must be unique, with list order authoritative.

### Code size

Physical lines including blank lines and comments, from `wc -l`; no generated build files.

| Rust files | v0.2 | v0.3 | Change |
| --- | ---: | ---: | ---: |
| Engine (`src/`) | 695 | 1,238 | +543 |
| Integration tests (`tests/`) | 462 | 645 | +183 |
| Examples (`examples/`) | 30 | 84 | +54 |
| **Total** | **1,187** | **1,967** | **+780** |

### Verification

Executed locally with **rustc 1.89.0**: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` (**28/28 passed**), `cargo test --release --all-targets` (**28/28 passed**), `cargo build --release`, `cargo run --example basic`, `cargo run --example rotation`, and `cargo run --release --example bench`. Nine integration cases were added for batches, concurrent readers, asynchronous compaction and later writes, manifest replay order/duplicates, automatic-compaction gating, no-op compaction, accounting and abandoned-temp cleanup.

**One local release smoke benchmark:** 20,000 group-synced inserts ~196k/s; 20,000 random reads ~353k/s; full rewrite ~80.7 ms for 1.92 MB of live records. These values are environment-specific, not a guarantee. The benchmark has no stale bytes, so its compaction step does not demonstrate space savings.

## 0.2.0 — Segmented storage

The storage engine now writes to numbered log segments instead of one growing `data.log` file. A small manifest tracks the active segments and is replaced atomically during rotation and compaction.

### Added

- **Segment rotation:** a new log starts when the next record would exceed the configured size.
- **Automatic compaction:** on rotation, the engine can reclaim old records once a configurable segment-count threshold is reached. It runs on the calling thread.
- **Safe publication:** replacement segments are synced before their manifest is published. Orphaned files are ignored during replay.
- **Options:** configure durability, maximum segment size, and automatic-compaction threshold.
- **Stats:** inspect segment count, live keys, total bytes, live bytes, and stale bytes.
- **Migration:** existing v0.1 `data.log` databases are moved to segment 0 without rewriting their BCK1 records.
- **Rotation example:** `cargo run --example rotation`.
- **Eight integration tests:** rotation, automatic compaction, manual durability across segments, multi-segment compaction, migration, bad manifests, oversized records/options, and orphan files.

### Code size

Counts include blank lines and comments.

| Rust files | v0.1 | v0.2 | Difference |
| --- | ---: | ---: | ---: |
| Engine (`src/`) | 375 | 695 | +320 |
| Integration tests (`tests/`) | 263 | 462 | +199 |
| Examples (`examples/`) | 13 | 30 | +17 |
| **Total** | **651** | **1,187** | **+536** |

### Verification

On Linux with Rust 1.89.0: `cargo fmt --check`, strict Clippy (`-D warnings`), `cargo test --all-targets` (19 passed), `cargo build --release`, and the rotation example completed successfully.

The public `open`, `open_with_durability`, `get`, `put`, `delete`, `sync`, `compact`, `len`, and `is_empty` APIs remain available. The on-disk record format is unchanged; the directory layout and manifest are new.
