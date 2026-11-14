//! The Nuage DB engine: tables on disk, behind a write-ahead log.
//!
//! * **Pages**: Nuage DB's own B-trees, with copy-on-write pages, checksums
//!   and a bounded page cache, keep the data in one file that grows with it
//!   ([`pages`], [`store`]).
//! * **Log**: every commit is appended to the write-ahead log, applied in
//!   memory, then synced before it returns or is shown to a reader ([`wal`]);
//!   commits made at the same time share one sync. The page store is made
//!   durable at each checkpoint, and a restart replays the log written since.
//! * **Statements**: Memoire's evaluator over the rows read from disk
//!   ([`exec`]).
//! * **Branches**: copy-on-write overlays of the main database.
//! * **Upkeep**: a background thread checkpoints, ships backups and compacts
//!   the file, with no operator.

pub mod backup;
pub(crate) mod exec;
pub(crate) mod keys;
pub(crate) mod legacy;
pub(crate) mod pages;
pub(crate) mod store;
pub(crate) mod wal;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use memoire_orm::nuage::Branch;
use memoire_orm::{Error, Result};

use crate::crypto::{Encryption, Header, Keys};

pub use backup::{BackupStore, DirectoryStore, PrefixedStore};
use store::Store;
pub(crate) use store::{FILE, MAIN};

/// An error of the storage layer (disk, damaged data).
#[derive(Debug)]
pub struct StorageError(pub String);

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Nuage DB storage: {}", self.0)
    }
}

impl std::error::Error for StorageError {}

pub(crate) fn storage_err(msg: impl Into<String>) -> Error {
    Error::Database(Box::new(StorageError(msg.into())))
}

/// Backup settings of an [`Engine`].
#[derive(Clone)]
pub struct BackupOptions {
    /// Where backups go.
    pub store: Arc<dyn BackupStore>,
    /// Time between two snapshots of the page store. Default: 1 hour.
    pub snapshot_every: Duration,
    /// Snapshots kept. Default: 2.
    pub keep_snapshots: usize,
    /// Restore the latest backup when the data directory is empty at startup
    /// (a fresh container, a new disk). Default: `true`.
    pub restore_when_empty: bool,
}

impl std::fmt::Debug for BackupOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupOptions")
            .field("store", &self.store.describe())
            .field("snapshot_every", &self.snapshot_every)
            .finish()
    }
}

impl BackupOptions {
    /// Backups to `store`, with the default intervals.
    pub fn new(store: Arc<dyn BackupStore>) -> Self {
        BackupOptions { store, snapshot_every: Duration::from_secs(3600), keep_snapshots: 2, restore_when_empty: true }
    }
}

/// Settings of an [`Engine`].
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Bytes of pages kept in memory. Data beyond it stays on disk and is
    /// read when needed. Default: 64 MiB.
    pub cache_bytes: usize,
    /// A checkpoint at least this often when something was written. Default:
    /// 30 seconds.
    pub checkpoint_every: Duration,
    /// ... or when the log grows past this many bytes. Default: 16 MiB.
    pub checkpoint_wal_bytes: u64,
    /// Compaction is tried this often, and runs when a quarter of the file
    /// is free space. Default: 1 hour.
    pub compact_every: Duration,
    /// Backups, if any.
    pub backup: Option<BackupOptions>,
    /// `fsync` the log before a commit returns. Only tests turn it off.
    pub sync: bool,
    /// Name of the database, shown by the server. Default: `nuage`.
    pub name: String,
    /// Encryption at rest (see [`crate::crypto`]): with it, the page store,
    /// the log and the backups are encrypted, and a database stored in clear
    /// is encrypted when it is opened. `None`: stored in clear (only for an
    /// embedded database on a disk you trust; the server's binary requires
    /// `NUAGE_ENCRYPTION_KEY`). Default: `None`.
    pub encryption: Option<Encryption>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            cache_bytes: 64 << 20,
            checkpoint_every: Duration::from_secs(30),
            checkpoint_wal_bytes: 16 << 20,
            compact_every: Duration::from_secs(3600),
            backup: None,
            sync: true,
            name: "nuage".into(),
            encryption: None,
        }
    }
}

