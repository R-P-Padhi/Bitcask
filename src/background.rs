//! Offline preparation only. The owner publishes the finished files atomically.
use crate::codec::{encode, read_live_at};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct Candidate {
    pub key: Vec<u8>,
    pub old_segment: u64,
    pub old_offset: u64,
    pub end: u64,
}

pub(crate) struct TempSegment {
    pub path: PathBuf,
    pub len: u64,
}

pub(crate) struct Rewritten {
    pub key: Vec<u8>,
    pub old_segment: u64,
    pub old_offset: u64,
    pub output: usize,
    pub offset: u64,
    pub size: u64,
}

pub(crate) struct Prepared {
    pub outputs: Vec<TempSegment>,
    pub records: Vec<Rewritten>,
}

pub(crate) fn spawn(
    directory: PathBuf,
    entries: Vec<Candidate>,
    segment_limit: u64,
) -> JoinHandle<io::Result<Prepared>> {
    thread::spawn(move || prepare(directory, entries, segment_limit))
}

fn prepare(directory: PathBuf, entries: Vec<Candidate>, limit: u64) -> io::Result<Prepared> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let tag = format!(
        "{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut source: Option<(u64, File)> = None;
    let mut output: Option<(File, TempSegment)> = None;
    let mut result = Prepared {
        outputs: Vec::new(),
        records: Vec::new(),
    };
    for entry in entries {
        if source
            .as_ref()
            .is_none_or(|(id, _)| *id != entry.old_segment)
        {
            source = Some((
                entry.old_segment,
                File::open(directory.join(format!("segment-{:020}.log", entry.old_segment)))?,
            ));
        }
        let record = read_live_at(
            &source
                .as_ref()
                .ok_or_else(|| io::Error::other("missing source"))?
                .1,
            entry.end,
            entry.old_offset,
            &entry.key,
        )?;
        let bytes = encode(record.key(), record.value(), false)?;
        if output.as_ref().is_some_and(|(_, segment)| {
            segment.len > 0 && segment.len.saturating_add(bytes.len() as u64) > limit
        }) {
            let (file, segment) = output
                .take()
                .ok_or_else(|| io::Error::other("missing output"))?;
            file.sync_all()?;
            result.outputs.push(segment);
        }
        if output.is_none() {
            let path = directory.join(format!(".compact-{tag}-{:06}.tmp", result.outputs.len()));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            output = Some((file, TempSegment { path, len: 0 }));
        }
        let (file, segment) = output
            .as_mut()
            .ok_or_else(|| io::Error::other("missing output"))?;
        let offset = segment.len;
        file.write_all(&bytes)?;
        segment.len += bytes.len() as u64;
        result.records.push(Rewritten {
            key: entry.key,
            old_segment: entry.old_segment,
            old_offset: entry.old_offset,
            output: result.outputs.len(),
            offset,
            size: bytes.len() as u64,
        });
    }
    if let Some((file, segment)) = output {
        file.sync_all()?;
        result.outputs.push(segment);
    }
    Ok(result)
}
