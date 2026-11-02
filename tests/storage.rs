use bitcask_lite::{Bitcask, Durability, Options, MAX_KEY, MAX_VALUE};
use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct Directory(PathBuf);

impl Directory {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        let path = std::env::temp_dir().join(format!(
            "bitcask-test-{}-{}-{}",
            std::process::id(),
            time.as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn active_path(directory: &std::path::Path) -> io::Result<PathBuf> {
    let manifest = fs::read_to_string(directory.join("MANIFEST"))?;
    let id = manifest
        .lines()
        .last()
        .ok_or_else(|| io::Error::other("empty manifest"))?;
    Ok(directory.join(format!("segment-{id}.log")))
}

fn small_segments(compact_after_segments: usize) -> Options {
    Options {
        durability: Durability::Sync,
        max_segment_bytes: 100,
        compact_after_segments,
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
        }
    }
    !crc
}

#[test]
fn crud_survives_reopen_and_reads_do_not_overwrite_the_log() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"name", b"first")?;
    db.put(b"empty", b"")?;
    assert_eq!(db.get(b"name")?, Some(b"first".to_vec()));
    db.put(b"name", b"second")?;
    db.put([0, 255], [255, 0, 128])?;
    assert!(db.delete(b"name")?);
    assert!(!db.delete(b"name")?);
    assert_eq!(db.get(b"name")?, None);
    assert_eq!(db.len(), 2);
    drop(db);
    let db = Bitcask::open(&dir.0)?;
    assert_eq!(db.get(b"empty")?, Some(vec![]));
    assert_eq!(db.get([0, 255])?, Some(vec![255, 0, 128]));
    assert_eq!(db.get(b"name")?, None);
    Ok(())
}

#[test]
fn every_incomplete_final_record_prefix_is_recovered() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"saved", b"intact")?;
    let path = active_path(&dir.0)?;
    let prefix = fs::metadata(&path)?.len() as usize;
    db.put(b"tail", b"a value whose write will be interrupted")?;
    drop(db);
    let complete = fs::read(&path)?;
    for length in prefix..complete.len() {
        fs::write(&path, &complete[..length])?;
        let mut db = Bitcask::open(&dir.0)?;
        assert_eq!(db.get(b"saved")?, Some(b"intact".to_vec()));
        assert_eq!(db.get(b"tail")?, None);
        assert_eq!(fs::metadata(&path)?.len(), prefix as u64);
        db.put(b"after", b"recovery")?;
        drop(db);
        let reopened = Bitcask::open(&dir.0)?;
        assert_eq!(reopened.get(b"after")?, Some(b"recovery".to_vec()));
    }
    Ok(())
}

#[test]
fn complete_corruption_is_reported_without_truncation() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"key", b"value")?;
    let path = active_path(&dir.0)?;
    drop(db);
    let original = fs::read(&path)?;
    for offset in [0, 4, 8, 12, 16, 20, 24, original.len() - 1] {
        let mut damaged = original.clone();
        damaged[offset] ^= 1;
        fs::write(&path, &damaged)?;
        let error = Bitcask::open(&dir.0).err().expect("corruption must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(fs::read(&path)?, damaged);
    }
    Ok(())
}

#[test]
fn valid_header_checksum_cannot_bypass_length_or_kind_limits() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"key", b"value")?;
    let path = active_path(&dir.0)?;
    drop(db);
    let original = fs::read(&path)?;
    for (offset, value) in [(4, u32::MAX), (8, u32::MAX), (12, 2), (4, 0), (12, 1)] {
        let mut damaged = original.clone();
        damaged[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        let crc = checksum(&damaged[..20]);
        damaged[20..24].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path, &damaged)?;
        let error = Bitcask::open(&dir.0)
            .err()
            .expect("invalid header must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(fs::read(&path)?, damaged);
    }
    Ok(())
}