/// Something that happened, for logs and the live console.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Unix time in milliseconds.
    pub at: i64,
    /// `info`, `warn` or `error`.
    pub level: &'static str,
    /// What happened.
    pub message: String,
}

/// State of the backups.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackupStatus {
    /// Where they go (`None`: backups are off).
    pub target: Option<String>,
    /// Last successful shipment or snapshot (Unix ms).
    pub last_at: Option<i64>,
    /// Last attempt succeeded.
    pub last_ok: bool,
    /// Error of the last failed attempt.
    pub last_error: Option<String>,
    /// Last LSN known to be in the backup.
    pub shipped_lsn: u64,
    /// Last snapshot (Unix ms).
    pub last_snapshot_at: Option<i64>,
    /// The data directory was restored from the backup at startup.
    pub restored: bool,
}

/// Disk and cache figures.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StorageStats {
    /// Size of the page store file.
    pub file_bytes: u64,
    /// Bytes of data in it.
    pub stored_bytes: u64,
    /// Free space inside the file (reclaimed by compaction).
    pub fragmented_bytes: u64,
    /// Pages allocated.
    pub pages: u64,
    /// Page size.
    pub page_size: u64,
    /// Bytes of the write-ahead log on disk.
    pub wal_bytes: u64,
    /// Free bytes on the disk holding the data, when known.
    pub disk_free_bytes: Option<u64>,
    /// Page cache in use and its limit.
    pub cache_bytes: u64,
    /// Configured cache size.
    pub cache_capacity_bytes: u64,
    /// Share of page reads served by the cache since startup.
    pub cache_hit_rate: f64,
    /// Last commit applied.
    pub lsn: u64,
    /// Last checkpoint.
    pub checkpoint_lsn: u64,
    /// When the last checkpoint ran.
    pub last_checkpoint: Option<SystemTime>,
}

/// An open Nuage DB database: the page store, the log, the branches and the
/// background upkeep. Shared by the embedded handle and the server.
pub struct Engine {
    pub(crate) store: Store,
    options: EngineOptions,
    events: Mutex<VecDeque<Event>>,
    backup_status: Mutex<BackupStatus>,
    last_compaction: Mutex<Option<(i64, bool)>>,
    started: Instant,
    started_at: SystemTime,
    stop: AtomicBool,
    events_seq: AtomicU64,
    /// Table schemas read by SQL, made `'static` once.
    pub(crate) schemas: memoire_orm::nuage::wire::SchemaCache,
    /// `key` or `passphrase` when the data is encrypted.
    encryption: Option<&'static str>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("dir", &self.store.dir).finish_non_exhaustive()
    }
}

