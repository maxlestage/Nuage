//! The page store: B-trees on disk ([`super::pages`]), one per branch, behind
//! the write-ahead log.
//!
//! A branch is an overlay: its tree holds only the keys it changed (values
//! or tombstones). Reading a branch reads its tree, then its parent's, up to
//! the main tree. When a key changes in a branch that has children, the old
//! value is first copied into every child that does not have its own: the
//! children keep reading the data as it was when they were made
//! (copy-on-write).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::SystemTime;

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::exec::TableMeta;
use super::pages::{MAX_ENTRY, Node, Pages, Version};
use super::storage_err;
use super::wal::{self, Op, Record};
use crate::crypto::Keys;

pub(crate) const MAIN: &str = "main";
/// The page store's file in a data directory.
pub(crate) const FILE: &str = "nuage.db";
pub(crate) const META: &str = "meta";
/// The store table of the main database.
const MAIN_TABLE: &str = "b:main";
const CHECKPOINT_LSN: &[u8] = b"checkpoint_lsn";
const GENERATION: &[u8] = b"generation";
const BRANCH_PREFIX: &[u8] = b"branch:";

/// The store table of a branch.
pub(crate) fn branch_table(branch: &str) -> String {
    format!("b:{branch}")
}

fn bad(e: impl std::fmt::Display) -> Error {
    storage_err(format!("page store: {e}"))
}

/// Live entries of a scan, and where to continue (`None`: done).
pub(crate) type Scanned = (Vec<(Vec<u8>, Vec<u8>)>, Option<Vec<u8>>);

/// Pending changes of one branch: key → new value, `None` to delete.
pub(crate) type Overlay = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// A branch as recorded in the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchInfo {
    pub name: String,
    pub parent: String,
    pub created_at: i64,
}

/// A consistent read view of one branch: the trees as one commit left them.
pub(crate) struct Snapshot {
    pages: Arc<Pages>,
    /// Held as long as the view is read: its pages are not reused meanwhile.
    version: Arc<Version>,
    /// The branch's tree, then its ancestors', main last. `None`: not
    /// created yet (a branch that never wrote anything).
    layers: Vec<Option<Arc<Node>>>,
}

/// Decodes a stored value: `[1, data...]` or the tombstone `[0]`.
fn stored(bytes: &[u8]) -> Option<Vec<u8>> {
    match bytes.split_first() {
        Some((1, rest)) => Some(rest.to_vec()),
        _ => None,
    }
}

fn tagged(value: Option<&[u8]>) -> Vec<u8> {
    match value {
        Some(v) => {
            let mut out = Vec::with_capacity(v.len() + 1);
            out.push(1);
            out.extend_from_slice(v);
            out
        }
        None => vec![0],
    }
}

impl Snapshot {
    /// The last commit the view holds.
    pub(crate) fn lsn(&self) -> u64 {
        self.version.lsn()
    }

    /// The value of `key`, looking in `overlay` first.
    pub(crate) fn get(&self, overlay: Option<&Overlay>, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(v) = overlay.and_then(|o| o.get(key)) {
            return Ok(v.clone());
        }
        for layer in self.layers.iter().flatten() {
            if let Some(v) = self.pages.find(layer, key, stored)? {
                return Ok(v);
            }
        }
        Ok(None)
    }

    /// The stored value of `key` (what is pending in a transaction aside),
    /// given to `f` where it is, without a copy.
    pub(crate) fn with<R>(&self, key: &[u8], f: impl FnOnce(&[u8]) -> R) -> Result<Option<R>> {
        let mut f = Some(f);
        for layer in self.layers.iter().flatten() {
            let found = self.pages.find(layer, key, |v| match (v.split_first(), f.take()) {
                (Some((1, value)), Some(f)) => Some(f(value)),
                _ => None,
            })?;
            // Found in this layer, as a value or as a deletion: the ones behind are hidden.
            if let Some(value) = found {
                return Ok(value);
            }
        }
        Ok(None)
    }

