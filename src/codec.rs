use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::FileExt;

const HEADER: usize = 24;
const MAGIC: &[u8; 4] = b"BCK1";

/// Maximum key length in bytes; keys must not be empty.
pub const MAX_KEY: usize = 64 * 1024;
/// Maximum value length in bytes; empty values are allowed.
pub const MAX_VALUE: usize = 16 * 1024 * 1024;

pub(crate) struct Record {
    body: Vec<u8>,
    key_len: usize,
    pub(crate) deleted: bool,
}

impl Record {
    pub(crate) fn key(&self) -> &[u8] {
        &self.body[..self.key_len]
    }
    pub(crate) fn value(&self) -> &[u8] {
        &self.body[self.key_len..]
    }
    pub(crate) fn size(&self) -> u64 {
        (HEADER + self.body.len()) as u64
    }
}

struct OffsetReader<'a> {
    file: &'a File,
    offset: u64,
}

impl Read for OffsetReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read_at(buf, self.offset)?;
        self.offset = self
            .offset
            .checked_add(count as u64)
            .ok_or_else(too_large)?;
        Ok(count)
    }
}

pub(crate) fn read_live_at(file: &File, end: u64, offset: u64, key: &[u8]) -> io::Result<Record> {
    let remaining = end
        .checked_sub(offset)
        .ok_or_else(|| corrupt("invalid offset"))?;
    let mut reader = OffsetReader { file, offset };
    let record =
        read_record(&mut reader, remaining)?.ok_or_else(|| corrupt("incomplete live record"))?;
    if record.deleted || record.key() != key {
        return Err(corrupt("index does not match live record"));
    }
    Ok(record)
}

/// Read only the value into an owned buffer; validate the key in small stack chunks.
pub(crate) fn read_value_at(file: &File, end: u64, offset: u64, key: &[u8]) -> io::Result<Vec<u8>> {
    let remaining = end
        .checked_sub(offset)
        .ok_or_else(|| corrupt("invalid offset"))?;
    if remaining < HEADER as u64 {
        return Err(corrupt("incomplete live record"));
    }
    let mut reader = OffsetReader { file, offset };
    let mut header = [0_u8; HEADER];
    reader.read_exact(&mut header)?;
    if &header[..4] != MAGIC || crc32(&header[..20]) != word(&header, 20) {
        return Err(corrupt("invalid record header or header checksum"));
    }
    let key_len = word(&header, 4) as usize;
    let value_len = word(&header, 8) as usize;
    if word(&header, 12) != 0 || key_len != key.len() {
        return Err(corrupt("index does not match live record"));
    }
    validate_lengths(key_len, value_len, false).map_err(|_| corrupt("invalid record lengths"))?;
    if HEADER as u64 + (key_len + value_len) as u64 > remaining {
        return Err(corrupt("incomplete live record"));
    }
    let mut chunk = [0_u8; 4096];
    for expected in key.chunks(chunk.len()) {
        reader.read_exact(&mut chunk[..expected.len()])?;
        if &chunk[..expected.len()] != expected {
            return Err(corrupt("index does not match live record"));
        }
    }
    let mut value = allocate(value_len)?;
    value.resize(value_len, 0);
    reader.read_exact(&mut value)?;
    if crc32_parts(key, &value) != word(&header, 16) {
        return Err(corrupt("record payload checksum mismatch"));
    }
    Ok(value)
}

// Only an incomplete tail is recoverable; a complete bad record is corruption.
pub(crate) fn read_record(reader: &mut impl Read, remaining: u64) -> io::Result<Option<Record>> {
    if remaining < HEADER as u64 {
        return Ok(None);
    }
    let mut header = [0; HEADER];
    reader.read_exact(&mut header)?;
    if &header[..4] != MAGIC || crc32(&header[..20]) != word(&header, 20) {
        return Err(corrupt("invalid record header or header checksum"));
    }
    let key_len = word(&header, 4) as usize;
    let value_len = word(&header, 8) as usize;
    let kind = word(&header, 12);
    if kind > 1 {
        return Err(corrupt("unknown record kind"));
    }
    validate_lengths(key_len, value_len, kind == 1)
        .map_err(|_| corrupt("invalid record lengths"))?;
    let length = key_len + value_len;
    if HEADER as u64 + length as u64 > remaining {
        return Ok(None);
    }
    let mut body = allocate(length)?;
    body.resize(length, 0);
    reader.read_exact(&mut body)?;
    if crc32(&body) != word(&header, 16) {
        return Err(corrupt("record payload checksum mismatch"));
    }
    Ok(Some(Record {
        body,
        key_len,
        deleted: kind == 1,
    }))
}

pub(crate) fn encode(key: &[u8], value: &[u8], deleted: bool) -> io::Result<Vec<u8>> {
    validate_lengths(key.len(), value.len(), deleted)?;
    let mut bytes = allocate(HEADER + key.len() + value.len())?;
    bytes.resize(HEADER, 0);
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(value);
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4..8].copy_from_slice(&(key.len() as u32).to_le_bytes());
    bytes[8..12].copy_from_slice(&(value.len() as u32).to_le_bytes());
    bytes[12..16].copy_from_slice(&u32::from(deleted).to_le_bytes());
    let payload_crc = crc32(&bytes[HEADER..]);
    bytes[16..20].copy_from_slice(&payload_crc.to_le_bytes());
    let header_crc = crc32(&bytes[..20]);
    bytes[20..24].copy_from_slice(&header_crc.to_le_bytes());
    Ok(bytes)
}

pub(crate) fn validate_lengths(key: usize, value: usize, deleted: bool) -> io::Result<()> {
    if key == 0 || key > MAX_KEY || value > MAX_VALUE || (deleted && value != 0) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "invalid key or value length",
        ));
    }
    Ok(())
}

fn allocate(length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(allocation_error)?;
    Ok(bytes)
}

pub(crate) fn copy_bytes(source: &[u8]) -> io::Result<Vec<u8>> {
    let mut bytes = allocate(source.len())?;
    bytes.extend_from_slice(source);
    Ok(bytes)
}

pub(crate) fn allocation_error(_: std::collections::TryReserveError) -> io::Error {
    io::Error::other("memory allocation failed")
}
pub(crate) fn corrupt(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}
pub(crate) fn too_large() -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, "log exceeds maximum offset")
}

fn word(header: &[u8; HEADER], at: usize) -> u32 {
    u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    !bytes.iter().fold(!0_u32, |crc, &byte| {
        TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

fn crc32_parts(key: &[u8], value: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    !key.iter().chain(value).fold(!0_u32, |crc, &byte| {
        TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}