impl Engine {
    /// Opens (or creates) the database in `dir`: restores the backup if the
    /// directory is empty and a backup is configured, replays the log, then
    /// starts the upkeep thread.
    pub fn open(dir: impl AsRef<Path>, options: EngineOptions) -> Result<Arc<Engine>> {
        let dir: PathBuf = dir.as_ref().to_owned();
        let mut restored = false;
        if let Some(b) = &options.backup
            && b.restore_when_empty
            && backup::is_empty(&dir)
        {
            restored = backup::restore(&dir, b.store.as_ref())?;
        }
        let Unlocked { keys, migrated, imported } = unlock(&dir, &options)?;
        let encryption = options.encryption.as_ref().filter(|_| keys.is_some()).map(Encryption::kind);
        let store = Store::open(&dir, options.cache_bytes, options.sync, keys.as_ref())?;
        let backup_status = BackupStatus {
            target: options.backup.as_ref().map(|b| b.store.describe()),
            last_ok: true,
            restored,
            shipped_lsn: store.applied_lsn(),
            ..BackupStatus::default()
        };
        let engine = Arc::new(Engine {
            store,
            options,
            events: Mutex::new(VecDeque::new()),
            backup_status: Mutex::new(backup_status),
            last_compaction: Mutex::new(None),
            started: Instant::now(),
            started_at: SystemTime::now(),
            stop: AtomicBool::new(false),
            events_seq: AtomicU64::new(0),
            schemas: memoire_orm::nuage::wire::SchemaCache::default(),
            encryption,
        });
        if restored {
            engine.event("info", "data restored from the backup");
        }
        if imported {
            engine.event(
                "info",
                format!(
                    "the page store of an earlier version ({}) was imported; it is kept as {}",
                    legacy::LEGACY_FILE,
                    legacy::IMPORTED
                ),
            );
            if !migrated && engine.options.backup.is_some() {
                // The next restore then starts from a snapshot of this version.
                let _ = engine.backup_now(true);
            }
        }
        if migrated {
            engine.event("warn", "the data was stored in clear: it is now encrypted");
            if let Some(b) = &engine.options.backup {
                // The backup in clear is replaced by an encrypted one.
                match engine.backup_now(true).and_then(|()| backup::purge_older(b.store.as_ref())) {
                    Ok(()) => engine.event("info", "the backup in clear was replaced by an encrypted one"),
                    Err(e) => engine.event("error", format!("could not replace the backup in clear: {e}")),
                }
            }
        }
        engine.event("info", format!("data is stored in {}", dir.display()));
        let weak = Arc::downgrade(&engine);
        std::thread::Builder::new()
            .name("nuage-upkeep".into())
            .spawn(move || upkeep(weak))
            .map_err(|e| storage_err(format!("cannot start the upkeep thread: {e}")))?;
        Ok(engine)
    }

