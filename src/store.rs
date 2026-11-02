use crate::background::{self, Candidate, Prepared};
use crate::codec::{
    allocation_error, copy_bytes, corrupt, encode, read_live_at, read_record, read_value_at,
    too_large, validate_lengths,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const MANIFEST: &str = "MANIFEST";
const MANIFEST_NEXT: &str = "MANIFEST.next";
const MANIFEST_MAGIC: &str = "BCKM2";
const FILE_CACHE_LIMIT: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Durability {
    /// Synchronize each mutation before reporting success.
    Sync,
    /// Call `Bitcask::sync` to establish a durability boundary.
    Manual,
}

/// Controls segment rotation and automatic compaction.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub durability: Durability,
    /// Rotate before a write would exceed this size (unless the record alone is larger).
    pub max_segment_bytes: u64,
    /// Compact on rotation when this many segments exist. Set to 0 to disable.
    pub compact_after_segments: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            durability: Durability::Sync,
            max_segment_bytes: 8 * 1024 * 1024,
            compact_after_segments: 8,
        }
    }
}

/// Sizes include obsolete records and tombstones until compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stats {
    pub segments: usize,
    pub live_keys: usize,
    pub total_bytes: u64,
    pub live_bytes: u64,
    pub stale_bytes: u64,
}

#[derive(Clone, Copy)]
struct Location {
    segment: u64,
    offset: u64,
    size: u64,
}

#[derive(Clone)]
struct Segment {
    id: u64,
    file: Option<Arc<File>>,
    len: u64,
}

struct CompactionTask {
    sources: Vec<u64>,
    worker: JoinHandle<io::Result<Prepared>>,
}

#[derive(Default)]
struct FileCache {
    files: HashMap<u64, Arc<File>>,
    order: VecDeque<u64>,
}

/// A single-owner append-only database with a bounded active segment.
/// Failed mutations poison the handle; reopen it to recover.
pub struct Bitcask {
    directory: PathBuf,
    directory_file: File,
    segments: Vec<Segment>,
    segment_lookup: HashMap<u64, usize>,
    file_cache: Mutex<FileCache>,
    index: HashMap<Vec<u8>, Location>,
    total_bytes: u64,
    live_bytes: u64,
    next_id: u64,
    options: Options,
    failed: bool,
    background: Option<CompactionTask>,
    _lock: File,
}

impl Drop for Bitcask {
    fn drop(&mut self) {
        if let Some(task) = self.background.take() {
            if let Ok(Ok(prepared)) = task.worker.join() {
                for output in prepared.outputs {
                    let _ = fs::remove_file(output.path);
                }
            }
        }
    }
}