    /// Calls `f` with each live entry of `start..end`, in key order, until it
    /// returns `Ok(false)`. The key and the value are lent where they are.
    pub(crate) fn each(
        &self,
        overlay: Option<&Overlay>,
        start: &[u8],
        end: &[u8],
        mut f: impl FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()> {
        if start >= end {
            return Ok(());
        }
        let range = (Bound::Included(start), Bound::Excluded(end));
        let pending = overlay.is_some_and(|o| o.range::<[u8], _>(range).next().is_some());
        let mut layers = self.layers.iter().flatten();
        let (first, second) = (layers.next(), layers.next());
        if !pending && second.is_none() {
            // One tree, nothing pending in the range: straight from its pages.
            let Some(root) = first else { return Ok(()) };
            let mut failed = None;
            self.pages.walk(root, start, end, &mut |k, v| match v.split_first() {
                Some((1, value)) => f(k, value).unwrap_or_else(|e| {
                    failed = Some(e);
                    false
                }),
                _ => true,
            })?;
            return failed.map_or(Ok(()), Err);
        }
        let mut at = start.to_vec();
        loop {
            let (entries, next) = self.scan(overlay, &at, end, super::exec::CHUNK)?;
            for (k, v) in &entries {
                if !f(k, v)? {
                    return Ok(());
                }
            }
            match next {
                Some(n) => at = n,
                None => return Ok(()),
            }
        }
    }

    /// Up to `limit` live entries in `start..end`, in key order, merging the
    /// layers (the closest one wins). Returns them and where to continue
    /// (`None`: the range is exhausted).
    pub(crate) fn scan(&self, overlay: Option<&Overlay>, start: &[u8], end: &[u8], limit: usize) -> Result<Scanned> {
        let limit = limit.max(1);
        let range = (Bound::Included(start), Bound::Excluded(end));
        let overlay = overlay.filter(|o| start < end && o.range::<[u8], _>(range).next().is_some());
        let mut layers = self.layers.iter().flatten();
        let (first, second) = (layers.next(), layers.next());
        if overlay.is_none() && second.is_none() {
            // One tree and nothing pending in the range (the main database,
            // outside a transaction that wrote there): no merge.
            let mut out = Vec::new();
            let mut next = None;
            if let Some(root) = first {
                self.pages.walk(root, start, end, &mut |k, v| {
                    if out.len() == limit {
                        next = Some(k.to_vec());
                        return false;
                    }
                    if let Some((1, value)) = v.split_first() {
                        out.push((k.to_vec(), value.to_vec()));
                    }
                    true
                })?;
            }
            return Ok((out, next));
        }
        // Each layer gives at most `limit` keys; past the smallest last key of
        // a layer that had more, the merge would be incomplete.
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let mut boundary: Option<Vec<u8>> = None;
        let cut = |last: Vec<u8>, boundary: &mut Option<Vec<u8>>| {
            if boundary.as_ref().is_none_or(|b| last < *b) {
                *boundary = Some(last);
            }
        };
        // Layers from the farthest to the closest, so closer ones overwrite.
        for layer in self.layers.iter().rev().flatten() {
            let mut last: Option<Vec<u8>> = None;
            let mut n = 0;
            let mut more = false;
            self.pages.walk(layer, start, end, &mut |k, v| {
                if n == limit {
                    more = true;
                    return false;
                }
                n += 1;
                merged.insert(k.to_vec(), stored(v));
                last = Some(k.to_vec());
                true
            })?;
            if more {
                cut(last.unwrap_or_default(), &mut boundary);
            }
        }
        if let Some(o) = overlay {
            let mut last = None;
            for (n, (k, v)) in o.range::<[u8], _>(range).enumerate() {
                if n == limit {
                    cut(last.take().unwrap_or_default(), &mut boundary);
                    break;
                }
                merged.insert(k.clone(), v.clone());
                last = Some(k.clone());
            }
        }
        let mut out = Vec::new();
        for (k, v) in merged {
            if boundary.as_ref().is_some_and(|b| k > *b) {
                break;
            }
            if let Some(v) = v {
                out.push((k, v));
            }
        }
        let next = boundary.map(|mut b| {
            b.push(0);
            b
        });
        Ok((out, next))
    }
}

/// State of the writer, behind one lock: writes are applied one at a time.
pub(crate) struct Writer {
    pub wal: wal::Writer,
    pub next_lsn: u64,
    pub checkpoint_lsn: u64,
    pub last_checkpoint: Option<SystemTime>,
    pub commits_since_checkpoint: u64,
}

/// The page file, the write-ahead log and the branch registry.
pub(crate) struct Store {
    pub pages: Arc<Pages>,
    pub dir: PathBuf,
    pub writer: Mutex<Writer>,
    branches: RwLock<HashMap<String, BranchInfo>>,
    generation: AtomicU64,
    /// The last commit that wrote each table of each branch, for optimistic
    /// transactions.
    written: Mutex<HashMap<String, Written>>,
    /// How far the log is on disk.
    durable: Durable,
    /// The log segment being written, for whoever syncs it.
    wal_file: Mutex<Arc<fs::File>>,
    /// `fsync` the log before a commit returns (off only in tests).
    sync: bool,
    /// The table generation the page store holds.
    saved_generation: AtomicU64,
    /// LSN of the last applied commit (what snapshots see).
    applied_lsn: AtomicU64,
    /// Catalog entries already parsed (see [`Store::parsed`]).
    catalog: Mutex<Catalog>,
    /// Bytes of changed nodes the page store may keep in memory before they
    /// are written out, whatever the checkpoint interval.
    dirty_limit: usize,
    /// Commits the log read at opening does not hold, between two it does:
    /// the last one before the gap and the first one after it.
    pub log_gap: Option<(u64, u64)>,
}

/// The commits that wrote to a branch.
#[derive(Default)]
struct Written {
    /// The last commit that wrote each table.
    tables: HashMap<String, u64>,
    /// The commit that reset or deleted the branch.
    reset: u64,
}

/// How far the log is on disk, and whether someone is syncing it.
struct Durable {
    /// Every commit up to this one is on disk.
    lsn: AtomicU64,
    /// A sync is running: the others wait for it instead of starting one.
    syncing: Mutex<bool>,
    done: Condvar,
    /// A sync of the log failed, and why. The system reports such an error
    /// once and may have dropped what it could not write: a later sync that
    /// succeeds proves nothing, so no commit is accepted until a restart.
    failed: OnceLock<String>,
    /// The next sync fails (tests).
    #[cfg(test)]
    fail_next: std::sync::atomic::AtomicBool,
}

/// A catalog entry parsed once: its table metadata and, when a statement asked for it,
/// its `'static` schema.
pub(crate) struct Parsed {
    pub meta: Arc<TableMeta>,
    pub schema: OnceLock<&'static memoire_orm::TableSchema>,
}

/// Catalog entries parsed once.
#[derive(Default)]
struct Catalog {
    /// By their stored bytes: the same bytes always describe the same table.
    by_bytes: HashMap<Vec<u8>, Arc<Parsed>>,
    /// The entry each table name last had, with its bytes: what nearly every
    /// statement asks for again.
    by_name: HashMap<String, (Vec<u8>, Arc<Parsed>)>,
}

/// Distinct catalog entries kept parsed; past it, the cache starts over.
const CATALOG_CACHE: usize = 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Opens (or creates) a page file, encrypted with `keys` if any.
pub(crate) fn open_pages(path: &Path, cache_bytes: usize, keys: Option<&Keys>, sync: bool) -> Result<Pages> {
    Pages::open(path, cache_bytes, keys.map(|k| k.pages.clone()), sync)
}

fn read_meta(pages: &Pages, key: &[u8]) -> Result<Option<Vec<u8>>> {
    match pages.version().root(META) {
        Some(root) => pages.find(&root, key, <[u8]>::to_vec),
        None => Ok(None),
    }
}

/// Writes the page store to disk as holding every commit up to `lsn`.
fn save(pages: &Pages, lsn: u64) -> Result<()> {
    pages.apply(
        &[Op::Put { table: META.into(), key: CHECKPOINT_LSN.to_vec(), value: lsn.to_le_bytes().to_vec() }],
        lsn,
    )?;
    pages.checkpoint()
}

fn u64_of(bytes: Option<Vec<u8>>) -> u64 {
    bytes.and_then(|b| b.try_into().ok()).map_or(0, u64::from_le_bytes)
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Store {
    /// Opens the store in `dir`, replaying the log written since the last
    /// checkpoint, then checkpoints.
    pub(crate) fn open(dir: &Path, cache_bytes: usize, sync: bool, keys: Option<&Keys>) -> Result<Store> {
        fs::create_dir_all(dir).map_err(|e| storage_err(format!("cannot create {}: {e}", dir.display())))?;
        let pages = open_pages(&dir.join(FILE), cache_bytes, keys, sync)?;
        let checkpoint_lsn = u64_of(read_meta(&pages, CHECKPOINT_LSN)?);
        let wal_dir = dir.join("wal");
        let records = wal::recover_after(&wal_dir, keys.map(|k| k.wal.as_ref()), checkpoint_lsn)?;
        let dirty_limit = (cache_bytes / 4).max(1 << 20);
        let mut last = checkpoint_lsn;
        let mut log_gap = None;
        for r in records.iter().filter(|r| r.lsn > checkpoint_lsn) {
            if r.lsn > last + 1 && log_gap.is_none() {
                log_gap = Some((last, r.lsn));
            }
            pages.apply(&r.ops, r.lsn)?;
            last = last.max(r.lsn);
            if pages.dirty_bytes() > dirty_limit {
                // A long log: what it changed goes to disk on the way rather
                // than wait in memory for the end. A crash resumes from here.
                save(&pages, last)?;
            }
        }
        let generation = u64_of(read_meta(&pages, GENERATION)?);
        let log = wal::Writer::open(&wal_dir, last + 1, keys.map(|k| k.wal.clone()))?;
        let store = Store {
            pages: Arc::new(pages),
            dir: dir.to_owned(),
            written: Mutex::new(HashMap::new()),
            durable: Durable {
                lsn: AtomicU64::new(0),
                syncing: Mutex::new(false),
                done: Condvar::new(),
                failed: OnceLock::new(),
                #[cfg(test)]
                fail_next: std::sync::atomic::AtomicBool::new(false),
            },
            wal_file: Mutex::new(log.file()),
            sync,
            writer: Mutex::new(Writer {
                wal: log,
                next_lsn: last + 1,
                checkpoint_lsn,
                last_checkpoint: None,
                commits_since_checkpoint: 0,
            }),
            branches: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(generation),
            saved_generation: AtomicU64::new(generation),
            applied_lsn: AtomicU64::new(last),
            catalog: Mutex::new(Catalog::default()),
            dirty_limit,
            log_gap,
        };
        store.load_branches()?;
        store.checkpoint(None)?;
        Ok(store)
    }

    fn load_branches(&self) -> Result<()> {
        let mut map = HashMap::new();
        for (key, value) in self.meta_entries(BRANCH_PREFIX)? {
            let name = String::from_utf8_lossy(&key[BRANCH_PREFIX.len()..]).into_owned();
            let j: Json = serde_json::from_slice(&value).map_err(bad)?;
            map.insert(
                name.clone(),
                BranchInfo {
                    name,
                    parent: j.get("parent").and_then(Json::as_str).unwrap_or(MAIN).to_owned(),
                    created_at: j.get("createdAt").and_then(Json::as_i64).unwrap_or(0),
                },
            );
        }
        *self.branches.write().unwrap_or_else(|e| e.into_inner()) = map;
        Ok(())
    }

    /// The branches (main excluded), oldest first.
    pub(crate) fn branches(&self) -> Vec<BranchInfo> {
        let mut list: Vec<BranchInfo> =
            self.branches.read().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        list.sort_by(|a, b| (a.created_at, &a.name).cmp(&(b.created_at, &b.name)));
        list
    }

    /// The catalog entry stored as `bytes`, parsed. Every statement reads the catalog
    /// entries of its tables: parsing their JSON each time cost most of a short statement.
    /// The same bytes always describe the same table, on every branch and in every
    /// transaction (a change to a table writes new bytes), so keying by them is exact.
    pub(crate) fn parsed(&self, bytes: Vec<u8>) -> Result<Arc<Parsed>> {
        if let Some(p) = lock(&self.catalog).by_bytes.get(&bytes) {
            return Ok(p.clone());
        }
        let parsed = Arc::new(Parsed { meta: Arc::new(TableMeta::from_json(&bytes)?), schema: OnceLock::new() });
        let mut cache = lock(&self.catalog);
        if cache.by_bytes.len() >= CATALOG_CACHE {
            cache.by_bytes.clear();
        }
        cache.by_bytes.insert(bytes, parsed.clone());
        Ok(parsed)
    }

    /// [`parsed`](Self::parsed) for the entry of `table` as it is stored: when
    /// its bytes are the ones the table had last time, comparing them is all
    /// it takes (no copy, no hash of the whole entry).
    pub(crate) fn parsed_as(&self, table: &str, bytes: &[u8]) -> Result<Arc<Parsed>> {
        if let Some((known, parsed)) = lock(&self.catalog).by_name.get(table)
            && known.as_slice() == bytes
        {
            return Ok(parsed.clone());
        }
        let parsed = self.parsed(bytes.to_vec())?;
        let mut cache = lock(&self.catalog);
        if cache.by_name.len() >= CATALOG_CACHE {
            cache.by_name.clear();
        }
        cache.by_name.insert(table.to_owned(), (bytes.to_vec(), parsed.clone()));
        Ok(parsed)
    }

    pub(crate) fn has_branch(&self, name: &str) -> bool {
        name == MAIN || self.branches.read().unwrap_or_else(|e| e.into_inner()).contains_key(name)
    }

    /// `branch`, then its ancestors, main last.
    fn chain(&self, branch: &str) -> Result<Vec<String>> {
        let branches = self.branches.read().unwrap_or_else(|e| e.into_inner());
        let mut chain = vec![branch.to_owned()];
        let mut at = branch.to_owned();
        while at != MAIN {
            let info = branches.get(&at).ok_or_else(|| Error::NotFound)?;
            at = info.parent.clone();
            chain.push(at.clone());
            if chain.len() > 1000 {
                return Err(storage_err("branch ancestry loops"));
            }
        }
        Ok(chain)
    }

    pub(crate) fn children_of(&self, branch: &str) -> Vec<String> {
        self.branches
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|b| b.parent == branch)
            .map(|b| b.name.clone())
            .collect()
    }

    /// A read view of `branch`: the last commit that is on disk. A reader
    /// never sees a commit a crash could still undo.
    pub(crate) fn snapshot(&self, branch: &str) -> Result<Snapshot> {
        self.view(branch, self.pages.visible())
    }

    /// A read view of `branch` with every commit applied so far, on disk or
    /// not yet: what a writer goes by. Whoever gives a caller something read
    /// from it first waits for [`wait_durable`](Self::wait_durable) of its
    /// [`lsn`](Snapshot::lsn).
    pub(crate) fn snapshot_latest(&self, branch: &str) -> Result<Snapshot> {
        self.view(branch, self.pages.version())
    }

    fn view(&self, branch: &str, version: Arc<Version>) -> Result<Snapshot> {
        let layers = if branch == MAIN {
            vec![version.root(MAIN_TABLE)]
        } else {
            self.chain(branch)?.iter().map(|b| version.root(&branch_table(b))).collect()
        };
        Ok(Snapshot { pages: self.pages.clone(), version, layers })
    }

    /// A new, never used table generation.
    pub(crate) fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Commits the changes of `branch`: `overlay` (and `extra` operations).
    /// `read`: the tables a transaction went by, in a view that held the
    /// commits up to `since`; if another commit wrote one of them after that,
    /// this one is refused ([`Error::Conflict`]). `touched` names the tables
    /// it writes. Returns the commit's number: it is in the log, not yet on
    /// disk (see [`wait_durable`](Self::wait_durable)).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit(
        &self,
        writer: &mut Writer,
        branch: &str,
        overlay: &Overlay,
        extra: Vec<Op>,
        read: &[String],
        since: u64,
        touched: &[String],
    ) -> Result<u64> {
        if !read.is_empty()
            && let Some(written) = lock(&self.written).get(branch)
        {
            if written.reset > since {
                return Err(Error::Conflict(branch.to_owned()));
            }
            if let Some(table) = read.iter().find(|t| written.tables.get(*t).is_some_and(|lsn| *lsn > since)) {
                return Err(Error::Conflict(table.clone()));
            }
        }
        if overlay.is_empty() && extra.is_empty() {
            return Ok(since);
        }
        // Refused before it is in the log: a record the page store cannot apply would stop every restart.
        if overlay.iter().any(|(k, v)| k.len() + v.as_ref().map_or(0, Vec::len) >= MAX_ENTRY) {
            return Err(Error::Validation("a row is too large for Nuage DB (1 GiB at most)".into()));
        }
        let mut ops = Vec::with_capacity(overlay.len() + extra.len());
        let children = self.children_of(branch);
        if !children.is_empty() {
            // Copy-on-write: children keep the values they were made with.
            let before = self.snapshot_latest(branch)?;
            let version = self.pages.version();
            for child in &children {
                let name = branch_table(child);
                let own = version.root(&name);
                for key in overlay.keys() {
                    let has = match &own {
                        Some(root) => self.pages.find(root, key, |_| ())?.is_some(),
                        None => false,
                    };
                    if !has {
                        let old = before.get(None, key)?;
                        ops.push(Op::Put { table: name.clone(), key: key.clone(), value: tagged(old.as_deref()) });
                    }
                }
            }
        }
        let table = branch_table(branch);
        for (key, value) in overlay {
            ops.push(match (branch == MAIN, value) {
                (true, None) => Op::Del { table: table.clone(), key: key.clone() },
                (_, v) => Op::Put { table: table.clone(), key: key.clone(), value: tagged(v.as_deref()) },
            });
        }
        ops.extend(extra);
        // The generation counter goes with the commit that moved it (a new table).
        let generation = self.generation.load(Ordering::SeqCst);
        if generation != self.saved_generation.load(Ordering::SeqCst) {
            ops.push(Op::Put {
                table: META.into(),
                key: GENERATION.to_vec(),
                value: generation.to_le_bytes().to_vec(),
            });
        }
        let lsn = self.log_and_apply(writer, ops)?;
        self.saved_generation.store(generation, Ordering::SeqCst);
        if !touched.is_empty() {
            let mut written = lock(&self.written);
            if !written.contains_key(branch) {
                written.insert(branch.to_owned(), Written::default());
            }
            if let Some(w) = written.get_mut(branch) {
                for t in touched {
                    match w.tables.get_mut(t) {
                        Some(at) => *at = lsn,
                        None => {
                            w.tables.insert(t.clone(), lsn);
                        }
                    }
                }
            }
        }
        Ok(lsn)
    }

