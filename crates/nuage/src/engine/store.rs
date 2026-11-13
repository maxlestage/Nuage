//! The page store: redb B-trees on disk, one per branch, behind the
//! write-ahead log.
//!
//! A branch is an overlay: its tree holds only the keys it changed (values
//! or tombstones). Reading a branch reads its tree, then its parent's, up to
//! the main tree. When a key changes in a branch that has children, the old
//! value is first copied into every child that does not have its own: the
//! children keep reading the data as it was when they were made
//! (copy-on-write).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};
use std::time::SystemTime;

use memoire_orm::{Error, Result};
use redb::{Durability, ReadOnlyTable, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde_json::{Value as Json, json};

use super::storage_err;
use super::wal::{self, Op, Record};
use crate::crypto::{EncryptedFile, Keys};

pub(crate) const MAIN: &str = "main";
/// The page store's file in a data directory.
pub(crate) const FILE: &str = "nuage.redb";
pub(crate) const META: &str = "meta";
const CHECKPOINT_LSN: &[u8] = b"checkpoint_lsn";
const GENERATION: &[u8] = b"generation";
const BRANCH_PREFIX: &[u8] = b"branch:";

type Def<'a> = TableDefinition<'a, &'static [u8], &'static [u8]>;

fn def(name: &str) -> Def<'_> {
    TableDefinition::new(name)
}

/// The store table of a branch.
pub(crate) fn branch_table(branch: &str) -> String {
    format!("b:{branch}")
}

