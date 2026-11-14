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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::SystemTime;

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::exec::TableMeta;
use super::pages::{Node, Pages, Version};
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
    _version: Arc<Version>,
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
    /// Version of each (branch, table) for optimistic transactions.
    versions: Mutex<HashMap<(String, String), u64>>,
    /// LSN of the last applied commit (what snapshots see).
    applied_lsn: AtomicU64,
    /// Catalog entries already parsed, by their stored bytes (see [`Store::parsed`]).
    catalog: Mutex<HashMap<Vec<u8>, Arc<Parsed>>>,
    /// Bytes of changed nodes the page store may keep in memory before they
    /// are written out, whatever the checkpoint interval.
    dirty_limit: usize,
}

/// A catalog entry parsed once: its table metadata and, when a statement asked for it,
/// its `'static` schema.
pub(crate) struct Parsed {
    pub meta: Arc<TableMeta>,
    pub schema: OnceLock<&'static memoire_orm::TableSchema>,
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
        let records = wal::recover(&wal_dir, keys.map(|k| k.wal.as_ref()))?;
        let dirty_limit = (cache_bytes / 4).max(1 << 20);
        let mut last = checkpoint_lsn;
        for r in records.iter().filter(|r| r.lsn > checkpoint_lsn) {
            pages.apply(&r.ops)?;
            last = last.max(r.lsn);
        }
        let generation = u64_of(read_meta(&pages, GENERATION)?);
        let store = Store {
            pages: Arc::new(pages),
            dir: dir.to_owned(),
            writer: Mutex::new(Writer {
                wal: wal::Writer::open(&wal_dir, last + 1, sync, keys.map(|k| k.wal.clone()))?,
                next_lsn: last + 1,
                checkpoint_lsn,
                last_checkpoint: None,
                commits_since_checkpoint: 0,
            }),
            branches: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(generation),
            versions: Mutex::new(HashMap::new()),
            applied_lsn: AtomicU64::new(last),
            catalog: Mutex::new(HashMap::new()),
            dirty_limit,
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
        if let Some(p) = lock(&self.catalog).get(&bytes) {
            return Ok(p.clone());
        }
        let parsed = Arc::new(Parsed { meta: Arc::new(TableMeta::from_json(&bytes)?), schema: OnceLock::new() });
        let mut cache = lock(&self.catalog);
        if cache.len() >= CATALOG_CACHE {
            cache.clear();
        }
        cache.insert(bytes, parsed.clone());
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

    /// A read view of `branch` as of now.
    pub(crate) fn snapshot(&self, branch: &str) -> Result<Snapshot> {
        if branch == MAIN {
            let version = self.pages.version();
            let layers = vec![version.root(MAIN_TABLE)];
            return Ok(Snapshot { pages: self.pages.clone(), _version: version, layers });
        }
        let chain = self.chain(branch)?;
        let version = self.pages.version();
        let layers = chain.iter().map(|b| version.root(&branch_table(b))).collect();
        Ok(Snapshot { pages: self.pages.clone(), _version: version, layers })
    }

    /// A new, never used table generation.
    pub(crate) fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Current version of a table of a branch (bumped by every commit to it).
    pub(crate) fn version(&self, branch: &str, table: &str) -> u64 {
        lock(&self.versions).get(&(branch.to_owned(), table.to_owned())).copied().unwrap_or(0)
    }

    /// Commits the changes of `branch`: `overlay` (and `extra` operations),
    /// after checking that the tables in `expect` are still at the given
    /// versions. `touched` names the tables whose version moves.
    pub(crate) fn commit(
        &self,
        writer: &mut Writer,
        branch: &str,
        overlay: &Overlay,
        extra: Vec<Op>,
        expect: &[(String, u64)],
        touched: &[String],
    ) -> Result<u64> {
        {
            let versions = lock(&self.versions);
            for (table, v) in expect {
                if versions.get(&(branch.to_owned(), table.clone())).copied().unwrap_or(0) != *v {
                    return Err(Error::Conflict(table.clone()));
                }
            }
        }
        if overlay.is_empty() && extra.is_empty() {
            return Ok(writer.next_lsn - 1);
        }
        let mut ops = Vec::with_capacity(overlay.len() + extra.len());
        let children = self.children_of(branch);
        if !children.is_empty() {
            // Copy-on-write: children keep the values they were made with.
            let before = self.snapshot(branch)?;
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
        ops.push(Op::Put {
            table: META.into(),
            key: GENERATION.to_vec(),
            value: self.generation.load(Ordering::SeqCst).to_le_bytes().to_vec(),
        });
        let lsn = self.log_and_apply(writer, ops)?;
        let mut versions = lock(&self.versions);
        for t in touched {
            *versions.entry((branch.to_owned(), t.clone())).or_insert(0) += 1;
        }
        Ok(lsn)
    }

    fn log_and_apply(&self, writer: &mut Writer, ops: Vec<Op>) -> Result<u64> {
        let record = Record { lsn: writer.next_lsn, ops };
        writer.wal.append(&record)?;
        writer.next_lsn += 1;
        writer.commits_since_checkpoint += 1;
        self.pages.apply(&record.ops)?;
        self.applied_lsn.store(record.lsn, Ordering::SeqCst);
        if self.pages.dirty_bytes() > self.dirty_limit {
            // Much was written since the last checkpoint: the changed nodes go
            // to disk now rather than wait in memory. The log is left as it is.
            self.flush(writer)?;
        }
        Ok(record.lsn)
    }

    /// Makes everything applied so far durable in the page store.
    fn flush(&self, w: &mut Writer) -> Result<u64> {
        let lsn = w.next_lsn - 1;
        self.pages.apply(&[Op::Put {
            table: META.into(),
            key: CHECKPOINT_LSN.to_vec(),
            value: lsn.to_le_bytes().to_vec(),
        }])?;
        self.pages.checkpoint()?;
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
        if w.wal.bytes > 0 {
            w.wal.rotate(w.next_lsn)?;
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
        self.log_and_apply(&mut w, vec![Op::Put { table: META.into(), key, value }])?;
        self.branches.write().unwrap_or_else(|e| e.into_inner()).insert(name.to_owned(), info.clone());
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
        self.log_and_apply(&mut w, ops)?;
        if !reset {
            self.branches.write().unwrap_or_else(|e| e.into_inner()).remove(name);
        }
        // Transactions open on the branch must not commit over the change.
        lock(&self.versions).retain(|(b, _), v| {
            if b == name {
                *v += 1_000_000;
            }
            true
        });
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
        self.log_and_apply(&mut w, vec![op]).map(|_| ())
    }

    /// Bytes stored by a branch's own tree.
    pub(crate) fn branch_bytes(&self, branch: &str) -> u64 {
        self.pages.version().bytes(&branch_table(branch))
    }

    pub(crate) fn applied_lsn(&self) -> u64 {
        self.applied_lsn.load(Ordering::SeqCst)
    }
}
