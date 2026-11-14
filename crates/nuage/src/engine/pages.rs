//! The page file: Nuage DB's own B-trees on disk.
//!
//! One file (`nuage.db`) holds every store table (one per branch, plus the
//! store's metadata) as a B-tree of 4 KiB pages:
//!
//! * **Nodes.** A leaf holds sorted keys and their values, a branch holds
//!   separators and its children. A node normally fits one page; a key or a
//!   value larger than a page makes a node of several pages in a row. A node
//!   on disk starts with a CRC-32 of its content, checked each time it is
//!   read, and stores the prefix its keys share once.
//! * **Copy-on-write.** A change never touches a node in place: it makes a new
//!   version of the nodes from the leaf to the root, in memory. A reader holds
//!   the root it started with (a [`Version`]) and keeps seeing the data as it
//!   was, without a lock; old versions go when their last reader does.
//! * **Checkpoint.** The write-ahead log makes commits durable, so changed
//!   nodes stay in memory until a checkpoint writes them to free pages,
//!   syncs, then writes the header that points to the new roots, in the slot
//!   the previous header does not use, and syncs again. A crash at any point
//!   leaves the previous header and every page it reaches untouched.
//! * **Free space.** A page is reused once no header and no reader can reach
//!   it. Which pages are free is not stored: opening the file walks its
//!   branches and takes what they do not reach, so a crash can neither leak a
//!   page nor hand one out twice.
//! * **Cache.** Nodes read from disk are kept, decoded, in a cache bounded in
//!   bytes; data beyond it stays on disk.
//! * **Encryption.** With a key, every page (headers included) is encrypted
//!   with XTS-AES-256, one data unit per page (see [`crate::crypto`]).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use memoire_orm::{Error, Result};

use super::storage_err;
use super::wal::Op;
use crate::crypto::Xts;

/// Bytes of a page.
pub(crate) const PAGE: usize = 4096;
/// A node on disk: CRC-32 (4), kind (1), reserved (3), length of the content (4), content.
const HEAD: usize = 12;
/// Content of a node of one page.
const ROOM: usize = PAGE - HEAD;
/// The two header slots; nodes start after them.
const FIRST: u64 = 2;
const MAGIC: &[u8; 8] = b"NUAGEPG\x01";
const KIND_LEAF: u8 = 1;
const KIND_BRANCH: u8 = 2;
const KIND_DIRECTORY: u8 = 3;
const KIND_HEADER: u8 = 4;
/// The longest key and value together.
pub(crate) const MAX_ENTRY: usize = 1 << 30;

fn err(message: impl std::fmt::Display) -> Error {
    storage_err(format!("page store: {message}"))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// CRC-32 (IEEE), eight bytes at a time
// ---------------------------------------------------------------------------

const fn crc_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 8 {
            t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xFF) as usize];
            k += 1;
        }
        i += 1;
    }
    t
}

