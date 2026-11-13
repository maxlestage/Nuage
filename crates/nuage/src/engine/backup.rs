//! Backups to a second location, and restore at startup.
//!
//! A backup is a **snapshot** of the page store, taken right after a
//! checkpoint, plus the **log segments** written since, shipped as they are
//! sealed (at each checkpoint). Restoring downloads the latest snapshot and
//! the segments after it; opening the store then replays them. Commits made
//! since the last shipped segment are not in the backup: at most one
//! checkpoint interval.
//!
//! Layout in the backup location:
//!
//! | Object | Content |
//! |---|---|
//! | `manifest.json` | latest snapshot and its LSN |
//! | `encryption.json` | the header of an encrypted database (salt, check value: no key) |
//! | `snapshots/<lsn>.redb` | page store files |
//! | `wal/<first lsn>.log` | log segments |

use std::fs;
use std::path::{Path, PathBuf};

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::storage_err;
use super::store::{FILE, Store, now_millis};
use super::wal::{self, segment_lsn, segment_name};
use crate::crypto::HEADER_FILE;

/// Where backups go: a directory, object storage...
///
/// Implementations are called from the engine's maintenance thread, never
/// from a request.
pub trait BackupStore: Send + Sync {
    /// Where it stores, for logs and the console (`/mnt/backup`,
    /// `s3://bucket/prefix`). Never includes a secret.
    fn describe(&self) -> String;
    /// Stores an object.
    fn put(&self, name: &str, data: &[u8]) -> Result<()>;
    /// Stores a file as an object (the default reads it whole).
    fn put_file(&self, name: &str, path: &Path) -> Result<()> {
        let data = fs::read(path).map_err(|e| storage_err(format!("cannot read {}: {e}", path.display())))?;
        self.put(name, &data)
    }
    /// Reads an object, `None` when it does not exist.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>>;
    /// Names of the objects starting with `prefix`.
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
    /// Removes an object (no error when it does not exist).
    fn delete(&self, name: &str) -> Result<()>;
}

/// Backups in a directory (another disk, a mounted network volume...).
#[derive(Clone, Debug)]
pub struct DirectoryStore {
    root: PathBuf,
}

impl DirectoryStore {
    /// Backups under `root`, created if needed.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DirectoryStore { root: root.into() }
    }

    fn path(&self, name: &str) -> Result<PathBuf> {
        if name.split('/').any(|p| p == ".." || p.is_empty()) {
            return Err(storage_err(format!("invalid backup object name `{name}`")));
        }
        Ok(self.root.join(name))
    }
}

fn io(e: std::io::Error) -> Error {
    storage_err(format!("backup directory: {e}"))
}

impl BackupStore for DirectoryStore {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn put(&self, name: &str, data: &[u8]) -> Result<()> {
        let path = self.path(name)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io)?;
        }
        // Written aside, then renamed: a reader never sees half an object.
        let tmp = path.with_extension("partial");
        fs::write(&tmp, data).map_err(io)?;
        fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(io)?;
        fs::rename(&tmp, &path).map_err(io)
    }

    fn put_file(&self, name: &str, source: &Path) -> Result<()> {
        let path = self.path(name)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = path.with_extension("partial");
        fs::copy(source, &tmp).map_err(io)?;
        fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(io)?;
        fs::rename(&tmp, &path).map_err(io)
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(name)?) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let (dir, file_prefix) = match prefix.rsplit_once('/') {
            Some((d, f)) => (self.root.join(d), f),
            None => (self.root.clone(), prefix),
        };
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else { return Ok(out) };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(file_prefix) && !name.ends_with(".partial") {
                let base = prefix.rsplit_once('/').map_or(String::new(), |(d, _)| format!("{d}/"));
                out.push(format!("{base}{name}"));
            }
        }
        out.sort();
        Ok(out)
    }

    fn delete(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.path(name)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(e)),
        }
    }
}

const MANIFEST: &str = "manifest.json";

/// Ships the sealed log segments not shipped yet. Returns the last LSN they
/// cover (everything up to it is in the backup).
pub(crate) fn ship_segments(store: &Store, backup: &dyn BackupStore, shipped: u64) -> Result<u64> {
    let segments = wal::segments(&store.dir.join("wal"))?;
    let current = store.writer().wal.first_lsn;
    let mut last = shipped;
    for (i, (first, path)) in segments.iter().enumerate() {
        let Some((next_first, _)) = segments.get(i + 1) else { break };
        if *first == current || next_first - 1 <= shipped {
            continue;
        }
        backup.put_file(&format!("wal/{}", segment_name(*first)), path)?;
        last = last.max(next_first - 1);
    }
    Ok(last)
}

