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
//!
//! A location holds the backup of one database. Before each snapshot, the
//! location's `encryption.json` is compared with the data directory's: when
//! the location holds another database's, the snapshot is refused and nothing
//! is written there or removed from it (see [`of_this_database`]). Only the
//! header is compared, never how far each copy has gone: a location ahead of
//! the data (another server of the same database still running beside this
//! one, as when a host starts the new one before stopping the old) is this
//! database's.
//!
//! A backup can go to two places at once ([`MirrorStore`]). The engine ships
//! to each one with its own progress and its own lock, each on a thread of
//! its own: one that does not answer never holds the other back,
//! and one that missed log segments the data directory no longer has gets a
//! whole snapshot before anything else ([`can_continue`]). An empty data
//! directory is restored from ONE of them, whole, never a mix: the one that
//! reaches furthest; when either cannot be read, nothing is restored
//! ([`restore_best`]).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::legacy::LEGACY_FILE;
use super::storage_err;
use super::store::{FILE, Store, now_millis};
use super::wal::{self, segment_lsn, segment_name};
use crate::crypto::{HEADER_FILE, same_database};

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

    /// The two places of a store that backs up to two ([`MirrorStore`]), each
    /// as a store of its own: the engine ships to each one with its own
    /// progress, and restores from one of them. `None` (the default): one
    /// place.
    fn parts(&self) -> Option<(Arc<dyn BackupStore>, Arc<dyn BackupStore>)> {
        None
    }
}

/// Backups to two places, kept in step: what an engine is given to back up
/// to both. It is only the pair: the engine ships to each place
/// ([`parts`](BackupStore::parts)) with its own progress, so a place that does
/// not answer never holds the other back, and an empty data directory is
/// restored from one place, whole (the one that reaches furthest; a place
/// that cannot be read stops the opening, see [`restore_best`]).
///
/// Used as a store by itself, it writes and deletes in both places (trying
/// both, and returning the first error), lists what either holds, and reads
/// from the first place, then from the second.
pub struct MirrorStore {
    first: Arc<dyn BackupStore>,
    second: Arc<dyn BackupStore>,
}

impl MirrorStore {
    /// Backups to `first` and to `second`. An empty data directory whose two
    /// places hold the same backup is restored from `first`.
    pub fn new(first: Arc<dyn BackupStore>, second: Arc<dyn BackupStore>) -> Self {
        MirrorStore { first, second }
    }

    /// `f` on both places: both are tried, and the first error is returned.
    fn both(&self, f: impl Fn(&dyn BackupStore) -> Result<()>) -> Result<()> {
        let first = f(self.first.as_ref());
        let second = f(self.second.as_ref());
        first.and(second)
    }
}

/// The names of both lists, sorted, each once.
fn union(mut a: Vec<String>, b: Vec<String>) -> Vec<String> {
    a.extend(b);
    a.sort();
    a.dedup();
    a
}

impl BackupStore for MirrorStore {
    fn describe(&self) -> String {
        format!("{} and {}", self.first.describe(), self.second.describe())
    }

    fn put(&self, name: &str, data: &[u8]) -> Result<()> {
        self.both(|s| s.put(name, data))
    }

