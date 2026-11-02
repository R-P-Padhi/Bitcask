use bitcask_lite::{Bitcask, Durability, Options};
use std::fs;
use std::io;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn main() -> io::Result<()> {
    const N: u64 = 20_000;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let path = std::env::temp_dir().join(format!(
        "bitcask-bench-{}-{}",
        std::process::id(),
        stamp.as_nanos()
    ));
    let options = Options {
        durability: Durability::Manual,
        max_segment_bytes: 64 * 1024,
        compact_after_segments: 0,
    };
    let mut db = Bitcask::open_with_options(&path, options)?;
    let start = Instant::now();
    for key in 0..N {
        db.put(key.to_le_bytes(), [42_u8; 64])?;
    }
    db.sync()?;
    let writes = start.elapsed();
    let start = Instant::now();
    for iteration in 0..N {
        let key = iteration.wrapping_mul(6364136223846793005) % N;
        let result = db.get(key.to_le_bytes())?;
        std::hint::black_box(result);
    }
    let reads = start.elapsed();
    let before = db.stats();
    let start = Instant::now();
    db.compact()?;
    let compact = start.elapsed();
    println!(
        "{N} writes + one sync: {writes:?} ({:.0}/s)",
        N as f64 / writes.as_secs_f64()
    );
    println!(
        "{N} random reads: {reads:?} ({:.0}/s)",
        N as f64 / reads.as_secs_f64()
    );
    println!(
        "compaction: {compact:?}; before: {before:?}; after: {:?}",
        db.stats()
    );
    drop(db);
    fs::remove_dir_all(path)?;
    Ok(())
}
