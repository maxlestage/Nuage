//! A key-value store in memory: values with an expiry,
//! counters, locks (`SET key value NX EX 30`), a cache in front of anything
//! slow. Each application of a server has its own, next to its database.
//!
//! It keeps strings (any bytes) under keys, in memory only: nothing touches
//! the disk, so a read or a write costs well under a microsecond, and a
//! restart empties it. What must survive belongs in the database.
//!
//! A server can keep each application's keys in that application's database
//! instead (`NUAGE_KV_STORAGE=database`): the same commands give the same
//! answers, run by the same code, and the keys survive restarts and are in
//! the backups; a write then waits for the disk, like a row.
//!
//! Memory is bounded ([`Kv::new`]): past the limit, the keys that expired go
//! first, then the least recently used ones.
//!
//! ```
//! use std::time::Duration;
//! use nuage::kv::{Kv, SetOptions};
//!
//! let kv = Kv::new(16 << 20);
//! kv.set(b"session:42", b"ada".to_vec(), &SetOptions::expiring(Duration::from_secs(1800))).unwrap();
//! assert_eq!(kv.get(b"session:42").as_deref(), Some(&b"ada"[..]));
//! assert_eq!(kv.incr_by(b"visits", 1, None).unwrap(), 1);
//! ```
//!
//! [`Kv::command`] runs commands by name (`GET`, `SET`, `DEL`, `INCR`,
//! `EXPIRE`, `TTL`, `MGET`, `KEYS`...), the names and the answers Redis
//! clients know: the server answers them over HTTP (`/v1/kv`) and on the
//! key-value port, where such a client connects as it is.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The store in an application's database (`NUAGE_KV_STORAGE=database`).
#[cfg(feature = "server")]
pub(crate) mod database;

/// Longest key, in bytes.
pub const MAX_KEY: usize = 1024;

/// Most bytes the answer of one command may hold (`MGET` of many keys), and
/// the answers of one transaction together: values are copied into the
/// answer under the store's lock, and a request of a few bytes could
/// otherwise name the same large value a million times. One value alone may
/// be larger.
pub const MAX_ANSWER: usize = 16 << 20;

/// What an entry weighs besides its key and its value: the maps' own nodes.
const ENTRY_OVERHEAD: usize = 128;

type Key = Arc<[u8]>;

struct Entry {
    value: Vec<u8>,
    /// Milliseconds since the store was made.
    expires: Option<u64>,
    /// When it was last used: its place in the eviction order.
    used: u64,
}

#[derive(Default)]
struct Inner {
    map: HashMap<Key, Entry>,
    /// Last use → key: the first is the least recently used.
    order: BTreeMap<u64, Key>,
    /// Keys that expire, by deadline.
    deadlines: BTreeSet<(u64, Key)>,
    tick: u64,
    bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    expired: u64,
}

fn weight(key: &[u8], value: &[u8]) -> usize {
    key.len() + value.len() + ENTRY_OVERHEAD
}

impl Inner {
    fn remove(&mut self, key: &[u8]) -> Option<Entry> {
        let (key, entry) = self.map.remove_entry(key)?;
        self.order.remove(&entry.used);
        if let Some(at) = entry.expires {
            self.deadlines.remove(&(at, key.clone()));
        }
        self.bytes -= weight(&key, &entry.value);
        Some(entry)
    }

    /// Drops the keys whose time has come.
    fn sweep(&mut self, now: u64) -> usize {
        let mut n = 0;
        while let Some((at, key)) = self.deadlines.first().cloned() {
            if at > now {
                break;
            }
            self.remove(&key);
            self.expired += 1;
            n += 1;
        }
        n
    }

    /// The entry of `key`, if it is still alive.
    fn alive(&mut self, key: &[u8], now: u64) -> bool {
        match self.map.get(key) {
            Some(e) if e.expires.is_some_and(|at| at <= now) => {
                self.remove(key);
                self.expired += 1;
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Reads `key`, which becomes the most recently used.
    fn read(&mut self, key: &[u8], now: u64) -> Option<&Entry> {
        if !self.alive(key, now) {
            self.misses += 1;
            return None;
        }
        self.hits += 1;
        self.tick += 1;
        let tick = self.tick;
        let (key, entry) = self.map.get_key_value(key)?;
        let key = key.clone();
        let old = entry.used;
        self.order.remove(&old);
        self.order.insert(tick, key.clone());
        let entry = self.map.get_mut(&key)?;
        entry.used = tick;
        Some(entry)
    }

    fn write(&mut self, key: &[u8], value: Vec<u8>, expires: Option<u64>) {
        self.remove(key);
        self.tick += 1;
        let key: Key = Arc::from(key);
        self.bytes += weight(&key, &value);
        self.order.insert(self.tick, key.clone());
        if let Some(at) = expires {
            self.deadlines.insert((at, key.clone()));
        }
        self.map.insert(key, Entry { value, expires, used: self.tick });
    }

    fn set_expiry(&mut self, key: &[u8], expires: Option<u64>) {
        let Some((key, entry)) = self.map.get_key_value(key) else { return };
        let (key, old) = (key.clone(), entry.expires);
        if let Some(at) = old {
            self.deadlines.remove(&(at, key.clone()));
        }
        if let Some(at) = expires {
            self.deadlines.insert((at, key.clone()));
        }
        if let Some(entry) = self.map.get_mut(&key) {
            entry.expires = expires;
        }
    }

    /// Makes room: the expired keys, then the least recently used ones.
    fn fit(&mut self, max: usize, now: u64) {
        if self.bytes <= max {
            return;
        }
        self.sweep(now);
        while self.bytes > max {
            let Some((_, key)) = self.order.first_key_value().map(|(t, k)| (*t, k.clone())) else { break };
            self.remove(&key);
            self.evictions += 1;
        }
    }
}

/// How [`Kv::set`] writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetOptions {
    /// The key expires after this long.
    pub ttl: Option<Duration>,
    /// Keeps the expiry the key already has (`KEEPTTL`).
    pub keep_ttl: bool,
    /// Writes only when the key does not exist (`NX`): a lock.
    pub only_if_absent: bool,
    /// Writes only when the key exists (`XX`).
    pub only_if_present: bool,
}

impl SetOptions {
    /// A value that expires after `ttl`.
    pub fn expiring(ttl: Duration) -> SetOptions {
        SetOptions { ttl: Some(ttl), ..SetOptions::default() }
    }
}

/// How long a key still lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// No such key.
    Missing,
    /// The key never expires.
    Forever,
    /// It expires in this many milliseconds.
    Millis(u64),
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvError {
    /// The value is not a whole number (`INCR` on a text).
    NotInteger,
    /// The counter would leave the 64-bit integers.
    Overflow,
    /// The key and its value are larger than the whole store (in memory), or
    /// the value is larger than 64 MiB (in an application's database).
    TooLarge,
    /// The key is longer than [`MAX_KEY`], or empty.
    BadKey,
}

impl std::fmt::Display for KvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            KvError::NotInteger => "value is not an integer or out of range",
            KvError::Overflow => "increment or decrement would overflow",
            KvError::TooLarge => {
                "the value is larger than the key-value store takes (NUAGE_KV_MB in memory, 64 MiB in the database)"
            }
            KvError::BadKey => "a key is 1 to 1024 bytes",
        })
    }
}

