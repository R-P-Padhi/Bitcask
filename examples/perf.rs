//! Reproducible, dependency-free Linux runner benchmarks. Values are runner-specific.
use bitcask_lite::{Bitcask, Durability, Options};
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::Arc;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const VALUE: [u8; 64] = [0x5a; 64];
const KEYS: usize = 20_000;

fn options(durability: Durability, max_segment_bytes: u64) -> Options {
    Options {
        durability,
        max_segment_bytes,
        compact_after_segments: 0,
    }
}

fn emit(name: &str, value: f64, unit: &str) {
    println!("METRIC|{name}|{value:.6}|{unit}");
}

fn rate(n: usize, elapsed: f64) -> f64 {
    n as f64 / elapsed.max(1e-9)
}

fn key(index: usize) -> [u8; 8] {
    (index as u64).to_le_bytes()
}

fn next(seed: &mut u64) -> usize {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*seed >> 32) as usize
}

fn percentile(samples: &mut [f64], p: f64) -> f64 {
    samples.sort_unstable_by(|a, b| a.total_cmp(b));
    let index = ((samples.len().saturating_sub(1)) as f64 * p).ceil() as usize;
    samples.get(index).copied().unwrap_or(0.0)
}

fn latencies(prefix: &str, samples: &mut [f64]) {
    for (name, fraction) in [("p50", 0.50), ("p95", 0.95), ("p99", 0.99), ("p999", 0.999)] {
        emit(
            &format!("{prefix}_{name}_us"),
            percentile(samples, fraction),
            "us",
        );
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        Ok(Self(std::env::temp_dir().join(format!(
            "bitcask-perf-{}-{}-{}-{tag}",
            process::id(),
            stamp.as_nanos(),
            thread::current().name().unwrap_or("main")
        ))))
    }

    fn open(&self, durability: Durability, segment: u64) -> io::Result<Bitcask> {
        Bitcask::open_with_options(&self.0, options(durability, segment))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn proc_value(file: &str, field: &str) -> Option<u64> {
    fs::read_to_string(file).ok()?.lines().find_map(|line| {
        line.strip_prefix(field)?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })
}

fn peak_rss_kib() -> Option<u64> {
    proc_value("/proc/self/status", "VmHWM:")
}

fn write_bytes() -> Option<u64> {
    proc_value("/proc/self/io", "write_bytes:")
}

fn fd_count() -> Option<usize> {
    Some(fs::read_dir("/proc/self/fd").ok()?.count())
}

fn populate(db: &mut Bitcask, n: usize, base: usize) -> io::Result<()> {
    for i in 0..n {
        db.put(key(i + base), VALUE)?;
    }
    db.sync()
}

fn read_pass(db: &Bitcask, label: &str, n: usize, keys: usize, random: bool) -> io::Result<()> {
    let mut seed = 0x1234_5678_9abc_def0;
    let mut samples = Vec::with_capacity(n);
    let start = Instant::now();
    for i in 0..n {
        let index = if random {
            next(&mut seed) % keys
        } else {
            i % keys
        };
        let t = Instant::now();
        black_box(db.get(key(index))?);
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let elapsed = start.elapsed().as_secs_f64();
    emit(&format!("{label}_ops_s"), rate(n, elapsed), "ops/s");
    emit(
        &format!("{label}_mb_s"),
        n as f64 * 64.0 / (elapsed * 1e6),
        "MB/s",
    );
    latencies(label, &mut samples);
    Ok(())
}

fn basic() -> io::Result<()> {
    let dir = Scratch::new("basic")?;
    let mut db = dir.open(Durability::Manual, 64 * 1024)?;
    let before_io = write_bytes();
    let mut samples = Vec::with_capacity(KEYS);
    let start = Instant::now();
    for i in 0..KEYS {
        let t = Instant::now();
        db.put(key(i), VALUE)?;
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let sync_start = Instant::now();
    db.sync()?;
    emit(
        "manual_sync_ms",
        sync_start.elapsed().as_secs_f64() * 1e3,
        "ms",
    );
    let elapsed = start.elapsed().as_secs_f64();
    emit("write_manual_plus_sync_ops_s", rate(KEYS, elapsed), "ops/s");
    emit(
        "write_manual_plus_sync_mb_s",
        KEYS as f64 * 72.0 / (elapsed * 1e6),
        "MB/s",
    );
    latencies("write_manual_unsynced", &mut samples);
    let stats = db.stats();
    emit(
        "record_log_amplification",
        stats.total_bytes as f64 / (KEYS as f64 * 72.0),
        "x",
    );
    emit(
        "segment_count_after_writes",
        stats.segments as f64,
        "segments",
    );
    if let (Some(a), Some(b)) = (before_io, write_bytes()) {
        emit(
            "physical_write_amplification_estimate",
            (b.saturating_sub(a)) as f64 / (KEYS as f64 * 72.0),
            "x",
        );
        emit(
            "physical_write_bytes_estimate",
            b.saturating_sub(a) as f64,
            "bytes",
        );
    }
    drop(db);
    let open_at = Instant::now();
    let db = dir.open(Durability::Manual, 64 * 1024)?;
    emit(
        "reopen_after_20k_ms",
        open_at.elapsed().as_secs_f64() * 1e3,
        "ms",
    );
    read_pass(
        &db,
        "first_pass_after_reopen_cache_not_cleared",
        KEYS,
        KEYS,
        true,
    )?;
    read_pass(&db, "warm_random_read", KEYS, KEYS, true)?;
    read_pass(&db, "warm_sequential_read", KEYS, KEYS, false)?;
    let start = Instant::now();
    for i in 0..KEYS {
        black_box(db.get(key(i + 10_000_000))?);
    }
    emit(
        "missing_key_lookup_ops_s",
        rate(KEYS, start.elapsed().as_secs_f64()),
        "ops/s",
    );
    if let Some(rss) = peak_rss_kib() {
        emit("process_peak_rss_kib", rss as f64, "KiB");
        emit(
            "peak_rss_bytes_per_live_key_upper_bound",
            rss as f64 * 1024.0 / KEYS as f64,
            "bytes/key",
        );
    }
    if let Some(fds) = fd_count() {
        emit("open_file_descriptors_after_reads", fds as f64, "fds");
    }
    Ok(())
}

fn durability_batches() -> io::Result<()> {
    let dir = Scratch::new("sync")?;
    let mut db = dir.open(Durability::Sync, 1024 * 1024)?;
    let n = 1200;
    let mut samples = Vec::with_capacity(n);
    let start = Instant::now();
    for i in 0..n {
        let t = Instant::now();
        db.put(key(i), VALUE)?;
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    emit(
        "sync_each_put_ops_s",
        rate(n, start.elapsed().as_secs_f64()),
        "ops/s",
    );
    latencies("sync_each_put", &mut samples);
    drop(db);
    for batch in [1, 10, 100, 1000] {
        let dir = Scratch::new(&format!("batch-{batch}"))?;
        let mut db = dir.open(Durability::Sync, 8 * 1024 * 1024)?;
        let n = 2000;
        let start = Instant::now();
        for offset in (0..n).step_by(batch) {
            let end = (offset + batch).min(n);
            let owned: Vec<_> = (offset..end).map(key).collect();
            let entries: Vec<(&[u8], &[u8])> = owned
                .iter()
                .map(|k| (k.as_slice(), VALUE.as_slice()))
                .collect();
            db.put_batch(&entries)?;
        }
        emit(
            &format!("sync_batch_{batch}_ops_s"),
            rate(n, start.elapsed().as_secs_f64()),
            "ops/s",
        );
    }
    Ok(())
}

fn deletes_churn() -> io::Result<()> {
    let dir = Scratch::new("churn")?;
    let mut db = dir.open(Durability::Manual, 64 * 1024)?;
    populate(&mut db, 10_000, 0)?;
    let mut samples = Vec::with_capacity(5000);
    let start = Instant::now();
    for i in 0..5000 {
        let t = Instant::now();
        if !db.delete(key(i))? {
            return Err(io::Error::other("unexpected missing delete"));
        }
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    db.sync()?;
    emit(
        "delete_manual_plus_sync_ops_s",
        rate(5000, start.elapsed().as_secs_f64()),
        "ops/s",
    );
    latencies("delete_manual_unsynced", &mut samples);
    let initial = db.stats();
    let start = Instant::now();
    for i in 5000..10_000 {
        db.put(key(i), VALUE)?;
        db.put(key(i), VALUE)?;
        db.put(key(i), VALUE)?;
    }
    db.sync()?;
    let after = db.stats();
    emit(
        "overwrite_churn_ops_s",
        rate(15_000, start.elapsed().as_secs_f64()),
        "ops/s",
    );
    emit("stale_bytes_after_churn", after.stale_bytes as f64, "bytes");
    emit(
        "stale_growth_bytes_per_overwrite",
        after.stale_bytes.saturating_sub(initial.stale_bytes) as f64 / 15_000.0,
        "bytes/overwrite",
    );
    emit(
        "live_key_count_after_deletes",
        after.live_keys as f64,
        "keys",
    );
    Ok(())
}

fn compaction() -> io::Result<()> {
    let dir = Scratch::new("compaction")?;
    let mut db = dir.open(Durability::Manual, 64 * 1024)?;
    populate(&mut db, 15_000, 0)?;
    for r in 0..3 {
        for i in 0..15_000 {
            db.put(key(i), [r as u8; 64])?;
        }
    }
    for i in 0..3000 {
        db.delete(key(i))?;
    }
    db.sync()?;
    let before = db.stats();
    let physical_before = write_bytes();
    let start = Instant::now();
    db.compact()?;
    let elapsed = start.elapsed().as_secs_f64();
    let after = db.stats();
    emit("blocking_compaction_ms", elapsed * 1e3, "ms");
    emit(
        "compaction_input_mb_s",
        before.total_bytes as f64 / (elapsed * 1e6),
        "MB/s",
    );
    emit(
        "compaction_live_rewrite_mb_s",
        before.live_bytes as f64 / (elapsed * 1e6),
        "MB/s",
    );
    emit(
        "compaction_reclaimed_bytes",
        before.total_bytes.saturating_sub(after.total_bytes) as f64,
        "bytes",
    );
    emit(
        "compaction_reclaimed_pct",
        100.0 * before.total_bytes.saturating_sub(after.total_bytes) as f64
            / before.total_bytes.max(1) as f64,
        "percent",
    );
    emit(
        "compaction_stale_removed_pct",
        100.0 * before.stale_bytes.saturating_sub(after.stale_bytes) as f64
            / before.stale_bytes.max(1) as f64,
        "percent",
    );
    emit(
        "compaction_segment_reduction",
        before.segments.saturating_sub(after.segments) as f64,
        "segments",
    );
    if let (Some(a), Some(b)) = (physical_before, write_bytes()) {
        emit(
            "compaction_physical_write_bytes_estimate",
            b.saturating_sub(a) as f64,
            "bytes",
        );
        emit(
            "compaction_physical_write_over_live_bytes_estimate",
            b.saturating_sub(a) as f64 / before.live_bytes.max(1) as f64,
            "x",
        );
    }
    if db.len() != 12_000 || db.get(key(0))?.is_some() || db.get(key(10_000))?.is_none() {
        return Err(io::Error::other("compaction verification failed"));
    }
    emit("compaction_verification_pass", 1.0, "bool");
    Ok(())
}

fn recovery() -> io::Result<()> {
    for n in [2000, 10_000, 40_000] {
        let dir = Scratch::new(&format!("recovery-{n}"))?;
        let mut db = dir.open(Durability::Manual, 64 * 1024)?;
        populate(&mut db, n, 0)?;
        let size = db.stats().total_bytes;
        drop(db);
        let start = Instant::now();
        let db = dir.open(Durability::Manual, 64 * 1024)?;
        let elapsed = start.elapsed().as_secs_f64();
        emit(&format!("recovery_{n}_ms"), elapsed * 1e3, "ms");
        emit(
            &format!("recovery_{n}_records_s"),
            rate(n, elapsed),
            "records/s",
        );
        emit(
            &format!("recovery_{n}_mb_s"),
            size as f64 / (elapsed * 1e6),
            "MB/s",
        );
        if db.len() != n || db.get(key(n - 1))?.as_deref() != Some(VALUE.as_slice()) {
            return Err(io::Error::other("recovery verification failed"));
        }
    }
    Ok(())
}

fn concurrent_reads() -> io::Result<()> {
    let dir = Scratch::new("concurrent")?;
    let mut db = dir.open(Durability::Manual, 64 * 1024)?;
    populate(&mut db, KEYS, 0)?;
    let db = Arc::new(db);
    for workers in [1, 2, 4, 8, 16] {
        let each = 6000;
        let start = Instant::now();
        thread::scope(|scope| -> io::Result<()> {
            let mut tasks = Vec::with_capacity(workers);
            for worker in 0..workers {
                let db = Arc::clone(&db);
                tasks.push(scope.spawn(move || -> io::Result<()> {
                    let mut seed = 1234 + worker as u64;
                    for _ in 0..each {
                        let k = key(next(&mut seed) % KEYS);
                        black_box(db.get(k)?);
                    }
                    Ok(())
                }));
            }
            for task in tasks {
                task.join()
                    .map_err(|_| io::Error::other("read worker panicked"))??;
            }
            Ok(())
        })?;
        emit(
            &format!("parallel_reads_{workers}_threads_ops_s"),
            rate(workers * each, start.elapsed().as_secs_f64()),
            "ops/s",
        );
    }
    Ok(())
}

fn mix_sizes() -> io::Result<()> {
    for writes_pct in [5, 50, 95] {
        let dir = Scratch::new(&format!("mixed-{writes_pct}"))?;
        let mut db = dir.open(Durability::Manual, 64 * 1024)?;
        populate(&mut db, 5000, 0)?;
        let mut seed = 23891_u64;
        let n = 12_000;
        let start = Instant::now();
        for i in 0..n {
            let index = next(&mut seed) % 5000;
            if i % 100 < writes_pct {
                db.put(key(index), VALUE)?;
            } else {
                black_box(db.get(key(index))?);
            }
        }
        db.sync()?;
        emit(
            &format!("mixed_{writes_pct}pct_writes_ops_s"),
            rate(n, start.elapsed().as_secs_f64()),
            "ops/s",
        );
    }
    for value_size in [64, 1024, 4096] {
        let dir = Scratch::new(&format!("value-{value_size}"))?;
        let mut db = dir.open(Durability::Manual, 1024 * 1024)?;
        let value = vec![0x38; value_size];
        let n = 4000;
        let start = Instant::now();
        for i in 0..n {
            db.put(key(i), &value)?;
        }
        db.sync()?;
        let elapsed = start.elapsed().as_secs_f64();
        emit(
            &format!("value_{value_size}_write_ops_s"),
            rate(n, elapsed),
            "ops/s",
        );
        emit(
            &format!("value_{value_size}_write_mb_s"),
            (n * value_size) as f64 / (elapsed * 1e6),
            "MB/s",
        );
    }
    Ok(())
}

fn rotation_fds() -> io::Result<()> {
    let dir = Scratch::new("rotation")?;
    let mut db = dir.open(Durability::Manual, 512)?;
    let mut normal = Vec::new();
    let mut rotate = Vec::new();
    for i in 0..1200 {
        let before = db.stats().segments;
        let start = Instant::now();
        db.put(key(i), VALUE)?;
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        if db.stats().segments > before {
            rotate.push(elapsed);
        } else {
            normal.push(elapsed);
        }
    }
    db.sync()?;
    emit("rotation_count", rotate.len() as f64, "rotations");
    emit(
        "nonrotation_put_p99_us",
        percentile(&mut normal, 0.99),
        "us",
    );
    emit("rotation_put_p99_us", percentile(&mut rotate, 0.99), "us");
    let segment_count = db.stats().segments;
    for i in 0..1200 {
        black_box(db.get(key(i))?);
    }
    emit(
        "segments_exercising_file_cache",
        segment_count as f64,
        "segments",
    );
    if let Some(fds) = fd_count() {
        emit("fds_after_240_segment_random_reads", fds as f64, "fds");
    }
    Ok(())
}

fn foreground(db: &mut Bitcask, n: usize) -> io::Result<(f64, f64)> {
    let mut samples = Vec::with_capacity(n);
    let mut seed = 49213_u64;
    let start = Instant::now();
    for i in 0..n {
        let index = next(&mut seed) % 10_000;
        let t = Instant::now();
        if i % 2 == 0 {
            db.put(key(index), VALUE)?;
        } else {
            black_box(db.get(key(index))?);
        }
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    Ok((
        rate(n, start.elapsed().as_secs_f64()),
        percentile(&mut samples, 0.99),
    ))
}

fn background() -> io::Result<()> {
    let dir = Scratch::new("background")?;
    let mut db = dir.open(Durability::Manual, 64 * 1024)?;
    populate(&mut db, 10_000, 0)?;
    for _ in 0..6 {
        for i in 0..10_000 {
            db.put(key(i), VALUE)?;
        }
    }
    db.sync()?;
    let (base_rate, base_p99) = foreground(&mut db, 2000)?;
    db.sync()?;
    emit("foreground_idle_ops_s", base_rate, "ops/s");
    emit("foreground_idle_p99_us", base_p99, "us");
    let start = Instant::now();
    if !db.start_compaction()? {
        return Err(io::Error::other("background compaction failed to start"));
    }
    let (active_rate, active_p99) = foreground(&mut db, 2000)?;
    db.sync()?;
    let running_at_end = db.finish_compaction()?;
    emit("foreground_compaction_period_ops_s", active_rate, "ops/s");
    emit("foreground_compaction_period_p99_us", active_p99, "us");
    emit(
        "foreground_compaction_p99_ratio",
        active_p99 / base_p99.max(1e-9),
        "x",
    );
    emit(
        "background_compaction_start_to_finish_ms",
        start.elapsed().as_secs_f64() * 1e3,
        "ms",
    );
    emit(
        "background_compaction_finish_joined_worker",
        if running_at_end { 1.0 } else { 0.0 },
        "bool",
    );
    let after = db.stats();
    emit(
        "background_compaction_post_stale_bytes",
        after.stale_bytes as f64,
        "bytes",
    );
    Ok(())
}

fn crash_recovery() -> io::Result<()> {
    let dir = Scratch::new("exit")?;
    let status = Command::new(std::env::current_exe()?)
        .arg("--abrupt-child")
        .arg(&dir.0)
        .status()?;
    if !status.success() {
        return Err(io::Error::other("abrupt child failed"));
    }
    let start = Instant::now();
    let db = dir.open(Durability::Sync, 64 * 1024)?;
    let good = db.len() == 200
        && (0..200).all(|i| db.get(key(i)).ok().flatten().as_deref() == Some(VALUE.as_slice()));
    emit(
        "abrupt_process_exit_recovered_keys",
        db.len() as f64,
        "keys",
    );
    emit(
        "abrupt_process_exit_recovery_ms",
        start.elapsed().as_secs_f64() * 1e3,
        "ms",
    );
    emit(
        "abrupt_process_exit_verify",
        if good { 1.0 } else { 0.0 },
        "bool",
    );
    if !good {
        return Err(io::Error::other(
            "abrupt process recovery verification failed",
        ));
    }
    drop(db);
    let dir = Scratch::new("torn-tail")?;
    let mut db = dir.open(Durability::Manual, 64 * 1024)?;
    populate(&mut db, 200, 0)?;
    drop(db);
    let last = fs::read_dir(&dir.0)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("segment-") && n.ends_with(".log"))
        })
        .max()
        .ok_or_else(|| io::Error::other("missing active log"))?;
    let original = fs::metadata(&last)?.len();
    OpenOptions::new()
        .append(true)
        .open(&last)?
        .write_all(&[0x42; 7])?;
    let start = Instant::now();
    let db = dir.open(Durability::Manual, 64 * 1024)?;
    let good = db.len() == 200 && fs::metadata(&last)?.len() == original;
    emit(
        "torn_tail_recovery_ms",
        start.elapsed().as_secs_f64() * 1e3,
        "ms",
    );
    emit(
        "torn_tail_recovered_without_loss",
        if good { 1.0 } else { 0.0 },
        "bool",
    );
    if !good {
        return Err(io::Error::other("torn tail recovery verification failed"));
    }
    Ok(())
}

fn abrupt_child(path: &Path) -> io::Result<()> {
    let mut db = Bitcask::open_with_options(path, options(Durability::Sync, 64 * 1024))?;
    for i in 0..200 {
        db.put(key(i), VALUE)?;
    }
    process::exit(0);
}

fn run() -> io::Result<()> {
    println!("PERF_RUN|name=Bitcask; workload=deterministic; key=8B; default_value=64B; caveat=host_specific");
    println!(
        "PERF_HOST|os={};arch={};available_parallelism={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        thread::available_parallelism().map_or(0, |p| p.get())
    );
    if let Ok(cpu) = fs::read_to_string("/proc/cpuinfo") {
        if let Some(model) = cpu.lines().find(|l| l.starts_with("model name")) {
            println!("PERF_HOST|{}", model.trim());
        }
    }
    basic()?;
    durability_batches()?;
    deletes_churn()?;
    compaction()?;
    recovery()?;
    concurrent_reads()?;
    mix_sizes()?;
    rotation_fds()?;
    background()?;
    crash_recovery()?;
    println!("PERF_RUN|status=complete");
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--abrupt-child") {
        return abrupt_child(Path::new(
            args.get(2)
                .ok_or_else(|| io::Error::other("missing path"))?,
        ));
    }
    run()
}