    fn put_file(&self, name: &str, path: &Path) -> Result<()> {
        self.both(|s| s.put_file(name, path))
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match self.first.get(name)? {
            Some(data) => Ok(Some(data)),
            None => self.second.get(name),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(union(self.first.list(prefix)?, self.second.list(prefix)?))
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.both(|s| s.delete(name))
    }

    fn put_aside(&self, dir: &str, name: &str, data: &[u8]) -> Result<()> {
        self.both(|s| s.put_aside(dir, name, data))
    }

    fn get_aside(&self, dir: &str, name: &str) -> Result<Option<Vec<u8>>> {
        match self.first.get_aside(dir, name)? {
            Some(data) => Ok(Some(data)),
            None => self.second.get_aside(dir, name),
        }
    }

    fn list_aside(&self, dir: &str, prefix: &str) -> Result<Vec<String>> {
        Ok(union(self.first.list_aside(dir, prefix)?, self.second.list_aside(dir, prefix)?))
    }

    fn delete_aside(&self, dir: &str, name: &str) -> Result<()> {
        self.both(|s| s.delete_aside(dir, name))
    }

    fn describe_aside(&self, dir: &str) -> String {
        format!("{} and {}", self.first.describe_aside(dir), self.second.describe_aside(dir))
    }

    fn parts(&self) -> Option<(Arc<dyn BackupStore>, Arc<dyn BackupStore>)> {
        Some((self.first.clone(), self.second.clone()))
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
        super::wal::sync_file(&tmp).map_err(io)?;
        fs::rename(&tmp, &path).map_err(io)
    }

    fn put_file(&self, name: &str, source: &Path) -> Result<()> {
        let path = self.path(name)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = path.with_extension("partial");
        fs::copy(source, &tmp).map_err(io)?;
        super::wal::sync_file(&tmp).map_err(io)?;
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
        match self.parts() {
            Some((first, second)) => format!("{} and {}", first.describe(), second.describe()),
            None => format!("{}/{}", self.inner.describe().trim_end_matches('/'), self.prefix.trim_end_matches('/')),
        }
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
        match self.parts() {
            Some((first, second)) => format!("{} and {}", first.describe_aside(dir), second.describe_aside(dir)),
            None => format!("{}/{}", self.inner.describe_aside(dir), self.prefix.trim_end_matches('/')),
        }
    }

    // The same prefix in each place: an application backs up under
    // `apps/<name>/` of both.
    fn parts(&self) -> Option<(Arc<dyn BackupStore>, Arc<dyn BackupStore>)> {
        let (first, second) = self.inner.parts()?;
        let part = |inner| -> Arc<dyn BackupStore> { Arc::new(PrefixedStore::new(inner, self.prefix.clone())) };
        Some((part(first), part(second)))
    }
}

/// Removes every object of a backup (a deleted application's). A backup to two
/// places is emptied in each, one after the other: a place that does not
/// answer does not keep the other's objects.
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) fn delete_all(backup: &dyn BackupStore) -> Result<()> {
    if let Some((first, second)) = backup.parts() {
        let first = delete_all(first.as_ref());
        return first.and(delete_all(second.as_ref()));
    }
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

pub(crate) const MANIFEST: &str = "manifest.json";

/// The places of `store`, in order: the two of a [`MirrorStore`] (and of a
/// [`PrefixedStore`] of one), or the store itself.
pub(crate) fn places(store: &Arc<dyn BackupStore>) -> Vec<Arc<dyn BackupStore>> {
    match store.parts() {
        Some((first, second)) => vec![first, second],
        None => vec![store.clone()],
    }
}

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

/// Ships the sealed log segments not shipped yet, oldest first. Returns the
/// last LSN they cover (everything up to it is in the backup). A segment is
/// sent only when the backup holds the log up to the commit before it: when
/// the segment that follows `shipped` is no longer on this disk (a second
/// destination that was away while the first's were removed), nothing is
/// sent, and the destination needs a snapshot ([`can_continue`]).
pub(crate) fn ship_segments(store: &Store, backup: &dyn BackupStore, shipped: u64) -> Result<u64> {
    let segments = wal::segments(&store.dir.join("wal"))?;
    let current = store.writer().wal.first_lsn;
    let mut last = shipped;
    for (i, (first, path)) in segments.iter().enumerate() {
        let Some((next_first, _)) = segments.get(i + 1) else { break };
        if *first == current || next_first - 1 <= shipped {
            continue;
        }
        let gone = || {
            storage_err(format!(
                "{}: the log after commit {last} is no longer on this disk, nothing after it was sent (a whole \
                 snapshot goes there next)",
                backup.describe()
            ))
        };
        if *first > last + 1 {
            return Err(gone());
        }
        // Removed while it was being sent (a destination that took too long to answer).
        if let Err(e) = backup.put_file(&format!("wal/{}", segment_name(*first)), path) {
            return Err(if path.exists() { e } else { gone() });
        }
        last = last.max(next_first - 1);
    }
    Ok(last)
}

/// `true` when a destination that holds the log up to `shipped` can go on
/// with log segments: this disk still has the segment that follows (its
/// oldest one starts at or before `shipped + 1`). `false`: the commits after
/// `shipped` are only in the page store now (the segments are kept until the
/// first destination has them, never for the second), and the destination
/// needs a whole snapshot before any further segment.
pub(crate) fn can_continue(store: &Store, shipped: u64) -> Result<bool> {
    let segments = wal::segments(&store.dir.join("wal"))?;
    Ok(segments.first().is_none_or(|(first, _)| *first <= shipped + 1))
}

/// Refuses a location that holds another database's backup: its
/// `encryption.json` is not the data directory's. A location without one is
/// empty, holds the start of an encrypted database's first snapshot (the
/// header is uploaded after the snapshot), or a backup in clear, which has
/// nothing that tells one database from another (the server's binary always
/// encrypts): nothing to refuse. A data directory in clear facing a location
/// that has a header is refused too: nothing goes from an encrypted backup
/// back to one in clear.
///
/// The header is made once, when a data directory is created; a restore
/// writes the one it downloads and a snapshot uploads the directory's own,
/// so a database is never refused its own backup.
fn of_this_database(store: &Store, backup: &dyn BackupStore) -> Result<()> {
    let Some(theirs) = backup.get(HEADER_FILE)? else { return Ok(()) };
    let ours = match fs::read(store.dir.join(HEADER_FILE)) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(storage_err(format!("cannot read {HEADER_FILE}: {e}"))),
    };
    if ours.is_some_and(|ours| same_database(&ours, &theirs)) {
        return Ok(());
    }
    Err(storage_err(format!(
        "{} holds the backup of another database (its {HEADER_FILE} is not this one's): nothing was written \
         there. Give this database a place of its own, or empty that one",
        backup.describe()
    )))
}

