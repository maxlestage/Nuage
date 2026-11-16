//! The key-value store of an application in its database
//! (`NUAGE_KV_STORAGE=database`): the keys live in a tree of the
//! application's page file, written through the same log and commit path as
//! its rows, so they survive restarts, are encrypted and are in the backups.
//! No table listing, SQL statement or branch sees them.
//!
//! The commands are run by the store's own executor ([`Locked::command`]),
//! unchanged, on a scratch copy of the keys they name: those keys are read
//! from the tree into a scratch store, the commands run there, and what they
//! changed is written as one log record, a transaction included. The
//! commands that look at the whole store (`KEYS`, `SCAN`, `DBSIZE`, `INFO`,
//! `FLUSHDB`, `FLUSHALL`) are answered here, from the tree and what the call
//! changed so far: `SCAN` page by page, with a cursor the store remembers,
//! and `KEYS` up to a limit, so that no call builds the list of every key of
//! a store bounded only by the disk.
//!
//! The tree `kv`:
//!
//! | Key | Value |
//! |---|---|
//! | `k` + key | the expiry (Unix ms, little endian, 0: never), then the value |
//! | `e` + expiry (big endian) + key | nothing: the keys that expire, by date |
//! | `n` | the keys, then the keys that expire (two numbers, little endian) |
//!
//! It is made by the first write, never at opening. A call that only reads
//! goes by the last commit on disk and takes no lock. A call that writes
//! holds the writer lock of the application's database, reads its last
//! commit, and is answered once its record is on disk. An expired key is
//! never returned nor counted; it leaves the tree when a write touches it,
//! or in the batches of a pass that runs at most every 30 seconds, followed
//! by the next one at once while more keys are due than one pass removes.
//!
//! Nothing is evicted: a value is 64 MiB at most, and the store is bounded
//! by the disk, like the tables.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use memoire_orm::{Error, Result};

use super::{
    Inner, KeyInfo, Kv, KvError, KvStats, Locked, MAX_KEY, READ_ONLY, Reply, SetOptions, Ttl, glob, info, number,
    scan_pattern, upper, writes,
};
use crate::engine::keys::prefix_end;
use crate::engine::pages::{Node, Pages, Version};
use crate::engine::store::now_millis;
use crate::engine::wal::Op;
use crate::engine::{Engine, storage_err};

/// The tree of the key-value store in the page file.
const TREE: &str = "kv";
/// The longest value, in bytes.
pub(crate) const MAX_VALUE: usize = 64 << 20;
/// Time between two passes that remove expired keys from the tree.
const SWEEP_EVERY_MS: i64 = 30_000;
/// Expired keys removed by one log record.
const SWEEP_BATCH: usize = 1000;
/// Records one pass writes at most; what is left goes in the next pass, at
/// once, on the same thread.
const SWEEP_ROUNDS: usize = 20;
/// The most one call may write, in bytes: what one log record carries.
const MAX_WRITE: usize = 1 << 30;
/// The key of the two counters.
const COUNTS: &[u8] = b"n";
/// The most keys `KEYS` answers: past it, it answers an error and `SCAN`
/// gives them page by page.
const KEYS_MAX: usize = 100_000;
/// ... and the most bytes of keys.
const KEYS_MAX_BYTES: usize = 16 << 20;
/// Entries of the tree one page of `SCAN` goes through when it is not given
/// a `COUNT` (as many as Redis's own).
const SCAN_COUNT: usize = 10;
/// How long the store remembers a `SCAN` cursor, at least: a client that
/// pauses between two pages keeps its cursor, however many pages the other
/// clients scan meanwhile ...
const CURSOR_KEEP_MS: u64 = 10 * 60 * 1000;
/// ... unless the cursors of the store take more than this many bytes: the
/// oldest go first.
const CURSORS_BYTES: usize = 4 << 20;
/// What one cursor takes besides its key: its number, when it was given, and
/// the key's own allocation.
const CURSOR_BYTES: usize = 48;
/// A `SCAN` cursor the store does not remember.
const UNKNOWN_CURSOR: &str = "invalid cursor: this server does not know it (it was given more than 10 minutes \
                              ago, or before a restart): start again with SCAN 0";

/// The clock of the expiries: Unix time in milliseconds.
fn now_ms() -> u64 {
    u64::try_from(now_millis()).unwrap_or(0)
}

fn entry_key(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 1);
    out.push(b'k');
    out.extend_from_slice(key);
    out
}

fn index_key(expires: u64, key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 9);
    out.push(b'e');
    out.extend_from_slice(&expires.to_be_bytes());
    out.extend_from_slice(key);
    out
}

/// An entry as stored: its expiry (0: never), then its value.
fn entry_value(expires: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 8);
    out.extend_from_slice(&expires.to_le_bytes());
    out.extend_from_slice(value);
    out
}

/// The expiry and the value of a stored entry; `None` when it is damaged.
fn split(stored: &[u8]) -> Option<(u64, &[u8])> {
    let (expires, value) = stored.split_first_chunk::<8>()?;
    Some((u64::from_le_bytes(*expires), value))
}

fn counts_value(keys: u64, expiring: u64) -> Vec<u8> {
    let mut out = keys.to_le_bytes().to_vec();
    out.extend_from_slice(&expiring.to_le_bytes());
    out
}

fn damaged(what: &str) -> Error {
    storage_err(format!("key-value store: {what} is damaged in the page store"))
}

/// A key with this expiry is still there at `now`.
fn alive(expires: u64, now: u64) -> bool {
    expires == 0 || expires > now
}

fn add(count: u64, delta: i64) -> u64 {
    count.saturating_add_signed(delta)
}

/// The counters of the tree under `tree`: the keys, and those that expire.
fn read_counts(pages: &Pages, tree: &Node) -> Result<(u64, u64)> {
    match pages.find(tree, COUNTS, |v| {
        let (keys, expiring) = v.split_first_chunk::<8>()?;
        let expiring: [u8; 8] = expiring.try_into().ok()?;
        Some((u64::from_le_bytes(*keys), u64::from_le_bytes(expiring)))
    })? {
        None => Ok((0, 0)),
        Some(Some(counts)) => Ok(counts),
        Some(None) => Err(damaged("the count of keys")),
    }
}

/// Up to `limit` entries of the index under `tree` whose time has come at
/// `now`: their expiry and their key.
fn due_entries(pages: &Pages, tree: &Node, now: u64, limit: usize) -> Result<Vec<(u64, Vec<u8>)>> {
    let mut out = Vec::new();
    pages.walk(tree, b"e", &index_key(now.saturating_add(1), b""), &mut |k, _| {
        if let Some((at, key)) = k[1..].split_first_chunk::<8>() {
            out.push((u64::from_be_bytes(*at), key.to_vec()));
        }
        out.len() < limit
    })?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Where each command finds its keys
// ---------------------------------------------------------------------------

/// Where a command's keys are among its arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// The first one.
    First,
    /// Every one.
    Every,
    /// Every other one, from the first (`MSET key value key value`).
    EveryOther,
    /// It looks at the whole store.
    Whole,
    /// It touches no key.
    Nothing,
}

/// Where the command `name` finds its keys; `None` when the store does not
/// know it.
fn place(name: &str) -> Option<Place> {
    Some(match name {
        "KEYS" | "SCAN" | "DBSIZE" | "INFO" | "FLUSHDB" | "FLUSHALL" => Place::Whole,
        "GET" | "SET" | "SETNX" | "SETEX" | "PSETEX" | "GETSET" | "GETDEL" | "GETEX" | "APPEND" | "STRLEN" | "INCR"
        | "DECR" | "INCRBY" | "DECRBY" | "EXPIRE" | "PEXPIRE" | "PERSIST" | "TTL" | "PTTL" | "TYPE" => Place::First,
        "MGET" | "DEL" | "UNLINK" | "EXISTS" => Place::Every,
        "MSET" | "MSETNX" => Place::EveryOther,
        "PING" | "ECHO" => Place::Nothing,
        _ => return None,
    })
}

/// What a command touches.
enum Touch<'a> {
    /// These keys: none for `PING`, `ECHO` and the names the store does not
    /// know (the executor answers those with its own error).
    Keys(Vec<&'a [u8]>),
    /// The whole store.
    Whole,
}

/// What the command `name` touches, `args` being its arguments (its name
/// aside).
fn keys_of<'a>(name: &str, args: &'a [Vec<u8>]) -> Touch<'a> {
    match place(name) {
        Some(Place::Whole) => Touch::Whole,
        Some(Place::First) => Touch::Keys(args.first().map(Vec::as_slice).into_iter().collect()),
        Some(Place::Every) => Touch::Keys(args.iter().map(Vec::as_slice).collect()),
        Some(Place::EveryOther) => Touch::Keys(args.iter().step_by(2).map(Vec::as_slice).collect()),
        Some(Place::Nothing) | None => Touch::Keys(Vec::new()),
    }
}

/// The `COUNT` of `SCAN`'s options: [`SCAN_COUNT`] without one. The other
/// options are checked by `scan_pattern`.
fn scan_count(options: &[Vec<u8>]) -> Result<usize, Reply> {
    let mut count = SCAN_COUNT;
    for pair in options.chunks(2) {
        if let [name, value] = pair
            && upper(name) == "COUNT"
        {
            count = match number::<i64>(value) {
                Some(n) if n >= 1 => usize::try_from(n).unwrap_or(usize::MAX),
                Some(_) => return Err(Reply::err("syntax error")),
                None => return Err(Reply::err("value is not an integer or out of range")),
            };
        }
    }
    Ok(count)
}

/// The cursors `SCAN` gave out: each number stands for the last key of its
/// page. Each one is kept [`CURSOR_KEEP_MS`] at least, so a client that asks
/// a page again gets it again, and one that pauses between two pages goes
/// on, whatever the other clients scan; [`CURSORS_BYTES`] bound them all.
///
/// The numbers of one opening of the store start at a value of their own
/// (its time of opening, shifted): a cursor given by an earlier opening (before
/// a restart, or by an application deleted since and made again) is not one
/// this one gave, and is refused instead of standing for another client's
/// page. They stay below 2^53, which a JavaScript client reads exactly.
struct Cursors {
    last: u64,
    /// Oldest first: the number, when it was given (Unix ms) and the key.
    given: std::collections::VecDeque<(u64, u64, Vec<u8>)>,
    /// Bytes they take ([`CURSOR_BYTES`] each, and their keys).
    bytes: usize,
}

impl Cursors {
    /// The cursors of a store opened at `now` (Unix ms): none yet.
    fn new(now: u64) -> Cursors {
        Cursors { last: now << 10, given: std::collections::VecDeque::new(), bytes: 0 }
    }

    /// A new cursor for the page after `key`, given at `now` (Unix ms).
    fn give(&mut self, key: Vec<u8>, now: u64) -> u64 {
        // 0 means "from the start": never given.
        self.last = self.last.checked_add(1).unwrap_or(1);
        self.bytes += CURSOR_BYTES + key.len();
        self.given.push_back((self.last, now, key));
        // Those past their time, then the oldest while they take too much.
        while let Some((_, at, key)) = self.given.front() {
            let expired = now.saturating_sub(*at) > CURSOR_KEEP_MS;
            if !expired && self.bytes <= CURSORS_BYTES || self.given.len() == 1 {
                break;
            }
            self.bytes -= CURSOR_BYTES + key.len();
            self.given.pop_front();
        }
        self.last
    }