impl std::error::Error for KvError {}

/// What the store holds, and how it is used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KvStats {
    /// Keys alive.
    pub keys: usize,
    /// Keys that will expire.
    pub expiring: usize,
    /// Memory they take, in bytes.
    pub bytes: usize,
    /// The limit.
    pub max_bytes: usize,
    /// Reads that found their key.
    pub hits: u64,
    /// Reads that did not.
    pub misses: u64,
    /// Keys dropped to make room.
    pub evictions: u64,
    /// Keys dropped because their time had come.
    pub expired: u64,
}

/// A key of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// The key.
    pub key: Vec<u8>,
    /// The size of its value, in bytes.
    pub size: usize,
    /// How long it still lives.
    pub ttl: Ttl,
}

/// A key-value store in memory. See the [module](self) docs.
pub struct Kv {
    start: Instant,
    /// It changes when a server shares its key-value memory among more or
    /// fewer applications.
    max_bytes: std::sync::atomic::AtomicUsize,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Kv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        f.debug_struct("Kv").field("keys", &stats.keys).field("bytes", &stats.bytes).finish_non_exhaustive()
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl Kv {
    /// A store that keeps `max_bytes` of keys and values at most (64 KiB at
    /// least).
    pub fn new(max_bytes: usize) -> Kv {
        Kv {
            start: Instant::now(),
            max_bytes: std::sync::atomic::AtomicUsize::new(max_bytes.max(64 << 10)),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Changes how much the store keeps at most (64 KiB at least); a smaller
    /// limit lets go at once of what is over it: the keys that expired, then
    /// the least recently used.
    pub fn set_max_bytes(&self, max_bytes: usize) {
        let max = max_bytes.max(64 << 10);
        self.max_bytes.store(max, std::sync::atomic::Ordering::Relaxed);
        let now = self.now();
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).fit(max, now);
    }

    fn now(&self) -> u64 {
        millis(self.start.elapsed())
    }

    fn locked<T>(&self, f: impl FnOnce(&mut Locked<'_>) -> T) -> T {
        let now = self.now();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let max_bytes = self.max_bytes.load(std::sync::atomic::Ordering::Relaxed);
        f(&mut Locked { max_bytes, inner: &mut inner, now })
    }

    /// The value of `key`.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.locked(|l| l.get(key))
    }

    /// The value of `key` and how long it still lives.
    pub fn get_with_ttl(&self, key: &[u8]) -> Option<(Vec<u8>, Ttl)> {
        self.locked(|l| l.get_with_ttl(key))
    }

    /// Writes `value` under `key`; `false` when `options` held it back
    /// (`only_if_absent` on a key that exists, `only_if_present` on one that
    /// does not).
    pub fn set(&self, key: &[u8], value: Vec<u8>, options: &SetOptions) -> Result<bool, KvError> {
        self.locked(|l| l.set(key, value, options))
    }

    /// Deletes `key`; `true` when it existed.
    pub fn delete(&self, key: &[u8]) -> bool {
        self.locked(|l| l.delete(key))
    }

    /// `key` exists.
    pub fn exists(&self, key: &[u8]) -> bool {
        self.locked(|l| l.exists(key))
    }

    /// Adds `by` to the counter `key` (0 when it does not exist) and returns
    /// it. `ttl_if_new` sets the expiry of a counter this call creates: a
    /// rate limit is `incr_by(key, 1, Some(window))`, in one step.
    pub fn incr_by(&self, key: &[u8], by: i64, ttl_if_new: Option<Duration>) -> Result<i64, KvError> {
        self.locked(|l| l.incr_by(key, by, ttl_if_new))
    }

    /// Makes `key` expire after `ttl` (`None`: never); `false` when it does
    /// not exist.
    pub fn expire(&self, key: &[u8], ttl: Option<Duration>) -> bool {
        self.locked(|l| l.expire(key, ttl))
    }

    /// How long `key` still lives.
    pub fn ttl(&self, key: &[u8]) -> Ttl {
        self.locked(|l| l.ttl(key))
    }

    /// The keys that start with `prefix`, sorted, `limit` at most.
    pub fn keys(&self, prefix: &[u8], limit: usize) -> Vec<KeyInfo> {
        self.locked(|l| l.keys(prefix, limit))
    }

    /// Deletes every key; returns how many there were.
    pub fn clear(&self) -> usize {
        self.locked(|l| l.clear())
    }

    /// Drops the keys whose time has come (the server does it every second;
    /// a key is also checked each time it is read).
    pub fn sweep(&self) -> usize {
        self.locked(|l| l.sweep())
    }

    /// What the store holds.
    pub fn stats(&self) -> KvStats {
        self.locked(|l| l.stats())
    }

    /// Runs one command: `args[0]` is its name, in any case. A
    /// command runs alone: nothing else reads or writes while it does. With
    /// `read_only`, the commands that write are refused.
    ///
    /// Strings and counters: `GET`, `SET` (`EX`, `PX`, `NX`, `XX`,
    /// `KEEPTTL`, `GET`), `SETNX`, `SETEX`, `PSETEX`, `GETSET`, `GETDEL`,
    /// `GETEX`, `MGET`, `MSET`, `MSETNX`, `APPEND`, `STRLEN`, `INCR`,
    /// `DECR`, `INCRBY`, `DECRBY`. Keys: `DEL`, `UNLINK`, `EXISTS`,
    /// `EXPIRE`, `PEXPIRE`, `TTL`, `PTTL`, `PERSIST`, `TYPE`, `KEYS`,
    /// `SCAN`, `DBSIZE`, `FLUSHDB`, `FLUSHALL`. And `PING`, `ECHO`, `INFO`.
    pub fn command(&self, args: &[Vec<u8>], read_only: bool) -> Reply {
        self.locked(|l| l.command(args, read_only))
    }

    /// Runs several commands as one (`MULTI` ... `EXEC`): nothing else reads
    /// or writes between them. Each one has its answer; one that is refused
    /// does not undo the others. Once the answers pass [`MAX_ANSWER`], the
    /// commands left are not run, and answer an error.
    pub fn transaction(&self, commands: &[Vec<Vec<u8>>], read_only: bool) -> Vec<Reply> {
        self.locked(|l| {
            let mut size = 0usize;
            commands
                .iter()
                .map(|c| {
                    if size > MAX_ANSWER {
                        return Reply::err(format!(
                            "the answers of this transaction passed {} MiB: the commands after were not run",
                            MAX_ANSWER >> 20
                        ));
                    }
                    let reply = l.command(c, read_only);
                    size = size.saturating_add(reply.weight());
                    reply
                })
                .collect()
        })
    }
}

// ---------------------------------------------------------------------------
// Commands by name
// ---------------------------------------------------------------------------

/// The answer to a command, in the shapes of RESP (the wire of the
/// key-value port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `OK`.
    Ok,
    /// A short status (`PONG`, `string`...).
    Status(String),
    /// Nothing: no such key.
    Nil,
    /// A whole number.
    Int(i64),
    /// A value.
    Bulk(Vec<u8>),
    /// Several answers.
    Array(Vec<Reply>),
    /// Names and their values (the answer to `HELLO`).
    Map(Vec<(Reply, Reply)>),
    /// A refusal (`ERR ...`, `READONLY ...`).
    Error(String),
}

impl Reply {
    fn err(message: impl std::fmt::Display) -> Reply {
        Reply::Error(format!("ERR {message}"))
    }