    /// Returns once the commit `lsn` is on disk. The log is synced by one
    /// caller at a time, and a sync covers every commit appended before it
    /// started: commits made at the same time wait for the same one.
    pub(crate) fn wait_durable(&self, lsn: u64) -> Result<()> {
        if self.durable.lsn.load(Ordering::SeqCst) >= lsn {
            return Ok(());
        }
        let mut syncing = lock(&self.durable.syncing);
        loop {
            if self.durable.lsn.load(Ordering::SeqCst) >= lsn {
                return Ok(());
            }
            self.log_is_sound()?;
            if *syncing {
                syncing = self.durable.done.wait(syncing).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            *syncing = true;
            drop(syncing);
            // What is applied now is in the log already: this sync covers it.
            // A segment closed meanwhile was synced when it was closed.
            let version = self.pages.version();
            let file = lock(&self.wal_file).clone();
            #[cfg(test)]
            let synced = match self.durable.fail_next.swap(false, Ordering::SeqCst) {
                true => Err(std::io::Error::other("no sync (test)")),
                false => file.sync_data(),
            };
            #[cfg(not(test))]
            let synced = file.sync_data();
            if let Err(e) = &synced {
                let _ = self.durable.failed.set(e.to_string());
            }
            syncing = lock(&self.durable.syncing);
            *syncing = false;
            self.durable.done.notify_all();
            if synced.is_ok() {
                self.on_disk(&version);
            }
        }
    }

    /// An error once a sync of the log has failed: the commits it covered
    /// may not be on disk, and no later sync can tell.
    fn log_is_sound(&self) -> Result<()> {
        match self.durable.failed.get() {
            Some(e) => Err(storage_err(format!(
                "write-ahead log: it could not be made durable ({e}): no commit is accepted until the database \
                 is opened again"
            ))),
            None => Ok(()),
        }
    }

    /// The commits of `version` are on disk: readers see them, then their
    /// callers are told.
    fn on_disk(&self, version: &Arc<Version>) {
        self.pages.show(version);
        self.durable.lsn.fetch_max(version.lsn(), Ordering::SeqCst);
    }

    /// Leaves the log segment being written and starts the next one. The
    /// segment left is synced first, and a sync that fails is final (see
    /// [`Durable::failed`]): what was appended to it may not be on disk.
    fn next_segment(&self, w: &mut Writer) -> Result<()> {
        if let Err(e) = w.wal.file().sync_all() {
            let _ = self.durable.failed.set(e.to_string());
            return self.log_is_sound();
        }
        w.wal.rotate(w.next_lsn)?;
        *lock(&self.wal_file) = w.wal.file();
        Ok(())
    }

    fn log_and_apply(&self, writer: &mut Writer, ops: Vec<Op>) -> Result<u64> {
        self.log_is_sound()?;
        if writer.wal.spent() {
            // A record was cut from this segment: its number is not sealed
            // twice under one key.
            self.next_segment(writer)?;
        }
        let record = Record { lsn: writer.next_lsn, ops };
        let logged = writer.wal.bytes;
        writer.wal.append(&record)?;
        if let Err(e) = self.pages.apply(&record.ops, record.lsn) {
            // Not applied: it must not come back when the log is replayed either.
            let _ = writer.wal.cut_to(logged);
            return Err(e);
        }
        writer.next_lsn += 1;
        writer.commits_since_checkpoint += 1;
        self.applied_lsn.store(record.lsn, Ordering::SeqCst);
        if !self.sync {
            // Nothing is synced: a commit counts as on disk as soon as it is applied.
            self.on_disk(&self.pages.version());
        }
        if self.pages.dirty_bytes() > self.dirty_limit {
            // Much was written since the last checkpoint: the changed nodes go
            // to disk now rather than wait in memory. The log is left as it is.
            // The commit is made whether this succeeds or not: a page store
            // that cannot be written is the next checkpoint's to report.
            let _ = self.flush(writer);
        }
        Ok(record.lsn)
    }

    /// Makes everything applied so far durable in the page store.
    fn flush(&self, w: &mut Writer) -> Result<u64> {
        let lsn = w.next_lsn - 1;
        // The page store holds every commit so far: they are on disk whatever the log's state.
        save(&self.pages, lsn)?;
        self.durable.lsn.fetch_max(lsn, Ordering::SeqCst);
        w.checkpoint_lsn = lsn;
        Ok(lsn)
    }

    /// Takes the writer lock.
    pub(crate) fn writer(&self) -> MutexGuard<'_, Writer> {
        lock(&self.writer)
    }