static CRC: [[u32; 256]; 8] = crc_tables();

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = CRC[7][(lo & 0xFF) as usize]
            ^ CRC[6][((lo >> 8) & 0xFF) as usize]
            ^ CRC[5][((lo >> 16) & 0xFF) as usize]
            ^ CRC[4][(lo >> 24) as usize]
            ^ CRC[3][(hi & 0xFF) as usize]
            ^ CRC[2][((hi >> 8) & 0xFF) as usize]
            ^ CRC[1][((hi >> 16) & 0xFF) as usize]
            ^ CRC[0][(hi >> 24) as usize];
    }
    for &b in rest {
        crc = CRC[0][((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// Variable-length integers
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_len(v: u64) -> usize {
    ((64 - v.leading_zeros()).max(1) as usize).div_ceil(7)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let (a, b) = self.0.split_at_checked(n).ok_or_else(|| err("a node is cut short"))?;
        self.0 = b;
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(err("a number in a node is too long"))
    }

    fn len(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| err("a length in a node is too long"))
    }
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

/// Pages in a row: where a node is on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    start: u64,
    pages: u32,
}

#[derive(Clone, Copy)]
struct Slot {
    off: u32,
    klen: u32,
    vlen: u32,
}

/// Sorted keys with their values, in one buffer: a key and its value follow
/// each other in `data`, `slots` gives them in key order.
#[derive(Default)]
pub(crate) struct Cells {
    slots: Vec<Slot>,
    data: Vec<u8>,
    /// Bytes of the keys and values in use.
    live: usize,
    /// Bytes of their lengths once on disk.
    lens: usize,
}

/// A node is copied to be changed: the copy leaves the unused bytes behind
/// and has room for what comes next, so that writing to it does not move it.
impl Clone for Cells {
    fn clone(&self) -> Cells {
        let mut slots = Vec::with_capacity(self.slots.len() + 8);
        slots.extend_from_slice(&self.slots);
        let mut data = Vec::with_capacity(self.live + self.live / 8 + 256);
        if self.data.len() == self.live {
            data.extend_from_slice(&self.data);
        } else {
            for s in &mut slots {
                let off = data.len() as u32;
                data.extend_from_slice(&self.data[s.off as usize..(s.off + s.klen + s.vlen) as usize]);
                s.off = off;
            }
        }
        Cells { slots, data, live: self.live, lens: self.lens }
    }
}

impl Cells {
    fn len(&self) -> usize {
        self.slots.len()
    }

    fn key(&self, i: usize) -> &[u8] {
        let s = self.slots[i];
        &self.data[s.off as usize..(s.off + s.klen) as usize]
    }

    fn val(&self, i: usize) -> &[u8] {
        let s = self.slots[i];
        let at = (s.off + s.klen) as usize;
        &self.data[at..at + s.vlen as usize]
    }

    fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.slots.binary_search_by(|s| self.data[s.off as usize..(s.off + s.klen) as usize].cmp(key))
    }

    /// The first position whose key is `key` or more.
    fn lower_bound(&self, key: &[u8]) -> usize {
        match self.search(key) {
            Ok(i) | Err(i) => i,
        }
    }

    fn insert(&mut self, i: usize, key: &[u8], val: &[u8]) {
        let off = self.data.len() as u32;
        self.data.extend_from_slice(key);
        self.data.extend_from_slice(val);
        self.slots.insert(i, Slot { off, klen: key.len() as u32, vlen: val.len() as u32 });
        self.live += key.len() + val.len();
        self.lens += varint_len(key.len() as u64) + varint_len(val.len() as u64);
    }

    fn push(&mut self, key: &[u8], val: &[u8]) {
        self.insert(self.slots.len(), key, val);
    }

    fn replace(&mut self, i: usize, val: &[u8]) {
        let s = self.slots[i];
        self.live = self.live - s.vlen as usize + val.len();
        self.lens = self.lens - varint_len(u64::from(s.vlen)) + varint_len(val.len() as u64);
        if val.len() <= s.vlen as usize {
            let at = (s.off + s.klen) as usize;
            self.data[at..at + val.len()].copy_from_slice(val);
            self.slots[i].vlen = val.len() as u32;
        } else {
            let off = self.data.len() as u32;
            self.data.extend_from_within(s.off as usize..(s.off + s.klen) as usize);
            self.data.extend_from_slice(val);
            self.slots[i] = Slot { off, klen: s.klen, vlen: val.len() as u32 };
        }
        self.tidy();
    }

    fn forget(&mut self, s: Slot) {
        self.live -= (s.klen + s.vlen) as usize;
        self.lens -= varint_len(u64::from(s.klen)) + varint_len(u64::from(s.vlen));
    }

    fn remove(&mut self, i: usize) {
        let s = self.slots.remove(i);
        self.forget(s);
        self.tidy();
    }

    fn remove_range(&mut self, lo: usize, hi: usize) {
        let gone: Vec<Slot> = self.slots.drain(lo..hi).collect();
        for s in gone {
            self.forget(s);
        }
        self.tidy();
    }

    /// Drops the bytes no slot uses once they outweigh the rest.
    fn tidy(&mut self) {
        if self.data.len() > 2 * self.live + 1024 {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let mut data = Vec::with_capacity(self.live);
        for s in &mut self.slots {
            let off = data.len() as u32;
            data.extend_from_slice(&self.data[s.off as usize..(s.off + s.klen + s.vlen) as usize]);
            s.off = off;
        }
        self.data = data;
    }

    /// Moves the entries from `at` on into new cells.
    fn split_off(&mut self, at: usize) -> Cells {
        let mut right = Cells::default();
        right.data.reserve(self.live / 2);
        for i in at..self.len() {
            right.push(self.key(i), self.val(i));
        }
        self.slots.truncate(at);
        self.live -= right.live;
        self.lens -= right.lens;
        self.compact();
        right
    }

    fn extend_from(&mut self, other: &Cells) {
        for i in 0..other.len() {
            self.push(other.key(i), other.val(i));
        }
    }

    /// Bytes every key starts with.
    fn prefix_len(&self) -> usize {
        match self.len() {
            0 => 0,
            n => common_prefix(self.key(0), self.key(n - 1)),
        }
    }

    /// Bytes of the entry at `i` on disk, its key whole.
    fn entry_size(&self, i: usize) -> usize {
        let s = self.slots[i];
        (s.klen + s.vlen) as usize + varint_len(u64::from(s.klen)) + varint_len(u64::from(s.vlen))
    }

    /// Bytes on disk (at most: the keys' lengths are counted before the
    /// shared prefix is taken off).
    fn size(&self) -> usize {
        let p = self.prefix_len();
        self.live + self.lens + p - p * self.len()
    }

    fn mem(&self) -> usize {
        self.data.capacity() + self.slots.capacity() * size_of::<Slot>()
    }

    fn encode(&self, out: &mut Vec<u8>, values: bool) {
        let p = self.prefix_len();
        put_varint(out, p as u64);
        if p > 0 {
            out.extend_from_slice(&self.key(0)[..p]);
        }
        for i in 0..self.len() {
            let key = &self.key(i)[p..];
            put_varint(out, key.len() as u64);
            if values {
                put_varint(out, u64::from(self.slots[i].vlen));
            }
            out.extend_from_slice(key);
            if values {
                out.extend_from_slice(self.val(i));
            }
        }
    }

    fn decode(r: &mut Reader<'_>, n: usize, values: bool) -> Result<Cells> {
        let p = r.len()?;
        let prefix = r.bytes(p)?;
        let mut cells = Cells::default();
        cells.slots.reserve(n.min(1 << 16));
        cells.data.reserve(r.0.len().saturating_add(p.saturating_mul(n)).min(1 << 26));
        for _ in 0..n {
            let klen = r.len()?;
            let vlen = if values { r.len()? } else { 0 };
            if p + klen + vlen > MAX_ENTRY {
                return Err(err("an entry in a node is too long"));
            }
            let off = cells.data.len() as u32;
            cells.data.extend_from_slice(prefix);
            cells.data.extend_from_slice(r.bytes(klen)?);
            cells.data.extend_from_slice(r.bytes(vlen)?);
            let klen = p + klen;
            cells.slots.push(Slot { off, klen: klen as u32, vlen: vlen as u32 });
            cells.live += klen + vlen;
            cells.lens += varint_len(klen as u64) + varint_len(vlen as u64);
        }
        Ok(cells)
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// The shortest key that is more than `left` and at most `right` (`left < right`).
fn separator(left: &[u8], right: &[u8]) -> Vec<u8> {
    let n = common_prefix(left, right);
    right[..(n + 1).min(right.len())].to_vec()
}

/// Where a child is: on disk, or in memory when it changed since the last checkpoint.
#[derive(Clone)]
enum Ptr {
    Disk(Extent),
    Mem(Arc<Node>),
}

#[derive(Clone)]
struct Child {
    ptr: Ptr,
    /// Bytes of the keys and values under it.
    bytes: u64,
}

/// Child `i` holds the keys from separator `i - 1` (included) to separator `i`.
#[derive(Clone)]
pub(crate) struct Branch {
    /// 1 when the children are leaves.
    level: u8,
    keys: Cells,
    children: Vec<Child>,
}

impl Branch {
    /// The child that holds `key`.
    fn child_index(&self, key: &[u8]) -> usize {
        match self.keys.search(key) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }

    fn bytes(&self) -> u64 {
        self.children.iter().map(|c| c.bytes).sum()
    }
}

/// Bytes a child takes in its parent on disk, about.
const CHILD_SIZE: usize = 12;

#[derive(Clone)]
pub(crate) enum Node {
    Leaf(Cells),
    Branch(Branch),
}

impl Node {
    fn is_empty(&self) -> bool {
        match self {
            Node::Leaf(c) => c.len() == 0,
            Node::Branch(b) => b.children.is_empty(),
        }
    }

    fn level(&self) -> u8 {
        match self {
            Node::Leaf(_) => 0,
            Node::Branch(b) => b.level,
        }
    }

    /// Bytes of its content on disk, about.
    fn size(&self) -> usize {
        match self {
            Node::Leaf(c) => 8 + c.size(),
            Node::Branch(b) => 9 + b.keys.size() + CHILD_SIZE * b.children.len(),
        }
    }

    /// Bytes it holds in memory, about.
    fn mem(&self) -> usize {
        64 + match self {
            Node::Leaf(c) => c.mem(),
            Node::Branch(b) => b.keys.mem() + b.children.capacity() * size_of::<Child>(),
        }
    }

    /// The node as it is written: whole pages, checksum first.
    fn encode(&self) -> Result<Vec<u8>> {
        let mut out = vec![0u8; HEAD];
        match self {
            Node::Leaf(c) => {
                out.reserve(c.size() + 8);
                put_varint(&mut out, c.len() as u64);
                c.encode(&mut out, true);
                Ok(seal(out, KIND_LEAF))
            }
            Node::Branch(b) => {
                out.reserve(self.size() + 8);
                out.push(b.level);
                put_varint(&mut out, b.children.len() as u64);
                b.keys.encode(&mut out, false);
                for child in &b.children {
                    let Ptr::Disk(e) = child.ptr else {
                        return Err(err("a node is written before its children"));
                    };
                    put_varint(&mut out, e.start);
                    put_varint(&mut out, u64::from(e.pages));
                    put_varint(&mut out, child.bytes);
                }
                Ok(seal(out, KIND_BRANCH))
            }
        }
    }

    fn decode(kind: u8, content: &[u8]) -> Result<Node> {
        let mut r = Reader(content);
        match kind {
            KIND_LEAF => {
                let n = r.len()?;
                Ok(Node::Leaf(Cells::decode(&mut r, n, true)?))
            }
            KIND_BRANCH => {
                let level = r.u8()?;
                let n = r.len()?;
                if level == 0 || n == 0 {
                    return Err(err("a branch without children"));
                }
                let keys = Cells::decode(&mut r, n - 1, false)?;
                let mut children = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    let start = r.varint()?;
                    let pages = u32::try_from(r.varint()?).map_err(|_| err("a node is too long"))?;
                    if start < FIRST || pages == 0 {
                        return Err(err("a branch points outside the nodes"));
                    }
                    children.push(Child { ptr: Ptr::Disk(Extent { start, pages }), bytes: r.varint()? });
                }
                Ok(Node::Branch(Branch { level, keys, children }))
            }
            other => Err(err(format!("unknown kind of node {other}"))),
        }
    }
}

/// Finishes a node whose content follows `HEAD` empty bytes: kind, length,
/// checksum, then zeros up to whole pages.
fn seal(mut buf: Vec<u8>, kind: u8) -> Vec<u8> {
    let len = (buf.len() - HEAD) as u32;
    buf[4] = kind;
    buf[8..HEAD].copy_from_slice(&len.to_le_bytes());
    let crc = crc32(&buf[4..]);
    buf[..4].copy_from_slice(&crc.to_le_bytes());
    buf.resize(buf.len().div_ceil(PAGE) * PAGE, 0);
    buf
}

/// The kind and the content of a node read from disk, its checksum verified.
fn unseal(buf: &[u8]) -> Option<(u8, &[u8])> {
    let len = u32::from_le_bytes(buf.get(8..HEAD)?.try_into().ok()?) as usize;
    let content = buf.get(HEAD..HEAD.checked_add(len)?)?;
    let crc = u32::from_le_bytes(buf[..4].try_into().ok()?);
    (crc32(&buf[4..HEAD + len]) == crc).then_some((buf[4], content))
}

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset)? {
            0 => return Err(std::io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

/// Elsewhere: one read or write at a time, through the file's position.
#[cfg(not(any(unix, windows)))]
static POSITION: Mutex<()> = Mutex::new(());

#[cfg(not(any(unix, windows)))]
fn read_at(mut file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let _one = lock(&POSITION);
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

#[cfg(not(any(unix, windows)))]
fn write_at(mut file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let _one = lock(&POSITION);
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(buf)
}

/// The file and its key.
struct Disk {
    file: File,
    xts: Option<Arc<Xts>>,
    path: PathBuf,
}

impl Disk {
    /// Opens (or creates) the file and takes it for this handle alone.
    fn open(path: &Path, xts: Option<Arc<Xts>>) -> Result<Disk> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| err(format!("cannot open {}: {e}", path.display())))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(err(format!(
                    "{} is already open (in this process or another): share one handle",
                    path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(err(format!("cannot lock {}: {e}", path.display())));
            }
        }
        Ok(Disk { file, xts, path: path.to_owned() })
    }

    fn len(&self) -> Result<u64> {
        self.file.metadata().map(|m| m.len()).map_err(|e| err(format!("{}: {e}", self.path.display())))
    }

    /// The pages of `ext`, decrypted, as they are on disk.
    fn read_raw(&self, ext: Extent) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; ext.pages as usize * PAGE];
        let offset = ext.start * PAGE as u64;
        read_at(&self.file, &mut buf, offset)
            .map_err(|e| err(format!("cannot read page {} of {}: {e}", ext.start, self.path.display())))?;
        if let Some(xts) = &self.xts {
            xts.decrypt(offset, &mut buf);
        }
        Ok(buf)
    }

    /// The pages of `ext`, their checksum verified.
    fn read(&self, ext: Extent) -> Result<Vec<u8>> {
        let buf = self.read_raw(ext)?;
        if unseal(&buf).is_none() {
            return Err(self.damaged(ext.start));
        }
        Ok(buf)
    }

    fn damaged(&self, page: u64) -> Error {
        err(format!("page {page} of {} is damaged (its checksum does not match)", self.path.display()))
    }

    fn read_node(&self, ext: Extent) -> Result<Node> {
        let buf = self.read(ext)?;
        let (kind, content) = unseal(&buf).ok_or_else(|| self.damaged(ext.start))?;
        Node::decode(kind, content)
    }

    /// Writes whole pages at page `start`.
    fn write(&self, start: u64, mut buf: Vec<u8>) -> Result<()> {
        let offset = start * PAGE as u64;
        if let Some(xts) = &self.xts {
            xts.encrypt(offset, &mut buf);
        }
        write_at(&self.file, &buf, offset)
            .map_err(|e| err(format!("cannot write page {start} of {}: {e}", self.path.display())))
    }

    fn sync(&self) -> Result<()> {
        self.file.sync_data().map_err(|e| err(format!("cannot sync {}: {e}", self.path.display())))
    }

    fn write_header(&self, h: &Header) -> Result<()> {
        let mut out = vec![0u8; HEAD];
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&h.counter.to_le_bytes());
        out.extend_from_slice(&h.end.to_le_bytes());
        out.extend_from_slice(&h.directory.start.to_le_bytes());
        out.extend_from_slice(&h.directory.pages.to_le_bytes());
        self.write(h.counter % 2, seal(out, KIND_HEADER))
    }

    /// The latest header, `None` for a file nothing was written to.
    fn read_header(&self) -> Result<Option<Header>> {
        let len = self.len()?;
        let mut best: Option<Header> = None;
        let mut blank = true;
        for slot in 0..FIRST {
            if len < (slot + 1) * PAGE as u64 {
                continue;
            }
            let mut raw = vec![0u8; PAGE];
            read_at(&self.file, &mut raw, slot * PAGE as u64)
                .map_err(|e| err(format!("cannot read {}: {e}", self.path.display())))?;
            if raw.iter().all(|b| *b == 0) {
                continue;
            }
            blank = false;
            if let Some(xts) = &self.xts {
                xts.decrypt(slot * PAGE as u64, &mut raw);
            }
            let Some((KIND_HEADER, content)) = unseal(&raw) else { continue };
            let Some(h) = Header::decode(content) else { continue };
            if h.counter % 2 == slot && best.as_ref().is_none_or(|b| h.counter > b.counter) {
                best = Some(h);
            }
        }
        match best {
            Some(h) => Ok(Some(h)),
            None if blank => Ok(None),
            None => {
                Err(err(format!("cannot open {}: is it damaged, or encrypted with another key?", self.path.display())))
            }
        }
    }
}

/// What a checkpoint leaves at the head of the file: slot `counter % 2`.
struct Header {
    counter: u64,
    /// Pages of the file in use, free ones among them.
    end: u64,
    /// The list of the trees.
    directory: Extent,
}

impl Header {
    fn decode(content: &[u8]) -> Option<Header> {
        let rest = content.strip_prefix(MAGIC.as_slice())?;
        let u64_at = |at: usize| Some(u64::from_le_bytes(rest.get(at..at + 8)?.try_into().ok()?));
        let pages = u32::from_le_bytes(rest.get(24..28)?.try_into().ok()?);
        Some(Header { counter: u64_at(0)?, end: u64_at(8)?, directory: Extent { start: u64_at(16)?, pages } })
    }
}

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

/// One store table: a B-tree.
#[derive(Clone)]
pub(crate) struct Tree {
    root: Arc<Node>,
    /// Where the root is on disk, when it did not change since.
    disk: Option<Extent>,
    /// Bytes of its keys and values.
    bytes: u64,
}

impl Tree {
    fn empty() -> Tree {
        Tree { root: Arc::new(Node::Leaf(Cells::default())), disk: None, bytes: 0 }
    }
}

type Trees = HashMap<Arc<str>, Tree>;

/// The versions readers hold, by sequence number.
#[derive(Default)]
struct Live(Mutex<BTreeMap<u64, usize>>);

impl Live {
    fn oldest(&self) -> u64 {
        lock(&self.0).keys().next().copied().unwrap_or(u64::MAX)
    }

    fn count(&self) -> usize {
        lock(&self.0).values().sum()
    }
}

/// The trees as of one commit: what a reader sees, however long it reads.
pub(crate) struct Version {
    seq: u64,
    /// What the caller numbered the commit (its place in the log).
    lsn: u64,
    trees: Trees,
    live: Arc<Live>,
}

impl Version {
    fn new(seq: u64, lsn: u64, trees: Trees, live: &Arc<Live>) -> Arc<Version> {
        *lock(&live.0).entry(seq).or_insert(0) += 1;
        Arc::new(Version { seq, lsn, trees, live: live.clone() })
    }

    /// The number given to the last commit it holds (see [`Pages::apply`]).
    pub(crate) fn lsn(&self) -> u64 {
        self.lsn
    }

    /// The root of a store table, `None` when there is no such table.
    pub(crate) fn root(&self, tree: &str) -> Option<Arc<Node>> {
        self.trees.get(tree).map(|t| t.root.clone())
    }

    /// Bytes of the keys and values of a store table.
    pub(crate) fn bytes(&self, tree: &str) -> u64 {
        self.trees.get(tree).map_or(0, |t| t.bytes)
    }

    /// The names of the store tables, sorted.
    #[cfg(feature = "import-redb")]
    pub(crate) fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.trees.keys().map(|n| n.to_string()).collect();
        names.sort();
        names
    }
}