    /// About what the answer weighs, on the wire or as JSON.
    pub(crate) fn weight(&self) -> usize {
        16 + match self {
            Reply::Ok | Reply::Nil | Reply::Int(_) => 0,
            Reply::Status(s) | Reply::Error(s) => s.len(),
            Reply::Bulk(b) => b.len(),
            Reply::Array(items) => items.iter().map(Reply::weight).sum(),
            Reply::Map(pairs) => pairs.iter().map(|(k, v)| k.weight() + v.weight()).sum(),
        }
    }

    /// The answer on the wire: RESP2, or RESP3 (`HELLO 3`), where
    /// "nothing" and maps have their own shapes.
    pub fn write_resp(&self, out: &mut Vec<u8>, resp3: bool) {
        match self {
            Reply::Ok => out.extend_from_slice(b"+OK\r\n"),
            Reply::Status(s) => {
                out.push(b'+');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Reply::Nil if resp3 => out.extend_from_slice(b"_\r\n"),
            Reply::Nil => out.extend_from_slice(b"$-1\r\n"),
            Reply::Int(n) => out.extend_from_slice(format!(":{n}\r\n").as_bytes()),
            Reply::Bulk(b) => {
                out.extend_from_slice(format!("${}\r\n", b.len()).as_bytes());
                out.extend_from_slice(b);
                out.extend_from_slice(b"\r\n");
            }
            Reply::Array(items) => {
                out.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
                for item in items {
                    item.write_resp(out, resp3);
                }
            }
            Reply::Map(pairs) => {
                let header = if resp3 { format!("%{}\r\n", pairs.len()) } else { format!("*{}\r\n", pairs.len() * 2) };
                out.extend_from_slice(header.as_bytes());
                for (name, value) in pairs {
                    name.write_resp(out, resp3);
                    value.write_resp(out, resp3);
                }
            }
            Reply::Error(e) => {
                out.push(b'-');
                out.extend_from_slice(e.replace(['\r', '\n'], " ").as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }
    }
}

/// `text` matches the glob `pattern` of `KEYS`: `*`, `?`, `[abc]`, `[a-z]`,
/// `[^a]`, and `\` before a character taken as it is.
pub fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        let step = match pattern.get(p) {
            Some(b'*') => {
                star = Some(p);
                mark = t;
                p += 1;
                continue;
            }
            Some(b'?') => Some(p + 1),
            Some(b'[') => class(pattern, p, text[t]),
            Some(b'\\') if p + 1 < pattern.len() => (pattern[p + 1] == text[t]).then_some(p + 2),
            Some(&c) => (c == text[t]).then_some(p + 1),
            None => None,
        };
        match (step, star) {
            (Some(next), _) => {
                p = next;
                t += 1;
            }
            (None, Some(s)) => {
                p = s + 1;
                mark += 1;
                t = mark;
            }
            (None, None) => return false,
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// A `[...]` class at `pattern[at]`: where the pattern goes on when `c` is in it.
fn class(pattern: &[u8], at: usize, c: u8) -> Option<usize> {
    let mut i = at + 1;
    let negated = matches!(pattern.get(i), Some(b'^' | b'!'));
    if negated {
        i += 1;
    }
    let mut found = false;
    let start = i;
    while i < pattern.len() && (pattern[i] != b']' || i == start) {
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' && pattern[i + 2] != b']' {
            found |= (pattern[i]..=pattern[i + 2]).contains(&c);
            i += 3;
        } else {
            found |= pattern[i] == c;
            i += 1;
        }
    }
    // No closing bracket: the `[` is an ordinary character.
    if i >= pattern.len() {
        return (c == b'[').then_some(at + 1);
    }
    (found != negated).then_some(i + 1)
}

/// The command writes (a read-only token may not send it).
pub fn writes(name: &str) -> bool {
    matches!(
        name,
        "SET"
            | "SETNX"
            | "SETEX"
            | "PSETEX"
            | "GETSET"
            | "GETDEL"
            | "GETEX"
            | "MSET"
            | "MSETNX"
            | "DEL"
            | "UNLINK"
            | "INCR"
            | "DECR"
            | "INCRBY"
            | "DECRBY"
            | "APPEND"
            | "EXPIRE"
            | "PEXPIRE"
            | "PERSIST"
            | "FLUSHDB"
            | "FLUSHALL"
    )
}

fn number<T: std::str::FromStr>(arg: &[u8]) -> Option<T> {
    std::str::from_utf8(arg).ok()?.parse().ok()
}

fn upper(arg: &[u8]) -> String {
    String::from_utf8_lossy(arg).to_ascii_uppercase()
}

const NOT_INTEGER: &str = "value is not an integer or out of range";

/// The refusal of a command that writes, sent with a read-only token.
const READ_ONLY: &str = "READONLY this token only reads: use a read-write token";

/// The pattern of `SCAN`'s options (`MATCH p`, `COUNT n`, `TYPE t`), or the
/// refusal of options it does not take.
fn scan_pattern(options: &[Vec<u8>]) -> Result<&[u8], Reply> {
    let mut pattern: &[u8] = b"*";
    for pair in options.chunks(2) {
        match (upper(&pair[0]).as_str(), pair.get(1)) {
            ("MATCH", Some(p)) => pattern = p,
            ("COUNT" | "TYPE", Some(_)) => {}
            _ => return Err(Reply::err("syntax error")),
        }
    }
    Ok(pattern)
}

/// The answer to `INFO`. `redis_version` is what clients read to choose how
/// they talk. A store with no limit (`max_bytes` 0) evicts nothing.
fn info(s: &KvStats) -> Reply {
    let policy = if s.max_bytes == 0 { "noeviction" } else { "allkeys-lru" };
    Reply::Bulk(
        format!(
            "# Server\r\nredis_version:7.0.0\r\nnuage_version:{}\r\n# Memory\r\nused_memory:{}\r\nmaxmemory:{}\r\n\
             maxmemory_policy:{policy}\r\n# Stats\r\nkeyspace_hits:{}\r\nkeyspace_misses:{}\r\nevicted_keys:{}\r\n\
             expired_keys:{}\r\n# Keyspace\r\ndb0:keys={},expires={}\r\n",
            crate::VERSION,
            s.bytes,
            s.max_bytes,
            s.hits,
            s.misses,
            s.evictions,
            s.expired,
            s.keys,
            s.expiring
        )
        .into_bytes(),
    )
}

/// The store, held: what runs on it runs alone.
struct Locked<'a> {
    max_bytes: usize,
    inner: &'a mut Inner,
    now: u64,
}

fn check(max: usize, key: &[u8], value_len: usize) -> Result<(), KvError> {
    if key.is_empty() || key.len() > MAX_KEY {
        return Err(KvError::BadKey);
    }
    if key.len() + value_len + ENTRY_OVERHEAD > max {
        return Err(KvError::TooLarge);
    }
    Ok(())
}

impl Locked<'_> {
    fn with<T>(&mut self, f: impl FnOnce(&mut Inner, u64) -> T) -> T {
        f(self.inner, self.now)
    }

    fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.with(|inner, now| inner.read(key, now).map(|e| e.value.clone()))
    }

    fn get_with_ttl(&mut self, key: &[u8]) -> Option<(Vec<u8>, Ttl)> {
        self.with(|inner, now| {
            inner.read(key, now).map(|e| (e.value.clone(), e.expires.map_or(Ttl::Forever, |at| Ttl::Millis(at - now))))
        })
    }

    fn set(&mut self, key: &[u8], value: Vec<u8>, options: &SetOptions) -> Result<bool, KvError> {
        let max = self.max_bytes;
        check(max, key, value.len())?;
        Ok(self.with(|inner, now| Self::set_in(max, inner, now, key, value, options)))
    }

    fn set_in(max: usize, inner: &mut Inner, now: u64, key: &[u8], value: Vec<u8>, options: &SetOptions) -> bool {
        let exists = inner.alive(key, now);
        if (options.only_if_absent && exists) || (options.only_if_present && !exists) {
            return false;
        }
        let expires = match options.ttl {
            Some(ttl) => Some(now.saturating_add(millis(ttl).max(1))),
            None if options.keep_ttl => inner.map.get(key).and_then(|e| e.expires),
            None => None,
        };
        inner.write(key, value, expires);
        inner.fit(max, now);
        true
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        self.with(|inner, now| inner.alive(key, now) && inner.remove(key).is_some())
    }

    fn exists(&mut self, key: &[u8]) -> bool {
        self.with(|inner, now| inner.alive(key, now))
    }

    fn incr_by(&mut self, key: &[u8], by: i64, ttl_if_new: Option<Duration>) -> Result<i64, KvError> {
        let max = self.max_bytes;
        check(max, key, 20)?;
        self.with(|inner, now| Self::incr_in(max, inner, now, key, by, ttl_if_new))
    }

    fn incr_in(
        max: usize,
        inner: &mut Inner,
        now: u64,
        key: &[u8],
        by: i64,
        ttl_if_new: Option<Duration>,
    ) -> Result<i64, KvError> {
        let (current, expires) = match inner.alive(key, now).then(|| inner.map.get(key)).flatten() {
            Some(e) => {
                let n = std::str::from_utf8(&e.value).ok().and_then(|s| s.parse::<i64>().ok());
                (n.ok_or(KvError::NotInteger)?, e.expires)
            }
            None => (0, ttl_if_new.map(|ttl| now.saturating_add(millis(ttl).max(1)))),
        };
        let next = current.checked_add(by).ok_or(KvError::Overflow)?;
        inner.write(key, next.to_string().into_bytes(), expires);
        inner.fit(max, now);
        Ok(next)
    }

    fn expire(&mut self, key: &[u8], ttl: Option<Duration>) -> bool {
        self.with(|inner, now| {
            if !inner.alive(key, now) {
                return false;
            }
            inner.set_expiry(key, ttl.map(|ttl| now.saturating_add(millis(ttl).max(1))));
            true
        })
    }

    fn ttl(&mut self, key: &[u8]) -> Ttl {
        self.with(|inner, now| Self::ttl_in(inner, now, key))
    }

    fn ttl_in(inner: &mut Inner, now: u64, key: &[u8]) -> Ttl {
        if !inner.alive(key, now) {
            return Ttl::Missing;
        }
        match inner.map.get(key).and_then(|e| e.expires) {
            Some(at) => Ttl::Millis(at - now),
            None => Ttl::Forever,
        }
    }