    /// Makes everything applied so far durable in the page store, starts a
    /// new log segment and removes the segments the checkpoint covers (up to
    /// `keep_after`, the last LSN shipped to the backup, when there is one).
    pub(crate) fn checkpoint(&self, keep_after: Option<u64>) -> Result<u64> {
        let mut w = self.writer();
        self.checkpoint_locked(&mut w, keep_after)
    }

    pub(crate) fn checkpoint_locked(&self, w: &mut Writer, keep_after: Option<u64>) -> Result<u64> {
        let lsn = self.flush(w)?;
        w.last_checkpoint = Some(SystemTime::now());
        w.commits_since_checkpoint = 0;
        if w.wal.bytes > 0 || w.wal.spent() {
            self.next_segment(w)?;
        }
        let limit = keep_after.map_or(lsn, |k| k.min(lsn));
        let segments = wal::segments(&self.dir.join("wal"))?;
        for (i, (first, path)) in segments.iter().enumerate() {
            // A segment ends where the next one starts.
            let Some((next_first, _)) = segments.get(i + 1) else { break };
            if *first != w.wal.first_lsn && next_first - 1 <= limit {
                let _ = fs::remove_file(path);
            }
        }
        wal::sync_dir(&self.dir.join("wal"));
        Ok(lsn)
    }

