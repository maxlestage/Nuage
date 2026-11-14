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
//! | `snapshots/<lsn>.db` | page store files (`.redb`: written by a version up to 0.4, see [`super::legacy`]) |
//! | `wal/<first lsn>.log` | log segments |
//! | `before-import/...` | the last backup of a version up to 0.4, as it was when this version took over (see [`keep_aside`]) |

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::legacy::LEGACY_FILE;
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

    /// Stores the object `name` of what is set aside under `dir`
    /// (`before-import/`): by default the object `dir` + `name`. A store that
    /// is one part of a larger one ([`PrefixedStore`]) sets aside under `dir`
    /// of the whole, at its own place there: `dir`, taken as a backup
    /// location, then has the shape of the location itself, each
    /// application's backup where a server looks for it.
    fn put_aside(&self, dir: &str, name: &str, data: &[u8]) -> Result<()> {
        self.put(&format!("{dir}{name}"), data)
    }
    /// Reads an object set aside under `dir` (see [`put_aside`](Self::put_aside)).
    fn get_aside(&self, dir: &str, name: &str) -> Result<Option<Vec<u8>>> {
        self.get(&format!("{dir}{name}"))
    }
    /// Names (without `dir`) of the objects set aside under `dir` that start with `prefix`.
    fn list_aside(&self, dir: &str, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .list(&format!("{dir}{prefix}"))?
            .into_iter()
            .filter_map(|n| n.strip_prefix(dir).map(str::to_owned))
            .collect())
    }
    /// Removes an object set aside under `dir`.
    fn delete_aside(&self, dir: &str, name: &str) -> Result<()> {
        self.delete(&format!("{dir}{name}"))
    }
    /// Where what is set aside under `dir` is, as [`describe`](Self::describe) tells a place.
    fn describe_aside(&self, dir: &str) -> String {
        format!("{}/{}", self.describe().trim_end_matches('/'), dir.trim_end_matches('/'))
    }
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

/// The objects of a store under a prefix (`apps/<name>/`): the backups of
/// one application next to the others'.
pub struct PrefixedStore {
    inner: Arc<dyn BackupStore>,
    prefix: String,
}

impl PrefixedStore {
    /// `inner`'s objects whose names start with `prefix` (which ends with `/`).
    pub fn new(inner: Arc<dyn BackupStore>, prefix: impl Into<String>) -> Self {
        PrefixedStore { inner, prefix: prefix.into() }
    }
}

impl BackupStore for PrefixedStore {
    fn describe(&self) -> String {
        format!("{}/{}", self.inner.describe().trim_end_matches('/'), self.prefix.trim_end_matches('/'))
    }

    fn put(&self, name: &str, data: &[u8]) -> Result<()> {
        self.inner.put(&format!("{}{name}", self.prefix), data)
    }

    fn put_file(&self, name: &str, path: &Path) -> Result<()> {
        self.inner.put_file(&format!("{}{name}", self.prefix), path)
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(&format!("{}{name}", self.prefix))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .inner
            .list(&format!("{}{prefix}", self.prefix))?
            .into_iter()
            .filter_map(|n| n.strip_prefix(&self.prefix).map(str::to_owned))
            .collect())
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.inner.delete(&format!("{}{name}", self.prefix))
    }

    // Aside under `dir` of the whole store, at this part's place there.
    fn put_aside(&self, dir: &str, name: &str, data: &[u8]) -> Result<()> {
        self.inner.put_aside(dir, &format!("{}{name}", self.prefix), data)
    }

    fn get_aside(&self, dir: &str, name: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get_aside(dir, &format!("{}{name}", self.prefix))
    }

    fn list_aside(&self, dir: &str, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .inner
            .list_aside(dir, &format!("{}{prefix}", self.prefix))?
            .into_iter()
            .filter_map(|n| n.strip_prefix(&self.prefix).map(str::to_owned))
            .collect())
    }

    fn delete_aside(&self, dir: &str, name: &str) -> Result<()> {
        self.inner.delete_aside(dir, &format!("{}{name}", self.prefix))
    }

    fn describe_aside(&self, dir: &str) -> String {
        format!("{}/{}", self.inner.describe_aside(dir), self.prefix.trim_end_matches('/'))
    }
}