    /// How the data is encrypted: `Some("key")` or `Some("passphrase")`, or
    /// `None` when it is stored in clear.
    pub fn encryption(&self) -> Option<&'static str> {
        self.encryption
    }

    /// The data directory.
    pub fn dir(&self) -> &Path {
        &self.store.dir
    }

    /// The options it was opened with.
    pub fn options(&self) -> &EngineOptions {
        &self.options
    }

    /// Time since it was opened.
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// When it was opened.
    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }

    pub(crate) fn event(&self, level: &'static str, message: impl Into<String>) {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.push_back(Event { at: store::now_millis(), level, message: message.into() });
        if events.len() > 200 {
            events.pop_front();
        }
        self.events_seq.fetch_add(1, Ordering::SeqCst);
    }

    /// Recent events, oldest first, and a counter that moves with each new one.
    pub fn events(&self) -> (Vec<Event>, u64) {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        (events.iter().cloned().collect(), self.events_seq.load(Ordering::SeqCst))
    }

    /// Makes every commit durable in the page store and starts a new log
    /// segment.
    pub fn checkpoint(&self) -> Result<u64> {
        let keep = self.options.backup.as_ref().map(|_| self.backup_status().shipped_lsn);
        let lsn = self.store.checkpoint(keep)?;
        self.event("info", format!("checkpoint at {lsn}"));
        Ok(lsn)
    }

    /// Rewrites the file without its free space. Returns `false` when it
    /// could not run now (a read is in progress); the upkeep thread tries
    /// again later.
    pub fn compact(&self) -> Result<bool> {
        let mut w = self.store.writer();
        let keep = self.options.backup.as_ref().map(|_| self.backup_status().shipped_lsn);
        self.store.checkpoint_locked(&mut w, keep)?;
        let done = self.store.pages.compact()?;
        drop(w);
        *self.last_compaction.lock().unwrap_or_else(|e| e.into_inner()) = Some((store::now_millis(), done));
        if done {
            self.event("info", "compaction done");
        }
        Ok(done)
    }

    /// Reads every page of the page store and checks it: checksums, order of
    /// the keys, sizes, no page used twice. `false` when something is wrong
    /// (the events say what). Commits wait while it runs; reads do not.
    pub fn check_integrity(&self) -> Result<bool> {
        let _w = self.store.writer();
        match self.store.pages.check() {
            Ok(()) => Ok(true),
            Err(reason) => {
                self.event("error", format!("integrity check: {reason}"));
                Ok(false)
            }
        }
    }

    /// Ships the sealed log segments, and takes a snapshot when one is due
    /// (or `force_snapshot`).
    pub fn backup_now(&self, force_snapshot: bool) -> Result<()> {
        let Some(b) = &self.options.backup else {
            return Err(Error::Unsupported("no backup location is configured".into()));
        };
        let status = self.backup_status();
        let due =
            status.last_snapshot_at.is_none_or(|at| store::now_millis() - at >= b.snapshot_every.as_millis() as i64);
        let result = (|| -> Result<(u64, bool)> {
            if force_snapshot || due {
                let lsn = backup::snapshot(&self.store, b.store.as_ref(), b.keep_snapshots)?;
                let shipped = backup::ship_segments(&self.store, b.store.as_ref(), lsn)?;
                Ok((shipped.max(lsn), true))
            } else {
                Ok((backup::ship_segments(&self.store, b.store.as_ref(), status.shipped_lsn)?, false))
            }
        })();
        let mut s = self.backup_status.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok((shipped, snap)) => {
                let moved = shipped != s.shipped_lsn || snap;
                s.shipped_lsn = shipped;
                s.last_ok = true;
                s.last_error = None;
                if moved {
                    s.last_at = Some(store::now_millis());
                }
                if snap {
                    s.last_snapshot_at = Some(store::now_millis());
                }
                drop(s);
                if snap {
                    self.event("info", format!("backup snapshot sent to {}", b.store.describe()));
                }
                Ok(())
            }
            Err(e) => {
                s.last_ok = false;
                s.last_error = Some(e.to_string());
                drop(s);
                self.event("error", format!("backup failed: {e}"));
                Err(e)
            }
        }
    }

    /// State of the backups.
    pub fn backup_status(&self) -> BackupStatus {
        self.backup_status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Last compaction (Unix ms, whether it ran).
    pub fn last_compaction(&self) -> Option<(i64, bool)> {
        *self.last_compaction.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Disk and cache figures.
    pub fn stats(&self) -> StorageStats {
        let file_bytes = std::fs::metadata(self.store.dir.join(FILE)).map_or(0, |m| m.len());
        let wal_bytes = wal::segments(&self.store.dir.join("wal"))
            .unwrap_or_default()
            .iter()
            .filter_map(|(_, p)| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let (lsn, checkpoint_lsn, last_checkpoint) = {
            let w = self.store.writer();
            (w.next_lsn - 1, w.checkpoint_lsn, w.last_checkpoint)
        };
        let pages = self.store.pages.stats();
        let (hits, misses) = (pages.cache_hits, pages.cache_misses);
        StorageStats {
            file_bytes,
            stored_bytes: pages.stored_bytes,
            fragmented_bytes: pages.free_bytes,
            pages: pages.pages,
            page_size: pages::PAGE as u64,
            wal_bytes,
            disk_free_bytes: disk_free(&self.store.dir),
            cache_bytes: pages.cache_bytes,
            cache_capacity_bytes: self.options.cache_bytes as u64,
            cache_hit_rate: if hits + misses == 0 { 1.0 } else { hits as f64 / (hits + misses) as f64 },
            lsn,
            checkpoint_lsn,
            last_checkpoint,
        }
    }

    /// `true` when `name` is a branch (or `main`, the main database).
    pub fn has_branch(&self, name: &str) -> bool {
        self.store.has_branch(name)
    }

    /// The branches, oldest first.
    pub fn branches(&self) -> Vec<Branch> {
        self.store
            .branches()
            .into_iter()
            .map(|b| Branch {
                own_bytes: self.store.branch_bytes(&b.name),
                parent: (b.parent != MAIN).then_some(b.parent),
                name: b.name,
                created_at: b.created_at,
            })
            .collect()
    }

    /// Creates a branch of the main database (or of the branch `from`):
    /// instant, whatever the size of the data. It shares every page of its
    /// parent and stores only its own changes; later changes of the parent
    /// do not reach it.
    pub fn create_branch(&self, name: &str, from: Option<&str>) -> Result<Branch> {
        check_branch_name(name)?;
        let parent = from.unwrap_or(MAIN);
        let info = self.store.create_branch(name, parent)?;
        self.event("info", format!("branch `{name}` created from `{}`", from.unwrap_or(&self.options.name)));
        Ok(Branch { name: info.name, parent: from.map(str::to_owned), created_at: info.created_at, own_bytes: 0 })
    }

    /// Deletes a branch and what it stored. `false` when there is none.
    pub fn delete_branch(&self, name: &str) -> Result<bool> {
        let done = self.store.drop_branch(name, false)?;
        if done {
            self.event("info", format!("branch `{name}` deleted"));
        }
        Ok(done)
    }

    /// Forgets a branch's changes: it is a fresh copy of its parent again.
    pub fn reset_branch(&self, name: &str) -> Result<bool> {
        let done = self.store.drop_branch(name, true)?;
        if done {
            self.event("info", format!("branch `{name}` reset"));
        }
        Ok(done)
    }

    /// Metadata entries starting with `prefix` (not data: the server's
    /// records, such as its applications).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn meta_entries(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.store.meta_entries(prefix)
    }

    /// Writes or removes a metadata entry, durably (through the log).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn set_meta(&self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        self.store.set_meta(key, value)
    }

    /// Stops the upkeep thread and makes everything durable.
    pub fn close(&self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        self.checkpoint().map(|_| ())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let keep = self.options.backup.as_ref().map(|_| self.backup_status().shipped_lsn);
        let _ = self.store.checkpoint(keep);
    }
}

