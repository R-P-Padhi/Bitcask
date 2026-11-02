# Disk format — BCKM2 manifest, BCK1 records

Each database is a directory. `MANIFEST` lists the numbered log segments in replay order, with the final segment being writable. Record bytes retain the BCK1 format from v0.1.

## Files

| File | Purpose |
| --- | --- |
| `LOCK` | Stable exclusive lock inode; never delete while the database is open |
| `MANIFEST` | Authoritative ordered list of live segment IDs |
| `MANIFEST.next` | Temporary manifest during atomic publication |
| `segment-00000000000000000000.log`, etc. | Append-only record segments |
| `.compact-*.tmp` | Unpublished background-compaction output; removed after interruption |

The manifest is UTF-8 text: `BCKM2` followed by one zero-padded, 20-digit unsigned segment ID per line. Every line ends in `\n`. IDs must be unique. Their **listed order determines replay order**; IDs may be numerically out of order after background compaction. For example:

```text
BCKM2
00000000000000000007
00000000000000000004
```

Only segments listed in `MANIFEST` are replayed. Files left behind by an interrupted compaction are ignored. A new manifest is written to `MANIFEST.next`, synchronized, renamed over `MANIFEST`, and followed by directory sync. Older segments are removed only after that publication succeeds.

## Record layout (unchanged from BCK1)

Records are concatenated inside each segment, with no per-segment header. Integers are unsigned little-endian.

| Offset | Bytes | Meaning |
| --- | --- | --- |
| 0 | 4 | ASCII `BCK1` |
| 4 | 4 | Key length |
| 8 | 4 | Value length |
| 12 | 4 | Kind: 0 = put; 1 = delete |
| 16 | 4 | CRC-32 of key + value |
| 20 | 4 | CRC-32 of header bytes 0–19 |
| 24 | Key length | Key |
| following | Value length | Value |

**CRC:** IEEE CRC-32; reflected polynomial `0xEDB88320`; initial/final XOR `0xFFFFFFFF`; `123456789` → `0xCBF43926`. This detects accidental corruption; it is not authentication.

**Put:** nonempty key, possibly empty value. **Delete:** nonempty key, zero-length value. The latest complete record across the listed segments wins. Length limits are checked before allocating the body, and the header checksum is validated before trusting its lengths.

**Recovery:** an incomplete tail in the final (active) segment is truncated to the previous complete record. A full invalid header or payload is an error; incomplete records in sealed segments are also errors. Truncated previously committed bytes cannot be distinguished from an interrupted append.

**Rotation:** before a write that would exceed the configured segment size, the current segment is synchronized and a fresh segment is created and published in the manifest. A single record larger than that limit may occupy an otherwise empty segment.

**Compaction:** a blocking full rewrite copies every live record. Background compaction copies a snapshot of sealed segments while the active log accepts new writes. Replacement segments are published *before* later segments in replay order, so overwrites and tombstones written during preparation still win. The manifest changes only after outputs are synced. On reopen, either the old or new manifest governs the complete set; incomplete replacement files are never included in the active set.

**Legacy format:** when no manifest exists but `data.log` does, the engine renames it to segment 0 and publishes an initial manifest. BCK1 records are unchanged. This format is not interoperable with unrelated Bitcask implementations.