impl Bitcask {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with_options(path, Options::default())
    }

    pub fn open_with_durability(
        path: impl AsRef<Path>,
        durability: Durability,
    ) -> io::Result<Self> {
        Self::open_with_options(
            path,
            Options {
                durability,
                ..Options::default()
            },
        )
    }

    /// Open or create a database. The parent directory must already exist.
    pub fn open_with_options(path: impl AsRef<Path>, options: Options) -> io::Result<Self> {
        if options.max_segment_bytes < 25
            || (options.compact_after_segments != 0 && options.compact_after_segments < 2)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid segment options",
            ));
        }
        match fs::create_dir(path.as_ref()) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let directory = fs::canonicalize(path)?;
        let directory_file = File::open(&directory)?;
        let lock = open_file(&directory.join("LOCK"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(ErrorKind::WouldBlock, "database is open"));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
        if let Some(parent) = directory.parent() {
            File::open(parent)?.sync_all()?;
        }
        clean_abandoned_temp(&directory);

        let ids = if directory.join(MANIFEST).exists() {
            read_manifest(&directory)?
        } else {
            initialize(&directory, &directory_file)?
        };
        let next_id = next_unused_id(&directory, &ids)?;
        let mut segments = Vec::new();
        segments.try_reserve(ids.len()).map_err(allocation_error)?;
        let mut segment_lookup = HashMap::new();
        segment_lookup
            .try_reserve(ids.len())
            .map_err(allocation_error)?;
        let mut index = HashMap::new();
        for (position, &id) in ids.iter().enumerate() {
            let path = segment_path(&directory, id);
            let mut file = if position + 1 == ids.len() {
                OpenOptions::new().read(true).write(true).open(&path)?
            } else {
                File::open(&path)?
            };
            let size = file.metadata()?.len();
            let mut end = 0;
            while end < size {
                let Some(record) = read_record(&mut file, size - end)? else {
                    break;
                };
                if record.deleted {
                    index.remove(record.key());
                } else {
                    index.try_reserve(1).map_err(allocation_error)?;
                    index.insert(
                        copy_bytes(record.key())?,
                        Location {
                            segment: id,
                            offset: end,
                            size: record.size(),
                        },
                    );
                }
                end += record.size();
            }
            if end != size {
                if position + 1 != ids.len() {
                    return Err(corrupt("incomplete record in sealed segment"));
                }
                file.set_len(end)?;
                file.sync_all()?;
            }
            segment_lookup.insert(id, segments.len());
            segments.push(Segment {
                id,
                file: (position + 1 == ids.len()).then(|| Arc::new(file)),
                len: end,
            });
        }
        directory_file.sync_all()?;
        let total_bytes = segments
            .iter()
            .fold(0_u64, |sum, segment| sum.saturating_add(segment.len));
        let live_bytes = index.values().fold(0_u64, |sum: u64, location: &Location| {
            sum.saturating_add(location.size)
        });
        Ok(Self {
            directory,
            directory_file,
            segments,
            segment_lookup,
            file_cache: Mutex::new(FileCache::default()),
            index,
            total_bytes,
            live_bytes,
            next_id,
            options,
            failed: false,
            background: None,
            _lock: lock,
        })
    }

    /// Positional reads don't alter a shared file cursor, allowing concurrent readers.
    pub fn get(&self, key: impl AsRef<[u8]>) -> io::Result<Option<Vec<u8>>> {
        self.check_health()?;
        let key = key.as_ref();
        let Some(&location) = self.index.get(key) else {
            return Ok(None);
        };
        let (file, len) = self.file_for(location.segment)?;
        Ok(Some(read_value_at(&file, len, location.offset, key)?))
    }

    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> io::Result<()> {
        self.append(key.as_ref(), value.as_ref(), false)
    }

    /// Apply a group of puts and synchronize once in `Sync` mode.
    /// This is not a transaction: earlier writes can survive a later failure.
    pub fn put_batch(&mut self, entries: &[(&[u8], &[u8])]) -> io::Result<()> {
        self.check_health()?;
        for &(key, value) in entries {
            validate_lengths(key.len(), value.len(), false)?;
        }
        let durability = self.options.durability;
        self.options.durability = Durability::Manual;
        let result = (|| {
            for &(key, value) in entries {
                self.put(key, value)?;
            }
            if durability == Durability::Sync && !entries.is_empty() {
                self.sync()?;
            }
            Ok(())
        })();
        self.options.durability = durability;
        result
    }

    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> io::Result<bool> {
        self.check_health()?;
        let key = key.as_ref();
        if !self.index.contains_key(key) {
            return Ok(false);
        }
        self.append(key, &[], true)?;
        Ok(true)
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            segments: self.segments.len(),
            live_keys: self.index.len(),
            total_bytes: self.total_bytes,
            live_bytes: self.live_bytes,
            stale_bytes: self.total_bytes.saturating_sub(self.live_bytes),
        }
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.check_health()?;
        self.failed = true;
        self.active()?
            .file
            .as_ref()
            .ok_or_else(|| corrupt("missing active file"))?
            .sync_all()?;
        self.failed = false;
        Ok(())
    }

    /// Prepare a rewrite of sealed segments on a worker thread. Writes may continue.
    /// Returns false when no sealed data exists or a worker is already running.
    pub fn start_compaction(&mut self) -> io::Result<bool> {
        self.check_health()?;
        if self.background.is_some() || self.segments.len() < 2 {
            return Ok(false);
        }
        let sources: Vec<u64> = self.segments[..self.segments.len() - 1]
            .iter()
            .map(|s| s.id)
            .collect();
        let included: HashSet<u64> = sources.iter().copied().collect();
        let mut entries = Vec::new();
        entries
            .try_reserve(self.index.len())
            .map_err(allocation_error)?;
        for (key, location) in &self.index {
            if included.contains(&location.segment) {
                let position = *self
                    .segment_lookup
                    .get(&location.segment)
                    .ok_or_else(|| corrupt("missing indexed segment"))?;
                entries.push(Candidate {
                    key: copy_bytes(key)?,
                    old_segment: location.segment,
                    old_offset: location.offset,
                    end: self.segments[position].len,
                });
            }
        }
        entries.sort_unstable_by_key(|entry| {
            (
                self.segment_lookup
                    .get(&entry.old_segment)
                    .copied()
                    .unwrap_or(usize::MAX),
                entry.old_offset,
            )
        });
        let worker = background::spawn(
            self.directory.clone(),
            entries,
            self.options.max_segment_bytes,
        );
        self.background = Some(CompactionTask { sources, worker });
        Ok(true)
    }

    /// Publish a prepared background rewrite if it has finished, without waiting for the worker.
    pub fn poll_compaction(&mut self) -> io::Result<bool> {
        if self
            .background
            .as_ref()
            .is_some_and(|task| task.worker.is_finished())
        {
            self.finish_compaction()
        } else {
            Ok(false)
        }
    }

    /// Wait for the worker, publish its result and retire the old sealed segments.
    pub fn finish_compaction(&mut self) -> io::Result<bool> {
        self.check_health()?;
        let Some(task) = self.background.take() else {
            return Ok(false);
        };
        let prepared = task
            .worker
            .join()
            .map_err(|_| io::Error::other("compaction worker panicked"))??;
        self.publish_prepared(&task.sources, prepared)?;
        Ok(true)
    }

    fn publish_prepared(&mut self, sources: &[u64], prepared: Prepared) -> io::Result<()> {
        if sources.len() >= self.segments.len()
            || !sources
                .iter()
                .zip(&self.segments)
                .all(|(&id, segment)| id == segment.id)
        {
            return Err(corrupt("compaction source set changed"));
        }
        self.failed = true;
        let old_bytes = self.segments[..sources.len()]
            .iter()
            .fold(0_u64, |sum, segment| sum.saturating_add(segment.len));
        let new_bytes = prepared
            .outputs
            .iter()
            .fold(0_u64, |sum, segment| sum.saturating_add(segment.len));
        let mut next_id = self.next_id;
        let mut ids = Vec::new();
        let mut replacement = Vec::new();
        replacement
            .try_reserve(prepared.outputs.len() + self.segments.len() - sources.len())
            .map_err(allocation_error)?;
        for output in &prepared.outputs {
            let id = next_id;
            next_id = next_id.checked_add(1).ok_or_else(too_large)?;
            fs::rename(&output.path, segment_path(&self.directory, id))?;
            ids.push(id);
            replacement.push(Segment {
                id,
                file: None,
                len: output.len,
            });
        }
        for segment in &self.segments[sources.len()..] {
            ids.push(segment.id);
            replacement.push(segment.clone());
        }
        let mut lookup = HashMap::new();
        lookup
            .try_reserve(replacement.len())
            .map_err(allocation_error)?;
        for (position, segment) in replacement.iter().enumerate() {
            lookup.insert(segment.id, position);
        }
        publish_manifest(&self.directory, &self.directory_file, &ids)?;
        for rewritten in prepared.records {
            if let Some(current) = self.index.get_mut(&rewritten.key) {
                if current.segment == rewritten.old_segment
                    && current.offset == rewritten.old_offset
                {
                    *current = Location {
                        segment: ids[rewritten.output],
                        offset: rewritten.offset,
                        size: rewritten.size,
                    };
                }
            }
        }
        self.segments = replacement;
        self.segment_lookup = lookup;
        self.next_id = next_id;
        self.total_bytes = self
            .total_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        let mut cache = self
            .file_cache
            .lock()
            .map_err(|_| io::Error::other("file cache lock poisoned"))?;
        cache.files.clear();
        cache.order.clear();
        self.failed = false;
        clean_unlisted_segments(&self.directory, &ids);
        let _ = self.directory_file.sync_all();
        Ok(())
    }

    /// Rewrite live records into fresh segments, then atomically publish their manifest.
    pub fn compact(&mut self) -> io::Result<()> {
        self.finish_compaction()?;
        self.sync()?;
        self.failed = true;
        let mut replacement = Vec::<Segment>::new();
        let mut index = HashMap::new();
        index
            .try_reserve(self.index.len())
            .map_err(allocation_error)?;
        let mut next_id = self.next_id;
        let mut live: Vec<_> = self
            .index
            .iter()
            .map(|(key, &location)| (key, location))
            .collect();
        live.sort_unstable_by_key(|(_, location)| {
            (
                self.segment_lookup
                    .get(&location.segment)
                    .copied()
                    .unwrap_or(usize::MAX),
                location.offset,
            )
        });
        for (key, old) in live {
            let (source, end) = self.file_for(old.segment)?;
            let record = read_live_at(&source, end, old.offset, key)?;
            let bytes = encode(record.key(), record.value(), false)?;
            if replacement.last().is_none_or(|s| {
                s.len > 0
                    && s.len.saturating_add(bytes.len() as u64) > self.options.max_segment_bytes
            }) {
                if let Some(last) = replacement.last_mut() {
                    last.file
                        .as_ref()
                        .ok_or_else(|| corrupt("missing compaction file"))?
                        .sync_all()?;
                    last.file = None;
                }
                replacement.try_reserve(1).map_err(allocation_error)?;
                replacement.push(create_segment(&self.directory, next_id)?);
                next_id = next_id.checked_add(1).ok_or_else(too_large)?;
            }
            let output = replacement
                .last_mut()
                .ok_or_else(|| corrupt("no output segment"))?;
            let offset = output.len;
            write_all_at(
                output
                    .file
                    .as_ref()
                    .ok_or_else(|| corrupt("missing compaction file"))?,
                &bytes,
                offset,
            )?;
            output.len = output
                .len
                .checked_add(bytes.len() as u64)
                .ok_or_else(too_large)?;
            index.insert(
                copy_bytes(key)?,
                Location {
                    segment: output.id,
                    offset,
                    size: bytes.len() as u64,
                },
            );
        }
        if replacement.is_empty() {
            replacement.push(create_segment(&self.directory, next_id)?);
            next_id = next_id.checked_add(1).ok_or_else(too_large)?;
        }
        for segment in &replacement {
            if let Some(file) = &segment.file {
                file.sync_all()?;
            }
        }
        let ids: Vec<u64> = replacement.iter().map(|s| s.id).collect();
        publish_manifest(&self.directory, &self.directory_file, &ids)?;
        self.segments = replacement;
        self.index = index;
        self.next_id = next_id;
        self.total_bytes = self
            .segments
            .iter()
            .fold(0_u64, |sum, segment| sum.saturating_add(segment.len));
        self.live_bytes = self
            .index
            .values()
            .fold(0_u64, |sum, location| sum.saturating_add(location.size));
        self.rebuild_lookup()?;
        let mut cache = self
            .file_cache
            .lock()
            .map_err(|_| io::Error::other("file cache lock poisoned"))?;
        cache.files.clear();
        cache.order.clear();
        self.failed = false;
        // Abandoned/original segments are safe to delete only after publication.
        clean_unlisted_segments(&self.directory, &ids);
        let _ = self.directory_file.sync_all();
        Ok(())
    }

    fn append(&mut self, key: &[u8], value: &[u8], deleted: bool) -> io::Result<()> {
        self.check_health()?;
        self.poll_compaction()?;
        validate_lengths(key.len(), value.len(), deleted)?;
        let bytes = encode(key, value, deleted)?;
        let new_key = !deleted && !self.index.contains_key(key);
        let old_size = self.index.get(key).map_or(0, |location| location.size);
        let owned = if new_key {
            Some(copy_bytes(key)?)
        } else {
            None
        };
        if new_key {
            self.index.try_reserve(1).map_err(allocation_error)?;
        }
        let length = bytes.len() as u64;
        if self.needs_rotation(length) {
            self.rotate()?;
            if self.options.compact_after_segments != 0
                && self.segments.len() >= self.options.compact_after_segments
                && self.stats().stale_bytes >= self.stats().total_bytes / 3
            {
                self.start_compaction()?;
            }
        }
        if self.needs_rotation(length) {
            self.rotate()?;
        }
        self.failed = true;
        let durable = self.options.durability == Durability::Sync;
        let active = self.active()?;
        let offset = active.len;
        let file = active
            .file
            .as_ref()
            .ok_or_else(|| corrupt("missing active file"))?;
        if file.metadata()?.len() != offset {
            return Err(corrupt("log size changed outside the database"));
        }
        let end = offset.checked_add(length).ok_or_else(too_large)?;
        write_all_at(file, &bytes, offset)?;
        if durable {
            file.sync_all()?;
        }
        let location = Location {
            segment: active.id,
            offset,
            size: length,
        };
        active.len = end;
        self.total_bytes = self.total_bytes.saturating_add(length);
        self.live_bytes = self.live_bytes.saturating_sub(old_size);
        if !deleted {
            self.live_bytes = self.live_bytes.saturating_add(length);
        }
        if deleted {
            self.index.remove(key);
        } else if let Some(existing) = self.index.get_mut(key) {
            *existing = location;
        } else if let Some(owned) = owned {
            self.index.insert(owned, location);
        }
        self.failed = false;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.failed = true;
        self.active()?
            .file
            .as_ref()
            .ok_or_else(|| corrupt("missing active file"))?
            .sync_all()?;
        self.segments.try_reserve(1).map_err(allocation_error)?;
        self.segment_lookup
            .try_reserve(1)
            .map_err(allocation_error)?;
        let id = self.next_id;
        let next_id = id.checked_add(1).ok_or_else(too_large)?;
        let new = create_segment(&self.directory, id)?;
        new.file
            .as_ref()
            .ok_or_else(|| corrupt("missing new file"))?
            .sync_all()?;
        let mut ids: Vec<u64> = self.segments.iter().map(|s| s.id).collect();
        ids.push(id);
        publish_manifest(&self.directory, &self.directory_file, &ids)?;
        self.active()?.file = None;
        self.segment_lookup.insert(id, self.segments.len());
        self.segments.push(new);
        self.next_id = next_id;
        self.failed = false;
        Ok(())
    }

    fn needs_rotation(&self, length: u64) -> bool {
        self.segments.last().is_some_and(|s| {
            s.len > 0 && s.len.saturating_add(length) > self.options.max_segment_bytes
        })
    }

    fn active(&mut self) -> io::Result<&mut Segment> {
        self.segments
            .last_mut()
            .ok_or_else(|| corrupt("missing active segment"))
    }

    fn file_for(&self, id: u64) -> io::Result<(Arc<File>, u64)> {
        let &position = self
            .segment_lookup
            .get(&id)
            .ok_or_else(|| corrupt("missing indexed segment"))?;
        let segment = &self.segments[position];
        if let Some(file) = &segment.file {
            return Ok((Arc::clone(file), segment.len));
        }
        let mut cache = self
            .file_cache
            .lock()
            .map_err(|_| io::Error::other("file cache lock poisoned"))?;
        let file = if let Some(file) = cache.files.get(&id) {
            Arc::clone(file)
        } else {
            let file = Arc::new(File::open(segment_path(&self.directory, id))?);
            if cache.files.len() == FILE_CACHE_LIMIT {
                if let Some(old) = cache.order.pop_front() {
                    cache.files.remove(&old);
                }
            }
            cache.files.insert(id, Arc::clone(&file));
            file
        };
        if let Some(position) = cache.order.iter().position(|&cached| cached == id) {
            cache.order.remove(position);
        }
        cache.order.push_back(id);
        Ok((file, segment.len))
    }

    fn rebuild_lookup(&mut self) -> io::Result<()> {
        let mut lookup = HashMap::new();
        lookup
            .try_reserve(self.segments.len())
            .map_err(allocation_error)?;
        for (position, segment) in self.segments.iter().enumerate() {
            lookup.insert(segment.id, position);
        }
        self.segment_lookup = lookup;
        Ok(())
    }

    fn check_health(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "previous I/O failure; drop and reopen database",
            ));
        }
        Ok(())
    }
}