    /// The key the cursor `n` stands for.
    fn find(&self, n: u64) -> Option<Vec<u8>> {
        // Given in order: the numbers grow from front to back.
        let i = self.given.binary_search_by_key(&n, |(c, _, _)| *c).ok()?;
        self.given.get(i).map(|(_, _, key)| key.clone())
    }
}

/// How far [`Run::walk`] went.
struct Walked {
    /// It reached the end of the tree.
    complete: bool,
    /// The last entry it went through.
    last: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// One call
// ---------------------------------------------------------------------------

/// What a key held in the tree a call found, compared with the scratch.
struct Old {
    expires: u64,
    len: usize,
    /// The scratch holds the same expiry and the same value.
    same: bool,
}

/// What a call changed, against the tree it found.
#[derive(Default)]
struct Change {
    /// Some key changed.
    changed: bool,
    /// The keys stored, and those that expire (expired or not).
    keys: i64,
    expiring: i64,
    /// The keys alive now, and those of them that expire.
    live: i64,
    live_expiring: i64,
    /// Bytes of the tree's keys and values.
    bytes: i64,
}

/// One call on the store: a command, or the commands of a transaction, run
/// in order on one scratch store.
struct Run<'a> {
    kv: &'a DatabaseKv,
    pages: &'a Pages,
    /// The commit the call goes by: held while it reads.
    version: Arc<Version>,
    /// The tree in `version` (`None`: there is none yet).
    tree: Option<Arc<Node>>,
    /// The call emptied the store (`FLUSHDB`): the tree goes, then what
    /// follows is written to a new one.
    flushed: bool,
    /// The keys the call names: the scratch holds them as the call left them.
    declared: BTreeSet<Vec<u8>>,
    scratch: Inner,
    now: u64,
    /// It holds the writer lock: what it changes is written.
    write: bool,
    /// Expired keys the call removed before it emptied the store.
    expired: u64,
}

impl<'a> Run<'a> {
    fn new(kv: &'a DatabaseKv, version: Arc<Version>, write: bool) -> Run<'a> {
        Run {
            kv,
            pages: &kv.engine.store.pages,
            tree: version.root(TREE),
            version,
            flushed: false,
            declared: BTreeSet::new(),
            scratch: Inner::default(),
            now: now_ms(),
            write,
            expired: 0,
        }
    }

    /// The tree as the call goes by it: none once it emptied the store.
    fn tree(&self) -> Option<&Node> {
        if self.flushed { None } else { self.tree.as_deref() }
    }

    /// Reads `keys` into the scratch, each with its expiry: one that expired
    /// is there too, and the executor treats it as missing, as it does in
    /// memory.
    fn load(&mut self, keys: &[&[u8]]) -> Result<()> {
        for key in keys {
            if !self.declared.insert(key.to_vec()) {
                continue;
            }
            let Some(tree) = self.tree.clone() else { continue };
            match self.pages.find(&tree, &entry_key(key), |stored| split(stored).map(|(at, v)| (at, v.to_vec())))? {
                None => {}
                Some(None) => return Err(damaged("a key")),
                Some(Some((at, value))) => self.scratch.write(key, value, (at != 0).then_some(at)),
            }
        }
        Ok(())
    }

    /// The executor, on the scratch. Its limit is no limit: past one, it
    /// would let go of the least recently used keys of the scratch, and the
    /// call would then delete them from the tree.
    fn locked(&mut self) -> Locked<'_> {
        Locked { max_bytes: usize::MAX, inner: &mut self.scratch, now: self.now }
    }

    /// Runs one command (`name` is `args[0]` in capitals).
    fn command(&mut self, name: &str, args: &[Vec<u8>], read_only: bool) -> Result<Reply> {
        let rest = args.get(1..).unwrap_or_default();
        let reply = match keys_of(name, rest) {
            Touch::Whole => self.whole(name, rest, args, read_only)?,
            Touch::Keys(_) if self.too_large(name, rest) => Reply::err(KvError::TooLarge),
            Touch::Keys(_) => self.locked().command(args, read_only),
        };
        self.kv.count(&mut self.scratch);
        Ok(reply)
    }

    /// The command would write a value larger than [`MAX_VALUE`]: refused
    /// where the executor refuses a value larger than the store in memory.
    fn too_large(&mut self, name: &str, args: &[Vec<u8>]) -> bool {
        if !self.write {
            return false;
        }
        let key_ok = |k: &[u8]| !k.is_empty() && k.len() <= MAX_KEY;
        let big = |v: &[u8]| v.len() > MAX_VALUE;
        match (name, args) {
            ("SET", [key, value, ..]) | ("SETNX" | "GETSET", [key, value]) => key_ok(key) && big(value),
            ("SETEX" | "PSETEX", [key, ttl, value]) => {
                number::<u64>(ttl).is_some_and(|n| n > 0) && key_ok(key) && big(value)
            }
            ("MSET" | "MSETNX", pairs) if !pairs.is_empty() && pairs.len().is_multiple_of(2) => {
                pairs.chunks(2).take_while(|pair| key_ok(&pair[0])).any(|pair| big(&pair[1]))
            }
            ("APPEND", [key, more]) if key_ok(key) => {
                let now = self.now;
                let len = match self.scratch.alive(key, now) {
                    true => self.scratch.map.get(key.as_slice()).map_or(0, |e| e.value.len()),
                    false => 0,
                };
                len + more.len() > MAX_VALUE
            }
            _ => false,
        }
    }

    /// A command on the whole store, answered from the tree and the scratch.
    fn whole(&mut self, name: &str, rest: &[Vec<u8>], args: &[Vec<u8>], read_only: bool) -> Result<Reply> {
        if read_only && writes(name) {
            return Ok(Reply::Error(READ_ONLY.into()));
        }
        let list = |keys: Vec<Vec<u8>>| Reply::Array(keys.into_iter().map(Reply::Bulk).collect());
        Ok(match (name, rest) {
            ("KEYS", [pattern]) => match self.matching(pattern)? {
                Some(keys) => list(keys),
                None => Reply::err(format!(
                    "too many keys for KEYS (more than {KEYS_MAX} or {} MiB): use SCAN, which answers page by page",
                    KEYS_MAX_BYTES >> 20
                )),
            },
            ("SCAN", [cursor, options @ ..]) => match (scan_pattern(options), scan_count(options)) {
                (Ok(pattern), Ok(count)) => self.scan(cursor, pattern, count)?,
                (Err(refused), _) | (_, Err(refused)) => refused,
            },
            ("DBSIZE", []) => Reply::Int(self.stats()?.keys as i64),
            ("INFO", _) => info(&self.stats()?),
            ("FLUSHDB" | "FLUSHALL", _) => {
                self.flush();
                Reply::Ok
            }
            // A wrong number of arguments: the executor's own answer.
            _ => Locked { max_bytes: usize::MAX, inner: &mut Inner::default(), now: self.now }.command(args, read_only),
        })
    }

    /// Empties the store: the tree goes when the call is written.
    fn flush(&mut self) {
        debug_assert!(self.write, "a call that only reads does not empty the store");
        self.expired += self.scratch.expired;
        self.kv.count(&mut self.scratch);
        self.scratch = Inner::default();
        self.flushed = true;
    }

    /// The keys alive that match `pattern`, sorted: those of the tree the call
    /// does not name, and those it names as the scratch holds them. `None`
    /// past [`KEYS_MAX`] keys or [`KEYS_MAX_BYTES`] of them: the list is not
    /// made, whatever the size of the store.
    fn matching(&self, pattern: &[u8]) -> Result<Option<Vec<Vec<u8>>>> {
        let mut keys = Vec::new();
        let mut bytes = 0;
        let mut keep = |keys: &mut Vec<Vec<u8>>, key: &[u8]| {
            bytes += key.len();
            keys.push(key.to_vec());
            keys.len() <= KEYS_MAX && bytes <= KEYS_MAX_BYTES
        };
        let walked = self.walk(&[], usize::MAX, &mut |key| !glob(pattern, key) || keep(&mut keys, key))?;
        if !walked.complete {
            return Ok(None);
        }
        for key in self.alive_declared(pattern, &[], None) {
            if !keep(&mut keys, &key) {
                return Ok(None);
            }
        }
        keys.sort();
        Ok(Some(keys))
    }

    /// Up to `limit` entries of the tree after the key `after` (from the
    /// first when empty), in order, each alive key the call does not name
    /// given to `f`, until it returns `false`. Expired keys count towards
    /// `limit` too: it bounds the work.
    fn walk(&self, after: &[u8], limit: usize, f: &mut dyn FnMut(&[u8]) -> bool) -> Result<Walked> {
        let mut walked = Walked { complete: true, last: None };
        let Some(tree) = self.tree() else { return Ok(walked) };
        let mut start = entry_key(after);
        if !after.is_empty() {
            // The key right after it.
            start.push(0);
        }
        let (mut seen, mut bad) = (0usize, false);
        self.pages.walk(tree, &start, b"l", &mut |k, stored| {
            if seen == limit {
                walked.complete = false;
                return false;
            }
            seen += 1;
            let key = &k[1..];
            walked.last = Some(key.to_vec());
            match split(stored) {
                Some((at, _)) => {
                    if alive(at, self.now) && !self.declared.contains(key) && !f(key) {
                        walked.complete = false;
                        return false;
                    }
                    true
                }
                None => {
                    bad = true;
                    false
                }
            }
        })?;
        if bad {
            return Err(damaged("a key"));
        }
        Ok(walked)
    }

    /// The keys the call names that are alive in the scratch and match
    /// `pattern`, after `after` and up to `upto` (to the end when `None`).
    fn alive_declared(&self, pattern: &[u8], after: &[u8], upto: Option<&[u8]>) -> Vec<Vec<u8>> {
        self.declared
            .iter()
            .filter(|key| key.as_slice() > after && upto.is_none_or(|upto| key.as_slice() <= upto))
            .filter(|key| {
                self.scratch.map.get(key.as_slice()).is_some_and(|e| e.expires.is_none_or(|at| at > self.now))
                    && glob(pattern, key)
            })
            .cloned()
            .collect()
    }

    /// One page of `SCAN`: the keys alive that match `pattern` among the
    /// next `count` entries of the tree after where `cursor` stopped, and
    /// the cursor of the next page (`0`: this was the last one). A cursor is
    /// a number the store gives out and remembers the last key of its page
    /// by ([`Cursors`]): every key alive from the first page to the last is
    /// returned, whatever is written meanwhile.
    fn scan(&self, cursor: &[u8], pattern: &[u8], count: usize) -> Result<Reply> {
        let after = match number::<u64>(cursor) {
            Some(0) => Vec::new(),
            Some(n) => match self.kv.cursors.lock().unwrap_or_else(|e| e.into_inner()).find(n) {
                Some(key) => key,
                None => return Ok(Reply::err(UNKNOWN_CURSOR)),
            },
            None => return Ok(Reply::err("invalid cursor")),
        };
        let mut keys = Vec::new();
        // A large COUNT is a large page, not the whole store at once.
        let walked = self.walk(&after, count.min(KEYS_MAX), &mut |key| {
            if glob(pattern, key) {
                keys.push(key.to_vec());
            }
            true
        })?;
        let upto = if walked.complete { None } else { walked.last.as_deref() };
        keys.extend(self.alive_declared(pattern, &after, upto));
        keys.sort();
        let next = match (walked.complete, walked.last) {
            (false, Some(last)) => self.kv.cursors.lock().unwrap_or_else(|e| e.into_inner()).give(last, now_ms()),
            _ => 0,
        };
        Ok(Reply::Array(vec![
            Reply::Bulk(next.to_string().into_bytes()),
            Reply::Array(keys.into_iter().map(Reply::Bulk).collect()),
        ]))
    }