    /// Registers a branch of `parent`.
    pub(crate) fn create_branch(&self, name: &str, parent: &str) -> Result<BranchInfo> {
        let mut w = self.writer();
        if self.has_branch(name) {
            return Err(Error::TableExists(name.to_owned()));
        }
        if !self.has_branch(parent) {
            return Err(Error::NotFound);
        }
        let info = BranchInfo { name: name.to_owned(), parent: parent.to_owned(), created_at: now_millis() };
        let mut key = BRANCH_PREFIX.to_vec();
        key.extend_from_slice(name.as_bytes());
        let value = json!({ "parent": parent, "createdAt": info.created_at }).to_string().into_bytes();
        let lsn = self.log_and_apply(&mut w, vec![Op::Put { table: META.into(), key, value }])?;
        self.branches.write().unwrap_or_else(|e| e.into_inner()).insert(name.to_owned(), info.clone());
        drop(w);
        self.wait_durable(lsn)?;
        Ok(info)
    }

    /// Deletes a branch (`reset`: only its changes, it stays registered).
    pub(crate) fn drop_branch(&self, name: &str, reset: bool) -> Result<bool> {
        let mut w = self.writer();
        if name == MAIN || !self.has_branch(name) {
            return Ok(false);
        }
        if !self.children_of(name).is_empty() {
            return Err(Error::Unsupported(format!("branch `{name}` has branches of its own: delete them first")));
        }
        let mut ops = vec![Op::DropTable { table: branch_table(name) }];
        if !reset {
            let mut key = BRANCH_PREFIX.to_vec();
            key.extend_from_slice(name.as_bytes());
            ops.push(Op::Del { table: META.into(), key });
        }
        let lsn = self.log_and_apply(&mut w, ops)?;
        if !reset {
            self.branches.write().unwrap_or_else(|e| e.into_inner()).remove(name);
        }
        // Transactions open on the branch must not commit over the change.
        lock(&self.written).insert(name.to_owned(), Written { tables: HashMap::new(), reset: lsn });
        drop(w);
        self.wait_durable(lsn)?;
        Ok(true)
    }