/// Removes every object of a backup (a deleted application's).
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) fn delete_all(backup: &dyn BackupStore) -> Result<()> {
    for prefix in ["snapshots/", "wal/"] {
        for name in backup.list(prefix)? {
            backup.delete(&name)?;
        }
        for name in backup.list_aside(BEFORE_IMPORT, prefix)? {
            backup.delete_aside(BEFORE_IMPORT, &name)?;
        }
    }
    for name in [crate::crypto::HEADER_FILE, MANIFEST] {
        backup.delete(name)?;
        backup.delete_aside(BEFORE_IMPORT, name)?;
    }
    Ok(())
}

const MANIFEST: &str = "manifest.json";

/// Where the last backup of a version up to 0.4 is kept once this version
/// writes its own (see [`keep_aside`]).
pub(crate) const BEFORE_IMPORT: &str = "before-import/";

/// Sets the backup of a version up to 0.4 aside, before this version writes
/// its first snapshot over it: its manifest, the snapshot it names, the log
/// segments after that snapshot and the encryption header are copied under
/// `before-import/`, where nothing prunes them. Whatever happens to the data
/// from then on, the database as that version left it can be restored (by
/// that version, or by this one) from `before-import/` taken as the backup
/// location: a server's applications are set aside under it where a server
/// looks for them (`before-import/apps/<name>/`, see
/// [`BackupStore::put_aside`]). To delete once the data has been checked.
///
/// Returns `true` when it copied something: `false` when the backup is of
/// this version, or this very backup was set aside before (one set aside
/// earlier, of an older state, gives way). `encrypted`: the data directory is
/// encrypted; a backup in clear of a directory encrypted since is not kept.
pub(crate) fn keep_aside(backup: &dyn BackupStore, encrypted: bool) -> Result<bool> {
    let Some(manifest) = backup.get(MANIFEST)? else { return Ok(false) };
    let j: Json = serde_json::from_slice(&manifest).map_err(|e| storage_err(format!("backup manifest: {e}")))?;
    let name = j.get("snapshot").and_then(Json::as_str).unwrap_or_default();
    let was_encrypted = j.get("encrypted").and_then(Json::as_bool) == Some(true);
    if !name.ends_with(".redb") || was_encrypted != encrypted {
        return Ok(false);
    }
    let aside = |object: &str, data: &[u8]| backup.put_aside(BEFORE_IMPORT, object, data);
    // The manifest goes last: it is there only when everything it needs is.
    // Another one is of a backup set aside earlier: this one takes its place.
    if backup.get_aside(BEFORE_IMPORT, MANIFEST)?.as_deref() == Some(manifest.as_slice()) {
        return Ok(false);
    }
    let lsn = j.get("lsn").and_then(Json::as_u64).unwrap_or(0);
    let snapshot = backup.get(name)?.ok_or_else(|| storage_err(format!("backup snapshot `{name}` is missing")))?;
    aside(name, &snapshot)?;
    drop(snapshot);
    for segment in backup.list("wal/")? {
        // The segments a restore of that snapshot reads (see `restore`).
        if segment_lsn(segment.trim_start_matches("wal/")).is_some_and(|first| first > lsn)
            && let Some(bytes) = backup.get(&segment)?
        {
            aside(&segment, &bytes)?;
        }
    }
    if was_encrypted {
        let header = backup.get(HEADER_FILE)?.ok_or_else(|| storage_err(format!("the backup has no {HEADER_FILE}")))?;
        aside(HEADER_FILE, &header)?;
    }
    aside(MANIFEST, &manifest)?;
    Ok(true)
}

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
/// The backup of a version up to 0.4 is first set aside ([`keep_aside`]).
/// Returns the snapshot's LSN, and whether an earlier version's backup was
/// set aside now.
pub(crate) fn snapshot(store: &Store, backup: &dyn BackupStore, keep: usize) -> Result<(u64, bool)> {
    let kept = keep_aside(backup, store.dir.join(HEADER_FILE).exists())?;
    let local = store.dir.join("snapshot.partial");
    let lsn = {
        // No commit may land while the file is copied.
        let mut w = store.writer();
        let lsn = store.checkpoint_locked(&mut w, Some(0))?;
        fs::copy(store.dir.join(FILE), &local).map_err(io)?;
        lsn
    };
    let name = format!("snapshots/{lsn:020}.db");
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
    // Oldest first; at the same place in the log, the one of an earlier
    // version (`.redb`) before the one just written.
    snapshots.sort_by_key(|n| (snapshot_lsn(n), n.ends_with(".db")));
    if snapshots.len() > keep.max(1) {
        let cut = snapshots.len() - keep.max(1);
        for old in &snapshots[..cut] {
            backup.delete(old)?;
        }
        // Segments that end before the oldest kept snapshot are useless.
        let oldest = snapshot_lsn(&snapshots[cut]);
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
    Ok((lsn, kept))
}

/// The LSN in the name of a snapshot (`snapshots/<lsn>.db`, or `.redb` for one
/// of a version up to 0.4).
fn snapshot_lsn(name: &str) -> u64 {
    let name = name.trim_start_matches("snapshots/");
    name.split_once('.').map_or(name, |(lsn, _)| lsn).parse().unwrap_or(0)
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
    !dir.join(FILE).exists() && !dir.join(LEGACY_FILE).exists()
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
    // A snapshot of a version up to 0.4 is its page store: opening imports it.
    let file = if name.ends_with(".redb") { LEGACY_FILE } else { FILE };
    let tmp = dir.join("nuage.db.partial");
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
    fs::rename(&tmp, dir.join(file)).map_err(io)?;
    wal::sync_dir(dir);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backup of an earlier version is set aside once; when that version
    /// has run again since and gone further, its new backup takes the place of
    /// the one set aside before.
    #[test]
    fn a_backup_set_aside_earlier_gives_way_to_a_later_one() {
        let dir = std::env::temp_dir().join(format!("nuage-backup-aside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = DirectoryStore::new(&dir);
        let old = |lsn: u64| {
            let name = format!("snapshots/{lsn:020}.redb");
            store.put(&name, format!("pages at {lsn}").as_bytes()).unwrap();
            let manifest = json!({ "snapshot": name, "lsn": lsn, "at": 1, "encrypted": false }).to_string();
            store.put(MANIFEST, manifest.as_bytes()).unwrap();
            manifest.into_bytes()
        };
        let aside = |name: &str| store.get_aside(BEFORE_IMPORT, name).unwrap();
        let first = old(5);
        store.put(&format!("wal/{}", segment_name(3)), b"before the snapshot").unwrap();
        store.put(&format!("wal/{}", segment_name(6)), b"after the snapshot").unwrap();
        assert!(keep_aside(&store, false).unwrap());
        assert_eq!(aside(MANIFEST), Some(first));
        assert_eq!(aside("snapshots/00000000000000000005.redb"), Some(b"pages at 5".to_vec()));
        assert_eq!(aside(&format!("wal/{}", segment_name(6))), Some(b"after the snapshot".to_vec()));
        assert_eq!(aside(&format!("wal/{}", segment_name(3))), None, "a restore does not read it");
        assert!(!keep_aside(&store, false).unwrap(), "the same backup: nothing to do");
        // The earlier version ran again from there, and wrote more.
        let second = old(9);
        assert!(keep_aside(&store, false).unwrap());
        assert_eq!(aside(MANIFEST), Some(second));
        assert_eq!(aside("snapshots/00000000000000000009.redb"), Some(b"pages at 9".to_vec()));
        // A backup of this version, or one in clear of data encrypted since: nothing to set aside.
        assert!(!keep_aside(&store, true).unwrap());
        let ours = json!({ "snapshot": "snapshots/00000000000000000012.db", "lsn": 12, "encrypted": false });
        store.put(MANIFEST, ours.to_string().as_bytes()).unwrap();
        assert!(!keep_aside(&store, false).unwrap());
        // A part of the store sets aside under `before-import/` of the whole, at its own place.
        let whole: Arc<dyn BackupStore> = Arc::new(store.clone());
        let part = PrefixedStore::new(whole, "apps/shop/");
        part.put_aside(BEFORE_IMPORT, MANIFEST, b"m").unwrap();
        assert_eq!(store.get("before-import/apps/shop/manifest.json").unwrap(), Some(b"m".to_vec()));
        assert_eq!(part.list_aside(BEFORE_IMPORT, "").unwrap(), [MANIFEST]);
        assert!(part.describe_aside(BEFORE_IMPORT).ends_with("before-import/apps/shop"));
        delete_all(&part).unwrap();
        assert_eq!(store.get("before-import/apps/shop/manifest.json").unwrap(), None);
        assert_eq!(aside(MANIFEST).map(|m| m.len() > 10), Some(true), "the others' stay");
        let _ = fs::remove_dir_all(&dir);
    }
}