/// Takes a snapshot: checkpoint, copy of the page store, upload, manifest.
/// Older snapshots and the segments before them are pruned, keeping `keep`.
pub(crate) fn snapshot(store: &Store, backup: &dyn BackupStore, keep: usize) -> Result<u64> {
    let local = store.dir.join("snapshot.partial");
    let lsn = {
        // No commit may land while the file is copied.
        let mut w = store.writer();
        let lsn = store.checkpoint_locked(&mut w, Some(0))?;
        fs::copy(store.dir.join(FILE), &local).map_err(io)?;
        lsn
    };
    let name = format!("snapshots/{lsn:020}.redb");
    let uploaded = backup.put_file(&name, &local);
    let _ = fs::remove_file(&local);
    uploaded?;
    // An encrypted snapshot needs its header (salt, check value) to be read.
    let header = fs::read(store.dir.join(HEADER_FILE)).ok();
    if let Some(h) = &header {
        backup.put(HEADER_FILE, h)?;
    }
    let manifest = json!({ "snapshot": name, "lsn": lsn, "at": now_millis(), "encrypted": header.is_some() });
    backup.put(MANIFEST, manifest.to_string().as_bytes())?;
    let mut snapshots = backup.list("snapshots/")?;
    snapshots.sort();
    if snapshots.len() > keep.max(1) {
        let cut = snapshots.len() - keep.max(1);
        for old in &snapshots[..cut] {
            backup.delete(old)?;
        }
        // Segments that end before the oldest kept snapshot are useless.
        let oldest: u64 =
            snapshots[cut].trim_start_matches("snapshots/").trim_end_matches(".redb").parse().unwrap_or(0);
        let mut segs: Vec<(u64, String)> = backup
            .list("wal/")?
            .into_iter()
            .filter_map(|n| Some((segment_lsn(n.trim_start_matches("wal/"))?, n)))
            .collect();
        segs.sort();
        for i in 0..segs.len() {
            if let Some((next, _)) = segs.get(i + 1)
                && next - 1 <= oldest
            {
                backup.delete(&segs[i].1)?;
            }
        }
    }
    Ok(lsn)
}

/// After a database in clear was encrypted and a snapshot of it taken:
/// removes every older snapshot and the log segments before the latest one,
/// which were in clear.
pub(crate) fn purge_older(backup: &dyn BackupStore) -> Result<()> {
    let Some(manifest) = backup.get(MANIFEST)? else { return Ok(()) };
    let manifest: Json = serde_json::from_slice(&manifest).map_err(|e| storage_err(format!("backup manifest: {e}")))?;
    let latest = manifest.get("snapshot").and_then(Json::as_str).unwrap_or_default().to_owned();
    let lsn = manifest.get("lsn").and_then(Json::as_u64).unwrap_or(0);
    for old in backup.list("snapshots/")? {
        if old != latest {
            backup.delete(&old)?;
        }
    }
    for seg in backup.list("wal/")? {
        if segment_lsn(seg.trim_start_matches("wal/")).is_some_and(|first| first <= lsn) {
            backup.delete(&seg)?;
        }
    }
    Ok(())
}

/// `true` when `dir` holds no database.
pub(crate) fn is_empty(dir: &Path) -> bool {
    !dir.join(FILE).exists()
}

/// Restores the latest backup into `dir`. Returns `false` when there is
/// none.
pub(crate) fn restore(dir: &Path, backup: &dyn BackupStore) -> Result<bool> {
    let Some(manifest) = backup.get(MANIFEST)? else { return Ok(false) };
    let manifest: Json = serde_json::from_slice(&manifest).map_err(|e| storage_err(format!("backup manifest: {e}")))?;
    let name = manifest.get("snapshot").and_then(Json::as_str).ok_or_else(|| storage_err("backup manifest"))?;
    let lsn = manifest.get("lsn").and_then(Json::as_u64).unwrap_or(0);
    let data = backup.get(name)?.ok_or_else(|| storage_err(format!("backup snapshot `{name}` is missing")))?;
    fs::create_dir_all(dir.join("wal")).map_err(io)?;
    // The header first: the snapshot is unreadable without it.
    if manifest.get("encrypted").and_then(Json::as_bool) == Some(true) {
        let header = backup.get(HEADER_FILE)?.ok_or_else(|| storage_err(format!("the backup has no {HEADER_FILE}")))?;
        fs::write(dir.join(HEADER_FILE), header).map_err(io)?;
    }
    let tmp = dir.join("nuage.redb.partial");
    fs::write(&tmp, data).map_err(io)?;
    for seg in backup.list("wal/")? {
        let Some(first) = segment_lsn(seg.trim_start_matches("wal/")) else { continue };
        // Segments that start before the snapshot only hold what it has.
        if first <= lsn {
            continue;
        }
        if let Some(bytes) = backup.get(&seg)? {
            fs::write(dir.join("wal").join(segment_name(first)), bytes).map_err(io)?;
        }
    }
    fs::rename(&tmp, dir.join(FILE)).map_err(io)?;
    wal::sync_dir(dir);
    Ok(true)
}