    /// Entries of the store's own metadata whose key starts with `prefix`
    /// (the server keeps its list of applications there).
    pub(crate) fn meta_entries(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        if let Some(root) = self.pages.version().root(META) {
            let end = super::keys::prefix_end(prefix);
            self.pages.walk(&root, prefix, &end, &mut |k, v| {
                out.push((k.to_vec(), v.to_vec()));
                true
            })?;
        }
        Ok(out)
    }

    /// Writes (`Some`) or removes (`None`) a metadata entry, through the log.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn set_meta(&self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        let mut w = self.writer();
        let op = match value {
            Some(v) => Op::Put { table: META.into(), key: key.to_vec(), value: v.to_vec() },
            None => Op::Del { table: META.into(), key: key.to_vec() },
        };
        let lsn = self.log_and_apply(&mut w, vec![op])?;
        drop(w);
        self.wait_durable(lsn)
    }

    /// Bytes stored by a branch's own tree.
    pub(crate) fn branch_bytes(&self, branch: &str) -> u64 {
        self.pages.version().bytes(&branch_table(branch))
    }

    pub(crate) fn applied_lsn(&self) -> u64 {
        self.applied_lsn.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use crate::{Embedded, EngineOptions};

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nuage-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn count(db: &Embedded) -> String {
        format!("{:?}", db.sql("SELECT count(*) FROM t", &[]).unwrap()[0].rows[0][0])
    }

    /// A sync of the log that fails is not tried again on the same file: the
    /// commit is refused, and so is every one after it, until the database is
    /// opened again. What was durable before stays.
    #[test]
    fn a_failed_sync_of_the_log_stops_the_commits() {
        let dir = temp("sync");
        {
            let db = Embedded::open(&dir).unwrap();
            db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
            db.sql("INSERT INTO t (id, v) VALUES (1, 'kept')", &[]).unwrap();
            let store = &db.engine().store;
            store.durable.fail_next.store(true, Ordering::SeqCst);
            let e = db.sql("INSERT INTO t (id, v) VALUES (2, 'not synced')", &[]).unwrap_err().to_string();
            assert!(e.contains("write-ahead log"), "{e}");
            // The next sync would succeed: it is not trusted.
            assert!(!store.durable.fail_next.load(Ordering::SeqCst));
            let e = db.sql("INSERT INTO t (id, v) VALUES (3, 'refused')", &[]).unwrap_err().to_string();
            assert!(e.contains("no commit is accepted until the database is opened again"), "{e}");
            let e = db.engine().create_branch("b", None).unwrap_err().to_string();
            assert!(e.contains("no commit is accepted"), "{e}");
            // Readers were never shown the commit whose sync failed.
            assert_eq!(count(&db), "I64(1)");
        }
        let db = open_again(&dir, EngineOptions::default());
        db.sql("INSERT INTO t (id, v) VALUES (4, 'after')", &[]).unwrap();
        assert_eq!(db.sql("SELECT count(*) FROM t WHERE id IN (1, 4)", &[]).unwrap()[0].rows[0][0].to_string(), "2");
        assert!(db.sql("SELECT count(*) FROM t WHERE id = 3", &[]).unwrap()[0].rows[0][0].to_string() == "0");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The log of `dir` gets one more segment: a record per `(lsn, bytes)`,
    /// each a metadata entry `replayed/<lsn>` of that many bytes.
    fn append_log(dir: &std::path::Path, records: &[(u64, usize)]) {
        let mut log = super::wal::Writer::open(&dir.join("wal"), records[0].0, None).unwrap();
        for (lsn, bytes) in records {
            let key = format!("replayed/{lsn:06}").into_bytes();
            let op = super::Op::Put { table: super::META.into(), key, value: vec![*lsn as u8; *bytes] };
            log.append(&super::Record { lsn: *lsn, ops: vec![op] }).unwrap();
        }
        log.file().sync_all().unwrap();
    }

    /// Opens `dir` once the handle dropped just before has let go of it.
    fn open_again(dir: &std::path::Path, options: EngineOptions) -> Embedded {
        (0..400)
            .find_map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                Embedded::open_with(dir, options.clone()).ok()
            })
            .expect("opens again")
    }

    /// A log much longer than what the page store keeps in memory is
    /// replayed whole: its changes go to disk on the way.
    #[test]
    fn a_long_log_is_replayed() {
        let dir = temp("replay");
        let options = EngineOptions { cache_bytes: 1 << 20, sync: false, ..EngineOptions::default() };
        let last = {
            let db = Embedded::open_with(&dir, options.clone()).unwrap();
            db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
            db.engine().checkpoint().unwrap()
        };
        // 12 MiB of commits the page store does not hold, for a limit of 1 MiB.
        let records: Vec<(u64, usize)> = (1..=192).map(|i| (last + i, 64 << 10)).collect();
        append_log(&dir, &records);
        let db = open_again(&dir, options);
        let store = &db.engine().store;
        assert_eq!(store.log_gap, None);
        let entries = store.meta_entries(b"replayed/").unwrap();
        assert_eq!(entries.len(), 192);
        assert!(entries.iter().zip(&records).all(|((_, v), (lsn, n))| v.len() == *n && v[0] == *lsn as u8));
        assert_eq!(store.applied_lsn(), last + 192);
        assert_eq!(count(&db), "I64(0)");
        assert!(db.engine().check_integrity().unwrap());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Commits missing in the middle of the log are told: what the log holds
    /// is applied, and an event says which ones are not there.
    #[test]
    fn a_gap_in_the_log_is_told() {
        let dir = temp("gap");
        let last = {
            let db = Embedded::open(&dir).unwrap();
            db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
            db.engine().checkpoint().unwrap()
        };
        append_log(&dir, &[(last + 1, 10), (last + 2, 10), (last + 5, 10), (last + 6, 10)]);
        let db = open_again(&dir, EngineOptions::default());
        assert_eq!(db.engine().store.log_gap, Some((last + 2, last + 5)));
        assert_eq!(db.engine().store.meta_entries(b"replayed/").unwrap().len(), 4);
        let (events, _) = db.engine().events();
        let told = events.iter().find(|e| e.level == "error").map(|e| e.message.clone()).unwrap_or_default();
        assert!(told.contains(&format!("commit {} then commit {}", last + 2, last + 5)), "{events:?}");
        drop(db);
        // The next opening starts from a checkpoint: nothing to tell any more.
        let db = open_again(&dir, EngineOptions::default());
        assert_eq!(db.engine().store.log_gap, None);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record cut from an encrypted log (its commit could not be applied)
    /// moves the log to another segment, under another key: the number the
    /// record had is sealed once per key. Both segments are replayed.
    #[test]
    fn a_record_cut_from_an_encrypted_log_starts_another_segment() {
        let (dir, copy) = (temp("rekey"), temp("rekey-copy"));
        let options =
            EngineOptions { encryption: Some(crate::Encryption::from_key([9; 32])), ..EngineOptions::default() };
        let db = Embedded::open_with(&dir, options.clone()).unwrap();
        db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        db.engine().checkpoint().unwrap();
        db.sql("INSERT INTO t (id, v) VALUES (1, 'before')", &[]).unwrap();
        let segments = || super::wal::segments(&dir.join("wal")).unwrap().len();
        let before = segments();
        {
            // What a commit that was logged and could not be applied leaves.
            let mut w = db.engine().store.writer();
            let whole = w.wal.bytes;
            w.wal.cut_to(whole).unwrap();
            assert!(w.wal.spent());
        }
        db.sql("INSERT INTO t (id, v) VALUES (2, 'after')", &[]).unwrap();
        assert_eq!(segments(), before + 1);
        assert!(!db.engine().store.writer().wal.spent());
        // As a crash would leave it: the page store of the last checkpoint, and the log.
        std::fs::create_dir_all(copy.join("wal")).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap().chain(std::fs::read_dir(dir.join("wal")).unwrap()).flatten() {
            if entry.path().is_file() {
                std::fs::copy(entry.path(), copy.join(entry.path().strip_prefix(&dir).unwrap())).unwrap();
            }
        }
        let again = Embedded::open_with(&copy, options).unwrap();
        assert_eq!(again.engine().store.log_gap, None);
        assert_eq!(count(&again), "I64(2)");
        drop((db, again));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&copy);
    }

    /// Past the limit of changed nodes kept in memory, a commit writes them
    /// to the page store. When that fails, the commit is made all the same:
    /// it is in the log and applied.
    #[test]
    fn a_commit_is_made_when_the_page_store_cannot_be_written() {
        let dir = temp("flush");
        let options = EngineOptions { cache_bytes: 1 << 20, sync: false, ..EngineOptions::default() };
        let db = Embedded::open_with(&dir, options).unwrap();
        db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        let store = &db.engine().store;
        store.pages.fail_checkpoints.store(true, Ordering::Relaxed);
        let body = "x".repeat(64 << 10);
        for id in 0..40 {
            db.sql("INSERT INTO t (id, v) VALUES (?, ?)", &[id.into(), body.clone().into()]).unwrap();
        }
        assert!(store.pages.dirty_bytes() > store.dirty_limit, "past the limit: every commit tried to flush");
        assert_eq!(count(&db), "I64(40)");
        assert!(db.engine().checkpoint().is_err());
        store.pages.fail_checkpoints.store(false, Ordering::Relaxed);
        db.engine().checkpoint().unwrap();
        assert_eq!(store.pages.dirty_bytes(), 0);
        assert_eq!(count(&db), "I64(40)");
        assert!(db.engine().check_integrity().unwrap());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