fn open_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(ErrorKind::WriteZero, "short segment write"));
        }
        bytes = &bytes[written..];
        offset = offset.checked_add(written as u64).ok_or_else(too_large)?;
    }
    Ok(())
}

fn segment_name(id: u64) -> String {
    format!("segment-{id:020}.log")
}
fn segment_path(directory: &Path, id: u64) -> PathBuf {
    directory.join(segment_name(id))
}

fn create_segment(directory: &Path, id: u64) -> io::Result<Segment> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(segment_path(directory, id))?;
    Ok(Segment {
        id,
        file: Some(Arc::new(file)),
        len: 0,
    })
}

fn initialize(directory: &Path, directory_file: &File) -> io::Result<Vec<u64>> {
    let first = segment_path(directory, 0);
    let legacy = directory.join("data.log");
    if legacy.exists() {
        if first.exists() {
            return Err(corrupt("conflicting legacy and segmented logs"));
        }
        fs::rename(legacy, &first)?;
    } else if !first.exists() {
        let file = create_segment(directory, 0)?;
        file.file
            .as_ref()
            .ok_or_else(|| corrupt("missing initial file"))?
            .sync_all()?;
    }
    File::open(&first)?.sync_all()?;
    publish_manifest(directory, directory_file, &[0])?;
    Ok(vec![0])
}