#[test]
fn compaction_preserves_latest_values_and_deletes() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    for value in 0_u64..20 {
        db.put(b"key", value.to_le_bytes())?;
    }
    db.put(b"deleted", b"gone")?;
    db.delete(b"deleted")?;
    db.put(b"empty", b"")?;
    let path = active_path(&dir.0)?;
    let before = fs::metadata(&path)?.len();
    drop(db);
    let orphan = dir.0.join("segment-00000000000000000999.log");
    fs::write(&orphan, b"unpublished compaction")?;
    let mut db = Bitcask::open(&dir.0)?;
    assert_eq!(db.get(b"key")?, Some(19_u64.to_le_bytes().to_vec()));
    db.compact()?;
    assert!(fs::metadata(active_path(&dir.0)?)?.len() < before);
    assert!(!orphan.exists());
    db.put(b"after", b"compaction")?;
    drop(db);
    let db = Bitcask::open(&dir.0)?;
    assert_eq!(db.len(), 3);
    assert_eq!(db.get(b"key")?, Some(19_u64.to_le_bytes().to_vec()));
    assert_eq!(db.get(b"deleted")?, None);
    assert_eq!(db.get(b"empty")?, Some(vec![]));
    assert_eq!(db.get(b"after")?, Some(b"compaction".to_vec()));
    Ok(())
}

#[test]
fn lock_excludes_other_handles_and_is_released_on_drop() -> io::Result<()> {
    let dir = Directory::new()?;
    let db = Bitcask::open(&dir.0)?;
    let error = Bitcask::open(&dir.0).err().expect("second open must fail");
    assert_eq!(error.kind(), ErrorKind::WouldBlock);
    drop(db);
    let _reopened = Bitcask::open(&dir.0)?;
    Ok(())
}

#[test]
fn empty_database_and_repeated_compaction_are_supported() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.compact()?;
    db.put(b"temporary", b"value")?;
    db.delete(b"temporary")?;
    db.compact()?;
    db.compact()?;
    assert!(db.is_empty());
    assert_eq!(fs::metadata(active_path(&dir.0)?)?.len(), 0);
    drop(db);
    assert!(Bitcask::open(&dir.0)?.is_empty());
    Ok(())
}

#[test]
fn deterministic_operations_match_a_reference_map() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_durability(&dir.0, Durability::Manual)?;
    let mut model = HashMap::new();
    let mut seed = 7_u64;
    for step in 0..1_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let key = ((seed >> 32) % 80).to_le_bytes().to_vec();
        if seed.is_multiple_of(5) {
            assert_eq!(db.delete(&key)?, model.remove(&key).is_some());
        } else {
            let value = seed.to_le_bytes().to_vec();
            db.put(&key, &value)?;
            model.insert(key.clone(), value);
        }
        assert_eq!(db.get(&key)?, model.get(&key).cloned());
        if step % 97 == 0 {
            db.compact()?;
        }
        if step % 151 == 0 {
            db.sync()?;
            drop(db);
            db = Bitcask::open_with_durability(&dir.0, Durability::Manual)?;
        }
    }
    db.sync()?;
    drop(db);
    let db = Bitcask::open(&dir.0)?;
    assert_eq!(db.len(), model.len());
    for number in 0_u64..80 {
        assert_eq!(
            db.get(number.to_le_bytes())?,
            model.get(number.to_le_bytes().as_slice()).cloned()
        );
    }
    Ok(())
}

#[test]
fn invalid_input_leaves_the_handle_usable() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"saved", b"value")?;
    assert_eq!(
        db.put(b"", b"v").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        db.put(vec![1; MAX_KEY + 1], b"v").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        db.put(b"k", vec![1; MAX_VALUE + 1]).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(db.get(b"saved")?, Some(b"value".to_vec()));
    db.put(b"next", b"okay")?;
    Ok(())
}

#[test]
fn reads_check_integrity_and_failed_compaction_poison_is_visible() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"key", b"value")?;
    let path = active_path(&dir.0)?;
    let mut bytes = fs::read(&path)?;
    bytes[24] ^= 1;
    fs::write(&path, &bytes)?;
    assert_eq!(db.get(b"key").unwrap_err().kind(), ErrorKind::InvalidData);
    assert_eq!(db.compact().unwrap_err().kind(), ErrorKind::InvalidData);
    assert!(db.put(b"another", b"value").is_err());
    assert!(db.get(b"key").is_err());
    assert!(db.sync().is_err());
    assert_eq!(fs::read(&path)?, bytes);
    Ok(())
}