    fn keys(&mut self, prefix: &[u8], limit: usize) -> Vec<KeyInfo> {
        self.with(|inner, now| {
            inner.sweep(now);
            let mut keys: Vec<KeyInfo> = inner
                .map
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, e)| KeyInfo {
                    key: k.to_vec(),
                    size: e.value.len(),
                    ttl: e.expires.map_or(Ttl::Forever, |at| Ttl::Millis(at - now)),
                })
                .collect();
            keys.sort_by(|a, b| a.key.cmp(&b.key));
            keys.truncate(limit);
            keys
        })
    }

    fn clear(&mut self) -> usize {
        self.with(|inner, _| {
            let n = inner.map.len();
            let (hits, misses, evictions, expired) = (inner.hits, inner.misses, inner.evictions, inner.expired);
            *inner = Inner { hits, misses, evictions, expired, ..Inner::default() };
            n
        })
    }

    fn sweep(&mut self) -> usize {
        self.with(|inner, now| inner.sweep(now))
    }

    fn stats(&mut self) -> KvStats {
        let max = self.max_bytes;
        self.with(|inner, now| {
            inner.sweep(now);
            KvStats {
                keys: inner.map.len(),
                expiring: inner.deadlines.len(),
                bytes: inner.bytes,
                max_bytes: max,
                hits: inner.hits,
                misses: inner.misses,
                evictions: inner.evictions,
                expired: inner.expired,
            }
        })
    }

    fn command(&mut self, args: &[Vec<u8>], read_only: bool) -> Reply {
        let max = self.max_bytes;
        let Some(name) = args.first().map(|a| upper(a)) else {
            return Reply::err("empty command");
        };
        if read_only && writes(&name) {
            return Reply::Error(READ_ONLY.into());
        }
        let args = &args[1..];
        let wrong = || Reply::err(format!("wrong number of arguments for '{}' command", name.to_lowercase()));
        let refused = |e: KvError| Reply::err(e);
        match (name.as_str(), args) {
            ("PING", []) => Reply::Status("PONG".into()),
            ("PING" | "ECHO", [message]) => Reply::Bulk(message.clone()),
            ("GET", [key]) => self.get(key).map_or(Reply::Nil, Reply::Bulk),
            ("SET", [key, value, options @ ..]) => self.set_command(key, value, options),
            ("SETNX", [key, value]) => {
                let options = SetOptions { only_if_absent: true, ..SetOptions::default() };
                self.set(key, value.clone(), &options).map_or_else(refused, |set| Reply::Int(i64::from(set)))
            }
            ("SETEX" | "PSETEX", [key, ttl, value]) => {
                let Some(ttl) = number::<u64>(ttl).filter(|n| *n > 0) else {
                    return Reply::err(format!("invalid expire time in '{}' command", name.to_lowercase()));
                };
                let ttl = if name == "SETEX" { Duration::from_secs(ttl) } else { Duration::from_millis(ttl) };
                self.set(key, value.clone(), &SetOptions::expiring(ttl)).map_or_else(refused, |_| Reply::Ok)
            }
            ("GETSET", [key, value]) => match check(max, key, value.len()) {
                Err(e) => refused(e),
                Ok(()) => self.with(|inner, now| {
                    let old = inner.read(key, now).map(|e| e.value.clone());
                    Self::set_in(max, inner, now, key, value.clone(), &SetOptions::default());
                    old.map_or(Reply::Nil, Reply::Bulk)
                }),
            },
            ("GETDEL", [key]) => self.with(|inner, now| {
                let old = inner.read(key, now).map(|e| e.value.clone());
                inner.remove(key);
                old.map_or(Reply::Nil, Reply::Bulk)
            }),
            ("GETEX", [key, options @ ..]) => {
                let expiry = match options {
                    [] => None,
                    [o] if upper(o) == "PERSIST" => Some(None),
                    [o, n] => match (upper(o).as_str(), number::<u64>(n).filter(|n| *n > 0)) {
                        ("EX", Some(n)) => Some(Some(Duration::from_secs(n))),
                        ("PX", Some(n)) => Some(Some(Duration::from_millis(n))),
                        _ => return Reply::err("syntax error"),
                    },
                    _ => return Reply::err("syntax error"),
                };
                if expiry.is_some() && read_only {
                    return Reply::Error(READ_ONLY.into());
                }
                self.with(|inner, now| {
                    let value = inner.read(key, now).map(|e| e.value.clone());
                    if let (Some(_), Some(ttl)) = (&value, expiry) {
                        inner.set_expiry(key, ttl.map(|ttl| now.saturating_add(millis(ttl).max(1))));
                    }
                    value.map_or(Reply::Nil, Reply::Bulk)
                })
            }
            ("MGET", [_, ..]) => self.with(|inner, now| {
                // Each value is counted before it is copied.
                let mut size = 0usize;
                let mut values = Vec::with_capacity(args.len().min(4096));
                for key in args {
                    let len = inner.read(key, now).map(|e| e.value.len());
                    size = size.saturating_add(16 + len.unwrap_or(0));
                    if size > MAX_ANSWER && !values.is_empty() {
                        return Reply::err(format!(
                            "the answer would pass {} MiB: ask for fewer keys at once",
                            MAX_ANSWER >> 20
                        ));
                    }
                    values.push(match len {
                        Some(_) => inner.map.get(key.as_slice()).map_or(Reply::Nil, |e| Reply::Bulk(e.value.clone())),
                        None => Reply::Nil,
                    });
                }
                Reply::Array(values)
            }),
            ("MSET" | "MSETNX", [_, _, ..]) if args.len().is_multiple_of(2) => {
                for pair in args.chunks(2) {
                    if let Err(e) = check(max, &pair[0], pair[1].len()) {
                        return refused(e);
                    }
                }
                self.with(|inner, now| {
                    if name == "MSETNX" && args.chunks(2).any(|pair| inner.alive(&pair[0], now)) {
                        return Reply::Int(0);
                    }
                    for pair in args.chunks(2) {
                        Self::set_in(max, inner, now, &pair[0], pair[1].clone(), &SetOptions::default());
                    }
                    if name == "MSETNX" { Reply::Int(1) } else { Reply::Ok }
                })
            }
            ("APPEND", [key, more]) => self.with(|inner, now| {
                let (mut value, expires) = match inner.alive(key, now).then(|| inner.map.get(key.as_slice())).flatten()
                {
                    Some(e) => (e.value.clone(), e.expires),
                    None => (Vec::new(), None),
                };
                value.extend_from_slice(more);
                if let Err(e) = check(max, key, value.len()) {
                    return refused(e);
                }
                let len = value.len();
                inner.write(key, value, expires);
                inner.fit(max, now);
                Reply::Int(len as i64)
            }),
            ("STRLEN", [key]) => {
                Reply::Int(self.with(|inner, now| inner.read(key, now).map_or(0, |e| e.value.len())) as i64)
            }
            ("INCR" | "DECR", [key]) => {
                let by = if name == "INCR" { 1 } else { -1 };
                self.incr_by(key, by, None).map_or_else(refused, Reply::Int)
            }
            ("INCRBY" | "DECRBY", [key, by]) => {
                let Some(by) = number::<i64>(by) else { return Reply::err(NOT_INTEGER) };
                let by = if name == "INCRBY" { Some(by) } else { by.checked_neg() };
                match by {
                    Some(by) => self.incr_by(key, by, None).map_or_else(refused, Reply::Int),
                    None => refused(KvError::Overflow),
                }
            }
            ("DEL" | "UNLINK", [_, ..]) => self.with(|inner, now| {
                Reply::Int(args.iter().filter(|k| inner.alive(k, now) && inner.remove(k).is_some()).count() as i64)
            }),
            ("EXISTS", [_, ..]) => {
                self.with(|inner, now| Reply::Int(args.iter().filter(|k| inner.alive(k, now)).count() as i64))
            }
            ("EXPIRE" | "PEXPIRE", [key, ttl]) => {
                let Some(ttl) = number::<i64>(ttl) else { return Reply::err(NOT_INTEGER) };
                if ttl <= 0 {
                    // An expiry in the past deletes the key.
                    return Reply::Int(i64::from(self.delete(key)));
                }
                let ttl = ttl as u64;
                let ttl = if name == "EXPIRE" { Duration::from_secs(ttl) } else { Duration::from_millis(ttl) };
                Reply::Int(i64::from(self.expire(key, Some(ttl))))
            }
            ("PERSIST", [key]) => self.with(|inner, now| {
                let had = matches!(Self::ttl_in(inner, now, key), Ttl::Millis(_));
                if had {
                    inner.set_expiry(key, None);
                }
                Reply::Int(i64::from(had))
            }),
            ("TTL" | "PTTL", [key]) => Reply::Int(match self.ttl(key) {
                Ttl::Missing => -2,
                Ttl::Forever => -1,
                // Rounded up: a key that lives 400 ms more has a TTL of 1, not 0.
                Ttl::Millis(ms) if name == "TTL" => ms.div_ceil(1000) as i64,
                Ttl::Millis(ms) => ms as i64,
            }),
            ("TYPE", [key]) => Reply::Status(if self.exists(key) { "string" } else { "none" }.into()),
            ("KEYS", [pattern]) => Reply::Array(self.matching(pattern).into_iter().map(Reply::Bulk).collect()),
            ("SCAN", [_cursor, options @ ..]) => {
                // The whole answer in one page: the cursor that comes back is 0.
                let pattern = match scan_pattern(options) {
                    Ok(p) => p,
                    Err(refused) => return refused,
                };
                Reply::Array(vec![
                    Reply::Bulk(b"0".to_vec()),
                    Reply::Array(self.matching(pattern).into_iter().map(Reply::Bulk).collect()),
                ])
            }
            ("DBSIZE", []) => Reply::Int(self.stats().keys as i64),
            ("FLUSHDB" | "FLUSHALL", _) => {
                self.clear();
                Reply::Ok
            }
            ("INFO", _) => info(&self.stats()),
            (
                "PING" | "ECHO" | "GET" | "SET" | "SETNX" | "SETEX" | "PSETEX" | "GETSET" | "GETDEL" | "GETEX" | "MGET"
                | "MSET" | "MSETNX" | "APPEND" | "STRLEN" | "INCR" | "DECR" | "INCRBY" | "DECRBY" | "DEL" | "UNLINK"
                | "EXISTS" | "EXPIRE" | "PEXPIRE" | "PERSIST" | "TTL" | "PTTL" | "TYPE" | "KEYS" | "SCAN" | "DBSIZE",
                _,
            ) => wrong(),
            _ => Reply::err(format!(
                "unknown command '{}': Nuage DB keeps strings and counters (GET, SET, DEL, INCR, EXPIRE, TTL, MGET, KEYS...)",
                name.to_lowercase()
            )),
        }
    }

    fn matching(&mut self, pattern: &[u8]) -> Vec<Vec<u8>> {
        self.with(|inner, now| {
            inner.sweep(now);
            let mut keys: Vec<Vec<u8>> = inner.map.keys().filter(|k| glob(pattern, k)).map(|k| k.to_vec()).collect();
            keys.sort();
            keys
        })
    }

    fn set_command(&mut self, key: &[u8], value: &[u8], options: &[Vec<u8>]) -> Reply {
        let max = self.max_bytes;
        let mut set = SetOptions::default();
        let mut get = false;
        let mut i = 0;
        while i < options.len() {
            match upper(&options[i]).as_str() {
                "NX" => set.only_if_absent = true,
                "XX" => set.only_if_present = true,
                "KEEPTTL" => set.keep_ttl = true,
                "GET" => get = true,
                unit @ ("EX" | "PX") => {
                    let Some(n) = options.get(i + 1).and_then(|n| number::<u64>(n)).filter(|n| *n > 0) else {
                        return Reply::err("invalid expire time in 'set' command");
                    };
                    set.ttl = Some(if unit == "EX" { Duration::from_secs(n) } else { Duration::from_millis(n) });
                    i += 1;
                }
                _ => return Reply::err("syntax error"),
            }
            i += 1;
        }
        if (set.only_if_absent && set.only_if_present) || (set.keep_ttl && set.ttl.is_some()) {
            return Reply::err("syntax error");
        }
        if let Err(e) = check(max, key, value.len()) {
            return Reply::err(e);
        }
        self.with(|inner, now| {
            let old = if get { inner.read(key, now).map(|e| e.value.clone()) } else { None };
            let written = Self::set_in(max, inner, now, key, value.to_vec(), &set);
            match (get, written) {
                (true, _) => old.map_or(Reply::Nil, Reply::Bulk),
                (false, true) => Reply::Ok,
                (false, false) => Reply::Nil,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(kv: &Kv, command: &[&str]) -> Reply {
        kv.command(&command.iter().map(|a| a.as_bytes().to_vec()).collect::<Vec<_>>(), false)
    }

    fn bulk(s: &str) -> Reply {
        Reply::Bulk(s.as_bytes().to_vec())
    }

    #[test]
    fn values_counters_and_expiry() {
        let kv = Kv::new(1 << 20);
        assert_eq!(kv.get(b"a"), None);
        assert!(kv.set(b"a", b"1".to_vec(), &SetOptions::default()).unwrap());
        assert_eq!(kv.get(b"a").as_deref(), Some(&b"1"[..]));
        assert_eq!(kv.incr_by(b"a", 41, None).unwrap(), 42);
        assert_eq!(kv.incr_by(b"n", -1, None).unwrap(), -1);
        kv.set(b"text", b"x".to_vec(), &SetOptions::default()).unwrap();
        assert_eq!(kv.incr_by(b"text", 1, None), Err(KvError::NotInteger));
        kv.set(b"big", i64::MAX.to_string().into_bytes(), &SetOptions::default()).unwrap();
        assert_eq!(kv.incr_by(b"big", 1, None), Err(KvError::Overflow));

        // A lock: the first one wins, until it expires.
        let lock = SetOptions { only_if_absent: true, ttl: Some(Duration::from_millis(30)), ..SetOptions::default() };
        assert!(kv.set(b"lock", b"me".to_vec(), &lock).unwrap());
        assert!(!kv.set(b"lock", b"you".to_vec(), &lock).unwrap());
        assert!(matches!(kv.ttl(b"lock"), Ttl::Millis(ms) if ms <= 30));
        assert_eq!(kv.ttl(b"a"), Ttl::Forever);
        assert_eq!(kv.ttl(b"nope"), Ttl::Missing);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(kv.get(b"lock"), None);
        assert!(kv.set(b"lock", b"you".to_vec(), &lock).unwrap());

        // A counter made with a window keeps it: a rate limit.
        assert_eq!(kv.incr_by(b"rate", 1, Some(Duration::from_secs(60))).unwrap(), 1);
        assert_eq!(kv.incr_by(b"rate", 1, Some(Duration::from_secs(600))).unwrap(), 2);
        assert!(matches!(kv.ttl(b"rate"), Ttl::Millis(ms) if ms <= 60_000));

        assert!(kv.expire(b"a", Some(Duration::from_secs(5))));
        assert!(matches!(kv.ttl(b"a"), Ttl::Millis(_)));
        assert!(kv.expire(b"a", None));
        assert_eq!(kv.ttl(b"a"), Ttl::Forever);
        assert!(!kv.expire(b"nope", None));
        assert!(kv.delete(b"a") && !kv.delete(b"a") && !kv.exists(b"a"));

        assert_eq!(kv.set(b"", b"x".to_vec(), &SetOptions::default()), Err(KvError::BadKey));
        assert_eq!(kv.set(&[b'k'; MAX_KEY + 1], vec![], &SetOptions::default()), Err(KvError::BadKey));
        assert_eq!(kv.set(b"huge", vec![0; 2 << 20], &SetOptions::default()), Err(KvError::TooLarge));

        let names: Vec<Vec<u8>> = kv.keys(b"", 100).into_iter().map(|k| k.key).collect();
        assert_eq!(names, [&b"big"[..], b"lock", b"n", b"rate", b"text"]);
        assert_eq!(kv.keys(b"r", 100).len(), 1);
        assert_eq!(kv.keys(b"", 2).len(), 2);
        assert_eq!(kv.clear(), 5);
        assert_eq!(kv.stats().keys, 0);
        assert_eq!(kv.stats().bytes, 0);
    }

    #[test]
    fn the_least_recently_used_go_first() {
        // Room for four values of 15 KiB, not five.
        let kv = Kv::new(64 << 10);
        let value = vec![7u8; 15 << 10];
        for key in [&b"a"[..], b"b", b"c"] {
            kv.set(key, value.clone(), &SetOptions::default()).unwrap();
        }
        kv.set(b"soon", value.clone(), &SetOptions::expiring(Duration::from_millis(1))).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        // The expired key goes first: nothing alive is dropped.
        kv.set(b"d", value.clone(), &SetOptions::default()).unwrap();
        let stats = kv.stats();
        assert_eq!((stats.expired, stats.evictions, stats.keys), (1, 0, 4));
        // `a` was read: `b` is now the oldest, and goes.
        assert!(kv.get(b"a").is_some());
        kv.set(b"e", value.clone(), &SetOptions::default()).unwrap();
        let stats = kv.stats();
        assert_eq!((stats.evictions, stats.keys), (1, 4));
        assert!(stats.bytes <= stats.max_bytes);
        assert!(kv.exists(b"a") && !kv.exists(b"b") && kv.exists(b"c") && kv.exists(b"d") && kv.exists(b"e"));
    }

    #[test]
    fn commands_by_name() {
        let kv = Kv::new(1 << 20);
        assert_eq!(run(&kv, &["ping"]), Reply::Status("PONG".into()));
        assert_eq!(run(&kv, &["SET", "k", "v"]), Reply::Ok);
        assert_eq!(run(&kv, &["get", "k"]), bulk("v"));
        assert_eq!(run(&kv, &["GET", "nope"]), Reply::Nil);
        assert_eq!(run(&kv, &["SET", "k", "w", "NX"]), Reply::Nil);
        assert_eq!(run(&kv, &["SET", "k", "w", "XX", "GET"]), bulk("v"));
        assert_eq!(run(&kv, &["SET", "k", "x", "EX", "100"]), Reply::Ok);
        assert_eq!(run(&kv, &["TTL", "k"]), Reply::Int(100));
        assert_eq!(run(&kv, &["SET", "k", "y", "KEEPTTL"]), Reply::Ok);
        assert!(matches!(run(&kv, &["PTTL", "k"]), Reply::Int(ms) if ms > 90_000 && ms <= 100_000));
        assert_eq!(run(&kv, &["PERSIST", "k"]), Reply::Int(1));
        assert_eq!(run(&kv, &["TTL", "k"]), Reply::Int(-1));
        assert_eq!(run(&kv, &["TTL", "nope"]), Reply::Int(-2));
        assert_eq!(run(&kv, &["EXPIRE", "k", "10"]), Reply::Int(1));
        assert_eq!(run(&kv, &["EXPIRE", "nope", "10"]), Reply::Int(0));
        assert_eq!(run(&kv, &["SETEX", "s", "10", "v"]), Reply::Ok);
        assert_eq!(run(&kv, &["SETNX", "s", "v"]), Reply::Int(0));
        assert_eq!(run(&kv, &["INCR", "n"]), Reply::Int(1));
        assert_eq!(run(&kv, &["INCRBY", "n", "9"]), Reply::Int(10));
        assert_eq!(run(&kv, &["DECR", "n"]), Reply::Int(9));
        assert_eq!(run(&kv, &["DECRBY", "n", "4"]), Reply::Int(5));
        assert_eq!(run(&kv, &["INCR", "k"]), Reply::Error("ERR value is not an integer or out of range".into()));
        assert_eq!(run(&kv, &["APPEND", "k", "z"]), Reply::Int(2));
        assert_eq!(run(&kv, &["STRLEN", "k"]), Reply::Int(2));
        assert_eq!(run(&kv, &["GETSET", "k", "new"]), bulk("yz"));
        assert_eq!(run(&kv, &["MSET", "a", "1", "b", "2"]), Reply::Ok);
        assert_eq!(run(&kv, &["MGET", "a", "nope", "b"]), Reply::Array(vec![bulk("1"), Reply::Nil, bulk("2")]));
        assert_eq!(run(&kv, &["MSETNX", "a", "9", "c", "3"]), Reply::Int(0));
        assert_eq!(run(&kv, &["EXISTS", "a", "b", "nope"]), Reply::Int(2));
        assert_eq!(run(&kv, &["TYPE", "a"]), Reply::Status("string".into()));
        assert_eq!(run(&kv, &["KEYS", "?"]), Reply::Array(vec![bulk("a"), bulk("b"), bulk("k"), bulk("n"), bulk("s")]));
        assert_eq!(
            run(&kv, &["SCAN", "0", "MATCH", "[ab]", "COUNT", "10"]),
            Reply::Array(vec![bulk("0"), Reply::Array(vec![bulk("a"), bulk("b")])])
        );
        assert_eq!(run(&kv, &["GETDEL", "a"]), bulk("1"));
        assert_eq!(run(&kv, &["DEL", "a", "b", "n"]), Reply::Int(2));
        assert_eq!(run(&kv, &["DBSIZE"]), Reply::Int(2));
        assert_eq!(run(&kv, &["EXPIRE", "k", "0"]), Reply::Int(1));
        assert_eq!(run(&kv, &["GET", "k"]), Reply::Nil);
        assert!(matches!(run(&kv, &["INFO"]), Reply::Bulk(b) if String::from_utf8_lossy(&b).contains("db0:keys=1")));
        assert_eq!(run(&kv, &["GET"]), Reply::Error("ERR wrong number of arguments for 'get' command".into()));
        assert!(matches!(run(&kv, &["LPUSH", "l", "x"]), Reply::Error(e) if e.contains("unknown command 'lpush'")));
        assert_eq!(run(&kv, &["FLUSHDB"]), Reply::Ok);
        assert_eq!(run(&kv, &["DBSIZE"]), Reply::Int(0));

        // A read-only token reads, and nothing else.
        let args = |c: &[&str]| c.iter().map(|a| a.as_bytes().to_vec()).collect::<Vec<_>>();
        kv.set(b"k", b"v".to_vec(), &SetOptions::default()).unwrap();
        assert_eq!(kv.command(&args(&["GET", "k"]), true), bulk("v"));
        for write in [&["SET", "k", "w"][..], &["DEL", "k"], &["INCR", "n"], &["FLUSHALL"], &["GETEX", "k", "EX", "5"]]
        {
            assert!(
                matches!(kv.command(&args(write), true), Reply::Error(e) if e.starts_with("READONLY")),
                "{write:?}"
            );
        }
        assert_eq!(kv.get(b"k").as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn answers_are_bounded() {
        let kv = Kv::new(64 << 20);
        kv.set(b"big", vec![b'x'; 1 << 20], &SetOptions::default()).unwrap();
        // One large value is answered; the same one named again and again is not.
        assert!(matches!(run(&kv, &["MGET", "big"]), Reply::Array(v) if v.len() == 1));
        let mut many = vec!["MGET"];
        many.extend(std::iter::repeat_n("big", 400));
        assert!(matches!(run(&kv, &many), Reply::Error(e) if e.contains("fewer keys")));
        let few: Vec<&str> = std::iter::once("MGET").chain(std::iter::repeat_n("big", 10)).collect();
        assert!(matches!(run(&kv, &few), Reply::Array(v) if v.len() == 10));
        // Missing keys count too: a million of them is a large answer.
        let nothing: Vec<Vec<u8>> =
            std::iter::once(b"MGET".to_vec()).chain(std::iter::repeat_n(b"nope".to_vec(), 1_100_000)).collect();
        assert!(matches!(kv.command(&nothing, true), Reply::Error(e) if e.contains("fewer keys")));
        // A transaction stops running commands once its answers are that large.
        let get = vec![b"GET".to_vec(), b"big".to_vec()];
        let set = vec![b"SET".to_vec(), b"after".to_vec(), b"1".to_vec()];
        let mut commands = vec![get; 20];
        commands.push(set);
        let replies = kv.transaction(&commands, false);
        assert_eq!(replies.len(), 21);
        assert!(matches!(&replies[0], Reply::Bulk(b) if b.len() == 1 << 20));
        assert!(matches!(&replies[20], Reply::Error(e) if e.contains("not run")));
        assert_eq!(kv.get(b"after"), None);
    }

    #[test]
    fn globs_and_the_wire() {
        for (pattern, text, yes) in [
            ("*", "", true),
            ("*", "anything", true),
            ("user:*", "user:42", true),
            ("user:*", "users", false),
            ("*:42", "user:42", true),
            ("a*b*c", "a--b--c", true),
            ("a*b*c", "a--b--", false),
            ("h?llo", "hello", true),
            ("h?llo", "hllo", false),
            ("h[ae]llo", "hallo", true),
            ("h[ae]llo", "hillo", false),
            ("h[^e]llo", "hallo", true),
            ("h[a-c]llo", "hbllo", true),
            ("h[a-c]llo", "hdllo", false),
            ("a\\*b", "a*b", true),
            ("a\\*b", "axb", false),
            ("[", "[", true),
        ] {
            assert_eq!(glob(pattern.as_bytes(), text.as_bytes()), yes, "{pattern} ~ {text}");
        }
        let reply = Reply::Array(vec![
            Reply::Ok,
            Reply::Nil,
            Reply::Int(-2),
            bulk("hé"),
            Reply::Error("ERR a\r\nb".into()),
            Reply::Map(vec![(bulk("proto"), Reply::Int(2))]),
        ]);
        let mut out = Vec::new();
        reply.write_resp(&mut out, false);
        assert_eq!(out, b"*6\r\n+OK\r\n$-1\r\n:-2\r\n$3\r\nh\xc3\xa9\r\n-ERR a  b\r\n*2\r\n$5\r\nproto\r\n:2\r\n");
        let mut out = Vec::new();
        reply.write_resp(&mut out, true);
        assert_eq!(out, b"*6\r\n+OK\r\n_\r\n:-2\r\n$3\r\nh\xc3\xa9\r\n-ERR a  b\r\n%1\r\n$5\r\nproto\r\n:2\r\n");
    }
}