    /// The counters of the tree the call found.
    fn counts(&self) -> Result<(u64, u64)> {
        match self.tree() {
            Some(tree) => read_counts(self.pages, tree),
            None => Ok((0, 0)),
        }
    }

    /// The figures of the store as the call leaves it so far.
    fn stats(&self) -> Result<KvStats> {
        let (keys, expiring) = self.counts()?;
        let due = match self.tree() {
            Some(tree) => {
                let mut n = 0u64;
                self.pages.walk(tree, b"e", &index_key(self.now.saturating_add(1), b""), &mut |_, _| {
                    n += 1;
                    true
                })?;
                n
            }
            None => 0,
        };
        let (change, _) = self.change(false)?;
        let mut bytes = if self.tree().is_some() { self.version.bytes(TREE) } else { 0 };
        if change.changed && self.counts_missing()? {
            bytes += (COUNTS.len() + 16) as u64;
        }
        let kv = self.kv;
        Ok(KvStats {
            keys: add(keys.saturating_sub(due), change.live) as usize,
            expiring: add(expiring.saturating_sub(due), change.live_expiring) as usize,
            bytes: add(bytes, change.bytes) as usize,
            max_bytes: 0,
            hits: kv.hits.load(Ordering::Relaxed),
            misses: kv.misses.load(Ordering::Relaxed),
            evictions: 0,
            expired: kv.expired.load(Ordering::Relaxed),
        })
    }

    /// The tree the call goes by has no counters yet.
    fn counts_missing(&self) -> Result<bool> {
        Ok(match self.tree() {
            Some(tree) => self.pages.find(tree, COUNTS, |_| ())?.is_none(),
            None => true,
        })
    }

    /// What the scratch changed against the tree, for every key the call
    /// names; with `with_ops`, the operations that write it too.
    fn change(&self, with_ops: bool) -> Result<(Change, Vec<Op>)> {
        let mut c = Change::default();
        let mut ops = Vec::new();
        for key in &self.declared {
            let new = self.scratch.map.get(key.as_slice()).map(|e| (e.expires.unwrap_or(0), e.value.as_slice()));
            let old = match self.tree() {
                Some(tree) => match self.pages.find(tree, &entry_key(key), |stored| {
                    split(stored).map(|(expires, value)| Old {
                        expires,
                        len: value.len(),
                        same: new == Some((expires, value)),
                    })
                })? {
                    Some(Some(old)) => Some(old),
                    Some(None) => return Err(damaged("a key")),
                    None => None,
                },
                None => None,
            };
            match (&old, new) {
                (None, None) => continue,
                (Some(o), Some(_)) if o.same => continue,
                _ => c.changed = true,
            }
            let (old_at, new_at) = (old.as_ref().map_or(0, |o| o.expires), new.map_or(0, |(at, _)| at));
            c.keys += i64::from(new.is_some()) - i64::from(old.is_some());
            c.expiring += i64::from(new_at != 0) - i64::from(old_at != 0);
            let live_old = old.as_ref().is_some_and(|o| alive(o.expires, self.now));
            let live_new = new.is_some_and(|(at, _)| alive(at, self.now));
            c.live += i64::from(live_new) - i64::from(live_old);
            c.live_expiring += i64::from(live_new && new_at != 0) - i64::from(live_old && old_at != 0);
            let entry = |len: usize| (9 + key.len() + len) as i64;
            let index = (9 + key.len()) as i64;
            c.bytes += new.map_or(0, |(_, v)| entry(v.len())) - old.as_ref().map_or(0, |o| entry(o.len));
            if old_at != new_at {
                c.bytes += index * (i64::from(new_at != 0) - i64::from(old_at != 0));
            }
            if !with_ops {
                continue;
            }
            match new {
                Some((_, value)) if value.len() > MAX_VALUE => {
                    return Err(Error::Validation(KvError::TooLarge.to_string()));
                }
                Some((at, value)) => {
                    ops.push(Op::Put { table: TREE.into(), key: entry_key(key), value: entry_value(at, value) });
                }
                None => ops.push(Op::Del { table: TREE.into(), key: entry_key(key) }),
            }
            if old_at != new_at {
                if old_at != 0 {
                    ops.push(Op::Del { table: TREE.into(), key: index_key(old_at, key) });
                }
                if new_at != 0 {
                    ops.push(Op::Put { table: TREE.into(), key: index_key(new_at, key), value: Vec::new() });
                }
            }
        }
        if c.changed && with_ops {
            let (keys, expiring) = self.counts()?;
            ops.push(Op::Put {
                table: TREE.into(),
                key: COUNTS.to_vec(),
                value: counts_value(add(keys, c.keys), add(expiring, c.expiring)),
            });
        }
        Ok((c, ops))
    }