/// Free bytes on the disk of `dir`.
#[cfg(unix)]
fn disk_free(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string and `stat` a valid out-pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
fn disk_free(_: &Path) -> Option<u64> {
    None
}

/// The upkeep thread: checkpoints, backups and compaction, until the engine
/// is dropped.
fn upkeep(engine: Weak<Engine>) {
    let mut last_compact = Instant::now();
    let mut last_backup_try = Instant::now() - Duration::from_secs(3600);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let Some(e) = engine.upgrade() else { return };
        if e.stop.load(Ordering::SeqCst) {
            return;
        }
        let (due, idle) = {
            let w = e.store.writer();
            let age = w.last_checkpoint.and_then(|t| t.elapsed().ok()).unwrap_or_default();
            let pending = w.commits_since_checkpoint > 0;
            (pending && (age >= e.options.checkpoint_every || w.wal.bytes >= e.options.checkpoint_wal_bytes), !pending)
        };
        if due && let Err(err) = e.checkpoint() {
            e.event("error", format!("checkpoint failed: {err}"));
        }
        if e.options.backup.is_some() && (due || last_backup_try.elapsed() >= Duration::from_secs(60)) {
            last_backup_try = Instant::now();
            let _ = e.backup_now(false);
        }
        if idle && last_compact.elapsed() >= e.options.compact_every {
            last_compact = Instant::now();
            let s = e.stats();
            if s.file_bytes > 0 && s.fragmented_bytes * 4 >= s.file_bytes {
                let _ = e.compact();
            }
        }
    }
}

/// Where an encrypted copy of a page store in clear is written before it
/// takes its place.
const ENCRYPTING: &str = "nuage.db.encrypting";

/// What [`unlock`] found.
struct Unlocked {
    /// The keys of the directory, when it is encrypted.
    keys: Option<Keys>,
    /// Data in clear was just encrypted.
    migrated: bool,
    /// The page store of an earlier version was just imported.
    imported: bool,
}

