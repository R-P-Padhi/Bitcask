# Bitcask v0.3.0 — performance measurements

**Environment:** GitHub-hosted Ubuntu Linux (x86_64), AMD EPYC 7763 host CPU, **4 available parallel workers**, Rust **1.89.0**, optimized release build, standard-library-only benchmark. **Date:** 2026-11-03 (UTC). **Source:** [3-trial GitHub Actions benchmark](https://github.com/R-P-Padhi/Bitcask/actions/runs/41585782358) and [4-job CI validation](https://github.com/R-P-Padhi/Bitcask/actions/runs/41585782374).

**Method:** Three complete runs on one GitHub Actions runner; values below are **medians**. 103 instrumented outputs per run. Each phase creates a separate database under the runner's temporary directory; data is removed after its phase. Keys are 8 bytes and values 64 bytes unless specified. Durability, segment limits, and key counts vary deliberately by phase. Timed operations include library and filesystem costs; no network service is involved. Results are **runner-specific**, not hardware-independent performance guarantees.

## 22 core measurements

| # | Metric | Median result | Workload / interpretation |
| ---: | --- | --- | --- |
| 1 | Write throughput (manual + final sync) | 152,254 ops/s; 10.96 MB/s | 20,000 distinct 8-byte keys, 64-byte values; one final sync |
| 2 | Individual write latency (manual mode) | p50 3.46 µs; p99 6.98 µs; p99.9 1,594.23 µs | Individual puts exclude final sync; latency spikes include rotations |
| 3 | Warm random-read throughput | 249,339 ops/s; 15.96 MB/s | 20,000 pseudo-random reads of existing keys |
| 4 | Warm random-read latency | p50 2.66 µs; p95 5.80 µs; p99 7.65 µs | p99.9 in CSV |
| 5 | Warm sequential-read throughput | 411,507 ops/s; 26.34 MB/s | 20,000 sequential key lookups |
| 6 | Warm sequential-read latency | p50 2.33 µs; p99 2.90 µs | OS page cache retained |
| 7 | First pass after reopening | 247,107 ops/s; p99 7.19 µs | Reopen proxy only; **not a true cold-cache benchmark** |
| 8 | Group commit / batch efficiency | 1: 4,662; 10: 35,860; 100: 147,643; 1,000: 249,402 ops/s | 2,000 total puts per configuration, Sync durability |
| 9 | Durability overhead | Sync per put: 4,622 ops/s; Manual + sync: 152,254 ops/s | Different record counts, so directional comparison, not matched-load ratio |
| 10 | Delete throughput | 217,894 ops/s | 5,000 existing keys, manual + final sync |
| 11 | Delete latency | p50 3.36 µs; p99 5.72 µs | Individual delete timings exclude final sync |
| 12 | Write amplification | Record log 1.333×; Linux process write-bytes estimate 1.419× | 72-byte key+value payload, 96-byte encoded record; /proc/self/io is not device-level amplification |
| 13 | Compaction efficiency | 80.33 % saved; 4,704,000 bytes reclaimed | 15,000 keys, three overwrite passes, 3,000 tombstones; 100% recorded stale bytes removed |
| 14 | Compaction throughput | 89.97 MB/s input; 17.70 MB/s live-data rewrite | Blocking compaction; 72 segments removed |
| 15 | Full compaction duration | 65.08 ms (range 61.96–69 ms) | Stale-heavy benchmark, unlike earlier smoke test |
| 16 | Background compaction elapsed period | 61.54 ms | Start through foreground workload and finish; not isolated worker CPU time |
| 17 | Foreground responsiveness during compaction | idle p99 8.81 → overlap p99 10.77 µs (1.223×) | 50/50 read/write workload; overlapping background rewrite |
| 18 | Recovery / startup cost | 2k: 3.64 ms; 10k: 17.59 ms; 40k: 74.97 ms | Reopen and replay committed logs |
| 19 | Recovery replay throughput | 2k: 548,819; 40k: 533,561 records/s | 40k log ≈3.84 MB of encoded records |
| 20 | Memory footprint | 20k-key checkpoint: 6,404 KiB; complete program peak: 9,940 KiB | 9,940 KiB is median /usr/bin/time RSS; not index-only memory |
| 21 | Parallel read scalability | 1t 234,279; 2t 237,157; 4t 189,182; 8t 207,797; 16t 213,274 ops/s | 4 available hardware threads reported; throughput did not scale linearly |
| 22 | Segment rotation latency spikes | normal put p99 8.62 µs; rotation put p99 7,157.37 µs | 240 segments using deliberately tiny 512-byte rotation limit |

## Additional metrics and correctness checks

| Metric | Median measurement |
| --- | --- |
| Manual sync boundary | 0.348 ms |
| Missing-key lookup | 26,467,770 ops/s; in-memory negative lookup, no disk record read |
| Overwrite churn | 157,605 ops/s; 2,080,000 stale bytes; 96 bytes/overwrite |
| Mixed workload, 5% / 50% / 95% writes | 367,783 / 233,897 / 163,772 ops/s |
| Value-size sensitivity, 64 / 1,024 / 4,096 bytes | 260,167 / 110,659 / 37,313 ops/s |
| Write bandwidth, 64 / 1,024 / 4,096 bytes | 16.65 / 113.31 / 152.84 MB/s |
| File handle pressure | 25 process fds after reading across 240 segments; 16-entry sealed-file cache configured in engine |
| Compaction rewrite physical-write estimate | 1.006× live record bytes, based on Linux process write counter |
| Abrupt child exit, bypassing `Drop` | 200/200 keys recovered; verified in all 3 runs |
| Simulated 7-byte partial log tail | Last segment truncated to valid boundary; 200/200 live keys preserved in all 3 runs |
| Compacted data checked against expected state | Passed all 3 runs |
| Recovery checked against expected final key/value | Passed at 2k / 10k / 40k keys in all 3 runs |

**Raw dataset:** [PERFORMANCE_DATA.csv](PERFORMANCE_DATA.csv) contains all **103 metric names**, units, each of the three raw results, median, minimum, and maximum. The benchmark itself is [examples/perf.rs](examples/perf.rs); the reproducible automation is [.github/workflows/perf.yml](.github/workflows/perf.yml).

## Main observations

- **Durability choice dominates short writes.** Syncing each individual put is substantially slower than amortizing sync cost over a batch. `put_batch()` is **not** an atomic transaction.
- **Compaction actively reclaims space.** The stale-heavy benchmark removed **80.33%** of on-disk log bytes in about **65 ms**. The benchmark also checks the post-compaction key state.
- **Concurrent random reads do not scale linearly here.** Throughput at 4, 8, and 16 readers was below the one- or two-reader results. Filesystem work, repeated validation, shared caching and the host's 4 available workers are possible contributors; this workload does **not** isolate which factor dominates.
- **Foreground latency rose while background compaction was active.** Median-of-trials p99 increased from **8.81 to 10.77 µs**, about **1.223×**. That is a measured trade-off, not a fundamental correctness flaw.
- **Rotation creates tail-latency spikes.** The 512-byte test segment is intentionally extreme; the normal default is 8 MiB. Rotation results should not be extrapolated to default settings.
- **Replay scales with logged bytes**, as expected for an append-only recovery scan. Partial active-tail recovery and process-exit persistence passed the tested scenarios.

## Reproduce

```sh
rustup toolchain install 1.89.0 --component rustfmt clippy
cargo +1.89.0 fmt --check
cargo +1.89.0 clippy --all-targets -- -D warnings
cargo +1.89.0 test --all-targets
cargo +1.89.0 build --release --example perf
./target/release/examples/perf
```

Linux-specific `/proc/self/io`, `/proc/self/status`, and `/proc/self/fd` counters are optional. The storage engine itself supports Linux, macOS, and WSL (not native Windows). Rust versions **below 1.89 are unsupported**.

**Measurement limits:** Hosted runners have unpinned CPU/disk conditions. No external cache drop is performed: the first pass after reopening is **not** a cold-cache measurement. Process `write_bytes` approximates write activity attributed to the benchmark, **not** NAND-level amplification or device endurance. The benchmark's process-exit test does not simulate power loss; the partial-tail test is synthetic. Memory includes the entire process, not just the in-memory index. Three trials characterize local variation but are not a comprehensive statistical confidence interval.
