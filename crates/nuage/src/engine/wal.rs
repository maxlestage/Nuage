//! The write-ahead log: every commit is appended to the current segment, and
//! the segment is synced to disk (`fsync`) before the commit returns to its
//! caller or is shown to readers, so a commit that returned survives a crash.
//! Commits made at the same time share one sync. Records carry a CRC-32: a
//! record torn by a crash is detected and dropped with everything after it.
//!
//! Segments are files named after their first log sequence number
//! (`wal/00000000000000000042.log`). Encrypted, a segment starts with a
//! header (`NUAGEWAL`, a version byte and a random salt) and each record is
//! sealed with AES-256-GCM under the segment's key (see [`crate::crypto`]). A checkpoint makes the page store durable
//! and starts a new segment; segments older than the checkpoint are removed
//! once the backup, if any, has shipped them.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::pages::crc32;
use super::storage_err;
use memoire_orm::Result;

use crate::crypto::{SEGMENT_SALT, SegmentKey, WalKey, random};

const MAGIC: &[u8; 9] = b"NUAGEWAL\x01";
/// Bytes of the header of an encrypted segment.
pub(crate) const HEADER_LEN: usize = MAGIC.len() + SEGMENT_SALT;

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

impl Op {
    /// The store table it changes.
    pub(crate) fn table(&self) -> &str {
        match self {
            Op::Put { table, .. } | Op::Del { table, .. } | Op::DelRange { table, .. } | Op::DropTable { table } => {
                table
            }
        }
    }
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

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
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
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn str(&mut self) -> Result<&'a str> {
        std::str::from_utf8(self.bytes()?).map_err(|_| storage_err("invalid UTF-8 in the log"))
    }
}

fn io(e: std::io::Error) -> memoire_orm::Error {
    storage_err(format!("write-ahead log: {e}"))
}

/// Bytes of the head of a record on disk: length, CRC-32.
const FRAME: usize = 8;

/// `length ‖ CRC-32 ‖ payload`: a CRC over what is on disk tells a torn
/// record from a whole one. `buf` is the payload after [`FRAME`] bytes left
/// for its head.
fn frame(buf: &mut [u8]) {
    let len = (buf.len() - FRAME) as u32;
    let crc = crc32(&buf[FRAME..]);
    buf[..4].copy_from_slice(&len.to_le_bytes());
    buf[4..FRAME].copy_from_slice(&crc.to_le_bytes());
}

impl Record {
    #[cfg(test)]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; FRAME];
        self.payload_into(&mut buf);
        frame(&mut buf);
        buf
    }

    /// Appends the record to `out`.
    fn payload_into(&self, out: &mut Vec<u8>) {
        let mut e = Enc(std::mem::take(out));
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
        *out = e.0;
    }

    /// The record of `payload`, `None` when its number is `after` or before
    /// (a commit the page store holds already). Such a record is read through
    /// all the same, checked as a kept one is, without copying anything out:
    /// whether a record is whole does not depend on what it is read for.
    fn decode(payload: &[u8], after: u64) -> Result<Option<Record>> {
        let mut d = Dec(payload);
        let lsn = d.i64()? as u64;
        let n = d.u32()? as usize;
        let keep = lsn > after;
        let mut ops = Vec::with_capacity(if keep { n.min(1 << 16) } else { 0 });
        for _ in 0..n {
            let op = d.u8()?;
            if !(1..=4).contains(&op) {
                return Err(storage_err(format!("unknown log operation {op}")));
            }
            let table = d.str()?;
            let (a, b) = match op {
                1 | 3 => (d.bytes()?, d.bytes()?),
                2 => (d.bytes()?, &[][..]),
                _ => (&[][..], &[][..]),
            };
            if keep {
                let table = table.to_owned();
                ops.push(match op {
                    1 => Op::Put { table, key: a.to_vec(), value: b.to_vec() },
                    2 => Op::Del { table, key: a.to_vec() },
                    3 => Op::DelRange { table, start: a.to_vec(), end: b.to_vec() },
                    _ => Op::DropTable { table },
                });
            }
        }
        Ok(keep.then_some(Record { lsn, ops }))
    }
}

/// The key of an encrypted segment, from its header.
fn segment_key(bytes: &[u8], key: &WalKey) -> Option<SegmentKey> {
    let header = bytes.get(..HEADER_LEN)?;
    if &header[..MAGIC.len()] != MAGIC {
        return None;
    }
    Some(key.segment(header[MAGIC.len()..].try_into().ok()?))
}