/// A snapshot is taken in three steps, so that a round in which both places
/// of a [`MirrorStore`] are due copies the page store once: [`prepare`] each
/// place, [`take`] the copy, [`send`] it to each place.
///
/// The first step, before anything is written to `backup`: a location that
/// holds another database's backup is refused, with nothing written
/// ([`of_this_database`]); the backup of a version up to 0.4 is then set
/// aside ([`keep_aside`]). Returns whether it was set aside now.
pub(crate) fn prepare(store: &Store, backup: &dyn BackupStore) -> Result<bool> {
    of_this_database(store, backup)?;
    keep_aside(backup, store.dir.join(HEADER_FILE).exists())
}

/// A copy of the page store for a snapshot ([`take`]), removed from the disk
/// when it is dropped. Each copy has a name of its own: the two destinations
/// may each be sending one.
pub(crate) struct Taken {
    /// The LSN the copy holds everything up to.
    pub(crate) lsn: u64,
    path: PathBuf,
}

impl Drop for Taken {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The second step of a snapshot: a checkpoint, then a copy of the page
/// store beside it, with no commit in between.
pub(crate) fn take(store: &Store) -> Result<Taken> {
    static COPIES: AtomicU64 = AtomicU64::new(0);
    let path = store.dir.join(format!("{COPY}{}.partial", COPIES.fetch_add(1, Ordering::Relaxed)));
    // No commit may land while the file is copied.
    let mut w = store.writer();
    let lsn = store.checkpoint_locked(&mut w, Some(0))?;
    // Made before the copy: what a copy that fails part way leaves is removed.
    let taken = Taken { lsn, path };
    // Through the store's own handle: Windows lets no other handle read the locked file.
    store.pages.copy_file_to(&taken.path)?;
    Ok(taken)
}

/// The start of the names of the copies [`take`] makes in the data directory.
const COPY: &str = "snapshot-";

/// Removes the copies of the page store left in `dir` by a process that
/// stopped while it sent one (they were never named by a manifest).
pub(crate) fn remove_copies(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `snapshot.partial`: the name a copy had up to 0.4.6.
        if (name.starts_with(COPY) && name.ends_with(".partial")) || name == "snapshot.partial" {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The last step of a snapshot: the copy is uploaded, then the header and the
/// manifest. Older snapshots and the segments before them are pruned,
/// keeping `keep`.
pub(crate) fn send(store: &Store, backup: &dyn BackupStore, taken: &Taken, keep: usize) -> Result<()> {
    let lsn = taken.lsn;
    let name = format!("snapshots/{lsn:020}.db");
    backup.put_file(&name, &taken.path)?;
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
    Ok(())
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

/// What a backup location holds, read without restoring it ([`inspect`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupInfo {
    /// The latest snapshot (`snapshots/<lsn>.db`).
    pub snapshot: String,
    /// The LSN that snapshot holds everything up to.
    pub lsn: u64,
    /// When it was written (Unix ms; 0 when the manifest does not say).
    pub at: i64,
    /// The snapshot is encrypted.
    pub encrypted: bool,
    /// The `encryption.json` of the location, if it has one: what tells one
    /// database from another.
    pub header: Option<Vec<u8>>,
    /// How far the backup reaches in the log: the larger of the snapshot's
    /// LSN and the first LSN of the latest log segment after it. Two copies
    /// of one database get their segments from the same data directory, and
    /// a snapshot is taken where a segment ends, so the larger reach is the
    /// copy that holds more.
    pub reach: u64,
}

/// What `backup` holds: its manifest, its header and how far its log
/// segments go. `None` when it holds no backup (no manifest).
pub fn inspect(backup: &dyn BackupStore) -> Result<Option<BackupInfo>> {
    let Some(manifest) = backup.get(MANIFEST)? else { return Ok(None) };
    let j: Json = serde_json::from_slice(&manifest).map_err(|e| storage_err(format!("backup manifest: {e}")))?;
    let snapshot = j.get("snapshot").and_then(Json::as_str).ok_or_else(|| storage_err("backup manifest"))?.to_owned();
    let lsn = j.get("lsn").and_then(Json::as_u64).unwrap_or(0);
    let header = backup.get(HEADER_FILE)?;
    let last_segment =
        backup.list("wal/")?.iter().filter_map(|name| segment_lsn(name.trim_start_matches("wal/"))).max();
    Ok(Some(BackupInfo {
        snapshot,
        lsn,
        at: j.get("at").and_then(Json::as_i64).unwrap_or(0),
        encrypted: j.get("encrypted").and_then(Json::as_bool) == Some(true),
        header,
        reach: last_segment.unwrap_or(0).max(lsn),
    }))
}

/// How long, after the first destination has been read, a restore waits for
/// the second one ([`restore_best`]): a second that hangs stops the start
/// within seconds, not after its own time limit for each database.
const SECOND_WAIT: Duration = Duration::from_secs(5);

/// Restores an empty `dir` from one of the two places of a backup, whole,
/// never a mix of both: the one that reaches furthest ([`BackupInfo::reach`]),
/// the first on a tie; the other one when only one holds a backup. Returns
/// the place restored from, `None` when neither holds a backup.
///
/// Both places are read at the same time. A place that cannot be read is an
/// error, and so is a second that has not answered [`SECOND_WAIT`] after the
/// first: either may be the one that reaches furthest, and an older copy is
/// never restored in silence while a newer one may exist (to start from one
/// alone, the other is taken out of the settings). Two places that hold
/// different databases are an error too. On an error, nothing is restored.
pub(crate) fn restore_best(
    dir: &Path,
    first: &Arc<dyn BackupStore>,
    second: &Arc<dyn BackupStore>,
) -> Result<Option<String>> {
    let (one, two) = (first.describe(), second.describe());
    let (sent, looked) = std::sync::mpsc::channel();
    let looking = second.clone();
    // Left to end on its own when it does not answer in time.
    std::thread::Builder::new()
        .name("nuage-restore-2".into())
        .spawn(move || {
            let _ = sent.send(inspect(looking.as_ref()));
        })
        .map_err(|e| storage_err(format!("cannot read the second backup destination: {e}")))?;
    let a = inspect(first.as_ref()).map_err(|e| {
        storage_err(format!(
            "the first backup destination does not answer ({one}: {}): nothing was restored. To start from the \
             second alone ({two}), take the first out of the settings",
            super::reason(&e)
        ))
    })?;
    let unread = |why: String| {
        storage_err(format!(
            "the second backup destination does not answer ({two}: {why}): nothing was restored, since it may hold \
             more than the first. To start from the first alone ({one}), take the second out of the settings"
        ))
    };
    let b = match looked.recv_timeout(SECOND_WAIT) {
        Ok(read) => read.map_err(|e| unread(super::reason(&e)))?,
        Err(_) => return Err(unread(format!("no answer {} seconds after the first's", SECOND_WAIT.as_secs()))),
    };
    let chosen = match (&a, &b) {
        (None, None) => return Ok(None),
        (Some(_), None) => first,
        (None, Some(_)) => second,
        (Some(a), Some(b)) => {
            let same = match (&a.header, &b.header) {
                (Some(x), Some(y)) => same_database(x, y),
                // Backups in clear have nothing that tells them apart.
                (None, None) => true,
                _ => false,
            };
            if !same {
                return Err(storage_err(format!(
                    "the two backup destinations hold different databases ({one} and {two}): nothing was restored. \
                     Keep the one of this database in the settings, and give the other a place of its own"
                )));
            }
            if b.reach > a.reach { second } else { first }
        }
    };
    let from = chosen.describe();
    if !restore(dir, chosen.as_ref())? {
        return Err(storage_err(format!("{from} held a backup a moment ago and holds none now: nothing was restored")));
    }
    Ok(Some(from))
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

    /// Two places as one store: written in both, read from either, and an
    /// application's part of it is the same part of each place. Emptying it
    /// empties each place, even when the other cannot be emptied.
    #[test]
    fn a_mirror_is_two_places() {
        let dir = std::env::temp_dir().join(format!("nuage-backup-mirror-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (a, b) = (DirectoryStore::new(dir.join("a")), DirectoryStore::new(dir.join("b")));
        let both: Arc<dyn BackupStore> = Arc::new(MirrorStore::new(Arc::new(a.clone()), Arc::new(b.clone())));
        assert_eq!(both.describe(), format!("{} and {}", a.describe(), b.describe()));
        both.put("x", b"1").unwrap();
        assert_eq!((a.get("x").unwrap(), b.get("x").unwrap()), (Some(b"1".to_vec()), Some(b"1".to_vec())));
        b.put("y", b"2").unwrap();
        assert_eq!(both.get("y").unwrap(), Some(b"2".to_vec()), "from the second when the first has none");
        assert_eq!(both.list("").unwrap(), ["x", "y"]);
        assert!(a.parts().is_none());

        let shop = PrefixedStore::new(both.clone(), "apps/shop/");
        let (one, two) = shop.parts().expect("the same prefix in each place");
        assert_eq!(one.describe(), format!("{}/apps/shop", a.describe()));
        assert_eq!(shop.describe(), format!("{} and {}", one.describe(), two.describe()));
        assert!(shop.describe_aside(BEFORE_IMPORT).ends_with("b/before-import/apps/shop"));
        for part in [&one, &two] {
            part.put(MANIFEST, b"m").unwrap();
            part.put("wal/00000000000000000001.log", b"w").unwrap();
        }
        delete_all(&shop).unwrap();
        for place in [&a, &b] {
            assert!(place.list("apps/shop/wal/").unwrap().is_empty());
            assert_eq!(place.get("apps/shop/manifest.json").unwrap(), None);
        }
        assert_eq!(a.get("x").unwrap(), Some(b"1".to_vec()), "the rest stays");

        // A place that cannot be emptied (a file where its folder should be).
        fs::write(dir.join("not-a-folder"), b"").unwrap();
        let broken: Arc<dyn BackupStore> = Arc::new(DirectoryStore::new(dir.join("not-a-folder")));
        b.put("apps/blog/manifest.json", b"m").unwrap();
        let blog = PrefixedStore::new(Arc::new(MirrorStore::new(broken, Arc::new(b.clone()))), "apps/blog/");
        assert!(delete_all(&blog).is_err());
        assert_eq!(b.get("apps/blog/manifest.json").unwrap(), None, "the other place is emptied all the same");
        let _ = fs::remove_dir_all(&dir);
    }
}