#[test]
fn disk_checksums_match_an_independent_crc32_implementation() -> io::Result<()> {
    assert_eq!(checksum(b"123456789"), 0xcbf4_3926);
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"key", b"value")?;
    let bytes = fs::read(active_path(&dir.0)?)?;
    assert_eq!(&bytes[..4], b"BCK1");
    assert_eq!(&bytes[16..20], &checksum(&bytes[24..]).to_le_bytes());
    assert_eq!(&bytes[20..24], &checksum(&bytes[..20]).to_le_bytes());
    Ok(())
}

#[test]
fn rotations_preserve_old_values_and_tombstones_across_reopen() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    for n in 0..20_u8 {
        db.put([n], vec![n; 40])?;
    }
    assert!(db.stats().segments > 10);
    assert!(db.delete([0])?);
    assert!(db.delete([5])?);
    drop(db);
    let db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    assert_eq!(db.len(), 18);
    assert_eq!(db.get([0])?, None);
    assert_eq!(db.get([5])?, None);
    for n in 1..20_u8 {
        if n != 5 {
            assert_eq!(db.get([n])?, Some(vec![n; 40]));
        }
    }
    Ok(())
}

#[test]
fn automatic_compaction_reclaims_overwritten_records() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(3))?;
    for n in 0..5_u8 {
        db.put(b"same", vec![n; 44])?;
    }
    db.finish_compaction()?;
    assert!(db.stats().segments < 5);
    for n in 5..12_u8 {
        db.put(b"same", vec![n; 44])?;
    }
    db.finish_compaction()?;
    assert_eq!(db.get(b"same")?, Some(vec![11; 44]));
    assert!(db.stats().total_bytes < 12 * 72);
    drop(db);
    let reopened = Bitcask::open_with_options(&dir.0, small_segments(3))?;
    assert_eq!(reopened.get(b"same")?, Some(vec![11; 44]));
    assert!(reopened.stats().segments < 12);
    Ok(())
}

#[test]
fn manual_durability_with_rotation_and_tail_recovery() -> io::Result<()> {
    let dir = Directory::new()?;
    let options = Options {
        durability: Durability::Manual,
        ..small_segments(0)
    };
    let mut db = Bitcask::open_with_options(&dir.0, options)?;
    db.put(b"first", vec![1; 55])?;
    db.put(b"second", vec![2; 55])?;
    assert_eq!(db.stats().segments, 2);
    db.sync()?;
    drop(db);
    let active = active_path(&dir.0)?;
    let mut contents = fs::read(&active)?;
    contents.extend_from_slice(b"BCK");
    fs::write(&active, contents)?;
    let reopened = Bitcask::open_with_options(&dir.0, options)?;
    assert_eq!(reopened.get(b"first")?, Some(vec![1; 55]));
    assert_eq!(reopened.get(b"second")?, Some(vec![2; 55]));
    assert_eq!(reopened.stats().segments, 2);
    Ok(())
}

#[test]
fn compaction_preserves_many_live_keys_across_multiple_outputs() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    for n in 0..25_u8 {
        db.put([n], vec![n; 50])?;
    }
    db.put([0], vec![99; 50])?;
    let before = db.stats().total_bytes;
    db.compact()?;
    let stats = db.stats();
    assert!(stats.segments > 1);
    assert_eq!(stats.stale_bytes, 0);
    assert!(stats.total_bytes < before);
    drop(db);
    let reopened = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    for n in 0..25_u8 {
        let expected = if n == 0 { 99 } else { n };
        assert_eq!(reopened.get([n])?, Some(vec![expected; 50]));
    }
    Ok(())
}

#[test]
fn legacy_single_log_is_migrated_without_changing_records() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"legacy", b"still readable")?;
    drop(db);
    let original = fs::read(active_path(&dir.0)?)?;
    fs::rename(active_path(&dir.0)?, dir.0.join("data.log"))?;
    fs::remove_file(dir.0.join("MANIFEST"))?;
    let reopened = Bitcask::open(&dir.0)?;
    assert_eq!(reopened.get(b"legacy")?, Some(b"still readable".to_vec()));
    assert!(!dir.0.join("data.log").exists());
    assert_eq!(fs::read(active_path(&dir.0)?)?, original);
    Ok(())
}