/// The keys of `dir` for `options.encryption`: opens its header, creates one
/// for a new directory, or encrypts a directory stored in clear. On the way,
/// the page store of an earlier version is imported (see [`legacy`]).
fn unlock(dir: &Path, options: &EngineOptions) -> Result<Unlocked> {
    let header = Header::read(dir)?;
    let tmp = dir.join(ENCRYPTING);
    match (&options.encryption, header) {
        (None, None) => {
            let imported = legacy::import(dir, None, options.cache_bytes)?;
            Ok(Unlocked { keys: None, migrated: false, imported })
        }
        (None, Some(_)) => Err(Error::Validation(format!(
            "the data in {} is encrypted: set NUAGE_ENCRYPTION_KEY (EngineOptions::encryption) to open it",
            dir.display()
        ))),
        (Some(encryption), Some(header)) => {
            let keys = header.unlock(encryption)?;
            // The encrypted copy was made, not yet put in place.
            let resumed = tmp.exists();
            if resumed {
                finish_encrypting(dir)?;
            }
            let imported = legacy::import(dir, Some(&keys), options.cache_bytes)?;
            Ok(Unlocked { keys: Some(keys), migrated: resumed, imported })
        }
        (Some(encryption), None) => {
            // A copy left by an encryption that did not finish: start again.
            let _ = std::fs::remove_file(&tmp);
            let imported = legacy::import(dir, None, options.cache_bytes)?;
            if dir.join(FILE).exists() {
                let keys = encrypt_in_place(dir, encryption, options)?;
                return Ok(Unlocked { keys: Some(keys), migrated: true, imported });
            }
            std::fs::create_dir_all(dir).map_err(|e| storage_err(format!("cannot create {}: {e}", dir.display())))?;
            let (header, keys) = Header::create(encryption)?;
            header.write(dir)?;
            Ok(Unlocked { keys: Some(keys), migrated: false, imported })
        }
    }
}

/// Encrypts a directory stored in clear: replays its log, copies every
/// table into an encrypted file, writes the header (from then on the
/// directory is encrypted), then puts the copy in place of the file in clear
/// and removes the log in clear. A crash at any point resumes or restarts it.
fn encrypt_in_place(dir: &Path, encryption: &Encryption, options: &EngineOptions) -> Result<Keys> {
    // Everything in the log in clear goes into the page store.
    drop(Store::open(dir, options.cache_bytes, true, None)?);
    let (header, keys) = Header::create(encryption)?;
    {
        let clear = store::open_pages(&dir.join(FILE), options.cache_bytes, None, true)?;
        clear.copy_to(&dir.join(ENCRYPTING), Some(keys.pages.clone()))?;
    }
    header.write(dir)?;
    finish_encrypting(dir)?;
    // What an import kept of the earlier version's file is in clear too.
    let _ = std::fs::remove_file(dir.join(legacy::IMPORTED));
    Ok(keys)
}

fn finish_encrypting(dir: &Path) -> Result<()> {
    let io = |e: std::io::Error| storage_err(format!("encrypting {}: {e}", dir.display()));
    std::fs::rename(dir.join(ENCRYPTING), dir.join(FILE)).map_err(io)?;
    // The checkpoint covers the whole log: the segments in clear go.
    for (_, path) in wal::segments(&dir.join("wal"))? {
        std::fs::remove_file(&path).map_err(io)?;
    }
    wal::sync_dir(dir);
    wal::sync_dir(&dir.join("wal"));
    Ok(())
}

/// Branch names: 1 to 63 lowercase letters, digits, `-` and `_`.
pub(crate) fn check_branch_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name != MAIN;
    if ok {
        Ok(())
    } else {
        Err(Error::Validation(format!(
            "invalid branch name `{name}`: use 1 to 63 lowercase letters, digits, `-` and `_` (and not `main`)"
        )))
    }
}