impl Drop for Version {
    fn drop(&mut self) {
        let mut live = lock(&self.live.0);
        if let Some(n) = live.get_mut(&self.seq) {
            *n -= 1;
            if *n == 0 {
                live.remove(&self.seq);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// Page numbers are already spread: one multiplication is hash enough.
#[derive(Default)]
struct PageHasher(u64);

impl Hasher for PageHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.write_u64(u64::from(*b) ^ self.0);
        }
    }

    fn write_u64(&mut self, v: u64) {
        let h = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

/// Places of the clock's ring that may stand for no page held before it is
/// made anew (beyond as many as the pages held).
const RING_SLACK: usize = 64;

struct Cached {
    node: Arc<Node>,
    bytes: usize,
    /// Read since the clock hand last passed.
    hot: bool,
}

#[derive(Default)]
struct Shard {
    map: HashMap<u64, Cached, BuildHasherDefault<PageHasher>>,
    /// The clock: pages in the order they came in.
    ring: VecDeque<u64>,
    bytes: usize,
}

const SHARDS: usize = 16;

/// Nodes read from disk, decoded, by their first page: bounded in bytes, the
/// ones not read lately go first.
struct Cache {
    shards: Vec<Mutex<Shard>>,
    /// Bytes a shard may hold.
    capacity: usize,
    used: AtomicUsize,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Cache {
    fn new(bytes: usize) -> Cache {
        Cache {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
            capacity: bytes / SHARDS,
            used: AtomicUsize::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn shard(&self, page: u64) -> MutexGuard<'_, Shard> {
        lock(&self.shards[(page.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60) as usize % SHARDS])
    }

    fn get(&self, page: u64) -> Option<Arc<Node>> {
        let mut shard = self.shard(page);
        match shard.map.get_mut(&page) {
            Some(c) => {
                c.hot = true;
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(c.node.clone())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// The node at `page` if it is there, without counting a read.
    fn peek(&self, page: u64) -> Option<Arc<Node>> {
        self.shard(page).map.get(&page).map(|c| c.node.clone())
    }

    fn insert(&self, page: u64, node: Arc<Node>) {
        let bytes = node.mem();
        let mut shard = self.shard(page);
        let mut freed = 0;
        if let Some(old) = shard.map.insert(page, Cached { node, bytes, hot: false }) {
            freed += old.bytes;
        } else {
            shard.ring.push_back(page);
            // A page taken out (rewritten at a checkpoint) keeps its place in
            // the ring, and takes another when it comes back: once such
            // places outnumber the pages held, the ring is made anew.
            if shard.ring.len() > 2 * shard.map.len() + RING_SLACK {
                let Shard { map, ring, .. } = &mut *shard;
                let mut seen = HashSet::with_capacity(map.len());
                ring.retain(|p| map.contains_key(p) && seen.insert(*p));
            }
        }
        shard.bytes += bytes;
        shard.bytes -= freed;
        // The clock: a page read since the hand last passed gets another turn.
        while shard.bytes > self.capacity {
            let Some(victim) = shard.ring.pop_front() else { break };
            match shard.map.get_mut(&victim) {
                Some(c) if c.hot && victim != page => {
                    c.hot = false;
                    shard.ring.push_back(victim);
                }
                Some(_) => {
                    if let Some(c) = shard.map.remove(&victim) {
                        shard.bytes -= c.bytes;
                        freed += c.bytes;
                    }
                }
                None => {}
            }
        }
        self.used.fetch_add(bytes, Ordering::Relaxed);
        self.used.fetch_sub(freed, Ordering::Relaxed);
    }

    fn remove(&self, page: u64) {
        let mut shard = self.shard(page);
        if let Some(c) = shard.map.remove(&page) {
            shard.bytes -= c.bytes;
            self.used.fetch_sub(c.bytes, Ordering::Relaxed);
        }
    }

    fn clear(&self) {
        for shard in &self.shards {
            let mut shard = lock(shard);
            self.used.fetch_sub(shard.bytes, Ordering::Relaxed);
            *shard = Shard::default();
        }
    }
}

// ---------------------------------------------------------------------------
// The page store
// ---------------------------------------------------------------------------

/// Free pages and what a checkpoint needs, behind the writer's lock.
struct State {
    /// Runs of free pages: first page → pages.
    free: BTreeMap<u64, u64>,
    free_pages: u64,
    /// Pages of the file in use, free ones among them.
    end: u64,
    /// Pages a commit stopped using, with the sequence number of the first
    /// version that does not reach them: free once that version is durable
    /// and no older one is read.
    pending: Vec<(u64, Extent)>,
    /// The version the last checkpoint wrote.
    durable_seq: u64,
    /// Number of the last header known to be on disk.
    counter: u64,
    /// Headers whose write failed since: the next one skips their numbers, so
    /// that it goes to the same slot and never over the last good header.
    failed: u64,
    directory: Option<Extent>,
    /// Something changed since the last checkpoint.
    changed: bool,
}

impl State {
    fn alloc(&mut self, pages: u32) -> Extent {
        let n = u64::from(pages);
        // First fit, looking at a few runs only: a long node goes to the end
        // of the file rather than through every hole.
        let found = self.free.iter().take(if n == 1 { 1 } else { 64 }).find(|(_, len)| **len >= n);
        if let Some((&start, &len)) = found {
            self.free.remove(&start);
            if len > n {
                self.free.insert(start + n, len - n);
            }
            self.free_pages -= n;
            return Extent { start, pages };
        }
        let start = self.end;
        self.end += n;
        Extent { start, pages }
    }

    fn release(&mut self, ext: Extent) {
        let (mut start, mut len) = (ext.start, u64::from(ext.pages));
        self.free_pages += len;
        if let Some((&before, &n)) = self.free.range(..start).next_back()
            && before + n == start
        {
            self.free.remove(&before);
            start = before;
            len += n;
        }
        if let Some(n) = self.free.remove(&(start + len)) {
            len += n;
        }
        self.free.insert(start, len);
    }

    /// Gives back the free pages at the end of the file.
    fn trim(&mut self) {
        while let Some((&start, &len)) = self.free.last_key_value()
            && start + len == self.end
        {
            self.free.remove(&start);
            self.free_pages -= len;
            self.end = start;
        }
    }
}

/// Figures of the page store.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PageStats {
    /// Bytes of the keys and values.
    pub stored_bytes: u64,
    /// Bytes of the free pages inside the file.
    pub free_bytes: u64,
    /// Pages holding nodes.
    pub pages: u64,
    pub cache_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

/// The page file, its cache and the current version of its trees.
pub(crate) struct Pages {
    path: PathBuf,
    disk: RwLock<Disk>,
    /// `fsync` at each checkpoint (off only in tests).
    sync: bool,
    cache: Cache,
    /// The trees as of the last commit.
    current: RwLock<Arc<Version>>,
    /// The trees readers are given: the last commit known to be on disk (in
    /// the log, or by a checkpoint). Never ahead of `current`.
    visible: RwLock<Arc<Version>>,
    live: Arc<Live>,
    state: Mutex<State>,
    /// Bytes of the nodes changed since the last checkpoint, about.
    dirty: AtomicUsize,
    /// Checkpoints fail (tests of a disk that cannot be written).
    #[cfg(test)]
    pub(crate) fail_checkpoints: std::sync::atomic::AtomicBool,
}

/// What a file holds: its header, its trees and the pages they use.
struct Loaded {
    counter: u64,
    end: u64,
    directory: Option<Extent>,
    trees: Trees,
    used: Vec<Extent>,
}

/// Reads the header and the list of the trees, then walks the branches: the
/// pages they reach are in use, every other one is free.
fn load(disk: &Disk) -> Result<Loaded> {
    let Some(header) = disk.read_header()? else {
        return Ok(Loaded { counter: 0, end: FIRST, directory: None, trees: Trees::default(), used: Vec::new() });
    };
    let mut used = vec![header.directory];
    let buf = disk.read(header.directory)?;
    let Some((KIND_DIRECTORY, content)) = unseal(&buf) else {
        return Err(disk.damaged(header.directory.start));
    };
    let mut r = Reader(content);
    let mut trees = Trees::default();
    for _ in 0..r.len()? {
        let n = r.len()?;
        let name = std::str::from_utf8(r.bytes(n)?).map_err(|_| err("a table name is not UTF-8"))?;
        let start = r.varint()?;
        let pages = u32::try_from(r.varint()?).map_err(|_| err("a node is too long"))?;
        let bytes = r.varint()?;
        let tree = if start == 0 {
            Tree::empty()
        } else {
            let ext = Extent { start, pages };
            let root = disk.read_node(ext)?;
            used.push(ext);
            reach(disk, &root, &mut used)?;
            Tree { root: Arc::new(root), disk: Some(ext), bytes }
        };
        trees.insert(Arc::from(name), tree);
    }
    used.sort_by_key(|e| e.start);
    let mut at = FIRST;
    for e in &used {
        if e.start < at || e.start + u64::from(e.pages) > header.end {
            return Err(err(format!("page {} of {} is used twice, or past the end", e.start, disk.path.display())));
        }
        at = e.start + u64::from(e.pages);
    }
    Ok(Loaded { counter: header.counter, end: header.end, directory: Some(header.directory), trees, used })
}

/// The pages under `node`. Leaves are not read: their parent says where they are.
fn reach(disk: &Disk, node: &Node, used: &mut Vec<Extent>) -> Result<()> {
    let Node::Branch(b) = node else { return Ok(()) };
    for child in &b.children {
        let Ptr::Disk(ext) = child.ptr else { continue };
        used.push(ext);
        if b.level > 1 {
            reach(disk, &disk.read_node(ext)?, used)?;
        }
    }
    Ok(())
}

/// Writes `trees`, all on disk in `src`, packed into the empty file `dst`:
/// each node after its children, then the list of the trees and the header.
fn copy_trees(src: &Disk, trees: &Trees, dst: &Disk) -> Result<()> {
    fn copy(src: &Disk, dst: &Disk, ext: Extent, next: &mut u64) -> Result<Extent> {
        let buf = src.read(ext)?;
        let out = match unseal(&buf) {
            Some((KIND_BRANCH, content)) => {
                let Node::Branch(mut b) = Node::decode(KIND_BRANCH, content)? else {
                    return Err(src.damaged(ext.start));
                };
                for child in &mut b.children {
                    if let Ptr::Disk(e) = child.ptr {
                        child.ptr = Ptr::Disk(copy(src, dst, e, next)?);
                    }
                }
                Node::Branch(b).encode()?
            }
            // A leaf moves as it is.
            _ => buf,
        };
        let pages = (out.len() / PAGE) as u32;
        let start = *next;
        *next += u64::from(pages);
        dst.write(start, out)?;
        Ok(Extent { start, pages })
    }
    let mut next = FIRST;
    let mut copied = Trees::default();
    for (name, tree) in trees {
        let disk = match tree.disk {
            Some(ext) => Some(copy(src, dst, ext, &mut next)?),
            None if tree.root.is_empty() => None,
            None => return Err(err("a table changed since the last checkpoint")),
        };
        copied.insert(name.clone(), Tree { root: tree.root.clone(), disk, bytes: tree.bytes });
    }
    let directory = encode_directory(&copied);
    let ext = Extent { start: next, pages: (directory.len() / PAGE) as u32 };
    dst.write(ext.start, directory)?;
    dst.sync()?;
    dst.write_header(&Header { counter: 1, end: ext.start + u64::from(ext.pages), directory: ext })?;
    dst.sync()
}

fn encode_directory(trees: &Trees) -> Vec<u8> {
    let mut names: Vec<&Arc<str>> = trees.keys().collect();
    names.sort();
    let mut out = vec![0u8; HEAD];
    put_varint(&mut out, names.len() as u64);
    for name in names {
        let tree = &trees[name];
        put_varint(&mut out, name.len() as u64);
        out.extend_from_slice(name.as_bytes());
        let ext = tree.disk.unwrap_or(Extent { start: 0, pages: 0 });
        put_varint(&mut out, ext.start);
        put_varint(&mut out, u64::from(ext.pages));
        put_varint(&mut out, tree.bytes);
    }
    seal(out, KIND_DIRECTORY)
}

impl Pages {
    /// Opens (or creates) the page file at `path`, encrypted with `xts` if any.
    pub(crate) fn open(path: &Path, cache_bytes: usize, xts: Option<Arc<Xts>>, sync: bool) -> Result<Pages> {
        let disk = Disk::open(path, xts)?;
        let loaded = load(&disk)?;
        let live = Arc::new(Live::default());
        let state = State::of(&loaded);
        let version = Version::new(0, 0, loaded.trees, &live);
        Ok(Pages {
            path: path.to_owned(),
            disk: RwLock::new(disk),
            sync,
            cache: Cache::new(cache_bytes),
            current: RwLock::new(version.clone()),
            visible: RwLock::new(version),
            live,
            state: Mutex::new(state),
            dirty: AtomicUsize::new(0),
            #[cfg(test)]
            fail_checkpoints: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn disk(&self) -> std::sync::RwLockReadGuard<'_, Disk> {
        self.disk.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The trees as of the last commit.
    pub(crate) fn version(&self) -> Arc<Version> {
        self.current.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The trees as of the last commit known to be on disk: what a reader may
    /// be shown.
    pub(crate) fn visible(&self) -> Arc<Version> {
        self.visible.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `version` (given by [`version`](Self::version)) is on disk: readers
    /// may see it. An older one than they already see changes nothing.
    pub(crate) fn show(&self, version: &Arc<Version>) {
        let mut visible = self.visible.write().unwrap_or_else(|e| e.into_inner());
        if version.seq > visible.seq {
            let old = std::mem::replace(&mut *visible, version.clone());
            drop(visible);
            drop(old);
        }
    }

    fn publish(&self, version: Arc<Version>) {
        let old = std::mem::replace(&mut *self.current.write().unwrap_or_else(|e| e.into_inner()), version);
        // Outside the lock: the last holder of a version frees its nodes.
        drop(old);
    }

    /// The node at `ext`, from the cache or the disk.
    fn load(&self, ext: Extent) -> Result<Arc<Node>> {
        if let Some(node) = self.cache.get(ext.start) {
            return Ok(node);
        }
        let node = Arc::new(self.disk().read_node(ext)?);
        self.cache.insert(ext.start, node.clone());
        Ok(node)
    }

    /// The value of `key` under `node`, given to `f`.
    pub(crate) fn find<R>(&self, node: &Node, key: &[u8], f: impl FnOnce(&[u8]) -> R) -> Result<Option<R>> {
        match node {
            Node::Leaf(c) => Ok(c.search(key).ok().map(|i| f(c.val(i)))),
            Node::Branch(b) => match &b.children[b.child_index(key)].ptr {
                Ptr::Mem(n) => self.find(n, key, f),
                Ptr::Disk(e) => self.find(&*self.load(*e)?, key, f),
            },
        }
    }

    /// Calls `f` with each entry of `start..end` under `node`, in key order,
    /// until it returns `false`. Returns `false` when the walk is over (`f`
    /// stopped it, or the range ended).
    pub(crate) fn walk<F>(&self, node: &Node, start: &[u8], end: &[u8], f: &mut F) -> Result<bool>
    where
        F: FnMut(&[u8], &[u8]) -> bool,
    {
        match node {
            Node::Leaf(c) => {
                for i in c.lower_bound(start)..c.len() {
                    let key = c.key(i);
                    if key >= end || !f(key, c.val(i)) {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Node::Branch(b) => {
                for i in b.child_index(start)..b.children.len() {
                    if i > 0 && b.keys.key(i - 1) >= end {
                        return Ok(false);
                    }
                    let more = match &b.children[i].ptr {
                        Ptr::Mem(n) => self.walk(n, start, end, f)?,
                        Ptr::Disk(e) => self.walk(&*self.load(*e)?, start, end, f)?,
                    };
                    if !more {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    /// Calls `f` with every entry under `node`, in key order, until it
    /// returns `false`. Returns `false` when `f` stopped the walk.
    #[cfg(feature = "import-redb")]
    pub(crate) fn walk_all<F>(&self, node: &Node, f: &mut F) -> Result<bool>
    where
        F: FnMut(&[u8], &[u8]) -> bool,
    {
        match node {
            Node::Leaf(c) => Ok((0..c.len()).all(|i| f(c.key(i), c.val(i)))),
            Node::Branch(b) => {
                for child in &b.children {
                    let more = match &child.ptr {
                        Ptr::Mem(n) => self.walk_all(n, f)?,
                        Ptr::Disk(e) => self.walk_all(&*self.load(*e)?, f)?,
                    };
                    if !more {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    /// Applies `ops` as one commit, in memory, numbered `lsn`: the next
    /// [`version`](Self::version) holds all of them or none. Readers see it
    /// once it is [shown](Self::show) or a checkpoint ran. Durable at the next
    /// checkpoint (the write-ahead log covers the time in between).
    pub(crate) fn apply(&self, ops: &[Op], lsn: u64) -> Result<()> {
        let mut st = lock(&self.state);
        let cur = self.version();
        let mut tx = Txn { pages: self, trees: cur.trees.clone(), freed: Vec::new(), dirty: 0 };
        tx.run(ops)?;
        let seq = cur.seq + 1;
        st.pending.extend(tx.freed.into_iter().map(|e| (seq, e)));
        st.changed = true;
        self.dirty.fetch_add(tx.dirty, Ordering::Relaxed);
        self.publish(Version::new(seq, lsn, tx.trees, &self.live));
        Ok(())
    }

    /// Bytes of the nodes changed since the last checkpoint, about: they stay
    /// in memory until it.
    pub(crate) fn dirty_bytes(&self) -> usize {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Frees the pages no header and no reader can reach any more.
    fn reclaim(&self, st: &mut State) {
        let oldest = self.live.oldest();
        let durable = st.durable_seq;
        let pending = std::mem::take(&mut st.pending);
        for (seq, ext) in pending {
            if seq <= durable && seq <= oldest {
                st.release(ext);
            } else {
                st.pending.push((seq, ext));
            }
        }
    }

    /// Writes the node and what changed under it, children first.
    fn flush(&self, disk: &Disk, st: &mut State, node: &mut Arc<Node>, written: &mut Vec<Extent>) -> Result<Extent> {
        let changed = matches!(&**node, Node::Branch(b) if b.children.iter().any(|c| matches!(c.ptr, Ptr::Mem(_))));
        if changed && let Node::Branch(b) = Arc::make_mut(node) {
            for child in &mut b.children {
                if let Ptr::Mem(n) = &mut child.ptr {
                    let ext = self.flush(disk, st, n, written)?;
                    self.cache.insert(ext.start, n.clone());
                    child.ptr = Ptr::Disk(ext);
                }
            }
        }
        let buf = node.encode()?;
        let ext = st.alloc((buf.len() / PAGE) as u32);
        written.push(ext);
        // What the cache kept of an earlier node at this place is stale.
        self.cache.remove(ext.start);
        disk.write(ext.start, buf)?;
        Ok(ext)
    }

    /// Makes every commit so far durable: writes the changed nodes to free
    /// pages, then the header that points to them.
    pub(crate) fn checkpoint(&self) -> Result<()> {
        #[cfg(test)]
        if self.fail_checkpoints.load(Ordering::Relaxed) {
            return Err(err("no checkpoint (test)"));
        }
        let mut st = lock(&self.state);
        self.reclaim(&mut st);
        if !st.changed && st.counter > 0 {
            return Ok(());
        }
        let cur = self.version();
        let mut trees = cur.trees.clone();
        let disk = self.disk();
        let mut written = Vec::new();
        let nodes = (|| -> Result<Extent> {
            for tree in trees.values_mut() {
                if tree.disk.is_none() && !tree.root.is_empty() {
                    tree.disk = Some(self.flush(&disk, &mut st, &mut tree.root, &mut written)?);
                }
            }
            let directory = encode_directory(&trees);
            let ext = st.alloc((directory.len() / PAGE) as u32);
            written.push(ext);
            self.cache.remove(ext.start);
            disk.write(ext.start, directory)?;
            if self.sync {
                disk.sync()?;
            }
            Ok(ext)
        })();
        let directory = match nodes {
            Ok(ext) => ext,
            Err(e) => {
                // Nothing points to what was written: the pages are free again.
                for ext in written {
                    st.release(ext);
                }
                return Err(e);
            }
        };
        // From here a failure leaves the new pages aside (a header that may
        // have reached the disk points to them) until the file is opened again.
        let counter = st.counter + 1 + 2 * st.failed;
        st.failed += 1;
        disk.write_header(&Header { counter, end: st.end, directory })?;
        if self.sync {
            disk.sync()?;
        }
        st.counter = counter;
        st.failed = 0;
        if let Some(old) = st.directory.replace(directory) {
            st.release(old);
        }
        st.durable_seq = cur.seq;
        st.changed = false;
        self.dirty.store(0, Ordering::Relaxed);
        // The same commits, their nodes on disk: what everyone reads from now on.
        let written = Version::new(cur.seq, cur.lsn, trees, &self.live);
        let shown = std::mem::replace(&mut *self.visible.write().unwrap_or_else(|e| e.into_inner()), written.clone());
        self.publish(written);
        drop(shown);
        drop(cur);
        self.reclaim(&mut st);
        let before = st.end;
        st.trim();
        if st.end < before {
            // Best effort: a file left longer only keeps free pages at its end.
            let _ = disk.file.set_len(st.end * PAGE as u64);
        }
        Ok(())
    }

    /// Writes the trees, packed, into a new file at `path`, encrypted with
    /// `xts` if any. Everything must be on disk (a checkpoint just ran).
    pub(crate) fn copy_to(&self, path: &Path, xts: Option<Arc<Xts>>) -> Result<()> {
        let _ = std::fs::remove_file(path);
        let dst = Disk::open(path, xts)?;
        copy_trees(&self.disk(), &self.version().trees, &dst)?;
        // Read back before it is trusted.
        load(&dst).map(|_| ())
    }

    /// Rewrites the file without its free pages. `false` when a reader is at
    /// work: it would lose its pages.
    pub(crate) fn compact(&self) -> Result<bool> {
        self.checkpoint()?;
        let mut st = lock(&self.state);
        let mut current = self.current.write().unwrap_or_else(|e| e.into_inner());
        // Only the two handles of this store on one version: no reader.
        let mut visible = self.visible.write().unwrap_or_else(|e| e.into_inner());
        if !Arc::ptr_eq(&current, &visible) || Arc::strong_count(&current) != 2 || self.live.count() != 1 || st.changed
        {
            return Ok(false);
        }
        if st.free_pages == 0 && st.pending.is_empty() {
            return Ok(true);
        }
        let tmp = self.path.with_extension("compacting");
        let _ = std::fs::remove_file(&tmp);
        let mut disk = self.disk.write().unwrap_or_else(|e| e.into_inner());
        let dst = Disk::open(&tmp, disk.xts.clone())?;
        let packed = copy_trees(&disk, &current.trees, &dst).and_then(|()| load(&dst));
        let loaded = match packed {
            Ok(loaded) => loaded,
            Err(e) => {
                drop(dst);
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        std::fs::rename(&tmp, &self.path).map_err(|e| err(format!("cannot replace {}: {e}", self.path.display())))?;
        if let Some(dir) = self.path.parent() {
            super::wal::sync_dir(dir);
        }
        *disk = Disk { file: dst.file, xts: dst.xts, path: self.path.clone() };
        self.cache.clear();
        let seq = current.seq;
        *st = State::of(&loaded);
        st.durable_seq = seq;
        let packed = Version::new(seq, current.lsn, loaded.trees, &self.live);
        *visible = packed.clone();
        *current = packed;
        Ok(true)
    }

    /// Reads every page: checksums, order of the keys, sizes, and that no
    /// page is used twice. `Err` says what is wrong.
    pub(crate) fn check(&self) -> std::result::Result<(), String> {
        let st = lock(&self.state);
        let cur = self.version();
        let disk = self.disk();
        let mut used = Vec::new();
        used.extend(st.directory);
        for (name, tree) in &cur.trees {
            used.extend(tree.disk);
            let bytes =
                check_node(&disk, &tree.root, None, None, None, &mut used).map_err(|e| format!("{name}: {e}"))?;
            if bytes != tree.bytes {
                return Err(format!("{name}: holds {bytes} bytes, {} recorded", tree.bytes));
            }
        }
        used.sort_by_key(|e| e.start);
        let mut at = FIRST;
        for e in &used {
            if e.start < at || e.start + u64::from(e.pages) > st.end {
                return Err(format!("page {} is used twice, or past the end", e.start));
            }
            at = e.start + u64::from(e.pages);
            let free_before = st.free.range(..=e.start).next_back().is_some_and(|(s, n)| s + n > e.start);
            let free_inside = st.free.range(e.start..at).next().is_some();
            if free_before || free_inside {
                return Err(format!("page {} is in use and free at once", e.start));
            }
        }
        Ok(())
    }

    pub(crate) fn stats(&self) -> PageStats {
        let st = lock(&self.state);
        let waiting: u64 = st.pending.iter().map(|(_, e)| u64::from(e.pages)).sum();
        let free = st.free_pages + waiting;
        PageStats {
            stored_bytes: self.version().trees.values().map(|t| t.bytes).sum(),
            free_bytes: free * PAGE as u64,
            pages: st.end.saturating_sub(free),
            cache_bytes: self.cache.used.load(Ordering::Relaxed) as u64,
            cache_hits: self.cache.hits.load(Ordering::Relaxed),
            cache_misses: self.cache.misses.load(Ordering::Relaxed),
        }
    }
}

impl State {
    /// The free pages of a file: the ones its trees do not use.
    fn of(loaded: &Loaded) -> State {
        let mut st = State {
            free: BTreeMap::new(),
            free_pages: 0,
            end: loaded.end,
            pending: Vec::new(),
            durable_seq: 0,
            counter: loaded.counter,
            failed: 0,
            directory: loaded.directory,
            changed: false,
        };
        let mut at = FIRST;
        for e in &loaded.used {
            if e.start > at {
                st.free.insert(at, e.start - at);
                st.free_pages += e.start - at;
            }
            at = e.start + u64::from(e.pages);
        }
        if loaded.end > at {
            st.free.insert(at, loaded.end - at);
            st.free_pages += loaded.end - at;
        }
        st.trim();
        st
    }
}

/// Verifies the node and what is under it, reading from disk what is there.
/// Returns the bytes of its keys and values.
fn check_node(
    disk: &Disk,
    node: &Node,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    level: Option<u8>,
    used: &mut Vec<Extent>,
) -> std::result::Result<u64, String> {
    if level.is_some_and(|l| l != node.level()) {
        return Err("the tree is uneven".into());
    }
    let cells = match node {
        Node::Leaf(c) => c,
        Node::Branch(b) => &b.keys,
    };
    for i in 0..cells.len() {
        let key = cells.key(i);
        let sorted = i == 0 || cells.key(i - 1) < key;
        if !sorted || lower.is_some_and(|l| key < l) || upper.is_some_and(|u| key >= u) {
            return Err("keys are out of order".into());
        }
    }
    match node {
        Node::Leaf(c) => Ok(c.live as u64),
        Node::Branch(b) => {
            if b.level == 0 || b.children.len() != b.keys.len() + 1 {
                return Err("a branch does not match its children".into());
            }
            let mut total = 0;
            for (i, child) in b.children.iter().enumerate() {
                let lo = if i == 0 { lower } else { Some(b.keys.key(i - 1)) };
                let hi = if i + 1 == b.children.len() { upper } else { Some(b.keys.key(i)) };
                let bytes = match &child.ptr {
                    Ptr::Mem(n) => check_node(disk, n, lo, hi, Some(b.level - 1), used)?,
                    Ptr::Disk(e) => {
                        used.push(*e);
                        let n = disk.read_node(*e).map_err(|e| e.to_string())?;
                        check_node(disk, &n, lo, hi, Some(b.level - 1), used)?
                    }
                };
                if bytes != child.bytes {
                    return Err(format!("a node holds {bytes} bytes, {} recorded", child.bytes));
                }
                total += bytes;
            }
            Ok(total)
        }
    }
}

// ---------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------

/// Nodes a split made, to the right of the one that was split: the first
/// key they may hold, the node, the bytes of its keys and values.
type Grown = Vec<(Vec<u8>, Arc<Node>, u64)>;

/// One commit at work: the trees it changes, the pages they stop using.
struct Txn<'a> {
    pages: &'a Pages,
    trees: Trees,
    freed: Vec<Extent>,
    dirty: usize,
}

impl Txn<'_> {
    fn run(&mut self, ops: &[Op]) -> Result<()> {
        let mut i = 0;
        while i < ops.len() {
            let table = ops[i].table();
            if let Op::DropTable { .. } = &ops[i] {
                if let Some(tree) = self.trees.remove(table) {
                    self.freed.extend(tree.disk);
                    self.free_under(&tree.root)?;
                }
                i += 1;
                continue;
            }
            // The operations in a row on one table, on its tree taken out of the list.
            let (name, mut tree) = self.trees.remove_entry(table).unwrap_or_else(|| (Arc::from(table), Tree::empty()));
            let mut result = Ok(());
            while i < ops.len() && ops[i].table() == table && !matches!(ops[i], Op::DropTable { .. }) {
                result = self.op(&mut tree, &ops[i]);
                if result.is_err() {
                    break;
                }
                i += 1;
            }
            self.trees.insert(name, tree);
            result?;
        }
        Ok(())
    }

    fn op(&mut self, tree: &mut Tree, op: &Op) -> Result<()> {
        match op {
            Op::Put { key, value, .. } => {
                if key.len() + value.len() > MAX_ENTRY {
                    return Err(Error::Validation("a row is too large for Nuage DB (1 GiB at most)".into()));
                }
                self.touch(tree);
                self.dirty += key.len() + value.len();
                let (delta, grown) = self.put(&mut tree.root, key, value)?;
                tree.bytes = tree.bytes.wrapping_add_signed(delta);
                if !grown.is_empty() {
                    // The root split: a new root over the pieces.
                    let level = tree.root.level() + 1;
                    let right: u64 = grown.iter().map(|g| g.2).sum();
                    let mut keys = Cells::default();
                    let mut children = vec![Child { ptr: Ptr::Mem(tree.root.clone()), bytes: tree.bytes - right }];
                    for (sep, node, bytes) in grown {
                        keys.push(&sep, &[]);
                        children.push(Child { ptr: Ptr::Mem(node), bytes });
                    }
                    tree.root = Arc::new(Node::Branch(Branch { level, keys, children }));
                    self.dirty += PAGE;
                }
            }
            Op::Del { key, .. } => {
                // A key that is not there changes nothing: no node is copied.
                if self.pages.find(&tree.root, key, |_| ())?.is_some() {
                    self.touch(tree);
                    tree.bytes -= self.del(&mut tree.root, key)?;
                    self.settle(tree)?;
                }
            }
            Op::DelRange { start, end, .. } => {
                let mut any = false;
                self.pages.walk(&tree.root, start, end, &mut |_, _| {
                    any = true;
                    false
                })?;
                if any {
                    self.touch(tree);
                    tree.bytes -= self.del_range(&mut tree.root, start, end)?;
                    self.settle(tree)?;
                }
            }
            Op::DropTable { .. } => {}
        }
        Ok(())
    }

    /// The root is about to change: its place on disk is no longer its own.
    fn touch(&mut self, tree: &mut Tree) {
        if let Some(ext) = tree.disk.take() {
            self.freed.push(ext);
            self.dirty += ext.pages as usize * PAGE;
        }
    }

    /// After removals: a root with one child gives way to it.
    fn settle(&mut self, tree: &mut Tree) -> Result<()> {
        loop {
            let Node::Branch(b) = &*tree.root else { return Ok(()) };
            match b.children.as_slice() {
                [] => {
                    tree.root = Arc::new(Node::Leaf(Cells::default()));
                    return Ok(());
                }
                [only] => match only.ptr.clone() {
                    Ptr::Mem(node) => tree.root = node,
                    Ptr::Disk(ext) => {
                        tree.root = self.pages.load(ext)?;
                        tree.disk = Some(ext);
                        return Ok(());
                    }
                },
                _ => return Ok(()),
            }
        }
    }

    /// The child as a node in memory this commit may change: read from disk
    /// if need be, its pages then no longer used.
    fn own<'b>(&mut self, child: &'b mut Child) -> Result<&'b mut Arc<Node>> {
        if let Ptr::Disk(ext) = child.ptr {
            // Not through the cache when it is not there: this copy is about to change.
            let node = match self.pages.cache.peek(ext.start) {
                Some(node) => node,
                None => Arc::new(self.pages.disk().read_node(ext)?),
            };
            self.freed.push(ext);
            self.dirty += ext.pages as usize * PAGE;
            child.ptr = Ptr::Mem(node);
        }
        match &mut child.ptr {
            Ptr::Mem(node) => Ok(node),
            Ptr::Disk(_) => Err(err("a node did not load")),
        }
    }

    /// Writes `value` at `key` under `node`. Returns the change in bytes and
    /// the nodes a split made.
    fn put(&mut self, node: &mut Arc<Node>, key: &[u8], value: &[u8]) -> Result<(i64, Grown)> {
        match Arc::make_mut(node) {
            Node::Leaf(c) => {
                let (delta, at) = match c.search(key) {
                    Ok(i) => {
                        let old = i64::from(c.slots[i].vlen);
                        c.replace(i, value);
                        (value.len() as i64 - old, i)
                    }
                    Err(i) => {
                        c.insert(i, key, value);
                        ((key.len() + value.len()) as i64, i)
                    }
                };
                if 8 + c.size() <= ROOM || c.len() < 2 {
                    return Ok((delta, Vec::new()));
                }
                let grown = split_leaf(c, at);
                self.dirty += grown.len() * PAGE;
                Ok((delta, grown))
            }
            Node::Branch(b) => {
                let i = b.child_index(key);
                let child = self.own(&mut b.children[i])?;
                let (delta, grown) = self.put(child, key, value)?;
                let right: u64 = grown.iter().map(|g| g.2).sum();
                b.children[i].bytes = b.children[i].bytes.wrapping_add_signed(delta) - right;
                for (n, (sep, node, bytes)) in grown.into_iter().enumerate() {
                    b.keys.insert(i + n, &sep, &[]);
                    b.children.insert(i + n + 1, Child { ptr: Ptr::Mem(node), bytes });
                }
                if 9 + b.keys.size() + CHILD_SIZE * b.children.len() <= ROOM || b.children.len() < 4 {
                    return Ok((delta, Vec::new()));
                }
                self.dirty += PAGE;
                Ok((delta, vec![split_branch(b)]))
            }
        }
    }

    /// Removes `key`, which is under `node`. Returns the bytes removed.
    fn del(&mut self, node: &mut Arc<Node>, key: &[u8]) -> Result<u64> {
        match Arc::make_mut(node) {
            Node::Leaf(c) => {
                let i = c.search(key).map_err(|_| err("a key vanished"))?;
                let removed = u64::from(c.slots[i].klen + c.slots[i].vlen);
                c.remove(i);
                Ok(removed)
            }
            Node::Branch(b) => {
                let i = b.child_index(key);
                let child = self.own(&mut b.children[i])?;
                let removed = self.del(child, key)?;
                b.children[i].bytes -= removed;
                self.tidy(b, i)?;
                Ok(removed)
            }
        }
    }

    /// Removes the keys of `start..end` under `node`. Returns the bytes removed.
    fn del_range(&mut self, node: &mut Arc<Node>, start: &[u8], end: &[u8]) -> Result<u64> {
        match Arc::make_mut(node) {
            Node::Leaf(c) => {
                let (lo, hi) = (c.lower_bound(start), c.lower_bound(end));
                let removed = (lo..hi).map(|i| u64::from(c.slots[i].klen + c.slots[i].vlen)).sum();
                c.remove_range(lo, hi);
                Ok(removed)
            }
            Node::Branch(b) => {
                // Child `i` holds `start`; child `j` is the last one that may hold a key before `end`.
                let i = b.child_index(start);
                let j = b.keys.lower_bound(end);
                let mut removed = 0;
                if j > i + 1 {
                    // The children in between are whole in the range: they go without being read.
                    let level = b.level - 1;
                    let gone: Vec<Child> = b.children.drain(i + 1..j).collect();
                    b.keys.remove_range(i, j - 1);
                    for child in gone {
                        removed += child.bytes;
                        self.free_child(&child.ptr, level)?;
                    }
                }
                // The edges, the right one first so that `i` stays where it is.
                if j > i {
                    removed += self.del_range_in(b, i + 1, start, end)?;
                }
                removed += self.del_range_in(b, i, start, end)?;
                Ok(removed)
            }
        }
    }

    fn del_range_in(&mut self, b: &mut Branch, i: usize, start: &[u8], end: &[u8]) -> Result<u64> {
        let child = self.own(&mut b.children[i])?;
        let removed = self.del_range(child, start, end)?;
        b.children[i].bytes -= removed;
        self.tidy(b, i)?;
        Ok(removed)
    }

    /// After removals in child `i` (in memory): an empty child goes, a small
    /// one joins a neighbour when both fit a page with room to spare.
    fn tidy(&mut self, b: &mut Branch, i: usize) -> Result<()> {
        let (empty, size) = match &b.children[i].ptr {
            Ptr::Mem(n) => (n.is_empty(), n.size()),
            Ptr::Disk(_) => return Ok(()),
        };
        if empty {
            b.children.remove(i);
            if b.keys.len() > 0 {
                b.keys.remove(i.saturating_sub(1));
            }
            return Ok(());
        }
        if size >= ROOM / 4 || b.children.len() < 2 {
            return Ok(());
        }
        let j = if i + 1 < b.children.len() { i + 1 } else { i - 1 };
        let other = match &b.children[j].ptr {
            Ptr::Mem(n) => n.clone(),
            Ptr::Disk(e) => self.pages.load(*e)?,
        };
        if size + other.size() > ROOM * 3 / 4 {
            return Ok(());
        }
        drop(other);
        let (l, r) = (i.min(j), i.max(j));
        self.own(&mut b.children[r])?;
        let right = b.children.remove(r);
        let sep = b.keys.key(l).to_vec();
        b.keys.remove(l);
        let Ptr::Mem(right_node) = &right.ptr else { return Err(err("a node did not load")) };
        match (Arc::make_mut(self.own(&mut b.children[l])?), &**right_node) {
            (Node::Leaf(a), Node::Leaf(c)) => a.extend_from(c),
            (Node::Branch(a), Node::Branch(c)) => {
                a.keys.push(&sep, &[]);
                a.keys.extend_from(&c.keys);
                a.children.extend(c.children.iter().cloned());
            }
            _ => return Err(err("the tree is uneven")),
        }
        b.children[l].bytes += right.bytes;
        Ok(())
    }

    /// The pages of a child at `level` and of everything under it are no longer used.
    fn free_child(&mut self, ptr: &Ptr, level: u8) -> Result<()> {
        match ptr {
            Ptr::Mem(node) => self.free_under(node),
            Ptr::Disk(ext) => {
                self.freed.push(*ext);
                if level > 0 {
                    let node = self.pages.load(*ext)?;
                    self.free_under(&node)?;
                }
                Ok(())
            }
        }
    }

    fn free_under(&mut self, node: &Node) -> Result<()> {
        if let Node::Branch(b) = node {
            for child in &b.children {
                self.free_child(&child.ptr, b.level - 1)?;
            }
        }
        Ok(())
    }
}

/// Splits a leaf that outgrew its page, the entry at `at` just written. The
/// first piece stays in `c`.
fn split_leaf(c: &mut Cells, at: usize) -> Grown {
    let n = c.len();
    let mut cuts = Vec::new();
    if at == n - 1 && n > 2 {
        // Keys that come in order (a new id, a new date): the page stays full
        // and the new key starts the next one.
        cuts.push(n - 1);
    } else {
        let total: usize = (0..n).map(|i| c.entry_size(i)).sum();
        let target = total / total.div_ceil(ROOM).max(2);
        let mut acc = 0;
        for i in 0..n {
            let size = c.entry_size(i);
            // An entry that fills a page on its own gets one.
            if acc > 0 && (size > ROOM / 2 || acc + size > ROOM) {
                cuts.push(i);
                acc = 0;
            }
            acc += size;
            if acc >= target && i + 1 < n {
                cuts.push(i + 1);
                acc = 0;
            }
        }
        cuts.dedup();
        if cuts.is_empty() {
            cuts.push(n / 2);
        }
    }
    let mut grown = Grown::with_capacity(cuts.len());
    for &cut in cuts.iter().rev() {
        let right = c.split_off(cut);
        let sep = separator(c.key(c.len() - 1), right.key(0));
        let bytes = right.live as u64;
        grown.push((sep, Arc::new(Node::Leaf(right)), bytes));
    }
    grown.reverse();
    grown
}

/// Splits a branch that outgrew its page in two halves; the separator between
/// them goes up.
fn split_branch(b: &mut Branch) -> (Vec<u8>, Arc<Node>, u64) {
    let n = b.children.len();
    let total = b.keys.live + b.keys.lens + CHILD_SIZE * n;
    let mut acc = 0;
    let mut m = n / 2;
    for i in 0..n - 1 {
        acc += b.keys.entry_size(i) + CHILD_SIZE;
        if acc >= total / 2 {
            m = i + 1;
            break;
        }
    }
    let m = m.clamp(2, n - 2);
    let children = b.children.split_off(m);
    let keys = b.keys.split_off(m);
    let sep = b.keys.key(m - 1).to_vec();
    b.keys.remove(m - 1);
    let right = Branch { level: b.level, keys, children };
    let bytes = right.bytes();
    (sep, Arc::new(Node::Branch(right)), bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{Encryption, Header as KeyHeader};

    const T: &str = "b:main";

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuage-pages-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("nuage.db")
    }

    fn put(key: &[u8], value: &[u8]) -> Op {
        Op::Put { table: T.into(), key: key.to_vec(), value: value.to_vec() }
    }

    fn all(pages: &Pages, table: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        if let Some(root) = pages.version().root(table) {
            pages
                .walk(&root, &[], &[0xFF; 9], &mut |k, v| {
                    out.push((k.to_vec(), v.to_vec()));
                    true
                })
                .unwrap();
        }
        out
    }

    /// A small generator (xorshift): the same run each time.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn crc32_is_the_ieee_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
        // The log's records written by earlier versions carry Memoire's CRC-32: the same one.
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        for n in [0, 1, 7, 8, 9, 63, 64, 1000] {
            assert_eq!(crc32(&data[..n]), memoire_orm::evaluator::crc32(&data[..n]), "{n} bytes");
        }
    }

    /// Nodes rewritten checkpoint after checkpoint leave and come back to
    /// the cache: the clock's ring does not grow with them.
    #[test]
    fn the_cache_ring_stays_as_small_as_the_cache() {
        let cache = Cache::new(64 << 20);
        let node = Arc::new(Node::Leaf(Cells::default()));
        for round in 0..2000u64 {
            for page in 0..200u64 {
                cache.remove(page);
                cache.insert(page, node.clone());
            }
            if round % 500 == 0 {
                cache.insert(1_000_000 + round, node.clone());
            }
        }
        let (mut held, mut ring) = (0, 0);
        for shard in &cache.shards {
            let shard = lock(shard);
            assert!(shard.ring.len() <= 2 * shard.map.len() + RING_SLACK + 1, "{} places", shard.ring.len());
            assert!(shard.map.keys().all(|p| shard.ring.contains(p)), "every page held has its place");
            held += shard.map.len();
            ring += shard.ring.len();
        }
        assert_eq!(held, 204);
        assert!(ring < 204 * 2 + SHARDS * (RING_SLACK + 1), "{ring} places for {held} pages");
        // The clock still makes room: a cache too small for all of them keeps the last ones in.
        let small = Cache::new(SHARDS * 4 * node.mem());
        for page in 0..10_000u64 {
            small.remove(page / 2);
            small.insert(page, node.clone());
        }
        assert!(small.used.load(Ordering::Relaxed) <= SHARDS * 4 * node.mem());
        assert!(small.peek(9_999).is_some());
    }

    #[test]
    fn varints_round_trip() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(out.len(), varint_len(v), "{v}");
            assert_eq!(Reader(&out).varint().unwrap(), v);
        }
    }

    /// Random commits against a `BTreeMap`: the same content at every step,
    /// through checkpoints, restarts and compactions, with or without a key.
    fn random_commits(name: &str, xts: Option<Arc<Xts>>, keys: u64, rounds: usize, batch: u64) {
        let path = temp(name);
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut pages = Pages::open(&path, 256 << 10, xts.clone(), false).unwrap();
        let key = |rng: &mut Rng| {
            let n = rng.below(keys);
            // Some keys long, most with a shared prefix.
            match n % 97 {
                0 => format!("k{n:05}{}", "long".repeat(300)).into_bytes(),
                _ => format!("k{n:05}").into_bytes(),
            }
        };
        // A wide tree is compared whole from time to time only.
        let every = if keys > 10_000 { 7 } else { 1 };
        let mut levels = 0;
        for round in 0..rounds {
            let mut ops = Vec::new();
            for _ in 0..rng.below(batch) + 1 {
                match rng.below(20) {
                    0..=11 => {
                        let k = key(&mut rng);
                        let len = match rng.below(50) {
                            0 if keys <= 10_000 => 9_000 + rng.below(30_000) as usize,
                            1 => 0,
                            _ if keys <= 10_000 => rng.below(200) as usize,
                            _ => rng.below(24) as usize,
                        };
                        let v: Vec<u8> = (0..len).map(|i| (i as u64 + rng.0) as u8).collect();
                        model.insert(k.clone(), v.clone());
                        ops.push(put(&k, &v));
                    }
                    12..=17 => {
                        let k = key(&mut rng);
                        model.remove(&k);
                        ops.push(Op::Del { table: T.into(), key: k });
                    }
                    _ => {
                        let (a, b) = (key(&mut rng), key(&mut rng));
                        let (start, end) = if a <= b { (a, b) } else { (b, a) };
                        // In the wide tree, a few deletions that take whole branches.
                        let width = match rng.below(if keys <= 10_000 { 4 } else { 600 }) {
                            0 if keys <= 10_000 => usize::MAX,
                            0 => 20_000,
                            _ if keys <= 10_000 => 30,
                            _ => 3,
                        };
                        let gone: Vec<Vec<u8>> =
                            model.range(start.clone()..end.clone()).take(width).map(|(k, _)| k.clone()).collect();
                        let end = if gone.len() == width { gone[width - 1].clone() } else { end };
                        for k in model.range(start.clone()..end.clone()).map(|(k, _)| k.clone()).collect::<Vec<_>>() {
                            model.remove(&k);
                        }
                        ops.push(Op::DelRange { table: T.into(), start, end });
                    }
                }
            }
            let whole = round % every == 0;
            let before = pages.version();
            levels = levels.max(before.root(T).map_or(0, |r| r.level()));
            let seen_before = if whole { all(&pages, T) } else { Vec::new() };
            pages.apply(&ops, 0).unwrap();
            let expected: Vec<_> =
                if whole { model.iter().map(|(k, v)| (k.clone(), v.clone())).collect() } else { Vec::new() };
            if whole {
                assert_eq!(all(&pages, T), expected, "round {round}");
                // A reader that started before the commit still sees what it saw.
                let mut seen = Vec::new();
                let root = before.root(T).unwrap_or_else(|| Arc::new(Node::Leaf(Cells::default())));
                pages
                    .walk(&root, &[], &[0xFF], &mut |k, v| {
                        seen.push((k.to_vec(), v.to_vec()));
                        true
                    })
                    .unwrap();
                assert_eq!(seen, seen_before, "round {round}: the earlier version moved");
            }
            drop(before);
            for (k, v) in model.iter().take(20) {
                let root = pages.version().root(T).unwrap();
                assert_eq!(pages.find(&root, k, |v| v.to_vec()).unwrap().as_ref(), Some(v));
            }
            assert_eq!(pages.version().bytes(T), model.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum::<u64>());
            match round % 23 {
                5 | 11 => pages.checkpoint().unwrap(),
                17 => {
                    pages.checkpoint().unwrap();
                    pages.check().unwrap();
                    drop(pages);
                    pages = Pages::open(&path, 256 << 10, xts.clone(), false).unwrap();
                    pages.check().unwrap();
                    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    assert_eq!(all(&pages, T), expected, "round {round}: after a restart");
                }
                21 => {
                    assert!(pages.compact().unwrap());
                    pages.check().unwrap();
                    assert_eq!(pages.stats().free_bytes, 0);
                    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    assert_eq!(all(&pages, T), expected, "round {round}: after a compaction");
                }
                _ => {}
            }
        }
        assert!(levels >= if keys > 10_000 { 2 } else { 1 }, "the tree grew to {levels} levels above its leaves");
        pages.checkpoint().unwrap();
        pages.check().unwrap();
        let stats = pages.stats();
        assert!(stats.cache_bytes <= 2 * (256 << 10), "{stats:?}");
        drop(pages);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn random_commits_match_a_map() {
        random_commits("random", None, 3_000, 400, 40);
    }

    /// Enough keys for three levels: branches split, merge and go whole in range deletions.
    #[test]
    fn random_commits_match_a_map_in_a_deep_tree() {
        random_commits("random-deep", None, 400_000, 140, 6_000);
    }

    #[test]
    fn random_commits_match_a_map_encrypted() {
        let (_, keys) = KeyHeader::create(&Encryption::from_key([9; 32])).unwrap();
        random_commits("random-enc", Some(keys.pages), 3_000, 400, 40);
    }

    #[test]
    fn keys_in_order_fill_their_pages_and_free_pages_are_reused() {
        let path = temp("fill");
        let pages = Pages::open(&path, 1 << 20, None, false).unwrap();
        let value = [7u8; 100];
        for chunk in 0..20u32 {
            let ops: Vec<Op> =
                (0..1_000u32).map(|i| put(format!("row{:08}", chunk * 1_000 + i).as_bytes(), &value)).collect();
            pages.apply(&ops, 0).unwrap();
        }
        pages.checkpoint().unwrap();
        let stats = pages.stats();
        // 20 000 keys of 11 bytes with values of 100: 2.2 MB of data.
        assert_eq!(stats.stored_bytes, 20_000 * 111);
        let file = std::fs::metadata(&path).unwrap().len();
        assert!(file < stats.stored_bytes * 12 / 10, "{file} bytes on disk for {} of data", stats.stored_bytes);
        // The same rows written again and again: the file does not grow with each round.
        for round in 0..30u32 {
            let ops: Vec<Op> = (0..500u32)
                .map(|i| put(format!("row{:08}", (round * 37 + i * 40) % 20_000).as_bytes(), &[round as u8; 100]))
                .collect();
            pages.apply(&ops, 0).unwrap();
            pages.checkpoint().unwrap();
        }
        let grown = std::fs::metadata(&path).unwrap().len();
        assert!(grown < file * 2, "{file} bytes grew to {grown}");
        pages.check().unwrap();
        // Dropping the table frees it all, and the file shrinks at the next checkpoint.
        pages.apply(&[Op::DropTable { table: T.into() }, put(b"left", b"1")], 0).unwrap();
        pages.checkpoint().unwrap();
        pages.checkpoint().unwrap();
        assert_eq!(all(&pages, T), vec![(b"left".to_vec(), b"1".to_vec())]);
        assert!(std::fs::metadata(&path).unwrap().len() < 64 << 10);
        pages.check().unwrap();
        drop(pages);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_reader_keeps_its_pages_through_checkpoints() {
        let path = temp("reader");
        let pages = Pages::open(&path, 64 << 10, None, false).unwrap();
        let ops: Vec<Op> = (0..5_000u32).map(|i| put(format!("k{i:06}").as_bytes(), &[1; 50])).collect();
        pages.apply(&ops, 0).unwrap();
        pages.checkpoint().unwrap();
        let old = pages.version();
        // Every row rewritten, several checkpoints: the pages the reader may
        // still read are not handed out.
        for round in 2..6u8 {
            let ops: Vec<Op> = (0..5_000u32).map(|i| put(format!("k{i:06}").as_bytes(), &[round; 50])).collect();
            pages.apply(&ops, 0).unwrap();
            pages.checkpoint().unwrap();
            assert!(!pages.compact().unwrap(), "a reader is at work");
        }
        let mut n = 0;
        pages
            .walk(&old.root(T).unwrap(), &[], &[0xFF], &mut |_, v| {
                assert_eq!(v, [1; 50]);
                n += 1;
                true
            })
            .unwrap();
        assert_eq!(n, 5_000);
        drop(old);
        assert!(pages.compact().unwrap());
        assert_eq!(all(&pages, T).len(), 5_000);
        pages.check().unwrap();
        drop(pages);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn damage_and_a_second_handle_are_refused() {
        let path = temp("damage");
        let pages = Pages::open(&path, 1 << 20, None, true).unwrap();
        let ops: Vec<Op> = (0..2_000u32).map(|i| put(format!("k{i:06}").as_bytes(), &[3; 80])).collect();
        pages.apply(&ops, 0).unwrap();
        pages.checkpoint().unwrap();
        let again = Pages::open(&path, 1 << 20, None, true).err().unwrap().to_string();
        assert!(again.contains("already open"), "{again}");
        drop(pages);
        // One byte of a leaf flipped: the checksum catches it.
        let mut raw = std::fs::read(&path).unwrap();
        raw[5 * PAGE + 100] ^= 0x40;
        std::fs::write(&path, &raw).unwrap();
        let pages = Pages::open(&path, 1 << 20, None, true).unwrap();
        let wrong = pages.check().unwrap_err();
        assert!(wrong.contains("damaged"), "{wrong}");
        drop(pages);
        // Both headers gone: not taken for an empty file.
        raw[..2 * PAGE].fill(0xAB);
        std::fs::write(&path, &raw).unwrap();
        let unreadable = Pages::open(&path, 1 << 20, None, true).err().unwrap().to_string();
        assert!(unreadable.contains("damaged"), "{unreadable}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_copy_under_another_key_holds_the_same_data() {
        let path = temp("copy");
        let pages = Pages::open(&path, 1 << 20, None, false).unwrap();
        let ops: Vec<Op> = (0..3_000u32).map(|i| put(format!("k{i:06}").as_bytes(), b"SECRET-VALUE")).collect();
        pages.apply(&ops, 0).unwrap();
        pages.apply(&[Op::Put { table: "meta".into(), key: b"a".to_vec(), value: b"b".to_vec() }], 0).unwrap();
        pages.checkpoint().unwrap();
        let copy = path.with_extension("copy");
        let (_, keys) = KeyHeader::create(&Encryption::from_key([3; 32])).unwrap();
        pages.copy_to(&copy, Some(keys.pages.clone())).unwrap();
        let raw = std::fs::read(&copy).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"SECRET"), "nothing in clear");
        assert!(std::fs::read(&path).unwrap().windows(6).any(|w| w == b"SECRET"));
        let encrypted = Pages::open(&copy, 1 << 20, Some(keys.pages), false).unwrap();
        assert_eq!(all(&encrypted, T), all(&pages, T));
        assert_eq!(all(&encrypted, "meta"), vec![(b"a".to_vec(), b"b".to_vec())]);
        encrypted.check().unwrap();
        drop(encrypted);
        // Without the key, or with another: refused.
        assert!(Pages::open(&copy, 1 << 20, None, false).is_err());
        let (_, other) = KeyHeader::create(&Encryption::from_key([4; 32])).unwrap();
        assert!(Pages::open(&copy, 1 << 20, Some(other.pages), false).is_err());
        drop(pages);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