#[test]
fn invalid_manifest_is_rejected_without_modifying_data() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"safe", b"value")?;
    drop(db);
    let data = fs::read(active_path(&dir.0)?)?;
    fs::write(dir.0.join("MANIFEST"), b"BCKM2\nnot-a-segment\n")?;
    assert_eq!(
        Bitcask::open(&dir.0)
            .err()
            .expect("bad manifest must fail")
            .kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        fs::read(dir.0.join("segment-00000000000000000000.log"))?,
        data
    );
    Ok(())
}

#[test]
fn invalid_options_and_oversized_single_records() -> io::Result<()> {
    let dir = Directory::new()?;
    let invalid = Options {
        max_segment_bytes: 10,
        ..Options::default()
    };
    assert_eq!(
        Bitcask::open_with_options(&dir.0, invalid)
            .err()
            .expect("invalid options")
            .kind(),
        ErrorKind::InvalidInput
    );
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    db.put(b"large", vec![3; 180])?;
    assert_eq!(db.stats().segments, 1);
    db.put(b"next", b"value")?;
    assert_eq!(db.stats().segments, 2);
    assert_eq!(db.get(b"large")?, Some(vec![3; 180]));
    Ok(())
}

#[test]
fn unlisted_segments_are_never_replayed() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"good", b"data")?;
    drop(db);
    let orphan = dir.0.join("segment-00000000000000000001.log");
    fs::write(&orphan, b"not a valid record")?;
    let mut reopened = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    assert_eq!(reopened.get(b"good")?, Some(b"data".to_vec()));
    reopened.put(b"more", vec![4; 85])?;
    reopened.put(b"rotate", vec![5; 85])?;
    assert!(dir.0.join("segment-00000000000000000002.log").exists());
    assert!(orphan.exists());
    Ok(())
}

#[test]
fn batch_writes_cross_segments_and_survive_reopen() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    let first = [1_u8; 50];
    let second = [2_u8; 50];
    db.put_batch(&[(b"one", &first), (b"two", &second), (b"three", b"last")])?;
    assert!(db.stats().segments >= 2);
    let bad = vec![7_u8; MAX_KEY + 1];
    assert_eq!(
        db.put_batch(&[(b"valid", b"value"), (&bad, b"invalid")])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(db.get(b"valid")?, None);
    drop(db);
    let db = Bitcask::open(&dir.0)?;
    assert_eq!(db.get(b"one")?, Some(first.to_vec()));
    assert_eq!(db.get(b"two")?, Some(second.to_vec()));
    assert_eq!(db.get(b"three")?, Some(b"last".to_vec()));
    Ok(())
}

#[test]
fn concurrent_readers_share_one_database_safely() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    for n in 0..48_u8 {
        db.put([n], vec![n; 60])?;
    }
    let shared = Arc::new(db);
    let mut readers = Vec::new();
    for _ in 0..8 {
        let db = Arc::clone(&shared);
        readers.push(std::thread::spawn(move || -> io::Result<()> {
            for _ in 0..10 {
                for n in (0..48_u8).rev() {
                    assert_eq!(db.get([n])?, Some(vec![n; 60]));
                }
            }
            Ok(())
        }));
    }
    for reader in readers {
        reader
            .join()
            .map_err(|_| io::Error::other("reader panicked"))??;
    }
    Ok(())
}

