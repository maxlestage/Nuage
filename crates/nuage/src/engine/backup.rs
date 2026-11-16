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
use std::time::Duration;

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
    /// Writes an object to the file `dest`, created or replaced. `false`, and
    /// no file, when the object does not exist. The default reads it whole
    /// first; a store that can writes it as it comes, so that restoring a
    /// large snapshot takes no more memory than a small one.
    fn get_file(&self, name: &str, dest: &Path) -> Result<bool> {
        match self.get(name)? {
            Some(data) => {
                fs::write(dest, data).map_err(|e| storage_err(format!("cannot write {}: {e}", dest.display())))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    /// Bytes of a stored object, when the store can tell (the default cannot:
    /// `None`). An object that is not there is an error. A snapshot is named by
    /// the manifest only once this, where it tells, gives the size of the
    /// file that was sent.
    fn size(&self, _name: &str) -> Result<Option<u64>> {
        Ok(None)
    }
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

/// What marks a file being written, before it takes the name of its object.
const PARTIAL: &str = ".partial";

/// A file being written that nobody has written to for this long was left by
/// a writer that never finished (a crash, a kill): it is removed.
const PARTIAL_LEFT: Duration = Duration::from_secs(15 * 60);

/// `name` followed by sixteen random hexadecimal digits and [`PARTIAL`]: a
/// file of its own for each writer, whatever else writes the same object at
/// the same time (another backup, another server on the same directory).
fn partial_name(name: &str) -> String {
    let unique: String = crate::crypto::random::<8>().iter().map(|b| format!("{b:02x}")).collect();
    format!("{name}.{unique}{PARTIAL}")
}

impl DirectoryStore {
    /// Stores the object `name`: `write` fills a file aside, which is synced
    /// and then renamed, so that a reader never sees half an object. The file
    /// aside is this writer's alone, and does not outlive a failure.
    fn store(&self, name: &str, write: impl FnOnce(&Path) -> std::io::Result<()>) -> Result<()> {
        let path = self.path(name)?;
        let dir = path.parent().unwrap_or(&self.root).to_owned();
        fs::create_dir_all(&dir).map_err(io)?;
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = dir.join(partial_name(&file));
        let stored =
            write(&tmp).and_then(|()| super::wal::sync_file(&tmp)).and_then(|()| fs::rename(&tmp, &path)).map_err(io);
        if stored.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        stored?;
        super::wal::sync_dir(&dir);
        // What a writer that was killed left there: nothing else removes it.
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let left = entry.file_name().to_string_lossy().ends_with(PARTIAL)
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|at| at.elapsed().is_ok_and(|d| d >= PARTIAL_LEFT));
            if left {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

impl BackupStore for DirectoryStore {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn put(&self, name: &str, data: &[u8]) -> Result<()> {
        self.store(name, |tmp| fs::write(tmp, data))
    }

    fn put_file(&self, name: &str, source: &Path) -> Result<()> {
        self.store(name, |tmp| fs::copy(source, tmp).map(|_| ()))
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(name)?) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    fn get_file(&self, name: &str, dest: &Path) -> Result<bool> {
        match fs::copy(self.path(name)?, dest) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io(e)),
        }
    }

    fn size(&self, name: &str) -> Result<Option<u64>> {
        fs::metadata(self.path(name)?).map(|m| Some(m.len())).map_err(io)
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
            if name.starts_with(file_prefix) && !name.ends_with(PARTIAL) {
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

    fn get_file(&self, name: &str, dest: &Path) -> Result<bool> {
        self.inner.get_file(&format!("{}{name}", self.prefix), dest)
    }

    fn size(&self, name: &str) -> Result<Option<u64>> {
        self.inner.size(&format!("{}{name}", self.prefix))
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
    // Not under the writer lock: a long write, or the last checkpoint of a
    // server that stops, would hold the shipment for as long as it runs.
    let current = store.current_segment();
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
/// set aside now. `encrypted`: the store is, and its snapshot is sent with its
/// header.
pub(crate) fn snapshot(store: &Store, backup: &dyn BackupStore, keep: usize, encrypted: bool) -> Result<(u64, bool)> {
    // An encrypted snapshot is unreadable without its header (salt, check
    // value): without the header, no snapshot, rather than one a manifest
    // would call in clear.
    let header = if encrypted {
        let read = fs::read(store.dir.join(HEADER_FILE));
        Some(read.map_err(|e| {
            storage_err(format!("cannot read {HEADER_FILE}, without which an encrypted snapshot cannot be read: {e}"))
        })?)
    } else {
        None
    };
    let kept = keep_aside(backup, encrypted)?;
    // A file of its own: nothing else, whatever it does at the same time,
    // writes to it or removes it.
    let local = store.dir.join(partial_name(LOCAL_SNAPSHOT));
    let copied = (|| -> Result<(u64, u64)> {
        // The snapshot is the page file as a checkpoint leaves it.
        let mut w = store.writer();
        let lsn = store.checkpoint_locked(&mut w, Some(0))?;
        // From here the file must not change, for as long as it is copied:
        // the checkpoints wait. The commits do not: they go to the log and to
        // nodes in memory, and reach the backup in the log segments after `lsn`.
        let held = store.pages.hold_file();
        drop(w);
        // Through the store's own handle: Windows lets no other handle read the locked file.
        let bytes = store.pages.copy_file_to(&local)?;
        drop(held);
        Ok((lsn, bytes))
    })();
    let sent = copied.and_then(|(lsn, bytes)| {
        let name = format!("snapshots/{lsn:020}.db");
        backup.put_file(&name, &local)?;
        // The manifest names the snapshot only once all of it is stored.
        match backup.size(&name)? {
            Some(stored) if stored != bytes => {
                let _ = backup.delete(&name);
                Err(storage_err(format!("backup snapshot `{name}` is incomplete: {stored} bytes stored of {bytes}")))
            }
            _ => Ok((lsn, bytes, name)),
        }
    });
    let _ = fs::remove_file(&local);
    let (lsn, bytes, name) = sent?;
    if let Some(h) = &header {
        backup.put(HEADER_FILE, h)?;
    }
    let manifest =
        json!({ "snapshot": name, "lsn": lsn, "bytes": bytes, "at": now_millis(), "encrypted": header.is_some() });
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

/// The start of the name of the copy of the page store a snapshot sends, in
/// the data directory.
const LOCAL_SNAPSHOT: &str = "snapshot";

/// Removes the copies of the page store that snapshots cut short (a crash, a
/// kill) left in the data directory. For when the database is opened: no
/// snapshot is being taken.
pub(crate) fn remove_left_copies(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(LOCAL_SNAPSHOT) && name.ends_with(PARTIAL) {
            let _ = fs::remove_file(entry.path());
        }
    }
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
    fs::create_dir_all(dir.join("wal")).map_err(io)?;
    // The snapshot goes straight to a file aside: it is never held in memory.
    let tmp = dir.join("nuage.db.partial");
    let fetched = (|| -> Result<()> {
        if !backup.get_file(name, &tmp)? {
            return Err(storage_err(format!("backup snapshot `{name}` is missing")));
        }
        // The manifest tells the size of the snapshot it names: a file cut
        // short is refused here, not found out later page by page.
        let got = fs::metadata(&tmp).map_err(io)?.len();
        match manifest.get("bytes").and_then(Json::as_u64) {
            Some(bytes) if bytes != got => {
                Err(storage_err(format!("backup snapshot `{name}` is incomplete: {got} bytes of {bytes}")))
            }
            _ => Ok(()),
        }
    })();
    if let Err(e) = fetched {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // The header before the snapshot is put in place: it is unreadable without it.
    if manifest.get("encrypted").and_then(Json::as_bool) == Some(true) {
        let header = backup.get(HEADER_FILE)?.ok_or_else(|| storage_err(format!("the backup has no {HEADER_FILE}")))?;
        fs::write(dir.join(HEADER_FILE), header).map_err(io)?;
    }
    // A snapshot of a version up to 0.4 is its page store: opening imports it.
    let file = if name.ends_with(".redb") { LEGACY_FILE } else { FILE };
    for seg in backup.list("wal/")? {
        let Some(first) = segment_lsn(seg.trim_start_matches("wal/")) else { continue };
        // Segments that start before the snapshot only hold what it has.
        if first <= lsn {
            continue;
        }
        // One at a time, each to its file.
        let path = dir.join("wal").join(segment_name(first));
        if backup.get_file(&seg, &path)? {
            wal::sync_file(&path).map_err(io)?;
        }
    }
    // All of it on disk before the page store has its name: a database that
    // is there is whole, log included.
    wal::sync_dir(&dir.join("wal"));
    wal::sync_file(&tmp).map_err(io)?;
    fs::rename(&tmp, dir.join(file)).map_err(io)?;
    wal::sync_dir(dir);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::{BackupOptions, Embedded, EngineOptions};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuage-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A database in `dir` backed up to `store`, with the table `t`. The
    /// upkeep thread backs up once, when it opens, and then leaves the test alone.
    fn open(dir: &Path, store: Arc<dyn BackupStore>) -> Embedded {
        let options = EngineOptions {
            backup: Some(BackupOptions::new(store)),
            checkpoint_every: Duration::from_secs(3600),
            sync: false,
            ..EngineOptions::default()
        };
        let db = Embedded::open_with(dir, options).unwrap();
        db.sql("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        db
    }

    fn insert(db: &Embedded, ids: std::ops::Range<i64>) {
        for id in ids {
            db.sql("INSERT INTO t (id, v) VALUES (?, ?)", &[id.into(), format!("row {id}").into()]).unwrap();
        }
    }

    fn count(db: &Embedded) -> String {
        db.sql("SELECT count(*) FROM t", &[]).unwrap()[0].rows[0][0].to_string()
    }

    /// Tells the threads of a test that it is over, however it ends: a
    /// failed assertion must not leave them writing.
    struct Over<'a>(&'a AtomicBool);

    impl Drop for Over<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A store written against the methods every store must have, and none of
    /// the others: objects in memory.
    #[derive(Default)]
    struct Plain(Mutex<BTreeMap<String, Vec<u8>>>);

    impl BackupStore for Plain {
        fn describe(&self) -> String {
            "memory".into()
        }
        fn put(&self, name: &str, data: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(name.to_owned(), data.to_vec());
            Ok(())
        }
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn list(&self, prefix: &str) -> Result<Vec<String>> {
            Ok(self.0.lock().unwrap().keys().filter(|k| k.starts_with(prefix)).cloned().collect())
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    /// A store that has the required methods alone backs up and restores, as
    /// one part of it does: files go through `put` and `get`, and a size it
    /// cannot tell is not asked of it.
    #[test]
    fn a_store_with_the_required_methods_alone_backs_up_and_restores() {
        let (src, dst) = (temp("plain-src"), temp("plain-dst"));
        let whole: Arc<dyn BackupStore> = Arc::new(Plain::default());
        let part = || -> Arc<dyn BackupStore> { Arc::new(PrefixedStore::new(whole.clone(), "apps/shop/")) };
        {
            let db = open(&src, part());
            insert(&db, 0..30);
            db.engine().backup_now(true).unwrap();
            insert(&db, 30..40);
            db.engine().checkpoint().unwrap();
            db.engine().backup_now(false).unwrap();
        }
        assert_eq!(whole.size("apps/shop/manifest.json").unwrap(), None);
        let db = open(&dst, part());
        assert!(db.engine().backup_status().restored);
        assert_eq!(count(&db), "40");
        drop(db);
        for dir in [src, dst] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// A directory that, when told to, stores only the first half of a file.
    struct Half {
        inner: DirectoryStore,
        on: AtomicBool,
    }

    impl BackupStore for Half {
        fn describe(&self) -> String {
            self.inner.describe()
        }
        fn put(&self, name: &str, data: &[u8]) -> Result<()> {
            self.inner.put(name, data)
        }
        fn put_file(&self, name: &str, path: &Path) -> Result<()> {
            if !self.on.load(Ordering::SeqCst) {
                return self.inner.put_file(name, path);
            }
            let data = fs::read(path).unwrap();
            self.inner.put(name, &data[..data.len() / 2])
        }
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
            self.inner.get(name)
        }
        fn get_file(&self, name: &str, dest: &Path) -> Result<bool> {
            self.inner.get_file(name, dest)
        }
        fn size(&self, name: &str) -> Result<Option<u64>> {
            self.inner.size(name)
        }
        fn list(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list(prefix)
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.inner.delete(name)
        }
    }

    /// A snapshot of which only a part was stored is an error: the manifest
    /// goes on naming the one before, the part is removed, and the backup
    /// restores, with what the log shipped since.
    #[test]
    fn a_snapshot_stored_in_part_is_never_named_by_the_manifest() {
        let (src, target, dst) = (temp("half-src"), temp("half-store"), temp("half-dst"));
        let store = Arc::new(Half { inner: DirectoryStore::new(&target), on: AtomicBool::new(false) });
        {
            let db = open(&src, store.clone());
            insert(&db, 0..30);
            db.engine().backup_now(true).unwrap();
            let manifest = store.get(MANIFEST).unwrap();
            insert(&db, 30..60);
            store.on.store(true, Ordering::SeqCst);
            let e = db.engine().backup_now(true).unwrap_err().to_string();
            assert!(e.contains("is incomplete"), "{e}");
            assert!(!db.engine().backup_status().last_ok);
            assert_eq!(store.get(MANIFEST).unwrap(), manifest, "the manifest still names the whole snapshot");
            assert_eq!(store.list("snapshots/").unwrap().len(), 1, "the part is not kept");
            store.on.store(false, Ordering::SeqCst);
            db.engine().backup_now(false).unwrap();
        }
        let db = open_quiet(&dst, store);
        assert!(db.engine().backup_status().restored);
        assert_eq!(count(&db), "60");
        assert!(db.engine().check_integrity().unwrap());
        drop(db);
        for dir in [src, target, dst] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// A snapshot cut short in the backup is refused when it is restored: no
    /// database is put in place from a part of one.
    #[test]
    fn a_snapshot_cut_short_is_not_restored() {
        let (src, target, dst) = (temp("short-src"), temp("short-store"), temp("short-dst"));
        let store = Arc::new(DirectoryStore::new(&target));
        {
            let db = open(&src, store.clone());
            insert(&db, 0..2000);
            db.engine().backup_now(true).unwrap();
        }
        let manifest: Json = serde_json::from_slice(&store.get(MANIFEST).unwrap().unwrap()).unwrap();
        let snapshot = target.join(manifest["snapshot"].as_str().unwrap());
        let whole = fs::read(&snapshot).unwrap();
        assert_eq!(manifest["bytes"].as_u64(), Some(whole.len() as u64));
        fs::write(&snapshot, &whole[..whole.len() / 2]).unwrap();
        let e = restore(&dst, store.as_ref()).unwrap_err().to_string();
        assert!(e.contains("is incomplete"), "{e}");
        assert!(is_empty(&dst), "nothing was put in place");
        assert!(!dst.join("nuage.db.partial").exists());
        // Whole again, it comes back.
        fs::write(&snapshot, &whole).unwrap();
        let db = open_quiet(&dst, store);
        assert!(db.engine().backup_status().restored);
        assert_eq!(count(&db), "2000");
        drop(db);
        for dir in [src, target, dst] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Commits go on while a snapshot copies the page store, and the file
    /// does not change under the copy (the checkpoints wait for it): each
    /// snapshot restores whole, every page in order, with every row that was
    /// acknowledged before it began.
    #[test]
    fn a_snapshot_taken_while_commits_land_restores_whole() {
        use std::sync::atomic::AtomicI64;
        let (src, target) = (temp("live-src"), temp("live-store"));
        let store = Arc::new(DirectoryStore::new(&target));
        // A small cache: the commits have changed pages to write to the file every few of them.
        let options = EngineOptions {
            backup: Some(BackupOptions::new(store.clone())),
            checkpoint_every: Duration::from_secs(3600),
            checkpoint_wal_bytes: u64::MAX,
            cache_bytes: 1 << 20,
            sync: false,
            ..EngineOptions::default()
        };
        let db = Embedded::open_with(&src, options).unwrap();
        db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        let body = "x".repeat(16 << 10);
        let write = |id: i64| {
            let row = format!("{id}{body}");
            db.sql("INSERT INTO t (id, v) VALUES (?, ?)", &[id.into(), row.into()]).unwrap();
            // Rows come and go: the pages they leave are written again.
            db.sql("DELETE FROM t WHERE id = ?", &[(id - 100).into()]).unwrap();
        };
        (0..200).for_each(write);
        // A copy long enough for several of those writes to the file to come up.
        db.engine().store.pages.slow_copies.store(true, Ordering::SeqCst);
        let (next, written, over) = (AtomicI64::new(200), AtomicI64::new(200), AtomicBool::new(false));
        std::thread::scope(|s| {
            let _over = Over(&over);
            for _ in 0..2 {
                s.spawn(|| {
                    // No more than the log of a short test should hold.
                    while !over.load(Ordering::SeqCst) && next.load(Ordering::SeqCst) < 1500 {
                        write(next.fetch_add(1, Ordering::SeqCst));
                        written.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
            for round in 0..4 {
                let before = written.load(Ordering::SeqCst);
                db.engine().backup_now(true).unwrap();
                let dst = temp(&format!("live-dst-{round}"));
                assert!(restore(&dst, store.as_ref()).unwrap());
                let copy =
                    Embedded::open_with(&dst, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
                assert!(copy.engine().check_integrity().unwrap(), "round {round}: {:?}", copy.engine().events());
                let newest = copy.sql("SELECT max(id) FROM t", &[]).unwrap()[0].rows[0][0].to_string();
                let newest: i64 = newest.parse().unwrap();
                assert!(newest >= before - 2, "round {round}: rows up to {newest} restored, {before} written before");
                let whole = copy.sql("SELECT count(*) FROM t WHERE length(v) < 16384", &[]).unwrap();
                assert_eq!(whole[0].rows[0][0].to_string(), "0");
                drop(copy);
                let _ = fs::remove_dir_all(&dst);
            }
        });
        assert!(written.load(Ordering::SeqCst) > 200, "commits landed while the snapshots were taken");
        assert!(db.engine().check_integrity().unwrap());
        drop(db);
        for dir in [src, target] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// A commit does not wait for a snapshot to be copied: it returns while
    /// the copy is still being written.
    #[test]
    fn a_commit_does_not_wait_for_the_copy_of_a_snapshot() {
        let (src, target) = (temp("nowait-src"), temp("nowait-store"));
        let db = open(&src, Arc::new(DirectoryStore::new(&target)));
        let body = "x".repeat(64 << 10);
        for id in 0..128 {
            db.sql("INSERT INTO t (id, v) VALUES (?, ?)", &[id.into(), body.clone().into()]).unwrap();
        }
        // The backup the upkeep thread sends when the database opens is over.
        assert!((0..2000).any(|_| {
            std::thread::sleep(Duration::from_millis(5));
            db.engine().backup_status().last_snapshot_at.is_some()
        }));
        // Several mebibytes, copied slowly: a snapshot that lasts.
        db.engine().store.pages.slow_copies.store(true, Ordering::SeqCst);
        let copy_of = |dir: &Path| {
            fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).find(|p| {
                let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                name.starts_with(LOCAL_SNAPSHOT) && name.ends_with(PARTIAL)
            })
        };
        std::thread::scope(|s| {
            s.spawn(|| db.engine().backup_now(true).unwrap());
            let copy = (0..5000).find_map(|_| {
                std::thread::sleep(Duration::from_millis(1));
                copy_of(&src)
            });
            let copy = copy.expect("the copy began");
            insert(&db, 1000..1001);
            let copied = fs::metadata(&copy).map_or(u64::MAX, |m| m.len());
            let whole = db.engine().stats().file_bytes;
            assert!(copied < whole, "the commit returned once {copied} bytes of {whole} were copied");
        });
        assert_eq!(count(&db), "129");
        drop(db);
        for dir in [src, target] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// A directory where a snapshot being sent waits, when told to, until it
    /// is let through: a snapshot to a slow bucket.
    struct Gate {
        inner: DirectoryStore,
        closed: Mutex<bool>,
        opened: std::sync::Condvar,
        /// Snapshots stored, and snapshots waiting to be.
        sent: std::sync::atomic::AtomicUsize,
        waiting: std::sync::atomic::AtomicUsize,
    }

    impl Gate {
        fn new(root: &Path) -> Arc<Gate> {
            Arc::new(Gate {
                inner: DirectoryStore::new(root),
                closed: Mutex::new(false),
                opened: std::sync::Condvar::new(),
                sent: Default::default(),
                waiting: Default::default(),
            })
        }

        /// Snapshots wait from now on, until the guard is dropped (whatever
        /// happens to the test: nothing is left waiting).
        fn close(&self) -> Opens<'_> {
            *self.closed.lock().unwrap() = true;
            Opens(self)
        }

        /// Waits until `n` snapshots wait at the gate.
        fn until_waiting(&self, n: usize) {
            assert!(
                (0..2000).any(|_| {
                    std::thread::sleep(Duration::from_millis(5));
                    self.waiting.load(Ordering::SeqCst) == n
                }),
                "{} snapshots wait, not {n}",
                self.waiting.load(Ordering::SeqCst)
            );
        }
    }

    /// Opens its gate when dropped.
    struct Opens<'a>(&'a Gate);

    impl Drop for Opens<'_> {
        fn drop(&mut self) {
            *self.0.closed.lock().unwrap() = false;
            self.0.opened.notify_all();
        }
    }

    impl BackupStore for Gate {
        fn describe(&self) -> String {
            self.inner.describe()
        }
        fn put(&self, name: &str, data: &[u8]) -> Result<()> {
            self.inner.put(name, data)
        }
        fn put_file(&self, name: &str, path: &Path) -> Result<()> {
            if name.starts_with("snapshots/") {
                self.waiting.fetch_add(1, Ordering::SeqCst);
                let mut closed = self.closed.lock().unwrap();
                while *closed {
                    closed = self.opened.wait(closed).unwrap();
                }
                drop(closed);
                self.waiting.fetch_sub(1, Ordering::SeqCst);
                self.sent.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.put_file(name, path)
        }
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
            self.inner.get(name)
        }
        fn get_file(&self, name: &str, dest: &Path) -> Result<bool> {
            self.inner.get_file(name, dest)
        }
        fn size(&self, name: &str) -> Result<Option<u64>> {
            self.inner.size(name)
        }
        fn list(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list(prefix)
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.inner.delete(name)
        }
    }

    /// `open`, once the backup the upkeep thread sends when the database
    /// opens is over: the test has the backups to itself, and nothing writes
    /// to them after it has removed them.
    fn open_quiet(dir: &Path, store: Arc<dyn BackupStore>) -> Embedded {
        let db = open(dir, store);
        assert!((0..2000).any(|_| {
            std::thread::sleep(Duration::from_millis(5));
            db.engine().backup_status().last_snapshot_at.is_some()
        }));
        db
    }

    /// The rows of the backup in `store`, restored into the empty `dst`.
    fn restored(dst: &Path, store: &dyn BackupStore) -> String {
        let _ = fs::remove_dir_all(dst);
        assert!(restore(dst, store).unwrap());
        let db = Embedded::open_with(dst, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
        let rows = count(&db);
        drop(db);
        let _ = fs::remove_dir_all(dst);
        rows
    }

    /// The last shipment of a server that stops does not wait for a snapshot
    /// being sent (one the console asked for, to a slow bucket): the log goes
    /// next to it, and the backup holds every write, whether that snapshot
    /// ever arrives or not.
    #[test]
    fn the_last_shipment_does_not_wait_for_a_snapshot_being_sent() {
        let (src, target, dst) = (temp("beside-src"), temp("beside-store"), temp("beside-dst"));
        let store = Gate::new(&target);
        let db = open_quiet(&src, store.clone());
        insert(&db, 0..30);
        let engine = db.engine();
        std::thread::scope(|s| {
            let gate = store.close();
            let forced = s.spawn(|| engine.backup_now(true));
            store.until_waiting(1);
            insert(&db, 30..60);
            // What a server that stops does.
            engine.begin_shutdown();
            engine.checkpoint().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let last = engine.clone();
            std::thread::spawn(move || tx.send(last.backup_now(false)));
            rx.recv_timeout(Duration::from_secs(10)).expect("the log was shipped beside the snapshot").unwrap();
            assert_eq!(store.waiting.load(Ordering::SeqCst), 1, "the snapshot is still being sent");
            // The process ends there: what the backup holds is every write.
            assert_eq!(restored(&dst, store.as_ref()), "60");
            drop(gate);
            forced.join().unwrap().unwrap();
        });
        // Or the snapshot arrived after all: every write too.
        assert_eq!(restored(&dst, store.as_ref()), "60");
        drop(db);
        for dir in [src, target, dst] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Snapshots asked for while one is being sent wait for it, and the one
    /// that follows serves them all: each began after they were asked, and
    /// holds every row written before.
    #[test]
    fn snapshots_asked_for_meanwhile_share_the_next_one() {
        let (src, target, dst) = (temp("share-src"), temp("share-store"), temp("share-dst"));
        let store = Gate::new(&target);
        let db = open_quiet(&src, store.clone());
        insert(&db, 0..10);
        let engine = db.engine();
        let before = store.sent.load(Ordering::SeqCst);
        std::thread::scope(|s| {
            let gate = store.close();
            let first = s.spawn(|| engine.backup_now(true));
            store.until_waiting(1);
            insert(&db, 10..20);
            let others: Vec<_> = (0..3).map(|_| s.spawn(|| engine.backup_now(true))).collect();
            // They have asked, and wait for their turn.
            std::thread::sleep(Duration::from_millis(300));
            assert!(others.iter().all(|t| !t.is_finished()));
            drop(gate);
            first.join().unwrap().unwrap();
            for t in others {
                t.join().unwrap().unwrap();
            }
        });
        assert_eq!(store.sent.load(Ordering::SeqCst) - before, 2, "the one being sent, then one for the others");
        assert_eq!(restored(&dst, store.as_ref()), "20");
        drop(db);
        for dir in [src, target, dst] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Closing a database waits for its backup under way, and none is made
    /// after: once it has returned, the backup location is left as it is. A
    /// deleted application's backups are removed then, and none of its
    /// objects lands afterwards for a new application of the same name to
    /// find.
    #[test]
    fn closing_waits_for_the_backup_under_way() {
        let (src, target) = (temp("close-src"), temp("close-store"));
        let store = Gate::new(&target);
        let db = open_quiet(&src, store.clone());
        insert(&db, 0..10);
        let engine = db.engine();
        std::thread::scope(|s| {
            let gate = store.close();
            let forced = s.spawn(|| engine.backup_now(true));
            store.until_waiting(1);
            let closing = s.spawn(|| engine.close());
            std::thread::sleep(Duration::from_millis(300));
            assert!(!closing.is_finished(), "close waits for the snapshot being sent");
            drop(gate);
            closing.join().unwrap().unwrap();
            forced.join().unwrap().unwrap();
        });
        for force in [true, false] {
            let e = engine.backup_now(force).unwrap_err().to_string();
            assert!(e.contains("is closed"), "{e}");
        }
        delete_all(store.as_ref()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(store.get(MANIFEST).unwrap(), None);
        assert!(store.list("snapshots/").unwrap().is_empty() && store.list("wal/").unwrap().is_empty());
        drop(db);
        for dir in [src, target] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// An encrypted database whose header cannot be read sends no snapshot:
    /// one would be unreadable, and the manifest goes on naming the last one
    /// that can be read.
    #[test]
    fn no_snapshot_of_an_encrypted_database_without_its_header() {
        let (src, target) = (temp("header-src"), temp("header-store"));
        let store = Arc::new(DirectoryStore::new(&target));
        let options = EngineOptions {
            backup: Some(BackupOptions::new(store.clone())),
            checkpoint_every: Duration::from_secs(3600),
            encryption: Some(crate::Encryption::from_key([7; 32])),
            sync: false,
            ..EngineOptions::default()
        };
        let db = Embedded::open_with(&src, options).unwrap();
        db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        insert(&db, 0..10);
        db.engine().backup_now(true).unwrap();
        let (manifest, snapshots) = (store.get(MANIFEST).unwrap(), store.list("snapshots/").unwrap());
        let header = fs::read(src.join(HEADER_FILE)).unwrap();
        fs::remove_file(src.join(HEADER_FILE)).unwrap();
        insert(&db, 10..20);
        let e = db.engine().backup_now(true).unwrap_err().to_string();
        assert!(e.contains(HEADER_FILE), "{e}");
        assert_eq!(store.get(MANIFEST).unwrap(), manifest);
        assert_eq!(store.list("snapshots/").unwrap(), snapshots);
        fs::write(src.join(HEADER_FILE), header).unwrap();
        db.engine().backup_now(true).unwrap();
        drop(db);
        for dir in [src, target] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Each object is written to a file of its own before it takes its name:
    /// writers of the same object at the same time all succeed, and the
    /// object is one of theirs, whole. A file a killed writer left is removed
    /// once it is old; one that may still be written is not.
    #[test]
    fn a_directory_never_shows_or_keeps_half_an_object() {
        let dir = temp("directory");
        let store = DirectoryStore::new(&dir);
        let bodies: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 3 << 20]).collect();
        let source = temp("directory-source");
        fs::create_dir_all(&source).unwrap();
        std::thread::scope(|s| {
            for (i, body) in bodies.iter().enumerate() {
                let (store, source) = (&store, source.join(i.to_string()));
                s.spawn(move || {
                    fs::write(&source, body).unwrap();
                    store.put_file("snapshots/1.db", &source).unwrap();
                    store.put("snapshots/1.db", body).unwrap();
                });
            }
        });
        let stored = store.get("snapshots/1.db").unwrap().unwrap();
        assert!(bodies.contains(&stored), "the object is what one writer wrote, whole");
        assert_eq!(store.size("snapshots/1.db").unwrap(), Some(3 << 20));
        assert!(store.size("snapshots/2.db").is_err());
        assert_eq!(store.list("snapshots/").unwrap(), ["snapshots/1.db"]);
        assert_eq!(fs::read_dir(dir.join("snapshots")).unwrap().count(), 1, "no file aside is left");
        // A writer that failed leaves nothing either.
        assert!(store.put_file("snapshots/3.db", &source.join("missing")).is_err());
        assert_eq!(fs::read_dir(dir.join("snapshots")).unwrap().count(), 1);
        // Left by writers that were killed: one long ago, one a moment ago (or still at it).
        let (old, recent) = (dir.join("snapshots/9.db.old.partial"), dir.join("snapshots/9.db.new.partial"));
        fs::write(&old, b"half").unwrap();
        fs::write(&recent, b"half").unwrap();
        let long_ago = std::time::SystemTime::now() - PARTIAL_LEFT - Duration::from_secs(60);
        fs::File::options().write(true).open(&old).unwrap().set_modified(long_ago).unwrap();
        store.put("snapshots/4.db", b"whole").unwrap();
        assert!(!old.exists() && recent.exists());
        assert_eq!(store.list("snapshots/").unwrap(), ["snapshots/1.db", "snapshots/4.db"]);
        // Fetched to a file: the object, or no file.
        assert!(store.get_file("snapshots/4.db", &source.join("back")).unwrap());
        assert_eq!(fs::read(source.join("back")).unwrap(), b"whole");
        assert!(!store.get_file("snapshots/5.db", &source.join("none")).unwrap());
        assert!(!source.join("none").exists());
        for dir in [dir, source] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// The copy of the page store a snapshot was sending when the process was
    /// killed is removed when the database is opened again, whatever its name.
    #[test]
    fn a_copy_left_by_a_snapshot_cut_short_is_removed_at_opening() {
        let (dir, target) = (temp("left"), temp("left-store"));
        drop(open(&dir, Arc::new(DirectoryStore::new(&target))));
        let left = [dir.join("snapshot.partial"), dir.join(partial_name(LOCAL_SNAPSHOT))];
        for path in &left {
            fs::write(path, vec![0u8; 4096]).unwrap();
        }
        let db = (0..400)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(10));
                Embedded::open_with(&dir, EngineOptions::default()).ok()
            })
            .expect("opens again");
        assert!(left.iter().all(|path| !path.exists()));
        assert_eq!(count(&db), "0");
        drop(db);
        for dir in [dir, target] {
            let _ = fs::remove_dir_all(&dir);
        }
    }

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