    /// The operations that write what the call changed, as one record.
    fn ops(&self) -> Result<Vec<Op>> {
        if self.scratch.map.keys().any(|k| !self.declared.contains(&k[..])) {
            return Err(storage_err(
                "key-value store: a command touched a key it did not declare: nothing was written",
            ));
        }
        let (_, mut ops) = self.change(true)?;
        if self.flushed && self.tree.is_some() {
            ops.insert(0, Op::DropTable { table: TREE.into() });
        }
        let written: usize = ops
            .iter()
            .map(|op| match op {
                Op::Put { key, value, .. } => key.len() + value.len(),
                _ => 0,
            })
            .sum();
        if written > MAX_WRITE {
            return Err(Error::Validation(
                "the key-value store writes 1 GiB at most in one call: nothing was written".into(),
            ));
        }
        Ok(ops)
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// The key-value store of an application, in its database. See the
/// [module](self) docs.
pub(crate) struct DatabaseKv {
    engine: Arc<Engine>,
    hits: AtomicU64,
    misses: AtomicU64,
    expired: AtomicU64,
    /// When the next pass that removes expired keys may run (Unix ms).
    next_sweep: AtomicI64,
    /// A pass is running.
    sweeping: AtomicBool,
    /// The cursors `SCAN` gave out.
    cursors: std::sync::Mutex<Cursors>,
}

impl DatabaseKv {
    /// The store of the database `engine`.
    pub(crate) fn new(engine: Arc<Engine>) -> DatabaseKv {
        DatabaseKv {
            engine,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            next_sweep: AtomicI64::new(0),
            sweeping: AtomicBool::new(false),
            cursors: std::sync::Mutex::new(Cursors::new(now_ms())),
        }
    }

    /// Moves the reads a scratch counted to the store's figures.
    fn count(&self, scratch: &mut Inner) {
        self.hits.fetch_add(std::mem::take(&mut scratch.hits), Ordering::Relaxed);
        self.misses.fetch_add(std::mem::take(&mut scratch.misses), Ordering::Relaxed);
    }

    /// A call that only reads: from the last commit on disk, with no lock.
    fn read<T>(&self, keys: &[&[u8]], f: impl FnOnce(&mut Run<'_>) -> Result<T>) -> Result<T> {
        let mut run = Run::new(self, self.engine.store.pages.visible(), false);
        run.load(keys)?;
        let out = f(&mut run);
        self.count(&mut run.scratch);
        out
    }

    /// A call that may write: under the writer lock, from the last commit.
    /// What it changed is one record, on disk before this returns; when it
    /// changed nothing, the commit it read is on disk before this returns.
    fn write<T>(&self, keys: &[&[u8]], f: impl FnOnce(&mut Run<'_>) -> Result<T>) -> Result<T> {
        let store = &self.engine.store;
        let mut w = store.writer();
        let mut run = Run::new(self, store.pages.version(), true);
        run.load(keys)?;
        let out = f(&mut run);
        self.count(&mut run.scratch);
        let out = out?;
        let ops = run.ops()?;
        let lsn = if ops.is_empty() { run.version.lsn() } else { store.log_and_apply(&mut w, ops)? };
        let expired = run.expired + run.scratch.expired;
        drop(run);
        drop(w);
        store.wait_durable(lsn)?;
        self.expired.fetch_add(expired, Ordering::Relaxed);
        Ok(out)
    }

    /// Runs `commands` in order as one call: one record for all they write.
    fn run(&self, commands: &[&[Vec<u8>]], read_only: bool) -> Result<Vec<Reply>> {
        let names: Vec<String> = commands.iter().map(|c| c.first().map(|n| upper(n)).unwrap_or_default()).collect();
        let mut keys = Vec::new();
        for (name, args) in names.iter().zip(commands) {
            if let Touch::Keys(k) = keys_of(name, args.get(1..).unwrap_or_default()) {
                keys.extend(k);
            }
        }
        let body = |run: &mut Run<'_>| -> Result<Vec<Reply>> {
            names.iter().zip(commands).map(|(name, args)| run.command(name, args, read_only)).collect()
        };
        if !read_only && names.iter().any(|n| writes(n)) { self.write(&keys, body) } else { self.read(&keys, body) }
    }

    /// Runs one command, as [`Kv::command`] does.
    pub(crate) fn command(&self, args: &[Vec<u8>], read_only: bool) -> Result<Reply> {
        Ok(self.run(&[args], read_only)?.pop().unwrap_or(Reply::Nil))
    }

    /// Runs several commands as one, as [`Kv::transaction`] does: one record
    /// for all they write.
    pub(crate) fn transaction(&self, commands: &[Vec<Vec<u8>>], read_only: bool) -> Result<Vec<Reply>> {
        let commands: Vec<&[Vec<u8>]> = commands.iter().map(Vec::as_slice).collect();
        self.run(&commands, read_only)
    }

    /// The value of `key` and how long it still lives.
    pub(crate) fn get_with_ttl(&self, key: &[u8]) -> Result<Option<(Vec<u8>, Ttl)>> {
        self.read(&[key], |run| Ok(run.locked().get_with_ttl(key)))
    }

    /// Writes `value` under `key`, as [`Kv::set`] does.
    pub(crate) fn set(&self, key: &[u8], value: Vec<u8>, options: &SetOptions) -> Result<Result<bool, KvError>> {
        if !key.is_empty() && key.len() <= MAX_KEY && value.len() > MAX_VALUE {
            return Ok(Err(KvError::TooLarge));
        }
        self.write(&[key], |run| Ok(run.locked().set(key, value, options)))
    }

    /// Deletes `key`; `true` when it existed.
    pub(crate) fn delete(&self, key: &[u8]) -> Result<bool> {
        self.write(&[key], |run| Ok(run.locked().delete(key)))
    }

    /// Adds `by` to the counter `key`, as [`Kv::incr_by`] does.
    pub(crate) fn incr_by(&self, key: &[u8], by: i64, ttl_if_new: Option<Duration>) -> Result<Result<i64, KvError>> {
        self.write(&[key], |run| Ok(run.locked().incr_by(key, by, ttl_if_new)))
    }

    /// Makes `key` expire after `ttl` (`None`: never); `false` when it does
    /// not exist.
    pub(crate) fn expire(&self, key: &[u8], ttl: Option<Duration>) -> Result<bool> {
        self.write(&[key], |run| Ok(run.locked().expire(key, ttl)))
    }

    /// The keys alive that start with `prefix`, sorted, `limit` at most.
    pub(crate) fn keys(&self, prefix: &[u8], limit: usize) -> Result<Vec<KeyInfo>> {
        let pages = &self.engine.store.pages;
        let version = pages.visible();
        let Some(tree) = version.root(TREE) else { return Ok(Vec::new()) };
        let now = now_ms();
        let start = entry_key(prefix);
        let mut out = Vec::new();
        let mut bad = false;
        pages.walk(&tree, &start, &prefix_end(&start), &mut |k, stored| {
            if out.len() >= limit {
                return false;
            }
            let Some((at, value)) = split(stored) else {
                bad = true;
                return false;
            };
            if alive(at, now) {
                let ttl = if at == 0 { Ttl::Forever } else { Ttl::Millis(at - now) };
                out.push(KeyInfo { key: k[1..].to_vec(), size: value.len(), ttl });
            }
            true
        })?;
        if bad {
            return Err(damaged("a key"));
        }
        Ok(out)
    }

    /// Deletes every key; returns how many were alive.
    pub(crate) fn clear(&self) -> Result<usize> {
        self.write(&[], |run| {
            let n = run.stats()?.keys;
            run.flush();
            Ok(n)
        })
    }

    /// What the store holds: no limit, nothing evicted.
    pub(crate) fn stats(&self) -> Result<KvStats> {
        self.read(&[], |run| run.stats())
    }

    /// Removes expired keys from the tree soon, on the blocking threads (or
    /// here, outside a runtime): at most every 30 seconds, never two passes
    /// at once. A pass that stops at its limit with more keys due is followed
    /// by the next one at once, on the same thread, until none is due (the
    /// writer lock is let go between two batches): the removals keep up with
    /// the expiries, whatever their rate. Meanwhile no read returns an
    /// expired key and the figures leave them out.
    pub(crate) fn sweep_soon(self: &Arc<Self>) {
        if now_millis() < self.next_sweep.load(Ordering::Relaxed) || self.sweeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let kv = self.clone();
        let pass = move || {
            loop {
                match kv.sweep_now() {
                    // More due, and this pass removed some: on. A pass that removed
                    // nothing (the commit it works from has already dropped what
                    // the visible one still shows) waits for the next tick.
                    Ok((removed, true)) if removed > 0 => continue,
                    Ok(_) => break,
                    Err(e) => {
                        kv.engine.event("error", format!("key-value store: expired keys could not be removed: {e}"));
                        break;
                    }
                }
            }
            kv.next_sweep.store(now_millis() + SWEEP_EVERY_MS, Ordering::Relaxed);
            kv.sweeping.store(false, Ordering::Release);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(pass)),
            Err(_) => pass(),
        }
    }

    /// Removes the keys whose time has come, a batch per record, and
    /// returns how many, and whether more were due when it stopped at its
    /// limit ([`SWEEP_ROUNDS`] batches). Each one is read again first: a key
    /// written since with another expiry stays, and only its old index entry
    /// goes.
    fn sweep_now(&self) -> Result<(usize, bool)> {
        let store = &self.engine.store;
        let pages = &store.pages;
        let mut removed = 0;
        for _ in 0..SWEEP_ROUNDS {
            let now = now_ms();
            // Nothing due on disk: the writer lock is not taken.
            let visible = pages.visible();
            match visible.root(TREE) {
                Some(tree) if !due_entries(pages, &tree, now, 1)?.is_empty() => {}
                _ => break,
            }
            drop(visible);
            let mut w = store.writer();
            let version = pages.version();
            let Some(tree) = version.root(TREE) else { break };
            let due = due_entries(pages, &tree, now, SWEEP_BATCH)?;
            let (mut keys, mut expiring) = read_counts(pages, &tree)?;
            let mut ops = Vec::new();
            let mut gone = 0;
            for (at, key) in &due {
                match pages.find(&tree, &entry_key(key), |stored| split(stored).map(|(e, _)| e))? {
                    Some(None) => return Err(damaged("a key")),
                    Some(Some(expires)) if expires == *at => {
                        ops.push(Op::Del { table: TREE.into(), key: entry_key(key) });
                        keys = keys.saturating_sub(1);
                        expiring = expiring.saturating_sub(1);
                        gone += 1;
                    }
                    // Written again since, with another expiry, or gone: its entry here is stale.
                    _ => {}
                }
                ops.push(Op::Del { table: TREE.into(), key: index_key(*at, key) });
            }
            if ops.is_empty() {
                break;
            }
            if gone > 0 {
                ops.push(Op::Put { table: TREE.into(), key: COUNTS.to_vec(), value: counts_value(keys, expiring) });
            }
            let lsn = store.log_and_apply(&mut w, ops)?;
            drop((w, version, tree));
            store.wait_durable(lsn)?;
            self.expired.fetch_add(gone, Ordering::Relaxed);
            removed += gone as usize;
            if due.len() < SWEEP_BATCH {
                return Ok((removed, false));
            }
        }
        // Stopped at the limit, or found nothing to do: is more due?
        Ok((removed, self.due_now()?))
    }

    /// Some key on disk is due to leave the tree.
    fn due_now(&self) -> Result<bool> {
        let pages = &self.engine.store.pages;
        let visible = pages.visible();
        Ok(match visible.root(TREE) {
            Some(tree) => !due_entries(pages, &tree, now_ms(), 1)?.is_empty(),
            None => false,
        })
    }
}

// ---------------------------------------------------------------------------
// An application's store, wherever it lives
// ---------------------------------------------------------------------------

/// The key-value store of an application: in the server's memory (the
/// default), or in the application's database (`NUAGE_KV_STORAGE=database`).
/// The same calls, the same answers; in the database, a call can also fail
/// as a write to the disk does.
pub(crate) enum AppKv {
    /// [`Kv`], as it is.
    Memory(Kv),
    /// [`DatabaseKv`].
    Database(Arc<DatabaseKv>),
}

impl AppKv {
    /// The store of the database `engine`: in it when its options say so
    /// ([`EngineOptions::kv_in_database`](crate::EngineOptions::kv_in_database)),
    /// else in memory, `memory_bytes` at most.
    pub(crate) fn open(engine: &Arc<Engine>, memory_bytes: usize) -> AppKv {
        if engine.options().kv_in_database {
            AppKv::Database(Arc::new(DatabaseKv::new(engine.clone())))
        } else {
            AppKv::Memory(Kv::new(memory_bytes))
        }
    }

    /// The keys are in the application's database: a call reads or waits for
    /// the disk.
    pub(crate) fn in_database(&self) -> bool {
        matches!(self, AppKv::Database(_))
    }

    /// Where the keys are: `memory` or `database`.
    pub(crate) fn storage(&self) -> &'static str {
        if self.in_database() { "database" } else { "memory" }
    }

    /// The memory a store in memory may hold; nothing for one in the
    /// database.
    pub(crate) fn set_max_bytes(&self, max_bytes: usize) {
        if let AppKv::Memory(kv) = self {
            kv.set_max_bytes(max_bytes);
        }
    }

    /// Removes the expired keys: at once in memory, soon in the database.
    pub(crate) fn sweep(&self) {
        match self {
            AppKv::Memory(kv) => {
                kv.sweep();
            }
            AppKv::Database(kv) => kv.sweep_soon(),
        }
    }

    pub(crate) fn get_with_ttl(&self, key: &[u8]) -> Result<Option<(Vec<u8>, Ttl)>> {
        match self {
            AppKv::Memory(kv) => Ok(kv.get_with_ttl(key)),
            AppKv::Database(kv) => kv.get_with_ttl(key),
        }
    }

    pub(crate) fn set(&self, key: &[u8], value: Vec<u8>, options: &SetOptions) -> Result<Result<bool, KvError>> {
        match self {
            AppKv::Memory(kv) => Ok(kv.set(key, value, options)),
            AppKv::Database(kv) => kv.set(key, value, options),
        }
    }

    pub(crate) fn delete(&self, key: &[u8]) -> Result<bool> {
        match self {
            AppKv::Memory(kv) => Ok(kv.delete(key)),
            AppKv::Database(kv) => kv.delete(key),
        }
    }

    pub(crate) fn incr_by(&self, key: &[u8], by: i64, ttl_if_new: Option<Duration>) -> Result<Result<i64, KvError>> {
        match self {
            AppKv::Memory(kv) => Ok(kv.incr_by(key, by, ttl_if_new)),
            AppKv::Database(kv) => kv.incr_by(key, by, ttl_if_new),
        }
    }

    pub(crate) fn expire(&self, key: &[u8], ttl: Option<Duration>) -> Result<bool> {
        match self {
            AppKv::Memory(kv) => Ok(kv.expire(key, ttl)),
            AppKv::Database(kv) => kv.expire(key, ttl),
        }
    }

    pub(crate) fn keys(&self, prefix: &[u8], limit: usize) -> Result<Vec<KeyInfo>> {
        match self {
            AppKv::Memory(kv) => Ok(kv.keys(prefix, limit)),
            AppKv::Database(kv) => kv.keys(prefix, limit),
        }
    }

    pub(crate) fn clear(&self) -> Result<usize> {
        match self {
            AppKv::Memory(kv) => Ok(kv.clear()),
            AppKv::Database(kv) => kv.clear(),
        }
    }

    pub(crate) fn stats(&self) -> Result<KvStats> {
        match self {
            AppKv::Memory(kv) => Ok(kv.stats()),
            AppKv::Database(kv) => kv.stats(),
        }
    }

    pub(crate) fn command(&self, args: &[Vec<u8>], read_only: bool) -> Result<Reply> {
        match self {
            AppKv::Memory(kv) => Ok(kv.command(args, read_only)),
            AppKv::Database(kv) => kv.command(args, read_only),
        }
    }

    pub(crate) fn transaction(&self, commands: &[Vec<Vec<u8>>], read_only: bool) -> Result<Vec<Reply>> {
        match self {
            AppKv::Memory(kv) => Ok(kv.transaction(commands, read_only)),
            AppKv::Database(kv) => kv.transaction(commands, read_only),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::{Embedded, EngineOptions};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuage-kv-database-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The engine of `dir`, keeping its key-value store; once the handle
    /// dropped just before has let go of it.
    fn open(dir: &Path, sync: bool) -> Arc<Engine> {
        let options = EngineOptions { sync, kv_in_database: true, ..EngineOptions::default() };
        for _ in 0..400 {
            match Engine::open(dir, options.clone()) {
                Ok(engine) => return engine,
                Err(e) if e.to_string().contains("already open") => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("{e}"),
            }
        }
        panic!("{} stays open", dir.display())
    }

    fn args(command: &[&str]) -> Vec<Vec<u8>> {
        command.iter().map(|a| a.as_bytes().to_vec()).collect()
    }

    fn bulk(s: &str) -> Reply {
        Reply::Bulk(s.as_bytes().to_vec())
    }

    fn lsn(engine: &Engine) -> u64 {
        engine.store.applied_lsn()
    }

    /// Each key of the tree with its expiry, the index (expiry, key), and the
    /// two counters.
    type Tree = (Vec<(Vec<u8>, u64)>, Vec<(u64, Vec<u8>)>, (u64, u64));

    /// What the tree holds; `None` when there is no tree.
    fn tree_of(engine: &Engine) -> Option<Tree> {
        let pages = &engine.store.pages;
        let version = pages.version();
        let tree = version.root(TREE)?;
        let (mut keys, mut index) = (Vec::new(), Vec::new());
        pages
            .walk(&tree, b"k", b"l", &mut |k, v| {
                keys.push((k[1..].to_vec(), split(v).unwrap().0));
                true
            })
            .unwrap();
        pages
            .walk(&tree, b"e", b"f", &mut |k, _| {
                index.push((u64::from_be_bytes(k[1..9].try_into().unwrap()), k[9..].to_vec()));
                true
            })
            .unwrap();
        Some((keys, index, read_counts(pages, &tree).unwrap()))
    }

    /// An `INFO` without what is said of the memory and of the keys removed
    /// on expiry: where the keys are changes those, by design.
    fn without_memory(info: &[u8]) -> String {
        String::from_utf8_lossy(info)
            .split("\r\n")
            .filter(|l| {
                !["used_memory:", "maxmemory:", "maxmemory_policy:", "expired_keys:"].iter().any(|p| l.starts_with(p))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The two stores gave the same answer to `command`.
    fn assert_same(command: &[Vec<u8>], memory: &Reply, database: &Reply) {
        let name = command.first().map(|n| upper(n)).unwrap_or_default();
        let what = String::from_utf8_lossy(&command.concat()).into_owned();
        match (name.as_str(), memory, database) {
            // The two clocks are read a moment apart.
            ("TTL", Reply::Int(a), Reply::Int(b)) if *a > 0 => assert!(a.abs_diff(*b) <= 1, "{what}: {a} {b}"),
            ("PTTL", Reply::Int(a), Reply::Int(b)) if *a > 0 => assert!(a.abs_diff(*b) <= 100, "{what}: {a} {b}"),
            ("INFO", Reply::Bulk(a), Reply::Bulk(b)) => assert_eq!(without_memory(a), without_memory(b), "{what}"),
            _ => assert_eq!(memory, database, "{what}"),
        }
    }

    /// Every command of the executor, in both stores, one after the other.
    const SCRIPT: &[&[&str]] = &[
        &["PING"],
        &["ping", "hello"],
        &["ECHO", "h\u{e9}"],
        &["SET", "k", "v"],
        &["get", "k"],
        &["GET", "nope"],
        &["SET", "k", "w", "NX"],
        &["SET", "k", "w", "XX", "GET"],
        &["SET", "new", "1", "XX"],
        &["SET", "k", "x", "EX", "100"],
        &["TTL", "k"],
        &["SET", "k", "y", "KEEPTTL"],
        &["PTTL", "k"],
        &["PERSIST", "k"],
        &["PERSIST", "k"],
        &["TTL", "k"],
        &["TTL", "nope"],
        &["PTTL", "nope"],
        &["EXPIRE", "k", "10"],
        &["PEXPIRE", "k", "20000"],
        &["PTTL", "k"],
        &["EXPIRE", "nope", "10"],
        &["SET", "lock", "me", "NX", "PX", "30000"],
        &["SET", "lock", "you", "NX", "GET"],
        &["SET", "k", "v", "NX", "XX"],
        &["SET", "k", "v", "EX", "0"],
        &["SET", "k", "v", "KEEPTTL", "EX", "5"],
        &["SET", "k", "v", "BOGUS"],
        &["SETEX", "s", "10", "v"],
        &["SETEX", "s", "0", "v"],
        &["PSETEX", "ps", "5000", "v"],
        &["SETNX", "s", "v"],
        &["SETNX", "fresh", "v"],
        &["GETSET", "fresh", "w"],
        &["GETSET", "other", "w"],
        &["GETDEL", "other"],
        &["GETDEL", "other"],
        &["GETEX", "s"],
        &["GETEX", "s", "PERSIST"],
        &["TTL", "s"],
        &["GETEX", "s", "EX", "50"],
        &["GETEX", "s", "PX", "50000"],
        &["GETEX", "s", "EX", "0"],
        &["GETEX", "s", "BOGUS"],
        &["GETEX", "nope", "EX", "5"],
        &["INCR", "n"],
        &["INCRBY", "n", "9"],
        &["DECR", "n"],
        &["DECRBY", "n", "4"],
        &["INCRBY", "n", "x"],
        &["DECRBY", "n", "-9223372036854775808"],
        &["INCR", "k"],
        &["SET", "big", "9223372036854775807"],
        &["INCR", "big"],
        &["APPEND", "k", "z"],
        &["APPEND", "appended", "abc"],
        &["STRLEN", "k"],
        &["STRLEN", "nope"],
        &["MSET", "a", "1", "b", "2"],
        &["MSET", "a", "1", "b"],
        &["MGET", "a", "nope", "b"],
        &["MSETNX", "a", "9", "c", "3"],
        &["MSETNX", "c", "3", "d", "4"],
        &["EXISTS", "a", "b", "nope", "a"],
        &["TYPE", "a"],
        &["TYPE", "nope"],
        &["DEL", "c", "d", "nope"],
        &["UNLINK", "appended"],
        &["KEYS", "?"],
        &["KEYS", "*"],
        &["KEYS", "l*"],
        &["SCAN", "0", "MATCH", "[ab]", "COUNT", "10"],
        &["SCAN", "0"],
        &["SCAN", "0", "MATCH"],
        &["DBSIZE"],
        &["INFO"],
        &["SET", "short", "v", "PX", "30"],
        &["MSET", "soon", "1", "later", "2"],
        &["PEXPIRE", "soon", "30"],
        &["sleep"],
        &["GET", "short"],
        &["TTL", "short"],
        &["EXISTS", "short", "soon", "later"],
        &["KEYS", "s*"],
        &["DBSIZE"],
        &["INFO", "keyspace"],
        &["SET", "short", "again"],
        &["GET", "short"],
        &["EXPIRE", "a", "0"],
        &["EXPIRE", "a", "0"],
        &["PEXPIRE", "b", "-5"],
        &["GET", "a"],
        &["GET"],
        &["GET", "a", "b"],
        &["SET", "k"],
        &["KEYS"],
        &["DBSIZE", "x"],
        &["SCAN"],
        &["MGET"],
        &["LPUSH", "l", "x"],
        &[],
        &["SET", "", "v"],
        &["DBSIZE"],
        &["FLUSHALL"],
        &["DBSIZE"],
        &["GET", "k"],
        &["SET", "k", "v"],
        &["INFO"],
    ];

    /// The same script in memory and in the database: the same answers, one
    /// by one; then transactions, then a read-only token.
    #[test]
    fn the_same_commands_give_the_same_answers_in_memory_and_in_the_database() {
        let dir = temp("same");
        let engine = open(&dir, false);
        let memory = AppKv::open(&Engine::open(temp("same-memory"), EngineOptions::default()).unwrap(), 64 << 20);
        let database = AppKv::open(&engine, 64 << 20);
        assert!(!memory.in_database() && database.in_database());
        assert_eq!((memory.storage(), database.storage()), ("memory", "database"));
        let long = "x".repeat(MAX_KEY + 1);
        let mut script: Vec<Vec<&str>> = SCRIPT.iter().map(|c| c.to_vec()).collect();
        script.insert(script.len() - 5, vec!["SET", &long, "v"]);
        for command in &script {
            if command == &["sleep"] {
                std::thread::sleep(Duration::from_millis(60));
                continue;
            }
            let command = args(command);
            let (m, d) = (memory.command(&command, false).unwrap(), database.command(&command, false).unwrap());
            assert_same(&command, &m, &d);
        }
        // In the database there is no limit, and nothing is evicted.
        let info = |kv: &AppKv| match kv.command(&args(&["INFO"]), false).unwrap() {
            Reply::Bulk(b) => String::from_utf8(b).unwrap(),
            other => panic!("{other:?}"),
        };
        assert!(info(&database).contains("\r\nmaxmemory:0\r\nmaxmemory_policy:noeviction\r\n"), "{}", info(&database));
        assert!(info(&memory).contains("\r\nmaxmemory_policy:allkeys-lru\r\n"));

        // A transaction of keyed commands, and one that looks at the whole store.
        let transactions: &[&[&[&str]]] = &[
            &[
                &["SET", "t", "1"],
                &["INCR", "t"],
                &["GET", "t"],
                &["MSET", "u", "1", "v", "2"],
                &["DEL", "u"],
                &["MGET", "t", "u", "v"],
            ],
            &[
                &["SET", "w", "1", "EX", "60"],
                &["DBSIZE"],
                &["KEYS", "*"],
                &["DEL", "t"],
                &["SCAN", "0", "MATCH", "?"],
                &["INFO"],
                &["FLUSHDB"],
                &["GET", "v"],
                &["DBSIZE"],
                &["SET", "x", "after", "PX", "60000"],
                &["KEYS", "*"],
                &["DBSIZE"],
                &["INFO"],
            ],
        ];
        for commands in transactions {
            let commands: Vec<Vec<Vec<u8>>> = commands.iter().map(|c| args(c)).collect();
            let (m, d) =
                (memory.transaction(&commands, false).unwrap(), database.transaction(&commands, false).unwrap());
            assert_eq!(m.len(), d.len());
            for ((command, m), d) in commands.iter().zip(&m).zip(&d) {
                assert_same(command, m, d);
            }
        }

        // A read-only token reads, and nothing else.
        for kv in [&memory, &database] {
            kv.set(b"k", b"v".to_vec(), &SetOptions::default()).unwrap().unwrap();
        }
        let reads: &[&[&str]] = &[
            &["GET", "k"],
            &["SET", "k", "w"],
            &["DEL", "k"],
            &["INCR", "n"],
            &["FLUSHALL"],
            &["FLUSHDB"],
            &["GETEX", "k", "EX", "5"],
            &["GETEX", "k"],
            &["KEYS", "*"],
            &["DBSIZE"],
            &["TTL", "x"],
        ];
        for command in reads {
            let command = args(command);
            let (m, d) = (memory.command(&command, true).unwrap(), database.command(&command, true).unwrap());
            assert_same(&command, &m, &d);
        }
        let commands: Vec<Vec<Vec<u8>>> = reads.iter().map(|c| args(c)).collect();
        let (m, d) = (memory.transaction(&commands, true).unwrap(), database.transaction(&commands, true).unwrap());
        for ((command, m), d) in commands.iter().zip(&m).zip(&d) {
            assert_same(command, m, d);
        }
        for kv in [&memory, &database] {
            assert_eq!(kv.get_with_ttl(b"k").unwrap(), Some((b"v".to_vec(), Ttl::Forever)));
        }

        // The typed calls answer as in memory.
        for kv in [&memory, &database] {
            assert!(
                kv.set(b"lock", b"me".to_vec(), &SetOptions { only_if_absent: true, ..SetOptions::default() })
                    .unwrap()
                    .unwrap()
            );
            assert!(
                !kv.set(b"lock", b"you".to_vec(), &SetOptions { only_if_absent: true, ..SetOptions::default() })
                    .unwrap()
                    .unwrap()
            );
            assert!(
                !kv.set(b"absent", b"x".to_vec(), &SetOptions { only_if_present: true, ..SetOptions::default() })
                    .unwrap()
                    .unwrap()
            );
            assert_eq!(kv.set(b"", b"x".to_vec(), &SetOptions::default()).unwrap(), Err(KvError::BadKey));
            assert_eq!(kv.incr_by(b"rate", 1, Some(Duration::from_secs(60))).unwrap(), Ok(1));
            assert_eq!(kv.incr_by(b"rate", 1, Some(Duration::from_secs(600))).unwrap(), Ok(2));
            assert!(
                matches!(kv.get_with_ttl(b"rate").unwrap(), Some((v, Ttl::Millis(ms))) if v == b"2" && ms <= 60_000)
            );
            assert_eq!(kv.incr_by(b"lock", 1, None).unwrap(), Err(KvError::NotInteger));
            assert!(kv.expire(b"lock", Some(Duration::from_secs(5))).unwrap());
            assert!(kv.expire(b"lock", None).unwrap() && !kv.expire(b"absent", None).unwrap());
            assert!(kv.delete(b"lock").unwrap() && !kv.delete(b"lock").unwrap());
            let keys: Vec<(Vec<u8>, usize)> = kv.keys(b"", 100).unwrap().into_iter().map(|k| (k.key, k.size)).collect();
            assert_eq!(keys, [(b"k".to_vec(), 1), (b"rate".to_vec(), 1), (b"x".to_vec(), 5)]);
            assert_eq!(kv.keys(b"r", 100).unwrap().len(), 1);
            assert_eq!(kv.keys(b"", 2).unwrap().len(), 2);
            assert!(matches!(kv.keys(b"x", 1).unwrap()[0].ttl, Ttl::Millis(ms) if ms <= 60_000));
            kv.set_max_bytes(1 << 20);
            kv.sweep();
        }
        let (m, d) = (memory.stats().unwrap(), database.stats().unwrap());
        assert_eq!((m.keys, m.expiring, m.hits, m.misses, m.evictions), (d.keys, d.expiring, d.hits, d.misses, 0));
        assert_eq!((d.max_bytes, m.max_bytes), (0, 1 << 20));
        assert!(d.bytes > 0);
        assert_eq!(memory.clear().unwrap(), database.clear().unwrap());
        assert_eq!(database.stats().unwrap().keys, 0);
        assert!(tree_of(&engine).is_none(), "emptied: the tree is gone");
        drop((memory, database, engine));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(temp("same-memory"));
    }

    /// The names of the commands the docs of `Kv::command` list, and those
    /// `writes` knows: each has its place.
    #[test]
    fn every_command_names_its_keys() {
        let source = include_str!("../kv.rs");
        let doc = &source[source.find("/// Strings and counters:").unwrap()..source.find("pub fn command(").unwrap()];
        // The options of SET, in brackets, are not commands.
        let mut listed = String::new();
        let mut depth = 0;
        for c in doc.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ if depth == 0 => listed.push(c),
                _ => {}
            }
        }
        let mut names: Vec<&str> = listed.split('`').skip(1).step_by(2).collect();
        let writes_body = &source[source.find("pub fn writes(").unwrap()..];
        let writes_body = &writes_body[..writes_body.find("\n}\n").unwrap()];
        let written: Vec<&str> = writes_body.split('"').skip(1).step_by(2).collect();
        assert!(names.len() >= 34 && written.len() >= 21, "{names:?} {written:?}");
        for name in &written {
            assert!(writes(name), "{name}");
        }
        names.extend(&written);
        for name in names {
            assert!(name.chars().all(|c| c.is_ascii_uppercase()), "{name}");
            assert!(place(name).is_some(), "`{name}` has no place in `keys_of`");
        }
        assert_eq!(place("LPUSH"), None);
        let a = args(&["k1", "v1", "k2", "v2"]);
        assert!(matches!(keys_of("MSET", &a), Touch::Keys(k) if k == [&b"k1"[..], b"k2"]));
        assert!(matches!(keys_of("DEL", &a), Touch::Keys(k) if k.len() == 4));
        assert!(matches!(keys_of("GETEX", &a), Touch::Keys(k) if k == [&b"k1"[..]]));
        assert!(matches!(keys_of("GET", &[]), Touch::Keys(k) if k.is_empty()));
        assert!(matches!(keys_of("FLUSHALL", &a), Touch::Whole));
        assert!(matches!(keys_of("ECHO", &a), Touch::Keys(k) if k.is_empty()));
    }

    /// Five writes queued together are one commit; so is a transaction that
    /// looks at the whole store and empties it on the way.
    #[test]
    fn a_transaction_is_one_commit() {
        let dir = temp("one-commit");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        let before = lsn(&engine);
        let five: Vec<Vec<Vec<u8>>> = [
            &["SET", "a", "1"][..],
            &["INCR", "a"],
            &["MSET", "b", "1", "c", "2"],
            &["EXPIRE", "b", "60"],
            &["DEL", "c"],
        ]
        .iter()
        .map(|c| args(c))
        .collect();
        let replies = kv.transaction(&five, false).unwrap();
        assert_eq!(replies, [Reply::Ok, Reply::Int(2), Reply::Ok, Reply::Int(1), Reply::Int(1)]);
        assert_eq!(lsn(&engine), before + 1);
        let whole: Vec<Vec<Vec<u8>>> =
            [&["SET", "d", "1"][..], &["DBSIZE"], &["KEYS", "*"], &["FLUSHDB"], &["SET", "e", "1"], &["DBSIZE"]]
                .iter()
                .map(|c| args(c))
                .collect();
        let replies = kv.transaction(&whole, false).unwrap();
        assert_eq!(replies[1], Reply::Int(3));
        assert_eq!(replies[2], Reply::Array(vec![bulk("a"), bulk("b"), bulk("d")]));
        assert_eq!(replies[5], Reply::Int(1));
        assert_eq!(lsn(&engine), before + 2);
        let (keys, index, counts) = tree_of(&engine).unwrap();
        assert_eq!((keys, index, counts), (vec![(b"e".to_vec(), 0)], vec![], (1, 0)));
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reading writes nothing, expired keys included; neither does a write
    /// that changes nothing. A read never makes the tree.
    #[test]
    fn a_read_writes_nothing() {
        let dir = temp("read");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        let start = lsn(&engine);
        assert_eq!(kv.command(&args(&["GET", "k"]), false).unwrap(), Reply::Nil);
        assert_eq!(kv.stats().unwrap(), KvStats { misses: 1, ..KvStats::default() });
        assert_eq!(kv.command(&args(&["DEL", "k"]), false).unwrap(), Reply::Int(0));
        assert_eq!(lsn(&engine), start);
        assert!(tree_of(&engine).is_none(), "nothing written: no tree");
        kv.command(&args(&["SET", "short", "v", "PX", "30"]), false).unwrap();
        kv.command(&args(&["SET", "k", "v"]), false).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let written = lsn(&engine);
        let bytes = engine.store.pages.version().bytes(TREE);
        for command in [&["GET", "short"][..], &["TTL", "short"], &["KEYS", "*"], &["GET", "k"], &["DBSIZE"], &["INFO"]]
        {
            kv.command(&args(command), false).unwrap();
        }
        assert_eq!(kv.get_with_ttl(b"short").unwrap(), None);
        assert_eq!(kv.keys(b"", 10).unwrap().len(), 1);
        let stats = kv.stats().unwrap();
        assert_eq!((stats.keys, stats.expiring, stats.expired), (1, 0, 0));
        // A write that finds nothing to change: SET NX on a key that is there.
        assert_eq!(kv.command(&args(&["SET", "k", "w", "NX"]), false).unwrap(), Reply::Nil);
        assert_eq!(lsn(&engine), written);
        assert_eq!(engine.store.pages.version().bytes(TREE), bytes);
        assert_eq!(tree_of(&engine).unwrap().0.len(), 2, "the expired key is still in the tree");
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The counters and the index follow every change: after the script,
    /// they agree with a walk of the tree, and after FLUSHDB the tree is gone.
    #[test]
    fn the_counters_and_the_index_follow() {
        let dir = temp("counters");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        for command in SCRIPT.iter().filter(|c| !c.is_empty() && c[0] != "sleep" && c[0] != "FLUSHALL") {
            kv.command(&args(command), false).unwrap();
            let Some((keys, index, (count, expiring))) = tree_of(&engine) else { continue };
            let with_expiry: Vec<(u64, Vec<u8>)> =
                keys.iter().filter(|(_, at)| *at != 0).map(|(k, at)| (*at, k.clone())).collect();
            let mut sorted = with_expiry.clone();
            sorted.sort();
            assert_eq!(index, sorted, "one index entry per key that expires, after {command:?}");
            assert_eq!((count, expiring), (keys.len() as u64, with_expiry.len() as u64), "after {command:?}");
        }
        let stats = kv.stats().unwrap();
        let (keys, _, _) = tree_of(&engine).unwrap();
        assert_eq!(stats.keys, keys.len());
        assert_eq!(stats.bytes as u64, engine.store.pages.version().bytes(TREE));
        assert_eq!(kv.command(&args(&["FLUSHDB"]), false).unwrap(), Reply::Ok);
        assert!(tree_of(&engine).is_none());
        assert_eq!(kv.stats().unwrap().keys, 0);
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transaction that loads and writes more than 64 MiB in all lets go of
    /// no key: the scratch store has no limit.
    #[test]
    fn a_large_transaction_evicts_nothing() {
        let dir = temp("large");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        let value = String::from_utf8(vec![b'v'; 8 << 20]).unwrap();
        let names: Vec<String> = (0..9).map(|i| format!("big-{i}")).collect();
        let sets: Vec<Vec<Vec<u8>>> = names.iter().map(|n| args(&["SET", n, &value])).collect();
        assert!(kv.transaction(&sets, false).unwrap().iter().all(|r| *r == Reply::Ok));
        // Every value read and written again, and one more.
        let mut again: Vec<Vec<Vec<u8>>> = names.iter().map(|n| args(&["GET", n])).collect();
        again.extend(names.iter().map(|n| args(&["APPEND", n, "!"])));
        again.push(args(&["SET", "big-9", &value]));
        let replies = kv.transaction(&again, false).unwrap();
        assert!(replies[..9].iter().all(|r| matches!(r, Reply::Bulk(b) if b.len() == 8 << 20)));
        assert!(replies[9..18].iter().all(|r| *r == Reply::Int((8 << 20) + 1)));
        let (keys, _, (count, _)) = tree_of(&engine).unwrap();
        assert_eq!((keys.len(), count), (10, 10));
        for n in &names {
            assert_eq!(kv.command(&args(&["STRLEN", n]), false).unwrap(), Reply::Int((8 << 20) + 1));
        }
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A value larger than 64 MiB is refused as in memory, and nothing is
    /// written.
    #[test]
    fn a_value_too_large_is_refused_and_nothing_is_written() {
        let dir = temp("too-large");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        let refused = Reply::err(KvError::TooLarge);
        assert!(refused.clone() == Reply::Error(format!("ERR {}", KvError::TooLarge)));
        let huge = String::from_utf8(vec![b'x'; MAX_VALUE + 1]).unwrap();
        let start = lsn(&engine);
        assert_eq!(kv.set(b"k", huge.clone().into_bytes(), &SetOptions::default()).unwrap(), Err(KvError::TooLarge));
        for command in [
            &["SET", "k", &huge][..],
            &["SETNX", "k", &huge],
            &["GETSET", "k", &huge],
            &["SETEX", "k", "10", &huge],
            &["MSET", "a", "1", "k", &huge],
        ] {
            assert_eq!(kv.command(&args(command), false).unwrap(), refused, "{}", command[0]);
        }
        assert_eq!(lsn(&engine), start);
        // Grown past the limit: refused, the key keeps its value.
        let half = "y".repeat(MAX_VALUE / 2);
        kv.command(&args(&["SET", "grows", &half]), false).unwrap();
        assert_eq!(kv.command(&args(&["APPEND", "grows", &half]), false).unwrap(), Reply::Int(MAX_VALUE as i64));
        let written = lsn(&engine);
        let replies = kv.transaction(&[args(&["APPEND", "grows", "!"]), args(&["SET", "other", "1"])], false).unwrap();
        assert_eq!(replies, [refused.clone(), Reply::Ok]);
        assert_eq!(lsn(&engine), written + 1);
        assert_eq!(kv.command(&args(&["STRLEN", "grows"]), false).unwrap(), Reply::Int(MAX_VALUE as i64));
        assert_eq!(kv.command(&args(&["EXISTS", "k", "a"]), false).unwrap(), Reply::Int(0));
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A command that writes a key it did not name is refused, and nothing
    /// is written: the scratch holds only what was read.
    #[test]
    fn a_key_not_declared_is_refused() {
        let dir = temp("undeclared");
        let engine = open(&dir, false);
        let kv = DatabaseKv::new(engine.clone());
        kv.set(b"a", b"1".to_vec(), &SetOptions::default()).unwrap().unwrap();
        let start = lsn(&engine);
        let e = kv
            .write(&[b"a"], |run| {
                run.locked().set(b"a", b"2".to_vec(), &SetOptions::default()).unwrap();
                run.locked().set(b"b", b"blind".to_vec(), &SetOptions::default()).unwrap();
                Ok(())
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("a command touched a key it did not declare: nothing was written"), "{e}");
        assert_eq!(lsn(&engine), start);
        assert_eq!(kv.get_with_ttl(b"a").unwrap(), Some((b"1".to_vec(), Ttl::Forever)));
        assert_eq!(kv.get_with_ttl(b"b").unwrap(), None);
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An expired key is missing at once, without a write; the sweep then
    /// removes it and its index entry, and counts it.
    #[test]
    fn an_expired_key_is_gone_before_it_is_swept_and_swept_after() {
        let dir = temp("sweep");
        let engine = open(&dir, false);
        let kv = Arc::new(DatabaseKv::new(engine.clone()));
        kv.command(&args(&["SET", "short", "v", "PX", "40"]), false).unwrap();
        kv.command(&args(&["SET", "later", "v", "EX", "60"]), false).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let before = lsn(&engine);
        assert_eq!(kv.get_with_ttl(b"short").unwrap(), None);
        assert_eq!(kv.command(&args(&["EXISTS", "short"]), false).unwrap(), Reply::Int(0));
        assert_eq!(kv.command(&args(&["KEYS", "*"]), false).unwrap(), Reply::Array(vec![bulk("later")]));
        let stats = kv.stats().unwrap();
        assert_eq!((stats.keys, stats.expiring, stats.expired), (1, 1, 0));
        assert_eq!(lsn(&engine), before);
        assert!(tree_of(&engine).unwrap().0.iter().any(|(k, _)| k == b"short"), "not swept yet");
        assert_eq!(kv.sweep_now().unwrap(), (1, false));
        assert_eq!(lsn(&engine), before + 1);
        let (keys, index, counts) = tree_of(&engine).unwrap();
        assert_eq!(keys.iter().map(|(k, _)| k.as_slice()).collect::<Vec<_>>(), [&b"later"[..]]);
        assert_eq!(index.iter().map(|(_, k)| k.as_slice()).collect::<Vec<_>>(), [&b"later"[..]]);
        assert_eq!(counts, (1, 1));
        let stats = kv.stats().unwrap();
        assert_eq!((stats.keys, stats.expiring, stats.expired), (1, 1, 1));
        // Nothing due: a pass writes nothing.
        assert_eq!(kv.sweep_now().unwrap(), (0, false));
        kv.sweep_soon();
        assert_eq!(lsn(&engine), before + 1);

        // An index entry left behind (the store never leaves one: as damage
        // would): the key it names expires later now. The pass removes the
        // entry alone, and the key stays.
        kv.command(&args(&["SET", "moved", "v", "PX", "40"]), false).unwrap();
        {
            let mut w = engine.store.writer();
            let at = tree_of(&engine).unwrap().0.iter().find(|(k, _)| k == b"moved").unwrap().1;
            let ops = vec![
                Op::Put { table: TREE.into(), key: entry_key(b"moved"), value: entry_value(at + 60_000, b"v") },
                Op::Put { table: TREE.into(), key: index_key(at + 60_000, b"moved"), value: Vec::new() },
            ];
            let lsn = engine.store.log_and_apply(&mut w, ops).unwrap();
            drop(w);
            engine.store.wait_durable(lsn).unwrap();
        }
        std::thread::sleep(Duration::from_millis(60));
        let before = lsn(&engine);
        assert_eq!(kv.sweep_now().unwrap(), (0, false));
        assert_eq!(lsn(&engine), before + 1);
        let (keys, index, counts) = tree_of(&engine).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(index.iter().map(|(_, k)| k.as_slice()).collect::<Vec<_>>(), [&b"later"[..], b"moved"]);
        assert_eq!(counts, (2, 2));
        assert_eq!(kv.get_with_ttl(b"moved").unwrap().map(|(v, _)| v), Some(b"v".to_vec()));
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The keys are on disk: a database opened again has them, with the
    /// same expiry dates; one whose time came while it was closed is gone.
    #[test]
    fn keys_survive_a_restart_and_expire_on_the_clock() {
        let dir = temp("restart");
        let ttl = {
            let engine = open(&dir, true);
            let kv = AppKv::open(&engine, 0);
            kv.command(&args(&["SET", "kept", "v"]), false).unwrap();
            kv.command(&args(&["SET", "dated", "v", "EX", "60"]), false).unwrap();
            kv.command(&args(&["SET", "short", "v", "PX", "50"]), false).unwrap();
            kv.command(&args(&["INCRBY", "n", "41"]), false).unwrap();
            let Some((_, Ttl::Millis(ttl))) = kv.get_with_ttl(b"dated").unwrap() else { panic!("an expiry") };
            ttl
        };
        std::thread::sleep(Duration::from_millis(80));
        let engine = open(&dir, true);
        let kv = AppKv::open(&engine, 0);
        assert_eq!(kv.command(&args(&["GET", "kept"]), false).unwrap(), bulk("v"));
        assert_eq!(kv.command(&args(&["INCR", "n"]), false).unwrap(), Reply::Int(42));
        let Some((_, Ttl::Millis(left))) = kv.get_with_ttl(b"dated").unwrap() else { panic!("an expiry") };
        assert!(left <= ttl - 80 && left > 50_000, "{left} after {ttl}");
        assert_eq!(kv.get_with_ttl(b"short").unwrap(), None);
        assert_eq!(kv.stats().unwrap().keys, 3);
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No table listing, statement or branch sees the keys, and the page
    /// store checks out.
    #[test]
    fn tables_and_branches_do_not_see_the_keys() {
        let dir = temp("unseen");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        for i in 0..200 {
            kv.command(&args(&["SET", &format!("key:{i}"), &"v".repeat(i), "EX", "60"]), false).unwrap();
        }
        let db = Embedded::from_engine(engine.clone());
        assert!(db.tables().unwrap().is_empty());
        assert!(db.sql("SELECT * FROM kv", &[]).is_err());
        let branch = engine.create_branch("try", None).unwrap();
        assert_eq!(branch.own_bytes, 0);
        assert!(db.on_branch("try").unwrap().tables().unwrap().is_empty());
        kv.command(&args(&["SET", "key:1", "changed"]), false).unwrap();
        assert_eq!(engine.branches()[0].own_bytes, 0, "a key written is not copied into the branch");
        engine.checkpoint().unwrap();
        assert!(engine.check_integrity().unwrap());
        drop((kv, db, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A write the disk refuses is an error, and what it wrote is never
    /// shown: a read after it does not see it.
    #[test]
    fn a_write_the_disk_refuses_is_an_error() {
        let dir = temp("refused");
        let engine = open(&dir, true);
        let kv = AppKv::open(&engine, 0);
        kv.command(&args(&["SET", "kept", "v"]), false).unwrap();
        engine.store.fail_next_sync();
        let e = kv.command(&args(&["SET", "lost", "v"]), false).unwrap_err().to_string();
        assert!(e.contains("write-ahead log"), "{e}");
        assert_eq!(kv.get_with_ttl(b"lost").unwrap(), None);
        assert_eq!(kv.command(&args(&["GET", "lost"]), false).unwrap(), Reply::Nil);
        assert_eq!(kv.command(&args(&["GET", "kept"]), false).unwrap(), bulk("v"));
        // A write that would change nothing is not answered from what the disk refused.
        assert!(kv.command(&args(&["SET", "lost", "again", "NX"]), false).is_err());
        assert!(kv.incr_by(b"n", 1, None).is_err());
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Counters written from many threads at once: every increment counts,
    /// and each one sees its own value.
    #[test]
    fn concurrent_increments_are_atomic() {
        let dir = temp("incr");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        let mut seen: Vec<i64> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        (0..50)
                            .map(|i| match i % 2 {
                                0 => kv.incr_by(b"n", 1, None).unwrap().unwrap(),
                                _ => match kv.command(&args(&["INCR", "n"]), false).unwrap() {
                                    Reply::Int(n) => n,
                                    other => panic!("{other:?}"),
                                },
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
        });
        seen.sort();
        assert_eq!(seen, (1..=400).collect::<Vec<_>>());
        assert_eq!(kv.command(&args(&["GET", "n"]), false).unwrap(), bulk("400"));
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The keys of a `SCAN` answer, and its cursor.
    fn scanned(reply: Reply) -> (String, Vec<String>) {
        let text = |r: &Reply| match r {
            Reply::Bulk(b) => String::from_utf8(b.clone()).unwrap(),
            other => panic!("{other:?}"),
        };
        match reply {
            Reply::Array(parts) => match &parts[..] {
                [cursor, Reply::Array(keys)] => (text(cursor), keys.iter().map(text).collect()),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// `MSET` of `keys`, each with the value `v`.
    fn mset(kv: &DatabaseKv, keys: impl Iterator<Item = String>) {
        let mut command = vec![b"MSET".to_vec()];
        for key in keys {
            command.push(key.into_bytes());
            command.push(b"v".to_vec());
        }
        kv.command(&command, false).unwrap();
    }

    /// `SCAN` answers page by page: `COUNT` entries of the tree at most (10
    /// without one), and the cursor of the next page, `0` after the last. A
    /// whole scan returns every key once, even when they are deleted as it
    /// goes; a page asked again comes again; a cursor the store does not
    /// know is an error. In a transaction, the keys it wrote are in the
    /// page they fall in.
    #[test]
    fn scan_answers_page_by_page() {
        let dir = temp("scan");
        let engine = open(&dir, false);
        let kv = DatabaseKv::new(engine.clone());
        mset(&kv, (0..25).map(|n| format!("key:{n:02}")).chain(["other".to_owned()]));
        let page = |cursor: &str, options: &[&str]| {
            scanned(kv.command(&args(&[&["SCAN", cursor], options].concat()), false).unwrap())
        };
        let (cursor, keys) = page("0", &[]);
        assert_eq!(keys, (0..10).map(|n| format!("key:{n:02}")).collect::<Vec<_>>());
        assert_ne!(cursor, "0");
        assert_eq!(page(&cursor, &[]).1, page(&cursor, &[]).1, "the same page again");
        let whole = |options: &[&str], delete: bool| {
            let (mut cursor, mut all, mut pages) = ("0".to_owned(), Vec::new(), 0);
            loop {
                let (next, keys) = page(&cursor, options);
                pages += 1;
                if delete {
                    for key in &keys {
                        assert_eq!(kv.command(&args(&["DEL", key]), false).unwrap(), Reply::Int(1));
                    }
                }
                all.extend(keys);
                if next == "0" {
                    return (all, pages);
                }
                cursor = next;
            }
        };
        let expected: Vec<String> = (0..25).map(|n| format!("key:{n:02}")).collect();
        assert_eq!(whole(&["MATCH", "key:*", "COUNT", "7"], false), (expected.clone(), 4));
        assert_eq!(whole(&["COUNT", "100"], false).1, 1);
        // `for key in scan_iter("key:*"): delete(key)`: every key goes.
        assert_eq!(whole(&["MATCH", "key:*"], true).0, expected);
        assert_eq!(kv.command(&args(&["KEYS", "*"]), false).unwrap(), Reply::Array(vec![bulk("other")]));
        let replies =
            kv.transaction(&[args(&["SET", "key:new", "v"]), args(&["SCAN", "0", "COUNT", "100"])], false).unwrap();
        assert_eq!(scanned(replies[1].clone()), ("0".to_owned(), vec!["key:new".to_owned(), "other".to_owned()]));
        // Refused.
        let refused = |command: &[&str]| kv.command(&args(command), false).unwrap();
        assert_eq!(refused(&["SCAN", "123456"]), Reply::err(UNKNOWN_CURSOR));
        assert_eq!(refused(&["SCAN", "x"]), Reply::err("invalid cursor"));
        assert_eq!(refused(&["SCAN", "0", "COUNT", "0"]), Reply::err("syntax error"));
        assert_eq!(refused(&["SCAN", "0", "COUNT", "x"]), Reply::err("value is not an integer or out of range"));
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A client that pauses between two pages of its scan keeps its cursor
    /// while other clients scan many more pages (more than the 1,024 cursors
    /// an earlier version kept for all of them): it finishes with every key.
    /// The cursors stay bounded: past their time they go, and past their
    /// bytes the oldest go.
    #[test]
    fn a_cursor_stays_while_other_clients_scan() {
        let dir = temp("scan-others");
        let engine = open(&dir, false);
        let kv = DatabaseKv::new(engine.clone());
        mset(&kv, (0..3000).map(|n| format!("job:{n:05}")));
        let page = |cursor: &str, options: &[&str]| {
            scanned(kv.command(&args(&[&["SCAN", cursor], options].concat()), false).unwrap())
        };
        let (mut cursor, mut mine) = page("0", &["MATCH", "job:*"]);
        // Meanwhile, other clients go through 1,500 pages.
        let mut given = 0;
        for _ in 0..5 {
            let mut other = "0".to_owned();
            loop {
                let (next, _) = page(&other, &["COUNT", "10"]);
                if next == "0" {
                    break;
                }
                given += 1;
                other = next;
            }
        }
        assert!(given > 1024, "{given}");
        while cursor != "0" {
            let (next, keys) = page(&cursor, &["MATCH", "job:*"]);
            mine.extend(keys);
            cursor = next;
        }
        assert_eq!(mine, (0..3000).map(|n| format!("job:{n:05}")).collect::<Vec<_>>());

        // Bounded: those past their time go, and the oldest past the bytes.
        let mut cursors = Cursors::new(1_000);
        let first = cursors.give(b"a".to_vec(), 1_000);
        let second = cursors.give(b"b".to_vec(), 1_000 + CURSOR_KEEP_MS);
        assert_eq!((cursors.find(first), cursors.find(second)), (Some(b"a".to_vec()), Some(b"b".to_vec())));
        cursors.give(b"c".to_vec(), 2_000 + CURSOR_KEEP_MS);
        assert_eq!(cursors.find(first), None, "past its time");
        assert_eq!(cursors.find(second), Some(b"b".to_vec()));
        let big = vec![b'k'; 1000];
        for _ in 0..(CURSORS_BYTES / 1000 + 10) {
            cursors.give(big.clone(), 2_000 + CURSOR_KEEP_MS);
        }
        assert!(cursors.bytes <= CURSORS_BYTES && cursors.find(second).is_none(), "{}", cursors.bytes);
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cursor given before the store was opened again (a restart, an
    /// application deleted and made again) is refused, never taken for a
    /// page another client was given since: a client that sends it again
    /// starts again from `SCAN 0` instead of missing keys. Cursors stay below
    /// 2^53, what a JavaScript client reads exactly.
    #[test]
    fn a_cursor_of_an_earlier_opening_is_refused() {
        let dir = temp("scan-restart");
        let engine = open(&dir, false);
        let kv = DatabaseKv::new(engine.clone());
        mset(&kv, (0..100).map(|n| format!("job:{n:03}")));
        let page = |kv: &DatabaseKv, cursor: &str, options: &[&str]| {
            kv.command(&args(&[&["SCAN", cursor], options].concat()), false).unwrap()
        };
        let mut cursor = "0".to_owned();
        for _ in 0..3 {
            cursor = scanned(page(&kv, &cursor, &["MATCH", "job:*"])).0;
        }
        assert!(cursor.parse::<u64>().unwrap() < 1 << 53, "{cursor}");
        drop(kv);
        std::thread::sleep(Duration::from_millis(5));
        // Opened again; another client scans first.
        let again = DatabaseKv::new(engine.clone());
        let mut other = "0".to_owned();
        for _ in 0..5 {
            other = scanned(page(&again, &other, &[])).0;
        }
        assert_ne!(other, cursor);
        assert_eq!(page(&again, &cursor, &["MATCH", "job:*"]), Reply::err(UNKNOWN_CURSOR));
        drop((again, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `KEYS` does not build a list of more than [`KEYS_MAX`] keys: past it,
    /// it says to use `SCAN`, and a `SCAN` with a larger `COUNT` gives a page
    /// of that size at most.
    #[test]
    fn keys_does_not_list_a_whole_large_store() {
        let dir = temp("keys-max");
        let engine = open(&dir, false);
        let kv = DatabaseKv::new(engine.clone());
        mset(&kv, (0..=KEYS_MAX).map(|n| format!("key:{n:06}")));
        match kv.command(&args(&["KEYS", "*"]), false).unwrap() {
            Reply::Error(e) => assert!(e.contains("too many keys for KEYS") && e.contains("use SCAN"), "{e}"),
            other => panic!("{other:?}"),
        }
        match kv.command(&args(&["KEYS", "key:00001?"]), false).unwrap() {
            Reply::Array(keys) => assert_eq!(keys.len(), 10),
            other => panic!("{other:?}"),
        }
        let (cursor, keys) = scanned(kv.command(&args(&["SCAN", "0", "COUNT", "1000000000"]), false).unwrap());
        assert_eq!((cursor == "0", keys.len()), (false, KEYS_MAX));
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// More keys expire than one pass removes: the next pass follows at once,
    /// not at the next call (the server's next tick) nor 30 seconds later, so
    /// the removals keep up whatever the rate of expiries.
    #[test]
    fn the_sweep_keeps_up_with_many_expiries() {
        let dir = temp("sweep-more");
        let engine = open(&dir, false);
        let kv = Arc::new(DatabaseKv::new(engine.clone()));
        let n = SWEEP_ROUNDS * SWEEP_BATCH + 5000;
        {
            // Expired a second ago.
            let at = now_ms() - 1000;
            let mut ops =
                vec![Op::Put { table: TREE.into(), key: COUNTS.to_vec(), value: counts_value(n as u64, n as u64) }];
            for i in 0..n {
                let key = format!("k{i:06}").into_bytes();
                ops.push(Op::Put { table: TREE.into(), key: entry_key(&key), value: entry_value(at, b"v") });
                ops.push(Op::Put { table: TREE.into(), key: index_key(at, &key), value: Vec::new() });
            }
            let mut w = engine.store.writer();
            let lsn = engine.store.log_and_apply(&mut w, ops).unwrap();
            drop(w);
            engine.store.wait_durable(lsn).unwrap();
        }
        assert_eq!(kv.stats().unwrap().keys, 0, "expired: not counted");
        kv.sweep_soon();
        let (keys, index, counts) = tree_of(&engine).unwrap();
        assert_eq!((keys.len(), index.len(), counts), (0, 0, (0, 0)), "one pass, then the next one at once");
        assert_eq!(kv.stats().unwrap().expired, n as u64);
        // Nothing more due: the next pass waits its 30 seconds.
        let before = lsn(&engine);
        kv.command(&args(&["SET", "short", "v", "PX", "1"]), false).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        kv.sweep_soon();
        kv.sweep_soon();
        assert_eq!(lsn(&engine), before + 1, "only the SET was written");
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Once the database stops taking commits (a server that stops), a write
    /// is the storage error and changes nothing; reads go on.
    #[test]
    fn a_write_after_the_stop_is_refused() {
        let dir = temp("stopped");
        let engine = open(&dir, false);
        let kv = AppKv::open(&engine, 0);
        kv.command(&args(&["SET", "kept", "v"]), false).unwrap();
        engine.stop_writes();
        for e in [
            kv.command(&args(&["SET", "lost", "v"]), false).unwrap_err(),
            kv.incr_by(b"n", 1, None).unwrap_err(),
            kv.set(b"lost", b"v".to_vec(), &SetOptions::default()).unwrap_err(),
        ] {
            assert!(e.to_string().contains("the database is stopping: nothing was written"), "{e}");
        }
        assert_eq!(kv.command(&args(&["GET", "lost"]), false).unwrap(), Reply::Nil);
        assert_eq!(kv.command(&args(&["GET", "kept"]), false).unwrap(), bulk("v"));
        drop((kv, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