#[test]
fn background_compaction_preserves_concurrent_updates_and_deletes() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    db.put(b"a", vec![1; 55])?;
    db.put(b"b", vec![2; 55])?;
    db.put(b"c", vec![3; 55])?;
    assert!(db.start_compaction()?);
    assert!(!db.start_compaction()?);
    db.put(b"a", vec![8; 55])?;
    assert!(db.delete(b"b")?);
    db.put(b"d", vec![4; 55])?;
    db.finish_compaction()?;
    assert!(!db.finish_compaction()?);
    assert_eq!(db.get(b"a")?, Some(vec![8; 55]));
    assert_eq!(db.get(b"b")?, None);
    assert_eq!(db.get(b"c")?, Some(vec![3; 55]));
    assert_eq!(db.get(b"d")?, Some(vec![4; 55]));
    drop(db);
    let db = Bitcask::open(&dir.0)?;
    assert_eq!(db.get(b"a")?, Some(vec![8; 55]));
    assert_eq!(db.get(b"b")?, None);
    assert_eq!(db.get(b"c")?, Some(vec![3; 55]));
    assert_eq!(db.get(b"d")?, Some(vec![4; 55]));
    Ok(())
}

#[test]
fn background_compaction_does_not_require_monotonic_manifest_ids() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    for n in 0..6_u8 {
        db.put([n], vec![n; 48])?;
    }
    assert!(db.start_compaction()?);
    db.put(b"newer", vec![9; 48])?;
    db.finish_compaction()?;
    let manifest = fs::read_to_string(dir.0.join("MANIFEST"))?;
    let ids: Vec<u64> = manifest
        .lines()
        .skip(1)
        .map(|line| line.parse::<u64>())
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    assert!(ids.windows(2).any(|pair| pair[0] > pair[1]));
    drop(db);
    let db = Bitcask::open_with_options(&dir.0, small_segments(0))?;
    for n in 0..6_u8 {
        assert_eq!(db.get([n])?, Some(vec![n; 48]));
    }
    assert_eq!(db.get(b"newer")?, Some(vec![9; 48]));
    Ok(())
}

#[test]
fn compaction_without_sealed_segments_is_a_noop() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    assert!(!db.start_compaction()?);
    assert!(!db.poll_compaction()?);
    assert!(!db.finish_compaction()?);
    db.put(b"key", b"value")?;
    assert!(!db.start_compaction()?);
    Ok(())
}

#[test]
fn clean_database_does_not_auto_compact_just_for_segment_count() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open_with_options(&dir.0, small_segments(3))?;
    for n in 0..8_u8 {
        db.put([n], vec![n; 60])?;
    }
    assert_eq!(db.stats().stale_bytes, 0);
    assert!(db.stats().segments >= 8);
    assert!(!db.finish_compaction()?);
    Ok(())
}

#[test]
fn stats_follow_overwrites_deletes_and_full_compaction() -> io::Result<()> {
    let dir = Directory::new()?;
    let mut db = Bitcask::open(&dir.0)?;
    db.put(b"k", b"1234")?;
    assert_eq!(db.stats().total_bytes, 29);
    assert_eq!(db.stats().live_bytes, 29);
    db.put(b"k", b"12")?;
    assert_eq!(db.stats().total_bytes, 56);
    assert_eq!(db.stats().live_bytes, 27);
    db.delete(b"k")?;
    assert_eq!(db.stats().live_bytes, 0);
    assert_eq!(db.stats().stale_bytes, db.stats().total_bytes);
    db.compact()?;
    assert_eq!(db.stats().total_bytes, 0);
    assert_eq!(db.stats().stale_bytes, 0);
    Ok(())
}

#[test]
fn stale_background_files_are_cleaned_on_reopen() -> io::Result<()> {
    let dir = Directory::new()?;
    let db = Bitcask::open(&dir.0)?;
    drop(db);
    let abandoned = dir.0.join(".compact-old-000000.tmp");
    fs::write(&abandoned, b"incomplete output")?;
    let _reopened = Bitcask::open(&dir.0)?;
    assert!(!abandoned.exists());
    Ok(())
}

#[test]
fn duplicate_manifest_segment_is_rejected() -> io::Result<()> {
    let dir = Directory::new()?;
    drop(Bitcask::open(&dir.0)?);
    fs::write(
        dir.0.join("MANIFEST"),
        b"BCKM2\n00000000000000000000\n00000000000000000000\n",
    )?;
    let error = Bitcask::open(&dir.0)
        .err()
        .expect("duplicate id should fail");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    Ok(())
}
