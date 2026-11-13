//! The write-ahead log: every commit is appended to the current segment and
//! synced to disk (`fsync`) before it is applied to the page store, so a
//! commit that returned survives a crash. Records carry a CRC-32: a record
//! torn by a crash is detected and dropped with everything after it.
//!
//! Segments are files named after their first log sequence number
//! (`wal/00000000000000000042.log`). A checkpoint makes the page store durable
//! and starts a new segment; segments older than the checkpoint are removed
//! once the backup, if any, has shipped them.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::storage_err;
use memoire_orm::Result;
use memoire_orm::evaluator::crc32;

/// One change to the page store.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Op {
    /// Writes `value` at `key` in the store table `table`.
    Put { table: String, key: Vec<u8>, value: Vec<u8> },
    /// Removes `key`.
    Del { table: String, key: Vec<u8> },
    /// Removes every key in `start..end`.
    DelRange { table: String, start: Vec<u8>, end: Vec<u8> },
    /// Removes a whole store table (a deleted branch).
    DropTable { table: String },
}

/// One commit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Record {
    pub lsn: u64,
    pub ops: Vec<Op>,
}

/// Little-endian encoder of log records.
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

struct Dec<'a>(&'a [u8]);

impl Dec<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            return Err(storage_err("truncated log record"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap_or_default()))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap_or_default()))
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| storage_err("invalid UTF-8 in the log"))
    }
}

fn io(e: std::io::Error) -> memoire_orm::Error {
    storage_err(format!("write-ahead log: {e}"))
}

impl Record {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        e.i64(self.lsn as i64);
        e.u32(self.ops.len() as u32);
        for op in &self.ops {
            match op {
                Op::Put { table, key, value } => {
                    e.u8(1);
                    e.str(table);
                    e.bytes(key);
                    e.bytes(value);
                }
                Op::Del { table, key } => {
                    e.u8(2);
                    e.str(table);
                    e.bytes(key);
                }
                Op::DelRange { table, start, end } => {
                    e.u8(3);
                    e.str(table);
                    e.bytes(start);
                    e.bytes(end);
                }
                Op::DropTable { table } => {
                    e.u8(4);
                    e.str(table);
                }
            }
        }
        let payload = e.0;
        let mut out = Vec::with_capacity(payload.len() + 8);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32(&payload).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn decode(payload: &[u8]) -> Result<Record> {
        let mut d = Dec(payload);
        let lsn = d.i64()? as u64;
        let n = d.u32()? as usize;
        let mut ops = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            ops.push(match d.u8()? {
                1 => Op::Put { table: d.str()?, key: d.bytes()?, value: d.bytes()? },
                2 => Op::Del { table: d.str()?, key: d.bytes()? },
                3 => Op::DelRange { table: d.str()?, start: d.bytes()?, end: d.bytes()? },
                4 => Op::DropTable { table: d.str()? },
                t => return Err(storage_err(format!("unknown log operation {t}"))),
            });
        }
        Ok(Record { lsn, ops })
    }
}

/// Reads the records of a segment. A torn or damaged tail ends the segment:
/// returns the records before it and the length of the valid part.
pub(crate) fn read_segment(bytes: &[u8]) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or_default()) as usize;
        let crc = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap_or_default());
        let Some(payload) = bytes.get(at + 8..at + 8 + len) else { break };
        if crc32(payload) != crc {
            break;
        }
        match Record::decode(payload) {
            Ok(r) => records.push(r),
            Err(_) => break,
        }
        at += 8 + len;
    }
    (records, at)
}

pub(crate) fn segment_name(first_lsn: u64) -> String {
    format!("{first_lsn:020}.log")
}

/// The first LSN of a segment file name.
pub(crate) fn segment_lsn(name: &str) -> Option<u64> {
    name.strip_suffix(".log")?.parse().ok()
}

/// Segment files of `dir`, oldest first.
pub(crate) fn segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        if let Some(lsn) = entry.file_name().to_str().and_then(segment_lsn) {
            out.push((lsn, entry.path()));
        }
    }
    out.sort_by_key(|(lsn, _)| *lsn);
    Ok(out)
}

/// The segment being written.
pub(crate) struct Writer {
    dir: PathBuf,
    file: File,
    pub first_lsn: u64,
    pub bytes: u64,
    /// `fsync` after each record (off only in tests of the crash recovery).
    sync: bool,
}

impl Writer {
    /// Opens (or creates) the segment starting at `first_lsn`.
    pub(crate) fn open(dir: &Path, first_lsn: u64, sync: bool) -> Result<Writer> {
        fs::create_dir_all(dir).map_err(io)?;
        let path = dir.join(segment_name(first_lsn));
        let file = OpenOptions::new().create(true).append(true).open(&path).map_err(io)?;
        let bytes = file.metadata().map_err(io)?.len();
        sync_dir(dir);
        Ok(Writer { dir: dir.to_owned(), file, first_lsn, bytes, sync })
    }

    pub(crate) fn append(&mut self, record: &Record) -> Result<()> {
        let bytes = record.encode();
        self.file.write_all(&bytes).map_err(io)?;
        if self.sync {
            self.file.sync_data().map_err(io)?;
        }
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    /// Closes this segment and starts the next one at `next_lsn`.
    pub(crate) fn rotate(&mut self, next_lsn: u64) -> Result<()> {
        self.file.sync_all().map_err(io)?;
        *self = Writer::open(&self.dir, next_lsn, self.sync)?;
        Ok(())
    }
}

/// Reads every segment of `dir`, cutting a torn tail off the last one so new
/// records follow valid ones.
pub(crate) fn recover(dir: &Path) -> Result<Vec<Record>> {
    let mut all = Vec::new();
    for (_, path) in segments(dir)? {
        let mut bytes = Vec::new();
        File::open(&path).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(io)?;
        let (records, valid) = read_segment(&bytes);
        if valid < bytes.len() {
            let f = OpenOptions::new().write(true).open(&path).map_err(io)?;
            f.set_len(valid as u64).map_err(io)?;
            f.sync_all().map_err(io)?;
        }
        all.extend(records);
    }
    Ok(all)
}

/// Makes a file creation or removal in `dir` durable (Unix; a no-op elsewhere).
pub(crate) fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torn_records_are_dropped() {
        let a = Record { lsn: 1, ops: vec![Op::Put { table: "b:main".into(), key: vec![1], value: vec![2] }] };
        let b = Record {
            lsn: 2,
            ops: vec![
                Op::DelRange { table: "x".into(), start: vec![0], end: vec![9] },
                Op::DropTable { table: "y".into() },
            ],
        };
        let mut bytes = a.encode();
        bytes.extend(b.encode());
        let full = bytes.len();
        assert_eq!(read_segment(&bytes), (vec![a.clone(), b.clone()], full));
        bytes.truncate(full - 3);
        assert_eq!(read_segment(&bytes).0, vec![a.clone()]);
        let mut flipped = a.encode();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(read_segment(&flipped).0.is_empty());
    }
}