fn redb_err(e: impl std::fmt::Display) -> Error {
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

/// A consistent read view of one branch (a redb read transaction).
pub(crate) struct Snapshot {
    /// The branch's tree, then its ancestors', main last. `None`: not
    /// created yet (a branch that never wrote anything).
    layers: Vec<Option<ReadOnlyTable<&'static [u8], &'static [u8]>>>,
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
            if let Some(v) = layer.get(key).map_err(redb_err)? {
                return Ok(stored(v.value()));
            }
        }
        Ok(None)
    }

    /// Up to `limit` live entries in `start..end`, in key order, merging the
    /// layers (the closest one wins). Returns them and where to continue
    /// (`None`: the range is exhausted).
    pub(crate) fn scan(&self, overlay: Option<&Overlay>, start: &[u8], end: &[u8], limit: usize) -> Result<Scanned> {
        let limit = limit.max(1);
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
            let mut last = None;
            for (n, item) in layer.range(start..end).map_err(redb_err)?.enumerate() {
                let (k, v) = item.map_err(redb_err)?;
                if n == limit {
                    cut(last.take().unwrap_or_default(), &mut boundary);
                    break;
                }
                let k = k.value().to_vec();
                merged.insert(k.clone(), stored(v.value()));
                last = Some(k);
            }
        }
        if let Some(o) = overlay {
            let mut last = None;
            for (n, (k, v)) in o.range(start.to_vec()..end.to_vec()).enumerate() {
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

/// redb, the write-ahead log and the branch registry.
pub(crate) struct Store {
    pub db: RwLock<redb::Database>,
    pub dir: PathBuf,
    pub writer: Mutex<Writer>,
    branches: RwLock<HashMap<String, BranchInfo>>,
    generation: AtomicU64,
    /// Version of each (branch, table) for optimistic transactions.
    versions: Mutex<HashMap<(String, String), u64>>,
    /// LSN of the last applied commit (what snapshots see).
    applied_lsn: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Applies operations in one redb write transaction.
fn apply(db: &redb::Database, ops: &[Op], durable: bool) -> Result<()> {
    let mut tx = db.begin_write().map_err(redb_err)?;
    if !durable {
        tx.set_durability(Durability::None).map_err(redb_err)?;
    }
    {
        let mut tables: HashMap<&str, redb::Table<'_, &'static [u8], &'static [u8]>> = HashMap::new();
        for op in ops {
            match op {
                Op::DropTable { table } => {
                    tables.remove(table.as_str());
                    tx.delete_table(def(table)).map_err(redb_err)?;
                }
                Op::Put { table, .. } | Op::Del { table, .. } | Op::DelRange { table, .. } => {
                    if !tables.contains_key(table.as_str()) {
                        let t = tx.open_table(def(table)).map_err(redb_err)?;
                        tables.insert(table.as_str(), t);
                    }
                    let t = tables.get_mut(table.as_str()).ok_or_else(|| storage_err("table vanished"))?;
                    match op {
                        Op::Put { key, value, .. } => {
                            t.insert(key.as_slice(), value.as_slice()).map_err(redb_err)?;
                        }
                        Op::Del { key, .. } => {
                            t.remove(key.as_slice()).map_err(redb_err)?;
                        }
                        Op::DelRange { start, end, .. } => {
                            t.retain_in(start.as_slice()..end.as_slice(), |_, _| false).map_err(redb_err)?;
                        }
                        Op::DropTable { .. } => {}
                    }
                }
            }
        }
    }
    tx.commit().map_err(redb_err)
}

/// Opens (or creates) a page store file, encrypted with `keys` if any.
pub(crate) fn open_redb(path: &Path, cache_bytes: usize, keys: Option<&Keys>) -> Result<redb::Database> {
    let builder = {
        let mut b = redb::Builder::new();
        b.set_cache_size(cache_bytes);
        b
    };
    match keys {
        Some(k) => {
            let file = EncryptedFile::open(path, k.pages.clone())
                .map_err(|e| storage_err(format!("cannot open {}: {e}", path.display())))?;
            builder.create_with_backend(file).map_err(|e| {
                storage_err(format!(
                    "cannot open the encrypted page store {} ({e}): is it really encrypted with this key?",
                    path.display()
                ))
            })
        }
        None => builder.create(path).map_err(redb_err),
    }
}

/// Copies every table of `from` into `to` (a page store encrypted, or
/// re-encrypted). Both are closed by the caller.
pub(crate) fn copy_tables(from: &redb::Database, to: &redb::Database) -> Result<()> {
    use redb::TableHandle;
    let r = from.begin_read().map_err(redb_err)?;
    let w = to.begin_write().map_err(redb_err)?;
    for handle in r.list_tables().map_err(redb_err)? {
        let name = handle.name().to_owned();
        let src = r.open_table(def(&name)).map_err(redb_err)?;
        let mut dst = w.open_table(def(&name)).map_err(redb_err)?;
        for item in src.iter().map_err(redb_err)? {
            let (k, v) = item.map_err(redb_err)?;
            dst.insert(k.value(), v.value()).map_err(redb_err)?;
        }
    }
    w.commit().map_err(redb_err)
}

fn read_meta(db: &redb::Database, key: &[u8]) -> Result<Option<Vec<u8>>> {
    let tx = db.begin_read().map_err(redb_err)?;
    let table = match tx.open_table(def(META)) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(redb_err(e)),
    };
    Ok(table.get(key).map_err(redb_err)?.map(|v| v.value().to_vec()))
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
        let db = open_redb(&dir.join(FILE), cache_bytes, keys)?;
        let checkpoint_lsn = u64_of(read_meta(&db, CHECKPOINT_LSN)?);
        let wal_dir = dir.join("wal");
        let records = wal::recover(&wal_dir, keys.map(|k| k.wal.as_ref()))?;
        let mut last = checkpoint_lsn;
        for r in records.iter().filter(|r| r.lsn > checkpoint_lsn) {
            apply(&db, &r.ops, false)?;
            last = last.max(r.lsn);
        }
        let generation = u64_of(read_meta(&db, GENERATION)?);
        let store = Store {
            db: RwLock::new(db),
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
        };
        store.load_branches()?;
        store.checkpoint(None)?;
        Ok(store)
    }

    fn load_branches(&self) -> Result<()> {
        let db = self.db.read().unwrap_or_else(|e| e.into_inner());
        let tx = db.begin_read().map_err(redb_err)?;
        let mut map = HashMap::new();
        if let Ok(table) = tx.open_table(def(META)) {
            let end = super::keys::prefix_end(BRANCH_PREFIX);
            for item in table.range(BRANCH_PREFIX..end.as_slice()).map_err(redb_err)? {
                let (k, v) = item.map_err(redb_err)?;
                let name = String::from_utf8_lossy(&k.value()[BRANCH_PREFIX.len()..]).into_owned();
                let j: Json = serde_json::from_slice(v.value()).map_err(redb_err)?;
                map.insert(
                    name.clone(),
                    BranchInfo {
                        name,
                        parent: j.get("parent").and_then(Json::as_str).unwrap_or(MAIN).to_owned(),
                        created_at: j.get("createdAt").and_then(Json::as_i64).unwrap_or(0),
                    },
                );
            }
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
        let chain = self.chain(branch)?;
        let db = self.db.read().unwrap_or_else(|e| e.into_inner());
        let lsn = self.applied_lsn.load(Ordering::SeqCst);
        let tx = db.begin_read().map_err(redb_err)?;
        let mut layers = Vec::with_capacity(chain.len());
        for b in &chain {
            let name = branch_table(b);
            layers.push(match tx.open_table(def(&name)) {
                Ok(t) => Some(t),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(redb_err(e)),
            });
        }
        let _ = lsn;
        Ok(Snapshot { layers })
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
            let db = self.db.read().unwrap_or_else(|e| e.into_inner());
            let tx = db.begin_read().map_err(redb_err)?;
            for child in &children {
                let name = branch_table(child);
                let table = match tx.open_table(def(&name)) {
                    Ok(t) => Some(t),
                    Err(redb::TableError::TableDoesNotExist(_)) => None,
                    Err(e) => return Err(redb_err(e)),
                };
                for key in overlay.keys() {
                    let own = match &table {
                        Some(t) => t.get(key.as_slice()).map_err(redb_err)?.is_some(),
                        None => false,
                    };
                    if !own {
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
        apply(&self.db.read().unwrap_or_else(|e| e.into_inner()), &record.ops, false)?;
        self.applied_lsn.store(record.lsn, Ordering::SeqCst);
        Ok(record.lsn)
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
        let lsn = w.next_lsn - 1;
        let ops = vec![Op::Put { table: META.into(), key: CHECKPOINT_LSN.to_vec(), value: lsn.to_le_bytes().to_vec() }];
        apply(&self.db.read().unwrap_or_else(|e| e.into_inner()), &ops, true)?;
        w.checkpoint_lsn = lsn;
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

    /// Bytes stored by a branch's own tree.
    pub(crate) fn branch_bytes(&self, branch: &str) -> u64 {
        let db = self.db.read().unwrap_or_else(|e| e.into_inner());
        let Ok(tx) = db.begin_read() else { return 0 };
        tx.open_table(def(&branch_table(branch))).ok().and_then(|t| t.stats().ok()).map_or(0, |s| s.stored_bytes())
    }

    pub(crate) fn applied_lsn(&self) -> u64 {
        self.applied_lsn.load(Ordering::SeqCst)
    }
}