/// Reads the records of a segment. A torn or damaged tail ends the segment:
/// returns the records before it and the length of the valid part.
/// Encrypted (`key`), a record that does not open under the segment's key
/// ends it too.
#[cfg(test)]
pub(crate) fn read_segment(bytes: &[u8], key: Option<&WalKey>) -> (Vec<Record>, usize) {
    read_segment_after(bytes, key, 0)
}

/// [`read_segment`], keeping only the records numbered after `after`.
fn read_segment_after(bytes: &[u8], key: Option<&WalKey>, after: u64) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let seg = match key {
        Some(k) => match segment_key(bytes, k) {
            Some(seg) => Some(seg),
            None => return (records, 0),
        },
        None => None,
    };
    let mut at = if seg.is_some() { HEADER_LEN } else { 0 };
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or_default()) as usize;
        let crc = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap_or_default());
        let Some(payload) = bytes.get(at + 8..at + 8 + len) else { break };
        if crc32(payload) != crc {
            break;
        }
        let opened;
        let payload = match &seg {
            Some(seg) => match seg.open(payload) {
                Some(p) => {
                    opened = p;
                    opened.as_slice()
                }
                None => break,
            },
            None => payload,
        };
        match Record::decode(payload, after) {
            Ok(r) => records.extend(r),
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

/// The segment being written. Appending does not sync it: whoever needs a
/// record on disk syncs [`file`](Writer::file), and one sync covers every
/// record appended before it (see `Store::wait_durable`).
pub(crate) struct Writer {
    dir: PathBuf,
    file: Arc<File>,
    pub first_lsn: u64,
    /// Bytes of records in the segment (its header not counted).
    pub bytes: u64,
    key: Option<Arc<WalKey>>,
    seg: Option<SegmentKey>,
    /// The file holds bytes past its records (a write that failed part way)
    /// that could not be cut yet: nothing is appended behind them.
    torn: bool,
    /// A record was cut from this encrypted segment: the number it had is
    /// never sealed under this segment's key again (a nonce is used once).
    /// The next record goes to a new segment, which has a key of its own.
    spent: bool,
}

impl Writer {
    /// Opens (or creates) the segment starting at `first_lsn`. Encrypted, the
    /// segment is (re)started with a fresh salt: a segment the writer opens
    /// holds no valid record (they would be past the last LSN), and a new
    /// salt means a new key, so no nonce of a torn record is ever reused.
    pub(crate) fn open(dir: &Path, first_lsn: u64, key: Option<Arc<WalKey>>) -> Result<Writer> {
        fs::create_dir_all(dir).map_err(io)?;
        let path = dir.join(segment_name(first_lsn));
        let (file, bytes, seg) = match &key {
            None => {
                let file = OpenOptions::new().create(true).append(true).open(&path).map_err(io)?;
                let bytes = file.metadata().map_err(io)?.len();
                (file, bytes, None)
            }
            Some(k) => {
                let salt = random::<SEGMENT_SALT>();
                let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(&path).map_err(io)?;
                file.write_all(MAGIC).and_then(|()| file.write_all(&salt)).map_err(io)?;
                file.sync_all().map_err(io)?;
                (file, 0, Some(k.segment(&salt)))
            }
        };
        sync_dir(dir);
        Ok(Writer { dir: dir.to_owned(), file: Arc::new(file), first_lsn, bytes, key, seg, torn: false, spent: false })
    }

    /// The segment's file, to sync it.
    pub(crate) fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    /// Appends `record`. When the write fails, what it wrote of the record is
    /// cut from the file: the next record follows the last whole one.
    pub(crate) fn append(&mut self, record: &Record) -> Result<()> {
        if self.spent {
            return Err(storage_err(
                "write-ahead log: a record was cut from this segment, the next one starts another",
            ));
        }
        if self.torn {
            self.cut_to(self.bytes)?;
        }
        // One buffer: the head, then the record, encrypted where it is.
        let mut bytes = vec![0u8; FRAME];
        match &self.seg {
            Some(seg) => {
                bytes.extend_from_slice(&record.lsn.to_le_bytes());
                record.payload_into(&mut bytes);
                seg.seal_in_place(record.lsn, &mut bytes, FRAME + 8);
            }
            None => record.payload_into(&mut bytes),
        }
        frame(&mut bytes);
        if let Err(e) = (&*self.file).write_all(&bytes) {
            // Recovery stops at the first record that is not whole: one
            // appended behind this one would be lost with it.
            let _ = self.cut_to(self.bytes);
            return Err(io(e));
        }
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    /// Cuts the segment back to its first `bytes` bytes of records: the
    /// records appended since are not in the log any more (their commits did
    /// not complete). Until it succeeds, nothing is appended.
    pub(crate) fn cut_to(&mut self, bytes: u64) -> Result<()> {
        self.bytes = bytes;
        self.torn = true;
        self.spent = self.seg.is_some();
        let len = bytes + if self.seg.is_some() { HEADER_LEN as u64 } else { 0 };
        self.file.set_len(len).map_err(io)?;
        (&*self.file).seek(SeekFrom::Start(len)).map_err(io)?;
        self.torn = false;
        Ok(())
    }

    /// `true` once a record was cut from an encrypted segment: the next
    /// record is for a new segment ([`rotate`](Self::rotate)).
    pub(crate) fn spent(&self) -> bool {
        self.spent
    }

    /// Leaves this segment, which its caller has synced, and starts the next
    /// one at `next_lsn`.
    pub(crate) fn rotate(&mut self, next_lsn: u64) -> Result<()> {
        *self = Writer::open(&self.dir, next_lsn, self.key.clone())?;
        Ok(())
    }
}

/// Reads every segment of `dir`, cutting a torn tail off the last one so new
/// records follow valid ones. Encrypted (`key`), a segment without a valid
/// header is an error, not a tail to cut: it was written in clear, or by
/// another key.
#[cfg(test)]
pub(crate) fn recover(dir: &Path, key: Option<&WalKey>) -> Result<Vec<Record>> {
    recover_after(dir, key, 0)
}

/// [`recover`], returning only the records numbered after `after` (those of
/// the commits the page store does not hold yet): the segments are checked
/// and cut the same way, the records before are not copied out.
pub(crate) fn recover_after(dir: &Path, key: Option<&WalKey>, after: u64) -> Result<Vec<Record>> {
    let mut all = Vec::new();
    for (_, path) in segments(dir)? {
        let mut bytes = Vec::new();
        File::open(&path).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(io)?;
        // A header torn by a crash (a prefix of it) is a tail to cut; anything
        // else that does not open is not this key's segment.
        let n = bytes.len().min(MAGIC.len());
        let torn_header = bytes.len() < HEADER_LEN && bytes[..n] == MAGIC[..n];
        if let Some(k) = key
            && !torn_header
            && segment_key(&bytes, k).is_none()
        {
            return Err(storage_err(format!("{} is not an encrypted log segment", path.display())));
        }
        let (records, valid) = read_segment_after(&bytes, key, after);
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
        assert_eq!(read_segment(&bytes, None), (vec![a.clone(), b.clone()], full));
        bytes.truncate(full - 3);
        assert_eq!(read_segment(&bytes, None).0, vec![a.clone()]);
        let mut flipped = a.encode();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(read_segment(&flipped, None).0.is_empty());
    }

    /// Records the page store holds already (numbered up to `after`) are left
    /// out, and checked all the same: a damaged one still ends the segment.
    #[test]
    fn records_before_the_checkpoint_are_checked_not_kept() {
        let ops = |n: u8| {
            vec![
                Op::Put { table: "b:main".into(), key: vec![n], value: vec![n; 40] },
                Op::Del { table: "b:main".into(), key: vec![n, 1] },
                Op::DelRange { table: "b:x".into(), start: vec![0], end: vec![n] },
                Op::DropTable { table: "b:y".into() },
            ]
        };
        let records: Vec<Record> = (1..=5).map(|lsn| Record { lsn, ops: ops(lsn as u8) }).collect();
        let bytes: Vec<u8> = records.iter().flat_map(Record::encode).collect();
        for after in 0..=6 {
            let (kept, valid) = read_segment_after(&bytes, None, after);
            assert_eq!(kept, records[(after as usize).min(5)..].to_vec(), "after {after}");
            assert_eq!(valid, bytes.len());
        }
        // Record 2 framed and checksummed, but not a record: 3 to 5 are not read either.
        let mut bad = records[0].encode();
        let mut payload = Vec::new();
        payload.extend_from_slice(&2u64.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(9);
        let mut framed = vec![0u8; FRAME];
        framed.extend_from_slice(&payload);
        frame(&mut framed);
        bad.extend_from_slice(&framed);
        let good_len = records[0].encode().len();
        for r in &records[2..] {
            bad.extend(r.encode());
        }
        for after in [0, 2, 4] {
            let (kept, valid) = read_segment_after(&bad, None, after);
            assert_eq!(valid, good_len, "after {after}");
            assert_eq!(kept, if after == 0 { vec![records[0].clone()] } else { vec![] });
        }
    }

    #[test]
    fn encrypted_segments() {
        let dir = std::env::temp_dir().join(format!("nuage-wal-enc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let enc = crate::crypto::Encryption::from_key([5; 32]);
        let (_, keys) = crate::crypto::Header::create(&enc).unwrap();
        let mut w = Writer::open(&dir, 1, Some(keys.wal.clone())).unwrap();
        let a = Record {
            lsn: 1,
            ops: vec![Op::Put { table: "b:main".into(), key: b"secret-key".to_vec(), value: b"secret-value".to_vec() }],
        };
        let b = Record { lsn: 2, ops: vec![Op::Del { table: "b:main".into(), key: b"secret-key".to_vec() }] };
        w.append(&a).unwrap();
        w.append(&b).unwrap();
        drop(w);
        let raw = fs::read(dir.join(segment_name(1))).unwrap();
        assert!(raw.starts_with(MAGIC));
        assert!(!raw.windows(6).any(|x| x == b"secret"), "nothing in clear");
        assert_eq!(read_segment(&raw, Some(&keys.wal)).0, vec![a.clone(), b.clone()]);
        assert_eq!(recover(&dir, Some(&keys.wal)).unwrap(), vec![a, b]);
        // Another key reads nothing; a key on a clear segment is an error.
        let (_, other) = crate::crypto::Header::create(&enc).unwrap();
        assert!(read_segment(&raw, Some(&other.wal)).0.is_empty());
        fs::write(dir.join(segment_name(1)), Record { lsn: 1, ops: vec![] }.encode()).unwrap();
        assert!(recover(&dir, Some(&keys.wal)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A record written in part (the write failed) or taken back (its commit
    /// did not complete) is cut from the segment: the records appended after
    /// it are read back, in clear and encrypted.
    #[test]
    fn a_record_that_did_not_complete_is_cut() {
        let record = |lsn: u64, value: &[u8]| Record {
            lsn,
            ops: vec![Op::Put { table: "b:main".into(), key: lsn.to_be_bytes().to_vec(), value: value.to_vec() }],
        };
        let enc = crate::crypto::Encryption::from_key([5; 32]);
        let (_, keys) = crate::crypto::Header::create(&enc).unwrap();
        for key in [None, Some(keys.wal.clone())] {
            let dir = std::env::temp_dir().join(format!("nuage-wal-cut-{}-{}", key.is_some(), std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            let mut w = Writer::open(&dir, 1, key.clone()).unwrap();
            w.append(&record(1, b"one")).unwrap();
            // What a write that fails half way leaves behind the last record.
            let whole = w.bytes;
            (&*w.file).write_all(&[0xAB; 37]).unwrap();
            w.cut_to(whole).unwrap();
            // Encrypted, the record cut had its number sealed under the segment's key:
            // that number goes to another segment, under another key.
            let next = |w: &mut Writer, lsn: u64| {
                assert_eq!(w.spent(), key.is_some());
                if w.spent() {
                    assert!(w.append(&record(lsn, b"under the same key")).is_err());
                    w.rotate(lsn).unwrap();
                }
            };
            next(&mut w, 2);
            w.append(&record(2, b"two")).unwrap();
            // A record that is whole, taken back: the next one has its number.
            let kept = w.bytes;
            w.append(&record(3, &[7; 5000])).unwrap();
            w.cut_to(kept).unwrap();
            next(&mut w, 3);
            w.append(&record(3, b"three")).unwrap();
            assert!(!w.torn && !w.spent());
            // The first record of a segment, cut: the segment starts again, with a new key.
            w.rotate(4).unwrap();
            w.append(&record(4, b"gone")).unwrap();
            w.cut_to(0).unwrap();
            next(&mut w, 4);
            w.append(&record(4, b"four")).unwrap();
            drop(w);
            let read = recover(&dir, key.as_deref()).unwrap();
            assert_eq!(read, vec![record(1, b"one"), record(2, b"two"), record(3, b"three"), record(4, b"four")]);
            assert_eq!(segments(&dir).unwrap().len(), if key.is_some() { 4 } else { 2 });
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