fn read_manifest(directory: &Path) -> io::Result<Vec<u64>> {
    let contents = fs::read_to_string(directory.join(MANIFEST))?;
    if !contents.ends_with('\n') {
        return Err(corrupt("incomplete manifest"));
    }
    let mut lines = contents.lines();
    if lines.next() != Some(MANIFEST_MAGIC) {
        return Err(corrupt("invalid manifest version"));
    }
    let mut ids = Vec::<u64>::new();
    let mut seen = HashSet::new();
    for line in lines {
        if line.len() != 20 || !line.bytes().all(|c| c.is_ascii_digit()) {
            return Err(corrupt("invalid segment identifier"));
        }
        let id: u64 = line
            .parse()
            .map_err(|_| corrupt("invalid segment identifier"))?;
        if !seen.insert(id) {
            return Err(corrupt("duplicate manifest segment"));
        }
        ids.try_reserve(1).map_err(allocation_error)?;
        ids.push(id);
    }
    if ids.is_empty() {
        return Err(corrupt("empty manifest"));
    }
    Ok(ids)
}

fn publish_manifest(directory: &Path, directory_file: &File, ids: &[u64]) -> io::Result<()> {
    let next = directory.join(MANIFEST_NEXT);
    match fs::remove_file(&next) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&next)?;
    writeln!(file, "{MANIFEST_MAGIC}")?;
    for id in ids {
        writeln!(file, "{id:020}")?;
    }
    file.sync_all()?;
    fs::rename(next, directory.join(MANIFEST))?;
    directory_file.sync_all()?;
    Ok(())
}

fn parse_segment_id(name: &str) -> Option<u64> {
    let number = name.strip_prefix("segment-")?.strip_suffix(".log")?;
    if number.len() != 20 || !number.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

fn next_unused_id(directory: &Path, used: &[u64]) -> io::Result<u64> {
    let mut maximum = used.iter().copied().max();
    for entry in fs::read_dir(directory)? {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(parse_segment_id) {
            maximum = Some(maximum.map_or(id, |previous| previous.max(id)));
        }
    }
    maximum
        .ok_or_else(|| corrupt("no segments"))?
        .checked_add(1)
        .ok_or_else(too_large)
}

fn clean_unlisted_segments(directory: &Path, ids: &[u64]) {
    let listed: HashSet<u64> = ids.iter().copied().collect();
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(id) = name.to_str().and_then(parse_segment_id) {
                if !listed.contains(&id) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

fn clean_abandoned_temp(directory: &Path) {
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(".compact-") && name.ends_with(".tmp") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}
